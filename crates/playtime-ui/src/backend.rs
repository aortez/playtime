//! Owned background runtime. Network, private files, parsing and decoding never
//! run in egui callbacks. Observation cancellation only detaches the client.
use crate::{
    media,
    model::*,
    transcript::{self, Transcript},
};
use playtime_client::{
    Client, Error, Observation, RetryPolicy,
    config::{self, Environment, Options, Resolved},
    state::{LaunchReceipt, ReceiptStore},
};
use playtime_core::RunId;
use playtime_local::{PrivateDirectory, read_private, replace_private};
use playtime_protocol::{api::*, events::EventEnvelope};
use std::{
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
                    model.notice = "Client background work stopped".into();
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
    outages: Arc<Mutex<usize>>,
}
impl Reads {
    async fn retry<T, F, R>(&self, request: F) -> Result<T>
    where
        F: FnMut() -> R,
        R: std::future::Future<Output = playtime_client::Result<T>>,
    {
        let mut recovering = false;
        let result = crate::read_retry::read(observation_retry(), request, || {
            recovering = true;
            async {
                let mut outages = self.outages.lock().await;
                *outages += 1;
                if *outages == 1 {
                    self.out
                        .send(
                            self.tag,
                            Some(self.run.clone()),
                            Data::Connection(ConnectionState::Reconnecting),
                        )
                        .await;
                }
            }
        })
        .await;
        if recovering {
            let mut outages = self.outages.lock().await;
            *outages -= 1;
            let state = match &result {
                Err(error) => Some(ConnectionState::Error(error.to_string())),
                Ok(_) if *outages == 0 => Some(ConnectionState::Connected),
                _ => None,
            };
            if let Some(state) = state {
                self.out
                    .send(self.tag, Some(self.run.clone()), Data::Connection(state))
                    .await;
            }
        }
        result.map_err(safe)
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
    let previews = Arc::new(Semaphore::new(2));
    let mut animation: Option<CancellationToken> = None;
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
                if matches!(command.action, Action::Window(..) | Action::Source(..)) {
                    if let Some((_, tx, _)) = &observer { tokio::select! { _ = stop.cancelled() => break, _ = tx.send(command) => () } }
                    continue;
                }
                let Some(s) = &session else { out.send(tag, None, action_error(&command.action, "Connect to a service first".into())).await; continue; };
                if jobs.len() >= 16 { out.send(tag, None, action_error(&command.action, "Client work limit reached; retry the explicit action".into())).await; continue; }
                let session = s.clone(); let out = out.clone(); let previews = previews.clone();
                jobs.spawn(async move {
                    let run = match &command.action {
                        Action::Cancel(run) | Action::Checkpoints(run, _) | Action::Checkpoint(run, _) | Action::Content(run, _, _) | Action::Retained(run, _) | Action::Download(run, _, _) => Some(run.clone()), _ => None,
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
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}
fn action_error(action: &Action, error: String) -> Data {
    match action {
        Action::Preview(..) => Data::PreviewError(error),
        Action::Launch(..) => Data::LaunchProgress(Mutation::Rejected(error), None),
        Action::Recover(..) => Data::LaunchProgress(recovery_unresolved(error), None),
        Action::Cancel(..) => Data::CancellationUnresolved(error),
        _ => Data::Error(error),
    }
}
pub fn prepare_preview(value: Preview) -> PreviewView {
    let limits = &value.parameters.limits;
    let workshop = &value.parameters.workshop;
    let summary = vec![
        format!(
            "Mode: {:?} · revision {}",
            value.mode, value.preview_revision
        ),
        format!(
            "Seeds: {} files · {} bytes",
            value.seeds.files, value.seeds.bytes
        ),
        format!(
            "Limits: {} sessions · {} turns/session · {} seconds",
            limits.sessions, limits.work_turns_per_session, limits.wall_seconds
        ),
        format!(
            "Tokens: {} total · {} per call · {} handoff · {} context",
            limits.generated_tokens,
            limits.tokens_per_call,
            limits.handoff_tokens,
            limits.context_tokens
        ),
        format!(
            "Timeouts: {}s model · {}s command",
            limits.model_seconds, limits.command_seconds
        ),
        format!(
            "Output: {} bytes · memory: {} bytes · log: {} bytes",
            limits.output_bytes, limits.memento_bytes, limits.log_bytes
        ),
        format!(
            "Workshop resources: {} cores · {} MiB RAM · {} MiB workspace · {} PIDs",
            workshop.cpu_cores,
            workshop.memory_bytes.get() / (1 << 20),
            workshop.workspace_bytes.get() / (1 << 20),
            workshop.pids
        ),
        format!(
            "Sampling seed: {} · memory condition: {:?}",
            value.parameters.sampling_seed, value.parameters.memory_condition
        ),
        format!("Model: {}", value.model_identity),
        format!("Workshop: {}", value.workshop_identity),
        if value.readiness.ready {
            "Ready to launch".into()
        } else {
            format!("Not ready: {}", value.readiness.reasons.join("; "))
        },
    ];
    let source = transcript::source_page(&serde_json::to_string_pretty(&value).unwrap(), 0);
    PreviewView {
        value,
        summary,
        source,
    }
}
pub fn prepare_run(value: RunView) -> RunViewData {
    let mut summary = vec![
        format!(
            "Mode {:?} · orchestration {:?}{} · controller {}",
            value.mode,
            value.orchestration.state,
            value
                .orchestration
                .reason
                .as_deref()
                .map_or(String::new(), |reason| format!(
                    " · {}",
                    orchestration_reason(reason)
                )),
            value
                .controller_phase
                .map_or("unavailable".into(), |phase| format!("{phase:?}"))
        ),
        format!(
            "Outcome: {}",
            value
                .outcome
                .as_ref()
                .map_or("not committed".into(), |o| format!("{:?}", o.reason))
        ),
        format!(
            "Cleanup: {:?} · evidence: {:?}",
            value.cleanup, value.evidence
        ),
    ];
    summary.push(value.usage.as_ref().map_or(
        "Known usage unavailable · uncertain usage unavailable".into(),
        |u| {
            format!(
                "{} {} · uncertain charge {} across {} requests · charged {} · complete {}",
                if value.accounting_coverage.is_some() {
                    "Settled known"
                } else {
                    "Known"
                },
                u.known_generated_tokens,
                u.unknown_reserved_tokens,
                u.unknown_requests,
                u.budget_charged_tokens,
                u.usage_complete
            )
        },
    ));
    summary.push(value.accounting_coverage.as_ref().map_or_else(
        || "Pending allowance and allocation: unavailable from this API projection".into(),
        |coverage| {
            format!(
                "Pending allowance {} across {} unsettled requests · coverage {:?}",
                coverage.unsettled_allowance, coverage.unsettled_requests, coverage.availability
            )
        },
    ));
    if let Some(c) = &value.contention {
        summary.push(if c.possible {
            format!(
                "Possible GPU contention: {} model reloads and {} queued requests across {} timed requests",
                c.reloads, c.queue_waits, c.timed_requests
            )
        } else {
            format!(
                "No GPU contention flagged across {} timed requests",
                c.timed_requests
            )
        });
    }
    if let Some(cancel) = &value.cancellation {
        summary.push(format!(
            "Cancellation: {:?} · {}",
            cancel.enforcement,
            cancel
                .uncertainty
                .as_deref()
                .unwrap_or("no uncertainty reported")
        ));
    }
    let source = transcript::source_page(&serde_json::to_string_pretty(&value).unwrap(), 0);
    RunViewData {
        value,
        summary,
        source,
    }
}
/// Reasons are snake_case API words. A released run must read differently from
/// a completed one.
pub fn orchestration_reason(reason: &str) -> String {
    if reason == "operator_released" {
        "released by operator".into()
    } else {
        reason.replace('_', " ")
    }
}
async fn perform(s: &Session, out: &Out, command: Command, previews: Arc<Semaphore>) -> Result<()> {
    let tag = command.tag;
    let client = &s.client;
    match command.action {
        Action::PreviewPage(preview, offset) => {
            out.send(
                tag,
                None,
                Data::PreviewPage(transcript::source_page(
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
        Action::Checkpoints(run, cursor) => {
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
            out.send(tag, Some(run), Data::Checkpoints(page)).await;
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
            let mut cursor = None;
            let mut found = None;
            for _ in 0..64 {
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
                found = page
                    .items
                    .into_iter()
                    .find(|artifact| artifact.content == expected);
                if found.is_some() || page.next_cursor.is_none() {
                    break;
                }
                cursor = page.next_cursor;
            }
            let artifact = found.ok_or("Retained payload has no matching opaque content ID in the first 12,800 published artifacts; source reference remains available")?;
            let bytes = content_bytes(client, &run, &artifact, 64 << 20).await?;
            let text =
                String::from_utf8(bytes).map_err(|_| "Retained observation is not UTF-8 JSON")?;
            out.send(
                tag,
                Some(run),
                Data::Content(ContentView {
                    artifact,
                    source: Some(transcript::source_page(&text, 0)),
                    image: None,
                    notice: "Verified retained observation source".into(),
                    animated: false,
                }),
            )
            .await;
        }
        Action::Download(run, id, path) => {
            let artifact = client.artifact(&run, &id).await.map_err(safe)?;
            client
                .download_to_path(&run, &artifact, &path, &CancellationToken::new())
                .await
                .map_err(safe)?;
            out.send(
                tag,
                Some(run),
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
            outages: Arc::new(Mutex::new(0)),
        };
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
                break anchors;
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
        let mut range = (0, 32);
        let mut dirty = true;
        let mut history_done = false;
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
                    Input::End(end) => { out.send(tag, Some(run.clone()), Data::Notice(format!("Stream reconciled: {:?}; lifecycle and cleanup are shown separately", end.status))).await; }
                },
                Some(command) = requests.recv() => match command.action {
                    Action::Window(start, count) => { range = (start, count); dirty = true; reload_failed = false; }
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
                },
                _ = redraw.tick(), if dirty => {
                    if !reloading && (!reload_failed || pending_source.is_some()) {
                        let request = pending_source.take().or_else(|| index.window(range.0, range.1).rows.iter().flat_map(|row| index.sources(row.id)).find(|seq| !index.contains(*seq)).map(|sequence| Reload { tag: None, sequence, offset: 0, cursor: index.cursor_for(sequence).map(str::to_owned) }));
                        if let Some(request) = request { reloading = true; jobs.spawn(reload(reads.clone(), tx.clone(), reload_budget.clone(), request)); }
                    }
                    index.metrics.inbox_bytes_high_water = reload_budget.high_bytes.load(Ordering::Relaxed);
                    index.metrics.inbox_records_high_water = reload_budget.high_records.load(Ordering::Relaxed);
                    let status = format!("{} records indexed · {}", index.metrics.records, if history_done { "Earlier history loaded" } else { "Loading earlier history independently of live activity" });
                    out.send(tag, Some(run.clone()), Data::Window(index.window(range.0, range.1), status, live)).await; dirty = false;
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
                        metrics: Default::default(),
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
