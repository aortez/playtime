//! Pure presentation state and explicit user intents. No execution authority.
use crate::transcript::{SourcePage, View, Window};
use playtime_client::{config::Options, state::LaunchReceipt};
use playtime_core::{RunId, RunMode, SessionId};
use playtime_protocol::api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tag {
    pub connection: u64,
    pub selection: u64,
    pub preview: u64,
    pub inspection: u64,
    pub launch: u64,
    pub authoring: u64,
    pub save: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Disclosure {
    Compact,
    #[default]
    Readable,
    Full,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Inspector {
    #[default]
    Details,
    Memory,
    Files,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Error(String),
}
impl ConnectionState {
    pub fn label(&self) -> &str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Reconnecting => "Reconnecting",
            Self::Error(_) => "Connection error",
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Mutation {
    #[default]
    Idle,
    Recording,
    Submitting,
    Accepted,
    Unresolved(String),
    Rejected(String),
}
#[derive(Clone, Debug)]
pub struct PreviewSection {
    pub title: &'static str,
    pub lines: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct PreviewView {
    pub value: Preview,
    pub sections: Vec<PreviewSection>,
    pub prompt: String,
    pub protocol: String,
    pub inventory: String,
    pub revision: String,
    pub source: SourcePage,
}
#[derive(Clone, Debug)]
pub struct RunViewData {
    pub value: RunView,
    pub summary: Vec<String>,
    pub source: SourcePage,
}
#[derive(Clone, Debug)]
pub struct MemoryView {
    pub incoming: String,
    pub outgoing: String,
    pub evidence: String,
    pub diff: String,
    pub notice: String,
}
#[derive(Clone, Debug)]
pub struct ContentView {
    pub artifact: Artifact,
    pub source: Option<SourcePage>,
    pub image: Option<Arc<crate::media::Pixels>>,
    pub notice: String,
    pub animated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub schema_version: u32,
    pub disclosure: Disclosure,
    pub left_width: f32,
    pub right_width: f32,
    pub service_id: Option<ServiceId>,
    pub run_id: Option<RunId>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema_version: 1,
            disclosure: Disclosure::Readable,
            left_width: 240.0,
            right_width: 330.0,
            service_id: None,
            run_id: None,
        }
    }
}
/// The timeline rows to send, in one view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowRequest {
    pub view: View,
    pub start: usize,
    pub count: usize,
    /// A row to locate in `view`, so changing views keeps it in place.
    pub anchor: Option<usize>,
}
#[derive(Clone, Debug)]
pub enum Action {
    Sessions(usize),
    Locate(u64, View),
    CancelNavigation,
    Trace(RunId, u64, crate::transcript::TraceContent),
    Connect(Options),
    Recipes(Option<String>),
    Definition(RecipeId),
    SaveRecipe(RecipeMutation),
    RecoverRecipe(IdempotencyKey, bool),
    RecipeSaveReceipts,
    Runs(Option<String>),
    Receipts(Option<IdempotencyKey>),
    Preview(RecipeId, RunMode),
    PreviewPage(Box<Preview>, usize),
    Launch(LaunchRequest),
    Recover(IdempotencyKey, bool),
    Cancel(RunId),
    Observe(RunId),
    StopObservation,
    Window(WindowRequest),
    Source(usize, u64, usize),
    Checkpoints(RunId, Option<String>),
    Checkpoint(RunId, SessionId),
    Content(RunId, ArtifactId, usize),
    Retained(RunId, playtime_protocol::archive::ContentRef),
    Download(RunId, ArtifactId, PathBuf),
    PlayGif(RunId, ArtifactId),
    StopGif,
    Preferences(Preferences),
}
#[derive(Clone, Debug)]
pub struct Command {
    pub tag: Tag,
    pub action: Action,
}
#[derive(Clone, Debug, Default)]
pub struct TraceView {
    pub sections: Vec<crate::transcript::Section>,
    pub files: String,
    pub notice: String,
}
pub enum Data {
    Trace(u64, TraceView),
    Connected(Capabilities, String),
    Preferences(Preferences),
    Recipes(Page<Recipe>),
    Definition(Box<crate::authoring::DefinitionView>),
    DefinitionError(String),
    RecipeSaveProgress(Mutation),
    RecipeSaved(Box<RecipeSaveResponse>),
    RecipeSaveReceipts(Vec<playtime_client::recipes::RecipeSaveReceipt>),
    Runs(Page<RunView>),
    Receipts(Vec<LaunchReceipt>, Option<IdempotencyKey>),
    Preview(Box<PreviewView>),
    PreviewError(String),
    PreviewPage(SourcePage),
    LaunchProgress(Mutation, Option<LaunchReceipt>),
    /// A fresh key's request was refused before admission and its receipt
    /// discarded. The flag marks `run_busy`.
    LaunchRefused(String, bool),
    Accepted(Box<LaunchResponse>, LaunchReceipt),
    Run(Box<RunViewData>),
    Cancellation(Box<RunViewData>),
    CancellationUnresolved(String),
    Window(Window, String, u64),
    /// The page and, by session, what each checkpoint saved.
    Checkpoints(Page<Checkpoint>, BTreeMap<String, String>),
    Checkpoint(Box<Checkpoint>, MemoryView),
    Content(ContentView),
    GifFrame(Arc<crate::media::Pixels>, tokio::sync::oneshot::Sender<()>),
    GifStopped(String),
    Source(u64, SourcePage),
    ArchiveWaiting(bool),
    Notice(String),
    Connection(ConnectionState),
    Error(String),
}
pub struct Reply {
    pub tag: Tag,
    pub run: Option<RunId>,
    pub data: Data,
}

pub struct Model {
    pub authoring: crate::authoring::Authoring,
    pub tag: Tag,
    pub connection: ConnectionState,
    pub service: Option<ServiceId>,
    pub backend: String,
    pub modes: Vec<RunMode>,
    pub endpoint: String,
    pub stale: bool,
    pub last_update: f64,
    pub clock: f64,
    pub recipes: Vec<Recipe>,
    pub recipe_next: Option<String>,
    pub recipe: Option<RecipeId>,
    pub mode: RunMode,
    pub preview: Option<PreviewView>,
    pub previewing: bool,
    pub runs: Vec<RunView>,
    pub runs_next: Option<String>,
    pub receipts: Vec<LaunchReceipt>,
    pub receipts_next: Option<IdempotencyKey>,
    pub launch: Mutation,
    pub saved: Option<LaunchReceipt>,
    pub cancellation: Mutation,
    pub selected: Option<RunId>,
    pub run: Option<RunViewData>,
    pub launcher: bool,
    pub window: Window,
    pub trace: BTreeMap<u64, TraceView>,
    /// Requested sequence and resolved row identity; never cache a row position.
    pub location: Option<(u64, usize)>,
    trace_pending: std::collections::BTreeSet<u64>,
    pub history: String,
    pub archive_waiting: bool,
    pub live_count: u64,
    pub seen_live: u64,
    pub following: bool,
    pub selected_row: Option<usize>,
    pub expanded: std::collections::BTreeSet<usize>,
    pub source: Option<SourcePage>,
    pub source_sequence: u64,
    pub checkpoints: Vec<Checkpoint>,
    pub checkpoints_next: Option<String>,
    /// What each checkpoint saved, by session, for the timeline's markers.
    pub checkpoint_files: BTreeMap<String, String>,
    pub checkpoint: Option<Checkpoint>,
    checkpoint_target: Option<SessionId>,
    pub file_page: usize,
    pub memory: Option<MemoryView>,
    pub content: Option<ContentView>,
    pub inspector: Inspector,
    pub show_inspector: bool,
    pub show_runs: bool,
    pub enlarged: bool,
    pub actual_size: bool,
    pub playing: bool,
    pub preferences: Preferences,
    pub notice: String,
    /// The run `notice` is about. Selecting a run or opening the launcher
    /// clears that run's notice.
    notice_run: Option<RunId>,
    pub destination: String,
    pub connection_file: String,
    pub commands: VecDeque<Command>,
}
impl Default for Model {
    fn default() -> Self {
        Self {
            authoring: Default::default(),
            tag: Tag::default(),
            connection: ConnectionState::Disconnected,
            service: None,
            backend: "Backend unavailable".into(),
            modes: vec![],
            endpoint: String::new(),
            stale: true,
            last_update: 0.0,
            clock: 0.0,
            recipes: vec![],
            recipe_next: None,
            recipe: None,
            mode: RunMode::Simulation,
            preview: None,
            previewing: false,
            runs: vec![],
            runs_next: None,
            receipts: vec![],
            receipts_next: None,
            launch: Mutation::Idle,
            saved: None,
            cancellation: Mutation::Idle,
            selected: None,
            run: None,
            launcher: true,
            window: Window::default(),
            trace: BTreeMap::new(),
            location: None,
            trace_pending: Default::default(),
            history: String::new(),
            archive_waiting: false,
            live_count: 0,
            seen_live: 0,
            following: true,
            selected_row: None,
            expanded: Default::default(),
            source: None,
            source_sequence: 0,
            checkpoints: vec![],
            checkpoints_next: None,
            checkpoint_files: BTreeMap::new(),
            checkpoint: None,
            checkpoint_target: None,
            file_page: 0,
            memory: None,
            content: None,
            inspector: Inspector::Details,
            show_inspector: true,
            show_runs: true,
            enlarged: false,
            actual_size: false,
            playing: false,
            preferences: Preferences::default(),
            notice: String::new(),
            notice_run: None,
            destination: String::new(),
            connection_file: String::new(),
            commands: VecDeque::new(),
        }
    }
}
impl Model {
    /// Shows `notice` in the status bar. A notice about a run lasts only while
    /// that run is open.
    pub fn notify(&mut self, notice: impl Into<String>, run: Option<RunId>) {
        self.notice = notice.into();
        self.notice_run = run;
    }
    fn clear_run_notice(&mut self) {
        if self.notice_run.take().is_some() {
            self.notice.clear();
        }
    }
    pub fn command(&mut self, action: Action) {
        if self.commands.len() >= 64 {
            self.notify(
                "Client work queue is full; retry this action after current work finishes",
                None,
            );
            match action {
                Action::Launch(_) => self.launch = Mutation::Rejected(self.notice.clone()),
                Action::Recover(..) => self.launch = Mutation::Unresolved(self.notice.clone()),
                Action::Cancel(_) => self.cancellation = Mutation::Unresolved(self.notice.clone()),
                Action::Preview(..) => self.previewing = false,
                Action::SaveRecipe(_) | Action::RecoverRecipe(..) => {
                    self.authoring.save = Mutation::Unresolved(self.notice.clone())
                }
                _ => (),
            }
            return;
        }
        if matches!(action, Action::Launch(_) | Action::Recover(..)) {
            self.tag.launch += 1;
            self.launch = if matches!(action, Action::Launch(_)) {
                Mutation::Recording
            } else {
                Mutation::Submitting
            };
        }
        if matches!(action, Action::SaveRecipe(_) | Action::RecoverRecipe(..)) {
            self.tag.save += 1;
            self.authoring.save = Mutation::Recording;
        }
        if let Action::Checkpoint(_, session) = &action {
            self.checkpoint_target = Some(session.clone());
        }
        self.commands.push_back(Command {
            tag: self.tag,
            action,
        });
    }
    pub fn connect(&mut self, options: Options) {
        if matches!(
            self.authoring.save,
            Mutation::Recording | Mutation::Submitting
        ) {
            self.authoring.save = Mutation::Unresolved("Connection changed during save; refresh save receipts and look up the original request".into());
        }
        if matches!(self.launch, Mutation::Recording | Mutation::Submitting) {
            self.launch = Mutation::Unresolved(
                "Connection changed while launching; look up the saved request before retrying"
                    .into(),
            );
        }
        self.trace_pending.clear();
        self.trace.clear();
        self.location = None;
        self.tag.connection += 1;
        self.archive_waiting = false;
        self.tag.selection += 1;
        self.tag.preview += 1;
        self.connection = ConnectionState::Connecting;
        self.stale = true;
        self.preview = None;
        self.command(Action::Connect(options));
    }
    pub fn choose_recipe(&mut self, recipe: RecipeId, mode: RunMode) {
        if self.authoring.draft.is_some() || self.authoring.pending() {
            self.notify(
                "Save or discard the recipe edits before changing the selection",
                None,
            );
            return;
        }
        self.tag.authoring += 1;
        self.authoring.definition = None;
        self.authoring.error.clear();
        if self.authoring.enabled && mode == RunMode::Experiment {
            self.command(Action::Definition(recipe.clone()));
        }
        self.recipe = Some(recipe.clone());
        self.mode = mode;
        self.preview = None;
        self.previewing = true;
        self.tag.preview += 1;
        if !matches!(
            self.launch,
            Mutation::Recording | Mutation::Submitting | Mutation::Unresolved(_)
        ) {
            self.launch = Mutation::Idle;
        }
        self.command(Action::Preview(recipe, mode));
    }
    pub fn can_launch(&self) -> bool {
        self.connection == ConnectionState::Connected
            && self.authoring.draft.is_none()
            && !self.authoring.pending()
            && self.modes.contains(&self.mode)
            && !matches!(
                self.launch,
                Mutation::Recording | Mutation::Submitting | Mutation::Unresolved(_)
            )
            && self.preview.as_ref().is_some_and(|p| {
                p.value.readiness.ready
                    && Some(&p.value.recipe_id) == self.recipe.as_ref()
                    && p.value.mode == self.mode
            })
    }
    pub fn launch(&mut self) {
        if !self.can_launch() {
            return;
        }
        let p = &self.preview.as_ref().unwrap().value;
        let request = LaunchRequest {
            recipe_id: p.recipe_id.clone(),
            mode: p.mode,
            preview_revision: p.preview_revision.clone(),
        };
        self.launch = Mutation::Recording;
        self.command(Action::Launch(request));
    }
    pub fn select_run(&mut self, run: RunId) {
        self.archive_waiting = false;
        self.tag.selection += 1;
        self.clear_run_notice();
        self.selected = Some(run.clone());
        self.launcher = false;
        self.run = None;
        self.window = Window::default();
        self.trace.clear();
        self.location = None;
        self.trace_pending.clear();
        self.selected_row = None;
        self.expanded.clear();
        self.source = None;
        self.checkpoints.clear();
        self.checkpoint_files.clear();
        self.checkpoint = None;
        self.checkpoint_target = None;
        self.checkpoints_next = None;
        self.memory = None;
        self.content = None;
        self.playing = false;
        self.cancellation = Mutation::Idle;
        self.following = true;
        self.live_count = 0;
        self.seen_live = 0;
        self.history = "Attaching live stream and loading history".into();
        self.preferences.service_id = self.service.clone();
        self.preferences.run_id = Some(run.clone());
        self.command(Action::Preferences(self.preferences.clone()));
        self.command(Action::Observe(run.clone()));
        self.command(Action::Checkpoints(run, None));
    }
    pub fn cancel(&mut self) {
        if self.connection != ConnectionState::Connected
            || matches!(
                self.cancellation,
                Mutation::Submitting | Mutation::Recording
            )
        {
            return;
        }
        if let Some(run) = self.selected.clone() {
            self.cancellation = Mutation::Submitting;
            self.command(Action::Cancel(run));
        }
    }
    pub fn new_experiment(&mut self) {
        self.archive_waiting = false;
        self.tag.selection += 1;
        self.clear_run_notice();
        self.launcher = true;
        self.command(Action::StopObservation);
        self.stop_animation();
    }
    pub fn inspect(&mut self, id: usize, sequence: u64) {
        self.stop_animation();
        self.tag.inspection += 1;
        self.following = false;
        self.selected_row = Some(id);
        self.source_sequence = sequence;
        self.source = None;
        self.inspector = Inspector::Details;
        self.show_inspector = true;
        self.command(Action::Source(id, sequence, 0));
    }
    pub fn jump_live(&mut self) {
        self.location = None;
        self.command(Action::CancelNavigation);
        self.following = true;
        self.seen_live = self.live_count;
    }
    pub fn new_activity(&self) -> u64 {
        self.live_count.saturating_sub(self.seen_live)
    }
    pub fn apply(&mut self, reply: Reply) {
        if reply.tag.connection != self.tag.connection {
            return;
        }
        if matches!(&reply.data, Data::Definition(_) | Data::DefinitionError(_))
            && reply.tag.authoring != self.tag.authoring
        {
            return;
        }
        if matches!(
            &reply.data,
            Data::RecipeSaved(_) | Data::RecipeSaveProgress(_) | Data::RecipeSaveReceipts(_)
        ) && reply.tag.save != self.tag.save
        {
            return;
        }
        if matches!(reply.data, Data::Trace(..)) && reply.tag.selection != self.tag.selection {
            return;
        }
        if reply.run.is_some()
            && (reply.tag.selection != self.tag.selection || reply.run != self.selected)
        {
            return;
        }
        if matches!(
            &reply.data,
            Data::Preview(_) | Data::PreviewError(_) | Data::PreviewPage(_)
        ) && (reply.tag.selection != self.tag.selection || reply.tag.preview != self.tag.preview)
        {
            return;
        }
        // A preview refresh or navigation does not resolve a submitted launch.
        // Only another explicit launch/recovery operation supersedes its replies.
        if matches!(
            &reply.data,
            Data::LaunchProgress(..) | Data::LaunchRefused(..) | Data::Accepted(..)
        ) && reply.tag.launch != self.tag.launch
        {
            return;
        }
        if matches!(
            &reply.data,
            Data::Checkpoint(..)
                | Data::Content(_)
                | Data::Source(..)
                | Data::GifFrame(..)
                | Data::GifStopped(_)
        ) && reply.tag.inspection != self.tag.inspection
        {
            return;
        }
        match reply.data {
            Data::Connected(caps, endpoint) => {
                if self
                    .service
                    .as_ref()
                    .is_some_and(|old| *old != caps.service_id)
                {
                    self.connection = ConnectionState::Error(
                        "Service identity changed; restart the workbench with the intended connection descriptor".into(),
                    );
                    self.stale = true;
                    return;
                }
                self.service = Some(caps.service_id);
                self.authoring.enabled = caps.features.iter().any(|f| f == RECIPE_AUTHORING);
                if self.authoring.enabled {
                    self.command(Action::RecipeSaveReceipts);
                }
                self.modes = caps.modes;
                self.endpoint = endpoint;
                self.backend = format!(
                    "{} · {}",
                    if caps.backend.kind == BackendKind::Fixture {
                        "Fixture replay"
                    } else {
                        "Controller"
                    },
                    caps.backend.description
                );
                self.connection = ConnectionState::Connected;
                self.stale = false;
                self.last_update = self.clock;
                self.command(Action::Recipes(None));
                self.command(Action::Runs(None));
                self.command(Action::Receipts(None));
                // Restore the saved run, or resume watching the open one. The
                // launcher stopped watching the run it left, so a reconnect
                // there doesn't resume it.
                if self.selected.is_none()
                    && self.preferences.service_id == self.service
                    && let Some(run) = self.preferences.run_id.clone()
                {
                    self.select_run(run);
                } else if !self.launcher
                    && let Some(run) = self.selected.clone()
                {
                    self.command(Action::Observe(run));
                }
            }
            Data::Preferences(p) => {
                self.preferences = p;
            }
            Data::Recipes(p) => {
                self.recipes = p.items;
                self.recipe_next = p.next_cursor;
            }
            Data::Definition(definition) => {
                self.authoring.definition = Some(*definition);
            }
            Data::DefinitionError(error) => {
                self.authoring.error = error;
            }
            Data::RecipeSaveProgress(state) => {
                self.authoring.save = state;
                if !matches!(
                    self.authoring.save,
                    Mutation::Recording | Mutation::Submitting
                ) {
                    self.command(Action::RecipeSaveReceipts);
                }
            }
            Data::RecipeSaveReceipts(receipts) => {
                self.authoring.receipts = receipts;
            }
            Data::RecipeSaved(response) => {
                self.authoring.save = Mutation::Accepted;
                self.authoring
                    .receipts
                    .retain(|r| r.pending.key != response.key);
                let select = self.authoring.draft.as_ref().is_none_or(|draft| {
                    draft
                        .request()
                        .is_ok_and(|request| request == response.request)
                });
                self.authoring.last_saved = Some(response.definition.execution_recipe_id.clone());
                if select {
                    self.authoring.draft = None;
                    self.choose_recipe(
                        response.definition.execution_recipe_id.clone(),
                        RunMode::Experiment,
                    );
                }
                self.notify(
                    "Recipe revision saved. Launching requires a fresh preview.",
                    None,
                );
                self.command(Action::Recipes(None));
                self.command(Action::RecipeSaveReceipts);
            }
            Data::Runs(p) => {
                self.runs = p.items;
                self.runs_next = p.next_cursor;
            }
            Data::Receipts(items, next) => {
                self.receipts = items;
                self.receipts_next = next;
            }
            Data::Preview(p) if reply.tag.preview == self.tag.preview => {
                self.previewing = false;
                self.preview = Some(*p);
                self.authoring.error.clear();
            }
            Data::Preview(_) => (),
            Data::PreviewPage(source) => {
                if let Some(preview) = &mut self.preview {
                    preview.source = source;
                }
            }
            Data::PreviewError(error) => {
                if self.authoring.enabled && self.mode == RunMode::Experiment {
                    self.authoring.error = error.clone();
                }
                self.previewing = false;
                self.preview = None;
                self.notify(error, None);
            }
            Data::LaunchProgress(state, saved) => {
                self.launch = state;
                if saved.is_some() {
                    self.saved = saved;
                }
            }
            Data::LaunchRefused(error, busy) => {
                // Nothing was admitted, so a fresh launch stays available and
                // there is no saved request left to look up or retry.
                self.launch = Mutation::Rejected(self.refusal(&error, busy));
                self.saved = None;
                self.command(Action::Receipts(None));
                self.command(Action::Runs(None));
            }
            Data::Accepted(accepted, saved) => {
                self.launch = Mutation::Accepted;
                self.saved = Some(saved);
                if reply.tag.selection == self.tag.selection {
                    self.select_run(accepted.run_id);
                }
                self.command(Action::Receipts(None));
                // Otherwise the run list shows the new run only at its next poll.
                self.command(Action::Runs(None));
            }
            Data::Run(run) => {
                self.apply_run(*run);
            }
            Data::Cancellation(cancel) => {
                self.cancellation = Mutation::Accepted;
                self.notify(
                    "Cancellation intent acknowledged; inspect enforcement and cleanup below",
                    reply.run,
                );
                self.apply_run(*cancel);
            }
            Data::CancellationUnresolved(error) => {
                self.cancellation = Mutation::Unresolved(error.clone());
                self.notify(error, reply.run);
            }
            Data::Trace(sequence, view) => {
                self.trace_pending.remove(&sequence);
                // A late result for an offscreen boundary never grows the cache.
                if self
                    .window
                    .rows
                    .iter()
                    .any(|r| r.first_sequence == sequence)
                {
                    self.trace.insert(sequence, view);
                }
                self.prepare_trace();
            }
            Data::Window(window, history, live) => {
                if window.location.is_some() {
                    self.location = window.location;
                }
                self.window = window;
                self.prepare_trace();
                self.history = history;
                self.live_count = live;
                if self.following {
                    self.seen_live = live;
                }
            }
            Data::Checkpoints(page, files) => {
                if self.checkpoint.is_none()
                    && self.checkpoint_target.is_none()
                    && self.selected_row.is_none()
                    && let Some(checkpoint) = page.items.first()
                    && let Some(run) = self.selected.clone()
                {
                    self.command(Action::Checkpoint(
                        run,
                        checkpoint.checkpoint.session_id.clone(),
                    ));
                }
                self.checkpoints = page.items;
                self.checkpoints_next = page.next_cursor;
                self.checkpoint_files.extend(files);
            }
            Data::Checkpoint(c, memory) => {
                if self
                    .checkpoint_target
                    .as_ref()
                    .is_some_and(|target| *target != c.checkpoint.session_id)
                {
                    return;
                }
                if self.content.is_none()
                    && let Some(run) = self.selected.clone()
                    && let Some(id) = c.files.iter().find_map(|file| {
                        if crate::media::image_name(&file.name)
                            && let ContentAvailability::Available { artifact_id, .. } =
                                &file.content
                        {
                            Some(artifact_id.clone())
                        } else {
                            None
                        }
                    })
                {
                    self.command(Action::Content(run, id, 0));
                }
                self.file_page = 0;
                self.checkpoint = Some(*c);
                self.memory = Some(memory);
            }
            Data::Content(content) => {
                if let Some(checkpoint) = &self.checkpoint {
                    let references = [
                        &checkpoint.incoming_memory,
                        &checkpoint.outgoing_memory,
                        &checkpoint.evidence,
                        &checkpoint.workspace,
                    ];
                    let expected = references
                        .into_iter()
                        .chain(checkpoint.files.iter().map(|f| &f.content))
                        .find_map(|reference| match reference {
                            ContentAvailability::Available {
                                artifact_id,
                                content: expected,
                            } if artifact_id == &content.artifact.artifact_id => Some(expected),
                            _ => None,
                        });
                    if expected.is_some_and(|expected| *expected != content.artifact.content) {
                        self.notify(
                            "Published content identity mismatch; preview refused",
                            reply.run,
                        );
                        return;
                    }
                }
                self.content = Some(content);
            }
            Data::GifFrame(pixels, applied) => {
                if self.playing
                    && let Some(content) = &mut self.content
                {
                    content.image = Some(pixels);
                }
                let _ = applied.send(());
            }
            Data::GifStopped(notice) => {
                self.playing = false;
                if let Some(content) = &mut self.content {
                    content.notice = notice;
                }
            }
            Data::Source(sequence, source) => {
                self.source_sequence = sequence;
                self.source = Some(source);
            }
            Data::ArchiveWaiting(waiting) => self.archive_waiting = waiting,
            Data::Notice(message) => {
                self.notify(message, reply.run);
            }
            Data::Connection(state) => {
                if matches!(
                    state,
                    ConnectionState::Error(_) | ConnectionState::Disconnected
                ) {
                    self.archive_waiting = false;
                }
                self.stale = state != ConnectionState::Connected;
                self.connection = state;
            }
            Data::Error(error) => {
                self.notify(error, reply.run);
            }
        }
    }
    fn refusal(&self, error: &str, busy: bool) -> String {
        let mut text = format!(
            "Launch refused ({error}). The service admitted nothing, so the saved request was discarded."
        );
        if busy {
            match self
                .runs
                .iter()
                .find(|run| run.orchestration.state != OrchestrationState::Terminal)
            {
                Some(run) if awaiting_release(run) => text.push_str(&format!(
                    " Run {0} holds the slot until `playtime runs release {0} --reason TEXT`.",
                    run.run_id
                )),
                Some(run) => text.push_str(&format!(
                    " Run {} holds the slot while it is {}.",
                    run.run_id,
                    crate::progress::state_words(run.orchestration.state).to_lowercase()
                )),
                None => {
                    text.push_str(" Another run holds the slot, possibly from another checkout.")
                }
            }
        }
        text
    }
    pub fn stop_animation(&mut self) {
        if self.playing {
            self.playing = false;
            self.command(Action::StopGif);
        }
    }
    fn apply_run(&mut self, run: RunViewData) {
        if let Some(current) = &self.run {
            let old = &current.value;
            let next = &run.value;
            // Status GET and Cancel run concurrently. updated_at is not a unique
            // revision: ties (and clock changes) must not erase durable facts.
            if snapshot_time(&next.updated_at) < snapshot_time(&old.updated_at)
                || (old.orchestration.state == OrchestrationState::Terminal
                    && next.orchestration.state != OrchestrationState::Terminal)
                || (old.cancellation.is_some() && next.cancellation.is_none())
                || old.cancellation.as_ref().is_some_and(|ack| {
                    next.cancellation.as_ref().is_some_and(|next| {
                        (ack.stop_accepted_at.is_some() && next.stop_accepted_at.is_none())
                            || (ack.enforcement == Enforcement::Stopped
                                && next.enforcement != Enforcement::Stopped)
                    })
                })
            {
                return;
            }
        }
        if run.value.checkpoint.as_ref()
            != self.run.as_ref().and_then(|r| r.value.checkpoint.as_ref())
        {
            self.command(Action::Checkpoints(run.value.run_id.clone(), None));
        }
        self.run = Some(run);
        self.stale = self.connection != ConnectionState::Connected;
        self.last_update = self.clock;
    }
    /// At most two background content reads, and only the current bounded
    /// window retained. Completion never changes the inspector selection.
    fn prepare_trace(&mut self) {
        self.trace
            .retain(|seq, _| self.window.rows.iter().any(|r| r.first_sequence == *seq));
        let Some(run) = self.selected.clone() else {
            return;
        };
        let requests: Vec<_> = self
            .window
            .rows
            .iter()
            .filter(|r| {
                !self.trace.contains_key(&r.first_sequence)
                    && !self.trace_pending.contains(&r.first_sequence)
            })
            .filter_map(|r| r.trace.clone().map(|kind| (r.first_sequence, kind)))
            .take(2usize.saturating_sub(self.trace_pending.len()))
            .collect();
        for (sequence, kind) in requests {
            if self.commands.len() >= 64 {
                break;
            }
            self.trace_pending.insert(sequence);
            self.command(Action::Trace(run.clone(), sequence, kind));
        }
    }
    pub fn inspect_checkpoint(&mut self, session: playtime_core::SessionId, inspector: Inspector) {
        if let Some(run) = self.selected.clone() {
            self.inspector = inspector;
            self.show_inspector = true;
            self.checkpoint = None;
            self.memory = None;
            self.inspect_action(Action::Checkpoint(run, session));
        }
    }
    pub fn inspect_action(&mut self, action: Action) {
        self.stop_animation();
        self.tag.inspection += 1;
        self.following = false;
        if matches!(action, Action::Checkpoint(..)) {
            self.checkpoint = None;
            self.memory = None;
        }
        if matches!(
            action,
            Action::Checkpoint(..) | Action::Content(..) | Action::Retained(..)
        ) {
            self.content = None;
        }
        self.command(action);
    }
}

/// The daemon stopped monitoring a run that isn't Terminal, so it keeps the slot
/// until `playtime runs release`. This matches the CLI's `awaiting_release`.
pub fn awaiting_release(view: &RunView) -> bool {
    view.orchestration.state == OrchestrationState::Reconciling
        && matches!(
            view.orchestration.reason.as_deref(),
            Some("worker_unavailable" | "coordinator_restarted")
        )
}

fn snapshot_time(time: &playtime_core::UtcTime) -> (&str, [u8; 9]) {
    // UtcTime validates UTC seconds and up to nine fractional digits. Its string
    // ordering alone would put a whole second after that second's fractions.
    let text = time.as_str();
    let mut fraction = [b'0'; 9];
    if text.as_bytes()[19] == b'.' {
        let digits = &text.as_bytes()[20..text.len() - 1];
        fraction[..digits.len()].copy_from_slice(digits);
    }
    (&text[..19], fraction)
}
