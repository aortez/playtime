use std::{
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
use sha2::{Digest, Sha256};

use crate::{
    CommittedFrontier, ErrorCode, Result, StorageError, StorageLimits,
    files::{Directory, WriterLock, digest_value, reference, sha256, utf8},
    log::{Carried, LogState, Pass, Record, ScanOptions, committed_error, decode_line, scan},
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

#[derive(Clone)]
pub struct Snapshot {
    frontier: EventFrontier,
    inspection: Inspection,
    /// Each record's end, digest and running prefix digest, as the pass that
    /// validated this prefix saw them, 72 bytes a record: a page checks the
    /// records it returns against them.
    records: Arc<Vec<Record>>,
    /// The rest of that pass, which a confirmation continues, until
    /// `without_continuation` drops it.
    carried: Option<Arc<Carried>>,
    /// The reader whose pass this is. Only it continues the pass, with the
    /// same directory and limits that validated the earlier bytes.
    reader: u64,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("frontier", &self.frontier)
            .field("inspection", &self.inspection)
            .finish_non_exhaustive()
    }
}
impl Snapshot {
    pub fn frontier(&self) -> &EventFrontier {
        &self.frontier
    }
    /// Checks that `position` is an exact record boundary of this snapshot's
    /// prefix, by sequence, byte count and running digest, as the pass that
    /// validated the prefix recorded them. It reads nothing.
    pub fn check_position(&self, position: &ReadPrefix) -> Result<()> {
        if position.sequence > self.frontier.committed_sequence
            || position.bytes > self.frontier.committed_bytes
        {
            return Err(StorageError::new(
                ErrorCode::CursorAhead,
                "position exceeds the snapshot",
            ));
        }
        let (bytes, sha256) = match position.sequence.checked_sub(1) {
            None => (0, digest_value(Sha256::new().finalize().as_slice())),
            Some(index) => {
                let record = self
                    .records
                    .get(index as usize)
                    .ok_or_else(|| StorageError::corrupt("snapshot pass is missing a record"))?;
                (record.end, digest_value(&record.prefix_sha256))
            }
        };
        if position.bytes != bytes || position.sha256 != sha256 {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "position is not an exact record boundary of the snapshot's prefix",
            ));
        }
        Ok(())
    }
    pub fn inspection(&self) -> &Inspection {
        &self.inspection
    }
    /// The same snapshot for reads only. It keeps what pages and position
    /// checks use and drops what a confirmation would continue from, most
    /// of its pass's memory, so it can't be continued.
    pub fn without_continuation(&self) -> Self {
        Self {
            carried: None,
            ..self.clone()
        }
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
    /// Unique among this process's readers, so a snapshot names the reader
    /// that validated it.
    id: u64,
}

static READERS: AtomicU64 = AtomicU64::new(0);

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
            id: READERS.fetch_add(1, Ordering::Relaxed),
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

    /// Checks before each pass that `archive.json` and the input manifest are
    /// still the bytes verified at open. The frozen inputs were hashed once,
    /// at open: no writer changes them after preparation, the next open
    /// verifies them again, and `read_content` verifies any it returns.
    fn validate_identity(&self) -> Result<()> {
        let bytes = self.directory.read("archive.json", METADATA_BYTES)?;
        if reference("archive.json", &bytes) != self.identity.descriptor {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                "archive identity changed after open",
            ));
        }
        self.directory
            .verify(&self.identity.input_manifest, self.limits.manifest_bytes)?;
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
    /// covering snapshot. The position must be an exact record boundary of
    /// the covering prefix, as the pass that validated it recorded, even for
    /// an equal cursor; a new pass then validates the prefix up to it.
    pub fn sub_snapshot(&self, covering: &Snapshot, prefix: ReadPrefix) -> Result<Snapshot> {
        if covering.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "wrong covering archive",
            ));
        }
        covering.check_position(&prefix)?;
        self.snapshot_at(self.read_frontier(prefix)?)
    }

    /// Read only, like `confirmed_snapshot`, and the same pass also checks
    /// that the authorized prefix exactly extends `previous`, a position this
    /// caller verified before. Neither position becomes a writer receipt.
    pub fn confirmed_snapshot_extending(
        &self,
        authority: &dyn ConfirmationAuthority,
        previous: ReadPrefix,
    ) -> Result<Snapshot> {
        let frontier = self.read_frontier(authority.authorize(&self.identity)?)?;
        if previous.sequence > frontier.committed_sequence
            || previous.bytes > frontier.committed_bytes
        {
            return Err(StorageError::new(
                ErrorCode::CursorAhead,
                "previous prefix exceeds confirmation",
            ));
        }
        Ok(self.scanned(frontier, Some(previous), None)?.0)
    }

    /// Read only, like `confirmed_snapshot`, for an authorized prefix that
    /// extends `previous`, a snapshot this reader validated. It continues the
    /// pass that validated `previous` over the bytes after it, from that
    /// pass's running digest and state, so it checks the new records as a
    /// full pass would. It never reads the earlier records or the content they
    /// reference again: a later change to them shows in a full pass, or in a
    /// page or content read that returns them. Neither position becomes a
    /// writer receipt.
    pub fn confirmed_snapshot_continuing(
        &self,
        authority: &dyn ConfirmationAuthority,
        previous: &Snapshot,
    ) -> Result<Snapshot> {
        if previous.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "previous snapshot belongs to another archive",
            ));
        }
        // Another reader may read another copy or have other limits, so its
        // pass says nothing about the earlier bytes this one would read.
        if previous.reader != self.id {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "previous snapshot was validated by another reader",
            ));
        }
        let carried = previous.carried.as_ref().ok_or_else(|| {
            StorageError::new(
                ErrorCode::InvalidCursor,
                "previous snapshot keeps no pass to continue",
            )
        })?;
        let frontier = self.read_frontier(authority.authorize(&self.identity)?)?;
        let pass = Pass::rejoined(&previous.records, carried);
        Ok(self.scanned(frontier, None, Some(pass))?.0)
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
        Ok(self.scanned(frontier, None, None)?.0)
    }

    /// One pass over the committed prefix at `frontier`, or over the bytes
    /// after a pass it resumes. With a checkpoint, that pass also verifies the
    /// position and returns the snapshot there, as a pass stopping at the
    /// checkpoint would have built it.
    fn scanned(
        &self,
        frontier: EventFrontier,
        checkpoint: Option<ReadPrefix>,
        resume: Option<Pass>,
    ) -> Result<(Snapshot, Option<Snapshot>)> {
        if frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "frontier has wrong run/manifest/archive identity",
            ));
        }
        self.validate_identity()?;
        let scanned = scan(
            &self.directory,
            &self.descriptor,
            &self.manifest,
            &frontier,
            &self.limits,
            ScanOptions {
                checkpoint: checkpoint.as_ref(),
                resume,
                ..ScanOptions::default()
            },
        )?;
        let inner = match (checkpoint, scanned.checkpoint) {
            (Some(prefix), Some(pass)) => {
                // Bytes between the checkpoint and the frontier are committed
                // but beyond that snapshot, as a pass ending there would see.
                let uncommitted = scanned.suffix_bytes + (frontier.committed_bytes - prefix.bytes);
                Some(self.snapshot(self.read_frontier(prefix)?, pass, uncommitted))
            }
            (None, None) => None,
            _ => return Err(StorageError::corrupt("checkpoint state missing")),
        };
        let covering = self.snapshot(frontier, scanned.pass, scanned.suffix_bytes);
        Ok((covering, inner))
    }

    /// The snapshot at `frontier` that `pass` validated.
    fn snapshot(
        &self,
        frontier: EventFrontier,
        pass: Pass,
        uncommitted_event_bytes: u64,
    ) -> Snapshot {
        let inspection = self.inspection(&pass.state, uncommitted_event_bytes);
        let (records, carried) = pass.split();
        Snapshot {
            frontier,
            inspection,
            records: Arc::new(records),
            carried: Some(Arc::new(carried)),
            reader: self.id,
        }
    }

    fn inspection(&self, state: &LogState, uncommitted_event_bytes: u64) -> Inspection {
        Inspection {
            usage: state.usage.into(),
            checkpoints: state.checkpoints.clone(),
            incomplete_evidence: state.terminal.is_none(),
            terminal: state.terminal.clone(),
            uncommitted_event_bytes,
            referenced_content: state.references.values().cloned().collect(),
            worker: WorkerAvailability {
                executable_bytes_verified: true,
                execution_checked: false,
                platform: self.manifest.worker.platform.clone(),
            },
        }
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
        self.page_of(&snapshot.frontier, &snapshot.records, query)
    }

    /// A page of the records up to `position`, an exact record boundary of
    /// `covering`, as `page` reads them at a snapshot there: from
    /// `after_sequence`, at most `limit` records, with the frontier at the
    /// position. The covering pass validated those records, and the page
    /// checks the ones it returns against its digests, as a page of the
    /// covering snapshot does, so it takes no pass to the position. It
    /// inspects nothing: what the run had recorded by the position takes a
    /// pass to it (`sub_snapshot`).
    pub fn page_within(
        &self,
        covering: &Snapshot,
        position: ReadPrefix,
        after_sequence: u64,
        limit: u32,
    ) -> Result<EventPage> {
        if covering.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "wrong covering archive",
            ));
        }
        covering.check_position(&position)?;
        if after_sequence > position.sequence {
            return Err(StorageError::new(
                ErrorCode::CursorAhead,
                "cursor exceeds its position",
            ));
        }
        if !(1..=1000).contains(&limit) {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "limit must be 1..=1000",
            ));
        }
        let frontier = self.read_frontier(position)?;
        let query = EventQuery {
            archive: self.identity.clone(),
            after_sequence,
            through_sequence: frontier.committed_sequence,
            limit,
        };
        self.page_of(&frontier, &covering.records, &query)
    }

    /// The page `query` asks for of the prefix at `frontier`, whose records
    /// are the first `frontier.committed_sequence` of `records`, as the pass
    /// that validated them recorded them. The query is within the frontier.
    fn page_of(
        &self,
        frontier: &EventFrontier,
        records: &[Record],
        query: &EventQuery,
    ) -> Result<EventPage> {
        self.validate_identity()?;
        let after = query.after_sequence as usize;
        let missing = || StorageError::corrupt("snapshot pass is missing a record");
        let page_records = records
            .get(after..query.through_sequence as usize)
            .ok_or_else(missing)?;
        let mut offset = match after.checked_sub(1) {
            None => 0,
            Some(index) => records.get(index).ok_or_else(missing)?.end,
        };
        let mut file = self.directory.open_file("events.jsonl", OFlags::RDONLY)?;
        if file.metadata()?.len() < frontier.committed_bytes {
            return Err(StorageError::corrupt(
                "commit marker points beyond event file",
            ));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut file = BufReader::new(file);
        let mut events = Vec::new();
        let mut page_bytes = 0;
        for record in page_records {
            // Returning fewer than limit is allowed; every page still has a
            // strict byte bound. Once full, never skip a large record to fit
            // a later small one: the next page starts after the last one.
            let length = record.end - offset;
            if events.len() == query.limit as usize || page_bytes + length > self.limits.page_bytes
            {
                break;
            }
            let mut bytes = vec![0; length as usize];
            file.read_exact(&mut bytes)?;
            if Sha256::digest(&bytes).as_slice() != record.sha256 {
                return Err(StorageError::corrupt(
                    "a committed record changed after its snapshot validated",
                ));
            }
            let decoded = decode_line(&bytes, &self.limits).map_err(committed_error)?;
            page_bytes += length;
            offset = record.end;
            events.push(StoredEvent {
                envelope: decoded.envelope().clone(),
                sha256: digest_value(&record.sha256),
                bytes,
            });
        }
        let next_after_sequence = events
            .last()
            .map_or(query.after_sequence, |event| event.envelope.sequence.get());
        Ok(EventPage {
            events,
            next_after_sequence,
            frontier: frontier.clone(),
            has_more: next_after_sequence < frontier.committed_sequence,
        })
    }

    /// Hands every record of the snapshot's committed prefix to `visitor` in
    /// order, in one pass, instead of a page at a time. The pass validates the
    /// complete prefix and the content it references, as a full confirmation
    /// does, but the visitor sees each record once that record has validated,
    /// before the rest of the prefix has: what it builds is evidence only if
    /// `visit` returns Ok. A visitor's error ends the pass and is returned as
    /// is.
    pub fn visit<E: From<StorageError>>(
        &self,
        snapshot: &Snapshot,
        mut visitor: impl FnMut(StoredEvent) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        if snapshot.frontier.archive != self.identity {
            return Err(StorageError::new(
                ErrorCode::InvalidCursor,
                "snapshot belongs to another archive",
            )
            .into());
        }
        self.validate_identity()?;
        let mut stopped = None;
        let scanned = scan(
            &self.directory,
            &self.descriptor,
            &self.manifest,
            &snapshot.frontier,
            &self.limits,
            ScanOptions {
                visitor: Some(&mut |event| {
                    visitor(event).map_err(|error| {
                        stopped = Some(error);
                        // Never returned: the visitor's own error replaces it.
                        StorageError::malformed("visitor stopped the pass")
                    })
                }),
                ..ScanOptions::default()
            },
        );
        match (scanned, stopped) {
            (_, Some(error)) => Err(error),
            (scanned, None) => scanned.map(drop).map_err(Into::into),
        }
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
