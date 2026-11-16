//! The config guard around a live smoke: what the agent CLIs keep in the owner's home,
//! read before the test and compared after it. Read-only.
//!
//! - Files (length, mtime, content hash): claude's, codex's, opencode's and cursor's
//!   config and credential files, and every `*.json` directly in `~/.pi/agent`.
//! - Trust/state dirs, listed recursively (names, sizes, mtimes): `~/.cursor/projects`
//!   (where cursor's `--trust` and its trust dialog land) and `~/.pi/agent` (`trust.json`,
//!   sessions).
//! - cursor's and claude's keychain items (their modification date only): a token refresh
//!   would show — the sandbox does not block it (`securityd` writes the item). The owner's
//!   own Claude Code session refreshing its token shows too: rerun then.
//!
//! Not listed: `~/.claude` and `~/.codex` — the owner's own sessions (the Claude Code or
//! codex process driving these tests, an editor's codex) write there all the time. The
//! sandbox denies the smokes every write under the home; this guard checks that the
//! records that matter did not change.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Guarded files, relative to the home.
const FILES: &[&str] = &[
    ".claude.json",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".codex/config.toml",
    ".codex/auth.json",
    ".config/opencode/opencode.json",
    ".config/opencode/opencode.jsonc",
    ".local/share/opencode/auth.json",
    ".cursor/cli-config.json",
];

/// Trust/state dirs listed recursively, relative to the home.
const STATE_DIRS: &[&str] = &[".cursor/projects", ".pi/agent"];

/// Keychain services whose items' modification date is compared.
const KEYCHAIN: &[&str] = &[
    "cursor-access-token",
    "cursor-refresh-token",
    "Claude Code-credentials",
];

/// What one file or listed entry looked like: `None` when absent.
type Stamp = Option<(u64, u128, u64)>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    files: BTreeMap<String, Stamp>,
    dirs: BTreeMap<String, BTreeMap<String, (u64, u128)>>,
    keychain: BTreeMap<String, Option<String>>,
}

/// A snapshot taken when the guard was armed.
pub struct Guard {
    home: PathBuf,
    before: Snapshot,
}

impl Guard {
    /// Reads everything guarded under `home`.
    pub fn arm(home: &Path) -> Guard {
        Guard {
            home: home.to_path_buf(),
            before: snapshot(home),
        }
    }

    /// One line on what is guarded (for the test output).
    pub fn summary(&self) -> String {
        let files = self.before.files.values().filter(|s| s.is_some()).count();
        let dirs: Vec<String> = self
            .before
            .dirs
            .iter()
            .map(|(d, l)| format!("~/{d}: {} entries", l.len()))
            .collect();
        let keys = self
            .before
            .keychain
            .values()
            .filter(|s| s.is_some())
            .count();
        format!(
            "{files} files hashed, {}, {keys} keychain items",
            dirs.join(", ")
        )
    }

    /// Every change since [`arm`](Self::arm), by name; empty when nothing changed.
    pub fn changes(&self) -> Vec<String> {
        let after = snapshot(&self.home);
        let mut out = Vec::new();
        let names = self.before.files.keys().chain(after.files.keys());
        let mut seen = std::collections::BTreeSet::new();
        for name in names {
            if seen.insert(name) && self.before.files.get(name) != after.files.get(name) {
                out.push(format!("file changed: ~/{name}"));
            }
        }
        for (dir, before) in &self.before.dirs {
            let empty = BTreeMap::new();
            let now = after.dirs.get(dir).unwrap_or(&empty);
            for (name, stamp) in before {
                match now.get(name) {
                    None => out.push(format!("removed: ~/{dir}/{name}")),
                    Some(s) if s != stamp => out.push(format!("changed: ~/{dir}/{name}")),
                    Some(_) => {}
                }
            }
            for name in now.keys().filter(|n| !before.contains_key(*n)) {
                out.push(format!("added: ~/{dir}/{name}"));
            }
        }
        for (service, mdat) in &self.before.keychain {
            if after.keychain.get(service) != Some(mdat) {
                out.push(format!("keychain item changed: {service}"));
            }
        }
        out
    }
}

fn snapshot(home: &Path) -> Snapshot {
    let mut files: Vec<String> = FILES.iter().map(|f| f.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(home.join(".pi/agent")) {
        let mut pi: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json"))
            .map(|n| format!(".pi/agent/{n}"))
            .collect();
        pi.sort();
        files.extend(pi);
    }
    let files = files
        .into_iter()
        .map(|rel| {
            let stamp = file_stamp(&home.join(&rel));
            (rel, stamp)
        })
        .collect();
    let dirs = STATE_DIRS
        .iter()
        .map(|rel| {
            let root = home.join(rel);
            let mut listing = BTreeMap::new();
            list(&root, &root, &mut listing);
            (rel.to_string(), listing)
        })
        .collect();
    let keychain = KEYCHAIN
        .iter()
        .map(|service| (service.to_string(), keychain_mdat(service)))
        .collect();
    Snapshot {
        files,
        dirs,
        keychain,
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

fn file_stamp(path: &Path) -> Stamp {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some((meta.len(), mtime_ns(&meta), hasher.finish()))
}

/// Every entry under `dir` (symlinks not followed): path relative to `root` → (size, mtime).
fn list(root: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, u128)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = if meta.is_dir() { 0 } else { meta.len() };
        out.insert(rel, (size, mtime_ns(&meta)));
        if meta.is_dir() {
            list(root, &path, out);
        }
    }
}

/// The `mdat` attribute line of a generic keychain item (`security` without `-w` reads
/// attributes only, never the secret), or `None` when there is no such item.
fn keychain_mdat(service: &str) -> Option<String> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("\"mdat\""))
        .map(|l| l.trim().to_string())
}
