//! Platform pieces of the capture tool: the login shell, tree kill, and the local tty's raw
//! mode for interactive captures.

use portable_pty::{Child, MasterPty};

/// What ends the typed shell line: Enter as a unix tty sees it, `\r` for cmd.exe.
#[cfg(unix)]
pub const LINE_END: &str = "\n";
/// What ends the typed shell line: Enter as a unix tty sees it, `\r` for cmd.exe.
#[cfg(windows)]
pub const LINE_END: &str = "\r";

/// The shell for `--shell-line`: `$SHELL -l` (fallback `/bin/sh -l`).
#[cfg(unix)]
pub fn login_shell() -> (String, Vec<String>) {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    (shell, vec!["-l".into()])
}

/// The shell for `--shell-line`: `%COMSPEC%` (fallback `cmd.exe`).
#[cfg(windows)]
pub fn login_shell() -> (String, Vec<String>) {
    let shell = std::env::var("COMSPEC")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cmd.exe".into());
    (shell, Vec::new())
}

/// The user's home directory and login name, for `--sanitize`.
pub fn home_and_login() -> (String, String) {
    let var = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default()
    };
    (
        var(&["HOME", "USERPROFILE"]),
        var(&["USER", "LOGNAME", "USERNAME"]),
    )
}

/// Whether the child has exited, without reaping it: while it is an unreaped zombie its pid
/// (and so its process group id) cannot be reused, so `kill_tree` stays safe afterwards.
#[cfg(unix)]
pub fn exited(child: &mut dyn Child) -> std::io::Result<bool> {
    let Some(pid) = child.process_id() else {
        return child.try_wait().map(|s| s.is_some());
    };
    // SAFETY: siginfo_t is plain data; waitid fills it (si_pid stays 0 while the child runs,
    // si_code says why it is waitable).
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: `info` is a valid out-pointer; WNOWAIT leaves the child waitable.
    if unsafe { libc::waitid(libc::P_PID, pid, &mut info, flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded, so `info` is initialised.
    let pid_set = unsafe { info.si_pid() } != 0;
    // Only an exit counts: some kernels (macOS among them) can report a stopped or continued
    // child here even though only WEXITED was asked for.
    let gone = matches!(
        info.si_code,
        libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
    );
    Ok(pid_set && gone)
}

/// Whether the child has exited (its process handle stays valid after this).
#[cfg(windows)]
pub fn exited(child: &mut dyn Child) -> std::io::Result<bool> {
    child.try_wait().map(|s| s.is_some())
}

/// Kills the PTY's foreground process group, then the child's own group (it leads a new
/// session), then the child itself. Call it before the child is reaped. A descendant that
/// moved to another process group and left the foreground survives.
#[cfg(unix)]
pub fn kill_tree(child: &mut dyn Child, master: &dyn MasterPty) {
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() };
    let mut groups = Vec::new();
    if let Some(fg) = master.process_group_leader() {
        groups.push(fg);
    }
    if let Some(pid) = child
        .process_id()
        .and_then(|p| libc::pid_t::try_from(p).ok())
    {
        groups.push(pid);
    }
    for group in groups {
        if group > 1 && group != own {
            // SAFETY: plain syscall; a negative pid addresses a process group we spawned.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Kills the child (ConPTY closes its console clients when the master drops); a no-op for
/// a child that already exited.
#[cfg(windows)]
pub fn kill_tree(child: &mut dyn Child, _master: &dyn MasterPty) {
    let _ = child.kill();
}

/// The local tty in raw mode; restores the saved mode on drop (also when unwinding).
#[cfg(unix)]
pub struct RawTty {
    saved: libc::termios,
}

#[cfg(unix)]
impl RawTty {
    /// Puts stdin's tty in raw mode.
    pub fn enable() -> Result<Self, String> {
        // SAFETY: termios is plain data; tcgetattr fills it before it is read.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `saved` is a valid out-pointer; a non-terminal fd 0 makes the call fail.
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err(format!("tcgetattr: {}", std::io::Error::last_os_error()));
        }
        let mut raw = saved;
        // SAFETY: `raw` is an initialised termios.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: fd 0 is a terminal and `raw` is a valid termios.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(format!("tcsetattr: {}", std::io::Error::last_os_error()));
        }
        Ok(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for RawTty {
    fn drop(&mut self) {
        // The child's output was echoed here, and the child is killed before it can undo its
        // modes: show the cursor; mouse, focus and bracketed-paste reporting off; pop the
        // kitty keyboard flags; leave the alt screen; plain attributes.
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(
            b"\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[<u\x1b[?1049l\x1b[0m",
        );
        let _ = out.flush();
        // SAFETY: restores the termios read from the same fd in `enable`.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}
