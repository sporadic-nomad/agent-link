//! cursor's ambient source. Recorded on cursor 2026.10.01 (and the controlled tier's
//! 2026.11.13-eacade1 writer images); no version is pinned.
//!
//! **Where.** By cursor's own rules (its configuration paths and workspace slug): the root
//! is `CURSOR_DATA_DIR` unless blank, else `<home>/.cursor`; a relative one is taken
//! against the agent's cwd. Each conversation is
//! `projects/<slug>/agent-transcripts/<id>/<id>.jsonl`, `<slug>` the project's resolved
//! path with every char outside `[A-Za-z0-9]` made `-`, runs collapsed and ends trimmed. A
//! subagent's transcript (`<id>/subagents/…`) is never taken, and a conversation directory
//! without its `<id>.jsonl` is passed over. Of the conversations created at or after the
//! launch, the one written last (cursor writes a transcript with the first prompt; `/new`
//! starts another, `/resume` writes to an earlier one). One created before the launch is
//! never followed — cursor rewrites its file in place, so where it stood when listed says
//! nothing about what is new — and while one is written after the followed one (`/resume`
//! of an older conversation) the lookup declines ([`AmbientUnavailable::Ambiguous`]) rather
//! than read on as if healthy. Without a launch time no conversation is taken. cursor's
//! print mode (`agent -p`) writes the same transcripts: nothing tells them apart.
//!
//! **Turns.** Lines are messages (`role` `user` / `assistant`, `message.content` blocks)
//! and `{"type":"turn_ended","status":…}`. A `turn_ended` with `status: "success"` ends a
//! turn explicitly. cursor rewrites the whole file at every turn's first write, dropping
//! the earlier `turn_ended` lines (the observer re-reads it and goes on after the turn end
//! it handled last); a prompt after an answer with no `turn_ended` between them — one a
//! rewrite dropped before any read saw it — ends that turn
//! ([`AmbientRecord::EndedBefore`]), its status unknown. As in the controlled tier, a turn
//! that ended otherwise (`aborted`, `error`) is not reported, so an explicit end is a
//! successful one. A `user` line without text, or with blocks other than text and images,
//! is a tool result some versions write: the text before it was narration.
//!
//! **Text.** cursor joins a message's reasoning into its text block with no marker, so its
//! text is not the displayed answer alone. Like the controlled tier's
//! `TranscriptSource::Cursor`, the profile's source therefore reports completion only:
//! every turn's `reply` is `None` and no text is kept.
//! [`CursorProfile::ambient_turns_with_unseparated_text`](crate::agent::profiles::CursorProfile::ambient_turns_with_unseparated_text)
//! opts in to the text blocks after the turn's last `tool_use`, joined by a blank line,
//! exactly as written (the controlled tier's `CursorWithUnseparatedText`).
//!
//! A `turn_ended` without a `status`, or a `user` or `assistant` line without a
//! `message.content` array, is [`AmbientRecord::Unrecognised`] and clears the pending reply and inference state.
//! Questions (the command
//! approval, read off the screen) are not part of this source.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use super::json::{Blocks, Json, Obj, decode};
use super::{Budget, append, dashed};
use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::paths::{anchored, created, project_path, written};
use crate::agent::turns::ambient::{
    AmbientContext, AmbientParser, AmbientRecord, AmbientSource, AmbientUnavailable,
};

/// cursor's data-directory override.
const DATA_DIR_VAR: &str = "CURSOR_DATA_DIR";

/// cursor's [`AmbientSource`]; with `text`, the unseparated reply text is reported.
pub(in crate::agent) struct CursorAmbient {
    pub(in crate::agent) text: bool,
}

/// cursor's directory name for a project: every char outside `[A-Za-z0-9]` made `-`, runs
/// collapsed, ends trimmed (`/Users/me/my proj` → `Users-me-my-proj`).
fn cursor_slug(project: &Path) -> String {
    dashed(project)
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// cursor's data directory: `over` unless blank, else `<home>/.cursor`. Not anchored.
fn cursor_root(over: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match over.filter(|dir| !dir.to_string_lossy().trim().is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home
            .filter(|home| !home.as_os_str().is_empty())
            .map(|home| home.join(".cursor")),
    }
}

impl AmbientSource for CursorAmbient {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        transcript(ctx, |_, meta| created(meta))
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::new(CursorParser {
            text: self.text,
            after_tool: TurnReply::default(),
            answered: false,
        })
    }
}

/// The transcript the cursor in `ctx` writes (see the module docs), `born` telling when a
/// file was created (tests set it: few file systems let a creation time be set).
fn transcript(
    ctx: &AmbientContext,
    born: impl Fn(&Path, &std::fs::Metadata) -> Option<SystemTime>,
) -> Result<PathBuf, AmbientUnavailable> {
    let root =
        cursor_root(ctx.var(DATA_DIR_VAR), ctx.home()).ok_or(AmbientUnavailable::NoConfigRoot)?;
    let project = project_path(&ctx.cwd);
    let slug = cursor_slug(&project);
    if slug.is_empty() {
        return Err(AmbientUnavailable::Unsupported);
    }
    let dir = anchored(root, &ctx.cwd)
        .join("projects")
        .join(slug)
        .join("agent-transcripts");
    // Without a launch time another conversation cannot be told apart.
    let launched = ctx.launched_at.ok_or(AmbientUnavailable::AgentUnknown)?;
    // The latest written of the conversations created since the launch (the one
    // followed), and of those created before it but written since (resumed).
    let mut followed: Option<(Option<SystemTime>, PathBuf)> = None;
    let mut resumed: Option<SystemTime> = None;
    for entry in Budget::new().list(&dir)? {
        let mut name = entry.file_name();
        name.push(".jsonl");
        let file = entry.path().join(name);
        let Some(meta) = std::fs::metadata(&file).ok().filter(|meta| meta.is_file()) else {
            continue;
        };
        let Some(born) = born(&file, &meta) else {
            continue;
        };
        let at = written(&meta);
        if born >= launched {
            // Written last (the path breaks a tie).
            if followed
                .as_ref()
                .is_none_or(|best| (&at, &file) > (&best.0, &best.1))
            {
                followed = Some((at, file));
            }
        } else if let Some(at) = at.filter(|at| *at >= launched) {
            resumed = resumed.max(Some(at));
        }
    }
    // A conversation from before the launch written last: the agent resumed it. It is
    // not followed, and the one followed no longer grows.
    if let Some(at) = resumed {
        if followed
            .as_ref()
            .is_none_or(|(written, _)| Some(at) > *written)
        {
            return Err(AmbientUnavailable::Ambiguous);
        }
    }
    followed
        .map(|(_, file)| file)
        .ok_or(AmbientUnavailable::NotFound)
}

/// One transcript line, as far as turns go.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    status: Json<'a>,
    #[serde(borrow)]
    role: Json<'a>,
    #[serde(borrow)]
    message: Obj<Message<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Message<'a> {
    #[serde(borrow)]
    content: Blocks<'a, true>,
}

/// Reads one cursor transcript: the turn's text since its last tool call.
struct CursorParser {
    /// Keep the unseparated text.
    text: bool,
    /// The turn's nonblank text blocks after its last `tool_use` (empty without `text`).
    after_tool: TurnReply,
    /// An assistant line came since the last prompt or `turn_ended`.
    answered: bool,
}

impl CursorParser {
    /// The turn's reply so far; the turn starts over.
    fn take_reply(&mut self) -> Option<TurnReply> {
        self.answered = false;
        let reply = std::mem::take(&mut self.after_tool);
        (self.text && (reply.truncated || !reply.text.is_empty())).then_some(reply)
    }
}

impl AmbientParser for CursorParser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let Some(line) = decode::<Line<'_>>(line) else {
            return Some(AmbientRecord::Unchanged);
        };
        if line.kind.is("turn_ended") {
            let reply = self.take_reply();
            let Some(status) = line.status.str() else {
                return Some(AmbientRecord::Unrecognised);
            };
            // Aborted and failed turns are not reported, as in the controlled tier.
            return Some(if status == "success" {
                AmbientRecord::Ended(reply)
            } else {
                AmbientRecord::SuppressedEnd
            });
        }
        let Some(role @ ("user" | "assistant")) = line.role.str() else {
            return Some(AmbientRecord::Unchanged);
        };
        let Some(content) = line
            .message
            .0
            .as_ref()
            .map(|message| &message.content)
            .filter(|content| content.items.is_some())
        else {
            self.take_reply();
            return Some(AmbientRecord::Unrecognised);
        };
        if role == "user" {
            if !content.cursor_prompt {
                // A tool result: what came before it was narration.
                self.after_tool = TurnReply::default();
                return None;
            }
            // A prompt after an answer with no `turn_ended` between: a rewrite dropped it.
            let answered = self.answered;
            let reply = self.take_reply();
            return answered.then_some(AmbientRecord::EndedBefore(reply));
        }
        self.answered = true;
        for block in content.blocks() {
            if block.kind.is("tool_use") {
                self.after_tool = TurnReply::default();
            } else if let (true, true, Some(text)) =
                (self.text, block.kind.is("text"), block.text.str())
            {
                append(&mut self.after_tool, text, reply_bytes);
            }
        }
        if self.text {
            self.after_tool.truncated |= content.more;
        }
        None
    }
}

#[cfg(test)]
mod tests;
