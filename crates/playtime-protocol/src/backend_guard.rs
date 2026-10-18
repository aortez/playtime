//! Backend-host ownership v1. These records are neither archive receipts nor
//! termination authority. Capabilities travel only over the private boundary.
use crate::worker::ControlId;
use playtime_core::{InvalidValue, RunId, Sha256Digest};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const CONTROL_BYTES: usize = 64 * 1024;
pub const BODY_BYTES: usize = 4 * 1024 * 1024;
pub const FRAME_BYTES: usize = 512 * 1024;
pub const RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const QUEUE_BYTES: usize = 1024 * 1024;
pub const STATE_BYTES: u64 = 64 * 1024 * 1024;
pub const STATE_RECORDS: u64 = 10_000;
pub const SUPERVISOR_STOP_MS: u64 = 90_000;
pub const STOP_OBSERVATION_MS: u64 = 120_000;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Capability(String);
impl Capability {
    pub fn new(value: String) -> Result<Self, InvalidValue> {
        Sha256Digest::new(value.clone())?;
        Ok(Self(value))
    }
    /// Private transport only. Do not include this in command lines or logs.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capability([redacted])")
    }
}
impl TryFrom<String> for Capability {
    type Error = InvalidValue;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<Capability> for String {
    fn from(value: Capability) -> String {
        value.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendIdentity {
    pub backend_id: ControlId,
    pub guard_id: ControlId,
    pub boot_id: ControlId,
    pub unit: String,
    pub invocation_id: ControlId,
    pub cgroup: String,
    pub generation: u64,
    pub profile_sha256: Sha256Digest,
}
impl BackendIdentity {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        let component = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-@".contains(&c))
                && s != "."
                && s != ".."
        };
        if self.generation == 0
            || !component(&self.unit)
            || !self.unit.ends_with(".service")
            || self.cgroup.len() > 1024
            || !self.cgroup.starts_with('/')
            || self.cgroup == "/"
            || !self.cgroup[1..].split('/').all(component)
        {
            return Err(InvalidValue("invalid backend identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseSpec {
    pub operation_id: ControlId,
    pub lease_id: ControlId,
    pub backend: BackendIdentity,
    pub service_id: ControlId,
    pub run_id: RunId,
    pub manifest_sha256: Sha256Digest,
    pub worker_id: ControlId,
    pub grant_id: ControlId,
    pub owner_revision: u64,
    pub execution_deadline_ms: u64,
    pub cleanup_deadline_ms: u64,
}
impl LeaseSpec {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        self.backend.validate()?;
        if self.owner_revision == 0
            || self.execution_deadline_ms == 0
            || self.execution_deadline_ms >= self.cleanup_deadline_ms
            || self
                .cleanup_deadline_ms
                .checked_add(STOP_OBSERVATION_MS)
                .is_none()
        {
            return Err(InvalidValue("invalid lease deadlines or revision"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestSpec {
    pub lease_id: ControlId,
    pub generation: u64,
    pub ordinal: u64,
    pub request_id: ControlId,
    pub effect_id: ControlId,
    pub body_sha256: Sha256Digest,
    pub body_bytes: u64,
    pub original_max_tokens: u64,
    pub deadline_ms: u64,
}
impl RequestSpec {
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.generation == 0
            || self.ordinal == 0
            || self.body_bytes == 0
            || self.body_bytes > BODY_BYTES as u64
            || self.original_max_tokens == 0
            || self.deadline_ms == 0
        {
            return Err(InvalidValue("invalid dispatch binding or bounds"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Revoked,
    Deadline,
    OwnerLost,
    BackendUncertain,
    StorageUncertain,
    Restart,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopOperation {
    pub operation_id: ControlId,
    pub backend: BackendIdentity,
    pub lease_id: ControlId,
    pub reason: StopReason,
    pub decided_ms: u64,
    pub supervisor_deadline_ms: u64,
    pub observation_deadline_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerObservation {
    Unavailable {},
    Submitted {
        job_id: ControlId,
    },
    Running {},
    IdentityMismatch {},
    DescendantsRemain {},
    /// A diagnostic only; a fresh held Supervisor result is required for reuse.
    Terminated {},
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestState {
    MayHaveDispatched {},
    Terminal { completed_ms: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub spec: RequestSpec,
    pub marked_ms: u64,
    pub state: RequestState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRecord {
    pub spec: LeaseSpec,
    pub capability_sha256: Sha256Digest,
    pub last_ordinal: u64,
    pub released: bool,
}

/// One immutable event per private journal record. No body/partial output or
/// capability plaintext is persisted here; no accounting settlements exist.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Prepared {
        backend: BackendIdentity,
    },
    Acquired {
        lease: LeaseRecord,
    },
    DispatchMarked {
        request: RequestRecord,
    },
    Terminal {
        request_id: ControlId,
        completed_ms: u64,
    },
    StopSelected {
        operation: StopOperation,
    },
    StopObserved {
        operation_id: ControlId,
        observation: ManagerObservation,
    },
    TerminationConfirmed {
        operation_id: ControlId,
    },
    Released {
        lease_id: ControlId,
        operation_id: ControlId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalRecord {
    pub version: u32,
    pub sequence: u64,
    pub previous_sha256: Sha256Digest,
    pub event: Event,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub version: u32,
    pub backend: BackendIdentity,
    pub lease: Option<LeaseRecord>,
    pub request: Option<RequestRecord>,
    pub stop: Option<StopOperation>,
    pub observation: Option<ManagerObservation>,
    pub termination_recorded: bool,
    pub sequence: u64,
    pub durable_sequence: u64,
    pub gate: Gate,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    Preparing,
    Ready,
    InFlight,
    Stopping,
    Blocked,
    Terminated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "At most 16 bounded private control records; retain an allocation-free typed envelope"
)]
pub enum Command {
    Acquire {
        lease: LeaseSpec,
        capability: Capability,
    },
    Query {},
    Dispatch {
        request: RequestSpec,
        capability: Capability,
    },
    Revoke {
        lease_id: ControlId,
        capability: Capability,
        operation_id: ControlId,
    },
    Release {
        lease_id: ControlId,
        capability: Capability,
        operation_id: ControlId,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub version: u32,
    pub nonce: ControlId,
    pub credential: Capability,
    pub command: Command,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Invalid,
    Unauthorized,
    Busy,
    Conflict,
    Expired,
    Blocked,
    Storage,
    Limit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Status { status: Box<Status> },
    Rejected { failure: Failure },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReply {
    pub version: u32,
    pub nonce: ControlId,
    pub outcome: Outcome,
}
