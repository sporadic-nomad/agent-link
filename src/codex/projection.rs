//! Conservative, stateless projection of the pinned Codex v2 notification subset.
use serde_json::Value;

use crate::SessionId;
use crate::portable::*;

fn string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(Into::into)
}

pub(super) fn context(generation: SessionId, seq: u64, value: &Value) -> EventContext {
    let params = &value["params"];
    let method = value["method"].as_str().unwrap_or_default();
    let request_id = value
        .get("id")
        .or_else(|| (method == "serverRequest/resolved").then(|| &params["requestId"]));
    EventContext {
        provider: Provider::Codex,
        generation,
        seq,
        // Read the identity defined by this method's schema. Additive fields must
        // not override a nested authoritative identity or fill in a missing one.
        session_id: if method == "thread/started" {
            string(&params["thread"], "id")
        } else {
            string(params, "threadId")
        },
        turn_id: if matches!(method, "turn/started" | "turn/completed") {
            string(&params["turn"], "id")
        } else {
            string(params, "turnId")
        },
        item_id: if matches!(method, "item/started" | "item/completed") {
            string(&params["item"], "id")
        } else {
            string(params, "itemId")
        },
        request_id: request_id.and_then(|id| {
            id.as_str()
                .map(|id| RequestId::String(id.into()))
                .or_else(|| id.as_i64().map(RequestId::Number))
        }),
    }
}

pub(super) fn project(generation: SessionId, seq: u64, value: &Value) -> EventV1 {
    let context = context(generation, seq, value);
    let method = value["method"].as_str().unwrap_or_default();
    // An unsupported reverse request with a notification-like method must not
    // project a notification (especially a terminal, usage or resolution event).
    let kind = if value.get("id").is_some() {
        matches!(
            method,
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
        )
        .then_some(EventKind::ApprovalRequested)
    } else {
        kind(method, &value["params"], &context)
    }
    .unwrap_or(EventKind::Other);
    EventV1 {
        context,
        native_method: method.into(),
        kind,
    }
}

fn kind(method: &str, params: &Value, context: &EventContext) -> Option<EventKind> {
    context.session_id.as_ref()?;
    if method == "thread/started" {
        return Some(EventKind::SessionStarted);
    }
    if method == "serverRequest/resolved" {
        context.request_id.as_ref()?;
        return Some(EventKind::ApprovalResolved);
    }
    context.turn_id.as_ref()?;
    match method {
        "turn/started" if params["turn"]["status"] == "inProgress" => Some(EventKind::TurnStarted),
        "turn/completed" => Some(EventKind::TurnCompleted {
            status: status(params["turn"]["status"].as_str()?),
        }),
        "thread/tokenUsage/updated" => Some(EventKind::Usage(usage(&params["tokenUsage"])?)),
        "item/agentMessage/delta" => {
            context.item_id.as_ref()?;
            Some(EventKind::Text {
                text: params["delta"].as_str()?.into(),
                update: TextUpdate::Delta,
                phase: TextPhase::Unknown,
            })
        }
        "item/started" | "item/completed" => {
            context.item_id.as_ref()?;
            let item = &params["item"];
            match item["type"].as_str()? {
                "agentMessage" => Some(EventKind::Text {
                    text: item["text"].as_str()?.into(),
                    update: TextUpdate::Replace,
                    phase: match item["phase"].as_str() {
                        Some("commentary") => TextPhase::Commentary,
                        Some("final_answer") => TextPhase::Final,
                        _ => TextPhase::Unknown,
                    },
                }),
                "commandExecution" | "fileChange" => Some(EventKind::Tool {
                    tool: if item["type"] == "commandExecution" {
                        ToolKind::Command
                    } else {
                        ToolKind::FileChange
                    },
                    state: if method == "item/started" {
                        ItemState::Started
                    } else {
                        ItemState::Completed
                    },
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn status(status: &str) -> CompletionStatus {
    match status {
        "completed" => CompletionStatus::Completed,
        "interrupted" => CompletionStatus::Interrupted,
        "failed" => CompletionStatus::Failed,
        _ => CompletionStatus::Unknown,
    }
}

fn optional_count(value: &Value) -> Option<Option<u64>> {
    if value.is_null() {
        Some(None)
    } else {
        value.as_u64().map(Some)
    }
}

fn counts(value: &Value) -> Option<TokenUsage> {
    Some(TokenUsage {
        total_tokens: value["totalTokens"].as_u64()?,
        input_tokens: value["inputTokens"].as_u64()?,
        output_tokens: value["outputTokens"].as_u64()?,
        cached_input_tokens: optional_count(&value["cachedInputTokens"])?,
        cache_write_input_tokens: optional_count(&value["cacheWriteInputTokens"])?,
        reasoning_output_tokens: optional_count(&value["reasoningOutputTokens"])?,
    })
}

fn usage(value: &Value) -> Option<Usage> {
    Some(Usage {
        total: counts(&value["total"])?,
        last: counts(&value["last"])?,
        model_context_window: optional_count(&value["modelContextWindow"])?,
    })
}
