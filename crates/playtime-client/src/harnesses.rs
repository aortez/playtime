use crate::{Client, Error, Result};
use playtime_protocol::api::*;
use reqwest::Method;

pub(crate) fn validate_preview(
    view: Option<&HarnessView>,
    configuration: &serde_json::Value,
    parameters: &playtime_core::RunParameters,
    advertised: bool,
) -> Result<()> {
    if (advertised || view.is_some())
        && (HarnessView::from_configuration(configuration)
            .map_err(|_| Error::InvalidResponse)?
            .as_ref()
            != view
            || view.is_some_and(|h| HarnessView::project(&h.id, parameters) != *h))
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}

impl Client {
    pub async fn harnesses(&self) -> Result<HarnessCatalog> {
        let catalog: HarnessCatalog = self.json(self.request(Method::GET, "harnesses")).await?;
        if catalog.schema_version != 1
            || catalog.harnesses.len() > HARNESS_CATALOG_LIMIT
            || catalog.harnesses.iter().enumerate().any(|(i, h)| {
                h.name.is_empty()
                    || h.name.len() > 128
                    || h.version == 0
                    || h.settings.len() > 32
                    || h.settings.iter().any(|s| s.is_empty() || s.len() > 128)
                    || h.required_model_capabilities.len() > 16
                    || h.required_model_capabilities
                        .iter()
                        .any(|s| s.is_empty() || s.len() > 128)
                    || h.required_workshop_image.is_empty()
                    || h.required_workshop_image.len() > 128
                    || catalog.harnesses[..i].iter().any(|old| old.id == h.id)
            })
        {
            return Err(Error::InvalidResponse);
        }
        Ok(catalog)
    }
    pub async fn recipe_harness(&self, definition: &RecipeDefinition) -> Result<RecipeHarnessView> {
        let view: RecipeHarnessView = self
            .json(self.request(
                Method::GET,
                &format!("recipes/{}/harness", definition.execution_recipe_id),
            ))
            .await?;
        validate_readback(&view, definition)?;
        Ok(view)
    }
}

fn validate_readback(view: &RecipeHarnessView, definition: &RecipeDefinition) -> Result<()> {
    let id = definition.inherited["harness"]
        .as_str()
        .ok_or(Error::InvalidResponse)?;
    let parameters = serde_json::from_value(definition.inherited["recipe"]["parameters"].clone())
        .map_err(|_| Error::InvalidResponse)?;
    if view.execution_recipe_id != definition.execution_recipe_id
        || view.definition_digest != definition.definition_digest
        || view.harness != HarnessView::project(id, &parameters)
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_cannot_omit_or_contradict_recorded_harness_but_old_services_still_work() {
        let parameters: playtime_core::RunParameters = serde_json::from_str(include_str!(
            "../../playtime-protocol/tests/fixtures/two-session/inputs/configuration.json"
        ))
        .unwrap();
        let config =
            serde_json::json!({"harness":MEMENTO_TURNS_V1,"scenario":{"parameters":parameters}});
        let view = HarnessView::project(MEMENTO_TURNS_V1, &parameters);
        validate_preview(Some(&view), &config, &parameters, true).unwrap();
        assert!(validate_preview(None, &config, &parameters, true).is_err());
        validate_preview(None, &config, &parameters, false).unwrap();
        let mut wrong = view.clone();
        wrong.settings.as_mut().unwrap().sessions += 1;
        assert!(validate_preview(Some(&wrong), &config, &parameters, true).is_err());
        let mut wrong = parameters.clone();
        wrong.limits.sessions = std::num::NonZeroU32::new(1).unwrap();
        assert!(validate_preview(Some(&view), &config, &wrong, true).is_err());
        let legacy = serde_json::to_value(&parameters).unwrap();
        validate_preview(None, &legacy, &parameters, true).unwrap();
        assert!(validate_preview(Some(&view), &legacy, &parameters, false).is_err());
    }
}
