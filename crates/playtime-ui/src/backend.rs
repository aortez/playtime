//! Owned background runtime. Network, private files, parsing and decoding never
//! run in egui callbacks. Observation cancellation only detaches the client.
use crate::{
    media,
    model::*,
    progress,
    transcript::{self, Transcript, View},
};
use playtime_client::{
    Client, Error, Observation, RetryPolicy,
    config::{self, Environment, Options, Resolved},
    recipes::{RecipeSaveReceipt, RecipeSaveStore},
    registrations::{ModelRegistrationReceipt, ModelRegistrationStore},
    state::{LaunchReceipt, ReceiptStore},
};
use playtime_core::RunId;
use playtime_local::{PrivateDirectory, read_private, replace_private};
use playtime_protocol::{api::*, events::EventEnvelope};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore, mpsc, oneshot},
    task::JoinSet,
    time::interval,
};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, String>;
#[derive(Clone)]
struct Out {
    tx: mpsc::Sender<Reply>,
    ctx: egui::Context,
    pixels: Arc<Semaphore>,
}
impl Out {
    async fn send(&self, tag: Tag, run: Option<RunId>, mut data: Data) {
        let image = match &mut data {
            Data::Content(content) => content.image.as_mut(),
            Data::GifFrame(pixels, _) => Some(pixels),
            _ => None,
        };
        if let Some(image) = image {
            let Ok(permit) = self
                .pixels
                .clone()
                .acquire_many_owned(image.bytes as u32)
                .await
            else {
                return;
            };
            *image = Arc::new((**image).clone().account(permit));
        }
        if self.tx.send(Reply { tag, run, data }).await.is_ok() {
            self.ctx.request_repaint();
        }
    }
}
pub struct Backend {
    pub commands: mpsc::Sender<Command>,
    pub replies: mpsc::Receiver<Reply>,
    stop: CancellationToken,
    pending: Option<Reply>,
    paint: egui::Context,
    pub prepared_this_frame: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Backend {
    pub fn start(ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let (out, replies) = mpsc::channel(64);
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let paint = ctx.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("client runtime");
            runtime.block_on(actor(
                rx,
                Out {
                    tx: out,
                    ctx,
                    pixels: Arc::new(Semaphore::new(media::PIXEL_BYTES)),
                },
                cancelled,
            ));
            runtime.shutdown_timeout(Duration::from_secs(5));
        });
        Self {
            commands: tx,
            replies,
            stop,
            pending: None,
            paint,
            prepared_this_frame: 0,
            thread: Some(thread),
        }
    }
    pub fn pump(&mut self, model: &mut Model) {
        let start = std::time::Instant::now();
        self.prepared_this_frame = 0;
        for _ in 0..100 {
            if start.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(reply) = self.pending.take().or_else(|| self.replies.try_recv().ok()) else {
                break;
            };
            let count = if let Data::Window(window, ..) = &reply.data {
                window.rows.len().max(1)
            } else {
                1
            };
            if self.prepared_this_frame + count > 100 {
                self.pending = Some(reply);
                break;
            }
            self.prepared_this_frame += count;
            model.apply(reply);
        }
        if self.pending.is_some() || !self.replies.is_empty() {
            self.paint.request_repaint();
        }
        while let Some(command) = model.commands.pop_front() {
            match self.commands.try_send(command) {
                Ok(()) => (),
                Err(mpsc::error::TrySendError::Full(command)) => {
                    model.commands.push_front(command);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    model.notify("Client background work stopped", None);
                    break;
                }
            }
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.replies.close();
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
#[derive(Clone)]
struct Session {
    client: Client,
    resolved: Resolved,
}
async fn local<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| "Local client task failed".to_string())?
}
fn safe(error: Error) -> String {
    if let Error::Api { response, .. } = &error
        && response.code == "execution_unavailable"
    {
        let explanation = response.details.get("reasons").and_then(serde_json::Value::as_array)
            .and_then(|reasons| reasons.iter().find_map(|reason| match reason.as_str()? {
                "selected_model_unsupported" => Some("This saved model identity has no supported execution profile in this build."),
                "selected_model_missing_or_changed" => Some("The selected model is missing or has changed. Restore its registered identity or choose a newly reviewed registration, then refresh the preview."),
                "target_required" => Some("Choose an inference target, then refresh the preview."),
                "target_unknown" => Some("This inference target is no longer configured. Choose a target and refresh the preview."),
                "model_unavailable" => Some("The configured model endpoint is unavailable. Restore it, then refresh the preview."),
                "workshop_unavailable" | "workshop_image_unavailable" => Some("The locked Docker workshop is unavailable. Restore it, then refresh the preview."),
                "worker_profile_unavailable" => Some("The worker bundle does not support this execution profile. Build and select a current bundle."),
                _ => None,
            })).unwrap_or("Execution is unavailable with the current service configuration.");
        return format!("{error}: {explanation} Saved definitions remain readable and editable.");
    }
    if let Error::Api { response, .. } = &error
        && response.code == "invalid_parameters"
    {
        return format!(
            "{}: {}",
            error,
            response.message.chars().take(512).collect::<String>()
        );
    }
    error.to_string()
}

fn observation_retry() -> RetryPolicy {
    RetryPolicy {
        max_elapsed: Duration::from_secs(60),
        max_reconnects: 100,
        ..Default::default()
    }
}

/// One selected observation. Each read owns its outage budget; a successful
/// status poll cannot reset a failing history page's deadline or connection state.
#[derive(Clone)]
struct Reads {
    client: Client,
    run: RunId,
    tag: Tag,
    out: Out,
    activity: Arc<Mutex<[usize; 2]>>,
}
impl Reads {
    async fn retry<T, F, R>(&self, request: F) -> Result<T>
    where
        F: FnMut() -> R,
        R: std::future::Future<Output = playtime_client::Result<T>>,
    {
        let mut recovering = None;
        let result = crate::read_retry::read(observation_retry(), request, |waiting| {
            let previous = recovering.replace(waiting);
            self.activity(previous, Some(waiting))
        })
        .await;
        if recovering.is_some() {
            self.activity(recovering, None).await;
            if let Err(error) = &result {
                self.out
                    .send(
                        self.tag,
                        Some(self.run.clone()),
                        Data::Connection(ConnectionState::Error(error.to_string())),
                    )
                    .await;
            }
        }
        result.map_err(safe)
    }
    // Shared counts prevent a successful poll from clearing another read's
    // waiting/reconnecting state. Category changes retain the original deadline.
    async fn activity(&self, previous: Option<bool>, next: Option<bool>) {
        let mut activity = self.activity.lock().await;
        let before = *activity;
        if let Some(waiting) = previous {
            activity[usize::from(waiting)] -= 1;
        }
        if let Some(waiting) = next {
            activity[usize::from(waiting)] += 1;
        }
        if (before[1] == 0) != (activity[1] == 0) {
            self.out
                .send(
                    self.tag,
                    Some(self.run.clone()),
                    Data::ArchiveWaiting(activity[1] > 0),
                )
                .await;
        }
        if (before[0] == 0) != (activity[0] == 0) {
            let state = if activity[0] > 0 {
                ConnectionState::Reconnecting
            } else {
                ConnectionState::Connected
            };
            self.out
                .send(self.tag, Some(self.run.clone()), Data::Connection(state))
                .await;
        }
    }
    async fn run(&self) -> Result<RunView> {
        self.retry(|| self.client.run(&self.run)).await
    }
    async fn history(&self, cursor: &str) -> Result<EventPage> {
        let query = PageQuery {
            cursor: Some(cursor.to_owned()),
            limit: Some(64),
        };
        self.retry(|| self.client.history(&self.run, &query)).await
    }
}
async fn connect(
    options: Options,
    expected: Option<ServiceId>,
    out: &Out,
    tag: Tag,
) -> Result<Session> {
    let (resolved, mut loaded, preferences, receipts) = local(move || {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let resolved =
            config::resolve(&options, &Environment::capture(), &cwd).map_err(|e| e.to_string())?;
        let preferences = read_private(&resolved.state_dir.join("workbench.json"), 64 << 10)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Preferences>(&bytes).ok())
            .filter(|p| p.schema_version == 1);
        let receipts = ReceiptStore::new(&resolved.state_dir)
            .list(None, 200)
            .map_err(|e| e.to_string())?;
        let loaded = resolved.load().map_err(|e| e.to_string())?;
        Ok((resolved, loaded, preferences, receipts))
    })
    .await?;
    if let Some(preferences) = preferences {
        out.send(tag, None, Data::Preferences(preferences)).await;
    }
    out.send(
        tag,
        None,
        Data::Receipts(receipts.items, receipts.next_cursor),
    )
    .await;
    if expected.is_some() {
        loaded.expected_service = expected;
    }
    let client = loaded.connect().await.map_err(safe)?;
    Ok(Session { client, resolved })
}
async fn actor(mut rx: mpsc::Receiver<Command>, out: Out, stop: CancellationToken) {
    let mut session: Option<Session> = None;
    let mut connection_tag = 0;
    let mut pinned_service = None;
    let mut jobs = JoinSet::new();
    let registration_gate = Arc::new(Semaphore::new(1));
    let previews = Arc::new(Semaphore::new(2));
    let mut animation: Option<CancellationToken> = None;
    let mut model_job: Option<tokio::task::JoinHandle<()>> = None;
    // A newer archived-model read supersedes the last one, whose reply the view
    // would discard, so the old read stops holding a preview permit.
    let mut run_model_job: Option<tokio::task::JoinHandle<()>> = None;
    let mut observer: Option<(
        CancellationToken,
        mpsc::Sender<Command>,
        tokio::task::JoinHandle<()>,
    )> = None;
    let (connected_tx, mut connected_rx) = mpsc::channel::<(Tag, Result<Session>)>(2);
    let mut poll = interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            Some((tag, result)) = connected_rx.recv() => {
                if tag.connection != connection_tag { continue; }
                match result {
                    Ok(s) => { pinned_service = Some(s.client.service_id().clone()); out.send(tag, None, Data::Connected(s.client.capabilities().clone(), s.client.endpoint().into())).await; session = Some(s); }
                    Err(error) => out.send(tag, None, Data::Connection(ConnectionState::Error(error))).await,
                }
            }
            _ = jobs.join_next(), if !jobs.is_empty() => (),
            _ = poll.tick(), if session.is_some() && jobs.len() < 16 => {
                let client = session.as_ref().unwrap().client.clone(); let out = out.clone();
                let tag = Tag { connection: connection_tag, ..Default::default() };
                jobs.spawn(async move { if let Ok(page) = client.runs(&PageQuery { limit: Some(200), cursor: None }).await { out.send(tag, None, Data::Runs(page)).await; } });
            }
            Some(command) = rx.recv() => {
                let tag = command.tag;
                if matches!(command.action, Action::Connect(_) | Action::StopObservation | Action::Observe(_) | Action::StopGif | Action::PlayGif(..) | Action::Checkpoint(..) | Action::Content(..) | Action::Retained(..))
                    && let Some(token) = animation.take() { token.cancel(); }
                if matches!(command.action,Action::Connect(_)|Action::StopModelDiscovery|Action::ModelInventory|Action::InspectModel(_)) && let Some(job)=model_job.take() {job.abort();let _=job.await;}
                if matches!(command.action,Action::Connect(_)|Action::RunModel(..)) && let Some(job)=run_model_job.take() {job.abort();let _=job.await;}
                if matches!(command.action,Action::StopModelDiscovery) {continue;}
                if let Action::Connect(options) = command.action {
                    if jobs.len() >= 16 { out.send(tag, None, Data::Connection(ConnectionState::Error("Client work limit reached; retry Reconnect after current work finishes".into()))).await; continue; }
                    connection_tag = tag.connection;
                    if let Some((cancel, _, handle)) = observer.take() { cancel.cancel(); let _ = handle.await; }
                    let expected = pinned_service.clone();
                    session = None;
                    let tx = connected_tx.clone(); let out = out.clone();
                    jobs.spawn(async move { let result = connect(options, expected, &out, tag).await; let _ = tx.send((tag, result)).await; });
                    continue;
                }
                if tag.connection != connection_tag { continue; }
                if matches!(command.action, Action::StopGif) { continue; }
                if let Action::PlayGif(run, id) = command.action {
                    if let Some(s) = &session {
                        let cancel = stop.child_token(); animation = Some(cancel.clone());
                        let client = s.client.clone(); let out = out.clone(); let previews = previews.clone();
                        jobs.spawn(async move { play_gif(client, run, id, tag, out, previews, cancel).await; });
                    }
                    continue;
                }
                if matches!(command.action, Action::StopObservation | Action::Observe(_)) {
                    if let Some((cancel, _, handle)) = observer.take() { cancel.cancel(); let _ = handle.await; }
                    if let Action::Observe(run) = command.action && let Some(s) = &session {
                        let (tx, requests) = mpsc::channel(8); let cancel = stop.child_token();
                        let client = s.client.clone(); let out = out.clone(); let child_stop = cancel.clone();
                        let handle = tokio::spawn(async move { observe(client, run, tag, requests, out, child_stop).await; });
                        observer = Some((cancel, tx, handle));
                    }
                    continue;
                }
                if matches!(command.action, Action::Window(..) | Action::Source(..) | Action::Sessions(..) | Action::Locate(..) | Action::CancelNavigation) {
                    if let Some((_, tx, _)) = &observer { tokio::select! { _ = stop.cancelled() => break, _ = tx.send(command) => () } }
                    continue;
                }
                let Some(s) = &session else { out.send(tag, None, action_error(&command.action, "Connect to a service first".into())).await; continue; };
                if matches!(command.action,Action::ModelInventory|Action::InspectModel(_)) {
                    let session=s.clone();let out=out.clone();let previews=previews.clone();
                    model_job=Some(tokio::spawn(async move {let action=command.action.clone();if let Err(error)=perform(&session,&out,command,previews).await {out.send(tag,None,action_error(&action,error)).await;}}));
                    continue;
                }
                if let Action::RunModel(run, _) = &command.action {
                    let run=Some(run.clone());let session=s.clone();let out=out.clone();let previews=previews.clone();
                    run_model_job=Some(tokio::spawn(async move {let action=command.action.clone();if let Err(error)=perform(&session,&out,command,previews).await {out.send(tag,run,action_error(&action,error)).await;}}));
                    continue;
                }
                if jobs.len() >= 16 { out.send(tag, None, action_error(&command.action, "Client work limit reached; retry the explicit action".into())).await; continue; }
                // Reserve before spawning: reconnect receipt scans must not race
                // a mutation that has not yet written its durable local receipt.
                let registration_permit = if matches!(command.action, Action::RegisterModel(_) | Action::RecoverModelRegistration(..) | Action::ModelRegistrationReceipts) {
                    match registration_gate.clone().try_acquire_owned() {
                        Ok(permit) => Some(permit),
                        Err(_) => {
                            out.send(tag,None,action_error(&command.action,"Registration work is still in progress; refresh receipts after it finishes".into())).await;
                            continue;
                        }
                    }
                } else { None };
                let session = s.clone(); let out = out.clone(); let previews = previews.clone();
                jobs.spawn(async move {
                    let _registration_permit = registration_permit;
                    // A download reports on the saved file, not the open run, so
                    // its result still arrives and stays after another run opens.
                    let run = match &command.action {
                        Action::FinalFiles(run) | Action::Trace(run, ..) | Action::Cancel(run) | Action::Checkpoints(run, _) | Action::Checkpoint(run, _) | Action::Content(run, _, _) | Action::Retained(run, _) => Some(run.clone()), _ => None,
                    };
                    let action = command.action.clone();
                    let result = perform(&session, &out, command, previews).await;
                    if let Err(error) = result { out.send(tag, run, action_error(&action, error)).await; }
                });
            }
            else => break,
        }
    }
    if let Some((cancel, _, handle)) = observer {
        cancel.cancel();
        let _ = handle.await;
    }
    for job in [model_job, run_model_job].into_iter().flatten() {
        job.abort();
        let _ = job.await;
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}
fn action_error(action: &Action, error: String) -> Data {
    match action {
        Action::RunModel(..) => Data::RunModel(Err(error)),
        Action::RegisterModel(_) => Data::ModelRegistrationProgress(Mutation::Rejected(error)),
        Action::RecoverModelRegistration(..) => {
            Data::ModelRegistrationProgress(Mutation::Unresolved(error))
        }
        Action::ModelRegistrations => Data::ModelRegistryError(error),
        Action::ModelRegistration(_) | Action::ModelRegistrationPage(..) => {
            Data::ModelRegistrationReadError(error)
        }
        Action::ModelRegistrationReceipts => Data::ModelRegistrationReceiptsError(error),
        Action::ModelInventory | Action::InspectModel(_) => Data::ModelError(error),
        Action::Trace(_, sequence, _) => Data::Trace(
            *sequence,
            TraceView {
                notice: format!("Unavailable: {}", transcript::excerpt(&error, 512)),
                ..Default::default()
            },
        ),
        Action::Definition(_) => Data::DefinitionError(error),
        Action::SaveRecipe(_) => Data::RecipeSaveProgress(Mutation::Rejected(error)),
        Action::RecoverRecipe(..) => Data::RecipeSaveProgress(Mutation::Unresolved(error)),
        Action::Preview(..) => Data::PreviewError(error),
        Action::Launch(..) => Data::LaunchProgress(Mutation::Rejected(error), None),
        Action::Recover(..) => Data::LaunchProgress(recovery_unresolved(error), None),
        Action::Cancel(..) => Data::CancellationUnresolved(error),
        _ => Data::Error(error),
    }
}
pub use crate::preview::prepare as prepare_preview;
pub fn prepare_run(value: RunView) -> RunViewData {
    let harness = value
        .harness
        .as_ref()
        .map(|h| {
            let mut lines = crate::harness::lines(&h.harness);
            lines.push(format!("Frozen configuration: {}", h.configuration.sha256));
            lines
        })
        .unwrap_or_default();
    let mut summary = progress::details(&value);
    if let Some(target) = &value.target {
        summary.insert(
            0,
            format!(
                "Target: {} ({}) · {}",
                target.name, target.id, target.endpoint
            ),
        );
    }
    let source = transcript::source_page(&serde_json::to_string_pretty(&value).unwrap(), 0);
    RunViewData {
        harness,
        value,
        summary,
        source,
    }
}
/// What each checkpoint saved, by session: its files and images, and which
/// files are new or changed since the checkpoint before it. Only a complete
/// list shows what the earliest checkpoint added.
pub fn checkpoint_files(checkpoints: &[Checkpoint], complete: bool) -> BTreeMap<String, String> {
    let mut ordered = checkpoints.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|c| c.checkpoint.published_sequence);
    let mut previous: Option<BTreeMap<&str, Option<&str>>> = complete.then(BTreeMap::new);
    let mut summaries = BTreeMap::new();
    for checkpoint in ordered {
        let files = checkpoint
            .files
            .iter()
            .filter(|file| !matches!(file.kind.as_str(), "5" | "directory"))
            .map(|file| {
                let digest = match &file.content {
                    ContentAvailability::Available { content, .. } => Some(content.sha256.as_str()),
                    ContentAvailability::Unavailable { .. } => None,
                };
                (file.name.as_str(), digest)
            })
            .collect::<BTreeMap<_, _>>();
        let images = files.keys().filter(|name| media::image_name(name)).count();
        let mut text = format!(
            "{} {}",
            files.len(),
            if files.len() == 1 { "file" } else { "files" }
        );
        if images > 0 {
            text.push_str(&format!(
                ", {images} {}",
                if images == 1 { "image" } else { "images" }
            ));
        }
        if let Some(previous) = &previous
            && files.values().all(Option::is_some)
            && previous.values().all(Option::is_some)
        {
            let changed = files
                .iter()
                .filter(|(name, digest)| previous.get(**name) != Some(*digest))
                .map(|(name, _)| transcript::excerpt(name, 60))
                .collect::<Vec<_>>();
            if changed.is_empty() {
                text.push_str(" · nothing new or changed");
            } else {
                text.push_str(&format!(
                    " · new or changed: {}",
                    changed[..changed.len().min(4)].join(", ")
                ));
                if changed.len() > 4 {
                    text.push_str(&format!(" and {} more", changed.len() - 4));
                }
            }
        } else {
            text.push_str(" · change classification unavailable");
        }
        summaries.insert(checkpoint.checkpoint.session_id.as_str().to_owned(), text);
        previous = complete.then_some(files);
    }
    summaries
}
/// A bounded file list from one published checkpoint; no adjacent-version claims.
pub fn checkpoint_file_list(checkpoint: &Checkpoint) -> String {
    let files: Vec<_> = checkpoint
        .files
        .iter()
        .filter(|f| !matches!(f.kind.as_str(), "5" | "directory"))
        .collect();
    let images = files.iter().filter(|f| media::image_name(&f.name)).count();
    let names = files
        .iter()
        .take(4)
        .map(|f| transcript::excerpt(&f.name, 80))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} saved {} · {}{}{}",
        files.len(),
        if files.len() == 1 { "file" } else { "files" },
        progress::count(images as u64, "image", "images"),
        if names.is_empty() {
            String::new()
        } else {
            format!(": {names}")
        },
        if files.len() > 4 {
            format!(" and {} more", files.len() - 4)
        } else {
            String::new()
        }
    )
}

async fn perform(s: &Session, out: &Out, command: Command, previews: Arc<Semaphore>) -> Result<()> {
    let tag = command.tag;
    let scoped = s.client.with_target(command.target.clone()).map_err(safe)?;
    let client = &scoped;
    match command.action {
        Action::Targets => {
            out.send(
                tag,
                None,
                Data::Targets(client.inference_targets().await.map_err(safe)?),
            )
            .await;
        }
        Action::ModelInventory => {
            let value = client.model_inventory().await.map_err(safe)?;
            out.send(
                tag,
                None,
                Data::ModelInventory(crate::models::inventory(value)),
            )
            .await;
        }
        Action::InspectModel(request) => {
            let view = if client
                .capabilities()
                .features
                .iter()
                .any(|f| f == MODEL_REGISTRATION)
            {
                let review = client.review_model(&request).await.map_err(safe)?;
                crate::models::review(review)
            } else {
                crate::models::inspection(client.inspect_model(&request).await.map_err(safe)?)
            };
            out.send(tag, None, Data::ModelInspection(view)).await;
        }
        Action::ModelRegistrations => {
            let value = client.registered_models().await.map_err(safe)?;
            out.send(tag, None, Data::ModelRegistrations(value)).await;
        }
        Action::ModelRegistration(id) => {
            let value = client.registered_model(&id).await.map_err(safe)?;
            out.send(
                tag,
                None,
                Data::ModelRegistration(crate::registrations::prepare(Arc::new(value), 0)),
            )
            .await;
        }
        Action::ModelRegistrationPage(value, offset) => {
            out.send(
                tag,
                None,
                Data::ModelRegistration(crate::registrations::prepare(value, offset)),
            )
            .await;
        }
        Action::ModelRegistrationReceipts => {
            let store = ModelRegistrationStore::new(&s.resolved.state_dir);
            let service = client.service_id().clone();
            let receipts = local(move || {
                store
                    .list_service(Some(&service))
                    .map_err(|e| e.to_string())
            })
            .await?;
            out.send(tag, None, Data::ModelRegistrationReceipts(receipts))
                .await;
        }
        Action::RegisterModel(request) => {
            let pending = client.prepare_model_registration(request).map_err(safe)?;
            let store = ModelRegistrationStore::new(&s.resolved.state_dir);
            let endpoint = client.endpoint().to_owned();
            let receipt =
                local(move || store.save(&pending, &endpoint).map_err(|e| e.to_string())).await?;
            model_registration(s, out, tag, receipt, LaunchAttempt::New).await;
        }
        Action::RecoverModelRegistration(key, retry) => {
            let store = ModelRegistrationStore::new(&s.resolved.state_dir);
            let receipt = local(move || {
                store
                    .read(&key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Saved registration request not found".into())
            })
            .await?;
            receipt
                .check_target(client.service_id(), client.endpoint())
                .map_err(|e| e.to_string())?;
            model_registration(
                s,
                out,
                tag,
                receipt,
                if retry {
                    LaunchAttempt::Retry
                } else {
                    LaunchAttempt::Lookup
                },
            )
            .await;
        }

        Action::Definition(id) => {
            let value = client.recipe_definition(&id).await.map_err(safe)?;
            let harness = if client
                .capabilities()
                .features
                .iter()
                .any(|f| f == RECIPE_HARNESS_SELECTION)
            {
                Some(crate::harness::RecipeHarness::new(
                    client.recipe_harness(&value).await.map_err(safe)?,
                    client.harnesses().await.map_err(safe)?,
                ))
            } else {
                None
            };
            let settings = if client
                .capabilities()
                .features
                .iter()
                .any(|f| f == MODEL_SETTINGS)
            {
                match client.recipe_model_settings(&value).await {
                    Ok(settings) => Some(settings),
                    Err(error) if error.api_code() == Some("unsupported_capability") => None,
                    Err(error) => return Err(safe(error)),
                }
            } else {
                None
            };
            let mut inherited_value = value.inherited.clone();
            if let Some(registration) = inherited_value
                .get_mut("model_registration")
                .and_then(serde_json::Value::as_object_mut)
            {
                registration.remove("definition");
            }
            if settings.is_some() {
                if let Some(recipe) = inherited_value["recipe"].as_object_mut() {
                    recipe.remove("model");
                }
                if let Some(parameters) = inherited_value["recipe"]["parameters"].as_object_mut() {
                    parameters.remove("sampling_seed");
                }
                if let Some(limits) =
                    inherited_value["recipe"]["parameters"]["limits"].as_object_mut()
                {
                    for key in ["context_tokens", "tokens_per_call", "handoff_tokens"] {
                        limits.remove(key);
                    }
                }
            }
            if inherited_value.get("compaction").is_some() {
                if let Some(recipe) = inherited_value["recipe"].as_object_mut() {
                    recipe.remove("protocol");
                    recipe.remove("inventory");
                }
                if let Some(parameters) = inherited_value["recipe"]["parameters"].as_object_mut() {
                    parameters.remove("memory_condition");
                }
                if let Some(limits) =
                    inherited_value["recipe"]["parameters"]["limits"].as_object_mut()
                {
                    for key in [
                        "sessions",
                        "work_turns_per_session",
                        "handoff_tokens",
                        "memento_bytes",
                        "tool_parse_retries",
                        "tool_parse_error_limit",
                    ] {
                        limits.remove(key);
                    }
                }
            }
            let inherited =
                serde_json::to_string_pretty(&inherited_value).map_err(|e| e.to_string())?;
            out.send(
                tag,
                None,
                Data::Definition(Box::new(crate::authoring::DefinitionView {
                    harness,
                    model_name: value
                        .model_registration
                        .as_ref()
                        .map(|m| m.identity.name.clone())
                        .or_else(|| {
                            value
                                .inherited
                                .pointer("/model_lock/name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        }),
                    value,
                    inherited,
                    settings,
                })),
            )
            .await;
        }
        Action::RecipeSaveReceipts => {
            let store = RecipeSaveStore::new(&s.resolved.state_dir);
            let service = client.service_id().clone();
            let receipts = local(move || {
                store
                    .list_service(Some(&service))
                    .map_err(|e| e.to_string())
            })
            .await?;
            out.send(tag, None, Data::RecipeSaveReceipts(receipts))
                .await;
        }
        Action::SaveRecipe(request) => {
            let pending = client.prepare_recipe_save(request).map_err(safe)?;
            let store = RecipeSaveStore::new(&s.resolved.state_dir);
            let endpoint = client.endpoint().to_owned();
            let receipt =
                local(move || store.save(&pending, &endpoint).map_err(|e| e.to_string())).await?;
            recipe_save(s, out, tag, receipt, LaunchAttempt::New).await;
        }
        Action::RecoverRecipe(key, retry) => {
            let store = RecipeSaveStore::new(&s.resolved.state_dir);
            let receipt = local(move || {
                store
                    .read(&key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Saved recipe request not found".into())
            })
            .await?;
            receipt
                .check_target(client.service_id(), client.endpoint())
                .map_err(|e| e.to_string())?;
            recipe_save(
                s,
                out,
                tag,
                receipt,
                if retry {
                    LaunchAttempt::Retry
                } else {
                    LaunchAttempt::Lookup
                },
            )
            .await;
        }
        Action::PreviewPage(preview, offset) => {
            out.send(
                tag,
                None,
                Data::PreviewPage(crate::preview::source_page(
                    &serde_json::to_string_pretty(&preview).map_err(|e| e.to_string())?,
                    offset,
                )),
            )
            .await;
        }
        Action::Recipes(cursor) => {
            out.send(
                tag,
                None,
                Data::Recipes(
                    client
                        .recipes(&PageQuery {
                            cursor,
                            limit: Some(200),
                        })
                        .await
                        .map_err(safe)?,
                ),
            )
            .await
        }
        Action::RunModel(run, expected) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let bytes = tokio::time::timeout(
                Duration::from_secs(30),
                crate::read_retry::read(
                    observation_retry(),
                    || archived_model_bytes(client, &run, &expected),
                    |_| async {},
                ),
            )
            .await
            .map_err(|_| "Archived model read deadline")?
            .map_err(safe)?;
            out.send(
                tag,
                Some(run),
                Data::RunModel(crate::run_model::parse(&bytes)),
            )
            .await;
        }
        Action::Runs(cursor) => {
            out.send(
                tag,
                None,
                Data::Runs(
                    client
                        .runs(&PageQuery {
                            cursor,
                            limit: Some(200),
                        })
                        .await
                        .map_err(safe)?,
                ),
            )
            .await
        }
        Action::Receipts(after) => {
            let store = ReceiptStore::new(&s.resolved.state_dir);
            let page =
                local(move || store.list(after.as_ref(), 200).map_err(|e| e.to_string())).await?;
            out.send(tag, None, Data::Receipts(page.items, page.next_cursor))
                .await;
        }
        Action::Preview(id, mode) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let value = client.preview(&id, mode).await.map_err(safe)?;
            out.send(tag, None, Data::Preview(Box::new(prepare_preview(value))))
                .await;
        }
        Action::Launch(request) => {
            let pending = client.prepare_launch(request, None).map_err(safe)?;
            let store = ReceiptStore::new(&s.resolved.state_dir);
            let endpoint = client.endpoint().to_owned();
            let saved =
                local(move || store.save(&pending, &endpoint).map_err(|e| e.to_string())).await?;
            out.send(
                tag,
                None,
                Data::LaunchProgress(Mutation::Submitting, Some(saved.clone())),
            )
            .await;
            admission(s, out, tag, saved, LaunchAttempt::New).await?;
        }
        Action::Recover(key, retry) => {
            let store = ReceiptStore::new(&s.resolved.state_dir);
            let saved = local(move || {
                store
                    .read(&key)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Saved launch not found".into())
            })
            .await?;
            out.send(
                tag,
                None,
                Data::LaunchProgress(Mutation::Submitting, Some(saved.clone())),
            )
            .await;
            saved
                .check_target(
                    client.service_id(),
                    client.endpoint(),
                    s.resolved.explicit_connection,
                )
                .map_err(|e| e.to_string())?;
            admission(
                s,
                out,
                tag,
                saved,
                if retry {
                    LaunchAttempt::Retry
                } else {
                    LaunchAttempt::Lookup
                },
            )
            .await?;
        }
        Action::Cancel(run) => match client.cancel(&run).await {
            Ok(reply) => {
                out.send(
                    tag,
                    Some(run),
                    Data::Cancellation(Box::new(prepare_run(reply.run))),
                )
                .await
            }
            Err(error) => {
                out.send(
                    tag,
                    Some(run),
                    Data::CancellationUnresolved(format!(
                        "Cancellation unresolved: {error}. Inspect status before retrying."
                    )),
                )
                .await;
            }
        },
        Action::Trace(run, sequence, kind) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let mut view = TraceView::default();
            match kind {
                transcript::TraceContent::Incoming(expected) => {
                    let artifact = retained_artifact(client, &run, &expected).await?;
                    let bytes = content_bytes(client, &run, &artifact, 256 << 10).await?;
                    let text =
                        String::from_utf8(bytes).map_err(|_| "Delivered memory is not UTF-8")?;
                    view.sections.push(transcript::Section {
                        label: "Memory received".into(),
                        text: if text.is_empty() {
                            "Available empty memento".into()
                        } else {
                            transcript::excerpt(&text, 2048)
                        },
                    });
                }
                transcript::TraceContent::Checkpoint(session) => {
                    let checkpoint = client.checkpoint(&run, &session).await.map_err(safe)?;
                    if checkpoint.checkpoint.published_sequence != sequence {
                        return Err("Checkpoint publication identity mismatch".into());
                    }
                    for (label, content) in [
                        ("Memory received", &checkpoint.incoming_memory),
                        ("Published memento", &checkpoint.outgoing_memory),
                    ] {
                        let text = memory(client, &run, content)
                            .await
                            .unwrap_or_else(|error| error);
                        view.sections.push(transcript::Section {
                            label: label.into(),
                            text: if text.is_empty() {
                                "Available empty memento".into()
                            } else {
                                transcript::excerpt(&text, 2048)
                            },
                        });
                    }
                    view.files = checkpoint_file_list(&checkpoint);
                    view.notice = "Saved checkpoint versions · model claims are not verified. Compare memory or browse files for exact sources.".into();
                }
            }
            out.send(tag, Some(run), Data::Trace(sequence, view)).await;
        }
        Action::Checkpoints(run, cursor) => {
            let first = cursor.is_none();
            let page = match client
                .checkpoints(
                    &run,
                    &PageQuery {
                        cursor,
                        limit: Some(200),
                    },
                )
                .await
            {
                // A just-admitted run has no archive identity yet. Its first
                // published checkpoint changes the run view, which reloads this list.
                Err(error) if error.api_code() == Some("evidence_not_ready") => Page {
                    items: vec![],
                    next_cursor: None,
                    frontier: None,
                },
                result => result.map_err(safe)?,
            };
            let files = checkpoint_files(&page.items, first && page.next_cursor.is_none());
            out.send(tag, Some(run), Data::Checkpoints(page, files))
                .await;
        }
        Action::Checkpoint(run, session) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let checkpoint = client.checkpoint(&run, &session).await.map_err(safe)?;
            let incoming = memory(client, &run, &checkpoint.incoming_memory).await;
            let outgoing = memory(client, &run, &checkpoint.outgoing_memory).await;
            let evidence = memory(client, &run, &checkpoint.evidence).await;
            let (diff, mut notice) = match (&incoming, &outgoing) {
                (Ok(left), Ok(right)) => media::memory_diff(left, right),
                _ => (String::new(), "Memory comparison unavailable: both exact sources are required. Saved means published, not verified claims.".into()),
            };
            if checkpoint.index.is_some() {
                let detail = match client
                    .checkpoint_index(&run, &checkpoint, &CancellationToken::new())
                    .await
                {
                    Ok(playtime_client::CheckpointIndexRead::Supported { index, .. }) => format!(
                        "Index v{} verified · incoming {} · outgoing {} · {} file changes · incoming evidence {} · outgoing evidence {}",
                        index.schema_version,
                        index.incoming_memory.status,
                        index.outgoing_memory.status,
                        index.changes.len(),
                        index.incoming_evidence.is_some(),
                        index.outgoing_evidence.is_some()
                    ),
                    Ok(playtime_client::CheckpointIndexRead::UnsupportedSchema {
                        received,
                        ..
                    }) => format!(
                        "Unsupported index schema {received}; exact bytes available in artifacts"
                    ),
                    Ok(playtime_client::CheckpointIndexRead::Unavailable { reason }) => {
                        format!("Index unavailable: {reason}")
                    }
                    Err(error) => format!("Index unavailable: {}", safe(error)),
                };
                notice.push_str(" · ");
                notice.push_str(&detail);
            }
            let incoming = transcript::excerpt(&incoming.unwrap_or_else(|e| e), 8192);
            let outgoing = transcript::excerpt(&outgoing.unwrap_or_else(|e| e), 8192);
            let evidence = transcript::excerpt(&evidence.unwrap_or_else(|e| e), 8192);
            out.send(
                tag,
                Some(run),
                Data::Checkpoint(
                    Box::new(checkpoint),
                    MemoryView {
                        incoming,
                        outgoing,
                        evidence,
                        diff,
                        notice,
                    },
                ),
            )
            .await;
        }
        Action::FinalFiles(run) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let mut cursor = None;
            let mut files = Vec::new();
            let mut complete = false;
            let mut bytes = 0usize;
            'pages: for _ in 0..64 {
                let page = client
                    .artifacts(
                        &run,
                        &PageQuery {
                            limit: Some(200),
                            cursor,
                        },
                    )
                    .await
                    .map_err(safe)?;
                for artifact in page.items.into_iter().filter(|a| {
                    a.session_id.is_none()
                        && matches!(
                            a.kind.as_str(),
                            "file_version" | "final_workspace" | "final_capture_index"
                        )
                }) {
                    bytes += artifact.name.len()
                        + artifact.kind.len()
                        + artifact.content.path.as_str().len()
                        + 512;
                    if bytes > 4 << 20 {
                        break 'pages;
                    }
                    files.push(artifact);
                }
                if page.next_cursor.is_none() {
                    complete = true;
                    break;
                }
                cursor = page.next_cursor;
            }
            files.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
            out.send(tag, Some(run), Data::FinalFiles(files, complete))
                .await;
        }
        Action::Content(run, id, offset) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let artifact = client.artifact(&run, &id).await.map_err(safe)?;
            let bytes = content_bytes(client, &run, &artifact, 64 << 20).await;
            let mut view = ContentView {
                artifact,
                source: None,
                image: None,
                notice: String::new(),
                animated: false,
            };
            match bytes {
                Ok(bytes) => {
                    if image::guess_format(&bytes).is_ok() {
                        view.animated =
                            matches!(image::guess_format(&bytes), Ok(image::ImageFormat::Gif));
                        match local(move || media::decode(&bytes)).await {
                            Ok(p) => {
                                view.notice = p.notice.clone();
                                view.image = Some(Arc::new(p));
                            }
                            Err(e) => view.notice = e,
                        }
                    } else if let Ok(text) = String::from_utf8(bytes) {
                        view.source = Some(transcript::source_page(&text, offset));
                    } else {
                        view.notice =
                            "Binary or unsupported content; verified download is available".into();
                    }
                }
                Err(e) => view.notice = e,
            }
            out.send(tag, Some(run), Data::Content(view)).await;
        }
        Action::Retained(run, expected) => {
            let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
            let artifact = retained_artifact(client, &run, &expected).await?;
            let bytes = content_bytes(client, &run, &artifact, 64 << 20).await?;
            let text = String::from_utf8(bytes).map_err(|_| "Retained source is not UTF-8 text")?;
            out.send(
                tag,
                Some(run),
                Data::Content(ContentView {
                    artifact,
                    source: Some(transcript::source_page(&text, 0)),
                    image: None,
                    notice: "Verified retained source".into(),
                    animated: false,
                }),
            )
            .await;
        }
        Action::Download(run, id, path) => {
            // The client's errors don't say what failed, and this notice can
            // arrive after another run opens.
            let failed =
                |error: Error| format!("Download not saved to {}: {error}", path.display());
            let artifact = client.artifact(&run, &id).await.map_err(failed)?;
            client
                .download_to_path(&run, &artifact, &path, &CancellationToken::new())
                .await
                .map_err(failed)?;
            out.send(
                tag,
                None,
                Data::Notice(format!("Verified download saved to {}", path.display())),
            )
            .await;
        }
        Action::Preferences(preferences) => {
            let root = s.resolved.state_dir.clone();
            local(move || {
                PrivateDirectory::open(&root, true).map_err(|e| e.to_string())?;
                replace_private(
                    &root.join("workbench.json"),
                    &serde_json::to_vec(&preferences).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())
            })
            .await?;
        }
        _ => (),
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaunchAttempt {
    New,
    Lookup,
    Retry,
}
fn recovery_unresolved(error: impl std::fmt::Display) -> Mutation {
    Mutation::Unresolved(format!(
        "Recovery unresolved: {error}. Look up or retry the original saved request."
    ))
}
async fn admission(
    s: &Session,
    out: &Out,
    tag: Tag,
    saved: LaunchReceipt,
    attempt: LaunchAttempt,
) -> Result<()> {
    let result = if attempt == LaunchAttempt::Lookup {
        s.client.lookup(&saved.pending()).await
    } else {
        s.client.launch(&saved.pending()).await
    };
    match result {
        Ok(accepted) => {
            let store = ReceiptStore::new(&s.resolved.state_dir);
            let run = accepted.run_id.clone();
            let original = saved.clone();
            match local(move || {
                store
                    .acknowledge(&original, &run)
                    .map_err(|e| e.to_string())
            })
            .await
            {
                Ok(saved) => {
                    out.send(tag, None, Data::Accepted(Box::new(accepted), saved))
                        .await
                }
                Err(error) => {
                    out.send(
                        tag,
                        None,
                        Data::LaunchProgress(
                            Mutation::Unresolved(format!(
                                "Accepted remotely; local receipt update failed: {error}"
                            )),
                            Some(saved),
                        ),
                    )
                    .await
                }
            }
        }
        Err(error) => {
            if attempt == LaunchAttempt::New && error.api_code() == Some("stale_preview") {
                out.send(tag, None, Data::PreviewError("Frozen inputs changed; refresh and review the displayed preview before launching".into())).await;
            }
            // A fresh key has carried no other request, so refusing its only request
            // before admission proves that nothing was admitted. Drop its receipt so
            // it never reads as an unresolved launch that could be retried later.
            if attempt == LaunchAttempt::New
                && matches!(error.api_code(), Some("run_busy" | "stale_preview"))
            {
                let store = ReceiptStore::new(&s.resolved.state_dir);
                let refused = saved.clone();
                let data = match local(move || store.discard(&refused).map_err(|e| e.to_string()))
                    .await
                {
                    Ok(()) => {
                        Data::LaunchRefused(error.to_string(), error.api_code() == Some("run_busy"))
                    }
                    Err(local_error) => Data::LaunchProgress(
                        Mutation::Rejected(format!(
                            "{error}; its saved request remains because discarding it failed: {local_error}"
                        )),
                        Some(saved),
                    ),
                };
                out.send(tag, None, data).await;
                return Ok(());
            }
            // A recovery failure describes this lookup/retry, not the original
            // admission. Even 404, a rejected retry or a validation failure cannot
            // prove that the earlier request was never accepted. Only a validated
            // successful response plus its saved acknowledgment resolves recovery.
            let state = if attempt != LaunchAttempt::New {
                recovery_unresolved(error)
            } else if matches!(
                error,
                Error::AmbiguousMutation(_) | Error::Transport | Error::Timeout
            ) {
                Mutation::Unresolved(error.to_string())
            } else {
                Mutation::Rejected(error.to_string())
            };
            out.send(tag, None, Data::LaunchProgress(state, Some(saved)))
                .await;
        }
    }
    Ok(())
}
async fn recipe_save(
    s: &Session,
    out: &Out,
    tag: Tag,
    receipt: RecipeSaveReceipt,
    attempt: LaunchAttempt,
) {
    out.send(tag, None, Data::RecipeSaveProgress(Mutation::Submitting))
        .await;
    let result = if attempt == LaunchAttempt::Lookup {
        s.client.lookup_recipe_save(&receipt.pending).await
    } else {
        s.client.save_recipe(&receipt.pending).await
    };
    let data = match result {
        Ok(response) => {
            let store = RecipeSaveStore::new(&s.resolved.state_dir);
            let saved = response.clone();
            match local(move || {
                store
                    .acknowledge(&receipt, &saved)
                    .map_err(|e| e.to_string())
            })
            .await
            {
                Ok(_) => Data::RecipeSaved(Box::new(response)),
                Err(error) => Data::RecipeSaveProgress(Mutation::Unresolved(format!(
                    "Saved remotely; receipt acknowledgment failed: {error}"
                ))),
            }
        }
        Err(error) => {
            // Only a definitive refusal of this fresh request permits a new key.
            // A lookup 404 or rejected retry cannot resolve an earlier submission.
            let refused = attempt == LaunchAttempt::New
                && matches!(
                    error.api_code(),
                    Some(
                        "invalid_parameters"
                            | "source_conflict"
                            | "head_conflict"
                            | "catalog_full"
                            | "not_found"
                            | "unsupported_capability"
                    )
                );
            let error = safe(error);
            if refused {
                let store = RecipeSaveStore::new(&s.resolved.state_dir);
                match local(move || store.discard(&receipt).map_err(|e| e.to_string())).await {
                    Ok(()) => Data::RecipeSaveProgress(Mutation::Rejected(format!(
                        "{error}. Your edits are retained."
                    ))),
                    Err(local_error) => Data::RecipeSaveProgress(Mutation::Unresolved(format!(
                        "{error}; receipt remains: {local_error}"
                    ))),
                }
            } else {
                Data::RecipeSaveProgress(Mutation::Unresolved(format!(
                    "{error}. Look up or retry the same saved request."
                )))
            }
        }
    };
    out.send(tag, None, data).await;
}

async fn model_registration(
    s: &Session,
    out: &Out,
    tag: Tag,
    receipt: ModelRegistrationReceipt,
    attempt: LaunchAttempt,
) {
    out.send(
        tag,
        None,
        Data::ModelRegistrationProgress(Mutation::Submitting),
    )
    .await;
    let result = if attempt == LaunchAttempt::Lookup {
        s.client.lookup_model_registration(&receipt.pending).await
    } else {
        s.client.register_model(&receipt.pending).await
    };
    let data = match result {
        Ok(response) => {
            let store = ModelRegistrationStore::new(&s.resolved.state_dir);
            let saved = response.clone();
            match local(move || {
                store
                    .acknowledge(&receipt, &saved)
                    .map_err(|e| e.to_string())
            })
            .await
            {
                Ok(_) => Data::ModelRegistered(Box::new(response)),
                Err(error) => Data::ModelRegistrationProgress(Mutation::Unresolved(format!(
                    "Registered remotely; receipt acknowledgment failed: {error}"
                ))),
            }
        }
        Err(error) => {
            // Only a definitive refusal of this fresh request permits a new key.
            // A lookup 404 or rejected retry cannot resolve an earlier submission.
            let refused = attempt == LaunchAttempt::New
                && matches!(
                    error.api_code(),
                    Some(
                        "invalid_parameters"
                            | "model_metadata_changed"
                            | "model_registry_full"
                            | "limit_exceeded"
                            | "unsupported_capability"
                    )
                );
            let error = safe(error);
            if refused {
                let store = ModelRegistrationStore::new(&s.resolved.state_dir);
                match local(move || store.discard(&receipt).map_err(|e| e.to_string())).await {
                    Ok(()) => Data::ModelRegistrationProgress(Mutation::Rejected(format!(
                        "{error}. The inspected identity is retained."
                    ))),
                    Err(local_error) => Data::ModelRegistrationProgress(Mutation::Unresolved(
                        format!("{error}; receipt remains: {local_error}"),
                    )),
                }
            } else {
                Data::ModelRegistrationProgress(Mutation::Unresolved(format!(
                    "{error}. Look up or retry the same saved request."
                )))
            }
        }
    };
    out.send(tag, None, data).await;
}

async fn archived_model_bytes(
    client: &Client,
    run: &RunId,
    expected: &playtime_protocol::archive::ContentRef,
) -> playtime_client::Result<Vec<u8>> {
    if expected.size_bytes > 4 << 20 {
        return Err(Error::LimitExceeded);
    }
    let mut cursor = None;
    for _ in 0..64 {
        let page = client
            .artifacts(
                run,
                &PageQuery {
                    limit: Some(200),
                    cursor,
                },
            )
            .await?;
        if let Some(artifact) = page.items.iter().find(|a| &a.content == expected) {
            // Each retry starts with an empty buffer; partial downloads never
            // become displayed identity. The shared client verifies the digest.
            let mut bytes = Vec::new();
            client
                .download(run, artifact, &mut bytes, &CancellationToken::new())
                .await?;
            return Ok(bytes);
        }
        if page.next_cursor.is_none() {
            break;
        }
        cursor = page.next_cursor;
    }
    Err(Error::ContentMismatch)
}

async fn content_bytes(
    client: &Client,
    run: &RunId,
    artifact: &Artifact,
    limit: u64,
) -> Result<Vec<u8>> {
    if artifact.content.size_bytes > limit {
        return Err(format!(
            "Preview limit is {} KiB; use verified download for the full saved content",
            limit / 1024
        ));
    }
    let mut bytes = vec![];
    client
        .download(run, artifact, &mut bytes, &CancellationToken::new())
        .await
        .map_err(safe)?;
    Ok(bytes)
}
async fn retained_artifact(
    client: &Client,
    run: &RunId,
    expected: &playtime_protocol::archive::ContentRef,
) -> Result<Artifact> {
    let mut cursor = None;
    let mut found = None;
    for _ in 0..64 {
        let page = client
            .artifacts(
                run,
                &PageQuery {
                    limit: Some(200),
                    cursor,
                },
            )
            .await
            .map_err(safe)?;
        found = page
            .items
            .into_iter()
            .find(|artifact| &artifact.content == expected);
        if found.is_some() || page.next_cursor.is_none() {
            break;
        }
        cursor = page.next_cursor;
    }
    let artifact = found.ok_or("Retained payload has no matching opaque content ID in the first 12,800 published artifacts; source reference remains available")?;
    Ok(artifact)
}

async fn memory(client: &Client, run: &RunId, content: &ContentAvailability) -> Result<String> {
    match content {
        ContentAvailability::Unavailable { reason } => Err(format!("Unavailable: {reason}")),
        ContentAvailability::Available {
            artifact_id,
            content,
        } => {
            let read: Result<String> = async {
                let artifact = client.artifact(run, artifact_id).await.map_err(safe)?;
                if artifact.content != *content {
                    return Err("Content identity mismatch".into());
                }
                let bytes = content_bytes(client, run, &artifact, 256 << 10).await?;
                String::from_utf8(bytes).map_err(|_| "Memory is not UTF-8 source".into())
            }
            .await;
            read.map_err(|e| format!("Unavailable preview: {e}"))
        }
    }
}

enum Input {
    Record(
        EventEnvelope,
        Option<String>,
        bool,
        oneshot::Sender<()>,
        (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
    ),
    HistoryDone,
    Reloaded(Reload, Option<String>),
    Error(String),
    End(StreamEnd),
}
async fn deliver(
    tx: &mpsc::Sender<Input>,
    budget: &Arc<Inbox>,
    event: EventEnvelope,
    page: Option<String>,
    live: bool,
) -> Result<()> {
    let size = serde_json::to_vec(&event).map_err(|e| e.to_string())?.len();
    if size > 1 << 20 {
        return Err("Event exceeds preview inbox record limit".into());
    }
    let record = budget
        .records
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| e.to_string())?;
    let permit = budget
        .bytes
        .clone()
        .acquire_many_owned(size as u32)
        .await
        .map_err(|e| e.to_string())?;
    let (ack, applied) = oneshot::channel();
    budget.high_bytes.fetch_max(
        (4 << 20) - budget.bytes.available_permits(),
        Ordering::Relaxed,
    );
    budget
        .high_records
        .fetch_max(64 - budget.records.available_permits(), Ordering::Relaxed);
    tx.send(Input::Record(event, page, live, ack, (permit, record)))
        .await
        .map_err(|_| "Observer detached")?;
    applied.await.map_err(|_| "Record was not applied".into())
}
async fn observe(
    client: Client,
    run: RunId,
    tag: Tag,
    mut requests: mpsc::Receiver<Command>,
    out: Out,
    stop: CancellationToken,
) {
    let task = async {
        let reads = Reads {
            client: client.clone(),
            run: run.clone(),
            tag,
            out: out.clone(),
            activity: Arc::new(Mutex::new([0; 2])),
        };
        let mut waiting = false;
        let anchors = loop {
            let view = reads.run().await?;
            let anchors = view.event_anchors.clone();
            out.send(
                tag,
                Some(run.clone()),
                Data::Run(Box::new(prepare_run(view))),
            )
            .await;
            if let Some(anchors) = anchors {
                if waiting {
                    reads.activity(Some(true), None).await;
                }
                break anchors;
            }
            if !waiting {
                reads.activity(None, Some(true)).await;
                waiting = true;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let (tx, mut events) = mpsc::channel(64);
        let budget = Arc::new(Inbox::default());
        let mut jobs = JoinSet::new();
        let live_client = client.clone();
        let live_run = run.clone();
        let live_tx = tx.clone();
        let live_budget = budget.clone();
        let live_stop = stop.clone();
        jobs.spawn(async move {
            let result = async {
                let mut follow =
                    live_client.follow(&live_run, Some(anchors.live_cursor), observation_retry());
                loop {
                    match follow.next(&live_stop).await.map_err(safe)? {
                        Observation::Record(record) => {
                            deliver(&live_tx, &live_budget, record.event.clone(), None, true)
                                .await?;
                            follow.acknowledge(&record).map_err(safe)?;
                        }
                        Observation::End(end) => {
                            let _ = live_tx.send(Input::End(end)).await;
                            return Ok::<_, String>(());
                        }
                        Observation::Disconnected => {
                            return Err("Stream disconnected; completion is unconfirmed".into());
                        }
                    }
                }
            }
            .await;
            if let Err(error) = result {
                let _ = live_tx.send(Input::Error(error)).await;
            }
        });
        let status_reads = reads.clone();
        let status_tx = tx.clone();
        jobs.spawn(async move {
            let mut poll = interval(Duration::from_secs(1));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                poll.tick().await;
                match status_reads.run().await {
                    Ok(view) => {
                        status_reads
                            .out
                            .send(
                                tag,
                                Some(status_reads.run.clone()),
                                Data::Run(Box::new(prepare_run(view))),
                            )
                            .await
                    }
                    Err(error) => {
                        let _ = status_tx.send(Input::Error(error)).await;
                        break;
                    }
                }
            }
        });
        let history_reads = reads.clone();
        let history_tx = tx.clone();
        let reload_budget = budget.clone();
        jobs.spawn(async move {
            let result = async {
                let mut cursor = anchors.history_cursor;
                loop {
                    let page = history_reads.history(&cursor).await?;
                    for event in page.events {
                        deliver(&history_tx, &budget, event, Some(cursor.clone()), false).await?;
                    }
                    let Some(next) = page.next_cursor else {
                        break;
                    };
                    cursor = next;
                }
                let _ = history_tx.send(Input::HistoryDone).await;
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = result {
                let _ = history_tx.send(Input::Error(error)).await;
            }
        });
        let mut index = Transcript::default();
        let mut reloading = false;
        let mut reload_failed = false;
        let mut pending_source: Option<Reload> = None;
        let mut range = WindowRequest {
            view: View::Readable,
            start: 0,
            count: 32,
            anchor: None,
        };
        let mut dirty = true;
        let mut history_done = false;
        let mut session_page = 0;
        let mut location = None;
        let mut live = 0;
        let mut redraw = interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = jobs.join_next(), if !jobs.is_empty() => (),
                Some(input) = events.recv() => match input {
                    Input::Record(event, page, is_live, ack, _permit) => {
                        let page = page.as_deref().map(|p| index.page(p)).transpose()?;
                        if index.apply(event, page)? && is_live { live += 1; }
                        let _ = ack.send(()); dirty = true;
                    }
                    Input::HistoryDone => { history_done = true; dirty = true; }
                    Input::Reloaded(request, error) => {
                        reloading = false; dirty = true;
                        if let Some(error) = error { reload_failed = true; out.send(request.tag.unwrap_or(tag), Some(run.clone()), Data::Notice(error)).await; }
                        else if let Some(tag) = request.tag && let Some(source) = index.raw(request.sequence, request.offset) { out.send(tag, Some(run.clone()), Data::Source(request.sequence, source)).await; }
                    }
                    Input::Error(error) => return Err(error),
                    Input::End(end) => { out.send(tag, Some(run.clone()), Data::Notice(format!("Stream reconciled: {}; lifecycle and cleanup are shown separately", transcript::words(&end.status)))).await; }
                },
                Some(command) = requests.recv() => {
                    if command.tag.connection != tag.connection || command.tag.selection != tag.selection { continue; }
                    match command.action {
                    Action::CancelNavigation => { location = None; range.anchor = None; }
                    Action::Locate(sequence, view) => { location = Some((sequence, view)); range.anchor = None; dirty = true; reload_failed = false; }
                    Action::Sessions(start) => { session_page = start; dirty = true; }
                    Action::Window(request) => {
                        // A later request in the same view keeps a row still to locate.
                        let anchor = request.anchor.or(range.anchor.filter(|_| range.view == request.view));
                        range = WindowRequest { anchor, ..request }; dirty = true; reload_failed = false;
                    }
                    Action::Source(row, requested, offset) => {
                        let sources = index.sources(row);
                        let sequence = sources.iter().copied().find(|seq| *seq >= requested).or_else(|| sources.last().copied());
                        if let Some(seq) = sequence {
                            if let Some(source) = index.raw(seq, offset) { out.send(command.tag, Some(run.clone()), Data::Source(seq, source)).await; }
                            else { pending_source = Some(Reload { tag: Some(command.tag), sequence: seq, offset, cursor: index.cursor_for(seq).map(str::to_owned) }); }
                            dirty = true;
                        }
                    }
                    _ => (),
                    }
                },
                _ = redraw.tick(), if dirty => {
                    index.metrics.inbox_bytes_high_water = reload_budget.high_bytes.load(Ordering::Relaxed);
                    index.metrics.inbox_records_high_water = reload_budget.high_records.load(Ordering::Relaxed);
                    let mut window = index.window(range.view, range.start, range.count);
                    // Answer a view change once, at the row it was made around.
                    if let Some(id) = range.anchor.take() && let Some(at) = index.position(range.view, id) {
                        range.start = at.saturating_sub(2);
                        window = index.window(range.view, range.start, range.count);
                        window.anchor = Some((id, at));
                    }
                    if let Some((sequence, view)) = location
                        && let Some((id, at)) = index.location(view, sequence) {
                        range.view = view;
                        range.start = at.saturating_sub(2);
                        window = index.window(view, range.start, range.count);
                        window.location = Some((sequence, id));
                        location = None;
                    }
                    if !reloading && (!reload_failed || pending_source.is_some()) {
                        let request = pending_source.take().or_else(|| window.rows.iter().flat_map(|row| index.sources(row.id)).find(|seq| !index.contains(*seq)).map(|sequence| Reload { tag: None, sequence, offset: 0, cursor: index.cursor_for(sequence).map(str::to_owned) }));
                        if let Some(request) = request { reloading = true; jobs.spawn(reload(reads.clone(), tx.clone(), reload_budget.clone(), request)); }
                    }
                    window.progress = index.progress(history_done);
                    window.sessions = index.sessions(session_page, history_done);
                    let status = format!("{} records indexed · {}", index.metrics.records, if history_done { "Earlier history loaded" } else { "Loading earlier history independently of live activity" });
                    out.send(tag, Some(run.clone()), Data::Window(window, status, live)).await; dirty = false;
                }
            }
        }
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        Ok::<_, String>(())
    };
    let result = tokio::select! { _ = stop.cancelled() => Ok(()), result = task => result };
    if let Err(error) = result {
        out.send(
            tag,
            Some(run),
            Data::Connection(ConnectionState::Error(error)),
        )
        .await;
    }
}

/// One selected animation. The producer waits for UI application before decoding
/// another frame; stopping observation never cancels the experiment.
async fn play_gif(
    client: Client,
    run: RunId,
    id: ArtifactId,
    tag: Tag,
    out: Out,
    previews: Arc<Semaphore>,
    stop: CancellationToken,
) {
    let work = async {
        let _permit = previews.acquire_owned().await.map_err(|e| e.to_string())?;
        let artifact = client.artifact(&run, &id).await.map_err(safe)?;
        let bytes = content_bytes(&client, &run, &artifact, media::ENCODED_BYTES as u64).await?;
        let handle = tokio::runtime::Handle::current();
        let out = out.clone();
        let run = run.clone();
        let stop = stop.clone();
        local(move || {
            let mut gif = media::Gif::new(bytes)?;
            loop {
                if stop.is_cancelled() { return Ok("Animation paused".into()); }
                let Some((pixels, delay)) = gif.next_frame()? else { return Ok("GIF finished; Replay starts from the first frame".into()); };
                handle.block_on(async {
                    tokio::select! {
                        _ = stop.cancelled() => Err("Animation paused".to_string()),
                        result = async {
                            let (applied, wait) = oneshot::channel();
                            out.send(tag, Some(run.clone()), Data::GifFrame(Arc::new(pixels), applied)).await;
                            wait.await.map_err(|_| "Animation selection changed".to_string())?;
                            tokio::time::sleep(Duration::from_millis(delay)).await;
                            Ok(())
                        } => result,
                    }
                })?;
            }
        }).await
    };
    tokio::select! {
        _ = stop.cancelled() => (),
        result = work => { out.send(tag, Some(run.clone()), Data::GifStopped(result.unwrap_or_else(|e| e))).await; }
    }
}

struct Reload {
    tag: Option<Tag>,
    sequence: u64,
    offset: usize,
    cursor: Option<String>,
}
async fn reload(reads: Reads, tx: mpsc::Sender<Input>, budget: Arc<Inbox>, request: Reload) {
    let result = async {
        let mut cursor = match &request.cursor {
            Some(cursor) => cursor.clone(),
            None => {
                reads
                    .run()
                    .await?
                    .event_anchors
                    .ok_or("Evidence is unavailable")?
                    .history_cursor
            }
        };
        loop {
            let page = reads.history(&cursor).await?;
            if page
                .events
                .iter()
                .any(|event| event.sequence.get() == request.sequence)
            {
                for event in page.events {
                    deliver(&tx, &budget, event, Some(cursor.clone()), false).await?;
                }
                return Ok::<_, String>(());
            }
            cursor = page
                .next_cursor
                .ok_or("Requested source is not in confirmed history")?;
        }
    }
    .await;
    let _ = tx.send(Input::Reloaded(request, result.err())).await;
}

struct Inbox {
    bytes: Arc<Semaphore>,
    records: Arc<Semaphore>,
    high_bytes: AtomicUsize,
    high_records: AtomicUsize,
}
impl Default for Inbox {
    fn default() -> Self {
        Self {
            bytes: Arc::new(Semaphore::new(4 << 20)),
            records: Arc::new(Semaphore::new(64)),
            high_bytes: AtomicUsize::new(0),
            high_records: AtomicUsize::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_refusals_explain_recovery_without_displaying_unreviewed_details() {
        for (reason, expected) in [
            (
                "selected_model_missing_or_changed",
                "Restore its registered identity",
            ),
            (
                "selected_model_unsupported",
                "no supported execution profile",
            ),
            ("model_unavailable", "model endpoint is unavailable"),
            (
                "unreviewed private diagnostic",
                "current service configuration",
            ),
        ] {
            let error = Error::Api {
                status: 503,
                response: playtime_protocol::v1::ErrorResponse {
                    code: "execution_unavailable".into(),
                    message: "unreviewed private message".into(),
                    request_id: None,
                    details: [("reasons".into(), serde_json::json!([reason]))]
                        .into_iter()
                        .collect(),
                },
            };
            let message = safe(error);
            assert!(message.contains(expected));
            assert!(message.contains("Saved definitions remain readable and editable"));
            assert!(!message.contains("unreviewed private"));
        }
    }
    #[test]
    fn prepared_windows_share_the_100_record_frame_budget() {
        let (commands, _commands) = mpsc::channel(64);
        let (tx, replies) = mpsc::channel(64);
        let mut backend = Backend {
            commands,
            replies,
            stop: CancellationToken::new(),
            pending: None,
            paint: egui::Context::default(),
            prepared_this_frame: 0,
            thread: None,
        };
        for number in 0..3 {
            let rows = (0..64)
                .map(|id| transcript::Row {
                    id,
                    first_sequence: id as u64 + 1,
                    title: String::new(),
                    excerpt: String::new(),
                    status: String::new(),
                    sources: String::new(),
                    sections: vec![],
                    needs_loading: false,
                    ..Default::default()
                })
                .collect();
            tx.try_send(Reply {
                tag: Tag::default(),
                run: None,
                data: Data::Window(
                    transcript::Window {
                        start: number,
                        total: 64,
                        rows,
                        ..Default::default()
                    },
                    String::new(),
                    0,
                ),
            })
            .ok()
            .unwrap();
        }
        let mut model = Model::default();
        for number in 0..3 {
            backend.pump(&mut model);
            assert_eq!(model.window.start, number);
            assert_eq!(backend.prepared_this_frame, 64);
        }
    }
    #[tokio::test]
    async fn missing_memory_is_not_an_available_empty_source() {
        // This branch never opens content or interprets the reason as model text.
        let root = playtime_test_support::TestRoot::new().unwrap();
        let credentials = playtime_local::Credentials::initialize(&root.join("service")).unwrap();
        let fixture = Arc::new(playtime_server::fixture::Fixture::new(
            ServiceId::new("missing-memory").unwrap(),
        ));
        let server = playtime_server::http::Server::new(fixture, credentials.token.clone())
            .unwrap()
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = Client::connect(
            &server.endpoint(),
            credentials.token.clone(),
            Some(ServiceId::new("missing-memory").unwrap()),
        )
        .await
        .unwrap();
        let missing = memory(
            &client,
            &RunId::new("run").unwrap(),
            &ContentAvailability::Unavailable {
                reason: "not retained".into(),
            },
        )
        .await;
        assert!(missing.is_err());
        assert!(missing.unwrap_err().contains("not retained"));
        server.stop().await.unwrap();
    }
}
