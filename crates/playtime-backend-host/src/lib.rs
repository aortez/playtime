//! Dedicated host policy. No public HTTP routes, archive writes or worker wiring.
pub mod clock;
pub mod deadline;
pub mod host;
pub mod ollama;
pub mod ssh;
pub mod systemd;
pub use playtime_backend_guard::{Result, wire};
pub use playtime_protocol::backend_host as protocol;
