//! Pin the object selected by the source. Walk names relative to directory descriptors
//! so a symlink or a different file returned by a resolver cannot authorize reading.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, Stat, fstat, open, openat, statat};

use super::{Binding, Gap, Launch};

pub(in crate::agent) type DirectoryId = (rustix::fs::Dev, u64);

fn read_flags() -> OFlags {
    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY
}

/// Captured before the child starts. The name lets the writer corroborate its
/// settings. All transcript lookups start from the held directory descriptor.
pub(in crate::agent) struct Root {
    pub(in crate::agent) path: PathBuf,
    dir: OwnedFd,
    opened_as: Stat,
}

impl Root {
    /// Compare launch scopes by the pinned directories, including names that alias them.
    pub(in crate::agent) fn directory_id(&self) -> DirectoryId {
        (self.opened_as.st_dev, self.opened_as.st_ino)
    }

    pub(super) fn open(path: PathBuf) -> std::io::Result<Self> {
        // A trailing slash or "/." would ask open() to follow a final symlink as
        // a directory before applying NOFOLLOW. Keep the pinned name unambiguous.
        let path: PathBuf = path.components().collect();
        let dir = open(&path, read_flags() | OFlags::DIRECTORY, Mode::empty())?;
        let opened_as = fstat(&dir)?;
        Ok(Self {
            path,
            dir,
            opened_as,
        })
    }

    pub(super) fn check_name(&self) -> Result<(), Gap> {
        self.check_path(&self.path)
    }

    pub(in crate::agent) fn check_path(&self, path: &Path) -> Result<(), Gap> {
        let named = statat(CWD, path, AtFlags::SYMLINK_NOFOLLOW).map_err(|err| match err {
            rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR => Gap::Replaced,
            _ => Gap::Io,
        })?;
        if named.st_dev != self.opened_as.st_dev || named.st_ino != self.opened_as.st_ino {
            return Err(Gap::Replaced);
        }
        Ok(())
    }

    fn parent<'a>(&self, relative: &'a Path) -> Result<(OwnedFd, &'a OsStr), Gap> {
        self.check_name()?;
        if relative.as_os_str().len() > 4096 {
            return Err(Gap::Uncorroborated);
        }
        let mut parts = relative.components();
        let Some(Component::Normal(name)) = parts.next_back() else {
            return Err(Gap::Uncorroborated);
        };
        let mut dir = self.dir.try_clone()?;
        for part in parts {
            let Component::Normal(component) = part else {
                return Err(Gap::Uncorroborated);
            };
            dir = openat(
                &dir,
                component,
                read_flags() | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
        }
        Ok((dir, name))
    }

    /// List one bounded level through the pinned directory, without following links.
    #[cfg(feature = "turns")]
    pub(in crate::agent) fn names(
        &self,
        relative: &Path,
        remaining: &mut usize,
    ) -> Result<Vec<std::ffi::OsString>, Gap> {
        use std::os::unix::ffi::OsStrExt;
        self.check_name()?;
        let mut dir = self.dir.try_clone()?;
        for part in relative.components() {
            let Component::Normal(part) = part else {
                return Err(Gap::Uncorroborated);
            };
            dir = openat(&dir, part, read_flags() | OFlags::DIRECTORY, Mode::empty())
                .map_err(std::io::Error::from)?;
        }
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&dir).map_err(std::io::Error::from)? {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            *remaining = remaining.checked_sub(1).ok_or(Gap::Oversized)?;
            names.push(OsStr::from_bytes(name).to_owned());
        }
        Ok(names)
    }

    fn stat(&self, relative: &Path) -> Result<Stat, Gap> {
        let (dir, name) = self.parent(relative)?;
        statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .map_err(Gap::from)
    }

    pub(in crate::agent) fn subdirectory_id(&self, relative: &Path) -> Result<DirectoryId, Gap> {
        let stat = self.stat(relative)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(Gap::Uncorroborated);
        }
        Ok((stat.st_dev, stat.st_ino))
    }

    pub(in crate::agent) fn require_absent(&self, relative: &Path) -> Result<(), Gap> {
        match self.stat(relative) {
            Err(Gap::Missing) => Ok(()),
            Ok(_) => Err(Gap::Uncorroborated),
            Err(reason) => Err(reason),
        }
    }

    fn regular(&self, relative: &Path) -> Result<(File, Stat), Gap> {
        let (dir, name) = self.parent(relative)?;
        let fd = openat(dir, name, read_flags(), Mode::empty()).map_err(std::io::Error::from)?;
        let stat = fstat(&fd).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
            return Err(Gap::Uncorroborated);
        }
        Ok((fd.into(), stat))
    }

    fn check_regular(&self, relative: &Path, opened_as: &Stat) -> Result<(), Gap> {
        let named = self.stat(relative)?;
        if named.st_dev != opened_as.st_dev || named.st_ino != opened_as.st_ino {
            return Err(Gap::Replaced);
        }
        if named.st_nlink != 1 {
            return Err(Gap::Uncorroborated);
        }
        Ok(())
    }

    /// Sample a small writer record under the same containment rules as the transcript.
    /// Reject replacement and changes sampled across the read.
    pub(in crate::agent) fn read_record(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Vec<u8>, Gap> {
        let (file, stat) = self.regular(relative)?;
        let before = FileVersion::sample(&file)?;
        if before.length > limit as u64 {
            return Err(Gap::Oversized);
        }
        let mut bytes = vec![0; before.length as usize];
        (&file).read_exact(&mut bytes)?;
        if FileVersion::sample(&file)? != before {
            return Err(Gap::Rewritten);
        }
        self.check_regular(relative, &stat)?;
        Ok(bytes)
    }
}

pub(in crate::agent) struct Opened {
    pub(in crate::agent) binding: Binding,
    file: File,
    opened_as: Stat,
    greatest_length: Mutex<Result<u64, Gap>>,
}

impl Opened {
    pub(in crate::agent) fn new(launch: &Launch, binding: Binding) -> Result<Self, Gap> {
        if binding.initial != launch.claim.initial {
            return Err(Gap::Uncorroborated);
        }
        let (file, opened_as) = launch.claim.root.regular(&binding.relative)?;
        let length = u64::try_from(opened_as.st_size).map_err(|_| Gap::Io)?;
        Ok(Self {
            binding,
            file,
            opened_as,
            greatest_length: Mutex::new(Ok(length)),
        })
    }

    /// Include bytes read since the opening stat in the minimum acceptable EOF.
    pub(in crate::agent) fn read_header(&mut self, limit: usize) -> Result<Option<Vec<u8>>, Gap> {
        // A previous poll may have reached EOF in the middle of this line.
        self.file.rewind()?;
        let mut head = Vec::new();
        BufReader::new((&self.file).take(limit as u64 + 1)).read_until(b'\n', &mut head)?;
        if self.version()?.length < head.len() as u64 {
            return Err(Gap::Truncated);
        }
        if head.len() > limit {
            return Err(Gap::Oversized);
        }
        Ok(head.ends_with(b"\n").then_some(head))
    }

    /// Cursor rewrites the same file. Snapshot reads sample both sides of the
    /// bounded I/O and keep their own continuity proof, independently of append-only tails.
    #[cfg(feature = "turns")]
    pub(super) fn snapshot(
        &self,
        limit: usize,
        chunk: usize,
    ) -> Result<Option<(Vec<u8>, FileVersion)>, Gap> {
        let before = FileVersion::sample(&self.file)?;
        let n = usize::try_from(before.length)
            .ok()
            .filter(|n| *n <= limit)
            .ok_or(Gap::Oversized)?;
        let mut bytes = vec![0u8; n];
        let mut offset = 0;
        for part in bytes.chunks_mut(chunk) {
            match self.file.read_exact_at(part, offset) {
                Ok(()) => offset += part.len() as u64,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        if FileVersion::sample(&self.file)? != before {
            return Ok(None);
        }
        Ok(Some((bytes, before)))
    }

    #[cfg(feature = "turns")]
    pub(super) fn snapshot_version(&self) -> Result<FileVersion, Gap> {
        Ok(FileVersion::sample(&self.file)?)
    }

    /// Serialize the metadata query with its length comparison so a worker and a
    /// queued pull cannot record their samples out of order. A shrink is terminal
    /// even if the file grows back before the detecting thread publishes the gap.
    fn version(&self) -> Result<FileVersion, Gap> {
        let mut greatest = self
            .greatest_length
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = (*greatest)?;
        let version = FileVersion::sample(&self.file)?;
        if version.length < previous {
            *greatest = Err(Gap::Truncated);
            return Err(Gap::Truncated);
        }
        *greatest = Ok(version.length);
        Ok(version)
    }

    #[cfg(feature = "questions")]
    pub(super) fn observed_len(&self) -> Result<u64, Gap> {
        Ok(self.version()?.length)
    }

    /// A writer sample may have blocked while the named file was replaced.
    pub(super) fn confirm(&self, launch: &Launch, current: Binding) -> Result<(), Gap> {
        if current != self.binding {
            return Err(Gap::SessionChanged);
        }
        self.check_name(launch)
    }

    /// Recheck the path against the configured root independently of the resolver.
    /// Only the original transcript is open. Later probes stat its directory entry.
    pub(super) fn check_name(&self, launch: &Launch) -> Result<(), Gap> {
        if self.binding.initial != launch.claim.initial {
            return Err(Gap::Uncorroborated);
        }
        launch
            .claim
            .root
            .check_regular(&self.binding.relative, &self.opened_as)
    }
}

const WINDOW: usize = 64;
const VERIFY_CHUNK_BYTES: usize = 4096;
/// Reserve space for two overlap probes and an exact reread of the consumed batch.
/// A retained record prefix can use the remaining space or later verification passes.
pub(super) const MAX_NEW_BYTES: usize = (super::CEILINGS.read_bytes - 2 * WINDOW) / 2;

/// The same bounded continuity sample is available to the receiver when the reader
/// is parked. Appends are valid, but a sampled shrink or changed suffix is a gap.
#[derive(Clone, Copy)]
pub(super) struct Cursor {
    pub(super) offset: u64,
    overlap: [u8; WINDOW],
}

impl Cursor {
    fn suffix(&self) -> &[u8] {
        &self.overlap[WINDOW - self.offset.min(WINDOW as u64) as usize..]
    }

    pub(super) fn advance(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(WINDOW);
        self.overlap.copy_within(n.., 0);
        self.overlap[WINDOW - n..].copy_from_slice(&bytes[bytes.len() - n..]);
        self.offset += bytes.len() as u64;
    }

    pub(super) fn confirm(&self, opened: &Opened) -> Result<(), Gap> {
        opened.version()?;
        let expected = self.suffix();
        let mut bytes = [0; WINDOW];
        let actual = &mut bytes[..expected.len()];
        opened
            .file
            .read_exact_at(actual, self.offset - expected.len() as u64)?;
        if actual != expected {
            return Err(Gap::Rewritten);
        }
        Ok(())
    }
}

/// A change during verification invalidates the bytes already checked. The next
/// pass compares them again to distinguish a rewrite from a concurrent append.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct FileVersion {
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileVersion {
    fn sample(file: &File) -> std::io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

pub(super) struct Prefix {
    len: usize,
    checked: usize,
    version: Option<FileVersion>,
}

impl Prefix {
    pub(super) fn new(len: usize) -> Self {
        Self {
            len,
            checked: 0,
            version: None,
        }
    }

    fn retry(&mut self) -> Pass {
        self.checked = 0;
        self.version = None;
        // A changing file yields to the poll clock before a fresh scan starts.
        Pass::RetryOnTick
    }
}

pub(super) enum Pass {
    Complete,
    VerifyMore,
    RetryOnTick,
}

pub(super) struct Tail {
    pub(super) opened: Arc<Opened>,
    pub(super) start: u64,
    pub(super) cursor: Cursor,
    #[cfg(test)]
    after_compare: Option<Box<dyn Fn(u64) + Send>>,
}

impl Tail {
    pub(super) fn at_eof(opened: Opened) -> Result<Self, Gap> {
        let offset = opened.version()?.length;
        let mut cursor = Cursor {
            offset,
            overlap: [0; WINDOW],
        };
        let n = cursor.suffix().len();
        opened
            .file
            .read_exact_at(&mut cursor.overlap[WINDOW - n..], offset - n as u64)?;
        let tail = Self {
            opened: Arc::new(opened),
            start: offset,
            cursor,
            #[cfg(test)]
            after_compare: None,
        };
        tail.check_cursor()?;
        Ok(tail)
    }

    pub(super) fn mid_record(&self) -> bool {
        self.cursor.suffix().last().is_some_and(|&b| b != b'\n')
    }

    fn verify(&self, offset: u64, expected: &[u8]) -> Result<(), Gap> {
        let mut scratch = [0; VERIFY_CHUNK_BYTES];
        for (i, part) in expected.chunks(scratch.len()).enumerate() {
            let pos = offset + (i * scratch.len()) as u64;
            self.opened
                .file
                .read_exact_at(&mut scratch[..part.len()], pos)?;
            if &scratch[..part.len()] != part {
                return Err(Gap::Rewritten);
            }
            #[cfg(test)]
            if let Some(after_compare) = &self.after_compare {
                after_compare(pos);
            }
        }
        Ok(())
    }

    fn check_cursor(&self) -> Result<FileVersion, Gap> {
        let version = self.opened.version()?;
        let suffix = self.cursor.suffix();
        self.verify(self.cursor.offset - suffix.len() as u64, suffix)?;
        Ok(version)
    }

    pub(super) fn read(&self, buf: &mut [u8]) -> Result<usize, Gap> {
        let version = self.check_cursor()?;
        let n = (version.length - self.cursor.offset).min(buf.len() as u64) as usize;
        self.opened
            .file
            .read_exact_at(&mut buf[..n], self.cursor.offset)?;
        Ok(n)
    }

    /// Reread every byte used by the parser, including a line prefix retained from
    /// earlier batches. Only a bounded slice of the prefix fits in each pass.
    pub(super) fn validate_batch(
        &self,
        prefix: &mut Prefix,
        partial: &[u8],
        bytes: &[u8],
        prefix_budget: usize,
    ) -> Result<Pass, Gap> {
        let version = self.check_cursor()?;
        if prefix.version.is_some_and(|old| old != version) {
            return Ok(prefix.retry());
        }
        let end = prefix.len.min(prefix.checked + prefix_budget);
        let start = self.cursor.offset - prefix.len as u64;
        self.verify(start + prefix.checked as u64, &partial[prefix.checked..end])?;
        if end < prefix.len {
            prefix.checked = end;
            prefix.version = Some(version);
            return Ok(Pass::VerifyMore);
        }
        self.verify(self.cursor.offset, bytes)?;
        prefix.version = Some(version);
        self.confirm_version(prefix)
    }

    /// Recheck after any intervening writer/terminal samples. A concurrent append
    /// restarts byte verification without parsing the candidate a second time.
    pub(super) fn confirm_version(&self, prefix: &mut Prefix) -> Result<Pass, Gap> {
        if Some(self.opened.version()?) != prefix.version {
            return Ok(prefix.retry());
        }
        Ok(Pass::Complete)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    use super::super::{Claim, ProviderId};
    use super::*;
    use crate::SessionId;

    #[test]
    fn attachment_rejects_a_shrink_below_the_opening_size_or_verified_header() {
        let dir = tempfile::tempdir().unwrap();
        let id = ProviderId::new("session").unwrap();
        let launch = Launch {
            session: SessionId(1),
            lifecycle: 1,
            claim: Arc::new(Claim::new(id.clone(), dir.path(), Path::new(".")).unwrap()),
        };
        let binding = Binding {
            initial: id.clone(),
            current: id,
            revision: 0,
            relative: "session.jsonl".into(),
        };
        let path = dir.path().join(&binding.relative);
        let head = b"{\"type\":\"mode\"}\n";
        let writer = File::create(path).unwrap();
        writer.write_all_at(head, 0).unwrap();
        writer
            .write_all_at(b"history\n", head.len() as u64)
            .unwrap();
        let mut opened = Opened::new(&launch, binding.clone()).unwrap();
        writer.set_len(head.len() as u64).unwrap();
        assert!(matches!(opened.read_header(1024), Err(Gap::Truncated)));

        writer.set_len(0).unwrap();
        let mut opened = Opened::new(&launch, binding).unwrap();
        writer.write_all_at(head, 0).unwrap();
        assert_eq!(
            opened.read_header(1024).unwrap().as_deref(),
            Some(head.as_slice())
        );
        writer.set_len(0).unwrap();
        assert!(matches!(Tail::at_eof(opened), Err(Gap::Truncated)));
    }

    #[test]
    fn rewriting_a_checked_chunk_retries_even_without_a_retained_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let id = ProviderId::new("session").unwrap();
        let launch = Launch {
            session: SessionId(1),
            lifecycle: 1,
            claim: Arc::new(Claim::new(id.clone(), dir.path(), Path::new(".")).unwrap()),
        };
        let opened = Opened::new(
            &launch,
            Binding {
                initial: id.clone(),
                current: id,
                revision: 0,
                relative: "session.jsonl".into(),
            },
        )
        .unwrap();
        let mut tail = Tail::at_eof(opened).unwrap();
        let turn = b"\"user\"\n{\"end\":\"reply\"}\n";
        let bytes = turn.repeat(VERIFY_CHUNK_BYTES / turn.len() + 1);
        writer.write_all_at(&bytes, 0).unwrap();
        tail.after_compare = Some(Box::new(move |offset| {
            if offset == 0 {
                writer.write_all_at(b"e", 2).unwrap();
                writer
                    .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
                    .unwrap();
            }
        }));
        let mut prefix = Prefix::new(0);
        assert!(matches!(
            tail.validate_batch(&mut prefix, &[], &bytes, 0),
            Ok(Pass::RetryOnTick)
        ));
        assert!(matches!(
            tail.validate_batch(&mut prefix, &[], &bytes, 0),
            Err(Gap::Rewritten)
        ));
    }
}
