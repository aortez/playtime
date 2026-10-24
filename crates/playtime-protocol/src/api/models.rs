//! Read-only metadata observations. These are never execution registrations.
use super::*;
pub const MODEL_DISCOVERY: &str = "model_discovery_v1";
pub const MODEL_INVENTORY_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelLoaded {
    Loaded,
    NotLoaded,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataStatus {
    Unverified,
    Rejected,
    Missing,
    Changed,
    Candidate,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveredModel {
    pub name: String,
    pub digest: Option<String>,
    pub status: ModelMetadataStatus,
    pub reason: Option<String>,
    pub loaded: ModelLoaded,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelInventory {
    pub schema_version: u32,
    #[schema(value_type = String, format = DateTime)]
    pub observed_at: UtcTime,
    pub backend_version: String,
    pub models: Vec<DiscoveredModel>,
    pub loaded_warning: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelInspectRequest {
    pub name: String,
    pub expected_digest: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelInspection {
    pub schema_version: u32,
    #[schema(value_type = String, format = DateTime)]
    pub observed_at: UtcTime,
    pub name: String,
    pub backend_version: String,
    pub digest: Option<String>,
    pub status: ModelMetadataStatus,
    pub reason: Option<String>,
    pub definition_sha256: Option<String>,
    pub advertised_capabilities: Vec<String>,
    pub capability_provenance: String,
    pub reviewed_capabilities: Option<RecipeModelCapabilities>,
}
