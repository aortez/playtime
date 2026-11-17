//! Version-one noninteractive worker; inference ownership stays in run-strata.py.
use clap::{Parser, ValueEnum};
use playtime_chat::{
    Result,
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
    path::PathBuf,
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

async fn run(args: Args, cancel: CancellationToken) -> Result<()> {
    if args.protocol_version != worker::PROTOCOL {
        return Err("unsupported worker protocol".into());
    }
    let read_only = args.role.is_readonly();
    if args.describe_protocol {
        let mut description = if read_only {
            readonly::capability()
        } else {
            json!({"protocol":"playtime-worker","version":worker::PROTOCOL,"roles":["worker"],"report":"worker-report-v1","continuation":"completed-session-only", "readonly_invocation":readonly::capability()})
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
            if lock.identity.service == playtime_strata::Service::Basalt {
                description["transport"] = "basalt-responses-v1".into();
                if !read_only {
                    description["readonly_invocation"]["transport"] = "basalt-responses-v1".into();
                }
            }
        }
        println!("{description}");
        return Ok(());
    }
    if read_only {
        if args.tool_backend != ToolBackend::Native
            || args.worker_session.is_some()
            || args.output_contract.is_none()
            || args.output_contract_sha256.is_none()
            || args.request_sha256.is_none()
        {
            return Err("read-only roles need native tools, output contract/hash and request hash; fresh invocations only".into());
        }
    } else if args.output_contract.is_some()
        || args.output_contract_sha256.is_some()
        || args.request_sha256.is_some()
    {
        return Err("structured output inputs require an explicit read-only role".into());
    }
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
    let resume = args.worker_session.is_some();
    let session_id = args.worker_session.unwrap_or(format!(
        "{}-{}",
        if read_only { "ptr" } else { "ptw" },
        Token::generate()?.expose()
    ));
    if !read_only {
        worker::session_id(&session_id)?;
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
            worker::native_config(&lock, &native, args.wall_seconds)?
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
        config
    };
    let execution = native_workspace
        .as_ref()
        .map(native_project::Workspace::binding)
        .unwrap_or(json!({"capability":"project-docker-v1"}));
    let invocation = contract.as_ref().map(|contract| {
        readonly::binding(args.role, contract, prompt.as_bytes(), execution.clone())
    });
    let prefix = if read_only { "invocation" } else { "worker" };
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
                json!({"protocol":if read_only {"playtime-readonly"} else {"playtime-worker"},"version":worker::PROTOCOL,"inputs_valid":true,"execution":execution})
            )
        );
        return Ok(());
    }
    if let Some(contract) = &contract {
        records.write_new("output-contract.json", contract.bytes())?;
        records.write_new("request.txt", prompt.as_bytes())?;
    }
    records.write_new(if read_only {"invocation.json"} else {"worker-invoked.json"}, &serde_json::to_vec(&bound(json!({"schema_version":1,"session_id":session_id,"resume":resume,"configuration":config.fingerprint()?,"execution":execution})))?)?;
    let log = Arc::new(std::sync::Mutex::new(Events {
        file: OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(run_dir.join(format!("{prefix}-events.jsonl")))?,
        bytes: 0,
    }));
    emit(
        &log,
        bound(json!({"schema_version":1,"type":format!("{prefix}_started"),"session_id":session_id,"execution":execution})),
    )
    .await?;
    let mut client = Client::new(
        Endpoint::parse(args.endpoint.as_deref().ok_or("missing endpoint")?)?,
        lock,
        Default::default(),
    )?;
    client
        .verify(Instant::now() + Duration::from_secs(20), &cancel)
        .await?;
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
        if let Some((_, image_bytes)) = &docker_environment {
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
    let backend = if let Some(workspace) = native_workspace {
        worker::ProjectEffects::Native(Box::new(native_project::ProjectEffects {
            inner: playtime_chat::native::ModelEffects(model),
            workspace,
        }))
    } else {
        let (directory, image_bytes) = docker_environment.ok_or("missing Docker environment")?;
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
        let owner = docker::Ownership {
            service: "playtime-task".into(),
            run: format!("turn-{}", docker::Ownership::random_nonce()?),
            grant: "devbot-worker".into(),
            invocation: docker::Ownership::random_nonce()?,
            nonce: docker::Ownership::random_nonce()?,
            daemon: engine.identity().clone(),
        };
        let wall = args.wall_seconds;
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
        worker::ProjectEffects::Docker(Box::new(NativeEffects {
            model,
            workspace,
            output_bytes: 16384,
            return_overhead: Duration::from_secs(1),
        }))
    };
    let effects = worker::EvidenceEffects::new(backend, artifacts, 64 * 1024 * 1024)?;
    let effects = Arc::new(Mutex::new(if read_only {
        effects.with_all_tools()
    } else {
        effects
    }));
    let (tx, mut rx) = runtime::events();
    let mut task = tokio::spawn(runtime::run_turn(
        session,
        prompt,
        effects.clone(),
        tx,
        cancel.clone(),
    ));
    let mut output_error = None;
    let completed = loop {
        tokio::select! {
            event=rx.recv()=>if let Some(event)=event {
                if output_error.is_none() && let Err(error)=emit(&log,json!({"schema_version":1,"type":"runtime","event":event})).await {cancel.cancel();output_error=Some(error);}
            } else {break task.await??;},
            result=&mut task=>{
                while let Some(event)=rx.recv().await {
                    if output_error.is_none() && let Err(error)=emit(&log,json!({"schema_version":1,"type":"runtime","event":event})).await {cancel.cancel();output_error=Some(error);}
                }
                break result??;
            }
        }
    };
    if let Some(error) = output_error {
        return Err(error);
    }
    let cleanup = completed.result.cleanup;
    if completed.result.reason != playtime_core::StopReason::AgentFinished
        || cleanup != playtime_protocol::events::CleanupOutcome::Complete
    {
        let failure = bound(
            json!({"schema_version":1,"type":format!("{prefix}_failed"),"session_id":session_id,"reason":completed.result.reason,"cleanup":cleanup,"usage":completed.result.usage}),
        );
        if read_only {
            records.write_new("invocation-result.json", &serde_json::to_vec(&failure)?)?;
        }
        emit(&log, failure).await?;
        return Err("worker turn did not finish; no automatic replay".into());
    }
    let session = completed.session.ok_or("completed session unavailable")?;
    let checkpoint = session.checkpoint().ok_or("missing completed checkpoint")?;
    let response = checkpoint
        .history
        .exchanges
        .last()
        .and_then(|exchange| exchange.messages.last())
        .filter(|message| message["role"] == "assistant")
        .and_then(|message| message["content"].as_str())
        .ok_or("missing final worker response")?;
    let result = if let Some(contract) = &contract {
        let response_sha256 = match readonly::save_response(response, &records, contract) {
            Ok(digest) => digest,
            Err(error) => {
                let failure = bound(
                    json!({"schema_version":1,"type":"invocation_failed","session_id":session_id,"turn_id":checkpoint.completed_turn,"reason":"invalid_output","cleanup":cleanup,"usage":completed.result.usage}),
                );
                records.write_new("invocation-result.json", &serde_json::to_vec(&failure)?)?;
                emit(&log, failure).await?;
                return Err(error);
            }
        };
        bound(
            json!({"schema_version":1,"type":"invocation_finished","session_id":session_id,"turn_id":checkpoint.completed_turn,"cleanup":cleanup,"usage":completed.result.usage,"response_sha256":response_sha256}),
        )
    } else {
        let (report, report_sha256) = worker::save_report(
            response,
            &records,
            &artifact_directory,
            effects.lock().await.evidence(),
        )?;
        json!({"schema_version":1,"type":"worker_finished","session_id":session_id,"turn_id":checkpoint.completed_turn,"cleanup":cleanup,"status":report.status,"usage":completed.result.usage,"report_sha256":report_sha256})
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
