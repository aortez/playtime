//! Configured inference locations. Model identities and recipe settings remain independent.
use super::*;
pub const INFERENCE_TARGETS: &str = "inference_targets_v1";
pub const TARGET_HEADER: &str = "x-playtime-target";
pub const TARGET_LIMIT: usize = 16;
/// The daemon has a target that holds a Strata host for each run.
pub const STRATA_TARGETS: &str = "strata_targets_v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InferenceProvider {
    Ollama,
    /// A reviewed Strata host that the daemon holds through the chat launcher
    /// for each run.
    Strata,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InferenceTarget {
    pub id: String,
    pub name: String,
    pub provider: InferenceProvider,
    /// An Ollama target's HTTP endpoint. A Strata target has none: each run's
    /// hold lends a loopback endpoint.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    /// A Strata target's chat launcher profile, such as `framework-gyro-m-fast`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}
impl InferenceTarget {
    pub fn valid(&self) -> bool {
        valid_id(&self.id)
            && !self.name.is_empty()
            && self.name.len() <= 128
            && !self.name.chars().any(char::is_control)
            && match self.provider {
                InferenceProvider::Ollama => {
                    self.profile.is_none()
                        && self.endpoint.len() <= 2048
                        && self.endpoint.starts_with("http://")
                        && !self.endpoint.chars().any(char::is_control)
                }
                InferenceProvider::Strata => {
                    self.endpoint.is_empty() && self.profile.as_deref().is_some_and(valid_profile)
                }
            }
    }
}
/// A launcher profile name: 1 to 64 lowercase letters, digits and hyphens.
pub fn valid_profile(profile: &str) -> bool {
    (1..=64).contains(&profile.len())
        && profile
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
