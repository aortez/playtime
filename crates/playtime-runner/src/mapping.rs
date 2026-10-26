use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU64,
};

use playtime_core::{RunId, RunMode, SessionId, UtcTime};
use playtime_engine::{
    BoundRecord, Checkpoint, ControlFact, ControlUpdate, FileIndex, Memory, RunResult,
    evidence::Evidence,
};
use playtime_protocol::{
    archive::ContentRef,
    bridge::*,
    events::{CheckpointRef, ContextStart, EventEnvelope, KnownEvent},
    observations::{
        ControllerObservation, ControllerRecord, INLINE_OBSERVATION_BYTES, ObservationData,
        ObservationIdentity, ObservationLocation,
    },
    storage::FINALIZATION_RESERVE_BYTES,
};
use playtime_storage::{BatchClass, FrozenInput, StorageLimits, sha256};
use serde::Serialize;
use serde_json::Value;

use crate::{Error, Result, prepare::reference};

pub(crate) const STREAM_BATCH_BYTES: u64 = 64 * 1024;
pub(crate) const STREAM_BATCH_MS: u64 = 250;

mod execution;

pub(crate) struct EncodedEvent {
    pub line: Vec<u8>,
    pub objects: Vec<FrozenInput>,
    pub kind: String,
    pub sequence: u64,
    pub effect_id: Option<u64>,
    pub log_cost: u64,
}

pub(crate) struct Publication {
    pub events: Vec<EncodedEvent>,
    pub class: BatchClass,
    pub checkpoint: Option<(u64, CheckpointRef, FileIndex)>,
    pub capture: Option<(u64, playtime_protocol::execution::CaptureRef)>,
}
impl Publication {
    pub fn log_cost(&self) -> u64 {
        self.events.iter().map(|event| event.log_cost).sum()
    }
    pub fn queued_bytes(&self) -> u64 {
        self.events
            .iter()
            .map(|event| {
                event.line.len() as u64
                    + event
                        .objects
                        .iter()
                        .map(|object| object.bytes.len() as u64)
                        .sum::<u64>()
            })
            .sum()
    }
}

pub(crate) struct Encoder {
    run_id: RunId,
    execution_v2: bool,
    last_capture: Option<playtime_protocol::execution::CaptureRef>,
    limits: StorageLimits,
    next_sequence: u64,
    /// Includes committed, in-flight, and queued JSONL plus observation objects.
    planned_log_bytes: u64,
    pub capacity_exhausted: bool,
    pub stopping: bool,
    pub finished: bool,
    queue: VecDeque<EncodedEvent>,
    sources: BTreeMap<u64, (ObservationLocation, ObservationIdentity)>,
    model_sources: BTreeMap<playtime_core::RequestId, ObservationLocation>,
    compaction_plan: Option<ObservationLocation>,
    previous_summary_request: Option<playtime_core::RequestId>,
    intents: BTreeSet<u64>,
    sessions: BTreeMap<SessionId, SessionDelivery>,
    previous_files: FileIndex,
    pub last_checkpoint: Option<CheckpointRef>,
}

fn object(path: &str, bytes: Vec<u8>, objects: &mut Vec<FrozenInput>) -> ContentRef {
    let reference = reference(path, &bytes);
    objects.push(FrozenInput {
        reference: reference.clone(),
        bytes,
    });
    reference
}
fn memory(path: &str, memory: &Memory, objects: &mut Vec<FrozenInput>) -> MemoryContent {
    MemoryContent {
        status: memory.status.clone(),
        content: object(path, memory.content.as_bytes().to_vec(), objects),
    }
}
fn evidence(
    prefix: &str,
    evidence: &Evidence,
    objects: &mut Vec<FrozenInput>,
) -> Result<EvidenceContent> {
    Ok(EvidenceContent {
        text: object(
            &format!("{prefix}.txt"),
            evidence.content.as_bytes().to_vec(),
            objects,
        ),
        provenance: object(
            &format!("{prefix}-provenance.json"),
            serde_json::to_vec(evidence)?,
            objects,
        ),
    })
}

impl Encoder {
    pub fn new(run_id: RunId, limits: StorageLimits, initial_files: FileIndex) -> Self {
        Self {
            run_id,
            execution_v2: false,
            last_capture: None,
            limits,
            next_sequence: 1,
            planned_log_bytes: 0,
            capacity_exhausted: false,
            stopping: false,
            finished: false,
            queue: VecDeque::new(),
            sources: BTreeMap::new(),
            model_sources: BTreeMap::new(),
            compaction_plan: None,
            previous_summary_request: None,
            intents: BTreeSet::new(),
            sessions: BTreeMap::new(),
            previous_files: initial_files,
            last_checkpoint: None,
        }
    }
    #[cfg(test)]
    pub fn start(&mut self, time: &UtcTime, manifest: ContentRef) -> Result<()> {
        self.start_mode(time, manifest, RunMode::Simulation)
    }
    pub fn start_mode(
        &mut self,
        time: &UtcTime,
        manifest: ContentRef,
        mode: RunMode,
    ) -> Result<()> {
        self.canonical(
            time,
            KnownEvent::RunStarted {
                mode,
                input_manifest: manifest,
            },
            Vec::new(),
        )
    }
    pub fn has_queue(&self) -> bool {
        !self.queue.is_empty()
    }
    pub fn queue_log_bytes(&self) -> u64 {
        self.queue.iter().map(|event| event.log_cost).sum()
    }
    pub fn queued_bytes(&self) -> u64 {
        self.queue
            .iter()
            .map(|event| {
                event.line.len() as u64
                    + event
                        .objects
                        .iter()
                        .map(|object| object.bytes.len() as u64)
                        .sum::<u64>()
            })
            .sum()
    }

    pub fn observations(&mut self, time: &UtcTime, records: Vec<BoundRecord>) -> Result<()> {
        for entry in records {
            let mut record = entry.record;
            if self.execution_v2 {
                self.link_compaction(&mut record)?;
            }
            let original_sequence = NonZeroU64::new(record.sequence).unwrap();
            let source = ControllerRecord {
                sequence: original_sequence,
                elapsed_ms: record.elapsed_ms,
                session_id: record.session_id.clone(),
                kind: record.kind.clone(),
                data: record.data.clone(),
            };
            let bytes = serde_json::to_vec(&record.data)?;
            let mut objects = Vec::new();
            let external_bytes = if bytes.len() > INLINE_OBSERVATION_BYTES {
                bytes.len() as u64
            } else {
                0
            };
            let data = if external_bytes > 0 {
                ObservationData::Artifact {
                    content: object(
                        &format!("artifacts/observations/{}.json", record.sequence),
                        bytes,
                        &mut objects,
                    ),
                }
            } else {
                ObservationData::Inline { value: record.data }
            };
            let (kind, data) = if self.execution_v2 {
                use playtime_protocol::execution::{Fact, KIND, Observation, Record};
                (
                    KIND,
                    serde_json::to_value(Record::new(Fact::Observation(Observation {
                        controller_sequence: original_sequence,
                        elapsed_ms: record.elapsed_ms,
                        scope: playtime_core::RequestScope::Run,
                        observation_kind: record.kind.clone(),
                        identity: entry.identity.clone(),
                        request: entry.request.as_ref().map(execution::request_identity),
                        data,
                    })))?,
                )
            } else {
                (
                    "controller_observation",
                    serde_json::to_value(ControllerObservation::from_record(
                        source,
                        entry.identity.clone(),
                        data,
                    ))?,
                )
            };
            let line = self.encode(time, kind, data)?;
            let log_cost = line.len() as u64 + external_bytes;
            let ordinary_ceiling = self
                .limits
                .event_log_bytes
                .saturating_sub(FINALIZATION_RESERVE_BYTES);
            if self.capacity_exhausted
                || self.planned_log_bytes.saturating_add(log_cost) > ordinary_ceiling
            {
                self.capacity_exhausted = true;
                if self.execution_v2 {
                    self.execution_event(
                        time,
                        playtime_protocol::execution::Fact::ObservationOmitted {
                            controller_sequence: Some(original_sequence),
                            observation_kind: record.kind,
                            elapsed_ms: record.elapsed_ms,
                            scope: playtime_core::RequestScope::Run,
                            identity: entry.identity,
                            request: entry.request.as_ref().map(execution::request_identity),
                            size_bytes: log_cost,
                            reason: "archive_log_limit".into(),
                        },
                        Vec::new(),
                    )?;
                    continue;
                }
                self.control_event(
                    time,
                    BridgeEvent::ObservationOmitted(ObservationOmitted {
                        schema_version: 1,
                        controller_sequence: Some(original_sequence),
                        observation_kind: record.kind,
                        elapsed_ms: record.elapsed_ms,
                        session_id: record.session_id,
                        identity: entry.identity,
                        size_bytes: log_cost,
                        reason: "archive_log_limit".into(),
                    }),
                )?;
            } else {
                let location = ObservationLocation {
                    controller_sequence: original_sequence,
                    archive_sequence: NonZeroU64::new(self.next_sequence).unwrap(),
                    record_sha256: sha256(&line),
                };
                self.push(EncodedEvent {
                    line,
                    objects,
                    sequence: self.next_sequence,
                    kind: "controller_observation".into(),
                    effect_id: None,
                    log_cost,
                })?;
                self.sources
                    .insert(record.sequence, (location.clone(), entry.identity.clone()));
                if self.execution_v2 {
                    match record.kind.as_str() {
                        "model_request" => {
                            self.model_sources
                                .insert(entry.identity.request_id.unwrap(), location);
                        }
                        "compaction_planned" => self.compaction_plan = Some(location),
                        "compaction_applied" => {
                            self.previous_summary_request = entry.identity.request_id
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    pub fn controls(&mut self, time: &UtcTime, updates: Vec<ControlUpdate>) -> Result<()> {
        for update in updates {
            if self.execution_v2
                && !matches!(
                    update.fact,
                    ControlFact::PhaseChanged(_)
                        | ControlFact::StopDecided(_)
                        | ControlFact::Finished
                )
            {
                self.execution_control(time, update)?;
                continue;
            }
            let session = update.session_id.clone();
            match update.fact {
                ControlFact::WorkspaceStarted { .. } | ControlFact::CaptureCommitted { .. } => {
                    return Err(Error::Invalid(
                        "session-neutral facts require execution v2".into(),
                    ));
                }
                ControlFact::PhaseChanged(phase) => self.canonical(
                    time,
                    KnownEvent::PhaseChanged {
                        phase,
                        session_id: session,
                    },
                    Vec::new(),
                )?,
                ControlFact::EffectPending(info) => {
                    if !self.intents.insert(info.id) {
                        return Err(Error::Invalid("duplicate controller effect intent".into()));
                    }
                    let sources = self
                        .sources
                        .values()
                        .filter(|(_, identity)| {
                            identity.effect_id.is_some_and(|id| id.get() == info.id)
                                || (info.identity.tool_request_sequence.is_some()
                                    && identity.tool_request_sequence
                                        == info.identity.tool_request_sequence)
                        })
                        .map(|(location, _)| location.clone())
                        .collect();
                    self.control_event(
                        time,
                        BridgeEvent::EffectIntent(EffectIntent {
                            schema_version: 1,
                            effect_id: NonZeroU64::new(info.id).unwrap(),
                            effect_kind: info.kind.into(),
                            session_id: info.session_id,
                            request_id: info.identity.request_id,
                            tool_request_sequence: info.identity.tool_request_sequence,
                            maximum_tokens: info.maximum_tokens.and_then(NonZeroU64::new),
                            deadline_ms: info.deadline_ms,
                            sources,
                        }),
                    )?;
                }
                ControlFact::EffectNotDispatched(info) => self.control_event(
                    time,
                    BridgeEvent::EffectNotDispatched(EffectNotDispatched {
                        schema_version: 1,
                        effect_id: NonZeroU64::new(info.id).unwrap(),
                        session_id: info.session_id,
                        identity: info.identity,
                    }),
                )?,
                ControlFact::RequestSettled { request, usage, .. } => self.canonical(
                    time,
                    KnownEvent::RequestFinished {
                        session_id: request.scope.session_id().cloned().ok_or_else(|| {
                            Error::Invalid("legacy event requires session scope".into())
                        })?,
                        request_id: request.request_id,
                        usage,
                        recovery_origin: None,
                    },
                    Vec::new(),
                )?,
                ControlFact::RequestNotDispatched {
                    request,
                    released_tokens,
                } => self.control_event(
                    time,
                    BridgeEvent::RequestNotDispatched(RequestNotDispatched {
                        schema_version: 1,
                        effect_id: NonZeroU64::new(request.effect_id).unwrap(),
                        session_id: request.scope.session_id().cloned().ok_or_else(|| {
                            Error::Invalid("legacy event requires session scope".into())
                        })?,
                        request_id: request.request_id,
                        released_tokens: NonZeroU64::new(released_tokens).unwrap(),
                    }),
                )?,
                ControlFact::SessionStarted {
                    ordinal,
                    workspace,
                    memory: incoming,
                    evidence: appendix,
                } => {
                    let session_id = session
                        .ok_or_else(|| Error::Invalid("session startup lacks identity".into()))?;
                    let mut objects = Vec::new();
                    let memento = memory(
                        &format!("sessions/{session_id}/incoming-memory.txt"),
                        &incoming,
                        &mut objects,
                    );
                    let appendix = appendix
                        .as_ref()
                        .map(|item| {
                            evidence(
                                &format!("sessions/{session_id}/incoming-evidence"),
                                item,
                                &mut objects,
                            )
                        })
                        .transpose()?;
                    let delivery = SessionDelivery {
                        memento,
                        evidence: appendix,
                    };
                    self.canonical(
                        time,
                        KnownEvent::SessionStarted {
                            session_id: session_id.clone(),
                            ordinal: ordinal.try_into().unwrap(),
                            context: ContextStart::Fresh,
                            workspace,
                            memento: delivery.memento.memory_ref(),
                            evidence: delivery.evidence.as_ref().map(|item| item.text.clone()),
                            delivery: Some(delivery.clone()),
                        },
                        objects,
                    )?;
                    self.sessions.insert(session_id, delivery);
                }
                ControlFact::SessionClosed {
                    finish_run,
                    handoff_saved,
                } => self.canonical(
                    time,
                    KnownEvent::SessionEnded {
                        session_id: session.ok_or_else(|| {
                            Error::Invalid("session closure lacks identity".into())
                        })?,
                        finish_run,
                        handoff_saved,
                    },
                    Vec::new(),
                )?,
                ControlFact::ExecResult {
                    request,
                    effect_id,
                    tool_request_sequence,
                    outcome,
                } => self.canonical(
                    time,
                    KnownEvent::CommandFinished {
                        session_id: request.scope.session_id().cloned().ok_or_else(|| {
                            Error::Invalid("legacy event requires session scope".into())
                        })?,
                        request_id: request.request_id.clone(),
                        outcome,
                        origin: Some(ObservationIdentity {
                            effect_id: NonZeroU64::new(effect_id),
                            request_id: Some(request.request_id),
                            tool_request_sequence: NonZeroU64::new(tool_request_sequence),
                        }),
                    },
                    Vec::new(),
                )?,
                // The driver already committed the canonical checkpoint before
                // delivering this receipt. Its raw observation remains a copy.
                ControlFact::CheckpointCommitted { session_id, .. } => {
                    if self
                        .last_checkpoint
                        .as_ref()
                        .is_none_or(|checkpoint| checkpoint.session_id != session_id)
                    {
                        return Err(Error::Invalid(
                            "controller acknowledged a checkpoint without its writer receipt"
                                .into(),
                        ));
                    }
                }
                ControlFact::StopDecided(reason) => {
                    self.stopping = true;
                    self.control_event(
                        time,
                        BridgeEvent::StopDecided(StopDecided {
                            schema_version: 1,
                            reason,
                            elapsed_ms: update.elapsed_ms,
                        }),
                    )?;
                }
                ControlFact::Finished => self.finished = true,
                ControlFact::ObservationOmitted {
                    kind,
                    size_bytes,
                    identity,
                    request: _,
                } => self.control_event(
                    time,
                    BridgeEvent::ObservationOmitted(ObservationOmitted {
                        schema_version: 1,
                        controller_sequence: None,
                        observation_kind: kind,
                        elapsed_ms: update.elapsed_ms,
                        session_id: session,
                        identity,
                        size_bytes,
                        reason: "controller_log_limit".into(),
                    }),
                )?,
            }
        }
        Ok(())
    }

    fn encode(&self, time: &UtcTime, kind: &str, data: Value) -> Result<Vec<u8>> {
        let envelope = EventEnvelope {
            schema_version: 1,
            run_id: self.run_id.clone(),
            sequence: NonZeroU64::new(self.next_sequence)
                .ok_or_else(|| Error::Capacity("archive sequence overflow".into()))?,
            time: time.clone(),
            kind: kind.into(),
            data,
        };
        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');
        if line.len() as u64 > self.limits.record_bytes {
            return Err(Error::Capacity(
                "serialized archive event exceeds record limit".into(),
            ));
        }
        Ok(line)
    }
    fn push(&mut self, event: EncodedEvent) -> Result<()> {
        let total = self
            .planned_log_bytes
            .checked_add(event.log_cost)
            .ok_or_else(|| Error::LogLimit("archive byte accounting overflow".into()))?;
        if total > self.limits.event_log_bytes {
            return Err(Error::LogLimit(
                "required archive facts exceed finalization allowance".into(),
            ));
        }
        // Reaching the ordinary ceiling is a policy input, even if a compact
        // mandatory fact can still use finalization space.
        if total
            > self
                .limits
                .event_log_bytes
                .saturating_sub(FINALIZATION_RESERVE_BYTES)
        {
            self.capacity_exhausted = true;
        }
        let object_bytes: u64 = event
            .objects
            .iter()
            .map(|object| object.bytes.len() as u64)
            .sum();
        if object_bytes > self.limits.verified_content_bytes
            || event.objects.len() as u64 > self.limits.references
            || event
                .objects
                .iter()
                .any(|object| object.reference.size_bytes > self.limits.content_bytes)
        {
            return Err(Error::Capacity(
                "queued content exceeds bounded publication limits".into(),
            ));
        }
        self.planned_log_bytes = total;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Capacity("archive sequence overflow".into()))?;
        self.queue.push_back(event);
        if self.queued_bytes() > self.limits.verified_content_bytes + self.limits.batch_bytes {
            return Err(Error::Capacity(
                "pending archive queue exceeds its byte bound".into(),
            ));
        }
        Ok(())
    }
    fn typed(
        &mut self,
        time: &UtcTime,
        event: impl Serialize,
        objects: Vec<FrozenInput>,
    ) -> Result<()> {
        let value = serde_json::to_value(event)?;
        let kind = value["kind"]
            .as_str()
            .ok_or_else(|| Error::Invalid("typed event lacks kind".into()))?;
        let effect_id = value["data"]["effect_id"].as_u64();
        let line = self.encode(time, kind, value["data"].clone())?;
        self.push(EncodedEvent {
            log_cost: line.len() as u64,
            line,
            objects,
            sequence: self.next_sequence,
            kind: kind.into(),
            effect_id,
        })
    }
    fn canonical(
        &mut self,
        time: &UtcTime,
        event: KnownEvent,
        objects: Vec<FrozenInput>,
    ) -> Result<()> {
        self.typed(time, event, objects)
    }
    fn control_event(&mut self, time: &UtcTime, event: BridgeEvent) -> Result<()> {
        self.typed(time, event, Vec::new())
    }

    pub fn batch(&mut self) -> Option<Publication> {
        let first = self.queue.pop_front()?;
        let mut bytes = first.line.len() as u64;
        let mut events = vec![first];
        // Large valid lines are committed individually. Every control barrier
        // drains all batches before the dependent external call can begin.
        while bytes < STREAM_BATCH_BYTES
            && let Some(next) = self.queue.front()
        {
            let size = next.line.len() as u64;
            if size > STREAM_BATCH_BYTES || bytes + size > STREAM_BATCH_BYTES {
                break;
            }
            bytes += size;
            events.push(self.queue.pop_front().unwrap());
        }
        Some(Publication {
            events,
            class: if self.stopping || self.capacity_exhausted || self.finished {
                BatchClass::Finalization
            } else {
                BatchClass::Ordinary
            },
            checkpoint: None,
            capture: None,
        })
    }

    pub fn checkpoint(
        &mut self,
        time: &UtcTime,
        effect_id: u64,
        checkpoint: &Checkpoint,
        workspace_bytes: &[u8],
        committed_sequence: u64,
    ) -> Result<Publication> {
        let file_bytes: BTreeMap<String, String> = serde_json::from_slice(workspace_bytes)?;
        let file_bytes: BTreeMap<_, _> = file_bytes
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_bytes()))
            .collect();
        self.checkpoint_contents(
            time,
            effect_id,
            checkpoint,
            workspace_bytes,
            &file_bytes,
            committed_sequence,
        )
    }

    /// Format-independent publication boundary: validated bytes remain a
    /// candidate until StorageThread returns the writer's receipt. D3 will
    /// supply native capture here; the simulator retains its JSON format.
    pub fn checkpoint_contents(
        &mut self,
        time: &UtcTime,
        effect_id: u64,
        checkpoint: &Checkpoint,
        workspace_bytes: &[u8],
        file_bytes: &BTreeMap<&str, &[u8]>,
        committed_sequence: u64,
    ) -> Result<Publication> {
        if self.has_queue() {
            return Err(Error::Invalid(
                "checkpoint cannot overtake pending observations".into(),
            ));
        }
        if reference(checkpoint.snapshot.workspace.path.as_str(), workspace_bytes)
            != checkpoint.snapshot.workspace
        {
            return Err(Error::Invalid(
                "snapshot bytes differ from controller reference".into(),
            ));
        }
        if file_bytes.keys().any(|path| {
            checkpoint
                .snapshot
                .files
                .get(*path)
                .is_none_or(|m| !matches!(m.entry_type.as_str(), "0" | "file") || m.link.is_some())
        }) {
            return Err(Error::Invalid("unaccounted checkpoint content".into()));
        }
        let prefix = format!("sessions/{}", checkpoint.session_id);
        let mut objects = vec![FrozenInput {
            reference: checkpoint.snapshot.workspace.clone(),
            bytes: workspace_bytes.to_vec(),
        }];
        let mut files = Vec::new();
        let mut changes = Vec::new();
        for (index, (path, metadata)) in checkpoint.snapshot.files.iter().enumerate() {
            let content = if matches!(metadata.entry_type.as_str(), "0" | "file")
                && metadata.link.is_none()
            {
                match file_bytes.get(path.as_str()) {
                    Some(text)
                        if metadata.size_bytes == text.len() as u64
                            && metadata.sha256.as_ref() == Some(&sha256(text)) =>
                    {
                        FileContent::Retained {
                            content: object(
                                &format!("{prefix}/files/{index}"),
                                text.to_vec(),
                                &mut objects,
                            ),
                        }
                    }
                    _ => {
                        return Err(Error::Invalid(
                            "regular file bytes do not match snapshot metadata".into(),
                        ));
                    }
                }
            } else {
                FileContent::Unavailable {
                    reason: "nonregular_entry_metadata_only".into(),
                }
            };
            files.push(FileVersion {
                path: path.clone(),
                size_bytes: metadata.size_bytes,
                mode: metadata.mode,
                entry_type: metadata.entry_type.clone(),
                sha256: metadata.sha256.clone(),
                link: metadata.link.clone(),
                content,
            });
            if self.previous_files.get(path) != Some(metadata) {
                changes.push(FileChange {
                    path: path.clone(),
                    change: if self.previous_files.contains_key(path) {
                        "modified"
                    } else {
                        "added"
                    }
                    .into(),
                });
            }
        }
        for path in self
            .previous_files
            .keys()
            .filter(|path| !checkpoint.snapshot.files.contains_key(*path))
        {
            changes.push(FileChange {
                path: path.clone(),
                change: "removed".into(),
            });
        }
        let incoming = self
            .sessions
            .get(&checkpoint.session_id)
            .cloned()
            .unwrap_or_else(|| SessionDelivery {
                // A partial startup may still leave a stopped, inspectable workspace.
                // No successful startup means no memory/evidence was delivered.
                memento: MemoryContent {
                    status: "not_delivered".into(),
                    content: object(
                        &format!("{prefix}/undelivered-memory.txt"),
                        Vec::new(),
                        &mut objects,
                    ),
                },
                evidence: None,
            });
        let outgoing_memory = memory(
            &format!("{prefix}/outgoing-memory.txt"),
            &checkpoint.snapshot.memory,
            &mut objects,
        );
        let outgoing_evidence = checkpoint
            .evidence
            .as_ref()
            .map(|item| evidence(&format!("{prefix}/outgoing-evidence"), item, &mut objects))
            .transpose()?;
        let map = ObservationSourceMap {
            schema_version: 1,
            run_id: self.run_id.clone(),
            entries: self
                .sources
                .values()
                .filter(|(location, _)| location.archive_sequence.get() <= committed_sequence)
                .map(|(location, _)| location.clone())
                .collect(),
        };
        let map_bytes = serde_json::to_vec(&map)?;
        if map_bytes.len() as u64 > self.limits.manifest_bytes {
            return Err(Error::Capacity(
                "checkpoint source map exceeds manifest limit".into(),
            ));
        }
        let source_map = object(
            &format!("{prefix}/source-map.json"),
            map_bytes,
            &mut objects,
        );
        let index = CheckpointIndex {
            schema_version: 1,
            run_id: self.run_id.clone(),
            session_id: checkpoint.session_id.clone(),
            ordinal: checkpoint.ordinal.try_into().unwrap(),
            workspace: checkpoint.snapshot.workspace.clone(),
            files,
            changes,
            incoming_memory: incoming.memento,
            outgoing_memory: outgoing_memory.clone(),
            incoming_evidence: incoming.evidence,
            outgoing_evidence: outgoing_evidence.clone(),
            source_map,
        };
        let index_bytes = serde_json::to_vec(&index)?;
        if index_bytes.len() as u64 > self.limits.manifest_bytes {
            return Err(Error::Capacity(
                "checkpoint index exceeds manifest limit".into(),
            ));
        }
        let index_ref = object(
            &format!("{prefix}/checkpoint-index.json"),
            index_bytes,
            &mut objects,
        );
        let published = CheckpointRef {
            session_id: checkpoint.session_id.clone(),
            workspace: checkpoint.snapshot.workspace.clone(),
            memento: outgoing_memory.memory_ref(),
            evidence: outgoing_evidence.map(|item| item.text),
            index: Some(index_ref),
        };
        self.canonical(
            time,
            KnownEvent::CheckpointPublished {
                checkpoint: published.clone(),
            },
            objects,
        )?;
        let mut batch = self.batch().unwrap();
        batch.checkpoint = Some((effect_id, published, checkpoint.snapshot.files.clone()));
        Ok(batch)
    }

    pub fn confirmed(&mut self, publication: &Publication) {
        if let Some((_, capture)) = &publication.capture {
            self.last_capture = Some(capture.clone());
        }
        if let Some((_, checkpoint, files)) = &publication.checkpoint {
            self.last_checkpoint = Some(checkpoint.clone());
            self.previous_files = files.clone();
        }
    }
    pub fn terminal(&mut self, time: &UtcTime, result: &RunResult) -> Result<()> {
        if self.execution_v2 {
            return self.execution_event(
                time,
                playtime_protocol::execution::Fact::RunFinished {
                    reason: result.reason,
                    usage: result.usage.clone(),
                    cleanup: result.cleanup.clone(),
                    last_capture: self.last_capture.clone(),
                },
                Vec::new(),
            );
        }
        self.canonical(
            time,
            KnownEvent::RunFinished {
                reason: result.reason,
                usage: result.usage.clone(),
                cleanup: result.cleanup.clone(),
                last_checkpoint: self.last_checkpoint.clone(),
                recovery_origin: None,
            },
            Vec::new(),
        )
    }
    pub fn retire(&mut self) -> (usize, u64) {
        let diagnostic = (self.queue.len(), self.queued_bytes());
        self.queue.clear();
        diagnostic
    }
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod capture_tests;
