//! Bounded pipe pumps and ordered terminal events. Payload logging is explicitly opt-in.

use std::io::{BufReader, Write};
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select_biased};

use super::framing::{ReadError, discard_line, read_frame};
use super::input::{Input, WriteCompletion};
use super::stderr_ring::StderrRing;
use super::{
    RpcEvent, RpcLimits, RpcStream, RpcTransportError, RpcUtf8Mode, RpcWriteFailure, frame_line,
    lock, strip_line_ending,
};
use crate::error::{Error, Result};
use crate::pty::SessionId;

const TRACE_LINE_MAX: usize = 400;
const STDERR_DRAIN_WAIT: Duration = Duration::from_secs(2);

/// `line` cut to `TRACE_LINE_MAX` characters. Only used after explicit trace opt-in.
pub(super) fn trace_snip(line: &str) -> String {
    let count = line.chars().count();
    if count <= TRACE_LINE_MAX {
        return line.to_string();
    }
    let head: String = line.chars().take(TRACE_LINE_MAX).collect();
    format!("{head}… (+{} chars)", count - TRACE_LINE_MAX)
}

/// The sending side of the event channel. `close` never waits for a consumer or for the
/// publication lock: it releases any send already blocked on the bounded channel.
pub(super) struct Publisher {
    stopped: AtomicBool,
    input: Input,
    tx: Mutex<Option<Sender<RpcEvent>>>,
    closed_tx: Mutex<Option<Sender<()>>>,
    closed_rx: Receiver<()>,
    /// stdout, stderr and writer failures can race. Exactly one terminal event wins,
    /// and no normal event can overtake or follow it.
    publishing: Mutex<()>,
}

impl Publisher {
    pub(super) fn new(capacity: usize, input: Input) -> (Arc<Self>, Receiver<RpcEvent>) {
        let (tx, rx) = bounded(capacity);
        let (closed_tx, closed_rx) = bounded(0);
        let publisher = Self {
            stopped: AtomicBool::new(false),
            input,
            tx: Mutex::new(Some(tx)),
            closed_tx: Mutex::new(Some(closed_tx)),
            closed_rx,
            publishing: Mutex::new(()),
        };
        (Arc::new(publisher), rx)
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
        self.input.stop(RpcWriteFailure::Closed);
        self.stopped.store(true, Ordering::Release);
        drop(lock(&self.closed_tx).take());
        drop(lock(&self.tx).take());
    }

    fn send(&self, event: RpcEvent) {
        let Some(tx) = lock(&self.tx).clone() else {
            return;
        };
        select_biased! {
            recv(self.closed_rx) -> _ => {}
            send(tx, event) -> _ => {}
        }
    }

    fn publish(&self, event: RpcEvent) {
        let _publishing = lock(&self.publishing);
        if !self.stopped() {
            self.send(event);
        }
    }

    fn end(&self, event: RpcEvent) {
        // Settle input before waiting behind a full event receiver.
        self.input.stop(RpcWriteFailure::Closed);
        let _publishing = lock(&self.publishing);
        if !self.stopped() {
            // Refuse new stdin before the terminal event becomes observable. The stop
            // channel stays open until this final send finishes (or teardown cancels it).
            self.stopped.store(true, Ordering::Release);
            self.send(event);
            self.close();
        }
    }

    fn wait(&self, done: &Receiver<()>, timeout: Duration) {
        select_biased! {
            recv(self.closed_rx) -> _ => {}
            recv(done) -> _ => {}
            default(timeout) => {}
        }
    }
}

#[derive(Clone)]
pub(super) struct Config {
    pub(super) id: SessionId,
    pub(super) tag: String,
    pub(super) publisher: Arc<Publisher>,
    pub(super) trace: bool,
    pub(super) trace_stderr: bool,
    pub(super) limits: RpcLimits,
    pub(super) stdout_utf8: RpcUtf8Mode,
}

impl Config {
    fn fail(&self, error: RpcTransportError) {
        log::warn!("{} transport failed: {error}", self.tag);
        self.publisher.end(RpcEvent::Error { id: self.id, error });
    }

    fn read_failed(&self, stream: RpcStream, limit: usize, error: ReadError) {
        self.fail(match error {
            ReadError::TooLong => RpcTransportError::LineTooLong { stream, limit },
            ReadError::Io(kind) => RpcTransportError::Io { stream, kind },
        });
    }
}

/// Keeps a bounded stderr tail for the exit event. Logging requires stderr trace opt-in.
pub(super) fn spawn_stderr(
    cfg: Config,
    stderr: ChildStderr,
    ring: StderrRing,
    done: Sender<()>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-rpc-stderr-{}", cfg.id))
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            while !cfg.publisher.stopped() {
                match read_frame(&mut reader, &mut buf, cfg.limits.max_stderr_line_bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(ReadError::TooLong) => {
                        // Never retain or trace any prefix of a discarded diagnostic.
                        // Publish its marker before draining: EOF may never arrive.
                        buf.clear();
                        ring.push("[agent-link: oversized stderr line dropped]".into());
                        match discard_line(&mut reader, || cfg.publisher.stopped()) {
                            Ok(true) => continue,
                            Ok(false) => break,
                            Err(err) => {
                                cfg.read_failed(
                                    RpcStream::Stderr,
                                    cfg.limits.max_stderr_line_bytes,
                                    err,
                                );
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        cfg.read_failed(RpcStream::Stderr, cfg.limits.max_stderr_line_bytes, err);
                        break;
                    }
                }
                let line = frame_line(&buf);
                if line.is_empty() {
                    continue;
                }
                if cfg.trace_stderr {
                    log::warn!("{} stderr: {}", cfg.tag, trace_snip(&line));
                }
                ring.push(line);
            }
            log::info!("{} stderr reader ended", cfg.tag);
            drop(done);
        })
        .map_err(|e| Error::Spawn(format!("spawn rpc stderr thread: {e}")))
}

/// Delivers complete bounded stdout frames. Read errors and oversize frames are errors,
/// never successful EOF, and never partially delivered as protocol lines.
pub(super) fn spawn_reader(
    cfg: Config,
    stdout: ChildStdout,
    ring: StderrRing,
    stderr_done: Receiver<()>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-rpc-reader-{}", cfg.id))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            let mut seq = 0u64;
            while !cfg.publisher.stopped() {
                match read_frame(&mut reader, &mut buf, cfg.limits.max_stdout_line_bytes) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) => {
                        cfg.read_failed(RpcStream::Stdout, cfg.limits.max_stdout_line_bytes, err);
                        return;
                    }
                }
                if cfg.publisher.stopped() {
                    return;
                }
                let raw_len = buf.len();
                let line = match cfg.stdout_utf8 {
                    RpcUtf8Mode::Lossy => frame_line(&buf),
                    RpcUtf8Mode::Strict => match String::from_utf8(std::mem::take(&mut buf)) {
                        Ok(line) => strip_line_ending(line),
                        Err(_) => {
                            cfg.fail(RpcTransportError::InvalidUtf8);
                            return;
                        }
                    },
                };
                if cfg.trace {
                    log::info!("{} < seq={seq} {}", cfg.tag, trace_snip(&line));
                } else if seq == 0 {
                    log::info!("{} first stdout line received ({} bytes)", cfg.tag, raw_len);
                }
                cfg.publisher.publish(RpcEvent::Line {
                    id: cfg.id,
                    seq,
                    line,
                });
                seq = seq.wrapping_add(1);
            }
            finish_stdout(&cfg, seq, &ring, &stderr_done);
        })
        .map_err(|e| Error::Spawn(format!("spawn rpc reader thread: {e}")))
}
// Input stop precedes stderr draining, so late stdin failures cannot replace this exit.
fn finish_stdout(cfg: &Config, seq: u64, ring: &StderrRing, stderr_done: &Receiver<()>) {
    cfg.publisher.input.stop(RpcWriteFailure::Closed);
    if !cfg.publisher.stopped() {
        cfg.publisher.wait(stderr_done, STDERR_DRAIN_WAIT);
    }
    if cfg.publisher.stopped() {
        log::info!(
            "{} stdout reader ended after {seq} line(s) — transport stopped",
            cfg.tag
        );
    } else {
        log::info!("{} stdout EOF after {seq} line(s)", cfg.tag);
        cfg.publisher.end(RpcEvent::Exit {
            id: cfg.id,
            stderr_tail: ring.tail(),
        });
    }
}

pub(super) fn spawn_deadlines(
    id: SessionId,
    tag: String,
    publisher: Arc<Publisher>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-rpc-deadlines-{id}"))
        .spawn(move || {
            if publisher.input.monitor() {
                let error = RpcTransportError::WriteDeadlineExceeded;
                log::warn!("{tag} transport failed: {error}");
                publisher.end(RpcEvent::Error { id, error });
            }
        })
        .map_err(|e| Error::Spawn(format!("spawn rpc deadline thread: {e}")))
}

/// Generic writer used by the pipe pump and deterministic partial/flush failure tests.
fn write_line(stdin: &mut impl Write, text: &str) -> std::io::Result<()> {
    stdin
        .write_all(text.as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .and_then(|()| stdin.flush())
}

fn run_writer(cfg: Config, mut stdin: impl Write) {
    let mut written = 0u64;
    while let Some(text) = cfg.publisher.input.next() {
        let result = write_line(&mut stdin, &text);
        let completed_at = Instant::now();
        let size = text.len() + 1;
        let trace = cfg.trace.then(|| trace_snip(&text));
        match cfg.publisher.input.finish(text, result, completed_at) {
            WriteCompletion::Written => {}
            WriteCompletion::Stopped => break,
            WriteCompletion::Failed(error) => {
                // Input receipts/admission are settled. Close the pipe before potentially
                // blocking on bounded event publication; teardown can cancel that send.
                drop(stdin);
                if let Some(trace) = trace {
                    log::info!("{} failed stdin payload: {trace}", cfg.tag);
                }
                if let RpcTransportError::Io {
                    stream: RpcStream::Stdin,
                    kind,
                } = error
                {
                    log::warn!("{} stdin failed: {kind:?}; output remains open", cfg.tag);
                    cfg.publisher
                        .publish(RpcEvent::StdinError { id: cfg.id, kind });
                } else {
                    cfg.fail(error);
                }
                log::info!("{} writer thread ending after {written} line(s)", cfg.tag);
                return;
            }
        }
        written += 1;
        if let Some(trace) = trace {
            log::info!("{} > #{written} flushed to stdin: {trace}", cfg.tag);
        } else if written == 1 {
            log::info!("{} first stdin line flushed ({size} bytes)", cfg.tag);
        }
    }
    drop(stdin);
    log::info!("{} writer thread ending after {written} line(s)", cfg.tag);
}

pub(super) fn spawn_writer(cfg: Config, stdin: ChildStdin) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("agent-link-rpc-writer-{}", cfg.id))
        .spawn(move || run_writer(cfg, stdin))
        .map_err(|e| Error::Spawn(format!("spawn rpc writer thread: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_failure_before_stdout_eof_preserves_output_and_exit() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                unreachable!()
            }
        }
        let input = Input::new(RpcLimits::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        let active = input.send_tracked("active", deadline, || Ok(())).unwrap();
        let queued = input.send_tracked("queued", deadline, || Ok(())).unwrap();
        let (publisher, events) = Publisher::new(4, input.clone());
        let cfg = Config {
            id: SessionId(1),
            tag: "test".into(),
            publisher,
            trace: false,
            trace_stderr: false,
            limits: RpcLimits::default(),
            stdout_utf8: RpcUtf8Mode::Strict,
        };
        run_writer(cfg.clone(), Broken);
        assert_eq!(
            active.wait_until(deadline),
            Some(super::super::RpcWriteOutcome::PossiblyWritten(
                RpcWriteFailure::Io(std::io::ErrorKind::BrokenPipe)
            ))
        );
        assert_eq!(
            queued.wait_until(deadline),
            Some(super::super::RpcWriteOutcome::NotWritten(
                RpcWriteFailure::Closed
            ))
        );
        assert!(matches!(input.send("later"), Err(Error::Closed)));
        cfg.publisher.publish(RpcEvent::Line {
            id: cfg.id,
            seq: 0,
            line: "final stdout".into(),
        });
        let ring = StderrRing::default();
        ring.push("final stderr".into());
        let (done, done_rx) = bounded(0);
        drop(done);
        finish_stdout(&cfg, 1, &ring, &done_rx);
        assert_eq!(
            events.recv_deadline(deadline).unwrap(),
            RpcEvent::StdinError {
                id: cfg.id,
                kind: std::io::ErrorKind::BrokenPipe
            }
        );
        assert_eq!(
            events.recv_deadline(deadline).unwrap(),
            RpcEvent::Line {
                id: cfg.id,
                seq: 0,
                line: "final stdout".into()
            }
        );
        assert_eq!(
            events.recv_deadline(deadline).unwrap(),
            RpcEvent::Exit {
                id: cfg.id,
                stderr_tail: "final stderr".into()
            }
        );
        assert!(events.recv_deadline(deadline).is_err());
    }

    #[test]
    fn backpressured_stdin_error_settles_receipts_and_teardown_releases_writer() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                unreachable!()
            }
        }
        let input = Input::new(RpcLimits::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        let active = input.send_tracked("active", deadline, || Ok(())).unwrap();
        let queued = input.send_tracked("queued", deadline, || Ok(())).unwrap();
        let (publisher, events) = Publisher::new(1, input.clone());
        publisher.publish(RpcEvent::Line {
            id: SessionId(1),
            seq: 0,
            line: "full".into(),
        });
        let cfg = Config {
            id: SessionId(1),
            tag: "test".into(),
            publisher: publisher.clone(),
            trace: false,
            trace_stderr: false,
            limits: RpcLimits::default(),
            stdout_utf8: RpcUtf8Mode::Strict,
        };
        let (done_tx, done) = bounded(1);
        let writer = std::thread::spawn(move || {
            run_writer(cfg, Broken);
            let _ = done_tx.send(());
        });
        let active_outcome = active.wait_until(deadline);
        let queued_outcome = queued.wait_until(deadline);
        let premature_completion = done.try_recv();
        let closed = input.send("late");
        // Always release the writer before assertions, even if settlement failed.
        publisher.close();
        done.recv_deadline(deadline).unwrap();
        writer.join().unwrap();
        assert_eq!(
            active_outcome,
            Some(super::super::RpcWriteOutcome::PossiblyWritten(
                RpcWriteFailure::Io(std::io::ErrorKind::BrokenPipe)
            ))
        );
        assert_eq!(
            queued_outcome,
            Some(super::super::RpcWriteOutcome::NotWritten(
                RpcWriteFailure::Closed
            ))
        );
        assert!(
            premature_completion.is_err(),
            "full receiver blocks notification only"
        );
        assert!(matches!(closed, Err(Error::Closed)));
        assert!(matches!(
            events.recv_deadline(deadline),
            Ok(RpcEvent::Line { .. })
        ));
        assert!(events.recv_deadline(deadline).is_err());
    }

    #[test]
    fn stdin_failure_after_stdout_eof_preserves_exit_and_final_stderr_tail() {
        use super::super::RpcWriteOutcome;
        use std::io;
        use std::time::Instant;

        struct PendingFailure {
            started: Sender<()>,
            release: Receiver<()>,
        }
        impl Write for PendingFailure {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.started.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                unreachable!("write fails")
            }
        }
        let limit = Instant::now() + Duration::from_secs(5);
        let input = Input::new(RpcLimits::default());
        let receipt = input.send_tracked("active", limit, || Ok(())).unwrap();
        let queued = input.send_tracked("queued", limit, || Ok(())).unwrap();
        let (publisher, events) = Publisher::new(4, input);
        let config = Config {
            id: SessionId(1),
            tag: "test".into(),
            publisher,
            trace: false,
            trace_stderr: false,
            limits: RpcLimits::default(),
            stdout_utf8: RpcUtf8Mode::Strict,
        };
        let (started_tx, started) = bounded(1);
        let (release, release_rx) = bounded(1);
        let (writer_done_tx, writer_done) = bounded(1);
        let writer_cfg = config.clone();
        let writer = std::thread::spawn(move || {
            run_writer(
                writer_cfg,
                PendingFailure {
                    started: started_tx,
                    release: release_rx,
                },
            );
            writer_done_tx.send(()).unwrap();
        });
        let started = started.recv_deadline(limit);
        let ring = StderrRing::default();
        ring.push("early stderr".into());
        let reader_ring = ring.clone();
        let (stderr_done, stderr_done_rx) = bounded(0);
        let reader =
            std::thread::spawn(move || finish_stdout(&config, 0, &reader_ring, &stderr_done_rx));
        // Receipt settlement proves EOF has stopped input, while stderr remains open.
        let outcome = receipt.wait_until(limit);
        let queued_outcome = queued.wait_until(limit);
        let _ = release.send(());
        let writer_done = writer_done.recv_deadline(limit);
        ring.push("final stderr".into());
        drop(stderr_done);
        writer.join().unwrap();
        reader.join().unwrap();
        started.unwrap();
        writer_done.unwrap();
        assert_eq!(
            outcome,
            Some(RpcWriteOutcome::PossiblyWritten(RpcWriteFailure::Closed))
        );
        assert_eq!(
            queued_outcome,
            Some(RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed))
        );
        assert_eq!(
            events.recv_deadline(limit).unwrap(),
            RpcEvent::Exit {
                id: SessionId(1),
                stderr_tail: "early stderr\nfinal stderr".into(),
            }
        );
        assert!(
            events.recv_deadline(limit).is_err(),
            "no late error after Exit"
        );
    }

    #[test]
    fn partial_write_and_flush_failure_invalidate_before_any_later_frame() {
        use super::super::{RpcWriteOutcome, RpcWriteStatus};
        use std::io;
        use std::time::Instant;

        struct FailWriter {
            bytes: Arc<Mutex<Vec<u8>>>,
            fail_at: usize,
            flush_fails: bool,
        }
        impl Write for FailWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let mut emitted = lock(&self.bytes);
                if emitted.len() >= self.fail_at {
                    return Err(io::Error::other("PRIVATE"));
                }
                let count = bytes.len().min(self.fail_at - emitted.len());
                emitted.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.flush_fails {
                    Err(io::Error::other("PRIVATE"))
                } else {
                    Ok(())
                }
            }
        }
        for (fail_at, flush_fails, expected) in
            [(2, false, &b"fi"[..]), (usize::MAX, true, &b"first\n"[..])]
        {
            let input = Input::new(RpcLimits::default());
            let deadline = Instant::now() + Duration::from_secs(10);
            let first = input.send_tracked("first", deadline, || Ok(())).unwrap();
            let later = input.send_tracked("later", deadline, || Ok(())).unwrap();
            let (publisher, events) = Publisher::new(4, input.clone());
            let bytes = Arc::new(Mutex::new(Vec::new()));
            run_writer(
                Config {
                    id: SessionId(1),
                    tag: "test".into(),
                    publisher: publisher.clone(),
                    trace: false,
                    trace_stderr: false,
                    limits: RpcLimits::default(),
                    stdout_utf8: RpcUtf8Mode::Strict,
                },
                FailWriter {
                    bytes: bytes.clone(),
                    fail_at,
                    flush_fails,
                },
            );
            let reason = RpcWriteFailure::Io(io::ErrorKind::Other);
            assert_eq!(
                first.status(),
                RpcWriteStatus::Finished(RpcWriteOutcome::PossiblyWritten(reason))
            );
            assert_eq!(
                later.wait(),
                RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed)
            );
            assert_eq!(*lock(&bytes), expected);
            assert!(matches!(input.send("next"), Err(Error::Closed)));
            assert!(matches!(
                events.recv().unwrap(),
                RpcEvent::StdinError {
                    kind: io::ErrorKind::Other,
                    ..
                }
            ));
            assert!(!publisher.stopped());
            publisher.close();
            assert!(events.recv().is_err());
        }
    }

    #[test]
    fn deadline_during_flush_stays_uncertain_when_the_write_later_succeeds() {
        use super::super::RpcWriteOutcome;
        use std::io;
        use std::time::Instant;

        struct BlockFlush {
            bytes: Arc<Mutex<Vec<u8>>>,
            entered: Sender<()>,
            release: Receiver<()>,
        }
        impl Write for BlockFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                lock(&self.bytes).extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            }
        }
        let input = Input::new(RpcLimits::default());
        let receipt = input
            .send_tracked("first", Instant::now() + Duration::from_secs(1), || Ok(()))
            .unwrap();
        let later = input
            .send_tracked("later", Instant::now() + Duration::from_secs(10), || Ok(()))
            .unwrap();
        let (publisher, events) = Publisher::new(4, input.clone());
        let config = Config {
            id: SessionId(1),
            tag: "test".into(),
            publisher,
            trace: false,
            trace_stderr: false,
            limits: RpcLimits::default(),
            stdout_utf8: RpcUtf8Mode::Strict,
        };
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered, started) = bounded(1);
        let (release, released) = bounded(1);
        let pipe = BlockFlush {
            bytes: bytes.clone(),
            entered,
            release: released,
        };
        let deadlines =
            spawn_deadlines(config.id, config.tag.clone(), config.publisher.clone()).unwrap();
        let writer = std::thread::spawn(move || run_writer(config, pipe));
        let limit = Instant::now() + Duration::from_secs(5);
        let started = started.recv_deadline(limit);
        let outcome = receipt.wait_until(limit);
        let queued = later.wait_until(limit);
        // Always release the worker before assertions, including unexpected outcomes.
        let _ = release.send(());
        writer.join().unwrap();
        deadlines.join().unwrap();
        started.unwrap();
        assert_eq!(
            outcome,
            Some(RpcWriteOutcome::PossiblyWritten(
                RpcWriteFailure::DeadlineExceeded
            ))
        );
        assert_eq!(
            receipt.wait(),
            outcome.unwrap(),
            "late flush cannot overwrite uncertainty"
        );
        assert_eq!(
            queued,
            Some(RpcWriteOutcome::NotWritten(RpcWriteFailure::Closed))
        );
        assert_eq!(*lock(&bytes), b"first\n");
        assert!(matches!(
            events.recv_deadline(limit).unwrap(),
            RpcEvent::Error {
                error: RpcTransportError::WriteDeadlineExceeded,
                ..
            }
        ));
        assert!(events.recv_deadline(limit).is_err());
    }

    fn failure() -> RpcEvent {
        RpcEvent::Error {
            id: SessionId(1),
            error: RpcTransportError::LineTooLong {
                stream: RpcStream::Stdout,
                limit: 8,
            },
        }
    }

    #[test]
    fn one_terminal_event_wins_and_no_later_event_is_delivered() {
        let (publisher, events) = Publisher::new(4, Input::new(RpcLimits::default()));
        let other = Arc::clone(&publisher);
        let first = std::thread::spawn(move || other.end(failure()));
        publisher.end(RpcEvent::Exit {
            id: SessionId(1),
            stderr_tail: String::new(),
        });
        first.join().unwrap();
        publisher.publish(RpcEvent::Line {
            id: SessionId(1),
            seq: 0,
            line: "late".into(),
        });
        assert!(matches!(
            events.recv().unwrap(),
            RpcEvent::Error { .. } | RpcEvent::Exit { .. }
        ));
        assert!(events.recv().is_err(), "exactly one terminal event");
    }

    #[test]
    fn teardown_releases_a_terminal_error_blocked_by_backpressure() {
        let (publisher, events) = Publisher::new(1, Input::new(RpcLimits::default()));
        publisher.publish(RpcEvent::Line {
            id: SessionId(1),
            seq: 0,
            line: "first".into(),
        });
        let other = Arc::clone(&publisher);
        let (done_tx, done_rx) = bounded(0);
        let worker = std::thread::spawn(move || {
            other.end(failure());
            let _ = done_tx.send(());
        });
        publisher.close();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(matches!(events.recv().unwrap(), RpcEvent::Line { .. }));
        assert!(events.recv().is_err());
    }
}
