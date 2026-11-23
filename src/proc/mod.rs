//! Child-process hygiene for `std::process` spawns — three rules every such spawn
//! needs.
//!
//! **1. Whole-TREE kill** ([`kill_tree`]; `kill_process_tree` on Windows). On Windows,
//! [`crate::resolve`] resolves an npm-installed launcher to a `.cmd` shim, so std
//! spawns `cmd.exe /d /c "…claude.cmd" …` and the real CLI is a `node.exe`
//! **grandchild**; `Child::kill` is a `TerminateProcess` on the wrapper alone and
//! nothing else reaps below it. On unix every kill targets the process GROUP
//! (`kill(-pid, SIGKILL)`), which already reaches grandchildren.
//!
//! **2. No console WINDOW** (`no_console_window`, Windows) — mandatory on every
//! `std::process::Command` a GUI host spawns on Windows; see that function.
//!
//! **3. Absolute System32 paths** ([`system32_program`]) — a Windows-shipped console
//! tool is spawned by its `%SystemRoot%\System32` path, never by bare name through
//! the ambient `PATH`.
//!
//! [`output_with_timeout`] (and `status_with_timeout` on Windows) run a CLI to completion under a
//! deadline and apply all three. They block their caller; synchronous spawn and cleanup
//! can add time beyond the deadline (see each helper's teardown limits).
//!
//! ```no_run
//! use std::process::Command;
//! use std::time::Duration;
//!
//! use agent_link::proc::output_with_timeout;
//!
//! let mut cmd = Command::new("codex");
//! cmd.arg("--version");
//! match output_with_timeout(cmd, Duration::from_secs(8))? {
//!     Some(out) => println!("{}", String::from_utf8_lossy(&out.stdout)),
//!     None => eprintln!("timed out: its process group was killed"),
//! }
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Deliberately **std-only** (plus `libc` on unix), so the Windows branches can be
//! type-checked for a Windows target from a non-Windows host. The exceptions are the
//! crate-internal `job` module (Windows Job Object and process snapshots, `windows-sys`),
//! which PTY sessions use, and the crate-internal liveness check `pid_alive` (`turns`),
//! which the ambient transcript lookups use.

#[cfg(all(unix, feature = "agent", any(test, feature = "turns")))]
pub(crate) mod instance;
#[cfg(any(windows, test))]
pub(crate) mod job;

/// `CREATE_NO_WINDOW` (`processthreadsapi.h`) — create a console child with **no**
/// console at all.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Apply `CREATE_NO_WINDOW` to `cmd`. **MANDATORY on every `std::process::Command`
/// a GUI host spawns on Windows** — a new spawn added without it regresses the
/// shipped app, invisibly in a debug build.
///
/// Why: a release GUI binary is usually `windows_subsystem = "windows"`, so it owns no
/// console. When a console-subsystem child is created from a console-less parent and
/// no flag says otherwise, Windows **allocates a new console window** for it — a black
/// window in the taskbar whose close button kills the child tree. Most Windows std
/// spawns are console-subsystem: the `cmd.exe` wrapper std uses for a `.cmd` shim,
/// `taskkill.exe`, a CLI's own `.exe`. So: a persistent window per long-lived child
/// (or per launched editor, whose launcher may BE the editor), and a flash per
/// teardown and per short probe.
///
/// Why it is invisible in development: a debug binary is typically console-subsystem,
/// children inherit *its* console, and nothing is allocated. Only the shipped build
/// shows the bug.
///
/// Not needed for PTY children: `portable_pty`'s ConPTY backend spawns with
/// `EXTENDED_STARTUPINFO_PRESENT` + the pseudoconsole thread attribute, so those
/// attach to the pseudoconsole instead of allocating a console.
///
/// `creation_flags` REPLACES the flag word rather than adding to it, which is safe
/// here: std documents (and does) `flags | CREATE_UNICODE_ENVIRONMENT`, and this is
/// the only creation flag these spawns set.
#[cfg(windows)]
pub fn no_console_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// `taskkill /T /F /PID <pid>` — a whole-tree kill with std alone, no new dependency.
///
/// Best-effort: a failure leaves the caller's `Child::kill` as the fallback (the
/// wrapper dies, a grandchild may survive), which is worth a `warn` rather than
/// silence — a wedged child is exactly the one a kill exists for.
///
/// **Blocks** on the `taskkill` process (`.status()`): one subprocess round-trip per
/// call.
///
/// Caller contract: only while the child is still unreaped, so `pid` cannot have
/// been recycled onto an unrelated process. `taskkill` reports a non-zero status for
/// an unknown pid, so a call for a child that already exited logs a spurious warning
/// — see [`kill_tree`] for when to skip it.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    const LEAK: &str = "a grandchild may survive";
    let program = system32_program("taskkill");
    let mut cmd = std::process::Command::new(&program);
    cmd.args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // `taskkill.exe` is a console app: without this it flashes a console window on
    // every teardown and probe timeout.
    no_console_window(&mut cmd);
    let status = cmd.status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => log::warn!(
            "[proc-kill] taskkill /T /F /PID {pid} exited {:?} — {LEAK}",
            s.code()
        ),
        Err(e) => log::warn!(
            "[proc-kill] could not run {} for pid {pid}: {e} — {LEAK}",
            program.display()
        ),
    }
}

/// Kill `pid` and every descendant: SIGKILL to the process GROUP on unix,
/// `kill_process_tree` (`taskkill /T /F`) on Windows. Does not reap — the caller
/// still `wait`s its child afterwards.
///
/// **unix contract:** `pid` leads its own process group (spawned with `setsid` or
/// `process_group(0)`) and is still unreaped, so `-pid` names that group and cannot
/// have been recycled. A descendant that moved to another group survives.
///
/// **Windows: skip it for a CONFIRMED-exited child.** `taskkill` reports a non-zero
/// status for an unknown pid, so an unconditional call warns about a possible leak on
/// every teardown of a child that had already exited — the common case for a shell
/// the user `exit`ed. Check `try_wait` first and skip only on `Ok(Some(_))`: a
/// `try_wait` error (or an unknown state) is not a confirmation and must still take
/// the tree down — that is exactly the state a surviving grandchild is most likely
/// in. A pid getter is no substitute for the check (`portable_pty`'s
/// `WinChild::process_id` answers `Some` for an exited child, it is `GetProcessId` on
/// a still-open handle). The unreaped child holds the handle, so the pid cannot have
/// been recycled. A child that exits between the check and the `taskkill` still
/// produces one spurious warning; the two cannot be atomic, so that race is accepted.
///
/// A unix `pid` of 0 or 1, or one that does not fit `pid_t`, is ignored.
///
/// **Blocks** on Windows (one `taskkill` round-trip); a single syscall on unix.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // `kill(0)` would hit the caller's own group, `kill(-1)` every process it may signal,
        // and a pid past `i32::MAX` has no negation: none of them names a child's group.
        let Ok(group) = libc::pid_t::try_from(pid) else {
            return;
        };
        if group <= 1 {
            return;
        }
        // SAFETY: a plain `kill(2)` on a process group the caller owns (see the contract).
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    kill_process_tree(pid);
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

/// Whether a process `pid` exists now: one this user may not signal (Unix) or open
/// (Windows) counts. A pid can be reused once its process is reaped, so this proves only
/// that *a* process has the number; callers compare a recorded start time too. Unix:
/// `kill(pid, 0)`; Windows: a query-only handle and its exit code (a process that exited
/// with code 259, `STILL_ACTIVE`, reads as running). Never signals or waits. Pid 0 is
/// never alive. Other platforms: never alive.
#[cfg(feature = "turns")]
pub(crate) fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // 0 and negative pids name process groups for `kill`.
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 is not sent; `kill` only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        if pid == 0 {
            return false;
        }
        // SAFETY: a query-only, non-inheritable handle; nothing else is passed.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_ACCESS_DENIED as i32);
        }
        // SAFETY: `raw` is a fresh handle nothing else owns; dropping it closes it.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut code = 0u32;
        // SAFETY: a live process handle and a valid out pointer.
        let ok = unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) } != 0;
        ok && code == STILL_ACTIVE as u32
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Absolute path to a Windows-shipped console tool: `stem` →
/// `%SystemRoot%\System32\<stem>.exe`.
///
/// NOT `Command::new("<stem>")`: a bare name is resolved through the ambient `PATH`,
/// where an earlier-listed same-named `.exe` would win — and these are the tools that
/// kill process trees (`taskkill`) or report who the user is (`whoami`).
///
/// Compiled on every platform so the composition is testable off Windows; spawn the
/// result only on Windows. Off Windows `SystemRoot` is normally unset, so this returns
/// the bare `stem`.
pub fn system32_program(stem: &str) -> std::path::PathBuf {
    system32_program_in(stem, std::env::var_os("SystemRoot"))
}

/// [`system32_program`] against an EXPLICIT `%SystemRoot%`. Threaded rather than
/// read from the env so tests need not mutate process-global state.
///
/// Falls back to the bare `stem` when `SystemRoot` is unset/empty — worse than an
/// absolute path, better than not running the tool at all.
fn system32_program_in(stem: &str, system_root: Option<std::ffi::OsString>) -> std::path::PathBuf {
    match system_root.filter(|r| !r.is_empty()) {
        Some(root) => std::path::PathBuf::from(root)
            .join("System32")
            .join(format!("{stem}.exe")),
        None => std::path::PathBuf::from(stem),
    }
}

#[cfg(any(unix, windows))]
mod output;

/// Run `cmd`, capturing stdout and stderr, with a total deadline. Returns `Ok(None)`
/// when the process or either output stream does not finish before the deadline.
/// Spawn, reader-thread startup, read and wait failures are returned as I/O errors.
/// An unrepresentable timeout returns [`std::io::ErrorKind::InvalidInput`] before spawn.
///
/// Both streams are drained concurrently. Any piped stdin handle is closed after
/// spawning. Captured output is not size-limited; use the bounded probe or RPC APIs
/// for untrusted output.
/// The caller retains the child until the timeout/signalling decision is complete:
/// pipe-reader completion never reaps it or releases its process identity.
///
/// On Unix the command starts in its own session (`setsid`), so an interactive login
/// shell is not stopped by background-terminal signals and its process group can be
/// killed on timeout. Descendants that escape that group are not contained. Teardown
/// checks for external reaping before signalling and skips a known-reaped PID. Hosts
/// with their own child reaper must still coordinate with this call: a PID check and
/// a later signal cannot be atomic.
///
/// Windows applies `no_console_window` and uses `kill_process_tree` before killing
/// the direct child; an open process handle preserves identity during that decision.
/// Tree cleanup is best effort, and an already-exited root is not passed to taskkill.
///
/// The deadline starts before the synchronous spawn call. Spawn itself cannot be
/// interrupted. Timeout/error cleanup allows up to two additional seconds to reap,
/// plus the Windows taskkill round trip. An unresponsive child is handed to a detached
/// reaper; escaped descendants can retain detached output readers until closing their
/// pipes. No reader or reaper is joined on a timeout.
#[cfg(any(unix, windows))]
pub fn output_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and preserves group ownership for teardown.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    no_console_window(&mut cmd);
    output::capture(cmd, timeout)
}

/// Run `cmd` to completion within `timeout`, discarding its output.
/// `Ok(None)` ⇒ it was still running at the deadline and has been killed + reaped.
///
/// The deadline is computed before spawning, so spawn time counts toward the timeout
/// budget (the OS spawn call itself is synchronous and cannot be interrupted here).
/// A timeout that cannot be represented by [`std::time::Instant`] returns
/// [`std::io::ErrorKind::InvalidInput`] without
/// starting the command. A zero timeout is valid: after spawning, an already exited
/// child returns its status; otherwise teardown starts immediately.
/// Spawn and wait failures are returned as I/O errors.
///
/// **Both streams are `null`.** A caller that reads the exit status alone would need a
/// piped stdout purely to avoid a fill-stall — and paying for that needs a reader
/// thread that blocks in `read_to_end` past the deadline (a `.cmd` shim's `node.exe`
/// grandchild can hold the write end), leaking one thread + pipe handle per timed-out
/// run. With no pipe there is nothing to drain and nothing to stall on.
///
/// **The kill is a whole-TREE kill** ([`kill_process_tree`]), then `Child::kill` +
/// `wait`. Running `<abs>\claude.cmd --version` makes std spawn a `cmd.exe` wrapper
/// with the real CLI as a `node.exe` GRANDCHILD: `Child::kill` alone would take the
/// wrapper and leave the grandchild — which, since the kill happens BECAUSE it hung,
/// is the hung process and would outlive us indefinitely, one per repeated probe. Only
/// `taskkill` itself failing leaves a grandchild now (warned there). The unix
/// `output_with_timeout` kills the process GROUP for the same reason. Applies
/// [`no_console_window`].
#[cfg(windows)]
pub fn status_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(25);

    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process timeout exceeds the supported deadline range",
            )
        })?;
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Console-subsystem child (`cmd.exe` wrapper or the CLI's own `.exe`) from a
    // console-less GUI: without this each run flashes a console window.
    no_console_window(&mut cmd);
    let mut child = cmd.spawn()?;
    // Safe to `taskkill` by pid at either kill site below: the child is unreaped
    // there, so its handle is still held and the pid cannot have been recycled.
    let pid = child.id();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            // Never propagate while the child is still ours to reap.
            Err(e) => {
                kill_process_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            kill_process_tree(pid);
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL.min(left));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on every platform: the `.exe` suffix, the `System32` component and the
    /// bare-stem fallback are plain path composition. Asserted component-wise
    /// because `PathBuf::join` uses the HOST separator, so the joined string differs
    /// off Windows while the components do not.
    #[test]
    fn system32_program_appends_exe_and_falls_back_to_the_bare_stem() {
        for stem in ["taskkill", "whoami"] {
            let p = system32_program_in(stem, Some(std::ffi::OsString::from(r"C:\Windows")));
            assert!(
                p.ends_with(format!("{stem}.exe")),
                "{stem}: leaf must carry .exe, got {}",
                p.display()
            );
            assert!(
                p.components().any(|c| c.as_os_str() == "System32"),
                "{stem}: must resolve under System32, got {}",
                p.display()
            );
            for unset in [None, Some(std::ffi::OsString::new())] {
                assert_eq!(
                    system32_program_in(stem, unset),
                    std::path::PathBuf::from(stem),
                    "{stem}: unset/empty SystemRoot must fall back to the bare stem"
                );
            }
        }
    }

    /// The exact absolute form, Windows-only because it asserts the `\` separator.
    #[cfg(windows)]
    #[test]
    fn taskkill_is_taken_from_system32_when_system_root_is_known() {
        assert_eq!(
            system32_program_in("taskkill", Some(std::ffi::OsString::from(r"C:\Windows"))),
            std::path::PathBuf::from(r"C:\Windows\System32\taskkill.exe")
        );
        assert_eq!(
            system32_program_in("whoami", Some(std::ffi::OsString::from(r"C:\Windows"))),
            std::path::PathBuf::from(r"C:\Windows\System32\whoami.exe")
        );
    }

    #[cfg(windows)]
    #[test]
    fn status_with_timeout_rejects_overflow_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let absent_program = dir.path().join("absent.exe");
        // A spawn attempt would return NotFound; invalid timeout validation must win.
        let error = status_with_timeout(
            std::process::Command::new(&absent_program),
            std::time::Duration::MAX,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        // Zero is a valid timeout, so it reaches the normal spawn-error path.
        let error = status_with_timeout(
            std::process::Command::new(&absent_program),
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn status_with_timeout_does_not_start_a_command_with_an_overflowing_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let command = || {
            let mut cmd = std::process::Command::new(system32_program("cmd"));
            cmd.args(["/d", "/c", "echo started>started & exit /b 7"])
                .current_dir(dir.path());
            cmd
        };
        let error = status_with_timeout(command(), std::time::Duration::MAX).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !marker.exists(),
            "a rejected timeout must not run the command"
        );

        // The same command is launchable with a valid timeout and keeps its exit status.
        let status = status_with_timeout(command(), std::time::Duration::from_secs(60))
            .unwrap()
            .expect("the command should finish before the deadline");
        assert_eq!(status.code(), Some(7));
        assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), "started");
    }

    #[cfg(windows)]
    #[test]
    fn status_with_timeout_zero_kills_a_running_command() {
        let mut cmd = std::process::Command::new(system32_program("ping"));
        cmd.args(["-n", "30", "127.0.0.1"]);
        assert!(
            status_with_timeout(cmd, std::time::Duration::ZERO)
                .unwrap()
                .is_none(),
            "a running command should time out immediately"
        );
    }

    #[cfg(feature = "turns")]
    #[test]
    fn pid_alive_knows_this_process_and_no_impossible_pid() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        // Above every platform's pid range.
        assert!(!pid_alive(999_999_999));
    }

    #[cfg(all(unix, feature = "turns"))]
    #[test]
    fn pid_alive_is_false_for_a_reaped_child() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Reaped: the number is free until the system reuses it.
        assert!(!pid_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_returns_fast_command_output() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "printf ok; printf diagnostic >&2; exit 7"]);
        // Generous: under the parallel tests a fresh `/bin/sh` has taken over 10 s to start
        // (an endpoint-security agent vetting each exec on a busy macOS host).
        let out = output_with_timeout(cmd, std::time::Duration::from_secs(60))
            .expect("spawn ok")
            .expect("must not time out");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
        assert_eq!(out.stderr, b"diagnostic");
        assert_eq!(out.status.code(), Some(7));
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_closes_caller_piped_stdin() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "cat; printf eof"])
            .stdin(std::process::Stdio::piped());
        let output = output_with_timeout(cmd, std::time::Duration::from_secs(60))
            .unwrap()
            .expect("stdin EOF must let the child finish");
        assert_eq!(output.stdout, b"eof");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn output_with_timeout_rejects_overflow_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let command = || std::process::Command::new(dir.path().join("absent-program"));
        let error = output_with_timeout(command(), std::time::Duration::MAX).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let error = output_with_timeout(command(), std::time::Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn output_with_timeout_captures_both_pipes_and_exit_status() {
        let mut cmd = std::process::Command::new(system32_program("cmd"));
        cmd.args(["/d", "/c", "echo output & echo diagnostic 1>&2 & exit /b 7"]);
        let output = output_with_timeout(cmd, std::time::Duration::from_secs(60))
            .unwrap()
            .expect("command must finish before its deadline");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "output");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "diagnostic");
        assert_eq!(output.status.code(), Some(7));
    }

    #[cfg(windows)]
    #[test]
    fn output_with_timeout_zero_kills_a_running_command() {
        let mut cmd = std::process::Command::new(system32_program("ping"));
        cmd.args(["-n", "30", "127.0.0.1"]);
        assert!(
            output_with_timeout(cmd, std::time::Duration::ZERO)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_kills_hung_command() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"]);
        let out =
            output_with_timeout(cmd, std::time::Duration::from_millis(200)).expect("spawn ok");
        assert!(out.is_none(), "a hung command must time out to None");
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_kills_descendant_holding_stdout() {
        let mut cmd = std::process::Command::new("/bin/sh");
        // Background a 30s sleep (inherits stdout), then the shell exits immediately.
        cmd.args(["-c", "sleep 30 & exit 0"]);
        let start = std::time::Instant::now();
        let out =
            output_with_timeout(cmd, std::time::Duration::from_millis(200)).expect("spawn ok");
        assert!(
            out.is_none(),
            "a descendant holding the pipe must still time out to None"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "must return long before the backgrounded 30s sleep would end"
        );
    }
}
