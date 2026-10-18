//! Bounded local Docker effects. This crate is not a controller or an admission
//! authority: its caller must commit ownership and dispatch intent first.
mod bridge;
mod engine;
mod sandbox;
pub mod transport;
mod types;

pub use bridge::{Tool, ToolLimits, ToolReply};
pub use engine::{Engine, Observation, ReadyImage};
pub use sandbox::{Cleanup, Quiescent, ResourceOutcome, Sandbox};
pub use tokio_util::sync::CancellationToken;
pub use types::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InvalidInput,
    Unsupported,
    MissingImage,
    DaemonUnavailable,
    ChangedIdentity,
    Conflict,
    Malformed,
    Deadline,
    Cancelled,
    Transport,
    Fenced,
    Journal,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Error {
    pub failure: Failure,
    pub operation: &'static str,
    /// Static, safe context; model payloads and arbitrary daemon diagnostics are
    /// never included in Display. Bounded raw transport evidence is separate.
    pub detail: &'static str,
}
impl Error {
    pub(crate) fn new(failure: Failure, operation: &'static str, detail: &'static str) -> Self {
        Self {
            failure,
            operation,
            detail,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {:?}: {}", self.operation, self.failure, self.detail)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
