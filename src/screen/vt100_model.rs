//! [`Vt100Model`]: the `vt100` parser behind the [`ScreenModel`] seam, with panic
//! containment.
//!
//! `vt100` 0.16.2 was replayed against every recorded agent stream and a set of hostile
//! synthetic streams (wide characters at the edges, scroll regions, combining marks) in
//! 1-, 7- and 8 192-byte chunks: the one panic found is a wide character printed into a
//! 1-column screen, inside `process`. So the model is never narrower than 2 columns, and
//! only `process` runs under `catch_unwind`; screen reads never panicked at any width of
//! at least 2 and are not guarded.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crossbeam_channel::Sender;

use super::tap::{Tag, Tap};
use super::{ScreenModel, ScreenSnapshot, TapSignal};
use crate::pty::{PtySize, SessionId};

/// The narrowest screen the model lays out: `vt100` panics on a wide character in a
/// 1-column screen.
const MIN_COLS: u16 = 2;

/// A `vt100` screen fed a PTY's output. A parser panic is contained: the model starts
/// over with a blank screen of the same size — title, frame state, reply channel, input
/// modes (cursor/keypad keys, bracketed paste, mouse), alternate screen and cursor
/// visibility kept; text, attributes and scroll region lost — and
/// [`process`](ScreenModel::process) ends its signals with [`TapSignal::Reset`]. The
/// process's panic hook still runs (the default one prints to stderr).
pub struct Vt100Model {
    parser: vt100::Parser<Tap>,
    /// The size as given; the parser's may be wider (see [`MIN_COLS`]).
    size: PtySize,
    scrollback_lines: usize,
    generation: u64,
    session: Option<SessionId>,
    /// The last chunk ended in `ESC`: a `c` first in the next one completes a reset (RIS).
    after_esc: bool,
}

impl Vt100Model {
    /// A blank screen of `size` keeping `scrollback_lines` rows of scrollback. With
    /// `replies` the model answers the child's capability queries by sending the reply
    /// bytes there; without, it answers none. Sends never block: a full or disconnected
    /// channel drops the reply with a debug log. Use a bounded channel to limit queued
    /// replies; PTY sessions use their shared byte and chunk input budget.
    pub fn new(size: PtySize, scrollback_lines: usize, replies: Option<Sender<Vec<u8>>>) -> Self {
        Self::build(size, scrollback_lines, Tap::new(replies, None), None)
    }

    /// [`new`](Self::new) for a session: its log lines carry the session id.
    pub(crate) fn for_session(
        id: SessionId,
        size: PtySize,
        scrollback_lines: usize,
        replies: Option<crate::pty::input::Input>,
    ) -> Self {
        Self::build(
            size,
            scrollback_lines,
            Tap::for_session(replies, id),
            Some(id),
        )
    }

    fn build(size: PtySize, scrollback_lines: usize, tap: Tap, session: Option<SessionId>) -> Self {
        let (rows, cols) = layout(size);
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, scrollback_lines, tap),
            size,
            scrollback_lines,
            generation: 0,
            session,
            after_esc: false,
        }
    }

    /// A blank parser at the current size, its tap carrying over title, frame state and
    /// reply channel, and the old screen's modes replayed into it (a child that set them
    /// once, like a TUI's bracketed paste, never sends them again).
    fn rebuild(&mut self) {
        let tap = self.parser.callbacks().successor();
        // Reads never panicked in the spike; guarded anyway, the parser just did.
        let mut modes = Vec::new();
        let _ = guarded(|| {
            let screen = self.parser.screen();
            if screen.alternate_screen() {
                modes.extend_from_slice(b"\x1b[?1049h");
            }
            if screen.hide_cursor() {
                modes.extend_from_slice(b"\x1b[?25l");
            }
            modes.extend(screen.input_mode_formatted());
        });
        let (rows, cols) = layout(self.size);
        self.parser = vt100::Parser::new_with_callbacks(rows, cols, self.scrollback_lines, tap);
        let parser = &mut self.parser;
        let _ = guarded(|| parser.process(&modes));
        self.parser.callbacks_mut().take_signals();
    }

    /// Feeds one part of a chunk; `Err` after a contained panic (the model was rebuilt).
    fn feed(&mut self, bytes: &[u8], signals: &mut Vec<TapSignal>) -> Result<(), ()> {
        let parser = &mut self.parser;
        let outcome = guarded(|| parser.process(bytes));
        signals.extend(self.parser.callbacks_mut().take_signals());
        let Err(message) = outcome else {
            return Ok(());
        };
        log::warn!(
            "[{}] screen model reset after a parser panic: {message}",
            Tag(self.session)
        );
        self.rebuild();
        self.generation += 1;
        signals.push(TapSignal::Reset(message));
        Err(())
    }
}

/// The index just past the first full reset (`ESC c`) in `bytes`; `after_esc` says the
/// previous chunk ended in `ESC`. `vt100` handles RIS without a callback, so the tap
/// would never learn that it ended a frame. `ESC` starts a new sequence from any parser
/// state, so the byte pair is a reset wherever it appears.
fn ris_end(after_esc: bool, bytes: &[u8]) -> Option<usize> {
    if after_esc && bytes.first() == Some(&b'c') {
        return Some(1);
    }
    bytes.windows(2).position(|w| w == b"\x1bc").map(|i| i + 2)
}

/// The parser's `(rows, cols)` for `size`: at least 1 row and [`MIN_COLS`] columns.
fn layout(size: PtySize) -> (u16, u16) {
    (size.rows.max(1), size.cols.max(MIN_COLS))
}

/// Runs `f`, turning a panic into `Err(message)`.
pub(crate) fn guarded(f: impl FnOnce()) -> Result<(), String> {
    catch_panic(f)
}

/// Runs `f` and returns its value, turning a panic into `Err(message)` (the agent
/// controller calls host profile code through it).
pub(crate) fn catch_panic<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into())
    })
}

impl ScreenModel for Vt100Model {
    fn process(&mut self, bytes: &[u8]) -> Vec<TapSignal> {
        self.generation += 1;
        let mut signals = Vec::new();
        let mut rest = bytes;
        // Split at each RIS, so a frame it ends is reported in stream order.
        while let Some(end) = ris_end(self.after_esc, rest) {
            self.after_esc = false;
            let (part, tail) = rest.split_at(end);
            if self.feed(part, &mut signals).is_err() {
                return signals;
            }
            self.parser.callbacks_mut().end_frame();
            signals.extend(self.parser.callbacks_mut().take_signals());
            rest = tail;
        }
        let fed = self.feed(rest, &mut signals).is_ok();
        self.after_esc = fed && rest.last() == Some(&0x1b);
        signals
    }

    fn resize(&mut self, size: PtySize) {
        let (rows, cols) = layout(size);
        self.parser.screen_mut().set_size(rows, cols);
        self.size = size;
    }

    fn snapshot(&self) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        ScreenSnapshot {
            rows: screen
                .rows(0, cols)
                .map(|row| row.trim_end().to_string())
                .collect(),
            cursor: screen.cursor_position(),
            alternate_screen: screen.alternate_screen(),
            bracketed_paste: screen.bracketed_paste(),
            title: self.parser.callbacks().title().to_string(),
            generation: self.generation,
            size: self.size,
        }
    }

    fn repaint_stream(&self) -> Vec<u8> {
        self.parser.screen().contents_formatted()
    }

    /// Scrolls the view up one row at a time and reads the top row, then scrolls back.
    fn scrollback_rows(&mut self, max: usize) -> Vec<String> {
        let screen = self.parser.screen_mut();
        let (_, cols) = screen.size();
        let mut rows = Vec::new();
        for offset in 1..=max {
            screen.set_scrollback(offset);
            if screen.scrollback() < offset {
                break;
            }
            let top = screen.rows(0, cols).next().unwrap_or_default();
            rows.push(top.trim_end().to_string());
        }
        screen.set_scrollback(0);
        rows.reverse();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    fn size(rows: u16, cols: u16) -> PtySize {
        PtySize { rows, cols }
    }

    fn model(rows: u16, cols: u16) -> Vt100Model {
        Vt100Model::new(size(rows, cols), 50, None)
    }

    /// Feeds `bytes` and asserts nothing reset.
    fn feed(m: &mut Vt100Model, bytes: &[u8]) -> Vec<TapSignal> {
        let signals = m.process(bytes);
        assert!(
            !signals.iter().any(|s| matches!(s, TapSignal::Reset(_))),
            "{signals:?}"
        );
        signals
    }

    #[test]
    fn guarded_returns_the_panic_message() {
        assert_eq!(guarded(|| {}), Ok(()));
        assert_eq!(guarded(|| panic!("static")), Err("static".to_string()));
        let n = 3;
        assert_eq!(
            guarded(|| panic!("formatted {n}")),
            Err("formatted 3".into())
        );
        assert_eq!(
            guarded(|| std::panic::panic_any(7u8)),
            Err("non-string panic payload".into())
        );
    }

    #[test]
    fn snapshot_reads_the_screen() {
        let mut m = model(4, 20);
        feed(
            &mut m,
            b"\x1b]0;title\x07hello   \r\n\x1b[31mred\x1b[0m\x1b[?2004h\x1b[3;5Hx",
        );
        let s = m.snapshot();
        assert_eq!(s.rows, ["hello", "red", "    x", ""]);
        assert_eq!(s.text(), "hello\nred\n    x\n");
        assert_eq!(s.cursor, (2, 5));
        assert!(s.bracketed_paste);
        assert!(!s.alternate_screen);
        assert_eq!(s.title, "title");
        assert_eq!(s.generation, 1);
        assert_eq!(s.size, size(4, 20));

        feed(&mut m, b"\x1b[?1049h");
        assert!(m.snapshot().alternate_screen);
        assert_eq!(m.snapshot().generation, 2);
    }

    #[test]
    fn repaint_stream_redraws_the_screen() {
        let mut m = model(5, 30);
        feed(&mut m, b"one\r\n\x1b[1;32mtwo\x1b[0m\r\n  three\x1b[2;2H");
        let mut copy = model(5, 30);
        feed(&mut copy, &m.repaint_stream());
        assert_eq!(copy.snapshot().rows, m.snapshot().rows);
        assert_eq!(copy.snapshot().cursor, (1, 1));
        assert_eq!(copy.repaint_stream(), m.repaint_stream());
    }

    #[test]
    fn resize_changes_the_layout_and_the_size() {
        let mut m = model(3, 10);
        feed(&mut m, b"0123456789abc");
        m.resize(size(6, 4));
        let s = m.snapshot();
        assert_eq!((s.size, s.rows.len()), (size(6, 4), 6));
        assert_eq!(s.rows[0], "0123");
        feed(&mut m, b"\x1b[6;1Hbot");
        assert_eq!(m.snapshot().rows[5], "bot");
    }

    /// The only vt100 panic found: a wide character in a 1-column screen (fresh, at the
    /// last column, after a resize). The model lays out at least 2 columns.
    #[test]
    fn one_column_wide_characters_do_not_reset() {
        let mut m = model(10, 1);
        feed(&mut m, "漢a😀\u{301}".as_bytes());
        feed(&mut m, "\x1b[1;1H漢a\x1b[2;1H😀\u{301}".as_bytes());
        assert_eq!(m.snapshot().size, size(10, 1));

        let mut m = model(10, 120);
        feed(&mut m, "\x1b[1;117H漢😀".as_bytes());
        m.resize(size(10, 1));
        feed(&mut m, "漢a😀\u{301}\x1b[1;1H漢".as_bytes());
        m.resize(size(10, 0));
        feed(&mut m, "漢".as_bytes());
    }

    #[test]
    fn a_parser_panic_resets_to_a_blank_screen_keeping_title_and_size() {
        let (tx, rx) = unbounded();
        let mut m = Vt100Model::new(size(6, 30), 50, Some(tx));
        feed(&mut m, b"\x1b]2;kept\x07before");
        m.resize(size(8, 40));
        let signals = m.process(b"\x1b]0;new\x07\x1b]agent-link-test-panic\x07lost");
        assert_eq!(
            signals,
            [
                TapSignal::Title("new".into()),
                TapSignal::Reset("test panic inside the parser".into())
            ]
        );
        let s = m.snapshot();
        assert_eq!(s.title, "new");
        assert_eq!(s.size, size(8, 40));
        assert_eq!(s.rows, vec![String::new(); 8]);
        assert_eq!(s.generation, 3, "two chunks plus the reset");
        // The rebuilt parser keeps working and still answers.
        feed(&mut m, b"after\x1b[6n");
        assert_eq!(m.snapshot().rows[0], "after");
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [b"\x1b[1;6R".to_vec()]);
    }

    /// A TUI sets its modes once; a reset must not drop them.
    #[test]
    fn a_parser_panic_keeps_the_modes() {
        let mut m = model(6, 30);
        feed(
            &mut m,
            b"\x1b[?2004h\x1b[?1049h\x1b[?25l\x1b[?1h\x1b[?1000h",
        );
        let signals = m.process(b"\x1b]agent-link-test-panic\x07");
        assert!(
            matches!(signals.last(), Some(TapSignal::Reset(_))),
            "{signals:?}"
        );
        let s = m.snapshot();
        assert!(s.bracketed_paste && s.alternate_screen, "{s:?}");
        let screen = m.parser.screen();
        assert!(screen.hide_cursor());
        assert!(screen.application_cursor());
        assert_eq!(
            screen.mouse_protocol_mode(),
            vt100::MouseProtocolMode::PressRelease
        );
    }

    /// `vt100` handles RIS without a callback; the model still ends an open frame, in
    /// stream order, also when `ESC` and `c` arrive in different chunks.
    #[test]
    fn a_full_reset_ends_an_open_frame() {
        let (tx, rx) = unbounded();
        let mut m = Vt100Model::new(size(6, 30), 50, Some(tx));
        assert_eq!(
            feed(&mut m, b"\x1b[?2026hxx\x1bc\x1b[?2026h"),
            [
                TapSignal::FrameBegin,
                TapSignal::FrameEnd,
                TapSignal::FrameBegin
            ]
        );
        assert_eq!(m.snapshot().rows[0], "", "vt100 reset the screen");
        assert!(feed(&mut m, b"abc\x1b").is_empty());
        assert_eq!(feed(&mut m, b"c\x1b[?2026$p"), [TapSignal::FrameEnd]);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [b"\x1b[?2026;2$y".to_vec()]
        );
        assert!(feed(&mut m, b"\x1bc").is_empty(), "no frame, no edge");
    }

    #[test]
    fn scrollback_rows_are_oldest_first() {
        let mut m = model(3, 20);
        assert!(m.scrollback_rows(10).is_empty());
        feed(&mut m, b"r1\r\nr2\r\nr3\r\nr4\r\nr5  \r\nr6");
        assert_eq!(m.scrollback_rows(10), ["r1", "r2", "r3"]);
        assert_eq!(m.scrollback_rows(2), ["r2", "r3"]);
        assert_eq!(m.scrollback_rows(0), Vec::<String>::new());
        // The view is back at the bottom.
        assert_eq!(m.snapshot().rows, ["r4", "r5", "r6"]);
    }

    #[test]
    fn scrollback_is_empty_under_a_scroll_region_and_on_the_alternate_screen() {
        let mut m = model(4, 20);
        feed(&mut m, b"\x1b[1;3ra\r\nb\r\nc\r\nd\r\ne");
        assert!(m.scrollback_rows(10).is_empty());
        let mut m = model(3, 20);
        feed(
            &mut m,
            b"x1\r\nx2\r\nx3\r\nx4\x1b[?1049hy1\r\ny2\r\ny3\r\ny4",
        );
        assert!(m.scrollback_rows(10).is_empty());
        feed(&mut m, b"\x1b[?1049l");
        assert_eq!(m.scrollback_rows(10), ["x1"]);
    }

    #[test]
    fn scrollback_keeps_the_configured_rows() {
        let mut m = Vt100Model::new(size(2, 10), 3, None);
        for i in 0..10 {
            feed(&mut m, format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(m.scrollback_rows(100), ["line6", "line7", "line8"]);
    }
}
