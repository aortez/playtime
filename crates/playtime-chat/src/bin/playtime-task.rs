//! Noninteractive worker, read-only and contract invocations; inference ownership
//! stays in run-strata.py.
use clap::{Parser, ValueEnum};
use playtime_chat::{
    Result, contract,
    native::{Journal, Model, NativeEffects},
    native_project,
    readonly::{self, Role},
    runtime, worker,
};
use playtime_docker::{
    self as docker,
    project::{ProjectDirectory, ProjectLimits, ProjectWorkspace},
    transport::Cli,
};
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
use playtime_strata::{Client, Endpoint, ModelLock};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum ToolBackend {
    Docker,
    Native,
}

#[derive(Parser)]
struct Args {
    #[arg(long, value_enum, default_value = "worker")]
    role: Role,
    #[arg(long)]
    output_contract: Option<PathBuf>,
    #[arg(long)]
    output_contract_sha256: Option<String>,
    #[arg(long)]
    request_sha256: Option<String>,
    #[arg(long)]
    evidence_property: Option<String>,
    #[arg(long, default_value_t = 0)]
    repair_turns: u32,
    #[arg(long)]
    caller_deadline_unix_ms: Option<u64>,
    #[arg(long, value_enum, default_value = "docker")]
    tool_backend: ToolBackend,
    #[arg(long)]
    native_policy: Option<PathBuf>,
    #[arg(long)]
    describe_protocol: bool,
    #[arg(long)]
    check_inputs: bool,
    #[arg(long, default_value_t = 1)]
    protocol_version: u32,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    model_lock: Option<PathBuf>,
    #[arg(long)]
    image_lock: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    run_dir: Option<PathBuf>,
    #[arg(long)]
    request: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    worker_session: Option<String>,
    #[arg(long, default_value_t = 900)]
    wall_seconds: u64,
}

struct Events {
    file: File,
    bytes: usize,
}
async fn emit(log: &Arc<std::sync::Mutex<Events>>, value: Value) -> Result<()> {
    let log = log.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        let mut log = log.lock().map_err(|_| "worker event writer failed")?;
        if log.bytes + bytes.len() > 16 * 1024 * 1024 {
            return Err("worker event log exceeds 16 MiB".into());
        }
        log.file.write_all(&bytes)?;
        log.bytes += bytes.len();
        // The session journal supplies dispatch durability. This append retains
        // streamed diagnostics even if the caller is interrupted before return.
        io::stdout().write_all(&bytes)?;
        io::stdout().flush()?;
        Ok(())
    })
    .await?
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Worker,
    Readonly,
    Contract,
}

/// What a caller reads to tell whether an attempt reached the model: the admitted
/// inputs, the invocation record and the event log with its started event. Only
/// `record_invocation` makes one and `connect` needs one, so all of them are on
/// disk before the first model contact.
struct Recorded {
    log: Arc<std::sync::Mutex<Events>>,
}

async fn record_invocation(
    records: &PrivateDirectory,
    run_dir: &Path,
    kind: Kind,
    inputs: Option<(&[u8], &[u8])>,
    invoked: &Value,
    started: Value,
) -> Result<Recorded> {
    if let Some((contract, request)) = inputs {
        records.write_new("output-contract.json", contract)?;
        records.write_new("request.txt", request)?;
    }
    let (invoked_name, events_name) = if kind == Kind::Worker {
        ("worker-invoked.json", "worker-events.jsonl")
    } else {
        ("invocation.json", "invocation-events.jsonl")
    };
    records.write_new(invoked_name, &serde_json::to_vec(invoked)?)?;
    let log = Arc::new(std::sync::Mutex::new(Events {
        file: OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(run_dir.join(events_name))?,
        bytes: 0,
    }));
    emit(&log, started).await?;
    Ok(Recorded { log })
}

/// Build the inference client and verify its endpoint, the first model contact.
async fn connect(
    _recorded: &Recorded,
    endpoint: Option<&str>,
    lock: ModelLock,
    cancel: &CancellationToken,
) -> Result<Client> {
    let mut client = Client::new(
        Endpoint::parse(endpoint.ok_or("missing endpoint")?)?,
        lock,
        Default::default(),
    )?;
    client
        .verify(Instant::now() + Duration::from_secs(20), cancel)
        .await?;
    Ok(client)
}

/// What another Docker turn needs: each turn owns a new container.
struct DockerTools {
    engine: docker::Engine,
    ready: docker::ReadyImage,
    directory: Arc<ProjectDirectory>,
    limits: ProjectLimits,
    state: PathBuf,
    wall: u64,
    grant: &'static str,
}
impl DockerTools {
    async fn effects(&self, model: Arc<Mutex<Model>>) -> Result<worker::ProjectEffects> {
        let owner = docker::Ownership {
            service: "playtime-task".into(),
            run: format!("turn-{}", docker::Ownership::random_nonce()?),
            grant: self.grant.into(),
            invocation: docker::Ownership::random_nonce()?,
            nonce: docker::Ownership::random_nonce()?,
            daemon: self.engine.identity().clone(),
        };
        let (ready, directory, limits, state, wall) = (
            self.ready.clone(),
            self.directory.clone(),
            self.limits.clone(),
            self.state.clone(),
            self.wall,
        );
        let workspace = tokio::task::spawn_blocking(move || {
            ProjectWorkspace::new(
                ready,
                owner,
                directory,
                limits,
                docker::WorkDeadline::after(Duration::from_secs(wall))?,
                Arc::new(Journal(state)),
            )
        })
        .await??;
        Ok(worker::ProjectEffects::Docker(Box::new(NativeEffects {
            model,
            workspace,
            output_bytes: 16384,
            return_overhead: Duration::from_secs(1),
        })))
    }
}

type TaskEffects = Arc<Mutex<worker::EvidenceEffects<worker::ProjectEffects>>>;

/// Run one turn and forward its events. A failed event write cancels the turn
/// and is reported only after the driver's owned cleanup.
async fn turn(
    session: Session,
    prompt: String,
    effects: TaskEffects,
    log: &Arc<std::sync::Mutex<Events>>,
    cancel: &CancellationToken,
) -> Result<runtime::Completed> {
    let (tx, mut rx) = runtime::events();
    let mut task = tokio::spawn(runtime::run_turn(
        session,
        prompt,
        effects,
        tx,
        cancel.clone(),
    ));
    let mut output_error = None;
    let completed = loop {
        tokio::select! {
            event=rx.recv()=>if let Some(event)=event {
                if output_error.is_none() && let Err(error)=emit(log,json!({"schema_version":1,"type":"runtime","event":event})).await {cancel.cancel();output_error=Some(error);}
            } else {break task.await??;},
            result=&mut task=>{
                while let Some(event)=rx.recv().await {
                    if output_error.is_none() && let Err(error)=emit(log,json!({"schema_version":1,"type":"runtime","event":event})).await {cancel.cancel();output_error=Some(error);}
                }
                break result??;
            }
        }
    };
    match output_error {
        Some(error) => Err(error),
        None => Ok(completed),
    }
}

fn final_response(session: &Session) -> Result<(Value, String)> {
    let checkpoint = session.checkpoint().ok_or("missing completed checkpoint")?;
    let response = checkpoint
        .history
        .exchanges
        .last()
        .and_then(|exchange| exchange.messages.last())
        .filter(|message| message["role"] == "assistant")
        .and_then(|message| message["content"].as_str())
        .ok_or("missing final worker response")?;
    Ok((json!(checkpoint.completed_turn), response.to_owned()))
}

/// Receipts that report repair turns say whether one ran and keep what it rejected.
fn repairs(reported: bool, rejected: &[Value], mut value: Value) -> Value {
    if reported {
        value["repair_turns_used"] = rejected.len().into();
        if !rejected.is_empty() {
            value["rejected"] = rejected.to_vec().into();
        }
    }
    value
}

async fn run(args: Args, cancel: CancellationToken) -> Result<()> {
    if args.protocol_version != worker::PROTOCOL {
        return Err("unsupported worker protocol".into());
    }
    let read_only = args.role.is_readonly();
    if args.describe_protocol {
        let mut description = if read_only {
            readonly::capability()
        } else {
            json!({"protocol":"playtime-worker","version":worker::PROTOCOL,"roles":["worker"],"report":"worker-report-v1","continuation":"completed-session-only", "readonly_invocation":readonly::capability(), "contract_invocation":contract::capability()})
        };
        description["tools"] = if read_only || args.tool_backend == ToolBackend::Native {
            native_project::CAPABILITY
        } else {
            "project-docker-v1"
        }
        .into();
        description["tool_backends"] = if read_only {
            json!({"native":native_project::CAPABILITY})
        } else {
            json!({"docker":"project-docker-v1","native":native_project::CAPABILITY})
        };
        description["native_policy_schema"] = 1.into();
        description["native_helper_sha256"] = native_project::helper_sha256().into();
        if let Some(path) = &args.model_lock {
            let lock: ModelLock = serde_json::from_slice(&std::fs::read(path)?)?;
            lock.validate()?;
            let transport = match lock.identity.service {
                playtime_strata::Service::Strata => None,
                playtime_strata::Service::Basalt => Some("basalt-responses-v1"),
                playtime_strata::Service::LlamaServer => Some("llama-chat-completions-v1"),
            };
            if let Some(transport) = transport {
                description["transport"] = transport.into();
                if !read_only {
                    description["readonly_invocation"]["transport"] = transport.into();
                    description["contract_invocation"]["transport"] = transport.into();
                }
            }
        }
        println!("{description}");
        return Ok(());
    }
    let kind = if read_only {
        Kind::Readonly
    } else if args.output_contract.is_some() {
        Kind::Contract
    } else {
        Kind::Worker
    };
    match kind {
        Kind::Readonly => {
            if args.tool_backend != ToolBackend::Native
                || args.worker_session.is_some()
                || args.output_contract.is_none()
                || args.output_contract_sha256.is_none()
                || args.request_sha256.is_none()
            {
                return Err("read-only roles need native tools, output contract/hash and request hash; fresh invocations only".into());
            }
        }
        Kind::Contract => {
            if args.output_contract_sha256.is_none() || args.request_sha256.is_none() {
                return Err(
                    "worker contract invocations need the output contract hash and request hash"
                        .into(),
                );
            }
        }
        Kind::Worker => {
            if args.output_contract_sha256.is_some() || args.request_sha256.is_some() {
                return Err("structured output inputs require an output contract or an explicit read-only role".into());
            }
        }
    }
    if kind != Kind::Contract && args.evidence_property.is_some() {
        return Err("evidence resolution belongs to worker contract invocations".into());
    }
    if kind == Kind::Worker && (args.repair_turns != 0 || args.caller_deadline_unix_ms.is_some()) {
        return Err(
            "repair turns and caller deadlines belong to contract and read-only invocations".into(),
        );
    }
    if args.repair_turns > contract::MAX_REPAIR_TURNS {
        return Err("at most one repair turn is supported".into());
    }
    let caller_deadline = args
        .caller_deadline_unix_ms
        .map(contract::CallerDeadline::new)
        .transpose()?;
    let workspace = args.workspace.ok_or("missing workspace")?.canonicalize()?;
    let run_dir = args
        .run_dir
        .ok_or("missing run directory")?
        .canonicalize()?;
    let request = args.request.ok_or("missing request")?.canonicalize()?;
    let model_path = args.model_lock.ok_or("missing model lock")?;
    if (args.tool_backend == ToolBackend::Native
        && (args.image_lock.is_some() || args.native_policy.is_none()))
        || (args.tool_backend == ToolBackend::Docker
            && (args.native_policy.is_some() || args.image_lock.is_none()))
    {
        return Err("select Docker with --image-lock or native with --native-policy; no mixed backend configuration".into());
    }
    let image_path = args.image_lock;
    let state = args.state_dir.unwrap_or(
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME required")?)
            .join(".local/state/playtime-chat"),
    );
    PrivateDirectory::open(&state, true)?;
    let records = PrivateDirectory::open(&run_dir, false)
        .map_err(|error| format!("worker run directory {}: {error}", run_dir.display()))?;
    let _run_lease = records.try_lock()?;
    let artifacts = run_dir.join("artifacts");
    let artifact_directory = PrivateDirectory::open(&artifacts, true)?;
    if !artifact_directory.names()?.is_empty() {
        return Err("worker artifacts must start empty".into());
    }
    let prompt = String::from_utf8(playtime_local::read_private(
        &request,
        playtime_engine::chat::MAX_USER_BYTES as u64,
    )?)?;
    if prompt.trim().is_empty() {
        return Err("empty worker assignment".into());
    }
    if let Some(expected) = &args.request_sha256 {
        readonly::check_digest(prompt.as_bytes(), expected)?;
    }
    let contract_path = args.output_contract.map(|p| p.canonicalize()).transpose()?;
    let contract = contract_path
        .as_ref()
        .map(|path| {
            readonly::Contract::parse(
                playtime_local::read_private(path, readonly::MAX_CONTRACT as u64)?,
                args.output_contract_sha256
                    .as_deref()
                    .ok_or("missing contract hash")?,
            )
        })
        .transpose()?;
    // The caller's contract describes the resolved result; read-only roles keep
    // their plain contract and version-one workers have none.
    let (contract, spec) = match (kind, contract) {
        (Kind::Contract, Some(contract)) => (
            None,
            Some(contract::Spec::new(
                contract,
                args.evidence_property.clone(),
            )?),
        ),
        (_, contract) => (contract, None),
    };
    let resume = args.worker_session.is_some();
    let session_id = args.worker_session.unwrap_or(format!(
        "{}-{}",
        match kind {
            Kind::Readonly => "ptr",
            Kind::Contract => "ptc",
            Kind::Worker => "ptw",
        },
        Token::generate()?.expose()
    ));
    match kind {
        Kind::Worker => worker::session_id(&session_id)?,
        Kind::Contract => contract::session_id(&session_id)?,
        Kind::Readonly => (),
    }
    let session_path = state.join(&session_id);
    if resume && !session_path.join("config.json").exists() {
        return Err("saved worker session unavailable; no automatic fresh fallback".into());
    }
    let mut protected = vec![run_dir.clone(), request, model_path.clone()];
    protected.extend(contract_path);
    let lock: ModelLock = serde_json::from_slice(&std::fs::read(&model_path)?)?;
    let limits = ProjectLimits {
        nano_cpus: 2_000_000_000,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        tmp_bytes: 256 * 1024 * 1024,
        tmp_inodes: 32768,
        pids: 256,
        seed: 1042,
    };
    let mut native_workspace = None;
    let mut docker_environment = None;
    let config = if args.tool_backend == ToolBackend::Native {
        let policy = args.native_policy.ok_or("missing native policy")?;
        let root = workspace.clone();
        let private = state.clone();
        let native = tokio::task::spawn_blocking(move || {
            native_project::Workspace::open(&policy, &root, &private, &protected)
        })
        .await??;
        let config = if let Some(contract) = &contract {
            readonly::config(&lock, &native, args.wall_seconds, args.role, contract)?
        } else {
            let config = worker::native_config(&lock, &native, args.wall_seconds)?;
            match &spec {
                Some(spec) => contract::configure(config, Some(native.policy()), spec)?,
                None => config,
            }
        };
        native_workspace = Some(native);
        config
    } else {
        let image_path = image_path.ok_or("missing image lock")?;
        let mut protected = protected;
        protected.push(image_path.clone());
        let root = workspace.clone();
        let private = state.clone();
        let directory = Arc::new(
            tokio::task::spawn_blocking(move || {
                ProjectDirectory::open(&root, &private, &protected)
            })
            .await??,
        );
        let image_bytes = std::fs::read(&image_path)?;
        let image: Value = serde_json::from_slice(&image_bytes)?;
        let config = worker::config(&lock, &image, &directory, &limits, args.wall_seconds)?;
        docker_environment = Some((directory, image_bytes));
        match &spec {
            Some(spec) => contract::configure(config, None, spec)?,
            None => config,
        }
    };
    // A saved session resumes under the binding it was saved with, and only with
    // the limits this invocation sets.
    let config = if resume {
        playtime_chat::sessions::resume_config(
            &config,
            lock.earlier_fingerprint()?.as_ref(),
            &playtime_chat::sessions::Overrides::all(&config),
            &session_path,
        )
    } else {
        config
    };
    // A new session's first turn would refuse this context only after the wrapper
    // has acquired the model. The same rule here refuses it before anything starts,
    // and says by how much. A resumed session keeps the context it began with.
    if !resume {
        let (size, limit) = config.initial_context(&prompt)?;
        if size >= limit {
            return Err(format!(
                "initial context is {size} encoded bytes of instructions, request and tools, and must be under {limit}: until the model has replied once, each byte counts as a token, and one reply must still fit below the compaction trigger and inside the context's reserves"
            )
            .into());
        }
    }
    let execution = native_workspace
        .as_ref()
        .map(native_project::Workspace::binding)
        .unwrap_or(json!({"capability":"project-docker-v1"}));
    let invocation = match (&contract, &spec) {
        (Some(contract), _) => Some(readonly::binding(
            args.role,
            contract,
            prompt.as_bytes(),
            args.repair_turns,
            args.caller_deadline_unix_ms,
            execution.clone(),
        )),
        (None, Some(spec)) => Some(contract::binding(
            spec,
            prompt.as_bytes(),
            args.repair_turns,
            args.caller_deadline_unix_ms,
            execution.clone(),
        )),
        (None, None) => None,
    };
    let prefix = if kind == Kind::Worker {
        "worker"
    } else {
        "invocation"
    };
    let bound = |mut value: Value| {
        if let Some(binding) = &invocation {
            value["invocation"] = binding.clone();
        }
        value
    };
    if args.check_inputs {
        if resume {
            Session::try_open(&session_path, &config)?;
        }
        println!(
            "{}",
            bound(
                json!({"protocol":match kind {Kind::Readonly => "playtime-readonly", Kind::Contract => contract::PROTOCOL, Kind::Worker => "playtime-worker"},"version":worker::PROTOCOL,"inputs_valid":true,"execution":execution})
            )
        );
        return Ok(());
    }
    let inputs = contract
        .as_ref()
        .or(spec.as_ref().map(contract::Spec::contract))
        .map(|contract| (contract.bytes(), prompt.as_bytes()));
    let recorded = record_invocation(
        &records,
        &run_dir,
        kind,
        inputs,
        &bound(
            json!({"schema_version":1,"session_id":session_id,"resume":resume,"configuration":config.fingerprint()?,"execution":execution}),
        ),
        bound(
            json!({"schema_version":1,"type":format!("{prefix}_started"),"session_id":session_id,"execution":execution}),
        ),
    )
    .await?;
    let log = recorded.log.clone();
    // Reach Docker before the model or a new session, so an outage leaves neither behind.
    let docker = match docker_environment {
        Some((directory, image_bytes)) => {
            let transport = Arc::new(Cli::resolve(
                std::path::Path::new("/usr/bin/docker"),
                std::path::Path::new("/var/run/docker.sock"),
                &state.join(format!("docker-{}", Token::generate()?.expose())),
            )?);
            let engine = docker::Engine::connect(
                transport,
                Instant::now() + Duration::from_secs(20),
                cancel.clone(),
            )
            .await?;
            let ready = engine
                .readiness(
                    &image_bytes,
                    Instant::now() + Duration::from_secs(20),
                    cancel.clone(),
                )
                .await?;
            Some((engine, ready, directory, image_bytes))
        }
        None => None,
    };
    let client = connect(&recorded, args.endpoint.as_deref(), lock, &cancel).await?;
    let saved_path = session_path.clone();
    let session = tokio::task::spawn_blocking(move || {
        if resume {
            Session::try_open(&saved_path, &config)
        } else {
            Session::create(&saved_path, config)
        }
    })
    .await??;
    if !resume {
        if let Some((_, _, _, image_bytes)) = &docker {
            playtime_chat::environment::save(&session_path, image_bytes)?;
        } else {
            PrivateDirectory::open(&session_path, false)?
                .write_new("native-execution.json", &serde_json::to_vec(&execution)?)?;
        }
    }
    let model = Arc::new(Mutex::new(Model {
        client,
        fenced: false,
    }));
    let mut docker_tools = None;
    let backend = if let Some(workspace) = native_workspace {
        worker::ProjectEffects::Native(Box::new(native_project::ProjectEffects {
            inner: playtime_chat::native::ModelEffects(model.clone()),
            workspace,
        }))
    } else {
        let (engine, ready, directory, _) = docker.ok_or("missing Docker environment")?;
        let tools = DockerTools {
            engine,
            ready,
            directory,
            limits,
            state,
            wall: args.wall_seconds,
            grant: if kind == Kind::Contract {
                "worker-contract"
            } else {
                "devbot-worker"
            },
        };
        let effects = tools.effects(model.clone()).await?;
        docker_tools = Some(tools);
        effects
    };
    let effects = worker::EvidenceEffects::new(backend, artifacts, 64 * 1024 * 1024)?;
    let effects = Arc::new(Mutex::new(if read_only {
        effects.with_all_tools()
    } else {
        effects
    }));
    let mut completed = turn(session, prompt, effects.clone(), &log, &cancel).await?;
    let mut rejected: Vec<Value> = Vec::new();
    // Contract receipts always report repair turns. Read-only receipts report them
    // only when the caller offered one, so other invocations keep their bytes.
    let reported = kind == Kind::Contract || (kind == Kind::Readonly && args.repair_turns > 0);
    // The completed session keeps its lock until the terminal receipt is written.
    let (result, _session) = loop {
        let cleanup = completed.result.cleanup;
        if completed.result.reason != playtime_core::StopReason::AgentFinished
            || cleanup != playtime_protocol::events::CleanupOutcome::Complete
        {
            let failure = repairs(
                reported,
                &rejected,
                bound(
                    json!({"schema_version":1,"type":format!("{prefix}_failed"),"session_id":session_id,"reason":completed.result.reason,"cleanup":cleanup,"usage":completed.result.usage}),
                ),
            );
            if kind != Kind::Worker {
                records.write_new("invocation-result.json", &serde_json::to_vec(&failure)?)?;
            }
            emit(&log, failure).await?;
            return Err("worker turn did not finish; no automatic replay".into());
        }
        let session = completed
            .session
            .take()
            .ok_or("completed session unavailable")?;
        let (turn_id, response) = final_response(&session)?;
        let raw = match rejected.len() {
            0 => "response.raw.txt".to_owned(),
            attempt => format!("response.repair-{attempt}.raw.txt"),
        };
        let saved = match kind {
            Kind::Worker => {
                let (report, report_sha256) = worker::save_report(
                    &response,
                    &records,
                    &artifact_directory,
                    effects.lock().await.evidence(),
                )?;
                break (
                    json!({"schema_version":1,"type":"worker_finished","session_id":session_id,"turn_id":turn_id,"cleanup":cleanup,"status":report.status,"usage":completed.result.usage,"report_sha256":report_sha256}),
                    session,
                );
            }
            Kind::Readonly => readonly::save_response(
                &response,
                &records,
                &raw,
                contract.as_ref().ok_or("missing output contract")?,
            )?,
            Kind::Contract => contract::save_response(
                &response,
                &records,
                &raw,
                spec.as_ref().ok_or("missing output contract")?,
                &artifact_directory,
                effects.lock().await.evidence(),
            )?,
        };
        let rejection = match saved {
            Ok(response_sha256) => {
                break (
                    repairs(
                        reported,
                        &rejected,
                        bound(
                            json!({"schema_version":1,"type":"invocation_finished","session_id":session_id,"turn_id":turn_id,"cleanup":cleanup,"usage":completed.result.usage,"response_sha256":response_sha256}),
                        ),
                    ),
                    session,
                );
            }
            Err(rejection) => rejection,
        };
        // The rejected turn is complete, so stopping here leaves a contract session
        // resumable. A cancelled repair turn would fence it instead.
        let mut repair = (rejected.len() as u32) < args.repair_turns && !cancel.is_cancelled();
        // The caller stops the worker at its deadline, so a repair turn that
        // couldn't use its whole wall budget and clean up first never starts.
        let late = repair
            && caller_deadline
                .as_ref()
                .is_some_and(|deadline| !deadline.allows(Duration::from_secs(args.wall_seconds)));
        if late {
            eprintln!(
                "playtime-task: repair turn skipped: it could not finish and clean up before the caller deadline"
            );
            repair = false;
        }
        if repair && let Some(tools) = &docker_tools {
            match tools.effects(model.clone()).await {
                Ok(next) => effects.lock().await.inner = next,
                Err(error) => {
                    eprintln!("playtime-task: repair turn unavailable: {error}");
                    repair = false;
                }
            }
        }
        if !repair {
            let mut failure = json!({"schema_version":1,"type":"invocation_failed","session_id":session_id,"turn_id":turn_id,"reason":rejection.reason,"cleanup":cleanup,"usage":completed.result.usage});
            // A read-only invocation offered no repair turn keeps its original receipt.
            if reported {
                failure["detail"] = rejection.detail.as_str().into();
            }
            let mut failure = repairs(reported, &rejected, bound(failure));
            if late {
                failure["repair_skipped"] = "caller_deadline".into();
            }
            records.write_new("invocation-result.json", &serde_json::to_vec(&failure)?)?;
            emit(&log, failure).await?;
            return Err(rejection.into());
        }
        rejected.push(json!({"turn_id":turn_id,"reason":rejection.reason,"detail":rejection.detail,"cleanup":cleanup,"usage":completed.result.usage}));
        let correction = if kind == Kind::Readonly {
            readonly::repair_request(&rejection)
        } else {
            contract::repair_request(&rejection)
        };
        completed = turn(session, correction, effects.clone(), &log, &cancel).await?;
    };
    records.write_new(
        &format!("{prefix}-result.json"),
        &serde_json::to_vec(&result)?,
    )?;
    emit(&log, result).await?;
    log.lock()
        .map_err(|_| "worker event writer failed")?
        .file
        .sync_all()?;
    Ok(())
}

#[tokio::main]
async fn main() {
    let cancel = CancellationToken::new();
    let signals = cancel.clone();
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
    let mut hangup =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).expect("SIGHUP");
    tokio::spawn(async move {
        tokio::select! {_=tokio::signal::ctrl_c()=>(),_=terminate.recv()=>(),_=hangup.recv()=>()};
        signals.cancel();
    });
    if let Err(error) = run(Args::parse(), cancel.clone()).await {
        eprintln!("playtime-task: {error}");
        std::process::exit(if cancel.is_cancelled() { 130 } else { 1 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invocation_records_exist_before_a_client_can_be_built() {
        let root = std::env::temp_dir().join(format!(
            "playtime-task-records-{}",
            Token::generate().unwrap().expose()
        ));
        let records = PrivateDirectory::open(&root, true).unwrap();
        let recorded = record_invocation(
            &records,
            &root,
            Kind::Readonly,
            Some(("{}".as_bytes(), "Assignment".as_bytes())),
            &json!({"schema_version":1}),
            json!({"type":"invocation_started"}),
        )
        .await
        .unwrap();
        // A caller may treat the absence of all four as proof that an attempt
        // never reached the model, since `connect` needs what this returned.
        for name in [
            "output-contract.json",
            "request.txt",
            "invocation.json",
            "invocation-events.jsonl",
        ] {
            assert!(root.join(name).is_file(), "{name}");
        }
        assert_eq!(records.read("request.txt", 64).unwrap(), b"Assignment");
        assert_eq!(
            std::fs::read_to_string(root.join("invocation-events.jsonl")).unwrap(),
            "{\"type\":\"invocation_started\"}\n"
        );
        drop(recorded);
        std::fs::remove_dir_all(root).unwrap();
    }
}
