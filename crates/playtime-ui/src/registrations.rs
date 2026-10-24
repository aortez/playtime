//! Explicit registration UI. Receipts and immutable definitions are prepared off-thread.
use crate::{
    model::{Action, Model, Mutation},
    transcript::SourcePage,
};
use playtime_client::registrations::ModelRegistrationReceipt;
use playtime_protocol::api::*;
use std::sync::Arc;
#[derive(Default)]
pub struct Registrations {
    pub enabled: bool,
    pub show_saved: bool,
    pub loading: bool,
    pub reading: bool,
    pub receipts_ready: bool,
    pub items: Vec<ModelRegistrationSummary>,
    pub selected: Option<ModelRegistrationId>,
    pub definition: Option<RegistrationView>,
    pub receipts: Vec<ModelRegistrationReceipt>,
    pub mutation: Mutation,
    pub notice: String,
    pub list_error: String,
    pub read_error: String,
    pub receipt_error: String,
}
#[derive(Clone, Debug)]
pub struct RegistrationView {
    pub value: Arc<RegisteredModel>,
    pub source: SourcePage,
}
pub fn prepare(value: Arc<RegisteredModel>, offset: usize) -> RegistrationView {
    let text = serde_json::to_string_pretty(&*value).expect("registered definition JSON");
    RegistrationView {
        value,
        source: crate::preview::source_page(&text, offset),
    }
}
impl Registrations {
    pub fn busy(&self) -> bool {
        matches!(self.mutation, Mutation::Recording | Mutation::Submitting)
    }
    pub fn unresolved(&self) -> bool {
        !self.receipts_ready
            || self.busy()
            || matches!(self.mutation, Mutation::Unresolved(_))
            || !self.receipts.is_empty()
    }
}
impl Model {
    pub fn refresh_registrations(&mut self) {
        if !self.registrations.enabled {
            return;
        }
        self.tag.registry_list += 1;
        self.registrations.loading = true;
        self.command(Action::ModelRegistrations);
    }
    pub fn select_registration(&mut self, id: ModelRegistrationId) {
        self.tag.registration_read += 1;
        self.registrations.selected = Some(id.clone());
        self.registrations.reading = true;
        self.registrations.definition = None;
        self.command(Action::ModelRegistration(id));
    }
    pub fn register_inspected_model(&mut self) {
        if !self.registrations.enabled || self.registrations.unresolved() {
            return;
        }
        if let Some(request) = self
            .models
            .inspection
            .as_ref()
            .and_then(|i| i.review.as_deref().cloned())
        {
            self.command(Action::RegisterModel(request));
        }
    }
}
pub fn toolbar(ui: &mut egui::Ui, model: &mut Model) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                model.models.enabled,
                egui::Button::new("Installed models").selected(!model.registrations.show_saved),
            )
            .clicked()
        {
            model.registrations.show_saved = false;
        }
        if ui
            .selectable_label(model.registrations.show_saved, "Registered models")
            .clicked()
        {
            model.registrations.show_saved = true;
        }
    });
    if !model.registrations.notice.is_empty() {
        ui.add(egui::Label::new(&model.registrations.notice).wrap());
    }
    for error in [
        &model.registrations.list_error,
        &model.registrations.read_error,
        &model.registrations.receipt_error,
    ] {
        if !error.is_empty() {
            ui.add(egui::Label::new(error).wrap());
        }
    }
    match &model.registrations.mutation {
        Mutation::Recording => {
            ui.label("Recording registration request…");
        }
        Mutation::Submitting => {
            ui.label("Registering model or recovering the original request…");
        }
        Mutation::Unresolved(error) | Mutation::Rejected(error) => {
            ui.add(egui::Label::new(error).wrap());
        }
        _ => (),
    }
    if (!model.registrations.receipts_ready
        || matches!(model.registrations.mutation, Mutation::Unresolved(_)))
        && ui.button("Refresh registration receipts").clicked()
    {
        model.command(Action::ModelRegistrationReceipts);
    }
    if !model.registrations.receipts.is_empty() {
        ui.collapsing("Unresolved model registrations", |ui| {
            ui.label("Look up or retry the original request before registering again.");
            let mut action = None;
            egui::ScrollArea::vertical()
                .id_salt("registration-receipts")
                .max_height(180.0)
                .show_rows(ui, 66.0, model.registrations.receipts.len(), |ui, range| {
                    for i in range {
                        let receipt = &model.registrations.receipts[i];
                        ui.push_id(&receipt.pending.key, |ui| {
                            ui.add(
                                egui::Label::new(format!(
                                    "{} · {}",
                                    receipt.pending.request.identity.name, receipt.pending.key
                                ))
                                .truncate(),
                            );
                            ui.horizontal(|ui| {
                                let enabled = !model.registrations.busy()
                                    && model.service.as_ref() == Some(&receipt.pending.service_id);
                                if ui
                                    .add_enabled(enabled, egui::Button::new("Look up registration"))
                                    .clicked()
                                {
                                    action = Some(Action::RecoverModelRegistration(
                                        receipt.pending.key.clone(),
                                        false,
                                    ));
                                }
                                if ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::new("Retry same registration"),
                                    )
                                    .clicked()
                                {
                                    action = Some(Action::RecoverModelRegistration(
                                        receipt.pending.key.clone(),
                                        true,
                                    ));
                                }
                            });
                            ui.add_space(10.0);
                        });
                    }
                });
            if let Some(action) = action {
                model.command(action);
            }
        });
    }
}
pub fn review(ui: &mut egui::Ui, model: &mut Model) {
    let eligible = model
        .models
        .inspection
        .as_ref()
        .is_some_and(|i| i.review.is_some());
    ui.label("Registration saves this reviewed identity. Recipe saving and launching are separate actions.");
    if ui
        .add_enabled(
            eligible && !model.registrations.unresolved(),
            egui::Button::new("Register model"),
        )
        .clicked()
    {
        model.register_inspected_model();
    }
    if !eligible {
        ui.label("Inspect a candidate with complete local identity before registering.");
    }
}
pub fn saved(ui: &mut egui::Ui, model: &mut Model) {
    ui.label("Immutable registrations remain readable while Ollama is unavailable. Registration does not certify harness compatibility.");
    if ui.button("Refresh registered models").clicked() {
        model.refresh_registrations();
    }
    if model.registrations.loading {
        ui.label("Reading registered models…");
    }
    if model.registrations.items.is_empty() && !model.registrations.loading {
        ui.label("No registered models.");
    }
    let mut selected = None;
    egui::ScrollArea::vertical()
        .id_salt("registered-models")
        .max_height(150.0)
        .show_rows(ui, 46.0, model.registrations.items.len(), |ui, range| {
            for i in range {
                let item = &model.registrations.items[i];
                ui.push_id(&item.registration_id, |ui| {
                    let button = ui.add(
                        egui::Button::new(&item.identity.name)
                            .selected(
                                model.registrations.selected.as_ref()
                                    == Some(&item.registration_id),
                            )
                            .wrap_mode(egui::TextWrapMode::Truncate),
                    );
                    button.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("Open registration {}", item.registration_id),
                        )
                    });
                    if button.clicked() {
                        selected = Some(item.registration_id.clone());
                    }
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(item.registration_id.as_str()).small(),
                        )
                        .truncate(),
                    );
                    ui.add_space(8.0);
                });
            }
        });
    if let Some(id) = selected {
        model.select_registration(id);
    }
    if model.registrations.reading {
        ui.label("Reading frozen model definition…");
    }
    let mut page = None;
    if let Some(view) = &model.registrations.definition {
        ui.separator();
        ui.heading(&view.value.summary.identity.name);
        ui.label(format!(
            "Registered identity: {}",
            view.value.summary.registration_id
        ));
        ui.label(format!("Digest: {}", view.value.summary.identity.digest));
        ui.label(format!(
            "Backend: {}",
            view.value.summary.identity.backend_version
        ));
        ui.label(format!(
            "Capability evidence: {}",
            view.value.summary.identity.capability_provenance
        ));
        ui.collapsing("Frozen identity and canonical definition", |ui| {
            let end = view.source.start + view.source.text.len();
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Bytes {}–{} of {}",
                    view.source.start, end, view.source.total
                ));
                if ui
                    .add_enabled(
                        !model.registrations.reading && view.source.start > 0,
                        egui::Button::new("Previous definition page"),
                    )
                    .clicked()
                {
                    page = Some((
                        view.value.clone(),
                        view.source
                            .start
                            .saturating_sub(crate::preview::SOURCE_BYTES),
                    ));
                }
                if ui
                    .add_enabled(
                        !model.registrations.reading && end < view.source.total,
                        egui::Button::new("Next definition page"),
                    )
                    .clicked()
                {
                    page = Some((view.value.clone(), end));
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("registered-source")
                .max_height(260.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&view.source.text).monospace()).wrap(),
                    );
                });
        });
    }
    if let Some((value, offset)) = page {
        model.tag.registration_read += 1;
        model.registrations.reading = true;
        model.command(Action::ModelRegistrationPage(value, offset));
    }
}
