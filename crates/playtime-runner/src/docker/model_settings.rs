use super::{FrozenRecipe, MODEL_LOCK};
use crate::{Error, Result};
use playtime_protocol::api::*;
use serde_json::Value;
use std::num::NonZeroU64;

impl FrozenRecipe {
    pub fn resolve_model_capabilities(&self) -> Result<RecipeModelCapabilities> {
        if let Some(model) = &self.model_registration {
            return model
                .summary
                .identity
                .reviewed_capabilities
                .clone()
                .ok_or_else(|| {
                    Error::Invalid(
                        "The selected model has no reviewed reasoning/context capabilities.".into(),
                    )
                });
        }
        reviewed_model_capabilities(&self.model_lock)
    }
    pub fn model_settings(&self) -> Result<RecipeModelSettings> {
        let m = &self.recipe.model;
        let p = &self.recipe.parameters;
        Ok(RecipeModelSettings {
            reasoning: match m.thinking.as_str() {
                "default" => RecipeReasoning::ModelDefault,
                "low" => RecipeReasoning::Low,
                "medium" => RecipeReasoning::Medium,
                "high" => RecipeReasoning::High,
                "on" => RecipeReasoning::On,
                "off" => RecipeReasoning::Off,
                _ => return Err(Error::Invalid("unsupported reasoning setting".into())),
            },
            temperature: m.temperature,
            context_tokens: p.limits.context_tokens.get(),
            tokens_per_call: p.limits.tokens_per_call.get(),
            handoff_tokens: p.limits.handoff_tokens.get(),
            top_p: m.top_p,
            top_k: m.top_k,
            repeat_penalty: m.repeat_penalty,
            sampling_seed: p.sampling_seed,
        })
    }
    pub fn settings_view(&self) -> Result<RecipeSettingsView> {
        Ok(RecipeSettingsView {
            execution_recipe_id: RecipeId::new(&self.recipe.id)?,
            definition_digest: self.digest()?,
            settings: self.model_settings()?,
            capabilities: self.resolve_model_capabilities()?,
            generated_tokens: self.recipe.parameters.limits.generated_tokens.get(),
        })
    }
    pub fn apply_model_settings(&mut self, settings: &RecipeModelSettings) -> Result<()> {
        let capabilities = self.check_model_settings(settings)?;
        let m = &mut self.recipe.model;
        m.thinking = match settings.reasoning {
            RecipeReasoning::ModelDefault => "default",
            RecipeReasoning::Low => "low",
            RecipeReasoning::Medium => "medium",
            RecipeReasoning::High => "high",
            RecipeReasoning::On => "on",
            RecipeReasoning::Off => "off",
        }
        .into();
        m.temperature = settings.temperature;
        m.top_p = settings.top_p;
        m.top_k = settings.top_k;
        m.repeat_penalty = settings.repeat_penalty;
        let p = &mut self.recipe.parameters;
        p.sampling_seed = settings.sampling_seed;
        p.limits.context_tokens = NonZeroU64::new(settings.context_tokens).unwrap();
        p.limits.tokens_per_call = NonZeroU64::new(settings.tokens_per_call).unwrap();
        p.limits.handoff_tokens = NonZeroU64::new(settings.handoff_tokens).unwrap();
        self.model_capabilities = Some(capabilities);
        self.validate()?;
        self.check_initial_context()
    }
    pub fn select_registered_model(&mut self, model: RegisteredModel) -> Result<()> {
        self.schema_version = if self.compaction.is_some() { 3 } else { 2 };
        self.model_capabilities = model.summary.identity.reviewed_capabilities.clone();
        self.model_registration = Some(model);
        self.validate()
    }
    /// Called on every new save, including when settings were inherited.
    pub fn check_model_settings(
        &self,
        settings: &RecipeModelSettings,
    ) -> Result<RecipeModelCapabilities> {
        if let Some(error) = settings.validation_error() {
            return Err(Error::Invalid(error.into()));
        }
        if let Some(model) = &self.model_registration
            && !["completion", "tools"].iter().all(|required| {
                model
                    .summary
                    .identity
                    .advertised_capabilities
                    .iter()
                    .any(|c| c == required)
            })
        {
            return Err(Error::Invalid(
                "The selected model must advertise completion and tools for this harness.".into(),
            ));
        }
        let capabilities = self.resolve_model_capabilities()?;
        if self.model_registration.is_some() && capabilities.context_ceiling.is_none() {
            return Err(Error::Invalid(
                "The selected model's context capacity is unreviewed.".into(),
            ));
        }
        if !capabilities.reasoning.contains(&settings.reasoning) {
            return Err(Error::Invalid("Reasoning is unsupported by the selected model. Choose an explicitly supported value; settings were not translated.".into()));
        }
        if capabilities
            .context_ceiling
            .is_some_and(|n| settings.context_tokens > n)
        {
            return Err(Error::Invalid(
                "Context capacity exceeds the selected model's reviewed ceiling.".into(),
            ));
        }
        Ok(capabilities)
    }
    pub fn check_initial_context(&self) -> Result<()> {
        if self.compaction.is_some() {
            playtime_engine::Controller::continuous(self.compaction_config()?, playtime_core::RunId::new("context-check").unwrap(), playtime_core::RunMode::Simulation, playtime_engine::simulation::SequentialIds, 0, playtime_engine::ToolPolicy::Simulation)
                .map_err(|e| Error::Invalid(format!("Compaction settings cannot safely begin work: {}. Increase context capacity or reduce the prompt, response, summary or retained-exchange settings.", e.0)))?;
            return Ok(());
        }
        let required = playtime_engine::SessionInput {
            parameters: &self.recipe.parameters,
            protocol: &self.recipe.protocol,
            prompt: &self.recipe.prompt,
            inventory: &self.recipe.inventory,
        }
        .initial_context_required();
        if required > self.recipe.parameters.limits.context_tokens.get() {
            return Err(Error::Invalid(format!(
                "Context capacity is too small to begin work: the controller estimates at least {required} tokens with these settings. Increase context capacity or reduce the prompt or response limits; the estimate changes with the settings."
            )));
        }
        Ok(())
    }
}

/// Read-only capability evidence, scoped to the complete reviewed lock.
pub fn reviewed_model_capabilities(model_lock: &Value) -> Result<RecipeModelCapabilities> {
    // This reviewed fallback is scoped to the complete lock, including the
    // backend version. Model discovery/registration will extend this resolver.
    let locked: Value = serde_json::from_slice(MODEL_LOCK)?;
    if *model_lock != locked {
        return Err(Error::Invalid(
            "model settings are unavailable for this identity".into(),
        ));
    }
    let info = &locked["definition"]["model_info"];
    let architecture = info["general.architecture"].as_str().unwrap_or("");
    Ok(RecipeModelCapabilities {
        model_digest: locked["digest"].as_str().unwrap().into(),
        backend_version: locked["ollama_version"].as_str().unwrap().into(),
        provenance: "reviewed-gpt-oss-ollama-0.32.3-v1; context ceiling from locked model_info"
            .into(),
        reasoning: vec![
            RecipeReasoning::ModelDefault,
            RecipeReasoning::Low,
            RecipeReasoning::Medium,
            RecipeReasoning::High,
        ],
        default_reasoning: None,
        context_ceiling: info[format!("{architecture}.context_length")]
            .as_u64()
            .filter(|n| *n > 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_preserve_omission_and_capability_evidence_is_checked() {
        let mut recipe = FrozenRecipe::current(super::super::Recipe::reference().unwrap()).unwrap();
        let mut settings = recipe.model_settings().unwrap();
        settings.reasoning = RecipeReasoning::ModelDefault;
        settings.temperature = 0.0;
        settings.sampling_seed = 0;
        recipe.apply_model_settings(&settings).unwrap();
        let wire = super::super::recipe::settings(settings.context_tokens, &recipe.recipe.model);
        assert!(wire["thinking"].is_null());
        assert_eq!(wire["temperature"], 0.0);
        let original = recipe.clone();
        recipe.model_capabilities.as_mut().unwrap().backend_version = "different".into();
        assert!(recipe.validate().is_err());
        recipe = original;
        recipe.model_lock["digest"] = serde_json::json!("another-model");
        recipe.model_capabilities.as_mut().unwrap().model_digest = "another-model".into();
        recipe.validate().unwrap();
        recipe.view(None).unwrap(); // Retained definition is still readable.
        assert!(recipe.resolve_model_capabilities().is_err());
        assert!(recipe.executable().is_err());
    }
    #[test]
    fn boundary_matches_controller_context_guard_and_invalid_values_are_refused() {
        let mut recipe = FrozenRecipe::current(super::super::Recipe::reference().unwrap()).unwrap();
        let settings = recipe.model_settings().unwrap();
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 2.1] {
            let mut s = settings.clone();
            s.temperature = invalid;
            assert!(recipe.apply_model_settings(&s).is_err());
        }
        // Find adjacent capacities where the shared initial-work guard changes.
        let mut first = None;
        for context in 1..65536 {
            recipe.recipe.parameters.limits.context_tokens = NonZeroU64::new(context).unwrap();
            if recipe.check_initial_context().is_ok() {
                first = Some(context);
                break;
            }
        }
        let first = first.unwrap();
        recipe.recipe.parameters.limits.context_tokens = NonZeroU64::new(first - 1).unwrap();
        assert!(recipe.check_initial_context().is_err());
        recipe.recipe.parameters.limits.context_tokens = NonZeroU64::new(first).unwrap();
        assert!(recipe.check_initial_context().is_ok());
        for (capacity, starts_work) in [(first - 1, false), (first, true)] {
            let mut scenario: playtime_engine::simulation::Scenario =
                serde_json::from_slice(super::super::LIVE_SCENARIO).unwrap();
            scenario.mode = playtime_core::RunMode::Simulation;
            scenario.parameters = recipe.recipe.parameters.clone();
            scenario.parameters.limits.context_tokens = NonZeroU64::new(capacity).unwrap();
            scenario.protocol = recipe.recipe.protocol.clone();
            scenario.prompt = recipe.recipe.prompt.clone();
            scenario.inventory = recipe.recipe.inventory.clone();
            scenario.models.clear();
            let run = playtime_engine::simulation::run(scenario).unwrap();
            let request = run.dispatches.iter().find_map(|d| match &d.effect.kind {
                playtime_engine::EffectKind::Model(request) => Some(request),
                _ => None,
            });
            assert_eq!(
                request.is_some_and(|r| r.purpose == playtime_core::RequestPurpose::Work),
                starts_work
            );
        }
    }
}
