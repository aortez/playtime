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
        };
        let mut unsettled = BTreeMap::new();
        let mut timings = Vec::new();
        self.visit(|entry| {
            match &entry.fact {
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

/// Possible contention in live request timing: a model reload after the first
/// request, or a client-observed latency far above the server's own total.
pub fn contention(timings: &[serde_json::Value]) -> Option<playtime_protocol::api::Contention> {
    if timings.is_empty() {
        return None;
    }
    let reloads = timings
        .iter()
        .skip(1)
        .filter(|t| {
            t["load_duration"]
                .as_u64()
                .is_some_and(|ns| ns > 1_000_000_000)
        })
        .count() as u64;
    let delays = timings
        .iter()
        .filter(
            |t| match (t["observed_ms"].as_u64(), t["total_duration"].as_u64()) {
                (Some(observed), Some(total)) => {
                    let total = total / 1_000_000;
                    observed > total + 2_000 && observed > total + total / 2
                }
                _ => false,
            },
        )
        .count() as u64;
    Some(playtime_protocol::api::Contention {
        timed_requests: timings.len() as u64,
        reloads,
        delays,
        possible: reloads + delays > 0,
    })
}

#[cfg(test)]
mod contention_tests {
    use serde_json::json;
    #[test]
    fn reloads_after_the_first_request_and_long_waits_are_flagged() {
        assert!(super::contention(&[]).is_none());
        let clean = [
            json!({"observed_ms": 12_000, "load_duration": 9_000_000_000u64, "total_duration": 11_500_000_000u64}),
            json!({"observed_ms": 2_100, "load_duration": 50_000_000u64, "total_duration": 2_000_000_000u64}),
            json!({"observed_ms": 900}),
        ];
        let summary = super::contention(&clean).unwrap();
        assert_eq!((summary.timed_requests, summary.possible), (3, false));
        let busy = [
            clean[0].clone(),
            json!({"observed_ms": 9_000, "load_duration": 4_000_000_000u64, "total_duration": 8_800_000_000u64}),
            json!({"observed_ms": 7_000, "load_duration": 10_000_000u64, "total_duration": 3_000_000_000u64}),
            // A two-second gap alone is not enough on a long request.
            json!({"observed_ms": 22_500, "total_duration": 20_000_000_000u64}),
        ];
        let summary = super::contention(&busy).unwrap();
        assert_eq!(
            (summary.reloads, summary.delays, summary.possible),
            (1, 1, true)
        );
    }
}
