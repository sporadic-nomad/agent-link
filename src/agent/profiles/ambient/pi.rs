//! pi's ambient source. Recorded on pi 0.85.1 (and the controlled tier's pi 1.5.2
//! capture); no version is pinned.
//!
//! **Where.** By pi's own rules (`getDefaultSessionDirPath`, `getAgentDir`): the sessions
//! directory is `PI_CODING_AGENT_SESSION_DIR` (nonempty), which holds every project's
//! sessions directly, else `<agent dir>/sessions/--<project>--`, the agent dir being
//! `PI_CODING_AGENT_DIR` (nonempty) else `<home>/.pi/agent`, and `<project>` the resolved
//! path with a leading separator dropped and `/`, `\` and `:` made `-`. A leading `~` is the
//! home; a relative directory is taken against the agent's cwd. (`--session-dir` and the
//! `sessionDir` setting are not read.) Of the `*.jsonl` there written at or after the
//! launch whose first line (`type: "session"`) names the project as its `cwd`, the one
//! written last: pi writes a session with its first reply; `/new`, `/fork` and `/clone`
//! start another, `/resume` writes to an earlier one again — also one from before the
//! launch, which the observer reads from its size when it first listed the directory. A
//! decoded header is cached while the file metadata stays unchanged. Without a launch time no session is taken.
//! pi's RPC mode writes the same sessions in the same place: nothing tells them from the
//! interactive ones.
//!
//! **Turns.** pi writes no turn-end record, so every end is inferred. A turn's final
//! message is an assistant `message` that stopped with `stop` or `length` and holds no
//! `toolCall`; its nonblank `text` blocks, joined by a blank line, are the reply (thinking
//! never counts). It ends the turn when the user's next prompt or `!` command
//! (`bashExecution`) follows it ([`AmbientRecord::EndedBefore`]) or when the transcript
//! stays quiet ([`AmbientParser::quiet`]). An assistant message that stopped with `error`
//! or `aborted` ends the turn with no reply, the same way. A later message of any other
//! role (a tool result: pi went on) takes the final message back. Entries that are no
//! `message` (model and thinking changes, labels, compaction, extension entries) change
//! nothing. A `message` without a `role`, or an assistant one without a `content` array
//! or a `stopReason`, is [`AmbientRecord::Unrecognised`].
//!
//! pi has no question tool.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::json::{Blocks, Json, Obj, decode};
use super::{Budget, Heads, append};
use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::paths::{anchored, project_path, same_dir, written};
use crate::agent::turns::ambient::{
    AmbientContext, AmbientParser, AmbientRecord, AmbientSource, AmbientUnavailable,
};

/// pi's session-directory override.
const SESSION_DIR_VAR: &str = "PI_CODING_AGENT_SESSION_DIR";
/// pi's configuration-directory override.
const AGENT_DIR_VAR: &str = "PI_CODING_AGENT_DIR";
/// Bytes of a session's first line read at most (a header is under 1 KiB).
const HEADER_CAP: u64 = 64 * 1024;

/// pi's [`AmbientSource`].
#[derive(Default)]
pub(in crate::agent) struct PiAmbient {
    heads: Heads<Option<String>>,
}

/// pi's directory name for a project's sessions: `--<path>--`, `path` without a leading
/// `/` or `\`, every `/`, `\` and `:` made `-` (`/Users/me/proj` → `--Users-me-proj--`,
/// `C:\Users\me\proj` → `--C--Users-me-proj--`). One path component.
fn pi_dir_name(project: &Path) -> String {
    let path = project.to_string_lossy();
    let path = path.strip_prefix(['/', '\\']).unwrap_or(&path);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

/// pi's sessions directory for `project` (see the module docs); `None` without either
/// override and a home. Not anchored.
fn sessions_dir(
    session_over: Option<OsString>,
    agent_over: Option<OsString>,
    home: Option<PathBuf>,
    project: &Path,
) -> Option<PathBuf> {
    let home = home.filter(|home| !home.as_os_str().is_empty());
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    if let Some(dir) = set(session_over) {
        return Some(expand_tilde(dir, home.as_deref()));
    }
    let agent = match set(agent_over) {
        Some(dir) => expand_tilde(dir, home.as_deref()),
        None => home?.join(".pi").join("agent"),
    };
    Some(agent.join("sessions").join(pi_dir_name(project)))
}

/// `dir` with a leading `~` (alone, or before `/` — Windows also `\`) made the home, as pi
/// expands it.
fn expand_tilde(dir: OsString, home: Option<&Path>) -> PathBuf {
    if let (Some(text), Some(home)) = (dir.to_str(), home) {
        if text == "~" {
            return home.to_path_buf();
        }
        let rest = text
            .strip_prefix("~/")
            .or_else(|| text.strip_prefix("~\\").filter(|_| cfg!(windows)));
        if let Some(rest) = rest {
            return home.join(rest);
        }
    }
    PathBuf::from(dir)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Header<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    cwd: Json<'a>,
}

/// A session header's `cwd`; `None` when the line is no session header.
fn header_cwd(line: &[u8]) -> Option<String> {
    let header = decode::<Header<'_>>(line).filter(|h| h.kind.is("session"))?;
    header.cwd.str().map(String::from)
}

impl AmbientSource for PiAmbient {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        let project = project_path(&ctx.cwd);
        let dir = sessions_dir(
            ctx.var(SESSION_DIR_VAR),
            ctx.var(AGENT_DIR_VAR),
            ctx.home(),
            &project,
        )
        .ok_or(AmbientUnavailable::NoConfigRoot)?;
        let dir = anchored(dir, &ctx.cwd);
        // Without a launch time another pi's session cannot be told apart.
        let launched = ctx.launched_at.ok_or(AmbientUnavailable::AgentUnknown)?;
        let mut budget = Budget::new();
        let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
        for entry in budget.list(&dir)? {
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".jsonl"))
            {
                continue;
            }
            let path = entry.path();
            let Some(meta) = path.metadata().ok().filter(|meta| meta.is_file()) else {
                continue;
            };
            if let Some(at) = written(&meta).filter(|at| *at >= launched) {
                candidates.push((at, path));
            }
        }
        // Written last first (the path breaks a tie).
        candidates.sort_by(|a, b| b.cmp(a));
        for (_, path) in candidates {
            if self
                .heads
                .of(&path, HEADER_CAP, &mut budget, header_cwd)?
                .flatten()
                .is_some_and(|cwd| same_dir(&cwd, &ctx.cwd))
            {
                return Ok(path);
            }
        }
        Err(AmbientUnavailable::NotFound)
    }

    fn accepts_file(&self, ctx: &AmbientContext, file: &std::fs::File) -> std::io::Result<bool> {
        Ok(self
            .heads
            .opened(file, HEADER_CAP, header_cwd)?
            .flatten()
            .is_some_and(|cwd| same_dir(&cwd, &ctx.cwd)))
    }

    fn is_session_header(&self, line: &[u8]) -> bool {
        decode::<Header<'_>>(line).is_some_and(|h| h.kind.is("session"))
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::<PiParser>::default()
    }
}

/// One session entry, as far as turns go.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    message: Obj<Message<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Message<'a> {
    #[serde(borrow)]
    role: Json<'a>,
    #[serde(borrow)]
    content: Blocks<'a>,
    #[serde(borrow)]
    stop_reason: Json<'a>,
}

/// Reads one pi session: whether its last message ends a turn.
#[derive(Default)]
struct PiParser {
    /// The last message ends the turn, with this reply (`None`: an error or an abort).
    end: Option<Option<TurnReply>>,
}

impl AmbientParser for PiParser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let Some(line) = decode::<Line<'_>>(line).filter(|line| line.kind.is("message")) else {
            return Some(AmbientRecord::Unchanged);
        };
        let Some(message) = line.message.0.as_ref() else {
            self.end = None;
            return Some(AmbientRecord::Unrecognised);
        };
        let Some(role) = message.role.str() else {
            self.end = None;
            return Some(AmbientRecord::Unrecognised);
        };
        match role {
            "assistant" => {
                let (Some(_), Some(stop)) = (&message.content.items, message.stop_reason.str())
                else {
                    self.end = None;
                    return Some(AmbientRecord::Unrecognised);
                };

                self.end = match stop {
                    "stop" | "length" if !message.content.pi_tool_call => {
                        let mut reply = TurnReply::default();
                        for block in message
                            .content
                            .blocks()
                            .filter(|block| block.kind.is("text"))
                        {
                            append(&mut reply, block.text.str().unwrap_or(""), reply_bytes);
                        }
                        reply.truncated |= message.content.more;
                        Some(Some(reply))
                    }
                    "error" | "aborted" => Some(None),
                    // `toolUse`, or a stop with tool calls: the turn goes on.
                    _ => None,
                };
                None
            }
            // The user acted at the composer: the turn ended with the final message.
            "user" | "bashExecution" => self.end.take().map(AmbientRecord::EndedBefore),
            _ => {
                self.end = None;
                None
            }
        }
    }

    fn quiet(&mut self, _reply_bytes: usize) -> Option<AmbientRecord> {
        self.end.take().map(AmbientRecord::Ended)
    }
}

#[cfg(test)]
mod tests;
