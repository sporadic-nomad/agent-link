//! Path helpers for [`AmbientSource`](super::AmbientSource) implementations: where a CLI's
//! configuration root is, how CLIs record the project directory, and the file times
//! lookups compare. Read the agent's variables through
//! [`AmbientContext::var`](super::AmbientContext::var), never this process's environment.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Opens a transcript or lookup record and measures the same file that will be read.
pub(in crate::agent) fn open_regular(path: &Path) -> io::Result<(File, u64)> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO must not wait for a writer before we can inspect its descriptor.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    Ok((file, meta.len()))
}

/// A CLI's configuration root: a nonempty `over` (its override variable, such as
/// `CODEX_HOME`), else `<home>/<leaf>`; `None` without either. Not anchored: pass the
/// result through [`anchored`].
pub fn config_root(over: Option<OsString>, home: Option<PathBuf>, leaf: &str) -> Option<PathBuf> {
    match over.filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home
            .filter(|home| !home.as_os_str().is_empty())
            .map(|home| home.join(leaf)),
    }
}

/// `root` as the agent resolves it: a relative one against its working directory `cwd`.
pub fn anchored(root: PathBuf, cwd: &Path) -> PathBuf {
    if root.is_absolute() {
        root
    } else {
        cwd.join(root)
    }
}

/// The project directory as CLIs usually record it: canonical (macOS `/tmp/x` becomes
/// `/private/tmp/x`), on Windows without the `\\?\` prefix `canonicalize` adds; `cwd` as
/// given when it cannot be resolved. Performs file system I/O.
pub fn project_path(cwd: &Path) -> PathBuf {
    strip_verbatim(std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()))
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Whether the absolute directory a CLI recorded and `project` resolve to the same
/// filesystem directory. Performs filesystem I/O and returns false if either identity
/// cannot be established.
pub fn same_dir(recorded: &str, project: &Path) -> bool {
    let recorded = Path::new(recorded);
    recorded.is_absolute()
        && directory_id(recorded).is_some_and(|id| Some(id) == directory_id(project))
}

#[cfg(unix)]
fn directory_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    meta.is_dir().then(|| (meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn directory_id(path: &Path) -> Option<(u64, [u8; 16])> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    };

    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_dir() {
        return None;
    }
    file_id(&file)
}

#[cfg(unix)]
pub(super) fn file_id(file: &File) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
pub(super) fn file_id(file: &File) -> Option<FileId> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the live handle and writable buffer have the type and size required
    // for FileIdInfo. A successful call initializes the whole result.
    unsafe {
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        ) == 0
        {
            return None;
        }
        let info = info.assume_init();
        Some((info.VolumeSerialNumber, info.FileId.Identifier))
    }
}

#[cfg(unix)]
pub(in crate::agent) type FileId = (u64, u64);
#[cfg(windows)]
pub(in crate::agent) type FileId = (u64, [u8; 16]);

/// Identity and revision of an opened transcript. Without a usable stamp, readers
/// validate its header again on each pass.
#[derive(PartialEq, Eq)]
pub(in crate::agent) struct FileStamp {
    pub(in crate::agent) id: FileId,
    pub(super) len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(windows)]
    changed: i64,
}

impl FileStamp {
    pub(in crate::agent) fn of(file: &File) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().ok()?;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok()?,
            #[cfg(unix)]
            id: (meta.dev(), meta.ino()),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
            #[cfg(windows)]
            id: file_id(file)?,
            #[cfg(windows)]
            changed: change_time(file)?,
        })
    }
}

#[cfg(windows)]
fn change_time(file: &File) -> Option<i64> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };

    let mut info = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: the output buffer matches FileBasicInfo and is initialized on success.
    unsafe {
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        ) == 0
        {
            return None;
        }
        Some(info.assume_init().ChangeTime)
    }
}

/// When a file was created — its modification time on a file system that records no
/// creation time (many Linux file systems): lookups that go by it then take the last
/// written.
pub fn created(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.created().or_else(|_| meta.modified()).ok()
}

/// When a file was last written (its creation time where no modification time is read).
pub fn written(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.modified().or_else(|_| meta.created()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_root_prefers_a_nonempty_override() {
        let home = Some(PathBuf::from("/home/me"));
        assert_eq!(
            config_root(Some("/cfg".into()), home.clone(), ".tool"),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(
            config_root(Some(OsString::new()), home.clone(), ".tool"),
            Some(PathBuf::from("/home/me/.tool"))
        );
        assert_eq!(config_root(None, Some(PathBuf::new()), ".tool"), None);
        assert_eq!(config_root(None, None, ".tool"), None);
    }

    #[test]
    fn anchored_takes_a_relative_root_against_the_cwd() {
        let cwd = std::env::temp_dir();
        assert_eq!(anchored("cfg".into(), &cwd), cwd.join("cfg"));
        assert_eq!(anchored(cwd.clone(), Path::new("elsewhere")), cwd);
    }

    #[test]
    fn same_dir_uses_the_volume_case_rules_and_requires_an_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("App");
        std::fs::create_dir(&project).unwrap();
        let alternate = tmp.path().join("app");
        let alias = match std::fs::create_dir(&alternate) {
            Ok(()) => false,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => true,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(same_dir(alternate.to_str().unwrap(), &project), alias);
        assert!(same_dir(&format!("{}/", project.display()), &project));
        let missing = tmp.path().join("missing");
        assert!(!same_dir(missing.to_str().unwrap(), &missing));
        std::fs::write(&missing, "").unwrap();
        assert!(!same_dir(missing.to_str().unwrap(), &missing));
    }

    #[cfg(unix)]
    #[test]
    fn same_dir_follows_symlinks_and_treats_backslash_as_a_filename_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let other = tmp.path().join(r"project\");
        let alias = tmp.path().join("alias");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        assert!(!same_dir(other.to_str().unwrap(), &project));
        assert!(same_dir(alias.to_str().unwrap(), &project));
    }

    #[test]
    fn project_path_resolves_and_falls_back_to_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = project_path(dir.path());
        assert!(canonical.is_absolute());
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        let missing = dir.path().join("missing");
        assert_eq!(project_path(&missing), missing);
    }

    #[test]
    fn file_times_fall_back_to_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(created(&meta).is_some());
        assert!(written(&meta).is_some());
    }
}
