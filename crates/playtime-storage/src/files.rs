use std::{
    fs::{File, TryLockError},
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

use playtime_core::{ArchivePath, Sha256Digest};
use playtime_protocol::archive::ContentRef;
use rustix::fs::{Mode, OFlags, RenameFlags, mkdirat, openat, renameat, renameat_with};
use sha2::{Digest, Sha256};

use crate::{ErrorCode, Result, StorageError};

pub fn sha256(bytes: &[u8]) -> Sha256Digest {
    digest_value(Sha256::digest(bytes).as_slice())
}

pub(crate) fn digest_value(bytes: &[u8]) -> Sha256Digest {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Sha256Digest::new(hex).expect("SHA-256 has 32 bytes")
}

pub fn content_ref(path: ArchivePath, bytes: &[u8]) -> ContentRef {
    ContentRef {
        path,
        sha256: sha256(bytes),
        size_bytes: bytes.len() as u64,
    }
}

pub(crate) fn reference(path: &str, bytes: &[u8]) -> ContentRef {
    content_ref(
        ArchivePath::new(path).expect("constant archive path"),
        bytes,
    )
}

pub(crate) fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(StorageError::malformed)
}

pub(crate) fn json_bytes(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(StorageError::malformed)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn verify_bytes(reference: &ContentRef, bytes: &[u8], bound: u64) -> Result<()> {
    if reference.size_bytes > bound || bytes.len() as u64 > bound {
        return Err(StorageError::limit(format!(
            "content exceeds byte bound: {}",
            reference.path
        )));
    }
    if reference.size_bytes != bytes.len() as u64 || reference.sha256 != sha256(bytes) {
        return Err(StorageError::new(
            ErrorCode::ContentMismatch,
            format!("length/digest mismatch: {}", reference.path),
        ));
    }
    Ok(())
}

/// Operations surrounding real filesystem calls. Observers may inject failures
/// or cap individual write lengths for validation; they cannot fabricate a
/// successful filesystem operation. No observer is installed by default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteStep {
    CreateDirectory,
    DirectorySync,
    ArtifactWrite,
    ArtifactSync,
    ArtifactRename,
    ArtifactDirectorySync,
    EventsWrite,
    EventsSync,
    MarkerWrite,
    MarkerSync,
    MarkerRename,
    CommitDirectorySync,
    StagingDirectorySync,
    ResultWrite,
    ResultSync,
    ResultRename,
    ResultDirectorySync,
}

pub trait PublicationObserver: Send {
    fn before(&mut self, _step: WriteStep) -> io::Result<()> {
        Ok(())
    }
    fn after(&mut self, _step: WriteStep) -> io::Result<()> {
        Ok(())
    }
    fn write_limit(&mut self, _step: WriteStep, requested: usize) -> usize {
        requested
    }
}
pub(crate) struct NoObserver;
impl PublicationObserver for NoObserver {}

pub(crate) fn operation<T>(
    observer: &mut dyn PublicationObserver,
    step: WriteStep,
    call: impl FnOnce() -> io::Result<T>,
) -> Result<T> {
    observer.before(step)?;
    let result = call()?;
    observer.after(step)?;
    Ok(result)
}

pub(crate) fn sync(
    file: &File,
    observer: &mut dyn PublicationObserver,
    step: WriteStep,
) -> Result<()> {
    operation(observer, step, || file.sync_all())
}

pub(crate) fn write_all(
    file: &mut File,
    mut bytes: &[u8],
    observer: &mut dyn PublicationObserver,
    step: WriteStep,
) -> Result<()> {
    while !bytes.is_empty() {
        let count = observer.write_limit(step, bytes.len()).min(bytes.len());
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero).into());
        }
        let written = match operation(observer, step, || file.write(&bytes[..count])) {
            Err(error) => return Err(error),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(written) => written,
        };
        bytes = &bytes[written..];
    }
    Ok(())
}

// Every component is opened relative to a held directory descriptor with
// NOFOLLOW. Nonblocking opens ensure a malicious FIFO cannot hang inspection.
// The archive owner must still protect directories from hostile host mutation.
pub(crate) struct Directory {
    pub(crate) file: File,
}

fn open_error(error: rustix::io::Errno) -> StorageError {
    if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        StorageError::new(
            ErrorCode::ContentMismatch,
            "symlink or non-directory path component",
        )
    } else {
        error.into()
    }
}

impl Directory {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut directory = Self {
            file: File::open("/")?,
        };
        for component in absolute.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    directory = Self {
                        file: openat(
                            &directory.file,
                            name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(open_error)?
                        .into(),
                    };
                }
                _ => {
                    return Err(StorageError::malformed(
                        "archive root must not contain parent components",
                    ));
                }
            }
        }
        Ok(directory)
    }

    pub(crate) fn create(path: &Path, observer: &mut dyn PublicationObserver) -> Result<Self> {
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = Self::open(parent_path)?;
        let name = path
            .file_name()
            .ok_or_else(|| StorageError::malformed("archive needs a new directory name"))?;
        operation(observer, WriteStep::CreateDirectory, || {
            mkdirat(&parent.file, name, Mode::from_raw_mode(0o700)).map_err(Into::into)
        })?;
        sync(&parent.file, observer, WriteStep::DirectorySync)?;
        Self::open(path)
    }

    fn child(&self, name: &str) -> Result<Self> {
        Ok(Self {
            file: openat(
                &self.file,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(open_error)?
            .into(),
        })
    }

    pub(crate) fn parent(
        &self,
        path: &str,
        create: bool,
        observer: &mut dyn PublicationObserver,
    ) -> Result<(Self, String)> {
        ArchivePath::new(path).map_err(StorageError::malformed)?;
        if path.len() > 4096 || path.split('/').count() > 32 {
            return Err(StorageError::limit("archive path too deep/long"));
        }
        let mut parts = path.split('/').peekable();
        let mut parent = Self {
            file: self.file.try_clone()?,
        };
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok((parent, part.to_owned()));
            }
            if create {
                match mkdirat(&parent.file, part, Mode::from_raw_mode(0o700)) {
                    Ok(()) => {
                        let child = parent.child(part)?;
                        sync(&child.file, observer, WriteStep::DirectorySync)?;
                        sync(&parent.file, observer, WriteStep::DirectorySync)?;
                    }
                    Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            parent = parent.child(part)?;
        }
        Err(StorageError::malformed("empty path"))
    }

    pub(crate) fn open_file(&self, path: &str, flags: OFlags) -> Result<File> {
        let (parent, name) = self.parent(path, false, &mut NoObserver)?;
        let file: File = openat(
            &parent.file,
            name.as_str(),
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(open_error)?
        .into();
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                "expected regular file with exactly one hard link",
            ));
        }
        Ok(file)
    }

    pub(crate) fn read(&self, path: &str, bound: u64) -> Result<Vec<u8>> {
        let file = self.open_file(path, OFlags::RDONLY)?;
        let length = file.metadata()?.len();
        if length > bound {
            return Err(StorageError::limit(format!(
                "file exceeds byte bound: {path}"
            )));
        }
        let mut bytes = Vec::new();
        file.take(bound.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > bound {
            return Err(StorageError::limit("file grew beyond byte bound"));
        }
        Ok(bytes)
    }

    pub(crate) fn verify(&self, reference: &ContentRef, bound: u64) -> Result<Vec<u8>> {
        if reference.size_bytes > bound {
            return Err(StorageError::limit("referenced content exceeds byte bound"));
        }
        let file = self
            .open_file(reference.path.as_str(), OFlags::RDONLY)
            .map_err(|mut error| {
                if error.io_kind == Some(io::ErrorKind::NotFound) {
                    error.code = ErrorCode::ContentMismatch;
                    error.message = format!("missing referenced content: {}", reference.path);
                }
                error
            })?;
        if file.metadata()?.len() != reference.size_bytes {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                format!("length mismatch: {}", reference.path),
            ));
        }
        let mut bytes = Vec::new();
        file.take(reference.size_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        verify_bytes(reference, &bytes, bound)?;
        Ok(bytes)
    }

    pub(crate) fn sync_content(
        &self,
        content: &ContentRef,
        observer: &mut dyn PublicationObserver,
    ) -> Result<()> {
        let file = self.open_file(content.path.as_str(), OFlags::RDONLY)?;
        sync(&file, observer, WriteStep::ArtifactSync)?;
        // Externally supplied content may have newly created ancestors too.
        let mut directory = Self {
            file: self.file.try_clone()?,
        };
        let parts: Vec<_> = content.path.as_str().split('/').collect();
        sync(&directory.file, observer, WriteStep::ArtifactDirectorySync)?;
        for part in &parts[..parts.len() - 1] {
            directory = directory.child(part)?;
            sync(&directory.file, observer, WriteStep::ArtifactDirectorySync)?;
        }
        Ok(())
    }

    pub(crate) fn publish(
        &self,
        path: &str,
        bytes: &[u8],
        replace: bool,
        counter: &mut u64,
        observer: &mut dyn PublicationObserver,
        steps: [WriteStep; 4],
    ) -> Result<()> {
        let (parent, name) = self.parent(path, true, observer)?;
        let (staging, temporary) =
            self.parent(&format!(".staging/{:016x}.tmp", *counter), true, observer)?;
        *counter = counter
            .checked_add(1)
            .ok_or_else(|| StorageError::limit("staging counter overflow"))?;
        let mut file: File = openat(
            &staging.file,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )?
        .into();
        write_all(&mut file, bytes, observer, steps[0])?;
        sync(&file, observer, steps[1])?;
        operation(observer, steps[2], || {
            if replace {
                renameat(
                    &staging.file,
                    temporary.as_str(),
                    &parent.file,
                    name.as_str(),
                )
                .map_err(Into::into)
            } else {
                renameat_with(
                    &staging.file,
                    temporary.as_str(),
                    &parent.file,
                    name.as_str(),
                    RenameFlags::NOREPLACE,
                )
                .map_err(Into::into)
            }
        })?;
        sync(&parent.file, observer, steps[3])?;
        sync(&staging.file, observer, WriteStep::StagingDirectorySync)?;
        Ok(())
    }

    pub(crate) fn inventory(&self, bound: u64) -> Result<Vec<String>> {
        let mut pending = vec![(self.file.try_clone()?, String::new(), 0)];
        let mut objects = Vec::new();
        let mut entries = 0;
        while let Some((directory, prefix, depth)) = pending.pop() {
            if depth > 32 {
                return Err(StorageError::limit("directory inventory too deep"));
            }
            for entry in rustix::fs::Dir::read_from(&directory)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .to_str()
                    .map_err(StorageError::malformed)?;
                if name == "." || name == ".." {
                    continue;
                }
                entries += 1;
                if entries > bound {
                    return Err(StorageError::limit(
                        "directory inventory exceeds entry bound",
                    ));
                }
                let path = format!("{prefix}{name}");
                if path.len() > 4096 {
                    return Err(StorageError::limit("inventory path too long"));
                }
                // fstatat does not follow links. Uncommitted special files are
                // diagnostic names only, never opened or treated as evidence.
                let stat =
                    rustix::fs::statat(&directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
                if rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::Directory
                {
                    let child = Self {
                        file: directory.try_clone()?,
                    }
                    .child(name)?;
                    pending.push((child.file, format!("{path}/"), depth + 1));
                } else {
                    objects.push(path);
                }
            }
        }
        objects.sort();
        Ok(objects)
    }
}

/// A per-archive lock only. Acquiring it confers no permission to launch a
/// worker, resume a writer, or reconcile a failed commit (#19/#20).
pub struct WriterLock {
    _file: File,
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // CLOEXEC closes the child descriptor at exec, not at fork. Explicitly
        // release ownership even if a concurrent spawn temporarily inherited
        // the same open-file description. Closing our descriptor alone can
        // leave flock held until that child execs/exits.
        let _ = self._file.unlock();
    }
}

impl WriterLock {
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_directory(&Directory::open(path.as_ref())?, false)
    }

    pub(crate) fn from_directory(directory: &Directory, create: bool) -> Result<Self> {
        let flags = if create {
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL
        } else {
            OFlags::RDWR
        };
        let file = directory.open_file(".writer.lock", flags)?;
        file.try_lock().map_err(lock_error)?;
        Ok(Self { _file: file })
    }

    pub(crate) fn offline_guard(directory: &Directory) -> Result<Self> {
        let file = directory.open_file(".writer.lock", OFlags::RDONLY)?;
        file.try_lock_shared().map_err(lock_error)?;
        Ok(Self { _file: file })
    }
}

fn lock_error(error: TryLockError) -> StorageError {
    match error {
        TryLockError::WouldBlock => {
            StorageError::new(ErrorCode::WriterBusy, "archive has an active writer/owner")
        }
        TryLockError::Error(error) => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dropping_owner_unlocks_even_if_an_inherited_description_is_still_open() {
        let path = std::env::temp_dir().join(format!(
            "playtime-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let directory = Directory::create(&path, &mut NoObserver).unwrap();
        let lock = WriterLock::from_directory(&directory, true).unwrap();
        // dup shares the open-file description, as an inherited fork fd does.
        let inherited = lock._file.try_clone().unwrap();
        assert_eq!(
            WriterLock::from_directory(&directory, false)
                .err()
                .unwrap()
                .code,
            ErrorCode::WriterBusy
        );
        drop(lock);
        let next = WriterLock::from_directory(&directory, false).unwrap();
        drop(inherited);
        assert_eq!(
            WriterLock::from_directory(&directory, false)
                .err()
                .unwrap()
                .code,
            ErrorCode::WriterBusy
        );
        drop(next);
        drop(directory);
        std::fs::remove_dir_all(path).unwrap();
    }
}
