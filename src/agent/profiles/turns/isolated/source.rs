use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::agent::AgentTerminal;
use crate::agent::turns::{
    AttachError, Binding, Claim, DirectoryId, Gap, IsolatedTranscriptRoot, Launch, MAX_WORKERS,
    Opened, ProviderId, Source, TranscriptReceiver, TranscriptSource, TurnOptions, Witness,
};
use crate::proc::instance::Instance;
use crate::{Error, SessionId, SpawnMode, SpawnSpec};

use super::{locate, parser};

fn invalid() -> Error {
    Error::InvalidInput("invalid or occupied interactive transcript namespace".into())
}

fn program_and_id(
    spec: &SpawnSpec,
    source: TranscriptSource,
) -> crate::Result<(PathBuf, Option<ProviderId>)> {
    let SpawnMode::Direct { program, args } = &spec.mode else {
        return Err(invalid());
    };
    if !program.is_absolute() || args.len() > 32 {
        return Err(invalid());
    }
    let mut args = args.iter().map(OsString::as_os_str);
    let executable_name = program
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(invalid)?;
    match source {
        TranscriptSource::Codex if executable_name == "codex" => {}
        TranscriptSource::Pi if executable_name == "node" => {
            let script = Path::new(args.next().ok_or_else(invalid)?);
            if !script.is_absolute()
                || !script.ends_with("pi-coding-agent/dist/bundle/cli.js")
                || !script.is_file()
            {
                return Err(invalid());
            }
        }
        TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText
            if executable_name == "node" =>
        {
            // The shipped cursor-agent shell wrapper execs its sibling node and
            // index.js. Start that foreground process directly to pin its identity.
            let mut entry = args.next().ok_or_else(invalid)?;
            if entry == "--use-system-ca" {
                entry = args.next().ok_or_else(invalid)?;
            }
            let entry = Path::new(entry);
            if !entry.is_absolute()
                || !entry.is_file()
                || entry != program.with_file_name("index.js")
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    let mut session = None;
    while let Some(arg) = args.next() {
        let value = arg.to_str().ok_or_else(invalid)?;
        match (source, value) {
            (_, "--model" | "-m") => {
                args.next()
                    .and_then(OsStr::to_str)
                    .filter(|s| !s.is_empty() && s.len() <= 256 && !s.starts_with('-'))
                    .ok_or_else(invalid)?;
            }
            (TranscriptSource::Pi, "--session-id") if session.is_none() => {
                let id = args
                    .next()
                    .and_then(OsStr::to_str)
                    .filter(|s| locate::uuid(s))
                    .ok_or_else(invalid)?;
                session = Some(ProviderId::new(id).map_err(|_| invalid())?);
            }
            (
                TranscriptSource::Pi,
                "--no-tools"
                | "--no-extensions"
                | "--no-skills"
                | "--no-prompt-templates"
                | "--no-themes"
                | "--approve",
            ) => {}
            (
                TranscriptSource::Codex,
                "--no-alt-screen" | "--dangerously-bypass-approvals-and-sandbox",
            ) => {}
            (
                TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText,
                "--force" | "--trust",
            ) => {}
            _ => return Err(invalid()),
        }
    }
    Ok((program.canonicalize().map_err(|_| invalid())?, session))
}

static ROOTS: Mutex<Vec<DirectoryId>> = Mutex::new(Vec::new());

struct Reservation {
    directory: DirectoryId,
    directory_name: PathBuf,
    executable: PathBuf,
    expected_id: Option<ProviderId>,
    child: OnceLock<(SessionId, Instance)>,
    session: OnceLock<ProviderId>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        ROOTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id| id != &self.directory);
    }
}

pub(in crate::agent) fn capture(
    spec: &mut SpawnSpec,
    source: TranscriptSource,
    isolated: &IsolatedTranscriptRoot,
) -> crate::Result<Claim> {
    let (executable, expected_id) = program_and_id(spec, source)?;
    let cwd = spec.cwd.canonicalize().map_err(|_| invalid())?;
    let root = &isolated.0;
    let initial = ProviderId::new(source.profile()).map_err(|_| invalid())?;
    let key = match source {
        TranscriptSource::Codex => "CODEX_HOME",
        TranscriptSource::Pi => "PI_CODING_AGENT_SESSION_DIR",
        TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText => "CURSOR_DATA_DIR",
    };
    let (identity_set, identity_remove) = spec.identity.env();
    if spec.env.removes(OsStr::new(key)) {
        return Err(invalid());
    }
    let configured = spec.env.set.iter().any(|(name, _)| name == key)
        || identity_set.iter().any(|(name, _)| name == key)
        || identity_remove.contains(&key);
    let effective = if configured {
        let [value] = spec.freeze_launch_env([key]);
        PathBuf::from(value.filter(|v| !v.is_empty()).ok_or_else(invalid)?)
    } else {
        root.launch_path.clone()
    };
    let mut claim = Claim::new(initial, &cwd, &effective)?;
    root.bind(&mut claim)?;
    claim.source = Some(source);
    let directory = claim.root.directory_id();
    let mut slots = ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    if slots.len() >= MAX_WORKERS || slots.contains(&directory) {
        return Err(invalid());
    }
    slots.push(directory);
    drop(slots);
    claim.writer = Some(Arc::new(Reservation {
        directory,
        directory_name: root.launch_path.clone(),
        executable,
        expected_id,
        child: OnceLock::new(),
        session: OnceLock::new(),
    }));
    check_empty(&claim)?;
    spec.env.set(key, &root.launch_path);
    spec.cwd = cwd;
    Ok(claim)
}

pub(in crate::agent) fn check_empty(claim: &Claim) -> crate::Result<()> {
    if locate::locate(claim).map_err(|_| invalid())?.is_some() {
        return Err(invalid());
    }
    Ok(())
}

impl Witness for Reservation {
    fn spawned(&self, term: &crate::Terminal) {
        if let Some(pid) = term.pid() {
            if let Some(instance) = Instance::at_spawn(pid) {
                let _ = self.child.set((term.id(), instance));
            }
        }
    }

    fn current(&self, launch: &Launch) -> Result<Binding, Gap> {
        let (terminal, instance) = self.child.get().ok_or(Gap::Uncorroborated)?;
        if *terminal != launch.session
            || self.directory != launch.claim.root.directory_id()
            || !instance.is_foreground_writer(&self.executable)
        {
            return Err(Gap::Uncorroborated);
        }
        launch.claim.root.check_path(&self.directory_name)?;
        let file = locate::locate(&launch.claim)?.ok_or(Gap::Missing)?;
        if self
            .expected_id
            .as_ref()
            .is_some_and(|expected| expected != &file.id)
        {
            return Err(Gap::SessionChanged);
        }
        if self.session.get_or_init(|| file.id.clone()) != &file.id {
            return Err(Gap::SessionChanged);
        }
        Ok(Binding {
            initial: launch.claim.initial.clone(),
            current: file.id,
            relative: file.path,
            revision: 0,
        })
    }
}

struct ProviderSource {
    writer: Arc<dyn Witness>,
    provider: TranscriptSource,
    cwd: PathBuf,
}

impl Source for ProviderSource {
    fn witness(&self) -> Arc<dyn Witness> {
        self.writer.clone()
    }

    fn snapshot_marker(&self) -> Option<fn(&[u8]) -> Result<bool, Gap>> {
        matches!(
            self.provider,
            TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText
        )
        .then_some(parser::cursor_marker)
    }

    fn header_ready(&mut self, opened: &mut Opened) -> Result<bool, Gap> {
        let Some(head) = opened.read_header(64 * 1024)? else {
            return Ok(false);
        };
        parser::header(self.provider, &head, &self.cwd, &opened.binding.current)?;
        Ok(true)
    }
}

pub(in crate::agent) fn attach(
    term: &AgentTerminal,
    options: TurnOptions,
) -> Result<TranscriptReceiver, AttachError> {
    let claim = term.turn_claim().ok_or(AttachError::NoLaunch)?;
    let source = claim.source.ok_or(AttachError::NoLaunch)?;
    let provider = ProviderSource {
        writer: claim.writer.clone().ok_or(AttachError::NoLaunch)?,
        cwd: claim.cwd.clone(),
        provider: source,
    };
    let receiver = term.attach_turns(
        provider,
        parser::ProviderParser::new(source),
        options.into(),
    )?;
    Ok(TranscriptReceiver {
        source,
        receiver: Arc::new(receiver),
    })
}
