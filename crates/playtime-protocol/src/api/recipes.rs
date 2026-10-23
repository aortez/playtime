//! Versioned recipe authoring contracts. Execution still uses immutable RecipeId
//! handles and the existing preview/admission protocol.
use super::*;

pub const RECIPE_AUTHORING: &str = "recipe_authoring_v1";

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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneRecipeRequest {
    pub source: RecipeSource,
    pub name: String,
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseRecipeRequest {
    pub expected_head: RecipeId,
    pub name: String,
    pub prompt: String,
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
