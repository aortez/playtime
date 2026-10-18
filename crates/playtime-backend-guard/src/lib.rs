//! Host-local durable inference ownership. No production backend adapter is
//! installed by this crate. See docs/rust-backend-guard.md for the trusted ports.
pub mod fixture;
mod journal;
mod machine;
pub mod ports;
mod service;
pub mod transport;

pub use journal::{Fault, Journal, PublicationObserver};
use playtime_core::Sha256Digest;
pub use playtime_protocol::backend_guard as wire;
use playtime_protocol::worker::ControlId;
pub use service::{Dispatch, Guard};
use sha2::{Digest, Sha256};
pub type Result<T> = std::result::Result<T, wire::Failure>;

pub fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .unwrap()
}
pub fn random_id() -> Result<ControlId> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| wire::Failure::Blocked)?;
    ControlId::new(bytes.iter().map(|b| format!("{b:02x}")).collect())
        .map_err(|_| wire::Failure::Invalid)
}
pub fn capability() -> Result<wire::Capability> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| wire::Failure::Blocked)?;
    wire::Capability::new(bytes.iter().map(|b| format!("{b:02x}")).collect())
        .map_err(|_| wire::Failure::Invalid)
}
