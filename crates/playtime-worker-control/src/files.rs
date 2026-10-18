use crate::{Error, Result};
use playtime_core::Sha256Digest;
use playtime_protocol::worker::ControlId;
use rustix::fs::{FlockOperation, OFlags, flock};
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
            sync_directory(path.parent().ok_or(Error::Invalid("missing parent"))?)?;
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
        // A FIFO must not block open before descriptor type validation.
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32);
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
fn open_private(path: &Path) -> Result<File> {
    let file = options().read(true).open(path)?;
    check_file(&file)?;
    Ok(file)
}
fn sync_directory(path: &Path) -> Result<()> {
    options()
        .read(true)
        .custom_flags((OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?
        .sync_all()?;
    Ok(())
}
pub(crate) fn sync_private(path: &Path) -> Result<()> {
    // Reconfirmation must validate its own descriptor, including when a path
    // was replaced after the preceding read.
    open_private(path)?.sync_all()?;
    sync_directory(path.parent().ok_or(Error::Invalid("missing parent"))?)
}
pub fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = open_private(path)?;
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
    sync_directory(path.parent().ok_or(Error::Invalid("missing parent"))?)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    #[test]
    #[ignore = "subprocess entry; bounded by private_io_rejects_special_and_replaced_files"]
    fn private_io_process() {
        let root = PathBuf::from(std::env::var_os("PLAYTIME_PRIVATE_IO_ROOT").unwrap());
        let regular = root.join("regular");
        write_once(&regular, b"ok").unwrap();
        assert_eq!(read_private(&regular, 2).unwrap(), b"ok");
        assert!(read_private(&regular, 1).is_err());
        sync_private(&regular).unwrap();

        let fifo = root.join("fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        assert!(FileLock::acquire(&fifo).is_err());
        symlink(&regular, root.join("symlink")).unwrap();
        write_once(&root.join("linked-source"), b"ok").unwrap();
        fs::hard_link(root.join("linked-source"), root.join("hardlink")).unwrap();
        create_private_dir(&root.join("directory")).unwrap();
        write_once(&root.join("public"), b"ok").unwrap();
        fs::set_permissions(root.join("public"), fs::Permissions::from_mode(0o644)).unwrap();
        for name in ["fifo", "symlink", "hardlink", "directory", "public"] {
            let path = root.join(name);
            assert!(read_private(&path, 2).is_err(), "read accepted {name}");
            assert!(sync_private(&path).is_err(), "sync accepted {name}");
        }

        // A prior successful read does not authorize a later unchecked reopen.
        let replaced = root.join("replaced");
        write_once(&replaced, b"ok").unwrap();
        assert_eq!(read_private(&replaced, 2).unwrap(), b"ok");
        fs::rename(&replaced, root.join("original")).unwrap();
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &replaced,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        assert!(sync_private(&replaced).is_err());
        write_once(&root.join("private-io-checked"), b"ok").unwrap();
    }

    #[test]
    fn private_io_rejects_special_and_replaced_files() {
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = Root(
            std::env::temp_dir().join(format!("pt-private-{}", random_id().unwrap().as_str())),
        );
        create_private_dir(&root.0).unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "files::tests::private_io_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PLAYTIME_PRIVATE_IO_ROOT", &root.0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "private I/O fixture failed: {status}");
                assert_eq!(fs::read(root.0.join("private-io-checked")).unwrap(), b"ok");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "private I/O blocked on an invalid file"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
