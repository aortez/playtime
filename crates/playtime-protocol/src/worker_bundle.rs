//! Frozen executable packaging, independent of public HTTP and admission policy.
use crate::archive::{BuildProvenance, ContentRef, WorkerIdentity};
use playtime_core::Sha256Digest;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const TARGET: &str = "x86_64-unknown-linux-musl";
pub const DESCRIPTOR: &str = "inputs/worker/bundle.json";
pub const BUILD: &str = "inputs/worker/build.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub private_protocol: u32,
    pub manifest_schema: u32,
    pub event_schema: u32,
    pub controller_simulation: bool,
    pub real_model: bool,
    pub real_workshop: bool,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            private_protocol: 1,
            manifest_schema: 1,
            event_schema: 1,
            controller_simulation: true,
            real_model: false,
            real_workshop: false,
        }
    }
}

/// Created before compilation and embedded verbatim; excludes executable and
/// descriptor hashes to avoid circular identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecord {
    pub schema_version: u32,
    pub source_sha256: Sha256Digest,
    pub lock_sha256: Sha256Digest,
    pub provenance: BuildProvenance,
    pub cargo_version: String,
    pub linker_version: String,
    pub features: Vec<String>,
    pub rustflags: Vec<String>,
    pub capabilities: Capabilities,
}

/// The bundle ID is SHA-256 of these exact serialized bytes, not a field here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleDescriptor {
    pub schema_version: u32,
    pub worker: WorkerIdentity,
    pub build_record: ContentRef,
    pub capabilities: Capabilities,
}
