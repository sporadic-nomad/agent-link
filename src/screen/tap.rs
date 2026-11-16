//! The `vt100` callbacks: what the parser does not model becomes a [`TapSignal`], and
//! capability queries are answered through the session's writer when a reply channel is
//! given.
//!
//! How `vt100` 0.16 routes the sequences handled here:
//! - OSC 0/2 with one parameter → `set_window_title`; a title containing `;` arrives split
//!   into several parameters at `unhandled_osc`.
//! - OSC 9;4 and OSC 133 → `unhandled_osc`.
//! - `CSI ? … h|l` → `unhandled_csi` once per mode `vt100` does not know, each time with
//!   **all** the modes of that sequence (`CSI ? 25 ; 2026 l` calls it once, `CSI ? 2026 ;
//!   1004 h` twice).
//! - DECRQM `CSI ? 2026 $ p` → `unhandled_csi(Some('?'), Some('$'), [[2026]], 'p')`; DA1
//!   and DSR → `unhandled_csi(None, None, …, 'c' | 'n')`.

use crossbeam_channel::Sender;

use super::{ProgressState, PromptMark, TapSignal};
use crate::pty::SessionId;

/// The DEC private mode for synchronized output.
const SYNC_MODE: u16 = 2026;

#[derive(Clone)]
enum ReplySink {
    External(Sender<Vec<u8>>),
    Session(crate::pty::input::Input),
}

pub(crate) struct Tap {
    /// Where query replies go (the session's writer queue); `None` ⇒ queries are not
    /// answered.
    replies: Option<ReplySink>,
    session: Option<SessionId>,
    /// The last title set; kept across a parser rebuild.
    title: String,
    /// Inside a synchronized-output frame. Edges that do not change it are not reported: a
    /// repeated `?2026h`, a `?2026l` outside a frame, and the second call `vt100` makes for
    /// a sequence that also sets another mode it does not know.
    in_frame: bool,
    signals: Vec<TapSignal>,
}

impl Tap {
    /// A tap that answers queries through `replies` when given; `session` tags its log
    /// lines.
    pub(crate) fn new(replies: Option<Sender<Vec<u8>>>, session: Option<SessionId>) -> Self {
        Self {
            replies: replies.map(ReplySink::External),
            session,
            title: String::new(),
            in_frame: false,
            signals: Vec::new(),
        }
    }

    pub(crate) fn for_session(replies: Option<crate::pty::input::Input>, id: SessionId) -> Self {
        let mut tap = Self::new(None, Some(id));
        tap.replies = replies.map(ReplySink::Session);
        tap
    }

    /// A tap for a rebuilt parser: same reply channel, title and frame state, no signals.
    pub(crate) fn successor(&self) -> Self {
        Self {
            replies: self.replies.clone(),
            session: self.session,
            title: self.title.clone(),
            in_frame: self.in_frame,
            signals: Vec::new(),
        }
    }

    /// The signals collected since the last call, in order.
    pub(crate) fn take_signals(&mut self) -> Vec<TapSignal> {
        std::mem::take(&mut self.signals)
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    fn set_title(&mut self, title: String) {
        self.title.clone_from(&title);
        self.signals.push(TapSignal::Title(title));
    }

    /// A full reset (RIS) ends any open frame.
    pub(crate) fn end_frame(&mut self) {
        self.frame_edge(false);
    }

    fn frame_edge(&mut self, begin: bool) {
        if self.in_frame != begin {
            self.in_frame = begin;
            self.signals.push(if begin {
                TapSignal::FrameBegin
            } else {
                TapSignal::FrameEnd
            });
        }
    }

    /// Never stall the reader for a query reply. Exhausted capacity drops the reply.
    fn reply(&self, which: &str, reply: &[u8]) {
        let Some(tx) = &self.replies else {
            return;
        };
        let tag = Tag(self.session);
        let sent = match tx {
            ReplySink::External(tx) => tx.try_send(reply.to_vec()).is_ok(),
            ReplySink::Session(tx) => tx.try_send(reply).is_ok(),
        };
        if sent {
            log::debug!("[{tag}] answered {which}");
        } else {
            log::debug!("[{tag}] {which} not answered: writer closed, busy or at capacity");
        }
    }
}

/// The log tag: `agent-link:<id>` in a session, `agent-link` for a standalone model.
pub(crate) struct Tag(pub(crate) Option<SessionId>);

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(id) => write!(f, "agent-link:{id}"),
            None => f.write_str("agent-link"),
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn number(param: &[u8]) -> Option<u32> {
    std::str::from_utf8(param).ok()?.parse().ok()
}

/// OSC 9;4;`state`[;`pct`]. An unknown or missing state is no report; a missing or
/// unreadable percentage is `None`, one above 100 is 100.
fn progress(params: &[&[u8]]) -> Option<ProgressState> {
    let pct = params
        .get(3)
        .and_then(|p| number(p))
        .map(|p| u8::try_from(p.min(100)).unwrap_or(100));
    Some(match number(params.get(2)?)? {
        0 => ProgressState::Clear,
        1 => ProgressState::Normal(pct),
        2 => ProgressState::Error(pct),
        3 => ProgressState::Indeterminate,
        4 => ProgressState::Paused(pct),
        _ => return None,
    })
}

fn prompt_mark(mark: &[u8]) -> Option<PromptMark> {
    Some(match mark {
        b"A" => PromptMark::A,
        b"B" => PromptMark::B,
        b"C" => PromptMark::C,
        b"D" => PromptMark::D,
        _ => return None,
    })
}

/// The params are exactly `[[n]]` — or empty when `n` is 0 (an omitted parameter).
fn only_param(params: &[&[u16]], n: u16) -> bool {
    match params {
        [] => n == 0,
        [[p]] => *p == n,
        _ => false,
    }
}

impl vt100::Callbacks for Tap {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.set_title(lossy(title));
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        match params {
            [b"0" | b"2", title @ ..] if title.len() > 1 => {
                self.set_title(lossy(&title.join(&b";"[..])));
            }
            [b"9", b"4", ..] => {
                if let Some(state) = progress(params) {
                    self.signals.push(TapSignal::Progress(state));
                }
            }
            [b"133", mark, ..] => {
                if let Some(mark) = prompt_mark(mark) {
                    self.signals.push(TapSignal::Prompt(mark));
                }
            }
            #[cfg(test)]
            [b"agent-link-test-panic"] => panic!("test panic inside the parser"),
            _ => {}
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
            (Some(b'?'), None, 'h' | 'l') if params.iter().any(|p| *p == [SYNC_MODE]) => {
                self.frame_edge(c == 'h');
            }
            // DECRQM for synchronized output: supported, set inside a frame, else reset.
            (Some(b'?'), Some(b'$'), 'p') if params.first().is_some_and(|p| *p == [SYNC_MODE]) => {
                let state = if self.in_frame { 1 } else { 2 };
                self.reply("DECRQM 2026", format!("\x1b[?2026;{state}$y").as_bytes());
            }
            // Primary device attributes: a VT100 with advanced video, as xterm.js answers.
            (None, None, 'c') if only_param(params, 0) => {
                self.reply("DA1", b"\x1b[?1;2c");
            }
            (None, None, 'n') if only_param(params, 5) => self.reply("DSR", b"\x1b[0n"),
            // Cursor position, 1-based; a cursor parked past the last column (the next
            // character wraps) reports the last column, as xterm does. The row is
            // absolute: `vt100` does not expose origin mode (DECOM), under which xterm
            // reports it relative to the scroll region's top.
            (None, None, 'n') if only_param(params, 6) => {
                let (row, col) = screen.cursor_position();
                let (_, cols) = screen.size();
                let col = col.min(cols.saturating_sub(1));
                let reply = format!("\x1b[{};{}R", u32::from(row) + 1, u32::from(col) + 1);
                self.reply("CPR", reply.as_bytes());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Receiver, unbounded};

    /// A parser with a tap; `answer` gives it a reply channel.
    fn parser(answer: bool) -> (vt100::Parser<Tap>, Receiver<Vec<u8>>) {
        let (tx, rx) = unbounded();
        let tap = Tap::new(answer.then_some(tx), None);
        (vt100::Parser::new_with_callbacks(10, 40, 0, tap), rx)
    }

    fn signals(bytes: &[u8]) -> Vec<TapSignal> {
        let (mut p, _rx) = parser(true);
        p.process(bytes);
        p.callbacks_mut().take_signals()
    }

    fn replies(bytes: &[u8]) -> Vec<Vec<u8>> {
        let (mut p, rx) = parser(true);
        p.process(bytes);
        rx.try_iter().collect()
    }

    fn title(t: &str) -> TapSignal {
        TapSignal::Title(t.into())
    }

    #[test]
    fn full_external_and_session_reply_queues_never_block_processing() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut external = vt100::Parser::new_with_callbacks(10, 40, 0, Tap::new(Some(tx), None));
        external.process(b"\x1b[5n\x1b[5n");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[0n");
        assert!(rx.is_empty());

        let (input, rx) = crate::pty::input::Input::new(crate::InputLimits {
            max_write_bytes: 4,
            max_pending_bytes: 4,
            max_pending_chunks: 1,
        });
        input.send(b"host").unwrap();
        let active = rx.rx.recv().unwrap();
        let mut session = vt100::Parser::new_with_callbacks(
            10,
            40,
            0,
            Tap::for_session(Some(input.clone()), SessionId(1)),
        );
        session.process(b"\x1b[5n");
        assert!(
            rx.rx.is_empty(),
            "a reply cannot bypass an active host write"
        );
        drop(active);
        session.process(b"\x1b[5n");
        assert_eq!(rx.rx.recv().unwrap().bytes, b"\x1b[0n");
    }

    #[test]
    fn titles_via_osc_0_and_2_also_with_semicolons() {
        let (mut p, _rx) = parser(true);
        p.process(b"\x1b]0;zero\x07\x1b]2;two\x1b\\\x1b]2;a;b c\x07\x1b]1;icon\x07");
        assert_eq!(
            p.callbacks_mut().take_signals(),
            [title("zero"), title("two"), title("a;b c")]
        );
        assert_eq!(p.callbacks().title(), "a;b c");
        p.process(b"\x1b]0;a;b c\x07");
        assert_eq!(p.callbacks_mut().take_signals(), [title("a;b c")]);
        assert!(p.callbacks_mut().take_signals().is_empty(), "taken once");
    }

    #[test]
    fn progress_states() {
        use ProgressState::*;
        assert_eq!(
            signals(
                b"\x1b]9;4;3\x07\x1b]9;4;1;40\x07\x1b]9;4;0\x07\x1b]9;4;2;250\x07\
                  \x1b]9;4;4;\x07\x1b]9;4;1;abc\x07\x1b]9;4;0;55\x1b\\"
            ),
            [
                TapSignal::Progress(Indeterminate),
                TapSignal::Progress(Normal(Some(40))),
                TapSignal::Progress(Clear),
                TapSignal::Progress(Error(Some(100))),
                TapSignal::Progress(Paused(None)),
                TapSignal::Progress(Normal(None)),
                TapSignal::Progress(Clear),
            ]
        );
    }

    #[test]
    fn malformed_progress_and_other_osc_are_ignored() {
        assert!(
            signals(
                b"\x1b]9;4\x07\x1b]9;4;7\x07\x1b]9;4;x;5\x07\x1b]9;hello\x07\x1b]7;file://h/t\x07"
            )
            .is_empty()
        );
    }

    #[test]
    fn prompt_marks() {
        assert_eq!(
            signals(b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;Z\x07"),
            [
                TapSignal::Prompt(PromptMark::A),
                TapSignal::Prompt(PromptMark::B),
                TapSignal::Prompt(PromptMark::C),
                TapSignal::Prompt(PromptMark::D),
            ]
        );
    }

    #[test]
    fn sync_frames() {
        assert_eq!(
            signals(b"\x1b[?2026hframe\x1b[?2026l"),
            [TapSignal::FrameBegin, TapSignal::FrameEnd]
        );
        // Combined with a mode vt100 knows: one call, one edge.
        assert_eq!(
            signals(b"\x1b[?2026h\x1b[?25;2026l"),
            [TapSignal::FrameBegin, TapSignal::FrameEnd]
        );
        // Combined with another unknown mode: vt100 calls twice, reported once.
        assert_eq!(
            signals(b"\x1b[?2026;1004h\x1b[?1004;2026l"),
            [TapSignal::FrameBegin, TapSignal::FrameEnd]
        );
        // Edges that change nothing, and other modes.
        assert_eq!(
            signals(b"\x1b[?2026l\x1b[?2026h\x1b[?2026h\x1b[?1004h\x1b[2026h"),
            [TapSignal::FrameBegin]
        );
    }

    #[test]
    fn decrqm_reports_the_frame_state() {
        assert_eq!(
            replies(b"\x1b[?2026$p\x1b[?2026h\x1b[?2026$p\x1b[?2026l\x1b[?1004$p"),
            [b"\x1b[?2026;2$y".to_vec(), b"\x1b[?2026;1$y".to_vec()]
        );
    }

    #[test]
    fn da1_and_dsr() {
        assert_eq!(
            replies(b"\x1b[c\x1b[0c\x1b[>c\x1b[=c\x1b[1c\x1b[5n\x1b[?5n\x1b[>5n"),
            [
                b"\x1b[?1;2c".to_vec(),
                b"\x1b[?1;2c".to_vec(),
                b"\x1b[0n".to_vec()
            ]
        );
    }

    #[test]
    fn cpr_reports_the_cursor_one_based() {
        assert_eq!(
            replies(b"\x1b[6n\x1b[5;7H\x1b[6n\x1b[?6n"),
            [b"\x1b[1;1R".to_vec(), b"\x1b[5;7R".to_vec()]
        );
        // After printing into the last column the cursor waits past it; xterm reports
        // the last column.
        let full_row = format!("\x1b[2;35H{}\x1b[6n", "x".repeat(6));
        assert_eq!(replies(full_row.as_bytes()), [b"\x1b[2;40R".to_vec()]);
    }

    #[test]
    fn no_replies_without_a_reply_channel() {
        let (mut p, rx) = parser(false);
        p.process(b"\x1b[c\x1b[5n\x1b[6n\x1b[?2026$p\x1b]0;t\x07\x1b[?2026h");
        assert!(rx.try_iter().next().is_none());
        assert_eq!(
            p.callbacks_mut().take_signals(),
            [title("t"), TapSignal::FrameBegin],
            "signals do not depend on answering"
        );
    }

    #[test]
    fn a_stopped_writer_drops_the_reply() {
        let (mut p, rx) = parser(true);
        drop(rx);
        p.process(b"\x1b[5n");
    }

    #[test]
    fn a_successor_keeps_title_frame_and_replies() {
        let (mut p, rx) = parser(true);
        p.process(b"\x1b]0;kept\x07\x1b[?2026h");
        let mut next = p.callbacks().successor();
        assert_eq!(next.title(), "kept");
        assert!(next.take_signals().is_empty());
        let mut q = vt100::Parser::new_with_callbacks(10, 40, 0, next);
        q.process(b"\x1b[?2026l\x1b[5n");
        assert_eq!(q.callbacks_mut().take_signals(), [TapSignal::FrameEnd]);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [b"\x1b[0n".to_vec()]);
    }

    #[test]
    fn tags() {
        assert_eq!(Tag(Some(SessionId(4))).to_string(), "agent-link:4");
        assert_eq!(Tag(None).to_string(), "agent-link");
    }
}
