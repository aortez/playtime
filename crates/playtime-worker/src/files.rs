//! Descriptor-relative bundle and disposable executable I/O. Never follow links.
use crate::{Error, Result};
use rustix::fs::{self, Mode, OFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

pub struct PrivateDirectory(File);
impl PrivateDirectory {
    pub fn open(path: &Path) -> Result<Self> {
        let mut directory = Self(File::from(fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?));
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        for part in path.components() {
            match part {
                Component::RootDir | Component::CurDir => (),
                Component::Normal(name) => {
                    directory = Self(File::from(fs::openat(
                        &directory.0,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?))
                }
                _ => return Err(Error::Invalid("parent traversal in directory path")),
            }
        }
        Ok(directory)
    }
    pub fn require_private(&self) -> Result<()> {
        let m = self.0.metadata()?;
        if m.uid() != rustix::process::getuid().as_raw() || m.mode() & 0o077 != 0 {
            return Err(Error::Invalid("directory is not service-private"));
        }
        Ok(())
    }
    pub fn create(&self, name: &str) -> Result<Self> {
        leaf(name)?;
        fs::mkdirat(&self.0, name, Mode::from_raw_mode(0o700))?;
        self.0.sync_all()?;
        Ok(Self(File::from(fs::openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)))
    }
    fn parent(&self, path: &str, create: bool) -> Result<(File, String)> {
        let mut parts: Vec<_> = path.split('/').collect();
        for part in &parts {
            leaf(part)?;
        }
        let name = parts
            .pop()
            .ok_or(Error::Invalid("empty file path"))?
            .to_owned();
        let mut directory = self.0.try_clone()?;
        for part in parts {
            if create {
                match fs::mkdirat(&directory, part, Mode::from_raw_mode(0o700)) {
                    Ok(()) => directory.sync_all()?,
                    Err(rustix::io::Errno::EXIST) => (),
                    Err(e) => return Err(e.into()),
                }
            }
            directory = File::from(fs::openat(
                &directory,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            if create {
                Self(directory.try_clone()?).require_private()?;
            }
        }
        Ok((directory, name))
    }
    pub fn read(&self, path: &str, bound: u64) -> Result<Vec<u8>> {
        let (directory, name) = self.parent(path, false)?;
        let file = File::from(fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let m = file.metadata()?;
        if !m.is_file() || m.nlink() != 1 {
            return Err(Error::Invalid("input must be a single-link regular file"));
        }
        if m.len() > bound {
            return Err(Error::Capacity);
        }
        let mut bytes = Vec::new();
        file.take(bound + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > bound {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }
    pub fn write_new(&self, path: &str, bytes: &[u8], executable: bool) -> Result<()> {
        self.require_private()?;
        let (directory, name) = self.parent(path, true)?;
        let mut file = File::from(fs::openat(
            &directory,
            &name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        file.write_all(bytes)?;
        fs::fchmod(
            &file,
            Mode::from_raw_mode(if executable { 0o500 } else { 0o400 }),
        )?;
        file.sync_all()?;
        directory.sync_all()?;
        if self.read(path, bytes.len() as u64)? != bytes {
            return Err(Error::Invalid("copy verification failed"));
        }
        if executable {
            if fs::fstatvfs(&file)?
                .f_flag
                .contains(fs::StatVfsMountFlags::NOEXEC)
            {
                return Err(Error::ExecutionUnavailable);
            }
            fs::accessat(&directory, &name, fs::Access::EXEC_OK, fs::AtFlags::EACCESS)
                .map_err(|_| Error::ExecutionUnavailable)?;
        }
        Ok(())
    }
    pub fn publish_directory(&self, temporary: &str, destination: &str) -> Result<()> {
        leaf(temporary)?;
        leaf(destination)?;
        fs::renameat_with(
            &self.0,
            temporary,
            &self.0,
            destination,
            fs::RenameFlags::NOREPLACE,
        )?;
        self.0.sync_all()?;
        Ok(())
    }
}
fn leaf(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        Err(Error::Invalid("unsafe relative path"))
    } else {
        Ok(())
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Self::Io(e.into())
    }
}
