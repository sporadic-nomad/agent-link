//! codex's ambient source. Recorded on codex 0.154.0; no version is pinned.
//!
//! **Where.** The root is `CODEX_HOME` (nonempty), else `<home>/.codex`; a relative one is
//! taken against the agent's cwd. Rollouts are `sessions/YYYY/MM/DD/rollout-*.jsonl`, by
//! the local date a session began: the UTC dates from two days back to one ahead are
//! listed, which holds the local today and yesterday in every zone. The current rollout
//! stays eligible after its directory ages out. Of the rollouts
//! modified at or after the agent's launch, the newest whose first line — its
//! `session_meta` — names the project as its `cwd` and is the interactive TUI's:
//! `payload.source` is `"cli"` and `payload.thread_source` is not `"subagent"`
//! (`codex exec` writes `exec`, `codex app-server` `vscode`, a subagent an object). A
//! decoded header is cached while the file metadata stays unchanged. Two interactive codex sessions in one project
//! cannot be told apart: the one written last is followed.
//!
//! **Turns.** An `event_msg` whose payload is `task_complete` ends a turn explicitly, its
//! `last_agent_message` the reply (`null` when the turn failed or wrote none);
//! `turn_aborted` (Esc) ends it without one. A `task_complete` without
//! `last_agent_message`, or an `event_msg` without a payload type, is
//! [`AmbientRecord::Unrecognised`]. Other lines, also unknown ones, change nothing.
//!
//! **Questions** (feature `questions`): a Plan-mode `request_user_input` `response_item`
//! `function_call`, read as the controlled tier reads it, is asked until the
//! `function_call_output` with its `call_id`, in a turn whose `task_started` was read; the
//! next `task_started`, `task_complete` and `turn_aborted` withdraw it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::json::{Json, Obj, decode};
use super::{Budget, Heads};
use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::paths::{anchored, config_root, same_dir, written};
use crate::agent::turns::ambient::{
    AmbientContext, AmbientParser, AmbientRecord, AmbientSource, AmbientUnavailable,
};

/// Bytes of a rollout's first line read at most: its `session_meta` carries codex's base
/// instructions (about 20 KiB in 0.154.0).
const META_CAP: u64 = 1024 * 1024;

/// codex's [`AmbientSource`].
#[derive(Default)]
pub(in crate::agent) struct CodexAmbient {
    heads: Heads<RolloutHead>,
}

impl AmbientSource for CodexAmbient {
    fn locate(&self, ctx: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        self.rollout_at(ctx, SystemTime::now())
    }

    fn accepts_file(&self, ctx: &AmbientContext, file: &std::fs::File) -> std::io::Result<bool> {
        Ok(self
            .heads
            .opened(file, META_CAP, RolloutHead::read)?
            .is_some_and(|head| head.is_tui_session_of(&ctx.cwd)))
    }

    fn is_session_header(&self, line: &[u8]) -> bool {
        decode::<Meta<'_>>(line).is_some_and(|h| h.kind.is("session_meta"))
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::<CodexParser>::default()
    }
}

/// What a rollout's first line says.
#[derive(Clone)]
struct RolloutHead {
    /// `payload.cwd` of a `session_meta`.
    cwd: Option<String>,
    /// The interactive TUI's.
    tui: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Meta<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    payload: Obj<MetaPayload<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct MetaPayload<'a> {
    #[serde(borrow)]
    cwd: Json<'a>,
    #[serde(borrow)]
    source: Json<'a>,
    #[serde(borrow)]
    thread_source: Json<'a>,
}

impl RolloutHead {
    fn read(line: &[u8]) -> Self {
        let meta = decode::<Meta<'_>>(line).filter(|meta| meta.kind.is("session_meta"));
        let payload = meta.and_then(|meta| meta.payload.0);
        Self {
            cwd: payload.as_ref().and_then(|p| p.cwd.str()).map(String::from),
            tui: payload
                .as_ref()
                .is_some_and(|p| p.source.is("cli") && !p.thread_source.is("subagent")),
        }
    }

    fn is_tui_session_of(&self, project: &Path) -> bool {
        self.tui
            && self
                .cwd
                .as_deref()
                .is_some_and(|cwd| same_dir(cwd, project))
    }
}

impl CodexAmbient {
    /// The rollout the codex in `ctx` writes, with the date directories of `now`.
    fn rollout_at(
        &self,
        ctx: &AmbientContext,
        now: SystemTime,
    ) -> Result<PathBuf, AmbientUnavailable> {
        let root = config_root(ctx.var("CODEX_HOME"), ctx.home(), ".codex")
            .ok_or(AmbientUnavailable::NoConfigRoot)?;
        let sessions = anchored(root, &ctx.cwd).join("sessions");
        let since = ctx.launched_at.unwrap_or(UNIX_EPOCH);
        let mut budget = Budget::new();
        let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
        let mut consider = |path: PathBuf| {
            let Ok(meta) = path.metadata() else { return };
            if let Some(modified) = written(&meta).filter(|at| meta.is_file() && *at >= since) {
                candidates.push((modified, path));
            }
        };
        let mut previous = ctx
            .current_transcript
            .as_ref()
            .filter(|p| p.starts_with(&sessions));
        for day in day_dirs(now) {
            let dir = sessions.join(day);
            previous = previous.filter(|path| path.parent() != Some(&dir));
            for entry in budget.list(&dir)? {
                let named = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                if named {
                    consider(entry.path());
                }
            }
        }
        // The TUI keeps appending under the session's start date.
        if let Some(path) = previous {
            consider(path.clone());
        }
        // Newest first (the path breaks a tie).
        candidates.sort_by(|a, b| b.cmp(a));
        for (_, path) in candidates {
            if self
                .heads
                .of(&path, META_CAP, &mut budget, RolloutHead::read)?
                .is_some_and(|head| head.is_tui_session_of(&ctx.cwd))
            {
                return Ok(path);
            }
        }
        Err(AmbientUnavailable::NotFound)
    }
}

/// The date directories (`YYYY/MM/DD`) listed at `now`, newest first. codex names them by
/// the local date and std knows no time zone, so these are the UTC dates from two days back
/// to one ahead: they hold the local today and yesterday in every zone (UTC−12 to UTC+14).
fn day_dirs(now: SystemTime) -> impl Iterator<Item = PathBuf> {
    let today = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400);
    // Days since the epoch fit an i64 by far.
    let today = i64::try_from(today).unwrap_or(0);
    (-2..=1).rev().map(move |offset| {
        let (year, month, day) = civil_date(today + offset);
        [
            format!("{year:04}"),
            format!("{month:02}"),
            format!("{day:02}"),
        ]
        .iter()
        .collect()
    })
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year from March 1, [0, 365]
    let mp = (5 * doy + 2) / 153; // month from March, [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are in range by construction.
    (year, month as u32, day as u32)
}

/// One rollout line, as far as turns go.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    payload: Obj<Event<'a>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: Json<'a>,
    #[serde(borrow)]
    last_agent_message: Json<'a>,
}

/// Reads one rollout. Each turn end stands on its own line: no turn state (only the
/// question rules' with `questions`).
#[derive(Default)]
struct CodexParser {
    #[cfg(feature = "questions")]
    asked: super::Asked,
}

impl AmbientParser for CodexParser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let line = decode::<Line<'_>>(line)?;
        if !line.kind.is("event_msg") {
            return None;
        }
        let Some(kind) = line.payload.0.as_ref().and_then(|event| event.kind.str()) else {
            return Some(AmbientRecord::Unrecognised);
        };
        #[cfg(feature = "questions")]
        match kind {
            "task_started" => self.asked.prompt(),
            "task_complete" | "turn_aborted" => self.asked.end(),
            _ => {}
        }
        let message = line
            .payload
            .0
            .as_ref()
            .map(|event| &event.last_agent_message);
        match (kind, message) {
            ("task_complete", Some(Json::Str(text))) => {
                let mut reply = TurnReply::default();
                reply.push(text, reply_bytes);
                Some(AmbientRecord::Ended(Some(reply)))
            }
            ("task_complete", Some(Json::Null)) | ("turn_aborted", _) => {
                Some(AmbientRecord::Ended(None))
            }
            ("task_complete", _) => Some(AmbientRecord::Unrecognised),
            _ => None,
        }
    }

    #[cfg(feature = "questions")]
    fn question(&mut self, line: &[u8]) -> Option<crate::agent::turns::ambient::AmbientQuestion> {
        self.asked.line("codex", line)
    }

    #[cfg(feature = "questions")]
    fn reset_question_context(&mut self) {
        self.asked = Default::default();
    }
}

#[cfg(test)]
mod tests;
