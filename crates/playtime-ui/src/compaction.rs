//! Editable strings preserve unfinished numeric input through validation and recovery.
use playtime_protocol::api::CompactionSettings;

#[derive(Clone, Debug)]
pub struct Draft {
    pub trigger: String,
    pub recent: String,
    pub summary: String,
    pub instructions: String,
    pub allow_finish_run: bool,
}
impl From<CompactionSettings> for Draft {
    fn from(s: CompactionSettings) -> Self {
        Self {
            trigger: s.trigger_percent.to_string(),
            recent: s.recent_exchanges.to_string(),
            summary: s.summary_tokens.to_string(),
            instructions: s.instructions,
            allow_finish_run: s.allow_finish_run,
        }
    }
}
impl Draft {
    pub fn value(&self) -> Result<CompactionSettings, String> {
        let s = CompactionSettings {
            schema_version: 1,
            trigger_percent: self
                .trigger
                .parse()
                .map_err(|_| "Enter a whole-number context trigger.")?,
            recent_exchanges: self
                .recent
                .parse()
                .map_err(|_| "Enter a whole-number retained exchange count.")?,
            summary_tokens: self
                .summary
                .parse()
                .map_err(|_| "Enter a whole-number summary limit.")?,
            instructions: self.instructions.clone(),
            allow_finish_run: self.allow_finish_run,
        };
        if let Some(error) = s.validation_error() {
            return Err(error.into());
        }
        Ok(s)
    }
    /// `finish_choice` shows the finish_run option when the service supports it.
    pub fn draw(&mut self, ui: &mut egui::Ui, finish_choice: bool) {
        ui.strong("Compaction settings");
        for (label, value) in [
            ("Context trigger (%)", &mut self.trigger),
            ("Recent complete exchanges", &mut self.recent),
            ("Summary limit (tokens)", &mut self.summary),
        ] {
            ui.horizontal(|ui| {
                let label = ui.label(label);
                ui.add(
                    egui::TextEdit::singleline(value)
                        .desired_width(90.0)
                        .char_limit(12),
                )
                .labelled_by(label.id);
            });
        }
        let label = ui.label("Compaction instructions");
        ui.add(
            egui::TextEdit::multiline(&mut self.instructions)
                .desired_width(f32::INFINITY)
                .desired_rows(5)
                .char_limit(65536),
        )
        .labelled_by(label.id);
        ui.small("Summaries use the selected model and whole-run budget. Compaction can start before the trigger when a summary request needs room. The workshop stays alive; only the final stopped workspace is captured.");
        if finish_choice {
            ui.checkbox(
                &mut self.allow_finish_run,
                "The model can end the run with finish_run",
            );
            if !self.allow_finish_run {
                ui.small("Without finish_run, the run lasts until its time or token limit.");
            }
        }
    }
}
