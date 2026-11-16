//! The three-way sign-in verdict: [`classify_login`], and its log wording.

use crate::agent::LoginCheck;

/// Reads one sign-in check run as signed in (`Some(true)`), signed out (`Some(false)`) or
/// unknown (`None`).
///
/// `run` is `None` when the command never ran to an end (spawn failure, timeout,
/// unresolvable launcher), else `(exited 0, stdout + "\n" + stderr)` — both streams, because
/// CLIs split their answer: codex prints `Not logged in` to stderr, claude its JSON to stdout.
///
/// **Only a positive answer is a claim; everything else is unknown.** A missed "signed out"
/// costs the user a badge; a false one tells someone with a working account that they are
/// signed out. So:
/// - exit 0 ⇒ signed in;
/// - non-zero with the check's [`signed_out_marker`](LoginCheck::signed_out_marker) in the
///   output (all whitespace removed and case ignored on both sides) ⇒ signed out;
/// - anything else ⇒ `None`: a timeout, a spawn failure, an unsupported subcommand on an
///   older CLI (claude exits 1 there too — the case the marker exists for), a reworded
///   message. A host treats `None` as "assume fine".
pub fn classify_login(check: &LoginCheck, run: Option<(bool, String)>) -> Option<bool> {
    let (exited_zero, output) = run?;
    if exited_zero {
        return Some(true);
    }
    let haystack = strip_whitespace(&output).to_lowercase();
    let needle = strip_whitespace(check.signed_out_marker).to_lowercase();
    // An empty marker would match anything: no verdict without one.
    (!needle.is_empty() && haystack.contains(&needle)).then_some(false)
}

/// The log wording for a verdict.
pub(super) fn describe_login(verdict: Option<bool>) -> &'static str {
    match verdict {
        Some(true) => "signed in",
        Some(false) => "NOT signed in",
        None => "inconclusive ⇒ reported as unknown (assume fine)",
    }
}

/// `s` with ALL whitespace removed, so a marker written readably (`"loggedIn": false`) matches
/// however the output is formatted. Removing, not collapsing to one space: collapsing matches
/// pretty-printed output (`"loggedIn" :  false`) but not compact output (`"loggedIn":false`),
/// which is what a CLI prints the moment it stops pretty-printing.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ProfileRegistry;

    /// The built-in profile's check for `id`.
    fn check_for(id: &str) -> LoginCheck {
        ProfileRegistry::builtin()
            .get(id)
            .and_then(|p| p.login_check())
            .expect("a built-in login check")
    }

    #[test]
    fn classify_login_reads_exit_zero_as_signed_in() {
        // The one unambiguous signal, and it needs no marker: both CLIs exit 0 only when
        // they are signed in.
        let run = Some((true, String::new()));
        assert_eq!(classify_login(&check_for("codex"), run.clone()), Some(true));
        assert_eq!(classify_login(&check_for("claude"), run), Some(true));
    }

    #[test]
    fn classify_login_confirms_signed_out_from_the_real_cli_output() {
        // Verbatim from codex-cli 0.146.0 / claude with a scratch config dir. codex puts its
        // verdict on STDERR with an empty stdout, claude puts JSON on STDOUT — which is why
        // the probe captures both streams.
        let codex = Some((false, "\nNot logged in\n".to_string()));
        assert_eq!(classify_login(&check_for("codex"), codex), Some(false));

        let claude = Some((
            false,
            "{\n  \"loggedIn\": false,\n  \"authMethod\": \"none\",\n  \
             \"apiProvider\": \"firstParty\"\n}\n\n"
                .to_string(),
        ));
        assert_eq!(classify_login(&check_for("claude"), claude), Some(false));
    }

    #[test]
    fn classify_login_reports_unknown_for_an_unsupported_subcommand() {
        // THE case the marker exists for. An older claude without `auth status` exits **1**
        // — indistinguishable from signed out on the exit code alone. The message shape is
        // measured (`claude auth bogus` → "error: unknown command 'bogus' (Did you mean
        // logout?)", exit 1, empty stdout); the subcommand name here is what an older CLI
        // missing `auth` entirely would print instead.
        let claude = Some((
            false,
            "\nerror: unknown command 'auth' (Did you mean logout?)\n".to_string(),
        ));
        assert_eq!(classify_login(&check_for("claude"), claude), None);

        // codex exits 2 here rather than 1, but goes through the same rule.
        let codex = Some((
            false,
            "\nerror: unrecognized subcommand 'status'\n\nUsage: codex login [OPTIONS]\n"
                .to_string(),
        ));
        assert_eq!(classify_login(&check_for("codex"), codex), None);
    }

    #[test]
    fn classify_login_reports_unknown_when_the_check_never_ran() {
        // A timeout (a login shell that hangs without a terminal) or a spawn failure: the
        // regression the three-way verdict fixes — it used to read as signed out.
        assert_eq!(classify_login(&check_for("codex"), None), None);
        assert_eq!(classify_login(&check_for("claude"), None), None);
    }

    #[test]
    fn classify_login_reports_unknown_when_the_message_is_reworded() {
        // A CLI that changes its wording degrades to silence, never to a false badge.
        let run = Some((false, "Error: credentials unavailable\n".to_string()));
        assert_eq!(classify_login(&check_for("codex"), run), None);
    }

    #[test]
    fn classify_login_matches_the_marker_across_formatting_changes() {
        // Matching ignores whitespace and case, so reflowed, re-indented and COMPACT JSON
        // all match. The compact form is the one a collapse-runs-to-one-space rule misses.
        for body in [
            "{\"loggedIn\": false}",
            "{\"loggedIn\":false}",
            "{\n\t\"loggedIn\"  :   false\n}",
            "{\"LOGGEDIN\": FALSE}",
        ] {
            assert_eq!(
                classify_login(&check_for("claude"), Some((false, body.to_string()))),
                Some(false),
                "should have matched: {body}"
            );
        }
    }

    #[test]
    fn an_empty_marker_never_reads_as_signed_out() {
        let check = LoginCheck {
            args: &["status"],
            signed_out_marker: " \n",
        };
        assert_eq!(
            classify_login(&check, Some((false, "anything".into()))),
            None
        );
        assert_eq!(
            classify_login(&check, Some((true, String::new()))),
            Some(true)
        );
    }

    #[test]
    fn the_log_wording_is_the_three_way_verdict() {
        assert_eq!(describe_login(Some(true)), "signed in");
        assert_eq!(describe_login(Some(false)), "NOT signed in");
        assert_eq!(
            describe_login(None),
            "inconclusive ⇒ reported as unknown (assume fine)"
        );
    }
}
