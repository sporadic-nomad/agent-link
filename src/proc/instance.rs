//! Kernel process-instance evidence for directly launched PTY transcript writers.

use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instance {
    pub(crate) pid: u32,
    started_us: u64,
    tty_device: u32,
}

impl Instance {
    pub(crate) fn started_ms(self) -> u64 {
        self.started_us / 1000
    }
}

#[cfg(target_os = "macos")]
impl Instance {
    fn sample(pid: u32) -> Option<(Self, libc::proc_bsdinfo)> {
        use std::mem::{MaybeUninit, size_of};

        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = size_of::<libc::proc_bsdinfo>() as i32;
        // SAFETY: libproc receives a writable buffer of exactly the requested size.
        let n = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        // SAFETY: the kernel reported writing the complete BSD info struct.
        let info = unsafe { info.assume_init() };
        // A direct PTY child starts as a new session leader on its own
        // controlling tty. A reused PID or a wrapper's grandchild is insufficient.
        // SAFETY: getsid only queries the already range-checked PID.
        let session = unsafe { libc::getsid(pid as i32) };
        if info.pbi_pid != pid
            || info.pbi_ppid != std::process::id()
            || info.pbi_status == libc::SZOMB
            || session != pid as i32
            || info.e_tdev == u32::MAX
        {
            return None;
        }
        let born = Self {
            pid,
            started_us: info
                .pbi_start_tvsec
                .checked_mul(1_000_000)?
                .checked_add(info.pbi_start_tvusec)?,
            tty_device: info.e_tdev,
        };
        Some((born, info))
    }

    /// The terminal master identified this direct child as its foreground group.
    /// Pin its birth and tty. Check the executable when a writer record is available.
    pub(crate) fn at_spawn(pid: u32) -> Option<Self> {
        Self::sample(pid).map(|(born, _)| born)
    }

    pub(crate) fn is_foreground_writer(&self, executable: &Path) -> bool {
        use std::ffi::{CStr, OsStr};
        use std::os::unix::ffi::OsStrExt;

        let mut path = [0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: at_spawn validated the PID range and this buffer spans the
        // supplied size. Only read a terminated path after a successful call.
        let n = unsafe {
            libc::proc_pidpath(self.pid as i32, path.as_mut_ptr().cast(), path.len() as u32)
        };
        n > 0
            && CStr::from_bytes_until_nul(&path)
                .is_ok_and(|p| Path::new(OsStr::from_bytes(p.to_bytes())) == executable)
            && Self::sample(self.pid).is_some_and(|(born, info)| {
                born == *self && info.pbi_pgid == self.pid && info.e_tpgid == self.pid
            })
    }
}

#[cfg(not(target_os = "macos"))]
impl Instance {
    pub(crate) fn at_spawn(_: u32) -> Option<Self> {
        None
    }
    pub(crate) fn is_foreground_writer(&self, _: &Path) -> bool {
        false
    }
}
