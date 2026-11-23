//! Ported from the predecessor observer's generic tests. The CLI-specific ones (a quiet
//! end without an end record, in-place rewrites that drop end records) use the synthetic
//! [`Synth`] format instead of recorded transcripts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::*;
use crate::agent::turns::TurnOptions;
use crate::agent::turns::ambient::{AmbientContext, AmbientUnavailable};

/// A synthetic JSON-lines format: `reply` holds a candidate final text, `turn_end` ends
/// the turn explicitly, `user` starts a turn (ending the previous one when its candidate
/// had no end record), `tool` discards the candidate. With `quiet`, a quiet transcript
/// ends a turn whose candidate is pending (a CLI without end records).
struct Synth {
    quiet: bool,
}

impl AmbientSource for Synth {
    fn locate(&self, _: &AmbientContext) -> Result<PathBuf, AmbientUnavailable> {
        unreachable!("not located here")
    }

    fn is_session_header(&self, line: &[u8]) -> bool {
        serde_json::from_slice::<Value>(line).is_ok_and(|v| v["type"] == "session")
    }

    fn parser(&self) -> Box<dyn AmbientParser> {
        Box::new(SynthParser {
            quiet: self.quiet,
            candidate: None,
        })
    }
}

const ENDS: Synth = Synth { quiet: false };
const QUIET: Synth = Synth { quiet: true };

struct SynthParser {
    quiet: bool,
    candidate: Option<TurnReply>,
}

fn reply(text: &str, limit: usize) -> TurnReply {
    let mut reply = TurnReply::default();
    reply.push(text, limit);
    reply
}

impl AmbientParser for SynthParser {
    fn record(&mut self, line: &[u8], reply_bytes: usize) -> Option<AmbientRecord> {
        let value: Value = serde_json::from_slice(line).ok()?;
        match value["type"].as_str()? {
            "reply" => {
                let Some(text) = value["text"].as_str() else {
                    return Some(AmbientRecord::Unrecognised);
                };
                self.candidate = Some(reply(text, reply_bytes));
                None
            }
            "turn_end" => Some(AmbientRecord::Ended(self.candidate.take())),
            "user" => self
                .candidate
                .take()
                .map(|reply| AmbientRecord::EndedBefore(Some(reply))),
            "tool" => {
                self.candidate = None;
                None
            }
            _ => None,
        }
    }

    fn quiet(&mut self, _: usize) -> Option<AmbientRecord> {
        if !self.quiet {
            return None;
        }
        self.candidate
            .take()
            .map(|reply| AmbientRecord::Ended(Some(reply)))
    }
}

fn limits() -> TurnOptions {
    TurnOptions::default()
}

fn turn(reply: &str) -> String {
    format!("{{\"type\":\"reply\",\"text\":\"{reply}\"}}\n{{\"type\":\"turn_end\"}}\n")
}

/// A turn whose end line is unique (as a real CLI's often are: ids, times).
fn turn_n(reply: &str, n: u32) -> String {
    format!("{{\"type\":\"reply\",\"text\":\"{reply}\"}}\n{{\"type\":\"turn_end\",\"n\":{n}}}\n")
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn texts(report: &Report) -> Vec<String> {
    report
        .batch
        .turns
        .iter()
        .map(|t| {
            t.turn
                .reply
                .as_ref()
                .map_or_else(String::new, |r| r.text.clone())
        })
        .collect()
}

/// `path` read from its start.
fn from_start_with(path: &Path, source: &dyn AmbientSource) -> Watched {
    let tail = Tail::new(path.to_path_buf());
    Watched::new(
        0,
        path.to_path_buf(),
        tail,
        source.parser(),
        Capacity::new(limits()),
        true,
    )
}

fn from_start(path: &Path) -> Watched {
    from_start_with(path, &ENDS)
}

/// Reads until the end of the file; the turns of every pass.
fn read_all(watched: &mut Watched, source: &dyn AmbientSource, seen: &mut Seen) -> Report {
    let mut all = Report::default();
    loop {
        let report = watched.read(source, seen);
        assert!(report.error.is_none(), "{report:?}");
        all.batch.append(report.batch);
        all.rewritten |= report.rewritten;
        all.rebaselined |= report.rebaselined;
        all.found_again = all.found_again.or(report.found_again);
        all.oversized += report.oversized;
        if !report.more {
            return all;
        }
    }
}

fn replies(watched: &mut Watched) -> Vec<String> {
    texts(&read_all(watched, &ENDS, &mut Seen::default()))
}

#[test]
fn a_rewrite_that_dropped_older_turns_reports_only_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &(turn("one") + &turn("two")));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one", "two"]);

    std::fs::write(&path, turn("two") + &turn("three")).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    assert_eq!(texts(&report), ["three"]);
    append(&path, &turn("four"));
    assert_eq!(replies(&mut w), ["four"]);
}

#[test]
fn a_turn_whose_reply_line_changed_is_found_by_its_end_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &turn("one"));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one"]);

    std::fs::write(&path, turn("ONE")).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    assert!(report.batch.turns.is_empty(), "{report:?}");
    append(&path, &turn("two"));
    assert_eq!(replies(&mut w), ["two"]);
}

#[test]
fn a_file_cut_back_and_written_again_reports_nothing_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let text = turn("one") + &turn("two");
    append(&path, &text);
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one", "two"]);

    // Caught part-way through the rewrite: the first turn only.
    std::fs::write(&path, turn("one")).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        report.rewritten && report.batch.turns.is_empty(),
        "{report:?}"
    );
    std::fs::write(&path, &text).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        !report.rebaselined && report.batch.turns.is_empty(),
        "{report:?}"
    );
    append(&path, &turn("three"));
    assert_eq!(replies(&mut w), ["three"]);
}

#[test]
fn a_cleanup_rewrite_read_part_way_is_waited_for() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &(turn("one") + &turn("two")));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one", "two"]);

    // The cleanup drops the first turn and is read after its first line: another first
    // line, shorter, no anchor yet.
    let rewritten = turn("two");
    let (head, rest) = rewritten.split_at(rewritten.find('\n').unwrap() + 1);
    std::fs::write(&path, head).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    append(&path, rest);
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        !report.rebaselined && report.batch.turns.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.found_again, Some(1));
    append(&path, &turn("three"));
    assert_eq!(replies(&mut w), ["three"]);
}

#[test]
fn a_shorter_rewrite_without_the_anchor_is_given_up_once_it_stops_growing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &(turn_n("one", 1) + &turn_n("two", 2)));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one", "two"]);

    // Another transcript at the same path, shorter.
    std::fs::write(&path, turn_n("x", 3)).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        report.rebaselined && report.batch.turns.is_empty(),
        "{report:?}"
    );
    append(&path, &turn("next"));
    assert_eq!(replies(&mut w), ["next"]);
}

#[test]
fn a_rewrite_without_the_last_turn_read_is_read_on_from_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &turn_n("one", 1));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one"]);

    // Another transcript at the same path (file replacement).
    std::fs::write(
        &path,
        format!(
            "{}{{\"type\":\"reply\",\"text\":\"old candidate\"}}\n",
            turn_n("x", 2)
        ),
    )
    .unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && report.rebaselined, "{report:?}");
    assert!(report.batch.turns.is_empty(), "{report:?}");
    append(&path, "{\"type\":\"turn_end\"}\n");
    let ended = read_all(&mut w, &ENDS, &mut Seen::default());
    assert_eq!(ended.batch.turns.len(), 1);
    assert!(ended.batch.turns[0].turn.reply.is_none());
    append(&path, &turn("next"));
    assert_eq!(replies(&mut w), ["next"]);
}

#[test]
fn a_rewrite_of_a_file_read_from_its_end_never_reports_what_was_there() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &(turn("old one") + &turn("old two")));
    let len = std::fs::metadata(&path).unwrap().len();
    let tail = Tail::from(path.clone(), len).unwrap();
    let mut w = Watched::new(
        0,
        path.clone(),
        tail,
        ENDS.parser(),
        Capacity::new(limits()),
        false,
    );
    assert!(replies(&mut w).is_empty(), "the baseline is old");

    // The older turn dropped, a new one added, in one rewrite.
    std::fs::write(&path, turn("old two") + &turn("new")).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    assert_eq!(texts(&report), ["new"]);
}

#[test]
fn a_file_cut_back_stays_looked_for_until_it_outgrows_what_was_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let first = "{\"type\":\"start\"}\n";
    append(&path, &format!("{first}{}", turn_n("one", 1)));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one"]);

    std::fs::write(&path, first).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    // Other text than before, longer than what was read: the anchor is not coming.
    append(&path, &turn_n("a much longer reply than before", 2));
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        report.rebaselined && report.batch.turns.is_empty(),
        "{report:?}"
    );
    append(&path, &turn("next"));
    assert_eq!(replies(&mut w), ["next"]);
}

/// Reads `path` through `files` with `seen`: its turns' texts.
fn read_in(files: &mut Files, seen: &mut Seen, path: &Path) -> Vec<String> {
    read_in_with(files, seen, path, &ENDS)
}

fn read_in_with(
    files: &mut Files,
    seen: &mut Seen,
    path: &Path,
    source: &dyn AmbientSource,
) -> Vec<String> {
    let w = files
        .watch(path, source, Capacity::new(limits()), seen)
        .unwrap();
    texts(&read_all(w, source, seen))
}

fn since_now() -> Files {
    // Files written before this are older than the reference time on every file system
    // with creation times, also those with a coarse clock.
    std::thread::sleep(Duration::from_millis(20));
    let files = Files::new(SystemTime::now());
    std::thread::sleep(Duration::from_millis(20));
    files
}

#[test]
fn files_keep_each_transcripts_position_across_switches() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    // Each first located while missing: read from its start.
    assert!(read_in(&mut files, &mut seen, &a).is_empty());
    append(&a, &turn("a1"));
    assert_eq!(read_in(&mut files, &mut seen, &a), ["a1"]);
    assert!(read_in(&mut files, &mut seen, &b).is_empty());
    append(&b, &turn("b1"));
    assert_eq!(read_in(&mut files, &mut seen, &b), ["b1"]);
    append(&a, &turn("a2"));
    assert_eq!(read_in(&mut files, &mut seen, &a), ["a2"]);
}

#[test]
fn a_file_that_existed_is_read_from_its_size_when_its_directory_was_listed() {
    let dir = tempfile::tempdir().unwrap();
    let (old, current) = (dir.path().join("old.jsonl"), dir.path().join("cur.jsonl"));
    append(&old, &turn("ancient"));
    append(&current, &turn("before"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    assert!(read_in(&mut files, &mut seen, &current).is_empty());

    append(&old, &turn("resumed"));
    assert_eq!(read_in(&mut files, &mut seen, &old), ["resumed"]);
}

#[test]
fn switching_to_a_listed_partial_line_reads_its_completion_and_later_turns() {
    let dir = tempfile::tempdir().unwrap();
    let inactive = dir.path().join("inactive");
    let first = turn("completed after listing");
    let (partial, rest) = first.split_at(12);
    append(&inactive, &(turn("old") + partial));
    let mut files = Files::new(SystemTime::now() + Duration::from_secs(60));
    let mut seen = Seen::default();
    read_in(&mut files, &mut seen, &dir.path().join("active"));
    append(&inactive, &format!("{rest}{}", turn("later")));
    assert_eq!(
        read_in(&mut files, &mut seen, &inactive),
        ["completed after listing", "later"]
    );
}

#[test]
fn a_listed_file_rewritten_since_is_read_from_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let (old, current) = (dir.path().join("old.jsonl"), dir.path().join("cur.jsonl"));
    append(&old, &turn("ancient"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    read_in(&mut files, &mut seen, &current);

    // Rewritten with its lines shifted: the listed size falls inside the old reply line,
    // which read from there would be taken whole — and its turn reported.
    let listed = turn("ancient").len();
    let header = "{\"type\":\"header\",\"pad\":\"xxxxxxxxxx\"}\n";
    assert!(header.len() < listed && listed < header.len() + turn("ancient").len() - 20);
    std::fs::write(&old, format!("{header}{}", turn("ancient"))).unwrap();
    assert!(read_in(&mut files, &mut seen, &old).is_empty());
    append(&old, &turn("new"));
    assert_eq!(read_in(&mut files, &mut seen, &old), ["new"]);
}

/// A transcript that appears only after observation started is new from its first line.
#[test]
fn a_file_created_after_the_reference_time_is_read_from_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.jsonl");
    let (mut files, mut seen) = (since_now(), Seen::default());
    append(&path, &turn("first"));
    if std::fs::metadata(&path).unwrap().created().is_err() {
        // No creation time on this file system: read from its end (documented).
        assert!(read_in(&mut files, &mut seen, &path).is_empty());
        return;
    }
    assert_eq!(read_in(&mut files, &mut seen, &path), ["first"]);
}

#[test]
fn a_fork_reports_only_what_follows_the_copied_history() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("p.jsonl"), dir.path().join("f.jsonl"));
    append(&parent, &(HEADER.to_owned() + &turn("before")));
    let (mut files, mut seen) = (since_now(), Seen::default());
    assert!(read_in(&mut files, &mut seen, &parent).is_empty());
    append(&parent, &turn("one"));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["one"]);

    // The whole history, including the baseline, under a new session header.
    let history = std::fs::read_to_string(&parent)
        .unwrap()
        .replacen(HEADER, FORK_HEADER, 1);
    append(&fork, &format!("{history}{}", turn("forked")));
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["forked"]);
    append(&fork, &turn("next"));
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["next"]);
}

#[test]
fn a_fork_from_an_earlier_point_reports_none_of_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("p.jsonl"), dir.path().join("f.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    read_in(&mut files, &mut seen, &parent);
    append(&parent, &(turn("one") + &turn("two")));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["one", "two"]);

    append(&fork, &(turn("one") + &turn("branched")));
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["branched"]);
}

#[test]
fn a_copy_written_in_pieces_is_still_told() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("p.jsonl"), dir.path().join("f.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    read_in(&mut files, &mut seen, &parent);
    append(&parent, &(turn("one") + &turn("two")));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["one", "two"]);

    append(&fork, &turn("one"));
    assert!(read_in(&mut files, &mut seen, &fork).is_empty());
    append(&fork, &turn("two"));
    assert!(read_in(&mut files, &mut seen, &fork).is_empty());
    append(&fork, &turn("three"));
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["three"]);
}

#[test]
fn a_fork_rewritten_before_a_turn_of_its_own_reports_none_of_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("p.jsonl"), dir.path().join("f.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    read_in(&mut files, &mut seen, &parent);
    append(&parent, &(turn("one") + &turn("two")));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["one", "two"]);
    append(&fork, &(turn("one") + &turn("two")));
    assert!(read_in(&mut files, &mut seen, &fork).is_empty());

    // Rewritten (first line changed) with the copy's tail and a turn of its own.
    std::fs::write(&fork, turn("two") + &turn("three")).unwrap();
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["three"]);
}

#[test]
fn a_rewritten_fork_can_extend_the_copy_past_its_previous_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("parent"), dir.path().join("fork"));
    let mut files = Files::new(SystemTime::UNIX_EPOCH);
    let mut seen = Seen::default();
    append(&parent, &("marker\n".to_owned() + &turn("old")));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["old"]);
    append(&fork, "marker\n");
    assert!(read_in(&mut files, &mut seen, &fork).is_empty());
    std::fs::write(
        &fork,
        HEADER.to_owned() + "marker\n" + &turn("old") + &turn("new"),
    )
    .unwrap();
    assert_eq!(read_in(&mut files, &mut seen, &fork), ["new"]);
}

#[test]
fn a_new_session_reports_all_its_turns() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, new) = (dir.path().join("p.jsonl"), dir.path().join("n.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    read_in(&mut files, &mut seen, &parent);
    append(&parent, &turn("one"));
    assert_eq!(read_in(&mut files, &mut seen, &parent), ["one"]);

    read_in(&mut files, &mut seen, &new);
    append(&new, &(turn("first") + &turn("second")));
    assert_eq!(read_in(&mut files, &mut seen, &new), ["first", "second"]);
}

#[test]
fn a_transcript_dropped_from_the_kept_ones_goes_on_where_it_was_left() {
    let dir = tempfile::tempdir().unwrap();
    let (mut files, mut seen) = (since_now(), Seen::default());
    let first = dir.path().join("t0.jsonl");
    read_in(&mut files, &mut seen, &first);
    append(&first, &turn("one"));
    assert_eq!(read_in(&mut files, &mut seen, &first), ["one"]);
    for n in 1..=MAX_WATCHED {
        let path = dir.path().join(format!("t{n}.jsonl"));
        read_in(&mut files, &mut seen, &path);
    }
    assert!(files.get_mut(&first).is_none(), "dropped");

    append(&first, &turn("two"));
    assert_eq!(read_in(&mut files, &mut seen, &first), ["two"]);
}

#[test]
fn an_evicted_transcript_with_a_newline_at_the_old_offset_cannot_replay_turns() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let (mut files, mut seen) = (Files::new(SystemTime::UNIX_EPOCH), Seen::default());
    read_in(&mut files, &mut seen, &first);
    append(&first, &(turn("one") + &"x".repeat(128)));
    assert_eq!(read_in(&mut files, &mut seen, &first), ["one"]);

    // Replace the file after reading but before eviction. Both versions have a
    // newline at the saved offset, and the tail read ahead into a partial record.
    std::fs::write(&first, turn("two") + &turn("one")).unwrap();
    for i in 0..MAX_WATCHED {
        read_in(&mut files, &mut seen, &dir.path().join(i.to_string()));
    }
    assert!(read_in(&mut files, &mut seen, &first).is_empty());
    append(&first, &turn("three"));
    assert_eq!(read_in(&mut files, &mut seen, &first), ["three"]);
}

#[test]
fn directory_entries_that_yield_no_offset_spend_the_launch_scan_budget() {
    let first = tempfile::tempdir().unwrap();
    std::fs::create_dir(first.path().join("subdirectory")).unwrap();
    let next = tempfile::tempdir().unwrap();
    let path = next.path().join("session");
    append(&path, &turn("old"));
    let mut files = Files::new(SystemTime::now() + Duration::from_secs(60));
    files.entries_left = 1;
    files.list(first.path());
    files.list(next.path());

    append(&path, &turn("before first lookup"));
    let mut seen = Seen::default();
    assert!(read_in(&mut files, &mut seen, &path).is_empty());
    append(&path, &turn("after first lookup"));
    assert_eq!(
        read_in(&mut files, &mut seen, &path),
        ["after first lookup"]
    );
}

#[test]
fn evictions_bound_saved_offsets_and_a_forgotten_file_resumes_at_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Files::new(SystemTime::UNIX_EPOCH);
    // No directory entries to baseline. All saved offsets come from evictions.
    for i in 0..MAX_LISTED + MAX_WATCHED {
        files
            .watch(
                &dir.path().join(i.to_string()),
                &ENDS,
                Capacity::new(limits()),
                &Seen::default(),
            )
            .unwrap();
    }
    let path = dir.path().join("session");
    let mut seen = Seen::default();
    read_in(&mut files, &mut seen, &path);
    append(&path, &turn("already reported"));
    assert_eq!(read_in(&mut files, &mut seen, &path), ["already reported"]);
    for i in 0..MAX_WATCHED {
        files
            .watch(
                &dir.path().join(format!("other{i}")),
                &ENDS,
                Capacity::new(limits()),
                &Seen::default(),
            )
            .unwrap();
    }
    assert_eq!(files.known.len(), MAX_LISTED);
    append(&path, &turn("while away"));
    assert!(read_in(&mut files, &mut seen, &path).is_empty());
    append(&path, &turn("after return"));
    assert_eq!(read_in(&mut files, &mut seen, &path), ["after return"]);
}

/// A rewrite that dropped the turn end handled last and kept the line before it (end
/// records all alike): what follows that line is new, though a later turn end has the
/// same text.
#[test]
fn a_turn_end_dropped_by_a_rewrite_is_placed_after_the_line_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &turn("one"));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one"]);

    // Turn one's end dropped, turn two written whole: one read sees both.
    let reply_one = "{\"type\":\"reply\",\"text\":\"one\"}\n";
    std::fs::write(&path, format!("{reply_one}{}", turn("two"))).unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && !report.rebaselined, "{report:?}");
    assert_eq!(texts(&report), ["two"]);
    append(&path, &turn("three"));
    assert_eq!(replies(&mut w), ["three"]);
}

/// The same, the rewrite first caught empty (truncated, then written).
#[test]
fn a_turn_end_dropped_by_a_rewrite_caught_empty_is_placed_after_the_line_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, &turn("one"));
    let mut w = from_start(&path);
    assert_eq!(replies(&mut w), ["one"]);

    std::fs::write(&path, "").unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(
        report.rewritten && report.batch.turns.is_empty(),
        "{report:?}"
    );
    let reply_one = "{\"type\":\"reply\",\"text\":\"one\"}\n";
    std::fs::write(&path, format!("{reply_one}{}", turn("two"))).unwrap();
    assert_eq!(replies(&mut w), ["two"]);
}

const P1: &str = "{\"type\":\"user\",\"text\":\"one\"}\n";
const R1: &str = "{\"type\":\"reply\",\"text\":\"first answer\"}\n";
const END: &str = "{\"type\":\"turn_end\"}\n";
const P2: &str = "{\"type\":\"user\",\"text\":\"two\"}\n";
const R2: &str = "{\"type\":\"reply\",\"text\":\"second answer\"}\n";
const P3: &str = "{\"type\":\"user\",\"text\":\"three\"}\n";
const T3: &str = "{\"type\":\"tool\"}\n";
const R3: &str = "{\"type\":\"reply\",\"text\":\"after tool\"}\n";

/// An in-place rewriting writer's file after each of its writes: a turn's prompt, its
/// reply and an end record (all end records alike); at the next turn's first write it
/// truncates the file (once caught empty) and writes it again without the earlier end.
fn rewriting_versions() -> Vec<String> {
    vec![
        P1.into(),
        [P1, R1, END].concat(),
        String::new(),
        [P1, R1, P2].concat(),
        [P1, R1, P2, R2, END].concat(),
        [P1, R1, P2, R2, P3].concat(),
        [P1, R1, P2, R2, P3, T3].concat(),
        [P1, R1, P2, R2, P3, T3, R3].concat(),
        [P1, R1, P2, R2, P3, T3, R3, END].concat(),
    ]
}

/// Each turn is reported once, whichever versions the reads see, also when no read saw a
/// turn's end record before the next turn's first write dropped it (the next prompt shows
/// the end then).
#[test]
fn in_place_rewrites_report_each_turn_once() {
    let versions = rewriting_versions();
    let read_after: [&[usize]; 9] = [
        &[0, 1, 2, 3, 4, 5, 6, 7, 8],
        &[1, 4, 8],
        &[1, 2, 4, 5, 8],
        &[0, 1, 3, 4, 6, 8],
        &[1, 4, 5, 7, 8],
        &[0, 3, 4, 8],
        &[0, 3, 6, 8],
        &[0, 8],
        &[3, 8],
    ];
    for picks in read_after {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut w = from_start(&path);
        let mut seen = Seen::default();
        let mut all = Vec::new();
        for &at in picks {
            std::fs::write(&path, &versions[at]).unwrap();
            all.extend(texts(&read_all(&mut w, &ENDS, &mut seen)));
        }
        assert_eq!(
            all,
            ["first answer", "second answer", "after tool"],
            "reads after versions {picks:?}"
        );
    }
}

#[test]
fn evidence_tells_an_end_record_from_a_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    std::fs::write(&path, [P1, R1, END, P2, R2, P3].concat()).unwrap();
    let mut w = from_start(&path);
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    let evidence: Vec<AmbientEvidence> =
        report.batch.turns.iter().map(|t| t.turn.evidence).collect();
    assert_eq!(
        evidence,
        [
            AmbientEvidence::ExplicitEnd,
            AmbientEvidence::InferredFromNextRecord
        ]
    );
    assert_eq!(texts(&report), ["first answer", "second answer"]);
}

/// A turn without an end record: ends once the file has been quiet for the window — not
/// before — and only once.
#[test]
fn a_quiet_turn_end_waits_for_the_window_and_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    std::fs::write(&path, [P1, R1].concat()).unwrap();
    let mut w = from_start_with(&path, &QUIET);
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    assert!(report.batch.turns.is_empty(), "{report:?}");
    w.quiet(Duration::from_secs(3600), false, &mut report);
    assert!(
        report.batch.turns.is_empty(),
        "the window has not passed: {report:?}"
    );
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["first answer"]);
    assert_eq!(
        report.batch.turns[0].turn.evidence,
        AmbientEvidence::InferredAfterQuiet
    );
    let mut again = Report::default();
    w.quiet(Duration::ZERO, false, &mut again);
    assert!(again.batch.turns.is_empty(), "{again:?}");
}

/// Forced (the agent left the transcript): the window is not waited for.
#[test]
fn a_forced_quiet_turn_end_does_not_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    std::fs::write(&path, [P1, R1].concat()).unwrap();
    let mut w = from_start_with(&path, &QUIET);
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    w.quiet(Duration::from_secs(3600), true, &mut report);
    assert_eq!(texts(&report), ["first answer"]);
    assert_eq!(
        report.batch.turns[0].turn.evidence,
        AmbientEvidence::InferredAtTranscriptEnd
    );
}

#[test]
fn pi_waits_for_a_partial_tool_result_before_inferring_a_reply() {
    use crate::agent::{AgentProfile, profiles::PiProfile};

    let source = PiProfile::new().ambient_turns().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pi.jsonl");
    let mut w = from_start_with(&path, &*source);
    let mut seen = Seen::default();
    let answer = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"early"}]}}"#;
    let tool_result = r#"{"type":"message","message":{"role":"toolResult"}}"#;
    append(&path, &format!("{answer}\n{tool_result}"));

    let mut report = read_all(&mut w, &*source, &mut seen);
    for force in [false, true] {
        w.quiet(Duration::ZERO, force, &mut report);
        assert!(report.batch.turns.is_empty(), "force={force}: {report:?}");
    }
    append(&path, "\n");
    let mut report = read_all(&mut w, &*source, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");

    append(&path, &format!("{}\n", answer.replace("early", "final")));
    let mut report = read_all(&mut w, &*source, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["final"]);
}

/// A pass that stopped part-way is not quiet: the parser is not asked.
#[test]
fn a_pass_with_more_to_read_is_never_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    std::fs::write(&path, [P1, R1, P2].concat()).unwrap();
    let mut options = limits();
    options.records = 2;
    let tail = Tail::new(path.clone());
    let mut w = Watched::new(0, path, tail, QUIET.parser(), Capacity::new(options), true);
    let mut report = w.read(&QUIET, &mut Seen::default());
    assert!(report.more, "{report:?}");
    w.quiet(Duration::ZERO, true, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
    let report = w.read(&QUIET, &mut Seen::default());
    assert_eq!(texts(&report), ["first answer"]);
    assert_eq!(
        report.batch.turns[0].turn.evidence,
        AmbientEvidence::InferredFromNextRecord
    );
}

const HEADER: &str = "{\"type\":\"session\",\"id\":\"parent\"}\n";
const FORK_HEADER: &str = "{\"type\":\"session\",\"id\":\"fork\"}\n";
const FORK_PROMPT: &str = "{\"type\":\"user\",\"text\":\"more\",\"id\":\"f1\"}\n";
const FORK_FINAL: &str = "{\"type\":\"reply\",\"text\":\"forked answer\",\"id\":\"f2\"}\n";

/// A parent whose first turn was reported once quiet, and a fork of it (the entries copied
/// byte for byte under another header) to write at the returned path.
fn quiet_parent_and_fork(dir: &Path) -> (Files, Seen, PathBuf, String) {
    let (parent, fork) = (dir.join("p.jsonl"), dir.join("f.jsonl"));
    let (mut files, mut seen) = (since_now(), Seen::default());
    let w = files
        .watch(&parent, &QUIET, Capacity::new(limits()), &seen)
        .unwrap();
    std::fs::write(&parent, [HEADER, P1, R1].concat()).unwrap();
    let mut report = read_all(w, &QUIET, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["first answer"]);
    (files, seen, fork, [FORK_HEADER, P1, R1].concat())
}

#[test]
fn pi_metadata_after_a_copied_reply_keeps_its_origin() {
    use crate::agent::{AgentProfile, profiles::PiProfile};
    let source = PiProfile::new().ambient_turns().unwrap();
    let prompt = r#"{"type":"message","message":{"role":"user","content":[]}}"#;
    let answer = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"parent answer"}]}}"#;
    let metadata = r#"{"type":"model_change","modelId":"fresh"}"#;
    for next_prompt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let fork = dir.path().join("fork");
        let mut files = Files::new(SystemTime::UNIX_EPOCH);
        let mut seen = Seen::default();
        let copied = format!("{prompt}\n{answer}\n");
        let w = files
            .watch(&parent, &*source, Capacity::new(limits()), &seen)
            .unwrap();
        append(&parent, &(HEADER.to_owned() + &copied));
        let mut report = read_all(w, &*source, &mut seen);
        w.quiet(Duration::ZERO, false, &mut report);
        assert_eq!(texts(&report), ["parent answer"]);
        let w = files
            .watch(&fork, &*source, Capacity::new(limits()), &seen)
            .unwrap();
        append(&fork, &(FORK_HEADER.to_owned() + &copied));
        assert!(texts(&read_all(w, &*source, &mut seen)).is_empty());
        append(&fork, &format!("{metadata}\n"));
        if next_prompt {
            append(&fork, &format!("{prompt}\n"));
        }
        let mut report = read_all(w, &*source, &mut seen);
        w.quiet(Duration::ZERO, false, &mut report);
        assert!(texts(&report).is_empty(), "{report:?}");
        append(
            &fork,
            &format!(
                "{}\n{metadata}\n",
                answer.replace("parent answer", "own answer")
            ),
        );
        let mut report = read_all(w, &*source, &mut seen);
        w.quiet(Duration::ZERO, false, &mut report);
        assert_eq!(texts(&report), ["own answer"]);
    }
}

#[test]
fn a_quiet_turn_end_in_copied_history_is_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (mut files, mut seen, fork, copy) = quiet_parent_and_fork(dir.path());
    std::fs::write(&fork, &copy).unwrap();
    let w = files
        .watch(&fork, &QUIET, Capacity::new(limits()), &seen)
        .unwrap();
    let mut report = read_all(w, &QUIET, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");

    append(&fork, &[FORK_PROMPT, FORK_FINAL].concat());
    let w = files
        .watch(&fork, &QUIET, Capacity::new(limits()), &seen)
        .unwrap();
    let mut report = read_all(w, &QUIET, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["forked answer"]);
}

/// The fork's own first prompt ends no turn: the final message before it is copied
/// history — by no path, quiet or forced, is it reported again.
#[test]
fn a_forks_first_prompt_does_not_report_its_copied_final_message() {
    let dir = tempfile::tempdir().unwrap();
    let (mut files, mut seen, fork, copy) = quiet_parent_and_fork(dir.path());
    std::fs::write(&fork, &copy).unwrap();
    let w = files
        .watch(&fork, &QUIET, Capacity::new(limits()), &seen)
        .unwrap();
    assert!(read_all(w, &QUIET, &mut seen).batch.turns.is_empty());
    append(&fork, FORK_PROMPT);
    let mut report = read_all(w, &QUIET, &mut seen);
    assert!(report.batch.turns.is_empty(), "{report:?}");
    w.quiet(Duration::ZERO, true, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
    append(&fork, FORK_FINAL);
    let mut report = read_all(w, &QUIET, &mut seen);
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["forked answer"]);
    let mut again = Report::default();
    w.quiet(Duration::ZERO, true, &mut again);
    assert!(again.batch.turns.is_empty(), "{again:?}");
}

#[test]
fn a_forks_copy_and_first_prompt_read_at_once_report_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut files, mut seen, fork, copy) = quiet_parent_and_fork(dir.path());
    std::fs::write(&fork, format!("{copy}{FORK_PROMPT}")).unwrap();
    let w = files
        .watch(&fork, &QUIET, Capacity::new(limits()), &seen)
        .unwrap();
    let mut report = read_all(w, &QUIET, &mut seen);
    w.quiet(Duration::ZERO, true, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
}

/// A rewrite that keeps the final message reported once quiet: not reported again, by
/// quiet or by the next prompt.
#[test]
fn a_quiet_turn_end_is_not_reported_again_after_a_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    std::fs::write(&path, [HEADER, P1, R1].concat()).unwrap();
    let mut w = from_start_with(&path, &QUIET);
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["first answer"]);

    // The header rewritten (another first line), the entries kept.
    std::fs::write(&path, [FORK_HEADER, P1, R1].concat()).unwrap();
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    assert!(report.rewritten, "{report:?}");
    w.quiet(Duration::ZERO, false, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
    append(&path, FORK_PROMPT);
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    w.quiet(Duration::ZERO, true, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
}

/// The same, the rewrite and the next prompt read at once.
#[test]
fn a_prompt_after_a_reported_final_message_reports_nothing_after_a_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.jsonl");
    std::fs::write(&path, [HEADER, P1, R1].concat()).unwrap();
    let mut w = from_start_with(&path, &QUIET);
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    w.quiet(Duration::ZERO, false, &mut report);
    assert_eq!(texts(&report), ["first answer"]);

    std::fs::write(&path, [FORK_HEADER, P1, R1, FORK_PROMPT].concat()).unwrap();
    let mut report = read_all(&mut w, &QUIET, &mut Seen::default());
    assert!(report.rewritten, "{report:?}");
    w.quiet(Duration::ZERO, true, &mut report);
    assert!(report.batch.turns.is_empty(), "{report:?}");
}

#[test]
fn an_unrecognised_record_is_flagged_until_a_turn_ends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    append(&path, "{\"type\":\"reply\"}\n");
    let mut w = from_start(&path);
    assert!(replies(&mut w).is_empty());
    assert!(w.unrecognised);
    append(&path, &turn("fine"));
    assert_eq!(replies(&mut w), ["fine"]);
    assert!(!w.unrecognised);
}

#[test]
fn an_oversized_line_is_counted_and_the_turns_around_it_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut options = limits();
    options.line_bytes = 64;
    options.read_bytes = 32;
    let huge = format!("{{\"type\":\"reply\",\"text\":\"{}\"}}\n", "z".repeat(500));
    append(&path, &(turn("before") + &huge + END + &turn("after")));
    let tail = Tail::new(path.clone());
    let mut w = Watched::new(0, path, tail, ENDS.parser(), Capacity::new(options), true);
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert_eq!(report.oversized, 1);
    // The oversized reply was never parsed: its turn ends without text.
    let replies: Vec<Option<String>> = report
        .batch
        .turns
        .iter()
        .map(|t| t.turn.reply.as_ref().map(|r| r.text.clone()))
        .collect();
    assert_eq!(replies, [Some("before".into()), None, Some("after".into())]);
}

#[test]
fn a_reply_is_cut_to_the_limit_and_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut options = limits();
    options.reply_bytes = 5;
    append(&path, &turn("héllo wörld"));
    append(&path, &turn("   "));
    let tail = Tail::new(path.clone());
    let mut w = Watched::new(0, path, tail, ENDS.parser(), Capacity::new(options), true);
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    let first = report.batch.turns[0].turn.reply.as_ref().unwrap();
    assert_eq!(first.text, "héll");
    assert!(first.truncated);
    assert!(
        report.batch.turns[1].turn.reply.is_none(),
        "blank text is no reply"
    );
}

/// A parser that keeps more than the limit is cut by the observer.
#[test]
fn an_overlong_parser_reply_is_cut_by_the_observer() {
    let reply = TurnReply {
        text: "ab€cd".into(),
        ..TurnReply::default()
    };
    let cut = bounded(reply, 3).unwrap();
    assert_eq!(cut.text, "ab");
    assert!(cut.truncated);
    let blank = TurnReply {
        text: " \n".into(),
        ..TurnReply::default()
    };
    assert!(bounded(blank, 10).is_none());
}

#[test]
fn turns_held_beyond_the_queue_while_placing_a_rewrite_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut options = limits();
    options.queue_items = 2;
    append(&path, &turn_n("one", 1));
    let tail = Tail::new(path.clone());
    let mut w = Watched::new(
        0,
        path.clone(),
        tail,
        ENDS.parser(),
        Capacity::new(options),
        true,
    );
    assert_eq!(
        texts(&read_all(&mut w, &ENDS, &mut Seen::default())),
        ["one"]
    );
    // Rewritten (another first line) with three new turns after the anchor.
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"header\"}}\n{}{}{}{}",
            turn_n("one", 1),
            turn_n("a", 2),
            turn_n("b", 3),
            turn_n("c", 4)
        ),
    )
    .unwrap();
    let report = read_all(&mut w, &ENDS, &mut Seen::default());
    assert!(report.rewritten && report.batch.overflow, "{report:?}");
    assert_eq!(texts(&report), ["a", "b"]);
}

/// Many turns over small budgets: each reported once, in order, across passes.
#[test]
fn bounded_passes_report_every_turn_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    let mut options = limits();
    options.read_bytes = 7;
    options.records = 3;
    let all: String = (0..40).map(|n| turn_n(&format!("r{n}"), n)).collect();
    append(&path, &all);
    let tail = Tail::new(path.clone());
    let mut w = Watched::new(0, path, tail, ENDS.parser(), Capacity::new(options), true);
    let mut seen = Seen::default();
    let mut passes = 0;
    let mut got = Vec::new();
    loop {
        let report = w.read(&ENDS, &mut seen);
        passes += 1;
        got.extend(texts(&report));
        if !report.more {
            break;
        }
    }
    let want: Vec<String> = (0..40).map(|n| format!("r{n}")).collect();
    assert_eq!(got, want);
    assert!(passes > 40, "{passes} passes");
}

#[test]
fn a_rewrite_during_baseline_keying_cannot_replay_history() {
    for prefix in ["", "header\n"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let old = format!("{prefix}{}\n{}", "x".repeat(100), turn("history"));
        std::fs::write(&path, &old).unwrap();
        let tail = Tail::from(path.clone(), old.len() as u64).unwrap();
        let options = TurnOptions {
            read_bytes: 16,
            ..limits()
        };
        let mut watched = Watched::new(
            0,
            path.clone(),
            tail,
            ENDS.parser(),
            Capacity::new(options),
            false,
        );
        let mut seen = Seen::default();
        assert!(watched.read(&ENDS, &mut seen).more);
        std::fs::write(&path, old.replace('x', "y")).unwrap();
        let report = read_all(&mut watched, &ENDS, &mut seen);
        assert!(
            report.rebaselined && report.batch.turns.is_empty(),
            "{report:?}"
        );
        append(&path, &turn("new"));
        assert_eq!(texts(&read_all(&mut watched, &ENDS, &mut seen)), ["new"]);
    }
}

#[test]
fn a_truncation_during_baseline_keying_uses_the_shorter_eof() {
    for (read_bytes, records) in [(16, limits().records), (limits().read_bytes, 1)] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let kept = format!("header\n{}", turn_n("kept history", 1));
        let old = format!("{kept}{}", turn_n(&"x".repeat(256), 2));
        std::fs::write(&path, &old).unwrap();
        let tail = Tail::from(path.clone(), old.len() as u64).unwrap();
        let options = TurnOptions {
            read_bytes,
            records,
            ..limits()
        };
        let mut watched = Watched::new(
            0,
            path.clone(),
            tail,
            ENDS.parser(),
            Capacity::new(options),
            false,
        );
        let mut seen = Seen::default();
        assert!(watched.read(&ENDS, &mut seen).more);

        // Keep the same prefix. With a small byte budget, every byte already read
        // still matches. With a small record budget, the buffer contains stale history.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(kept.len() as u64)
            .unwrap();
        let report = watched.read(&ENDS, &mut seen);
        assert!(report.rebaselined && report.more, "{report:?}");
        assert!(report.batch.turns.is_empty(), "{report:?}");

        // Append before the scan reaches the shorter EOF, and stay below the old one.
        let fresh = turn_n("fresh", 3);
        assert!(kept.len() + fresh.len() < old.len());
        append(&path, &fresh);
        assert_eq!(texts(&read_all(&mut watched, &ENDS, &mut seen)), ["fresh"]);
        assert!(
            read_all(&mut watched, &ENDS, &mut seen)
                .batch
                .turns
                .is_empty()
        );
    }
}

#[test]
fn direct_and_possible_fork_turns_keep_only_the_prefix_that_fits() {
    for may_copy in [false, true] {
        for (items, bytes) in [(1, 16), (4, 3)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session");
            append(
                &path,
                &(turn_n("ab", 1) + &turn_n("cd", 2) + &turn_n("e", 3)),
            );
            let limits = TurnOptions {
                queue_items: items,
                queue_bytes: bytes,
                reply_bytes: 3,
                ..limits()
            };
            let mut watched = Watched::new(
                0,
                path.clone(),
                Tail::new(path),
                ENDS.parser(),
                Capacity::new(limits),
                may_copy,
            );
            let report = watched.read(&ENDS, &mut Seen::default());
            assert!(report.batch.overflow && !report.more);
            assert_eq!(texts(&report), ["ab"]);
        }
    }
}

#[test]
fn a_fork_after_the_digest_limit_starts_at_its_observed_end() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let fork = dir.path().join("fork");
    let mut files = Files::new(SystemTime::UNIX_EPOCH);
    let mut seen = Seen::default();
    let pool = Capacity::new(TurnOptions {
        records: 3,
        ..limits()
    });
    let first = turn("old reply");
    let history = (0..MAX_SEEN).fold(first.clone(), |mut text, i| {
        use std::fmt::Write;
        writeln!(text, "line {i}").unwrap();
        text
    });
    append(&parent, &history);
    let original = files.watch(&parent, &ENDS, pool.clone(), &seen).unwrap();
    assert_eq!(texts(&read_all(original, &ENDS, &mut seen)), ["old reply"]);
    // Even a fork from an early point, with no retained overlap, must not replay.
    append(&fork, &(first + &turn("also present at discovery")));
    let forked = files.watch(&fork, &ENDS, pool.clone(), &seen).unwrap();
    assert!(read_all(forked, &ENDS, &mut seen).batch.turns.is_empty());
    append(&fork, &turn("new"));
    assert_eq!(texts(&read_all(forked, &ENDS, &mut seen)), ["new"]);

    let empty = dir.path().join("empty");
    std::fs::write(&empty, "").unwrap();
    let watched = files.watch(&empty, &ENDS, pool, &seen).unwrap();
    assert!(watched.read(&ENDS, &mut seen).batch.turns.is_empty());
    append(&empty, &turn("first append"));
    assert_eq!(
        texts(&read_all(watched, &ENDS, &mut seen)),
        ["first append"]
    );
}

#[test]
fn copied_prefixes_do_not_reserve_queue_capacity_across_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (parent, fork) = (dir.path().join("parent"), dir.path().join("fork"));
    let pool = Capacity::new(TurnOptions {
        queue_items: 1,
        records: 1,
        ..limits()
    });
    let mut files = Files::new(SystemTime::UNIX_EPOCH);
    let mut seen = Seen::default();
    let history = turn("first") + &turn("second");
    append(&parent, &history);
    read_in(&mut files, &mut seen, &parent);
    let watched = files.watch(&fork, &ENDS, pool, &seen).unwrap();
    append(&fork, &(history + &turn("keep")));
    let report = read_all(watched, &ENDS, &mut seen);
    assert!(!report.batch.overflow);
    assert_eq!(texts(&report), ["keep"]);
}

#[test]
fn cursor_sessions_with_a_common_reply_keep_every_turn_after_divergence() {
    use crate::agent::profiles::CursorProfile;
    let source = CursorProfile.ambient_turns_with_unseparated_text();
    let message = |role, text| {
        format!(
            "{}\n",
            serde_json::json!({"role":role,"message":{"content":[{"type":"text","text":text}]}})
        )
    };
    let turn = |prompt, reply| {
        message("user", prompt)
            + &message("assistant", reply)
            + "{\"type\":\"turn_ended\",\"status\":\"success\"}\n"
    };
    for records in [1, 100] {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::new(SystemTime::UNIX_EPOCH);
        let mut seen = Seen::default();
        let parent = dir.path().join("first");
        append(
            &parent,
            &(turn("common history", "old") + &turn("first question", "OK")),
        );
        assert_eq!(
            read_in_with(&mut files, &mut seen, &parent, &*source),
            ["old", "OK"]
        );
        for (name, prefix) in [
            ("independent", String::new()),
            ("fork", turn("common history", "old")),
        ] {
            let path = dir.path().join(name);
            let watched = files
                .watch(
                    &path,
                    &*source,
                    Capacity::new(TurnOptions {
                        records,
                        ..limits()
                    }),
                    &seen,
                )
                .unwrap();
            append(
                &path,
                &(prefix + &turn(name, "unique") + &turn("another question", "OK")),
            );
            assert_eq!(
                texts(&read_all(watched, &*source, &mut seen)),
                ["unique", "OK"]
            );
        }
    }
}

#[test]
fn cursor_reports_identical_replies_across_successive_rewrites() {
    use crate::agent::profiles::CursorProfile;
    let source = CursorProfile.ambient_turns_with_unseparated_text();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut watched = from_start_with(&path, &*source);
    watched.capacity.limits.records = 1;
    let mut seen = Seen::default();
    let prompt = r#"{"role":"user","message":{"content":[{"type":"text","text":"repeat"}]}}"#;
    let reply = r#"{"role":"assistant","message":{"content":[{"type":"text","text":"OK"}]}}"#;
    let end = r#"{"type":"turn_ended","status":"success"}"#;
    let mut history = String::new();
    for _ in 0..3 {
        history.push_str(&format!("{prompt}\n{reply}\n"));
        std::fs::write(&path, format!("{history}{end}\n")).unwrap();
        assert_eq!(texts(&read_all(&mut watched, &*source, &mut seen)), ["OK"]);
        assert!(texts(&read_all(&mut watched, &*source, &mut seen)).is_empty());
    }
}

#[test]
fn a_reformatted_end_after_the_original_reply_still_counts_as_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    std::fs::write(&path, [P1, R1, END].concat()).unwrap();
    let mut watched = from_start(&path);
    assert_eq!(replies(&mut watched), ["first answer"]);
    std::fs::write(&path, [P1, R1, "  ", END, P2, R1, END].concat()).unwrap();
    assert_eq!(replies(&mut watched), ["first answer"]);
}

#[test]
fn metadata_inserted_before_a_reported_marker_does_not_repeat_the_turn() {
    for split_at_metadata in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        std::fs::write(&path, [P1, R1, END].concat()).unwrap();
        let mut watched = from_start(&path);
        watched.capacity = Capacity::new(TurnOptions {
            records: 1,
            queue_items: 1,
            ..limits()
        });
        assert_eq!(replies(&mut watched), ["first answer"]);

        let prefix = [P1, R1, "{\"type\":\"metadata\",\"added\":true}\n"].concat();
        let suffix = [END, P2, R2, END].concat();
        if split_at_metadata {
            std::fs::write(&path, prefix).unwrap();
            assert!(replies(&mut watched).is_empty());
            append(&path, &suffix);
        } else {
            std::fs::write(&path, prefix + &suffix).unwrap();
        }
        let report = read_all(&mut watched, &ENDS, &mut Seen::default());
        assert!(!report.batch.overflow);
        assert_eq!(texts(&report), ["second answer"]);
        assert!(replies(&mut watched).is_empty());
    }
}

#[test]
fn an_inferred_end_moved_by_metadata_is_history_at_quiet_or_the_next_prompt() {
    for next_prompt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        std::fs::write(&path, [P1, R1].concat()).unwrap();
        let mut seen = Seen::default();
        let mut watched = from_start_with(&path, &QUIET);
        let mut first = read_all(&mut watched, &QUIET, &mut seen);
        watched.quiet(Duration::ZERO, false, &mut first);
        assert_eq!(texts(&first), ["first answer"]);

        let mut rewrite = [P1, "{\"type\":\"metadata\"}\n", R1].concat();
        if next_prompt {
            rewrite.push_str(P2);
        }
        std::fs::write(&path, rewrite).unwrap();
        let mut reread = read_all(&mut watched, &QUIET, &mut seen);
        watched.quiet(Duration::ZERO, false, &mut reread);
        assert!(texts(&reread).is_empty());
        append(&path, R2);
        let mut next = read_all(&mut watched, &QUIET, &mut seen);
        watched.quiet(Duration::ZERO, false, &mut next);
        assert_eq!(texts(&next), ["second answer"]);
    }
}

#[test]
fn a_new_prompt_after_settling_on_the_predecessor_preserves_an_identical_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    std::fs::write(&path, [P1, R1, END].concat()).unwrap();
    let mut watched = from_start(&path);
    assert_eq!(replies(&mut watched), ["first answer"]);
    std::fs::write(&path, [P1, R1].concat()).unwrap();
    assert!(replies(&mut watched).is_empty());
    assert!(replies(&mut watched).is_empty()); // The shorter rewrite has stopped growing.
    append(&path, &[P2, R1, END].concat());
    assert_eq!(replies(&mut watched), ["first answer"]);
}

#[test]
fn staging_a_short_reply_releases_excess_string_capacity() {
    let capacity = Capacity::new(TurnOptions {
        queue_bytes: 2,
        ..limits()
    });
    let mut batch = TurnBatch::default();
    for _ in 0..2 {
        let mut reply = TurnReply::default();
        reply.text.reserve(1024 * 1024);
        reply.text.push('x');
        batch.push(
            AmbientTurn {
                reply: Some(reply),
                evidence: AmbientEvidence::ExplicitEnd,
            },
            &capacity,
        );
    }
    assert!(!batch.overflow);
    let retained: usize = batch
        .turns
        .iter()
        .map(|t| t.turn.reply.as_ref().unwrap().text.capacity())
        .sum();
    assert_eq!(retained, 2);
}

#[test]
fn cursor_rewrites_cannot_publish_a_turn_seen_to_end_unsuccessfully() {
    use crate::agent::profiles::CursorProfile;
    let source = CursorProfile.ambient_turns_with_unseparated_text();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let message = |role, text| {
        format!(
            "{}\n",
            serde_json::json!({"role":role,"message":{"content":[{"type":"text","text":text}]}})
        )
    };
    let partial = message("user", "first") + &message("assistant", "partial reply");
    let metadata = "{\"type\":\"metadata\"}\n{\"role\":\"system\"}\n";
    for (status, inserted) in [("aborted", ""), ("aborted", metadata), ("error", metadata)] {
        let mut seen = Seen::default();
        let mut watched = from_start_with(&path, &*source);
        let marker = serde_json::json!({"type":"turn_ended","status":status});
        std::fs::write(&path, format!("{partial}{marker}\n")).unwrap();
        assert!(texts(&read_all(&mut watched, &*source, &mut seen)).is_empty());
        let rewritten = partial.clone()
            + inserted
            + &message("user", "next")
            + &message("assistant", "new reply");
        std::fs::write(
            &path,
            rewritten + "{\"type\":\"turn_ended\",\"status\":\"success\"}\n",
        )
        .unwrap();
        assert_eq!(
            texts(&read_all(&mut watched, &*source, &mut seen)),
            ["new reply"],
            "{status}"
        );
    }
}

#[test]
fn skipped_cursor_tool_records_invalidate_narration_before_the_final_answer() {
    use crate::agent::profiles::CursorProfile;
    let source = CursorProfile.ambient_turns_with_unseparated_text();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut files = Files::new(SystemTime::UNIX_EPOCH);
    let limits = TurnOptions {
        line_bytes: 256,
        ..limits()
    };
    let mut seen = Seen::default();
    let watched = files
        .watch(&path, &*source, Capacity::new(limits), &seen)
        .unwrap();
    for (role, kind, text) in [
        ("assistant", "text", "narration"),
        ("assistant", "tool_use", &"x".repeat(300)),
        ("user", "tool_result", &"y".repeat(300)),
        ("assistant", "text", "final answer"),
    ] {
        let line =
            serde_json::json!({"role":role,"message":{"content":[{"type":kind,"text":text}]}});
        append(&path, &format!("{line}\n"));
    }
    append(&path, "{\"type\":\"turn_ended\",\"status\":\"success\"}\n");
    let report = read_all(watched, &*source, &mut seen);
    assert_eq!(report.oversized, 2);
    assert_eq!(texts(&report), ["final answer"]);
}

#[test]
fn the_old_launch_can_drain_a_late_file_that_predates_its_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut watched = from_start(&path);
    let mut seen = Seen::default();
    assert!(
        read_all(&mut watched, &ENDS, &mut seen)
            .batch
            .turns
            .is_empty()
    );
    append(&path, &turn("last old reply"));
    let replacement = SystemTime::now() + Duration::from_secs(1);
    let fence = watched.final_read(|| Some(replacement)).unwrap();
    if std::fs::metadata(&path).unwrap().created().is_ok() {
        let report = watched.read_through(&ENDS, &mut seen, Some(&fence.unwrap()), |_| Ok(true));
        assert_eq!(texts(&report), ["last old reply"]);
    } else {
        assert!(fence.is_none());
        assert_eq!(
            texts(&read_all(&mut watched, &ENDS, &mut seen)),
            ["last old reply"]
        );
        assert!(watched.final_read(|| Some(replacement)).unwrap().is_some());
    }
}

#[test]
fn losing_an_empty_transcript_after_capturing_its_final_eof_fails_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut watched = from_start(&path);
    let mut seen = Seen::default();
    assert!(watched.final_read(|| None).unwrap().is_none());
    std::fs::write(&path, "").unwrap();
    let fence = watched.final_read(|| None).unwrap().unwrap();
    assert_eq!(fence.end(), 0);
    std::fs::remove_file(&path).unwrap();
    let report = watched.read_through(&ENDS, &mut seen, Some(&fence), |_| Ok(true));
    assert!(report.error.is_some());
}

#[test]
fn replacement_between_eof_capture_and_the_first_drain_pass_fails_the_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut watched = from_start(&path);
    let mut seen = Seen::default();
    append(&path, &turn("old"));
    let fence = watched.final_read(|| None).unwrap().unwrap();
    std::fs::rename(&path, dir.path().join("original")).unwrap();
    append(&path, &turn("new"));
    let report = watched.read_through(&ENDS, &mut seen, Some(&fence), |_| Ok(true));
    assert!(report.error.is_some());
    assert!(texts(&report).is_empty());
}

#[test]
fn a_same_length_rewrite_after_eof_capture_fails_before_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    let mut watched = from_start(&path);
    let mut seen = Seen::default();
    // Keep the first line and the EOF suffix identical in both revisions.
    let original = "{}\n".to_owned() + &turn("old") + &"{}\n".repeat(64);
    std::fs::write(&path, &original).unwrap();
    let fence = watched.final_read(|| None).unwrap().unwrap();
    std::fs::write(&path, original.replace("old", "new")).unwrap();
    // Make the metadata difference deterministic even with coarse file timestamps.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
        .unwrap();
    let report = watched.read_through(&ENDS, &mut seen, Some(&fence), |_| Ok(true));
    assert!(report.error.is_some());
    assert!(report.batch.turns.is_empty());
}

#[test]
fn departure_reads_only_through_its_initial_eof_and_can_resume_afterward() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session");
    append(&path, &turn("present"));
    let mut watched = from_start(&path);
    let fence = watched.final_read(|| None).unwrap().unwrap();
    append(&path, &turn("later"));
    let mut seen = Seen::default();
    let report = watched.read_through(&ENDS, &mut seen, Some(&fence), |_| Ok(true));
    assert!(!report.more);
    assert_eq!(texts(&report), ["present"]);
    assert_eq!(texts(&watched.read(&ENDS, &mut seen)), ["later"]);
}
