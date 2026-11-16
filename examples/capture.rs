//! Capture a command's PTY session: run it in a pseudo-terminal with a chosen terminal
//! identity, record the raw output stream, log every signal a host could key on (window
//! title, OSC 9;4 progress, OSC 133 marks, DEC 2026 sync frames, terminal queries) with
//! timestamps, drive the child from a timed script, and write a fixture set.
//!
//! Standalone over `portable-pty` and `vt100`; it uses no API of this crate.
//!
//! ```text
//! cargo run -p agent-link --example capture -- --cwd <dir> (--cmd <program> [--arg <a>]… | --shell-line "<line>") [options]
//!
//! --cmd <program>          direct mode: exec the program; one --arg per argument (no splitting)
//! --arg <a>                an argument for --cmd (repeatable)
//! --shell-line "<line>"    shell mode: spawn $SHELL -l (unix) / %COMSPEC% (Windows), wait
//!                          --shell-settle ms, then type <line> and Enter
//! --shell-settle <ms>      default 1500; only with --shell-line
//! --cwd <dir>              working directory (required)
//! --identity <id>          every identity first removes the inherited terminal, multiplexer
//!                          and agent-session variables (list below)
//!                          honest (default): + TERM=xterm-256color, COLORTERM=truecolor
//!                          vscode: honest + TERM_PROGRAM=vscode, TERM_PROGRAM_VERSION=--term-version
//!                          custom:K=V,K=V: the stripped environment without TERM and COLORTERM,
//!                          plus the given pairs (so custom:TERM=xterm-256color matches an app
//!                          not started from a terminal, whatever terminal runs the capture);
//!                          values cannot contain `,`
//! --term-version <v>       TERM_PROGRAM_VERSION for vscode, default 1.104.0; only with
//!                          --identity vscode
//! --size <rows>x<cols>     default 40x120
//! --duration <ms>          stop after this long, then kill the tree; default 20000, and no
//!                          deadline with --interactive (it ends on child exit or Ctrl-])
//! --script "<ms>:<payload>;…"
//!                          timed writes, offsets from spawn, run in order (an offset smaller
//!                          than the one before it is an error). Payload escapes:
//!                          \r \n \t \e \xHH (exactly two hex digits) \u{H…} \\ \; \{ \};
//!                          tokens {enter} {up} {down} {esc} {ctrl-c} {paste:<text>} (wrapped in
//!                          ESC[200~ … ESC[201~) {wait-text:<needle>} (block until the screen
//!                          shows it, max 30 s; on a miss the rest of the script is dropped
//!                          and an `error` event says so, so later keys never answer a screen
//!                          they were not written for)
//! --answer-queries         reply to DA1, DSR 5, CPR and DECRQM 2026; on by default, as there
//!                          is no renderer here to answer them
//! --no-answer-queries      leave every query unanswered
//! --out <prefix>           write <prefix>.bin (raw stream), .events.jsonl, .txt (final screen)
//!                          and .meta (rows, cols, identity, agent, version, scene); a file
//!                          prefix in an existing directory, not a directory or `dir/`
//! --agent <id>  --agent-version <v>  --scene <s>
//!                          .meta values (no `=`, line break or other control character);
//!                          default: parsed from the --out file name (fixture naming rule
//!                          below). With --sanitize a name that does not follow the rule is an
//!                          error unless all three are given; without it the missing values
//!                          stay empty (with a warning). Without --out they are errors
//! --sanitize               replace the home path with /home/u, then the home directory's
//!                          name (when 3+ characters; it can differ from the login) and the
//!                          login name with u, in the stored stream, events and every .meta
//!                          value; .txt is replayed from the stored stream. Afterwards all four
//!                          files are searched for the home path and both names, and a warning
//!                          names each file that still has one (e.g. split by escape sequences
//!                          in the raw stream). Names are replaced as plain substrings, so a
//!                          short login also rewrites unrelated words; the hostname (prompts,
//!                          OSC 7) is not replaced — read every file before committing it
//! --interactive            unix only: raw local tty, stdin forwarded to the child, its output
//!                          shown; Ctrl-] ends the capture; events are not echoed. The real
//!                          terminal answers the child's queries on stdin: those replies are
//!                          dropped before forwarding (each logged as `stdin_reply_dropped`).
//!                          Trade-off: Ctrl/Alt/Shift-F3 as `ESC [ 1 ; <mod> R` has the shape
//!                          of a cursor position report and is dropped too. On exit the
//!                          cursor, mouse, focus, paste, keyboard and alt-screen modes the
//!                          child may have left set are reset
//! --quiet                  do not echo events to stderr
//! ```
//!
//! Single-value options may be given once; `--cmd` with `--shell-line`, `--answer-queries`
//! with `--no-answer-queries`, and options that would be ignored are errors.
//!
//! Fixtures are named `tests/fixtures/<agent>-<version>-<scene>`: the agent has no `-`, the
//! version starts with a digit and has no `-` (dots, e.g. `2.1.282`), the scene is the rest
//! and may contain `-` (`claude-2.1.282-bypass-warning`). Record them at 40x120 with
//! `--sanitize`. The four files are written to temp siblings and renamed into place only
//! after all writes succeeded, so a failed write never mixes old and new files (a rename
//! failing partway still can, and says which files were replaced).
//!
//! Numeric limits: milliseconds (`--duration`, `--shell-settle`, script offsets) up to
//! 86 400 000; `--size` sides 1..=1000.
//!
//! Stripped by every identity (inherited only; pairs given to `custom:` are kept):
//! terminal and multiplexer markers `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`,
//! `TERM_SESSION_ID`, `TERM_FEATURES`, `TERMINAL_EMULATOR`, `TERMINFO`, `COLORFGBG`,
//! `VTE_VERSION`, `LC_TERMINAL`, `LC_TERMINAL_VERSION`, `TMUX`, `ZELLIJ`, `STY`, `WINDOW`,
//! `TILIX_ID` and every `TMUX_*`, `ZELLIJ_*`, `ITERM*`, `WT_*`, `KITTY_*`, `GHOSTTY_*`,
//! `WEZTERM_*`, `KONSOLE_*`, `ALACRITTY_*`, `GNOME_TERMINAL_*`, `XTERM_*`, `TERMINATOR_*`,
//! `VSCODE_*`, `WARP_*`, `CMUX_*`, `HERDR_*`; agent-session markers a parent agent sets for
//! its commands `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SSE_PORT`,
//! `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_SESSION_ATTENDED`,
//! `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_EXECPATH`,
//! `CLAUDE_PID`, `CLAUDE_EFFORT`, `CODEX_SANDBOX`, `CODEX_SANDBOX_NETWORK_DISABLED`,
//! `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_ESCALATE_SOCKET`, `OPENCODE`, `OPENCODE_PID`.
//! Kept: configuration passed on purpose (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_*`, API
//! keys, other `CLAUDE_CODE_*` / `CODEX_*` settings).
//!
//! Event lines (`.events.jsonl`, also echoed to stderr): `{"t_ms":…,"kind":…,…}` with kinds
//! `title` (`value`), `progress` (`state`, `name`, `pct`; `state`/`pct` null when missing or
//! not a number, then `name` is `unknown` and `raw` holds the params), `osc133` (`mark`,
//! `params`), `osc` (any other OSC, e.g. OSC 9 / 777 notifications, OSC 7 cwd, OSC 52 copy:
//! `params` joined with `;`, cut to 256 bytes, `len` = full length when cut), `bell`,
//! `frame_begin`, `frame_end` (vt100 reports `?2026;<mode>h/l` once per mode it does not
//! know, so a combined sequence logs the frame event more than once), `query` (`which`: `da1`, `dsr`, `cpr`, `decrqm2026`, and the
//! never-answered `da2`, `xtversion`, `kitty_keyboard`, `osc10`, `osc11`, `osc52`, `other`;
//! `answered`: a reply goes to the child — the event is logged before the reply is queued,
//! so the `write` with source `reply` follows and confirms delivery, or an `error` says the
//! writer had stopped), `write` (`value` in payload notation, `source`: `script`, `stdin` or
//! `reply`; logged once the bytes reached the PTY), `stdin_reply_dropped` (`value`),
//! `wait_text` (`needle`, `found`), `exit` (`status`, `signal`, `killed`), `screen_reset`
//! (vt100 panicked; `offset` = start of the read chunk it panicked on, a byte offset into the
//! stored `.bin` — the stderr echo shows the raw-stream offset — and `message`), `error`
//! (`value`, `source` for a failed write).
//!
//! `query` `other` carries `raw`, the sequence after `ESC [` (an omitted param reads 0) or
//! `ESC ]`: DA3 `=0c` and other `c`/`n` reports (DECXCPR `?6n`, colour scheme `?996n`),
//! DECRQM for other modes (`?…$p`, `…$p`), XTQMODKEYS `?…m`, XTWINOPS reports (`14t`, …),
//! OSC requests ending in `?` (palette `4;<n>;?`). DCS requests (XTGETTCAP, DECRQSS) never
//! reach vt100's callbacks and are not logged.
//!
//! Teardown kills the PTY's foreground process group and the child's own group whether the
//! child exited by itself or hit the deadline, then reaps the child. The PTY writer is never
//! dropped: portable-pty's unix writer types Enter + EOF into the PTY on drop.

#[path = "capture/args.rs"]
mod args;
#[path = "capture/events.rs"]
mod events;
#[path = "capture/identity.rs"]
mod identity;
#[cfg(unix)]
#[path = "capture/replies.rs"]
mod replies;
#[path = "capture/sanitize.rs"]
mod sanitize;
#[path = "capture/sys.rs"]
mod sys;
#[path = "capture/tap.rs"]
mod tap;
#[path = "capture/text.rs"]
mod text;
#[path = "capture/threads.rs"]
mod threads;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use args::{HELP, Mode, Options};
use events::{EventLog, Val};
use sanitize::Sanitizer;
use tap::{LiveScreen, Tap};
use text::{Entry, Step};
use threads::{Outgoing, Shared, run_script, spawn_reader, spawn_writer};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("capture: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(o) = args::parse_args()? else {
        println!("{HELP}");
        return Ok(());
    };
    let sanitizer = if o.sanitize {
        let (home, login) = sys::home_and_login();
        Some(Sanitizer::new(&home, &login)?)
    } else {
        None
    };

    let (program, args) = match &o.mode {
        Mode::Direct { program, args } => (program.clone(), args.clone()),
        Mode::Shell { .. } => sys::login_shell(),
    };
    let mut cmd = CommandBuilder::new(&program);
    cmd.args(&args);
    cmd.cwd(&o.cwd);
    identity::apply(&mut cmd, &o.identity, &o.term_version)?;
    // Before the spawn, so a failure here never leaves a child running unsupervised.
    #[cfg(unix)]
    let raw_tty = o.interactive.then(sys::RawTty::enable).transpose()?;

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: o.rows,
            cols: o.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e:#}"))?;
    let log = Arc::new(EventLog::new(o.quiet || o.interactive, sanitizer.clone()));
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {program:?}: {e:#}"))?;
    drop(pair.slave);
    let master = pair.master;
    let reader = master
        .try_clone_reader()
        .map_err(|e| format!("pty reader: {e:#}"))?;
    let writer = master
        .take_writer()
        .map_err(|e| format!("pty writer: {e:#}"))?;

    let (tx, rx) = mpsc::channel::<Outgoing>();
    spawn_writer(writer, rx, log.clone())?;

    let stream = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::new(Mutex::new(Shared::default()));
    let tap = Tap::new(log.clone(), o.answer.then(|| tx.clone()));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let screen = LiveScreen::new(o.rows, o.cols, tap);
    spawn_reader(
        reader,
        screen,
        stream.clone(),
        shared.clone(),
        o.interactive,
        done_tx,
    )?;

    let mut script = o.script.clone();
    if let Mode::Shell { line } = &o.mode {
        let typed = Entry {
            at: o.shell_settle,
            steps: vec![Step::Write(format!("{line}{}", sys::LINE_END).into_bytes())],
        };
        let at = script
            .iter()
            .position(|e| e.at > o.shell_settle)
            .unwrap_or(script.len());
        script.insert(at, typed);
    }
    let start = log.start();
    {
        let (log, tx, shared) = (log.clone(), tx.clone(), shared.clone());
        thread::Builder::new()
            .name("capture-script".into())
            .spawn(move || run_script(&script, start, &tx, &shared, &log))
            .map_err(|e| format!("script thread: {e}"))?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    if o.interactive {
        threads::spawn_stdin(tx.clone(), stop.clone(), log.clone())?;
    }
    drop(tx);

    let deadline = o.duration.map(|d| start + d);
    if o.interactive {
        match o.duration {
            Some(d) => eprint!("capture: stops after {} ms or on Ctrl-]\r\n", d.as_millis()),
            None => eprint!("capture: no deadline; stops when the child exits or on Ctrl-]\r\n"),
        }
    }
    let mut exited = false;
    while deadline.is_none_or(|d| Instant::now() < d) && !stop.load(Ordering::Relaxed) {
        match sys::exited(child.as_mut()) {
            Ok(true) => {
                exited = true;
                break;
            }
            Ok(false) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                log.emit("error", &[("value", Val::Str(format!("wait: {e}")))]);
                break;
            }
        }
    }
    // Always, also after a clean exit: descendants in the foreground group can outlive the
    // child and keep the slave open. The child is not reaped yet, so its pid is still ours.
    sys::kill_tree(child.as_mut(), master.as_ref());
    reap(child.as_mut(), &log, !exited);
    // The reader ends when nothing holds the slave any more. On unix it reads a dup of the
    // master fd, so dropping the master does not end it: the read fails once the killed
    // tree's last slave fd closes. On Windows dropping the master closes the pseudoconsole,
    // which ends the output pipe. Give it a moment to drain what the child wrote last.
    drop(master);
    let drained = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    #[cfg(unix)]
    drop(raw_tty);
    if !drained {
        eprintln!("capture: the reader did not finish; the stream may miss its last bytes");
    }

    let raw = stream.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let live_text = shared
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .text
        .clone();
    if let Some(out) = &o.out {
        write_files(out, &o, &raw, live_text, &log, sanitizer.as_ref())?;
    } else if !o.quiet {
        eprint!("{live_text}");
    }
    Ok(())
}

/// Reaps the child (up to 2 s) and logs its `exit`.
fn reap(child: &mut dyn portable_pty::Child, log: &EventLog, killed: bool) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return log_exit(log, &status, killed),
            Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let msg = "the child did not exit within 2 s of the kill".to_string();
                return log.emit("error", &[("value", Val::Str(msg))]);
            }
            Err(e) => return log.emit("error", &[("value", Val::Str(format!("reap: {e}")))]),
        }
    }
}

fn log_exit(log: &EventLog, status: &portable_pty::ExitStatus, killed: bool) {
    let signal = status
        .signal()
        .map_or(Val::Null, |s| Val::Str(s.to_string()));
    log.emit(
        "exit",
        &[
            ("status", Val::Num(i64::from(status.exit_code()))),
            ("signal", signal),
            ("killed", Val::Bool(killed)),
        ],
    );
}

fn with_ext(prefix: &Path, ext: &str) -> PathBuf {
    let mut s = prefix.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

/// Writes every file to a temp sibling first and renames them into place only once all were
/// written, so a failed write leaves the previous fixture set as it was (not a mix of old and
/// new files). Only a failed rename can still leave a mix; its error says so.
fn store(out: &Path, files: &[(&str, &[u8])]) -> Result<(), String> {
    let tmp = |ext: &str| with_ext(out, &format!("{ext}.tmp-{}", std::process::id()));
    for (i, (ext, bytes)) in files.iter().enumerate() {
        if let Err(e) = std::fs::write(tmp(ext), bytes) {
            for (ext, _) in &files[..=i] {
                let _ = std::fs::remove_file(tmp(ext));
            }
            return Err(format!("write {}: {e}", with_ext(out, ext).display()));
        }
    }
    for (i, (ext, _)) in files.iter().enumerate() {
        let path = with_ext(out, ext);
        if let Err(e) = std::fs::rename(tmp(ext), &path) {
            for (ext, _) in &files[i..] {
                let _ = std::fs::remove_file(tmp(ext));
            }
            return Err(format!(
                "rename into {}: {e}; the files before it are already replaced",
                path.display()
            ));
        }
    }
    Ok(())
}

fn write_files(
    out: &Path,
    o: &Options,
    raw: &[u8],
    live_text: String,
    log: &EventLog,
    sanitizer: Option<&Sanitizer>,
) -> Result<(), String> {
    let (stream, screen) = match sanitizer {
        Some(san) => {
            let stream = san.apply_bytes(raw);
            let (screen, panics) = tap::replay_text(&stream, o.rows, o.cols);
            if panics > 0 {
                eprintln!("capture: vt100 panicked {panics} time(s) replaying the stored stream");
            }
            (stream, screen)
        }
        None => (raw.to_vec(), live_text),
    };
    let clean = |v: &str| sanitizer.map_or_else(|| v.to_string(), |s| s.apply_str(v));
    let identity = clean(&o.identity);
    let [agent, version, scene] = o.meta.each_ref().map(|v| clean(v));
    let meta = format!(
        "rows={}\ncols={}\nidentity={identity}\nagent={agent}\nversion={version}\nscene={scene}\n",
        o.rows, o.cols
    );
    // `screen_reset` offsets were taken on the raw stream; store them as offsets into `.bin`.
    let offsets = |off: usize| sanitizer.map_or(off, |s| s.map_offset(raw, off));
    let lines = log.lines(Some(&offsets));
    let mut events = lines.join("\n");
    if !events.is_empty() {
        events.push('\n');
    }
    let files: [(&str, &[u8]); 4] = [
        (".bin", &stream),
        (".events.jsonl", events.as_bytes()),
        (".txt", screen.as_bytes()),
        (".meta", meta.as_bytes()),
    ];
    store(out, &files)?;
    if let Some(san) = sanitizer {
        let leaky: Vec<String> = files
            .iter()
            .filter(|(_, bytes)| san.leaks(bytes))
            .map(|(ext, _)| with_ext(out, ext).display().to_string())
            .collect();
        if !leaky.is_empty() {
            eprintln!(
                "capture: warning: the home path, home directory name or login name is still in {}; read them before committing",
                leaky.join(", ")
            );
        }
    }
    if !o.quiet {
        eprintln!(
            "capture: {} bytes, {} events -> {}.{{bin,events.jsonl,txt,meta}}",
            stream.len(),
            lines.len(),
            out.display()
        );
    }
    Ok(())
}
