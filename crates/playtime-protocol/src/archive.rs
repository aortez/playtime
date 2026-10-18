use std::collections::BTreeSet;

use playtime_core::{ArchivePath, RunId, Sha256Digest};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ReadError, read::require_version};

pub const INPUT_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const INPUT_MANIFEST_PATH: &str = "inputs/manifest.json";

/// SHA-256 and length refer to exact stored bytes, without canonicalization.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ContentRef {
    #[schema(value_type = String, min_length = 1)]
    pub path: ArchivePath,
    #[schema(value_type = String, pattern = "^[0-9a-f]{64}$")]
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct BuildProvenance {
    pub rustc_version: String,
    pub target_triple: String,
    pub cargo_profile: String,
    pub source_revision: Option<String>,
    pub dirty: bool,
}

/// Recorded requirements are verified on the executing host by the worker route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct PlatformRequirements {
    pub os: String,
    pub architecture: String,
    pub abi: String,
    pub requirements: Vec<String>,
}

/// Repeat identity belongs to the experimental worker, independently of viewers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct WorkerIdentity {
    pub executable: ContentRef,
    pub source: ContentRef,
    pub cargo_lock: ContentRef,
    pub build: BuildProvenance,
    pub platform: PlatformRequirements,
}

/// Raw frozen-input DTO; use decode_input_manifest() for structural validation.
/// The stored manifest's own hash is held outside this file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct InputManifest {
    pub schema_version: u32,
    #[schema(value_type = String, pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")]
    pub run_id: RunId,
    pub worker: WorkerIdentity,
    pub configuration: ContentRef,
    pub initial_workspace: ContentRef,
    /// Prompts, tool definitions, seed materials, and model/workshop identities.
    pub files: Vec<ContentRef>,
}

impl InputManifest {
    pub fn references(&self) -> impl Iterator<Item = &ContentRef> {
        [
            &self.configuration,
            &self.initial_workspace,
            &self.worker.executable,
            &self.worker.source,
            &self.worker.cargo_lock,
        ]
        .into_iter()
        .chain(&self.files)
    }

    fn validate(&self) -> Result<(), ReadError> {
        let mut paths = BTreeSet::new();
        for reference in self.references() {
            let path = reference.path.as_str();
            if !path.starts_with("inputs/") || path == INPUT_MANIFEST_PATH || !paths.insert(path) {
                return Err(ReadError::malformed(
                    "input paths must be unique, under inputs/, and exclude the manifest itself",
                ));
            }
        }
        let build = &self.worker.build;
        let platform = &self.worker.platform;
        if [
            &build.rustc_version,
            &build.target_triple,
            &build.cargo_profile,
            &platform.os,
            &platform.architecture,
            &platform.abi,
        ]
        .iter()
        .any(|value| value.is_empty())
        {
            return Err(ReadError::malformed(
                "worker build and platform identity fields must not be empty",
            ));
        }
        Ok(())
    }
}

/// Structural validation only; hashing bytes and publishing archives belong to #19.
pub fn decode_input_manifest(json: &str) -> Result<InputManifest, ReadError> {
    require_version(json, "input manifest", INPUT_MANIFEST_SCHEMA_VERSION)?;
    let manifest: InputManifest = serde_json::from_str(json).map_err(ReadError::malformed)?;
    manifest.validate()?;
    Ok(manifest)
}
