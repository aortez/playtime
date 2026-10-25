use super::{FrozenRecipe, IMAGE_LOCK};
use crate::{Error, Result};
use playtime_protocol::api::*;

/// Installed support only; no model, Docker, or host probing.
pub fn harness_catalog() -> Result<HarnessCatalog> {
    let image: serde_json::Value = serde_json::from_slice(IMAGE_LOCK)?;
    Ok(HarnessCatalog {
        schema_version: 1,
        harnesses: vec![HarnessDescriptor {
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
            required_workshop_image: image["image_id"]
                .as_str()
                .ok_or_else(|| Error::Invalid("missing workshop identity".into()))?
                .into(),
        }],
    })
}

impl FrozenRecipe {
    pub fn harness_view(&self) -> Result<RecipeHarnessView> {
        self.validate()?;
        Ok(RecipeHarnessView {
            execution_recipe_id: RecipeId::new(&self.recipe.id)?,
            definition_digest: self.digest()?,
            harness: HarnessView::project(&self.harness, &self.recipe.parameters),
        })
    }
    pub fn select_harness(&mut self, id: &HarnessId) -> Result<()> {
        if id.as_str() != MEMENTO_TURNS_V1 {
            return Err(Error::Invalid(
                "The selected harness is not supported by this service.".into(),
            ));
        }
        self.harness = id.to_string();
        Ok(())
    }
}
