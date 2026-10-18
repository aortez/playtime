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
    /// Absence means the older bundle did not advertise recovery semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_profile: Option<String>,
    /// Digest of the exact frozen Docker profiles this worker implements.
    /// Absence means the older bundle did not advertise one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_profile: Option<Sha256Digest>,
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
            recovery_profile: None,
            docker_profile: None,
        }
    }
}
impl Capabilities {
    pub fn current() -> Self {
        Self {
            real_model: true,
            real_workshop: true,
            recovery_profile: Some(crate::recovery::RULE_SET.into()),
            ..Self::default()
        }
    }
    pub fn supported(&self) -> bool {
        let mut ordinary = self.clone();
        ordinary.recovery_profile = None;
        ordinary.real_model = false;
        ordinary.real_workshop = false;
        ordinary.docker_profile = None;
        ordinary == Self::default()
            && self
                .recovery_profile
                .as_deref()
                .is_none_or(|p| p == crate::recovery::RULE_SET)
    }
    /// Docker execution requires the exact frozen profiles the caller implements.
    pub fn implements_docker(&self, profiles: &Sha256Digest) -> bool {
        self.real_workshop && self.docker_profile.as_ref() == Some(profiles)
    }
    /// The live profile also requires the real-model capability.
    pub fn implements_live(&self, profiles: &Sha256Digest) -> bool {
        self.real_model && self.implements_docker(profiles)
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
    /// Embedded fixed helpers; absent on older inspectable bundles.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub capture_helpers: std::collections::BTreeMap<String, String>,
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
