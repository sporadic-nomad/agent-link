//! [`LiveEnv`]: where a live smoke runs one real agent CLI so that it leaves **no record in
//! the owner's home** (the owner's rule for these tests).
//!
//! - The shell — and so every agent typed into it, and everything those start — runs
//!   under `/usr/bin/sandbox-exec` with a profile that denies writes anywhere under the
//!   home, and under the target of every symlink in the home's dotfile dirs that leads out
//!   of it. That covers each CLI's config and trust/state dir (`~/.claude*`, `~/.codex`,
//!   `~/.pi/agent`, `~/.cursor/projects`, opencode's dirs). A denied write fails inside
//!   the CLI. The sandbox blocks the process's own file writes only: what a system service
//!   writes on its behalf (a Keychain item, a preference through `cfprefsd`) is not
//!   blocked — the [`Guard`] watches the CLIs' keychain items.
//! - claude, codex, pi and opencode run **isolated** in a mode-700 scratch dir: claude
//!   `CLAUDE_CONFIG_DIR` seeded with copies of `~/.claude.json` (minus MCP servers) and
//!   `~/.claude/settings.json` (minus hooks and the status line); codex `CODEX_HOME` with
//!   the model and notice settings of `~/.codex/config.toml` and a dummy API key (codex
//!   cannot start on a read-only home: it writes its state database at startup); pi
//!   `PI_CODING_AGENT_DIR` with a copy of `settings.json` (minus packages, provider set to
//!   OpenAI) and a dummy `OPENAI_API_KEY`; opencode `XDG_*_HOME`. Credential files are
//!   never copied: isolated claude runs signed out, codex's and pi's turns fail on the
//!   dummy key, opencode answers with its anonymous free models (no account).
//! - cursor has no config-dir override: it uses the **real home**, read-only through the
//!   sandbox.
//! - codex also gets `-c history.persistence="none"` and no MCP servers, plugins, hooks
//!   or notify command.
//! - Dialogs are not answered (`steer_dialogs: false`) except for claude, whose answers
//!   land in the scratch copies; pre-trust flags are used instead.
//! - A [`Guard`] reads the guarded files and state dirs before anything runs; the test
//!   compares after the teardown.
//! - The agent-session variables of a Claude Code session driving the tests
//!   (`CLAUDECODE`, `CLAUDE_*`) and every credential-looking variable (provider API keys,
//!   `*_TOKEN`, `*_SECRET`, …: [`is_credential_var`]) are removed — from the session's env
//!   and, through `/usr/bin/env -u`, from the launch line (the login shell's rc files may
//!   export them again) — so an isolated agent cannot sign in with the owner's account. Auto-updates off (claude
//!   `DISABLE_AUTOUPDATER`, opencode `OPENCODE_DISABLE_AUTOUPDATE` and `autoupdate: false`;
//!   codex's update check is off in its built-in launch line): an update could rewrite an
//!   install outside the home.
//!
//! macOS only: elsewhere (no `sandbox-exec`) [`LiveEnv::new`] skips the test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_link::agent::{
    AgentProfile, Dialog, LaunchPlan, LoginCheck, Permissions, PreTrust, ProfileRegistry,
    RpcLaunch, ScreenState, ShellKind, SignalHints, TrustPolicy,
};
use agent_link::env::EnvPolicy;
use agent_link::shell::{ShellSpec, login_shell, shell_base};
use agent_link::{PtySize, SpawnMode, SpawnSpec};
use tempfile::TempDir;

use super::guard::Guard;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Per-command limit for `--version` and the sign-in check.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// codex overrides: no message history, and no MCP servers, plugins, hooks or notify
/// command (they would start the owner's programs).
const CODEX_ARGS: &[&str] = &[
    "-c",
    "history.persistence=\"none\"",
    "-c",
    "mcp_servers={}",
    "-c",
    "plugins={}",
    "-c",
    "features.hooks=false",
    "-c",
    "notify=[]",
];

/// One agent's guarded environment. Dropping it deletes the scratch dir and the cwd.
pub struct LiveEnv {
    /// The built-in profile's id.
    pub id: &'static str,
    /// The built-in profile.
    pub builtin: Arc<dyn AgentProfile>,
    /// The launcher found on `PATH`.
    pub launcher: PathBuf,
    /// `<launcher> --version`, first line.
    pub version: String,
    /// "isolated" or "real home (read-only)".
    pub home_mode: &'static str,
    /// The trust policy the smoke launches with.
    pub trust: TrustPolicy,
    /// The launch cwd (fresh, empty).
    pub cwd: PathBuf,
    env: EnvPolicy,
    extra_args: Vec<String>,
    /// Credential variables the launch line unsets ([`credential_names`]).
    unset: Vec<String>,
    shell: PathBuf,
    sandbox: PathBuf,
    guard: Guard,
    _cwd: TempDir,
    _scratch: TempDir,
}

impl LiveEnv {
    /// The environment for built-in `id`, the cwd a fresh temp dir (under `cwd_parent` when
    /// given). `None` (with a message) when the test must skip: no `sandbox-exec`, or no
    /// launcher on `PATH`.
    pub fn new(id: &'static str, cwd_parent: Option<&Path>) -> Option<LiveEnv> {
        if !Path::new(SANDBOX_EXEC).exists() {
            println!(
                "SKIP {id}: no {SANDBOX_EXEC} (the live smokes run only under the macOS sandbox)"
            );
            return None;
        }
        let builtin = ProfileRegistry::builtin()
            .get(id)
            .unwrap_or_else(|| panic!("no built-in profile {id}"));
        let Some(launcher) = agent_link::resolve::find_on_path(builtin.launchers()) else {
            println!("SKIP {id}: none of {:?} on PATH", builtin.launchers());
            return None;
        };
        let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME is set"))
            .expect("the home dir resolves");
        let home_str = home.to_str().expect("a UTF-8 home path");
        assert!(
            !home_str.contains(['"', '\\', '\'']),
            "a home path with quotes cannot go into the sandbox profile: {home_str}"
        );

        let scratch = tempfile::Builder::new()
            .prefix("agent-link-live")
            .tempdir()
            .expect("scratch dir");
        set_mode(scratch.path(), 0o700);
        let scratch_dir = std::fs::canonicalize(scratch.path()).unwrap();
        let cwd_dir = match cwd_parent {
            Some(parent) => tempfile::Builder::new()
                .prefix("apt-live")
                .tempdir_in(parent),
            None => tempfile::Builder::new().prefix("apt-live").tempdir(),
        }
        .expect("cwd");
        for dir in [
            &scratch_dir,
            &std::fs::canonicalize(cwd_dir.path()).unwrap(),
        ] {
            assert!(
                !dir.starts_with(&home),
                "{} is under the home, where the sandbox denies every write",
                dir.display()
            );
        }

        // Armed before any CLI runs (the version and sign-in checks too).
        let guard = Guard::arm(&home);

        let sandbox = scratch_dir.join("live.sb");
        let cwd_real = std::fs::canonicalize(cwd_dir.path()).unwrap();
        let outside = symlink_targets(&home, &[&scratch_dir, &cwd_real]);
        std::fs::write(&sandbox, sandbox_profile(home_str, &outside)).unwrap();
        assert_denies_home_writes(&sandbox, &home, &scratch_dir);
        let shell = shell_wrapper(&scratch_dir, &sandbox);

        let mut env = base_env(id, &scratch_dir);
        let mut extra_args = Vec::new();
        let real_home_trust = TrustPolicy {
            steer_dialogs: false,
            ..TrustPolicy::default()
        };
        let (home_mode, trust) = match id {
            "claude" => {
                let dir = scratch_dir.join("claude");
                std::fs::create_dir(&dir).unwrap();
                copy_json(&home.join(".claude.json"), &dir.join(".claude.json"), |v| {
                    remove_key(v, "mcpServers");
                    if let Some(projects) = v.get_mut("projects").and_then(|p| p.as_object_mut()) {
                        for project in projects.values_mut() {
                            remove_key(project, "mcpServers");
                        }
                    }
                });
                copy_json(
                    &home.join(".claude/settings.json"),
                    &dir.join("settings.json"),
                    |v| {
                        remove_key(v, "hooks");
                        remove_key(v, "statusLine");
                    },
                );
                env.set("CLAUDE_CONFIG_DIR", &dir);
                ("isolated", TrustPolicy::unattended())
            }
            "pi" => {
                let dir = scratch_dir.join("pi");
                std::fs::create_dir(&dir).unwrap();
                // The owner's default provider may sign in through OAuth, whose files are not
                // copied: with no model pi keeps a submitted text in its composer at times.
                // OpenAI's API with a dummy key ([`dummy_key`]) fails every turn on the key.
                copy_json(
                    &home.join(".pi/agent/settings.json"),
                    &dir.join("settings.json"),
                    |v| {
                        remove_key(v, "packages");
                        if let Some(map) = v.as_object_mut() {
                            map.insert("defaultProvider".into(), "openai".into());
                            map.insert("defaultModel".into(), "gpt-4o-mini".into());
                            map.insert("defaultThinkingLevel".into(), "off".into());
                        }
                    },
                );
                env.set("PI_CODING_AGENT_DIR", &dir);
                extra_args.push("--no-session".to_string());
                ("isolated", real_home_trust)
            }
            "opencode" => {
                for (var, sub) in [
                    ("XDG_CONFIG_HOME", "config"),
                    ("XDG_DATA_HOME", "data"),
                    ("XDG_STATE_HOME", "state"),
                    ("XDG_CACHE_HOME", "cache"),
                ] {
                    let dir = scratch_dir.join("xdg").join(sub);
                    std::fs::create_dir_all(&dir).unwrap();
                    env.set(var, dir);
                }
                let config = scratch_dir.join("xdg/config/opencode");
                std::fs::create_dir_all(&config).unwrap();
                std::fs::write(config.join("opencode.json"), r#"{"autoupdate": false}"#).unwrap();
                ("isolated", real_home_trust)
            }
            "codex" => {
                let dir = scratch_dir.join("codex");
                std::fs::create_dir(&dir).unwrap();
                copy_codex_config(&home.join(".codex/config.toml"), &dir.join("config.toml"));
                // Not a credential: lets codex start at its composer (signed out it opens
                // its login screen); every turn fails with 401.
                std::fs::write(
                    dir.join("auth.json"),
                    r#"{"OPENAI_API_KEY":"sk-dummy-agent-link-live"}"#,
                )
                .unwrap();
                set_mode(&dir.join("auth.json"), 0o600);
                env.set("CODEX_HOME", &dir);
                extra_args.extend(CODEX_ARGS.iter().map(|a| a.to_string()));
                ("isolated (dummy API key)", real_home_trust)
            }
            "cursor" => ("real home (read-only)", real_home_trust),
            other => panic!("no live environment for {other}"),
        };

        let cwd = cwd_dir.path().to_path_buf();
        let mut live = LiveEnv {
            id,
            builtin,
            launcher,
            version: String::new(),
            home_mode,
            trust,
            cwd,
            env,
            extra_args,
            unset: credential_names()
                .into_iter()
                .filter(|key| dummy_key(id).is_none_or(|(dummy, _)| dummy != key))
                .collect(),
            shell,
            sandbox,
            guard,
            _cwd: cwd_dir,
            _scratch: scratch,
        };
        live.version = live
            .run(&live.launcher, &["--version"])
            .map(|out| first_line(&out))
            .unwrap_or_else(|| "unknown (--version timed out)".into());
        Some(live)
    }

    /// The built-in profile with this environment's extra launch args (and `more`).
    pub fn profile(&self, more: &[String]) -> Arc<dyn AgentProfile> {
        let mut extra = self.extra_args.clone();
        extra.extend(more.iter().cloned());
        Arc::new(LiveProfile {
            inner: Arc::clone(&self.builtin),
            extra,
            unset: self.unset.clone(),
        })
    }

    /// The sandboxed login shell in the cwd, 40x120, with this environment; identity
    /// `Honest` and query answering on (the defaults).
    pub fn spawn_spec(&self) -> SpawnSpec {
        let mut spec = SpawnSpec::shell(
            &self.cwd,
            PtySize {
                rows: 40,
                cols: 120,
            },
        );
        spec.mode = SpawnMode::Shell(ShellSpec {
            program: Some(self.shell.clone()),
            login: true,
        });
        spec.env = self.env.clone();
        spec
    }

    /// `program args…` under the sandbox with this environment, in the cwd; `None` on a
    /// timeout or a spawn error.
    pub fn run(&self, program: &Path, args: &[&str]) -> Option<Output> {
        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-f")
            .arg(&self.sandbox)
            .arg(program)
            .args(args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null());
        self.env.apply(&mut cmd);
        agent_link::proc::output_with_timeout(cmd, COMMAND_TIMEOUT)
            .ok()
            .flatten()
    }

    /// The profile's sign-in check, sandboxed: `Some(true)` signed in, `Some(false)` signed
    /// out, `None` unknown (no check, or an answer it cannot read).
    pub fn signed_in(&self) -> Option<bool> {
        let check = self.builtin.login_check()?;
        let out = self.run(&self.launcher, check.args)?;
        if out.status.success() {
            return Some(true);
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        text.contains(check.signed_out_marker).then_some(false)
    }

    /// What the guard reads (one line).
    pub fn guard_summary(&self) -> String {
        self.guard.summary()
    }

    /// Every change to the guarded files and dirs since [`new`](Self::new).
    pub fn guard_changes(&self) -> Vec<String> {
        self.guard.changes()
    }
}

/// The built-in profile with extra args appended to its launch plan, run through
/// `/usr/bin/env -u <each of unset>` (the login shell's rc files may export credentials
/// again after the session's env dropped them); everything else delegated.
struct LiveProfile {
    inner: Arc<dyn AgentProfile>,
    extra: Vec<String>,
    unset: Vec<String>,
}

impl AgentProfile for LiveProfile {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn dialog(&self, text: &str) -> Option<Dialog> {
        self.inner.dialog(text)
    }
    fn input_ready(&self, screen: &str) -> bool {
        self.inner.input_ready(screen)
    }
    fn classify(&self, screen: &str) -> ScreenState {
        self.inner.classify(screen)
    }
    fn pending_input(&self, screen: &str, sent: &str) -> Option<bool> {
        self.inner.pending_input(screen, sent)
    }
    fn launchers(&self) -> &'static [&'static str] {
        self.inner.launchers()
    }
    fn launch(
        &self,
        perms: &Permissions,
        trust: &TrustPolicy,
        cwd: &Path,
        shell: ShellKind,
    ) -> LaunchPlan {
        let mut plan = self.inner.launch(perms, trust, cwd, shell);
        plan.args.extend(self.extra.iter().cloned());
        let mut args: Vec<String> = self
            .unset
            .iter()
            .flat_map(|key| ["-u".to_string(), key.clone()])
            .collect();
        args.push(std::mem::replace(&mut plan.program, "/usr/bin/env".into()));
        args.append(&mut plan.args);
        plan.args = args;
        plan
    }
    fn reset_command(&self) -> Option<&'static str> {
        self.inner.reset_command()
    }
    fn signals(&self) -> SignalHints {
        self.inner.signals()
    }
    fn login_check(&self) -> Option<LoginCheck> {
        self.inner.login_check()
    }
    fn rpc_launch(&self, resume: bool, probe: bool) -> Option<RpcLaunch> {
        self.inner.rpc_launch(resume, probe)
    }
}

/// No write anywhere under the home or under `outside` (symlink targets out of it); the
/// `/System/Volumes/Data` firmlink path of each too.
fn sandbox_profile(home: &str, outside: &[PathBuf]) -> String {
    let mut roots = vec![home.to_string()];
    roots.extend(outside.iter().map(|p| p.to_str().unwrap().to_string()));
    let mut deny = String::new();
    for root in &roots {
        deny.push_str(&format!(
            "  (subpath \"{root}\")\n  (subpath \"/System/Volumes/Data{root}\")\n"
        ));
    }
    format!(
        "(version 1)\n(allow default)\n\
         ; agent-link live smokes: leave no record in the owner's home.\n\
         (deny file-write*\n{deny})\n"
    )
}

/// Where the home's dotfile dirs lead out of it: the resolved target of every symlink
/// directly in the home, `~/.config`, `~/.local/share`, `~/.local/state` and `~/.cache`
/// (dotfile managers link CLI config there). A target holding a scratch dir (`keep`) is
/// left out with a note: denying it would deny the test itself.
fn symlink_targets(home: &Path, keep: &[&Path]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["", ".config", ".local/share", ".local/state", ".cache"] {
        let Ok(entries) = std::fs::read_dir(home.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink());
            let Some(target) = is_link.then(|| std::fs::canonicalize(&path).ok()).flatten() else {
                continue;
            };
            if target.starts_with(home) || target == Path::new("/") || out.contains(&target) {
                continue;
            }
            if keep.iter().any(|k| k.starts_with(&target)) {
                println!(
                    "note: {} leads to {}, which holds the test's scratch dir: not denied",
                    path.display(),
                    target.display()
                );
                continue;
            }
            let text = target.to_string_lossy();
            assert!(
                !text.contains(['"', '\\', '\'']),
                "a symlink target with quotes cannot go into the sandbox profile: {text}"
            );
            out.push(target);
        }
    }
    out
}

/// Proves the profile applies and denies a write under the home: under it a write to the
/// scratch dir succeeds (so the command ran — `sandbox-exec` that cannot apply a profile
/// runs nothing), and an append-open of an existing file in the home (which changes
/// nothing even where it is allowed) fails on that file.
fn assert_denies_home_writes(sandbox: &Path, home: &Path, scratch: &Path) {
    let target = [".zshrc", ".profile", ".claude.json"]
        .iter()
        .map(|f| home.join(f))
        .find(|p| p.is_file())
        .expect("an existing file in the home to probe the sandbox with");
    let allowed = scratch.join("sandbox-probe");
    let run = |script: &str, path: &Path| {
        Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(sandbox)
            .args(["/bin/sh", "-c", script, "probe"])
            .arg(path)
            .output()
            .expect("run sandbox-exec")
    };
    let out = run(": >> \"$1\"", &allowed);
    assert!(
        out.status.success() && allowed.is_file(),
        "a write to the scratch dir fails under the sandbox (profile not applied?): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run("[ -f \"$1\" ] && : >> \"$1\"", &target);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success()
            && err.contains("Operation not permitted")
            && err.contains(&*target.to_string_lossy()),
        "the sandbox profile does not deny writes under the home ({}): {err}",
        target.display()
    );
}

/// A script named like the user's login shell (so the session passes it `-l`) that runs
/// that shell under the sandbox.
fn shell_wrapper(scratch: &Path, sandbox: &Path) -> PathBuf {
    let shell = login_shell();
    let dir = scratch.join("bin");
    std::fs::create_dir(&dir).unwrap();
    let wrapper = dir.join(shell_base(&shell));
    let quoted = |p: &str| {
        assert!(!p.contains('\''), "a quote in {p}");
        format!("'{p}'")
    };
    let script = format!(
        "#!/bin/sh\nexec {SANDBOX_EXEC} -f {} {} \"$@\"\n",
        quoted(sandbox.to_str().unwrap()),
        quoted(&shell)
    );
    std::fs::write(&wrapper, script).unwrap();
    set_mode(&wrapper, 0o755);
    wrapper
}

/// Inherited env minus a driving Claude Code session's variables and every credential
/// ([`is_credential_var`]); no auto-updates; shell history into the scratch dir.
fn base_env(id: &str, scratch: &Path) -> EnvPolicy {
    let mut env = EnvPolicy::default();
    let dummy = dummy_key(id);
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        let ours = (id == "claude" && key == "CLAUDE_CONFIG_DIR")
            || dummy.is_some_and(|(dummy, _)| dummy == key);
        if !ours && (key == "CLAUDECODE" || key.starts_with("CLAUDE_") || is_credential_var(key)) {
            env.remove(key);
        }
    }
    if let Some((key, value)) = dummy {
        env.set(key, value);
    }
    env.set("DISABLE_AUTOUPDATER", "1")
        .set("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .set("HISTFILE", scratch.join("shell-history"));
    env
}

/// A dummy API key (not a credential) for an isolated agent that has no model otherwise:
/// pi, whose turns then fail on the key like codex's.
fn dummy_key(id: &str) -> Option<(&'static str, &'static str)> {
    (id == "pi").then_some(("OPENAI_API_KEY", "sk-dummy-agent-link-live"))
}

/// The credential variables of this process's environment ([`is_credential_var`]) plus
/// the common ones a shell rc file may export, sorted.
fn credential_names() -> Vec<String> {
    let mut names: BTreeSet<String> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| is_credential_var(key))
        .collect();
    for key in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "OPENROUTER_API_KEY",
        "GITLAB_TOKEN",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "CURSOR_API_KEY",
    ] {
        names.insert(key.to_string());
    }
    names.into_iter().collect()
}

/// A variable that may sign an agent in with the owner's account: a provider's key (opencode
/// picks providers from the environment — an inherited `GITLAB_TOKEN` made it use GitLab
/// Duo), or anything named like a token, key, secret or password.
fn is_credential_var(key: &str) -> bool {
    const PROVIDERS: &[&str] = &[
        "ANTHROPIC_",
        "OPENAI_",
        "AZURE_",
        "AWS_",
        "GOOGLE_",
        "GEMINI_",
        "VERTEX",
        "GITLAB_",
        "GITHUB_",
        "GH_",
        "OPENROUTER_",
        "MISTRAL_",
        "GROQ_",
        "XAI_",
        "DEEPSEEK_",
        "COHERE_",
        "TOGETHER_",
        "FIREWORKS_",
        "PERPLEXITY_",
        "HF_",
        "HUGGINGFACE_",
        "CURSOR_",
        "OPENCODE_API",
        "COPILOT_",
    ];
    const MARKERS: &[&str] = &[
        "TOKEN",
        "API_KEY",
        "APIKEY",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
    ];
    let upper = key.to_ascii_uppercase();
    PROVIDERS.iter().any(|p| upper.starts_with(p))
        || MARKERS.iter().any(|m| upper.contains(m))
        || upper.ends_with("_KEY")
}

/// Copies the JSON file `from` to `to` with `edit` applied; no file ⇒ nothing written.
fn copy_json(from: &Path, to: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let Ok(text) = std::fs::read_to_string(from) else {
        return;
    };
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not JSON: {e}", from.display()));
    edit(&mut value);
    std::fs::write(to, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    set_mode(to, 0o600);
}

/// The codex settings that keep its one-time notices away: top-level `model` and
/// `model_reasoning_effort`, and the `[notice…]` and `[tui…]` tables. Nothing else — no
/// MCP servers, hooks, notify command, plugins or project paths.
fn copy_codex_config(from: &Path, to: &Path) {
    let Ok(text) = std::fs::read_to_string(from) else {
        return;
    };
    let mut out = String::new();
    let mut top_level = true;
    let mut keep = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            top_level = false;
            let name = trimmed.trim_start_matches('[');
            keep = name.starts_with("notice") || name.starts_with("tui");
        } else if top_level {
            // A continuation line of a multi-line value matches no key: dropped.
            let key = trimmed.split('=').next().unwrap_or("").trim();
            keep = trimmed.contains('=') && ["model", "model_reasoning_effort"].contains(&key);
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    std::fs::write(to, out).unwrap();
    set_mode(to, 0o600);
}

fn remove_key(value: &mut serde_json::Value, key: &str) {
    if let Some(map) = value.as_object_mut() {
        map.remove(key);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn first_line(out: &Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // codex warns first when it cannot write its PATH aliases (denied by the sandbox).
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("WARNING:"))
        .unwrap_or("")
        .to_string()
}

/// The processes below `pid` (children first), with their command names.
pub fn descendants(pid: u32) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        let Ok(children) = Command::new("/usr/bin/pgrep")
            .args(["-P", &parent.to_string()])
            .output()
        else {
            continue;
        };
        for child in String::from_utf8_lossy(&children.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
        {
            out.push((child, command_name(child)));
            queue.push(child);
        }
    }
    out
}

fn command_name(pid: u32) -> String {
    Command::new("/bin/ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Whether `pid` is still a process (a zombie counts until reaped).
pub fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The processes of `pids` still alive after `timeout`.
pub fn survivors(pids: &[(u32, String)], timeout: Duration) -> Vec<(u32, String)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left: Vec<(u32, String)> = pids.iter().filter(|(p, _)| alive(*p)).cloned().collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A trust policy with pre-trust off (dialogs not steered): for the codex control launch.
pub fn no_pre_trust() -> TrustPolicy {
    TrustPolicy {
        pre_trust: PreTrust::Off,
        steer_dialogs: false,
    }
}
