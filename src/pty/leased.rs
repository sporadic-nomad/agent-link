//! Prepare a controlled direct spawn before taking the writer lease's admission guard.
//! portable-pty's Unix spawn resolves the shell (and can log) inside spawn_command.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::process::Command;

use portable_pty::{CommandBuilder, MasterPty};

use super::{SpawnMode, SpawnSpec};

pub(super) fn prepare(
    spec: &SpawnSpec,
    master: &dyn MasterPty,
    builder: &CommandBuilder,
) -> io::Result<Command> {
    let SpawnMode::Direct { program, args } = &spec.mode else {
        return Err(io::Error::other("leased spawn requires a direct program"));
    };
    let mut command = Command::new(program);
    command.args(args).current_dir(&spec.cwd);
    if spec.env.inherit {
        command.env_clear().envs(std::env::vars_os());
    }
    super::session::apply_env(spec, &mut command);
    // portable-pty sets the resolved shell as a default, then overlays its complete
    // environment. Even a non-executable or non-UTF-8 explicit SHELL reaches the child.
    let shell = builder.get_shell();
    command.env(
        "SHELL",
        builder.get_env("SHELL").unwrap_or(OsStr::new(&shell)),
    );

    // The pair still owns both ends, so the kernel cannot recycle this slave name.
    // O_NOCTTY keeps the host independent of the newly opened terminal.
    let name = master
        .tty_name()
        .ok_or_else(|| io::Error::other("missing PTY slave name"))?;
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(name)?;
    command
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);

    // SAFETY: after fork only signal/TTY syscalls and Darwin's fork-safe descriptor
    // enumeration run. All inherited descriptors above stdio close on successful exec,
    // including std's own error pipe which must stay usable when exec fails.
    unsafe {
        command.pre_exec(|| {
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut empty = 0;
            if libc::sigemptyset(&mut empty) == -1
                || libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) == -1
                || libc::setsid() == -1
                || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) == -1
            {
                return Err(io::Error::last_os_error());
            }
            close_fds::set_fds_cloexec(3, &[]);
            Ok(())
        });
    }
    Ok(command)
}
