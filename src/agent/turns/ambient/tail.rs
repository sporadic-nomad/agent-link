//! [`Tail`]: a bounded, incremental reader of one JSON-lines transcript. Each pass reads at
//! most a byte budget from where the last one stopped and hands over at most a record
//! budget of complete lines, in file order. A partial last line waits for its newline; each
//! byte is scanned for a newline once. A line longer than the line budget is never
//! buffered: its bytes are hashed as they arrive and the line is reported as oversized.
//!
//! A file that shrank below what was read, whose first line changed, or whose last bytes
//! read (up to [`SEAL_CAP`], just before the read position) changed was rewritten or
//! replaced. A file that disappears after it was read counts as rewritten. If the file
//! shrinks below an unfinished baseline, the observed EOF becomes the new baseline.
//! Either restart is announced by [`Feed::Rewritten`]. Each nonblank line carries
//! a [`LineKey`] — its content and its predecessor's — by which the observer finds the
//! lines it handled already in a rewritten file.
//!
//! "From now": lines that end at or before the [baseline](Tail::from) are keyed
//! ([`Feed::Baseline`]) but never handed to a parser. Only the last
//! [`BASELINE_KEY_BYTES`] before the baseline are read for their keys.

use std::fs::File;
use std::hash::{DefaultHasher, Hasher};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::paths::{FileStamp, file_id, open_regular};

/// Bytes of the first line kept to tell a rewrite; a longer line is compared by this
/// prefix.
const FIRST_LINE_CAP: usize = 4096;
/// The last bytes read kept (the *seal*): a file whose bytes just before the read position
/// differ from them was rewritten, though it kept its first line and grew.
const SEAL_CAP: usize = 64;
/// Existing content keyed before the baseline, at most. Older lines are skipped unread.
pub(super) const BASELINE_KEY_BYTES: u64 = 4 * 1024 * 1024;

/// A nonblank line and its predecessor, identified by content and absolute end
/// offsets. Positions distinguish repeated records and work with a partial baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineKey {
    pub(super) hash: u64,
    end: u64,
    prev: Option<(u64, u64)>,
}

impl LineKey {
    fn next(last: Option<Self>, hash: u64, end: u64) -> Self {
        Self {
            hash,
            end,
            prev: last.map(|key| (key.hash, key.end)),
        }
    }

    /// Content identity for copied history or a rewrite that moved the lines.
    pub(super) fn pair(self) -> (Option<u64>, u64) {
        (self.prev.map(|(hash, _)| hash), self.hash)
    }

    /// The pair is still in place, even if whitespace changed the end marker's length.
    pub(super) fn same_occurrence(self, anchor: Self) -> bool {
        self.pair() == anchor.pair() && (self.end == anchor.end || self.prev == anchor.prev)
    }

    pub(super) fn at_predecessor(self, anchor: Self) -> bool {
        anchor.prev == Some((self.hash, self.end))
    }

    /// The same line text, whatever line came before.
    pub(super) fn same_text(&self, other: &LineKey) -> bool {
        self.hash == other.hash
    }

    /// This line's text is the one that came before `next`'s.
    pub(super) fn preceded(&self, next: &LineKey) -> bool {
        next.prev.is_some_and(|(hash, _)| hash == self.hash)
    }
}

/// The file whose EOF was measured at departure. Keeping its handle open also
/// prevents its filesystem ID from being recycled while the drain is in progress.
pub(super) struct FinalRead {
    _file: File,
    stamp: FileStamp,
    suffix: Vec<u8>,
}

impl FinalRead {
    pub(super) fn capture(mut file: File) -> io::Result<Self> {
        let stamp = Self::stamp(&file)?;
        let suffix_len = stamp.len.min(SEAL_CAP as u64);
        file.seek(SeekFrom::Start(stamp.len - suffix_len))?;
        let mut suffix = vec![0; suffix_len as usize];
        file.read_exact(&mut suffix)?;
        if Self::stamp(&file)? != stamp {
            return Err(io::Error::other("transcript changed while capturing EOF"));
        }
        Ok(Self {
            _file: file,
            stamp,
            suffix,
        })
    }

    fn stamp(file: &File) -> io::Result<FileStamp> {
        FileStamp::of(file)
            .ok_or_else(|| io::Error::other("cannot verify final transcript revision"))
    }

    pub(super) fn end(&self) -> u64 {
        self.stamp.len
    }

    fn verify(&self, file: &mut File) -> io::Result<FileStamp> {
        let now = Self::stamp(file)?;
        if now.len < self.end()
            || (now.len == self.end() && now != self.stamp)
            || !matches_suffix(file, self.end(), &self.suffix)?
        {
            return Err(io::Error::other(
                "final transcript changed after EOF capture",
            ));
        }
        Ok(now)
    }
}

/// What one pass may do.
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    /// New bytes read from the file, at most.
    pub(super) bytes: usize,
    /// Complete lines handed over (blank ones included), at most.
    pub(super) records: usize,
    /// A longer line (without its newline) is not buffered.
    pub(super) line_bytes: usize,
}

/// What [`Tail::read_pass`] hands over, in file order.
#[derive(Debug)]
pub(super) enum Feed<'a> {
    /// The file was rewritten (or went away).
    Rewritten(Rewrite),
    /// A nonblank line that ended at or before the baseline: keyed, not new.
    Baseline {
        key: LineKey,
        bytes: Option<&'a [u8]>,
    },
    /// A complete, nonblank line within the line budget, without its newline and trimmed
    /// of surrounding ASCII whitespace. `index` numbers lines from where reading began,
    /// blank and oversized ones included.
    Line {
        index: u64,
        key: LineKey,
        bytes: &'a [u8],
    },
    /// A complete line longer than the line budget; its bytes were not kept.
    Oversized { index: u64, key: LineKey },
}

#[derive(Debug)]
pub(super) enum Rewrite {
    /// Restart at the first line. `read` includes the original baseline when only
    /// part of it was read.
    Replay {
        read: u64,
        unfinished_baseline: bool,
    },
    /// The shortened file's EOF is the new baseline.
    Rebaseline { skipped_prefix: bool },
}

/// The line being read is longer than the buffer may hold.
struct Long {
    /// `None`: the unkeyed rest of a line that began before the baseline window.
    hasher: Option<DefaultHasher>,
}

/// Reads one transcript file incrementally (see the module docs). A missing file reads as
/// empty.
pub(super) struct Tail {
    /// A file existed at discovery or on a later read, even if it was empty.
    pub(super) observed: bool,
    path: PathBuf,
    /// Bytes read from the file so far.
    offset: u64,
    /// The file offset right after the last complete line taken.
    consumed: u64,
    /// Bytes ending at `consumed`, kept separately from read-ahead at `offset`.
    consumed_suffix: Vec<u8>,
    /// Bytes `consumed..offset`, unless `long` takes them.
    buf: Vec<u8>,
    /// The leading bytes of `buf` already scanned: none of them is a newline.
    scanned: usize,
    long: Option<Long>,
    /// Complete lines ending at or before this offset are keyed only.
    baseline: u64,
    /// The index the next complete line gets.
    next_index: u64,
    /// The file's first line (at most [`FIRST_LINE_CAP`] bytes), once complete.
    first_line: Option<Vec<u8>>,
    /// The opened file version whose first line was last checked.
    header_version: Option<FileStamp>,
    /// The last nonblank line taken.
    last_key: Option<LineKey>,
    /// The last bytes read (at most [`SEAL_CAP`]), ending at `offset`.
    seal: Vec<u8>,
    /// Test hook: read calls left before one fails.
    #[cfg(test)]
    pub(super) fail_reads_after: Option<usize>,
}

impl Tail {
    /// A reader at the start of `path`: everything it holds or will hold is new.
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            observed: false,
            offset: 0,
            consumed: 0,
            consumed_suffix: Vec::new(),
            buf: Vec::new(),
            scanned: 0,
            long: None,
            baseline: 0,
            next_index: 0,
            first_line: None,
            header_version: None,
            last_key: None,
            seal: Vec::new(),
            #[cfg(test)]
            fail_reads_after: None,
        }
    }

    /// A reader of `path` whose lines ending at or before `baseline` are old. A line still
    /// incomplete at `baseline` is new once it ends. At most [`BASELINE_KEY_BYTES`] before
    /// the baseline are read, starting after the first line boundary in that window.
    pub(super) fn from(path: PathBuf, baseline: u64) -> io::Result<Self> {
        let mut tail = Self::new(path);
        let start = baseline.saturating_sub(BASELINE_KEY_BYTES);
        let at_boundary = line_boundary(&tail.path, start)?;
        tail.start_baseline(baseline, start, at_boundary);
        Ok(tail)
    }

    fn start_baseline(&mut self, baseline: u64, start: u64, at_boundary: bool) {
        self.long = (!at_boundary).then_some(Long { hasher: None });
        self.offset = start;
        self.consumed = start;
        self.baseline = baseline;
    }

    /// Bytes read so far, a partial last line included.
    pub(super) fn offset(&self) -> u64 {
        self.offset
    }

    /// The file offset right after the last complete line taken.
    pub(super) fn complete(&self) -> u64 {
        self.consumed
    }

    /// Where an evicted file can resume, with evidence from the read that consumed it.
    pub(super) fn into_saved_offset(self) -> SavedOffset {
        SavedOffset {
            at: self.complete(),
            suffix: self.consumed_suffix,
        }
    }

    /// The index the next complete line gets.
    #[cfg(test)]
    pub(super) fn next_index(&self) -> u64 {
        self.next_index
    }

    /// The key of the last nonblank line taken (baseline lines included).
    pub(super) fn last_key(&self) -> Option<LineKey> {
        self.last_key
    }

    pub(super) fn last_file_id(&self) -> Option<super::paths::FileId> {
        self.header_version.as_ref().map(|stamp| stamp.id)
    }

    /// A prefix of historical lines lay outside the bounded digest scan.
    pub(super) fn skipped_baseline_prefix(&self) -> bool {
        self.baseline > BASELINE_KEY_BYTES
    }

    /// Whether the baseline is fully keyed.
    pub(super) fn past_baseline(&self) -> bool {
        self.consumed >= self.baseline
    }

    /// One bounded pass over buffered lines and at most `budget.bytes` new bytes, whose
    /// complete lines go to `each`. Checks for a rewrite before reading new bytes and
    /// before consuming buffered lines during baseline and departure reads.
    /// Returns `true` when more is pending
    /// (a budget ran out before the file's size at the start of this pass was reached).
    /// An error stops the pass: what was handed over stays handed over, and the next pass
    /// goes on from there.
    #[cfg(test)]
    pub(super) fn read_pass(
        &mut self,
        budget: &Budget,
        final_read: Option<&FinalRead>,
        each: &mut dyn FnMut(Feed<'_>),
    ) -> io::Result<bool> {
        self.read_checked(budget, final_read, |_| Ok(true), each)
    }

    pub(super) fn read_checked(
        &mut self,
        budget: &Budget,
        final_read: Option<&FinalRead>,
        accepts: impl FnOnce(&File) -> io::Result<bool>,
        each: &mut dyn FnMut(Feed<'_>),
    ) -> io::Result<bool> {
        let mut records = budget.records;
        let check_buffered = final_read.is_some() || !self.past_baseline();
        if !check_buffered && !self.take_lines(&mut records, budget.line_bytes, each) {
            return Ok(true);
        }
        let (mut file, len) = match open_regular(&self.path) {
            Ok(opened) => opened,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if final_read.is_some() {
                    return Err(e);
                }
                // Gone (or not there yet): what was read is not the file's any more.
                if self.offset > 0 {
                    self.rewrite(each);
                }
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        self.observed = true;
        // Only pass the captured file to the source's header check. Recheck its
        // revision below since the callback can overlap a write.
        if final_read.is_some_and(|fence| file_id(&file) != Some(fence.stamp.id)) {
            return Err(io::Error::other("cannot verify final transcript identity"));
        }
        let accepted = accepts(&file)?;
        let (len, version) = match final_read {
            Some(fence) => (fence.end(), Some(fence.verify(&mut file)?)),
            None => (len, FileStamp::of(&file)),
        };
        if !accepted {
            // A replacement belongs to another project. Discard parser and buffered
            // records before a departure read can publish anything from it.
            self.rewrite(each);
            return Ok(false);
        }
        let baseline_shrank = !self.past_baseline() && len < self.baseline;
        let continuity_lost = baseline_shrank || len < self.offset || !self.sealed(&mut file)?;
        if continuity_lost || version.is_none() || version != self.header_version {
            file.rewind()?;
            let first = first_line(&mut file)?;
            let changed =
                matches!((&self.first_line, &first), (Some(known), Some(now)) if known != now);
            if baseline_shrank {
                let start = len.saturating_sub(BASELINE_KEY_BYTES);
                let at_boundary = file_line_boundary(&mut file, start)?;
                self.reset();
                self.start_baseline(len, start, at_boundary);
                each(Feed::Rewritten(Rewrite::Rebaseline {
                    skipped_prefix: start > 0,
                }));
            } else if continuity_lost || changed {
                self.rewrite(each);
            }
            if self.first_line.is_none() {
                self.first_line = first;
            }
            self.header_version = version;
        }
        // Check before buffered history can complete a stale baseline and before a
        // departure read can release records from an altered file.
        if check_buffered && !self.take_lines(&mut records, budget.line_bytes, each) {
            return Ok(true);
        }
        let want = usize::try_from(len.saturating_sub(self.offset))
            .map_or(budget.bytes, |left| left.min(budget.bytes));
        if want == 0 {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = vec![0; want];
        let mut filled = 0;
        while filled < want {
            #[cfg(test)]
            if let Some(left) = self.fail_reads_after.as_mut() {
                if *left == 0 {
                    return Err(io::Error::other("injected read error"));
                }
                *left -= 1;
            }
            let n = match file.read(&mut chunk[filled..]) {
                Ok(0) if final_read.is_some() => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "final transcript read ended early",
                    ));
                }
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            filled += n;
        }
        // A writer can change the file during I/O. Validate before any of the
        // newly read bytes reach the parser or enter its read-ahead buffer.
        if let Some(fence) = final_read {
            fence.verify(&mut file)?;
        }
        let chunk = &chunk[..filled];
        self.offset += filled as u64;
        let done = self.push(chunk, &mut records, budget.line_bytes, each);
        extend_suffix(&mut self.seal, chunk);
        Ok(!done || self.offset < len)
    }

    /// Back to the file's start, and `each` is told.
    fn rewrite(&mut self, each: &mut dyn FnMut(Feed<'_>)) {
        let read = self.offset.max(self.baseline);
        let unfinished_baseline = !self.past_baseline();
        self.reset();
        each(Feed::Rewritten(Rewrite::Replay {
            read,
            unfinished_baseline,
        }));
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.consumed = 0;
        self.consumed_suffix.clear();
        self.buf.clear();
        self.scanned = 0;
        self.long = None;
        self.baseline = 0;
        self.next_index = 0;
        self.first_line = None;
        self.header_version = None;
        self.last_key = None;
        self.seal.clear();
    }

    /// The bytes just before the read position are still the last ones read (the file is
    /// at least that long).
    fn sealed(&self, file: &mut File) -> io::Result<bool> {
        matches_suffix(file, self.offset, &self.seal)
    }

    /// New bytes: the rest of a long line first, then buffered and taken line by line.
    /// Returns `false` when the record budget ran out with complete lines left.
    fn push(
        &mut self,
        mut bytes: &[u8],
        records: &mut usize,
        line_bytes: usize,
        each: &mut dyn FnMut(Feed<'_>),
    ) -> bool {
        if let Some(long) = self.long.as_mut() {
            let Some(newline) = bytes.iter().position(|&b| b == b'\n') else {
                if let Some(hasher) = long.hasher.as_mut() {
                    hasher.write(bytes);
                }
                return true;
            };
            if let Some(hasher) = long.hasher.as_mut() {
                hasher.write(&bytes[..newline]);
            }
            let long = self.long.take();
            // The line ends in this chunk: the bytes before `bytes` were all part of it.
            let end = self.offset - (bytes.len() - newline - 1) as u64;
            self.consumed = end;
            // The discarded part of this long line ended at the previous read position.
            self.consumed_suffix.clone_from(&self.seal);
            extend_suffix(&mut self.consumed_suffix, &bytes[..=newline]);
            if let Some(hasher) = long.and_then(|long| long.hasher) {
                // Counted against the records like any other line.
                *records = records.saturating_sub(1);
                let index = self.next_index;
                self.next_index += 1;
                let key = self.key(hasher.finish(), end);
                if end <= self.baseline {
                    each(Feed::Baseline { key, bytes: None });
                } else {
                    each(Feed::Oversized { index, key });
                }
            }
            bytes = &bytes[newline + 1..];
        }
        self.buf.extend_from_slice(bytes);
        self.take_lines(records, line_bytes, each)
    }

    /// Takes the complete lines out of `buf`, numbering and keying each, while the record
    /// budget lasts. Returns `false` when it ran out with complete lines left. A partial
    /// line longer than `line_bytes` turns into a long line.
    fn take_lines(
        &mut self,
        records: &mut usize,
        line_bytes: usize,
        each: &mut dyn FnMut(Feed<'_>),
    ) -> bool {
        let mut start = 0;
        let mut from = self.scanned;
        let done = loop {
            let Some(newline) = self.buf[from..].iter().position(|&b| b == b'\n') else {
                break true;
            };
            if *records == 0 {
                break false;
            }
            *records -= 1;
            let end = from + newline;
            let index = self.next_index;
            self.next_index += 1;
            let line_end = self.consumed + (end + 1) as u64;
            let raw = &self.buf[start..end];
            if raw.len() > line_bytes {
                let key = self.key(hash(raw), line_end);
                if line_end <= self.baseline {
                    each(Feed::Baseline { key, bytes: None });
                } else {
                    each(Feed::Oversized { index, key });
                }
            } else {
                let line = raw.trim_ascii();
                if !line.is_empty() {
                    let hash = hash(line);
                    let key = LineKey::next(self.last_key, hash, line_end);
                    self.last_key = Some(key);
                    if line_end <= self.baseline {
                        each(Feed::Baseline {
                            key,
                            bytes: Some(line),
                        });
                    } else {
                        each(Feed::Line {
                            index,
                            key,
                            bytes: line,
                        });
                    }
                }
            }
            start = end + 1;
            from = start;
        };
        extend_suffix(&mut self.consumed_suffix, &self.buf[..start]);
        self.buf.drain(..start);
        self.consumed += start as u64;
        if !done {
            self.scanned = 0;
            return false;
        }
        self.scanned = self.buf.len();
        if self.buf.len() > line_bytes {
            let mut hasher = DefaultHasher::new();
            hasher.write(&self.buf);
            self.buf = Vec::new();
            self.scanned = 0;
            self.long = Some(Long {
                hasher: Some(hasher),
            });
        }
        true
    }

    /// A key for a line with `hash`, which becomes the last one.
    fn key(&mut self, hash: u64, end: u64) -> LineKey {
        let key = LineKey::next(self.last_key, hash, end);
        self.last_key = Some(key);
        key
    }

    /// Test helper: the lines one unbounded pass hands over (and whether it rewound).
    #[cfg(test)]
    pub(super) fn read(&mut self) -> io::Result<TailRead> {
        let mut out = TailRead::default();
        let budget = Budget {
            bytes: usize::MAX >> 1,
            records: usize::MAX,
            line_bytes: 1 << 20,
        };
        let result = self.read_pass(&budget, None, &mut |feed| match feed {
            Feed::Rewritten(_) => out.rewritten = true,
            Feed::Line { index, bytes, .. } => out.lines.push((index, bytes.to_vec())),
            Feed::Oversized { index, .. } => out.oversized.push(index),
            Feed::Baseline { .. } => out.baseline += 1,
        });
        match result {
            Err(e) if !out.rewritten && out.lines.is_empty() => Err(e),
            _ => Ok(out),
        }
    }
}

/// What one test [`Tail::read`] handed over.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct TailRead {
    pub(super) rewritten: bool,
    pub(super) lines: Vec<(u64, Vec<u8>)>,
    pub(super) oversized: Vec<u64>,
    pub(super) baseline: usize,
}

fn hash(line: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(line);
    hasher.finish()
}

/// An inactive transcript's baseline and the bytes immediately before it.
pub(super) struct SavedOffset {
    pub(super) at: u64,
    suffix: Vec<u8>,
}

impl SavedOffset {
    pub(super) fn listed(path: &Path, at: u64) -> io::Result<Self> {
        let (mut file, _) = open_regular(path)?;
        let len = at.min(SEAL_CAP as u64) as usize;
        let mut suffix = vec![0; len];
        file.seek(SeekFrom::Start(at - len as u64))?;
        file.read_exact(&mut suffix)?;
        Ok(Self { at, suffix })
    }

    pub(super) fn still_matches(&self, path: &Path) -> io::Result<bool> {
        // A bounded baseline scan can start mid-file and be evicted before it
        // consumes a single complete line. Nothing validates that position.
        if self.at > 0 && self.suffix.is_empty() {
            return Ok(false);
        }
        let (mut file, len) = open_regular(path)?;
        Ok(len >= self.at && matches_suffix(&mut file, self.at, &self.suffix)?)
    }
}

/// Retains at most the last [`SEAL_CAP`] bytes across successive slices.
fn extend_suffix(suffix: &mut Vec<u8>, bytes: &[u8]) {
    let keep = bytes.len().min(SEAL_CAP);
    let excess = (suffix.len() + keep).saturating_sub(SEAL_CAP);
    suffix.drain(..excess);
    suffix.extend_from_slice(&bytes[bytes.len() - keep..]);
}

fn matches_suffix(file: &mut File, at: u64, suffix: &[u8]) -> io::Result<bool> {
    if suffix.is_empty() {
        return Ok(true);
    }
    let mut now = vec![0; suffix.len()];
    file.seek(SeekFrom::Start(at - suffix.len() as u64))?;
    match file.read_exact(&mut now) {
        Ok(()) => Ok(now == suffix),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `at` is the start of a line in `path`: 0, or right after a newline. A file
/// shorter than `at` has no line starting there.
pub(super) fn line_boundary(path: &Path, at: u64) -> io::Result<bool> {
    if at == 0 {
        return Ok(true);
    }
    let (mut file, _) = open_regular(path)?;
    file_line_boundary(&mut file, at)
}

fn file_line_boundary(file: &mut File, at: u64) -> io::Result<bool> {
    if at == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::Start(at - 1))?;
    let mut byte = [0];
    match file.read_exact(&mut byte) {
        Ok(()) => Ok(byte[0] == b'\n'),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// The file's first line without its newline — or its first [`FIRST_LINE_CAP`] bytes when
/// it is longer; `None` while it is shorter and not complete yet.
fn first_line(file: &mut File) -> io::Result<Option<Vec<u8>>> {
    let mut head = Vec::with_capacity(FIRST_LINE_CAP);
    file.by_ref()
        .take(FIRST_LINE_CAP as u64)
        .read_to_end(&mut head)?;
    Ok(match head.iter().position(|&b| b == b'\n') {
        Some(end) => {
            head.truncate(end);
            Some(head)
        }
        None if head.len() == FIRST_LINE_CAP => Some(head),
        None => None,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::*;

    fn file(dir: &Path) -> PathBuf {
        dir.join("transcript.jsonl")
    }

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn texts(read: &TailRead) -> Vec<String> {
        read.lines
            .iter()
            .map(|(_, l)| String::from_utf8(l.clone()).unwrap())
            .collect()
    }

    fn indexes(read: &TailRead) -> Vec<u64> {
        read.lines.iter().map(|(i, _)| *i).collect()
    }

    /// The whole file as old content (what `Files` does for a file that existed).
    fn from_end(path: &Path) -> Tail {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        let mut tail = Tail::from(path.to_path_buf(), len).unwrap();
        tail.read().unwrap();
        tail
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_without_a_writer_is_rejected_promptly() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated path throughout the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let (tx, rx) = mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(Tail::new(reader_path).read());
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        if result.is_err() {
            // Release a regressed reader waiting in open or read.
            let _ = File::options().read(true).write(true).open(&path);
        }
        let error = result.expect("tail blocked on FIFO").unwrap_err();
        reader.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_partial_line_waits_until_it_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\"");
        let mut tail = Tail::new(path.clone());
        let read = tail.read().unwrap();
        assert!(!read.rewritten);
        assert_eq!(texts(&read), ["{\"a\":1}"]);
        assert_eq!(indexes(&read), [0]);

        append(&path, ":2}\n");
        let read = tail.read().unwrap();
        assert!(!read.rewritten);
        assert_eq!(texts(&read), ["{\"b\":2}"]);
        assert_eq!(indexes(&read), [1]);

        let read = tail.read().unwrap();
        assert!(!read.rewritten && read.lines.is_empty(), "nothing new");
    }

    #[test]
    fn a_file_that_shrank_below_the_offset_is_read_again_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\":2}\n");
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().unwrap().lines.len(), 2);

        std::fs::write(&path, "{\"a\":1}\n").unwrap();
        let read = tail.read().unwrap();
        assert!(read.rewritten);
        assert_eq!(texts(&read), ["{\"a\":1}"]);
        assert_eq!(indexes(&read), [0]);
    }

    #[test]
    fn the_same_size_with_another_first_line_is_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\":2}\n");
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().unwrap().lines.len(), 2);

        std::fs::write(&path, "{\"c\":3}\n{\"d\":4}\n").unwrap();
        let read = tail.read().unwrap();
        assert!(read.rewritten);
        assert_eq!(texts(&read), ["{\"c\":3}", "{\"d\":4}"]);
        assert_eq!(indexes(&read), [0, 1]);
    }

    /// A rewrite that keeps the first line and grows: the bytes before what was read
    /// changed, so it is read again from the start.
    #[test]
    fn a_rewrite_that_kept_the_first_line_and_grew_is_read_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"end\":1}\n");
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().unwrap().lines.len(), 2);

        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        let read = tail.read().unwrap();
        assert!(read.rewritten);
        assert_eq!(texts(&read), ["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"]);
        append(&path, "{\"d\":4}\n");
        let read = tail.read().unwrap();
        assert!(!read.rewritten);
        assert_eq!(texts(&read), ["{\"d\":4}"]);
    }

    #[test]
    fn a_rewrite_of_a_partial_line_read_is_read_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\"");
        let mut tail = Tail::new(path.clone());
        assert_eq!(texts(&tail.read().unwrap()), ["{\"a\":1}"]);

        std::fs::write(&path, "{\"a\":1}\n{\"x\":9}\n").unwrap();
        let read = tail.read().unwrap();
        assert!(read.rewritten);
        assert_eq!(texts(&read), ["{\"a\":1}", "{\"x\":9}"]);
    }

    /// Baseline lines count as read: a later rewrite of them is found too.
    #[test]
    fn a_rewrite_of_baseline_lines_is_read_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"end\":1}\n");
        let mut tail = from_end(&path);

        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        assert!(tail.read().unwrap().rewritten);
    }

    #[test]
    fn a_file_that_went_away_counts_as_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n");
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().unwrap().lines.len(), 1);
        std::fs::remove_file(&path).unwrap();
        assert!(tail.read().unwrap().rewritten);
        append(&path, "{\"b\":2}\n");
        let read = tail.read().unwrap();
        assert!(!read.rewritten);
        assert_eq!(texts(&read), ["{\"b\":2}"]);
    }

    #[test]
    fn blank_lines_are_skipped_and_still_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n\n   \r\n{\"b\":2}\r\n");
        let read = Tail::new(path).read().unwrap();
        assert_eq!(texts(&read), ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(indexes(&read), [0, 3]);
    }

    #[test]
    fn a_missing_file_reads_as_empty_until_it_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let mut tail = Tail::new(path.clone());
        let read = tail.read().unwrap();
        assert!(!read.rewritten && read.lines.is_empty());

        append(&path, "{\"a\":1}\n");
        let read = tail.read().unwrap();
        assert!(!read.rewritten);
        assert_eq!(texts(&read), ["{\"a\":1}"]);
    }

    #[test]
    fn baseline_lines_are_never_handed_over_and_a_line_open_at_the_baseline_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"old\":1}\n{\"old\":2}\n{\"new\"");
        let mut tail = from_end(&path);
        assert_eq!(tail.next_index(), 2);
        append(&path, ":3}\n{\"new\":4}\n");
        let read = tail.read().unwrap();
        assert_eq!(texts(&read), ["{\"new\":3}", "{\"new\":4}"]);
        assert_eq!(indexes(&read), [2, 3]);
    }

    /// Only the window before the baseline is keyed: a line that began before it is
    /// skipped unkeyed, the lines after it are keyed, and the anchor is the last one.
    #[test]
    fn a_large_baseline_keys_only_its_last_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let line = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(1000));
        let lines = (BASELINE_KEY_BYTES as usize / line.len()) + 50;
        append(&path, &line.repeat(lines));
        append(&path, "{\"last\":1}\n");
        let len = std::fs::metadata(&path).unwrap().len();
        let mut tail = Tail::from(path.clone(), len).unwrap();
        let read = tail.read().unwrap();
        assert!(read.lines.is_empty() && !read.rewritten, "{read:?}");
        assert!(read.baseline < lines, "{} of {lines} keyed", read.baseline);
        assert!(tail.past_baseline());
        let last = hash(b"{\"last\":1}");
        assert!(tail.last_key().is_some_and(|k| k.hash == last));
        append(&path, "{\"new\":1}\n");
        assert_eq!(texts(&tail.read().unwrap()), ["{\"new\":1}"]);
    }

    #[test]
    fn a_line_over_the_line_budget_is_hashed_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let budget = Budget {
            bytes: 16,
            records: 100,
            line_bytes: 24,
        };
        let long = format!("{{\"long\":\"{}\"}}", "y".repeat(100));
        append(&path, &format!("{{\"a\":1}}\n{long}\n{{\"b\":2}}\n"));
        let mut tail = Tail::new(path.clone());
        let mut seen = Vec::new();
        let mut oversized = Vec::new();
        let mut keys = Vec::new();
        while tail
            .read_pass(&budget, None, &mut |feed| match feed {
                Feed::Line { bytes, key, .. } => {
                    seen.push(String::from_utf8(bytes.to_vec()).unwrap());
                    keys.push(key);
                }
                Feed::Oversized { index, key } => {
                    oversized.push(index);
                    keys.push(key);
                }
                other => panic!("{other:?}"),
            })
            .unwrap()
        {
            assert!(tail.buf.len() <= budget.line_bytes + budget.bytes);
        }
        assert_eq!(seen, ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(oversized, [1]);
        // The same long line read whole keys the same.
        let mut whole = Tail::new(path);
        let mut again = Vec::new();
        let wide = Budget {
            bytes: 1 << 16,
            ..budget
        };
        whole
            .read_pass(&wide, None, &mut |feed| match feed {
                Feed::Line { key, .. } | Feed::Oversized { key, .. } => again.push(key),
                other => panic!("{other:?}"),
            })
            .unwrap();
        assert_eq!(keys, again);
    }

    #[test]
    fn the_saved_offset_uses_the_end_of_an_oversized_line_before_read_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let line = "a".repeat(100) + "\n";
        append(&path, &(line.clone() + &"x".repeat(100)));
        let mut tail = Tail::new(path.clone());
        let budget = Budget {
            bytes: 16,
            records: 1,
            line_bytes: 20,
        };
        while tail.read_pass(&budget, None, &mut |_| {}).unwrap() {}
        let saved = tail.into_saved_offset();
        assert_eq!(saved.at, line.len() as u64);
        std::fs::write(&path, line.clone() + "different partial line").unwrap();
        assert!(saved.still_matches(&path).unwrap());
        // This changed byte came in the chunk before the terminating newline.
        let mut changed = line.into_bytes();
        changed[79] = b'b';
        std::fs::write(&path, changed).unwrap();
        assert!(!saved.still_matches(&path).unwrap());
    }

    #[test]
    fn the_record_budget_leaves_the_rest_for_the_next_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
        let budget = Budget {
            bytes: 1 << 16,
            records: 2,
            line_bytes: 1 << 10,
        };
        let mut tail = Tail::new(path);
        let mut lines = Vec::new();
        let mut take = |tail: &mut Tail| {
            tail.read_pass(&budget, None, &mut |feed| {
                if let Feed::Line { bytes, .. } = feed {
                    lines.push(bytes.to_vec());
                }
            })
            .unwrap()
        };
        assert!(take(&mut tail), "one line left");
        assert!(!take(&mut tail));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], b"{\"c\":3}");
    }

    /// A read that fails part-way, after it found a rewrite: across it and the next read,
    /// the rewrite is reported once and every line once.
    #[test]
    fn a_read_that_fails_part_way_loses_neither_lines_nor_the_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "{\"a\":1}\n{\"b\":2}\n");
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.read().unwrap().lines.len(), 2);

        std::fs::write(&path, "{\"c\":3}\n{\"d\":4}\n{\"e\"").unwrap();
        // The rewrite is found; the first read call fails.
        tail.fail_reads_after = Some(0);
        let failed = tail.read();
        tail.fail_reads_after = None;
        append(&path, ":5}\n");
        let next = tail.read().unwrap();

        let reads: Vec<TailRead> = failed.into_iter().chain([next]).collect();
        let rewrites = reads.iter().filter(|r| r.rewritten).count();
        assert_eq!(rewrites, 1, "{reads:?}");
        assert!(reads[0].rewritten, "the rewrite comes first: {reads:?}");
        let lines: Vec<(u64, Vec<u8>)> = reads.iter().flat_map(|r| r.lines.clone()).collect();
        assert_eq!(
            lines,
            [
                (0, b"{\"c\":3}".to_vec()),
                (1, b"{\"d\":4}".to_vec()),
                (2, b"{\"e\":5}".to_vec())
            ]
        );
    }

    /// A line much longer than a read is scanned for its newline once, not once per read.
    #[test]
    fn a_long_line_is_read_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        let long = vec![b'x'; 8 * 1024 * 1024];
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&long).unwrap();
        f.write_all(b"\n{\"a\":1}\n").unwrap();
        drop(f);
        let budget = Budget {
            bytes: 64 * 1024,
            records: 256,
            line_bytes: 1 << 20,
        };
        let started = std::time::Instant::now();
        let mut tail = Tail::new(path);
        while tail.read_pass(&budget, None, &mut |_| {}).unwrap() {}
        let took = started.elapsed();
        assert_eq!(tail.next_index(), 2);
        assert!(took < std::time::Duration::from_secs(4), "took {took:?}");
    }

    #[cfg(unix)]
    #[test]
    fn baseline_boundary_checks_reject_devices() {
        assert_eq!(
            line_boundary(Path::new("/dev/null"), 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn line_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        append(&path, "ab\ncd");
        assert!(line_boundary(&path, 0).unwrap());
        assert!(!line_boundary(&path, 2).unwrap());
        assert!(line_boundary(&path, 3).unwrap());
        assert!(!line_boundary(&path, 9).unwrap(), "past the end");
    }
}
