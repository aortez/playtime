use playtime_core::{RunId, RunMode, Sha256Digest, UtcTime};
use playtime_protocol::worker::{Binding, ControlId, Status};
use playtime_worker_control::Endpoint;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedLaunch {
    pub binding: Binding,
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub bootstrap: PathBuf,
    pub private_dir: PathBuf,
    pub socket: PathBuf,
    pub startup_deadline_boot_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Saved authenticated worker evidence, not a new liveness assertion.
    pub last_status: Option<Status>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Acceptance {
    pub replayed: bool,
    pub admission: Admission,
}
