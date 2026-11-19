//! Fresh tool calls from the verified transcript parser. A malformed or ambiguous
//! request withdraws the previous one. Raw JSON slices retain duplicate-field detection
//! and leave unrelated tool arguments and results undecoded.
use super::{MAX_OPTIONS, MAX_QUESTION_BYTES, MAX_QUESTIONS, PendingQuestion, QuestionItem};
use crate::agent::profiles::turns::json::{array, fields, object};
use serde::Deserialize;
use serde::de::IgnoredAny;

pub(in crate::agent) enum TranscriptChange {
    Keep,
    Open(PendingQuestion),
    Resolved(String),
    Clear,
}

pub(in crate::agent) fn provider_record(provider: &str, line: &[u8]) -> TranscriptChange {
    if line.len() > MAX_QUESTION_BYTES {
        return TranscriptChange::Clear;
    }
    match provider {
        "claude" => claude(line).unwrap_or(TranscriptChange::Clear),
        "codex" => codex(line).unwrap_or(TranscriptChange::Clear),
        _ => TranscriptChange::Keep,
    }
}

#[derive(Deserialize)]
struct Item {
    header: String,
    question: String,
    #[serde(rename = "multiSelect")]
    multi: Option<bool>,
    #[serde(rename = "isOther")]
    other: Option<bool>,
    #[serde(rename = "isSecret")]
    secret: Option<bool>,
}
#[derive(Deserialize)]
struct Choice {
    label: String,
}

fn request(id: &str, raw: &str, claude: bool) -> Option<TranscriptChange> {
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    // Codex embeds the request as a JSON string. Validate that document before
    // borrowing field ranges from it, just as for a Claude tool input.
    object::<IgnoredAny>(raw).ok()?;
    let [items] = fields(raw, ["questions"]).ok()?;
    let items = array(items?, MAX_QUESTIONS, Ok).ok()?;
    let mut questions = Vec::with_capacity(items.len());
    for item in items {
        let q: Item = object(item).ok()?;
        if q.secret == Some(true) {
            return None;
        }
        let [options] = fields(item, ["options"]).ok()?;
        let options = array(options?, MAX_OPTIONS, |raw| {
            object::<Choice>(raw).map(|choice| choice.label)
        })
        .ok()?;
        questions.push(
            QuestionItem::new(q.header, q.question, options)
                .with_multi_select(claude && q.multi == Some(true))
                .with_free_text(if claude {
                    q.multi != Some(true)
                } else {
                    q.other.unwrap_or(true)
                }),
        );
    }
    let pending = PendingQuestion::new(id, questions);
    pending.valid().then_some(TranscriptChange::Open(pending))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRow<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    is_sidechain: Option<bool>,
    #[serde(default)]
    is_api_error_message: bool,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_aborted_mid_stream: bool,
}
#[derive(Deserialize)]
struct Message<'a> {
    role: &'a str,
}
#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: Option<&'a str>,
    id: Option<&'a str>,
    tool_use_id: Option<&'a str>,
}

fn claude(line: &[u8]) -> Option<TranscriptChange> {
    let raw = std::str::from_utf8(line).ok()?;
    let row: ClaudeRow = object(raw).ok()?;
    let [message] = fields(raw, ["message"]).ok()?;
    if row.is_sidechain != Some(false) || row.is_meta {
        return Some(TranscriptChange::Keep);
    }
    if row.is_api_error_message || row.is_aborted_mid_stream {
        return None;
    }
    let Some(raw) = message.filter(|&value| value != "null") else {
        return Some(TranscriptChange::Keep);
    };
    let msg: Message = object(raw).ok()?;
    let [content] = fields(raw, ["content"]).ok()?;
    let content = content?;
    if msg.role != row.kind {
        return None;
    }
    if !content.starts_with('[') {
        return Some(TranscriptChange::Keep);
    }
    let blocks = array(content, content.len(), Ok).ok()?;
    let mut change = TranscriptChange::Keep;
    for raw in blocks {
        let b: Block = object(raw).ok()?;
        let [input] = fields(raw, ["input"]).ok()?;
        let next = match (msg.role, b.kind) {
            ("assistant", "tool_use") if b.name == Some("AskUserQuestion") => {
                request(b.id?, input?, true)?
            }
            ("user", "tool_result") => resolution(b.tool_use_id?)?,
            _ => continue,
        };
        if !matches!(change, TranscriptChange::Keep) {
            return None;
        }
        change = next;
    }
    Some(change)
}

#[derive(Deserialize)]
struct CodexRow<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}
#[derive(Deserialize)]
struct Response<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: Option<&'a str>,
    arguments: Option<String>,
    call_id: Option<&'a str>,
}

fn codex(line: &[u8]) -> Option<TranscriptChange> {
    let raw = std::str::from_utf8(line).ok()?;
    let row: CodexRow = object(raw).ok()?;
    let [payload] = fields(raw, ["payload"]).ok()?;
    if row.kind != "response_item" {
        return Some(TranscriptChange::Keep);
    }
    let r: Response = object(payload?).ok()?;
    match r.kind {
        "function_call" if r.name == Some("request_user_input") => {
            request(r.call_id?, &r.arguments?, false)
        }
        "function_call_output" => resolution(r.call_id?),
        _ => Some(TranscriptChange::Keep),
    }
}

fn resolution(id: &str) -> Option<TranscriptChange> {
    (!id.is_empty() && id.len() <= 128).then(|| TranscriptChange::Resolved(id.to_owned()))
}
