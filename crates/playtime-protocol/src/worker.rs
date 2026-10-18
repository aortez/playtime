//! Private worker control v1. These values confer neither execution nor read authority.
//! Credentials are carried separately by the private transport, never in Status.

use playtime_core::{InvalidValue, RunId, Sha256Digest};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const FRAME_BYTES: usize = 65_536;
pub const REQUEST_SECONDS: u64 = 5;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ControlId(String);
impl ControlId {
    pub fn new(value: String) -> Result<Self, InvalidValue> {
        if value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value))
        } else {
            Err(InvalidValue(
                "expected 128-bit lowercase hex control identity",
            ))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ControlId {
    type Error = InvalidValue;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<ControlId> for String {
    fn from(value: ControlId) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    /// Lifecycle fixtures only; no controller, experimental archive or real adapters.
    SupervisionFixture,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub service_id: ControlId,
    pub run_id: RunId,
    pub manifest_sha256: Sha256Digest,
    pub executable_sha256: Sha256Digest,
    pub grant_id: ControlId,
    pub worker_id: ControlId,
    pub invocation_id: ControlId,
    pub boot_id: ControlId,
    pub unit: String,
    pub kind: WorkerKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Status,
    Begin,
    Cancel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub nonce: ControlId,
    pub binding: Binding,
    pub command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Prepared,
    Running,
    CancelledBeforeBegin,
    Cancelling,
    Reconciling,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub sequence: u64,
    pub bytes: u64,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub state: WorkerState,
    pub work_started_boot_ms: Option<u64>,
    pub wall_ms: u64,
    pub latest_receipt: Option<Receipt>,
    pub publication_pending: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub nonce: ControlId,
    pub binding: Binding,
    pub status: Status,
}
