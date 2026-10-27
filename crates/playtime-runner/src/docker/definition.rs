//! Immutable authoring snapshot. It contains no host bindings or credentials.
use super::{IMAGE_LOCK, MODEL_LOCK, Recipe};
use crate::{Error, Result};
use playtime_protocol::api::{
    CONTINUOUS_COMPACTION_V1, MEMENTO_TURNS_V1, RecipeDefinition, RecipeId, RecipeSeed,
    RecipeSource, SavedRecipe,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRecipe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionDefinition>,
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
/// Harness-specific inputs frozen at selection, independent of the source's
/// inactive memento instructions and allocations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionDefinition {
    pub settings: playtime_protocol::api::CompactionSettings,
    pub protocol: String,
    pub inventory: String,
}
impl CompactionDefinition {
    /// Defaults installed when a recipe switches harness. The protocol ends with
    /// the recipe's own run limits and the inventory with its workshop capacity,
    /// so the frozen text is exactly what the model receives.
    pub fn new(recipe: &Recipe) -> Self {
        let l = &recipe.parameters.limits;
        let w = &recipe.parameters.workshop;
        Self {
            settings: Default::default(),
            protocol: format!(
                "{}\nThis run lasts at most {} or {} generated tokens, whichever comes first. Calling finish_run ends it early; unused time and tokens are not carried over.\n",
                include_str!("continuous-protocol-v1.txt"),
                duration(l.wall_seconds.get()),
                grouped(l.generated_tokens.get()),
            ),
            inventory: format!(
                "{}\nYour workspace /work holds {} and {} files and directories; /tmp holds {}. Both last until the run ends.\n",
                include_str!("continuous-inventory-v1.txt"),
                binary_size(w.workspace_bytes.get()),
                recipe.workspace_entries,
                binary_size(w.tmp_bytes.get()),
            ),
        }
    }
}
/// The largest whole unit of time, such as 30 minutes, or exact seconds.
fn duration(seconds: u64) -> String {
    let (count, unit) = [(3600, "hour"), (60, "minute")]
        .into_iter()
        .find(|&(size, _)| seconds >= size && seconds.is_multiple_of(size))
        .map_or((seconds, "second"), |(size, unit)| (seconds / size, unit));
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}
/// A count with thousands separators, such as 300,000.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut text = String::new();
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}
/// The largest whole binary unit, such as 16 MiB, or exact bytes.
fn binary_size(bytes: u64) -> String {
    for (unit, shift) in [("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        if bytes >= 1 << shift && bytes.is_multiple_of(1 << shift) {
            return format!("{} {unit}", bytes >> shift);
        }
    }
    format!("{bytes} bytes")
}
impl FrozenRecipe {
    pub fn current(recipe: Recipe) -> Result<Self> {
        let value = Self {
            compaction: None,
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
        if !(matches!(
            (self.schema_version, self.model_registration.is_some()),
            (1, false) | (2, true)
        ) && self.compaction.is_none()
            || self.schema_version == 3 && self.compaction.is_some())
            || self.harness.is_empty()
            || !self.model_lock.is_object()
            || !self.environment_lock.is_object()
        {
            return Err(Error::Invalid("unsupported recipe definition".into()));
        }
        if let Some(c) = &self.compaction {
            if self.harness != CONTINUOUS_COMPACTION_V1
                || c.settings.validation_error().is_some()
                || [&c.protocol, &c.inventory]
                    .iter()
                    .any(|s| s.trim().is_empty() || s.len() > 65536)
            {
                return Err(Error::Invalid("invalid compaction definition".into()));
            }
        } else if self.harness == CONTINUOUS_COMPACTION_V1 {
            return Err(Error::Invalid("missing compaction definition".into()));
        }
        if let Some(model) = &self.model_registration {
            if model.schema_version != 1
                || !model.summary.identity.valid()
                || !model.definition.is_object()
                || self.model_capabilities != model.summary.identity.reviewed_capabilities
            {
                return Err(Error::Invalid("invalid retained model registration".into()));
            }
            self.recipe
                .check_for_harness(true, self.compaction.is_some())?;
        } else {
            self.recipe
                .check_for_harness(false, self.compaction.is_some())?;
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
        if !matches!(
            self.harness.as_str(),
            MEMENTO_TURNS_V1 | CONTINUOUS_COMPACTION_V1
        ) || self.environment_lock != serde_json::from_slice::<Value>(IMAGE_LOCK)?
        {
            return Err(Error::Invalid(
                "recipe execution identities are unavailable".into(),
            ));
        }
        if let Some(model) = &self.model_registration {
            super::registered_model::execution_model(model)?;
            self.check_model_settings(&self.model_settings()?)?;
            return self.check_initial_context();
        }
        if self.model_lock != serde_json::from_slice::<Value>(MODEL_LOCK)? {
            return Err(Error::Invalid(
                "recipe execution identities are unavailable".into(),
            ));
        }
        if let Some(capabilities) = &self.model_capabilities
            && *capabilities != self.resolve_model_capabilities()?
        {
            return Err(Error::Invalid("unsupported model capabilities".into()));
        }
        if self.compaction.is_some() {
            self.check_initial_context()?;
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
