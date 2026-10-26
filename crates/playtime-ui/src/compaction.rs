//! Editable strings preserve unfinished numeric input through validation and recovery.
use playtime_protocol::api::CompactionSettings;

#[derive(Clone, Debug)]
pub struct Draft {
    pub trigger: String,
    pub recent: String,
    pub summary: String,
    pub instructions: String,
}
impl From<CompactionSettings> for Draft {
    fn from(s: CompactionSettings) -> Self {
        Self {
            trigger: s.trigger_percent.to_string(),
            recent: s.recent_exchanges.to_string(),
            summary: s.summary_tokens.to_string(),
            instructions: s.instructions,
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
        };
        if let Some(error) = s.validation_error() {
            return Err(error.into());
        }
        Ok(s)
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) {
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
        ui.small("Summaries use the selected model and whole-run budget. The workshop stays alive; only the final stopped workspace is captured.");
    }
}
