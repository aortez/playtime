use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
};

use playtime_core::{ArchivePath, RequestId, UsageTotals};
use playtime_protocol::{
    archive::{ContentRef, InputManifest},
    events::{CheckpointRef, DecodedEvent, KnownEvent, MemoryRef, decode_event},
    storage::{ArchiveDescriptor, EventFrontier},
    v1::UsageSummary,
};
use rustix::fs::OFlags;
use sha2::{Digest, Sha256};

use crate::{
    ErrorCode, Result, StorageError, StorageLimits,
    files::{Directory, digest_value, utf8},
    reader::StoredEvent,
};

#[derive(Clone, Default)]
pub(crate) struct LogState {
    pub sequence: u64,
    pub usage: UsageTotals,
    requests: BTreeSet<RequestId>,
    pub checkpoints: Vec<CheckpointRef>,
    pub terminal: Option<StoredEvent>,
    pub references: BTreeMap<ArchivePath, ContentRef>,
    pub referenced_bytes: u64,
}

pub(crate) fn evidence_path(reference: &ContentRef) -> Result<()> {
    if !["inputs/", "sessions/", "artifacts/"]
        .iter()
        .any(|prefix| reference.path.as_str().starts_with(prefix))
    {
        return Err(StorageError::malformed(
            "content must be under inputs/, sessions/, or artifacts/",
        ));
    }
    Ok(())
}

impl LogState {
    pub(crate) fn prepared(
        descriptor: &ArchiveDescriptor,
        manifest: &InputManifest,
        limits: &StorageLimits,
    ) -> Result<Self> {
        let mut state = Self::default();
        state.add_reference(&descriptor.input_manifest, limits)?;
        for reference in manifest.references() {
            state.add_reference(reference, limits)?;
        }
        Ok(state)
    }

    pub(crate) fn add_reference(
        &mut self,
        reference: &ContentRef,
        limits: &StorageLimits,
    ) -> Result<bool> {
        evidence_path(reference)?;
        if let Some(previous) = self.references.get(&reference.path) {
            if previous != reference {
                return Err(StorageError::new(
                    ErrorCode::ContentMismatch,
                    "immutable path referenced with different identity",
                ));
            }
            return Ok(false);
        }
        if reference.size_bytes > limits.content_bytes
            || self.references.len() as u64 >= limits.references
        {
            return Err(StorageError::limit("content/reference bound exceeded"));
        }
        self.referenced_bytes = self
            .referenced_bytes
            .checked_add(reference.size_bytes)
            .filter(|size| *size <= limits.verified_content_bytes)
            .ok_or_else(|| StorageError::limit("total verified content byte bound exceeded"))?;
        self.references
            .insert(reference.path.clone(), reference.clone());
        Ok(true)
    }

    pub(crate) fn observe(
        &mut self,
        decoded: &DecodedEvent,
        bytes: &[u8],
        descriptor: &ArchiveDescriptor,
        limits: &StorageLimits,
    ) -> Result<Vec<ContentRef>> {
        let envelope = decoded.envelope();
        if envelope.run_id != descriptor.run_id
            || self.sequence.checked_add(1) != Some(envelope.sequence.get())
        {
            return Err(StorageError::malformed(
                "events must match the run and be contiguous from sequence 1",
            ));
        }
        if self.sequence >= limits.events {
            return Err(StorageError::limit("event count bound exceeded"));
        }
        if self.terminal.is_some() {
            return Err(StorageError::malformed("cannot append after run_finished"));
        }
        if self.sequence == 0
            && !matches!(
                decoded.payload().known(),
                Some(KnownEvent::RunStarted { .. })
            )
        {
            return Err(StorageError::malformed(
                "first event must bind run_started to the prepared archive",
            ));
        }
        let mut references = Vec::new();
        match decoded.payload().known() {
            Some(KnownEvent::RunStarted {
                mode,
                input_manifest,
            }) => {
                if self.sequence != 0
                    || *mode != descriptor.mode
                    || *input_manifest != descriptor.input_manifest
                {
                    return Err(StorageError::malformed(
                        "run_started disagrees with the archive descriptor or is repeated",
                    ));
                }
                references.push(input_manifest.clone());
            }
            Some(KnownEvent::SessionStarted {
                workspace,
                memento,
                evidence,
                ..
            }) => {
                references.push(workspace.clone());
                memory_references(memento, evidence, &mut references);
            }
            Some(KnownEvent::RequestFinished {
                request_id, usage, ..
            }) => {
                if !self.requests.insert(request_id.clone()) {
                    return Err(StorageError::malformed("duplicate request settlement"));
                }
                self.usage.record(*usage).map_err(StorageError::malformed)?;
            }
            Some(KnownEvent::CheckpointPublished { checkpoint }) => {
                checkpoint_references(checkpoint, &mut references);
                self.checkpoints.push(checkpoint.clone());
            }
            Some(KnownEvent::RunFinished {
                usage,
                last_checkpoint,
                ..
            }) => {
                if *usage != UsageSummary::from(self.usage) {
                    return Err(StorageError::malformed(
                        "run_finished usage disagrees with committed request settlements",
                    ));
                }
                if last_checkpoint.as_ref() != self.checkpoints.last() {
                    return Err(StorageError::malformed(
                        "run_finished must name the last published checkpoint",
                    ));
                }
                if let Some(checkpoint) = last_checkpoint {
                    checkpoint_references(checkpoint, &mut references);
                }
                self.terminal = Some(StoredEvent::new(envelope.clone(), bytes.to_vec()));
            }
            _ => {}
        }
        for reference in &references {
            self.add_reference(reference, limits)?;
        }
        self.sequence = envelope.sequence.get();
        Ok(references)
    }
}

fn memory_references(
    memento: &MemoryRef,
    evidence: &Option<ContentRef>,
    references: &mut Vec<ContentRef>,
) {
    if let MemoryRef::Available { content } = memento {
        references.push(content.clone());
    }
    if let Some(content) = evidence {
        references.push(content.clone());
    }
}
fn checkpoint_references(checkpoint: &CheckpointRef, references: &mut Vec<ContentRef>) {
    references.push(checkpoint.workspace.clone());
    memory_references(&checkpoint.memento, &checkpoint.evidence, references);
}

pub(crate) fn decode_line(bytes: &[u8], limits: &StorageLimits) -> Result<DecodedEvent> {
    if bytes.len() as u64 > limits.record_bytes {
        return Err(StorageError::limit(
            "event record byte bound exceeded (including LF)",
        ));
    }
    if bytes.last() != Some(&b'\n') || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n') {
        return Err(StorageError::malformed(
            "each event must be one complete JSON object followed by LF",
        ));
    }
    decode_event(utf8(bytes)?).map_err(Into::into)
}

pub(crate) struct Scan {
    pub state: LogState,
    pub events: Vec<StoredEvent>,
    pub suffix_bytes: u64,
}

pub(crate) fn scan(
    directory: &Directory,
    descriptor: &ArchiveDescriptor,
    manifest: &InputManifest,
    frontier: &EventFrontier,
    limits: &StorageLimits,
    after: u64,
    limit: u32,
) -> Result<Scan> {
    if frontier.committed_bytes > limits.event_log_bytes
        || frontier.committed_sequence > limits.events
    {
        return Err(StorageError::limit(
            "committed prefix exceeds reader limits",
        ));
    }
    let file = directory.open_file("events.jsonl", OFlags::RDONLY)?;
    let length = file.metadata()?.len();
    if length < frontier.committed_bytes {
        return Err(StorageError::corrupt(
            "commit marker points beyond event file",
        ));
    }
    let mut reader = BufReader::new(file.take(frontier.committed_bytes));
    let mut hash = Sha256::new();
    let mut state = LogState::prepared(descriptor, manifest, limits)?;
    let mut verified: BTreeSet<_> = state.references.keys().cloned().collect();
    let mut events = Vec::new();
    let mut page_bytes = 0;
    let mut page_full = false;
    let mut consumed = 0;
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take(limits.record_bytes + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        consumed += count as u64;
        hash.update(&bytes);
        let decoded = decode_line(&bytes, limits).map_err(committed_error)?;
        for reference in state
            .observe(&decoded, &bytes, descriptor, limits)
            .map_err(committed_error)?
        {
            if verified.insert(reference.path.clone()) {
                directory.verify(&reference, limits.content_bytes)?;
            }
        }
        if !page_full && state.sequence > after && events.len() < limit as usize {
            // Returning fewer than limit is allowed; every page still has a
            // strict byte bound. Validate the rest of the prefix before return.
            if page_bytes + bytes.len() as u64 <= limits.page_bytes {
                page_bytes += bytes.len() as u64;
                events.push(StoredEvent::new(decoded.envelope().clone(), bytes));
            } else {
                // Once full, never skip a large record to fit a later small one.
                // The next page starts immediately after the last returned event.
                page_full = true;
            }
        }
    }
    if consumed != frontier.committed_bytes
        || state.sequence != frontier.committed_sequence
        || digest_value(hash.finalize().as_slice()) != frontier.prefix_sha256
    {
        return Err(StorageError::corrupt(
            "committed prefix length, sequence, or SHA-256 mismatch",
        ));
    }
    Ok(Scan {
        state,
        events,
        suffix_bytes: length - frontier.committed_bytes,
    })
}

fn committed_error(error: StorageError) -> StorageError {
    if error.code == ErrorCode::MalformedData {
        StorageError::corrupt(error.message)
    } else {
        error
    }
}
