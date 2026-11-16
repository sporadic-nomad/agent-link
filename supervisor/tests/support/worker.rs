#[cfg(feature = "codex")]
#[path = "../../../tests/support/fake_agent/codex_peer.rs"]
mod codex_peer;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_link_supervisor::*;

fn binding() -> Binding {
    Binding {
        provider: "fixture".into(),
        session: "existing-session".into(),
    }
}

fn announce(line: &str) {
    println!("{line}");
    std::io::stdout().flush().unwrap();
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    #[cfg(feature = "codex")]
    if args[1] == "codex-peer" {
        codex_peer::run(&args[2]);
        return;
    }
    let path = Path::new(&args[2]);
    match args[1].as_str() {
        "launcher" => {
            // No supervisor handle lives in the launcher; the worker owns native IO.
            // Inherited pipes connect the test viewer directly to the surviving worker.
            // Intentional launcher-exit test: the OS adopts the worker. The test
            // retains its stdin/stdout, requests stop, and waits for EOF/cleanup.
            #[allow(clippy::zombie_processes)]
            let _worker = Command::new(std::env::current_exe().unwrap())
                .arg("worker")
                .arg(path)
                .env_clear()
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
        }
        "provider" => provider(path),
        "worker" => worker(path),
        _ => panic!("unknown fixture mode"),
    }
}

fn provider(path: &Path) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.join("dispatch.log"))
        .unwrap();
    for (index, line) in std::io::stdin().lock().lines().enumerate() {
        let line = line.unwrap();
        writeln!(log, "{line}").unwrap();
        log.sync_all().unwrap();
        announce(&format!("turn-{index}"));
    }
    // Test cleanup acknowledgement on normal EOF after the owner crashes.
    std::fs::write(path.join("provider-eof"), b"done").unwrap();
}

struct Native {
    child: Mutex<Child>,
    input: Mutex<ChildStdin>,
    output: Mutex<BufReader<ChildStdout>>,
    phase: Mutex<String>,
    path: PathBuf,
}

impl Runtime for Native {
    fn binding(&self) -> Binding {
        binding()
    }
    fn submit(&self, intent: &Receipt, payload: &[u8]) -> Observation {
        let db = rusqlite::Connection::open(self.path.join("worker.sqlite3")).unwrap();
        let state: u8 = db
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                [&intent.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, 0, "intent must be committed before native IO");
        if *self.phase.lock().unwrap() == "before-dispatch" {
            std::process::exit(77);
        }
        {
            let mut input = self.input.lock().unwrap();
            input.write_all(payload).unwrap();
            input.write_all(b"\n").unwrap();
            input.flush().unwrap();
        }
        let mut line = String::new();
        self.output.lock().unwrap().read_line(&mut line).unwrap();
        if *self.phase.lock().unwrap() == "after-dispatch" {
            std::process::exit(77);
        }
        Observation::Accepted(line.trim().into())
    }
    fn interrupt(&self, _: &Receipt, _: &str) -> Observation {
        Observation::Unknown
    }
    fn stop(&self) {
        let _ = self.child.lock().unwrap().kill();
    }
}

fn worker(path: &Path) {
    let mut owner = Supervisor::open(path, binding(), Limits::default()).unwrap();
    let mut native = None;
    let runtime_id = owner
        .resume(Instant::now(), |expected, _| {
            assert_eq!(expected, &binding());
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("provider")
                .arg(path)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let output = BufReader::new(child.stdout.take().unwrap());
            let instance = Arc::new(Native {
                child: Mutex::new(child),
                input: Mutex::new(input),
                output: Mutex::new(output),
                phase: Mutex::new(String::new()),
                path: path.into(),
            });
            native = Some(instance.clone());
            Ok(instance)
        })
        .unwrap();
    let native = native.unwrap();
    announce(&format!(
        "READY {} {}",
        native.child.lock().unwrap().id(),
        runtime_id.generation
    ));
    for command in std::io::stdin().lock().lines() {
        let command = command.unwrap();
        if command == "stop" {
            break;
        }
        if command == "before-intent" {
            std::process::exit(77);
        }
        if command == "uncommitted" {
            let db = rusqlite::Connection::open(path.join("worker.sqlite3")).unwrap();
            db.execute_batch("BEGIN IMMEDIATE; INSERT INTO operations VALUES('op',zeroblob(32),0,NULL,1,0,NULL,NULL);").unwrap();
            // Crash with a rollback journal and an uncommitted intent.
            std::process::exit(77);
        }
        *native.phase.lock().unwrap() = command.clone();
        let mut receipt = owner.submit("op", b"payload").unwrap();
        if command == "after-accept" {
            std::process::exit(77);
        }
        if command == "after-terminal" {
            let Outcome::Accepted(turn) = &receipt.outcome else {
                panic!("expected accepted")
            };
            receipt.outcome = Outcome::Terminal {
                turn: turn.clone(),
                result: NativeResult(b"done".to_vec()),
            };
            owner.reconcile(&binding(), receipt).unwrap();
            std::process::exit(77);
        }
        announce(&format!(
            "ACCEPTED {} {}",
            native.child.lock().unwrap().id(),
            runtime_id.generation
        ));
    }
    owner.stop_handle().unwrap().request();
    native.child.lock().unwrap().wait().unwrap();
    owner.confirm_stopped(runtime_id).unwrap();
    announce("STOPPED");
}
