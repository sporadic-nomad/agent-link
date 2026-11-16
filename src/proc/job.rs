//! The Windows process model behind PTY teardown and the foreground heuristic: a Job
//! Object the session's child joins at spawn, and Toolhelp process snapshots for when
//! there is no job.
//!
//! **Why a job.** `TerminateJobObject` kills every member in one call, including
//! descendants whose parent already exited (which `taskkill /T`, a walk by parent pid,
//! misses), and `KILL_ON_JOB_CLOSE` takes the tree down when the host dies without a
//! teardown. Its member list also keeps unrelated processes out of the foreground walk.
//!
//! **Cost: detached programs die with the session.** Every process started from the
//! session joins the job, including programs meant to outlive it — an editor opened with
//! `code .`, a browser a sign-in flow opened with `start <url>`, `gpg-agent`. Their
//! launcher exits (they are orphans by parent pid, so `taskkill /T` spared them), but they
//! stay members, so teardown — and a host crash — kills them. Deliberate for now (reliable
//! teardown of orphaned agent helpers); an open owner decision. The foreground heuristic
//! does not count them: it only looks at members below the child by parent pid.
//!
//! **Limit.** Membership is inherited at process creation, so a process the child started
//! before it was assigned (the first instant of its life, e.g. a `cmd.exe` AutoRun command)
//! is not a member; [`children_outside`] finds those so teardown can `taskkill` them.
//!
//! Parent pids in a snapshot can be stale (a dead parent's pid reused), so walks by parent
//! pid are a heuristic; cycles are cut. The pure helpers compile everywhere for their tests;
//! the rest is Windows-only.

/// A process and its parent, as a snapshot lists them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcEntry {
    pub(crate) pid: u32,
    pub(crate) parent: u32,
}

/// Every process below `root` in `entries` (children, their children, …), `root` excluded.
pub(crate) fn descendants(entries: &[ProcEntry], root: u32) -> Vec<u32> {
    let mut found: Vec<u32> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for entry in entries {
            if entry.parent == parent && entry.pid != root && !found.contains(&entry.pid) {
                found.push(entry.pid);
                frontier.push(entry.pid);
            }
        }
    }
    found
}

/// `root` (when `entries` lists it, i.e. while it runs) and every process below it.
pub(crate) fn tree_in(entries: &[ProcEntry], root: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    if entries.iter().any(|e| e.pid == root) {
        pids.push(root);
    }
    pids.extend(descendants(entries, root));
    pids
}

/// Whether `pid` has a direct child that is not in `members` (a job's pid list).
pub(crate) fn children_outside(entries: &[ProcEntry], members: &[u32], pid: u32) -> bool {
    entries
        .iter()
        .any(|e| e.parent == pid && e.pid != pid && !members.contains(&e.pid))
}

#[cfg(all(windows, test))]
pub(crate) use imp::tree;
#[cfg(windows)]
pub(crate) use imp::{Job, snapshot};

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::Foundation::{
        ERROR_MORE_DATA, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };

    use super::{ProcEntry, children_outside};

    /// Pids a member query makes room for at first.
    const FIRST_ROOM: usize = 64;

    /// One member query's answer.
    enum Members {
        /// Every member's pid.
        All(Vec<u32>),
        /// The room was too small; the job reported this many members (0 when it did not
        /// say).
        More(usize),
    }

    /// An anonymous Job Object whose members die when its handle closes (on drop).
    pub(crate) struct Job {
        handle: OwnedHandle,
    }

    impl Job {
        /// A new job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Its handle is not
        /// inheritable, so a member cannot keep it alive.
        pub(crate) fn create() -> io::Result<Self> {
            // SAFETY: null attributes and name create an anonymous, non-inheritable job.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh handle nothing else owns; dropping the job closes it.
            let job = Self {
                handle: unsafe { OwnedHandle::from_raw_handle(raw) },
            };
            // SAFETY: plain data; all zeroes means "no limits".
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is a valid value of the class's type, passed with its size.
            let ok = unsafe {
                SetInformationJobObject(
                    job.raw(),
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }

        fn raw(&self) -> RawHandle {
            self.handle.as_raw_handle()
        }

        /// Makes `process` (an open process handle) a member; its later children join too.
        pub(crate) fn assign(&self, process: RawHandle) -> io::Result<()> {
            // SAFETY: both handles are open for the duration of the call.
            if unsafe { AssignProcessToJobObject(self.raw(), process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Kills every member (exit code 1). Does not wait for them to end.
        pub(crate) fn terminate(&self) -> io::Result<()> {
            // SAFETY: an open job handle.
            if unsafe { TerminateJobObject(self.raw(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The pids of the running members. Room for 64 first, grown once when the job
        /// has more; an error when it still does not fit (never a partial list).
        pub(crate) fn pids(&self) -> io::Result<Vec<u32>> {
            let room = match self.query_pids(FIRST_ROOM)? {
                Members::All(pids) => return Ok(pids),
                Members::More(assigned) => assigned.max(4 * FIRST_ROOM) + 16,
            };
            match self.query_pids(room)? {
                Members::All(pids) => Ok(pids),
                Members::More(_) => Err(io::Error::other("the job's member list keeps growing")),
            }
        }

        /// The members' pids if they fit in `room`.
        fn query_pids(&self, room: usize) -> io::Result<Members> {
            const WORD: usize = std::mem::size_of::<usize>();
            // The id array follows two `u32` counts and is `usize`-aligned.
            const FIRST_ID: usize =
                std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList) / WORD;
            let mut buf = vec![0usize; FIRST_ID + room];
            let len = u32::try_from(buf.len() * WORD)
                .map_err(|_| io::Error::other("job member list too large"))?;
            // SAFETY: `buf` is `len` bytes, zeroed and aligned for the list type.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.raw(),
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    len,
                    std::ptr::null_mut(),
                )
            };
            let list = buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            // SAFETY: `buf` starts with a list header (zeroed, or filled in by the call).
            let (assigned, listed) = unsafe {
                (
                    (*list).NumberOfAssignedProcesses as usize,
                    (*list).NumberOfProcessIdsInList as usize,
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                    return Ok(Members::More(assigned));
                }
                return Err(err);
            }
            if listed < assigned || listed > room {
                return Ok(Members::More(assigned));
            }
            let ids = buf[FIRST_ID..FIRST_ID + listed]
                .iter()
                .map(|&id| id as u32)
                .collect();
            Ok(Members::All(ids))
        }

        /// Whether `pid` has a direct child outside the job — one it started before it
        /// joined.
        pub(crate) fn has_children_outside(&self, pid: u32) -> io::Result<bool> {
            // Snapshot first: a process started after it cannot look like an escapee.
            let entries = snapshot()?;
            Ok(children_outside(&entries, &self.pids()?, pid))
        }
    }

    /// Every running process with its parent (Toolhelp).
    pub(crate) fn snapshot() -> io::Result<Vec<ProcEntry>> {
        // SAFETY: a process snapshot takes no pointers.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh handle nothing else owns; dropping `snap` closes it.
        let snap = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: plain data; `dwSize` is set below, as the API requires.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut entries = Vec::new();
        // SAFETY: `entry` is a valid out-pointer with `dwSize` set.
        let mut more = unsafe { Process32FirstW(snap.as_raw_handle(), &mut entry) } != 0;
        while more {
            entries.push(ProcEntry {
                pid: entry.th32ProcessID,
                parent: entry.th32ParentProcessID,
            });
            // SAFETY: as above.
            more = unsafe { Process32NextW(snap.as_raw_handle(), &mut entry) } != 0;
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(entries)
        } else {
            Err(err)
        }
    }

    /// `root` (while it runs) and every process below it, from a fresh snapshot.
    #[cfg(test)]
    pub(crate) fn tree(root: u32) -> io::Result<Vec<u32>> {
        Ok(super::tree_in(&snapshot()?, root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(u32, u32)]) -> Vec<ProcEntry> {
        pairs
            .iter()
            .map(|&(pid, parent)| ProcEntry { pid, parent })
            .collect()
    }

    #[test]
    fn descendants_walk_the_whole_subtree() {
        // 10 → 11 → 12, 10 → 13; 20 is unrelated.
        let list = entries(&[(10, 1), (11, 10), (12, 11), (13, 10), (20, 1)]);
        let mut found = descendants(&list, 10);
        found.sort_unstable();
        assert_eq!(found, vec![11, 12, 13]);
        assert!(descendants(&list, 12).is_empty());
        assert!(descendants(&list, 99).is_empty());
    }

    /// Stale parent pids can form a cycle; the walk still ends and never returns the root.
    #[test]
    fn descendants_survive_cycles() {
        let list = entries(&[(10, 12), (11, 10), (12, 11), (0, 0)]);
        let mut found = descendants(&list, 10);
        found.sort_unstable();
        assert_eq!(found, vec![11, 12]);
        assert!(descendants(&list, 0).is_empty(), "a self-parented pid");
    }

    #[test]
    fn a_tree_lists_its_root_only_while_it_runs() {
        let list = entries(&[(10, 1), (11, 10), (12, 11)]);
        assert_eq!(tree_in(&list, 10), vec![10, 11, 12]);
        // 10 exited: Windows does not reparent, so its children still name it.
        let list = entries(&[(11, 10), (12, 11)]);
        assert_eq!(tree_in(&list, 10), vec![11, 12]);
        assert!(tree_in(&list, 99).is_empty());
    }

    #[test]
    fn children_outside_the_job_are_found() {
        let list = entries(&[(10, 1), (11, 10), (12, 10), (13, 11)]);
        assert!(children_outside(&list, &[10, 11], 10), "12 is not a member");
        // Only direct children count: 13 is missing from the list, but its parent is a
        // member, so it was born in the job (the list was read before it started).
        assert!(!children_outside(&list, &[10, 11, 12], 10));
        assert!(!children_outside(&list, &[], 99));
    }
}
