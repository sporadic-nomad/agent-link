//! claude's ambient source. Recorded on Claude Code 2.1.296; no version is pinned.
//!
//! **Where.** The root is `CLAUDE_CONFIG_DIR` (nonempty), else `<home>/.claude`; a
//! relative one is taken against the agent's cwd. The running claude's
//! `sessions/<pid>.json` (`<pid>`: the context's program) names its `sessionId` — `/clear`
//! rewrites it with the new session's — and the transcript is
//! `projects/<slug>/<sessionId>.jsonl`: `<slug>` is the cwd the pid file records (the
//! project unless claude started elsewhere) with every char outside `[A-Za-z0-9]` made
//! `-`; one longer than 200 chars claude cuts and suffixes with a hash, so the directory
//! sharing the first 200 that holds the transcript is taken, else the only one sharing
//! them. A pid file written before the launch (`startedAt`) is a dead claude's whose pid
//! was reused, and one whose `entrypoint` is not `cli` is a print or SDK session
//! (`sdk-cli`): neither is used. Without a usable pid file (the program not known yet, a
//! job led by a wrapper such as a Windows `.cmd` shim) and with the launch known: the
//! `sessions/*.json` of an interactive claude (`entrypoint: cli`) whose `cwd` is the
//! project, started at or after the launch and still alive, the latest started. A
//! `sessionId` that is not a plain name is never used, so a crafted pid file cannot point
//! outside `projects/`.
//!
//! **Turns.** A transcript line is one content block of a message (lines of one message
//! share `message.id`). A `system` line of subtype `turn_duration` ends a turn explicitly;
//! its reply is the turn's last assistant message if that one stopped with `end_turn`: its
//! nonblank `text` blocks, joined by a blank line. Thinking and other blocks never count;
//! a message that stopped otherwise (`tool_use`, `max_tokens`) leaves no reply, and a tool
//! result retires the message before it. A prompt (a `user` line that is neither a tool
//! result nor `isMeta`) starts the turn afresh. claude writes no `turn_duration` for a turn
//! interrupted with Esc: its `user` line carrying `interruptedMessageId` ends the turn
//! explicitly with no reply (and the `isAbortedMidStream` message before it never counts).
//! Subagent lines (`isSidechain`), API error messages (`isApiErrorMessage`) and other meta
//! assistant lines never count. An `assistant` line without a `message.content` array, or
//! a `system` line without a `subtype`, is [`AmbientRecord::Unrecognised`]; other lines,
//! also unknown ones, change nothing. A non-meta assistant line missing that array
//! discards the previous reply candidate.
//!
//! **Questions** (feature `questions`): an `AskUserQuestion` `tool_use` block, read as the
//! controlled tier reads it, is asked until its `tool_result`, in a turn whose prompt was
//! read; the next prompt, `turn_duration` and an interruption withdraw it. The pid file's
//! `status: "waiting"` is not read. Claude 2.1.296 writes the call while its dialog is
//! open only in some sessions (the predecessor crate saw it in `bypassPermissions` mode on
//! a session's first turn); otherwise the call comes with its result and nothing is asked.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::json::{Blocks, Json, Obj, decode};
use super::{Budget, append, dashed, plain_name, small_file};
use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::paths::{anchored, config_root, project_path, same_dir};
use crate::agent::turns::ambient::{
    AmbientContext, AmbientParser, AmbientRecord, AmbientSource, AmbientUnavailable,
};

/// claude cuts a project directory name longer than this and appends a hash.
const SLUG_MAX: usize = 200;
/// Bytes of a pid file read at most (one is under 1 KiB).
const SESSION_FILE_CAP: u64 = 64 * 1024;

/// claude's [`AmbientSource`].
#[derive(Default)]
pub(in crate::agent) struct ClaudeAmbient;

impl AmbientSource for ClaudeAmbient {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        let root = config_root(ctx.var("CLAUDE_CONFIG_DIR"), ctx.home(), ".claude")
            .ok_or(AmbientUnavailable::NoConfigRoot)?;
        let root = anchored(root, &ctx.cwd);
        let project = project_path(&ctx.cwd);
        let sessions = root.join("sessions");
        let mut budget = Budget::new();
        let own = match ctx.program {
            Some(pid) => read_session(&sessions.join(format!("{pid}.json")), &mut budget)?,
            None => None,
        };
        let own = own.filter(|s| s.since(ctx.launched_at) && s.entrypoint.is_none_or(|e| e));
        let session = match (own, ctx.launched_at) {
            (Some(session), _) => session,
            (None, Some(launched)) => live_session(&sessions, &ctx.cwd, launched, &mut budget)?,
            // Without a launch time another claude's session cannot be told apart.
            (None, None) => return Err(AmbientUnavailable::AgentUnknown),
        };
        // claude names the directory after the cwd it started in, as its pid file records
        // it: the project, unless the shell moved before the launch.
        let started_in = session
            .cwd
            .filter(|cwd| !cwd.is_empty())
            .map_or(project, PathBuf::from);
        let file = format!("{}.jsonl", session.id);
        let dir = project_dir(&root.join("projects"), &started_in, &file, &mut budget)?;
        Ok(dir.join(file))
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::<ClaudeParser>::default()
    }
}

/// What a pid file (`sessions/<pid>.json`) says.
struct Session {
    id: String,
    pid: Option<u32>,
    cwd: Option<String>,
    /// `startedAt`, ms since the epoch.
    started: Option<u64>,
    /// `entrypoint` is `cli` (the interactive TUI); `None` without one.
    entrypoint: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PidFile<'a> {
    #[serde(borrow)]
    session_id: Json<'a>,
    #[serde(borrow)]
    pid: Json<'a>,
    #[serde(borrow)]
    cwd: Json<'a>,
    #[serde(borrow)]
    started_at: Json<'a>,
    #[serde(borrow)]
    entrypoint: Json<'a>,
}

impl Session {
    /// Not written before `launched` (when both are known): one that was is a dead
    /// claude's whose pid the agent reuses.
    fn since(&self, launched: Option<SystemTime>) -> bool {
        match (self.started, launched) {
            (Some(started), Some(launched)) => started >= epoch_ms(launched),
            _ => true,
        }
    }
}

/// `at` in whole milliseconds since the epoch (claude's `startedAt` unit).
fn epoch_ms(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// The session a pid file names; `Ok(None)` when it cannot be read or names no usable
/// session id.
fn read_session(path: &Path, budget: &mut Budget) -> Result<Option<Session>, AmbientUnavailable> {
    let Some(bytes) = small_file(path, SESSION_FILE_CAP, budget)? else {
        return Ok(None);
    };
    let Some(file) = decode::<PidFile<'_>>(&bytes) else {
        return Ok(None);
    };
    let Some(id) = file.session_id.str().filter(|id| plain_name(id)) else {
        return Ok(None);
    };
    Ok(Some(Session {
        id: id.to_string(),
        pid: file.pid.int().and_then(|pid| u32::try_from(pid).ok()),
        cwd: file.cwd.str().map(String::from),
        started: file.started_at.int(),
        entrypoint: file.entrypoint.str().map(|e| e == "cli"),
    }))
}

/// The latest started session in `sessions/*.json` of an interactive claude whose `cwd` is
/// `project`, started at or after `launched` and whose process is alive (`pid`, else the
/// file's name).
fn live_session(
    sessions: &Path,
    project: &Path,
    launched: SystemTime,
    budget: &mut Budget,
) -> Result<Session, AmbientUnavailable> {
    let mut best: Option<Session> = None;
    for entry in budget.list(sessions)? {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        // A newer session may be unread if the byte budget runs out.
        let Some(session) = read_session(&path, budget)? else {
            continue;
        };
        let ours = session.entrypoint == Some(true)
            && session.started.is_some()
            && session.since(Some(launched))
            && session
                .cwd
                .as_deref()
                .is_some_and(|cwd| same_dir(cwd, project));
        let named = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse().ok());
        if !ours || !session.pid.or(named).is_some_and(crate::proc::pid_alive) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| (session.started, &session.id) > (b.started, &b.id))
        {
            best = Some(session);
        }
    }
    best.ok_or(AmbientUnavailable::NotFound)
}

/// The directory under `projects` that holds `project`'s transcripts: its dashed name, or —
/// for a name longer than [`SLUG_MAX`] — the directory sharing the first [`SLUG_MAX`] chars
/// that holds `file`, else the only one sharing them (a session before its first message);
/// the plain name when that does not tell.
fn project_dir(
    projects: &Path,
    project: &Path,
    file: &str,
    budget: &mut Budget,
) -> Result<PathBuf, AmbientUnavailable> {
    let name = dashed(project);
    if name.len() > SLUG_MAX {
        // `dashed` is ASCII: any byte index is a char boundary.
        let prefix = &name[..SLUG_MAX];
        let cut: Vec<PathBuf> = budget
            .list(projects)?
            .into_iter()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(prefix))
            })
            .map(|entry| entry.path())
            .filter(|dir| dir.is_dir())
            .collect();
        if let Some(dir) = cut.iter().find(|dir| dir.join(file).is_file()) {
            return Ok(dir.clone());
        }
        if let [only] = cut.as_slice() {
            return Ok(only.clone());
        }
    }
    Ok(projects.join(name))
}

/// One transcript line, as far as turns go.
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    is_sidechain: Json<'a>,
    #[serde(borrow)]
    is_meta: Json<'a>,
    #[serde(borrow)]
    is_api_error_message: Json<'a>,
    #[serde(borrow)]
    is_aborted_mid_stream: Json<'a>,
    #[serde(borrow)]
    interrupted_message_id: Json<'a>,
    #[serde(borrow)]
    subtype: Json<'a>,
    #[serde(borrow)]
    message: Obj<Message<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Message<'a> {
    #[cfg(feature = "questions")]
    #[serde(borrow)]
    role: Json<'a>,
    #[serde(borrow)]
    id: Json<'a>,
    #[serde(borrow)]
    stop_reason: Json<'a>,
    #[serde(borrow)]
    content: Blocks<'a>,
}

#[cfg(feature = "questions")]
impl Message<'_> {
    fn is_prompt(&self) -> bool {
        if !self.role.is("user") {
            return false;
        }
        if self.content.string_prompt {
            return true;
        }
        let Some(items) = self.content.items.as_ref().filter(|_| !self.content.more) else {
            return false;
        };
        let mut has_text = false;
        for item in items {
            match item.0.as_ref().map(|b| (b.kind.str(), b.text.str())) {
                Some((Some("text"), Some(text))) => has_text |= !text.trim().is_empty(),
                Some((Some("image"), _)) => {}
                _ => return false,
            }
        }
        has_text
    }
}

/// The turn's latest assistant message.
struct Candidate {
    /// `message.id`; a line without one is a message of its own.
    id: Option<String>,
    reply: TurnReply,
    /// The last stop reason it carried was `end_turn`.
    end_turn: bool,
}

/// Reads one claude transcript: the turn's latest message until its end.
#[derive(Default)]
struct ClaudeParser {
    candidate: Option<Candidate>,
    /// A prompt or an assistant line was read since the last end.
    open: bool,
    #[cfg(feature = "questions")]
    asked: super::Asked,
}

impl AmbientParser for ClaudeParser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let line = decode::<Line<'_>>(line)?;
        // A subagent's line (older builds wrote them into the main transcript).
        if line.is_sidechain.is_true() {
            return None;
        }
        match line.kind.str()? {
            "assistant" => self.assistant(&line, reply_bytes),
            "user" => self.user(&line),
            "system" => self.system(&line),
            _ => None,
        }
    }

    #[cfg(feature = "questions")]
    fn question(&mut self, line: &[u8]) -> Option<crate::agent::turns::ambient::AmbientQuestion> {
        self.asked.line("claude", line)
    }

    #[cfg(feature = "questions")]
    fn reset_question_context(&mut self) {
        self.asked = Default::default();
    }
}

impl ClaudeParser {
    fn assistant(&mut self, line: &Line<'_>, reply_bytes: usize) -> Option<AmbientRecord> {
        let Some(message) = line
            .message
            .0
            .as_ref()
            .filter(|m| m.content.items.is_some())
        else {
            if !line.is_meta.is_true() {
                self.candidate = None;
            }
            return Some(AmbientRecord::Unrecognised);
        };
        if line.is_meta.is_true() {
            return None;
        }
        self.open = true;
        // A synthetic message about a failed request ("API Error: …"), or one cut short by
        // an interruption: not the model's answer.
        if line.is_api_error_message.is_true() || line.is_aborted_mid_stream.is_true() {
            self.candidate = None;
            return None;
        }
        let id = message.id.str();
        let same = matches!(
            (&self.candidate, id),
            (Some(Candidate { id: Some(current), .. }), Some(id)) if current == id
        );
        if !same {
            self.candidate = None;
        }
        let candidate = self.candidate.get_or_insert_with(|| Candidate {
            id: id.map(String::from),
            reply: TurnReply::default(),
            end_turn: false,
        });
        if let Some(stop) = message.stop_reason.str() {
            candidate.end_turn = stop == "end_turn";
        }
        for block in message.content.blocks() {
            if let (true, Some(text)) = (block.kind.is("text"), block.text.str()) {
                append(&mut candidate.reply, text, reply_bytes);
            }
        }
        // Blocks past the item bound were skipped.
        candidate.reply.truncated |= message.content.more;
        None
    }

    fn user(&mut self, line: &Line<'_>) -> Option<AmbientRecord> {
        // Esc: claude writes no `turn_duration` for the interrupted turn.
        if line.interrupted_message_id.str().is_some() {
            self.candidate = None;
            #[cfg(feature = "questions")]
            self.asked.end();
            return std::mem::take(&mut self.open).then_some(AmbientRecord::Ended(None));
        }
        if line.is_meta.is_true() {
            return None;
        }
        let tool_result = line.message.0.as_ref().is_some_and(|message| {
            message
                .content
                .blocks()
                .any(|block| block.kind.is("tool_result"))
        });
        // A tool result retires the message before it; a prompt starts the turn afresh.
        self.candidate = None;
        self.open |= !tool_result;
        #[cfg(feature = "questions")]
        if line.message.0.as_ref().is_some_and(Message::is_prompt) {
            self.asked.prompt();
        } else if !tool_result {
            self.asked.end();
        }
        None
    }

    fn system(&mut self, line: &Line<'_>) -> Option<AmbientRecord> {
        let Some(subtype) = line.subtype.str() else {
            return Some(AmbientRecord::Unrecognised);
        };
        if subtype != "turn_duration" {
            return None;
        }
        self.open = false;
        #[cfg(feature = "questions")]
        self.asked.end();
        let reply = self
            .candidate
            .take()
            .filter(|message| message.end_turn)
            .map(|message| message.reply);
        Some(AmbientRecord::Ended(reply))
    }
}

#[cfg(test)]
mod tests;
