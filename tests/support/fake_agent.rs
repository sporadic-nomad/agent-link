//! `agent-link-fake-agent`: a scripted stand-in for an agent TUI, run by the crate's
//! integration tests (found through `CARGO_BIN_EXE_agent-link-fake-agent`). Not part of the
//! library; only std plus the crate's platform dependencies.
//!
//! `agent-link-fake-agent <scene> [--sync] [--title] [--exit-on-input] [--reset-delay-ms N]
//! [--reset-silence-ms N] [--dialog-after-ms N] [--swallow-ms N] [--keep-frame]` — an
//! unknown scene, flag or value prints the error, the usage and the scene list, and exits 2.
//!
//! Screen (rows 1-based, absolute cursor moves + erase-line, any terminal size):
//! - row 1 `fake-agent v1 (<scene>)`, then the transcript (`you: <text>`, `agent: ok #<n>`,
//!   `enter-absorbed`, `bye`; oldest lines dropped when the screen is full);
//! - then one block: the dialog (`fake trust dialog`, `❯ 1. No, exit`,
//!   `  2. Yes, trust this folder`, `enter to confirm`; `❯` marks the selection), or the
//!   composer `> <content>` above the footer `fake-agent ready · esc to interrupt`, or
//!   `resetting conversation`.
//!
//! Terminal: raw input (unix: `ICANON`, `ECHO`, `IEXTEN`, `ICRNL`, `IXON` off, `ISIG` kept —
//! ^C raises SIGINT, which a dedicated thread takes with `sigwait` and feeds in as `\x03`;
//! Windows: line input, echo and processed input off, VT input on — ^C arrives as `\x03`).
//! `\x03` exits 130 through the normal exit, so output never stops inside a sequence.
//! Bracketed paste on while running. Every exit erases the block (the shell's prompt
//! follows the transcript), turns bracketed paste off, ends a pending spinner title and
//! progress, and restores the terminal. `--keep-frame`: the block stays and the shell's
//! prompt goes on the row below it — the last frame an inline agent leaves (codex, claude),
//! its ready footer included.
//!
//! Shared behaviour (a synthetic model, simpler than real agents):
//! - A bracketed paste of more than one line shows `[Pasted text #<k> +<lines> lines]` in
//!   the composer (claude shows lines − 1 and chips only long pastes); one line shows inline
//!   (an empty one adds nothing). Backspace removes one character or a whole chip.
//! - Enter with a non-empty composer clears it and adds `you: <composer>` at once;
//!   `agent: ok #<n>` follows 300 ms later. Typed `/new` + Enter resets instead: pending
//!   replies, spinner and progress end at once, nothing is drawn for `--reset-silence-ms`
//!   (default 0), then `resetting conversation`, then a fresh screen `--reset-delay-ms`
//!   (default 200) later. Input during a reset is dropped.
//! - Esc is read (a lone ESC never swallows the byte after it) and ignored.
//! - `--title`: OSC 0 title `fake`, a Braille glyph + ` fake` every 100 ms while a reply is
//!   pending. `--background-title`: a title update every 100 ms even while idle.
//!   `--record-enter`: show every Enter; `--ignore-enter`: keep its composer unchanged.
//!   `--submit-display-delay-ms N`: accept each ordinary submit immediately but keep
//!   the composer unchanged until its display delay; repeated Enters queue duplicates.
//!   `--sync`: every redraw inside DEC 2026 (`ESC[?2026h` … `ESC[?2026l`).
//! - `--exit-on-input`: reads nothing; exits 0 (the normal exit) as soon as input is
//!   pending, leaving it unread for the shell — an agent that leaves before reading a
//!   paste (unix: `poll`; Windows: the console handle's wait).
//!
//! | Scene | Extra |
//! |---|---|
//! | `trust` | Dialog first, pointer on No; Up/Down move it; Enter on Yes ⇒ composer, on No ⇒ `bye`, exit 1. |
//! | `ready`, `composer` | Composer at once. |
//! | `swallow` | Enter < `--swallow-ms` (default 100) after a paste end is absorbed (`enter-absorbed` line, composer kept). |
//! | `late-dialog` | Composer, then the dialog `--dialog-after-ms` (default 1500) after start (after the fresh screen if a reset is running then). |
//! | `slow-dialog` | Nothing drawn until the dialog, `--dialog-after-ms` after start. |
//! | `exit-to-shell` | Exits 0 at the first submit, right after drawing `you: <text>` (no reply). |
//! | `progress` | OSC `9;4;3` on each submit, `9;4;0` 400 ms after the last. |
//! | `silent` | Never draws a composer or dialog; a submit still adds transcript lines. |

#[cfg(feature = "acp")]
#[path = "fake_agent/acp_peer.rs"]
mod acp_peer;
#[cfg(feature = "claude")]
#[path = "fake_agent/claude_peer.rs"]
mod claude_peer;
#[cfg(feature = "codex")]
#[path = "fake_agent/codex_peer.rs"]
mod codex_peer;
#[path = "fake_agent/input.rs"]
mod input;
#[path = "fake_agent/scenes.rs"]
mod scenes;
#[cfg(unix)]
#[path = "fake_agent/tty_unix.rs"]
mod tty;
#[cfg(windows)]
#[path = "fake_agent/tty_windows.rs"]
mod tty;

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use input::{Key, Parser};
use scenes::SCENES;

/// Three lines, each under 80 columns.
const USAGE: &str = "usage: agent-link-fake-agent <scene> [--sync] [--title] \
                     [--exit-on-input]\n       [--reset-delay-ms N] [--reset-silence-ms N] \
                     [--dialog-after-ms N]\n       [--swallow-ms N] [--keep-frame]";
const REPLY_AFTER: Duration = Duration::from_millis(300);
const SPINNER_EVERY: Duration = Duration::from_millis(100);
const PROGRESS_FOR: Duration = Duration::from_millis(400);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn main() {
    #[cfg(feature = "acp")]
    if std::env::args().nth(1).as_deref() == Some("--acp-peer") {
        acp_peer::run(&std::env::args().nth(2).expect("peer scenario"));
        return;
    }
    #[cfg(feature = "claude")]
    if std::env::args().nth(1).as_deref() == Some("--claude-peer") {
        claude_peer::run(&std::env::args().nth(2).expect("peer scenario"));
        return;
    }
    #[cfg(feature = "codex")]
    if std::env::args().nth(1).as_deref() == Some("--codex-peer") {
        codex_peer::run(&std::env::args().nth(2).expect("peer scenario"));
        return;
    }
    let opts = Opts::parse(std::env::args().skip(1)).unwrap_or_else(|e| {
        let scenes = SCENES.join(", ");
        eprintln!("agent-link-fake-agent: {e}\n{USAGE}\nscenes: {scenes}");
        std::process::exit(2)
    });
    let (tx, rx) = mpsc::channel();
    // Before raw mode (^C) and before the reader thread (it inherits the signal mask).
    let interrupt = tx.clone();
    tty::route_interrupt(move || {
        let _ = interrupt.send(Input::Interrupt);
    });
    tty::enter_raw();
    if opts.exit_on_input {
        std::thread::spawn(move || {
            tty::wait_readable();
            let _ = tx.send(Input::Unread);
        });
    } else {
        std::thread::spawn(move || read_input(tx));
    }
    let mut app = App::new(opts);
    app.start(Instant::now());
    let code = app.run(&rx);
    app.finish();
    tty::restore();
    std::process::exit(code);
}

/// What the main loop receives.
enum Input {
    /// One stdin read; empty at EOF or a read error (the terminal went away).
    Bytes(Vec<u8>),
    /// SIGINT (unix). Not a byte through the parser: a ^C during a paste or an escape
    /// sequence would otherwise be swallowed by it.
    Interrupt,
    /// Input is pending, left unread (`--exit-on-input`): the run ends.
    Unread,
}

/// Forwards every stdin read to the main loop, then an empty one at EOF or a read error,
/// which ends the run. (The ^C sender keeps the channel open.)
fn read_input(tx: Sender<Input>) {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read(&mut buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(n) => n,
            Err(_) => 0,
        };
        if tx.send(Input::Bytes(buf[..n].to_vec())).is_err() || n == 0 {
            return;
        }
    }
}

/// Writes and flushes; errors are ignored (the terminal is gone, stdin EOF ends the run).
fn emit(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

struct Opts {
    scene: &'static str,
    sync: bool,
    title: bool,
    background_title: bool,
    ignore_enter: bool,
    record_enter: bool,
    exit_on_input: bool,
    keep_frame: bool,
    reset_delay: Duration,
    reset_silence: Duration,
    dialog_after: Duration,
    swallow: Duration,
    submit_display_delay: Duration,
}

impl Opts {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Opts, String> {
        let scene = args.next().ok_or("missing <scene>")?;
        let scene = SCENES
            .iter()
            .copied()
            .find(|s| *s == scene)
            .ok_or_else(|| format!("unknown scene {scene:?}"))?;
        let mut opts = Opts {
            scene,
            sync: false,
            title: false,
            background_title: false,
            ignore_enter: false,
            record_enter: false,
            exit_on_input: false,
            keep_frame: false,
            reset_delay: Duration::from_millis(200),
            reset_silence: Duration::ZERO,
            dialog_after: Duration::from_millis(1500),
            swallow: Duration::from_millis(100),
            submit_display_delay: Duration::ZERO,
        };
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--sync" => {
                    opts.sync = true;
                    continue;
                }
                "--title" => {
                    opts.title = true;
                    continue;
                }
                "--background-title" => {
                    opts.background_title = true;
                    continue;
                }
                "--ignore-enter" => {
                    opts.ignore_enter = true;
                    continue;
                }
                "--record-enter" => {
                    opts.record_enter = true;
                    continue;
                }
                "--exit-on-input" => {
                    opts.exit_on_input = true;
                    continue;
                }
                "--keep-frame" => {
                    opts.keep_frame = true;
                    continue;
                }
                "--reset-delay-ms" => &mut opts.reset_delay,
                "--reset-silence-ms" => &mut opts.reset_silence,
                "--dialog-after-ms" => &mut opts.dialog_after,
                "--swallow-ms" => &mut opts.swallow,
                "--submit-display-delay-ms" => &mut opts.submit_display_delay,
                _ => return Err(format!("unknown flag {flag:?}")),
            };
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            let ms: u64 = value
                .parse()
                .map_err(|_| format!("{flag}: {value:?} is not a number of milliseconds"))?;
            *slot = Duration::from_millis(ms);
        }
        Ok(opts)
    }
}

/// A composer part: typed or single-line pasted bytes, or a multi-line paste's chip.
enum Piece {
    Text(Vec<u8>),
    Chip(String),
}

enum Stage {
    /// Nothing drawn yet (`slow-dialog` before its dialog); input dropped.
    Blank,
    Dialog {
        yes: bool,
    },
    Chat,
    /// `/new` submitted: `resetting conversation` at `visible_at` (`None` once drawn), the
    /// fresh screen at `fresh_at`.
    Resetting {
        visible_at: Option<Instant>,
        fresh_at: Instant,
    },
}

struct App {
    opts: Opts,
    stage: Stage,
    transcript: Vec<String>,
    composer: Vec<Piece>,
    /// Chips shown in this conversation (`#k`).
    pastes: usize,
    /// Replies drawn in this conversation (`#n`).
    replies_done: usize,
    /// When each pending reply is drawn.
    replies: VecDeque<Instant>,
    /// Accepted submits whose displayed composer/transcript update is delayed.
    delayed_submits: VecDeque<(Instant, String)>,
    /// Next spinner tick and its frame, while a reply is pending under `--title`.
    spinner: Option<(Instant, usize)>,
    progress_clear: Option<Instant>,
    background_title_at: Option<Instant>,
    dialog_at: Option<Instant>,
    last_paste_end: Option<Instant>,
    /// First row (1-based) of the block in the last draw; `None` before the first draw.
    block_row: Option<usize>,
    /// Last row (1-based) of the last draw.
    last_row: usize,
    exit: Option<i32>,
}

impl App {
    fn new(opts: Opts) -> Self {
        App {
            opts,
            stage: Stage::Blank,
            transcript: Vec::new(),
            composer: Vec::new(),
            pastes: 0,
            replies_done: 0,
            replies: VecDeque::new(),
            delayed_submits: VecDeque::new(),
            spinner: None,
            progress_clear: None,
            background_title_at: None,
            dialog_at: None,
            last_paste_end: None,
            block_row: None,
            last_row: 0,
            exit: None,
        }
    }

    fn start(&mut self, now: Instant) {
        emit("\x1b[?2004h");
        if self.opts.background_title {
            self.background_title_at = Some(now);
        }
        if self.opts.title {
            set_title("fake");
        }
        self.stage = match self.opts.scene {
            "trust" => Stage::Dialog { yes: false },
            "slow-dialog" => Stage::Blank,
            _ => Stage::Chat,
        };
        if matches!(self.opts.scene, "late-dialog" | "slow-dialog") {
            self.dialog_at = Some(now + self.opts.dialog_after);
        }
        self.draw();
    }

    /// Runs until an exit is due or stdin closes; returns the exit code.
    fn run(&mut self, rx: &Receiver<Input>) -> i32 {
        let mut parser = Parser::default();
        let mut keys = Vec::new();
        loop {
            // Before the timers: nothing is drawn after the key that ends the run.
            if let Some(code) = self.exit {
                return code;
            }
            self.fire(Instant::now());
            let input = match self.next_deadline() {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match input {
                Ok(Input::Interrupt) => self.key(Key::Interrupt, Instant::now()),
                Ok(Input::Unread) => return 0,
                Ok(Input::Bytes(bytes)) if bytes.is_empty() => return 0, // stdin ended
                Ok(Input::Bytes(bytes)) => {
                    // One time for the whole read: a paste and an Enter read together
                    // arrived together.
                    let now = Instant::now();
                    for byte in bytes {
                        parser.feed(byte, &mut keys);
                    }
                    for key in keys.drain(..) {
                        self.key(key, now);
                        if self.exit.is_some() {
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return 0,
            }
        }
    }

    /// Ends the working signals, erases the block (the shell's prompt goes there; with
    /// `--keep-frame` below the last row instead) and turns bracketed paste off.
    fn finish(&mut self) {
        self.end_signals();
        let mut out = String::new();
        if let Some(row) = self.block_row {
            if self.opts.keep_frame {
                let _ = write!(out, "\x1b[{};1H\r\n", self.last_row);
            } else {
                let _ = write!(out, "\x1b[{row};1H\x1b[J");
            }
        }
        out.push_str("\x1b[?2004l");
        emit(&out);
    }

    /// The spinner title back to `fake`, a pending progress cleared.
    fn end_signals(&mut self) {
        if self.spinner.take().is_some() {
            set_title("fake");
        }
        if self.progress_clear.take().is_some() {
            emit("\x1b]9;4;0\x07");
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let (visible_at, fresh_at) = match self.stage {
            Stage::Resetting {
                visible_at,
                fresh_at,
            } => (visible_at, Some(fresh_at)),
            _ => (None, None),
        };
        // A dialog due during a reset waits for the fresh screen (`fresh_at`).
        let dialog_at = self.dialog_at.filter(|_| fresh_at.is_none());
        [
            visible_at,
            fresh_at,
            dialog_at,
            self.replies.front().copied(),
            self.delayed_submits.front().map(|(at, _)| *at),
            self.spinner.map(|(at, _)| at),
            self.progress_clear,
            self.background_title_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Runs every timer due at `now`.
    fn fire(&mut self, now: Instant) {
        if self.background_title_at.is_some_and(|at| at <= now) {
            set_title("background status");
            self.background_title_at = Some(now + SPINNER_EVERY);
        }
        if let Stage::Resetting {
            visible_at,
            fresh_at,
        } = self.stage
        {
            if visible_at.is_some_and(|at| at <= now) {
                self.stage = Stage::Resetting {
                    visible_at: None,
                    fresh_at,
                };
                self.draw();
            }
            if fresh_at <= now {
                self.transcript.clear();
                self.pastes = 0;
                self.replies_done = 0;
                self.stage = Stage::Chat;
                if self.opts.title {
                    set_title("fake");
                }
                self.draw();
            }
        }
        // Not during a reset: the dialog then comes with the fresh screen.
        let resetting = matches!(self.stage, Stage::Resetting { .. });
        if !resetting && self.dialog_at.is_some_and(|at| at <= now) {
            self.dialog_at = None;
            if matches!(self.stage, Stage::Blank | Stage::Chat) {
                self.stage = Stage::Dialog { yes: false };
                self.draw();
            }
        }
        while self
            .delayed_submits
            .front()
            .is_some_and(|(at, _)| *at <= now)
        {
            let (_, text) = self.delayed_submits.pop_front().expect("queued submit");
            self.show_submit(text, now);
        }
        while self.replies.front().is_some_and(|at| *at <= now) {
            self.replies.pop_front();
            self.replies_done += 1;
            self.transcript
                .push(format!("agent: ok #{}", self.replies_done));
            if self.replies.is_empty() && self.spinner.take().is_some() {
                set_title("fake");
            }
            self.draw();
        }
        if let Some((at, frame)) = self.spinner {
            if at <= now {
                set_title(&format!("{} fake", SPINNER[frame % SPINNER.len()]));
                self.spinner = Some((at + SPINNER_EVERY, frame + 1));
            }
        }
        if self.progress_clear.is_some_and(|at| at <= now) {
            self.progress_clear = None;
            emit("\x1b]9;4;0\x07");
        }
    }

    fn key(&mut self, key: Key, now: Instant) {
        if let Key::Interrupt = key {
            self.exit = Some(130);
            return;
        }
        match self.stage {
            Stage::Blank | Stage::Resetting { .. } => {}
            Stage::Dialog { yes } => match key {
                Key::Up | Key::Down => {
                    self.stage = Stage::Dialog {
                        yes: matches!(key, Key::Down),
                    };
                    self.draw();
                }
                Key::Enter if yes => {
                    self.stage = Stage::Chat;
                    self.draw();
                }
                Key::Enter => {
                    self.transcript.push("bye".into());
                    self.draw();
                    self.exit = Some(1);
                }
                _ => {}
            },
            Stage::Chat => match key {
                Key::Text(byte) => {
                    self.type_text(&[byte]);
                    self.draw();
                }
                Key::Backspace => {
                    // A text piece is never empty, so this removes a character or a chip.
                    if let Some(Piece::Text(bytes)) = self.composer.last_mut() {
                        // Drop one UTF-8 character: its continuation bytes, then its lead.
                        while bytes.pop().is_some_and(|b| b & 0xc0 == 0x80) {}
                        if bytes.is_empty() {
                            self.composer.pop();
                        }
                    } else {
                        self.composer.pop();
                    }
                    self.draw();
                }
                Key::Paste(bytes) => {
                    self.paste(&bytes, now);
                    self.draw();
                }
                Key::Enter => self.enter(now),
                _ => {}
            },
        }
    }

    fn paste(&mut self, bytes: &[u8], now: Instant) {
        self.last_paste_end = Some(now);
        let text = String::from_utf8_lossy(bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(&text)
            .split('\n')
            .collect();
        if lines.len() > 1 {
            self.pastes += 1;
            let chip = format!("[Pasted text #{} +{} lines]", self.pastes, lines.len());
            self.composer.push(Piece::Chip(chip));
        } else {
            let line: String = lines[0]
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            self.type_text(line.as_bytes());
        }
    }

    /// Appends to the last text piece, or starts one; nothing for no bytes (the composer
    /// never holds an empty piece).
    fn type_text(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match self.composer.last_mut() {
            Some(Piece::Text(text)) => text.extend_from_slice(bytes),
            _ => self.composer.push(Piece::Text(bytes.to_vec())),
        }
    }

    fn enter(&mut self, now: Instant) {
        if self.opts.record_enter {
            self.transcript.push("enter-received".into());
            self.draw();
        }
        if self.opts.ignore_enter {
            return;
        }
        let text = self.composer_text();
        if text.is_empty() {
            return;
        }
        let since_paste = self.last_paste_end.map(|at| now.duration_since(at));
        if self.opts.scene == "swallow" && since_paste.is_some_and(|d| d < self.opts.swallow) {
            self.transcript.push("enter-absorbed".into());
            self.draw();
            return;
        }
        if !self.opts.submit_display_delay.is_zero() && text.trim() != "/new" {
            self.delayed_submits
                .push_back((now + self.opts.submit_display_delay, text));
            return;
        }
        self.show_submit(text, now);
    }

    fn show_submit(&mut self, text: String, now: Instant) {
        self.composer.clear();
        if text.trim() == "/new" {
            self.delayed_submits.clear();
            self.replies.clear();
            self.end_signals();
            let silent = !self.opts.reset_silence.is_zero();
            let start = now + self.opts.reset_silence;
            self.stage = Stage::Resetting {
                visible_at: silent.then_some(start),
                fresh_at: start + self.opts.reset_delay,
            };
            if !silent {
                self.draw();
            }
            return;
        }
        self.transcript.push(format!("you: {text}"));
        if self.opts.scene == "exit-to-shell" {
            // Gone right after the submit shows: a controller verifying this Enter must
            // find the shell, never a fake about to exit.
            self.draw();
            self.exit = Some(0);
            return;
        }
        self.replies.push_back(now + REPLY_AFTER);
        if self.opts.title && self.spinner.is_none() {
            self.spinner = Some((now, 0));
        }
        if self.opts.scene == "progress" {
            emit("\x1b]9;4;3\x07");
            self.progress_clear = Some(now + PROGRESS_FOR);
        }
        self.draw();
    }

    fn composer_text(&self) -> String {
        let mut text = String::new();
        for piece in &self.composer {
            match piece {
                Piece::Text(bytes) => text.push_str(&String::from_utf8_lossy(bytes)),
                Piece::Chip(chip) => text.push_str(chip),
            }
        }
        text
    }

    /// Redraws the whole screen; the cursor ends after the composer text, else on the
    /// block's first row.
    fn draw(&mut self) {
        let composer = matches!(self.stage, Stage::Chat) && self.opts.scene != "silent";
        let block: Vec<String> = match self.stage {
            Stage::Blank => return,
            Stage::Dialog { yes } => vec![
                "fake trust dialog".into(),
                format!("{} 1. No, exit", pointer(!yes)),
                format!("{} 2. Yes, trust this folder", pointer(yes)),
                "enter to confirm".into(),
            ],
            Stage::Chat if composer => vec![
                format!("> {}", self.composer_text()),
                "fake-agent ready · esc to interrupt".into(),
            ],
            Stage::Chat => Vec::new(),
            Stage::Resetting { .. } => vec!["resetting conversation".into()],
        };
        let (rows, cols) = tty::size();
        let (rows, cols) = (usize::from(rows.max(2)), usize::from(cols.max(2)));
        // The banner, as much transcript as fits, and at least one row for the block.
        let room = rows.saturating_sub(1 + block.len().max(1));
        let skip = self.transcript.len().saturating_sub(room);
        let mut lines = vec![format!("fake-agent v1 ({})", self.opts.scene)];
        lines.extend(self.transcript[skip..].iter().cloned());
        let block_row = lines.len() + 1;
        let cursor_col = match block.first() {
            Some(row) if composer => (row.chars().count() + 1).min(cols),
            _ => 1,
        };
        lines.extend(block);

        let mut out = String::new();
        if self.opts.sync {
            out.push_str("\x1b[?2026h");
        }
        for (i, line) in lines.iter().enumerate() {
            let line: String = line.chars().take(cols).collect();
            let _ = write!(out, "\x1b[{};1H\x1b[2K{line}", i + 1);
        }
        let _ = write!(out, "\x1b[J\x1b[{block_row};{cursor_col}H");
        if self.opts.sync {
            out.push_str("\x1b[?2026l");
        }
        emit(&out);
        self.block_row = Some(block_row);
        self.last_row = lines.len();
    }
}

fn pointer(selected: bool) -> char {
    if selected { '❯' } else { ' ' }
}

fn set_title(title: &str) {
    emit(&format!("\x1b]0;{title}\x07"));
}
