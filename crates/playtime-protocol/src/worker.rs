//! Private worker control v1. These values confer neither execution nor read authority.
//! Credentials are carried separately by the private transport, never in Status.

use playtime_core::{InvalidValue, RunId, Sha256Digest};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const FRAME_BYTES: usize = 65_536;
pub const REQUEST_SECONDS: u64 = 5;
/// Longest failure diagnostic a worker reports, in bytes.
pub const FAILURE_BYTES: usize = 1024;

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
    /// Real controller and archive writer with CPU-only scripted external effects.
    ControllerSimulation,
    /// Scripted model and real Docker workshop; requires systemd supervision.
    DockerScripted,
    /// Live locked model and real Docker workshop; requires systemd supervision
    /// and the machine-wide live-model lock.
    DockerLive,
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
    /// Why the execution ended without a normal committed outcome, once it
    /// has: printable text of at most `FAILURE_BYTES`. The worker then only
    /// serves receipts until supervision stops it. Older workers omit it and
    /// report only `reconciling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// A failure diagnostic in the form a worker reports it: control characters
/// become spaces, and longer text is cut on a character boundary to end with
/// an ellipsis within `FAILURE_BYTES`.
pub fn failure_text(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return "the execution ended without a normal outcome".into();
    }
    if text.len() <= FAILURE_BYTES {
        return text.into();
    }
    let mut end = FAILURE_BYTES - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
/// Whether a reported failure diagnostic has the form `failure_text` gives.
pub fn valid_failure(text: &str) -> bool {
    !text.is_empty() && text.len() <= FAILURE_BYTES && !text.chars().any(char::is_control)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub nonce: ControlId,
    pub binding: Binding,
    pub status: Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_text_is_printable_bounded_and_never_empty() {
        let long = failure_text(&"é".repeat(FAILURE_BYTES));
        assert!(valid_failure(&long));
        assert!(long.ends_with('…') && long.len() > FAILURE_BYTES - 4);
        assert_eq!(failure_text("one\ntwo\u{1b}[31m "), "one two [31m");
        assert!(valid_failure(&failure_text(" \n ")));
        assert!(!valid_failure("") && !valid_failure("one\ntwo"));
        assert!(!valid_failure(&"x".repeat(FAILURE_BYTES + 1)));
    }
    #[test]
    fn a_status_without_a_failure_keeps_its_wire_form() {
        let status = Status {
            state: WorkerState::Running,
            work_started_boot_ms: Some(1),
            wall_ms: 2,
            latest_receipt: None,
            publication_pending: false,
            failure: None,
        };
        let wire = serde_json::to_value(&status).unwrap();
        assert!(wire.get("failure").is_none());
        assert_eq!(serde_json::from_value::<Status>(wire).unwrap(), status);
    }
}
