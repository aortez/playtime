//! Explicitly retained session files. These records are presentation history,
//! not model input, controller observations, or proof of tool execution.
use playtime_core::{RunId, Sha256Digest};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Image,
    Diagram,
    File,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFile {
    pub original_name: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub schema_version: u32,
    pub id: String,
    pub kind: ArtifactKind,
    pub caption: String,
    pub source_turn: RunId,
    pub retained_at_unix_ms: u64,
    pub file: ArtifactFile,
    pub source: Option<ArtifactFile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCatalog {
    pub schema_version: u32,
    pub artifacts: Vec<Artifact>,
}
