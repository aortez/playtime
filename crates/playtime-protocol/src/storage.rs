//! Portable archive layout v1. These DTOs alone do not certify a durable commit.

use playtime_core::{RunId, RunMode, Sha256Digest};
use serde::{Deserialize, Serialize};

use crate::{ReadError, archive::ContentRef, read::require_version};

pub const ARCHIVE_SCHEMA_VERSION: u32 = 1;
pub const COMMIT_SCHEMA_VERSION: u32 = 1;
pub const FINALIZATION_RESERVE_BYTES: u64 = 65_536;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveDescriptor {
    pub schema_version: u32,
    pub run_id: RunId,
    pub mode: RunMode,
    pub input_manifest: ContentRef,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommitMarker {
    pub schema_version: u32,
    pub run_id: RunId,
    pub committed_sequence: u64,
    pub committed_bytes: u64,
    pub prefix_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchiveIdentity {
    pub run_id: RunId,
    pub descriptor: ContentRef,
    pub input_manifest: ContentRef,
}

/// Serializable inspection data. Live adapters need the storage writer's
/// non-deserializable CommittedFrontier receipt, not a discovered marker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventFrontier {
    pub archive: ArchiveIdentity,
    pub marker: ContentRef,
    pub committed_sequence: u64,
    pub committed_bytes: u64,
    pub prefix_sha256: Sha256Digest,
}

fn default_page_limit() -> u32 {
    200
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventQuery {
    pub archive: ArchiveIdentity,
    pub after_sequence: u64,
    pub through_sequence: u64,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

/// Local I/O limits, not model/controller admission settings. Limits apply to
/// actual bytes, including event envelopes and line feeds. Readers can choose
/// stricter limits without changing the archive's identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageLimits {
    pub event_log_bytes: u64,
    pub record_bytes: u64,
    pub batch_bytes: u64,
    pub content_bytes: u64,
    pub manifest_bytes: u64,
    pub verified_content_bytes: u64,
    pub references: u64,
    pub events: u64,
    pub page_bytes: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            event_log_bytes: 64 * 1024 * 1024,
            record_bytes: 1024 * 1024,
            batch_bytes: 4 * 1024 * 1024,
            content_bytes: 64 * 1024 * 1024,
            manifest_bytes: 4 * 1024 * 1024,
            verified_content_bytes: 512 * 1024 * 1024,
            references: 10_000,
            events: 100_000,
            page_bytes: 4 * 1024 * 1024,
        }
    }
}

pub fn decode_archive_descriptor(json: &str) -> Result<ArchiveDescriptor, ReadError> {
    require_version(json, "archive layout", ARCHIVE_SCHEMA_VERSION)?;
    serde_json::from_str(json).map_err(ReadError::malformed)
}

pub fn decode_commit_marker(json: &str) -> Result<CommitMarker, ReadError> {
    require_version(json, "event commit marker", COMMIT_SCHEMA_VERSION)?;
    serde_json::from_str(json).map_err(ReadError::malformed)
}
