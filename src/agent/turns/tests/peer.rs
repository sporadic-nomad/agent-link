use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use tempfile::{TempDir, tempdir_in};

use super::super::*;
use crate::agent::{
    AgentProfile, AgentSpec, AgentTerminal, Dialog, Permissions, ShellKind, Timing, TrustPolicy,
};
use crate::shell::ShellSpec;
use crate::{Event, PtySize, SpawnMode, SpawnSpec};

pub(in crate::agent) const WAIT: Duration = Duration::from_secs(6);
const SIZE: PtySize = PtySize {
    rows: 24,
    cols: 100,
};

// The worker ceiling is process-wide. Hold the library's PTY fixture lock across
// each scenario, including those deliberately running two or more terminals at once.
pub(in crate::agent) fn project() -> (std::sync::MutexGuard<'static, ()>, TempDir) {
    (crate::pty::test_spawn_lock(), tempfile::tempdir().unwrap())
}

pub(in crate::agent) fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

pub(in crate::agent) struct Pause {
    entered: Sender<()>,
    release: Receiver<()>,
}

pub(in crate::agent) struct Barrier {
    entered: Receiver<()>,
    release: Sender<()>,
}

impl Barrier {
    pub(in crate::agent) fn new() -> (Self, Pause) {
        let (enter, entered) = bounded(1);
        let (release, resume) = bounded(1);
        (
            Self { entered, release },
            Pause {
                entered: enter,
                release: resume,
            },
        )
    }

    pub(in crate::agent) fn wait(&self) {
        self.entered
            .recv_timeout(WAIT)
            .expect("worker reached barrier");
    }

    pub(in crate::agent) fn release(self) {
        let _ = self.release.try_send(());
    }
}

impl Pause {
    pub(in crate::agent) fn park(&self) {
        let _ = self.entered.try_send(());
        // Dropping the fixture's sender during unwinding also releases the worker.
        let _ = self.release.recv_timeout(WAIT * 2);
    }
}

struct Profile {
    id: String,
    monitor: Option<Pause>,
}

impl AgentProfile for Profile {
    fn id(&self) -> &str {
        "turn-pilot-peer"
    }
    fn launch(&self, _: &Permissions, _: &TrustPolicy, _: &Path, _: ShellKind) -> LaunchPlan {
        let mut plan = LaunchPlan::new("/bin/sh");
        plan.args = vec!["peer.sh".into(), "--session-id".into(), self.id.clone()];
        plan.env.push(("TURN_ROOT".into(), "records".into()));
        plan
    }
    fn dialog(&self, screen: &str) -> Option<Dialog> {
        if screen.contains("monitor barrier") {
            if let Some(pause) = &self.monitor {
                pause.park();
            }
        }
        None
    }
    fn input_ready(&self, screen: &str) -> bool {
        screen.lines().any(|line| line.starts_with(">"))
    }
    fn reset_command(&self) -> Option<&'static str> {
        Some("/clear")
    }
    fn reset_rejected(&self, screen: &str) -> bool {
        screen.contains("reset refused")
    }
    fn question_dialog(&self, screen: &str) -> bool {
        screen.contains("choose an option")
    }
}

pub(super) fn capture(spec: &mut SpawnSpec, plan: &LaunchPlan) -> crate::Result<Claim> {
    // This fixture has a deliberately narrow launch grammar.
    let (flag, id) = match &spec.mode {
        SpawnMode::Direct { args, .. } if args.len() == 3 => (args[1].to_str(), args[2].to_str()),
        SpawnMode::Shell(_) if plan.args.len() == 3 => {
            (Some(plan.args[1].as_str()), Some(plan.args[2].as_str()))
        }
        _ => (None, None),
    };
    let id = id
        .filter(|_| flag == Some("--session-id"))
        .and_then(|id| ProviderId::new(id).ok())
        .ok_or_else(|| crate::Error::InvalidInput("missing synthetic launch id".into()))?;
    let [root] = spec.freeze_launch_env(["TURN_ROOT"]);
    let root = root.ok_or_else(|| crate::Error::InvalidInput("missing synthetic root".into()))?;
    Claim::new(id, &spec.cwd, Path::new(&root))
}

pub(super) struct Peer {
    pub term: AgentTerminal,
    pub events: Receiver<Event>,
    pub file: PathBuf,
    pub witness: PathBuf,
    _writer: TempDir,
}

impl Peer {
    pub(super) fn start(project: &Path, id: &str, shell: bool, monitor: Option<Pause>) -> Self {
        Self::start_with_capture(project, id, shell, monitor, Arc::new(capture))
    }

    pub(super) fn start_with_capture(
        project: &Path,
        id: &str,
        shell: bool,
        monitor: Option<Pause>,
        capture: CaptureLaunch,
    ) -> Self {
        fs::write(project.join("peer.sh"), include_bytes!("peer.sh")).unwrap();
        let root = project.join("records");
        fs::create_dir_all(&root).unwrap();
        let file = root.join(format!("{id}.jsonl"));
        File::create_new(&file).unwrap();
        let writer = tempdir_in(project).unwrap();
        let witness = writer.path().join("current");
        let mut spec = if shell {
            let mut spec = SpawnSpec::shell(project, SIZE);
            spec.mode = SpawnMode::Shell(ShellSpec {
                program: Some("/bin/sh".into()),
                login: false,
            });
            spec
        } else {
            SpawnSpec::direct("/bin/sh", ["peer.sh", "--session-id", id], project, SIZE)
        };
        spec.env.inherit = false;
        spec.env
            .set("HOME", writer.path())
            .set("PATH", "/usr/bin:/bin")
            .set("WRITER", &witness)
            .set("TURN_ROOT", "wrong-before-merge");
        let agent = AgentSpec {
            turn_launch: Some(capture),
            timing: Timing {
                startup_max: Duration::from_millis(300),
                pre_paste_quiet: Duration::from_millis(20),
                paste_echo_quiet: Duration::from_millis(20),
                submit_verify_quiet: Duration::from_millis(30),
                submit_verify_poll: Duration::from_millis(10),
                ..Timing::default()
            },
            ..AgentSpec::new(Arc::new(Profile {
                id: id.into(),
                monitor,
            }))
        };
        let (term, events) = AgentTerminal::spawn(spec, agent).unwrap();
        if shell {
            term.launch().unwrap();
        }
        until("peer acknowledged its launch", || {
            witness.exists() && term.state() == AgentState::Ready
        });
        Self {
            term,
            events,
            file,
            witness,
            _writer: writer,
        }
    }

    pub(super) fn source(&self) -> Writer {
        self.source_with_witness(&self.witness)
    }

    pub(super) fn source_with_witness(&self, path: &Path) -> Writer {
        Writer {
            witness: Arc::new(WriterWitness {
                session: self.term.terminal().id(),
                path: path.to_path_buf(),
            }),
            pause: None,
        }
    }

    pub(super) fn attach(&self) -> TurnReceiver {
        self.attach_with(CEILINGS)
    }

    pub(super) fn attach_with(&self, limits: Limits) -> TurnReceiver {
        let mut receiver = None;
        until("previous worker released its permit", || {
            match self.term.attach_turns(
                self.source(),
                JsonParser::default(),
                Options {
                    limits,
                    ..Options::default()
                },
            ) {
                Ok(rx) => {
                    receiver = Some(rx);
                    true
                }
                Err(AttachError::Busy) => false,
                Err(err) => panic!("attach refused: {err:?}"),
            }
        });
        receiver.unwrap()
    }

    pub(super) fn input(&self, line: &str) {
        self.term
            .terminal()
            .write(format!("{line}\n").as_bytes())
            .unwrap();
    }
}

pub(super) struct Writer {
    witness: Arc<WriterWitness>,
    pub pause: Option<Pause>,
}

impl Source for Writer {
    fn witness(&self) -> Arc<dyn Witness> {
        self.witness.clone()
    }

    fn current(&mut self, launch: &Launch) -> Result<Binding, Gap> {
        if let Some(pause) = self.pause.take() {
            pause.park();
        }
        self.witness.current(launch)
    }
}

struct WriterWitness {
    session: SessionId,
    path: PathBuf,
}

impl Witness for WriterWitness {
    fn current(&self, launch: &Launch) -> Result<Binding, Gap> {
        if self.session != launch.session {
            return Err(Gap::Uncorroborated);
        }
        let mut body = String::new();
        File::open(&self.path)?
            .take(4097)
            .read_to_string(&mut body)?;
        let mut fields = body.lines();
        let mut field = || fields.next().ok_or(Gap::Uncorroborated);
        let initial = ProviderId::new(field()?)?;
        let id = field()?;
        let current = ProviderId::new(id)?;
        let revision = field()?.parse().map_err(|_| Gap::Uncorroborated)?;
        let root = Path::new(field()?);
        let cwd = Path::new(field()?);
        if body.len() > 4096
            || fields.next().is_some()
            || root != launch.claim.root.path.canonicalize()?
            || cwd != launch.claim.cwd.canonicalize()?
        {
            return Err(Gap::Uncorroborated);
        }
        let relative = format!("{}.jsonl", current.as_str()).into();
        Ok(Binding {
            initial,
            current,
            revision,
            relative,
        })
    }
}

#[derive(Default)]
pub(super) struct JsonParser {
    pub pause: Option<Pause>,
    pub reset: Option<Sender<()>>,
}

impl Parser for JsonParser {
    fn record(&mut self, _: &Binding, line: &[u8], limit: usize) -> Result<Record, Gap> {
        use serde_json::Value;
        let value: Value = serde_json::from_slice(line).map_err(|_| Gap::Format)?;
        let record = match value {
            Value::String(s) if s == "user" => Record::User,
            Value::String(s) if s == "ignore" => Record::Ignore,
            Value::Object(object) => match object.get("end") {
                Some(Value::Null) => Record::End(None),
                Some(Value::String(s)) => {
                    let mut reply = Reply::default();
                    reply.push(s, limit);
                    Record::End(Some(reply))
                }
                Some(Value::Array(blocks)) if blocks.len() <= 16 => {
                    let mut reply = Reply::default();
                    for block in blocks {
                        reply.push(block.as_str().ok_or(Gap::Format)?, limit);
                    }
                    Record::End(Some(reply))
                }
                _ => return Err(Gap::Format),
            },
            _ => return Err(Gap::Format),
        };
        if matches!(&record, Record::End(Some(r)) if r.text == "pause") {
            if let Some(pause) = self.pause.take() {
                pause.park();
            }
        }
        Ok(record)
    }
    fn reset(&mut self) {
        if let Some(reset) = self.reset.take() {
            let _ = reset.try_send(());
        }
    }
}

pub(in crate::agent) fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

pub(super) fn turn(path: &Path, text: &str) {
    append(
        path,
        format!("\"user\"\n{}\n", serde_json::json!({"end": text})).as_bytes(),
    );
}

pub(in crate::agent) fn observed(rx: &TurnReceiver, path: &Path) {
    let eof = path.metadata().unwrap().len();
    until("validated observer offset", || match rx.status() {
        Status::Active { offset, .. } => offset == eof,
        Status::Waiting => false,
        other => panic!("unexpected observation status {other:?}"),
    });
}

pub(in crate::agent) fn gap(rx: &TurnReceiver, expected: Gap) {
    until("sticky observation gap", || {
        rx.status() == Status::Gap(expected)
    });
    assert!(rx.try_recv().is_none());
}

pub(in crate::agent) fn clock() -> (Sender<Instant>, Options) {
    let (tx, ticks) = bounded(0);
    (
        tx,
        Options {
            ticks,
            limits: CEILINGS,
        },
    )
}

pub(in crate::agent) fn tick(clock: &Sender<Instant>) {
    clock
        .send_timeout(Instant::now(), WAIT)
        .expect("reader awaiting next tick");
}

/// Let the direct child exit while its final output is blocked on a full subscriber.
/// The caller can now exercise a queued pull before EOF or Exit reaches the monitor.
pub(in crate::agent) fn with_undelivered_exit(
    terminal: &crate::Terminal,
    events: Receiver<Event>,
    check: impl FnOnce(),
) {
    let full = terminal.subscribe();
    let handle = terminal.handle();
    let id = terminal.id();
    let capacity = full.capacity().unwrap();
    // Only one producer fills the queue; wake the peer after the last available slot.
    let filler = std::thread::spawn(move || {
        for _ in 0..capacity {
            handle.publish(Event::Frame { id });
        }
    });
    until("subscriber queue full", || {
        events.try_iter().for_each(drop);
        full.is_full()
    });
    let before = terminal.handle().output_chunks();
    terminal.write(b"exit-peer\n").unwrap();
    until("exit command reached blocked reader fanout", || {
        terminal.handle().output_chunks() > before
    });
    // Observe kernel exit without updating the terminal's cached lifecycle.
    let pid = terminal.pid().unwrap();
    until("child exited with output fanout blocked", || {
        crate::pty::peek_exit(pid).unwrap().is_some()
    });
    check();
    drop(full);
    drop(events);
    filler.join().unwrap();
}
