//! Fresh tool calls from the verified transcript parser. A malformed or ambiguous
//! request withdraws the previous one. Raw JSON slices retain duplicate-field detection
//! and leave unrelated tool arguments and results undecoded.
use super::{MAX_OPTIONS, MAX_QUESTION_BYTES, MAX_QUESTIONS, PendingQuestion, QuestionItem};
use serde::Deserialize;
use serde_json::value::RawValue;

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

fn fields<'a, T: Deserialize<'a>>(raw: &'a str) -> Option<T> {
    raw.trim_ascii_start()
        .starts_with('{')
        .then(|| serde_json::from_str(raw).ok())
        .flatten()
}

#[derive(Deserialize)]
struct ToolInput<'a> {
    #[serde(borrow)]
    questions: Vec<&'a RawValue>,
}
#[derive(Deserialize)]
struct Item<'a> {
    header: String,
    question: String,
    #[serde(borrow)]
    options: Vec<&'a RawValue>,
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
    let input: ToolInput = fields(raw)?;
    if input.questions.len() > MAX_QUESTIONS {
        return None;
    }
    let mut questions = Vec::with_capacity(input.questions.len());
    for raw in input.questions {
        let q: Item = fields(raw.get())?;
        if q.options.len() > MAX_OPTIONS || q.secret == Some(true) {
            return None;
        }
        let options = q
            .options
            .iter()
            .map(|raw| fields::<Choice>(raw.get()).map(|o| o.label))
            .collect::<Option<_>>()?;
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
    message: Option<&'a RawValue>,
}
#[derive(Deserialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a RawValue,
}
#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: Option<&'a str>,
    id: Option<&'a str>,
    tool_use_id: Option<&'a str>,
    input: Option<&'a RawValue>,
}

fn claude(line: &[u8]) -> Option<TranscriptChange> {
    let row: ClaudeRow = fields(std::str::from_utf8(line).ok()?)?;
    if row.is_sidechain != Some(false) || row.is_meta {
        return Some(TranscriptChange::Keep);
    }
    if row.is_api_error_message || row.is_aborted_mid_stream {
        return None;
    }
    let Some(raw) = row.message else {
        return Some(TranscriptChange::Keep);
    };
    let msg: Message = fields(raw.get())?;
    if msg.role != row.kind {
        return None;
    }
    if !msg.content.get().trim_ascii_start().starts_with('[') {
        return Some(TranscriptChange::Keep);
    }
    let blocks: Vec<&RawValue> = serde_json::from_str(msg.content.get()).ok()?;
    let mut change = TranscriptChange::Keep;
    for raw in blocks {
        let b: Block = fields(raw.get())?;
        let next = match (msg.role, b.kind) {
            ("assistant", "tool_use") if b.name == Some("AskUserQuestion") => {
                request(b.id?, b.input?.get(), true)?
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
    payload: Option<&'a RawValue>,
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
    let row: CodexRow = fields(std::str::from_utf8(line).ok()?)?;
    if row.kind != "response_item" {
        return Some(TranscriptChange::Keep);
    }
    let r: Response = fields(row.payload?.get())?;
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
