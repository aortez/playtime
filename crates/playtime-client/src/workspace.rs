//! A run's final workspace: its files resolved to the verified artifacts
//! holding their content, and a download of them into a directory, one
//! staged and verified file at a time, under the names their index gave
//! them. No link is ever followed on the way to a file.
use crate::{Client, Error, Result, downloads::Staged, report::ListingSource};
use playtime_core::{RunId, SessionId};
use playtime_protocol::{
    api::{Artifact, Checkpoint, ContentAvailability, PageQuery, RunView},
    bridge::FileContent,
    execution::{CaptureIndex, decode_capture_index},
};
use rustix::{
    fs::{AtFlags, Mode, OFlags, mkdir, mkdirat, open, openat, statat},
    io::Errno,
};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// Artifact or checkpoint pages read for a workspace; a run with more is
/// refused rather than read in part.
const PAGES: usize = 64;
/// The most bytes of a capture index that is read.
const INDEX_BYTES: u64 = 4 << 20;
/// The longest name saved, in bytes, and the longest component of one.
const NAME_BYTES: usize = 1024;
const COMPONENT_BYTES: usize = 255;

/// A file of a workspace, as its index lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceFile {
    /// The index's logical workspace name; never a host path to open.
    pub name: String,
    /// The index's entry type: a tar type flag or a word.
    pub kind: String,
    /// The artifact holding the file's content, when it was retained.
    pub content: ContentAvailability,
}
/// A published workspace: a continuous run's final capture or a memento
/// run's checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub source: ListingSource,
    pub session_id: Option<SessionId>,
    pub published_sequence: u64,
    /// Every entry but a directory, in index order.
    pub files: Vec<WorkspaceFile>,
}
impl Workspace {
    /// A published checkpoint's files.
    pub fn from_checkpoint(checkpoint: &Checkpoint) -> Self {
        Self {
            source: ListingSource::Checkpoint,
            session_id: Some(checkpoint.checkpoint.session_id.clone()),
            published_sequence: checkpoint.checkpoint.published_sequence,
            files: checkpoint
                .files
                .iter()
                .filter(|file| !directory(&file.kind))
                .map(|file| WorkspaceFile {
                    name: file.name.clone(),
                    kind: file.kind.clone(),
                    content: file.content.clone(),
                })
                .collect(),
        }
    }
    /// A final capture's files, each retained content resolved to the listed
    /// artifact holding it; one the listing lacks is unavailable.
    pub fn from_capture(
        index: &CaptureIndex,
        published_sequence: u64,
        artifacts: &[Artifact],
    ) -> Self {
        let files = index
            .files
            .iter()
            .filter(|file| !directory(&file.entry_type))
            .map(|file| {
                let content = match &file.content {
                    FileContent::Retained { content } => artifacts
                        .iter()
                        .find(|artifact| artifact.content == *content)
                        .map_or_else(
                            || ContentAvailability::Unavailable {
                                reason: "not_listed".into(),
                            },
                            |artifact| ContentAvailability::Available {
                                artifact_id: artifact.artifact_id.clone(),
                                content: content.clone(),
                            },
                        ),
                    FileContent::Unavailable { reason } => ContentAvailability::Unavailable {
                        reason: reason.clone(),
                    },
                };
                WorkspaceFile {
                    name: file.path.clone(),
                    kind: file.entry_type.clone(),
                    content,
                }
            })
            .collect();
        Self {
            source: ListingSource::FinalCapture,
            session_id: None,
            published_sequence,
            files,
        }
    }
}
fn directory(kind: &str) -> bool {
    matches!(kind, "5" | "directory")
}

/// What became of one file of a workspace download.
#[derive(Debug)]
pub enum Outcome {
    /// Verified and published under its name.
    Saved { path: PathBuf, bytes: u64 },
    /// The archive didn't retain the content; there is nothing to save.
    Unavailable { reason: String },
    /// The name can't be a path under the directory.
    Refused { reason: &'static str },
    /// Something is at the path already and `force` was off.
    Exists { path: PathBuf },
    /// The download or the write failed; nothing was published.
    Failed { path: PathBuf, error: Error },
}
/// A workspace file and what became of it.
#[derive(Debug)]
pub struct Downloaded {
    pub file: WorkspaceFile,
    pub outcome: Outcome,
}

impl Client {
    /// The run's final workspace: a continuous run's final capture, once
    /// published, or a memento run's last published checkpoint; None when
    /// neither is published yet.
    pub async fn final_workspace(
        &self,
        view: &RunView,
        cancel: &CancellationToken,
    ) -> Result<Option<Workspace>> {
        let continuous = view
            .harness
            .as_ref()
            .is_some_and(|harness| harness.harness.compaction.is_some());
        if !continuous {
            let mut last: Option<Checkpoint> = None;
            let mut query = PageQuery::default();
            for _ in 0..PAGES {
                let page = self.checkpoints(&view.run_id, &query).await?;
                for checkpoint in page.items {
                    if last.as_ref().is_none_or(|last| {
                        checkpoint.checkpoint.published_sequence
                            > last.checkpoint.published_sequence
                    }) {
                        last = Some(checkpoint);
                    }
                }
                if page.next_cursor.is_none() {
                    return Ok(last.as_ref().map(Workspace::from_checkpoint));
                }
                query.cursor = page.next_cursor;
            }
            return Err(Error::LimitExceeded);
        }
        let artifacts = self.all_artifacts(&view.run_id).await?;
        let Some(index) = artifacts
            .iter()
            .filter(|artifact| artifact.kind == "final_capture_index")
            .max_by_key(|artifact| artifact.published_sequence)
        else {
            return Ok(None);
        };
        if index.content.size_bytes > INDEX_BYTES {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = Vec::new();
        self.download(&view.run_id, index, &mut bytes, cancel)
            .await?;
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::InvalidResponse)?;
        let decoded = decode_capture_index(text).map_err(|_| Error::InvalidResponse)?;
        if decoded.run_id != view.run_id {
            return Err(Error::InvalidResponse);
        }
        Ok(Some(Workspace::from_capture(
            &decoded,
            index.published_sequence,
            &artifacts,
        )))
    }
    /// A published checkpoint's workspace.
    pub async fn checkpoint_workspace(
        &self,
        run: &RunId,
        session: &SessionId,
    ) -> Result<Workspace> {
        Ok(Workspace::from_checkpoint(
            &self.checkpoint(run, session).await?,
        ))
    }
    async fn all_artifacts(&self, run: &RunId) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();
        let mut query = PageQuery::default();
        for _ in 0..PAGES {
            let page = self.artifacts(run, &query).await?;
            artifacts.extend(page.items);
            if page.next_cursor.is_none() {
                return Ok(artifacts);
            }
            query.cursor = page.next_cursor;
        }
        Err(Error::LimitExceeded)
    }
    /// Saves each retained file of the workspace under its name in
    /// `directory`, created when missing (its parents aren't), as
    /// `download_to_path_with_force` saves one: staged beside its
    /// destination, verified, then published, so a failed file publishes
    /// nothing. Directories in a name are created as needed and never
    /// followed through a link; a name that can't be a path under the
    /// directory is refused. One file's failure leaves the others to be
    /// tried; cancellation ends the download.
    pub async fn download_workspace(
        &self,
        run: &RunId,
        workspace: &Workspace,
        directory: &Path,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<Downloaded>> {
        let root = directory.to_owned();
        let root = Arc::new(
            tokio::task::spawn_blocking(move || open_root(&root))
                .await
                .map_err(|_| Error::Io)??,
        );
        let mut downloads = Vec::with_capacity(workspace.files.len());
        for file in &workspace.files {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let outcome = self
                .save_file(run, file, &root, directory, force, cancel)
                .await?;
            downloads.push(Downloaded {
                file: file.clone(),
                outcome,
            });
        }
        Ok(downloads)
    }
    /// One file's outcome; an error is only cancellation.
    async fn save_file(
        &self,
        run: &RunId,
        file: &WorkspaceFile,
        root: &Arc<File>,
        directory: &Path,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        let (artifact_id, content) = match &file.content {
            ContentAvailability::Available {
                artifact_id,
                content,
            } => (artifact_id, content),
            ContentAvailability::Unavailable { reason } => {
                return Ok(Outcome::Unavailable {
                    reason: reason.clone(),
                });
            }
        };
        if let Err(reason) = local_name(&file.name) {
            return Ok(Outcome::Refused { reason });
        }
        let path = directory.join(&file.name);
        let (root, name) = (root.clone(), file.name.clone());
        let staged = tokio::task::spawn_blocking(move || stage(&root, &name, force))
            .await
            .map_err(|_| Error::Io)
            .and_then(|staged| staged);
        let (staged, file) = match staged {
            Ok(Some(staged)) => staged,
            Ok(None) => return Ok(Outcome::Exists { path }),
            Err(error) => return Ok(Outcome::Failed { path, error }),
        };
        let mut file = tokio::fs::File::from_std(file);
        let saved = async {
            let bytes = self
                .download_content(run, artifact_id, content, &mut file, cancel)
                .await?;
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
        .await;
        match saved {
            Ok(bytes) => Ok(Outcome::Saved { path, bytes }),
            Err(Error::Cancelled) => Err(Error::Cancelled),
            Err(error) => Ok(Outcome::Failed { path, error }),
        }
    }
}

/// The components of a workspace name that can be a path under the output
/// directory: relative, without an empty, `.` or `..` component, a control
/// character or a backslash, each component at most 255 bytes and the name
/// at most 1,024.
pub fn local_name(name: &str) -> std::result::Result<Vec<&str>, &'static str> {
    if name.is_empty() {
        return Err("empty name");
    }
    if name.len() > NAME_BYTES {
        return Err("name longer than 1,024 bytes");
    }
    if name.chars().any(char::is_control) {
        return Err("name has a control character");
    }
    if name.contains('\\') {
        return Err("name has a backslash");
    }
    let components: Vec<&str> = name.split('/').collect();
    for component in &components {
        match *component {
            "" if name.starts_with('/') => return Err("name is absolute"),
            "" => return Err("name has an empty component"),
            "." | ".." => return Err("name has a . or .. component"),
            component if component.len() > COMPONENT_BYTES => {
                return Err("name has a component longer than 255 bytes");
            }
            _ => {}
        }
    }
    Ok(components)
}
/// The output directory, created private when missing (its parents aren't),
/// open for descriptor-relative work under it.
fn open_root(directory: &Path) -> Result<File> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    match open(directory, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::NOENT) => {
            mkdir(directory, Mode::RWXU).map_err(|_| Error::Io)?;
            Ok(File::from(
                open(directory, flags, Mode::empty()).map_err(|_| Error::Io)?,
            ))
        }
        Err(_) => Err(Error::Io),
    }
}
/// The directory the name's directories lead to under `root`, each opened
/// without following a link and created private when missing.
fn open_parent(root: &File, directories: &[&str]) -> Result<File> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = root.try_clone().map_err(|_| Error::Io)?;
    for component in directories.iter().copied() {
        let next = match openat(&parent, component, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                mkdirat(&parent, component, Mode::RWXU).map_err(|_| Error::Io)?;
                openat(&parent, component, flags, Mode::empty()).map_err(|_| Error::Io)?
            }
            Err(_) => return Err(Error::Io),
        };
        parent = File::from(next);
    }
    Ok(parent)
}
/// A download staged under `name` in `root`, in the directories it names;
/// None when something is at the name and `force` is off.
fn stage(root: &File, name: &str, force: bool) -> Result<Option<(Staged, File)>> {
    let components = local_name(name).map_err(|_| Error::Io)?;
    let (leaf, directories) = components.split_last().ok_or(Error::Io)?;
    let leaf = *leaf;
    let parent = open_parent(root, directories)?;
    if !force && statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Ok(None);
    }
    Staged::in_directory(parent, OsString::from(leaf), force).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use playtime_core::{ArchivePath, Sha256Digest};
    use playtime_protocol::{
        api::{ArtifactId, CheckpointSummary, FileVersion},
        archive::ContentRef,
        bridge,
    };
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn reference(path: &str, bytes: &[u8]) -> ContentRef {
        ContentRef {
            path: ArchivePath::new(path).unwrap(),
            sha256: Sha256Digest::new(
                Sha256::digest(bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
            .unwrap(),
            size_bytes: bytes.len() as u64,
        }
    }
    fn artifact(id: &str, content: &ContentRef) -> Artifact {
        Artifact {
            artifact_id: ArtifactId::new(id).unwrap(),
            name: content.path.as_str().rsplit('/').next().unwrap().into(),
            kind: "file_version".into(),
            content: content.clone(),
            session_id: None,
            published_sequence: 7,
            frozen_input: false,
        }
    }
    fn entry(path: &str, entry_type: &str, content: FileContent) -> bridge::FileVersion {
        let (size_bytes, sha256) = match &content {
            FileContent::Retained { content } => (content.size_bytes, Some(content.sha256.clone())),
            FileContent::Unavailable { .. } => (0, None),
        };
        bridge::FileVersion {
            path: path.into(),
            size_bytes,
            mode: 0o644,
            entry_type: entry_type.into(),
            sha256,
            link: None,
            content,
        }
    }
    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_name_is_a_relative_path_without_tricks() {
        for name in [
            "a.txt",
            "seeds/pulses.json",
            "saved <image>.png",
            ".hidden/-dash",
            "unicode é/名前.txt",
        ] {
            assert_eq!(
                local_name(name).unwrap(),
                name.split('/').collect::<Vec<_>>(),
                "{name}"
            );
        }
        let long = "x".repeat(255);
        assert!(local_name(&format!("{long}/{long}/{long}")).is_ok());
        for (name, reason) in [
            ("", "empty name"),
            ("/etc/passwd", "name is absolute"),
            ("a//b", "name has an empty component"),
            ("a/", "name has an empty component"),
            ("./a", "name has a . or .. component"),
            ("a/../b", "name has a . or .. component"),
            ("..", "name has a . or .. component"),
            ("a\nb", "name has a control character"),
            ("a\0", "name has a control character"),
            ("a\\b", "name has a backslash"),
            (
                &"x".repeat(256),
                "name has a component longer than 255 bytes",
            ),
            (&"x/".repeat(600), "name longer than 1,024 bytes"),
        ] {
            assert_eq!(local_name(name), Err(reason), "{name:?}");
        }
    }

    #[test]
    fn a_capture_resolves_each_retained_file_to_its_listed_artifact() {
        let listed = reference("objects/aa", b"listed");
        let unlisted = reference("objects/bb", b"unlisted");
        let index = CaptureIndex {
            schema_version: 2,
            run_id: RunId::new("run").unwrap(),
            workspace: reference("artifacts/final/workspace.tar", b"tar"),
            files: vec![
                entry(
                    "out",
                    "5",
                    FileContent::Unavailable {
                        reason: "directory".into(),
                    },
                ),
                entry(
                    "out/listed.txt",
                    "0",
                    FileContent::Retained {
                        content: listed.clone(),
                    },
                ),
                entry(
                    "out/unlisted.txt",
                    "0",
                    FileContent::Retained { content: unlisted },
                ),
                entry(
                    "big.bin",
                    "0",
                    FileContent::Unavailable {
                        reason: "oversized".into(),
                    },
                ),
                entry(
                    "link",
                    "2",
                    FileContent::Unavailable {
                        reason: "link".into(),
                    },
                ),
            ],
            source_map: reference("artifacts/final/source-map.json", b"{}"),
        };
        let artifacts = [
            artifact("other", &reference("objects/cc", b"other")),
            artifact("listed", &listed),
        ];
        let workspace = Workspace::from_capture(&index, 787, &artifacts);
        assert_eq!(workspace.source, ListingSource::FinalCapture);
        assert_eq!(workspace.session_id, None);
        assert_eq!(workspace.published_sequence, 787);
        assert_eq!(
            workspace.files,
            vec![
                WorkspaceFile {
                    name: "out/listed.txt".into(),
                    kind: "0".into(),
                    content: ContentAvailability::Available {
                        artifact_id: ArtifactId::new("listed").unwrap(),
                        content: listed,
                    },
                },
                WorkspaceFile {
                    name: "out/unlisted.txt".into(),
                    kind: "0".into(),
                    content: ContentAvailability::Unavailable {
                        reason: "not_listed".into()
                    },
                },
                WorkspaceFile {
                    name: "big.bin".into(),
                    kind: "0".into(),
                    content: ContentAvailability::Unavailable {
                        reason: "oversized".into()
                    },
                },
                WorkspaceFile {
                    name: "link".into(),
                    kind: "2".into(),
                    content: ContentAvailability::Unavailable {
                        reason: "link".into()
                    },
                },
            ]
        );
    }

    #[test]
    fn a_checkpoint_lists_every_entry_but_a_directory() {
        let content = reference("objects/aa", b"saved");
        let available = ContentAvailability::Available {
            artifact_id: ArtifactId::new("saved").unwrap(),
            content,
        };
        let unavailable = ContentAvailability::Unavailable {
            reason: "not retained".into(),
        };
        let checkpoint = Checkpoint {
            checkpoint: CheckpointSummary {
                session_id: SessionId::new("s4").unwrap(),
                published_sequence: 714,
            },
            incoming_memory: unavailable.clone(),
            outgoing_memory: unavailable.clone(),
            evidence: unavailable.clone(),
            workspace: unavailable.clone(),
            files: vec![
                FileVersion {
                    name: "canvas.py".into(),
                    kind: "regular".into(),
                    content: available.clone(),
                },
                FileVersion {
                    name: "seeds".into(),
                    kind: "directory".into(),
                    content: unavailable.clone(),
                },
                FileVersion {
                    name: "seeds/x".into(),
                    kind: "5".into(),
                    content: unavailable.clone(),
                },
                FileVersion {
                    name: "big.bin".into(),
                    kind: "0".into(),
                    content: unavailable.clone(),
                },
            ],
            index: None,
        };
        let workspace = Workspace::from_checkpoint(&checkpoint);
        assert_eq!(workspace.source, ListingSource::Checkpoint);
        assert_eq!(workspace.session_id, Some(SessionId::new("s4").unwrap()));
        assert_eq!(workspace.published_sequence, 714);
        assert_eq!(
            workspace.files,
            vec![
                WorkspaceFile {
                    name: "canvas.py".into(),
                    kind: "regular".into(),
                    content: available,
                },
                WorkspaceFile {
                    name: "big.bin".into(),
                    kind: "0".into(),
                    content: unavailable,
                },
            ]
        );
    }

    #[test]
    fn staging_creates_private_directories_and_never_follows_a_link() {
        let root = std::env::temp_dir().join(format!(
            "playtime-workspace-{}-{}",
            std::process::id(),
            &crate::Token::generate().unwrap().expose()[..8]
        ));
        // The output directory is created, private; its parents must exist.
        assert!(open_root(&root.join("missing/parent")).is_err());
        let directory = open_root(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        let (staged, file) = stage(&directory, "a/b/c.txt", false).unwrap().unwrap();
        assert_eq!(mode(&root.join("a")), 0o700);
        assert_eq!(mode(&root.join("a/b")), 0o700);
        drop(file);
        staged.publish(false).unwrap();
        assert!(root.join("a/b/c.txt").is_file());
        assert_eq!(mode(&root.join("a/b/c.txt")), 0o600);
        // A name something is at is refused without force and staged with it;
        // an unpublished staging leaves nothing behind.
        assert!(stage(&directory, "a/b/c.txt", false).unwrap().is_none());
        assert!(stage(&directory, "a/b/c.txt", true).unwrap().is_some());
        assert_eq!(std::fs::read_dir(root.join("a/b")).unwrap().count(), 1);
        // A link among the name's directories is never followed, nor is a
        // file taken for a directory, nor a link at the name replaced.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, root.join("a/link")).unwrap();
        assert!(stage(&directory, "a/link/x.txt", false).is_err());
        assert!(stage(&directory, "a/link/x.txt", true).is_err());
        assert!(stage(&directory, "a/b/c.txt/x.txt", true).is_err());
        symlink(&elsewhere, root.join("a/leaf")).unwrap();
        assert!(stage(&directory, "a/leaf", false).unwrap().is_none());
        assert!(stage(&directory, "a/leaf", true).is_err());
        assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
        assert!(stage(&directory, "../escape", true).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
