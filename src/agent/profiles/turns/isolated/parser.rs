use std::path::Path;

use super::super::json::{Text, blocks, fields};
use super::locate::uuid;
use crate::agent::turns::{Binding, Gap, Parser, ProviderId, Record, Reply, TranscriptSource};

fn raw<'a>(row: &'a str, name: &str) -> Result<&'a str, Gap> {
    fields(row, [name])?[0].ok_or(Gap::Format)
}

fn string(raw: &str) -> Result<&str, Gap> {
    serde_json::from_str(raw).map_err(|_| Gap::Format)
}

fn tag<'a>(row: &'a str, name: &str) -> Result<&'a str, Gap> {
    string(raw(row, name)?)
}

fn valid_json(raw: &[u8]) -> Result<&str, Gap> {
    let text = std::str::from_utf8(raw).map_err(|_| Gap::Format)?;
    // Validate trailing input and nested values even for records ignored by the parser.
    serde_json::from_str::<serde::de::IgnoredAny>(text).map_err(|_| Gap::Format)?;
    Ok(text)
}

fn cursor_kind(row: &str) -> Result<&str, Gap> {
    match fields(row, ["role", "type"])? {
        [Some(role), None] => match string(role)? {
            role @ ("user" | "assistant") => Ok(role),
            _ => Err(Gap::Format),
        },
        [None, Some(marker)] if string(marker)? == "turn_ended" => Ok("turn_ended"),
        _ => Err(Gap::Format),
    }
}

pub(super) fn cursor_marker(line: &[u8]) -> Result<bool, Gap> {
    let row = valid_json(line)?;
    let kind = cursor_kind(row)?;
    if kind == "turn_ended" {
        return match tag(row, "status")? {
            "success" | "error" | "aborted" => Ok(true),
            _ => Err(Gap::Format),
        };
    }
    blocks(raw(raw(row, "message")?, "content")?)?;
    Ok(false)
}

pub(super) fn header(
    provider: TranscriptSource,
    line: &[u8],
    cwd: &Path,
    session: &ProviderId,
) -> Result<(), Gap> {
    let row = valid_json(line)?;
    let kind = match provider {
        TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText => cursor_kind(row)?,
        _ => tag(row, "type")?,
    };
    let (id, path) = match provider {
        TranscriptSource::Codex if kind == "session_meta" => {
            let payload = raw(row, "payload")?;
            if tag(payload, "source")? != "cli"
                || tag(payload, "originator")? != "codex-tui"
                || tag(payload, "thread_source")? != "user"
                || tag(payload, "cli_version")? != "0.154.0"
            {
                return Err(Gap::Uncorroborated);
            }
            (tag(payload, "id")?, raw(payload, "cwd")?)
        }
        TranscriptSource::Pi if kind == "session" && raw(row, "version")? == "3" => {
            (tag(row, "id")?, raw(row, "cwd")?)
        }
        // Cursor's first row is a user message, not an identity record. Its binding
        // comes from the newly admitted namespace and the UUID directory / filename.
        TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText
            if kind == "user" =>
        {
            return Ok(());
        }
        _ => return Err(Gap::Uncorroborated),
    };
    let path: String = serde_json::from_str(path).map_err(|_| Gap::Format)?;
    if id != session.as_str() || Path::new(&path) != cwd {
        return Err(Gap::Uncorroborated);
    }
    Ok(())
}

pub(super) struct ProviderParser {
    provider: TranscriptSource,
    turn: Option<ProviderId>,
    candidate: Option<Reply>,
    // Pi branches inside the same append-only file. A discontinuous parent after
    // the observed user entry discards the candidate before it can be published.
    pi_parent: Option<ProviderId>,
    cursor_armed: bool,
    cursor_answer: bool,
}

impl ProviderParser {
    pub(super) fn new(provider: TranscriptSource) -> Self {
        Self {
            provider,
            turn: None,
            candidate: None,
            pi_parent: None,
            cursor_armed: false,
            cursor_answer: false,
        }
    }

    fn codex(&mut self, row: &str, kind: &str, limit: usize) -> Result<Record, Gap> {
        if kind == "session_meta" {
            return Err(Gap::SessionChanged);
        }
        if kind != "event_msg" {
            return Ok(Record::Ignore);
        }
        let payload = raw(row, "payload")?;
        let event = tag(payload, "type")?;
        if !matches!(event, "task_started" | "task_complete" | "turn_aborted") {
            return Ok(Record::Ignore);
        }
        let [id] = fields(payload, ["turn_id"])?;
        if event == "turn_aborted" && matches!(id, None | Some("null")) {
            self.turn = None;
            return Ok(Record::End(None));
        }
        let id = string(id.ok_or(Gap::Format)?)?;
        if !uuid(id) {
            return Err(Gap::Format);
        }
        if event == "task_started" {
            self.turn = Some(ProviderId::new(id)?);
            return Ok(Record::User);
        }
        let Some(current) = self.turn.take() else {
            return Ok(Record::End(None));
        };
        if current.as_str() != id {
            return Err(Gap::Format);
        }
        if event == "turn_aborted" {
            return Ok(Record::End(None));
        }
        let text = raw(payload, "last_agent_message")?;
        if text == "null" {
            return Ok(Record::End(None));
        }
        let mut reply = Reply::default();
        Text::new(text)?.append_to(&mut reply, limit)?;
        Ok(Record::End(Some(reply)))
    }

    fn pi(&mut self, row: &str, kind: &str, limit: usize) -> Result<Record, Gap> {
        if kind == "session" {
            return Err(Gap::SessionChanged);
        }
        let id = ProviderId::new(tag(row, "id")?)?;
        let parent = raw(row, "parentId")?;
        let linear = if let Some(last) = &self.pi_parent {
            string(parent).is_ok_and(|p| p == last.as_str())
        } else {
            true
        };
        self.pi_parent = Some(id);
        if !linear {
            self.candidate = None;
        }
        if kind != "message" {
            // Metadata can be inserted mid-turn. Branch/context edits can change what
            // the candidate refers to and require a fresh prompt.
            if !linear || matches!(kind, "branch_summary" | "context_edit" | "compaction") {
                self.candidate = None;
                return Ok(Record::End(None));
            }
            return Ok(Record::Ignore);
        }
        let msg = raw(row, "message")?;
        match tag(msg, "role")? {
            "user" => {
                let content = raw(msg, "content")?;
                let genuine = if content.starts_with('"') {
                    !Text::new(content)?.is_blank()?
                } else {
                    let mut prompt = false;
                    for block in blocks(content)? {
                        match block.kind {
                            "text" => prompt |= !block.text()?.is_blank()?,
                            "image" => prompt = true,
                            _ => {
                                self.candidate = None;
                                return Ok(Record::End(None));
                            }
                        }
                    }
                    prompt
                };
                let previous = self.candidate.take();
                Ok(if genuine {
                    Record::Next(previous)
                } else {
                    Record::End(None)
                })
            }
            "bashExecution" => {
                Text::new(raw(msg, "command")?)?.validate()?;
                Ok(Record::End(self.candidate.take()))
            }
            "assistant" => {
                self.candidate = None;
                if !linear {
                    return Ok(Record::End(None));
                }
                let stop = tag(msg, "stopReason")?;
                let content = blocks(raw(msg, "content")?)?;
                if matches!(stop, "error" | "aborted") {
                    return Ok(Record::End(None));
                }
                if matches!(stop, "stop" | "length")
                    && content
                        .iter()
                        .all(|b| matches!(b.kind, "text" | "thinking" | "redactedThinking"))
                {
                    let mut reply = Reply::default();
                    for b in content.iter().filter(|b| b.kind == "text") {
                        let text = b.text()?;
                        if !text.is_blank()? {
                            append(&mut reply, text, limit)?;
                        }
                    }
                    if !reply.text.is_empty() || reply.truncated {
                        self.candidate = Some(reply);
                    }
                }
                Ok(Record::Ignore)
            }
            _ => {
                self.candidate = None;
                if linear {
                    Ok(Record::Ignore)
                } else {
                    Ok(Record::End(None))
                }
            }
        }
    }

    fn cursor(&mut self, row: &str, kind: &str, limit: usize) -> Result<Record, Gap> {
        match kind {
            "user" => {
                let content = blocks(raw(raw(row, "message")?, "content")?)?;
                // Tool results are stored under the user envelope in some versions.
                if !content.iter().any(|b| b.kind == "text")
                    || content.iter().any(|b| !matches!(b.kind, "text" | "image"))
                {
                    self.candidate = None;
                    self.cursor_answer = false;
                    return Ok(Record::Ignore);
                }
                let previous = self.cursor_armed && self.cursor_answer;
                self.cursor_armed = true;
                self.cursor_answer = false;
                let mut reply = self.candidate.take().unwrap_or_default();
                reply.completion_only = true;
                reply.inferred = true;
                Ok(Record::Next(previous.then_some(reply)))
            }
            "assistant" => {
                let content = blocks(raw(raw(row, "message")?, "content")?)?;
                for b in content {
                    match b.kind {
                        "tool_use" => {
                            self.candidate = None;
                            self.cursor_answer = false;
                        }
                        "text" if self.cursor_armed => {
                            let text = b.text()?;
                            self.cursor_answer |= !text.is_blank()?;
                            if self.provider == TranscriptSource::CursorWithUnseparatedText {
                                append(
                                    self.candidate.get_or_insert_with(Reply::default),
                                    text,
                                    limit,
                                )?;
                            } else {
                                text.validate()?;
                            }
                        }
                        "thinking" | "redacted_thinking" | "text" => {}
                        _ => return Err(Gap::Format),
                    }
                }
                Ok(Record::Ignore)
            }
            "turn_ended" => {
                let status = tag(row, "status")?;
                let armed = std::mem::take(&mut self.cursor_armed);
                let mut reply = self.candidate.take().unwrap_or_default();
                reply.completion_only = true;
                Ok(Record::End((armed && status == "success").then_some(reply)))
            }
            _ => Ok(Record::Ignore),
        }
    }
}

fn append(reply: &mut Reply, text: Text<'_>, limit: usize) -> Result<(), Gap> {
    if !text.is_empty() {
        if !reply.text.is_empty() {
            reply.push("\n\n", limit);
        }
        text.append_to(reply, limit)?;
    }
    Ok(())
}

impl Parser for ProviderParser {
    fn record(&mut self, _: &Binding, bytes: &[u8], limit: usize) -> Result<Record, Gap> {
        let row = valid_json(bytes)?;
        let kind = match self.provider {
            TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText => {
                cursor_kind(row)?
            }
            _ => tag(row, "type")?,
        };
        match self.provider {
            TranscriptSource::Codex => self.codex(row, kind, limit),
            TranscriptSource::Pi => self.pi(row, kind, limit),
            TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText => {
                self.cursor(row, kind, limit)
            }
        }
    }

    fn has_quiet_candidate(&self) -> bool {
        self.provider == TranscriptSource::Pi && self.candidate.is_some()
    }
    fn quiet(&mut self) -> Option<Reply> {
        if self.provider == TranscriptSource::Pi {
            self.candidate.take()
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.turn = None;
        self.pi_parent = None;
        self.candidate = None;
        self.cursor_armed = false;
        self.cursor_answer = false;
    }
}
