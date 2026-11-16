//! Command-line parsing of the capture tool: hand-rolled, every value validated here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::text::{self, Entry};

pub const HELP: &str =
    "usage: capture --cwd <dir> (--cmd <program> [--arg <a>]... | --shell-line <line>)
  [--shell-settle <ms>] [--identity honest|vscode|custom:K=V,...] [--term-version <v>]
  [--size <rows>x<cols>] [--duration <ms>] [--script \"<ms>:<payload>;...\"]
  [--answer-queries | --no-answer-queries] [--out <prefix>] [--agent <id>]
  [--agent-version <v>] [--scene <s>] [--sanitize] [--interactive] [--quiet]
See the doc comment of crates/agent-link/examples/capture.rs for details.";

/// Default `--duration` outside `--interactive`.
const DEFAULT_DURATION: Duration = Duration::from_millis(20_000);

/// Upper bound of every millisecond value (a day): far past any capture, and keeps
/// `Instant + Duration` from overflowing.
pub const MAX_MS: u64 = 86_400_000;

/// Upper bound of `--size` rows and columns: vt100 allocates rows x cols cells up front.
const MAX_SIDE: u16 = 1000;

/// Options that take one value and may be given at most once.
const SINGLE: &[&str] = &[
    "--cmd",
    "--shell-line",
    "--shell-settle",
    "--cwd",
    "--identity",
    "--term-version",
    "--size",
    "--duration",
    "--script",
    "--out",
    "--agent",
    "--agent-version",
    "--scene",
];

pub enum Mode {
    Direct { program: String, args: Vec<String> },
    Shell { line: String },
}

pub struct Options {
    pub mode: Mode,
    pub cwd: PathBuf,
    pub identity: String,
    pub term_version: String,
    pub rows: u16,
    pub cols: u16,
    /// `None`: no deadline (interactive without `--duration`).
    pub duration: Option<Duration>,
    pub shell_settle: Duration,
    pub script: Vec<Entry>,
    pub answer: bool,
    pub out: Option<PathBuf>,
    /// `.meta` agent, version and scene.
    pub meta: [String; 3],
    pub sanitize: bool,
    pub interactive: bool,
    pub quiet: bool,
}

/// Parses the process arguments; `Ok(None)` for `--help`.
pub fn parse_args() -> Result<Option<Options>, String> {
    let mut args = std::env::args_os().skip(1).map(|a| {
        a.into_string()
            .map_err(|a| format!("argument {a:?} is not UTF-8"))
    });
    let (mut cmd, mut cmd_args, mut shell_line) = (None, Vec::new(), None);
    let (mut cwd, mut duration, mut shell_settle, mut term_version) = (None, None, None, None);
    let mut answer: Option<bool> = None;
    let mut o = Options {
        mode: Mode::Shell {
            line: String::new(),
        },
        cwd: PathBuf::new(),
        identity: "honest".into(),
        term_version: "1.104.0".into(),
        rows: 40,
        cols: 120,
        duration: None,
        shell_settle: Duration::from_millis(1500),
        script: Vec::new(),
        answer: true,
        out: None,
        meta: Default::default(),
        sanitize: false,
        interactive: false,
        quiet: false,
    };
    let mut meta: [Option<String>; 3] = Default::default();
    let mut seen: Vec<String> = Vec::new();
    let ms = |v: String, flag: &str| {
        v.parse::<u64>()
            .ok()
            .filter(|&n| n <= MAX_MS)
            .map(Duration::from_millis)
            .ok_or_else(|| format!("{flag}: {v:?} is not a number of milliseconds up to {MAX_MS}"))
    };
    while let Some(arg) = args.next() {
        let arg = arg?;
        if SINGLE.contains(&arg.as_str()) {
            if seen.contains(&arg) {
                return Err(format!("{arg} given more than once"));
            }
            seen.push(arg.clone());
        }
        let mut value = || {
            args.next()
                .unwrap_or_else(|| Err(format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--cmd" => cmd = Some(value()?),
            "--arg" => cmd_args.push(value()?),
            "--shell-line" => shell_line = Some(value()?),
            "--shell-settle" => shell_settle = Some(ms(value()?, "--shell-settle")?),
            "--cwd" => cwd = Some(PathBuf::from(value()?)),
            "--identity" => o.identity = value()?,
            "--term-version" => term_version = Some(meta_value("--term-version", value()?)?),
            "--size" => {
                let v = value()?;
                let parsed = v
                    .split_once('x')
                    .and_then(|(r, c)| Some((r.parse::<u16>().ok()?, c.parse::<u16>().ok()?)))
                    .filter(|&(r, c)| (1..=MAX_SIDE).contains(&r) && (1..=MAX_SIDE).contains(&c));
                (o.rows, o.cols) = parsed.ok_or_else(|| {
                    format!("--size: {v:?} is not <rows>x<cols>, each 1..={MAX_SIDE}")
                })?;
            }
            "--duration" => duration = Some(ms(value()?, "--duration")?),
            "--script" => o.script = text::parse_script(&value()?)?,
            flag @ ("--answer-queries" | "--no-answer-queries") => {
                let on = flag == "--answer-queries";
                if answer.is_some_and(|prev| prev != on) {
                    return Err(
                        "--answer-queries and --no-answer-queries exclude each other".into(),
                    );
                }
                answer = Some(on);
            }
            "--out" => o.out = Some(PathBuf::from(value()?)),
            "--agent" => meta[0] = Some(meta_value("--agent", value()?)?),
            "--agent-version" => meta[1] = Some(meta_value("--agent-version", value()?)?),
            "--scene" => meta[2] = Some(meta_value("--scene", value()?)?),
            "--sanitize" => o.sanitize = true,
            "--interactive" => o.interactive = true,
            "--quiet" => o.quiet = true,
            other => return Err(format!("unknown option {other:?}\n{HELP}")),
        }
    }
    o.answer = answer.unwrap_or(true);
    o.mode = match (cmd, shell_line) {
        (Some(_), Some(_)) => return Err("--cmd and --shell-line exclude each other".into()),
        (Some(_), None) if shell_settle.is_some() => {
            return Err("--shell-settle is only valid with --shell-line".into());
        }
        (Some(program), None) => Mode::Direct {
            program,
            args: cmd_args,
        },
        (None, Some(line)) if cmd_args.is_empty() => Mode::Shell { line },
        (None, Some(_)) => return Err("--arg is only valid with --cmd".into()),
        (None, None) => {
            return Err(format!(
                "give exactly one of --cmd and --shell-line\n{HELP}"
            ));
        }
    };
    if let Some(settle) = shell_settle {
        o.shell_settle = settle;
    }
    o.cwd = cwd.ok_or_else(|| format!("--cwd is required\n{HELP}"))?;
    if !o.cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", o.cwd.display()));
    }
    let is_custom = o.identity.starts_with("custom:");
    if !matches!(o.identity.as_str(), "honest" | "vscode") && !is_custom {
        return Err(format!(
            "--identity {:?}: expected honest, vscode or custom:K=V,…",
            o.identity
        ));
    }
    if o.identity.chars().any(char::is_control) {
        return Err("--identity must not contain control characters".into());
    }
    if let Some(v) = term_version {
        if o.identity != "vscode" {
            return Err("--term-version is only valid with --identity vscode".into());
        }
        o.term_version = v;
    }
    if let Some(out) = &o.out {
        check_out(out)?;
        o.meta = fixture_meta(out, meta, o.sanitize, o.quiet)?;
    } else if o.sanitize {
        return Err("--sanitize needs --out".into());
    } else if meta.iter().any(Option::is_some) {
        return Err("--agent, --agent-version and --scene need --out (they go to .meta)".into());
    }
    if o.interactive && cfg!(windows) {
        return Err("--interactive is unix only".into());
    }
    if o.interactive && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err("--interactive needs stdin to be a terminal".into());
    }
    // Interactive captures end on child exit or Ctrl-], unless a deadline is asked for.
    o.duration = duration.or((!o.interactive).then_some(DEFAULT_DURATION));
    Ok(Some(o))
}

/// A `.meta` value (one `key=value` line): no line breaks, no `=`, no other control
/// characters.
fn meta_value(flag: &str, v: String) -> Result<String, String> {
    if v.contains('=') || v.chars().any(char::is_control) {
        return Err(format!(
            "{flag} {v:?}: must not contain `=`, a line break or other control characters"
        ));
    }
    Ok(v)
}

/// `--out` is a file prefix: an existing parent directory plus a non-empty file name that is
/// not `.`/`..`, not ending in a path separator and not an existing directory.
fn check_out(out: &Path) -> Result<(), String> {
    let s = out.to_string_lossy();
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    let last = s.rsplit(seps).next().unwrap_or("");
    if matches!(last, "" | "." | "..") || out.is_dir() {
        return Err(format!(
            "--out {}: give a file prefix such as dir/claude-2.1.282-trust, not a directory",
            out.display()
        ));
    }
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty());
    if parent.is_some_and(|p| !p.is_dir()) {
        return Err(format!(
            "--out {}: the parent directory must exist",
            out.display()
        ));
    }
    Ok(())
}

/// Resolves the `.meta` agent, version and scene: explicit options first, then the `--out`
/// file name. A name that does not follow the fixture rule is an error when sanitising (a
/// fixture is being recorded) and leaves the missing values empty otherwise.
fn fixture_meta(
    out: &Path,
    given: [Option<String>; 3],
    sanitize: bool,
    quiet: bool,
) -> Result<[String; 3], String> {
    let name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parsed = text::fixture_parts(&name).map(|(a, v, s)| [a, v, s]);
    if parsed.is_none() && given.iter().any(Option::is_none) {
        let why = format!(
            "--out name {name:?} is not <agent>-<version>-<scene> (version starts with a digit)"
        );
        if sanitize {
            return Err(format!(
                "{why}; rename it or pass --agent, --agent-version and --scene"
            ));
        }
        if !quiet {
            eprintln!("capture: {why}; .meta leaves the values not given as options empty");
        }
    }
    let (mut given, mut parsed) = (given, parsed.unwrap_or_default());
    let values: [String; 3] = std::array::from_fn(|i| {
        given[i]
            .take()
            .unwrap_or_else(|| std::mem::take(&mut parsed[i]))
    });
    // Values taken from the file name go through the same check as the options.
    for (flag, v) in ["--agent", "--agent-version", "--scene"]
        .iter()
        .zip(&values)
    {
        meta_value(flag, v.clone())?;
    }
    Ok(values)
}
