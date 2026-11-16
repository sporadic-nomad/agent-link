//! Who owns the terminal's foreground: the session's shell at its prompt, or something it
//! started (an agent CLI typed into it). A host uses this before typing into the terminal.
//!
//! unix asks the kernel (`tcgetpgrp` on the PTY master, through portable-pty's
//! `process_group_leader`), so the answer is exact. Windows has no foreground process
//! group: it infers the owner from the child's process tree — the child and its
//! descendants by parent pid in a Toolhelp snapshot, only those in its Job Object when it
//! has one — and every answer is `Heuristic`. A program the shell itself left running
//! (`start notepad`, `start /b …`) reads as `Agent`; one whose launcher exited (an editor
//! opened with `code .`) is not below the shell and does not count, job member or not.
//!
//! A query also names the program in front ([`ProgramId`]) when it is not the shell: unix
//! its process group; Windows the shell's one direct child in the tree (none when the shell
//! runs several).

#[cfg(windows)]
use super::SessionId;
use super::{Confidence, Foreground, ForegroundOwner, ProgramId};
#[cfg(windows)]
use crate::proc::job::Job;
#[cfg(any(windows, test))]
use crate::proc::job::{self, ProcEntry};

/// What a session's child is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChildKind {
    /// An interactive shell.
    Shell,
    /// The program itself.
    Direct,
}

/// Classifies the terminal's foreground process group `fg` for a child with pid
/// `child_pid`. Every answer is [`Confidence::Exact`]: `fg` comes from the kernel.
///
/// - Shell: `fg` is the shell's own group (the shell leads it) ⇒ `Shell`; any other
///   positive group ⇒ `Agent` — a job the shell put in the foreground (an unknown shell
///   pid also gives `Agent`).
/// - Direct: any positive group ⇒ `Agent`: the child is the agent, and whatever it put in
///   the foreground is its own.
/// - No positive `fg` ⇒ `Unknown`.
#[cfg(any(unix, test))]
pub(crate) fn classify(kind: ChildKind, fg: Option<i32>, child_pid: Option<u32>) -> Foreground {
    let shell = child_pid.and_then(|pid| i32::try_from(pid).ok());
    let owner = match (kind, fg) {
        (ChildKind::Shell, Some(fg)) if Some(fg) == shell => ForegroundOwner::Shell,
        (_, Some(fg)) if fg > 0 => ForegroundOwner::Agent,
        _ => ForegroundOwner::Unknown,
    };
    Foreground {
        owner,
        confidence: Confidence::Exact,
    }
}

/// [`classify`], plus the program it names: the foreground group, when that is not the
/// shell's.
#[cfg(any(unix, test))]
pub(crate) fn classify_program(
    kind: ChildKind,
    fg: Option<i32>,
    child_pid: Option<u32>,
) -> (Foreground, Option<ProgramId>) {
    let answer = classify(kind, fg, child_pid);
    let program = fg
        .filter(|_| answer.owner == ForegroundOwner::Agent)
        .and_then(|group| u32::try_from(group).ok())
        .map(ProgramId);
    (answer, program)
}

/// The foreground of a session whose master is `master` (`None` once torn down), and the
/// program in front ([`classify_program`]).
#[cfg(unix)]
pub(crate) fn query(
    kind: ChildKind,
    master: Option<&(dyn portable_pty::MasterPty + Send)>,
    child_pid: Option<u32>,
) -> (Foreground, Option<ProgramId>) {
    let fg = master.and_then(|m| m.process_group_leader());
    classify_program(kind, fg, child_pid)
}

/// Classifies the live processes of a child's tree, `procs` (from [`tree_procs`]), for a
/// child with pid `child_pid`. Every answer is [`Confidence::Heuristic`].
///
/// - Nothing runs ⇒ `Unknown`.
/// - Shell: only the shell ⇒ `Shell`; anything else (also a child still running after the
///   shell exited) ⇒ `Agent`.
/// - Direct: anything runs ⇒ `Agent`.
#[cfg(any(windows, test))]
pub(crate) fn classify_tree(kind: ChildKind, child_pid: u32, procs: &[u32]) -> Foreground {
    let owner = match kind {
        _ if procs.is_empty() => ForegroundOwner::Unknown,
        ChildKind::Shell if procs.iter().all(|&pid| pid == child_pid) => ForegroundOwner::Shell,
        _ => ForegroundOwner::Agent,
    };
    Foreground {
        owner,
        confidence: Confidence::Heuristic,
    }
}

/// The processes a Windows foreground is decided on: `child_pid` (while `entries` lists
/// it) and its descendants by parent pid, restricted to `members` (the child's Job
/// Object) when known. The restriction drops unrelated processes a stale parent pid links
/// in; the walk drops members whose launcher exited (detached programs such as an editor
/// or a browser started from the session), which would otherwise read as an agent forever.
#[cfg(any(windows, test))]
pub(crate) fn tree_procs(
    entries: &[ProcEntry],
    child_pid: u32,
    members: Option<&[u32]>,
) -> Vec<u32> {
    let mut procs = job::tree_in(entries, child_pid);
    if let Some(members) = members {
        procs.retain(|pid| members.contains(pid));
    }
    procs
}

/// The program a Windows shell runs: its one direct child among `procs` (from
/// [`tree_procs`]). `None` without one, or with several (a program left running with
/// `start /b` beside it): no single program to name.
#[cfg(any(windows, test))]
pub(crate) fn tree_program(entries: &[ProcEntry], shell_pid: u32, procs: &[u32]) -> Option<u32> {
    let mut children = entries
        .iter()
        .filter(|e| e.parent == shell_pid && e.pid != shell_pid && procs.contains(&e.pid))
        .map(|e| e.pid);
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

/// The foreground of session `id` while it is still `open` — [`classify_tree`] over
/// [`tree_procs`] of a fresh snapshot, with `job`'s members when its query works — and, for
/// a shell running a program, that program ([`tree_program`]). A failed snapshot, a closed
/// session or an unknown pid ⇒ `Unknown`.
#[cfg(windows)]
pub(crate) fn query(
    id: SessionId,
    kind: ChildKind,
    open: bool,
    child_pid: Option<u32>,
    job: Option<&Job>,
) -> (Foreground, Option<ProgramId>) {
    let unknown = Foreground {
        owner: ForegroundOwner::Unknown,
        confidence: Confidence::Heuristic,
    };
    let (true, Some(pid)) = (open, child_pid) else {
        return (unknown, None);
    };
    let members = job.and_then(|job| match job.pids() {
        Ok(pids) => Some(pids),
        Err(e) => {
            log::debug!("[agent-link:{id}] job member query failed: {e}");
            None
        }
    });
    // No member but the child (the shell at its prompt): the answer needs no snapshot.
    if let Some(members) = &members {
        if members.iter().all(|&m| m == pid) {
            return (classify_tree(kind, pid, members), None);
        }
    }
    match job::snapshot() {
        Ok(entries) => {
            let procs = tree_procs(&entries, pid, members.as_deref());
            let answer = classify_tree(kind, pid, &procs);
            let program = (kind == ChildKind::Shell && answer.owner == ForegroundOwner::Agent)
                .then(|| tree_program(&entries, pid, &procs))
                .flatten()
                .map(ProgramId);
            (answer, program)
        }
        Err(e) => {
            log::debug!("[agent-link:{id}] process snapshot failed: {e}");
            (unknown, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(kind: ChildKind, fg: Option<i32>, pid: Option<u32>) -> ForegroundOwner {
        let answer = classify(kind, fg, pid);
        assert_eq!(answer.confidence, Confidence::Exact);
        answer.owner
    }

    #[test]
    fn shell_when_fg_equals_shell() {
        assert_eq!(
            owner(ChildKind::Shell, Some(4242), Some(4242)),
            ForegroundOwner::Shell
        );
    }

    #[test]
    fn agent_when_fg_differs_from_shell() {
        assert_eq!(
            owner(ChildKind::Shell, Some(4300), Some(4242)),
            ForegroundOwner::Agent
        );
    }

    #[test]
    fn unknown_when_fg_missing_or_not_positive() {
        assert_eq!(
            owner(ChildKind::Shell, None, Some(4242)),
            ForegroundOwner::Unknown
        );
        assert_eq!(
            owner(ChildKind::Shell, Some(0), Some(4242)),
            ForegroundOwner::Unknown
        );
        assert_eq!(
            owner(ChildKind::Shell, Some(-1), None),
            ForegroundOwner::Unknown
        );
    }

    #[test]
    fn agent_when_shell_pid_unknown() {
        assert_eq!(
            owner(ChildKind::Shell, Some(4300), None),
            ForegroundOwner::Agent
        );
    }

    /// A shell pid past `i32::MAX` can never equal a group id: anything in front is not it.
    #[test]
    fn shell_pid_out_of_range_never_matches() {
        assert_eq!(
            owner(ChildKind::Shell, Some(4300), Some(u32::MAX)),
            ForegroundOwner::Agent
        );
    }

    #[test]
    fn direct_child_in_front_is_the_agent() {
        // The child leads its own group, so fg == child pid: still the agent.
        assert_eq!(
            owner(ChildKind::Direct, Some(4242), Some(4242)),
            ForegroundOwner::Agent
        );
        // Something the agent itself put in front is the agent's too.
        assert_eq!(
            owner(ChildKind::Direct, Some(4300), Some(4242)),
            ForegroundOwner::Agent
        );
    }

    fn tree_owner(kind: ChildKind, procs: &[u32]) -> ForegroundOwner {
        let answer = classify_tree(kind, 4242, procs);
        assert_eq!(answer.confidence, Confidence::Heuristic);
        answer.owner
    }

    #[test]
    fn tree_shell_alone_is_the_shell() {
        assert_eq!(
            tree_owner(ChildKind::Shell, &[4242]),
            ForegroundOwner::Shell
        );
    }

    #[test]
    fn tree_shell_with_a_descendant_is_the_agent() {
        assert_eq!(
            tree_owner(ChildKind::Shell, &[4242, 4300]),
            ForegroundOwner::Agent
        );
        // The shell exited but something it started still runs.
        assert_eq!(
            tree_owner(ChildKind::Shell, &[4300]),
            ForegroundOwner::Agent
        );
    }

    #[test]
    fn tree_direct_child_alive_is_the_agent() {
        assert_eq!(
            tree_owner(ChildKind::Direct, &[4242]),
            ForegroundOwner::Agent
        );
        assert_eq!(
            tree_owner(ChildKind::Direct, &[4242, 4300]),
            ForegroundOwner::Agent
        );
    }

    #[test]
    fn tree_empty_is_unknown() {
        assert_eq!(tree_owner(ChildKind::Shell, &[]), ForegroundOwner::Unknown);
        assert_eq!(tree_owner(ChildKind::Direct, &[]), ForegroundOwner::Unknown);
    }

    fn entries(pairs: &[(u32, u32)]) -> Vec<ProcEntry> {
        pairs
            .iter()
            .map(|&(pid, parent)| ProcEntry { pid, parent })
            .collect()
    }

    /// Snapshot + job members → owner, for shell 4242.
    fn job_owner(
        kind: ChildKind,
        pairs: &[(u32, u32)],
        members: Option<&[u32]>,
    ) -> ForegroundOwner {
        classify_tree(kind, 4242, &tree_procs(&entries(pairs), 4242, members)).owner
    }

    /// A detached program (its launcher exited, so its parent pid names a dead process)
    /// stays in the job but is not below the shell: the shell is still in front.
    #[test]
    fn a_detached_job_member_does_not_count() {
        // 4300 was started by 4250, which exited; 4250 was the shell's child.
        let pairs = [(4242, 1), (4300, 4250)];
        let members = [4242, 4300];
        assert_eq!(
            job_owner(ChildKind::Shell, &pairs, Some(&members)),
            ForegroundOwner::Shell
        );
        // A real child of the shell beside it is the agent.
        let pairs = [(4242, 1), (4300, 4250), (4400, 4242), (4401, 4400)];
        let members = [4242, 4300, 4400, 4401];
        assert_eq!(
            job_owner(ChildKind::Shell, &pairs, Some(&members)),
            ForegroundOwner::Agent
        );
    }

    /// With a job, a process below the shell that is not a member does not count: started
    /// before the shell joined, or an unrelated process whose parent pid is stale.
    #[test]
    fn only_job_members_below_the_child_count() {
        let pairs = [(4242, 1), (4500, 4242)];
        assert_eq!(
            job_owner(ChildKind::Shell, &pairs, Some(&[4242])),
            ForegroundOwner::Shell
        );
        // Without a job (or when its query failed) the snapshot alone decides.
        assert_eq!(
            job_owner(ChildKind::Shell, &pairs, None),
            ForegroundOwner::Agent
        );
    }

    #[test]
    fn a_direct_child_is_judged_by_its_own_tree() {
        // Running: the agent.
        assert_eq!(
            job_owner(ChildKind::Direct, &[(4242, 1)], Some(&[4242])),
            ForegroundOwner::Agent
        );
        // Exited, a child of it still runs (its parent pid still names it): the agent.
        assert_eq!(
            job_owner(ChildKind::Direct, &[(4300, 4242)], Some(&[4300])),
            ForegroundOwner::Agent
        );
        // Exited, only a detached member is left: nothing of the child runs.
        assert_eq!(
            job_owner(ChildKind::Direct, &[(4300, 4250)], Some(&[4300])),
            ForegroundOwner::Unknown
        );
    }

    /// A program other than the shell is named by its group; the shell and an unknown
    /// owner are not named.
    #[test]
    fn the_program_in_front_is_named_by_its_group() {
        let program = |kind, fg, pid| classify_program(kind, fg, pid).1;
        assert_eq!(
            program(ChildKind::Shell, Some(4300), Some(4242)),
            Some(ProgramId(4300))
        );
        assert_eq!(program(ChildKind::Shell, Some(4242), Some(4242)), None);
        assert_eq!(program(ChildKind::Shell, None, Some(4242)), None);
        assert_eq!(program(ChildKind::Shell, Some(-1), Some(4242)), None);
        assert_eq!(
            program(ChildKind::Direct, Some(4242), Some(4242)),
            Some(ProgramId(4242))
        );
        let (answer, _) = classify_program(ChildKind::Shell, Some(4300), Some(4242));
        assert_eq!(answer, classify(ChildKind::Shell, Some(4300), Some(4242)));
    }

    /// Windows names the shell's one direct child in the tree — a nested shell started
    /// after the agent exited is another child, so another program; none when the shell
    /// runs several programs (one left running with `start /b`) or none.
    #[test]
    fn the_shell_s_one_child_names_the_program() {
        let named = |pairs: &[(u32, u32)], members: Option<&[u32]>| {
            let entries = entries(pairs);
            let procs = tree_procs(&entries, 4242, members);
            tree_program(&entries, 4242, &procs)
        };
        // The agent (4300) and the tools it runs (4301).
        let agent = [(4242, 1), (4300, 4242), (4301, 4300)];
        assert_eq!(named(&agent, None), Some(4300));
        assert_eq!(named(&agent, Some(&[4242, 4300, 4301])), Some(4300));
        // The agent exited, a nested shell runs.
        assert_eq!(named(&[(4242, 1), (4400, 4242)], None), Some(4400));
        // Two programs beside each other; none.
        assert_eq!(named(&[(4242, 1), (4300, 4242), (4500, 4242)], None), None);
        assert_eq!(named(&[(4242, 1)], None), None);
        // A child outside the job does not count.
        assert_eq!(
            named(
                &[(4242, 1), (4300, 4242), (4500, 4242)],
                Some(&[4242, 4300])
            ),
            Some(4300)
        );
    }

    #[test]
    fn direct_unknown_without_a_foreground_group() {
        assert_eq!(
            owner(ChildKind::Direct, None, Some(4242)),
            ForegroundOwner::Unknown
        );
        assert_eq!(
            owner(ChildKind::Direct, Some(0), None),
            ForegroundOwner::Unknown
        );
    }
}
