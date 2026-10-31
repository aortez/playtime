//! Target selection is presentation state; network work uses captured command scope.
use crate::model::Model;
pub fn draw(ui: &mut egui::Ui, model: &mut Model) {
    if model.targets.is_empty() {
        return;
    }
    let mut selected = model.selected_target.clone();
    ui.horizontal(|ui| {
        ui.label("Inference target");
        egui::ComboBox::from_id_salt("inference-target")
            .selected_text(
                model
                    .targets
                    .iter()
                    .find(|t| Some(&t.id) == selected.as_ref())
                    .map(|t| t.name.as_str())
                    .unwrap_or("Choose a machine"),
            )
            .show_ui(ui, |ui| {
                for target in &model.targets {
                    ui.selectable_value(&mut selected, Some(target.id.clone()), &target.name)
                        .on_hover_text(&target.endpoint);
                }
            });
    });
    if selected != model.selected_target {
        model.select_target(selected);
    }
}
