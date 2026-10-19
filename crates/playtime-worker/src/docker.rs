//! One retained native effect owner. The controller thread never waits on Docker
//! or ownership-journal I/O, and a cancelled request cannot create another owner.
use playtime_docker::{
    CancellationToken, Engine, Ownership, OwnershipJournal, Proposal, ReadyImage, Sandbox, Tool,
    ToolLimits, ToolReply, WorkDeadline,
    capture::{ArchiveUsage, CaptureProfile, SnapshotCandidate},
    transport::Cli,
};
use playtime_engine::{
    Effect, EffectKind, FileInfo, Input, Memory, ModelFailure, ModelRequest, Outcome, ToolPolicy,
};
use playtime_ollama::{
    Adapter, ChatResult, EffectiveSettings, Endpoint, FailureKind, HttpResponse, ModelLock,
    NativeTransport, Observation, ObservationKind, Request, Resolution, Transport, Trust,
};
use playtime_protocol::worker::Binding;
use playtime_runner::{
    CheckpointBytes, Effects, Error, Result,
    docker::{LiveModel, Profile},
};
use playtime_storage::CommittedFrontier;
use playtime_worker_control::{Bootstrap, boot_ms, write_once};
use serde::Serialize;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Clone, Serialize)]
struct Intent {
    effect: u64,
    frontier: playtime_protocol::storage::EventFrontier,
}
struct Journal {
    directory: PathBuf,
    binding: Binding,
    intent: Mutex<Option<Intent>>,
    slot: Arc<tokio::sync::Semaphore>,
}
#[async_trait::async_trait]
impl OwnershipJournal for Journal {
    async fn commit(&self, proposal: &Proposal) -> playtime_docker::Result<()> {
        let error = || playtime_docker::Error {
            failure: playtime_docker::Failure::Journal,
            operation: "ownership",
            detail: "durable ownership publication failed; no create authorized",
        };
        let intent = self
            .intent
            .lock()
            .map_err(|_| error())?
            .clone()
            .ok_or_else(error)?;
        let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"binding":self.binding,
            "intent":intent,"proposal":proposal}))
        .map_err(|_| error())?;
        if bytes.len() > 32768 {
            return Err(error());
        }
        let permit = self.slot.clone().try_acquire_owned().map_err(|_| error())?;
        let path = self.directory.join(format!(
            "docker-{}-{}.json",
            match proposal.role {
                playtime_docker::Role::Volume => "volume",
                playtime_docker::Role::Holder => "holder",
                playtime_docker::Role::Agent => "agent",
            },
            proposal.session.unwrap_or(0)
        ));
        // Cancellation detaches observation only. The single retained permit
        // prevents an overlapping journal write after an uncertain publication.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            write_once(&path, &bytes)
        })
        .await
        .map_err(|_| error())?
        .map_err(|_| error())
    }
}

struct Task {
    effect: Effect,
    confirmed: CommittedFrontier,
    cancel: CancellationToken,
}
struct Reply {
    id: u64,
    outcome: Outcome,
    candidate: Option<SnapshotCandidate>,
}
struct Actor {
    sandbox: Sandbox,
    profile: Profile,
    memory: Memory,
    journal: Arc<Journal>,
    model: Model,
    chunks: mpsc::Sender<(u64, serde_json::Value)>,
    private: PathBuf,
    /// The controller's work deadline in boot milliseconds.
    work_end_ms: u64,
}
/// Scripted responses for the CPU and CI gates, or the live locked model.
enum Model {
    Scripted(VecDeque<playtime_engine::simulation::ModelScript>),
    Live(Box<Live>),
}
/// The live transport; CPU tests replace the native one.
struct Port(Box<dyn Transport>);
#[async_trait::async_trait]
impl Transport for Port {
    async fn send(&self, request: Request) -> playtime_ollama::Result<HttpResponse> {
        self.0.send(request).await
    }
}
struct Live {
    adapter: Adapter<Port>,
    settings: EffectiveSettings,
    limits: playtime_ollama::Limits,
}
impl Live {
    fn prepare(model: &LiveModel) -> crate::Result<Self> {
        // The static worker can't resolve mDNS names, so it connects only to
        // the addresses the daemon resolved and froze. It has no TLS: the
        // endpoint is plain HTTP and the trust setting is unused.
        let transport = NativeTransport::with_resolution(
            Endpoint::parse(&model.host.endpoint).map_err(invalid)?,
            Trust::BundledWebPki,
            Resolution::Pinned {
                addresses: model.host.addresses.clone(),
            },
            &playtime_ollama::Limits::default(),
        )
        .map_err(invalid)?;
        Self::with_transport(Box::new(transport), model)
    }
    fn with_transport(transport: Box<dyn Transport>, model: &LiveModel) -> crate::Result<Self> {
        let limits = playtime_ollama::Limits::default();
        let lock = ModelLock::parse(&serde_json::to_vec(&model.lock)?).map_err(invalid)?;
        Ok(Self {
            adapter: Adapter::new(Port(transport), lock, limits.clone()).map_err(invalid)?,
            settings: serde_json::from_value(model.settings.clone())?,
            limits,
        })
    }
    /// Streamed records become chunks while the request runs, so partial
    /// output is visible live and stays recorded if the request fails. A final
    /// timing chunk records the latency seen here and the server's durations.
    async fn chat(
        &mut self,
        id: u64,
        request: &ModelRequest,
        deadline: Instant,
        cancel: &CancellationToken,
        chunks: &mpsc::Sender<(u64, serde_json::Value)>,
    ) -> Result<Outcome> {
        let (sender, mut receiver) = playtime_ollama::observations(&self.limits)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let started = std::time::Instant::now();
        let (mut terminal, mut pending) = (None, Pending::default());
        let result = {
            let chat = self
                .adapter
                .chat(request, &self.settings, deadline, cancel, &sender);
            tokio::pin!(chat);
            loop {
                let due = pending.since.map(|since| since + CHUNK_INTERVAL);
                tokio::select! {
                    result = &mut chat => break result,
                    Some(observation) = receiver.recv() => {
                        terminal = forward(id, observation, &mut pending, chunks).or(terminal);
                    }
                    () = tokio::time::sleep_until(due.unwrap_or(deadline)), if due.is_some() => {
                        pending.flush(id, chunks);
                    }
                }
            }
        };
        let observed_ms = started.elapsed().as_millis() as u64;
        while let Some(observation) = receiver.try_recv() {
            terminal = forward(id, observation, &mut pending, chunks).or(terminal);
        }
        pending.flush(id, chunks);
        let mut timing = serde_json::json!({"observation": "timing", "observed_ms": observed_ms});
        for key in [
            "load_duration",
            "prompt_eval_duration",
            "eval_duration",
            "total_duration",
        ] {
            if let Some(ns) = terminal.as_ref().and_then(|t| t[key].as_u64()) {
                timing[key] = ns.into();
            }
        }
        let _ = chunks.send((id, timing));
        Ok(outcome(result))
    }
}
fn invalid(_: playtime_ollama::Failure) -> crate::Error {
    crate::Error::Invalid("invalid frozen live model configuration")
}
/// Native records arrive about once per token. They're coalesced into at most
/// two chunks a second, which keeps long runs inside their event-log budget and
/// lets live readers keep up.
const CHUNK_INTERVAL: Duration = Duration::from_millis(500);
const CHUNK_TEXT_BYTES: usize = 16 * 1024;
/// Message deltas from native records not yet sent as a chunk.
#[derive(Default)]
struct Pending {
    thinking: String,
    content: String,
    tool_calls: Vec<serde_json::Value>,
    records: u64,
    since: Option<Instant>,
}
impl Pending {
    fn add(&mut self, record: &serde_json::Value) {
        let message = &record["message"];
        self.thinking
            .push_str(message["thinking"].as_str().unwrap_or_default());
        self.content
            .push_str(message["content"].as_str().unwrap_or_default());
        if let Some(calls) = message["tool_calls"].as_array() {
            self.tool_calls.extend(calls.iter().cloned());
        }
        self.records += 1;
        self.since.get_or_insert_with(Instant::now);
    }
    /// The same message shape as a native record, plus how many it covers.
    fn flush(&mut self, id: u64, chunks: &mpsc::Sender<(u64, serde_json::Value)>) {
        if self.records == 0 {
            return;
        }
        let mut message = serde_json::json!({"role": "assistant"});
        for (key, text) in [
            ("thinking", std::mem::take(&mut self.thinking)),
            ("content", std::mem::take(&mut self.content)),
        ] {
            if !text.is_empty() {
                message[key] = text.into();
            }
        }
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = std::mem::take(&mut self.tool_calls).into();
        }
        let chunk = serde_json::json!({"message": message, "done": false, "records": self.records});
        (self.records, self.since) = (0, None);
        // A retired effect owner discards chunks; nothing is lost while it drains.
        let _ = chunks.send((id, chunk));
    }
}
/// Returns the terminal record, which is always sent verbatim.
fn forward(
    id: u64,
    observation: Observation,
    pending: &mut Pending,
    chunks: &mpsc::Sender<(u64, serde_json::Value)>,
) -> Option<serde_json::Value> {
    let text = || String::from_utf8_lossy(&observation.bytes).into_owned();
    let chunk = match &observation.kind {
        ObservationKind::NativeChunk => {
            match serde_json::from_slice::<serde_json::Value>(&observation.bytes) {
                Ok(record) if record["done"] != true => {
                    pending.add(&record);
                    if pending.thinking.len() + pending.content.len() >= CHUNK_TEXT_BYTES {
                        pending.flush(id, chunks);
                    }
                    return None;
                }
                Ok(record) => record,
                Err(_) => serde_json::json!({"observation": "native_chunk", "text": text()}),
            }
        }
        ObservationKind::HttpError { status, truncated } => serde_json::json!({
            "observation": "http_error", "status": status, "truncated": truncated, "text": text()}),
        ObservationKind::ProtocolFragment => {
            serde_json::json!({"observation": "protocol_fragment", "text": text()})
        }
    };
    pending.flush(id, chunks);
    let terminal = (chunk["done"] == true).then(|| chunk.clone());
    let _ = chunks.send((id, chunk));
    terminal
}
/// No retry here: only a tool-parse failure lets the controller ask again.
fn outcome(result: ChatResult) -> Outcome {
    match result.outcome {
        Ok(response) => Outcome::Model(response),
        Err(failure) => Outcome::ModelError(match failure.kind {
            FailureKind::BackendToolParse => ModelFailure::Parse(failure.detail),
            FailureKind::Timeout => ModelFailure::Timeout(failure.detail),
            FailureKind::MissingUsage => ModelFailure::MissingUsage(failure.detail),
            FailureKind::ByteLimit => ModelFailure::ByteLimit(failure.detail),
            FailureKind::Incomplete | FailureKind::Protocol => {
                ModelFailure::Incomplete(failure.detail)
            }
            FailureKind::Http | FailureKind::Transport => ModelFailure::Connection(failure.detail),
            FailureKind::Identity | FailureKind::Request | FailureKind::Cancelled => {
                ModelFailure::Infrastructure(failure.detail)
            }
        }),
    }
}
impl Actor {
    /// Writes a recipe's frozen seeds into `/work/seeds` through the fixed
    /// bridge, as uid 1000, before session 1's first request. Any failure stops
    /// the run before the model is asked anything.
    async fn seed(&mut self, deadline: Instant, cancel: &CancellationToken) -> Result<()> {
        let Some(recipe) = &self.profile.recipe else {
            return Ok(());
        };
        let limits = ToolLimits {
            command_seconds: 10.0,
            output_bytes: self.profile.scenario.parameters.limits.output_bytes.get() as u32,
        };
        for (path, content) in &recipe.seeds {
            let write = Tool::WriteFile {
                path: format!("seeds/{path}"),
                content: content.clone(),
                append: false,
            };
            match self
                .sandbox
                .tool(write, limits, deadline, cancel.clone())
                .await
                .map_err(|e| Error::Uncertain(e.to_string()))?
            {
                ToolReply::Write { bytes_written, .. } if bytes_written == content.len() as u64 => {
                }
                _ => return Err(Error::Invalid("seed import refused".into())),
            }
        }
        Ok(())
    }
    async fn execute(&mut self, task: &Task) -> Result<(Outcome, Option<SnapshotCandidate>)> {
        let effect = &task.effect;
        let deadline =
            Instant::now() + Duration::from_millis(effect.deadline_ms.saturating_sub(boot_ms()));
        if task.cancel.is_cancelled() || boot_ms() >= effect.deadline_ms {
            return Err(Error::Uncertain(
                "effect retired before native dispatch".into(),
            ));
        }
        *self.journal.intent.lock().expect("journal context") = Some(Intent {
            effect: effect.id,
            frontier: task.confirmed.frontier().clone(),
        });
        let cancel = task.cancel.clone();
        let map = |e: playtime_docker::Error| Error::Uncertain(e.to_string());
        let outcome = match &effect.kind {
            EffectKind::StartSession { ordinal, .. } => {
                // A changed model stops the run before this session dispatches.
                if let Model::Live(live) = &mut self.model {
                    live.adapter
                        .inspect(false, deadline, &cancel)
                        .await
                        .map_err(|e| Error::Invalid(format!("live model identity: {e}")))?;
                }
                if *ordinal == 1 {
                    self.sandbox
                        .initialize(deadline, cancel.clone())
                        .await
                        .map_err(map)?;
                }
                self.sandbox
                    .start_session(*ordinal, deadline, cancel.clone())
                    .await
                    .map_err(map)?;
                if *ordinal == 1 {
                    self.seed(deadline, &cancel).await?;
                }
                Outcome::Started(self.memory.clone())
            }
            EffectKind::Model(request) => match &mut self.model {
                Model::Scripted(models) => {
                    let model = models
                        .pop_front()
                        .ok_or_else(|| Error::Invalid("frozen model responses exhausted".into()))?;
                    let response = model
                        .completion
                        .ok_or_else(|| Error::Invalid("missing scripted completion".into()))?;
                    match response.outcome {
                        playtime_engine::simulation::ModelEnding::Response(r) => Outcome::Model(r),
                        playtime_engine::simulation::ModelEnding::Error(e) => {
                            Outcome::ModelError(e)
                        }
                    }
                }
                Model::Live(live) => {
                    live.chat(effect.id, request, deadline, &cancel, &self.chunks)
                        .await?
                }
            },
            EffectKind::Tool(request) => {
                let a = &request.arguments;
                let string = |key: &str| {
                    a[key]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| Error::Invalid("invalid frozen tool argument".into()))
                };
                let tool = match request.name.as_str() {
                    "exec" => Tool::Exec {
                        command: string("command")?,
                        cwd: a["cwd"].as_str().map(str::to_owned),
                        timeout_seconds: a["timeout_seconds"].as_f64(),
                    },
                    "read_file" => Tool::ReadFile {
                        path: string("path")?,
                        offset_bytes: a["offset_bytes"].as_u64().unwrap_or(0),
                    },
                    "write_file" => Tool::WriteFile {
                        path: string("path")?,
                        content: string("content")?,
                        append: a["append"].as_bool().unwrap_or(false),
                    },
                    _ => return Err(Error::Invalid("unsupported native tool".into())),
                };
                // Reserve return overhead INSIDE the original frozen effect cap.
                // The bridge's own timeout is authoritative only after its reply.
                let remaining = effect.deadline_ms.saturating_sub(boot_ms());
                let Some(command_ms) = remaining
                    .checked_sub(self.profile.command_return_ms)
                    .filter(|n| *n > 0)
                else {
                    if effect.deadline_ms >= self.work_end_ms {
                        // Too close to the work deadline to run and confirm a
                        // command, so nothing reaches Docker. The controller's
                        // wall limit retires this effect at that deadline.
                        tokio::select! {
                            _ = cancel.cancelled() => {}
                            _ = tokio::time::sleep_until(deadline) => {}
                        }
                        return Err(Error::Uncertain(
                            "work deadline left no time to run the command".into(),
                        ));
                    }
                    return Err(Error::Uncertain(
                        "insufficient command return allowance".into(),
                    ));
                };
                let limits = ToolLimits {
                    command_seconds: command_ms as f64 / 1000.0,
                    output_bytes: self.profile.scenario.parameters.limits.output_bytes.get() as u32,
                };
                Outcome::Tool(serde_json::to_value(
                    self.sandbox
                        .tool(tool, limits, deadline, cancel)
                        .await
                        .map_err(map)?,
                )?)
            }
            EffectKind::Snapshot { session_id } => {
                let profile = capture_profile(&self.profile, &task.confirmed)?;
                let mut stopped = self
                    .sandbox
                    .quiesce(deadline, cancel.clone())
                    .await
                    .map_err(map)?;
                let candidate = stopped.capture(&profile, cancel).await.map_err(map)?;
                let workspace = candidate.workspace();
                let files = workspace
                    .entries()
                    .iter()
                    .map(|e| {
                        (
                            e.path.clone(),
                            FileInfo {
                                size_bytes: e.size_bytes,
                                mode: e.mode,
                                entry_type: e.entry_type.clone(),
                                sha256: e.sha256.as_ref().map(|s| {
                                    playtime_core::Sha256Digest::new(s.clone())
                                        .expect("validated digest")
                                }),
                                link: e.link.clone(),
                            },
                        )
                    })
                    .collect();
                self.memory = Memory {
                    status: candidate.memory().status.clone(),
                    content: candidate.memory().content.clone(),
                };
                let snapshot = playtime_engine::Snapshot {
                    workspace: playtime_storage::content_ref(
                        playtime_core::ArchivePath::new(format!(
                            "sessions/{session_id}/workspace.tar"
                        ))
                        .unwrap(),
                        workspace.tar(),
                    ),
                    files,
                    memory: self.memory.clone(),
                };
                return Ok((Outcome::Snapshot(snapshot), Some(candidate)));
            }
            EffectKind::PublishCheckpoint(_) => Outcome::Published,
            EffectKind::Cleanup => {
                let cleanup = self.sandbox.cleanup(deadline, cancel).await;
                let evidence =
                    serde_json::to_vec(&serde_json::json!({"version":1,"cleanup":cleanup,
                    "observations":self.sandbox.observations()}))?;
                if evidence.len() > 256 * 1024 {
                    return Err(Error::Capacity("Docker diagnostic bound".into()));
                }
                // Service-private diagnostics are not writer receipts. Persist
                // before reporting cleanup, so uncertain writes retain admission.
                let path = self.private.join("docker-cleanup.json");
                tokio::task::spawn_blocking(move || write_once(&path, &evidence))
                    .await
                    .map_err(|_| Error::Uncertain("cleanup evidence owner lost".into()))?
                    .map_err(|e| Error::Uncertain(e.to_string()))?;
                Outcome::Cleaned(if cleanup.complete() {
                    vec![]
                } else {
                    vec![
                        "exact Docker cleanup remains uncertain; ownership records retained".into(),
                    ]
                })
            }
        };
        Ok((outcome, None))
    }
}

fn capture_profile(profile: &Profile, confirmed: &CommittedFrontier) -> Result<CaptureProfile> {
    let usage = confirmed.usage();
    let add = |a: u64, b: u64| {
        a.checked_add(b)
            .ok_or_else(|| Error::Capacity("capture reservation overflow".into()))
    };
    let storage = &profile.storage;
    // Conservative upper bounds for canonical index + source map, memory
    // leaves, event envelopes and references, on top of exact cumulative
    // writer-confirmed counts. The writer independently checks actual bytes.
    let usage = ArchiveUsage {
        content_bytes: add(
            usage.content_bytes,
            add(
                storage
                    .manifest_bytes
                    .checked_mul(2)
                    .ok_or_else(|| Error::Capacity("manifest reservation overflow".into()))?,
                profile
                    .capture
                    .memory_bytes
                    .checked_mul(2)
                    .ok_or_else(|| Error::Capacity("memory reservation overflow".into()))?,
            )?,
        )?,
        references: add(usage.references, 16)?,
        events: add(usage.events, 16)?,
        log_bytes: add(
            usage.log_bytes,
            storage
                .record_bytes
                .checked_mul(4)
                .ok_or_else(|| Error::Capacity("event reservation overflow".into()))?,
        )?,
    };
    CaptureProfile::prepare(
        profile.capture.clone(),
        profile.resources.workspace_bytes,
        storage,
        usage,
    )
    .map_err(|e| Error::Capacity(e.to_string()))
}

pub(crate) struct NativeEffects {
    prepared: Option<(ReadyImage, Ownership, Arc<Journal>, PathBuf, Model)>,
    profile: Profile,
    sender: Option<mpsc::SyncSender<Task>>,
    receiver: Option<mpsc::Receiver<Reply>>,
    chunks: Option<mpsc::Receiver<(u64, serde_json::Value)>>,
    active: Option<(u64, CancellationToken, bool)>,
    retiring: bool,
    /// Streamed chunks, then the effect's completion.
    inputs: VecDeque<(u64, Input)>,
    candidate: Option<SnapshotCandidate>,
}
impl NativeEffects {
    pub async fn prepare(profile: Profile, bootstrap: &Bootstrap) -> crate::Result<Self> {
        profile.validate()?;
        let cli = Cli::resolve(
            &profile.selection.executable,
            &profile.selection.socket,
            &bootstrap.private_dir.join("docker-config"),
        )
        .map_err(|_| crate::Error::Invalid("Docker selection unavailable"))?;
        if cli.selection() != &profile.selection {
            return Err(crate::Error::Invalid(
                "frozen Docker executable/socket changed",
            ));
        }
        let deadline = Instant::now()
            + Duration::from_millis(
                bootstrap
                    .startup_deadline_boot_ms
                    .saturating_sub(boot_ms())
                    .min(20000),
            );
        let engine = Engine::connect(Arc::new(cli), deadline, CancellationToken::new())
            .await
            .map_err(|_| crate::Error::Invalid("Docker daemon unavailable"))?;
        if engine.identity() != &profile.daemon {
            return Err(crate::Error::Invalid("frozen Docker daemon changed"));
        }
        let ready = engine
            .readiness(
                &serde_json::to_vec(&profile.image_lock)?,
                deadline,
                CancellationToken::new(),
            )
            .await
            .map_err(|_| crate::Error::Invalid("locked Docker image unavailable"))?;
        let binding = &bootstrap.binding;
        let owner = Ownership {
            service: binding.service_id.as_str().to_owned(),
            run: binding.run_id.to_string(),
            grant: binding.grant_id.as_str().to_owned(),
            invocation: binding.invocation_id.as_str().to_owned(),
            nonce: Ownership::random_nonce()
                .map_err(|_| crate::Error::Invalid("Docker nonce unavailable"))?,
            daemon: profile.daemon.clone(),
        };
        let journal = Arc::new(Journal {
            directory: bootstrap.private_dir.clone(),
            binding: binding.clone(),
            intent: Mutex::new(None),
            slot: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let model = match &profile.model {
            None => Model::Scripted(profile.scenario.models.clone().into()),
            Some(model) => Model::Live(Box::new(Live::prepare(model)?)),
        };
        Ok(Self {
            prepared: Some((ready, owner, journal, bootstrap.private_dir.clone(), model)),
            profile,
            sender: None,
            receiver: None,
            chunks: None,
            active: None,
            retiring: false,
            inputs: VecDeque::new(),
            candidate: None,
        })
    }
}
impl Effects for NativeEffects {
    fn policy(&self) -> ToolPolicy {
        ToolPolicy::ConfirmedExternal {
            return_overhead_ms: self.profile.command_return_ms,
        }
    }
    fn begin(&mut self, started_ms: u64) -> Result<()> {
        let (ready, owner, journal, private, model) = self
            .prepared
            .take()
            .ok_or_else(|| Error::Invalid("native owner already begun".into()))?;
        let now_boot = boot_ms();
        let monotonic = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let now_ns = monotonic.tv_sec as u64 * 1_000_000_000 + monotonic.tv_nsec as u64;
        let end = started_ms
            .checked_add(self.profile.scenario.parameters.limits.wall_seconds.get() * 1000)
            .ok_or_else(|| Error::Invalid("work deadline overflow".into()))?;
        let work = WorkDeadline::at(now_ns + end.saturating_sub(now_boot) * 1_000_000)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let sandbox = Sandbox::new(
            ready,
            owner,
            self.profile.resources.clone(),
            work,
            journal.clone(),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
        // Unbounded here, but each request's adapter limits bound its records.
        let (chunk_sender, chunks) = mpsc::channel();
        let mut actor = Actor {
            sandbox,
            profile: self.profile.clone(),
            memory: Memory {
                status: "missing".into(),
                content: String::new(),
            },
            journal,
            model,
            chunks: chunk_sender,
            private,
            work_end_ms: end,
        };
        let (sender, tasks) = mpsc::sync_channel::<Task>(1);
        let (replies, receiver) = mpsc::sync_channel(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::spawn(move || {
            while let Ok(task) = tasks.recv() {
                let result = runtime.block_on(actor.execute(&task));
                let (outcome, candidate) = match result {
                    Ok(v) => v,
                    Err(e) => (Outcome::Failed(e.to_string()), None),
                };
                if replies
                    .send(Reply {
                        id: task.effect.id,
                        outcome,
                        candidate,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        self.sender = Some(sender);
        self.receiver = Some(receiver);
        self.chunks = Some(chunks);
        Ok(())
    }
    fn poll(&mut self, now: u64) {
        if self.active.is_none() {
            return;
        }
        let result = self.receiver.as_ref().expect("begun").try_recv();
        // Drain after checking for the reply: every chunk the actor sent before
        // replying is then queued ahead of the completion.
        let id = self.active.as_ref().unwrap().0;
        while let Ok((effect_id, chunk)) = self.chunks.as_ref().expect("begun").try_recv() {
            if !self.retiring && effect_id == id {
                self.inputs
                    .push_back((now, Input::Chunk { effect_id, chunk }));
            }
        }
        let reply = match result {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Reply {
                id,
                outcome: Outcome::Failed(
                    "native effect owner lost; resources remain reserved".into(),
                ),
                candidate: None,
            },
        };
        self.active = None;
        if !self.retiring {
            if let Some(candidate) = reply.candidate {
                self.candidate = Some(candidate);
            }
            self.inputs.push_back((
                now,
                Input::Complete {
                    effect_id: reply.id,
                    outcome: reply.outcome,
                },
            ));
        }
        self.retiring = false;
    }
    fn active(&self) -> Option<u64> {
        self.active.as_ref().map(|v| v.0).or_else(|| {
            self.inputs.iter().find_map(|(_, i)| {
                if let Input::Complete { effect_id, .. } = i {
                    Some(*effect_id)
                } else {
                    None
                }
            })
        })
    }
    fn next_input_ms(&self) -> Option<u64> {
        self.inputs.front().map(|v| v.0)
    }
    fn pop_input(&mut self) -> Option<(u64, Input)> {
        self.inputs.pop_front()
    }
    fn cancel(&mut self) {
        self.inputs.clear();
        if let Some((_, cancel, _)) = &self.active {
            self.retiring = true;
            cancel.cancel();
        }
    }
    fn signal_cancel(&mut self) {
        if let Some((_, cancel, true)) = &self.active {
            cancel.cancel();
        }
    }
    fn retiring(&self) -> bool {
        self.active.is_some()
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, confirmed: &CommittedFrontier) -> Result<()> {
        if self.active().is_some() || now >= effect.deadline_ms {
            return Err(Error::Uncertain("native dispatch owner unavailable".into()));
        }
        let cancel = CancellationToken::new();
        self.sender
            .as_ref()
            .ok_or_else(|| Error::Invalid("native owner not begun".into()))?
            .try_send(Task {
                effect: effect.clone(),
                confirmed: confirmed.clone(),
                cancel: cancel.clone(),
            })
            .map_err(|_| Error::Uncertain("native owner unavailable; no replay".into()))?;
        // User cancellation never aborts finalization. The controller keeps a
        // pending Snapshot through its stop to publish an interrupted
        // checkpoint, and each finalization effect stays bounded by its deadline.
        self.active = Some((
            effect.id,
            cancel,
            !matches!(
                effect.kind,
                EffectKind::Snapshot { .. }
                    | EffectKind::Cleanup
                    | EffectKind::PublishCheckpoint(_)
            ),
        ));
        Ok(())
    }
    fn checkpoint<'a>(&'a self, _: &playtime_engine::Checkpoint) -> Result<CheckpointBytes<'a>> {
        let workspace = self
            .candidate
            .as_ref()
            .ok_or_else(|| Error::Invalid("no validated native capture".into()))?
            .workspace();
        let files: BTreeMap<_, _> = workspace
            .entries()
            .iter()
            .filter_map(|e| {
                e.content_slot.map(|slot| {
                    (
                        e.path.as_str(),
                        workspace.content(slot).expect("validated slot"),
                    )
                })
            })
            .collect();
        Ok(CheckpointBytes::Contents {
            workspace: workspace.tar(),
            files,
        })
    }
}
impl Drop for NativeEffects {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
#[path = "docker_tests.rs"]
mod tests;
