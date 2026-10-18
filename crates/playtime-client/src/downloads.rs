use super::*;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, linkat, open, openat, renameat, statat, unlinkat,
};
use std::{ffi::OsString, fs::File, path::PathBuf};

struct Staged {
    directory: File,
    temporary: String,
    destination: OsString,
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = unlinkat(&self.directory, &self.temporary, AtFlags::empty());
    }
}
impl Staged {
    fn create(path: &Path, force: bool) -> Result<(Self, tokio::fs::File)> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let destination = path.file_name().ok_or(Error::Io)?.to_owned();
        let directory = File::from(
            open(
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Error::Io)?,
        );
        let staged = Self {
            directory,
            temporary: format!(
                ".playtime-download-{}.part",
                Token::generate().map_err(|_| Error::Io)?.expose()
            ),
            destination,
        };
        staged.check_destination(force)?;
        let fd = openat(
            &staged.directory,
            &staged.temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| Error::Io)?;
        Ok((staged, tokio::fs::File::from_std(File::from(fd))))
    }
    fn check_destination(&self, force: bool) -> Result<()> {
        match statat(
            &self.directory,
            &self.destination,
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if force && FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                Ok(())
            }
            Err(rustix::io::Errno::NOENT) => Ok(()),
            _ => Err(Error::Io),
        }
    }
    fn publish(self, force: bool) -> Result<()> {
        self.check_destination(force)?;
        if force {
            // Rename replaces the directory entry, never follows a symlink.
            renameat(
                &self.directory,
                &self.temporary,
                &self.directory,
                &self.destination,
            )
            .map_err(|_| Error::Io)?;
        } else {
            linkat(
                &self.directory,
                &self.temporary,
                &self.directory,
                &self.destination,
                AtFlags::empty(),
            )
            .map_err(|_| Error::Io)?;
            unlinkat(&self.directory, &self.temporary, AtFlags::empty()).map_err(|_| Error::Io)?;
        }
        self.directory.sync_all().map_err(|_| Error::Io)
    }
}
impl Client {
    pub async fn download_to_path(
        &self,
        id: &RunId,
        artifact: &Artifact,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        self.download_to_path_with_force(id, artifact, path, false, cancel)
            .await
    }
    /// Verifies the complete content before publishing. `force` only permits
    /// replacing a regular file, never a symlink/directory/device. The held parent
    /// descriptor prevents a renamed parent from redirecting the publication.
    pub async fn download_to_path_with_force(
        &self,
        id: &RunId,
        artifact: &Artifact,
        path: &Path,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let path: PathBuf = path.to_owned();
        let (staged, mut file) = tokio::task::spawn_blocking(move || Staged::create(&path, force))
            .await
            .map_err(|_| Error::Io)??;
        let bytes = self.download(id, artifact, &mut file, cancel).await?;
        file.sync_all().await.map_err(|_| Error::Io)?;
        drop(file);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        tokio::task::spawn_blocking(move || staged.publish(force))
            .await
            .map_err(|_| Error::Io)??;
        Ok(bytes)
    }
}
