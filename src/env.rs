//! Child environment: one [`EnvPolicy`] describes what a spawned process inherits, sets
//! and loses, and [`EnvPolicy::apply`] writes it onto any [`EnvTarget`] — a PTY
//! `CommandBuilder` or a `std::process::Command` — so both spawn paths get the same
//! environment from the same description.
//!
//! The crate owns how a policy is applied; *what* goes in (tool paths, user settings,
//! keys that must never reach a child) stays with the host.
//!
//! Use one as `SpawnSpec::env` (PTY sessions) and `RpcSpawnSpec::env` (stdio children).
//! Applied in this order: clear (only when `inherit` is false) → `set` → `remove` → `PATH`
//! prepend, so a key both set and removed ends up removed. Resolve a child's launcher
//! against [`EnvPolicy::effective_path`], never against this process's `PATH`:
//!
//! ```
//! use agent_link::env::EnvPolicy;
//! use agent_link::resolve::find_in_path_os;
//!
//! let mut env = EnvPolicy::default();
//! env.remove("OPENAI_API_KEY"); // never reaches the child
//! env.path_prepend.push("/opt/tools/bin".into());
//! let claude = find_in_path_os(&env.effective_path(), &["claude"]);
//! ```
//!
//! Keys and values are `OsStr`/`OsString` throughout: an inherited `PATH` need not be
//! UTF-8 (a Windows `PATH` may hold unpaired surrogates), and losing it to `""` would
//! make every launcher unresolvable.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The `PATH` key, as the crate sets it.
const PATH_KEY: &str = "PATH";

/// Where an env policy is applied: a PTY `CommandBuilder` or a `std::process::Command`.
///
/// Implement it for another builder to apply an [`EnvPolicy`] there too.
pub trait EnvTarget {
    /// Set `key=val` in the child's environment.
    fn set_env(&mut self, key: &OsStr, val: &OsStr);
    /// Remove `key` from the child's environment.
    fn remove_env(&mut self, key: &OsStr);
    /// Drop every variable, inherited ones included.
    fn clear_env(&mut self);
}

impl EnvTarget for std::process::Command {
    fn set_env(&mut self, key: &OsStr, val: &OsStr) {
        self.env(key, val);
    }
    fn remove_env(&mut self, key: &OsStr) {
        self.env_remove(key);
    }
    fn clear_env(&mut self) {
        self.env_clear();
    }
}

impl EnvTarget for portable_pty::CommandBuilder {
    fn set_env(&mut self, key: &OsStr, val: &OsStr) {
        self.env(key, val);
    }
    fn remove_env(&mut self, key: &OsStr) {
        self.env_remove(key);
    }
    fn clear_env(&mut self) {
        self.env_clear();
    }
}

/// What a child's environment is: inherited or empty, plus sets, removals and `PATH`
/// prepends.
///
/// [`apply`](Self::apply) runs in a fixed order — clear (when `!inherit`) → `set` →
/// `remove` → `PATH` prepend — so a key that is both set and removed ends up removed,
/// and prepended directories always land in front of whatever `PATH` the other steps
/// left. On Windows keys compare ASCII case-insensitively (`Path` is `PATH`).
///
/// ```
/// use agent_link::env::EnvPolicy;
///
/// let mut policy = EnvPolicy::default();
/// policy.set("TOOL_BIN", "/opt/tools/tool").remove("SECRET_TOKEN");
/// policy.path_prepend.push("/opt/tools".into());
///
/// let mut cmd = std::process::Command::new("sh");
/// policy.apply(&mut cmd);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Variables to set, applied in order; a later duplicate wins.
    pub set: Vec<(OsString, OsString)>,
    /// Variables to remove, after `set`.
    pub remove: Vec<OsString>,
    /// Directories put in front of `PATH`; the first entry ends up first.
    pub path_prepend: Vec<PathBuf>,
    /// `true` starts from the inherited environment, `false` from an empty one.
    pub inherit: bool,
}

/// Prints the `set` keys only: values may be credentials, and a policy can reach a log.
impl std::fmt::Debug for EnvPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set_keys: Vec<&OsStr> = self.set.iter().map(|(k, _)| k.as_os_str()).collect();
        f.debug_struct("EnvPolicy")
            .field("set_keys", &set_keys)
            .field("remove", &self.remove)
            .field("path_prepend", &self.path_prepend)
            .field("inherit", &self.inherit)
            .finish()
    }
}

impl Default for EnvPolicy {
    /// Inherit everything, change nothing.
    fn default() -> Self {
        Self {
            set: Vec::new(),
            remove: Vec::new(),
            path_prepend: Vec::new(),
            inherit: true,
        }
    }
}

impl EnvPolicy {
    /// Add `key=val` to [`set`](Self::set).
    pub fn set(&mut self, key: impl Into<OsString>, val: impl Into<OsString>) -> &mut Self {
        self.set.push((key.into(), val.into()));
        self
    }

    /// Add `key` to [`remove`](Self::remove).
    pub fn remove(&mut self, key: impl Into<OsString>) -> &mut Self {
        self.remove.push(key.into());
        self
    }

    /// Whether the child gets `key` from [`set`](Self::set) — set and not removed.
    pub fn sets(&self, key: &OsStr) -> bool {
        self.set_value(key).is_some()
    }

    /// The `PATH` the child will see.
    ///
    /// Resolve the child's launchers against this value, never against the process
    /// `PATH`: once directories are prepended (or `PATH` is set), the two differ, and a
    /// launcher found one way but spawned the other is reported installed and then fails
    /// to start.
    ///
    /// Starts from the `set` value of `PATH`, else the inherited one when `inherit`
    /// (`var_os`, so a non-UTF-8 `PATH` survives instead of collapsing to `""`), else
    /// empty; a removed `PATH` starts empty. Then `path_prepend` goes in front.
    pub fn effective_path(&self) -> OsString {
        self.effective_path_with(std::env::var_os(PATH_KEY))
    }

    /// [`effective_path`](Self::effective_path) with the inherited `PATH` passed in, so
    /// tests need not touch the process environment.
    fn effective_path_with(&self, inherited: Option<OsString>) -> OsString {
        let path_key = OsStr::new(PATH_KEY);
        let base = if self.removes(path_key) {
            OsString::new()
        } else if let Some(set) = self.last_set(path_key) {
            set.to_os_string()
        } else if self.inherit {
            inherited.unwrap_or_default()
        } else {
            OsString::new()
        };
        // Prepend in reverse so the first entry ends up first.
        self.path_prepend
            .iter()
            .rev()
            .fold(base, |path, dir| prepend_path(dir, &path))
    }

    /// Write the policy onto `target`: clear (if `!inherit`) → set → remove → `PATH`
    /// prepend.
    ///
    /// `PATH` is written only when `path_prepend` is non-empty or `set` holds it; an
    /// untouched `PATH` stays whatever the target inherits.
    pub fn apply<T: EnvTarget + ?Sized>(&self, target: &mut T) {
        self.apply_over(target, |_| {});
    }

    /// [`apply`](Self::apply) with `base` run right after the clear and before `set`: the
    /// spawn's own defaults (a PTY session's terminal identity). They survive
    /// `inherit: false`, and the policy's `set`/`remove` still override them.
    pub(crate) fn apply_over<T: EnvTarget + ?Sized>(
        &self,
        target: &mut T,
        base: impl FnOnce(&mut T),
    ) {
        if !self.inherit {
            target.clear_env();
        }
        base(target);
        for (key, val) in &self.set {
            target.set_env(key, val);
        }
        for key in &self.remove {
            target.remove_env(key);
        }
        if !self.path_prepend.is_empty() {
            target.set_env(OsStr::new(PATH_KEY), &self.effective_path());
        }
    }

    /// The value the child gets for `key` from `set` (the last duplicate), unless removed.
    fn set_value(&self, key: &OsStr) -> Option<&OsStr> {
        if self.removes(key) {
            return None;
        }
        self.last_set(key)
    }

    /// Snapshot a non-PATH launch setting before spawn, after the profile merge.
    #[cfg(all(test, unix, feature = "agent"))]
    pub(crate) fn launch_value(&self, key: &OsStr) -> Option<OsString> {
        if self.removes(key) {
            return None;
        }
        self.last_set(key)
            .map(OsStr::to_owned)
            .or_else(|| self.inherit.then(|| std::env::var_os(key)).flatten())
    }

    /// The last `set` value for `key`, ignoring `remove`.
    fn last_set(&self, key: &OsStr) -> Option<&OsStr> {
        self.set
            .iter()
            .rev()
            .find(|(k, _)| same_key(k, key))
            .map(|(_, v)| v.as_os_str())
    }

    /// Whether `remove` lists `key`.
    pub(crate) fn removes(&self, key: &OsStr) -> bool {
        self.remove.iter().any(|k| same_key(k, key))
    }
}

/// Environment keys are case-insensitive on Windows.
fn same_key(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(windows) {
        a.as_encoded_bytes()
            .eq_ignore_ascii_case(b.as_encoded_bytes())
    } else {
        a == b
    }
}

/// Prepend `dir` to a `PATH` value with the platform separator (`;` on Windows, `:`
/// elsewhere); an empty `existing` gives `dir` alone, with no trailing separator.
///
/// `OsStr` in, `OsString` out: a `PATH` (and the directory) need not be UTF-8, and the
/// result is both handed to the child and parsed by [`crate::resolve`] — a lossy
/// round-trip would make every launcher unresolvable.
pub fn prepend_path(dir: &Path, existing: &OsStr) -> OsString {
    let mut out = dir.as_os_str().to_os_string();
    if !existing.is_empty() {
        out.push(if cfg!(windows) { ";" } else { ":" });
        out.push(existing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Command;

    /// A `Command`'s own env changes: `Some` = set, `None` = removed.
    fn envs(cmd: &Command) -> HashMap<OsString, Option<OsString>> {
        cmd.get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
            .collect()
    }

    fn sep() -> &'static str {
        if cfg!(windows) { ";" } else { ":" }
    }

    #[test]
    fn prepend_path_uses_platform_separator() {
        let out = prepend_path(Path::new("/opt/flowbin"), OsStr::new("/usr/bin:/bin"));
        assert_eq!(
            out,
            OsString::from(format!("/opt/flowbin{}/usr/bin:/bin", sep()))
        );
    }

    #[test]
    fn prepend_path_handles_empty_existing() {
        assert_eq!(
            prepend_path(Path::new("/x"), OsStr::new("")),
            OsString::from("/x")
        );
    }

    /// A `PATH` that is not valid UTF-8 must survive intact — a lossy conversion would
    /// leave resolution with the prepended dir alone.
    #[cfg(unix)]
    #[test]
    fn prepend_path_keeps_a_non_utf8_path() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let existing = OsString::from_vec(b"/usr/bin:/opt/\xff\xfebin".to_vec());
        let out = prepend_path(Path::new("/opt/flowbin"), &existing);
        assert_eq!(
            out.as_bytes(),
            b"/opt/flowbin:/usr/bin:/opt/\xff\xfebin",
            "the invalid bytes must be preserved verbatim"
        );
    }

    #[test]
    fn default_inherits_and_changes_nothing() {
        let policy = EnvPolicy::default();
        assert!(policy.inherit);
        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert!(envs(&cmd).is_empty(), "no PATH write without a prepend");
    }

    #[test]
    fn set_then_remove_of_the_same_key_removes_it() {
        let mut policy = EnvPolicy::default();
        // Call order does not matter: removal always runs after every set.
        policy
            .remove("TOKEN")
            .set("TOKEN", "secret")
            .set("KEEP", "1");
        assert!(!policy.sets(OsStr::new("TOKEN")));
        assert!(policy.sets(OsStr::new("KEEP")));

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        let envs = envs(&cmd);
        assert_eq!(envs.get(OsStr::new("TOKEN")), Some(&None));
        assert_eq!(envs.get(OsStr::new("KEEP")), Some(&Some("1".into())));

        let mut builder = portable_pty::CommandBuilder::new("true");
        policy.apply(&mut builder);
        assert_eq!(builder.get_env("TOKEN"), None);
        assert_eq!(builder.get_env("KEEP"), Some(OsStr::new("1")));
    }

    #[test]
    fn a_later_duplicate_wins() {
        let mut policy = EnvPolicy::default();
        policy.set("A", "first").set("A", "second");

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert_eq!(
            envs(&cmd).get(OsStr::new("A")),
            Some(&Some("second".into()))
        );

        let mut builder = portable_pty::CommandBuilder::new("true");
        policy.apply(&mut builder);
        assert_eq!(builder.get_env("A"), Some(OsStr::new("second")));
    }

    /// `inherit: false` starts from an empty environment. `Command::get_envs` does not
    /// report a clear, so the builder's view (which starts from the process env) is the
    /// check that the inherited variables are gone.
    #[test]
    fn inherit_false_clears_the_inherited_env() {
        let policy = EnvPolicy {
            set: vec![("ONLY".into(), "this".into())],
            inherit: false,
            ..EnvPolicy::default()
        };

        let mut builder = portable_pty::CommandBuilder::new("true");
        assert!(
            builder.get_env(PATH_KEY).is_some(),
            "a fresh builder inherits PATH"
        );
        policy.apply(&mut builder);
        assert_eq!(builder.get_env(PATH_KEY), None);
        assert_eq!(builder.get_env("ONLY"), Some(OsStr::new("this")));
        assert_eq!(builder.iter_full_env_as_str().count(), 1);

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        let envs = envs(&cmd);
        assert_eq!(envs.len(), 1, "only the set key is listed: {envs:?}");
        assert_eq!(envs.get(OsStr::new("ONLY")), Some(&Some("this".into())));
    }

    #[test]
    fn path_prepend_goes_in_front_of_the_inherited_path() {
        let policy = EnvPolicy {
            path_prepend: vec!["/a".into(), "/b".into()],
            ..EnvPolicy::default()
        };
        let s = sep();
        assert_eq!(
            policy.effective_path_with(Some("/usr/bin".into())),
            OsString::from(format!("/a{s}/b{s}/usr/bin"))
        );
        assert_eq!(
            policy.effective_path_with(None),
            OsString::from(format!("/a{s}/b"))
        );

        let expected = policy.effective_path();
        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert_eq!(
            envs(&cmd).get(OsStr::new(PATH_KEY)),
            Some(&Some(expected.clone()))
        );
        assert!(expected.to_string_lossy().starts_with(&format!("/a{s}/b")));

        let mut builder = portable_pty::CommandBuilder::new("true");
        policy.apply(&mut builder);
        assert_eq!(builder.get_env(PATH_KEY), Some(expected.as_os_str()));
    }

    #[test]
    fn path_prepend_goes_in_front_of_a_set_path() {
        let mut policy = EnvPolicy::default();
        policy.set(PATH_KEY, "/set/bin");
        policy.path_prepend.push("/a".into());
        let want = OsString::from(format!("/a{}/set/bin", sep()));

        assert_eq!(
            policy.effective_path_with(Some("/inherited".into())),
            want,
            "a set PATH replaces the inherited one"
        );
        assert_eq!(policy.effective_path(), want);

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert_eq!(
            envs(&cmd).get(OsStr::new(PATH_KEY)),
            Some(&Some(want.clone()))
        );

        let mut builder = portable_pty::CommandBuilder::new("true");
        policy.apply(&mut builder);
        assert_eq!(builder.get_env(PATH_KEY), Some(want.as_os_str()));
    }

    #[test]
    fn a_set_path_without_prepend_is_applied_as_is() {
        let mut policy = EnvPolicy::default();
        policy.set(PATH_KEY, "/only/bin");
        assert_eq!(policy.effective_path(), OsString::from("/only/bin"));

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert_eq!(
            envs(&cmd).get(OsStr::new(PATH_KEY)),
            Some(&Some("/only/bin".into()))
        );
    }

    #[test]
    fn path_prepend_without_inherit_starts_empty() {
        let policy = EnvPolicy {
            path_prepend: vec!["/a".into()],
            inherit: false,
            ..EnvPolicy::default()
        };
        assert_eq!(
            policy.effective_path_with(Some("/usr/bin".into())),
            OsString::from("/a")
        );

        let mut builder = portable_pty::CommandBuilder::new("true");
        policy.apply(&mut builder);
        assert_eq!(builder.get_env(PATH_KEY), Some(OsStr::new("/a")));
    }

    #[test]
    fn a_removed_path_leaves_only_the_prepend() {
        let mut policy = EnvPolicy::default();
        policy.set(PATH_KEY, "/set/bin").remove(PATH_KEY);
        policy.path_prepend.push("/a".into());
        assert_eq!(
            policy.effective_path_with(Some("/usr/bin".into())),
            OsString::from("/a")
        );

        let mut cmd = Command::new("true");
        policy.apply(&mut cmd);
        assert_eq!(
            envs(&cmd).get(OsStr::new(PATH_KEY)),
            Some(&Some("/a".into()))
        );
    }

    /// An inherited `PATH` that is not UTF-8 survives into the effective value.
    #[cfg(unix)]
    #[test]
    fn effective_path_keeps_a_non_utf8_inherited_path() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let inherited = OsString::from_vec(b"/usr/bin:/opt/\xff\xfebin".to_vec());
        let policy = EnvPolicy {
            path_prepend: vec!["/opt/flowbin".into()],
            ..EnvPolicy::default()
        };
        assert_eq!(
            policy
                .effective_path_with(Some(inherited.clone()))
                .as_bytes(),
            b"/opt/flowbin:/usr/bin:/opt/\xff\xfebin"
        );
        assert_eq!(
            EnvPolicy::default().effective_path_with(Some(inherited.clone())),
            inherited,
            "no prepend: the inherited value as is"
        );
    }

    #[test]
    fn keys_compare_case_insensitively_only_on_windows() {
        let mut policy = EnvPolicy::default();
        policy.set("Path", "/x").remove("token");
        assert_eq!(policy.sets(OsStr::new(PATH_KEY)), cfg!(windows));
        policy.set("TOKEN", "v");
        assert_eq!(policy.sets(OsStr::new("TOKEN")), !cfg!(windows));
    }

    /// The trait is object safe, so a host can apply a policy through `dyn EnvTarget`.
    #[test]
    fn apply_works_through_dyn_env_target() {
        let mut policy = EnvPolicy::default();
        policy.set("A", "1");
        let mut cmd = Command::new("true");
        let target: &mut dyn EnvTarget = &mut cmd;
        policy.apply(target);
        assert_eq!(envs(&cmd).get(OsStr::new("A")), Some(&Some("1".into())));
    }

    /// `Debug` names the set keys but never their values (credentials end up there).
    #[test]
    fn debug_never_prints_set_values() {
        let mut policy = EnvPolicy::default();
        policy.set("API_TOKEN", "s3cr3t-value");
        let shown = format!("{policy:?}");
        assert!(shown.contains("API_TOKEN"), "{shown}");
        assert!(!shown.contains("s3cr3t-value"), "{shown}");
    }

    /// The base layer lands after the clear and under the policy's own sets/removes.
    #[test]
    fn apply_over_puts_the_base_under_the_policy() {
        let mut policy = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        policy.set("WINS", "policy").remove("GONE");
        let mut builder = portable_pty::CommandBuilder::new("true");
        builder.env("INHERITED", "x");
        policy.apply_over(&mut builder, |b| {
            b.set_env(OsStr::new("BASE"), OsStr::new("1"));
            b.set_env(OsStr::new("WINS"), OsStr::new("base"));
            b.set_env(OsStr::new("GONE"), OsStr::new("base"));
        });
        assert_eq!(
            builder.get_env("INHERITED"),
            None,
            "cleared before the base"
        );
        assert_eq!(builder.get_env("BASE"), Some(OsStr::new("1")));
        assert_eq!(builder.get_env("WINS"), Some(OsStr::new("policy")));
        assert_eq!(builder.get_env("GONE"), None);
    }
}
