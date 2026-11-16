//! Screen text for marker matching: [`normalize`] turns screen rows or a raw output
//! stream into escape-free, whitespace-collapsed, lowercase text; [`ScreenText`] holds the
//! visible rows for row-based checks (the composer's pending input).

use std::sync::OnceLock;

use regex::Regex;

use super::ScreenSnapshot;

/// JavaScript's `\s` set (WhiteSpace + LineTerminator). The markers were written against
/// JavaScript's normaliser; Rust's `\s` differs in two characters (it has U+0085, lacks
/// U+FEFF).
pub(crate) const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// A built-in pattern, compiled once per process.
#[cfg(feature = "agent")]
pub(crate) fn cached(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("built-in pattern compiles"))
}

/// The normaliser's passes, in order; each match becomes one space.
fn passes() -> &'static [Regex; 5] {
    static PASSES: OnceLock<[Regex; 5]> = OnceLock::new();
    PASSES.get_or_init(|| {
        let re = |p: &str| Regex::new(p).expect("built-in pattern compiles");
        [
            // OSC, lazy body: it ends at its FIRST terminator (BEL or ST). A greedy body
            // runs from an ST-terminated OSC (codex opens with `ESC ] 10;? ESC \` colour
            // queries) on to the next BEL-terminated one (a title, a hyperlink) and erases
            // all text in between.
            re(r"\x1b\][^\x07]*?(?:\x07|\x1b\\)"),
            // CSI, 7-bit and 8-bit.
            re(r"(?:\x1b\[|\x{9b})[0-?]*[ -/]*[@-~]"),
            // Other two-byte escapes.
            re(r"\x1b[@-_]"),
            // C0 controls except TAB and LF, and DEL.
            re(r"[\x00-\x08\x0B-\x1F\x7F]"),
            re(&format!("[{JS_SPACE}]+")),
        ]
    })
}

/// Text for marker matching: escape sequences (OSC, CSI, other `ESC` pairs) and control
/// characters become spaces, whitespace runs collapse to one space, the ends are trimmed
/// and the result is lowercase. Takes screen rows joined by `\n` or a raw output stream
/// decoded as UTF-8.
pub fn normalize(raw: &str) -> String {
    let [osc, csi, esc, control, space] = passes();
    let text = osc.replace_all(raw, " ");
    let text = csi.replace_all(&text, " ");
    let text = esc.replace_all(&text, " ");
    let text = control.replace_all(&text, " ");
    let text = space.replace_all(&text, " ");
    // Every whitespace run is a single space now, so trimming spaces trims them all.
    text.trim_matches(' ').to_lowercase()
}

/// The visible screen as rows of text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenText {
    rows: Vec<String>,
}

impl ScreenText {
    /// The rows of `text`, split on `\n` (a screen read joined that way, such as
    /// [`ScreenSnapshot::text`]).
    pub fn from_text(text: &str) -> Self {
        Self {
            rows: text.split('\n').map(str::to_owned).collect(),
        }
    }

    /// Every row, top to bottom.
    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    /// The rows joined with `\n`.
    pub fn joined(&self) -> String {
        self.rows.join("\n")
    }

    /// The last `n` rows that end at the lowest non-blank row (fewer when the screen has
    /// fewer). Blank rows below the content do not count: an inline renderer (claude's
    /// classic one, codex, pi) draws its composer near the top of a fresh screen with blank
    /// rows below it. Empty for a blank screen.
    pub fn bottom_rows(&self, n: usize) -> &[String] {
        let end = self
            .rows
            .iter()
            .rposition(|row| !row.trim().is_empty())
            .map_or(0, |last| last + 1);
        &self.rows[end.saturating_sub(n)..end]
    }
}

impl From<&ScreenSnapshot> for ScreenText {
    fn from(snapshot: &ScreenSnapshot) -> Self {
        Self {
            rows: snapshot.rows.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_osc_ends_at_its_bel_or_its_st() {
        assert_eq!(normalize("a\u{1b}]0;title\u{7}b"), "a b");
        assert_eq!(normalize("a\u{1b}]0;title\u{1b}\\b"), "a b");
    }

    #[test]
    fn an_osc_ends_at_its_first_terminator() {
        // codex opens with ST-terminated colour queries; a later BEL-terminated OSC (title,
        // hyperlink) must not swallow the text in between.
        let raw = "\u{1b}]10;?\u{1b}\\\u{1b}]11;?\u{1b}\\Update available!\u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7}";
        assert_eq!(normalize(raw), "update available! link");
    }

    #[test]
    fn csi_sequences_with_params_and_intermediates_are_removed() {
        assert_eq!(normalize("a\u{1b}[38;2;1;2;3mb\u{1b}[0mc"), "a b c");
        assert_eq!(normalize("a\u{1b}[?2026hb\u{1b}[?2026l"), "a b");
        assert_eq!(normalize("a\u{1b}[1 qb"), "a b"); // intermediate byte
        assert_eq!(normalize("a\u{9b}31mb"), "a b"); // 8-bit CSI
    }

    #[test]
    fn other_escapes_and_controls_become_spaces() {
        assert_eq!(normalize("a\u{1b}Db\u{1b}Mc\u{1b}\\d"), "a b c d");
        // `ESC 7` is outside `ESC @`..`ESC _`: only the ESC goes (as in the original rule).
        assert_eq!(normalize("a\u{1b}7b"), "a 7b");
        assert_eq!(normalize("a\u{0}b\u{8}c\u{7f}d\u{1b}"), "a b c d");
        assert_eq!(normalize("a\rb\u{b}c"), "a b c");
    }

    #[test]
    fn whitespace_collapses_and_the_result_is_trimmed_and_lowercase() {
        assert_eq!(
            normalize("  Quick\t\tSafety \n\n Check  "),
            "quick safety check"
        );
        assert_eq!(normalize("A\u{a0}B\u{3000}C\u{feff}D"), "a b c d");
        assert_eq!(normalize(" \u{1b}[2J \n "), "");
        assert_eq!(normalize("ÄÖ Σ"), "äö σ");
    }

    #[test]
    fn the_claude_raw_stream_reads_as_words() {
        let raw = "\u{1b}[2GQuick\u{1b}[8Gsafety\u{1b}[15Gcheck:\r\r\n\u{1b}[2G\u{1b}[38;2;177;185;249m❯\u{1b}[4GNo,\u{1b}[8Gexit\u{1b}[39m";
        assert_eq!(normalize(raw), "quick safety check: ❯ no, exit");
    }

    #[test]
    fn bottom_rows_end_at_the_last_non_blank_row() {
        let text = ScreenText::from_text("a\nb\n\nc\n\n   \n");
        assert_eq!(text.rows().len(), 7);
        assert_eq!(text.bottom_rows(2), ["", "c"]);
        assert_eq!(text.bottom_rows(9), ["a", "b", "", "c"]);
        assert_eq!(text.bottom_rows(0), [] as [&str; 0]);
        assert!(ScreenText::from_text("\n \n").bottom_rows(3).is_empty());
        assert_eq!(text.joined(), "a\nb\n\nc\n\n   \n");
    }

    #[test]
    fn a_snapshot_converts_row_for_row() {
        let snapshot = ScreenSnapshot {
            rows: vec!["one".into(), "".into(), "two".into()],
            cursor: (0, 0),
            alternate_screen: false,
            bracketed_paste: false,
            title: String::new(),
            generation: 0,
            size: crate::PtySize { rows: 3, cols: 10 },
        };
        let text = ScreenText::from(&snapshot);
        assert_eq!(text.rows(), snapshot.rows);
        assert_eq!(text.joined(), snapshot.text());
    }
}
