//! The screen a launch line was typed over, and the rows drawn since. An inline agent
//! (codex, claude) leaves its last frame — ready composer, a dialog — on the normal screen
//! when it exits, and the next one draws below it: read whole, that screen shows the new
//! agent ready (or in a dialog) before its launch line is even typed. So every read of
//! the agent's state from the screen — the startup gate's, a delivery's dialog and lull
//! checks — takes only the rows drawn since the launch ([`Leftover::fresh`]). An explicit
//! reset takes its own baseline before Enter to isolate newly drawn refusal notices.

use std::sync::Arc;

/// The screen's rows when the last launch line was typed; none before the first launch
/// (or in a direct session: the child starts on a blank screen).
#[derive(Clone, Debug, Default)]
pub(crate) struct Leftover {
    rows: Arc<[String]>,
}

impl Leftover {
    /// The screen `rows` (top to bottom) as the launch line is typed over them.
    pub(crate) fn new(rows: Vec<String>) -> Self {
        Self { rows: rows.into() }
    }

    /// Apply the same profile matcher to the original rows when a redraw exposes a
    /// possible old notice in the fresh suffix.
    pub(crate) fn text(&self) -> String {
        self.rows.join("\n")
    }

    /// The part of `screen` (rows joined by `\n`) drawn since: the rows below the longest
    /// run at its top that the leftover screen holds — at the same rows, or scrolled up by
    /// the output since (an inline agent draws below what was there). Every row when
    /// nothing matches (the leftover scrolled off, the alternate screen, a cleared screen);
    /// nothing when it is all still there. Blank rows carry no evidence, so a run of blank
    /// rows matched by chance hides nothing.
    pub(crate) fn fresh<'a>(&self, screen: &'a str) -> &'a str {
        if self.rows.is_empty() {
            return screen;
        }
        let rows: Vec<&str> = screen.split('\n').collect();
        let matched = (0..self.rows.len())
            .map(|shift| {
                rows.iter()
                    .zip(&self.rows[shift..])
                    .take_while(|(now, then)| **now == then.as_str())
                    .count()
            })
            .max()
            .unwrap_or(0);
        if matched >= rows.len() {
            return "";
        }
        let start: usize = rows[..matched].iter().map(|row| row.len() + 1).sum();
        &screen[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leftover(rows: &[&str]) -> Leftover {
        Leftover::new(rows.iter().map(|r| r.to_string()).collect())
    }

    #[test]
    fn nothing_before_a_launch_hides_nothing() {
        assert_eq!(Leftover::default().fresh("a\nb"), "a\nb");
    }

    /// The old frame stays where it was; the shell's rows and the new agent's frame below
    /// it are fresh.
    #[test]
    fn an_agent_drawn_below_the_old_frame_is_fresh() {
        let then = leftover(&["old banner", "> ", "ready footer", "$ ", "", ""]);
        let now = "old banner\n> \nready footer\n$ agent\nnew banner\n> \nready footer";
        assert_eq!(then.fresh(now), "$ agent\nnew banner\n> \nready footer");
        // Only the shell's rows so far (its `^C`, a new prompt): nothing of an agent.
        let now = "old banner\n> \nready footer\n$ ^C\n$ \n";
        assert_eq!(then.fresh(now), "$ ^C\n$ \n");
        // Unchanged: nothing new.
        assert_eq!(then.fresh("old banner\n> \nready footer\n$ \n\n"), "");
    }

    /// The new agent's output scrolled the old frame up (and partly off the top).
    #[test]
    fn a_scrolled_old_frame_is_still_old() {
        let then = leftover(&["older", "old banner", "> ", "ready footer", "$ "]);
        let now = "> \nready footer\n$ agent\nnew banner\n> ";
        assert_eq!(then.fresh(now), "$ agent\nnew banner\n> ");
    }

    /// A screen with none of the leftover rows at its top — the alternate screen, a
    /// cleared screen, the leftover long scrolled off — is fresh as a whole.
    #[test]
    fn a_new_screen_is_fresh_as_a_whole() {
        let then = leftover(&["old banner", "> ", "ready footer", "$ "]);
        assert_eq!(
            then.fresh("new banner\n> \nready footer"),
            "new banner\n> \nready footer"
        );
        // Blank rows matched by chance hide only blank rows.
        let then = leftover(&["old", "", "", ""]);
        assert_eq!(then.fresh("\n\nnew dialog\nyes"), "new dialog\nyes");
    }
}
