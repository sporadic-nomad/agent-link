use std::cell::Cell;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::super::parser::ClaudeParser;
use super::super::source::{ClaudeSource, capture, capture_scripted};
use super::Turn;
use super::launch::SIZE;
use crate::agent::turns::tests::peer::{
    Barrier, Pause, WAIT, append, gap, observed, project, until,
};
use crate::agent::turns::{
    AttachError, Binding, Gap, Options, Parser, Record, Status, TurnReceiver,
};
use crate::agent::{
    AgentProfile, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, LaunchPlan, Permissions,
    SendOptions, ShellKind, Timing, TrustPolicy,
};
use crate::shell::ShellSpec;
use crate::{Event, ForegroundOwner, SpawnMode, SpawnSpec};

mod writer;

fn fresh_id() -> String {
    let mut b = [0u8; 16];
    File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut b)
        .unwrap();
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

struct Profile(String);

impl AgentProfile for Profile {
    fn id(&self) -> &str {
        "claude-turn-fixture"
    }
    fn launch(&self, _: &Permissions, _: &TrustPolicy, _: &Path, _: ShellKind) -> LaunchPlan {
        let mut plan = LaunchPlan::new("/bin/bash");
        plan.args = vec!["peer.sh".into(), "--session-id".into(), self.0.clone()];
        plan.env.push(("CLAUDE_CONFIG_DIR".into(), "config".into()));
        plan
    }
    fn input_ready(&self, screen: &str) -> bool {
        screen.lines().any(|l| l.starts_with('>'))
    }
    fn dialog(&self, _: &str) -> Option<crate::agent::Dialog> {
        None
    }
    fn reset_command(&self) -> Option<&'static str> {
        Some("/clear")
    }
}

struct Peer {
    term: AgentTerminal,
    events: Receiver<Event>,
    initial: String,
    file: PathBuf,
    witness: PathBuf,
    cwd: PathBuf,
    home: TempDir,
    ack: Cell<usize>,
}

impl Peer {
    fn start(project: &Path, shell: bool, wrapper: bool) -> Self {
        Self::launch(project, shell, wrapper, false, |_| {})
    }

    fn launch(
        project: &Path,
        shell: bool,
        wrapper: bool,
        hold_writer_record: bool,
        after_spawn: impl FnOnce(&AgentTerminal),
    ) -> Self {
        fs::write(project.join("peer.sh"), include_bytes!("peer.sh")).unwrap();
        fs::create_dir_all(project.join("config")).unwrap();
        let home = tempfile::tempdir_in(project).unwrap();
        let initial = fresh_id();
        let mut spec = if shell {
            let mut spec = SpawnSpec::shell(project, SIZE);
            spec.mode = SpawnMode::Shell(ShellSpec {
                program: Some("/bin/sh".into()),
                login: false,
            });
            spec
        } else if wrapper {
            // A real launcher script supplies a different kernel executable
            // and starts a descendant with its own pid-scoped writer record.
            let path = project.join("claude");
            fs::write(&path, "#!/bin/bash\n/bin/bash peer.sh \"$@\"\nexit $?\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            SpawnSpec::direct(path, ["--session-id", &initial], project, SIZE)
        } else {
            SpawnSpec::direct(
                "/bin/bash",
                ["peer.sh", "--session-id", &initial],
                project,
                SIZE,
            )
        };
        spec.env.inherit = false;
        spec.env
            .set("HOME", home.path())
            .set("PATH", "/bin:/usr/bin")
            .set("CLAUDE_CONFIG_DIR", "before-plan");
        if hold_writer_record {
            spec.env.set("HOLD_WRITER_RECORD", "yes");
        }
        let profile_id = if shell { initial.clone() } else { fresh_id() }; // direct argv is authoritative
        let agent = AgentSpec {
            turn_launch: Some(Arc::new(if wrapper { capture } else { capture_scripted })),
            timing: Timing {
                startup_max: Duration::from_millis(300),
                pre_paste_quiet: Duration::from_millis(20),
                paste_echo_quiet: Duration::from_millis(20),
                submit_verify_quiet: Duration::from_millis(30),
                submit_verify_poll: Duration::from_millis(10),
                ..Timing::default()
            },
            ..AgentSpec::new(Arc::new(Profile(profile_id)))
        };
        let (term, events) = AgentTerminal::spawn(spec, agent).unwrap();
        if shell {
            term.launch().unwrap();
        }
        if hold_writer_record {
            // The fixture uses portable-pty's ordinary spawn, which can return before
            // exec. Match the leased launch's exec boundary while the record is absent.
            let waiting = home.path().join("awaiting-bind");
            until("scripted writer reached the delayed record", || {
                waiting.exists()
            });
        }
        after_spawn(&term);
        if hold_writer_record {
            term.terminal().write(b"bind\n").unwrap();
        }
        let ready = home.path().join("started");
        until("writer acknowledged argv and reached its prompt", || {
            ready.exists() && term.state() == AgentState::Ready
        });
        let witness = PathBuf::from(fs::read_to_string(ready).unwrap().trim_end());
        let row: Value = serde_json::from_slice(&fs::read(&witness).unwrap()).unwrap();
        assert_eq!(row["sessionId"], initial);
        let cwd = project.canonicalize().unwrap();
        let slug: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let file = cwd
            .join("config/projects")
            .join(slug)
            .join(format!("{initial}.jsonl"));
        Self {
            term,
            events,
            initial,
            file,
            witness,
            cwd,
            home,
            ack: Cell::new(0),
        }
    }

    fn attach(&self) -> TurnReceiver {
        let mut found = None;
        until(
            "retiring worker released slot",
            || match ClaudeSource::attach(&self.term, Options::default()) {
                Ok(rx) => {
                    found = Some(rx);
                    true
                }
                Err(AttachError::Busy) => false,
                Err(e) => panic!("attach: {e:?}"),
            },
        );
        found.unwrap()
    }

    fn rows(&self, current: &str, text: &str) -> [Value; 3] {
        let t = Turn::recorded();
        let mut rows = [t.user, t.assistant, t.end];
        for row in &mut rows {
            row["sessionId"] = json!(current);
            row["cwd"] = json!(self.cwd);
        }
        rows[1]["session_id"] = json!(self.initial);
        rows[1]["message"]["content"][0]["text"] = json!(text);
        rows
    }

    fn input(&self, command: &str) {
        self.term
            .terminal()
            .write(format!("{command}\n").as_bytes())
            .unwrap();
    }

    fn command(&self, command: &str) {
        let next = self.ack.get() + 1;
        self.input(command);
        let ack = self.home.path().join("ack");
        until("scripted writer committed command", || {
            fs::read_to_string(&ack)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                == Some(next)
        });
        self.ack.set(next);
    }

    fn write(&self, current: &str, text: &str) {
        fs::write(
            self.home.path().join("batch"),
            encode(&self.rows(current, text)),
        )
        .unwrap();
        self.command("append");
    }
}

fn encode(rows: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    bytes
}

#[test]
fn interleaved_same_project_ptys_ignore_sdk_decoy_history_and_file_times() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let b = Peer::start(home.path(), false, false);
    a.write(&a.initial, "history");
    let decoy = a.file.with_file_name(format!("{}.jsonl", fresh_id()));
    let mut sdk = a.rows(&a.initial, "foreign SDK");
    for row in &mut sdk {
        row["entrypoint"] = json!("sdk-cli");
    }
    fs::write(&decoy, encode(&sdk)).unwrap();
    let ar = a.attach();
    let br = b.attach();
    observed(&ar, &a.file);
    observed(&br, &b.file);
    for (p, text) in [
        (&b, "second one"),
        (&a, "same"),
        (&a, "same"),
        (&b, "second two"),
    ] {
        p.write(&p.initial, text);
    }
    for path in [&a.file, &b.file, &decoy] {
        File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(100000))
            .unwrap();
    }
    for (p, rx, expected) in [
        (&a, &ar, ["same", "same"]),
        (&b, &br, ["second one", "second two"]),
    ] {
        observed(rx, &p.file);
        for text in expected {
            assert_eq!(rx.try_recv().unwrap().text, text);
        }
        assert!(rx.try_recv().is_none());
    }
}

#[test]
fn shell_wrapper_and_replayed_shell_launch_cannot_borrow_a_writers_pid() {
    let (_lock, home) = project();
    let shell = Peer::start(home.path(), true, false);
    let wrapper = Peer::start(home.path(), false, true);
    for peer in [&shell, &wrapper] {
        let row: Value = serde_json::from_slice(&fs::read(&peer.witness).unwrap()).unwrap();
        assert_ne!(row["pid"], peer.term.terminal().pid().unwrap());
        peer.write(&peer.initial, "wrong writer");
        gap(&peer.attach(), Gap::Uncorroborated);
    }
    shell.input("exit-peer");
    until("shell regained foreground", || {
        shell.term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    shell.term.launch().unwrap();
    assert!(matches!(
        ClaudeSource::attach(&shell.term, Options::default()),
        Err(AttachError::NoLaunch)
    ));
}

#[test]
fn missing_stale_contradictory_and_oversized_witnesses_withhold_every_reply() {
    let (_lock, home) = project();
    let p = Peer::start(home.path(), false, false);
    let body = fs::read(&p.witness).unwrap();
    let witness: Value = serde_json::from_slice(&body).unwrap();
    let replace = |bytes: &[u8]| {
        let temp = p.home.path().join("replacement");
        fs::write(&temp, bytes).unwrap();
        fs::rename(temp, &p.witness).unwrap();
    };
    for (field, value) in [
        ("startedAt", json!(1)),
        ("pid", json!(1)),
        ("kind", json!("sdk")),
        ("entrypoint", json!("sdk-cli")),
        ("sessionId", json!("../path-sentinel")),
        ("cwd", json!("/wrong-project")),
        ("version", json!("other")),
    ] {
        let rx = p.attach();
        observed(&rx, &p.file);
        p.write(&p.initial, "never expose");
        observed(&rx, &p.file);
        let mut bad = witness.clone();
        bad[field] = value;
        replace(&serde_json::to_vec(&bad).unwrap());
        assert!(rx.try_recv().is_none());
        gap(&rx, Gap::Uncorroborated);
        replace(&body);
    }
    let mut exact = body.clone();
    exact.resize(64 * 1024, b' ');
    replace(&exact);
    let rx = p.attach();
    observed(&rx, &p.file);
    p.write(&p.initial, "at limit");
    observed(&rx, &p.file);
    assert_eq!(rx.try_recv().unwrap().text, "at limit");
    exact.push(b' ');
    replace(&exact);
    gap(&rx, Gap::Oversized);
    fs::remove_file(&p.witness).unwrap();
    gap(&p.attach(), Gap::Uncorroborated);
    let outside = p.home.path().join("witness");
    fs::write(&outside, body).unwrap();
    symlink(&outside, &p.witness).unwrap();
    gap(&p.attach(), Gap::Io);
    fs::remove_file(&p.witness).unwrap();
    fs::create_dir(&p.witness).unwrap();
    gap(&p.attach(), Gap::Uncorroborated);
}

struct PausedFinal {
    inner: ClaudeParser,
    pause: Option<Pause>,
}
impl Parser for PausedFinal {
    fn record(&mut self, binding: &Binding, line: &[u8], limit: usize) -> Result<Record, Gap> {
        let record = self.inner.record(binding, line, limit)?;
        if matches!(&record, Record::End(Some(r)) if r.text == "held") {
            self.pause.take().unwrap().park();
        }
        Ok(record)
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
}

#[test]
fn switch_while_parsed_discards_old_queue_and_peer_append_then_skips_copied_history() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let b = Peer::start(home.path(), false, false);
    let (barrier, pause) = Barrier::new();
    let (source, inner) = ClaudeSource::for_terminal(&a.term).unwrap();
    let ar = a
        .term
        .attach_turns(
            source,
            PausedFinal {
                inner,
                pause: Some(pause),
            },
            Options::default(),
        )
        .unwrap();
    let br = b.attach();
    observed(&ar, &a.file);
    observed(&br, &b.file);
    a.write(&a.initial, "queued");
    observed(&ar, &a.file);
    a.write(&a.initial, "held");
    barrier.wait();

    let next = fresh_id();
    let dest = a.file.with_file_name(format!("{next}.jsonl"));
    fs::write(
        &dest,
        format!("{{\"type\":\"mode\",\"mode\":\"normal\",\"sessionId\":\"{next}\"}}\n"),
    )
    .unwrap();
    append(&dest, &encode(&a.rows(&a.initial, "copied prior turn")));
    fs::write(a.home.path().join("next"), &next).unwrap();
    a.command("/clear");
    fs::write(b.home.path().join("foreign"), &a.initial).unwrap();
    fs::write(
        b.home.path().join("batch"),
        encode(&a.rows(&a.initial, "foreign after switch")),
    )
    .unwrap();
    b.command("foreign");
    b.write(&b.initial, "unaffected");
    observed(&br, &b.file);
    assert_eq!(br.try_recv().unwrap().text, "unaffected");
    let submitted = b.term.send(
        "ordinary input",
        SendOptions {
            submit: true,
            ..SendOptions::default()
        },
    );
    assert_eq!(submitted.wait(WAIT), Some(DeliveryOutcome::Delivered));
    let mut output = Vec::new();
    for event in b.events.try_iter() {
        if let Event::Output { bytes, .. } = event {
            output.extend_from_slice(&bytes);
        }
    }
    assert!(String::from_utf8_lossy(&output).contains("accepted input"));

    barrier.release();
    gap(&ar, Gap::SessionChanged);
    let rebound = a.attach();
    observed(&rebound, &dest);
    assert!(rebound.try_recv().is_none());
    a.write(&next, "fresh after clear");
    observed(&rebound, &dest);
    assert_eq!(rebound.try_recv().unwrap().text, "fresh after clear");
    assert!(rebound.try_recv().is_none());

    // The other terminal's ordinary verified reset still completes independently.
    fs::write(b.home.path().join("next"), fresh_id()).unwrap();
    assert_eq!(b.term.reset().wait(WAIT), Some(DeliveryOutcome::Delivered));
    gap(&br, Gap::SessionChanged);
}

#[test]
fn observer_requires_verified_header_and_discards_text_on_replacement_or_close() {
    let (_lock, home) = project();
    let p = Peer::start(home.path(), false, false);
    let header = fs::read(&p.file).unwrap();
    fs::write(
        &p.file,
        encode(&[json!({"type":"mode","mode":"normal","sessionId":fresh_id()})]),
    )
    .unwrap();
    gap(&p.attach(), Gap::Uncorroborated);
    let outside = p.home.path().join("outside");
    fs::write(&outside, &header).unwrap();
    fs::remove_file(&p.file).unwrap();
    symlink(&outside, &p.file).unwrap();
    gap(&p.attach(), Gap::Io);
    fs::remove_file(&p.file).unwrap();
    fs::create_dir(&p.file).unwrap();
    gap(&p.attach(), Gap::Uncorroborated);
    fs::remove_dir(&p.file).unwrap();
    fs::write(&p.file, &header).unwrap();
    let rx = p.attach();
    observed(&rx, &p.file);
    p.write(&p.initial, "queued");
    observed(&rx, &p.file);
    fs::rename(outside, &p.file).unwrap();
    assert!(rx.try_recv().is_none());
    gap(&rx, Gap::Replaced);
    let rx = p.attach();
    observed(&rx, &p.file);
    append(&p.file, &encode(&p.rows(&p.initial, "unfinished")[..2]));
    observed(&rx, &p.file);
    drop(p);
    until("terminal closed observation", || {
        rx.status() == Status::Closed
    });
    assert!(rx.try_recv().is_none());
}

#[test]
fn a_partial_mode_header_keeps_the_first_file_and_length_across_polls() {
    use crate::agent::turns::tests::peer::{clock, tick};
    use std::time::Instant;

    let (_lock, home) = project();
    for loss in [
        Some(Gap::Replaced),
        Some(Gap::Truncated),
        Some(Gap::Missing),
        None,
    ] {
        let p = Peer::start(home.path(), false, false);
        let header = fs::read(&p.file).unwrap();
        let split = header.len() / 2;
        fs::write(&p.file, &header[..1]).unwrap();
        let (clock, options) = clock();
        let rx = ClaudeSource::attach(&p.term, options).unwrap();
        tick(&clock); // The first poll has read the partial header.
        append(&p.file, &header[1..split]);
        tick(&clock);
        tick(&clock); // A later poll has now sampled the longer prefix.
        assert_eq!(rx.status(), Status::Waiting);
        match loss {
            Some(Gap::Replaced) => {
                let replacement = p.home.path().join("replacement");
                fs::write(&replacement, &header).unwrap();
                fs::rename(replacement, &p.file).unwrap();
            }
            Some(Gap::Truncated) => fs::OpenOptions::new()
                .write(true)
                .open(&p.file)
                .unwrap()
                .set_len((split - 1) as u64)
                .unwrap(),
            Some(Gap::Missing) => fs::remove_file(&p.file).unwrap(),
            None => append(&p.file, &header[split..]),
            _ => unreachable!(),
        }
        if let Some(loss) = loss {
            // The poll started by the first tick can also see the change.
            until("partial header continuity gap", || {
                let _ = clock.try_send(Instant::now());
                rx.status() == Status::Gap(loss)
            });
            assert!(rx.try_recv().is_none());
        } else {
            tick(&clock);
            observed(&rx, &p.file);
            assert!(rx.try_recv().is_none());
            p.write(&p.initial, "after the completed header");
            tick(&clock);
            observed(&rx, &p.file);
            assert_eq!(rx.try_recv().unwrap().text, "after the completed header");
        }
    }
}

#[test]
fn split_utf8_exact_line_limit_and_missing_end_require_a_fresh_boundary() {
    let (_lock, home) = project();
    let p = Peer::start(home.path(), false, false);
    let rows = p.rows(&p.initial, "café");
    let parts = rows.map(|v| encode(&[v]));
    let mut options = Options::default();
    options.limits.line_bytes = parts.iter().map(|p| p.len() - 1).max().unwrap();
    let limit = options.limits.line_bytes;
    let rx = ClaudeSource::attach(&p.term, options).unwrap();
    observed(&rx, &p.file);
    append(&p.file, &parts[0]);
    let split = parts[1].iter().position(|&b| b == 0xc3).unwrap() + 1;
    append(&p.file, &parts[1][..split]);
    observed(&rx, &p.file);
    append(&p.file, &parts[1][split..]);
    observed(&rx, &p.file);
    assert!(rx.try_recv().is_none());
    p.write(&p.initial, "café");
    observed(&rx, &p.file);
    assert!(matches!(
        rx.status(),
        Status::Active {
            incomplete_turns: 1,
            ..
        }
    ));
    assert_eq!(rx.try_recv().unwrap().text, "café");
    append(&p.file, &vec![b' '; limit]);
    observed(&rx, &p.file);
    append(&p.file, b" ");
    gap(&rx, Gap::Oversized);
    let fresh = p.attach();
    observed(&fresh, &p.file);
    append(&p.file, b"\n");
    append(&p.file, &parts[2]);
    p.write(&p.initial, "recovered");
    observed(&fresh, &p.file);
    assert_eq!(fresh.try_recv().unwrap().text, "recovered");
    assert!(fresh.try_recv().is_none());
}

#[test]
fn an_observed_overlapping_claim_invalidates_both_readers() {
    use crate::agent::turns::tests::peer::{clock, tick};
    let (_lock, home) = project();
    for overlap_ends in [false, true] {
        let a = Peer::start(home.path(), false, false);
        let b = Peer::start(home.path(), false, false);
        let (clock, options) = clock();
        let ar = ClaudeSource::attach(&a.term, options).unwrap();
        let br = b.attach();
        observed(&ar, &a.file);
        observed(&br, &b.file);
        a.write(&a.initial, "queued before overlap");
        tick(&clock);
        observed(&ar, &a.file);
        fs::write(b.home.path().join("next"), &a.initial).unwrap();
        b.command("/clear");
        gap(&br, Gap::Uncorroborated);
        if overlap_ends {
            let next = fresh_id();
            fs::write(b.home.path().join("next"), &next).unwrap();
            b.command("/clear");
            observed(&b.attach(), &b.file.with_file_name(format!("{next}.jsonl")));
        }
        assert!(ar.try_recv().is_none());
        if overlap_ends {
            gap(&ar, Gap::SessionChanged);
        } else {
            gap(&ar, Gap::Uncorroborated);
            gap(&a.attach(), Gap::Uncorroborated);
        }
    }
}

#[test]
fn a_concurrent_peer_clear_preserves_queued_replies_unless_the_sample_saw_an_overlap() {
    use super::super::source::AFTER_WRITER_SAMPLES;
    use crate::agent::turns::tests::peer::{clock, tick};

    let (_lock, home) = project();
    for overlap in [false, true] {
        let a = Peer::start(home.path(), false, false);
        let b = Peer::start(home.path(), false, false);
        let (a_clock, a_options) = clock();
        let (b_clock, b_options) = clock();
        let ar = ClaudeSource::attach(&a.term, a_options).unwrap();
        let br = ClaudeSource::attach(&b.term, b_options).unwrap();
        observed(&ar, &a.file);
        observed(&br, &b.file);
        a.write(&a.initial, "queued before clear");
        tick(&a_clock);
        observed(&ar, &a.file);

        if overlap {
            fs::write(b.home.path().join("next"), &a.initial).unwrap();
            b.command("/clear");
        }
        let (barrier, pause) = Barrier::new();
        let reply = std::thread::scope(|scope| {
            let ar = &ar;
            let pull = scope.spawn(move || {
                AFTER_WRITER_SAMPLES.set(Some(pause));
                ar.try_recv()
            });
            barrier.wait();
            fs::write(b.home.path().join("next"), fresh_id()).unwrap();
            b.command("/clear");
            tick(&b_clock);
            gap(&br, Gap::SessionChanged);
            barrier.release();
            pull.join().unwrap()
        });

        if overlap {
            assert!(reply.is_none());
            gap(&ar, Gap::SessionChanged);
        } else {
            assert_eq!(reply.unwrap().text, "queued before clear");
            observed(&ar, &a.file);
        }
    }
}

#[test]
fn retiring_parser_holds_its_uuid_after_terminal_drop() {
    let (_lock, home) = project();
    let a = Peer::start(home.path(), false, false);
    let (barrier, pause) = Barrier::new();
    let (source, inner) = ClaudeSource::for_terminal(&a.term).unwrap();
    let rx = a
        .term
        .attach_turns(
            source,
            PausedFinal {
                inner,
                pause: Some(pause),
            },
            Options::default(),
        )
        .unwrap();
    observed(&rx, &a.file);
    a.write(&a.initial, "held");
    barrier.wait();
    let mut spec = SpawnSpec::direct(
        "/bin/bash",
        ["peer.sh", "--session-id", &a.initial],
        &a.cwd,
        SIZE,
    );
    spec.env.inherit = false;
    spec.env.set("CLAUDE_CONFIG_DIR", "config");
    let plan = LaunchPlan::new("ignored");
    let old = a.file.clone();
    drop(a);
    assert_eq!(rx.status(), Status::Closed);
    fs::remove_file(old).unwrap(); // isolate the active-claim check from the existing-file check
    assert!(capture_scripted(&mut spec, &plan).is_err());
    barrier.release();
    until("retiring writer released its reservation", || {
        capture_scripted(&mut spec, &plan).is_ok()
    });
    assert!(rx.try_recv().is_none());
}

#[test]
fn session_change_while_waiting_for_a_file_requires_another_attachment() {
    use crate::agent::turns::tests::peer::{clock, tick};
    let (_lock, home) = project();
    let p = Peer::start(home.path(), false, false);
    fs::remove_file(&p.file).unwrap();
    let (clock, options) = clock();
    let rx = ClaudeSource::attach(&p.term, options).unwrap();
    tick(&clock);
    assert_eq!(rx.status(), Status::Waiting);
    let next = fresh_id();
    fs::write(p.home.path().join("next"), &next).unwrap();
    p.command("/clear");
    tick(&clock);
    gap(&rx, Gap::SessionChanged);
    p.write(&next, "preceded reattachment");
    let fresh = p.attach();
    let file = p.file.with_file_name(format!("{next}.jsonl"));
    observed(&fresh, &file);
    assert!(fresh.try_recv().is_none());
    p.write(&next, "after attachment");
    observed(&fresh, &file);
    assert_eq!(fresh.try_recv().unwrap().text, "after attachment");
}
