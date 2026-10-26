//! Prepared harness readback; rendering never interprets retained JSON.
use crate::{
    authoring::Draft,
    progress,
    transcript::{excerpt, thousands},
};
use playtime_protocol::api::*;

#[derive(Clone, Debug)]
pub struct RecipeHarness {
    pub id_label: String,
    pub view: RecipeHarnessView,
    pub catalog: HarnessCatalog,
    pub lines: Vec<String>,
}
impl RecipeHarness {
    pub fn new(view: RecipeHarnessView, catalog: HarnessCatalog) -> Self {
        let lines = lines(&view.harness);
        Self {
            id_label: excerpt(&view.harness.id, 256),
            view,
            catalog,
            lines,
        }
    }
}
pub fn lines(view: &HarnessView) -> Vec<String> {
    let id = excerpt(&view.id, 256);
    let mut lines = vec![match view.version {
        Some(version) => format!("Harness: {id} · version {version}"),
        None => format!("Harness: {id} · unsupported"),
    }];
    if let Some(s) = &view.settings {
        lines.extend([
            format!(
                "{} sessions · {} work turns per session",
                thousands(s.sessions.into()),
                thousands(s.work_turns_per_session.into())
            ),
            format!(
                "{} · {} memento bytes",
                progress::memory_words(s.memory_condition),
                thousands(s.memento_bytes)
            ),
            format!(
                "{} handoff tokens · {} handoff attempts",
                thousands(s.handoff_tokens),
                s.handoff_attempts
            ),
            format!(
                "Tool parse policy: {} consecutive retries · {} errors per session",
                s.tool_parse_retries, s.tool_parse_error_limit
            ),
        ]);
    } else if let Some(s) = &view.compaction {
        lines.extend([
            format!(
                "Compact at {}% context · retain {} complete exchanges",
                s.trigger_percent, s.recent_exchanges
            ),
            format!(
                "Summary cap: {} tokens · shared model and run budget",
                thousands(s.summary_tokens)
            ),
            "One continuous workshop · final workspace capture only".into(),
        ]);
    } else {
        lines.push("This service cannot execute this retained harness. Its recorded identity has been preserved.".into());
    }
    lines
}
pub fn draw(ui: &mut egui::Ui, harness: &RecipeHarness, draft: Option<&mut Draft>, busy: bool) {
    let editing_continuous = draft.as_ref().is_some_and(|d| d.continuous());
    if editing_continuous {
        ui.label(format!("Source harness: {}", harness.id_label));
    } else {
        ui.label(&harness.lines[0]);
    }
    if let Some(draft) = draft {
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_label("Harness")
                .selected_text(
                    draft
                        .harness
                        .as_ref()
                        .map_or(harness.id_label.as_str(), |id| id.as_str()),
                )
                .show_ui(ui, |ui| {
                    for choice in &harness.catalog.harnesses {
                        ui.selectable_value(
                            &mut draft.harness,
                            Some(choice.id.clone()),
                            format!("{} · v{}", choice.name, choice.version),
                        );
                    }
                });
            if draft.harness.is_some() && ui.small_button("Inherit source harness").clicked() {
                draft.harness = None;
            }
            if draft.continuous() {
                draft.compaction.draw(ui);
            }
        });
    }
    if !editing_continuous {
        ui.collapsing("Inherited harness settings (read only)", |ui| {
            for line in &harness.lines[1..] {
                ui.label(line);
            }
            ui.small("Preview shows the effective settings of the saved revision.");
        });
    }
    ui.collapsing("Harness requirements", |ui| {
        for h in &harness.catalog.harnesses {
            ui.label(format!("{} · version {}", h.name, h.version));
            ui.label(format!(
                "Model: {}",
                h.required_model_capabilities.join(", ")
            ));
            ui.label(format!("Workshop image: {}", h.required_workshop_image));
        }
        ui.small("Installed support. Preview checks whether execution is ready.");
    });
}
