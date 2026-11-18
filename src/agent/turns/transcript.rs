//! Pin the object selected by the source. Walk names relative to directory descriptors
//! so a symlink or a different file returned by a resolver cannot authorize reading.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, Stat, fstat, open, openat, statat};

use super::{Binding, Gap, Launch};

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
    pub(super) fn open(path: PathBuf) -> std::io::Result<Self> {
        let dir = open(&path, read_flags() | OFlags::DIRECTORY, Mode::empty())?;
        let opened_as = fstat(&dir)?;
        Ok(Self {
            path,
            dir,
            opened_as,
        })
    }

    pub(super) fn check_name(&self) -> Result<(), Gap> {
        let named =
            statat(CWD, &self.path, AtFlags::SYMLINK_NOFOLLOW).map_err(|err| match err {
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
}

pub(in crate::agent) struct Opened {
    pub(super) binding: Binding,
    file: File,
    opened_as: Stat,
}

impl Opened {
    pub(in crate::agent) fn new(launch: &Launch, binding: Binding) -> Result<Self, Gap> {
        if binding.initial != launch.claim.initial {
            return Err(Gap::Uncorroborated);
        }
        let (dir, name) = launch.claim.root.parent(&binding.relative)?;
        let fd = openat(dir, name, read_flags(), Mode::empty()).map_err(std::io::Error::from)?;
        let opened_as = fstat(&fd).map_err(std::io::Error::from)?;
        if FileType::from_raw_mode(opened_as.st_mode) != FileType::RegularFile
            || opened_as.st_nlink != 1
        {
            return Err(Gap::Uncorroborated);
        }
        Ok(Self {
            binding,
            file: fd.into(),
            opened_as,
        })
    }

    /// Providers may inspect bounded metadata on this same opened object.
    pub(in crate::agent) fn file(&self) -> &File {
        &self.file
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
        let (dir, name) = launch.claim.root.parent(&self.binding.relative)?;
        let current = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        if current.st_dev != self.opened_as.st_dev || current.st_ino != self.opened_as.st_ino {
            return Err(Gap::Replaced);
        }
        if current.st_nlink != 1 {
            return Err(Gap::Uncorroborated);
        }
        Ok(())
    }
}

const WINDOW: usize = 64;
const VERIFY_CHUNK_BYTES: usize = 4096;
/// Reserve space for two overlap probes and an exact reread of the consumed batch.
/// A retained record prefix can use the remaining space or later verification passes.
pub(super) const MAX_NEW_BYTES: usize = (super::CEILINGS.read_bytes - 2 * WINDOW) / 2;

/// A change during verification invalidates the bytes already checked. The next
/// pass compares them again to distinguish a rewrite from a concurrent append.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileVersion {
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
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
    pub(super) offset: u64,
    length: u64,
    overlap: Vec<u8>,
    #[cfg(test)]
    after_compare: Option<Box<dyn Fn(u64) + Send>>,
}

impl Tail {
    pub(super) fn at_eof(opened: Opened) -> Result<Self, Gap> {
        let offset = opened.file().metadata()?.len();
        let mut overlap = vec![0; offset.min(WINDOW as u64) as usize];
        let before = offset - overlap.len() as u64;
        opened.file.read_exact_at(&mut overlap, before)?;
        let mut tail = Self {
            opened: Arc::new(opened),
            start: offset,
            offset,
            length: offset,
            overlap,
            #[cfg(test)]
            after_compare: None,
        };
        tail.check_cursor()?;
        Ok(tail)
    }

    pub(super) fn mid_record(&self) -> bool {
        self.overlap.last().is_some_and(|&b| b != b'\n')
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

    /// Also catches shrinkage ahead of the cursor when a previous pass had a backlog.
    fn version(&mut self) -> Result<FileVersion, Gap> {
        let metadata = self.opened.file.metadata()?;
        let length = metadata.len();
        if length < self.length {
            return Err(Gap::Truncated);
        }
        self.length = length;
        Ok(FileVersion {
            length,
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    fn check_cursor(&mut self) -> Result<FileVersion, Gap> {
        let version = self.version()?;
        self.verify(self.offset - self.overlap.len() as u64, &self.overlap)?;
        Ok(version)
    }

    pub(super) fn read(&mut self, buf: &mut [u8]) -> Result<usize, Gap> {
        self.check_cursor()?;
        let n = (self.length - self.offset).min(buf.len() as u64) as usize;
        self.opened.file.read_exact_at(&mut buf[..n], self.offset)?;
        Ok(n)
    }

    /// Reread every byte used by the parser, including a line prefix retained from
    /// earlier batches. Only a bounded slice of the prefix fits in each pass.
    pub(super) fn validate_batch(
        &mut self,
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
        let start = self.offset - prefix.len as u64;
        self.verify(start + prefix.checked as u64, &partial[prefix.checked..end])?;
        if end < prefix.len {
            prefix.checked = end;
            prefix.version = Some(version);
            return Ok(Pass::VerifyMore);
        }
        self.verify(self.offset, bytes)?;
        if self.version()? != version {
            return Ok(prefix.retry());
        }
        Ok(Pass::Complete)
    }

    pub(super) fn advance(&mut self, bytes: &[u8]) {
        let keep = WINDOW.saturating_sub(bytes.len());
        let remove = self.overlap.len().saturating_sub(keep);
        self.overlap.drain(..remove);
        self.overlap
            .extend_from_slice(&bytes[bytes.len().saturating_sub(WINDOW)..]);
        self.offset += bytes.len() as u64;
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
