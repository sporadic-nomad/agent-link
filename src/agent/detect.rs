//! Shared helpers for [`AgentProfile`](super::AgentProfile) implementations: marker
//! matching on normalised text, recognising a dialog at the bottom of the screen,
//! reading a dialog's pointer, and the composer's pending-input check.
//!
//! The built-in profiles are made of them; use them to write a profile for a CLI the crate
//! does not know. Match on [`normalize`]d text, and give a dialog's key only when the
//! selection was read: a `None` key leaves the dialog to the user.
//!
//! ```
//! use std::sync::OnceLock;
//!
//! use agent_link::agent::detect::{from_last, has_all, steer_two};
//! use agent_link::agent::{AgentProfile, Dialog, DialogKind, TrustKey};
//! use agent_link::screen::normalize;
//! use regex::Regex;
//!
//! /// A CLI with a two-option trust dialog and a footer once it is ready.
//! struct MyCli;
//!
//! fn pointer() -> &'static Regex {
//!     static RE: OnceLock<Regex> = OnceLock::new();
//!     RE.get_or_init(|| Regex::new(r"❯ ?(?:\d\. ?)?(yes, trust|no, exit)").unwrap())
//! }
//!
//! impl AgentProfile for MyCli {
//!     fn id(&self) -> &str {
//!         "mycli"
//!     }
//!     fn dialog(&self, text: &str) -> Option<Dialog> {
//!         let text = normalize(text);
//!         if !has_all(&text, &["trust this folder?", "yes, trust"]) {
//!             return None;
//!         }
//!         let dialog = from_last(&text, "trust this folder?"); // the latest render
//!         let key = steer_two(dialog, pointer(), "yes, trust", "no, exit");
//!         Some(Dialog { kind: DialogKind::Trust, key })
//!     }
//!     fn input_ready(&self, screen: &str) -> bool {
//!         normalize(screen).contains("mycli ready") && self.dialog(screen).is_none()
//!     }
//! }
//!
//! let dialog = "Trust this folder?\n❯ 1. No, exit\n  2. Yes, trust\n";
//! assert_eq!(MyCli.dialog(dialog).and_then(|d| d.key), Some(TrustKey::Down));
//! assert!(MyCli.input_ready("> \n  mycli ready · esc to interrupt"));
//! ```

use std::sync::OnceLock;

use regex::Regex;

use super::TrustKey;
use crate::screen::{JS_SPACE, ScreenText, cached, normalize};

/// Every fragment occurs in `text` (normalised text, see [`normalize`]).
pub fn has_all(text: &str, fragments: &[&str]) -> bool {
    fragments.iter().all(|f| text.contains(f))
}

/// Some fragment occurs in `text` (normalised text).
pub fn has_any(text: &str, fragments: &[&str]) -> bool {
    fragments.iter().any(|f| text.contains(f))
}

/// `text` from the last occurrence of `header` on, or all of `text` when `header` is
/// absent: older renders of a dialog in a rolling buffer cannot vote.
pub fn from_last<'t>(text: &'t str, header: &str) -> &'t str {
    text.rfind(header).map_or(text, |at| &text[at..])
}

/// Whether the current visible screen ends with the specified dialog. The window covers
/// at most `rows` rows up to and including the lowest row with a letter or digit.
/// Only blank rows and recognized horizontal separators (such as `╰──╯`) may follow it.
/// A bare composer prompt such as `>` or `❯` below the footer closes the dialog block.
///
/// Supply markers in [normalised](normalize) form. The last `header` in the window
/// starts the block, which must contain every fragment and end with `end`. A missing
/// header, an empty header or footer, or a zero-row window yields `false`.
pub fn dialog_at_bottom(
    screen: &str,
    rows: usize,
    header: &str,
    fragments: &[&str],
    end: &str,
) -> bool {
    if rows == 0 || header.is_empty() || end.is_empty() {
        return false;
    }
    let Some(last_word_char) = screen.rfind(char::is_alphanumeric) else {
        return false;
    };
    // Keep the rest of this row, including any punctuation in the footer.
    let end_of_row = screen[last_word_char..]
        .find('\n')
        .map_or(screen.len(), |offset| last_word_char + offset);
    if screen[end_of_row..]
        .lines()
        .any(|row| !row.trim().is_empty() && !is_rule(row))
    {
        return false;
    }
    let content = &screen[..end_of_row];
    let start = content
        .rmatch_indices('\n')
        .nth(rows - 1)
        .map_or(0, |(offset, _)| offset + 1);
    let window = normalize(&content[start..]);
    let Some(at) = window.rfind(header) else {
        return false;
    };
    let block = &window[at..];
    block.ends_with(end) && has_all(block, fragments)
}

/// The option the LAST match of `pointer` in `text` names (its first capture group):
/// partial redraws after an arrow key follow the full render in a rolling buffer. `None`
/// when no pointer row is readable.
pub fn selected_option<'t>(text: &'t str, pointer: &Regex) -> Option<&'t str> {
    pointer
        .captures_iter(text)
        .last()
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// The next key toward `wanted` in a two-option dialog whose options are `wanted` and
/// `other` (normalised labels, the captures of `pointer`): Enter when `wanted` is
/// selected; when `other` is, the arrow toward where `wanted` is listed; `None` when the
/// selection or either label cannot be read.
pub fn steer_two(dialog: &str, pointer: &Regex, wanted: &str, other: &str) -> Option<TrustKey> {
    let selected = selected_option(dialog, pointer)?;
    if selected == wanted {
        return Some(TrustKey::Enter);
    }
    if selected != other {
        return None;
    }
    let wanted_at = dialog.find(wanted)?;
    let other_at = dialog.find(other)?;
    Some(if wanted_at > other_at {
        TrustKey::Down
    } else {
        TrustKey::Up
    })
}

/// How many rows, up from the lowest non-blank row, may hold the composer. A slash-command
/// menu opens below claude's composer plus a footer line and pushes it several rows up; 12
/// keeps it in reach while leaving older transcript rows out.
pub const PENDING_SCAN_ROWS: usize = 12;

/// A composer chip an agent collapses a long paste into (`[Pasted text #1 +11 lines]`,
/// `[Pasted Content 1430 chars]`, `[paste #1 +12 lines]`), in normalised text.
fn paste_chip() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r"\[[{JS_SPACE}]*pasted?(?-u:\b)[^\]]*\]"))
            .expect("built-in pattern compiles")
    })
}

/// Composer chrome before the prompt marker: whitespace and box-drawing borders.
fn composer_prefix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!("^[{JS_SPACE}│┃|]*")).expect("built-in pattern compiles"))
}

/// Composer chrome after the text: whitespace and a right border.
fn composer_suffix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!("[{JS_SPACE}│┃|]*$")).expect("built-in pattern compiles"))
}

/// A horizontal rule or box edge (`────`, `╰───╯`): the end of a composer's rows.
fn is_rule(row: &str) -> bool {
    let row = row.trim();
    !row.is_empty() && row.chars().all(|c| "─━═╌╍┄┅-╭╮╰╯┌┐└┘".contains(c))
}

/// The composer's prompt marker. Transcript echoes of sent messages reuse it, which is why
/// only the bottom-most marker row counts.
fn prompt_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    cached(&RE, "^[>›❯]")
}

/// Whether `sent` still sits unsubmitted in the composer on `screen` (rows joined by
/// `\n`; the current screen, never a rolling buffer):
///
/// - The composer is the **bottom-most** row, among the last [`PENDING_SCAN_ROWS`] rows
///   ending at the lowest non-blank one, that starts (after whitespace and `│ ┃ |`
///   borders) with a prompt marker `>`, `›` or `❯`. The transcript echoes a sent message
///   (and its chip) with the same marker above an empty composer, so only the lowest
///   marker row may testify. No such row ⇒ `None` (unreadable — no submission evidence).
/// - Its text after the marker decides, see [`composer_holds`]. A first line of `sent`
///   wider than the row wraps onto the rows below it: those continuation rows (down to a
///   blank row, a rule or another marker row) are joined to it for the first-line check,
///   whitespace and right borders aside (a word wrap drops a space, a hard wrap splits a
///   word). The empty, chip and short-send checks read the marker row alone, as drawn —
///   nothing trimmed, so a short send ending in `|` still matches.
pub fn pending_input_default(screen: &str, sent: &str) -> Option<bool> {
    pending_input_where(screen, sent, |_| false)
}

/// [`pending_input_default`] skipping marker rows whose leading chrome (the whitespace and
/// borders before the marker) `skip_row` rejects — a menu row that carries the marker
/// (claude's slash menu: an indented `❯` row).
pub fn pending_input_where(
    screen: &str,
    sent: &str,
    skip_row: impl Fn(&str) -> bool,
) -> Option<bool> {
    let text = ScreenText::from_text(screen);
    let rows = text.bottom_rows(PENDING_SCAN_ROWS);
    for (at, row) in rows.iter().enumerate().rev() {
        let chrome = composer_prefix().find(row).map_or(0, |m| m.end());
        let (prefix, rest) = row.split_at(chrome);
        let Some(marker) = prompt_marker().find(rest) else {
            continue;
        };
        if skip_row(prefix) {
            continue;
        }
        // One optional space after the marker belongs to it; normalising trims it anyway.
        // The empty, chip and short-send checks read the row as it is (a short send may end
        // in `|`); only the first-line check joins the rows, right borders trimmed.
        let first = &rest[marker.end()..];
        let composer = normalize(first);
        let mut wrapped = trim_suffix(first).to_string();
        for below in &rows[at + 1..] {
            let inner = trim_suffix(&below[composer_prefix().find(below).map_or(0, |m| m.end())..]);
            if inner.is_empty() || is_rule(below) || prompt_marker().is_match(inner) {
                break;
            }
            wrapped.push('\n');
            wrapped.push_str(inner);
        }
        return Some(holds(&composer, &normalize(&wrapped), sent));
    }
    None
}

/// `text` without trailing whitespace and right border.
fn trim_suffix(text: &str) -> &str {
    composer_suffix()
        .find(text)
        .map_or(text, |m| &text[..m.start()])
}

/// Whether a composer showing `composer` (normalised text) still holds `sent`. The
/// evidence is always tied to the sent text, so a draft the user typed is never taken for
/// it:
///
/// - empty ⇒ `false` (submitted);
/// - a paste chip ⇒ `true`;
/// - else the first line of `sent` with at least 12 characters once normalised ⇒ whether
///   the composer contains it (whitespace aside, so a line the composer wrapped over its
///   rows still counts);
/// - else (a short send such as `/clear`) ⇒ whether the composer equals all of `sent`.
pub fn composer_holds(composer: &str, sent: &str) -> bool {
    holds(composer, composer, sent)
}

/// [`composer_holds`] with the first-line check on `wrapped` (the composer's first row
/// plus its continuation rows, normalised) and every other check on `composer` (its first
/// row).
fn holds(composer: &str, wrapped: &str, sent: &str) -> bool {
    if composer.is_empty() {
        return false;
    }
    if paste_chip().is_match(composer) {
        return true;
    }
    let first_long_line = sent
        .split('\n')
        .map(|line| normalize(line.strip_suffix('\r').unwrap_or(line)))
        .find(|line| line.chars().count() >= 12);
    if let Some(fragment) = first_long_line {
        return wrapped.contains(&fragment) || squash(wrapped).contains(&squash(&fragment));
    }
    let whole = normalize(sent);
    !whole.is_empty() && composer == whole
}

/// `text` without whitespace.
fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASTE: &str = "first line of the pasted text\nsecond line\nthird line";

    fn screen(rows: &[&str]) -> String {
        rows.join("\n")
    }

    #[test]
    fn marker_helpers_match_fragments() {
        assert!(has_all("a b c", &["a", "c"]));
        assert!(!has_all("a b c", &["a", "d"]));
        assert!(has_any("a b c", &["d", "b"]));
        assert!(!has_any("a b c", &["d"]));
        assert_eq!(from_last("x h1 y h1 z", "h1"), "h1 z");
        assert_eq!(from_last("x y", "h1"), "x y");
    }

    #[test]
    fn a_dialog_ends_at_its_footer_above_blank_rows_and_box_edges() {
        let drawn = "history\nChoose   one\n1. Café\nPress   Enter (Y)\n╰──╯\n  \n";
        let open = |s: &str| dialog_at_bottom(s, 3, "choose one", &["café"], "enter (y)");
        assert!(open(drawn));
        assert!(!open(&drawn.replace("Café", "Other")));
        // A redrawn composer and a status line both put the former dialog in history.
        assert!(!open(&format!("{drawn}❯ new draft")));
        assert!(!open(&format!("{drawn}2")));
        assert!(!open(&format!("{drawn}新")));
    }

    #[test]
    fn a_bare_composer_below_a_question_footer_closes_the_dialog() {
        let dialog = "Choose one\n1. Continue\nEnter to select\n╰──╯";
        for row in [">", "❯ ", "›", "│ ❯  │", "*"] {
            let screen = format!("{dialog}\n{row}\n────\n");
            assert!(
                !dialog_at_bottom(&screen, 24, "choose one", &["continue"], "enter to select"),
                "{row:?}"
            );
        }
    }

    #[test]
    fn a_quoted_footer_or_fragments_from_an_older_render_are_insufficient() {
        let open = |s: &str| dialog_at_bottom(s, 24, "choose one", &["1. first"], "enter (y)");
        assert!(!open("The menu says 1. first, then Enter (Y)"));
        assert!(!open(
            "Choose one\n1. First\nChoose one\n2. Second\nEnter (Y)"
        ));
        assert!(!open("Choose one\n1. First\nEnter (Y) ends the prompt"));
    }

    #[test]
    fn the_dialog_window_counts_interior_blank_rows() {
        let drawn = "Choose one\n\nYes\nEnter\n\n────";
        let open = |rows| dialog_at_bottom(drawn, rows, "choose one", &["yes"], "enter");
        assert!(!open(0));
        assert!(!open(3));
        assert!(open(4));
        assert!(open(usize::MAX));
    }

    #[test]
    fn dialog_matching_needs_screen_text_and_nonempty_markers() {
        assert!(!dialog_at_bottom("", 24, "choose", &[], "enter"));
        assert!(!dialog_at_bottom("  \n❯\n╰──╯", 24, "❯", &[], "╯"));
        assert!(!dialog_at_bottom("Choose Enter", 24, "", &[], "enter"));
        assert!(!dialog_at_bottom("Choose Enter", 24, "choose", &[], ""));
    }

    #[test]
    fn the_last_pointer_match_names_the_selection() {
        let pointer = Regex::new(r"> (yes|no)").unwrap();
        assert_eq!(selected_option("> yes no > no", &pointer), Some("no"));
        assert_eq!(selected_option("yes no", &pointer), None);
        assert_eq!(
            steer_two("> no yes", &pointer, "yes", "no"),
            Some(TrustKey::Down)
        );
        assert_eq!(
            steer_two("yes > no", &pointer, "yes", "no"),
            Some(TrustKey::Up)
        );
        assert_eq!(
            steer_two("no > yes", &pointer, "yes", "no"),
            Some(TrustKey::Enter)
        );
        assert_eq!(steer_two("no yes", &pointer, "yes", "no"), None);
    }

    #[test]
    fn an_empty_composer_is_submitted() {
        let s = screen(&["❯ first line of the pasted text", "", "❯ ", "  footer"]);
        assert_eq!(pending_input_default(&s, PASTE), Some(false));
    }

    #[test]
    fn a_chip_in_the_composer_is_pending() {
        let s = screen(&["header", "❯ [Pasted text #1 +2 lines]", "  footer"]);
        assert_eq!(pending_input_default(&s, PASTE), Some(true));
        let s = screen(&["› [Pasted Content 1430 chars]"]);
        assert_eq!(pending_input_default(&s, "anything"), Some(true));
    }

    #[test]
    fn a_transcript_echo_of_the_chip_above_an_empty_composer_is_not_pending() {
        let s = screen(&["> [Pasted text #1 +2 lines]", "  answer", "> "]);
        assert_eq!(pending_input_default(&s, PASTE), Some(false));
    }

    #[test]
    fn the_first_long_line_of_the_send_proves_an_inline_paste() {
        let s = screen(&[
            "› first line of the pasted text",
            "  second line",
            "",
            "  footer",
        ]);
        assert_eq!(pending_input_default(&s, PASTE), Some(true));
        // CRLF line ends are split like LF ones.
        let crlf = "first line of the pasted text\r\nsecond";
        assert_eq!(pending_input_default(&s, crlf), Some(true));
    }

    #[test]
    fn menu_rows_below_the_composer_carry_no_marker_and_are_skipped() {
        let s = screen(&[
            "› /clear",
            "",
            "  /clear  clear the terminal and start a new chat",
        ]);
        assert_eq!(pending_input_default(&s, "/clear"), Some(true));
    }

    #[test]
    fn a_short_send_must_equal_the_composer() {
        let s = screen(&["❯ /clear"]);
        assert_eq!(pending_input_default(&s, "/clear"), Some(true));
        assert_eq!(pending_input_default(&s, "/new"), Some(false));
        let s = screen(&["❯ /clear extra"]);
        assert_eq!(pending_input_default(&s, "/clear"), Some(false));
    }

    #[test]
    fn a_foreign_draft_is_not_our_send() {
        let s = screen(&["❯ something the user typed"]);
        assert_eq!(pending_input_default(&s, PASTE), Some(false));
        assert_eq!(pending_input_default(&s, "/clear"), Some(false));
    }

    #[test]
    fn box_drawing_borders_before_the_marker_are_chrome() {
        let s = screen(&["╭────╮", "│ > first line of the pasted text │", "╰────╯"]);
        assert_eq!(pending_input_default(&s, PASTE), Some(true));
        let s = screen(&["┃ › [paste #1 +12 lines]"]);
        assert_eq!(pending_input_default(&s, PASTE), Some(true));
    }

    #[test]
    fn no_marker_row_is_unreadable() {
        let s = screen(&["first line of the pasted text", "footer"]);
        assert_eq!(pending_input_default(&s, PASTE), None);
        assert_eq!(pending_input_default("", PASTE), None);
    }

    #[test]
    fn blank_rows_below_the_content_do_not_count_toward_the_window() {
        let mut rows = vec!["banner", "❯ first line of the pasted text", "  footer"];
        rows.extend([""; 30]);
        assert_eq!(pending_input_default(&screen(&rows), PASTE), Some(true));
        // A composer more than the window above the last content row is out of reach.
        let mut rows = vec!["❯ first line of the pasted text"];
        rows.extend(["transcript"; PENDING_SCAN_ROWS]);
        assert_eq!(pending_input_default(&screen(&rows), PASTE), None);
    }

    const LONG: &str = "a first line long enough to wrap past the composer row\nsecond line";

    #[test]
    fn a_wrapped_first_line_is_read_across_its_rows() {
        // Hard wrap inside a word, then a rule.
        let s = screen(&[
            "❯ a first line long enough to wr",
            "  ap past the composer row",
            "────",
            "  ? for shortcuts",
        ]);
        assert_eq!(pending_input_default(&s, LONG), Some(true));
        // Word wrap (the space at the break is gone), then a blank row and a footer; a
        // right border is chrome.
        let s = screen(&[
            "│ › a first line long enough to wrap │",
            "│   past the composer row           │",
            "",
            "  footer",
        ]);
        assert_eq!(pending_input_default(&s, LONG), Some(true));
        // The first row alone does not hold it.
        let s = screen(&["❯ a first line long enough to wr", "────"]);
        assert_eq!(pending_input_default(&s, LONG), Some(false));
    }

    #[test]
    fn rows_below_the_composer_never_change_the_short_and_empty_checks() {
        let s = screen(&["❯ /clear", "  ? for shortcuts"]);
        assert_eq!(pending_input_default(&s, "/clear"), Some(true));
        // A short send ending in a border character is read as typed, not trimmed.
        let s = screen(&["❯ ls |", "  ? for shortcuts"]);
        assert_eq!(pending_input_default(&s, "ls |"), Some(true));
        assert_eq!(pending_input_default(&s, "ls"), Some(false));
        let s = screen(&["❯ |"]);
        assert_eq!(pending_input_default(&s, "|"), Some(true));
        let s = screen(&[
            "❯",
            "  a first line long enough to wrap past the composer row",
        ]);
        assert_eq!(pending_input_default(&s, LONG), Some(false));
        // A transcript echo above an empty composer stays an echo.
        let s = screen(&[
            "❯ a first line long enough to wrap past the composer row",
            "  answer",
            "❯ ",
        ]);
        assert_eq!(pending_input_default(&s, LONG), Some(false));
    }

    #[test]
    fn composer_holds_needs_evidence_tied_to_the_send() {
        assert!(!composer_holds("", PASTE));
        assert!(composer_holds("[ paste #1 +12 lines]", ""));
        assert!(!composer_holds("[pastel]", "x"));
        assert!(composer_holds("a first line of the pasted text b", PASTE));
        assert!(!composer_holds("second line", PASTE));
        assert!(!composer_holds("x", ""));
        assert!(!composer_holds("x", " \n "));
    }
}
