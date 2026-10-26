use super::{CompactionDefinition, FrozenRecipe, IMAGE_LOCK};
use crate::{Error, Result};
use playtime_protocol::api::*;

/// Installed support only; no model, Docker, or host probing.
pub fn harness_catalog() -> Result<HarnessCatalog> {
    let image: serde_json::Value = serde_json::from_slice(IMAGE_LOCK)?;
    let image_id = image["image_id"]
        .as_str()
        .ok_or_else(|| Error::Invalid("missing workshop identity".into()))?;
    Ok(HarnessCatalog {
        schema_version: 1,
        harnesses: vec![
            HarnessDescriptor {
                id: HarnessId::new(MEMENTO_TURNS_V1)?,
                name: "Memento (turn-based)".into(),
                version: 1,
                settings: [
                    "sessions",
                    "work_turns_per_session",
                    "handoff_tokens",
                    "memento_bytes",
                    "memory_condition",
                    "tool_parse_retries",
                    "tool_parse_error_limit",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                required_model_capabilities: vec!["completion".into(), "tools".into()],
                required_workshop_image: image_id.into(),
            },
            HarnessDescriptor {
                id: HarnessId::new(CONTINUOUS_COMPACTION_V1)?,
                name: "Continuous conversation with compaction".into(),
                version: 1,
                settings: [
                    "trigger_percent",
                    "recent_exchanges",
                    "summary_tokens",
                    "instructions",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                required_model_capabilities: vec!["completion".into(), "tools".into()],
                required_workshop_image: image_id.into(),
            },
        ],
    })
}

impl FrozenRecipe {
    pub fn harness_view(&self) -> Result<RecipeHarnessView> {
        self.validate()?;
        Ok(RecipeHarnessView {
            execution_recipe_id: RecipeId::new(&self.recipe.id)?,
            definition_digest: self.digest()?,
            harness: HarnessView::from_definition(&serde_json::to_value(self)?)
                .map_err(|e| Error::Invalid(e.into()))?,
        })
    }
    pub fn select_harness(&mut self, id: &HarnessId) -> Result<()> {
        if !matches!(id.as_str(), MEMENTO_TURNS_V1 | CONTINUOUS_COMPACTION_V1) {
            return Err(Error::Invalid(
                "The selected harness is not supported by this service.".into(),
            ));
        }
        if id.as_str() == CONTINUOUS_COMPACTION_V1 {
            self.compaction
                .get_or_insert_with(|| CompactionDefinition::new(&self.recipe));
            self.schema_version = 3;
        } else {
            self.compaction = None;
            self.schema_version = if self.model_registration.is_some() {
                2
            } else {
                1
            };
        }
        self.harness = id.to_string();
        Ok(())
    }
    pub fn apply_compaction_settings(&mut self, settings: &CompactionSettings) -> Result<()> {
        if let Some(error) = settings.validation_error() {
            return Err(Error::Invalid(error.into()));
        }
        let compaction = self.compaction.as_mut().ok_or_else(|| {
            Error::Invalid("Compaction settings require the continuous compaction harness.".into())
        })?;
        compaction.settings = settings.clone();
        Ok(())
    }
    pub fn compaction_config(&self) -> Result<playtime_engine::compaction::CompactionConfig> {
        let c = self
            .compaction
            .as_ref()
            .ok_or_else(|| Error::Invalid("missing compaction inputs".into()))?;
        let settings = (&c.settings).try_into()?;
        Ok(playtime_engine::compaction::CompactionConfig {
            parameters: self.recipe.parameters.common(),
            protocol: c.protocol.clone(),
            prompt: self.recipe.prompt.clone(),
            inventory: c.inventory.clone(),
            settings,
            initial_workspace: crate::prepare::reference(
                "inputs/empty-workspace.tar",
                &super::Profile::initial_bytes(),
            ),
            initial_files: Default::default(),
        })
    }
}
