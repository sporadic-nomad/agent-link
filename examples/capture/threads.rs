//! The capture tool's threads: PTY writer, PTY reader (stream + screen model), local stdin
//! for interactive captures (minus the real terminal's query replies), and the timed script.

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::events::{EventLog, Val};
#[cfg(unix)]
use crate::replies::ReplyFilter;
use crate::tap::{self, LiveScreen};
use crate::text::{self, Entry, Step};

/// Ctrl-]: ends an interactive capture.
#[cfg(unix)]
const STOP_KEY: u8 = 0x1d;
/// Upper bound of one `{wait-text:…}`.
const WAIT_TEXT_MAX: Duration = Duration::from_secs(30);

/// Screen state the script thread reads.
#[derive(Default)]
pub struct Shared {
    pub text: String,
    pub app_cursor: bool,
}

/// Who sent bytes to the PTY; the `source` of a `write` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A `--script` step (or the typed `--shell-line`).
    Script,
    /// Forwarded local stdin of an interactive capture (unix only).
    #[cfg(unix)]
    Stdin,
    /// A query reply from the tap.
    Reply,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Self::Script => "script",
            #[cfg(unix)]
            Self::Stdin => "stdin",
            Self::Reply => "reply",
        }
    }
}

/// Bytes for the PTY writer, logged as a `write` event once they reached the PTY.
pub struct Outgoing {
    pub bytes: Vec<u8>,
    pub source: Source,
}

impl Outgoing {
    pub fn new(source: Source, bytes: Vec<u8>) -> Self {
        Self { bytes, source }
    }
}

/// Runs the PTY writer until every `Sender` is gone or a write fails. The writer is never
/// dropped: on unix portable-pty's writer `Drop` types Enter + EOF (VEOF) into the PTY, an
/// unlogged keystroke pair that ends `cat` or a shell and can confirm a dialog. The last
/// sender can go long before teardown (the script ends, `--no-answer-queries`), and a
/// descendant outside the killed groups may still read the slave afterwards, so no drop is
/// safe; the fd (Windows: the input pipe handle) is released at process exit instead.
pub fn spawn_writer(
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Outgoing>,
    log: Arc<EventLog>,
) -> Result<(), String> {
    // Wrapped before the closure exists, so not even a failed thread spawn drops it.
    let mut writer = ManuallyDrop::new(writer);
    thread::Builder::new()
        .name("capture-writer".into())
        .spawn(move || {
            for out in rx {
                let shown = text::escape_bytes(&out.bytes);
                let source = Val::Str(out.source.name().into());
                match writer.write_all(&out.bytes).and_then(|()| writer.flush()) {
                    Ok(()) => log.emit("write", &[("value", Val::Str(shown)), ("source", source)]),
                    Err(e) => {
                        let msg = format!("pty write of {shown} failed: {e}");
                        log.emit("error", &[("value", Val::Str(msg)), ("source", source)]);
                        break;
                    }
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("writer thread: {e}"))
}

pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut screen: LiveScreen,
    stream: Arc<Mutex<Vec<u8>>>,
    shared: Arc<Mutex<Shared>>,
    echo: bool,
    done: Sender<()>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("capture-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; 8192];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                let chunk = &buf[..n];
                let offset = {
                    let mut stream = stream.lock().unwrap_or_else(|e| e.into_inner());
                    stream.extend_from_slice(chunk);
                    stream.len() - n
                };
                if echo {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(chunk).and_then(|()| out.flush());
                }
                screen.feed(chunk, offset);
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                state.text = tap::screen_text(screen.screen());
                state.app_cursor = screen.screen().application_cursor();
            }
            let _ = done.send(());
        })
        .map(drop)
        .map_err(|e| format!("reader thread: {e}"))
}

/// Forwards local stdin to the child, minus the real terminal's query replies (see
/// `replies`); each dropped reply is logged as `stdin_reply_dropped`. Ctrl-] stops.
#[cfg(unix)]
pub fn spawn_stdin(
    tx: Sender<Outgoing>,
    stop: Arc<AtomicBool>,
    log: Arc<EventLog>,
) -> Result<(), String> {
    thread::Builder::new()
        .name("capture-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            let mut filter = ReplyFilter::default();
            while let Ok(n @ 1..) = stdin.read(&mut buf) {
                let chunk = &buf[..n];
                let stop_at = chunk.iter().position(|&b| b == STOP_KEY);
                let out = filter.filter(&chunk[..stop_at.unwrap_or(n)]);
                for reply in &out.dropped {
                    let shown = Val::Str(text::escape_bytes(reply));
                    log.emit("stdin_reply_dropped", &[("value", shown)]);
                }
                let sent =
                    out.pass.is_empty() || tx.send(Outgoing::new(Source::Stdin, out.pass)).is_ok();
                if stop_at.is_some() {
                    stop.store(true, Ordering::Relaxed);
                }
                if stop_at.is_some() || !sent {
                    break;
                }
            }
        })
        .map(drop)
        .map_err(|e| format!("stdin thread: {e}"))
}

pub fn run_script(
    entries: &[Entry],
    start: Instant,
    tx: &Sender<Outgoing>,
    shared: &Mutex<Shared>,
    log: &EventLog,
) {
    // The writer thread logs `write` once the bytes reached the PTY.
    let send = |bytes: Vec<u8>| tx.send(Outgoing::new(Source::Script, bytes)).is_ok();
    for entry in entries {
        let due = start + entry.at;
        let now = Instant::now();
        if due > now {
            thread::sleep(due - now);
        }
        for step in &entry.steps {
            let sent = match step {
                Step::Write(bytes) => send(bytes.clone()),
                Step::Arrow(key) => {
                    let app = shared.lock().unwrap_or_else(|e| e.into_inner()).app_cursor;
                    let intro: &[u8] = if app { b"\x1bO" } else { b"\x1b[" };
                    let mut bytes = intro.to_vec();
                    bytes.push(*key);
                    send(bytes)
                }
                Step::WaitText(needle) => {
                    let begin = Instant::now();
                    let found = loop {
                        if shared
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .text
                            .contains(needle.as_str())
                        {
                            break true;
                        }
                        if begin.elapsed() >= WAIT_TEXT_MAX {
                            break false;
                        }
                        thread::sleep(Duration::from_millis(25));
                    };
                    log.emit(
                        "wait_text",
                        &[
                            ("needle", Val::Str(needle.clone())),
                            ("found", Val::Bool(found)),
                        ],
                    );
                    if !found {
                        // The screen is not what the rest of the script expects: the next
                        // keys could answer a dialog that persists a choice. Stop here.
                        let msg = format!(
                            "script aborted: {needle:?} did not appear within {} s",
                            WAIT_TEXT_MAX.as_secs()
                        );
                        log.emit("error", &[("value", Val::Str(msg))]);
                    }
                    found
                }
            };
            if !sent {
                return;
            }
        }
    }
}
