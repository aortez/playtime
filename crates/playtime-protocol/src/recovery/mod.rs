//! Portable recovery descriptions, never execution, takeover or write authority.
//! Hashes name exact bytes. The dependency graph is inventory -> plan -> patch
//! and target marker -> selection; nothing references the selection.
mod validate;
pub use validate::{RecoveryRecord, decode};

use crate::{
    archive::ContentRef,
    events::CheckpointRef,
    storage::{ArchiveIdentity, EventFrontier, StorageLimits},
    v1::UsageSummary,
    worker::{Binding, ControlId},
};
use playtime_core::{RequestId, SessionId, Sha256Digest, StopReason, UtcTime};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

pub const VERSION: u32 = 1;
pub const EXECUTION_CONTRACT: &str = "inputs/execution-contract.json";
pub const RULE_SET: &str = "controller_archive_v1";
pub const OBSERVATION_ACCOUNTING: &str = "jsonl_plus_each_retained_payload_v1";
pub const REQUIRED_EVENTS: &[&str] = &[
    "checkpoint_published",
    "command_finished",
    "controller_observation",
    "effect_intent",
    "effect_not_dispatched",
    "observation_omitted",
    "phase_changed",
    "request_finished",
    "request_not_dispatched",
    "run_finished",
    "run_started",
    "session_ended",
    "session_started",
    "stop_decided",
];
pub const EFFECT_KINDS: &[&str] = &[
    "cleanup",
    "model",
    "publish_checkpoint",
    "snapshot",
    "start_session",
    "tool",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContractV1 {
    pub schema_version: u32,
    pub rule_set: String,
    pub event_schema_version: u32,
    pub bridge_schema_version: u32,
    pub observation_schema_version: u32,
    pub required_event_kinds: Vec<String>,
    pub effect_kinds: Vec<String>,
    pub worker_executable: ContentRef,
    pub configuration: ContentRef,
    pub generated_tokens: NonZeroU64,
    pub log_bytes: NonZeroU64,
    #[serde(deserialize_with = "validate::full_limits")]
    pub storage_limits: StorageLimits,
    pub finalization_reserve_bytes: u64,
    pub observation_accounting: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBindingV1 {
    pub schema_version: u32,
    pub archive: ArchiveIdentity,
    pub worker: Binding,
    pub original_generation: NonZeroU64,
    pub superseding_generation: NonZeroU64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryProvenanceV1 {
    OwnedFixture,
    Systemd { manager_id: ControlId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceLimitsV1 {
    pub total_bytes: u64,
    pub entries: u64,
    pub plan_bytes: u64,
    pub inventory_bytes: u64,
    pub selection_bytes: u64,
    pub resolution_bytes: u64,
    pub inventory_entries: u64,
    pub resolution_reserve_bytes: u64,
    pub resolution_reserve_entries: u64,
}
impl Default for MaintenanceLimitsV1 {
    fn default() -> Self {
        Self {
            total_bytes: 128 * 1024 * 1024,
            entries: 256,
            plan_bytes: 4 * 1024 * 1024,
            inventory_bytes: 4 * 1024 * 1024,
            selection_bytes: 65_536,
            resolution_bytes: 65_536,
            inventory_entries: 10_000,
            resolution_reserve_bytes: 131_072,
            resolution_reserve_entries: 4,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryAttemptV1 {
    pub schema_version: u32,
    pub attempt_id: ControlId,
    pub chosen_time: UtcTime,
    pub binding: RecoveryBindingV1,
    pub provenance: RecoveryProvenanceV1,
    pub maintenance_limits: MaintenanceLimitsV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrontierProvenanceV1 {
    Worker,
    Recovery { attempt_id: ControlId },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedFrontierV1 {
    pub frontier: EventFrontier,
    pub provenance: FrontierProvenanceV1,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedBoundsV1 {
    pub schema_version: u32,
    pub worker: Option<EventFrontier>,
    pub public: Option<PublishedFrontierV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceEventV1 {
    pub archive_sequence: NonZeroU64,
    pub record_sha256: Sha256Digest,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedIntentV1 {
    pub source: SourceEventV1,
    pub effect_id: NonZeroU64,
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub maximum_tokens: NonZeroU64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstStopV1 {
    pub source: SourceEventV1,
    pub reason: StopReason,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastCheckpointV1 {
    pub source: SourceEventV1,
    pub checkpoint: CheckpointRef,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPlanV1 {
    pub schema_version: u32,
    pub attempt: RecoveryAttemptV1,
    pub archive: ArchiveIdentity,
    pub base: EventFrontier,
    pub prior_bounds: ConfirmedBoundsV1,
    pub execution_contract: ContentRef,
    pub initial_inventory: ContentRef,
    pub unresolved: Vec<UnresolvedIntentV1>,
    pub first_stop: Option<FirstStopV1>,
    pub last_checkpoint: Option<LastCheckpointV1>,
    pub existing_usage: UsageSummary,
    pub resulting_usage: UsageSummary,
    pub base_accounted_log_bytes: u64,
    pub available_log_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryOriginV1 {
    pub schema_version: u32,
    pub attempt_id: ControlId,
    pub plan: ContentRef,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationCapacityV1 {
    pub base_accounted_log_bytes: u64,
    pub patch_bytes: u64,
    pub final_accounted_log_bytes: u64,
    pub frozen_log_bytes: u64,
    pub remaining_log_bytes: u64,
    pub event_count: u64,
    pub reference_count: u64,
    pub referenced_content_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySelectionV1 {
    pub schema_version: u32,
    pub attempt_id: ControlId,
    pub archive: ArchiveIdentity,
    pub base: EventFrontier,
    pub plan: ContentRef,
    pub inventory: ContentRef,
    pub patch: ContentRef,
    pub target_marker: ContentRef,
    pub final_frontier: EventFrontier,
    pub capacity: FinalizationCapacityV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingAvailabilityV1 {
    Available { usage: UsageSummary },
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionV1 {
    TerminalPreserved {
        frontier: EventFrontier,
    },
    NeverStarted {
        evidence_sha256: Sha256Digest,
    },
    AbandonedIncomplete {
        evidence_sha256: Sha256Digest,
        operation_id: ControlId,
        review_id: ControlId,
        reason: String,
        accounting: AccountingAvailabilityV1,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryResolutionV1 {
    pub schema_version: u32,
    pub archive: ArchiveIdentity,
    pub attempt_id: ControlId,
    pub resolution: ResolutionV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryBlockedReasonV1 {
    OwnershipUnresolved,
    EvidenceConflict,
    UnsupportedContract,
    Incomplete,
    PreservationLimit,
    FinalizationLimit,
    IoUncertain,
}

pub fn plan_path(attempt: &ControlId) -> String {
    format!("artifacts/recovery/{}/plan.json", attempt.as_str())
}
pub fn private_path(attempt: &ControlId, name: &str) -> String {
    format!("recovery/{}/{name}", attempt.as_str())
}

/// Mutation planning understands only the compiled canonical/control fields.
/// Observation wrappers/payloads retain their ordinary inspection compatibility.
pub fn strict_control_fields(
    event: &crate::events::DecodedEvent,
    bytes: &[u8],
) -> Result<(), crate::ReadError> {
    let known = match event.payload() {
        crate::events::EventPayload::Known(value) => serde_json::to_value(value),
        crate::events::EventPayload::Control(value) => serde_json::to_value(value),
        _ => return Ok(()),
    }
    .map_err(crate::ReadError::malformed)?;
    let raw = validate::unique_json(bytes)?;
    let mut envelope =
        serde_json::to_value(event.envelope()).map_err(crate::ReadError::malformed)?;
    envelope["data"] = known["data"].clone();
    validate::reject_unknown(&raw, &envelope)
}
