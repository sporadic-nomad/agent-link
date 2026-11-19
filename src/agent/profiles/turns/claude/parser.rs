use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::VERSION;
use super::wire::{Assistant, Header, MAX_BLOCKS, Origin, Row, Tag, Text, User, blocks, object};
use crate::agent::turns::{Binding, Gap, Parser, Record, Reply};

const MAX_MESSAGE_IDS: usize = 256;

#[derive(Default)]
struct Message {
    id: Box<str>,
    blocks: usize,
    reply: Reply,
    has_text: bool,
    excluded: bool,
    final_stop: bool,
}

/// Only the latest candidate retains text. A completed or superseded message
/// cannot appear again as a fresh candidate within the same user turn.
#[derive(Default)]
struct Turn {
    last: Option<Message>,
    retired: HashSet<Box<str>>,
}

impl Turn {
    fn retire(&mut self) {
        if let Some(message) = self.last.take() {
            self.retired.insert(message.id);
        }
    }

    fn message(&mut self, id: &str) -> Result<&mut Message, Gap> {
        if self.last.as_ref().is_some_and(|m| m.id.as_ref() != id) {
            self.retire();
        }
        if self.retired.contains(id) {
            return Err(Gap::Format);
        }
        if self.retired.len() == MAX_MESSAGE_IDS {
            return Err(Gap::Oversized);
        }
        Ok(self.last.get_or_insert_with(|| Message {
            id: id.into(),
            ..Message::default()
        }))
    }
}

pub(super) struct ClaudeParser {
    cwd: PathBuf,
    turn: Option<Turn>,
}

impl ClaudeParser {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self { cwd, turn: None }
    }

    fn finish(&mut self, publish: bool) -> Record {
        let reply = self
            .turn
            .take()
            .and_then(|turn| turn.last)
            .filter(|m| publish && m.final_stop && !m.excluded && m.has_text);
        Record::End(reply.map(|m| m.reply))
    }

    fn user(&mut self, row: &Row<'_>) -> Result<Record, Gap> {
        if let Some(id) = row.interrupted_message_id {
            check_id(id)?;
            return Ok(self.finish(false));
        }
        if row.is_meta {
            return Ok(Record::Ignore);
        }
        let (msg, content) = row.message::<User>()?;
        if msg.role != "user" {
            return Err(Gap::Format);
        }
        let has_text = if content.starts_with('"') {
            let text = Text::new(content)?;
            text.validate()?;
            !text.is_empty()
        } else {
            let mut has_text = false;
            let mut tool_result = false;
            for block in blocks(content)? {
                match block.kind {
                    "tool_result" => tool_result = true,
                    "text" => {
                        let text = block.text()?;
                        text.validate()?;
                        has_text |= !text.is_empty();
                    }
                    _ => {}
                }
            }
            if tool_result {
                if let Some(turn) = &mut self.turn {
                    turn.retire();
                }
                return Ok(Record::Ignore);
            }
            has_text
        };
        // The recorded local /clear command has none of the prompt provenance
        // fields. A partially identified prompt cannot stand in for that record.
        if row.prompt_source.is_none() && (row.origin.is_some() || row.turn_origin.is_some()) {
            return Err(Gap::Format);
        }
        if !has_text || row.prompt_source != Some("typed") {
            return Ok(self.finish(false));
        }
        let origin: Origin = object(row.origin.ok_or(Gap::Format)?)?;
        let turn_origin = row.turn_origin.ok_or(Gap::Format)?;
        if origin.kind != "human" || turn_origin != "human" {
            return Ok(self.finish(false));
        }
        self.turn = Some(Turn::default());
        Ok(Record::User)
    }

    fn assistant(&mut self, row: &Row<'_>, limit: usize) -> Result<Record, Gap> {
        let (msg, content) = row.message::<Assistant>()?;
        check_id(msg.id)?;
        let index = row.api_block_index.ok_or(Gap::Format)?;
        let blocks = blocks(content)?;
        if msg.role != "assistant" || blocks.is_empty() {
            return Err(Gap::Format);
        }
        let Some(turn) = &mut self.turn else {
            // Attachment can start between fragments. Their required fields
            // still have to be valid even though no fresh prompt owns them.
            for block in blocks.iter().filter(|b| b.kind == "text") {
                block.text()?.validate()?;
            }
            return Ok(Record::Ignore);
        };
        let last = turn.message(msg.id)?;
        if index != last.blocks as u64 {
            return Err(Gap::Format);
        }
        last.blocks += blocks.len();
        if last.blocks > MAX_BLOCKS {
            return Err(Gap::Oversized);
        }
        last.final_stop = msg.stop_reason == Some("end_turn");
        last.excluded |= msg.stop_reason.is_some() && !last.final_stop;
        for block in blocks {
            match block.kind {
                "text" => {
                    let text = block.text()?;
                    if !text.is_empty() {
                        if last.has_text {
                            last.reply.push("\n\n", limit);
                        }
                        text.append_to(&mut last.reply, limit)?;
                        last.has_text = true;
                    }
                }
                "thinking" | "redacted_thinking" => {}
                _ => last.excluded = true,
            }
        }
        Ok(Record::Ignore)
    }
}

fn check_id(id: &str) -> Result<(), Gap> {
    if id.len() > 256 {
        return Err(Gap::Oversized);
    }
    if id.is_empty() {
        return Err(Gap::Format);
    }
    Ok(())
}

impl Parser for ClaudeParser {
    fn record(&mut self, binding: &Binding, line: &[u8], limit: usize) -> Result<Record, Gap> {
        let json = std::str::from_utf8(line).map_err(|_| Gap::Format)?;
        let tag: Tag = object(json)?;
        if tag.sidechain == Some(true) {
            return Ok(Record::Ignore);
        }
        if tag.kind == "mode" {
            let mode: Header = object(json)?;
            if mode.session != binding.current.as_str() {
                return Err(Gap::SessionChanged);
            }
            if mode.mode != "normal" {
                return Err(Gap::Uncorroborated);
            }
            return Ok(Record::Ignore);
        }
        if !matches!(tag.kind, "user" | "assistant" | "system") {
            return Ok(Record::Ignore);
        }
        tag.sidechain.ok_or(Gap::Format)?;
        let row = Row::parse(json)?;
        if row.session_id != binding.current.as_str() {
            return Err(Gap::SessionChanged);
        }
        if row
            .launch_id
            .is_some_and(|id| id != binding.initial.as_str())
            || row.entrypoint != "cli"
            || row.version != VERSION
            || Path::new(row.cwd.as_ref()) != self.cwd
        {
            return Err(Gap::Uncorroborated);
        }
        if tag.kind == "assistant" && !row.is_meta && row.launch_id.is_none() {
            return Err(Gap::Format);
        }
        match tag.kind {
            "user" => self.user(&row),
            "assistant" if row.is_aborted_mid_stream || row.is_api_error_message => {
                Ok(self.finish(false))
            }
            _ if row.is_meta => Ok(Record::Ignore),
            "assistant" => self.assistant(&row, limit),
            _ if row.subtype.ok_or(Gap::Format)? == "turn_duration" => {
                row.duration_ms.ok_or(Gap::Format)?;
                Ok(self.finish(true))
            }
            _ => Ok(Record::Ignore),
        }
    }

    fn reset(&mut self) {
        self.turn = None;
    }
}
