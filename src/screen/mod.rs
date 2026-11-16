//! The headless screen model a PTY session feeds every output chunk to, so the crate can
//! read the child's screen without the host's renderer.
//!
//! - [`ScreenModel`] is the seam; [`Vt100Model`] implements it with `vt100`. A session
//!   whose [`ScreenConfig`](crate::pty::ScreenConfig) is `enabled` owns one and feeds it
//!   each chunk **before** publishing it, so [`Terminal::screen`](crate::Terminal::screen)
//!   is always at or ahead of what the host has rendered.
//! - The model's tap turns what `vt100` does not model into [`TapSignal`]s: the window title
//!   (OSC 0/2), progress (OSC 9;4), prompt marks (OSC 133) and synchronized-output frames
//!   (DEC mode 2026). A session publishes them as [`Event::Title`](crate::Event::Title),
//!   [`Event::Progress`](crate::Event::Progress), [`Event::Frame`](crate::Event::Frame) (at
//!   each frame end) and [`Event::ScreenReset`](crate::Event::ScreenReset); prompt marks are
//!   not published.
//! - With a reply channel the tap also answers the child's capability queries (DA1, DSR,
//!   cursor position, the DEC 2026 mode report). A host whose own renderer answers them
//!   (xterm.js does) turns that off with `answer_queries: false`, or the child gets every
//!   reply twice.
//! - A `vt100` panic is contained: the model starts over with a blank screen of the same
//!   size and reports [`TapSignal::Reset`]. The panic still prints through the process's
//!   panic hook (the crate installs none).
//! - [`normalize`] turns screen text or a raw output stream into the escape-free,
//!   lowercase text that screen markers are matched against; [`ScreenText`] holds the rows.
//!
//! Read the model with `Terminal::screen` — to find text on screen without a renderer of
//! your own, or to draw a late-attached renderer from `Terminal::repaint_stream`. A session
//! that only needs the bytes turns it off (`ScreenConfig::enabled`); an `AgentTerminal`
//! always keeps one, its controller reads every decision off it.
//!
//! ```
//! use agent_link::screen::normalize;
//!
//! let raw = "\u{1b}[1mDo you trust\u{1b}[0m the files\r\n  in this folder?";
//! assert_eq!(normalize(raw), "do you trust the files in this folder?");
//! ```

mod tap;
mod text;
mod vt100_model;

use std::hash::{Hash, Hasher};

pub use crate::pty::ProgressState;
use crate::pty::PtySize;
#[cfg(feature = "agent")]
pub(crate) use text::{JS_SPACE, cached};
pub use text::{ScreenText, normalize};
pub use vt100_model::Vt100Model;
#[cfg(feature = "agent")]
pub(crate) use vt100_model::catch_panic;

/// A terminal emulator without a display, fed the same bytes the host renders, in order.
/// A session owns one per [`ScreenConfig`](crate::pty::ScreenConfig) with `enabled`.
pub trait ScreenModel: Send {
    /// Feeds one chunk of child output, in stream order, and returns the signals it carried,
    /// in order. A contained parser panic ends the list with [`TapSignal::Reset`].
    fn process(&mut self, bytes: &[u8]) -> Vec<TapSignal>;
    /// Follows a resize of the PTY.
    fn resize(&mut self, size: PtySize);
    /// The visible screen now.
    fn snapshot(&self) -> ScreenSnapshot;
    /// Bytes that redraw the visible screen on a blank terminal of the same size: text,
    /// colours and attributes, then the cursor position and visibility.
    fn repaint_stream(&self) -> Vec<u8>;
    /// Up to `max` rows that scrolled off the top of the main screen, oldest first,
    /// trailing blanks trimmed. Empty on the alternate screen and for rows that left
    /// through a scroll region (`DECSTBM`) — screen detection must not depend on it.
    fn scrollback_rows(&mut self, max: usize) -> Vec<String>;
}

/// The visible screen at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenSnapshot {
    /// One string per visible row, top to bottom, trailing blanks trimmed (an empty
    /// string for a blank row). A wide character fills two columns but appears once.
    pub rows: Vec<String>,
    /// `(row, col)`, 0-based. `col` equals the width after a character was printed into
    /// the last column (the next one wraps).
    pub cursor: (u16, u16),
    /// The alternate screen is shown (full-screen programs).
    pub alternate_screen: bool,
    /// The child turned bracketed paste on.
    pub bracketed_paste: bool,
    /// The last window title the child set (OSC 0/2); empty until it sets one.
    pub title: String,
    /// +1 per chunk the model processed and per reset: a snapshot with a higher value saw
    /// more of the stream.
    pub generation: u64,
    /// The terminal size. The model lays the screen out at least 2 columns wide.
    pub size: PtySize,
}

impl ScreenSnapshot {
    /// The rows joined with `\n` (no trailing newline).
    pub fn text(&self) -> String {
        self.rows.join("\n")
    }

    /// A hash of the rows, to tell whether the text changed between two snapshots. Stable
    /// within one process only.
    pub fn row_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.rows.hash(&mut hasher);
        hasher.finish()
    }
}

/// Something the child's output said that the screen text does not show.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TapSignal {
    /// The window title was set (OSC 0 or 2), also to the same value again.
    Title(String),
    /// A progress report (OSC 9;4).
    Progress(ProgressState),
    /// A shell-integration prompt mark (OSC 133).
    Prompt(PromptMark),
    /// A synchronized-output frame began (`CSI ? 2026 h` outside a frame).
    FrameBegin,
    /// A synchronized-output frame ended (`CSI ? 2026 l` inside a frame, or a full reset
    /// `ESC c` inside one): the redraw is complete at this point of the stream.
    FrameEnd,
    /// The parser panicked; the model starts over with a blank screen of the same size
    /// (modes kept, see [`Vt100Model`]). The rest of that chunk is lost. Carries the panic
    /// message.
    Reset(String),
}

/// An OSC 133 (FinalTerm) prompt mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PromptMark {
    /// `A`: the prompt starts.
    A,
    /// `B`: the prompt ends, user input starts.
    B,
    /// `C`: the command was submitted, its output starts.
    C,
    /// `D`: the command finished.
    D,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(rows: &[&str]) -> ScreenSnapshot {
        ScreenSnapshot {
            rows: rows.iter().map(|r| r.to_string()).collect(),
            cursor: (0, 0),
            alternate_screen: false,
            bracketed_paste: false,
            title: String::new(),
            generation: 0,
            size: PtySize { rows: 3, cols: 10 },
        }
    }

    #[test]
    fn text_joins_rows_without_a_trailing_newline() {
        assert_eq!(snapshot(&["a", "", "b"]).text(), "a\n\nb");
        assert_eq!(snapshot(&[]).text(), "");
    }

    #[test]
    fn row_hash_follows_the_rows_only() {
        let a = snapshot(&["a", "b"]);
        let mut moved = a.clone();
        moved.cursor = (1, 1);
        moved.generation = 9;
        assert_eq!(a.row_hash(), moved.row_hash());
        assert_ne!(a.row_hash(), snapshot(&["a", "c"]).row_hash());
        assert_ne!(a.row_hash(), snapshot(&["ab"]).row_hash());
    }
}
