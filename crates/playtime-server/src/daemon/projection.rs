use crate::{
    application::{ApiError, Result},
    lifecycle::{Admission, Phase, StopDelivery},
};
use playtime_core::{RunId, SessionId};
use playtime_protocol::{
    api::*,
    archive::ContentRef,
    archive_queries::ArchiveFact,
    bridge::FileContent,
    events::{CleanupOutcome, KnownEvent, MemoryRef},
    observations::ObservationData,
    storage::EventFrontier,
};
use playtime_runner::ArchiveQueries;
use std::collections::BTreeMap;

pub(super) fn backend() -> BackendIdentity {
    BackendIdentity { kind: BackendKind::Controller, description: "Durable controller simulation with a frozen detached worker; scripted model/workshop effects".into() }
}
pub(super) fn docker_backend() -> BackendIdentity {
    BackendIdentity {
        kind: BackendKind::Controller,
        description:
            "Frozen detached worker with real Docker tools and explicitly scripted model responses"
                .into(),
    }
}
pub(super) fn live_backend() -> BackendIdentity {
    BackendIdentity {
        kind: BackendKind::Controller,
        description: "Frozen detached worker with real Docker tools and the live locked model"
            .into(),
    }
}
pub(super) fn docker_and_live_backend() -> BackendIdentity {
    BackendIdentity {
        kind: BackendKind::Controller,
        description:
            "Frozen detached worker with real Docker tools and scripted or live locked model responses"
                .into(),
    }
}
pub(super) fn frontier(f: &EventFrontier) -> Frontier {
    Frontier {
        run_id: f.archive.run_id.clone(),
        manifest_sha256: f.archive.input_manifest.sha256.clone(),
        prefix: Prefix {
            sequence: f.committed_sequence,
            byte_offset: f.committed_bytes,
            sha256: f.prefix_sha256.clone(),
        },
    }
}
pub(super) fn evidence(_: impl std::fmt::Display) -> ApiError {
    ApiError::new("archive_corrupt")
}
pub(super) fn artifact_id(run: &RunId, content: &ContentRef) -> ArtifactId {
    ArtifactId::new(
        playtime_storage::sha256(&serde_json::to_vec(&(run, content)).unwrap()).as_str(),
    )
    .unwrap()
}
pub(super) fn available(run: &RunId, reference: &ContentRef) -> ContentAvailability {
    ContentAvailability::Available {
        artifact_id: artifact_id(run, reference),
        content: reference.clone(),
    }
}
fn unavailable(reason: &str) -> ContentAvailability {
    ContentAvailability::Unavailable {
        reason: reason.into(),
    }
}
fn memory(run: &RunId, reference: &MemoryRef) -> ContentAvailability {
    match reference {
        MemoryRef::Available { content } => available(run, content),
        MemoryRef::Missing => unavailable("missing"),
        MemoryRef::Unavailable { reason } => unavailable(reason),
    }
}

pub(super) struct Evidence {
    pub snapshot: playtime_storage::Snapshot,
    pub view: RunView,
    pub checkpoints: Vec<Checkpoint>,
    pub artifacts: Vec<Artifact>,
}
fn base(record: &Admission) -> RunView {
    RunView {
        target: None,
        request_accounting: None,
        harness: None,
        run_id: record.run_id.clone(),
        mode: record.request.mode,
        accepted_at: record.accepted.utc.clone(),
        updated_at: record.accepted.utc.clone(),
        recipe_id: RecipeId::new(&record.request.recipe_id).unwrap(),
        preview_revision: PreviewRevision::new(&record.request.preview_revision).unwrap(),
        backend: match record.request.mode {
            playtime_core::RunMode::ScriptedSmoke => docker_backend(),
            // Experiments are catalog recipes, which run on the live model.
            playtime_core::RunMode::LiveSmoke | playtime_core::RunMode::Experiment => {
                live_backend()
            }
            playtime_core::RunMode::Simulation => backend(),
        },
        orchestration: Orchestration {
            state: match record.phase {
                Phase::Preparing => OrchestrationState::Preparing,
                Phase::Starting => OrchestrationState::Starting,
                Phase::Running => OrchestrationState::Running,
                Phase::Cancelling => OrchestrationState::Cancelling,
                Phase::Reconciling => OrchestrationState::Reconciling,
                Phase::Terminal => OrchestrationState::Terminal,
            },
            reason: record.reason.map(|r| {
                serde_json::to_value(r)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            }),
        },
        controller_phase: None,
        outcome: None,
        usage: None,
        accounting_coverage: None,
        checkpoint: None,
        cancellation: record
            .cancel_requested
            .as_ref()
            .map(|requested| Cancellation {
                requested_at: requested.utc.clone(),
                enforcement: match record.stop_delivery {
                    Some(StopDelivery::Accepted) => Enforcement::StopAccepted,
                    Some(StopDelivery::Stopped) => Enforcement::Stopped,
                    Some(StopDelivery::Uncertain) => Enforcement::Uncertain,
                    _ => Enforcement::Requested,
                },
                stop_accepted_at: record.stop_accepted.as_ref().map(|s| s.utc.clone()),
                grace_deadline: None,
                uncertainty: (record.stop_delivery == Some(StopDelivery::Uncertain))
                    .then(|| "worker termination unresolved".into()),
            }),
        cleanup: Cleanup::Unknown,
        evidence: EvidenceAvailability::Unavailable {
            reason: "awaiting authenticated worker confirmation".into(),
        },
        input_manifest: None,
        worker: None,
        last_confirmed_frontier: None,
        event_anchors: None,
        contention: None,
    }
}
pub(super) fn unprepared(record: &Admission) -> RunView {
    base(record)
}
pub(super) fn project(record: &Admission, queries: &ArchiveQueries) -> Result<Evidence> {
    let mut view = base(record);
    let configuration = queries.inputs().configuration.clone();
    let value: serde_json::Value =
        serde_json::from_slice(&queries.content(&configuration).map_err(evidence)?)
            .map_err(evidence)?;
    view.target = value
        .pointer("/model/host/target")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(evidence)?;
    if view.target.as_ref().is_some_and(|t| !t.valid())
        || view.target.as_ref().map(|t| &t.id) != record.request.target.as_ref()
    {
        return Err(ApiError::new("content_mismatch"));
    }
    view.harness = HarnessView::from_configuration(&value)
        .map_err(evidence)?
        .map(|harness| ArchivedHarness {
            configuration,
            harness,
        });
    let run = queries.run().map_err(evidence)?;
    // Only execution records carry final captures; skip their archive pass otherwise.
    let execution = run.execution.is_some();
    let f = frontier(&run.frontier);
    view.controller_phase = run.phase;
    view.usage = Some(run.usage);
    view.contention = run.contention;
    let allowances: Vec<_> = if let Some(execution) = &run.execution {
        view.request_accounting = Some(queries.request_accounting().map_err(evidence)?);
        execution
            .unsettled_model_intents
            .iter()
            .map(|i| i.maximum_tokens)
            .collect()
    } else {
        run.unsettled_model_intents
            .iter()
            .map(|i| i.maximum_tokens)
            .collect()
    };
    let allowance = allowances
        .iter()
        .try_fold(0u64, |total, n| total.checked_add(n.map_or(0, |n| n.get())))
        .ok_or_else(|| evidence("allowance overflow"))?;
    view.accounting_coverage = Some(AccountingCoverage {
        schema_version: 1,
        availability: EvidenceAvailability::Available,
        unsettled_requests: allowances.len() as u64,
        unsettled_allowance: allowance,
        frontier: f.clone(),
    });
    if let Some(outcome) = run.outcome {
        view.outcome = Some(ExperimentalOutcome {
            reason: outcome.reason,
            event_sequence: queries
                .snapshot()
                .inspection()
                .terminal
                .as_ref()
                .ok_or_else(|| evidence("missing terminal"))?
                .envelope
                .sequence
                .get(),
        });
        view.cleanup = match outcome.cleanup {
            CleanupOutcome::Complete => Cleanup::Complete,
            CleanupOutcome::Failed { errors } => Cleanup::Failed { errors },
            CleanupOutcome::Unknown => Cleanup::Unknown,
        };
    }
    view.evidence = EvidenceAvailability::Available;
    view.input_manifest = Some(run.frontier.archive.input_manifest.clone());
    view.worker = Some(queries.inputs().worker.clone());
    view.last_confirmed_frontier = Some(f.clone());
    view.event_anchors = Some(EventAnchors {
        frontier: f.clone(),
        history_cursor: EventCursor::new(f.clone(), 0)
            .map_err(evidence)?
            .encode()
            .map_err(evidence)?,
        live_cursor: EventCursor::new(f.clone(), f.prefix.sequence)
            .map_err(evidence)?
            .encode()
            .map_err(evidence)?,
    });
    let mut metadata: BTreeMap<String, (String, Option<SessionId>, u64)> = BTreeMap::new();
    let mut sequences = BTreeMap::new();
    let mut capture_sequences = BTreeMap::new();
    let mut after = 0;
    loop {
        let page = queries.history(after, 200, false).map_err(evidence)?;
        for entry in page.entries {
            let sequence = entry.envelope.sequence.get();
            view.updated_at = entry.envelope.time.clone();
            match entry.fact {
                ArchiveFact::Canonical {
                    event: KnownEvent::CheckpointPublished { checkpoint, .. },
                } => {
                    sequences.insert(checkpoint.session_id, sequence);
                }
                ArchiveFact::Canonical {
                    event:
                        KnownEvent::SessionStarted {
                            session_id,
                            delivery: Some(delivery),
                            ..
                        },
                } => {
                    metadata
                        .entry(delivery.memento.content.path.to_string())
                        .or_insert(("incoming_memory".into(), Some(session_id.clone()), sequence));
                    if let Some(evidence) = delivery.evidence {
                        metadata.entry(evidence.text.path.to_string()).or_insert((
                            "incoming_evidence".into(),
                            Some(session_id.clone()),
                            sequence,
                        ));
                        metadata
                            .entry(evidence.provenance.path.to_string())
                            .or_insert(("evidence_provenance".into(), Some(session_id), sequence));
                    }
                }
                ArchiveFact::Execution { event, .. } => {
                    use playtime_protocol::execution::Fact;
                    match event.fact {
                        Fact::CapturePublished { capture } => {
                            capture_sequences.insert(capture.index.path, sequence);
                        }
                        Fact::Observation(o) => {
                            if let ObservationData::Artifact { content } = o.data {
                                metadata.insert(
                                    content.path.to_string(),
                                    ("controller_observation".into(), None, sequence),
                                );
                            }
                        }
                        _ => (),
                    }
                }
                ArchiveFact::Observation { observation, .. } => {
                    if let ObservationData::Artifact { content } = observation.data {
                        metadata.insert(
                            content.path.to_string(),
                            (
                                "controller_observation".into(),
                                observation.session_id,
                                sequence,
                            ),
                        );
                    }
                }
                _ => (),
            }
        }
        if !page.has_more {
            break;
        }
        after = page.next_after_sequence;
    }
    let mut checkpoints = Vec::new();
    let mut names = BTreeMap::new();
    for item in queries.checkpoints().map_err(evidence)? {
        let session = &item.checkpoint.session_id;
        let sequence = *sequences
            .get(session)
            .ok_or_else(|| evidence("checkpoint sequence unavailable"))?;
        let mut remember = |content: &ContentRef, kind: &str| {
            let entry = metadata.entry(content.path.to_string()).or_insert((
                kind.into(),
                Some(session.clone()),
                sequence,
            ));
            entry.0 = kind.into();
            entry.2 = entry.2.min(sequence);
        };
        remember(&item.checkpoint.workspace, "workspace");
        let index_ref = item.checkpoint.index.as_ref();
        if let Some(content) = index_ref {
            remember(content, "checkpoint_index");
        }
        let (incoming_memory, outgoing_memory, outgoing_evidence, files) =
            if let Some(index) = item.index {
                for content in index.content_leaves() {
                    remember(content, "checkpoint_content");
                }
                remember(&index.incoming_memory.content, "incoming_memory");
                remember(&index.outgoing_memory.content, "outgoing_memory");
                if let Some(ref content) = index.outgoing_evidence {
                    remember(&content.text, "controller_evidence");
                }
                let files = index
                    .files
                    .iter()
                    .map(|file| {
                        let content = match &file.content {
                            FileContent::Retained { content } => {
                                names.insert(content.path.clone(), file.path.clone());
                                remember(content, "file_version");
                                available(&record.run_id, content)
                            }
                            FileContent::Unavailable { reason } => unavailable(reason),
                        };
                        FileVersion {
                            name: file.path.clone(),
                            kind: file.entry_type.clone(),
                            content,
                        }
                    })
                    .collect();
                (
                    memory(&record.run_id, &index.incoming_memory.memory_ref()),
                    memory(&record.run_id, &index.outgoing_memory.memory_ref()),
                    index.outgoing_evidence.as_ref().map_or_else(
                        || unavailable("not_recorded"),
                        |content| available(&record.run_id, &content.text),
                    ),
                    files,
                )
            } else {
                (
                    unavailable("not_recorded"),
                    memory(&record.run_id, &item.checkpoint.memento),
                    item.checkpoint.evidence.as_ref().map_or_else(
                        || unavailable("not_recorded"),
                        |r| available(&record.run_id, r),
                    ),
                    vec![],
                )
            };
        checkpoints.push(Checkpoint {
            checkpoint: CheckpointSummary {
                session_id: session.clone(),
                published_sequence: sequence,
            },
            incoming_memory,
            outgoing_memory,
            evidence: outgoing_evidence,
            workspace: available(&record.run_id, &item.checkpoint.workspace),
            files,
            index: Some(index_ref.map_or_else(
                || unavailable("not_recorded"),
                |r| available(&record.run_id, r),
            )),
        });
    }
    let captures = if execution {
        queries.captures().map_err(evidence)?
    } else {
        Vec::new()
    };
    for item in captures {
        let sequence = *capture_sequences
            .get(&item.capture.index.path)
            .ok_or_else(|| evidence("capture sequence unavailable"))?;
        let mut remember = |content: &ContentRef, kind: &str| {
            metadata.insert(content.path.to_string(), (kind.into(), None, sequence));
        };
        remember(&item.capture.workspace, "final_workspace");
        remember(&item.capture.index, "final_capture_index");
        remember(&item.index.source_map, "capture_source_map");
        for file in &item.index.files {
            if let FileContent::Retained { content } = &file.content {
                names.insert(content.path.clone(), file.path.clone());
                remember(content, "file_version");
            }
        }
    }
    view.checkpoint = checkpoints.last().map(|c| c.checkpoint.clone());
    let artifacts = queries
        .snapshot()
        .inspection()
        .referenced_content
        .iter()
        .map(|content| {
            let frozen_input = queries.inputs().references().any(|input| input == content)
                || content == &run.frontier.archive.input_manifest;
            let (kind, session_id, published_sequence) = metadata
                .get(content.path.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    (
                        if frozen_input {
                            "frozen_input"
                        } else {
                            "retained_content"
                        }
                        .into(),
                        None,
                        0,
                    )
                });
            Artifact {
                artifact_id: artifact_id(&record.run_id, content),
                name: names.get(&content.path).cloned().unwrap_or_else(|| {
                    content
                        .path
                        .as_str()
                        .rsplit('/')
                        .next()
                        .unwrap_or("content")
                        .into()
                }),
                kind,
                content: content.clone(),
                session_id,
                published_sequence,
                frozen_input,
            }
        })
        .collect();
    Ok(Evidence {
        snapshot: queries.snapshot().clone(),
        view,
        checkpoints,
        artifacts,
    })
}
