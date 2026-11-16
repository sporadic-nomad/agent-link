//! Bounded output capture for probes. Readers keep a prefix and drain the remainder.
//! The owner retains the Unix PID until teardown; no user callbacks run here.
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

pub(super) const STREAM_LIMIT: usize = 8 * 1024;

#[derive(Default)]
pub(super) struct Stream {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}
pub(super) struct Captured {
    pub code: Option<i32>,
    pub stdout: Stream,
    pub stderr: Stream,
}

fn reader(mut pipe: impl Read + Send + 'static) -> io::Result<Receiver<io::Result<Stream>>> {
    let (tx, rx) = sync_channel(1);
    std::thread::Builder::new()
        .name("agent-link-probe-output".into())
        .spawn(move || {
            let mut stream = Stream::default();
            let mut buf = [0; 4096];
            let result = loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break Ok(stream),
                    Ok(n) => {
                        let keep = n.min(STREAM_LIMIT - stream.bytes.len());
                        stream.bytes.extend_from_slice(&buf[..keep]);
                        stream.truncated |= keep < n;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(result);
        })?;
    Ok(rx)
}

struct Owner(Option<Child>);
impl Owner {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("owner holds child until drop")
    }
    fn exit(&mut self) -> io::Result<Option<Option<i32>>> {
        #[cfg(unix)]
        {
            crate::pty::peek_exit(self.child().id()).map(|exit| exit.map(|e| e.status))
        }
        #[cfg(not(unix))]
        {
            self.child().try_wait().map(|exit| exit.map(|e| e.code()))
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        #[cfg(unix)]
        {
            // Recheck ownership immediately before signalling. An external reaper can
            // still race this check; PID checks and signals are not atomic.
            if matches!(crate::pty::peek_exit(child.id()), Err(e) if e.raw_os_error() == Some(libc::ECHILD))
            {
                return;
            }
            crate::proc::kill_tree(child.id());
        }
        #[cfg(windows)]
        if !matches!(child.try_wait(), Ok(Some(_))) {
            crate::proc::kill_process_tree(child.id());
        }
        let _ = child.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(None) => {
                    let _ = std::thread::Builder::new()
                        .name("agent-link-probe-reaper".into())
                        .spawn(move || {
                            let _ = child.wait();
                        });
                    break;
                }
                _ => break,
            }
        }
    }
}

pub(super) fn capture(mut cmd: Command, timeout: Duration) -> io::Result<Option<Captured>> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and preserves group ownership for teardown.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    crate::proc::no_console_window(&mut cmd);
    let deadline = Instant::now() + timeout;
    let mut owner = Owner(Some(
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?,
    ));
    // Piped handles are guaranteed by the command construction above.
    let out = reader(owner.child().stdout.take().expect("piped stdout"))?;
    let err = reader(owner.child().stderr.take().expect("piped stderr"))?;
    let (mut stdout, mut stderr) = (None, None);
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if stdout.is_none() {
            stdout = out.try_recv().ok().transpose()?;
        }
        if stderr.is_none() {
            stderr = err.try_recv().ok().transpose()?;
        }
        if let (Some(stdout), Some(stderr)) = (&mut stdout, &mut stderr) {
            if let Some(code) = owner.exit()? {
                return Ok(Some(Captured {
                    code,
                    stdout: std::mem::take(stdout),
                    stderr: std::mem::take(stderr),
                }));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
