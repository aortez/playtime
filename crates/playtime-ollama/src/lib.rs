//! Native external I/O for the exact locked Ollama compatibility profile.
//! No tool execution, retries, canonical accounting or backend stop authority.
mod adapter;
mod identity;
pub mod json;
mod observation;
mod request;
mod stream;
mod transport;

pub use adapter::{Adapter, ChatResult, Dispatch, RemoteOutcome};
pub use identity::{Inspection, ModelLock, Profile};
pub use observation::{
    Observation, ObservationKind, ObservationReceiver, ObservationSender, observations,
};
pub use request::{EffectiveSettings, Thinking, encode_request};
use serde::{Deserialize, Serialize};
pub use stream::{AssistantMessage, Decoder};
pub use tokio_util::sync::CancellationToken;
pub use transport::{
    Endpoint, HttpResponse, NativeTransport, Operation, Request, Resolution, ResponseBody,
    Transport, Trust,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Identity,
    Request,
    Protocol,
    BackendToolParse,
    Http,
    Transport,
    Incomplete,
    MissingUsage,
    ByteLimit,
    Timeout,
    Cancelled,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: FailureKind,
    pub detail: String,
}
impl Failure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        let mut end = detail.len().min(4096);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
        Self { kind, detail }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;

/// Policy limits may only be reduced. They describe local adapter work, not
/// server termination or a larger controller allocation.
#[derive(Clone, Debug)]
pub struct Limits {
    pub metadata_bytes: usize,
    pub request_bytes: usize,
    pub record_bytes: usize,
    pub response_bytes: usize,
    pub assembled_bytes: usize,
    pub error_bytes: usize,
    pub records: u64,
    pub queue_records: usize,
    pub queue_bytes: usize,
    pub connect_timeout: std::time::Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            metadata_bytes: 16 * 1024 * 1024,
            request_bytes: 4 * 1024 * 1024,
            record_bytes: 512 * 1024,
            response_bytes: 4 * 1024 * 1024,
            assembled_bytes: 4 * 1024 * 1024,
            error_bytes: 512 * 1024,
            records: 65_536,
            queue_records: 65_536,
            queue_bytes: 4 * 1024 * 1024,
            connect_timeout: std::time::Duration::from_secs(5),
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        let d = Self::default();
        if [
            (self.metadata_bytes, d.metadata_bytes),
            (self.request_bytes, d.request_bytes),
            (self.record_bytes, d.record_bytes),
            (self.response_bytes, d.response_bytes),
            (self.assembled_bytes, d.assembled_bytes),
            (self.error_bytes, d.error_bytes),
            (self.queue_records, d.queue_records),
            (self.queue_bytes, d.queue_bytes),
        ]
        .into_iter()
        .any(|(v, max)| v == 0 || v > max)
            || self.records == 0
            || self.records > d.records
            || self.connect_timeout.is_zero()
            || self.connect_timeout > d.connect_timeout
        {
            return Err(Failure::new(
                FailureKind::Request,
                "invalid adapter policy limits",
            ));
        }
        Ok(())
    }
}
