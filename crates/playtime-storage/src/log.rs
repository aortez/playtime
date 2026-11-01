use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
};

use playtime_core::{ArchivePath, RequestId, Sha256Digest, UsageTotals};
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
    ErrorCode, ReadPrefix, Result, StorageError, StorageLimits,
    files::{Directory, digest_value, utf8},
    reader::StoredEvent,
};

mod execution;

#[derive(Clone, Default)]
pub(crate) struct LogState {
    manifest_version: u32,
    pub sequence: u64,
    execution: execution::ExecutionState,
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
    CaptureIndex,
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
        let mut state = Self {
            manifest_version: manifest.schema_version,
            ..Self::default()
        };
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
        if self.manifest_version == 2
            && (matches!(decoded.payload(), EventPayload::Observation(_))
                || matches!(decoded.payload(), EventPayload::Control(control) if !matches!(control.as_ref(), BridgeEvent::StopDecided(_))))
        {
            return Err(StorageError::malformed(
                "execution v2 requires versioned request/observation records",
            ));
        }
        let mut references = Vec::new();
        match decoded.payload() {
            EventPayload::Execution(record) => {
                self.observe_execution(
                    record,
                    bytes,
                    descriptor,
                    limits,
                    directory,
                    committed_sequence,
                    &mut references,
                )?;
            }
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
        if self.manifest_version == 2
            && matches!(
                decoded.payload().known(),
                Some(
                    KnownEvent::SessionStarted { .. }
                        | KnownEvent::RequestFinished { .. }
                        | KnownEvent::CommandFinished { .. }
                        | KnownEvent::SessionEnded { .. }
                        | KnownEvent::CheckpointPublished { .. }
                        | KnownEvent::RunFinished { .. }
                )
            )
        {
            return Err(StorageError::malformed(
                "legacy session records cannot mix with execution v2",
            ));
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

/// One record as the pass that validated it saw it: where it ends, its own
/// digest, and the digest of the committed prefix through it.
#[derive(Clone, Copy)]
pub(crate) struct Record {
    pub end: u64,
    pub sha256: [u8; 32],
    pub prefix_sha256: [u8; 32],
}

/// A pass's state where it stopped. A later pass can continue it over the
/// bytes after it without reading the earlier ones again, and a read can
/// check the records it returns against what the pass validated.
#[derive(Clone)]
pub(crate) struct Pass {
    pub state: LogState,
    hash: Sha256,
    verified: BTreeSet<ArchivePath>,
    pub records: Vec<Record>,
}

/// What a pass carries from record to record besides the records themselves:
/// the log state, the running digest and the content already verified. A
/// continuation needs it, reads don't, and it is most of a pass's memory.
pub(crate) struct Carried {
    state: LogState,
    hash: Sha256,
    verified: BTreeSet<ArchivePath>,
}

impl Pass {
    /// The records, which reads check, apart from what a continuation needs.
    pub(crate) fn split(self) -> (Vec<Record>, Carried) {
        let Self {
            state,
            hash,
            verified,
            records,
        } = self;
        (
            records,
            Carried {
                state,
                hash,
                verified,
            },
        )
    }

    /// The pass that `split` divided, to continue it.
    pub(crate) fn rejoined(records: &[Record], carried: &Carried) -> Self {
        Self {
            state: carried.state.clone(),
            hash: carried.hash.clone(),
            verified: carried.verified.clone(),
            records: records.to_vec(),
        }
    }

    fn prepared(
        descriptor: &ArchiveDescriptor,
        manifest: &InputManifest,
        limits: &StorageLimits,
    ) -> Result<Self> {
        let state = LogState::prepared(descriptor, manifest, limits)?;
        Ok(Self {
            verified: state.references.keys().cloned().collect(),
            state,
            hash: Sha256::new(),
            records: Vec::new(),
        })
    }

    /// The committed bytes this pass has validated.
    pub(crate) fn bytes(&self) -> u64 {
        self.records.last().map_or(0, |record| record.end)
    }

    fn prefix_sha256(&self) -> Sha256Digest {
        digest_value(self.hash.clone().finalize().as_slice())
    }
}

pub(crate) struct Scan {
    pub pass: Pass,
    pub suffix_bytes: u64,
    /// The pass as it stood at the requested checkpoint, verified as an exact
    /// record boundary of the scanned prefix during the same pass.
    pub checkpoint: Option<Pass>,
}

fn checkpoint_mismatch() -> StorageError {
    StorageError::new(
        ErrorCode::InvalidCursor,
        "checkpoint prefix is not an exact record boundary of the committed prefix",
    )
}

/// What one pass does besides validation: check a position beneath the
/// frontier, hand every record to a visitor once it has validated, or
/// continue an earlier pass instead of starting at the first record.
#[derive(Default)]
pub(crate) struct ScanOptions<'a> {
    pub checkpoint: Option<&'a ReadPrefix>,
    pub visitor: Option<&'a mut dyn FnMut(StoredEvent) -> Result<()>>,
    /// Validates only the bytes after this pass, from its state: it never
    /// reads the earlier records again or verifies the content they reference.
    pub resume: Option<Pass>,
}

/// Validates the committed prefix at `frontier` in one pass, or only the bytes
/// after an earlier pass it resumes. A checkpoint position beneath it is
/// checked where the pass crosses its byte count: the sequence and running
/// SHA-256 must match there exactly, so a position inside a record or with
/// another history fails without a second pass. The pass still runs to the
/// frontier first, so a prefix that fails its own validation reports
/// corruption rather than a bad position.
pub(crate) fn scan(
    directory: &Directory,
    descriptor: &ArchiveDescriptor,
    manifest: &InputManifest,
    frontier: &EventFrontier,
    limits: &StorageLimits,
    options: ScanOptions<'_>,
) -> Result<Scan> {
    let ScanOptions {
        checkpoint,
        mut visitor,
        resume,
    } = options;
    if checkpoint.is_some_and(|prefix| {
        prefix.bytes > frontier.committed_bytes || prefix.sequence > frontier.committed_sequence
    }) {
        return Err(StorageError::new(
            ErrorCode::CursorAhead,
            "checkpoint prefix exceeds the committed prefix",
        ));
    }
    if resume.as_ref().is_some_and(|pass| {
        pass.bytes() > frontier.committed_bytes || pass.state.sequence > frontier.committed_sequence
    }) {
        return Err(StorageError::new(
            ErrorCode::CursorAhead,
            "previous prefix exceeds the committed prefix",
        ));
    }
    if frontier.committed_bytes > limits.event_log_bytes
        || frontier.committed_sequence > limits.events
    {
        return Err(StorageError::limit(
            "committed prefix exceeds reader limits",
        ));
    }
    let mut file = directory.open_file("events.jsonl", OFlags::RDONLY)?;
    let length = file.metadata()?.len();
    if length < frontier.committed_bytes {
        return Err(StorageError::corrupt(
            "commit marker points beyond event file",
        ));
    }
    let mut pass = match resume {
        Some(pass) => pass,
        None => Pass::prepared(descriptor, manifest, limits)?,
    };
    // The frontier says how many records the pass ends with, within the event
    // bound, so their list is allocated once at that size.
    pass.records
        .reserve_exact((frontier.committed_sequence - pass.state.sequence) as usize);
    let start = pass.bytes();
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(frontier.committed_bytes - start));
    let mut consumed = start;
    let mut checkpoint_state = None;
    let mut checkpoint_crossed = false;
    if let Some(prefix) = checkpoint
        && prefix.bytes <= start
    {
        checkpoint_crossed = true;
        if prefix.bytes == start
            && prefix.sequence == pass.state.sequence
            && prefix.sha256 == pass.prefix_sha256()
        {
            checkpoint_state = Some(pass.clone());
        }
    }
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
        pass.hash.update(&bytes);
        let decoded = decode_line(&bytes, limits).map_err(committed_error)?;
        for reference in pass
            .state
            .observe(
                &decoded,
                &bytes,
                descriptor,
                limits,
                directory,
                pass.state.sequence,
            )
            .map_err(committed_error)?
        {
            if pass.verified.insert(reference.path.clone()) {
                directory.verify(&reference, limits.content_bytes)?;
            }
        }
        pass.records.push(Record {
            end: consumed,
            sha256: digest_bytes(Sha256::digest(&bytes).as_slice()),
            prefix_sha256: digest_bytes(pass.hash.clone().finalize().as_slice()),
        });
        if let Some(prefix) = checkpoint
            && !checkpoint_crossed
            && consumed >= prefix.bytes
        {
            checkpoint_crossed = true;
            if consumed == prefix.bytes
                && pass.state.sequence == prefix.sequence
                && pass.prefix_sha256() == prefix.sha256
            {
                checkpoint_state = Some(pass.clone());
            }
        }
        if let Some(visitor) = visitor.as_mut() {
            visitor(StoredEvent::new(decoded.envelope().clone(), bytes))?;
        }
    }
    if consumed != frontier.committed_bytes
        || pass.state.sequence != frontier.committed_sequence
        || pass.prefix_sha256() != frontier.prefix_sha256
    {
        return Err(StorageError::corrupt(
            "committed prefix length, sequence, or SHA-256 mismatch",
        ));
    }
    if checkpoint.is_some() && checkpoint_state.is_none() {
        return Err(checkpoint_mismatch());
    }
    Ok(Scan {
        pass,
        suffix_bytes: length - frontier.committed_bytes,
        checkpoint: checkpoint_state,
    })
}

pub(crate) fn digest_bytes(digest: &[u8]) -> [u8; 32] {
    digest.try_into().expect("SHA-256 has 32 bytes")
}

pub(crate) fn committed_error(error: StorageError) -> StorageError {
    if error.code == ErrorCode::MalformedData {
        StorageError::corrupt(error.message)
    } else {
        error
    }
}
