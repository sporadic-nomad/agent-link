//! The vt100 callback tap that turns terminal sequences into events (and optional query
//! replies), and the screen holder that survives vt100 panics.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::events::{EventLog, Val};
use crate::threads::{Outgoing, Source};

/// Turns the sequences vt100 does not model into events; answers terminal queries when it
/// has a reply channel.
#[derive(Clone)]
pub struct Tap {
    log: Arc<EventLog>,
    replies: Option<Sender<Outgoing>>,
}

impl Tap {
    /// A tap that logs to `log` and, with `replies`, answers queries through it.
    pub fn new(log: Arc<EventLog>, replies: Option<Sender<Outgoing>>) -> Self {
        Self { log, replies }
    }

    /// Logs a `query`, then queues the reply (when answering) so the writer's `write` event
    /// with source `reply` always follows it. `answered` means a reply was sent to the writer;
    /// an `error` follows when the writer had already stopped.
    fn query(&self, which: &str, reply: Option<Vec<u8>>) {
        self.query_with(which, reply, &[]);
    }

    fn query_with(&self, which: &str, reply: Option<Vec<u8>>, extra: &[(&str, Val)]) {
        let send = self.replies.as_ref().zip(reply);
        let mut fields = vec![
            ("which", Val::Str(which.into())),
            ("answered", Val::Bool(send.is_some())),
        ];
        fields.extend_from_slice(extra);
        self.log.emit("query", &fields);
        if let Some((tx, bytes)) = send {
            if tx.send(Outgoing::new(Source::Reply, bytes)).is_err() {
                let msg = format!("reply to {which} not sent: the writer has stopped");
                self.log.emit("error", &[("value", Val::Str(msg))]);
            }
        }
    }

    /// A query this tool does not know how to answer, with its `raw` sequence.
    fn query_other(&self, raw: String) {
        self.query_with("other", None, &[("raw", Val::Str(raw))]);
    }

    /// OSC params joined with `;` (sanitised first, so the cut never leaves half a name),
    /// cut to `OSC_MAX` bytes; `len` gives the full length when cut.
    fn osc_fields(&self, params: &[&[u8]]) -> Vec<(&'static str, Val)> {
        let joined = self.log.clean(&lossy(&params.join(&b";"[..])));
        if joined.len() <= OSC_MAX {
            return vec![("params", Val::Str(joined))];
        }
        let mut cut = OSC_MAX;
        while !joined.is_char_boundary(cut) {
            cut -= 1;
        }
        let len = i64::try_from(joined.len()).unwrap_or(i64::MAX);
        vec![
            ("params", Val::Str(joined[..cut].to_string())),
            ("len", Val::Num(len)),
        ]
    }

    /// OSC 9;4: `state` and `pct` are null when missing or not numbers; `raw` carries the
    /// params whenever one could not be read, so a malformed report never looks like a
    /// valid state.
    fn progress(&self, params: &[&[u8]]) {
        let num = |p: Option<&&[u8]>| -> Option<i64> {
            p.and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|s| s.parse().ok())
        };
        let state = num(params.get(2));
        let pct_param = params.get(3).filter(|p| !p.is_empty());
        let pct = num(pct_param);
        let mut fields = vec![
            ("state", state.map_or(Val::Null, Val::Num)),
            (
                "name",
                Val::Str(state.map_or("unknown", progress_name).into()),
            ),
            ("pct", pct.map_or(Val::Null, Val::Num)),
        ];
        if state.is_none() || (pct_param.is_some() && pct.is_none()) || params.len() > 4 {
            fields.push(("raw", Val::Str(lossy(&params.join(&b";"[..])))));
        }
        self.log.emit("progress", &fields);
    }
}

/// Longest OSC payload kept in an `osc` event (clipboard data and inline images are large).
const OSC_MAX: usize = 256;

fn has_param(params: &[&[u16]], wanted: u16) -> bool {
    params.iter().any(|p| p.first() == Some(&wanted))
}

fn first_param(params: &[&[u16]]) -> u16 {
    params.first().and_then(|p| p.first()).copied().unwrap_or(0)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn progress_name(state: i64) -> &'static str {
    match state {
        0 => "remove",
        1 => "normal",
        2 => "error",
        3 => "indeterminate",
        4 => "paused",
        _ => "unknown",
    }
}

/// A CSI without the leading `ESC [`: private marker (`?`, `>`, `=`, `<`), params (`;`,
/// sub-params `:`), intermediates, final byte. vt100 passes the marker as the first
/// intermediate, and an omitted param as 0 (`ESC [ = c` reads `=0c`).
fn csi_raw(i1: Option<u8>, i2: Option<u8>, params: &[&[u16]], c: char) -> String {
    let marker = i1.filter(|b| (b'<'..=b'?').contains(b));
    let mut raw: String = marker.map(char::from).into_iter().collect();
    let params: Vec<String> = params
        .iter()
        .map(|p| p.iter().map(u16::to_string).collect::<Vec<_>>().join(":"))
        .collect();
    raw.push_str(&params.join(";"));
    let trailing = [i1.filter(|_| marker.is_none()), i2];
    raw.extend(trailing.into_iter().flatten().map(char::from));
    raw.push(c);
    raw
}

impl vt100::Callbacks for Tap {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.log.emit("bell", &[]);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.log.emit("title", &[("value", Val::Str(lossy(title)))]);
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        let fields = self.osc_fields(&[b"52", ty, data]);
        self.log.emit("osc", &fields);
    }

    fn paste_from_clipboard(&mut self, _: &mut vt100::Screen, _ty: &[u8]) {
        self.query("osc52", None);
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            // vt100 handles a title without `;` itself; one with `;` is split into params.
            [b"0" | b"2", title @ ..] if title.len() > 1 => {
                let title = lossy(&title.join(&b";"[..]));
                self.log.emit("title", &[("value", Val::Str(title))]);
            }
            [b"9", b"4", ..] => self.progress(params),
            [b"133", mark, rest @ ..] => {
                let mut fields = vec![("mark", Val::Str(lossy(mark)))];
                if !rest.is_empty() {
                    let rest: Vec<String> = rest.iter().map(|p| lossy(p)).collect();
                    fields.push(("params", Val::Str(rest.join(";"))));
                }
                self.log.emit("osc133", &fields);
            }
            // Colour queries: logged so a capture shows what an agent asks; never answered.
            [b"10", b"?"] => self.query("osc10", None),
            [b"11", b"?"] => self.query("osc11", None),
            // Other `?` requests (palette, cursor colour, …).
            [.., b"?"] => self.query_other(lossy(&params.join(&b";"[..]))),
            // Everything else (notifications as OSC 9 / 777, cwd as OSC 7, hyperlinks, …).
            _ => {
                let fields = self.osc_fields(params);
                self.log.emit("osc", &fields);
            }
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        match (i1, i2, c) {
            // vt100 reports a DECSET/DECRST once per mode it does not know, each time with all
            // params, so a combined `?2026;<other>h` can log the frame edge twice.
            (Some(b'?'), None, 'h') if has_param(params, 2026) => {
                self.log.emit("frame_begin", &[]);
            }
            (Some(b'?'), None, 'l') if has_param(params, 2026) => {
                self.log.emit("frame_end", &[]);
            }
            (Some(b'?'), Some(b'$'), 'p') if has_param(params, 2026) => {
                self.query("decrqm2026", Some(b"\x1b[?2026;2$y".to_vec()));
            }
            (None, None, 'c') if first_param(params) == 0 => {
                self.query("da1", Some(b"\x1b[?1;2c".to_vec()));
            }
            (None, None, 'n') if first_param(params) == 5 => {
                self.query("dsr", Some(b"\x1b[0n".to_vec()));
            }
            (None, None, 'n') if first_param(params) == 6 => {
                let (row, col) = screen.cursor_position();
                let reply = format!("\x1b[{};{}R", u32::from(row) + 1, u32::from(col) + 1);
                self.query("cpr", Some(reply.into_bytes()));
            }
            // Logged, never answered: secondary DA, XTVERSION, kitty keyboard flags.
            (Some(b'>'), None, 'c') => self.query("da2", None),
            (Some(b'>'), None, 'q') => self.query("xtversion", None),
            (Some(b'?'), None, 'u') => self.query("kitty_keyboard", None),
            // Other requests, logged raw and never answered: DA3 and other `c`, `n` reports
            // (DECXCPR `?6n`, colour scheme `?996n`; `>…n` sets key modifiers, no query),
            // DECRQM for other modes, XTQMODKEYS, XTWINOPS size/title reports.
            (_, _, 'c') => self.query_other(csi_raw(i1, i2, params, c)),
            (i, _, 'n') if i != Some(b'>') => self.query_other(csi_raw(i1, i2, params, c)),
            (Some(b'$'), None, 'p') | (Some(_), Some(b'$'), 'p') | (Some(b'?'), None, 'm') => {
                self.query_other(csi_raw(i1, i2, params, c));
            }
            (None, None, 't') if matches!(first_param(params), 11 | 13..=16 | 18..=21) => {
                self.query_other(csi_raw(i1, i2, params, c));
            }
            _ => {}
        }
    }
}

/// Screen text rows joined with `\n`, one line per row, trailing blanks of a row omitted.
pub fn screen_text(screen: &vt100::Screen) -> String {
    let (_, cols) = screen.size();
    let mut text = String::new();
    for row in screen.rows(0, cols) {
        text.push_str(&row);
        text.push('\n');
    }
    text
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".into()
    }
}

/// A vt100 parser fed under `catch_unwind`: a panic logs `screen_reset` and starts over
/// with a blank screen of the same size.
pub struct LiveScreen {
    parser: vt100::Parser<Tap>,
    tap: Tap,
    rows: u16,
    cols: u16,
}

impl LiveScreen {
    /// Scrollback rows kept by the model.
    const SCROLLBACK: usize = 200;

    /// A blank screen of `rows` × `cols` reporting to `tap`.
    pub fn new(rows: u16, cols: u16, tap: Tap) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, Self::SCROLLBACK, tap.clone()),
            tap,
            rows,
            cols,
        }
    }

    /// Feeds one chunk read at raw stream byte `offset` (logged as `screen_reset`'s offset when
    /// the parser panics on this chunk).
    pub fn feed(&mut self, chunk: &[u8], offset: usize) {
        let parser = &mut self.parser;
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| parser.process(chunk))) {
            let offset = i64::try_from(offset).unwrap_or(i64::MAX);
            self.tap.log.emit(
                "screen_reset",
                &[
                    ("offset", Val::Num(offset)),
                    ("message", Val::Str(panic_message(payload.as_ref()))),
                ],
            );
            self.parser = vt100::Parser::new_with_callbacks(
                self.rows,
                self.cols,
                Self::SCROLLBACK,
                self.tap.clone(),
            );
        }
    }

    /// The current screen.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

/// Replays a stored stream into a fresh screen (8 KiB chunks, no callbacks) and returns its
/// text plus the number of vt100 panics survived.
pub fn replay_text(stream: &[u8], rows: u16, cols: u16) -> (String, usize) {
    let mut parser = vt100::Parser::new(rows, cols, LiveScreen::SCROLLBACK);
    let mut panics = 0;
    for chunk in stream.chunks(8192) {
        if catch_unwind(AssertUnwindSafe(|| parser.process(chunk))).is_err() {
            panics += 1;
            parser = vt100::Parser::new(rows, cols, LiveScreen::SCROLLBACK);
        }
    }
    (screen_text(parser.screen()), panics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::Sanitizer;
    use std::sync::mpsc::{self, Receiver};

    /// A screen whose tap logs quietly and, when `answer`, queues replies on the receiver.
    fn screen(answer: bool) -> (LiveScreen, Arc<EventLog>, Receiver<Outgoing>) {
        let log = Arc::new(EventLog::new(true, None));
        let (tx, rx) = mpsc::channel();
        let tap = Tap::new(log.clone(), answer.then_some(tx));
        (LiveScreen::new(10, 40, tap), log, rx)
    }

    /// Event lines without their `t_ms`.
    fn events(log: &EventLog) -> Vec<String> {
        log.lines(None)
            .into_iter()
            .map(|l| {
                let (_, rest) = l.split_once(',').expect("t_ms then fields");
                format!("{{{rest}")
            })
            .collect()
    }

    fn feed(bytes: &[u8]) -> Vec<String> {
        let (mut s, log, _rx) = screen(true);
        s.feed(bytes, 0);
        events(&log)
    }

    fn replies(rx: &Receiver<Outgoing>) -> Vec<Vec<u8>> {
        rx.try_iter()
            .map(|o| {
                assert_eq!(o.source, Source::Reply);
                o.bytes
            })
            .collect()
    }

    #[test]
    fn titles_with_and_without_semicolons() {
        assert_eq!(
            feed(b"\x1b]0;hi\x07\x1b]2;a;b c\x07\x1b]0;x;y;z\x1b\\"),
            [
                r#"{"kind":"title","value":"hi"}"#,
                r#"{"kind":"title","value":"a;b c"}"#,
                r#"{"kind":"title","value":"x;y;z"}"#,
            ]
        );
    }

    #[test]
    fn progress_with_and_without_percent() {
        assert_eq!(
            feed(b"\x1b]9;4;1;42\x07\x1b]9;4;3\x07\x1b]9;4;0;\x07"),
            [
                r#"{"kind":"progress","state":1,"name":"normal","pct":42}"#,
                r#"{"kind":"progress","state":3,"name":"indeterminate","pct":null}"#,
                r#"{"kind":"progress","state":0,"name":"remove","pct":null}"#,
            ]
        );
    }

    #[test]
    fn malformed_progress_is_not_a_remove() {
        assert_eq!(
            feed(b"\x1b]9;4\x07\x1b]9;4;x;5\x07\x1b]9;4;1;abc\x07\x1b]9;4;7\x07"),
            [
                r#"{"kind":"progress","state":null,"name":"unknown","pct":null,"raw":"9;4"}"#,
                r#"{"kind":"progress","state":null,"name":"unknown","pct":5,"raw":"9;4;x;5"}"#,
                r#"{"kind":"progress","state":1,"name":"normal","pct":null,"raw":"9;4;1;abc"}"#,
                r#"{"kind":"progress","state":7,"name":"unknown","pct":null}"#,
            ]
        );
    }

    #[test]
    fn other_osc_and_bell_are_logged() {
        let long = format!("\x1b]1337;File={}\x07", "a".repeat(300));
        let mut bytes =
            b"\x1b]9;hello\x07\x1b]777;notify;T;B\x1b\\\x1b]7;file://h/tmp\x07".to_vec();
        bytes.extend_from_slice(b"\x1b]4;1;?\x07\x1b]52;c;aGk=\x07\x1b]52;c;?\x07x\x07");
        bytes.extend_from_slice(long.as_bytes());
        let got = feed(&bytes);
        let cut = format!("1337;File={}", "a".repeat(256 - "1337;File=".len()));
        assert_eq!(
            got,
            [
                r#"{"kind":"osc","params":"9;hello"}"#.to_string(),
                r#"{"kind":"osc","params":"777;notify;T;B"}"#.into(),
                r#"{"kind":"osc","params":"7;file://h/tmp"}"#.into(),
                r#"{"kind":"query","which":"other","answered":false,"raw":"4;1;?"}"#.into(),
                r#"{"kind":"osc","params":"52;c;aGk="}"#.into(),
                r#"{"kind":"query","which":"osc52","answered":false}"#.into(),
                r#"{"kind":"bell"}"#.into(),
                format!(r#"{{"kind":"osc","params":"{cut}","len":310}}"#),
            ]
        );
    }

    #[test]
    fn long_osc_is_sanitised_before_the_cut() {
        let san = Sanitizer::new("/Users/jdoe", "jdoe").unwrap();
        let log = Arc::new(EventLog::new(true, Some(san)));
        let mut s = LiveScreen::new(10, 40, Tap::new(log.clone(), None));
        // Cut before sanitising, the 256-byte limit would fall inside `/Users/jdoe`.
        let body = format!("{}/Users/jdoe/{}", "x".repeat(245), "y".repeat(40));
        s.feed(format!("\x1b]8;;{body}\x07").as_bytes(), 0);
        let line = &events(&log)[0];
        assert!(
            line.contains("x/home/u/\"") && !line.contains("/Users"),
            "{line}"
        );
    }

    #[test]
    fn other_queries_are_logged_raw() {
        let (mut s, log, rx) = screen(true);
        s.feed(
            b"\x1b[=c\x1b[?6n\x1b[?996n\x1b[?12$p\x1b[4$p\x1b[?4m\x1b[14t\x1b[15n",
            0,
        );
        // Not queries: key modifier setup, a resize request, a mode set, SGR.
        s.feed(b"\x1b[>4;1n\x1b[8;5;5t\x1b[?1234h\x1b[1m", 0);
        let other = |raw: &str| {
            format!(r#"{{"kind":"query","which":"other","answered":false,"raw":"{raw}"}}"#)
        };
        let want: Vec<String> = ["=0c", "?6n", "?996n", "?12$p", "4$p", "?4m", "14t", "15n"]
            .map(other)
            .into();
        assert_eq!(events(&log), want);
        assert!(replies(&rx).is_empty());
    }

    #[test]
    fn query_is_logged_before_its_reply_and_a_dead_writer_is_an_error() {
        let (mut s, log, rx) = screen(true);
        drop(rx);
        s.feed(b"\x1b[5n", 0);
        assert_eq!(
            events(&log),
            [
                r#"{"kind":"query","which":"dsr","answered":true}"#,
                r#"{"kind":"error","value":"reply to dsr not sent: the writer has stopped"}"#,
            ]
        );
    }

    #[test]
    fn osc133_marks() {
        assert_eq!(
            feed(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;D;0\x07"),
            [
                r#"{"kind":"osc133","mark":"A"}"#,
                r#"{"kind":"osc133","mark":"B"}"#,
                r#"{"kind":"osc133","mark":"D","params":"0"}"#,
            ]
        );
    }

    #[test]
    fn sync_frames_including_combined_modes() {
        assert_eq!(
            feed(b"\x1b[?2026hx\x1b[?2026l\x1b[?2026;1h\x1b[?1;2026l"),
            [
                r#"{"kind":"frame_begin"}"#,
                r#"{"kind":"frame_end"}"#,
                r#"{"kind":"frame_begin"}"#,
                r#"{"kind":"frame_end"}"#,
            ]
        );
    }

    #[test]
    fn queries_are_answered() {
        let (mut s, log, rx) = screen(true);
        s.feed(b"\x1b[?2026$p\x1b[c\x1b[0c\x1b[5n", 0);
        s.feed(b"abc\x1b[6n\x1b[3;5H\x1b[6n", 0);
        let answered =
            |which: &str| format!(r#"{{"kind":"query","which":"{which}","answered":true}}"#);
        assert_eq!(
            events(&log),
            [
                answered("decrqm2026"),
                answered("da1"),
                answered("da1"),
                answered("dsr"),
                answered("cpr"),
                answered("cpr"),
            ]
        );
        let want: [&[u8]; 6] = [
            b"\x1b[?2026;2$y",
            b"\x1b[?1;2c",
            b"\x1b[?1;2c",
            b"\x1b[0n",
            b"\x1b[1;4R",
            b"\x1b[3;5R",
        ];
        assert_eq!(replies(&rx), want);
    }

    #[test]
    fn answering_off_logs_unanswered() {
        let (mut s, log, rx) = screen(false);
        s.feed(
            b"\x1b[c\x1b[5n\x1b[6n\x1b[?2026$p\x1b[>c\x1b[>q\x1b[?u\x1b]11;?\x07",
            0,
        );
        let unanswered =
            |which: &str| format!(r#"{{"kind":"query","which":"{which}","answered":false}}"#);
        let want: Vec<String> = [
            "da1",
            "dsr",
            "cpr",
            "decrqm2026",
            "da2",
            "xtversion",
            "kitty_keyboard",
            "osc11",
        ]
        .map(unanswered)
        .into();
        assert_eq!(events(&log), want);
        assert!(replies(&rx).is_empty());
    }
}
