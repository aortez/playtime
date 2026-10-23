//! Recipe editor state and widgets. Definitions are prepared by the backend.
use crate::model::{Action, Model, Mutation};
use playtime_client::recipes::RecipeSaveReceipt;
use playtime_core::RunMode;
use playtime_protocol::api::*;

#[derive(Clone, Debug)]
pub struct DefinitionView {
    pub value: RecipeDefinition,
    pub inherited: String,
}
#[derive(Clone, Debug)]
pub struct Draft {
    pub original: RecipeDefinition,
    pub clone: bool,
    pub name: String,
    pub prompt: String,
}
impl Draft {
    pub fn request(&self) -> RecipeMutation {
        if !self.clone
            && let Some(saved) = &self.original.saved
        {
            RecipeMutation::Revise {
                recipe_id: saved.recipe_id.clone(),
                request: ReviseRecipeRequest {
                    expected_head: self.original.execution_recipe_id.clone(),
                    name: self.name.clone(),
                    prompt: self.prompt.clone(),
                },
            }
        } else {
            RecipeMutation::Clone {
                request: CloneRecipeRequest {
                    source: RecipeSource {
                        execution_recipe_id: self.original.execution_recipe_id.clone(),
                        definition_digest: self.original.definition_digest.clone(),
                    },
                    name: self.name.clone(),
                    prompt: self.prompt.clone(),
                },
            }
        }
    }
    pub fn valid(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= 128
            && !self.name.chars().any(char::is_control)
            && !self.prompt.trim().is_empty()
            && self.prompt.len() <= 65536
    }
}
#[derive(Default)]
pub struct Authoring {
    pub enabled: bool,
    pub definition: Option<DefinitionView>,
    pub draft: Option<Draft>,
    pub save: Mutation,
    pub receipts: Vec<RecipeSaveReceipt>,
    pub last_saved: Option<RecipeId>,
    pub error: String,
}
impl Authoring {
    pub fn pending(&self) -> bool {
        matches!(
            self.save,
            Mutation::Recording | Mutation::Submitting | Mutation::Unresolved(_)
        ) || !self.receipts.is_empty()
    }
}
impl Model {
    pub fn edit_recipe(&mut self, clone: bool) {
        if self.authoring.pending() || self.authoring.draft.is_some() {
            return;
        }
        if let Some(view) = &self.authoring.definition {
            self.authoring.draft = Some(Draft {
                original: view.value.clone(),
                clone,
                name: view.value.name.clone(),
                prompt: view.value.prompt.clone(),
            });
            self.preview = None;
        }
    }
    pub fn save_recipe(&mut self) {
        if self.authoring.pending() {
            return;
        }
        if let Some(draft) = &self.authoring.draft
            && draft.valid()
        {
            self.command(Action::SaveRecipe(draft.request()));
        }
    }
    pub fn discard_recipe_edits(&mut self) {
        if self.authoring.pending() {
            return;
        }
        self.authoring.draft = None;
        if let Some(recipe) = self.recipe.clone() {
            self.choose_recipe(recipe, self.mode);
        }
    }
}

pub fn draw(model: &mut Model, ui: &mut egui::Ui) {
    if !model.authoring.enabled {
        if model.mode == RunMode::Experiment {
            ui.label("This service does not support recipe authoring.");
        }
        return;
    }
    let busy = model.authoring.pending();
    if matches!(model.authoring.save, Mutation::Unresolved(_))
        && ui.button("Refresh save receipts").clicked()
    {
        model.command(Action::RecipeSaveReceipts);
    }
    if !model.authoring.receipts.is_empty() {
        ui.collapsing("Unresolved recipe saves", |ui| {
            ui.label("Look up or retry the original request before saving again.");
            let mut action = None;
            for receipt in &model.authoring.receipts {
                ui.label(format!(
                    "{} · {}",
                    receipt.pending.service_id, receipt.pending.key
                ));
                let enabled = !matches!(
                    model.authoring.save,
                    Mutation::Recording | Mutation::Submitting
                ) && model.service.as_ref() == Some(&receipt.pending.service_id);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(enabled, egui::Button::new("Look up save"))
                        .clicked()
                    {
                        action = Some(Action::RecoverRecipe(receipt.pending.key.clone(), false));
                    }
                    if ui
                        .add_enabled(enabled, egui::Button::new("Retry same save"))
                        .clicked()
                    {
                        action = Some(Action::RecoverRecipe(receipt.pending.key.clone(), true));
                    }
                });
            }
            if let Some(action) = action {
                model.command(action);
            }
        });
    }
    match &model.authoring.save {
        Mutation::Recording => {
            ui.label("Recording save receipt…");
        }
        Mutation::Submitting => {
            ui.label("Saving immutable revision…");
        }
        Mutation::Unresolved(error) | Mutation::Rejected(error) => {
            ui.colored_label(egui::Color32::YELLOW, error);
        }
        _ => (),
    }
    if !model.authoring.error.is_empty() {
        ui.label(&model.authoring.error);
    }
    let Some(definition) = &model.authoring.definition else {
        return;
    };
    ui.separator();
    ui.label(format!(
        "{} · {}",
        definition.value.name, definition.value.execution_recipe_id
    ));
    if let Some(saved) = &definition.value.saved {
        ui.label(format!(
            "Saved recipe {} · revision {}",
            saved.recipe_id, saved.revision
        ));
    }
    ui.collapsing("Inherited settings and seeds (read only)", |ui| {
        egui::ScrollArea::vertical()
            .id_salt("recipe-inherited")
            .max_height(180.0)
            .show(ui, |ui| {
                ui.monospace(&definition.inherited);
                for seed in &definition.value.seeds {
                    ui.label(format!(
                        "{} · {} bytes · {}",
                        seed.path, seed.size_bytes, seed.sha256
                    ));
                }
            });
    });
    if model.authoring.draft.is_none() {
        let saved = definition.value.saved.is_some();
        ui.collapsing("Saved prompt", |ui| {
            egui::ScrollArea::vertical()
                .id_salt("recipe-prompt")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.label(&definition.value.prompt);
                });
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Clone recipe"))
                .clicked()
            {
                model.edit_recipe(true);
            }
            if saved
                && ui
                    .add_enabled(!busy, egui::Button::new("Edit recipe"))
                    .clicked()
            {
                model.edit_recipe(false);
            }
        });
    } else {
        let draft = model.authoring.draft.as_mut().unwrap();
        ui.label(if draft.clone {
            "Clone into a new saved recipe"
        } else {
            "Create a new revision of this saved recipe"
        });
        ui.add_enabled_ui(!busy, |ui| {
            let name_label = ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut draft.name)
                    .char_limit(128)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(name_label.id);
            let prompt_label = ui.label("Prompt");
            egui::ScrollArea::vertical()
                .id_salt("prompt-editor")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut draft.prompt)
                            .char_limit(65536)
                            .desired_rows(6)
                            .desired_width(f32::INFINITY),
                    )
                    .labelled_by(prompt_label.id);
                });
        });
        ui.label(format!("{} / 65,536 prompt bytes", draft.prompt.len()));
        if !draft.clone && !busy && ui.button("Keep edits as a new recipe instead").clicked() {
            draft.clone = true;
        }
        let valid = draft.valid();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy && valid, egui::Button::new("Save revision"))
                .clicked()
            {
                model.save_recipe();
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Discard edits"))
                .clicked()
            {
                model.discard_recipe_edits();
            }
        });
    }
    if let Some(id) = model.authoring.last_saved.clone()
        && model.recipe.as_ref() != Some(&id)
        && !busy
        && model.authoring.draft.is_none()
        && ui.button("Open last saved revision").clicked()
    {
        model.choose_recipe(id, RunMode::Experiment);
    }
}
