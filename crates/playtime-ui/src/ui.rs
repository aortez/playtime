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
    timeline_anchor: Option<(usize, f32)>,
    heights: crate::timeline_layout::Heights,
    /// The row a view change was made around, until the backend locates it.
    view_anchor: Option<usize>,
    navigation: Option<(u64, f32)>,
    navigation_sent: bool,
}
/// The last timeline layout, so a row-height or view change can keep a row in
/// place.
struct TimelineView {
    run: Option<RunId>,
    connection: u64,
    sequence: Option<u64>,
    view: View,
    width: f32,
    disclosure: Disclosure,
    total: usize,
    top: Option<(usize, usize, f32)>,
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
        crate::models::draw(ui, &mut self.model);
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
            if (self.model.models.enabled || self.model.registrations.enabled)
                && ui.button("Models").clicked()
            {
                self.model.browse_models();
            }
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
        ui.horizontal_wrapped(|ui| {
            ui.small(
                self.model
                    .backend
                    .split(" · ")
                    .next()
                    .unwrap_or("Backend unavailable"),
            );
            if !self.model.endpoint.is_empty() {
                ui.small(&self.model.endpoint);
            }
            ui.menu_button("Backend details", |ui| {
                ui.set_max_width(440.0);
                ui.label(brief(&self.model.backend, 4096));
            });
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
            .show_rows(ui, 96.0, self.model.runs.len(), |ui, range| {
                for i in range {
                    ui.scope(|ui| {
                        ui.set_min_height(96.0);
                        let run = &self.model.runs[i];
                        let recipe = self
                            .model
                            .recipes
                            .iter()
                            .find(|recipe| recipe.recipe_id == run.recipe_id)
                            .map_or(run.recipe_id.as_str(), |recipe| recipe.name.as_str());
                        let response = ui.add(
                            egui::Button::new(brief(recipe, 60))
                                .selected(self.model.selected.as_ref() == Some(&run.run_id))
                                .wrap_mode(egui::TextWrapMode::Truncate),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::SelectableLabel,
                                true,
                                format!("Run {} · {}", run.run_id, run_state(run)),
                            )
                        });
                        if response.clicked() {
                            selected = Some(run.run_id.clone());
                        }
                        // Keep actionable state visible even when the recipe is ellipsized.
                        ui.add(egui::Label::new(run_state(run)).wrap());
                        ui.small(format!(
                            "{} UTC",
                            run.accepted_at
                                .as_str()
                                .trim_end_matches('Z')
                                .replace('T', " ")
                        ));
                        ui.add(
                            egui::Label::new(RichText::new(format!("Run {}", run.run_id)).small())
                                .truncate(),
                        )
                        .on_hover_text(run.run_id.as_str());
                    });
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
        if self.model.authoring.enabled {
            // Saved-revision metadata and readiness errors can make the editor
            // taller than a narrow window. Keep its save controls reachable.
            egui::ScrollArea::vertical()
                .id_salt("authoring-launcher")
                .show(ui, |ui| self.launcher_contents(ui));
        } else {
            self.launcher_contents(ui);
        }
    }
    fn launcher_contents(&mut self, ui: &mut egui::Ui) {
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
            if self.model.authoring.enabled && ui.button("Refresh recipes").clicked() {
                self.model.command(Action::Recipes(None));
            }
        });
        if let Some((id, mode)) = choice {
            self.model.choose_recipe(id, mode);
        }
        crate::authoring::draw(&mut self.model, ui);
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
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_space(12.0);
                    ui.heading("Prompt");
                    prose(ui, &preview.prompt, &mut self.metrics);
                    if preview.prompt.len() < preview.value.prompt.len() {
                        ui.small("Prompt excerpt · full text in Details");
                    }
                    for section in &preview.sections {
                        ui.add_space(12.0);
                        ui.heading(section.title);
                        for line in &section.lines {
                            prose(ui, line, &mut self.metrics);
                        }
                    }
                    ui.add_space(12.0);
                    ui.label(format!(
                        "Frozen revision {} · {}",
                        preview.revision,
                        if preview.value.readiness.ready {
                            "Ready to launch"
                        } else {
                            "Execution unavailable"
                        }
                    ));
                    for reason in &preview.value.readiness.reasons {
                        ui.colored_label(Color32::YELLOW, brief(reason, 500));
                    }
                    egui::CollapsingHeader::new("Protocol").show(ui, |ui| {
                        prose(ui, &preview.protocol, &mut self.metrics);
                        if preview.protocol.len() < preview.value.protocol.len() {
                            ui.small("Protocol excerpt · full text in Details");
                        }
                    });
                    egui::CollapsingHeader::new("Workshop inventory")
                        .show(ui, |ui| prose(ui, &preview.inventory, &mut self.metrics));
                    for warning in &preview.value.warnings {
                        ui.colored_label(Color32::YELLOW, brief(warning, 500));
                    }
                    egui::CollapsingHeader::new("Details · full frozen preview").show(ui, |ui| {
                        text(ui, &preview.source.text, &mut self.metrics);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    preview.source.start > 0,
                                    egui::Button::new("Previous preview page"),
                                )
                                .clicked()
                            {
                                preview_page = Some(
                                    preview
                                        .source
                                        .start
                                        .saturating_sub(crate::preview::SOURCE_BYTES),
                                );
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
                    });
                });
        } else {
            ui.add_space(30.0);
            ui.label(if self.model.authoring.draft.is_some() {
                "Unsaved edits: save a revision before previewing and launching."
            } else if self.model.authoring.definition.is_some() {
                "Refresh preview when execution is available."
            } else {
                "Select a recipe to load its prompt, limits, model, workshop and seed summary."
            });
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
        let current_session = self
            .timeline
            .as_ref()
            .and_then(|t| t.top)
            .and_then(|(id, _, _)| self.model.window.rows.iter().find(|r| r.id == id))
            .or_else(|| self.model.window.rows.first())
            .and_then(|r| r.session.clone());
        let mut jump_session = None;
        if self.model.window.sessions.total > 0 {
            ui.horizontal_wrapped(|ui| {
                let sessions = &self.model.window.sessions;
                egui::ComboBox::from_id_salt("session-chooser")
                    .selected_text(current_session.as_ref().map_or_else(
                        || "Choose session".into(),
                        |s| format!("Session {}", s.ordinal),
                    ))
                    .height(240.0)
                    .show_ui(ui, |ui| {
                        for session in &sessions.items {
                            if ui
                                .selectable_label(
                                    current_session
                                        .as_ref()
                                        .is_some_and(|s| s.session == session.session),
                                    format!("Session {} · {}", session.ordinal, session.session),
                                )
                                .clicked()
                            {
                                jump_session = Some(session.sequence);
                            }
                        }
                    });
                let mut page = None;
                if sessions.start > 0 && ui.small_button("Earlier sessions").clicked() {
                    page = Some(sessions.start.saturating_sub(WINDOW_ROWS));
                }
                if sessions.start + sessions.items.len() < sessions.total
                    && ui.small_button("More sessions").clicked()
                {
                    page = Some(sessions.start + WINDOW_ROWS);
                }
                ui.small(if sessions.complete {
                    "Recorded sessions"
                } else {
                    "Session list incomplete · loading history"
                });
                if let Some(start) = page {
                    self.model.command(Action::Sessions(start));
                }
            });
        }
        if self.model.archive_waiting {
            ui.label("Waiting for archive publication…");
        }
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
        let view = if self.model.preferences.disclosure == Disclosure::Full {
            View::Full
        } else {
            View::Readable
        };
        let total = self.model.window.total;
        let width = ui.available_width();
        let mut range = (0, 32);
        let mut selection = None;
        let mut expansion = None;
        let mut checkpoint_selection = None;
        let mut memory_source = None;
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("timeline", self.model.selected.clone()))
            .stick_to_bottom(self.model.following);
        let mut view_anchor = None;
        let mut anchor = None;
        if let Some(last) = &self.timeline {
            if last.run != self.model.selected {
                self.heights.clear();
                self.view_anchor = None;
                self.requested_range = None;
                self.navigation = None;
                self.navigation_sent = false;
            } else if last.connection != self.model.tag.connection {
                self.navigation = self
                    .navigation
                    .or_else(|| last.sequence.zip(last.top.map(|(_, _, within)| within)));
                self.navigation_sent = false;
                self.view_anchor = None;
                self.heights.clear();
                self.requested_range = None;
            } else {
                let order_changed =
                    last.total != total
                        || self.model.window.rows.iter().enumerate().any(|(i, row)| {
                            !self.heights.matches(self.model.window.start + i, row.id)
                        });
                if !self.model.following
                    && self.navigation.is_none()
                    && let Some((id, index, within)) = last.top
                {
                    let located = self
                        .model
                        .window
                        .rows
                        .iter()
                        .position(|row| row.id == id)
                        .map(|i| self.model.window.start + i);
                    if (last.view != view || (order_changed && located.is_none()))
                        && self.view_anchor.is_none()
                    {
                        view_anchor = Some(id);
                        self.view_anchor = Some(id);
                    } else {
                        anchor = Some((located.unwrap_or(index), within));
                    }
                }
                if last.width != width
                    || last.disclosure != self.model.preferences.disclosure
                    || order_changed
                {
                    self.heights.clear();
                }
            }
        }
        if let Some((id, at)) = self.model.window.anchor
            && self.view_anchor == Some(id)
        {
            self.view_anchor = None;
            anchor = Some((at, 0.0));
        }
        if let Some((id, within)) = self.timeline_anchor.take()
            && let Some(index) = self.model.window.rows.iter().position(|row| row.id == id)
        {
            anchor = Some((self.model.window.start + index, within));
        }
        if let Some(sequence) = jump_session {
            self.model.location = None;
            self.model.following = false;
            self.navigation = Some((sequence, 0.0));
            self.navigation_sent = false;
            self.view_anchor = None;
            anchor = None;
        }
        if self.model.following {
            self.navigation = None;
        }
        if let Some((sequence, within)) = self.navigation {
            if !self.navigation_sent && self.model.connection == ConnectionState::Connected {
                self.model.command(Action::Locate(sequence, view));
                self.navigation_sent = true;
            }
            if let Some((located, id)) = self.model.location
                && located == sequence
            {
                self.model.location = None;
                // History can replace the location reply's window before a
                // frame renders. Resolve its stable row identity in the current
                // window, or keep the jump pending and ask for that target again.
                if let Some(offset) = self.model.window.rows.iter().position(|row| row.id == id) {
                    anchor = Some((self.model.window.start + offset, within));
                    self.navigation = None;
                } else {
                    self.model.command(Action::Locate(sequence, view));
                }
            }
        }
        if let Some((index, within)) = anchor {
            area = area.vertical_scroll_offset((self.heights.top(index) + within).max(0.0));
        }
        let mut top = None;
        let response = area.show_viewport(ui, |ui, viewport| {
            // Establish the whole content rectangle before placing a cold
            // viewport in its middle; otherwise egui treats that first child
            // as the beginning and counts the skipped prefix twice.
            ui.set_height(self.heights.top(total));
            let origin = ui.max_rect().min;
            let start = self.heights.at(viewport.min.y, total);
            let mut y = self.heights.top(start);
            let mut end = start;
            for i in start..total.min(start + WINDOW_ROWS) {
                if y > viewport.max.y { break; }
                self.metrics.laid_out_rows += 1;
                let row = self.model.window.rows.get(i.saturating_sub(self.model.window.start))
                    .filter(|_| i >= self.model.window.start);
                if top.is_none() && let Some(row) = row { top = Some((row.id, i, viewport.min.y - y)); }
                let rect = egui::Rect::from_min_size(origin + egui::vec2(0.0, y), egui::vec2(ui.available_width(), 0.0));
                let response = ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    if let Some(row) = row {
                        ui.push_id(row.id, |ui| {
                            let expanded = self.model.expanded.contains(&row.id);
                            let nested = row.nested;
                            let trace = self.model.trace.get(&row.first_sequence);
                            let sections = if view == View::Readable {
                                trace.map_or(row.sections.as_slice(), |v| v.sections.as_slice())
                            } else { row.sections.as_slice() };
                            egui::Frame::new().inner_margin(10).corner_radius(5)
                                .outer_margin(egui::Margin { left: if nested { 22 } else { 0 }, ..Default::default() })
                                .fill(if self.model.selected_row == Some(row.id) { Color32::from_rgb(31, 57, 57) } else { Color32::from_rgb(29, 34, 42) })
                                .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                if let Some(parent) = &row.parent { ui.small(parent); }
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(&row.title).color(ACCENT).strong());
                                    if let Some(files) = row.checkpoint.as_ref().and_then(|session| self.model.checkpoint_files.get(session)) { ui.label(files); }
                                    let more = sections.len() > 6 || sections.iter().any(|s| s.text.len() > 512)
                                        || self.model.preferences.disclosure == Disclosure::Compact;
                                    if more || expanded {
                                    let button = ui.small_button(if expanded { "Show less" } else { "Show more" });
                                    button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{} activity {}", if expanded { "Collapse" } else { "Expand" }, row.first_sequence)));
                                    if button.clicked() { expansion = Some((row.id, viewport.min.y - y)); }
                                    }
                                    let button = ui.small_button("Inspect source");
                                    button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Inspect source {}", row.first_sequence)));
                                    if button.clicked() { selection = Some((row.id, row.first_sequence)); }
                                });
                                if !row.marker && !row.status.is_empty() {
                                    if row.status.starts_with("Failed") || row.status.starts_with("Timed out") {
                                        ui.label(RichText::new(&row.status).small().color(Color32::LIGHT_RED));
                                    } else { ui.small(&row.status); }
                                }
                                let show = self.model.preferences.disclosure != Disclosure::Compact || expanded;
                                if show {
                                    if sections.is_empty() && !row.marker && !row.nested && !row.excerpt.is_empty() { text(ui, &row.excerpt, &mut self.metrics); }
                                    let limit = if expanded || view == View::Full { 4096 } else { 512 };
                                    // The backend prepares a bounded window. A single block
                                    // also caps its displayed sections; exact continuation
                                    // remains available through the paged source inspector.
                                    let count = if expanded { 16 } else { 6 };
                                    for section in sections.iter().take(count) {
                                        ui.strong(brief(&section.label, 120));
                                        let value = crate::transcript::excerpt(&section.text, limit);
                                        trace_text(ui, &section.label, &value, &mut self.metrics);
                                        if value.len() < section.text.len() { ui.small("Excerpt · Show more or inspect source for continuation"); }
                                    }
                                    if sections.len() > count { ui.small("More recorded sections · Show more or inspect source"); }
                                }
                                if let Some(kind) = &row.trace {
                                    if let Some(trace) = trace {
                                        if !trace.files.is_empty() {
                                            ui.label(&trace.files);
                                            if row.checkpoint.as_ref().is_none_or(|session| !self.model.checkpoint_files.contains_key(session)) { ui.small("Change classification unavailable"); }
                                        }
                                        if !trace.notice.is_empty() { ui.small(&trace.notice); }
                                    } else { ui.small("Loading boundary content…"); }
                                    ui.horizontal_wrapped(|ui| match kind {
                                        crate::transcript::TraceContent::Checkpoint(session) => {
                                            if ui.small_button("Compare memory").clicked() { checkpoint_selection = Some((session.clone(), Inspector::Memory)); }
                                            if ui.small_button("Browse files").clicked() { checkpoint_selection = Some((session.clone(), Inspector::Files)); }
                                        }
                                        crate::transcript::TraceContent::Incoming(reference) => {
                                            if ui.small_button("Read full memory").clicked() { memory_source = Some(reference.clone()); }
                                        }
                                    });
                                }
                                if row.needs_loading { ui.small("Earlier content is loading or unavailable · inspect source"); }
                                if view == View::Full { ui.small(&row.sources); }
                            });
                        });
                    } else { ui.set_min_height(112.0); ui.label("Loading activity page…"); }
                });
                let height = response.response.rect.height().max(24.0) + 8.0;
                if let Some(row) = row && self.heights.set(i, row.id, height) { ui.ctx().request_repaint(); }
                y += height; end = i + 1;
            }
            range = (start.saturating_sub(2), (end - start + 4).min(WINDOW_ROWS));
        });
        // egui applies wheel/scrollbar movement after the viewport callback.
        // Anchor the resulting offset, not the pre-input viewport, or the next
        // frame would undo scrolling.
        let at = self.heights.at(response.state.offset.y, total);
        if let Some(row) = self
            .model
            .window
            .rows
            .get(at.saturating_sub(self.model.window.start))
            .filter(|_| at >= self.model.window.start)
        {
            top = Some((row.id, at, response.state.offset.y - self.heights.top(at)));
        }
        self.timeline = Some(TimelineView {
            run: self.model.selected.clone(),
            connection: self.model.tag.connection,
            sequence: top.and_then(|(id, _, _)| {
                self.model
                    .window
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| r.first_sequence)
            }),
            view,
            width,
            disclosure: self.model.preferences.disclosure,
            total,
            top,
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
        if let Some((id, within)) = expansion {
            self.timeline_anchor = Some((id, within));
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
        if let Some((session, inspector)) = checkpoint_selection {
            self.model.inspect_checkpoint(session, inspector);
        }
        if let Some(reference) = memory_source
            && let Some(run) = self.model.selected.clone()
        {
            self.model.inspector = Inspector::Files;
            self.model.show_inspector = true;
            self.model.inspect_action(Action::Retained(run, reference));
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
        let continuous = self
            .model
            .run
            .as_ref()
            .and_then(|r| r.value.harness.as_ref())
            .is_some_and(|h| h.harness.compaction.is_some());
        if continuous && self.model.inspector == Inspector::Memory {
            self.model.inspector = Inspector::Files;
        }
        ui.horizontal(|ui| {
            for (label, tab) in [
                ("Details", Inspector::Details),
                ("Memory", Inspector::Memory),
                ("Files", Inspector::Files),
            ] {
                if continuous && tab == Inspector::Memory {
                    continue;
                }
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
            if self.model.inspector != Inspector::Details && !continuous {
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
                    } else if let Some(run) = &self.model.run {
                        if !run.harness.is_empty() {
                            ui.strong("Archived harness");
                            for line in &run.harness { ui.label(line); }
                            ui.separator();
                        }
                        text(ui, &run.source.text, &mut self.metrics);
                    }
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
                    if continuous {
                        ui.strong("Final stopped workspace");
                        ui.small("Retained files and exact capture index. Files unavailable to capture are listed in the index. Compaction does not publish checkpoints.");
                        if ui.button("Load final workspace files").clicked() && let Some(run) = self.model.selected.clone() { self.model.inspect_action(Action::FinalFiles(run)); }
                        if let Some((files, complete)) = &self.model.final_files {
                            if !complete { ui.colored_label(egui::Color32::YELLOW, "Listing limit reached; this listing is incomplete."); }
                            if files.is_empty() { ui.label("No final workspace publication found in the inspected archive prefix."); }
                            ui.horizontal(|ui| {
                                if ui.add_enabled(self.model.file_page > 0, egui::Button::new("Previous files")).clicked() { self.model.file_page = self.model.file_page.saturating_sub(20); }
                                if ui.add_enabled(self.model.file_page + 20 < files.len(), egui::Button::new("Next files")).clicked() { self.model.file_page += 20; }
                            });
                            for file in files.iter().skip(self.model.file_page).take(20) {
                                ui.separator(); ui.strong(brief(&file.name,120));
                                ui.small(format!("{} · {}",file.kind,crate::preview::bytes(file.content.size_bytes)));
                                ui.push_id(file.artifact_id.as_str(), |ui| { if ui.button(if crate::media::image_name(&file.name) { "Open image" } else { "Open file" }).clicked() { open = Some(file.artifact_id.clone()); } });
                            }
                        }
                    } else if let Some(checkpoint) = &self.model.checkpoint {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(self.model.file_page > 0, egui::Button::new("Previous files")).clicked() { self.model.file_page = self.model.file_page.saturating_sub(20); }
                            if ui.add_enabled(self.model.file_page + 20 < checkpoint.files.len(), egui::Button::new("Next files")).clicked() { self.model.file_page += 20; }
                        });
                        for file in checkpoint.files.iter().skip(self.model.file_page).take(20) {
                            ui.separator(); ui.strong(brief(&file.name, 120)); ui.small(file_kind(&file.kind));
                            match &file.content {
                                ContentAvailability::Available { artifact_id, content } => {
                                    ui.small(format!("{} · session {}", crate::preview::bytes(content.size_bytes), checkpoint.checkpoint.session_id));
                                    ui.push_id(artifact_id.as_str(), |ui| {
                                        let response = ui.button(if crate::media::image_name(&file.name) { "Open image" } else { "Open file" });
                                        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Inspect file {artifact_id}")));
                                        if response.clicked() { open = Some(artifact_id.clone()); }
                                        egui::CollapsingHeader::new("Details").show(ui, |ui| {
                                            ui.label(brief(&file.name, 4096));
                                            ui.label(format!("{} bytes · Recorded kind: {}", content.size_bytes, brief(&file.kind, 120)));
                                            ui.label(format!("SHA-256: {}", content.sha256));
                                            ui.label(format!("Artifact ID: {artifact_id}"));
                                        });
                                    });
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
        "0" | "\0" | "regular" => "file",
        "1" | "hard_link" => "hard link",
        "2" | "symlink" => "symbolic link",
        "5" | "directory" => "directory",
        _ => "unknown file kind",
    }
}
fn brief(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn trace_text(ui: &mut egui::Ui, label: &str, value: &str, metrics: &mut FrameMetrics) {
    let remaining = TEXT_BYTES.saturating_sub(metrics.text_bytes);
    let visible = brief(value, remaining);
    metrics.text_bytes += visible.len();
    let mut rich = RichText::new(visible).color(Color32::from_gray(220));
    if !matches!(
        label,
        "Preview"
            | "Recorded thinking"
            | "Model response"
            | "Submitted memento"
            | "Memory received"
            | "Published memento"
    ) {
        rich = rich.monospace();
    }
    ui.add(egui::Label::new(rich).wrap());
    if visible.len() < value.len() {
        ui.small("Text viewport limit · inspect source for continuation");
    }
}
fn prose(ui: &mut egui::Ui, value: &str, metrics: &mut FrameMetrics) {
    trace_text(ui, "Preview", value, metrics);
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
