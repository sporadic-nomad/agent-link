//! Claude's writer, header and turn record fields.
use super::super::json::fields;
pub(super) use super::super::json::{MAX_BLOCKS, Text, blocks, decode, object};
use crate::agent::turns::Gap;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WriterRecord<'a> {
    pub pid: u32,
    pub started_at: u64,
    pub session_id: &'a str,
    #[serde(borrow)]
    pub cwd: Cow<'a, str>,
    pub kind: &'a str,
    pub entrypoint: &'a str,
    pub pid_domain: &'a str,
    pub version: &'a str,
}

#[derive(Deserialize)]
pub(super) struct Header<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub mode: &'a str,
    #[serde(rename = "sessionId")]
    pub session: &'a str,
}

#[derive(Deserialize)]
pub(super) struct Tag<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    #[serde(rename = "isSidechain")]
    pub sidechain: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Row<'a> {
    pub session_id: &'a str,
    // This spelling is distinct in the captured assistant records after /clear.
    #[serde(rename = "session_id")]
    pub launch_id: Option<&'a str>,
    #[serde(borrow)]
    pub cwd: Cow<'a, str>,
    pub entrypoint: &'a str,
    pub version: &'a str,
    #[serde(skip)]
    message: Option<&'a str>,
    #[serde(default)]
    pub is_meta: bool,
    #[serde(default)]
    pub is_aborted_mid_stream: bool,
    #[serde(default)]
    pub is_api_error_message: bool,
    pub interrupted_message_id: Option<&'a str>,
    pub api_block_index: Option<u64>,
    #[serde(skip)]
    pub origin: Option<&'a str>,
    pub turn_origin: Option<&'a str>,
    pub prompt_source: Option<&'a str>,
    pub subtype: Option<&'a str>,
    pub duration_ms: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct Origin<'a> {
    pub kind: &'a str,
}

impl<'a> Row<'a> {
    pub fn parse(json: &'a str) -> Result<Self, Gap> {
        let mut row: Self = object(json)?;
        let [message, origin] = fields(json, ["message", "origin"])?;
        row.message = message.filter(|&raw| raw != "null");
        row.origin = origin.filter(|&raw| raw != "null");
        Ok(row)
    }

    pub fn message<T: Deserialize<'a>>(&self) -> Result<(T, &'a str), Gap> {
        let raw = self.message.ok_or(Gap::Format)?;
        let message = object(raw)?;
        let [content] = fields(raw, ["content"])?;
        Ok((message, content.ok_or(Gap::Format)?))
    }
}

#[derive(Deserialize)]
pub(super) struct User<'a> {
    pub role: &'a str,
}

#[derive(Deserialize)]
pub(super) struct Assistant<'a> {
    pub id: &'a str,
    pub role: &'a str,
    // The key is required. A recorded null is an unconfirmed stop.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub stop_reason: Option<&'a str>,
}
