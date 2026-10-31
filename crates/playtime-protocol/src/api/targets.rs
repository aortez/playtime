//! Configured inference locations. Model identities and recipe settings remain independent.
use super::*;
pub const INFERENCE_TARGETS: &str = "inference_targets_v1";
pub const TARGET_HEADER: &str = "x-playtime-target";
pub const TARGET_LIMIT: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProvider {
    Ollama,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InferenceTarget {
    pub id: String,
    pub name: String,
    pub provider: InferenceProvider,
    pub endpoint: String,
}
impl InferenceTarget {
    pub fn valid(&self) -> bool {
        valid_id(&self.id)
            && !self.name.is_empty()
            && self.name.len() <= 128
            && !self.name.chars().any(char::is_control)
            && self.endpoint.len() <= 2048
            && self.endpoint.starts_with("http://")
            && !self.endpoint.chars().any(char::is_control)
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InferenceTargets {
    pub schema_version: u32,
    pub targets: Vec<InferenceTarget>,
}
impl InferenceTargets {
    pub fn valid(&self) -> bool {
        let mut ids = std::collections::BTreeSet::new();
        self.schema_version == 1
            && self.targets.len() <= TARGET_LIMIT
            && self.targets.iter().all(|t| t.valid() && ids.insert(&t.id))
    }
}
