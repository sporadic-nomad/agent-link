//! Recorded agent sessions, `tests/fixtures/<agent>-<version>-<scene>.*`: `.bin` the raw
//! PTY stream, `.txt` the final screen, `.events.jsonl` the capture tool's event log, `.meta`
//! the size, agent, version and scene.
//!
//! - Agent, version, scene and size come from the `.meta`, never from the file name
//!   (cursor's version contains a `-`).
//! - A missing or empty fixture dir, or a file that cannot be read, fails the test: a run
//!   over nothing must not pass.
//! - Rows of the `pi-0.85.1-*` fixtures that contain `/home/u` are 10 columns shorter than pi
//!   drew them (the recorder shortened the home path inside the stream): compare their
//!   text, never their column positions or widths.

use std::fs;
use std::path::{Path, PathBuf};

use agent_link::PtySize;

/// The end of a DEC 2026 synchronized-output frame.
pub const FRAME_END: &[u8] = b"\x1b[?2026l";

/// A fixture's `.meta`.
pub struct Meta {
    pub rows: u16,
    pub cols: u16,
    pub agent: String,
    pub version: String,
    pub scene: String,
}

/// One recorded session.
pub struct Fixture {
    pub bin: PathBuf,
    pub meta: Meta,
    /// The raw PTY stream (`.bin`).
    pub stream: Vec<u8>,
    /// The final screen (`.txt`), as recorded (rows may keep trailing spaces).
    pub text: String,
    /// The event log (`.events.jsonl`).
    pub events: String,
}

impl Fixture {
    /// `agent version scene (file)` for messages.
    pub fn label(&self) -> String {
        format!(
            "{} {} {} ({})",
            self.meta.agent,
            self.meta.version,
            self.meta.scene,
            self.bin.display()
        )
    }

    /// The recorded terminal size.
    pub fn size(&self) -> PtySize {
        PtySize {
            rows: self.meta.rows,
            cols: self.meta.cols,
        }
    }

    /// The final screen with every row right-trimmed (all rows kept), the way a screen read
    /// returns it.
    pub fn screen(&self) -> String {
        self.text
            .split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The parsed event lines, in order.
    pub fn event_lines(&self) -> Vec<Vec<(String, Val)>> {
        self.events
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(parse_line)
            .collect()
    }

    /// The stream cut after each DEC 2026 frame end (`ESC [ ? 2026 l`) — every 256 bytes
    /// for a stream without frames — plus what follows the last cut: what a screen read
    /// after each redraw sees.
    pub fn frames(&self) -> Vec<&[u8]> {
        let bytes = &self.stream;
        let mut cuts: Vec<usize> = bytes
            .windows(FRAME_END.len())
            .enumerate()
            .filter(|(_, w)| *w == FRAME_END)
            .map(|(at, _)| at + FRAME_END.len())
            .collect();
        if cuts.is_empty() {
            cuts = (256..bytes.len()).step_by(256).collect();
        }
        cuts.push(bytes.len());
        let mut frames = Vec::new();
        let mut from = 0;
        for cut in cuts {
            if cut > from {
                frames.push(&bytes[from..cut]);
                from = cut;
            }
        }
        frames
    }

    /// The `t_ms` of every recorded `frame_end` event, in order (the n-th matches the n-th
    /// cut of [`frames`](Self::frames) when the stream has frames).
    pub fn frame_end_times(&self) -> Vec<u64> {
        self.event_lines()
            .iter()
            .filter(|f| str_field(f, "kind") == "frame_end")
            .map(|f| match field(f, "t_ms") {
                Some(Val::Num(n)) => u64::try_from(*n).expect("a non-negative t_ms"),
                other => panic!("frame_end without t_ms: {other:?}"),
            })
            .collect()
    }

    /// The bytes the capture script wrote to the agent (pastes, keys), in order; query
    /// replies are not included.
    pub fn script_writes(&self) -> Vec<Vec<u8>> {
        self.event_lines()
            .iter()
            .filter(|f| str_field(f, "kind") == "write" && str_field(f, "source") == "script")
            .map(|f| unescape_payload(str_field(f, "value")))
            .collect()
    }
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn read_meta(path: &Path) -> Meta {
    let meta = read_text(path);
    let get = |key: &str| -> String {
        meta.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("{} has no `{key}=` line", path.display()))
            .trim()
            .to_string()
    };
    let size = |key: &str| -> u16 {
        let value = get(key);
        match value.parse() {
            Ok(n) if n > 0 => n,
            _ => panic!("{}: `{key}={value}` is not a positive size", path.display()),
        }
    };
    let meta = Meta {
        rows: size("rows"),
        cols: size("cols"),
        agent: get("agent"),
        version: get("version"),
        scene: get("scene"),
    };
    for (key, value) in [
        ("agent", &meta.agent),
        ("version", &meta.version),
        ("scene", &meta.scene),
    ] {
        assert!(!value.is_empty(), "{}: empty `{key}=`", path.display());
    }
    meta
}

/// Every `.bin` with its `.meta`, `.txt` and `.events.jsonl`, sorted by file name.
pub fn all() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let entries =
        fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut bins: Vec<PathBuf> = entries
        .map(|e| {
            e.unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
                .path()
        })
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    assert!(!bins.is_empty(), "no .bin fixtures in {}", dir.display());
    bins.sort();
    bins.into_iter()
        .map(|bin| {
            let stream =
                fs::read(&bin).unwrap_or_else(|e| panic!("cannot read {}: {e}", bin.display()));
            Fixture {
                meta: read_meta(&bin.with_extension("meta")),
                text: read_text(&bin.with_extension("txt")),
                events: read_text(&bin.with_extension("events.jsonl")),
                stream,
                bin,
            }
        })
        .collect()
}

/// The one fixture whose `.meta` names `agent` and `scene`.
pub fn find(agent: &str, scene: &str) -> Fixture {
    let mut hits: Vec<Fixture> = all()
        .into_iter()
        .filter(|f| f.meta.agent == agent && f.meta.scene == scene)
        .collect();
    assert_eq!(hits.len(), 1, "{agent}/{scene}: {} fixtures", hits.len());
    hits.remove(0)
}

// --- the capture tool's event log -------------------------------------------------------

/// A value in an event line (the log has no nested values).
#[derive(Debug, PartialEq)]
pub enum Val {
    Str(String),
    Num(i64),
    Bool(bool),
    Null,
}

/// One flat JSON object, as the capture tool writes it.
pub fn parse_line(line: &str) -> Vec<(String, Val)> {
    let mut chars = line.trim().chars().peekable();
    let fail = |what: &str| -> ! { panic!("bad event line ({what}): {line}") };
    let string = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| -> String {
        if chars.next() != Some('"') {
            fail("expected a string");
        }
        let mut out = String::new();
        loop {
            match chars.next() {
                Some('"') => return out,
                Some('\\') => match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        let code = u32::from_str_radix(&hex, 16).unwrap_or_else(|_| fail("\\u"));
                        out.push(char::from_u32(code).unwrap_or_else(|| fail("\\u code")));
                    }
                    _ => fail("escape"),
                },
                Some(c) => out.push(c),
                None => fail("unterminated string"),
            }
        }
    };
    if chars.next() != Some('{') {
        fail("expected an object");
    }
    let mut fields = Vec::new();
    loop {
        let key = string(&mut chars);
        if chars.next() != Some(':') {
            fail("expected ':'");
        }
        let value = match chars.peek() {
            Some('"') => Val::Str(string(&mut chars)),
            _ => {
                let mut raw = String::new();
                while let Some(c) = chars.peek().filter(|c| !matches!(c, ',' | '}')) {
                    raw.push(*c);
                    chars.next();
                }
                match raw.as_str() {
                    "true" => Val::Bool(true),
                    "false" => Val::Bool(false),
                    "null" => Val::Null,
                    n => Val::Num(n.parse().unwrap_or_else(|_| fail("number"))),
                }
            }
        };
        fields.push((key, value));
        match chars.next() {
            Some(',') => {}
            Some('}') => return fields,
            _ => fail("expected ',' or '}'"),
        }
    }
}

/// The capture tool's payload notation (`\e`, `\;`, `\xHH`, `\u{…}`, …) back to bytes.
pub fn unescape_payload(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('e') => out.push(0x1b),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some(c @ ('\\' | ';' | '{' | '}')) => out.push(c as u8),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).expect("\\x escape"));
            }
            other => panic!("unsupported payload escape \\{other:?} in {s:?}"),
        }
    }
    out
}

/// A field's value.
pub fn field<'a>(fields: &'a [(String, Val)], key: &str) -> Option<&'a Val> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A string field's value; panics when it is missing or not a string.
pub fn str_field<'a>(fields: &'a [(String, Val)], key: &str) -> &'a str {
    match field(fields, key) {
        Some(Val::Str(s)) => s,
        other => panic!("event field {key} is {other:?}: {fields:?}"),
    }
}
