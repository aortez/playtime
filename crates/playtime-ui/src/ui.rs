//! egui presentation. Widgets consume prepared text/pixels and emit user intent.
use crate::{
    backend::Backend,
    model::*,
    progress,
    transcript::{TEXT_BYTES, View, WINDOW_ROWS},
};
use egui::{Color32, RichText, Vec2};
use playtime_client::config::Options;
use playtime_core::{RunId, RunMode};
use playtime_protocol::api::{ContentAvailability, Enforcement, OrchestrationState, RunView};
use std::{path::PathBuf, sync::Arc};

const ACCENT: Color32 = Color32::from_rgb(96, 214, 185);
#[derive(Clone, Debug, Default)]
pub struct FrameMetrics {
    pub laid_out_rows: usize,
    pub text_bytes: usize,
    pub texture_bytes: usize,
}
#[derive(Default)]
pub struct Workbench {
    pub model: Model,
    pub backend: Option<Backend>,
    pub metrics: FrameMetrics,
    texture: Option<(Arc<crate::media::Pixels>, egui::TextureHandle)>,
    requested_range: Option<(View, usize, usize)>,
    pub options: Options,
    previous_narrow: Option<bool>,
    timeline: Option<TimelineView>,
    timeline_anchor: Option<usize>,
    /// The row a view change was made around, until the backend locates it.
    view_anchor: Option<usize>,
}
/// The last timeline layout, so a row-height or view change can keep a row in
/// place.
struct TimelineView {
    run: Option<RunId>,
    view: View,
    row_height: f32,
    offset: f32,
}

impl Workbench {
    pub fn desktop(ctx: egui::Context, options: Options) -> Self {
        let mut app = Self {
            backend: Some(Backend::start(ctx)),
            options: options.clone(),
            ..Default::default()
        };
        app.model.connect(options);
        app
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.metrics = FrameMetrics::default();
        if self
            .model
            .content
            .as_ref()
            .is_none_or(|c| c.image.is_none())
        {
            self.texture = None;
        }
        let old_widths = (
            self.model.preferences.left_width,
            self.model.preferences.right_width,
        );
        ui.ctx().set_theme(egui::Theme::Dark);
        ui.style_mut().visuals.selection.bg_fill = Color32::from_rgb(31, 85, 79);
        ui.style_mut().visuals.selection.stroke.color = ACCENT;
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.model.enlarged = false;
            if ui.max_rect().width() < 1100.0 {
                self.model.show_inspector = false;
            }
        }
        let narrow = ui.max_rect().width() < 1100.0;
        if self.model.playing
            && (self.model.launcher
                || (narrow && !self.model.show_inspector && !self.model.enlarged))
        {
            self.model.stop_animation();
        }
        if narrow && self.previous_narrow != Some(true) {
            self.model.show_inspector = false;
        }
        self.previous_narrow = Some(narrow);
        egui::Panel::top("connection-bar")
            .exact_size(
                if matches!(self.model.connection, ConnectionState::Error(_)) {
                    96.0
                } else {
                    68.0
                },
            )
            .show_inside(ui, |ui| self.connection(ui, narrow));
        egui::Panel::bottom("workbench-status")
            .exact_size(32.0)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.small(brief(&self.model.notice, 220));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.model.stale {
                            ui.colored_label(Color32::YELLOW, "Cached information · stale");
                        } else {
                            ui.small(format!(
                                "Updated {:.0}s ago",
                                (self.model.clock - self.model.last_update).max(0.0)
                            ));
                        }
                    });
                });
            });
        if !narrow || self.model.show_runs {
            let response = egui::Panel::left("run-navigation")
                .default_size(self.model.preferences.left_width)
                .size_range(180.0..=360.0)
                .resizable(true)
                .show_inside(ui, |ui| self.navigation(ui));
            self.model.preferences.left_width = response.response.rect.width();
        }
        if !narrow {
            let response = egui::Panel::right("inspection")
                .default_size(self.model.preferences.right_width)
                .size_range(260.0..=520.0)
                .resizable(true)
                .show_inside(ui, |ui| self.inspector(ui));
            self.model.preferences.right_width = response.response.rect.width();
        }
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.model.launcher {
                self.launcher(ui);
            } else {
                self.timeline(ui);
            }
        });
        if narrow && self.model.show_inspector {
            let mut open = true;
            egui::Window::new("Inspector drawer")
                .open(&mut open)
                .anchor(egui::Align2::RIGHT_TOP, [-12.0, 80.0])
                .default_width(360.0)
                .default_height(430.0)
                .resizable(true)
                .show(ui.ctx(), |ui| self.inspector(ui));
            self.model.show_inspector = open;
        }
        if self.model.enlarged {
            let mut open = true;
            egui::Window::new("Saved image enlargement")
                .open(&mut open)
                .default_size([720.0, 600.0])
                .show(ui.ctx(), |ui| self.image(ui, true));
            self.model.enlarged = open;
        }
        if old_widths
            != (
                self.model.preferences.left_width,
                self.model.preferences.right_width,
            )
            && ui.input(|i| i.pointer.any_released())
        {
            self.model
                .command(Action::Preferences(self.model.preferences.clone()));
        }
        if !self.model.commands.is_empty() && self.backend.is_some() {
            ui.ctx().request_repaint();
        }
    }
    fn connection(&mut self, ui: &mut egui::Ui, narrow: bool) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("PLAYTIME").size(21.0).strong().color(ACCENT));
            ui.separator();
            ui.label(self.model.connection.label());
            if narrow && ui.button("Runs").clicked() {
                self.model.show_runs = !self.model.show_runs;
            }
            if narrow && ui.button("Inspector").clicked() {
                self.model.show_inspector = !self.model.show_inspector;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Reconnect").clicked() {
                    self.model.connect(self.options.clone());
                    self.requested_range = None;
                }
                ui.menu_button("Connection settings", |ui| {
                    ui.label("Protected connection descriptor");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.model.connection_file)
                            .hint_text("Connection file path"),
                    )
                    .labelled_by(ui.label("Connection file").id);
                    if ui.button("Connect").clicked() {
                        let options = Options {
                            connection_file: Some(PathBuf::from(&self.model.connection_file)),
                            state_dir: self.options.state_dir.clone(),
                            ..Default::default()
                        };
                        self.options = options.clone();
                        self.model.connect(options);
                    }
                });
            });
        });
        ui.horizontal(|ui| {
            ui.small(brief(&self.model.backend, 190));
            if !self.model.endpoint.is_empty() {
                ui.small(&self.model.endpoint);
            }
        });
        if let ConnectionState::Error(error) = &self.model.connection {
            ui.colored_label(Color32::YELLOW, brief(error, 150));
        }
    }
    fn navigation(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        if ui
            .add_sized(
                [ui.available_width(), 34.0],
                egui::Button::new("New experiment"),
            )
            .clicked()
        {
            self.model.new_experiment();
        }
        ui.add_space(10.0);
        ui.heading("Runs");
        ui.horizontal(|ui| {
            if ui.button("Refresh runs").clicked() {
                self.model.command(Action::Runs(None));
            }
            if self.model.runs_next.is_some() && ui.button("More runs").clicked() {
                self.model
                    .command(Action::Runs(self.model.runs_next.clone()));
            }
        });
        let mut selected = None;
        egui::ScrollArea::vertical()
            .id_salt("runs")
            .max_height(310.0)
            .show_rows(ui, 36.0, self.model.runs.len(), |ui, range| {
                for i in range {
                    let run = &self.model.runs[i];
                    if ui
                        .selectable_label(
                            self.model.selected.as_ref() == Some(&run.run_id),
                            format!("Run {} · {}", run.run_id, run_state(run)),
                        )
                        .clicked()
                    {
                        selected = Some(run.run_id.clone());
                    }
                }
            });
        if let Some(run) = selected {
            self.model.select_run(run);
            self.texture = None;
            self.requested_range = None;
        }
        ui.separator();
        ui.heading("Pending launches");
        if self.model.receipts.iter().all(|r| r.run_id.is_some()) {
            ui.small("No unresolved local requests");
        }
        let mut action = None;
        egui::ScrollArea::vertical()
            .id_salt("receipts")
            .show(ui, |ui| {
                for receipt in self
                    .model
                    .receipts
                    .iter()
                    .filter(|r| r.run_id.is_none())
                    .take(200)
                {
                    // A full key is wider than the panel, and an overflowing row widens
                    // the panel's allocation past what it paints. The key prefix keeps
                    // each control's name distinct.
                    ui.add(egui::Label::new(receipt.key.as_str()).truncate());
                    let prefix = brief(receipt.key.as_str(), 8);
                    let enabled = !matches!(
                        self.model.launch,
                        Mutation::Submitting | Mutation::Recording
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(enabled, egui::Button::new(format!("Look up {prefix}")))
                            .clicked()
                        {
                            action = Some(Action::Recover(receipt.key.clone(), false));
                        }
                        if ui
                            .add_enabled(enabled, egui::Button::new(format!("Retry {prefix}")))
                            .clicked()
                        {
                            action = Some(Action::Recover(receipt.key.clone(), true));
                        }
                    });
                }
            });
        if let Some(action) = action {
            self.model.command(action);
        }
        if self.model.receipts_next.is_some() && ui.button("More receipts").clicked() {
            self.model
                .command(Action::Receipts(self.model.receipts_next.clone()));
        }
    }
    fn launcher(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.heading(RichText::new("New experiment").size(26.0));
        ui.label("Choose a recipe and review its frozen inputs before launching.");
        ui.add_space(12.0);
        let mut choice = None;
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Recipe")
                .selected_text(
                    self.model
                        .recipe
                        .as_ref()
                        .map_or("Select a recipe", |r| r.as_str()),
                )
                .show_ui(ui, |ui| {
                    for recipe in &self.model.recipes {
                        if ui
                            .selectable_label(
                                self.model.recipe.as_ref() == Some(&recipe.recipe_id),
                                brief(&recipe.name, 80),
                            )
                            .clicked()
                        {
                            choice = Some((
                                recipe.recipe_id.clone(),
                                recipe.modes.first().copied().unwrap_or(RunMode::Simulation),
                            ));
                        }
                    }
                });
            let modes = self
                .model
                .recipes
                .iter()
                .find(|r| Some(&r.recipe_id) == self.model.recipe.as_ref())
                .map(|r| r.modes.clone())
                .unwrap_or_default();
            egui::ComboBox::from_label("Mode")
                .selected_text(progress::capitalized(progress::mode_words(self.model.mode)))
                .show_ui(ui, |ui| {
                    for mode in modes {
                        if ui
                            .selectable_label(
                                self.model.mode == mode,
                                progress::capitalized(progress::mode_words(mode)),
                            )
                            .clicked()
                            && let Some(id) = &self.model.recipe
                        {
                            choice = Some((id.clone(), mode));
                        }
                    }
                });
            if self.model.recipe_next.is_some() && ui.button("More recipes").clicked() {
                self.model
                    .command(Action::Recipes(self.model.recipe_next.clone()));
            }
        });
        if let Some((id, mode)) = choice {
            self.model.choose_recipe(id, mode);
        }
        if self.model.previewing {
            ui.label("Loading read-only preview…");
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.model.can_launch(),
                    egui::Button::new("Launch experiment").fill(Color32::from_rgb(32, 104, 86)),
                )
                .clicked()
            {
                self.model.launch();
            }
            if let Some(id) = self.model.recipe.clone()
                && ui.button("Refresh preview").clicked()
            {
                self.model.choose_recipe(id, self.model.mode);
            }
            match &self.model.launch {
                Mutation::Recording => {
                    ui.label("Saving recovery receipt…");
                }
                Mutation::Submitting => {
                    ui.label("Submitting saved request…");
                }
                _ => (),
            }
        });
        // Outside the button row, so long messages wrap instead of being clipped.
        match &self.model.launch {
            Mutation::Unresolved(message) => {
                ui.colored_label(Color32::YELLOW, brief(message, 500));
            }
            Mutation::Rejected(message) => {
                ui.colored_label(Color32::LIGHT_RED, brief(message, 500));
            }
            _ => (),
        }
        if let Some(saved) = &self.model.saved
            && matches!(
                self.model.launch,
                Mutation::Unresolved(_) | Mutation::Rejected(_)
            )
        {
            let key = saved.key.clone();
            ui.label(format!("Recovery key: {key}"));
            ui.horizontal(|ui| {
                if ui.button("Look up saved launch").clicked() {
                    self.model.command(Action::Recover(key.clone(), false));
                }
                if ui.button("Retry saved launch").clicked() {
                    self.model.command(Action::Recover(key, true));
                }
            });
        }
        let mut preview_page = None;
        if let Some(preview) = &self.model.preview {
            egui::ScrollArea::vertical()
                .id_salt("preview")
                .show(ui, |ui| {
                    ui.add_space(12.0);
                    ui.heading("Prompt");
                    text(ui, &preview.value.prompt, &mut self.metrics);
                    ui.separator();
                    for line in &preview.summary {
                        ui.label(brief(line, 1024));
                    }
                    for warning in &preview.value.warnings {
                        ui.colored_label(Color32::YELLOW, brief(warning, 500));
                    }
                    egui::CollapsingHeader::new("Full frozen preview and protocol").show(
                        ui,
                        |ui| {
                            text(ui, &preview.source.text, &mut self.metrics);
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(
                                        preview.source.start > 0,
                                        egui::Button::new("Previous preview page"),
                                    )
                                    .clicked()
                                {
                                    preview_page =
                                        Some(preview.source.start.saturating_sub(TEXT_BYTES));
                                }
                                if ui
                                    .add_enabled(
                                        preview.source.start + preview.source.text.len()
                                            < preview.source.total,
                                        egui::Button::new("Next preview page"),
                                    )
                                    .clicked()
                                {
                                    preview_page =
                                        Some(preview.source.start + preview.source.text.len());
                                }
                            });
                        },
                    );
                });
        } else {
            ui.add_space(30.0);
            ui.label(
                "Select a recipe to load its prompt, limits, model, workshop and seed summary.",
            );
        }
        if let Some(offset) = preview_page
            && let Some(preview) = &self.model.preview
        {
            self.model
                .command(Action::PreviewPage(Box::new(preview.value.clone()), offset));
        }
    }
    fn timeline(&mut self, ui: &mut egui::Ui) {
        ui.heading(
            self.model
                .selected
                .as_ref()
                .map_or("Select a run", |r| r.as_str()),
        );
        if let Some(run) = &self.model.run {
            // Progress first, then state and accounting.
            let lead = progress::lead(&run.value, &self.model.window.progress);
            if let Some((first, rest)) = lead.split_first() {
                ui.label(RichText::new(brief(first, 1000)).size(17.0).strong());
                for line in rest {
                    ui.label(brief(line, 1000));
                }
            }
            for line in &run.summary {
                ui.small(brief(line, 1000));
            }
            if awaiting_release(&run.value) {
                ui.colored_label(
                    Color32::YELLOW,
                    format!(
                        "The daemon stopped monitoring this run, so it holds the slot until released: playtime runs release {} --reason TEXT",
                        run.value.run_id
                    ),
                );
            }
        }
        // A stopped worker has nothing left to cancel.
        let can_cancel = self.model.connection == ConnectionState::Connected
            && self.model.run.as_ref().is_some_and(|r| {
                r.value.orchestration.state != OrchestrationState::Terminal
                    && !awaiting_release(&r.value)
                    && r.value
                        .cancellation
                        .as_ref()
                        .is_none_or(|c| c.enforcement != Enforcement::Stopped)
            })
            && !matches!(self.model.cancellation, Mutation::Submitting);
        ui.horizontal(|ui| {
            for (label, mode) in [
                ("Compact", Disclosure::Compact),
                ("Readable", Disclosure::Readable),
                ("Full", Disclosure::Full),
            ] {
                if ui
                    .selectable_label(self.model.preferences.disclosure == mode, label)
                    .clicked()
                {
                    self.model.preferences.disclosure = mode;
                    self.model
                        .command(Action::Preferences(self.model.preferences.clone()));
                }
            }
            ui.separator();
            if ui
                .add_enabled(can_cancel, egui::Button::new("Cancel run"))
                .clicked()
            {
                self.model.cancel();
            }
            if matches!(self.model.cancellation, Mutation::Submitting) {
                ui.small("Saving cancellation intent…");
            }
            if self.model.following {
                if ui.button("Pause following").clicked() {
                    self.model.following = false;
                }
            } else if ui
                .button(format!("Jump to live ({})", self.model.new_activity()))
                .clicked()
            {
                self.model.jump_live();
            }
        });
        ui.small(&self.model.history);
        if self.model.preferences.disclosure != Disclosure::Compact
            && self
                .model
                .content
                .as_ref()
                .is_some_and(|c| c.image.is_some())
        {
            egui::CollapsingHeader::new("Saved checkpoint image")
                .default_open(true)
                .show(ui, |ui| self.image(ui, false));
        }
        ui.separator();
        let row_height = if !self.model.expanded.is_empty() {
            360.0
        } else {
            match self.model.preferences.disclosure {
                Disclosure::Compact => 120.0,
                Disclosure::Readable => 210.0,
                Disclosure::Full => 360.0,
            }
        };
        let view = if self.model.preferences.disclosure == Disclosure::Full {
            View::Full
        } else {
            View::Readable
        };
        let total = self.model.window.total;
        let mut range = (0, 32);
        let mut selection = None;
        let mut expansion = None;
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("timeline", self.model.selected.clone()))
            .stick_to_bottom(self.model.following);
        // Every row shares one height, so changing it moves every row below the
        // top of the list. Keep the toggled row, or else the top row, in place.
        let anchor = self.timeline_anchor.take();
        let spacing = ui.spacing().item_spacing.y;
        let mut view_anchor = None;
        if let Some(last) = &self.timeline
            && !self.model.following
            && last.run == self.model.selected
        {
            let old = last.row_height + spacing;
            if last.view != view {
                // The views list different rows, so the backend locates the top row
                // in the new one.
                let top = (last.offset / old) as usize;
                view_anchor = top
                    .checked_sub(self.model.window.start)
                    .and_then(|i| self.model.window.rows.get(i))
                    .map(|row| row.id);
                self.view_anchor = view_anchor;
            } else if last.row_height != row_height {
                let new = row_height + spacing;
                let row = anchor.unwrap_or((last.offset / old) as usize);
                area =
                    area.vertical_scroll_offset((last.offset + row as f32 * (new - old)).max(0.0));
            }
        }
        if let Some((id, at)) = self.model.window.anchor
            && self.view_anchor == Some(id)
        {
            self.view_anchor = None;
            area = area.vertical_scroll_offset(at as f32 * (row_height + spacing));
        }
        let response = area.show_rows(ui, row_height, total, |ui, visible| {
            let start = visible.start.saturating_sub(2); range = (start, (visible.end - start + 2).min(WINDOW_ROWS));
            for i in visible {
                self.metrics.laid_out_rows += 1;
                ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), row_height), egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.set_min_height(row_height);
                    if let Some(row) = self.model.window.rows.get(i.saturating_sub(self.model.window.start)).filter(|_| i >= self.model.window.start) {
                        egui::Frame::new().inner_margin(10).corner_radius(5).fill(if self.model.selected_row == Some(row.id) { Color32::from_rgb(31, 57, 57) } else { Color32::from_rgb(29, 34, 42) }).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.set_min_height(row_height - 28.0);
                            ui.horizontal_wrapped(|ui| { ui.label(RichText::new(&row.title).color(ACCENT));
                            // A checkpoint marker adds what that checkpoint saved.
                            if let Some(files) = row.checkpoint.as_ref().and_then(|session| self.model.checkpoint_files.get(session)) { ui.label(files); }
                            if ui.button(format!("{} activity {}", if self.model.expanded.contains(&row.id) { "Collapse" } else { "Expand" }, row.first_sequence)).clicked() { expansion = Some((row.id, i)); }
                            if ui.button(format!("Inspect source {}", row.first_sequence)).clicked() { selection = Some((row.id, row.first_sequence)); } });
                            if !row.marker { ui.small(&row.status); }
                            if !row.marker && self.model.preferences.disclosure != Disclosure::Compact { text(ui, &row.excerpt, &mut self.metrics); }
                            if self.model.preferences.disclosure == Disclosure::Full || self.model.expanded.contains(&row.id) {
                                egui::ScrollArea::vertical().id_salt(("row-body", row.id)).max_height(175.0).show(ui, |ui| {
                                    for section in &row.sections { ui.strong(brief(&section.label, 120)); text(ui, &section.text, &mut self.metrics); }
                                    if row.needs_loading { ui.label("Earlier source is outside the payload cache; inspect to reload it."); }
                                });
                            }
                            if !row.marker { ui.small(&row.sources); }
                        });
                    } else { ui.label("Loading activity page…"); }
                });
            }
        });
        self.timeline = Some(TimelineView {
            run: self.model.selected.clone(),
            view,
            row_height,
            offset: response.state.offset.y,
        });
        if ui.rect_contains_pointer(response.inner_rect)
            && ui.input(|i| i.smooth_scroll_delta.y != 0.0 || i.pointer.any_pressed())
        {
            self.model.following = false;
        }
        if self.requested_range != Some((view, range.0, range.1)) || view_anchor.is_some() {
            self.requested_range = Some((view, range.0, range.1));
            self.model.command(Action::Window(WindowRequest {
                view,
                start: range.0,
                count: range.1,
                anchor: view_anchor,
            }));
        }
        if let Some((id, index)) = expansion {
            self.timeline_anchor = Some(index);
            self.model.following = false;
            if !self.model.expanded.remove(&id) {
                if self.model.expanded.len() < 64 {
                    self.model.expanded.insert(id);
                } else {
                    self.model.notify(
                        "64 activities are expanded; collapse one before opening another",
                        self.model.selected.clone(),
                    );
                }
            }
        }
        if let Some((id, seq)) = selection {
            self.model.inspect(id, seq);
        }
    }
    fn inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Inspector");
        if self.model.launcher {
            ui.label("Review the frozen preview in the center before launching.");
            ui.separator();
            ui.label("Launch writes a private recovery receipt before sending the request. Pending launches remain available after reopening this window.");
            return;
        }
        ui.horizontal(|ui| {
            for (label, tab) in [
                ("Details", Inspector::Details),
                ("Memory", Inspector::Memory),
                ("Files", Inspector::Files),
            ] {
                if ui
                    .selectable_label(self.model.inspector == tab, label)
                    .clicked()
                {
                    self.model.inspector = tab;
                    self.model.following = false;
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt("inspector-body").show(ui, |ui| {
            if self.model.inspector != Inspector::Details {
                let mut selected = None;
                egui::ComboBox::from_label("Checkpoint").selected_text(self.model.checkpoint.as_ref().map_or("Select checkpoint", |c| c.checkpoint.session_id.as_str())).show_ui(ui, |ui| {
                    for checkpoint in &self.model.checkpoints { if ui.selectable_label(self.model.checkpoint.as_ref().map(|c| &c.checkpoint.session_id) == Some(&checkpoint.checkpoint.session_id), checkpoint.checkpoint.session_id.as_str()).clicked() { selected = Some(checkpoint.checkpoint.session_id.clone()); } }
                });
                if let Some(session) = selected && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::Checkpoint(run, session)); }
                if self.model.checkpoints_next.is_some() && ui.button("More checkpoints").clicked() && let Some(run) = self.model.selected.clone() { self.model.command(Action::Checkpoints(run, self.model.checkpoints_next.clone())); }
            }
            match self.model.inspector {
                Inspector::Details => {
                    if let Some(source) = &self.model.source {
                        ui.label(format!("Recorded source · {} bytes", source.total)); text(ui, &source.text, &mut self.metrics);
                        let (start, total) = (source.start, source.total);
                        let retained = source.retained.clone();
                        if let Some(reference) = retained && ui.button("Load verified retained payload").clicked() && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::Retained(run, reference)); }
                        if let Some(row) = self.model.selected_row {
                            ui.horizontal(|ui| {
                                if ui.add_enabled(start > 0, egui::Button::new("Previous source page")).clicked() { self.model.inspect_action(Action::Source(row, self.model.source_sequence, start.saturating_sub(TEXT_BYTES))); }
                                if ui.add_enabled(start + TEXT_BYTES < total, egui::Button::new("Next source page")).clicked() { self.model.inspect_action(Action::Source(row, self.model.source_sequence, start + TEXT_BYTES)); }
                            });
                            if ui.button("Next source record").clicked() { self.model.source_sequence += 1; self.model.inspect_action(Action::Source(row, self.model.source_sequence, 0)); }
                        }
                    } else if let Some(run) = &self.model.run { text(ui, &run.source.text, &mut self.metrics); }
                    else { ui.label("Select an activity or run to inspect its recorded source."); }
                }
                Inspector::Memory => {
                    if let Some(checkpoint) = self.model.checkpoint.clone() {
                        for (label, reference) in [("Incoming source", checkpoint.incoming_memory), ("Outgoing source", checkpoint.outgoing_memory), ("Evidence source", checkpoint.evidence)] {
                            if let ContentAvailability::Available { artifact_id, .. } = reference && ui.button(label).clicked() && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::Content(run, artifact_id, 0)); }
                        }
                    }
                    if let Some(memory) = &self.model.memory {
                        ui.strong("Incoming · delivered to this session"); text(ui, &memory.incoming, &mut self.metrics);
                        ui.separator(); ui.strong("Outgoing · saved publication"); text(ui, &memory.outgoing, &mut self.metrics);
                        ui.label(brief(&memory.notice, 500));
                        egui::CollapsingHeader::new("Memory diff").show(ui, |ui| text(ui, &memory.diff, &mut self.metrics));
                        egui::CollapsingHeader::new("Separate controller evidence").show(ui, |ui| text(ui, &memory.evidence, &mut self.metrics));
                    } else { ui.label("Choose a published checkpoint to compare exact delivered and saved memory."); }
                }
                Inspector::Files => {
                    ui.small("Saved versions · change classification is unavailable from this API");
                    let mut open = None;
                    if let Some(checkpoint) = &self.model.checkpoint {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(self.model.file_page > 0, egui::Button::new("Previous files")).clicked() { self.model.file_page = self.model.file_page.saturating_sub(20); }
                            if ui.add_enabled(self.model.file_page + 20 < checkpoint.files.len(), egui::Button::new("Next files")).clicked() { self.model.file_page += 20; }
                        });
                        for file in checkpoint.files.iter().skip(self.model.file_page).take(20) {
                            ui.separator(); ui.strong(brief(&file.name, 120)); ui.small(file_kind(&file.kind));
                            match &file.content {
                                ContentAvailability::Available { artifact_id, content } => {
                                    ui.small(format!("{} bytes · session {}", content.size_bytes, checkpoint.checkpoint.session_id)); ui.small(content.sha256.to_string());
                                    if ui.button(format!("Inspect file {artifact_id}")).clicked() { open = Some(artifact_id.clone()); }
                                }
                                ContentAvailability::Unavailable { reason } => { ui.label(format!("Unavailable: {}", brief(reason, 200))); }
                            }
                        }
                    } else { ui.label("Choose a checkpoint to inspect saved file versions."); }
                    if let Some(id) = open && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::Content(run, id, 0)); }
                }
            }
            if self.model.content.is_some() {
                ui.separator(); self.image(ui, false);
                if let Some(content) = &self.model.content {
                    ui.label(brief(&content.notice, 500));
                    let mut page_offset = None;
                    if let Some(source) = &content.source {
                        ui.label(format!("Source bytes {}–{} of {}", source.start, source.start + source.text.len(), source.total));
                        text(ui, &source.text, &mut self.metrics);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(source.start > 0, egui::Button::new("Previous file page")).clicked() { page_offset = Some(source.start.saturating_sub(TEXT_BYTES)); }
                            if ui.add_enabled(source.start + source.text.len() < source.total, egui::Button::new("Next file page")).clicked() { page_offset = Some(source.start + source.text.len()); }
                        });
                    }
                    let artifact = content.artifact.artifact_id.clone();
                    if let Some(offset) = page_offset && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::Content(run, artifact.clone(), offset)); }
                    ui.add(egui::TextEdit::singleline(&mut self.model.destination).hint_text("Explicit destination path")).labelled_by(ui.label("Download destination").id);
                    if ui.add_enabled(!self.model.destination.is_empty(), egui::Button::new("Download verified content")).clicked()
                        && let Some(run) = self.model.selected.clone() { self.model.command(Action::Download(run, artifact, PathBuf::from(&self.model.destination))); }
                }
            }
        });
    }
    fn image(&mut self, ui: &mut egui::Ui, enlarged: bool) {
        let pixels = self.model.content.as_ref().and_then(|c| c.image.clone());
        let Some(pixels) = pixels else {
            self.texture = None;
            return;
        };
        if self
            .texture
            .as_ref()
            .is_none_or(|(old, _)| !Arc::ptr_eq(old, &pixels))
        {
            self.texture = None;
            let texture = ui.ctx().load_texture(
                "verified-saved-image",
                egui::ImageData::Color(pixels.image.clone()),
                egui::TextureOptions::LINEAR,
            );
            self.texture = Some((pixels.clone(), texture));
        }
        self.metrics.texture_bytes = pixels.bytes;
        if self.model.content.as_ref().is_some_and(|c| c.animated) {
            if self.model.playing {
                if ui.button("Pause GIF").clicked() {
                    self.model.stop_animation();
                }
            } else if ui.button("Play GIF from start").clicked()
                && let Some(run) = self.model.selected.clone()
            {
                let id = self
                    .model
                    .content
                    .as_ref()
                    .unwrap()
                    .artifact
                    .artifact_id
                    .clone();
                self.model.playing = true;
                self.model.command(Action::PlayGif(run, id));
            }
        }
        let texture = &self.texture.as_ref().unwrap().1;
        let size = texture.size_vec2();
        let scale = (ui.available_width() / size.x).min(if enlarged {
            4.0
        } else {
            (128.0 / size.y).min(1.0)
        });
        if enlarged {
            ui.checkbox(&mut self.model.actual_size, "Actual size");
            egui::ScrollArea::both().show(ui, |ui| {
                ui.image((
                    texture.id(),
                    if self.model.actual_size {
                        size
                    } else {
                        size * scale
                    },
                ));
            });
        } else {
            ui.image((texture.id(), size * scale));
            if ui.button("Enlarge saved image").clicked() {
                self.model.enlarged = true;
                self.model.following = false;
            }
        }
    }
}
/// The run list is narrow; the header shows the full orchestration reason.
fn run_state(run: &RunView) -> String {
    let state = progress::state_words(run.orchestration.state);
    if awaiting_release(run) {
        format!("{state} · needs release")
    } else if run.orchestration.reason.as_deref() == Some("operator_released") {
        format!("{state} · released")
    } else {
        state.into()
    }
}
/// Captured workspace entries carry their tar type flag.
fn file_kind(kind: &str) -> &str {
    match kind {
        "0" | "\0" => "regular",
        "1" => "hard link",
        "2" => "symbolic link",
        "5" => "directory",
        other => brief(other, 120),
    }
}
fn brief(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn text(ui: &mut egui::Ui, value: &str, metrics: &mut FrameMetrics) {
    let remaining = TEXT_BYTES.saturating_sub(metrics.text_bytes);
    let visible = brief(value, remaining);
    metrics.text_bytes += visible.len();
    ui.add(egui::Label::new(RichText::new(visible).monospace()).wrap());
    if visible.len() < value.len() {
        ui.small("Text viewport limit reached · inspect the next source page for continuation");
    }
}
impl eframe::App for Workbench {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.model.clock = ctx.input(|i| i.time);
        if let Some(backend) = &mut self.backend {
            backend.pump(&mut self.model);
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.draw(ui);
    }
}
