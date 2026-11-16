//! `PATH` launcher resolution — the one place a bare CLI name becomes an absolute
//! executable path.
//!
//! On Windows `Command::new("claude")` appends only `.exe` and ignores `PATHEXT`, so an
//! npm `.cmd` shim never resolves, while `cmd /C claude --version` does honour it. A
//! host that checks "is it installed" one way and spawns the other reports a CLI as
//! installed and then fails to start it. Resolve once here, then spawn the absolute
//! path this returns, and the two cannot disagree.
//!
//! **Resolve against the `PATH` the child will get**, not the process one
//! ([`find_in_path_os`]): a host that prepends its own directories to the child's
//! `PATH` must resolve against that value.
//!
//! `PATH` is carried as `OsStr`/`OsString` throughout — a Windows `PATH` is not
//! guaranteed UTF-8, and losing it to `""` would make every launcher unresolvable.
//!
//! [`DEFAULT_PATHEXT`] and [`LAUNCHABLE_EXTS`] are public so a host that mirrors these
//! rules in a script can guard the mirror against drift.
//!
//! ```
//! use agent_link::resolve::find_on_path;
//!
//! // The order of the names is the priority, whatever the PATH order.
//! if let Some(path) = find_on_path(&["cursor-agent", "agent"]) {
//!     println!("launcher: {}", path.display());
//! }
//! ```
//!
//! Deliberately **std-only**, so the Windows branches can be type-checked for a Windows
//! target from a non-Windows host.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// First of `bins` found as an executable on the process `PATH`. `bins` order is
/// the priority, so `idea` beats `idea.sh` regardless of `PATH` order.
pub fn find_on_path(bins: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_path_os(&path, bins)
}

/// [`find_on_path`] against an EXPLICIT `PATH` value — the child's own, when it
/// differs from ours (see the module note).
pub fn find_in_path_os(path: &OsStr, bins: &[&str]) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .collect();
    find_bin_in(&dirs, bins)
}

/// The scan against an explicit directory list (testable).
///
/// Iteration is `bin` → `dir` → extension (`cmd.exe`'s own order, pinned by tests).
/// The candidate list is built ONCE PER `bin`, not per (bin, dir): on Windows it
/// parses `PATHEXT` and allocates, and a real Windows `PATH` is dozens of dirs
/// scanned for each of several launchers, often in parallel.
pub(crate) fn find_bin_in(dirs: &[PathBuf], bins: &[&str]) -> Option<PathBuf> {
    for bin in bins {
        let names = candidate_names(bin);
        for dir in dirs {
            for name in &names {
                let candidate = dir.join(name);
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

// The `PATHEXT` core below is `cfg(any(windows, test))`, not `cfg(windows)`: it is
// pure, platform-independent string handling, and its tests are the only executable
// check on it when development happens off Windows. Only the items that touch the
// platform (`candidate_names`, which reads the process `PATHEXT`, and the
// `find_bin_in` tests, which need Windows filename semantics) are `cfg(windows)`.
// The two constants are unconditional: a host's drift test reads them from a normal
// (non-test) build of this crate.

/// Windows' documented `PATHEXT` default, used when the variable is unset or names
/// no launchable extension. Same set as [`LAUNCHABLE_EXTS`], in Windows' casing.
pub const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The only extensions a resolved candidate can actually be EXECUTED as: a `.com` /
/// `.exe` image, or a `.bat` / `.cmd` that std hands to `cmd.exe`. `PATHEXT` is
/// INTERSECTED with this set.
///
/// Why the intersection is not paranoia: `npm i -g` writes `claude.ps1` beside
/// `claude.cmd`, and adding `.PS1` to `PATHEXT` is a common PowerShell tweak — with
/// it sorted before `.CMD` the resolver would return the `.ps1`, which
/// `std::process::Command` cannot launch and which `cmd /c` hands to the file
/// association (an editor window, or nothing). Not a rare edge either: the `PATHEXT`
/// Windows itself sets goes well past cmd.exe's built-in default, listing the WSH /
/// MMC entries (`.VBS`, `.VBE`, `.JS`, `.JSE`, `.WSF`, `.WSH`, `.MSC`) — none of
/// which is a program image.
pub const LAUNCHABLE_EXTS: &[&str] = &[".com", ".exe", ".bat", ".cmd"];

/// Filenames to try for a launcher name on Windows: `PATHEXT`-driven (intersected
/// with [`LAUNCHABLE_EXTS`]), in `PATHEXT` order — the search `CreateProcess` will
/// not do for us (`Command::new` appends only `.exe`).
#[cfg(windows)]
fn candidate_names(bin: &str) -> Vec<String> {
    candidate_names_for(bin, std::env::var("PATHEXT").ok().as_deref())
}

/// [`candidate_names`] against an EXPLICIT `PATHEXT` (see [`launchable_exts`] for
/// how it is parsed). Threaded rather than read from the env so tests need not
/// mutate process-global state.
///
/// **Extension-bearing only.** `npm i -g` writes three siblings into
/// `%APPDATA%\npm`: `claude` (a `#!/bin/sh` shim for Git Bash), `claude.cmd` and
/// `claude.ps1`. The bare name is neither a PE nor a batch file, so emitting it as
/// a candidate would resolve the one file `CreateProcess` cannot launch — and
/// would shadow the `.cmd` beside it, since candidates are tried innermost per
/// directory. (The `.ps1` is excluded by [`LAUNCHABLE_EXTS`] for the same reason,
/// whatever `PATHEXT` says.)
///
/// Consequence, accepted deliberately: a `bin` carrying an extension that is not an
/// accepted one (`idea.sh`) yields no candidate at all. That is correct — it is
/// unlaunchable on Windows.
#[cfg(any(windows, test))]
fn candidate_names_for(bin: &str, pathext: Option<&str>) -> Vec<String> {
    let exts = launchable_exts(pathext);
    // Windows appends `PATHEXT` only to a name with NO extension; one that carries
    // an extension is taken as given — and is unlaunchable when that extension is
    // not an accepted one. `PATHEXT` is conventionally UPPERCASE and filenames are
    // not, so the comparison is case-insensitive.
    if let Some(ext) = extension_of(bin) {
        return if exts.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
            vec![bin.to_string()]
        } else {
            Vec::new()
        };
    }
    exts.iter().map(|ext| format!("{bin}{ext}")).collect()
}

/// `pathext`'s entries, normalised and kept only if [`LAUNCHABLE_EXTS`] lists them,
/// in `PATHEXT` order — [`DEFAULT_PATHEXT`]'s when that leaves NOTHING.
///
/// **The fallback is on the PARSED list, not on the raw string**: `";"`, `";;"` and
/// `".PS1"` are all non-empty `PATHEXT` values that yield zero entries, and zero
/// entries means zero candidates — every launcher unresolvable, so every installed
/// CLI would silently read as "not found on PATH". A `PATHEXT` with no launchable extension in
/// it is indistinguishable from a missing one, so it is treated as missing rather
/// than as "launch nothing".
#[cfg(any(windows, test))]
fn launchable_exts(pathext: Option<&str>) -> Vec<String> {
    let exts = parse_pathext(pathext.unwrap_or(DEFAULT_PATHEXT));
    if exts.is_empty() {
        parse_pathext(DEFAULT_PATHEXT)
    } else {
        exts
    }
}

/// Split a `PATHEXT` value into normalised, launchable extensions (may be empty).
#[cfg(any(windows, test))]
fn parse_pathext(pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != ".")
        // A `PATHEXT` entry carries its dot; tolerate one that does not. LOWERCASED:
        // `PATHEXT` is conventionally uppercase while the shims on disk are not
        // (`claude.cmd`, `code.cmd`), and the candidate we build becomes the path we
        // log and spawn. Windows filenames are case-insensitive, so this only
        // matters for how the result READS — except in a case-sensitive NTFS
        // directory, where lowercase is the only casing that could match. It also
        // makes the [`LAUNCHABLE_EXTS`] test below a plain `contains`.
        .map(|e| {
            let e = e.to_ascii_lowercase();
            if e.starts_with('.') {
                e
            } else {
                format!(".{e}")
            }
        })
        .filter(|e| LAUNCHABLE_EXTS.contains(&e.as_str()))
        .collect()
}

/// The trailing `.ext` of the final filename (dot included), or `None` when it
/// has none. Parent-directory dots do not count; a leading filename dot is not
/// an extension. Parse Windows separators even in the pure tests on other hosts.
#[cfg(any(windows, test))]
fn extension_of(bin: &str) -> Option<&str> {
    let bin = bin.rsplit(['/', '\\']).next()?;
    let dot = bin.rfind('.')?;
    (dot > 0).then(|| &bin[dot..])
}

/// Unix launchers carry no extension: the caller's name is the filename.
#[cfg(not(windows))]
fn candidate_names(bin: &str) -> Vec<String> {
    vec![bin.to_string()]
}

/// Is `path` a file we could exec? A directory named `code` on `PATH` is not.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && has_exec_bit(&meta)
}

#[cfg(unix)]
fn has_exec_bit(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Windows has no exec bit — being a file of the right name is the whole test.
#[cfg(not(unix))]
fn has_exec_bit(_meta: &std::fs::Metadata) -> bool {
    true
}

/// TWO KINDS OF TEST HERE, and the `cfg` on each says which:
///   * **Pure string logic** — the `windows_candidates_*` / `windows_falls_back_*`
///     trio drive [`candidate_names_for`]. Nothing platform-specific happens in them,
///     so they carry NO `cfg` and run on every host, so the candidate-ordering rules
///     are asserted without a Windows host.
///   * **Filesystem semantics** — anything going through [`find_bin_in`] resolves
///     real files, so it is `cfg(unix)` or `cfg(windows)`: the `write_exec` layout,
///     the exec bit, and which extension the platform's [`candidate_names`] tries.
#[cfg(test)]
mod tests {
    use super::*;

    /// Write `name` in `dir` as something [`is_executable_file`] accepts on this
    /// platform (exec bit on unix; Windows has none).
    fn write_exec(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    /// The platform's filename for a launcher named `bin`.
    fn exe(bin: &str) -> String {
        if cfg!(windows) {
            format!("{bin}.exe")
        } else {
            bin.to_string()
        }
    }

    #[cfg(unix)]
    #[test]
    fn finds_a_launcher_on_path_by_bins_order_skipping_non_executables() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).expect("mkdir");
        std::fs::create_dir_all(&second).expect("mkdir");

        // A non-executable file of the right name in the first dir must be skipped,
        // not returned.
        std::fs::write(first.join("idea"), b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(first.join("idea"), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        // A directory named like a launcher must not match either.
        std::fs::create_dir(first.join("idea.sh")).expect("mkdir");
        let exec = write_exec(&second, "idea");

        let dirs = vec![first.clone(), second.clone()];
        assert_eq!(find_bin_in(&dirs, &["idea", "idea.sh"]), Some(exec));
        assert_eq!(find_bin_in(&dirs, &["code"]), None);
    }

    /// `bins` order is the priority, not `PATH` order — so the canonical launcher
    /// wins over a fallback name that happens to sit in an earlier `PATH` entry.
    #[cfg(unix)]
    #[test]
    fn bins_order_beats_path_order() {
        let root = tempfile::tempdir().expect("tempdir");
        let early = root.path().join("early");
        let late = root.path().join("late");
        std::fs::create_dir_all(&early).expect("mkdir");
        std::fs::create_dir_all(&late).expect("mkdir");
        write_exec(&early, "idea.sh");
        let wanted = write_exec(&late, "idea");

        assert_eq!(
            find_bin_in(&[early, late], &["idea", "idea.sh"]),
            Some(wanted)
        );
    }

    /// Agent resolution follows the exact PATH value the child receives.
    #[test]
    fn find_in_path_os_resolves_against_the_given_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let installed = root.path().join("bin");
        std::fs::create_dir_all(&installed).expect("mkdir");
        let pi = write_exec(&installed, &exe("pi"));

        let path = std::env::join_paths([installed.as_path()]).expect("join_paths");
        assert_eq!(find_in_path_os(&path, &["pi"]), Some(pi));
        assert_eq!(find_in_path_os(&path, &["claude"]), None);
        // Empty entries are skipped rather than resolving against the cwd.
        assert!(find_in_path_os(OsStr::new(""), &["pi"]).is_none());
    }

    /// The candidate list is `PATHEXT`, in `PATHEXT` order, and NEVER the bare
    /// name — see [`candidate_names_for`] for why the bare name is the bug.
    ///
    /// Pure string logic ⇒ runs on EVERY platform (see the module note above).
    #[test]
    fn windows_candidates_are_pathext_in_order_and_never_the_bare_name() {
        // Unset / blank ⇒ the Windows default, order preserved, extensions lowercased
        // to the casing the shims on disk actually use.
        for pathext in [None, Some(""), Some("   ")] {
            assert_eq!(
                candidate_names_for("claude", pathext),
                vec!["claude.com", "claude.exe", "claude.bat", "claude.cmd"]
            );
        }
        // A custom PATHEXT is honoured, in its own order; a dotless or empty entry
        // is tolerated.
        assert_eq!(
            candidate_names_for("claude", Some(".CMD;.EXE;;BAT")),
            vec!["claude.cmd", "claude.exe", "claude.bat"]
        );
        // An extension already in PATHEXT is taken as given, case-insensitively.
        assert_eq!(candidate_names_for("claude.cmd", None), vec!["claude.cmd"]);
        assert_eq!(candidate_names_for("CLAUDE.Cmd", None), vec!["CLAUDE.Cmd"]);
        // A NON-PATHEXT extension is unlaunchable on Windows ⇒ no candidate.
        assert!(candidate_names_for("idea.sh", None).is_empty());
    }

    #[test]
    fn windows_candidates_ignore_dots_in_parent_directories() {
        for (bin, expected) in [
            (
                r"C:\tools\v1.2\codex",
                [r"C:\tools\v1.2\codex.cmd", r"C:\tools\v1.2\codex.exe"],
            ),
            (r"..\bin\codex", [r"..\bin\codex.cmd", r"..\bin\codex.exe"]),
            (
                "../tools/v1.2/codex",
                ["../tools/v1.2/codex.cmd", "../tools/v1.2/codex.exe"],
            ),
            (
                r"C:\tools\v1.2\.codex",
                [r"C:\tools\v1.2\.codex.cmd", r"C:\tools\v1.2\.codex.exe"],
            ),
        ] {
            assert_eq!(candidate_names_for(bin, Some(".CMD;.EXE")), expected);
        }
        assert_eq!(
            candidate_names_for(r"C:\tools\v1.2\codex.EXE", None),
            [r"C:\tools\v1.2\codex.EXE"]
        );
        assert!(candidate_names_for(r"..\bin\codex.ps1", None).is_empty());
    }

    /// A `PATHEXT` entry we could not actually EXECUTE is dropped, whatever its
    /// position: npm's `claude.ps1` sits beside `claude.cmd` and `.PS1;.CMD` is a
    /// PATHEXT a PowerShell user really has — resolving the `.ps1` there hands the
    /// spawn a file `Command` cannot launch. Pure string logic ⇒ every platform.
    #[test]
    fn windows_candidates_drop_extensions_the_spawn_cannot_execute() {
        assert_eq!(
            candidate_names_for("claude", Some(".PS1;.CMD;.VBS;.EXE")),
            vec!["claude.cmd", "claude.exe"]
        );
        // …including when the NAME carries it.
        assert!(candidate_names_for("claude.ps1", Some(".PS1;.CMD")).is_empty());
    }

    /// The fallback is on the PARSED list, not on the raw string: these are all
    /// non-empty `PATHEXT` values with zero launchable entries, and zero entries
    /// would make EVERY launcher unresolvable (a misconfigured `PATHEXT`
    /// must not read as "no agent installed"). Pure string logic ⇒ every platform.
    #[test]
    fn windows_falls_back_to_the_default_when_pathext_parses_to_nothing() {
        for pathext in [";", ";;", " ; . ; ", ".PS1", ".PS1;.VBS;.WSF"] {
            assert_eq!(
                candidate_names_for("claude", Some(pathext)),
                vec!["claude.com", "claude.exe", "claude.bat", "claude.cmd"],
                "PATHEXT={pathext:?} must fall back to the documented default"
            );
        }
    }

    /// The npm layout: `claude` (a `#!/bin/sh` Git Bash shim) NEXT TO `claude.cmd`.
    /// Resolution must return the `.cmd` — `Command::new` on the extension-less
    /// sibling cannot launch it, so returning it would make probe and spawn agree
    /// on the wrong answer.
    ///
    /// Stays `cfg(windows)`: it goes through [`find_bin_in`], so it depends on the
    /// platform's [`candidate_names`] and on real files. The ORDERING rule it pins is
    /// covered on macOS too, by the pure `candidate_names_for` tests above.
    #[cfg(windows)]
    #[test]
    fn windows_never_resolves_the_extensionless_npm_shim() {
        let root = tempfile::tempdir().expect("tempdir");
        let npm = root.path().join("npm");
        let sh_only = root.path().join("sh-only");
        std::fs::create_dir_all(&npm).expect("mkdir");
        std::fs::create_dir_all(&sh_only).expect("mkdir");

        // The full npm layout, `claude.ps1` third sibling included: it is never a
        // candidate (not launchable), so the `.cmd` wins even here.
        write_exec(&npm, "claude");
        write_exec(&npm, "claude.ps1");
        let shim = write_exec(&npm, "claude.cmd");
        assert_eq!(find_bin_in(&[npm], &["claude"]), Some(shim));

        // The sh shim ALONE resolves to nothing rather than to itself.
        write_exec(&sh_only, "claude");
        assert_eq!(find_bin_in(&[sh_only], &["claude"]), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_candidate_is_the_bare_name() {
        assert_eq!(candidate_names("claude"), vec!["claude"]);
        assert_eq!(candidate_names("idea.sh"), vec!["idea.sh"]);
    }
}
