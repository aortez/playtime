//! Form state contains prepared values; rendering performs no I/O or JSON work.
use playtime_protocol::api::*;

#[derive(Clone, Debug)]
pub struct SettingsDraft {
    pub continuous: bool,
    original: RecipeSettingsView,
    capabilities: Option<RecipeModelCapabilities>,
    model_error: Option<String>,
    reasoning: RecipeReasoning,
    fields: Vec<Field>,
}
#[derive(Clone, Debug)]
struct Field {
    label: &'static str,
    original: String,
    text: String,
}
impl SettingsDraft {
    pub fn new(view: RecipeSettingsView) -> Self {
        let s = &view.settings;
        let fields = [
            ("Temperature", s.temperature.to_string()),
            ("Context capacity (tokens)", s.context_tokens.to_string()),
            ("Response limit (tokens)", s.tokens_per_call.to_string()),
            (
                "Memento handoff limit (tokens)",
                s.handoff_tokens.to_string(),
            ),
            ("Top-p", s.top_p.to_string()),
            ("Top-k", s.top_k.to_string()),
            ("Repeat penalty", s.repeat_penalty.to_string()),
            ("Sampling seed (number)", s.sampling_seed.to_string()),
        ]
        .into_iter()
        .map(|(label, text)| Field {
            label,
            original: text.clone(),
            text,
        })
        .collect();
        Self {
            continuous: false,
            capabilities: Some(view.capabilities.clone()),
            model_error: None,
            reasoning: s.reasoning,
            fields,
            original: view,
        }
    }
    pub fn value(&self) -> Result<RecipeModelSettings, String> {
        fn number<T: std::str::FromStr>(field: &Field) -> Result<T, String> {
            field
                .text
                .parse()
                .map_err(|_| format!("Enter a valid number for {}.", field.label))
        }
        let f = &self.fields;
        let s = RecipeModelSettings {
            reasoning: self.reasoning,
            temperature: number(&f[0])?,
            context_tokens: number(&f[1])?,
            tokens_per_call: number(&f[2])?,
            // Continuous recipes hide the handoff field and keep its saved value.
            handoff_tokens: if self.continuous {
                self.original.settings.handoff_tokens
            } else {
                number(&f[3])?
            },
            top_p: number(&f[4])?,
            top_k: number(&f[5])?,
            repeat_penalty: number(&f[6])?,
            sampling_seed: number(&f[7])?,
        };
        if let Some(error) = s.validation_error() {
            return Err(error.into());
        }
        if let Some(error) = &self.model_error {
            return Err(error.clone());
        }
        let capabilities = self
            .capabilities
            .as_ref()
            .ok_or("This model's reasoning/context support is unreviewed.")?;
        if !capabilities.reasoning.contains(&s.reasoning) {
            return Err("This reasoning choice is unsupported by the selected model.".into());
        }
        if capabilities
            .context_ceiling
            .is_some_and(|n| s.context_tokens > n)
        {
            return Err("Context capacity exceeds the selected model's reported ceiling.".into());
        }
        if !self.continuous && s.handoff_tokens >= self.original.generated_tokens {
            return Err(
                "The whole-run generation budget must leave room beyond the handoff limit.".into(),
            );
        }
        Ok(s)
    }
    pub fn select_model(&mut self, model: Option<&ModelRegistrationSummary>) {
        if let Some(model) = model {
            self.capabilities = model.identity.reviewed_capabilities.clone();
            self.model_error = if !["completion", "tools"].iter().all(|required| {
                model
                    .identity
                    .advertised_capabilities
                    .iter()
                    .any(|c| c == required)
            }) {
                Some("This harness requires a model with completion and tool support.".into())
            } else if self
                .capabilities
                .as_ref()
                .is_some_and(|c| c.context_ceiling.is_none())
            {
                Some("This model's context capacity is unreviewed.".into())
            } else {
                None
            };
        } else {
            self.capabilities = Some(self.original.capabilities.clone());
            self.model_error = None;
        }
    }
    pub fn reset(&mut self) {
        let continuous = self.continuous;
        *self = Self::new(self.original.clone());
        self.continuous = continuous;
    }
    /// The owner resets model selection together with settings when this returns true.
    pub fn draw(&mut self, ui: &mut egui::Ui) -> bool {
        let mut reset = false;
        ui.separator();
        ui.strong("Model settings");
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_label("Reasoning")
                .selected_text(reasoning_label(self.reasoning))
                .show_ui(ui, |ui| {
                    if let Some(capabilities) = &self.capabilities {
                        for choice in &capabilities.reasoning {
                            ui.selectable_value(
                                &mut self.reasoning,
                                *choice,
                                reasoning_label(*choice),
                            );
                        }
                    }
                });
            if ui.button("Reset reasoning").clicked() {
                self.reasoning = self.original.settings.reasoning;
            }
        });
        if self.reasoning != self.original.settings.reasoning {
            ui.label(format!(
                "Reasoning: {} to {}",
                reasoning_label(self.original.settings.reasoning),
                reasoning_label(self.reasoning)
            ));
        }
        if let Err(error) = self.value() {
            ui.colored_label(egui::Color32::YELLOW, error);
        }
        if self.reasoning == RecipeReasoning::ModelDefault {
            if let Some(default) = self.capabilities.as_ref().and_then(|c| c.default_reasoning) {
                ui.small(format!(
                    "Omit the reasoning setting; reported model default: {}.",
                    reasoning_label(default)
                ));
            } else {
                ui.small("Omit the reasoning setting; use the locked model/backend default. The exact default is not advertised by this capability record.");
            }
        }
        let visible = if self.continuous { 3 } else { 4 };
        for field in &mut self.fields[..visible] {
            field.draw(ui);
        }
        ui.small(if self.continuous { "Context holds the conversation and the next response. Response limits cap generation per call, including reasoning where reported." } else { "Context holds the conversation and the next response. Response limits cap generation per call, including reasoning where reported. Memento handoffs have their own cap." });
        ui.label(format!(
            "Whole-run generation budget: {} tokens (inherited)",
            self.original.generated_tokens
        ));
        ui.small("The service checks conservative context fit when saving. This does not predict exact tokenization or GPU memory fit.");
        ui.collapsing("Advanced model settings", |ui| {
            for field in &mut self.fields[4..] { field.draw(ui); }
            ui.small("The controller advances the numeric sampling seed per request. Matching seeds do not guarantee identical results. Seed files are separate inputs.");
        });
        if ui.button("Reset model settings").clicked() {
            reset = true;
        }
        reset
    }
}
impl Field {
    fn draw(&mut self, ui: &mut egui::Ui) {
        ui.push_id(self.label, |ui| {
            let label = ui.label(self.label);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.text)
                        // Validation text above the fields changes layout while typing.
                        // Keep focus on this field through those changes.
                        .id(egui::Id::new(("recipe-model-setting", self.label)))
                        .desired_width(150.0)
                        .char_limit(32),
                )
                .labelled_by(label.id);
                if ui.button(format!("Reset {}", self.label)).clicked() {
                    self.text = self.original.clone();
                }
            });
            if self.text != self.original {
                ui.small(format!("{} to {}", self.original, self.text));
            }
        });
    }
}
pub(crate) fn reasoning_label(value: RecipeReasoning) -> &'static str {
    match value {
        RecipeReasoning::ModelDefault => "Model default",
        RecipeReasoning::Low => "Low",
        RecipeReasoning::Medium => "Medium",
        RecipeReasoning::High => "High",
        RecipeReasoning::On => "On",
        RecipeReasoning::Off => "Off",
    }
}
