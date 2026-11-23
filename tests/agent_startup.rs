//! `AgentTerminal` startup against the fake agent (`agent-link-fake-agent`): launch, the
//! startup gate's states and `blocking` flag as events, dialog steering, the post-ready
//! dialog watch, the direct-child and bare-shell modes, activity, and the session's end.
//! Timings are shortened (or lengthened, where a slow first exec must not end a window)
//! per test.
#![cfg(all(unix, feature = "agent"))]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_link::agent::{
    Activity, AgentProfile, AgentSpec, AgentState, AgentTerminal, DeliveryOutcome, Dialog,
    LaunchPlan, ModelChoice, Permissions, ProfileRegistry, ShellKind, Timing, TrustPolicy,
};
use agent_link::shell::ShellSpec;
use agent_link::{Error, Event, ForegroundOwner, PtySize, SpawnMode, SpawnSpec};
use common::agent::{
    activity_until, spawn_agent, start_agent_shell, start_agent_shell_spec, states_until,
};
use common::delivery::{outcome, submit};
use common::fake::{READY_MARKER, fake_profile};
use common::{LAUNCH_WAIT, WAIT, fake_agent_path, logger, screen_contains, wait_event, wait_until};
use crossbeam_channel::Receiver;
use tempfile::TempDir;

use AgentState::{Exited, Ready, ShellOnly, Starting, TrustPrompt};

/// The fake with `scene` and `flags` in the test shell, not launched yet.
fn shell_with(
    scene: &str,
    flags: &[&str],
    timing: Timing,
) -> (AgentTerminal, Receiver<Event>, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let agent = AgentSpec {
        timing,
        trust: TrustPolicy::unattended(),
        ..AgentSpec::new(fake_profile(scene, flags))
    };
    let (term, events) = start_agent_shell(home.path(), agent);
    (term, events, home)
}

/// Default timing, but a trust window that outlasts a slow first exec of the fake.
fn long_window() -> Timing {
    Timing {
        trust_watch: LAUNCH_WAIT + Duration::from_secs(10),
        ..Timing::default()
    }
}

fn is_ready(state: AgentState, _: bool) -> bool {
    state == Ready
}

fn position(seen: &[(AgentState, bool)], want: (AgentState, bool)) -> usize {
    seen.iter()
        .position(|s| *s == want)
        .unwrap_or_else(|| panic!("no {want:?} in {seen:?}"))
}

#[test]
fn agent_terminal_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AgentTerminal>();
}

#[test]
fn the_trust_dialog_is_steered_to_yes_then_ready() {
    let (term, events, _home) = shell_with("trust", &[], long_window());
    assert_eq!(term.state(), ShellOnly);
    assert!(!term.blocking());
    term.launch().unwrap();
    assert_eq!(term.state(), Starting);
    assert!(term.blocking());
    let seen = states_until(&term, &events, LAUNCH_WAIT, is_ready);
    assert_eq!(seen.first(), Some(&(Starting, true)), "{seen:?}");
    let dialog = position(&seen, (TrustPrompt, true));
    assert!(!seen[..dialog].iter().any(|s| s.0 == Ready), "{seen:?}");
    assert_eq!(seen.last(), Some(&(Ready, false)), "{seen:?}");
    assert!(!term.blocking());
    // The pointer was moved to "Yes" and confirmed: the composer, no `bye`.
    let screen = screen_contains(term.terminal(), READY_MARKER, WAIT);
    assert!(!screen.text().contains("bye"), "{}", screen.text());
}

#[test]
fn the_ready_screen_readies_without_any_fallback() {
    // Quiet settle, cap and window far away: only the ready composer can ready the gate.
    let far = Duration::from_secs(120);
    let timing = Timing {
        startup_quiet: far,
        startup_max: far,
        trust_watch: far,
        ..Timing::default()
    };
    let (term, events, _home) = shell_with("ready", &[], timing);
    term.launch().unwrap();
    let seen = states_until(&term, &events, LAUNCH_WAIT, is_ready);
    assert_eq!(seen, [(Starting, true), (Ready, false)]);
}

#[test]
fn a_slow_dialog_is_answered_after_the_overlay_lifted() {
    let (term, events, _home) =
        shell_with("slow-dialog", &["--dialog-after-ms", "1500"], long_window());
    term.launch().unwrap();
    // Nothing drawn for 1.5 s: the quiet settle lifts the overlay, input still waits.
    let seen = states_until(&term, &events, LAUNCH_WAIT, |s, b| s == Starting && !b);
    assert_eq!(seen, [(Starting, true), (Starting, false)]);
    assert_eq!(term.state(), Starting);
    // Then the dialog: masked again while steered, confirmed inside the window.
    let seen = states_until(&term, &events, LAUNCH_WAIT, is_ready);
    assert_eq!(seen.first(), Some(&(TrustPrompt, true)), "{seen:?}");
    assert_eq!(seen.last(), Some(&(Ready, false)), "{seen:?}");
    screen_contains(term.terminal(), READY_MARKER, WAIT);
}

#[test]
fn a_silent_agent_is_ready_when_the_trust_window_ends() {
    logger::install();
    let window = Duration::from_secs(3);
    let timing = Timing {
        trust_watch: window,
        ..Timing::default()
    };
    let (term, events, _home) = shell_with("silent", &[], timing);
    let launched = Instant::now();
    term.launch().unwrap();
    let seen = states_until(&term, &events, LAUNCH_WAIT, is_ready);
    assert!(
        launched.elapsed() >= window,
        "ready after {:?}",
        launched.elapsed()
    );
    assert_eq!(seen.first(), Some(&(Starting, true)), "{seen:?}");
    assert_eq!(seen.last(), Some(&(Ready, false)), "{seen:?}");
    let id = term.terminal().id();
    let line = logger::wait_line(id, "startup markers not recognised", WAIT);
    assert!(line.starts_with("WARN "), "{line}");
    assert!(line.contains("for fake after "), "{line}");
    assert!(line.contains("(identity honest); bottom rows: "), "{line}");
    // The fake's banner, or the shell's echo of the launch line if it had not started yet.
    assert!(
        line.contains("fake-agent v1 (silent)") || line.contains("agent-link-fake-agent silent"),
        "{line}"
    );
    let reports = logger::lines_for(id)
        .into_iter()
        .filter(|l| l.contains("startup markers not recognised"))
        .count();
    assert_eq!(reports, 1);
}

#[test]
fn a_dialog_right_after_ready_is_answered_by_the_watch() {
    let (term, events, _home) =
        shell_with("late-dialog", &["--dialog-after-ms", "300"], long_window());
    term.launch().unwrap();
    // Usually `Ready` on the composer, then the dialog 300 ms later — inside the 1 s watch
    // it un-readies the gate. Under load the first screen read can already show the
    // dialog (no `Ready` before it). Either way: ready again once it was answered.
    let mut prompted = false;
    let seen = states_until(&term, &events, LAUNCH_WAIT, |state, _| {
        prompted |= state == TrustPrompt;
        prompted && state == Ready
    });
    let dialog = position(&seen, (TrustPrompt, true));
    assert!(
        seen[..dialog]
            .iter()
            .all(|s| matches!(s.0, Starting | Ready)),
        "{seen:?}"
    );
    assert_eq!(seen.last(), Some(&(Ready, false)), "{seen:?}");
    // Steered down to "Yes" and confirmed: the composer is back.
    let screen = screen_contains(term.terminal(), READY_MARKER, WAIT);
    assert!(
        !screen.text().contains("fake trust dialog"),
        "{}",
        screen.text()
    );
}

/// A dialog the gate does not answer (steering off) parks it in `TrustPrompt` with the
/// overlay lifted — never `Ready` — until the user answers; then it is ready.
#[test]
fn a_dialog_left_to_the_user_parks_until_answered() {
    let home = tempfile::tempdir().unwrap();
    let agent = AgentSpec {
        timing: long_window(),
        trust: TrustPolicy::default(),
        ..AgentSpec::new(fake_profile("trust", &[]))
    };
    let (term, events) = start_agent_shell(home.path(), agent);
    term.launch().unwrap();
    let seen = states_until(&term, &events, LAUNCH_WAIT, |s, b| s == TrustPrompt && !b);
    assert!(!seen.iter().any(|s| s.0 == Ready), "{seen:?}");
    // Nothing pressed, nothing changes: the pointer stays on "No".
    while let Ok(event) = events.recv_timeout(Duration::from_millis(1000)) {
        assert!(!matches!(event, Event::AgentState { .. }), "{event:?}");
    }
    screen_contains(term.terminal(), "❯ 1. No, exit", WAIT);
    assert_eq!(term.state(), TrustPrompt);
    assert!(!term.blocking());
    // The user answers: Down to "Yes", Enter.
    term.terminal().write(b"\x1b[B").unwrap();
    screen_contains(term.terminal(), "❯ 2. Yes, trust this folder", WAIT);
    term.terminal().write(b"\r").unwrap();
    let seen = states_until(&term, &events, WAIT, is_ready);
    assert_eq!(seen, [(Ready, false)]);
    screen_contains(term.terminal(), READY_MARKER, WAIT);
}

#[test]
fn a_direct_agent_is_armed_at_spawn() {
    let home = tempfile::tempdir().unwrap();
    let size = PtySize { rows: 24, cols: 80 };
    let spec = SpawnSpec::direct(fake_agent_path(), ["ready"], home.path(), size);
    let (term, events) = spawn_agent(spec, AgentSpec::new(fake_profile("ready", &[])));
    assert_eq!(term.state(), Starting);
    assert!(term.blocking());
    // Nothing to type: the child is the agent.
    term.launch().unwrap();
    let seen = states_until(&term, &events, LAUNCH_WAIT, is_ready);
    assert_eq!(seen, [(Starting, true), (Ready, false)]);
}

#[derive(Default)]
struct ModelPeer {
    launches: AtomicUsize,
    mappings: AtomicUsize,
}

impl AgentProfile for ModelPeer {
    fn id(&self) -> &str {
        "model-peer"
    }
    fn dialog(&self, _: &str) -> Option<Dialog> {
        None
    }
    fn input_ready(&self, screen: &str) -> bool {
        screen.contains(READY_MARKER)
    }
    fn launch(
        &self,
        perms: &Permissions,
        _: &TrustPolicy,
        _: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        self.launches.fetch_add(1, Ordering::Relaxed);
        LaunchPlan::new("./model-peer")
            .with_permissions(perms, &[], &[])
            .for_shell(shell)
    }
    fn model_args(&self, choice: &ModelChoice, _: &[String]) -> agent_link::Result<Vec<String>> {
        self.mappings.fetch_add(1, Ordering::Relaxed);
        let mut args = vec!["--model".into(), choice.model.clone()];
        if let Some(effort) = &choice.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        Ok(args)
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[test]
fn explicit_launch_and_delivery_relaunch_reuse_the_selected_argv() {
    let home = tempfile::tempdir().unwrap();
    // Record each word before handing the PTY to the usual fake composer.
    write_script(
        home.path(),
        "model-peer",
        "printf '%s\\n' \"$PEER_POLICY\" \"$@\" >> received-argv\nexec \"$PEER_PROGRAM\" ready",
    );
    let report = home.path().join("received-argv");
    let model = "provider/model [1m]:tag";
    // Fill the canonical input line with a base argument before the selection.
    // Both the first launch and the cached relaunch must deliver the trailing words.
    let line_limit = if cfg!(any(target_os = "linux", target_os = "android")) {
        4095
    } else if cfg!(target_vendor = "apple") {
        1023
    } else {
        254
    };
    let fixed_bytes =
        "./model-peer --custom-policy ".len() + format!(" --model '{model}' --effort high").len();
    let padding = "p".repeat(line_limit - fixed_bytes);
    let profile = Arc::new(ModelPeer::default());
    let agent = AgentSpec {
        model: Some(ModelChoice::new(model).with_effort("high")),
        permissions: Permissions::Custom {
            args: vec!["--custom-policy".into(), padding.clone()],
            env: vec![("PEER_POLICY".into(), "custom".into())],
        },
        timing: long_window(),
        ..AgentSpec::new(profile.clone())
    };
    let mut spec = common::sh_spec(home.path());
    spec.env
        .set("PEER_PROGRAM", fake_agent_path())
        .set("PEER_POLICY", "host");
    // The quoted argv, shell terminator and line-clearing keys fit exactly.
    spec.input_limits.max_write_bytes = line_limit + b" \x15\n".len();
    let (term, events) = start_agent_shell_spec(spec, agent);
    term.launch().unwrap();
    states_until(&term, &events, LAUNCH_WAIT, is_ready);
    let expected =
        format!("custom\n--custom-policy\n{padding}\n--model\n{model}\n--effort\nhigh\n");
    assert_eq!(fs::read_to_string(&report).unwrap(), expected);

    term.terminal().write(b"\x03").unwrap();
    wait_until("the shell in front", || {
        term.terminal().foreground().owner == ForegroundOwner::Shell
    });
    let ticket = term.send("after relaunch", submit());
    assert_eq!(outcome(&term, &events, &ticket), DeliveryOutcome::Delivered);
    assert_eq!(fs::read_to_string(report).unwrap(), expected.repeat(2));
    assert_eq!(profile.launches.load(Ordering::Relaxed), 1);
    assert_eq!(profile.mappings.load(Ordering::Relaxed), 1);
}

#[test]
fn a_model_launch_uses_fish_quoting_for_the_resolved_shell() {
    let home = tempfile::tempdir().unwrap();
    let fish = write_script(
        home.path(),
        "fish",
        "printf 'fish fixture ready\\r\\n'\nIFS= read -r line\nprintf '%s\\n' \"$line\" > received-line",
    );
    let mut spec = common::sh_spec(home.path());
    spec.mode = SpawnMode::Shell(ShellSpec {
        program: Some(fish),
        login: false,
    });
    let agent = AgentSpec {
        model: Some(ModelChoice::new(r"provider\model's\").with_effort("%self")),
        ..AgentSpec::new(Arc::new(ModelPeer::default()))
    };
    let (term, _events) = spawn_agent(spec, agent);
    screen_contains(term.terminal(), "fish fixture ready", WAIT);
    term.launch().unwrap();
    let report = home.path().join("received-line");
    wait_until("the shell recorded the launch line", || {
        fs::read_to_string(&report).is_ok_and(|line| line.ends_with('\n'))
    });
    assert_eq!(
        fs::read_to_string(report).unwrap().trim_end(),
        r"./model-peer --model 'provider\\model\'s\\' --effort '%self'"
    );
}

#[test]
fn rejected_model_requests_leave_the_child_start_marker_absent() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("child-started");
    let program = write_script(home.path(), "mark-start", ": > child-started");
    let mut shell = common::sh_spec(home.path());
    let direct = SpawnSpec::direct(&program, [] as [&str; 0], home.path(), shell.size);
    shell.mode = SpawnMode::Shell(ShellSpec {
        program: Some(program),
        login: false,
    });
    // Check that a successful spawn of this shell writes the marker.
    let (term, events) = spawn_agent(shell.clone(), AgentSpec::default());
    wait_event(term.terminal(), &events, LAUNCH_WAIT, |e| {
        matches!(e, Event::Exit { .. })
    });
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();

    let refused = |spec, agent| {
        let error = {
            let _guard = common::spawn_lock();
            AgentTerminal::spawn(spec, agent).unwrap_err()
        };
        assert!(!marker.exists(), "child started on refusal: {error}");
        error
    };
    let mut agent = AgentSpec {
        model: Some(ModelChoice::new("selected")),
        ..AgentSpec::default()
    };
    assert!(matches!(
        refused(shell.clone(), agent.clone()),
        Error::InvalidInput(_)
    ));
    agent.profile = Some(fake_profile("ready", &[])); // default hook
    assert!(matches!(
        refused(direct, agent.clone()),
        Error::InvalidInput(_)
    ));
    assert!(matches!(
        refused(shell.clone(), agent.clone()),
        Error::Unsupported(_)
    ));
    // OpenCode's TUI has no effort option and Cursor has no model mapping.
    for (id, choice) in [
        ("opencode", ModelChoice::new("selected").with_effort("high")),
        ("cursor", ModelChoice::new("selected")),
    ] {
        let builtin = AgentSpec {
            model: Some(choice),
            ..AgentSpec::new(ProfileRegistry::builtin().get(id).unwrap())
        };
        assert!(
            matches!(refused(shell.clone(), builtin), Error::Unsupported(_)),
            "{id}"
        );
    }
    agent.profile = Some(Arc::new(ModelPeer::default()));
    agent.model = Some(ModelChoice::new("selected").with_effort("bad\nlevel"));
    assert!(matches!(
        refused(shell.clone(), agent.clone()),
        Error::InvalidInput(_)
    ));
    agent.model = Some(ModelChoice::new("selected").with_effort("high"));
    let long_base = AgentSpec {
        permissions: Permissions::Custom {
            args: vec!["p".repeat(4095)],
            env: Vec::new(),
        },
        ..agent.clone()
    };
    assert!(matches!(
        refused(shell.clone(), long_base),
        Error::InvalidInput(_)
    ));
    shell.input_limits.max_write_bytes = 1;
    assert!(matches!(
        refused(shell, agent),
        Error::InputTooLarge { limit: 1, .. }
    ));
}

/// The built-in OpenCode profile types its model words after the default posture, whose
/// permission env default still reaches the agent. The stand-in named `opencode` is
/// found on a PATH without the host's directories, so an installed CLI never runs.
#[test]
fn an_opencode_model_launch_keeps_the_permission_env_default() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    write_script(
        &bin,
        "opencode",
        "printf '%s\\n' \"$OPENCODE_PERMISSION\" \"$@\" > argv.tmp && mv argv.tmp received-argv",
    );
    let mut path = bin.into_os_string();
    path.push(":/usr/bin:/bin");
    let mut spec = common::sh_spec(home.path());
    spec.env.set("PATH", path);
    let model = "provider/model [1m]";
    let agent = AgentSpec {
        model: Some(ModelChoice::new(model)),
        ..AgentSpec::new(ProfileRegistry::builtin().get("opencode").unwrap())
    };
    let (term, _events) = start_agent_shell_spec(spec, agent);
    term.launch().unwrap();
    let report = home.path().join("received-argv");
    wait_until("the stand-in recorded its argv", || report.exists());
    assert_eq!(
        fs::read_to_string(report).unwrap(),
        format!("{{\"*\":\"allow\"}}\n-m\n{model}\n")
    );
}

/// No profile: only session events — not for a spinner title or progress, not at the exit.
#[test]
fn a_bare_shell_has_no_agent_events() {
    let home = tempfile::tempdir().unwrap();
    let (term, events) = start_agent_shell(home.path(), AgentSpec::default());
    assert_eq!(term.state(), ShellOnly);
    term.launch().unwrap();
    // A Braille spinner title and an active progress report, then a marker line.
    term.terminal()
        .write(b"printf '\\033]0;\\342\\240\\213 spin\\007\\033]9;4;3\\007'; echo bare-$((1+1))\n")
        .unwrap();
    let is_agent = |e: &Event| matches!(e, Event::AgentState { .. } | Event::Activity { .. });
    let deadline = Instant::now() + WAIT;
    let mut text = String::new();
    let (mut titles, mut progress) = (0, 0);
    // The title and progress events follow the output that carried them.
    while !(text.contains("bare-2") && titles > 0 && progress > 0) {
        match events.recv_deadline(deadline) {
            Ok(Event::Output { bytes, .. }) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Event::Title { .. }) => titles += 1,
            Ok(Event::Progress { .. }) => progress += 1,
            Ok(other) => assert!(!is_agent(&other), "{other:?}"),
            Err(e) => panic!("no output ({e}): {text:?}"),
        }
    }
    assert_eq!((titles, progress), (1, 1), "{text:?}");
    assert_eq!(term.activity(), Activity::Unknown);
    term.terminal().write(b"exit\n").unwrap();
    wait_event(term.terminal(), &events, WAIT, |e| {
        assert!(!is_agent(e), "{e:?}");
        matches!(e, Event::Exit { .. })
    });
    // Nothing follows the exit either.
    while let Ok(event) = events.recv_timeout(Duration::from_millis(500)) {
        assert!(!is_agent(&event), "{event:?}");
    }
    wait_until("the state is Exited", || term.state() == Exited);
    assert!(!term.blocking());
    assert_eq!(term.activity(), Activity::Unknown);
}

#[test]
fn a_turn_with_a_spinner_title_is_working_then_idle() {
    let (term, events, _home) = shell_with("progress", &["--title"], long_window());
    term.launch().unwrap();
    assert_eq!(term.activity(), Activity::Unknown);
    states_until(&term, &events, LAUNCH_WAIT, is_ready);
    term.terminal().write(b"hello\r").unwrap();
    activity_until(&term, &events, WAIT, Activity::Working);
    // The reply comes 300 ms later, the progress clear 400 ms after the submit; idle
    // follows after the debounce.
    let after = activity_until(&term, &events, WAIT, Activity::Idle);
    assert_eq!(after, [Activity::Idle]);
    assert_eq!(term.activity(), Activity::Idle);
}

#[test]
fn the_shell_exiting_ends_the_agent_terminal() {
    let (term, events, _home) = shell_with("ready", &[], Timing::default());
    term.terminal().write(b"exit\n").unwrap();
    let seen = states_until(&term, &events, WAIT, |s, _| s == Exited);
    assert_eq!(seen, [(Exited, false)]);
    assert_eq!(term.state(), Exited);
    assert_eq!(term.activity(), Activity::Idle);
    assert!(matches!(term.launch(), Err(Error::Closed)));
}
