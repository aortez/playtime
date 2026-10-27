//! Admission and private worker handshake, independently testable without HTTP.

mod confirmation;
mod coordinator;
pub mod fixture;
mod flights;
mod ledger;
pub mod ports;
mod supervision;
pub mod systemd;
mod types;

pub use coordinator::Coordinator;
pub(crate) use flights::RUNS;
pub use ledger::Ledger;
pub use types::*;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Database(rusqlite::Error),
    Json(serde_json::Error),
    Control(playtime_worker_control::Error),
    Invalid(&'static str),
    Busy,
    Conflict,
    ServiceMismatch,
    Uncertain,
    /// An operator release found evidence that the run still owns something.
    Refused(Vec<String>),
    Shared(std::sync::Arc<Error>),
    Supervisor(zbus::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "lifecycle I/O: {e}"),
            Self::Database(e) => write!(f, "lifecycle ledger: {e}"),
            Self::Json(_) => f.write_str("invalid lifecycle record"),
            Self::Control(e) => write!(f, "{e}"),
            Self::Invalid(s) => f.write_str(s),
            Self::Busy => f.write_str("lifecycle capacity or active slot occupied"),
            Self::Conflict => f.write_str("idempotency key identifies a different request"),
            Self::ServiceMismatch => f.write_str("service identity mismatch"),
            Self::Uncertain => f.write_str("worker ownership unresolved"),
            Self::Refused(remaining) => write!(f, "release refused: {}", remaining.join("; ")),
            Self::Shared(error) => error.fmt(f),
            Self::Supervisor(error) => write!(f, "systemd user supervisor: {error}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<zbus::Error> for Error {
    fn from(error: zbus::Error) -> Self {
        Self::Supervisor(error)
    }
}
impl From<zbus::zvariant::Error> for Error {
    fn from(error: zbus::zvariant::Error) -> Self {
        Self::Supervisor(error.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<playtime_worker_control::Error> for Error {
    fn from(e: playtime_worker_control::Error) -> Self {
        Self::Control(e)
    }
}
