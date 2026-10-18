use std::{collections::BTreeMap, fs::File, io, path::Path};

use playtime_protocol::{
    archive::{ContentRef, INPUT_MANIFEST_PATH, decode_input_manifest},
    events::KnownEvent,
    storage::{
        ArchiveDescriptor, ArchiveIdentity, COMMIT_SCHEMA_VERSION, CommitMarker, EventFrontier,
        FINALIZATION_RESERVE_BYTES, decode_archive_descriptor,
    },
};
use rustix::fs::OFlags;
use sha2::{Digest, Sha256};

use crate::{
    ErrorCode, Result, StorageError, StorageLimits,
    files::{
        Directory, NoObserver, PublicationObserver, WriteStep, WriterLock, digest_value,
        json_bytes, reference, sync, utf8, verify_bytes, write_all,
    },
    log::{LogState, decode_line, evidence_path},
    reader::prepared,
    validate_limits,
};

const ARTIFACT_STEPS: [WriteStep; 4] = [
    WriteStep::ArtifactWrite,
    WriteStep::ArtifactSync,
    WriteStep::ArtifactRename,
    WriteStep::ArtifactDirectorySync,
];
const MARKER_STEPS: [WriteStep; 4] = [
    WriteStep::MarkerWrite,
    WriteStep::MarkerSync,
    WriteStep::MarkerRename,
    WriteStep::CommitDirectorySync,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchClass {
    Ordinary,
    Finalization,
}

pub struct FrozenInput {
    pub reference: ContentRef,
    pub bytes: Vec<u8>,
}

/// Only a successful publication can construct this live-read receipt. There is
/// deliberately no Deserialize or constructor accepting a discovered marker.
#[derive(Clone, Debug)]
pub struct CommittedFrontier {
    frontier: EventFrontier,
}
impl CommittedFrontier {
    pub fn frontier(&self) -> &EventFrontier {
        &self.frontier
    }
}

pub struct ArchiveWriter {
    directory: Directory,
    _lock: WriterLock,
    events: File,
    descriptor: ArchiveDescriptor,
    limits: StorageLimits,
    state: LogState,
    hash: Sha256,
    receipt: CommittedFrontier,
    temporary_counter: u64,
    stopped: bool,
    observer: Box<dyn PublicationObserver>,
}

impl ArchiveWriter {
    pub fn create(
        path: impl AsRef<Path>,
        descriptor: ArchiveDescriptor,
        manifest: &[u8],
        inputs: &[FrozenInput],
        limits: StorageLimits,
    ) -> Result<Self> {
        Self::create_with_observer(
            path,
            descriptor,
            manifest,
            inputs,
            limits,
            Box::new(NoObserver),
        )
    }

    pub fn create_with_observer(
        path: impl AsRef<Path>,
        descriptor: ArchiveDescriptor,
        manifest_bytes: &[u8],
        inputs: &[FrozenInput],
        limits: StorageLimits,
        mut observer: Box<dyn PublicationObserver>,
    ) -> Result<Self> {
        validate_limits(&limits)?;
        let descriptor_bytes = json_bytes(&descriptor)?;
        decode_archive_descriptor(utf8(&descriptor_bytes)?)?;
        if descriptor_bytes.len() > 65_536
            || descriptor.input_manifest.path.as_str() != INPUT_MANIFEST_PATH
        {
            return Err(StorageError::malformed(
                "invalid archive descriptor/manifest path",
            ));
        }
        verify_bytes(
            &descriptor.input_manifest,
            manifest_bytes,
            limits.manifest_bytes,
        )?;
        let manifest = decode_input_manifest(utf8(manifest_bytes)?)?;
        if manifest.run_id != descriptor.run_id {
            return Err(StorageError::new(
                ErrorCode::ContentMismatch,
                "manifest run does not match archive",
            ));
        }
        let state = LogState::prepared(&descriptor, &manifest, &limits)?;
        if inputs.len() as u64 >= limits.references {
            return Err(StorageError::limit(
                "input object count exceeds reference bound",
            ));
        }
        let supplied: BTreeMap<_, _> = inputs
            .iter()
            .map(|input| (&input.reference.path, input))
            .collect();
        if supplied.len() != inputs.len() || supplied.len() != manifest.references().count() {
            return Err(StorageError::malformed(
                "supply exactly the manifest's input objects, once each",
            ));
        }
        for expected in manifest.references() {
            let input = supplied
                .get(&expected.path)
                .ok_or_else(|| StorageError::malformed("missing frozen input"))?;
            if input.reference != *expected {
                return Err(StorageError::new(
                    ErrorCode::ContentMismatch,
                    "input identity differs from manifest",
                ));
            }
            verify_bytes(expected, &input.bytes, limits.content_bytes)?;
        }
        // From directory creation onward, any failure leaves an explicitly
        // unprepared/uncertain directory for diagnosis. Never remove or retry it.
        let creation = || -> Result<Self> {
            let directory = Directory::create(path.as_ref(), observer.as_mut())?;
            let lock = WriterLock::from_directory(&directory, true)?;
            let mut counter = 0;
            for input in inputs {
                directory.publish(
                    input.reference.path.as_str(),
                    &input.bytes,
                    false,
                    &mut counter,
                    observer.as_mut(),
                    ARTIFACT_STEPS,
                )?;
            }
            directory.publish(
                INPUT_MANIFEST_PATH,
                manifest_bytes,
                false,
                &mut counter,
                observer.as_mut(),
                ARTIFACT_STEPS,
            )?;
            let events = directory.open_file(
                "events.jsonl",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::APPEND,
            )?;
            sync(&events, observer.as_mut(), WriteStep::EventsSync)?;
            let marker = CommitMarker {
                schema_version: COMMIT_SCHEMA_VERSION,
                run_id: descriptor.run_id.clone(),
                committed_sequence: 0,
                committed_bytes: 0,
                prefix_sha256: crate::sha256(b""),
            };
            let marker_bytes = json_bytes(&marker)?;
            directory.publish(
                "events.commit.json",
                &marker_bytes,
                false,
                &mut counter,
                observer.as_mut(),
                MARKER_STEPS,
            )?;
            directory.publish(
                "archive.json",
                &descriptor_bytes,
                false,
                &mut counter,
                observer.as_mut(),
                ARTIFACT_STEPS,
            )?;
            let (_, identity, _) = prepared(&directory, &limits)?;
            let receipt = receipt(identity, &marker, &marker_bytes);
            Ok(Self {
                directory,
                _lock: lock,
                events,
                descriptor,
                limits,
                state,
                hash: Sha256::new(),
                receipt,
                temporary_counter: counter,
                stopped: false,
                observer,
            })
        };
        creation().map_err(|mut error| {
            error.outcome_uncertain = true;
            error
        })
    }

    pub fn confirmed_frontier(&self) -> &CommittedFrontier {
        &self.receipt
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Replace the validation observer between operations. A stopped writer
    /// remains stopped; installing an observer cannot make a retry legal.
    pub fn set_observer(&mut self, observer: Box<dyn PublicationObserver>) {
        self.observer = observer;
    }

    fn ensure_active(&self) -> Result<()> {
        if self.stopped {
            let mut error = StorageError::new(
                ErrorCode::IoError,
                "writer stopped after uncertain publication; reconciliation requires external ownership",
            );
            error.outcome_uncertain = true;
            Err(error)
        } else {
            Ok(())
        }
    }

    fn stop_on_failure<T>(&mut self, result: Result<T>) -> Result<T> {
        result.map_err(|mut error| {
            self.stopped = true;
            error.outcome_uncertain = true;
            error
        })
    }

    /// An immutable object is not evidence until a committed event references
    /// it. Existing identical bytes are verified; different bytes never replace
    /// the existing object. No input may be added after preparation.
    pub fn publish_artifact(&mut self, content: &ContentRef, bytes: &[u8]) -> Result<()> {
        let result = self.publish_artifact_inner(content, bytes);
        if result
            .as_ref()
            .is_err_and(|error| error.code == ErrorCode::IoError)
        {
            self.stop_on_failure(result)
        } else {
            result
        }
    }

    fn publish_artifact_inner(&mut self, content: &ContentRef, bytes: &[u8]) -> Result<()> {
        self.ensure_active()?;
        evidence_path(content)?;
        if content.path.as_str().starts_with("inputs/") {
            return Err(StorageError::malformed(
                "frozen inputs cannot be added after preparation",
            ));
        }
        verify_bytes(content, bytes, self.limits.content_bytes)?;
        match self.directory.verify(content, self.limits.content_bytes) {
            Ok(_) => return Ok(()),
            Err(error) if error.io_kind == Some(io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let result = (|| {
            self.directory.publish(
                content.path.as_str(),
                bytes,
                false,
                &mut self.temporary_counter,
                self.observer.as_mut(),
                ARTIFACT_STEPS,
            )?;
            self.directory.verify(content, self.limits.content_bytes)?;
            Ok(())
        })();
        self.stop_on_failure(result)
    }

    /// Each supplied slice is exact JSON plus one LF. Validation failures before
    /// publication leave the writer usable. Any publication failure stops it.
    pub fn append(&mut self, lines: &[&[u8]], class: BatchClass) -> Result<CommittedFrontier> {
        let result = self.append_inner(lines, class);
        if result
            .as_ref()
            .is_err_and(|error| error.code == ErrorCode::IoError)
        {
            self.stop_on_failure(result)
        } else {
            result
        }
    }

    fn append_inner(&mut self, lines: &[&[u8]], class: BatchClass) -> Result<CommittedFrontier> {
        self.ensure_active()?;
        if lines.is_empty() {
            return Err(StorageError::malformed("empty batches are not commits"));
        }
        let batch_bytes = lines
            .iter()
            .try_fold(0_u64, |size, line| size.checked_add(line.len() as u64))
            .ok_or_else(|| StorageError::limit("batch length overflow"))?;
        if batch_bytes > self.limits.batch_bytes {
            return Err(StorageError::limit("batch byte bound exceeded"));
        }
        let committed_bytes = self
            .receipt
            .frontier
            .committed_bytes
            .checked_add(batch_bytes)
            .ok_or_else(|| StorageError::limit("event log length overflow"))?;
        let allowance = match class {
            BatchClass::Ordinary => self
                .limits
                .event_log_bytes
                .saturating_sub(FINALIZATION_RESERVE_BYTES),
            BatchClass::Finalization => self.limits.event_log_bytes,
        };
        if committed_bytes > allowance {
            return Err(StorageError::limit(
                "event log allowance/finalization reserve exceeded",
            ));
        }
        let mut next = self.state.clone();
        let mut references = BTreeMap::new();
        for bytes in lines {
            let decoded = decode_line(bytes, &self.limits)?;
            if class == BatchClass::Ordinary
                && matches!(
                    decoded.payload().known(),
                    Some(KnownEvent::RunFinished { .. })
                )
            {
                return Err(StorageError::malformed(
                    "run_finished requires an explicitly finalization-class batch",
                ));
            }
            for reference in next.observe(&decoded, bytes, &self.descriptor, &self.limits)? {
                references.insert(reference.path.clone(), reference);
            }
        }
        for content in references.values() {
            self.directory.verify(content, self.limits.content_bytes)?;
        }
        if self.events.metadata()?.len() != self.receipt.frontier.committed_bytes {
            return self.stop_on_failure(Err(StorageError::corrupt(
                "event file changed outside this writer",
            )));
        }
        let mut hash = self.hash.clone();
        for bytes in lines {
            hash.update(bytes);
        }
        let marker = CommitMarker {
            schema_version: COMMIT_SCHEMA_VERSION,
            run_id: self.descriptor.run_id.clone(),
            committed_sequence: next.sequence,
            committed_bytes,
            prefix_sha256: digest_value(hash.clone().finalize().as_slice()),
        };
        let marker_bytes = json_bytes(&marker)?;
        let publication = (|| {
            // Also sync references supplied by an external producer. Content is
            // verified before events; no checkpoint acknowledgment precedes this.
            for content in references.values() {
                self.directory
                    .sync_content(content, self.observer.as_mut())?;
            }
            for bytes in lines {
                write_all(
                    &mut self.events,
                    bytes,
                    self.observer.as_mut(),
                    WriteStep::EventsWrite,
                )?;
            }
            sync(&self.events, self.observer.as_mut(), WriteStep::EventsSync)?;
            self.directory.publish(
                "events.commit.json",
                &marker_bytes,
                true,
                &mut self.temporary_counter,
                self.observer.as_mut(),
                MARKER_STEPS,
            )?;
            Ok(())
        })();
        self.stop_on_failure(publication)?;
        self.receipt = receipt(
            self.receipt.frontier.archive.clone(),
            &marker,
            &marker_bytes,
        );
        self.state = next;
        self.hash = hash;
        Ok(self.receipt.clone())
    }

    /// Optional derived bytes. A failure here cannot revoke an already returned
    /// terminal commit receipt, and readers never consult result.json.
    pub fn regenerate_result(&mut self) -> Result<()> {
        self.ensure_active()?;
        let terminal = self
            .state
            .terminal
            .as_ref()
            .ok_or_else(|| StorageError::malformed("no committed run_finished to derive"))?;
        let result = self.directory.publish(
            "result.json",
            &terminal.bytes,
            true,
            &mut self.temporary_counter,
            self.observer.as_mut(),
            [
                WriteStep::ResultWrite,
                WriteStep::ResultSync,
                WriteStep::ResultRename,
                WriteStep::ResultDirectorySync,
            ],
        );
        self.stop_on_failure(result)
    }
}

fn receipt(
    archive: ArchiveIdentity,
    marker: &CommitMarker,
    marker_bytes: &[u8],
) -> CommittedFrontier {
    CommittedFrontier {
        frontier: EventFrontier {
            archive,
            marker: reference("events.commit.json", marker_bytes),
            committed_sequence: marker.committed_sequence,
            committed_bytes: marker.committed_bytes,
            prefix_sha256: marker.prefix_sha256.clone(),
        },
    }
}
