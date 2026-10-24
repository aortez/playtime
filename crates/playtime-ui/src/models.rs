//! Prepared read-only model browser. Opening it never changes a recipe.
use crate::model::{Action, Model};
use crate::transcript::excerpt;
use playtime_protocol::api::*;
#[derive(Clone, Debug)]
pub struct InventoryView {
    pub value: ModelInventory,
    pub rows: Vec<(String, String)>,
}
impl InventoryView {
    pub(crate) fn conflicts_with(&self, inspection: &ModelInspection) -> bool {
        if self.value.backend_version != inspection.backend_version {
            return true;
        }
        let cached = self.value.models.iter().find(|m| m.name == inspection.name);
        match (cached, inspection.status) {
            (None, ModelMetadataStatus::Missing) => false,
            (Some(_), ModelMetadataStatus::Missing) | (None, _) => true,
            (Some(model), _) => {
                model.digest != inspection.digest
                    || (model.status == ModelMetadataStatus::Rejected)
                        != (inspection.status == ModelMetadataStatus::Rejected)
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct InspectionView {
    pub review: Option<Box<ModelRegistrationRequest>>,
    pub value: ModelInspection,
    pub lines: Vec<String>,
}
#[derive(Default)]
pub struct Browser {
    pub enabled: bool,
    pub open: bool,
    pub pending: bool,
    pub stale: bool,
    pub inventory: Option<InventoryView>,
    pub inspection: Option<InspectionView>,
    pub selection: Option<ModelInspectRequest>,
    pub notice: String,
}
pub fn inventory(value: ModelInventory) -> InventoryView {
    let rows = value
        .models
        .iter()
        .map(|m| {
            let loaded = match m.loaded {
                ModelLoaded::Loaded => "loaded",
                ModelLoaded::NotLoaded => "not loaded",
                ModelLoaded::Unknown => "loaded state unknown",
            };
            let aliases = value
                .models
                .iter()
                .filter(|other| {
                    m.digest.is_some() && other.digest == m.digest && other.name != m.name
                })
                .map(|other| other.name.as_str())
                .take(4)
                .collect::<Vec<_>>()
                .join(", ");
            (
                m.name.clone(),
                format!(
                    "Installed · {loaded}{}{}",
                    if m.status == ModelMetadataStatus::Rejected {
                        " · rejected"
                    } else {
                        ""
                    },
                    if aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" · same digest: {}", excerpt(&aliases, 200))
                    }
                ),
            )
        })
        .collect();
    InventoryView { value, rows }
}
pub fn inspection(value: ModelInspection) -> InspectionView {
    let status = match value.status {
        ModelMetadataStatus::Candidate => "Identity candidate · not registered",
        ModelMetadataStatus::Unverified => "Identity unverified",
        ModelMetadataStatus::Rejected => "Unsupported model",
        ModelMetadataStatus::Missing => "Model missing",
        ModelMetadataStatus::Changed => "Identity changed",
    };
    let mut lines = vec![
        status.into(),
        format!("Observed {}", value.observed_at.as_str()),
        format!("Backend: {}", value.backend_version),
        format!(
            "Digest: {}",
            value.digest.as_deref().unwrap_or("unavailable")
        ),
        format!(
            "Definition fingerprint: {}",
            value.definition_sha256.as_deref().unwrap_or("unavailable")
        ),
        format!(
            "Advertised capabilities: {}",
            if value.advertised_capabilities.is_empty() {
                "unknown".into()
            } else {
                value.advertised_capabilities.join(", ")
            }
        ),
        format!("Capability evidence: {}", value.capability_provenance),
    ];
    if let Some(c) = &value.reviewed_capabilities {
        lines.push(format!(
            "Reviewed reasoning choices: {}",
            c.reasoning
                .iter()
                .map(|r| match r {
                    RecipeReasoning::ModelDefault => "Model default (omitted)",
                    RecipeReasoning::Low => "Low",
                    RecipeReasoning::Medium => "Medium",
                    RecipeReasoning::High => "High",
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
        lines.push(format!(
            "Reviewed context ceiling: {}",
            c.context_ceiling
                .map(|n| format!("{n} tokens"))
                .unwrap_or_else(|| "unknown".into())
        ));
    } else {
        lines.push("Reasoning choices and context support: unreviewed / unknown".into());
    }
    if let Some(reason) = &value.reason {
        lines.push(reason.clone());
    }
    InspectionView {
        value,
        lines,
        review: None,
    }
}
pub fn review(value: ModelRegistrationReview) -> InspectionView {
    let mut view = inspection(value.inspection);
    if view.value.status == ModelMetadataStatus::Candidate {
        view.lines[0] = "Identity candidate".into();
    }
    view.review = value.request.map(Box::new);
    view
}
impl Model {
    pub fn browse_models(&mut self) {
        self.models.open = true;
        if self.models.selection.is_none() {
            let name = self
                .authoring
                .definition
                .as_ref()
                .and_then(|d| d.model_name.clone())
                .or_else(|| self.preview.as_ref().and_then(|p| p.model_name.clone()));
            self.models.selection = name.map(|name| ModelInspectRequest {
                name,
                expected_digest: None,
            });
        }
        if self.registrations.enabled {
            self.registrations.show_saved = !self.models.enabled;
            self.refresh_registrations();
            self.command(Action::ModelRegistrationReceipts);
        }
        self.refresh_models();
    }
    pub fn refresh_models(&mut self) {
        if !self.models.enabled {
            return;
        }
        self.tag.models += 1;
        self.models.pending = true;
        self.models.stale = true;
        self.models.inspection = None;
        self.models.notice = "Reading installed model metadata…".into();
        self.command(Action::ModelInventory);
    }
    pub fn inspect_model(&mut self, request: ModelInspectRequest) {
        if self.models.pending {
            return;
        }
        self.tag.models += 1;
        self.models.selection = Some(request.clone());
        self.models.inspection = None;
        self.models.pending = true;
        self.models.notice = "Inspecting selected model metadata…".into();
        self.command(Action::InspectModel(request));
    }
    pub fn close_models(&mut self) {
        self.models.open = false;
        self.models.pending = false;
        self.models.stale = true;
        self.tag.models += 1;
        self.command(Action::StopModelDiscovery);
    }
}
pub fn draw(ui: &egui::Ui, model: &mut Model) {
    if !model.models.open {
        return;
    }
    let mut open = true;
    let max_size =
        (ui.ctx().content_rect().size() - egui::vec2(32.0, 120.0)).max(egui::vec2(260.0, 260.0));
    egui::Window::new(if model.registrations.enabled { "Ollama models" } else { "Installed Ollama models" })
        .open(&mut open)
        .default_size([820.0, 620.0])
        .max_size(max_size)
        .min_width(max_size.x.min(600.0))
        .vscroll(true)
        .show(ui.ctx(), |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            if model.registrations.enabled {
                crate::registrations::toolbar(ui,model);
                if model.registrations.show_saved {crate::registrations::saved(ui,model);return;}
                ui.label("Inspect an installed model and explicitly register its reviewed identity.");
            } else {
                ui.label("Browse metadata only. Registration and recipe model selection are not available yet.");
            }
            ui.label("Loaded state is advisory; it does not establish GPU fit.");
            if ui.add_enabled(model.models.enabled, egui::Button::new("Refresh installed models")).clicked() {
                model.refresh_models();
            }
            if model.models.stale {
                ui.colored_label(egui::Color32::YELLOW, "Cached metadata · stale until refreshed");
            }
            ui.label(&model.models.notice);
            if let Some(selection) = &model.models.selection {
                ui.add(egui::Label::new(format!("Selected request name: {}", selection.name)).wrap());
                if model.models.inventory.as_ref().is_some_and(|v| !v.value.models.iter().any(|m| m.name == selection.name)) {
                    ui.label("Selected model is missing from this inventory; no fallback selected.");
                }
            }
            let mut selected = None;
            if let Some(inventory) = &model.models.inventory {
                ui.small(format!("Inventory observed {} · Ollama {}", inventory.value.observed_at.as_str(), inventory.value.backend_version));
                if let Some(warning) = &inventory.value.loaded_warning {
                    ui.label(warning);
                }
                if inventory.rows.is_empty() {
                    ui.label("No installed models in this inventory.");
                }
                egui::ScrollArea::vertical()
                    .id_salt("model-list")
                    .max_height(170.0)
                    .show_rows(ui, 46.0, inventory.rows.len(), |ui, range| {
                        for i in range {
                            ui.scope(|ui| {
                                ui.set_min_height(46.0);
                                let (name, summary) = &inventory.rows[i];
                                let button = ui.add_enabled(
                                    !model.models.pending,
                                    egui::Button::new(name)
                                        .selected(model.models.selection.as_ref().is_some_and(|s| &s.name == name))
                                        .wrap_mode(egui::TextWrapMode::Truncate),
                                ).on_hover_text(name);
                                button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Inspect model {name}")));
                                if button.clicked() {
                                    selected = Some(ModelInspectRequest {
                                        name: name.clone(),
                                        expected_digest: inventory.value.models[i].digest.clone(),
                                    });
                                }
                                ui.add(egui::Label::new(egui::RichText::new(summary).small()).truncate()).on_hover_text(summary);
                            });
                        }
                    });
            }
            if let Some(request) = selected {
                model.inspect_model(request);
            }
            if let Some(detail) = &model.models.inspection {
                ui.separator();
                ui.heading(&detail.value.name);
                egui::ScrollArea::vertical()
                    .id_salt("model-detail")
                    .auto_shrink([!model.registrations.enabled, true])
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for line in &detail.lines {
                            ui.add(egui::Label::new(line).wrap());
                        }
                    });
            }
            if model.registrations.enabled {crate::registrations::review(ui,model);}
        });
    if !open {
        model.close_models();
    }
}
