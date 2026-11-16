//! Replays every recorded agent stream (`tests/fixtures/*.bin`) through the screen model
//! and checks it against what the capture tool recorded live:
//! - the final screen equals the fixture's `.txt` (every line right-trimmed and trailing
//!   empty lines dropped on both sides — 20 of the `.txt` files keep trailing spaces);
//! - the tap reports the titles, progress reports, prompt marks and frame edges of the
//!   fixture's `.events.jsonl`, in order (timestamps ignored);
//! - the query replies equal the replies the capture tool wrote to the agent.
//!
//! Each stream is fed in 8 KiB chunks (a PTY read) and in 7-byte chunks (escape sequences
//! split everywhere). Size, agent, version and scene come from the fixture's `.meta`,
//! never from the file name. A missing or empty fixture dir, or a fixture file that cannot
//! be read, fails the test: a run over nothing must not pass.
//!
//! Rows of the `pi-0.85.1-*` fixtures that contain `/home/u` are 10 columns shorter than pi
//! drew them (the recorder shortened the home path inside the stream): compare their text,
//! never their column positions or widths.
#![cfg(feature = "screen")]

mod common;

use agent_link::ProgressState;
use agent_link::screen::{PromptMark, ScreenModel, TapSignal, Vt100Model};
use common::fixtures::{self, Fixture, Val, field, parse_line, str_field, unescape_payload};

const CHUNKINGS: [usize; 2] = [8192, 7];
const SCROLLBACK: usize = 200;

/// Lines right-trimmed, trailing empty lines dropped.
fn normalized(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').map(str::trim_end).collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

// --- the capture tool's event log -------------------------------------------------------

/// The capture tool's `progress` event as the tap reports it; `None` where the tap ignores
/// the report (missing or unknown state).
fn progress(fields: &[(String, Val)]) -> Option<ProgressState> {
    let pct = match field(fields, "pct") {
        Some(Val::Num(n)) => Some(u8::try_from((*n).clamp(0, 100)).expect("clamped")),
        _ => None,
    };
    Some(match field(fields, "state") {
        Some(Val::Num(0)) => ProgressState::Clear,
        Some(Val::Num(1)) => ProgressState::Normal(pct),
        Some(Val::Num(2)) => ProgressState::Error(pct),
        Some(Val::Num(3)) => ProgressState::Indeterminate,
        Some(Val::Num(4)) => ProgressState::Paused(pct),
        _ => return None,
    })
}

/// The signals and query replies the capture tool recorded, in order.
fn recorded(events: &str) -> (Vec<TapSignal>, Vec<Vec<u8>>) {
    let mut signals = Vec::new();
    let mut replies = Vec::new();
    for line in events.lines().filter(|l| !l.trim().is_empty()) {
        let fields = parse_line(line);
        match str_field(&fields, "kind") {
            "title" => signals.push(TapSignal::Title(str_field(&fields, "value").into())),
            "progress" => signals.extend(progress(&fields).map(TapSignal::Progress)),
            "osc133" => {
                let mark = match str_field(&fields, "mark") {
                    "A" => Some(PromptMark::A),
                    "B" => Some(PromptMark::B),
                    "C" => Some(PromptMark::C),
                    "D" => Some(PromptMark::D),
                    _ => None,
                };
                signals.extend(mark.map(TapSignal::Prompt));
            }
            "frame_begin" => signals.push(TapSignal::FrameBegin),
            "frame_end" => signals.push(TapSignal::FrameEnd),
            "screen_reset" => panic!("a recorded parser panic: {line}"),
            "write" if str_field(&fields, "source") == "reply" => {
                replies.push(unescape_payload(str_field(&fields, "value")));
            }
            _ => {}
        }
    }
    (signals, replies)
}

// --- the replay -------------------------------------------------------------------------

/// What a replay produced: the final screen text, the signals and the query replies.
struct Replayed {
    text: String,
    signals: Vec<TapSignal>,
    replies: Vec<Vec<u8>>,
}

/// Feeds the stream in `chunk`-byte pieces. Fails on a reset or a generation that does not
/// count the chunks.
fn replay(f: &Fixture, chunk: usize) -> Result<Replayed, String> {
    let size = f.size();
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut model = Vt100Model::new(size, SCROLLBACK, Some(tx));
    let mut signals = Vec::new();
    let mut chunks = 0u64;
    for piece in f.stream.chunks(chunk) {
        signals.extend(model.process(piece));
        chunks += 1;
    }
    if let Some(reset) = signals.iter().find(|s| matches!(s, TapSignal::Reset(_))) {
        return Err(format!("{reset:?}"));
    }
    let snapshot = model.snapshot();
    if snapshot.generation != chunks || snapshot.size != size {
        return Err(format!(
            "generation {} after {chunks} chunks, size {:?}",
            snapshot.generation, snapshot.size
        ));
    }
    if snapshot.rows.len() != usize::from(size.rows) {
        return Err(format!("{} rows", snapshot.rows.len()));
    }
    drop(model);
    Ok(Replayed {
        text: snapshot.text(),
        signals,
        replies: rx.try_iter().collect(),
    })
}

/// The first difference between two line lists, for the failure report.
fn first_difference(got: &[&str], want: &[&str]) -> String {
    let at = got
        .iter()
        .zip(want)
        .position(|(g, w)| g != w)
        .unwrap_or(got.len().min(want.len()));
    format!(
        "line {at}: got {:?}, want {:?} ({} vs {} lines)",
        got.get(at),
        want.get(at),
        got.len(),
        want.len()
    )
}

#[test]
fn every_fixture_replays_to_its_recorded_screen_signals_and_replies() {
    let fixtures = fixtures::all();
    let mut failures = Vec::new();
    let mut compared_signals = 0;
    for f in &fixtures {
        let (want_signals, want_replies) = recorded(&f.events);
        compared_signals += want_signals.len();
        let want_text = normalized(&f.text);
        for chunk in CHUNKINGS {
            let what = format!("{} in {chunk}-byte chunks", f.label());
            let Replayed {
                text,
                signals,
                replies,
            } = match replay(f, chunk) {
                Ok(out) => out,
                Err(e) => {
                    failures.push(format!("{what}: {e}"));
                    continue;
                }
            };
            let got_text = normalized(&text);
            if got_text != want_text {
                failures.push(format!(
                    "{what}: screen differs from the .txt, {}",
                    first_difference(&got_text, &want_text)
                ));
            }
            if signals != want_signals {
                failures.push(format!(
                    "{what}: signals differ: got {} {signals:?}, want {} {want_signals:?}",
                    signals.len(),
                    want_signals.len()
                ));
            }
            if replies != want_replies {
                failures.push(format!(
                    "{what}: replies differ: got {replies:?}, want {want_replies:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fixture replays failed:\n{}",
        failures.len(),
        fixtures.len() * CHUNKINGS.len(),
        failures.join("\n")
    );
    // The recordings hold titles and frames; a parser that reported nothing would pass
    // the comparison only if the recordings were empty too.
    assert!(compared_signals > 0, "no recorded signals compared");
}

#[test]
fn the_event_line_parser_reads_the_capture_format() {
    let fields =
        parse_line(r#"{"t_ms":5,"kind":"title","value":"a\"b\\c\u001b","x":null,"y":true}"#);
    assert_eq!(str_field(&fields, "value"), "a\"b\\c\u{1b}");
    assert_eq!(field(&fields, "t_ms"), Some(&Val::Num(5)));
    assert_eq!(field(&fields, "x"), Some(&Val::Null));
    assert_eq!(field(&fields, "y"), Some(&Val::Bool(true)));
    assert_eq!(unescape_payload(r"\e[?1\;2c\x07"), b"\x1b[?1;2c\x07");
    let (signals, replies) = recorded(concat!(
        r#"{"t_ms":1,"kind":"progress","state":1,"name":"normal","pct":40}"#,
        "\n",
        r#"{"t_ms":2,"kind":"osc133","mark":"A"}"#,
        "\n",
        r#"{"t_ms":3,"kind":"write","value":"\\e[0n","source":"reply"}"#,
        "\n",
        r#"{"t_ms":4,"kind":"write","value":"x","source":"script"}"#,
    ));
    assert_eq!(
        signals,
        [
            TapSignal::Progress(ProgressState::Normal(Some(40))),
            TapSignal::Prompt(PromptMark::A)
        ]
    );
    assert_eq!(replies, [b"\x1b[0n".to_vec()]);
}
