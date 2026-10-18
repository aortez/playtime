//! Versioned host readiness and clock sampling. This is not a lease or receipt.
use crate::{
    backend_guard::{BackendIdentity, Capability},
    worker::ControlId,
};
use playtime_core::Sha256Digest;
use serde::{Deserialize, Serialize};
pub const VERSION: u32 = 1;
pub const MAX_RTT_MS: u64 = 1_000;
pub const MAX_HORIZON_MS: u64 = 300_000;
pub const MARGIN_MS: u64 = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Challenge {
    pub version: u32,
    pub nonce: ControlId,
    pub credential: Capability,
    pub local_boot: ControlId,
    pub local_send_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileReply {
    pub version: u32,
    pub nonce: ControlId,
    pub local_boot: ControlId,
    pub local_send_ms: u64,
    pub host_ms: u64,
    pub backend: BackendIdentity,
    pub lock_sha256: Sha256Digest,
    pub settings_sha256: Sha256Digest,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeadlineMapping {
    pub version: u32,
    pub nonce: ControlId,
    pub local_boot: ControlId,
    pub backend: BackendIdentity,
    pub local_send_ms: u64,
    pub local_receive_ms: u64,
    pub host_sample_ms: u64,
    pub local_deadline_ms: u64,
    pub host_deadline_ms: u64,
}
