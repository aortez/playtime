//! Descriptor-relative, private, crash-safe native client state.
use super::*;
use rustix::fs::{Dir, linkat};
use std::ffi::OsStr;

pub fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

pub fn read_private(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    Directory::open(path.parent().ok_or_else(invalid)?, false)?
        .read(path.file_name().ok_or_else(invalid)?, limit)
}

/// Writes a private file in an existing, protected parent directory.
/// Callers coordinating multiple writers must hold their shared state lock.
pub fn replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = Directory::open(path.parent().ok_or_else(invalid)?, false)?;
    publish(
        &directory,
        path.file_name().ok_or_else(invalid)?,
        bytes,
        true,
    )
}

fn publish(directory: &Directory, name: &OsStr, bytes: &[u8], replace: bool) -> io::Result<()> {
    if replace {
        match directory.read(name, 1 << 20) {
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
    }
    let temporary = format!(".state-{}.tmp", Token::generate()?.expose());
    let result = (|| {
        directory.create(&temporary, bytes)?;
        if replace {
            renameat(&directory.0, &temporary, &directory.0, name)?;
        } else {
            linkat(
                &directory.0,
                &temporary,
                &directory.0,
                name,
                AtFlags::empty(),
            )?;
        }
        Ok(())
    })();
    let _ = unlinkat(&directory.0, &temporary, AtFlags::empty());
    directory.0.sync_all()?;
    result
}

/// Hold the returned lock for each read/modify/write transaction. Files remain
/// valid after process death; a killed writer releases its advisory lock.
pub struct PrivateDirectory(Directory);
impl PrivateDirectory {
    pub fn open(path: &Path, create: bool) -> io::Result<Self> {
        let directory = Directory::open(path, create)?;
        if directory.0.metadata()?.mode() & 0o077 != 0 {
            return Err(invalid());
        }
        Ok(Self(directory))
    }
    pub fn lock(&self) -> io::Result<File> {
        lock(&self.0, OsStr::new(".lock"))
    }
    fn name(name: &str) -> io::Result<&OsStr> {
        if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\0') {
            return Err(invalid());
        }
        Ok(OsStr::new(name))
    }
    pub fn read(&self, name: &str, limit: u64) -> io::Result<Vec<u8>> {
        self.0.read(Self::name(name)?, limit)
    }
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        publish(&self.0, Self::name(name)?, bytes, false)
    }
    pub fn replace(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        publish(&self.0, Self::name(name)?, bytes, true)
    }
    pub fn names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in Dir::read_from(&self.0.0)? {
            let entry = entry?;
            let name = entry.file_name().to_str().map_err(|_| invalid())?;
            if name.starts_with('.') {
                continue;
            }
            if names.len() == 100_000 {
                return Err(invalid());
            }
            names.push(name.to_owned());
        }
        names.sort();
        Ok(names)
    }
}

/// Lock file is private and must be in an existing protected directory.
pub fn lock_private(path: &Path) -> io::Result<File> {
    let directory = Directory::open(path.parent().ok_or_else(invalid)?, false)?;
    lock(&directory, path.file_name().ok_or_else(invalid)?)
}
fn lock(directory: &Directory, name: &OsStr) -> io::Result<File> {
    let file = File::from(openat(
        &directory.0,
        name,
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
        || m.len() != 0
    {
        return Err(invalid());
    }
    directory.0.sync_all()?;
    // Bounded contention avoids wedging a caller behind a hung peer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "client state lock timeout",
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}
