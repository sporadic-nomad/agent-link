//! Pure helpers of the capture tool: script and payload parsing, escaping for event lines,
//! and fixture names. No IO here.

use std::time::Duration;

/// Start marker of a bracketed paste.
pub const PASTE_START: &[u8] = b"\x1b[200~";
/// End marker of a bracketed paste.
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// One action of a script entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Bytes to write to the child as one write.
    Write(Vec<u8>),
    /// A cursor key (`b'A'` up, `b'B'` down); its bytes depend on the terminal's cursor-key
    /// mode at the time it is sent.
    Arrow(u8),
    /// Block the script until the screen contains this text.
    WaitText(String),
}

/// One `<ms>:<payload>` entry of `--script`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Offset from spawn.
    pub at: Duration,
    /// What to do at that offset, in order.
    pub steps: Vec<Step>,
}

/// Parses `"<ms>:<payload>;<ms>:<payload>…"`. A `;` inside `{…}` or written as `\;` belongs
/// to the payload. Empty entries (a trailing `;`) are ignored. Offsets must not decrease:
/// entries run in order, so a smaller offset after a larger one would be a silent no-wait.
pub fn parse_script(src: &str) -> Result<Vec<Entry>, String> {
    let entries: Vec<Entry> = split_entries(src)
        .into_iter()
        .filter(|piece| !piece.trim().is_empty())
        .map(|piece| {
            let (ms, payload) = piece
                .split_once(':')
                .ok_or_else(|| format!("script entry {piece:?} has no `<ms>:` prefix"))?;
            let ms: u64 = ms
                .trim()
                .parse()
                .ok()
                .filter(|&n| n <= crate::args::MAX_MS)
                .ok_or_else(|| {
                    format!(
                        "script entry {piece:?}: {ms:?} is not a millisecond offset up to {}",
                        crate::args::MAX_MS
                    )
                })?;
            let steps =
                parse_payload(payload).map_err(|e| format!("script entry {piece:?}: {e}"))?;
            Ok(Entry {
                at: Duration::from_millis(ms),
                steps,
            })
        })
        .collect::<Result<_, String>>()?;
    if let Some(w) = entries.windows(2).find(|w| w[1].at < w[0].at) {
        return Err(format!(
            "script offsets must not decrease: {} ms comes after {} ms",
            w[1].at.as_millis(),
            w[0].at.as_millis()
        ));
    }
    Ok(entries)
}

fn split_entries(src: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ';' if depth == 0 => pieces.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    pieces.push(current);
    pieces
}

/// Parses one payload: text with escapes (`\r \n \t \e \xHH \u{HHHH} \\ \; \{ \}`) and the
/// tokens `{enter}` `{up}` `{down}` `{esc}` `{ctrl-c}` `{paste:<text>}` `{wait-text:<needle>}`.
/// Adjacent bytes are merged into one write; arrows and waits split writes.
pub fn parse_payload(src: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let mut buf = Vec::new();
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => unescape_one(&mut chars, &mut buf)?,
            '{' => {
                let token = read_token(&mut chars)?;
                let (name, arg) = match token.split_once(':') {
                    Some((name, arg)) => (name, Some(arg)),
                    None => (token.as_str(), None),
                };
                match (name, arg) {
                    ("enter", None) => buf.push(b'\r'),
                    ("esc", None) => buf.push(0x1b),
                    ("ctrl-c", None) => buf.push(0x03),
                    ("up" | "down", None) => {
                        flush(&mut buf, &mut steps);
                        steps.push(Step::Arrow(if name == "up" { b'A' } else { b'B' }));
                    }
                    ("paste", Some(text)) => {
                        buf.extend_from_slice(PASTE_START);
                        buf.extend_from_slice(&unescape(text)?);
                        buf.extend_from_slice(PASTE_END);
                    }
                    ("wait-text", Some(needle)) => {
                        let needle = String::from_utf8(unescape(needle)?)
                            .map_err(|_| "wait-text needle is not UTF-8".to_string())?;
                        if needle.is_empty() {
                            return Err("wait-text needs a non-empty needle".into());
                        }
                        flush(&mut buf, &mut steps);
                        steps.push(Step::WaitText(needle));
                    }
                    _ => return Err(format!("unknown token {{{token}}}")),
                }
            }
            _ => push_char(&mut buf, c),
        }
    }
    flush(&mut buf, &mut steps);
    Ok(steps)
}

fn flush(buf: &mut Vec<u8>, steps: &mut Vec<Step>) {
    if !buf.is_empty() {
        steps.push(Step::Write(std::mem::take(buf)));
    }
}

/// Reads a token body up to its closing `}` (inner braces, as in `\u{2713}`, nest); backslash
/// escapes are kept as written so the token's argument is unescaped once, by the token itself.
fn read_token(chars: &mut std::str::Chars<'_>) -> Result<String, String> {
    let mut token = String::new();
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '}' if depth == 0 => return Ok(token),
            '}' => {
                depth -= 1;
                token.push(c);
            }
            '{' => {
                depth += 1;
                token.push(c);
            }
            '\\' => {
                token.push(c);
                token.push(chars.next().ok_or("dangling `\\` in a token")?);
            }
            _ => token.push(c),
        }
    }
    Err(format!("unterminated token {{{token}"))
}

/// Unescapes text (no tokens) into bytes.
pub fn unescape(src: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            unescape_one(&mut chars, &mut buf)?;
        } else {
            push_char(&mut buf, c);
        }
    }
    Ok(buf)
}

fn unescape_one(chars: &mut std::str::Chars<'_>, buf: &mut Vec<u8>) -> Result<(), String> {
    match chars.next() {
        Some('r') => buf.push(b'\r'),
        Some('n') => buf.push(b'\n'),
        Some('t') => buf.push(b'\t'),
        Some('e') => buf.push(0x1b),
        Some(c @ ('\\' | ';' | '{' | '}')) => push_char(buf, c),
        Some('x') => {
            let hex: String = chars.by_ref().take(2).collect();
            // `from_str_radix` alone would also take a sign (`\x+1`).
            let byte = Some(&hex)
                .filter(|h| h.len() == 2 && h.chars().all(|c| c.is_ascii_hexdigit()))
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("`\\x{hex}` needs exactly two hex digits"))?;
            buf.push(byte);
        }
        Some('u') => {
            if chars.next() != Some('{') {
                return Err("`\\u` must be written `\\u{HHHH}`".into());
            }
            let mut hex = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(c) => hex.push(c),
                    None => return Err(format!("`\\u{{{hex}` has no closing `}}`")),
                }
            }
            let c = Some(&hex)
                .filter(|h| (1..=6).contains(&h.len()) && h.chars().all(|c| c.is_ascii_hexdigit()))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .and_then(char::from_u32)
                .ok_or_else(|| format!("`\\u{{{hex}}}` is not a Unicode scalar value"))?;
            push_char(buf, c);
        }
        Some(c) => return Err(format!("unknown escape `\\{c}`")),
        None => return Err("dangling `\\` at the end".into()),
    }
    Ok(())
}

fn push_char(buf: &mut Vec<u8>, c: char) {
    let mut tmp = [0u8; 4];
    buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
}

/// Renders bytes in the payload notation, so a logged write parses back to the same bytes
/// as a `--script` payload: printable text as is, `\r \n \t \e \\ \; \{ \}`, other C0
/// controls, DEL and invalid UTF-8 as `\xHH`, C1 controls (U+0080–U+009F) as `\u{HH}`.
pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\r' => out.push_str("\\r"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\u{1b}' => out.push_str("\\e"),
                '\\' | ';' | '{' | '}' => {
                    out.push('\\');
                    out.push(c);
                }
                c if (c as u32) < 0x20 || c == '\u{7f}' => {
                    out.push_str(&format!("\\x{:02x}", c as u32));
                }
                c if is_c1(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// A JSON string literal (with the quotes) for `s`.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || is_c1(c) => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A C1 control (U+0080–U+009F): a terminal may act on it, so it is never written raw.
fn is_c1(c: char) -> bool {
    ('\u{80}'..='\u{9f}').contains(&c)
}

/// `<agent>-<version>-<scene>` from a fixture prefix's file name: the agent is the part before
/// the first `-`, the version the next part (it starts with a digit and has no `-`), the scene
/// everything after it (it may contain `-`). `None` when the name does not have that shape.
pub fn fixture_parts(name: &str) -> Option<(String, String, String)> {
    let (agent, rest) = name.split_once('-')?;
    let (version, scene) = rest.split_once('-')?;
    let fits = !agent.is_empty()
        && version.starts_with(|c: char| c.is_ascii_digit())
        && !scene.is_empty()
        && !scene.starts_with('-')
        && !scene.ends_with('-');
    fits.then(|| (agent.into(), version.into(), scene.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_escapes_and_tokens() {
        let steps = parse_payload(r"a\tb\e[1m\x03\u{2713}{enter}{esc}{ctrl-c}").unwrap();
        assert_eq!(
            steps,
            vec![Step::Write(
                b"a\tb\x1b[1m\x03\xe2\x9c\x93\r\x1b\x03".to_vec()
            )]
        );
        assert_eq!(
            parse_payload(r"{paste:\u{41}\}}").unwrap(),
            vec![Step::Write(b"\x1b[200~A}\x1b[201~".to_vec())]
        );
        let steps = parse_payload("{paste:x\\;y}{down}{wait-text:Ready}\\r").unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Write(b"\x1b[200~x;y\x1b[201~".to_vec()),
                Step::Arrow(b'B'),
                Step::WaitText("Ready".into()),
                Step::Write(b"\r".to_vec()),
            ]
        );
        assert!(parse_payload("{nope}").is_err());
        assert!(parse_payload(r"\q").is_err());
        assert!(parse_payload(r"\x4").is_err());
        assert!(parse_payload("{paste:abc").is_err());
    }

    #[test]
    fn hex_escapes_are_strict() {
        assert_eq!(parse_payload(r"\x0A").unwrap(), [Step::Write(vec![0x0a])]);
        assert_eq!(
            parse_payload(r"\u{1F600}").unwrap(),
            [Step::Write("\u{1f600}".into())]
        );
        for bad in [
            r"\x+1",
            r"\x-1",
            r"\x 1",
            r"\xg0",
            r"\u{41",
            r"\u{",
            r"\u{}",
            r"\u{+41}",
            r"\u{1234567}",
            r"\u{d800}",
            r"\u41",
        ] {
            assert!(unescape(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn c1_controls_are_escaped() {
        let bytes = "a\u{80}b\u{9b}c\u{9f}\u{a0}".as_bytes();
        let escaped = escape_bytes(bytes);
        assert_eq!(escaped, "a\\u{80}b\\u{9b}c\\u{9f}\u{a0}");
        assert_eq!(unescape(&escaped).unwrap(), bytes);
        assert_eq!(json_str("\u{85}x\u{9b}\u{a0}"), "\"\\u0085x\\u009b\u{a0}\"");
    }

    #[test]
    fn script_splits_outside_braces() {
        let entries = parse_script("300:{paste:a;b}; 800:x\\;y;").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].at, Duration::from_millis(300));
        assert_eq!(
            entries[0].steps,
            vec![Step::Write(b"\x1b[200~a;b\x1b[201~".to_vec())]
        );
        assert_eq!(entries[1].steps, vec![Step::Write(b"x;y".to_vec())]);
        assert!(parse_script("abc").is_err());
        assert!(parse_script("x:abc").is_err());
        assert_eq!(parse_script("100:a;100:b;200:c").unwrap().len(), 3);
        assert!(parse_script("500:a;200:b").is_err());
    }

    #[test]
    fn escape_and_json() {
        assert_eq!(
            escape_bytes(b"\x1b[200~a\\b\r\n\x07\xff\xe2\x9c\x93"),
            "\\e[200~a\\\\b\\r\\n\\x07\\xff\u{2713}"
        );
        assert_eq!(escape_bytes(b"a;{b}"), r"a\;\{b\}");
        assert_eq!(json_str("a\"b\\c\n\u{1b}"), "\"a\\\"b\\\\c\\n\\u001b\"");
        let escaped = escape_bytes(b"x\x1by\\");
        assert_eq!(unescape(&escaped).unwrap(), b"x\x1by\\");
    }

    #[test]
    fn logged_write_round_trips_through_script() {
        let bytes: &[u8] = b"\x1b[200~a;b {paste:x} \\ \xe2\x9c\x93\x03\xff\x1b[201~\r";
        let entries = parse_script(&format!("0:{};5:z", escape_bytes(bytes))).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].steps, vec![Step::Write(bytes.to_vec())]);
        assert_eq!(entries[1].steps, vec![Step::Write(b"z".to_vec())]);
    }

    #[test]
    fn fixture_name_parts() {
        let parts = |a: &str, v: &str, s: &str| Some((a.into(), v.into(), s.into()));
        assert_eq!(
            fixture_parts("claude-2.1.282-trust"),
            parts("claude", "2.1.282", "trust")
        );
        assert_eq!(
            fixture_parts("claude-2.1.282-bypass-warning"),
            parts("claude", "2.1.282", "bypass-warning")
        );
        assert_eq!(
            fixture_parts("codex-0.46.0-swallowed-enter"),
            parts("codex", "0.46.0", "swallowed-enter")
        );
        for bad in [
            "cap-smoke",
            "claude-latest-trust",
            "claude-2.1.282",
            "claude-2.1.282-",
            "claude-2.1.282--x",
            "-2.1-trust",
            "claude",
        ] {
            assert_eq!(fixture_parts(bad), None, "{bad}");
        }
    }
}
