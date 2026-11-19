use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::VERSION;
use super::parser::ClaudeParser;
use super::wire::{Header, WriterRecord, decode};
use crate::agent::turns::{
    AttachError, Binding, Claim, DirectoryId, Gap, Launch, MAX_WORKERS, Opened, Options,
    ProviderId, Source, TurnReceiver, Witness,
};
use crate::agent::{AgentTerminal, LaunchPlan};
use crate::proc::instance::Instance;
use crate::{Error, SessionId, SpawnMode, SpawnSpec};

const WRITER_BYTES: usize = 64 * 1024;
const HEADER_BYTES: usize = 1024;

fn invalid() -> Error {
    Error::InvalidInput("unavailable Claude transcript launch".into())
}

// Controlled new launches use canonical lower-case random (v4) UUIDs.
fn uuid(s: &str) -> Result<ProviderId, Gap> {
    let bytes = s.as_bytes();
    if bytes.len() != 36 || bytes[14] != b'4' || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b') {
        return Err(Gap::Uncorroborated);
    }
    for (i, &b) in bytes.iter().enumerate() {
        let valid = match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        };
        if !valid {
            return Err(Gap::Uncorroborated);
        }
    }
    ProviderId::new(s)
}

struct Identity {
    initial: ProviderId,
    root: DirectoryId,
    project: PathBuf,
    directory_at_admission: Result<DirectoryId, Gap>,
    cwd: PathBuf,
    executable: PathBuf,
    admitted: AtomicBool,
    child: OnceLock<DirectChild>,
    record_started: OnceLock<u64>,
}

impl Identity {
    fn shares_transcripts(
        &self,
        directory: Result<DirectoryId, Gap>,
        peer: &Self,
        peer_directory: impl FnOnce() -> Result<DirectoryId, Gap>,
    ) -> bool {
        if self.project == peer.project {
            return true;
        }
        match (directory, peer_directory()) {
            (Ok(a), Ok(b)) => a == b,
            // Before creation, the supported ASCII slugs may alias on a
            // case-insensitive volume. Other lookup failures are ambiguous.
            (Ok(_) | Err(Gap::Missing), Err(Gap::Missing)) | (Err(Gap::Missing), Ok(_)) => self
                .project
                .as_os_str()
                .eq_ignore_ascii_case(peer.project.as_os_str()),
            _ => true,
        }
    }
}

struct DirectChild {
    session: SessionId,
    pid: u32,
    writer: Option<Instance>,
    exited: AtomicBool,
    turns_ended: Arc<AtomicBool>,
}

impl DirectChild {
    fn confirm_exit(&self) -> bool {
        // Only the PID supplied by the spawning PTY is eligible for this query.
        // A stopped process, PTY EOF, or loss of foreground ownership can all
        // leave a writer alive. WNOWAIT preserves the terminal's reap ownership.
        let exited = match crate::pty::peek_exit(self.pid) {
            Ok(exit) => exit.is_some(),
            Err(err) => err.raw_os_error() == Some(libc::ECHILD),
        };
        if exited {
            // Close this child's receiver before any pull can treat its absence
            // from the writer set as attribution loss. PTY EOF alone, conversely,
            // does not prove to peers that this process has exited.
            self.turns_ended.store(true, Ordering::Release);
            self.exited.store(true, Ordering::Release);
        }
        exited
    }
}

struct Held {
    writer: Arc<Identity>,
    current: Option<ProviderId>,
    revision: u64,
    initial_observed: bool,
}

impl Held {
    // Preflight reserves the UUID and a slot. Both passes of peer reconciliation
    // include the writer only once process creation can begin.
    fn admitted_in(&self, root: DirectoryId) -> bool {
        self.writer.root == root && self.writer.admitted.load(Ordering::Acquire)
    }

    fn claims(&self, id: &ProviderId) -> bool {
        self.writer.initial == *id || self.current.as_ref() == Some(id)
    }

    fn invalidate(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn has_exclusive_claim(group: &[&mut Self], index: usize, target: &ProviderId) -> bool {
        group[index].revision != u64::MAX
            && group
                .iter()
                .enumerate()
                .all(|(i, peer)| i == index || (peer.current.is_some() && !peer.claims(target)))
    }
}

static CLAIMS: Mutex<Vec<Held>> = Mutex::new(Vec::new());

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    // Pause the calling pull after I/O while the independent worker can still commit.
    pub(super) static AFTER_WRITER_SAMPLES:
        std::cell::Cell<Option<crate::agent::turns::tests::peer::Pause>> =
        const { std::cell::Cell::new(None) };
}

/// One bounded entry survives the terminal and any retiring worker or in-flight pull.
struct Reservation(Arc<Identity>);

impl Reservation {
    fn take(writer: Identity) -> crate::Result<Self> {
        let mut held = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        if held.len() == MAX_WORKERS || held.iter().any(|h| h.claims(&writer.initial)) {
            return Err(invalid());
        }
        let writer = Arc::new(writer);
        held.push(Held {
            writer: writer.clone(),
            current: Some(writer.initial.clone()),
            revision: 0,
            initial_observed: false,
        });
        Ok(Self(writer))
    }

    fn observe(&self, launch: &Launch) -> Result<(ProviderId, u64), Gap> {
        // One concurrent switch may supersede the first sample. Bound the synchronous
        // pull even if writers keep switching during its filesystem and process probes.
        for _ in 0..2 {
            if let Some(current) = self.observe_once(launch)? {
                return Ok(current);
            }
        }
        Err(Gap::Uncorroborated)
    }

    fn observe_once(&self, launch: &Launch) -> Result<Option<(ProviderId, u64)>, Gap> {
        // A cooperating writer may switch conversations even if it never attaches a
        // reader. Look up only the exact writer records for known claims in this
        // transcript directory. Never enumerate sessions or hold the lock across I/O.
        let candidates: Vec<_> = CLAIMS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|h| h.admitted_in(self.0.root))
            .map(|h| (h.writer.clone(), h.revision))
            .collect();
        let root = &launch.claim.root;
        let directory = root.subdirectory_id(&self.0.project);
        let mut peers = Vec::new();
        let mut excluded = Vec::new();
        for (writer, revision) in candidates {
            let shared = self
                .0
                .shares_transcripts(directory, &writer, || root.subdirectory_id(&writer.project));
            if shared {
                if let Some(sample) = writer.sample(&launch.claim) {
                    peers.push((writer, revision, sample));
                    continue;
                }
            }
            excluded.push(writer);
        }
        #[cfg(all(test, target_os = "macos"))]
        if let Some(pause) = AFTER_WRITER_SAMPLES.take() {
            pause.park();
        }
        let mut held = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
        let mut group: Vec<_> = held
            .iter_mut()
            .filter(|h| {
                if !h.admitted_in(self.0.root) || excluded.iter().any(|w| Arc::ptr_eq(w, &h.writer))
                {
                    return false;
                }
                // A new arrival has no writer sample. Its admission captured enough
                // directory evidence to avoid fencing an independent project.
                peers.iter().any(|(w, ..)| Arc::ptr_eq(w, &h.writer))
                    || self.0.shares_transcripts(directory, &h.writer, || {
                        h.writer.directory_at_admission
                    })
            })
            .collect();
        // A new writer has no sample in this pass.
        if group
            .iter()
            .any(|h| !peers.iter().any(|(w, ..)| Arc::ptr_eq(&h.writer, w)))
        {
            for h in group {
                h.invalidate();
            }
            return Err(Gap::Uncorroborated);
        }
        let retired = group.len() != peers.len();
        let mut own_overlap = false;
        if retired {
            // CLAIMS retains insertion order, so surviving samples still align
            // with the group. Fence any overlap before dropping a retired sample:
            // its writer will be absent from the retry.
            let (survivors, departed): (Vec<_>, Vec<_>) = peers
                .into_iter()
                .partition(|(w, ..)| group.iter().any(|h| Arc::ptr_eq(&h.writer, w)));
            for (h, (_, _, sample)) in group.iter_mut().zip(&survivors) {
                let overlap = departed.iter().any(|(w, _, other)| {
                    let (Ok(current), Ok(other)) = (sample, other) else {
                        return true;
                    };
                    h.claims(other)
                        || h.claims(&w.initial)
                        || current == other
                        || current == &w.initial
                });
                if overlap {
                    h.invalidate();
                    own_overlap |= Arc::ptr_eq(&h.writer, &self.0);
                }
            }
            peers = survivors;
        }
        let stale = group.iter().zip(&peers).any(|(h, (_, rev, sample))| {
            h.revision != *rev && h.current.as_ref() != sample.as_ref().ok()
        });
        for (index, (_, _, sample)) in peers.iter().enumerate() {
            let current = sample.as_ref().ok();
            if group[index].current.as_ref() != current {
                // An out-of-order sample still fences any writer it might implicate,
                // including a transient overlap that could be gone by the retry.
                for (i, peer) in group.iter_mut().enumerate() {
                    if i == index || current.is_none_or(|id| peer.claims(id)) {
                        peer.invalidate();
                    }
                }
                // Only a consistent sample may replace the last recorded UUID.
                if !stale {
                    group[index].current = current.cloned();
                }
            }
        }
        if own_overlap {
            return Err(Gap::Uncorroborated);
        }
        if stale || retired {
            return Ok(None);
        }
        // Either reader can corroborate a direct child's launch UUID before /clear.
        // Record only a sampled writer with an exclusive claim in this directory.
        for (i, (_, _, sample)) in peers.iter().enumerate() {
            let h = &group[i];
            if !h.initial_observed
                && sample.as_ref().ok() == Some(&h.writer.initial)
                && Held::has_exclusive_claim(&group, i, &h.writer.initial)
            {
                group[i].initial_observed = true;
            }
        }
        let own = group
            .iter()
            .position(|h| Arc::ptr_eq(&h.writer, &self.0))
            .ok_or(Gap::Uncorroborated)?;
        // A delayed first record can wait only while the known peers still
        // corroborate exclusive access to the new launch's UUID.
        let own_sample = &peers[own].2;
        let target = match own_sample {
            Ok(current) => current,
            Err(Gap::Missing) => &self.0.initial,
            Err(reason) => return Err(*reason),
        };
        if !Held::has_exclusive_claim(&group, own, target) {
            return Err(Gap::Uncorroborated);
        }
        let current = own_sample.clone()?;
        if !group[own].initial_observed {
            return Err(Gap::Uncorroborated);
        }
        Ok(Some((current, group[own].revision)))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        CLAIMS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|h| !Arc::ptr_eq(&h.writer, &self.0));
    }
}

fn new_session<'a>(args: impl Iterator<Item = &'a OsStr>) -> crate::Result<ProviderId> {
    // Accept only the recorded interactive subset. An unknown option can consume
    // another word or select a different writer, configuration, or conversation.
    let mut args = args.map(OsStr::to_str);
    let mut id = None;
    while let Some(arg) = args.next() {
        match arg {
            Some("--session-id") if id.is_none() => {
                let value = args.next().flatten().ok_or_else(invalid)?;
                id = Some(uuid(value).map_err(|_| invalid())?);
            }
            Some("--model") => {
                args.next()
                    .flatten()
                    .filter(|v| !v.is_empty() && !v.starts_with('-'))
                    .ok_or_else(invalid)?;
            }
            Some("--tools") if args.next() == Some(Some("")) => {}
            Some("--strict-mcp-config") => {}
            Some("--settings") => {
                let value = args.next().flatten().ok_or_else(invalid)?;
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Settings {
                    #[serde(rename = "showTurnDuration")]
                    _display: bool,
                }
                if value.len() > 128 {
                    return Err(invalid());
                }
                decode::<Settings>(value.as_bytes()).map_err(|_| invalid())?;
            }
            _ => return Err(invalid()),
        }
    }
    id.ok_or_else(invalid)
}

fn project_dir(cwd: &Path) -> crate::Result<PathBuf> {
    let path = cwd
        .to_str()
        .filter(|s| s.is_ascii() && s.len() <= 200)
        .ok_or_else(invalid)?;
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Ok(Path::new("projects").join(slug))
}

fn relative(project: &Path, id: &ProviderId) -> PathBuf {
    project.join(format!("{}.jsonl", id.as_str()))
}

/// The caller must control every writer in the isolated config root. An external
/// --resume can use the same prompt, assistant and end-record metadata as this process.
#[cfg(test)]
pub(in crate::agent) fn capture(spec: &mut SpawnSpec, plan: &LaunchPlan) -> crate::Result<Claim> {
    capture_launch(spec, plan, false, |_| Ok(()))
}

#[cfg(feature = "turns")]
pub(in crate::agent) fn capture_exclusive(
    spec: &mut SpawnSpec,
    plan: &LaunchPlan,
    root: &crate::agent::turns::ExclusiveClaudeRoot,
) -> crate::Result<Claim> {
    let claim = capture_launch(spec, plan, false, |claim| root.bind(claim))?;
    // Even HOME/.claude fallback and relative overrides must reach the exact
    // directory admitted for this child when it later resolves this setting.
    spec.env.remove.retain(|key| key != "CLAUDE_CONFIG_DIR");
    spec.env.set("CLAUDE_CONFIG_DIR", &root.launch_path);
    Ok(claim)
}

// The offline interpreter receives its script name before the actual Claude argv.
#[cfg(all(test, target_os = "macos"))]
pub(super) fn capture_scripted(spec: &mut SpawnSpec, plan: &LaunchPlan) -> crate::Result<Claim> {
    capture_launch(spec, plan, true, |_| Ok(()))
}

fn capture_launch(
    spec: &mut SpawnSpec,
    plan: &LaunchPlan,
    scripted: bool,
    authorize: impl FnOnce(&mut Claim) -> crate::Result<()>,
) -> crate::Result<Claim> {
    let (program, initial) = match &spec.mode {
        SpawnMode::Direct { program, args } => launch_identity(program, args, scripted)?,
        SpawnMode::Shell(_) => launch_identity(Path::new(&plan.program), &plan.args, scripted)?,
    };
    let executable = program.canonicalize().map_err(|_| invalid())?;
    let cwd = spec.cwd.canonicalize().map_err(|_| invalid())?;
    let project = project_dir(&cwd)?;
    // portable-pty snapshots its inherited environment later, when building the
    // command. Pin these two inputs so it launches with the values used here.
    let [config, home] = spec.freeze_launch_env(["CLAUDE_CONFIG_DIR", "HOME"]);
    let root = config
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".claude"))
        })
        .ok_or_else(invalid)?;
    let mut claim = Claim::new(initial.clone(), &cwd, &root)?;
    authorize(&mut claim)?;
    let transcript = relative(&project, &initial);
    claim
        .root
        .require_absent(&transcript)
        .map_err(|_| invalid())?;
    let reservation = Reservation::take(Identity {
        initial,
        cwd,
        root: claim.root.directory_id(),
        directory_at_admission: claim.root.subdirectory_id(&project),
        project,
        executable,
        admitted: AtomicBool::new(false),
        child: OnceLock::new(),
        record_started: OnceLock::new(),
    })?;
    // An intervening claimant could have created the file and retired since preflight.
    // Check again while the reservation prevents another admission for the UUID.
    claim
        .root
        .require_absent(&transcript)
        .map_err(|_| invalid())?;
    claim.writer = Some(Arc::new(reservation));
    // Relative cwd resolution must be the same at capture and at process creation.
    spec.cwd = claim.cwd.clone();
    Ok(claim)
}

fn launch_identity<'a>(
    program: &'a Path,
    args: &[impl AsRef<OsStr>],
    scripted: bool,
) -> crate::Result<(&'a Path, ProviderId)> {
    let mut args = args.iter().map(AsRef::as_ref);
    if scripted {
        if program != Path::new("/bin/bash") || args.next() != Some(OsStr::new("peer.sh")) {
            return Err(invalid());
        }
    } else if !program.is_absolute() || program.file_name() != Some(OsStr::new("claude")) {
        return Err(invalid());
    }
    Ok((program, new_session(args)?))
}

impl Identity {
    /// None means the direct child has definitely exited. Its reservation still
    /// protects its UUIDs until the terminal and any retiring observers release it.
    fn sample(&self, claim: &Claim) -> Option<Result<ProviderId, Gap>> {
        let Some(child) = self.child.get() else {
            return Some(Err(Gap::Uncorroborated));
        };
        if child.exited.load(Ordering::Acquire) {
            return None;
        }
        let sample = child
            .writer
            .as_ref()
            .ok_or(Gap::Uncorroborated)
            .and_then(|writer| self.read_session(claim, writer));
        if sample.is_err() && child.confirm_exit() {
            return None;
        }
        Some(sample)
    }

    fn read_session(&self, claim: &Claim, child: &Instance) -> Result<ProviderId, Gap> {
        let path = Path::new("sessions").join(format!("{}.json", child.pid));
        let bytes = claim
            .root
            .read_record(&path, WRITER_BYTES)
            .map_err(|reason| {
                if reason == Gap::Missing
                    && (self.record_started.get().is_some()
                        || !child.is_foreground_writer(&self.executable))
                {
                    Gap::Uncorroborated
                } else {
                    reason
                }
            })?;
        let row: WriterRecord = decode(&bytes)?;
        let current = uuid(row.session_id)?;
        let started = row.started_at;
        if row.pid != child.pid
            || row.kind != "interactive"
            || row.entrypoint != "cli"
            || row.version != VERSION
            || row.pid_domain != "darwin"
            || Path::new(row.cwd.as_ref()) != self.cwd
            || started < child.started_ms()
            || u128::from(started)
                > SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Gap::Uncorroborated)?
                    .as_millis()
            || !child.is_foreground_writer(&self.executable)
        {
            return Err(Gap::Uncorroborated);
        }
        // Other claims can observe this record before its own first attachment.
        // Once corroborated, disappearance or a different start is evidence loss.
        if *self.record_started.get_or_init(|| started) != started {
            return Err(Gap::Uncorroborated);
        }
        Ok(current)
    }
}

impl Witness for Reservation {
    fn begin_spawn(&self) {
        self.0.admitted.store(true, Ordering::Release);
    }

    fn spawned(&self, term: &crate::Terminal) {
        if let Some(pid) = term.pid() {
            let handle = term.handle();
            let writer = handle
                .foreground_program()
                .1
                .filter(|group| group.0 == pid)
                .and_then(|_| Instance::at_spawn(pid));
            let _ = self.0.child.set(DirectChild {
                session: term.id(),
                pid,
                writer,
                exited: AtomicBool::new(false),
                turns_ended: handle.turn_end_signal(),
            });
        }
    }

    fn current(&self, launch: &Launch) -> Result<Binding, Gap> {
        let session = self.0.child.get().map(|child| child.session);
        if session != Some(launch.session)
            || self.0.initial != launch.claim.initial
            || self.0.root != launch.claim.root.directory_id()
        {
            return Err(Gap::Uncorroborated);
        }
        let (current, revision) = self.observe(launch)?;
        Ok(Binding {
            initial: self.0.initial.clone(),
            revision,
            relative: relative(&self.0.project, &current),
            current,
        })
    }
}

pub(in crate::agent) struct ClaudeSource {
    writer: Arc<dyn Witness>,
    selected: Option<Binding>,
}

impl ClaudeSource {
    pub(super) fn for_terminal(term: &AgentTerminal) -> Result<(Self, ClaudeParser), AttachError> {
        let claim = term.turn_claim().ok_or(AttachError::NoLaunch)?;
        #[cfg(feature = "turns")]
        if claim.source.is_some() {
            return Err(AttachError::NoLaunch);
        }
        let writer = claim.writer.clone().ok_or(AttachError::NoLaunch)?;
        Ok((
            Self {
                writer,
                selected: None,
            },
            ClaudeParser::new(claim.cwd.clone()),
        ))
    }

    pub(in crate::agent) fn attach(
        term: &AgentTerminal,
        options: Options,
    ) -> Result<TurnReceiver, AttachError> {
        let (source, parser) = Self::for_terminal(term)?;
        term.attach_turns(source, parser, options)
    }
}

impl Source for ClaudeSource {
    fn witness(&self) -> Arc<dyn Witness> {
        self.writer.clone()
    }

    fn open(&mut self, launch: &Launch) -> Result<Opened, Gap> {
        let binding = self.current(launch)?;
        if self.selected.get_or_insert_with(|| binding.clone()) != &binding {
            return Err(Gap::SessionChanged);
        }
        Opened::new(launch, binding)
    }

    fn header_ready(&mut self, opened: &mut Opened) -> Result<bool, Gap> {
        // Every captured file, including the /clear destination, started with mode.
        // Limit this lookup independently of historical file size.
        let Some(head) = opened.read_header(HEADER_BYTES)? else {
            return Ok(false);
        };
        let row: Header = decode(&head)?;
        if row.kind != "mode"
            || row.mode != "normal"
            || row.session != opened.binding.current.as_str()
        {
            return Err(Gap::Uncorroborated);
        }
        Ok(true)
    }
}
