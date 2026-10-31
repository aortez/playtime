use playtime_core::{RunId, RunMode, Sha256Digest, UtcTime};
use playtime_protocol::worker::{Binding, ControlId, Status};
use playtime_worker_control::Endpoint;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub recipe_id: String,
    pub mode: RunMode,
    pub preview_revision: String,
}
impl AdmissionRequest {
    pub fn validate(&self) -> super::Result<()> {
        for id in [&self.recipe_id, &self.preview_revision] {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                return Err(super::Error::Invalid("invalid admission identifier"));
            }
        }
        Ok(())
    }
}

/// Fully resolved preview, including the selected worker. Replays retain this
/// value even after the service's default bundle or recipe changes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub executable_sha256: Sha256Digest,
    pub wall_ms: u64,
    pub token_limit: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Stamp {
    pub utc: UtcTime,
    pub boot_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Starting,
    Running,
    Cancelling,
    Reconciling,
    Terminal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    CoordinatorRestarted,
    PreparationFailed,
    StartUncertain,
    WorkerUnavailable,
    BeginUncertain,
    CancelRequested,
    ControlUncertain,
    OperatorReleased,
    /// The worker reported that its execution ended without a normal outcome,
    /// and the daemon stopped it. The run waits for operator release.
    ExecutionFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedLaunch {
    pub binding: Binding,
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub bootstrap: PathBuf,
    pub private_dir: PathBuf,
    pub socket: PathBuf,
    pub startup_deadline_boot_ms: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelDelivery {
    Requested,
    WorkerAcknowledged,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopDelivery {
    Requested,
    Accepted,
    Stopped,
    Uncertain,
}

/// Supervisor evidence is distinct from archive/controller outcome and cannot
/// release admission or authorize recovery. Zero timestamps mean unobserved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SupervisorObservation {
    pub manager_id: ControlId,
    pub cgroup: String,
    pub active_state: String,
    pub main_pid: u32,
    pub ready_monotonic_us: u64,
    pub stopping_monotonic_us: u64,
    pub result: String,
    pub stopped: bool,
    pub exit: Option<playtime_worker_control::systemd::Exit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    pub service_id: ControlId,
    pub run_id: RunId,
    pub key: String,
    pub request: AdmissionRequest,
    pub request_sha256: Sha256Digest,
    pub selection: Selection,
    pub accepted: Stamp,
    pub phase: Phase,
    pub reason: Option<Reason>,
    pub prepared: Option<PreparedLaunch>,
    pub start_attempted: bool,
    pub endpoint: Option<Endpoint>,
    pub begin_authorized: bool,
    pub cancel_requested: Option<Stamp>,
    pub cancel_delivery: Option<CancelDelivery>,
    pub stop_delivery: Option<StopDelivery>,
    pub stop_accepted: Option<Stamp>,
    pub supervisor: Option<SupervisorObservation>,
    /// Superseded by exclusive takeover, never inferred from a worker reply.
    pub owner_generation: u64,
    /// Incremented on each changed durable record, including control decisions.
    pub revision: u64,
    /// Saved authenticated worker evidence, not a new liveness assertion.
    pub last_status: Option<Status>,
    /// The published read boundary is separate from the last worker reply so
    /// future recovery cannot silently relabel recovered bytes as confirmation.
    #[serde(default)]
    pub confirmed_read: Option<ConfirmedRead>,
    /// Explicit operator release of an interrupted or failed run. It is not
    /// normal completion and never changes the archive or its accounting.
    #[serde(default)]
    pub operator_release: Option<OperatorRelease>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperatorRelease {
    pub released: Stamp,
    pub reason: String,
    /// Fresh evidence that the original invocation stopped. None only when no
    /// worker start was ever submitted.
    pub supervisor: Option<SupervisorObservation>,
    /// Exact external resources verified absent before release.
    pub absent_resources: Vec<String>,
}

/// External resources a run's worker may own, checked by exact identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExternalResources {
    pub absent: Vec<String>,
    pub present: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfirmedRead {
    pub frontier: playtime_protocol::storage::EventFrontier,
    pub owner_generation: u64,
    pub source: ConfirmationSource,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationSource {
    Worker,
}

#[derive(Clone, Debug, Serialize)]
pub struct Acceptance {
    pub replayed: bool,
    pub admission: Admission,
}
