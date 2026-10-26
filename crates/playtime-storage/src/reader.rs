use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use playtime_core::Sha256Digest;
use playtime_protocol::{
    archive::{
        ContentRef, INPUT_MANIFEST_PATH, InputManifest, PlatformRequirements,
        decode_readable_input_manifest,
    },
    events::{CheckpointRef, EventEnvelope},
    storage::{
        ArchiveDescriptor, ArchiveIdentity, EventFrontier, EventQuery, decode_archive_descriptor,
        decode_commit_marker,
    },
    v1::UsageSummary,
};
use rustix::fs::OFlags;

use crate::{
    CommittedFrontier, ErrorCode, Result, StorageError, StorageLimits,
    files::{Directory, WriterLock, reference, sha256, utf8},
    log::{LogState, scan},
    validate_limits,
};

const METADATA_BYTES: u64 = 65_536;

/// Position supplied by a trusted confirmation authority or checked beneath a
/// covering snapshot. This serializable-looking data is never a writer receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadPrefix {
    pub sequence: u64,
    pub bytes: u64,
    pub sha256: Sha256Digest,
}

/// Host trust boundary, implemented privately by the service's authenticated,
/// persisted-confirmation coordinator. Implementations must authorize this exact
/// archive identity, not a cursor or a discovered marker. Storage independently
/// verifies all bytes. Test authorities are permitted; this is not a sandbox
/// against arbitrary host code.
pub trait ConfirmationAuthority {
    fn authorize(&self, archive: &ArchiveIdentity) -> Result<ReadPrefix>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvent {
    pub envelope: EventEnvelope,
    /// Exact JSONL bytes, including LF. Optional/unknown fields are preserved.
    pub bytes: Vec<u8>,
    pub sha256: Sha256Digest,
}
impl StoredEvent {
    pub(crate) fn new(envelope: EventEnvelope, bytes: Vec<u8>) -> Self {
        Self {
            envelope,
            sha256: sha256(&bytes),
            bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerAvailability {
    pub executable_bytes_verified: bool,
    /// Storage never tests execution, host ABI, dependencies, or rebuildability.
    pub execution_checked: bool,
    pub platform: PlatformRequirements,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inspection {
    pub usage: UsageSummary,
    pub checkpoints: Vec<CheckpointRef>,
    pub terminal: Option<StoredEvent>,
    pub incomplete_evidence: bool,
    pub uncommitted_event_bytes: u64,
    pub referenced_content: Vec<ContentRef>,
    pub worker: WorkerAvailability,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    frontier: EventFrontier,
    inspection: Inspection,
}
impl Snapshot {
    pub fn frontier(&self) -> &EventFrontier {
        &self.frontier
    }
    pub fn inspection(&self) -> &Inspection {
        &self.inspection
    }
    pub fn query(&self, after_sequence: u64, limit: u32) -> EventQuery {
        EventQuery {
            archive: self.frontier.archive.clone(),
            after_sequence,
            through_sequence: self.frontier.committed_sequence,
            limit,
        }
    }
}

#[derive(Debug)]
pub struct EventPage {
    pub events: Vec<StoredEvent>,
    pub next_after_sequence: u64,
    pub frontier: EventFrontier,
    pub has_more: bool,
}

pub struct ArchiveReader {
    directory: Directory,
    descriptor: ArchiveDescriptor,
    identity: ArchiveIdentity,
    manifest: InputManifest,
    limits: StorageLimits,
}

pub(crate) fn prepared(
    directory: &Directory,
    limits: &StorageLimits,
) -> Result<(ArchiveDescriptor, ArchiveIdentity, InputManifest)> {
    let bytes = directory.read("archive.json", METADATA_BYTES)?;
    let descriptor = decode_archive_descriptor(utf8(&bytes)?)?;
    if descriptor.input_manifest.path.as_str() != INPUT_MANIFEST_PATH {
        return Err(StorageError::malformed(
            "archive must reference inputs/manifest.json",
        ));
    }
    let manifest_bytes = directory.verify(&descriptor.input_manifest, limits.manifest_bytes)?;
    let manifest = decode_readable_input_manifest(utf8(&manifest_bytes)?)?;
    if manifest.run_id != descriptor.run_id {
        return Err(StorageError::new(
            ErrorCode::ContentMismatch,
            "input manifest belongs to a different run",
        ));
    }
    // Check aggregate bounds before reading any of the manifest's objects.
    LogState::prepared(&descriptor, &manifest, limits)?;
    for input in manifest.references() {
        directory.verify(input, limits.content_bytes)?;
    }
    let identity = ArchiveIdentity {
        run_id: descriptor.run_id.clone(),
        descriptor: reference("archive.json", &bytes),
        input_manifest: descriptor.input_manifest.clone(),
    };
    Ok((descriptor, identity, manifest))
}

impl ArchiveReader {
    /// Opening only reads bytes; it never upgrades or repairs an archive.
    pub fn open(path: impl AsRef<Path>, limits: StorageLimits) -> Result<Self> {
        validate_limits(&limits)?;
        let directory = Directory::open(path.as_ref())?;
        let (descriptor, identity, manifest) = prepared(&directory, &limits)?;
        Ok(Self {
            directory,
            descriptor,
            identity,
            manifest,
            limits,
        })
    }
    pub fn descriptor(&self) -> &ArchiveDescriptor {
        &self.descriptor
    }
    pub fn identity(&self) -> &ArchiveIdentity {
        &self.identity
    }
    pub fn manifest(&self) -> &InputManifest {
        &self.manifest
    }

    fn validate_inputs(&self) -> Result<()> {
        let (_, identity, _) = prepared(&self.directory, &self.limits)?;
        if identity != self.identity {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                "archive identity changed after open",
            ));
        }
        Ok(())
    }

    /// Offline recovery only: a shared lock refuses an active writer. This is
    /// not a service ownership or execution-admission decision.
    pub fn offline_snapshot(&self) -> Result<Snapshot> {
        let _guard = WriterLock::offline_guard(&self.directory)?;
        let bytes = self.directory.read("events.commit.json", METADATA_BYTES)?;
        let marker = decode_commit_marker(utf8(&bytes)?)?;
        if marker.run_id != self.identity.run_id {
            return Err(StorageError::corrupt(
                "commit marker belongs to a different run",
            ));
        }
        self.snapshot_at(EventFrontier {
            archive: self.identity.clone(),
            marker: reference("events.commit.json", &bytes),
            committed_sequence: marker.committed_sequence,
            committed_bytes: marker.committed_bytes,
            prefix_sha256: marker.prefix_sha256,
        })
    }

    /// Never reads the currently visible marker or advances the supplied
    /// receipt, even if a later commit/failed publication is visible on disk.
    pub fn live_snapshot(&self, receipt: &CommittedFrontier) -> Result<Snapshot> {
        self.snapshot_at(receipt.frontier().clone())
    }

    /// Read only: no marker discovery, writer acquisition or receipt creation.
    pub fn confirmed_snapshot(&self, authority: &dyn ConfirmationAuthority) -> Result<Snapshot> {
        self.snapshot_at(self.read_frontier(authority.authorize(&self.identity)?)?)
    }

    /// A public historical position is usable only beneath an already trusted
    /// covering snapshot. Verify both exact prefixes, even for an equal cursor.
    pub fn sub_snapshot(&self, covering: &Snapshot, prefix: ReadPrefix) -> Result<Snapshot> {
        if covering.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "wrong covering archive",
            ));
        }
        if prefix.sequence > covering.frontier.committed_sequence
            || prefix.bytes > covering.frontier.committed_bytes
        {
            return Err(StorageError::new(
                ErrorCode::CursorAhead,
                "prefix exceeds confirmation",
            ));
        }
        self.snapshot_at(covering.frontier.clone())?;
        self.snapshot_at(self.read_frontier(prefix)?)
    }

    fn read_frontier(&self, prefix: ReadPrefix) -> Result<EventFrontier> {
        // The v1 marker encoding is deterministic. This describes the marker
        // associated with the authorized prefix; it never reads a current marker
        // or pretends that synthesizing its identity certifies a write.
        let marker = playtime_protocol::storage::CommitMarker {
            schema_version: playtime_protocol::storage::COMMIT_SCHEMA_VERSION,
            run_id: self.identity.run_id.clone(),
            committed_sequence: prefix.sequence,
            committed_bytes: prefix.bytes,
            prefix_sha256: prefix.sha256.clone(),
        };
        Ok(EventFrontier {
            archive: self.identity.clone(),
            marker: reference("events.commit.json", &crate::files::json_bytes(&marker)?),
            committed_sequence: prefix.sequence,
            committed_bytes: prefix.bytes,
            prefix_sha256: prefix.sha256,
        })
    }

    fn snapshot_at(&self, frontier: EventFrontier) -> Result<Snapshot> {
        if frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "frontier has wrong run/manifest/archive identity",
            ));
        }
        self.validate_inputs()?;
        let scanned = scan(
            &self.directory,
            &self.descriptor,
            &self.manifest,
            &frontier,
            &self.limits,
            0,
            0,
        )?;
        let state = scanned.state;
        let inspection = Inspection {
            usage: state.usage.into(),
            checkpoints: state.checkpoints,
            incomplete_evidence: state.terminal.is_none(),
            terminal: state.terminal,
            uncommitted_event_bytes: scanned.suffix_bytes,
            referenced_content: state.references.into_values().collect(),
            worker: WorkerAvailability {
                executable_bytes_verified: true,
                execution_checked: false,
                platform: self.manifest.worker.platform.clone(),
            },
        };
        Ok(Snapshot {
            frontier,
            inspection,
        })
    }

    pub fn page(&self, snapshot: &Snapshot, query: &EventQuery) -> Result<EventPage> {
        if query.archive != self.identity || snapshot.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "cursor has wrong run/manifest/archive identity",
            ));
        }
        if query.through_sequence > snapshot.frontier.committed_sequence
            || query.after_sequence > query.through_sequence
        {
            return Err(StorageError::new(
                ErrorCode::CursorAhead,
                "cursor exceeds its confirmed frontier",
            ));
        }
        if query.through_sequence != snapshot.frontier.committed_sequence
            || !(1..=1000).contains(&query.limit)
        {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "snapshot frontier must stay fixed; limit must be 1..=1000",
            ));
        }
        self.validate_inputs()?;
        let scanned = scan(
            &self.directory,
            &self.descriptor,
            &self.manifest,
            &snapshot.frontier,
            &self.limits,
            query.after_sequence,
            query.limit,
        )?;
        let next_after_sequence = scanned
            .events
            .last()
            .map_or(query.after_sequence, |event| event.envelope.sequence.get());
        Ok(EventPage {
            events: scanned.events,
            next_after_sequence,
            frontier: snapshot.frontier.clone(),
            has_more: next_after_sequence < snapshot.frontier.committed_sequence,
        })
    }

    /// Only content referenced by this committed snapshot (or its frozen input
    /// manifest) is normal evidence. Merely publishing an object is insufficient.
    pub fn read_content(&self, snapshot: &Snapshot, content: &ContentRef) -> Result<Vec<u8>> {
        if snapshot.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "snapshot belongs to another archive",
            ));
        }
        if !snapshot.inspection.referenced_content.contains(content) {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                "content is not referenced by this snapshot",
            ));
        }
        self.directory.verify(content, self.limits.content_bytes)
    }

    /// Diagnostic names only, including staging and orphan objects. They are
    /// explicitly excluded from inspection.referenced_content and read_content.
    pub fn uncommitted_objects(&self, snapshot: &Snapshot) -> Result<Vec<String>> {
        if snapshot.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "snapshot belongs to another archive",
            ));
        }
        let inventory = self
            .directory
            .inventory(self.limits.references.saturating_mul(4))?;
        Ok(inventory
            .into_iter()
            .filter(|path| {
                ![
                    "archive.json",
                    "events.jsonl",
                    "events.commit.json",
                    "result.json",
                    ".writer.lock",
                ]
                .contains(&path.as_str())
                    && !snapshot
                        .inspection
                        .referenced_content
                        .iter()
                        .any(|content| content.path.as_str() == path)
            })
            .collect())
    }

    /// Unverified byte diagnostics, usable even if the committed log is corrupt.
    /// The offset is a file position, never a trusted event/resumption cursor.
    pub fn diagnostic_event_bytes(&self, offset: u64, max_bytes: u64) -> Result<Vec<u8>> {
        if max_bytes > self.limits.record_bytes {
            return Err(StorageError::limit("diagnostic byte bound exceeded"));
        }
        let mut file = self.directory.open_file("events.jsonl", OFlags::RDONLY)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(max_bytes).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}
