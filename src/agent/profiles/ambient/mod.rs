//! The built-in profiles' best-effort ("ambient") transcript sources (feature `turns`,
//! every platform): where each CLI writes the transcript of the agent in a terminal, and
//! which of its records end a turn. Each profile returns its source from
//! [`AgentProfile::ambient_turns`](crate::agent::AgentProfile::ambient_turns); see
//! [`turns::ambient`](crate::agent::turns::ambient) for what this tier does and does not
//! establish (no writer exclusion, no attribution).
//!
//! The sources read the user's own configuration directory with whatever CLI version is
//! installed, so they pin no version: unknown record kinds and fields are skipped, and a
//! record of a kind they know without the fields they read is reported as
//! [`AmbientRecord::Unrecognised`](crate::agent::turns::ambient::AmbientRecord::Unrecognised).
//! Every lookup reads the agent's variables through
//! [`AmbientContext::var`](crate::agent::turns::ambient::AmbientContext::var), stays under
//! the configuration root it resolved (paths are built from that root and from names it
//! listed or validated, never from a path a file records; symbolic links in it are followed
//! as the CLI follows them), walks at most a bounded number of directory entries and reads
//! a bounded number of bytes of first lines and session files per wake. Ported from the
//! predecessor observer's per-CLI sources.
//!
//! | | transcript | turn end | reply | evidence |
//! | --- | --- | --- | --- | --- |
//! | `claude` | `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/<slug>/<sessionId>.jsonl`, the session named by `sessions/<pid>.json` | `system` `turn_duration`; an interruption record | the turn's last assistant message if it stopped with `end_turn` | explicit |
//! | `codex` | `<CODEX_HOME or ~/.codex>/sessions/YYYY/MM/DD/rollout-*.jsonl`, the newest interactive one of the project | `event_msg` `task_complete`, `turn_aborted` | `last_agent_message` | explicit |
//! | `pi` | `PI_CODING_AGENT_SESSION_DIR`, else `<PI_CODING_AGENT_DIR or ~/.pi/agent>/sessions/--<project>--/`: the project's session written last | none written: the next prompt, or quiet | the final assistant message's text | inferred |
//! | `cursor` | `<CURSOR_DATA_DIR or ~/.cursor>/projects/<slug>/agent-transcripts/<id>/<id>.jsonl` created since the launch, written last | `turn_ended` `success`; the next prompt after a rewrite dropped it | none, unless opted in | explicit or inferred |

pub(in crate::agent) mod claude;
pub(in crate::agent) mod codex;
pub(in crate::agent) mod cursor;
mod json;
pub(in crate::agent) mod opencode;
pub(in crate::agent) mod pi;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Take};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::agent::turns::TurnReply;
use crate::agent::turns::ambient::AmbientUnavailable;
use crate::agent::turns::ambient::paths::{FileId, FileStamp, open_regular};

/// Directory entries one lookup walks at most (all directories together). Past this the
/// lookup declines to guess ([`AmbientUnavailable::Ambiguous`]).
pub(super) const MAX_ENTRIES: usize = 4096;
/// First lines (or session files) one lookup reads at most, in bytes (cached first lines
/// are not read again).
pub(super) const READ_BUDGET: u64 = 4 * 1024 * 1024;
/// First lines a source keeps; past this its cache starts over.
const MAX_HEADS: usize = 1024;

/// What one lookup may still walk and read.
pub(super) struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            entries: MAX_ENTRIES,
            bytes: READ_BUDGET,
        }
    }

    /// The entries of `dir` (none when it does not exist), counted against the budget;
    /// [`AmbientUnavailable::Ambiguous`] when there are more than are left.
    pub(super) fn list(
        &mut self,
        dir: &Path,
    ) -> Result<Vec<std::fs::DirEntry>, AmbientUnavailable> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(AmbientUnavailable::Io(e.kind())),
        };
        let mut listed = Vec::new();
        for entry in entries {
            if self.entries == 0 {
                return Err(AmbientUnavailable::Ambiguous);
            }
            self.entries -= 1;
            // An entry that vanished while listed is no candidate.
            if let Ok(entry) = entry {
                listed.push(entry);
            }
        }
        Ok(listed)
    }

    /// Nothing is left to read: the lookup decides on the next wake.
    pub(super) fn spent(&self) -> bool {
        self.bytes == 0
    }

    /// Counts `bytes` read. A read starts only while some budget is left, so one lookup
    /// reads at most [`READ_BUDGET`] plus one capped read.
    fn charge(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

/// The first line of `path`, its newline included, read up to `cap` bytes; `None` while
/// it is not complete (shorter than `cap` without a newline) or the file cannot be read. A
/// line of `cap` bytes or more comes back cut at `cap`.
#[cfg(test)]
fn first_line(path: &Path, cap: u64) -> Option<Vec<u8>> {
    let (file, _) = open_regular(path).ok()?;
    header(&mut (&file).take(cap)).ok().flatten()
}

fn header(input: &mut Take<&std::fs::File>) -> std::io::Result<Option<Vec<u8>>> {
    let cap = input.limit();
    let mut first = Vec::new();
    BufReader::new(input).read_until(b'\n', &mut first)?;
    let complete = first.last() == Some(&b'\n');
    Ok((complete || first.len() as u64 >= cap).then_some(first))
}

/// The small file at `path` (a session record), at most `cap` bytes: `Ok(None)` when it
/// cannot be read or is longer, [`AmbientUnavailable::ReadBudgetExceeded`] when
/// the lookup has no bytes left.
pub(super) fn small_file(
    path: &Path,
    cap: u64,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>, AmbientUnavailable> {
    if budget.spent() {
        return Err(AmbientUnavailable::ReadBudgetExceeded);
    }
    let mut bytes = Vec::new();
    let Ok((file, _)) = open_regular(path) else {
        return Ok(None);
    };
    // One byte past the cap tells a longer file.
    let read = file.take(cap + 1).read_to_end(&mut bytes);
    budget.charge(bytes.len() as u64);
    Ok(read
        .ok()
        .filter(|_| bytes.len() as u64 <= cap)
        .map(|_| bytes))
}

pub(super) struct Heads<T> {
    known: Mutex<HashMap<FileId, (FileStamp, T)>>,
}

impl<T> Default for Heads<T> {
    fn default() -> Self {
        Self {
            known: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone> Heads<T> {
    /// What the first line of `path` (read up to `cap` bytes) says,
    /// read with `read` when not known: `Ok(None)` while that line is incomplete or the
    /// file unreadable, [`AmbientUnavailable::ReadBudgetExceeded`] when the lookup
    /// has no bytes left for an uncached file.
    pub(super) fn of(
        &self,
        path: &Path,
        cap: u64,
        budget: &mut Budget,
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, AmbientUnavailable> {
        let Ok((file, _)) = open_regular(path) else {
            return Ok(None);
        };
        self.cached(&file, || {
            if budget.spent() {
                return Err(AmbientUnavailable::ReadBudgetExceeded);
            }
            let mut input = (&file).take(cap);
            let line = header(&mut input);
            // The file supplied these bytes even if the newline came earlier or the read failed.
            budget.charge(cap - input.limit());
            Ok(line.ok().flatten().as_deref().map(read))
        })
    }

    /// Validate the descriptor used for the actual read, including on departure.
    /// Only the decoded header is shared. Project membership is checked by the caller.
    fn opened(
        &self,
        file: &std::fs::File,
        cap: u64,
        read: impl FnOnce(&[u8]) -> T,
    ) -> std::io::Result<Option<T>> {
        self.cached(file, || {
            Ok(header(&mut file.take(cap))?.as_deref().map(read))
        })
    }

    fn cached<E>(
        &self,
        file: &std::fs::File,
        load: impl FnOnce() -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let Some(stamp) = FileStamp::of(file) else {
            return load();
        };
        {
            let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((_, head)) = known.get(&stamp.id).filter(|(cached, _)| *cached == stamp) {
                return Ok(Some(head.clone()));
            }
        }
        let head = load()?;
        if let Some(head) = &head {
            let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
            if known.len() >= MAX_HEADS {
                known.clear();
            }
            known.insert(stamp.id, (stamp, head.clone()));
        }
        Ok(head)
    }
}

/// Appends `text` to `reply` when it is not blank — after a blank line when `reply`
/// already has text — keeping at most `limit` bytes.
pub(super) fn append(reply: &mut TurnReply, text: &str, limit: usize) {
    if text.trim().is_empty() {
        return;
    }
    if !reply.text.is_empty() {
        reply.push("\n\n", limit);
    }
    reply.push(text, limit);
}

/// What a built-in transcript shows of questions (feature `questions`), by the controlled
/// tier's rules: a question tool's call counts only in a turn whose prompt this parser read
/// (written after observation started), and the next prompt and the turn's end withdraw it.
/// The records are read by the controlled tier's strict `provider_record`: a malformed or
/// oversized (over 64 KiB) record withdraws the question.
#[cfg(feature = "questions")]
#[derive(Default)]
pub(super) struct Asked {
    /// A prompt was read since the last turn end.
    armed: bool,
    /// The line just recorded started or ended a turn.
    edge: bool,
}

#[cfg(feature = "questions")]
impl Asked {
    /// The line just recorded is a prompt.
    pub(super) fn prompt(&mut self) {
        self.armed = true;
        self.edge = true;
    }

    /// The line just recorded ends the turn.
    pub(super) fn end(&mut self) {
        self.armed = false;
        self.edge = true;
    }

    /// What `line`, just recorded, tells of questions in `provider`'s records (`claude`,
    /// `codex`).
    pub(super) fn line(
        &mut self,
        provider: &str,
        line: &[u8],
    ) -> Option<crate::agent::turns::ambient::AmbientQuestion> {
        use crate::agent::questions::{TranscriptChange, provider_record};
        use crate::agent::turns::ambient::AmbientQuestion;
        if std::mem::take(&mut self.edge) {
            return Some(AmbientQuestion::Withdrawn);
        }
        if !self.armed {
            return None;
        }
        match provider_record(provider, line) {
            TranscriptChange::Keep => None,
            TranscriptChange::Open(question) => Some(AmbientQuestion::Asked(question)),
            TranscriptChange::Resolved(id) => Some(AmbientQuestion::Answered(id)),
            TranscriptChange::Clear => Some(AmbientQuestion::Withdrawn),
        }
    }
}

/// A name a CLI recorded, used as one path component: letters, digits, `-` and `_` only
/// (session ids are UUIDs), so a crafted record cannot point elsewhere.
pub(super) fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A directory name built from `path` the way claude and cursor build theirs: every char
/// outside `[A-Za-z0-9]` becomes `-`. Always ASCII.
pub(super) fn dashed(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
pub(in crate::agent) mod testing {
    //! Lookup test helpers: an [`AmbientContext`] whose environment holds only what a test
    //! puts there, temporary project and home directories, file times set explicitly.

    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::agent::turns::ambient::{AmbientContext, AmbientRecord, AmbientSource};
    use crate::env::EnvPolicy;

    /// A context in `cwd` whose environment holds only the home (`HOME` and `USERPROFILE`)
    /// and `vars`, never this process's own.
    pub(crate) fn ctx(cwd: &Path, home: &Path, vars: &[(&str, &Path)]) -> AmbientContext {
        let mut env = EnvPolicy {
            inherit: false,
            ..EnvPolicy::default()
        };
        env.set("HOME", home).set("USERPROFILE", home);
        for (key, value) in vars {
            env.set(*key, value.as_os_str());
        }
        AmbientContext::new(cwd, env)
    }

    /// A temporary directory with a project (whose name needs dashing) and a home in it.
    pub(crate) fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("my project.x");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        (tmp, project, home)
    }

    pub(crate) fn canonical(path: &Path) -> PathBuf {
        crate::agent::turns::ambient::paths::project_path(path)
    }

    /// The tests' launch time.
    pub(crate) fn start() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// `start() + secs` (negative: before it).
    pub(crate) fn at(secs: i64) -> SystemTime {
        if secs >= 0 {
            start() + Duration::from_secs(secs.unsigned_abs())
        } else {
            start() - Duration::from_secs(secs.unsigned_abs())
        }
    }

    /// Writes `text` to `path` (its directories created) and sets its modification time.
    pub(crate) fn write_at(path: &Path, text: &str, modified: SystemTime) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        stamp(path, modified);
    }

    pub(crate) fn stamp(path: &Path, modified: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    /// What a parser told about one line, or a quiet transcript; replies as the observer
    /// passes them on (blank text is none).
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Seen {
        /// [`AmbientRecord::Ended`] from a line: an explicit end.
        Ended(Option<String>),
        /// [`AmbientRecord::EndedBefore`]: the line before ended the turn.
        Before(Option<String>),
        /// An end from [`AmbientParser::quiet`].
        Quiet(Option<String>),
        Unrecognised,
    }

    /// `Seen::Ended(Some(text))`.
    pub(crate) fn ended(text: &str) -> Seen {
        Seen::Ended(Some(text.into()))
    }

    fn seen(record: AmbientRecord, quiet: bool) -> Option<Seen> {
        let text = |reply: Option<crate::agent::turns::TurnReply>| {
            reply
                .map(|reply| reply.text)
                .filter(|text| !text.trim().is_empty())
        };
        Some(match record {
            AmbientRecord::Ended(reply) | AmbientRecord::EndedBefore(reply) if quiet => {
                Seen::Quiet(text(reply))
            }
            AmbientRecord::Ended(reply) => Seen::Ended(text(reply)),
            AmbientRecord::EndedBefore(reply) => Seen::Before(text(reply)),
            AmbientRecord::Unrecognised => Seen::Unrecognised,
            AmbientRecord::Unchanged | AmbientRecord::SuppressedEnd => return None,
        })
    }

    /// Feeds `lines` to a fresh parser of `source` as the observer does, the transcript
    /// quiet after each line `i` for which `quiet_after(i)` holds and after the last one.
    pub(crate) fn feed_quiet(
        source: &dyn AmbientSource,
        lines: &[String],
        quiet_after: impl Fn(usize) -> bool,
    ) -> Vec<Seen> {
        let limit = crate::agent::turns::TurnOptions::default().reply_bytes;
        let mut parser = source.parser();
        let mut seen = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            seen.extend(
                parser
                    .record(line.trim().as_bytes(), limit)
                    .and_then(|r| self::seen(r, false)),
            );
            if i + 1 == lines.len() || quiet_after(i) {
                seen.extend(parser.quiet(limit).and_then(|r| self::seen(r, true)));
            }
        }
        seen
    }

    /// [`feed_quiet`], quiet only after the last line.
    pub(crate) fn feed(source: &dyn AmbientSource, lines: &[String]) -> Vec<Seen> {
        feed_quiet(source, lines, |_| false)
    }

    /// Feeds `lines` to a fresh parser of `source` as the observer does (`record`, then
    /// `question`): what the lines told of questions (`asked <id>`, `answered <id>`,
    /// `withdrawn`; nothing for the lines that told nothing), and the questions asked.
    #[cfg(feature = "questions")]
    pub(crate) fn questions(
        source: &dyn AmbientSource,
        lines: &[String],
    ) -> (Vec<String>, Vec<crate::agent::questions::PendingQuestion>) {
        use crate::agent::turns::ambient::AmbientQuestion;
        let limit = crate::agent::turns::TurnOptions::default().reply_bytes;
        let mut parser = source.parser();
        let (mut told, mut asked) = (Vec::new(), Vec::new());
        for line in lines {
            let line = line.trim().as_bytes();
            parser.record(line, limit);
            match parser.question(line) {
                None => {}
                Some(AmbientQuestion::Asked(question)) => {
                    told.push(format!("asked {}", question.id));
                    asked.push(question);
                }
                Some(AmbientQuestion::Answered(id)) => told.push(format!("answered {id}")),
                Some(AmbientQuestion::Withdrawn) => told.push("withdrawn".into()),
            }
        }
        (told, asked)
    }

    /// `value` as one transcript line.
    pub(crate) fn line(value: serde_json::Value) -> String {
        value.to_string()
    }

    /// The fixture `name` under `tests/fixtures/transcripts`, its lines.
    pub(crate) fn fixture(name: &str) -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transcripts")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(String::from)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_counts_against_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let mut budget = Budget::new();
        budget.entries = 3;
        assert_eq!(budget.list(dir.path()).unwrap().len(), 3);
        assert_eq!(
            budget.list(dir.path()).unwrap_err(),
            AmbientUnavailable::Ambiguous
        );
        let mut budget = Budget::new();
        assert!(budget.list(&dir.path().join("missing")).unwrap().is_empty());
        assert_eq!(budget.entries, MAX_ENTRIES);
    }

    #[cfg(unix)]
    #[test]
    fn lookups_reject_device_files() {
        assert_eq!(first_line(Path::new("/dev/zero"), 64), None);
        assert_eq!(
            small_file(Path::new("/dev/null"), 64, &mut Budget::new()),
            Ok(None)
        );
    }

    #[test]
    fn a_first_line_is_complete_or_cut_at_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "{\"a\":1}\nrest").unwrap();
        assert_eq!(first_line(&path, 1024).unwrap(), b"{\"a\":1}\n");
        assert_eq!(first_line(&path, 3).unwrap(), b"{\"a");
        std::fs::write(&path, "{\"a\":").unwrap();
        assert_eq!(first_line(&path, 1024), None, "still being written");
        assert_eq!(first_line(&dir.path().join("missing"), 1024), None);
    }

    #[test]
    fn unchanged_files_reuse_the_decoded_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "one\nmore\n").unwrap();
        let heads = Heads::<String>::default();
        let mut reads = 0;
        let mut budget = Budget::new();
        let mut head = |reads: &mut usize| {
            heads.of(&path, 64, &mut budget, |line| {
                *reads += 1;
                String::from_utf8_lossy(line).into_owned()
            })
        };
        assert_eq!(head(&mut reads), Ok(Some("one\n".into())));
        assert_eq!(head(&mut reads), Ok(Some("one\n".into())));
        assert_eq!(reads, 1);
        std::fs::write(&path, "x\n").unwrap();
        assert_eq!(head(&mut reads), Ok(Some("x\n".into())), "cut below it");
        assert_eq!(reads, 2);
    }

    #[test]
    fn header_reads_charge_read_ahead_and_cached_headers_remain_free() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::<usize>::default();
        let mut budget = Budget::new();
        budget.bytes = 160;
        for (name, size) in [("short", 32), ("long", 4096), ("next", 256)] {
            let mut contents = vec![b'x'; size];
            contents[1] = b'\n';
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        let mut head = |name| heads.of(&dir.path().join(name), 128, &mut budget, |line| line.len());
        assert_eq!(head("short"), Ok(Some(2)));
        assert_eq!(head("long"), Ok(Some(2)));
        assert_eq!(head("next"), Err(AmbientUnavailable::ReadBudgetExceeded));
        assert_eq!(head("short"), Ok(Some(2)));
    }

    #[test]
    fn reads_stop_at_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "0123456789\n").unwrap();
        let mut budget = Budget::new();
        budget.bytes = 5;
        assert_eq!(
            small_file(&path, 64, &mut budget).unwrap().unwrap().len(),
            11
        );
        assert!(
            budget.spent(),
            "one read goes past what was left, no further"
        );
        assert_eq!(
            small_file(&path, 64, &mut budget),
            Err(AmbientUnavailable::ReadBudgetExceeded)
        );
        let mut budget = Budget::new();
        assert_eq!(small_file(&path, 4, &mut budget), Ok(None), "over the cap");
        assert_eq!(
            small_file(&dir.path().join("missing"), 4, &mut budget),
            Ok(None)
        );
        let heads = Heads::<usize>::default();
        budget.bytes = 0;
        assert_eq!(
            heads.of(&path, 64, &mut budget, |l| l.len()),
            Err(AmbientUnavailable::ReadBudgetExceeded)
        );
    }

    #[test]
    fn text_is_joined_by_a_blank_line_within_the_limit() {
        let mut reply = TurnReply::default();
        append(&mut reply, "one", 64);
        append(&mut reply, "  \n", 64);
        append(&mut reply, "two", 64);
        assert_eq!(reply.text, "one\n\ntwo");
        assert!(!reply.truncated);
        append(&mut reply, "three", 10);
        assert_eq!(reply.text, "one\n\ntwo\n\n");
        assert!(reply.truncated);
    }

    #[test]
    fn plain_names_and_dashed_paths() {
        assert!(plain_name("4b981fee-eaf9-4aea-85b3-846545dd6169"));
        for name in ["", "../x", "a/b", r"a\b", "a.b", &"x".repeat(129)] {
            assert!(!plain_name(name), "{name}");
        }
        assert_eq!(
            dashed(Path::new("/Users/me/my proj.x")),
            "-Users-me-my-proj-x"
        );
        assert_eq!(dashed(Path::new(r"C:\Users\me\proj")), "C--Users-me-proj");
        assert_eq!(dashed(Path::new("/tmp/ü")), "-tmp--");
    }
}
