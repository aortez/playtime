use clap::{Parser, ValueEnum};
mod oneshot;
use playtime_chat::{
    Result,
    native::{Journal, Model, ModelEffects, NativeEffects},
    output::{Output, clean},
    prompt::AgentPrompt,
    runtime::{self, Effects},
    sessions::Overrides,
    terminal::{Console, Key},
};
use playtime_core::{CommonLimits, RunMode, Sha256Digest, WorkshopLimits};
use playtime_docker::{
    self as docker,
    project::{ProjectDirectory, ProjectLimits, ProjectWorkspace},
    transport::Cli,
};
use playtime_engine::{
    ToolPolicy,
    chat::ConversationConfig,
    compaction::CompactionSettings,
    execution::{ExecutionConfig, ToolProfile},
};
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
use playtime_strata::{Client, Endpoint, ModelLock};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::{self, IsTerminal},
    num::NonZeroU64,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// Writes one diagnostic line and ignores a failed terminal. `eprintln!` panics on
/// EIO, and a hangup during a turn must not skip its records or owned cleanup.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

// A new session's limits. A reopened session keeps the limits it was saved with,
// apart from those given explicitly.
const WALL_SECONDS: u64 = 1800;
const MODEL_SECONDS: u64 = 600;
const TOKENS_PER_CALL: u64 = 16384;
const GENERATED_TOKENS: u64 = 65536;
const SUMMARY_TOKENS: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
enum Preset {
    #[default]
    Workshop,
    CodingHost,
}

#[derive(Parser)]
#[command(
    about = "Project chat using Playtime's compacting harness and a caller-owned Strata endpoint",
    group(clap::ArgGroup::new("one_shot").args(["prompt", "prompt_file"])),
    version
)]
struct Args {
    /// Loopback endpoint held through the host's exclusive session helper.
    #[arg(long, required_unless_present_any = ["select_session", "list_sessions", "inspect_interrupted"])]
    endpoint: Option<String>,
    /// Reviewed identity lock. The server owner must verify its host artifacts.
    #[arg(long)]
    model_lock: PathBuf,
    #[arg(long)]
    image_lock: Option<PathBuf>,
    /// workshop uses Docker; coding-host uses your user account and host tools.
    #[arg(long, value_enum, default_value_t = Preset::Workshop)]
    preset: Preset,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Private state outside the workspace (default: ~/.local/state/playtime-chat).
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Create or reopen this private session directory. Only completed turns resume.
    #[arg(long)]
    session: Option<PathBuf>,
    /// Pick a compatible saved conversation locally, without starting inference.
    #[arg(long, conflicts_with_all = ["session", "recover_compaction", "recover_interrupted", "inspect_interrupted", "prompt", "list_sessions", "endpoint"])]
    select_session: bool,
    /// List recent compatible sessions as JSON without contacting inference.
    #[arg(long, conflicts_with_all = ["session", "recover_compaction", "recover_interrupted", "inspect_interrupted", "prompt", "endpoint"])]
    list_sessions: bool,
    /// Fork the last completed history after a truncated first compaction.
    /// Raises the summary cap to --summary-tokens (default: 4096); keeps the
    /// failed session and all evidence intact.
    #[arg(long, conflicts_with = "session")]
    recover_compaction: Option<PathBuf>,
    /// Inspect a stopped turn locally; does not start inference or execute tools.
    #[arg(long, conflicts_with_all = ["session", "recover_compaction", "recover_interrupted", "prompt", "endpoint"])]
    inspect_interrupted: Option<PathBuf>,
    /// Fork completed history and an interruption report; never replay tools.
    #[arg(long, conflicts_with_all = ["session", "recover_compaction", "prompt"])]
    recover_interrupted: Option<PathBuf>,
    /// Accept that tools without results may have partially changed project files.
    #[arg(long, requires = "recover_interrupted")]
    acknowledge_uncertain_tools: bool,
    /// Run one turn and exit; otherwise open basic interactive chat.
    #[arg(long)]
    prompt: Option<String>,
    /// Read one exact UTF-8 prompt from a regular file and start a fresh session.
    #[arg(long, conflicts_with_all = ["prompt", "session", "recover_compaction", "recover_interrupted", "select_session", "list_sessions", "inspect_interrupted"])]
    prompt_file: Option<PathBuf>,
    /// Retain host-tool cleanup evidence for a one-shot launcher invocation.
    #[arg(long, requires_all = ["prompt_file", "launch_id"])]
    completion_receipt: Option<PathBuf>,
    #[arg(long, requires = "completion_receipt")]
    launch_id: Option<String>,
    /// Coding instructions; a saved conversation reopens only with the prompt it was created with.
    #[arg(long, value_enum, default_value_t)]
    agent_prompt: AgentPrompt,
    #[arg(long, requires = "one_shot")]
    json: bool,
    /// Print model text as it arrives instead of showing a compact activity row.
    #[arg(long)]
    stream: bool,
    /// Context tokens (default: the locked server context, or a saved session's own).
    #[arg(long)]
    context_tokens: Option<u64>,
    /// Seconds per turn (default: 1800, or a saved session's own).
    #[arg(long)]
    wall_seconds: Option<u64>,
    /// Output tokens per model call (default: 16384, or a saved session's own).
    #[arg(long)]
    tokens_per_call: Option<u64>,
    /// Generated tokens per turn (default: 65536, or a saved session's own).
    #[arg(long)]
    generated_tokens: Option<u64>,
    /// Output tokens per compaction summary (default: 4096, or a saved session's own).
    #[arg(long)]
    summary_tokens: Option<u64>,
    #[arg(long, default_value_t = 16384)]
    output_bytes: u32,
}
fn config(
    args: &Args,
    lock: &ModelLock,
    binding: &serde_json::Value,
    inventory: String,
    limits: Option<&ProjectLimits>,
) -> Result<ConversationConfig> {
    let context = args.context_tokens.unwrap_or(lock.identity.context_tokens);
    if context > lock.identity.context_tokens {
        return Err("requested context exceeds the locked server context".into());
    }
    let workspace = Sha256Digest::new(
        Sha256::digest(serde_json::to_vec(binding)?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )?;
    let config = ConversationConfig {
        schema_version: if args.preset == Preset::CodingHost {
            2
        } else {
            1
        },
        model: lock.fingerprint()?,
        workspace,
        instructions: if args.preset == Preset::CodingHost {
            args.agent_prompt.host_instructions()
        } else {
            args.agent_prompt.instructions()
        },
        inventory,
        mode: RunMode::LiveSmoke,
        execution: ExecutionConfig {
            tool_profile: if args.preset == Preset::CodingHost {
                ToolProfile::HostV1
            } else {
                ToolProfile::Workshop
            },
            tool_policy: ToolPolicy::ConfirmedExternal {
                return_overhead_ms: 1000,
            },
            sampling_seed: limits.map_or(1042, |limits| limits.seed),
            limits: CommonLimits {
                wall_seconds: args.wall_seconds.unwrap_or(WALL_SECONDS).try_into()?,
                generated_tokens: args
                    .generated_tokens
                    .unwrap_or(GENERATED_TOKENS)
                    .try_into()?,
                tokens_per_call: args.tokens_per_call.unwrap_or(TOKENS_PER_CALL).try_into()?,
                context_tokens: context.try_into()?,
                model_seconds: MODEL_SECONDS.try_into()?,
                command_seconds: (if args.preset == Preset::CodingHost {
                    300
                } else {
                    90
                })
                .try_into()?,
                output_bytes: u64::from(args.output_bytes).try_into()?,
                log_bytes: (16 * 1024 * 1024).try_into()?,
            },
            workshop: limits
                .map(|limits| -> Result<WorkshopLimits> {
                    Ok(WorkshopLimits {
                        cpu_cores: 2.0,
                        memory_bytes: limits.memory_bytes.try_into()?,
                        workspace_bytes: (1024 * 1024).try_into()?,
                        tmp_bytes: limits.tmp_bytes.try_into()?,
                        pids: limits.pids.try_into()?,
                    })
                })
                .transpose()?,
        },
        compaction: CompactionSettings {
            allow_finish_run: false,
            summary_tokens: args.summary_tokens.unwrap_or(SUMMARY_TOKENS).try_into()?,
            ..Default::default()
        },
    };
    Ok(config)
}
/// The limits given explicitly, which a reopened session takes instead of its own.
fn explicit_limits(args: &Args) -> Result<Overrides> {
    let explicit = |value: Option<u64>| value.map(NonZeroU64::try_from).transpose();
    Ok(Overrides {
        wall_seconds: explicit(args.wall_seconds)?,
        generated_tokens: explicit(args.generated_tokens)?,
        tokens_per_call: explicit(args.tokens_per_call)?,
        context_tokens: explicit(args.context_tokens)?,
        // No flag sets it, so a saved session keeps its own.
        model_seconds: None,
        // Compaction recovery exists to raise the summary cap, so the new cap
        // never comes from the source session.
        summary_tokens: if args.recover_compaction.is_some() {
            Some(args.summary_tokens.unwrap_or(SUMMARY_TOKENS).try_into()?)
        } else {
            explicit(args.summary_tokens)?
        },
    })
}
async fn read_prompt<W: io::Write, D: io::Write>(
    input: &mut Console,
    shutdown: &CancellationToken,
    output: &mut Output<W, D>,
) -> Result<Option<String>> {
    loop {
        let Some(line) = input.prompt(shutdown).await? else {
            return Ok(None);
        };
        match line.trim() {
            "/quit" | "/exit" => return Ok(None),
            "/help" => {
                println!(
                    "Paste inserts an editable multiline message; Enter submits it. Escape, Ctrl-C or /stop cancels a running turn. /tool lists recent results; /tool N shows captured output. /quit exits. Safely stopped turns return to input with completed history and a recovery notice. Unknown tool outcomes require explicit recovery. /paste then /end also accepts multiple lines."
                );
            }
            line if line == "/tool" || line.starts_with("/tool ") => {
                let number = line[5..].trim();
                if number.is_empty() {
                    output.inspect_tool(None)?;
                } else if let Ok(number) = number.parse::<usize>() {
                    output.inspect_tool(Some(number))?;
                } else {
                    println!("Use /tool to list results or /tool N to inspect one.");
                }
            }
            "/paste" => {
                println!("Paste text; finish with /end on a line by itself.");
                let mut text = String::new();
                loop {
                    let Some(line) = input.prompt(shutdown).await? else {
                        return Ok(None);
                    };
                    if line == "/end" {
                        break;
                    }
                    text.push_str(&line);
                    text.push('\n');
                    if text.len() > playtime_engine::chat::MAX_USER_BYTES {
                        return Err("paste exceeds 256 KiB".into());
                    }
                }
                if !text.trim().is_empty() {
                    return Ok(Some(text));
                }
            }
            "" => (),
            _ => return Ok(Some(line)),
        }
    }
}
async fn recover_stopped_turn(
    input: &mut Console,
    shutdown: &CancellationToken,
    source: &std::path::Path,
    destination: &std::path::Path,
    config: &ConversationConfig,
) -> Result<Session> {
    let path = source.to_owned();
    let c = config.clone();
    let report =
        tokio::task::spawn_blocking(move || Session::inspect_interrupted(&path, &c)).await??;
    let acknowledge = report.uncertain_tools != 0;
    if acknowledge {
        note!("{}", clean(&report.context()?));
        loop {
            note!(
                "Turn stopped and processes cleaned up, but some tool outcomes were not recorded. Inspect possible partial changes. Type /recover to acknowledge and continue, or /quit to exit. Nothing will be replayed."
            );
            match input.prompt(shutdown).await?.as_deref().map(str::trim) {
                Some("/recover") => break,
                None | Some("/quit" | "/exit") => {
                    return Err("stopped turn retained; recovery not acknowledged".into());
                }
                _ => (),
            }
        }
    }
    let source = source.to_owned();
    let destination = destination.to_owned();
    let config = config.clone();
    Ok(tokio::task::spawn_blocking(move || {
        Session::recover_interrupted(&source, &destination, config, acknowledge)
    })
    .await??)
}

async fn main_run(mut args: Args) -> Result<i32> {
    if let Some(path) = &args.prompt_file {
        args.prompt = Some(oneshot::prompt(path)?);
    }
    if args.completion_receipt.is_some() && args.preset != Preset::CodingHost {
        return Err("completion receipts currently require --preset coding-host".into());
    }
    if let Some(path) = &args.completion_receipt {
        oneshot::check_slot(path)?;
    }
    if args.preset == Preset::CodingHost
        && (args.image_lock.is_some() || matches!(args.agent_prompt, AgentPrompt::Minimal))
    {
        return Err("coding-host uses host tools and a full coding prompt; image locks and the legacy minimal prompt do not apply".into());
    }
    let image_path = args.image_lock.clone().unwrap_or_else(|| {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../environments/general.lock.json"
        ))
    });
    let shutdown = CancellationToken::new();
    if !(args.select_session || args.list_sessions || args.inspect_interrupted.is_some()) {
        let signal_cancel = shutdown.clone();
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::spawn(async move {
            tokio::select! {_=terminate.recv()=>(),_=hangup.recv()=>()};
            signal_cancel.cancel();
        });
    }
    let private = args.state_dir.clone().unwrap_or(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set; pass --state-dir")?
            .join(".local/state/playtime-chat"),
    );
    PrivateDirectory::open(&private, true)?;
    let private = std::fs::canonicalize(private)?;
    let session_path = args
        .session
        .clone()
        .unwrap_or(private.join(format!("session-{}", Token::generate()?.expose())));
    let local_selection =
        args.select_session || args.list_sessions || args.inspect_interrupted.is_some();
    let mut session_path = if local_selection {
        session_path
    } else {
        PrivateDirectory::open(&session_path, true)?;
        std::fs::canonicalize(session_path)?
    };
    let project = args.workspace.clone();
    let state = private.clone();
    let mut protected = vec![args.model_lock.clone(), image_path.clone()];
    if !local_selection {
        protected.push(session_path.clone());
    }
    for source in [
        &args.recover_compaction,
        &args.recover_interrupted,
        &args.inspect_interrupted,
    ]
    .into_iter()
    .flatten()
    {
        protected.push(source.clone());
    }
    let mut directory = None;
    let mut host = None;
    let root;
    if args.preset == Preset::CodingHost {
        let output_bytes = args.output_bytes;
        let workspace = tokio::task::spawn_blocking(move || {
            playtime_chat::host::Workspace::open(&project, &state, output_bytes)
        })
        .await??;
        root = workspace.root.clone();
        host = Some(Arc::new(Mutex::new(workspace)));
    } else {
        let opened = tokio::task::spawn_blocking(move || {
            ProjectDirectory::open(&project, &state, &protected)
        })
        .await??;
        root = opened.root().path().to_owned();
        directory = Some(Arc::new(opened));
    }
    let lock: ModelLock = serde_json::from_slice(&std::fs::read(&args.model_lock)?)?;
    let image_bytes = if host.is_none() {
        std::fs::read(&image_path)?
    } else {
        Vec::new()
    };
    if image_bytes.len() > 65536 {
        return Err("image lock exceeds 64 KiB".into());
    }
    let image: serde_json::Value = if host.is_none() {
        serde_json::from_slice(&image_bytes)?
    } else {
        serde_json::Value::Null
    };
    let limits = (args.preset == Preset::Workshop).then_some(ProjectLimits {
        nano_cpus: 2_000_000_000,
        memory_bytes: 4 * 1024 * 1024 * 1024,
        tmp_bytes: 256 * 1024 * 1024,
        tmp_inodes: 32768,
        pids: 256,
        seed: 1042,
    });
    let (binding, inventory) = if let Some(host) = &host {
        let workspace = host.lock().await;
        (workspace.binding(), workspace.inventory())
    } else {
        (
            json!({"profile":"project-docker-v1","root":directory.as_ref().unwrap().root(),"limits":limits,"image":image,"bridge":docker::BRIDGE_SHA256,"project_disk_quota":null}),
            playtime_chat::environment::inventory(&image, &root)?,
        )
    };
    let config = config(&args, &lock, &binding, inventory, limits.as_ref())?;
    let earlier = lock.earlier_fingerprint()?;
    let overrides = explicit_limits(&args)?;
    // A saved session is resumed, inspected or forked under the binding and limits
    // it was saved with.
    let saved = [
        &args.inspect_interrupted,
        &args.recover_compaction,
        &args.recover_interrupted,
    ]
    .into_iter()
    .flatten()
    .next()
    .cloned()
    .or_else(|| {
        (!local_selection && session_path.join("config.json").exists())
            .then(|| session_path.clone())
    });
    let config = match &saved {
        Some(path) => {
            playtime_chat::sessions::resume_config(&config, earlier.as_ref(), &overrides, path)
        }
        None => config,
    };
    config.validate()?;
    if let Some(source) = &args.inspect_interrupted {
        let report = Session::inspect_interrupted(source, &config)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }
    if local_selection {
        let entries =
            playtime_chat::sessions::catalog(&private, &config, earlier.as_ref(), &overrides)?;
        if args.list_sessions {
            println!("{}", serde_json::to_string(&entries)?);
        } else {
            eprintln!(
                "Playtime sessions — {}\nModel: {}\nAgent prompt: {}\nPreset: {}",
                clean(&root.display().to_string()),
                clean(&lock.identity.model),
                args.agent_prompt.name(),
                if host.is_some() {
                    "coding-host"
                } else {
                    "workshop"
                }
            );
            let choice = playtime_chat::sessions::select(
                &entries,
                &config,
                earlier.as_ref(),
                &overrides,
                &mut io::stdin().lock(),
                &mut io::stderr().lock(),
            )?;
            println!("{}", serde_json::to_string(&choice)?);
        }
        return Ok(0);
    }
    let mut client = Client::new(
        Endpoint::parse(args.endpoint.as_deref().ok_or("missing endpoint")?)?,
        lock,
        Default::default(),
    )?;
    client
        .verify(
            Instant::now() + Duration::from_secs(20),
            &CancellationToken::new(),
        )
        .await?;
    let model = Arc::new(Mutex::new(Model {
        client,
        fenced: false,
    }));
    let docker_backend = if host.is_none() {
        let transport = Arc::new(Cli::resolve(
            std::path::Path::new("/usr/bin/docker"),
            std::path::Path::new("/var/run/docker.sock"),
            &private.join(format!("docker-{}", Token::generate()?.expose())),
        )?);
        let engine = docker::Engine::connect(
            transport,
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
        let ready = engine
            .readiness(
                &image_bytes,
                Instant::now() + Duration::from_secs(20),
                CancellationToken::new(),
            )
            .await?;
        Some((engine, ready))
    } else {
        None
    };
    let is_new_session = args.recover_compaction.is_some()
        || args.recover_interrupted.is_some()
        || !session_path.join("config.json").exists();
    let (initial_session, recovered_input) = {
        let p = session_path.clone();
        let c = config.clone();
        let source = args.recover_compaction.clone();
        let interrupted = args.recover_interrupted.clone();
        let acknowledged = args.acknowledge_uncertain_tools;
        tokio::task::spawn_blocking(move || {
            if let Some(source) = source {
                let recovered = Session::recover_compaction(&source, &p, c)?;
                Ok::<_, io::Error>((recovered.session, Some(recovered.pending_input)))
            } else if let Some(source) = interrupted {
                Session::recover_interrupted(&source, &p, c, acknowledged)
                    .map(|session| (session, None))
            } else if p.join("config.json").exists() {
                Session::open(&p, &c).map(|session| (session, None))
            } else {
                Session::create(&p, c).map(|session| (session, None))
            }
        })
        .await??
    };
    if is_new_session {
        if host.is_some() {
            PrivateDirectory::open(&session_path, false)?
                .write_new("host-execution.json", &serde_json::to_vec(&binding)?)?;
        } else {
            playtime_chat::environment::save(&session_path, &image_bytes)?;
        }
    }
    if let Some(report) = initial_session.pending_recovery() {
        eprintln!(
            "{}\nRecovered into a new session; the source is unchanged. Enter a fresh instruction when ready. No commands or messages have been replayed.",
            clean(&report.context()?)
        );
    }
    let mut session = Some(initial_session);
    eprintln!(
        "Playtime chat — {}\nSession: {}\nAgent prompt: {}\nPreset: {}\nContext: {} tokens. Tools: {}. /help for commands.",
        root.display(),
        session_path.display(),
        args.agent_prompt.name(),
        if host.is_some() {
            "coding-host"
        } else {
            "workshop"
        },
        config.execution.limits.context_tokens,
        if host.is_some() {
            "host coding; normal user filesystem/network access"
        } else {
            "Docker, no network"
        }
    );
    if let Some(input) = recovered_input {
        eprintln!(
            "Recovered completed history into a new session; the failed source is unchanged.\nPending message (submit again when ready): {}",
            clean(&input)
        );
    }
    let headless = args.prompt.is_some();
    let mut first = args.prompt.clone();
    let mut input = if headless {
        None
    } else {
        Some(Console::new()?)
    };
    let mut output = Output::new(
        io::stdout(),
        io::stderr(),
        args.json,
        !args.stream && io::stdout().is_terminal() && io::stderr().is_terminal(),
    );
    let mut one_shot_outcome = "failed";
    let result: Result<Option<PathBuf>> = async {
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let prompt = if headless {
                first.take()
            } else {
                read_prompt(input.as_mut().unwrap(), &shutdown, &mut output).await?
            };
            let Some(prompt) = prompt else {
                break;
            };
            let effects = if let Some(host) = &host {
                ChatEffects::Host(playtime_chat::host::HostEffects { inner: ModelEffects(model.clone()), workspace: host.clone() })
            } else {
            let (engine, ready) = docker_backend.as_ref().unwrap();
            let directory = directory.as_ref().unwrap();
            let owner = docker::Ownership {
                service: "playtime-chat".into(),
                run: format!("turn-{}", docker::Ownership::random_nonce()?),
                grant: "interactive-project".into(),
                invocation: docker::Ownership::random_nonce()?,
                nonce: docker::Ownership::random_nonce()?,
                daemon: engine.identity().clone(),
            };
            let (ready, directory, limits, private) = (
                ready.clone(),
                directory.clone(),
                limits.as_ref().unwrap().clone(),
                private.clone(),
            );
            let wall = config.execution.limits.wall_seconds.get();
            let workspace = tokio::task::spawn_blocking(move || {
                ProjectWorkspace::new(
                    ready,
                    owner,
                    directory,
                    limits,
                    docker::WorkDeadline::after(Duration::from_secs(wall))?,
                    Arc::new(Journal(private)),
                )
            })
            .await??;
            ChatEffects::Docker(Box::new(NativeEffects {
                model: model.clone(),
                workspace,
                output_bytes: args.output_bytes,
                return_overhead: Duration::from_secs(1),
            }))
            };
            let effects = Arc::new(Mutex::new(effects));
            let (tx, mut rx) = runtime::events();
            let cancel = shutdown.child_token();
            let interrupt_cancel = cancel.clone();
            let interrupt_watch = tokio::spawn(async move {
                tokio::select! {
                    _=tokio::signal::ctrl_c()=>interrupt_cancel.cancel(),
                    _=interrupt_cancel.cancelled()=>(),
                }
            });
            let mut task = tokio::spawn(runtime::run_turn(
                session.take().ok_or("session is not resumable")?,
                prompt,
                effects,
                tx,
                cancel.clone(),
            ));
            let mut refresh = tokio::time::interval(Duration::from_millis(100));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut input_open = !headless;
            let mut shutting_down = false;
            let mut leave_after_turn = false;
            let mut requested_stop = false;
            let mut typed = String::new();
            let completed = loop {
                tokio::select! {
                    event=rx.recv()=>if let Some(event)=event {if let Err(error)=output.render(&event){cancel.cancel();note!("output failed: {error}");}}else{break task.await??;},
                    result=&mut task=>{while let Some(event)=rx.recv().await {output.render(&event)?;}break result??;},
                    _=tokio::signal::ctrl_c()=>{requested_stop=true;cancel.cancel();let _=output.notice("Stopping; waiting for owned cleanup...",true);},
                    _=shutdown.cancelled(),if !shutting_down=>{shutting_down=true;cancel.cancel();let _=output.notice("Stopping; waiting for owned cleanup...",true);},
                    _=refresh.tick()=>{
                        let columns=rustix::termios::tcgetwinsize(io::stderr()).map_or(80,|size|if size.ws_col == 0 {80} else {usize::from(size.ws_col)});
                        if let Err(error)=output.tick(columns) {cancel.cancel();note!("output failed: {error}");}
                    },
                    key=async { input.as_mut().unwrap().recv().await },if input_open=>match key {
                        Err(error)=>{let _=output.notice(&format!("Input failed: {error}"),true);input_open=false;leave_after_turn=true;cancel.cancel();},
                        Ok(key)=>match key {
                        Some(Key::Escape | Key::Interrupt)=>{requested_stop=true;cancel.cancel();let _=output.notice("Stopping; waiting for owned cleanup...",true);},
                        Some(Key::Text(text))=>{if typed.len()+text.len()<=playtime_engine::chat::MAX_USER_BYTES {typed.push_str(&text);}},
                        Some(Key::Backspace)=>{typed.pop();},
                        event @ Some(Key::Line(_)|Key::Enter)=>{
                            let line=if let Some(Key::Line(line))=event {line} else {std::mem::take(&mut typed)};
                            if matches!(line.trim(),"/stop"|"/quit"|"/exit") {leave_after_turn=line.trim()!="/stop";requested_stop=true;cancel.cancel();let _=output.notice("Stopping; waiting for owned cleanup...",true);}
                            else {let _=output.notice("A turn is running. Press Escape to stop it; submit your next message after cleanup.",false);}
                        },
                        None|Some(Key::Eof)=>{input_open=false;leave_after_turn=true;cancel.cancel();let _=output.notice("Input closed; waiting for owned cleanup...",true);},
                        Some(Key::Warning(warning))=>{let _=output.notice(&warning,false);},
                        _=>(),
                        },
                    },
                }
            };
            interrupt_watch.abort();
            one_shot_outcome = match completed.result.reason {
                playtime_core::StopReason::AgentFinished => "completed",
                playtime_core::StopReason::Interrupted => "cancelled",
                playtime_core::StopReason::WallLimit => "timed_out",
                _ => "failed",
            };
            output.finish(completed.result.reason != playtime_core::StopReason::AgentFinished)?;
            session = completed.session;
            if session.is_none() && requested_stop && !headless && !leave_after_turn && !shutdown.is_cancelled()
                && completed.result.reason == playtime_core::StopReason::Interrupted {
                let source = session_path.clone();
                let destination = private.join(format!("session-{}", Token::generate()?.expose()));

                // The shared recovery audit requires terminal records, confirmed
                // tool cleanup and observed tool outcomes. Never replay this turn.
                match recover_stopped_turn(input.as_mut().unwrap(), &shutdown, &source, &destination, &config).await {
                    Ok(recovered) => {
                        if host.is_some() {
                            PrivateDirectory::open(&destination, false)?.write_new("host-execution.json", &serde_json::to_vec(&binding)?)?;
                        } else { playtime_chat::environment::save(&destination, &image_bytes)?; }
                        note!("Stopped. Completed history retained; partial changes may remain. No input or tools were replayed.\nSession: {}", destination.display());
                        session_path = destination;
                        session = Some(recovered);
                        input.as_mut().unwrap().discard_pending();
                        if model.lock().await.fenced {
                            if std::env::var_os("PLAYTIME_CHAT_RESTART_FILE").is_some() {
                                note!("Reconnecting once the inference owner confirms shutdown...");
                                return Ok(Some(session_path.clone()));
                            }
                            return Err(format!("remote inference needs owner shutdown before resuming --session {}",session_path.display()).into());
                        }
                    }
                    Err(error) => note!("Automatic continuation is unavailable: {}", clean(&error.to_string())),
                }
            }
            if session.is_none() {
                if completed.result.reason == playtime_core::StopReason::CompactionFailed {
                    note!(
                        "If this was the first call of the turn and its summary hit the output cap, use --recover-compaction {} with a larger --summary-tokens value. Recovery verifies the saved events and cleanup before creating a new session.",
                        session_path.display()
                    );
                }
                note!(
                    "Use the session picker recovery action, or --inspect-interrupted {} followed by --recover-interrupted with the same settings. Missing cleanup evidence stays blocked.",
                    session_path.display()
                );
                return Err(format!("turn stopped with {:?}; evidence retained at {}; no tools will be automatically replayed",completed.result.reason,session_path.display()).into());
            }
            if headless || leave_after_turn {
                break;
            }
        }
        Ok(None::<PathBuf>)
    }
    .await;
    if shutdown.is_cancelled() {
        one_shot_outcome = "cancelled";
    }
    let cleanup = if let Some(host) = &host {
        host.lock().await.close().await
    } else {
        Ok(())
    };
    // Report unresolved cleanup ahead of a failed receipt write. Without the
    // receipt, the launcher already reports the tools as unresolved.
    let receipt = args
        .completion_receipt
        .as_deref()
        .map_or(Ok(()), |path| -> Result<()> {
            oneshot::completion(
                path,
                args.launch_id.as_deref().ok_or("missing launch ID")?,
                args.prompt.as_deref().ok_or("missing one-shot prompt")?,
                one_shot_outcome,
                if cleanup.is_ok() {
                    "confirmed"
                } else {
                    "unresolved"
                },
                &session_path,
            )
        });
    cleanup?;
    receipt?;
    let restart = result?;
    if let Some(session) = restart {
        let path = PathBuf::from(
            std::env::var_os("PLAYTIME_CHAT_RESTART_FILE").ok_or("missing restart path")?,
        );
        let parent = PrivateDirectory::open(path.parent().ok_or("invalid restart path")?, false)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("invalid restart filename")?;
        parent.write_new(name, &serde_json::to_vec(&json!({"session":session}))?)?;
        Ok(75)
    } else {
        Ok(0)
    }
}

enum ChatEffects {
    Docker(Box<NativeEffects>),
    Host(playtime_chat::host::HostEffects<ModelEffects>),
}
#[async_trait::async_trait]
impl Effects for ChatEffects {
    async fn execute(
        &mut self,
        effect: &playtime_engine::Effect,
        deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<serde_json::Value>,
    ) -> playtime_engine::Outcome {
        match self {
            Self::Docker(e) => e.execute(effect, deadline, cancel, chunks).await,
            Self::Host(e) => e.execute(effect, deadline, cancel, chunks).await,
        }
    }
    async fn cleanup(&mut self, deadline: Instant) -> Vec<String> {
        match self {
            Self::Docker(e) => e.cleanup(deadline).await,
            Self::Host(e) => e.cleanup(deadline).await,
        }
    }
}
#[tokio::main]
async fn main() {
    let code = match main_run(Args::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("playtime-chat: {}", clean(&error.to_string()));
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod one_shot_arguments {
    use super::*;
    #[test]
    fn file_prompt_is_headless_and_cannot_select_saved_history() {
        let base = [
            "playtime-chat",
            "--endpoint",
            "http://127.0.0.1:1/v1",
            "--model-lock",
            "model.json",
            "--prompt-file",
            "prompt.md",
            "--json",
        ];
        assert!(Args::try_parse_from(base).is_ok());
        for extra in [
            ["--prompt", "inline"],
            ["--session", "old"],
            ["--recover-compaction", "old"],
            ["--recover-interrupted", "old"],
        ] {
            assert!(Args::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
        assert!(
            Args::try_parse_from(
                base.into_iter()
                    .chain(["--completion-receipt", "receipt.json"])
            )
            .is_err()
        );
        assert!(
            Args::try_parse_from(base.into_iter().chain([
                "--completion-receipt",
                "receipt.json",
                "--launch-id",
                "launch-123"
            ]))
            .is_ok()
        );
    }
}

#[cfg(test)]
mod limit_arguments {
    use super::*;
    #[test]
    fn only_limits_given_explicitly_override_a_saved_session() {
        let base = [
            "playtime-chat",
            "--endpoint",
            "http://127.0.0.1:1/v1",
            "--model-lock",
            "model.json",
        ];
        let parse = |extra: &[&str]| {
            let args = Args::try_parse_from(base.iter().chain(extra)).unwrap();
            explicit_limits(&args).unwrap()
        };
        let tokens = |n: u64| Some(NonZeroU64::new(n).unwrap());
        assert_eq!(parse(&[]), Overrides::default());
        assert_eq!(
            parse(&["--tokens-per-call", "8192", "--wall-seconds", "900"]),
            Overrides {
                tokens_per_call: tokens(8192),
                wall_seconds: tokens(900),
                ..Default::default()
            }
        );
        // Compaction recovery always sets the new summary cap.
        let recover = ["--recover-compaction", "old"];
        assert_eq!(parse(&recover).summary_tokens, tokens(SUMMARY_TOKENS));
        let given = [&recover[..], &["--summary-tokens", "8192"]].concat();
        assert_eq!(parse(&given).summary_tokens, tokens(8192));
        let zero = Args::try_parse_from(base.iter().chain(&["--wall-seconds", "0"])).unwrap();
        assert!(explicit_limits(&zero).is_err());
    }
}
