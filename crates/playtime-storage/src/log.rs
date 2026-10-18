use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
};

use playtime_core::{ArchivePath, RequestId, UsageTotals};
use playtime_protocol::{
    archive::{ContentRef, InputManifest},
    bridge::{BridgeEvent, decode_checkpoint_index, decode_source_map},
    events::{CheckpointRef, DecodedEvent, EventPayload, KnownEvent, MemoryRef, decode_event},
    observations::{ObservationData, ObservationLocation},
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
    observations: BTreeMap<u64, ObservationLocation>,
    roles: BTreeMap<ArchivePath, ReferenceRole>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReferenceRole {
    InputManifest,
    Leaf,
    CheckpointIndex,
    SourceMap,
    ObservationPayload,
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
        state.typed_reference(
            &descriptor.input_manifest,
            ReferenceRole::InputManifest,
            limits,
        )?;
        for reference in manifest.references() {
            state.typed_reference(reference, ReferenceRole::Leaf, limits)?;
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
        directory: &Directory,
        committed_sequence: u64,
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
        match decoded.payload() {
            EventPayload::Observation(observation) => {
                let source = observation.observation();
                if self
                    .observations
                    .last_key_value()
                    .is_some_and(|(previous, _)| *previous >= source.controller_sequence.get())
                {
                    return Err(StorageError::malformed(
                        "controller observation sequences must increase",
                    ));
                }
                if let ObservationData::Artifact { content } = &source.data {
                    self.typed_reference(content, ReferenceRole::ObservationPayload, limits)?;
                    let bytes = directory.verify(content, limits.content_bytes)?;
                    observation.clone().resolve(&bytes)?;
                    references.push(content.clone());
                }
                self.observations.insert(
                    source.controller_sequence.get(),
                    ObservationLocation {
                        controller_sequence: source.controller_sequence,
                        archive_sequence: envelope.sequence,
                        record_sha256: crate::sha256(bytes),
                    },
                );
            }
            EventPayload::Control(control) => {
                if let BridgeEvent::EffectIntent(intent) = control.as_ref() {
                    for source in &intent.sources {
                        self.validate_source(source, self.sequence)?;
                    }
                }
            }
            _ => {}
        }
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
                delivery,
                ..
            }) => {
                references.push(workspace.clone());
                memory_references(memento, evidence, &mut references);
                if let Some(delivery) = delivery {
                    references.push(delivery.memento.content.clone());
                    if let Some(evidence) = &delivery.evidence {
                        references.extend([evidence.text.clone(), evidence.provenance.clone()]);
                    }
                }
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
                if let Some(content) = &checkpoint.index {
                    self.typed_reference(content, ReferenceRole::CheckpointIndex, limits)?;
                    let bytes = directory.verify(content, limits.manifest_bytes)?;
                    let index = decode_checkpoint_index(utf8(&bytes)?)?;
                    if index.files.len() as u64 > limits.references
                        || index.changes.len() as u64 > limits.references
                    {
                        return Err(StorageError::limit(
                            "checkpoint metadata count bound exceeded",
                        ));
                    }
                    if index.run_id != descriptor.run_id
                        || index.session_id != checkpoint.session_id
                        || index.workspace != checkpoint.workspace
                        || index
                            .outgoing_evidence
                            .as_ref()
                            .map(|evidence| &evidence.text)
                            != checkpoint.evidence.as_ref()
                        || checkpoint.memento != index.outgoing_memory.memory_ref()
                    {
                        return Err(StorageError::malformed(
                            "checkpoint index disagrees with checkpoint event",
                        ));
                    }
                    references.push(content.clone());
                    self.typed_reference(&index.source_map, ReferenceRole::SourceMap, limits)?;
                    let map_bytes = directory.verify(&index.source_map, limits.manifest_bytes)?;
                    let map = decode_source_map(utf8(&map_bytes)?)?;
                    if map.entries.len() as u64 > limits.events {
                        return Err(StorageError::limit("source map entry count bound exceeded"));
                    }
                    if map.run_id != descriptor.run_id {
                        return Err(StorageError::malformed(
                            "source map belongs to a different run",
                        ));
                    }
                    for source in &map.entries {
                        self.validate_source(source, committed_sequence.min(self.sequence))?;
                    }
                    references.push(index.source_map.clone());
                    for leaf in index.content_leaves() {
                        self.typed_reference(leaf, ReferenceRole::Leaf, limits)?;
                        references.push(leaf.clone());
                    }
                }
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
        let mut leaves = Vec::new();
        match decoded.payload().known() {
            Some(KnownEvent::SessionStarted {
                workspace,
                memento,
                evidence,
                delivery,
                ..
            }) => {
                leaves.push(workspace.clone());
                memory_references(memento, evidence, &mut leaves);
                if let Some(delivery) = delivery {
                    leaves.push(delivery.memento.content.clone());
                    if let Some(evidence) = &delivery.evidence {
                        leaves.extend([evidence.text.clone(), evidence.provenance.clone()]);
                    }
                }
            }
            Some(KnownEvent::CheckpointPublished { checkpoint }) => {
                checkpoint_references(checkpoint, &mut leaves)
            }
            Some(KnownEvent::RunFinished {
                last_checkpoint: Some(checkpoint),
                ..
            }) => checkpoint_references(checkpoint, &mut leaves),
            _ => {}
        }
        for leaf in &leaves {
            self.typed_reference(leaf, ReferenceRole::Leaf, limits)?;
        }
        for reference in &references {
            self.add_reference(reference, limits)?;
        }
        self.sequence = envelope.sequence.get();
        Ok(references)
    }

    fn typed_reference(
        &mut self,
        reference: &ContentRef,
        role: ReferenceRole,
        limits: &StorageLimits,
    ) -> Result<()> {
        if self
            .roles
            .get(&reference.path)
            .is_some_and(|previous| *previous != role)
        {
            return Err(StorageError::malformed(
                "content reference has conflicting traversal roles or a manifest cycle",
            ));
        }
        self.add_reference(reference, limits)?;
        self.roles.insert(reference.path.clone(), role);
        Ok(())
    }

    fn validate_source(&self, source: &ObservationLocation, committed_sequence: u64) -> Result<()> {
        if source.archive_sequence.get() > committed_sequence
            || self.observations.get(&source.controller_sequence.get()) != Some(source)
        {
            return Err(StorageError::malformed(
                "source reference must match the exact preceding committed observation including LF",
            ));
        }
        Ok(())
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
            .observe(
                &decoded,
                &bytes,
                descriptor,
                limits,
                directory,
                state.sequence,
            )
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
