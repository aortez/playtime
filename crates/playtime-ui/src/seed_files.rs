//! The recipe editor's seed-file choice. The backend reads and hashes local
//! files; this module holds the draft and draws it. Seed files are the run's
//! starting files under `/work/seeds`, separate from the numeric sampling seed.
use playtime_client::words::thousands;
use playtime_protocol::api::*;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Choice {
    /// Keep the source revision's files.
    #[default]
    Source,
    Empty,
    Uploaded,
}

#[derive(Clone, Debug)]
pub struct Draft {
    pub choice: Choice,
    /// The service's bounds, narrowed by the source recipe's workspace. The
    /// service gives only a generic refusal, so the editor checks them first.
    pub max_files: usize,
    pub max_bytes: u64,
    /// Files in the order they were added. They stay while another choice is
    /// selected, so switching back doesn't lose them.
    pub files: Vec<SeedUpload>,
    /// Added files whose destination was already listed, each waiting for an
    /// explicit Replace or Skip.
    pub conflicts: Vec<SeedUpload>,
    /// Why the last files couldn't be added.
    pub errors: Vec<String>,
    /// The path field's text.
    pub path: String,
    /// A file chooser or read is running.
    pub reading: bool,
}

/// Local files the editor asks the backend for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    Choose,
    Read(Vec<PathBuf>),
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            choice: Choice::default(),
            max_files: SEED_FILES,
            max_bytes: SEED_BYTES as u64,
            files: vec![],
            conflicts: vec![],
            errors: vec![],
            path: String::new(),
            reading: false,
        }
    }
}

impl Draft {
    /// Seed files and their directory may take at most half of the recipe's
    /// workspace bytes and entries. Uploads are named after their files, so the
    /// only directory is `/work/seeds` itself.
    pub fn new(definition: &RecipeDefinition) -> Self {
        let recipe = &definition.inherited["recipe"];
        let mut draft = Self::default();
        if let Some(bytes) = recipe
            .pointer("/parameters/workshop/workspace_bytes")
            .and_then(Value::as_u64)
        {
            draft.max_bytes = draft.max_bytes.min(bytes / 2);
        }
        if let Some(entries) = recipe.get("workspace_entries").and_then(Value::as_u64) {
            let files = (entries / 2).saturating_sub(1);
            draft.max_files = draft.max_files.min(files.try_into().unwrap_or(usize::MAX));
        }
        draft
    }
    /// The request's seed field: None keeps the source's files.
    pub fn selection(&self) -> Result<Option<SeedSelection>, String> {
        match self.choice {
            Choice::Source => Ok(None),
            Choice::Empty => Ok(Some(SeedSelection::Empty)),
            Choice::Uploaded => {
                if self.files.is_empty() {
                    return Err("Add at least one seed file, or choose Start empty.".into());
                }
                if !self.conflicts.is_empty() {
                    return Err("Replace or skip each file whose name is already listed.".into());
                }
                if self.files.len() > self.max_files {
                    return Err(format!(
                        "This recipe allows at most {} seed files.",
                        self.max_files
                    ));
                }
                if self.total() > self.max_bytes {
                    return Err(format!(
                        "This recipe allows at most {} bytes of seed files.",
                        thousands(self.max_bytes)
                    ));
                }
                Ok(Some(SeedSelection::Uploaded {
                    files: self.files.clone(),
                }))
            }
        }
    }
    fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size_bytes).sum()
    }
    /// Adds read files. A file whose destination is already listed waits for
    /// an explicit choice instead of silently replacing the listed one.
    pub fn add(&mut self, results: Vec<Result<SeedUpload, String>>) {
        self.errors.clear();
        for result in results {
            match result {
                Ok(file) => {
                    if self.files.iter().any(|f| f.path == file.path)
                        || self.conflicts.iter().any(|f| f.path == file.path)
                    {
                        self.conflicts.push(file);
                    } else {
                        self.files.push(file);
                    }
                    self.choice = Choice::Uploaded;
                }
                Err(error) => self.errors.push(error),
            }
        }
    }
    pub fn replace(&mut self, index: usize) {
        if index < self.conflicts.len() {
            let file = self.conflicts.remove(index);
            if let Some(listed) = self.files.iter_mut().find(|f| f.path == file.path) {
                *listed = file;
            }
        }
    }
    pub fn skip(&mut self, index: usize) {
        if index < self.conflicts.len() {
            self.conflicts.remove(index);
        }
    }
    pub fn remove(&mut self, index: usize) {
        if index < self.files.len() {
            self.files.remove(index);
        }
    }
    fn summary(&self, source: &[RecipeSeed]) -> String {
        match self.choice {
            Choice::Source if source.is_empty() => "none in the source recipe".into(),
            Choice::Source => format!("{} from the source recipe", files(source.len())),
            Choice::Empty => "start empty".into(),
            Choice::Uploaded => format!("{} uploaded", files(self.files.len())),
        }
    }
}

fn files(count: usize) -> String {
    if count == 1 {
        "1 file".into()
    } else {
        format!("{count} files")
    }
}

/// How a definition's seed files came to be, for read-only views.
pub fn origin_label(origin: Option<SeedOrigin>) -> &'static str {
    match origin {
        None => "Seed files from the catalog recipe",
        Some(SeedOrigin::Empty) => "Started without seed files",
        Some(SeedOrigin::Uploaded) => "Uploaded seed files",
    }
}

/// An absolute path from the path field, with `~/` meaning the home directory.
fn typed_path(text: &str) -> Result<PathBuf, String> {
    let text = text.trim();
    let path = match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .ok_or("HOME is not set; enter the full path.")?,
        None => PathBuf::from(text),
    };
    if !path.is_absolute() {
        return Err("Enter an absolute path, such as ~/notes.txt.".into());
    }
    Ok(path)
}

/// A button whose accessible name says which file it acts on.
fn named_button(ui: &mut egui::Ui, enabled: bool, text: &str, path: &str) -> egui::Response {
    let button = ui.add_enabled(enabled, egui::Button::new(text));
    button.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, format!("{text} {path}"))
    });
    button
}

/// Draws the section and returns any files it asks the backend to read.
pub fn draw(
    ui: &mut egui::Ui,
    draft: &mut Draft,
    source: &[RecipeSeed],
    source_origin: Option<SeedOrigin>,
    busy: bool,
) -> Option<Request> {
    let mut request = None;
    egui::CollapsingHeader::new(format!("Seed files · {}", draft.summary(source)))
        .id_salt("seed-files")
        .show(ui, |ui| {
            ui.small("Files the run finds in /work/seeds. They're separate from the numeric sampling seed.");
            ui.add_enabled_ui(!busy, |ui| {
                ui.radio_value(&mut draft.choice, Choice::Source, "Use source recipe files");
                ui.radio_value(&mut draft.choice, Choice::Empty, "Start empty");
                ui.radio_value(&mut draft.choice, Choice::Uploaded, "Use uploaded files");
            });
            match draft.choice {
                Choice::Source => {
                    if source.is_empty() {
                        ui.label("The source recipe has no seed files.");
                    } else {
                        ui.label(origin_label(source_origin));
                        for seed in source {
                            ui.label(format!(
                                "{} · {} bytes",
                                seed.path,
                                thousands(seed.size_bytes)
                            ));
                        }
                    }
                }
                Choice::Empty => {
                    ui.label("The run starts without seed files.");
                }
                Choice::Uploaded => {
                    request = uploaded(ui, draft, busy);
                }
            }
        });
    request
}

fn uploaded(ui: &mut egui::Ui, draft: &mut Draft, busy: bool) -> Option<Request> {
    let mut request = None;
    let idle = !busy && !draft.reading;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(idle, egui::Button::new("Add files…"))
            .clicked()
        {
            request = Some(Request::Choose);
        }
        let label = ui.label("or path");
        ui.add_enabled(
            idle,
            egui::TextEdit::singleline(&mut draft.path)
                .hint_text("~/notes.txt")
                .desired_width(220.0),
        )
        .labelled_by(label.id);
        if ui
            .add_enabled(
                idle && !draft.path.trim().is_empty(),
                egui::Button::new("Add path"),
            )
            .clicked()
        {
            match typed_path(&draft.path) {
                Ok(path) => {
                    draft.path.clear();
                    request = Some(Request::Read(vec![path]));
                }
                Err(error) => draft.errors = vec![error],
            }
        }
    });
    if draft.reading {
        ui.label("Reading files…");
    }
    for error in &draft.errors {
        ui.colored_label(egui::Color32::YELLOW, error);
    }
    let mut decision = None;
    for (index, file) in draft.conflicts.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "{} is already listed. The new file has {} bytes.",
                    file.path,
                    thousands(file.size_bytes)
                ),
            );
            if named_button(ui, !busy, "Replace", &file.path).clicked() {
                decision = Some((index, true));
            }
            if named_button(ui, !busy, "Skip", &file.path).clicked() {
                decision = Some((index, false));
            }
        });
    }
    match decision {
        Some((index, true)) => draft.replace(index),
        Some((index, false)) => draft.skip(index),
        None => (),
    }
    let mut removed = None;
    for (index, file) in draft.files.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            if named_button(ui, !busy, "Remove", &file.path).clicked() {
                removed = Some(index);
            }
            ui.label(format!(
                "{} · {} bytes",
                file.path,
                thousands(file.size_bytes)
            ));
        });
        ui.small(format!("SHA-256 {}", file.sha256));
    }
    if let Some(index) = removed {
        draft.remove(index);
    }
    ui.label(format!(
        "{} of {} files · {} of {} bytes · at most {} bytes each",
        draft.files.len(),
        draft.max_files,
        thousands(draft.total()),
        thousands(draft.max_bytes),
        thousands(SEED_FILE_BYTES as u64),
    ));
    if let Err(error) = draft.selection() {
        ui.colored_label(egui::Color32::YELLOW, error);
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> SeedUpload {
        SeedUpload {
            path: path.into(),
            size_bytes: content.len() as u64,
            sha256: format!("{:064}", content.len()),
            content: content.into(),
        }
    }

    #[test]
    fn a_listed_name_waits_for_an_explicit_replace_or_skip() {
        let mut draft = Draft::default();
        assert_eq!(draft.selection(), Ok(None));
        draft.choice = Choice::Uploaded;
        assert!(draft.selection().is_err());
        draft.add(vec![
            Ok(file("a.txt", "one")),
            Err("b.bin isn't UTF-8 text".into()),
        ]);
        assert_eq!(draft.errors, ["b.bin isn't UTF-8 text"]);
        draft.add(vec![Ok(file("a.txt", "two"))]);
        assert!(draft.errors.is_empty());
        assert_eq!(draft.files[0].content, "one");
        assert!(draft.selection().is_err());
        draft.replace(0);
        assert_eq!(draft.files.len(), 1);
        assert_eq!(draft.files[0].content, "two");
        draft.add(vec![Ok(file("a.txt", "three"))]);
        draft.skip(0);
        assert_eq!(draft.files[0].content, "two");
        let Ok(Some(SeedSelection::Uploaded { files })) = draft.selection() else {
            panic!("expected an uploaded set");
        };
        assert_eq!(files, draft.files);
        draft.choice = Choice::Empty;
        assert_eq!(draft.selection(), Ok(Some(SeedSelection::Empty)));
        // Switching back keeps the listed files.
        draft.choice = Choice::Uploaded;
        assert_eq!(draft.files.len(), 1);
        draft.remove(0);
        assert!(draft.selection().is_err());
    }

    #[test]
    fn counts_and_totals_are_checked_before_saving() {
        let definition = |workspace_bytes: u64, workspace_entries: u64| RecipeDefinition {
            schema_version: 1,
            execution_recipe_id: RecipeId::new("curiosity").unwrap(),
            definition_digest: "a".repeat(64),
            saved: None,
            source: None,
            name: "Curiosity".into(),
            prompt: "Explore.".into(),
            inherited: serde_json::json!({"recipe": {
                "parameters": {"workshop": {"workspace_bytes": workspace_bytes}},
                "workspace_entries": workspace_entries,
            }}),
            model_registration: None,
            seeds: vec![],
            seed_origin: None,
        };
        // A large workspace leaves the service's own bounds.
        let draft = Draft::new(&definition(48 << 20, 4096));
        assert_eq!((draft.max_files, draft.max_bytes), (16, 524_288));
        // Files and their directory take at most half of a small one.
        let draft = Draft::new(&definition(1001, 17));
        assert_eq!((draft.max_files, draft.max_bytes), (7, 500));
        let mut draft = Draft::default();
        draft.add(
            (0..=SEED_FILES)
                .map(|i| Ok(file(&format!("{i}.txt"), "x")))
                .collect(),
        );
        assert_eq!(draft.choice, Choice::Uploaded);
        assert!(
            draft
                .selection()
                .unwrap_err()
                .contains("at most 16 seed files")
        );
        draft.files.truncate(5);
        for f in &mut draft.files {
            f.size_bytes = SEED_FILE_BYTES as u64;
        }
        assert!(
            draft
                .selection()
                .unwrap_err()
                .contains("at most 524,288 bytes")
        );
        draft.files.truncate(4);
        assert!(draft.selection().is_ok());
    }

    #[test]
    fn the_path_field_takes_absolute_and_home_paths() {
        assert_eq!(typed_path(" /tmp/a.txt "), Ok(PathBuf::from("/tmp/a.txt")));
        assert!(typed_path("notes.txt").is_err());
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(typed_path("~/a.txt"), Ok(PathBuf::from(home).join("a.txt")));
        }
    }
}
