use std::{collections::BTreeMap, num::NonZeroU64, path::Path};

use playtime_core::SessionId;
use playtime_protocol::{
    archive::{ContentRef, InputManifest},
    archive_queries::*,
    bridge::{BridgeEvent, FileVersion, decode_checkpoint_index},
    events::{EventPayload, KnownEvent, decode_event},
    observations::{ObservationData, ObservationLocation},
};
use playtime_storage::{ArchiveReader, CommittedFrontier, Snapshot, StorageLimits};

use crate::{Error, Result};
mod execution;

/// A fixed verified frontier. Online construction requires a writer receipt;
/// offline construction acquires the storage inspection lock. No database or
/// original checkout participates in any query.
pub struct ArchiveQueries {
    reader: ArchiveReader,
    snapshot: Snapshot,
}

impl ArchiveQueries {
    /// Reuse a storage-verified read authority without reconstructing a writer
    /// receipt. A snapshot cannot be constructed from public DTOs.
    pub fn from_snapshot(reader: ArchiveReader, snapshot: Snapshot) -> Result<Self> {
        if reader.identity() != &snapshot.frontier().archive {
            return Err(Error::Invalid("query snapshot archive mismatch".into()));
        }
        Ok(Self { reader, snapshot })
    }
    pub fn offline(path: impl AsRef<Path>, limits: StorageLimits) -> Result<Self> {
        let reader = ArchiveReader::open(path, limits)?;
        let snapshot = reader.offline_snapshot()?;
        Ok(Self { reader, snapshot })
    }

    pub fn live(
        path: impl AsRef<Path>,
        limits: StorageLimits,
        receipt: &CommittedFrontier,
    ) -> Result<Self> {
        let reader = ArchiveReader::open(path, limits)?;
        let snapshot = reader.live_snapshot(receipt)?;
        Ok(Self { reader, snapshot })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn inputs(&self) -> &InputManifest {
        self.reader.manifest()
    }
    pub fn content(&self, reference: &ContentRef) -> Result<Vec<u8>> {
        Ok(self.reader.read_content(&self.snapshot, reference)?)
    }

    pub fn history(
        &self,
        after: u64,
        limit: u32,
        resolve_payloads: bool,
    ) -> Result<ArchiveHistoryPage> {
        let page = self
            .reader
            .page(&self.snapshot, &self.snapshot.query(after, limit))?;
        let mut entries = Vec::new();
        for stored in page.events {
            let decoded = decode_event(
                std::str::from_utf8(&stored.bytes)
                    .map_err(|error| Error::Invalid(error.to_string()))?,
            )?;
            let fact = match decoded.payload() {
                EventPayload::Execution(record) => {
                    let resolved =
                        if let playtime_protocol::execution::Fact::Observation(observation) =
                            &record.fact
                        {
                            match &observation.data {
                                ObservationData::Inline { value } => Some(value.clone()),
                                ObservationData::Artifact { content } if resolve_payloads => {
                                    Some(observation.resolve(&self.content(content)?)?)
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };
                    ArchiveFact::Execution {
                        event: *record.clone(),
                        resolved,
                    }
                }
                EventPayload::Known(event) => ArchiveFact::Canonical {
                    event: *event.clone(),
                },
                EventPayload::Control(event) => ArchiveFact::Control {
                    event: *event.clone(),
                },
                EventPayload::Observation(observation) => {
                    let resolved = if resolve_payloads {
                        match &observation.observation().data {
                            ObservationData::Artifact { content } => {
                                Some(observation.resolve(&self.content(content)?)?)
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    ArchiveFact::Observation {
                        observation: observation.observation().clone(),
                        record: resolved.as_ref().unwrap_or(observation).record().cloned(),
                    }
                }
                EventPayload::Unknown { .. } => ArchiveFact::Unknown,
            };
            entries.push(ArchiveHistoryEntry {
                envelope: stored.envelope,
                jsonl: String::from_utf8(stored.bytes).expect("decoded UTF-8 event"),
                record_sha256: stored.sha256,
                fact,
            });
        }
        Ok(ArchiveHistoryPage {
            entries,
            next_after_sequence: page.next_after_sequence,
            frontier: page.frontier,
            has_more: page.has_more,
        })
    }

    pub fn run(&self) -> Result<ArchiveRun> {
        let inspection = self.snapshot.inspection();
        let mut run = ArchiveRun {
            run_id: self.reader.descriptor().run_id.clone(),
            mode: self.reader.descriptor().mode,
            phase: None,
            first_stop: None,
            usage: inspection.usage.clone(),
            unsettled_model_intents: Vec::new(),
            outcome: None,
            frontier: self.snapshot.frontier().clone(),
            incomplete_evidence: inspection.incomplete_evidence,
            uncommitted_event_bytes: inspection.uncommitted_event_bytes,
            contention: None,
            execution: (self.reader.manifest().schema_version == 2).then(ExecutionRun::default),
        };
        let mut unsettled = BTreeMap::new();
        let mut execution_intents = BTreeMap::new();
        let mut timings = Vec::new();
        self.visit(|entry| {
            match &entry.fact {
                ArchiveFact::Execution { event, .. } => {
                    use playtime_protocol::execution::Fact;
                    match &event.fact {
                        Fact::EffectIntent(intent) if intent.effect_kind == "model" => {
                            execution_intents.insert(
                                intent.request.as_ref().unwrap().request_id.clone(),
                                intent.clone(),
                            );
                        }
                        Fact::RequestSettled { request, .. }
                        | Fact::RequestNotDispatched { request, .. } => {
                            execution_intents.remove(&request.request_id);
                        }
                        Fact::CapturePublished { capture } => {
                            run.execution.as_mut().unwrap().last_capture = Some(capture.clone())
                        }
                        Fact::RunFinished {
                            reason, cleanup, ..
                        } => {
                            run.outcome = Some(ArchiveOutcome {
                                reason: *reason,
                                cleanup: cleanup.clone(),
                                last_checkpoint: None,
                            })
                        }
                        _ => {}
                    }
                }

                // The live worker's own timing chunk follows each request's records.
                ArchiveFact::Observation {
                    record: Some(record),
                    ..
                } if record.kind == "model_chunk"
                    && record.data["chunk"]["observation"] == "timing" =>
                {
                    timings.push(record.data["chunk"].clone());
                }
                ArchiveFact::Control {
                    event: BridgeEvent::EffectIntent(intent),
                } if intent.effect_kind == "model" => {
                    unsettled.insert(intent.request_id.clone().unwrap(), intent.clone());
                }
                ArchiveFact::Control {
                    event: BridgeEvent::RequestNotDispatched(fact),
                } => {
                    unsettled.remove(&fact.request_id);
                }
                ArchiveFact::Canonical {
                    event: KnownEvent::RequestFinished { request_id, .. },
                } => {
                    unsettled.remove(request_id);
                }
                ArchiveFact::Canonical {
                    event: KnownEvent::PhaseChanged { phase, .. },
                } => run.phase = Some(*phase),
                ArchiveFact::Control {
                    event: BridgeEvent::StopDecided(stop),
                } if run.first_stop.is_none() => run.first_stop = Some(stop.reason),
                ArchiveFact::Canonical {
                    event:
                        KnownEvent::RunFinished {
                            reason,
                            cleanup,
                            last_checkpoint,
                            ..
                        },
                } => {
                    run.outcome = Some(ArchiveOutcome {
                        reason: *reason,
                        cleanup: cleanup.clone(),
                        last_checkpoint: last_checkpoint.clone(),
                    })
                }
                _ => {}
            }
            Ok(())
        })?;
        run.unsettled_model_intents = unsettled.into_values().collect();
        if let Some(execution) = &mut run.execution {
            execution.unsettled_model_intents = execution_intents.into_values().collect();
        }
        run.contention = contention(&timings);
        Ok(run)
    }

    pub fn checkpoints(&self) -> Result<Vec<ArchiveCheckpoint>> {
        self.snapshot
            .inspection()
            .checkpoints
            .iter()
            .map(|checkpoint| {
                let index = checkpoint
                    .index
                    .as_ref()
                    .map(|reference| -> Result<_> {
                        let bytes = self.content(reference)?;
                        Ok(decode_checkpoint_index(
                            std::str::from_utf8(&bytes)
                                .map_err(|error| Error::Invalid(error.to_string()))?,
                        )?)
                    })
                    .transpose()?;
                Ok(ArchiveCheckpoint {
                    checkpoint: checkpoint.clone(),
                    index,
                })
            })
            .collect()
    }

    pub fn files(&self, session: &SessionId) -> Result<Option<Vec<FileVersion>>> {
        Ok(self
            .checkpoints()?
            .into_iter()
            .find(|item| &item.checkpoint.session_id == session)
            .and_then(|item| item.index.map(|index| index.files)))
    }

    pub fn observation(&self, sequence: NonZeroU64) -> Result<ObservationResolution> {
        let mut resolution = ObservationResolution::NotRecorded;
        self.visit(|entry| {
            match &entry.fact {
                ArchiveFact::Observation { observation, .. }
                    if observation.controller_sequence == sequence =>
                {
                    let data = serde_json::to_string(&entry.envelope.data)?;
                    let decoded = playtime_protocol::observations::decode_observation(&data)?;
                    let resolved = match &observation.data {
                        ObservationData::Artifact { content } => {
                            decoded.resolve(&self.content(content)?)?
                        }
                        _ => decoded,
                    };
                    resolution = ObservationResolution::Recorded {
                        location: ObservationLocation {
                            controller_sequence: sequence,
                            archive_sequence: entry.envelope.sequence,
                            record_sha256: entry.record_sha256.clone(),
                        },
                        record: resolved.record().unwrap().clone(),
                    };
                }
                ArchiveFact::Control {
                    event: BridgeEvent::ObservationOmitted(omission),
                } if omission.controller_sequence == Some(sequence) => {
                    resolution = ObservationResolution::Omitted {
                        omission: omission.clone(),
                    }
                }
                _ => {}
            }
            Ok(())
        })?;
        Ok(resolution)
    }

    fn visit(&self, mut visitor: impl FnMut(&ArchiveHistoryEntry) -> Result<()>) -> Result<()> {
        let mut after = 0;
        loop {
            let page = self.history(after, 200, false)?;
            for entry in &page.entries {
                visitor(entry)?;
            }
            if !page.has_more {
                return Ok(());
            }
            if page.next_after_sequence <= after {
                return Err(Error::Invalid("archive page made no progress".into()));
            }
            after = page.next_after_sequence;
        }
    }
}

/// Possible contention in live request timing, from the server's own
/// durations: a model reload after the first request, or a request whose total
/// exceeds its load, prompt and generation time, so it waited behind other work.
/// A queued request's wait shows up inside its total, not in its client-observed
/// latency.
pub fn contention(timings: &[serde_json::Value]) -> Option<playtime_protocol::api::Contention> {
    const LIMIT_NS: u64 = 2_000_000_000;
    if timings.is_empty() {
        return None;
    }
    let reloads = timings
        .iter()
        .skip(1)
        .filter(|t| t["load_duration"].as_u64().is_some_and(|ns| ns > LIMIT_NS))
        .count() as u64;
    let queue_waits = timings
        .iter()
        .filter(|t| {
            t["total_duration"].as_u64().is_some_and(|total| {
                let busy = ["load_duration", "prompt_eval_duration", "eval_duration"]
                    .into_iter()
                    .filter_map(|key| t[key].as_u64())
                    .fold(0, u64::saturating_add);
                total.saturating_sub(busy) > LIMIT_NS
            })
        })
        .count() as u64;
    Some(playtime_protocol::api::Contention {
        timed_requests: timings.len() as u64,
        reloads,
        queue_waits,
        possible: reloads + queue_waits > 0,
    })
}

#[cfg(test)]
mod contention_tests {
    use serde_json::json;
    fn timing(
        observed_ms: u64,
        load: u64,
        prompt: u64,
        eval: u64,
        total: u64,
    ) -> serde_json::Value {
        json!({"observed_ms": observed_ms, "load_duration": load,
            "prompt_eval_duration": prompt, "eval_duration": eval, "total_duration": total})
    }

    #[test]
    fn reloads_after_the_first_request_and_queue_waits_are_flagged() {
        assert!(super::contention(&[]).is_none());
        let clean = [
            // A cold first load is not contention.
            timing(7_700, 7_000_000_000, 40_000_000, 500_000_000, 7_600_000_000),
            timing(1_900, 300_000_000, 40_000_000, 1_500_000_000, 1_860_000_000),
            // A failed request without a terminal record has no durations.
            json!({"observed_ms": 900}),
            // Client-observed latency far above the server's total is not
            // contention on its own.
            timing(
                30_000,
                300_000_000,
                40_000_000,
                1_000_000_000,
                1_400_000_000,
            ),
        ];
        let summary = super::contention(&clean).unwrap();
        assert_eq!((summary.timed_requests, summary.possible), (4, false));
        let busy = [
            clean[0].clone(),
            // Another model or context size was loaded in between.
            timing(4_700, 4_000_000_000, 40_000_000, 500_000_000, 4_600_000_000),
            // Measured on 2026-10-18: a request queued behind another one.
            timing(
                20_600,
                310_000_000,
                40_000_000,
                1_000_000_000,
                20_500_000_000,
            ),
        ];
        let summary = super::contention(&busy).unwrap();
        assert_eq!(
            (summary.reloads, summary.queue_waits, summary.possible),
            (1, 1, true)
        );
    }
}
