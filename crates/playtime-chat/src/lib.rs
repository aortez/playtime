//! Async effect driver and terminal-client adapters for the shared chat harness.
//! No alternate agent loop or compaction policy lives here.
pub mod native;
pub mod runtime;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
