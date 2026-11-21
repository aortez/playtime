//! Async effect driver and terminal-client adapters for the shared chat harness.
//! No alternate agent loop or compaction policy lives here.
pub mod environment;
pub mod host;
pub mod native;
pub mod output;
pub mod prompt;
pub mod runtime;
pub mod sessions;
pub mod terminal;
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub mod worker;

pub mod native_project;
pub mod readonly;
