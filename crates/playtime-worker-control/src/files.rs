use crate::{Error, Result};
use playtime_core::Sha256Digest;
use playtime_protocol::worker::ControlId;
use rustix::fs::{FlockOperation, flock};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

pub fn random_id() -> Result<ControlId> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Invalid("OS randomness unavailable"))?;
    Ok(ControlId::new(bytes.iter().map(|b| format!("{b:02x}")).collect()).unwrap())
}
pub fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
    .unwrap()
}
pub fn boot_id() -> Result<ControlId> {
    ControlId::new(
        fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .replace('-', ""),
    )
    .map_err(|_| Error::Invalid("invalid host boot identity"))
}
pub fn boot_ms() -> u64 {
    let stamp = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    stamp.tv_sec as u64 * 1000 + stamp.tv_nsec as u64 / 1_000_000
}

/// Create only beneath a caller-owned root. Existing paths must already be private.
pub fn create_private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            File::open(path.parent().ok_or(Error::Invalid("missing parent"))?)?.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir()
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "private directory ownership or mode mismatch",
        ));
    }
    Ok(())
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    options
}
fn check_file(file: &File) -> Result<()> {
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::getuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err(Error::Invalid(
            "private file ownership, link or mode mismatch",
        ));
    }
    Ok(())
}
pub fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = options().read(true).open(path)?;
    check_file(&file)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid("private file exceeds limit"));
    }
    Ok(bytes)
}

/// Never overwrite a one-use grant or immutable bootstrap. An interrupted write
/// leaves evidence that blocks a retry; it does not restore unused authority.
pub fn write_once(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = options().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().ok_or(Error::Invalid("missing parent"))?)?.sync_all()?;
    Ok(())
}

#[derive(Debug)]
pub struct FileLock {
    _file: File,
}
impl FileLock {
    /// flock on the existing inode, compatible with Python's RunLock.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // Legacy run.lock may be 0644. It carries no credentials and must not be replaced.
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != rustix::process::getuid().as_raw()
        {
            return Err(Error::Invalid("lock ownership or type mismatch"));
        }
        flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(std::io::Error::from)?;
        Ok(Self { _file: file })
    }
}
