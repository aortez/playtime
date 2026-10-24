//! Immutable authoring snapshot. It contains no host bindings or credentials.
use super::{IMAGE_LOCK, MODEL_LOCK, Recipe};
use crate::{Error, Result};
use playtime_protocol::api::{RecipeDefinition, RecipeId, RecipeSeed, RecipeSource, SavedRecipe};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRecipe {
    pub schema_version: u32,
    pub harness: String,
    pub recipe: Recipe,
    pub model_lock: Value,
    pub environment_lock: Value,
    pub source: Option<RecipeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_capabilities: Option<playtime_protocol::api::RecipeModelCapabilities>,
    /// Selected immutable authority. model_lock remains the base recipe provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_registration: Option<playtime_protocol::api::RegisteredModel>,
}
impl FrozenRecipe {
    pub fn current(recipe: Recipe) -> Result<Self> {
        let value = Self {
            schema_version: 1,
            harness: "memento_turns_v1".into(),
            recipe,
            model_lock: serde_json::from_slice(MODEL_LOCK)?,
            environment_lock: serde_json::from_slice(IMAGE_LOCK)?,
            source: None,
            model_capabilities: None,
            model_registration: None,
        };
        value.validate()?;
        Ok(value)
    }
    /// Structural validation intentionally does not require current identities.
    pub fn validate(&self) -> Result<()> {
        if !matches!(
            (self.schema_version, self.model_registration.is_some()),
            (1, false) | (2, true)
        ) || self.harness.is_empty()
            || !self.model_lock.is_object()
            || !self.environment_lock.is_object()
        {
            return Err(Error::Invalid("unsupported recipe definition".into()));
        }
        if let Some(model) = &self.model_registration {
            if model.schema_version != 1
                || !model.summary.identity.valid()
                || !model.definition.is_object()
                || self.model_capabilities != model.summary.identity.reviewed_capabilities
            {
                return Err(Error::Invalid("invalid retained model registration".into()));
            }
            self.recipe.check_registered()?;
        } else {
            self.recipe.check()?;
        }
        if let Some(c) = &self.model_capabilities {
            // Retained evidence belongs to its own lock, not whichever model
            // this build can currently execute. Old definitions remain readable.
            let (digest, backend) = self
                .model_registration
                .as_ref()
                .map(|m| {
                    (
                        Some(m.summary.identity.digest.as_str()),
                        Some(m.summary.identity.backend_version.as_str()),
                    )
                })
                .unwrap_or((
                    self.model_lock["digest"].as_str(),
                    self.model_lock["ollama_version"].as_str(),
                ));
            if digest != Some(c.model_digest.as_str())
                || backend != Some(c.backend_version.as_str())
                || c.provenance.is_empty()
                || c.provenance.len() > 512
                || c.reasoning.is_empty()
                || c.reasoning.len() > 4
                || c.reasoning
                    .iter()
                    .enumerate()
                    .any(|(i, v)| c.reasoning[..i].contains(v))
                || c.default_reasoning
                    .is_some_and(|v| !c.reasoning.contains(&v))
                || c.context_ceiling == Some(0)
            {
                return Err(Error::Invalid("invalid retained model capabilities".into()));
            }
        }
        Ok(())
    }
    pub fn executable(&self) -> Result<()> {
        self.validate()?;
        // Authoring a registered identity does not widen the locked worker.
        // The next execution slice must verify this authority at every boundary.
        if self.model_registration.is_some() {
            return Err(Error::Invalid(
                "registered model execution is unavailable".into(),
            ));
        }
        if self.harness != "memento_turns_v1"
            || self.model_lock != serde_json::from_slice::<Value>(MODEL_LOCK)?
            || self.environment_lock != serde_json::from_slice::<Value>(IMAGE_LOCK)?
        {
            return Err(Error::Invalid(
                "recipe execution identities are unavailable".into(),
            ));
        }
        if let Some(capabilities) = &self.model_capabilities
            && *capabilities != self.resolve_model_capabilities()?
        {
            return Err(Error::Invalid("unsupported model capabilities".into()));
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        Ok(playtime_storage::sha256(&serde_json::to_vec(self)?).to_string())
    }
    pub fn view(&self, saved: Option<SavedRecipe>) -> Result<RecipeDefinition> {
        self.validate()?;
        let mut inherited = serde_json::to_value(self)?;
        let recipe = inherited["recipe"].as_object_mut().unwrap();
        for field in ["id", "name", "prompt", "seeds"] {
            recipe.remove(field);
        }
        Ok(RecipeDefinition {
            schema_version: self.schema_version,
            execution_recipe_id: RecipeId::new(&self.recipe.id)?,
            definition_digest: self.digest()?,
            saved,
            source: self.source.clone(),
            name: self.recipe.name.clone(),
            prompt: self.recipe.prompt.clone(),
            inherited,
            model_registration: self.model_registration.as_ref().map(|m| m.summary.clone()),
            seeds: self
                .recipe
                .seeds
                .iter()
                .map(|(path, content)| RecipeSeed {
                    path: path.clone(),
                    size_bytes: content.len() as u64,
                    sha256: playtime_storage::sha256(content.as_bytes()).to_string(),
                })
                .collect(),
        })
    }
}
