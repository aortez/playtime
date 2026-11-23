//! Versioned recipe authoring contracts. Execution still uses immutable RecipeId
//! handles and the existing preview/admission protocol.
use super::*;

pub const RECIPE_MODEL_SELECTION: &str = "recipe_model_selection_v1";
pub const RECIPE_MODEL_EXECUTION: &str = "recipe_model_execution_v1";

pub const MODEL_SETTINGS: &str = "recipe_model_settings_v1";

pub const RECIPE_AUTHORING: &str = "recipe_authoring_v1";

/// Saves may replace a recipe's seed files with an empty or uploaded set.
pub const RECIPE_SEED_FILES: &str = "recipe_seed_files_v1";

pub const RECIPE_CONTEXT_CEILING: u64 = 131_072;

/// Seed-file bounds that clients can check before sending a set. The service
/// also keeps a set within half of the recipe's workspace.
pub const SEED_FILES: usize = 16;
pub const SEED_FILE_BYTES: usize = 131_072;
/// A file's bound once JSON escapes its control characters.
pub const SEED_ENCODED_BYTES: usize = 196_608;
pub const SEED_BYTES: usize = 524_288;
pub const SEED_PATH_BYTES: usize = 200;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeSource {
    pub execution_recipe_id: RecipeId,
    pub definition_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SavedRecipe {
    pub recipe_id: RecipeId,
    pub execution_recipe_id: RecipeId,
    pub revision: u64,
    pub creation_sequence: u64,
    pub definition_digest: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeSeed {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Where a definition's seed files came from when they aren't its catalog
/// recipe's files.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SeedOrigin {
    /// A save chose to start without seed files.
    Empty,
    /// A save replaced the files with ones a client uploaded.
    Uploaded,
}

/// Replaces the source revision's seed files. These are the files under
/// `/work/seeds`, not the numeric sampling seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum SeedSelection {
    Empty,
    /// At least one file. Each one is sent whole, so the service never reads a
    /// client path.
    Uploaded {
        files: Vec<SeedUpload>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SeedUpload {
    /// Relative path under `/work/seeds`.
    pub path: String,
    /// Byte length of `content`. The service refuses a mismatch.
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of `content`. The service refuses a mismatch.
    pub sha256: String,
    /// The file's exact UTF-8 text.
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeDefinition {
    pub schema_version: u32,
    pub execution_recipe_id: RecipeId,
    pub definition_digest: String,
    pub saved: Option<SavedRecipe>,
    pub source: Option<RecipeSource>,
    pub name: String,
    pub prompt: String,
    /// Stored inherited settings, without seed bytes or operational credentials.
    #[schema(value_type = Object)]
    pub inherited: Value,
    pub seeds: Vec<RecipeSeed>,
    /// Absent when the seed files are the catalog recipe's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_origin: Option<SeedOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_registration: Option<ModelRegistrationSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneRecipeRequest {
    /// Explicit settings for the continuous harness, applied after harness selection. Omission keeps the source's settings, or the defaults after a switch. Refused when the resulting harness is memento.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    /// Omission inherits the source harness. Switching to continuous installs the default compaction settings; switching to memento drops them and uses the source's retained memento settings. Shared budgets are preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub harness: Option<HarnessId>,
    pub source: RecipeSource,
    pub name: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_settings: Option<RecipeModelSettings>,
    /// Omission inherits the source revision's model. Selecting a different registration defaults context to its reviewed window unless model_settings is supplied; other settings are inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_registration: Option<ModelRegistrationId>,
    /// Omission keeps the source revision's seed files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeds: Option<SeedSelection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseRecipeRequest {
    /// Explicit settings for the continuous harness, applied after harness selection. Omission keeps the source's settings, or the defaults after a switch. Refused when the resulting harness is memento.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    /// Omission inherits the source harness. Switching to continuous installs the default compaction settings; switching to memento drops them and uses the source's retained memento settings. Shared budgets are preserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub harness: Option<HarnessId>,
    pub expected_head: RecipeId,
    pub name: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_settings: Option<RecipeModelSettings>,
    /// Omission inherits the source revision's model. Selecting a different registration defaults context to its reviewed window unless model_settings is supplied; other settings are inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_registration: Option<ModelRegistrationId>,
    /// Omission keeps the previous revision's seed files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeds: Option<SeedSelection>,
}

impl RecipeMutation {
    pub fn seeds(&self) -> Option<&SeedSelection> {
        match self {
            Self::Clone { request } => request.seeds.as_ref(),
            Self::Revise { request, .. } => request.seeds.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecipeMutation {
    Clone {
        request: CloneRecipeRequest,
    },
    Revise {
        recipe_id: RecipeId,
        request: ReviseRecipeRequest,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeSaveResponse {
    pub service_id: ServiceId,
    pub key: IdempotencyKey,
    pub request: RecipeMutation,
    pub definition: RecipeDefinition,
    pub replayed: bool,
}

/// An explicit omission is distinct from any particular reasoning level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecipeReasoning {
    ModelDefault,
    Low,
    Medium,
    High,
    On,
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeModelSettings {
    pub reasoning: RecipeReasoning,
    pub temperature: f64,
    pub context_tokens: u64,
    pub tokens_per_call: u64,
    pub handoff_tokens: u64,
    pub top_p: f64,
    pub top_k: u32,
    pub repeat_penalty: f64,
    pub sampling_seed: u32,
}
impl RecipeModelSettings {
    /// Shared form checks; the service also checks inherited constraints and context fit.
    pub fn validation_error(&self) -> Option<&'static str> {
        if !self.temperature.is_finite() || !(0.0..=2.0).contains(&self.temperature) {
            return Some("Temperature must be between 0 and 2.");
        }
        if !(self.top_p.is_finite() && self.top_p > 0.0 && self.top_p <= 1.0) {
            return Some("Top-p must be greater than 0 and at most 1.");
        }
        if !(1..=1000).contains(&self.top_k) {
            return Some("Top-k must be between 1 and 1,000.");
        }
        if !(self.repeat_penalty.is_finite()
            && self.repeat_penalty > 0.0
            && self.repeat_penalty <= 2.0)
        {
            return Some("Repeat penalty must be greater than 0 and at most 2.");
        }
        if self.sampling_seed > 2147483647 {
            return Some("Sampling seed must be between 0 and 2,147,483,647.");
        }
        if !(1..=RECIPE_CONTEXT_CEILING).contains(&self.context_tokens) {
            return Some("Context capacity must be between 1 and 131,072 tokens.");
        }
        if !(1..=16384).contains(&self.tokens_per_call)
            || !(1..=16384).contains(&self.handoff_tokens)
        {
            return Some("Response and handoff limits must be between 1 and 16,384 tokens.");
        }
        None
    }
}

/// Reviewed capabilities for the exact locked model and backend, retained in new definitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeModelCapabilities {
    pub model_digest: String,
    pub backend_version: String,
    pub provenance: String,
    pub reasoning: Vec<RecipeReasoning>,
    pub default_reasoning: Option<RecipeReasoning>,
    pub context_ceiling: Option<u64>,
}

impl RecipeModelCapabilities {
    /// A new selection's default allocation, bounded by the recipe API limit.
    /// This is reviewed model capacity, not a measurement of host memory fit.
    pub fn default_context_tokens(&self) -> Option<u64> {
        self.context_ceiling
            .filter(|&tokens| tokens > 0)
            .map(|tokens| tokens.min(RECIPE_CONTEXT_CEILING))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeSettingsView {
    pub execution_recipe_id: RecipeId,
    pub definition_digest: String,
    pub settings: RecipeModelSettings,
    pub capabilities: RecipeModelCapabilities,
    pub generated_tokens: u64,
}
