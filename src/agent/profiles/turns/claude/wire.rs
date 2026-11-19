//! Decode the fields used for attribution and completion. Serde rejects duplicate
//! declared fields. Unrecognized payloads (attachments, tools, reasoning) are skipped.

use std::borrow::Cow;

use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::agent::turns::{Gap, Reply};

pub(super) const MAX_BLOCKS: usize = 256;

pub(super) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Gap> {
    object(std::str::from_utf8(bytes).map_err(|_| Gap::Format)?)
}

pub(super) fn object<'a, T: Deserialize<'a>>(json: &'a str) -> Result<T, Gap> {
    // Serde structs also accept positional arrays. The recorded protocol uses
    // named fields for attribution, messages and blocks.
    if !json.trim_ascii_start().starts_with('{') {
        return Err(Gap::Format);
    }
    serde_json::from_str(json).map_err(|_| Gap::Format)
}

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

#[derive(Deserialize)]
pub(super) struct Block<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    #[serde(skip)]
    text: Option<&'a str>,
}

impl Block<'_> {
    pub fn text(&self) -> Result<Text<'_>, Gap> {
        Text::new(self.text.ok_or(Gap::Format)?)
    }
}

/// A borrowed JSON string. Decoding a heavily escaped line into a String first
/// would allocate in proportion to the line even when only one reply byte fits.
pub(super) struct Text<'a>(&'a str);

impl<'a> Text<'a> {
    pub fn new(raw: &'a str) -> Result<Self, Gap> {
        raw.strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(Self)
            .ok_or(Gap::Format)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn validate(&self) -> Result<(), Gap> {
        self.visit(|_| {})
    }

    pub fn append_to(&self, reply: &mut Reply, limit: usize) -> Result<(), Gap> {
        if !self.0.contains('\\') {
            reply.push(self.0, limit);
            return Ok(());
        }
        if reply.truncated {
            return self.validate();
        }
        // Queue capacity is charged by decoded bytes. A Unicode escape can use
        // six bytes on disk for a single retained byte.
        let mut decoded = 0;
        self.visit(|part| decoded += part.len())?;
        let available = limit.saturating_sub(reply.text.len());
        reply.text.reserve_exact(decoded.min(available));
        self.visit(|part| reply.push(part, limit))?;
        if reply.truncated {
            // A multibyte scalar at the boundary can leave unused reserved bytes.
            reply.text.shrink_to_fit();
        }
        Ok(())
    }

    fn visit(&self, mut emit: impl FnMut(&str)) -> Result<(), Gap> {
        let mut remaining = self.0;
        while let Some((plain, escaped)) = remaining.split_once('\\') {
            emit(plain);
            let (code, tail) = escaped.split_at_checked(1).ok_or(Gap::Format)?;
            let (value, tail) = match code {
                "\"" => ('"', tail),
                "\\" => ('\\', tail),
                "/" => ('/', tail),
                "b" => ('\x08', tail),
                "f" => ('\x0c', tail),
                "n" => ('\n', tail),
                "r" => ('\r', tail),
                "t" => ('\t', tail),
                "u" => unicode_escape(tail)?,
                _ => return Err(Gap::Format),
            };
            emit(value.encode_utf8(&mut [0; 4]));
            remaining = tail;
        }
        emit(remaining);
        Ok(())
    }
}

fn unicode_escape(digits: &str) -> Result<(char, &str), Gap> {
    fn unit(input: &str) -> Result<(u16, &str), Gap> {
        let (hex, rest) = input.split_at_checked(4).ok_or(Gap::Format)?;
        let unit = u16::from_str_radix(hex, 16).map_err(|_| Gap::Format)?;
        Ok((unit, rest))
    }
    let (first, rest) = unit(digits)?;
    if (0xd800..=0xdbff).contains(&first) {
        let (second, rest) = unit(rest.strip_prefix("\\u").ok_or(Gap::Format)?)?;
        let scalar = char::decode_utf16([first, second])
            .next()
            .and_then(Result::ok)
            .ok_or(Gap::Format)?;
        return Ok((scalar, rest));
    }
    Ok((char::from_u32(u32::from(first)).ok_or(Gap::Format)?, rest))
}

// Borrow the source range consumed by Serde. IgnoredAny skips a complete JSON
// value without building its tree or decoding its strings. Enabling serde_json's
// raw_value feature would also change how other users deserialize literal map keys.
fn take<'a, T: Deserialize<'a>>(input: &mut &'a str) -> Result<(T, &'a str), Gap> {
    let start = input.trim_ascii_start();
    let mut values = serde_json::Deserializer::from_str(start).into_iter::<T>();
    let value = values.next().ok_or(Gap::Format)?.map_err(|_| Gap::Format)?;
    let (raw, rest) = start.split_at(values.byte_offset());
    *input = rest;
    Ok((value, raw))
}

/// The enclosing JSON was already validated by Serde. Visit the comma-separated
/// members and let the caller consume each key/value pair or array element.
fn members<'a>(
    raw: &'a str,
    open: char,
    close: char,
    mut member: impl FnMut(&mut &'a str) -> Result<(), Gap>,
) -> Result<(), Gap> {
    let mut rest = raw
        .trim_ascii()
        .strip_prefix(open)
        .and_then(|r| r.strip_suffix(close))
        .ok_or(Gap::Format)?;
    if rest.trim_ascii().is_empty() {
        return Ok(());
    }
    loop {
        member(&mut rest)?;
        rest = rest.trim_ascii_start();
        if rest.is_empty() {
            return Ok(());
        }
        rest = rest.strip_prefix(',').ok_or(Gap::Format)?;
    }
}

/// Select only the named raw fields. Match escaped field names just as Serde's
/// derived structs do and reject duplicates, including duplicate null values.
fn fields<'a, const N: usize>(raw: &'a str, names: [&str; N]) -> Result<[Option<&'a str>; N], Gap> {
    #[derive(Deserialize)]
    struct Key<'a>(#[serde(borrow)] Cow<'a, str>);

    let mut found = [None; N];
    members(raw, '{', '}', |rest| {
        let (key, _) = take::<Key>(rest)?;
        *rest = rest
            .trim_ascii_start()
            .strip_prefix(':')
            .ok_or(Gap::Format)?;
        let (_, value) = take::<IgnoredAny>(rest)?;
        if let Some(i) = names.iter().position(|&name| name == key.0) {
            if found[i].replace(value).is_some() {
                return Err(Gap::Format);
            }
        }
        Ok(())
    })?;
    Ok(found)
}

/// Keep at most MAX_BLOCKS small descriptors borrowing the already bounded line.
pub(super) fn blocks(input: &str) -> Result<Vec<Block<'_>>, Gap> {
    let mut blocks = Vec::new();
    members(input, '[', ']', |rest| {
        if blocks.len() == MAX_BLOCKS {
            return Err(Gap::Oversized);
        }
        let (_, raw) = take::<IgnoredAny>(rest)?;
        let mut block: Block = object(raw)?;
        block.text = fields(raw, ["text"])?[0];
        blocks.push(block);
        Ok(())
    })?;
    Ok(blocks)
}
