//! Bounded, explicit attachment retention. Blobs are durable before the atomic
//! catalog update. Orphans from a failed update consume quota but are never
//! promoted by a reader. No artifact changes a checkpoint or model context.
use crate::{CONFIG_BYTES, Head, Session, invalid};
use playtime_core::Sha256Digest;
use playtime_engine::chat::ConversationConfig;
use playtime_local::{PrivateDirectory, Token};
pub use playtime_protocol::artifacts::{Artifact, ArtifactCatalog, ArtifactFile, ArtifactKind};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io, path::Path, time::SystemTime};

pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
pub const MAX_SESSION_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_ARTIFACTS: usize = 128;
const CATALOG: &str = "artifacts-v1.json";
const MAX_CATALOG_BYTES: u64 = 512 * 1024;

pub struct RetainedFile {
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}
impl RetainedFile {
    fn metadata(&self) -> ArtifactFile {
        ArtifactFile {
            original_name: self.name.clone(),
            media_type: self.media_type.clone(),
            size: self.bytes.len() as u64,
            sha256: digest(&self.bytes),
        }
    }
}
fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .expect("SHA-256 hex")
}
fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn validate_file(file: &ArtifactFile, source: bool) -> io::Result<()> {
    if !valid_text(&file.original_name, 255)
        || file.original_name.contains(['/', '\\'])
        || matches!(file.original_name.as_str(), "." | "..")
        || file.size == 0
        || file.size
            > if source {
                MAX_SOURCE_BYTES
            } else {
                MAX_FILE_BYTES
            }
        || if source {
            file.media_type != "text/plain"
        } else {
            !matches!(
                file.media_type.as_str(),
                "image/png" | "image/jpeg" | "application/octet-stream"
            )
        }
    {
        return Err(invalid(
            "invalid artifact file metadata or size limit exceeded",
        ));
    }
    Ok(())
}
fn validate(record: &Artifact) -> io::Result<()> {
    if record.schema_version != 1
        || record.id.len() != 32
        || !record
            .id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !valid_text(&record.caption, 2048)
    {
        return Err(invalid("invalid artifact record"));
    }
    validate_file(&record.file, false)?;
    if let Some(source) = &record.source {
        validate_file(source, true)?;
    }
    let kind = if record.source.is_some() {
        if !record.file.media_type.starts_with("image/") {
            return Err(invalid("a diagram source requires a rendered PNG or JPEG"));
        }
        ArtifactKind::Diagram
    } else if record.file.media_type.starts_with("image/") {
        ArtifactKind::Image
    } else {
        ArtifactKind::File
    };
    if record.kind != kind {
        return Err(invalid("artifact kind does not match its files"));
    }
    Ok(())
}
fn catalog(directory: &PrivateDirectory) -> io::Result<ArtifactCatalog> {
    let catalog: ArtifactCatalog = match directory.read(CATALOG, MAX_CATALOG_BYTES) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => ArtifactCatalog {
            schema_version: 1,
            artifacts: vec![],
        },
        Err(e) => return Err(e),
    };
    if catalog.schema_version != 1 || catalog.artifacts.len() > MAX_ARTIFACTS {
        return Err(invalid("invalid artifact catalog version or count"));
    }
    let mut ids = BTreeSet::new();
    let mut bytes = 0;
    for record in &catalog.artifacts {
        validate(record)?;
        if !ids.insert(&record.id) {
            return Err(invalid("duplicate artifact ID"));
        }
        bytes += record.file.size + record.source.as_ref().map_or(0, |f| f.size);
    }
    if bytes > MAX_SESSION_BYTES {
        return Err(invalid("artifact quota exceeded"));
    }
    Ok(catalog)
}

/// Fixed, generated basenames; original filenames never select storage paths.
pub fn blob_name(record: &Artifact, source: bool) -> io::Result<String> {
    validate(record)?;
    let suffix = if source {
        if record.source.is_none() {
            return Err(invalid("artifact has no source file"));
        }
        "-source.txt"
    } else {
        match record.file.media_type.as_str() {
            "image/png" => ".png",
            "image/jpeg" => ".jpg",
            _ => ".bin",
        }
    };
    Ok(format!("artifact-{}{suffix}", record.id))
}

/// Read-only access remains available even when a stopped turn is fenced or
/// another process owns the session. Only catalog-published IDs are visible.
pub struct Artifacts(PrivateDirectory);
impl Artifacts {
    pub fn open(path: &Path) -> io::Result<Self> {
        let directory = PrivateDirectory::open(path, false)?;
        let config: ConversationConfig =
            serde_json::from_slice(&directory.read("config.json", CONFIG_BYTES)?)?;
        let head: Head = serde_json::from_slice(&directory.read("head.json", 4096)?)?;
        if head.schema_version != 1
            || head.configuration != config.fingerprint().map_err(invalid)?
        {
            return Err(invalid("invalid session identity"));
        }
        Ok(Self(directory))
    }
    pub fn list(&self) -> io::Result<ArtifactCatalog> {
        catalog(&self.0)
    }
    pub fn info(&self, id: &str) -> io::Result<Artifact> {
        self.list()?
            .artifacts
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact ID not found"))
    }
    pub fn read(&self, id: &str, source: bool) -> io::Result<Vec<u8>> {
        let record = self.info(id)?;
        let name = blob_name(&record, source)?;
        let metadata = if source {
            record.source.as_ref().unwrap()
        } else {
            &record.file
        };
        let bytes = self.0.read(
            &name,
            if source {
                MAX_SOURCE_BYTES
            } else {
                MAX_FILE_BYTES
            },
        )?;
        if bytes.len() as u64 != metadata.size || digest(&bytes) != metadata.sha256 {
            return Err(invalid("retained artifact size or SHA-256 mismatch"));
        }
        Ok(bytes)
    }
}

impl Session {
    /// Requires this session's exclusive lease and a completed checkpoint.
    /// The caller supplies bounded bytes, never a path to be followed by storage.
    pub fn retain_artifact(
        &mut self,
        caption: String,
        file: RetainedFile,
        source: Option<RetainedFile>,
    ) -> io::Result<Artifact> {
        let checkpoint = self
            .checkpoint()
            .ok_or_else(|| invalid("retain requires a completed turn"))?;
        let record = Artifact {
            schema_version: 1,
            id: Token::generate()?.expose()[..32].to_owned(),
            kind: if source.is_some() {
                ArtifactKind::Diagram
            } else if file.media_type.starts_with("image/") {
                ArtifactKind::Image
            } else {
                ArtifactKind::File
            },
            caption,
            source_turn: checkpoint.completed_turn.clone(),
            retained_at_unix_ms: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(invalid)?
                .as_millis()
                .try_into()
                .map_err(invalid)?,
            file: file.metadata(),
            source: source.as_ref().map(RetainedFile::metadata),
        };
        validate(&record)?;
        if source
            .as_ref()
            .is_some_and(|s| std::str::from_utf8(&s.bytes).is_err())
        {
            return Err(invalid("diagram source must be UTF-8 text"));
        }
        let mut catalog = catalog(&self.directory)?;
        if catalog.artifacts.len() == MAX_ARTIFACTS {
            return Err(invalid("session artifact count limit reached"));
        }
        // Include abandoned blobs, rather than letting failed publications bypass
        // the physical storage bound. Reads also reject links and special files.
        let mut used =
            file.bytes.len() as u64 + source.as_ref().map_or(0, |s| s.bytes.len() as u64);
        let names = self.directory.names()?;
        let blobs: Vec<_> = names
            .iter()
            .filter(|n| n.starts_with("artifact-"))
            .collect();
        if blobs.len() + 1 + usize::from(source.is_some()) > MAX_ARTIFACTS * 2 {
            return Err(invalid(
                "artifact storage needs offline inspection: blob count limit",
            ));
        }
        for name in blobs {
            used += self.directory.read(name, MAX_FILE_BYTES)?.len() as u64;
            if used > MAX_SESSION_BYTES {
                return Err(invalid("session artifact byte limit reached"));
            }
        }
        catalog.artifacts.push(record.clone());
        let bytes = serde_json::to_vec(&catalog)?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(invalid("artifact catalog limit reached"));
        }
        self.directory
            .write_new(&blob_name(&record, false)?, &file.bytes)?;
        if let Some(source) = source {
            self.directory
                .write_new(&blob_name(&record, true)?, &source.bytes)?;
        }
        self.directory.replace(CATALOG, &bytes)?;
        Ok(record)
    }
}

/// Offline retention acquires the same lease and checkpoint checks as chat,
/// without constructing an endpoint, tool adapter, or runtime.
pub fn open_for_retention(path: &Path) -> io::Result<Session> {
    let directory = PrivateDirectory::open(path, false)?;
    let config = serde_json::from_slice(&directory.read("config.json", CONFIG_BYTES)?)?;
    Session::try_open(path, &config)
}
