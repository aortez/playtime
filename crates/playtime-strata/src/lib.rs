//! Strata's stateless Responses transport. Server ownership, process execution
//! and canonical usage settlement remain with the caller and shared controller.
mod client;
mod identity;
mod request;
mod stream;

pub use client::{Client, Endpoint, Inspection, inspect_endpoint};
pub use identity::{HostIdentity, Identity, ModelLock};
pub use request::{Settings, Thinking, encode_request};
pub use stream::{Decoder, StreamEvent};
pub use tokio_util::sync::CancellationToken;

use playtime_engine::{ModelFailure, ModelResponse, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Request,
    Identity,
    Busy,
    Http,
    Connection,
    Protocol,
    ToolParse,
    Remote,
    Incomplete,
    ByteLimit,
    Timeout,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub detail: String,
    /// Only a validated terminal usage report can supply this value.
    pub generated_tokens: Option<u64>,
}
impl Failure {
    pub(crate) fn new(kind: FailureKind, detail: impl ToString) -> Self {
        Self {
            kind,
            detail: detail.to_string().chars().take(1024).collect(),
            generated_tokens: None,
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Completion {
    pub response: ModelResponse,
    pub finish_reason: String,
    pub response_id: String,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub timings: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteState {
    NotDispatched,
    Uncertain,
    TerminalObserved,
}

/// A transport observation, never proof of remote process termination.
#[derive(Clone, Debug)]
pub struct CallResult {
    pub completion: Result<Completion>,
    pub remote: RemoteState,
    pub partial: Value,
    pub received_bytes: usize,
}
impl CallResult {
    pub fn into_outcome(self) -> Outcome {
        match self.completion {
            Ok(done) => Outcome::ModelCompleted {
                response: done.response,
                finish_reason: done.finish_reason,
            },
            Err(error)
                if error.kind == FailureKind::ToolParse && error.generated_tokens.is_some() =>
            {
                Outcome::ModelParseError {
                    detail: error.detail,
                    generated_tokens: error.generated_tokens.unwrap(),
                }
            }
            Err(error) => Outcome::ModelError(match error.kind {
                FailureKind::ToolParse => ModelFailure::Parse(error.detail),
                FailureKind::Connection | FailureKind::Http => {
                    ModelFailure::Connection(error.detail)
                }
                FailureKind::Timeout => ModelFailure::Timeout(error.detail),
                FailureKind::ByteLimit => ModelFailure::ByteLimit(error.detail),
                FailureKind::Incomplete | FailureKind::Cancelled => {
                    ModelFailure::Incomplete(error.detail)
                }
                _ => ModelFailure::Infrastructure(error.detail),
            }),
        }
    }
}

/// Local transport ceilings, independent of the controller's allocations.
#[derive(Clone, Debug)]
pub struct Limits {
    pub metadata_bytes: usize,
    pub request_bytes: usize,
    pub event_bytes: usize,
    pub response_bytes: usize,
    pub assembled_bytes: usize,
    pub events: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            metadata_bytes: 2 << 20,
            request_bytes: 4 << 20,
            event_bytes: 4 << 20,
            response_bytes: 32 << 20,
            assembled_bytes: 4 << 20,
            events: 100_000,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        let cap = Self::default();
        if [
            (self.metadata_bytes, cap.metadata_bytes),
            (self.request_bytes, cap.request_bytes),
            (self.event_bytes, cap.event_bytes),
            (self.response_bytes, cap.response_bytes),
            (self.assembled_bytes, cap.assembled_bytes),
        ]
        .iter()
        .any(|(v, c)| *v == 0 || v > c)
            || self.events == 0
            || self.events > cap.events
        {
            return Err(Failure::new(
                FailureKind::Request,
                "invalid Strata transport bounds",
            ));
        }
        Ok(())
    }
}

/// A bounded raw-event channel. Drain it while awaiting the model; a slow
/// consumer applies backpressure, and the model deadline/cancellation still run.
pub fn events() -> (
    tokio::sync::mpsc::Sender<StreamEvent>,
    tokio::sync::mpsc::Receiver<StreamEvent>,
) {
    tokio::sync::mpsc::channel(8)
}
