//! Normalized Rust-archive views. These are separate from transport IDs and
//! never grant filesystem access or execution authority.

use playtime_core::{RunId, RunMode, RunPhase, Sha256Digest, StopReason};
use serde::{Deserialize, Serialize};

use crate::{
    bridge::{BridgeEvent, CheckpointIndex, EffectIntent, ObservationOmitted},
    events::{CheckpointRef, CleanupOutcome, EventEnvelope, KnownEvent},
    observations::{ControllerObservation, ControllerRecord, ObservationLocation},
    storage::EventFrontier,
    v1::UsageSummary,
};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveOutcome {
    pub reason: StopReason,
    pub cleanup: CleanupOutcome,
    pub last_checkpoint: Option<CheckpointRef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveRun {
    pub run_id: RunId,
    pub mode: RunMode,
    pub phase: Option<RunPhase>,
    pub first_stop: Option<StopReason>,
    /// Committed canonical settlements only. Unsettled intents remain visible
    /// in history; recovery must not silently treat them as measured zero.
    pub usage: UsageSummary,
    /// Authorized model work without a committed settlement or non-dispatch
    /// proof. Its actual dispatch/usage is unresolved, never measured zero.
    pub unsettled_model_intents: Vec<EffectIntent>,
    pub outcome: Option<ArchiveOutcome>,
    pub frontier: EventFrontier,
    pub incomplete_evidence: bool,
    pub uncommitted_event_bytes: u64,
    /// Missing means the run recorded no live request timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contention: Option<crate::api::Contention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionRun>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "classification", rename_all = "snake_case")]
pub enum ArchiveFact {
    Execution {
        event: crate::execution::Record,
        resolved: Option<serde_json::Value>,
    },
    Canonical {
        event: KnownEvent,
    },
    Control {
        event: BridgeEvent,
    },
    Observation {
        observation: ControllerObservation,
        /// None when content resolution was not requested. The wrapper still
        /// explicitly identifies inline/reference storage and its exact bytes.
        record: Option<ControllerRecord>,
    },
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveHistoryEntry {
    /// Complete raw envelope, including unknown payload fields.
    pub envelope: EventEnvelope,
    /// Exact source bytes including LF, preserving unknown envelope fields too.
    pub jsonl: String,
    pub record_sha256: Sha256Digest,
    pub fact: ArchiveFact,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveHistoryPage {
    pub entries: Vec<ArchiveHistoryEntry>,
    pub next_after_sequence: u64,
    pub frontier: EventFrontier,
    pub has_more: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveCheckpoint {
    pub checkpoint: CheckpointRef,
    /// None explicitly means that the older archive did not record an index.
    pub index: Option<CheckpointIndex>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum ObservationResolution {
    Recorded {
        location: ObservationLocation,
        record: ControllerRecord,
    },
    Omitted {
        omission: ObservationOmitted,
    },
    NotRecorded,
}

/// Session-neutral facts stay distinct from legacy checkpoint/session meanings.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExecutionRun {
    pub unsettled_model_intents: Vec<crate::execution::EffectIntent>,
    pub last_capture: Option<crate::execution::CaptureRef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArchiveCapture {
    pub capture: crate::execution::CaptureRef,
    pub index: crate::execution::CaptureIndex,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, utoipa::ToSchema)]
pub struct PurposeAccounting {
    pub admitted_calls: u64,
    pub settled_calls: u64,
    pub not_dispatched_calls: u64,
    pub usage: UsageSummary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, utoipa::ToSchema)]
pub struct RequestAccounting {
    /// None when the archive predates admission evidence. Settlements alone
    /// cannot establish the number of attempted calls.
    pub model_calls: Option<u64>,
    #[schema(value_type = std::collections::BTreeMap<String, PurposeAccounting>)]
    pub by_purpose: std::collections::BTreeMap<playtime_core::RequestPurpose, PurposeAccounting>,
    /// Missing purpose evidence remains explicit, including old omitted raw
    /// model requests; it is never guessed from turns, tool calls or phases.
    pub unclassified: PurposeAccounting,
}
