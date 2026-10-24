//! Registration selection changes only the unsaved draft.
use crate::authoring::Draft;
use playtime_protocol::api::ModelRegistrationSummary;

pub fn draw(
    ui: &mut egui::Ui,
    draft: &mut Draft,
    original_name: Option<&str>,
    choices: &[ModelRegistrationSummary],
    busy: bool,
) {
    ui.separator();
    ui.strong("Recipe model");
    let selected = draft.model_registration.as_ref();
    let name = selected
        .map(|m| m.identity.name.as_str())
        .or(original_name)
        .unwrap_or("Model identity unavailable");
    let mut choice = None;
    ui.add_enabled_ui(!busy, |ui| {
        egui::ComboBox::from_label("Registered model")
            .selected_text(name)
            .width(280.0)
            .height(220.0)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .show_ui(ui, |ui| {
                for model in choices {
                    let current =
                        selected.is_some_and(|s| s.registration_id == model.registration_id);
                    let button = ui
                        .add(
                            egui::Button::new(&model.identity.name)
                                .selected(current)
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        )
                        .on_hover_text(&model.identity.name);
                    button.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("Choose registration {}", model.registration_id),
                        )
                    });
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(model.registration_id.as_str()).small(),
                        )
                        .truncate(),
                    );
                    ui.add_space(6.0);
                    if button.clicked() {
                        choice = Some(model.clone());
                        ui.close();
                    }
                }
                if choices.is_empty() {
                    ui.label("No registered models. Use Models to register an inspected identity.");
                }
            });
    });
    if let Some(choice) = choice {
        draft.select_model(choice);
    }
    if let Some(model) = &draft.model_registration {
        ui.add(egui::Label::new(format!("Registration: {}", model.registration_id)).wrap());
        ui.add(egui::Label::new(format!("Digest: {}", model.identity.digest)).wrap());
        ui.add(
            egui::Label::new(format!(
                "Capability evidence: {}",
                model.identity.capability_provenance
            ))
            .wrap(),
        );
        ui.label(
            "This service can save the selection; execution of registered models is unavailable.",
        );
    }
    if draft.model_registration != draft.original.model_registration {
        let original = original_name.unwrap_or("original model");
        let current = draft
            .model_registration
            .as_ref()
            .map(|m| m.identity.name.as_str())
            .unwrap_or(original);
        ui.label(format!("Model changed from {original} to {current}"));
        ui.small("Reasoning and numeric settings are preserved. Correct any unsupported values before saving.");
    }
    if ui
        .add_enabled(!busy, egui::Button::new("Reset model"))
        .clicked()
    {
        draft.reset_model();
    }
}
