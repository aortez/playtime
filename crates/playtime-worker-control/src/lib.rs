//! Bounded authenticated private control. No server, HTTP, SQLite or controller dependency.

mod files;
mod transport;
mod worker;

pub use files::{
    FileLock, boot_id, boot_ms, create_private_dir, digest, random_id, read_private, write_once,
};
pub use transport::{
    Client, Credential, Endpoint, serve_connection, serve_connection_with_delivery,
};
pub use worker::{Bootstrap, Grant, PreparedExecution, Worker};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    Invalid(&'static str),
    Unauthenticated,
    Deadline,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "private control I/O: {e}"),
            Self::Json(_) => f.write_str("invalid private control JSON"),
            Self::Invalid(e) => f.write_str(e),
            Self::Unauthenticated => f.write_str("worker authentication failed"),
            Self::Deadline => f.write_str("private control deadline exceeded"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
