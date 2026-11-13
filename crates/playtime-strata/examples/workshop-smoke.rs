//! Explicit, bounded developer gate: a caller-owned Strata endpoint and a new
//! uid-1000 Docker workshop. This is not a production session/ownership driver.
use playtime_core::{RunMode, Sha256Digest, StopReason};
use playtime_docker::{self as docker, OwnershipJournal, Proposal, transport::Cli};
use playtime_engine::{
    EffectKind, Input, Outcome, ToolPolicy,
    chat::ConversationConfig,
    compaction::CompactionSettings,
    simulation::{Scenario, SequentialIds},
};
use playtime_harness::{Session, Turn};
use playtime_strata::{self as strata, CancellationToken, Client, Endpoint, ModelLock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Journal(PathBuf);
#[async_trait::async_trait]
impl OwnershipJournal for Journal {
    async fn commit(&self, proposal: &Proposal) -> docker::Result<()> {
        let path = self.0.join(format!("{}.json", proposal.name));
        let proposal = proposal.clone();
        tokio::task::spawn_blocking(move || {
            write_new(&path, &serde_json::to_vec_pretty(&proposal)?)
        })
        .await
        .map_err(|_| journal_error())?
        .map_err(|_| journal_error())
    }
}
fn journal_error() -> docker::Error {
    docker::Error {
        failure: docker::Failure::Journal,
        operation: "strata_smoke_journal",
        detail: "cannot commit owned resource",
    }
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::File::open(path.parent().ok_or("missing parent")?)?.sync_all()?;
    Ok(())
}
fn now(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}
async fn advance(
    mut turn: Turn<SequentialIds>,
    start: Instant,
    input: Input,
) -> Result<Turn<SequentialIds>> {
    // The HTTP task and Docker's absolute lifetime keep running while fsync is
    // blocked. No effect starts until its observation/dispatch records commit.
    tokio::task::spawn_blocking(move || {
        turn.advance(now(start), input)?;
        Ok(turn)
    })
    .await?
}
fn config(lock: &ModelLock, image: &Value, nonce: &str) -> Result<ConversationConfig> {
    let fixture: Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))?;
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = lock.identity.context_tokens.try_into()?;
    parameters.limits.tokens_per_call = 512.try_into()?;
    parameters.limits.generated_tokens = 2048.try_into()?;
    parameters.limits.wall_seconds = 240.try_into()?;
    parameters.limits.model_seconds = 90.try_into()?;
    parameters.limits.command_seconds = 10.try_into()?;
    parameters.limits.output_bytes = 4096.try_into()?;
    parameters.limits.log_bytes = (4 * 1024 * 1024).try_into()?;
    let limits = docker::Limits::smoke();
    parameters.workshop.memory_bytes = limits.memory_bytes.try_into()?;
    parameters.workshop.workspace_bytes = limits.workspace_bytes.try_into()?;
    parameters.workshop.tmp_bytes = limits.tmp_bytes.try_into()?;
    parameters.workshop.pids = limits.pids.try_into()?;
    let workspace = serde_json::to_vec(
        &json!({"image":image,"limits":limits,"seed_file":nonce,"bridge":docker::BRIDGE_SHA256}),
    )?;
    let workspace = Sha256Digest::new(
        Sha256::digest(workspace)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )?;
    Ok(ConversationConfig{schema_version:1,model:lock.fingerprint()?,workspace,
        instructions:"You are testing a tool transport in a disposable workshop. Follow the user request, use the tools when needed, and give a concise plain final answer.".into(),
        inventory:"An isolated uid-1000 workshop with a prepared /work/probe.txt file.".into(),
        mode:RunMode::LiveSmoke,tool_policy:ToolPolicy::ConfirmedExternal{return_overhead_ms:1000},parameters,
        compaction:CompactionSettings{allow_finish_run:false,summary_tokens:256.try_into()?,..Default::default()}})
}
fn tool(request: &playtime_engine::ToolRequest) -> Result<docker::Tool> {
    let a = &request.arguments;
    let string = |key| {
        a[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("invalid tool argument {key}"))
    };
    Ok(match request.name.as_str() {
        "read_file" => docker::Tool::ReadFile {
            path: string("path")?,
            offset_bytes: a["offset_bytes"].as_u64().unwrap_or(0),
        },
        "write_file" => docker::Tool::WriteFile {
            path: string("path")?,
            content: string("content")?,
            append: a["append"].as_bool().unwrap_or(false),
        },
        "exec" => docker::Tool::Exec {
            command: string("command")?,
            cwd: a["cwd"].as_str().map(str::to_owned),
            timeout_seconds: a["timeout_seconds"].as_f64(),
        },
        _ => return Err("unsupported workshop tool".into()),
    })
}

async fn conversation(
    root: &Path,
    mut client: Client,
    sandbox: &mut docker::Sandbox,
    config: ConversationConfig,
    nonce: &str,
) -> Result<Client> {
    let start = Instant::now();
    let session_root = root.join("conversation");
    let mut turn=tokio::task::spawn_blocking(move||Session::create(&session_root,config)?.begin("Use read_file to read /work/probe.txt, then reply with its exact contents. Do not modify files or execute commands.".into(),SequentialIds,0)).await??;
    let mut model_calls = 0;
    let mut tool_calls = 0;
    let mut final_text = String::new();
    loop {
        let Some(effect) = turn.pending().cloned() else {
            break;
        };
        turn = advance(
            turn,
            start,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .await?;
        if turn.pending().is_none_or(|e| e.id != effect.id) {
            continue;
        }
        let deadline = start + Duration::from_millis(effect.deadline_ms);
        let outcome = match &effect.kind {
            EffectKind::StartWorkspace => {
                sandbox
                    .initialize(deadline, CancellationToken::new())
                    .await?;
                sandbox
                    .start_workspace(deadline, CancellationToken::new())
                    .await?;
                sandbox
                    .tool(
                        docker::Tool::WriteFile {
                            path: "/work/probe.txt".into(),
                            content: nonce.into(),
                            append: false,
                        },
                        docker::ToolLimits {
                            command_seconds: 5.0,
                            output_bytes: 4096,
                        },
                        deadline,
                        CancellationToken::new(),
                    )
                    .await?;
                Outcome::WorkspaceStarted
            }
            EffectKind::Model(request) => {
                model_calls += 1;
                let (tx, mut rx) = strata::events();
                let request = request.clone();
                let cancel = CancellationToken::new();
                let model_cancel = cancel.clone();
                let _cancel_on_error = cancel.clone().drop_guard();
                let task = tokio::spawn(async move {
                    let result = client.complete(&request, deadline, &model_cancel, tx).await;
                    (client, result)
                });
                let mut events = Vec::new();
                while let Some(event) = rx.recv().await {
                    if let Some(chunk) = event.controller_chunk() {
                        turn = advance(
                            turn,
                            start,
                            Input::Chunk {
                                effect_id: effect.id,
                                chunk,
                            },
                        )
                        .await?;
                        if turn.pending().is_none_or(|e| e.id != effect.id) {
                            cancel.cancel();
                        }
                    }
                    events.push(event.data);
                }
                let (returned, result) = task.await?;
                client = returned;
                if let Ok(done) = &result.completion {
                    final_text = done.response.message["content"]
                        .as_str()
                        .unwrap_or("")
                        .into();
                }
                let evidence = json!({"events":events,"completion":result.completion,"remote":result.remote,"partial":result.partial,"received_bytes":result.received_bytes});
                let path = root.join(format!("model-{model_calls}.json"));
                tokio::task::spawn_blocking(move || {
                    write_new(&path, &serde_json::to_vec_pretty(&evidence)?)
                })
                .await??;
                result.into_outcome()
            }
            EffectKind::Tool(request) => {
                tool_calls += 1;
                let command_seconds = deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    - 1.0;
                let reply = sandbox
                    .tool(
                        tool(request)?,
                        docker::ToolLimits {
                            command_seconds,
                            output_bytes: 4096,
                        },
                        deadline,
                        CancellationToken::new(),
                    )
                    .await?;
                Outcome::Tool(serde_json::to_value(reply)?)
            }
            EffectKind::Cleanup => {
                let cleanup = sandbox.cleanup(deadline, CancellationToken::new()).await;
                if !cleanup.complete() {
                    return Err("workshop cleanup is incomplete".into());
                }
                Outcome::Cleaned(vec![])
            }
            _ => return Err("unexpected effect in chat smoke".into()),
        };
        turn = advance(
            turn,
            start,
            Input::Complete {
                effect_id: effect.id,
                outcome,
            },
        )
        .await?;
    }
    let result = turn.result().ok_or("missing turn result")?.clone();
    let report = json!({"model_calls":model_calls,"tool_calls":tool_calls,"final_text":final_text,"result":result});
    let path = root.join("conversation-result.json");
    tokio::task::spawn_blocking(move || write_new(&path, &serde_json::to_vec_pretty(&report)?))
        .await??;
    if result.reason != StopReason::AgentFinished
        || model_calls < 2
        || tool_calls == 0
        || !final_text.contains(nonce)
    {
        return Err(
            "model did not complete the file/tool/follow-up smoke; evidence retained".into(),
        );
    }
    tokio::task::spawn_blocking(move || turn.finish()).await??;
    Ok(client)
}

async fn cancellation(root: &Path, mut client: Client) -> Result<()> {
    let request = playtime_engine::ModelRequest {
        request_id: playtime_core::RequestId::new("cancel-smoke")?,
        call: 1,
        purpose: playtime_core::RequestPurpose::Work,
        sampling_seed: 1042,
        maximum_tokens: 2048,
        messages: vec![
            json!({"role":"user","content":"Enumerate the integers from 1 through 500, one per line. Continue until 500."}),
        ],
        tools: vec![],
        context_input_bound: 512,
        context_basis: "bounded smoke".into(),
        context_reserve: 256,
    };
    let (tx, mut rx) = strata::events();
    let cancel = CancellationToken::new();
    let future = client.complete(
        &request,
        Instant::now() + Duration::from_secs(90),
        &cancel,
        tx,
    );
    let (result, events) = tokio::join!(future, async {
        let mut events = vec![];
        while let Some(event) = rx.recv().await {
            if event.controller_chunk().is_some() {
                cancel.cancel();
            }
            events.push(event.data);
        }
        events
    });
    let report = json!({"completion":result.completion,"remote":result.remote,"partial":result.partial,"events":events});
    let path = root.join("cancel-result.json");
    tokio::task::spawn_blocking(move || write_new(&path, &serde_json::to_vec_pretty(&report)?))
        .await??;
    if result.remote != strata::RemoteState::Uncertain
        || !matches!(
            result.completion,
            Err(strata::Failure {
                kind: strata::FailureKind::Cancelled,
                ..
            })
        )
    {
        return Err("did not observe cancellation before terminal response".into());
    }
    Ok(())
}

async fn run(args: &[String]) -> Result<()> {
    let root = PathBuf::from(&args[4]);
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let lock: ModelLock = serde_json::from_slice(&fs::read(&args[2])?)?;
    write_new(&root.join("lock.json"), &serde_json::to_vec_pretty(&lock)?)?;
    let client = Client::new(
        Endpoint::parse(&args[1])?,
        lock.clone(),
        strata::Limits::default(),
    )?;
    let image_bytes = fs::read(&args[3])?;
    let image: Value = serde_json::from_slice(&image_bytes)?;
    let transport = Arc::new(Cli::resolve(
        Path::new("/usr/bin/docker"),
        Path::new("/var/run/docker.sock"),
        &root.join("docker-config"),
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
    let nonce = docker::Ownership::random_nonce()?;
    let owner = docker::Ownership {
        service: "strata-developer-smoke".into(),
        run: "tool-roundtrip".into(),
        grant: "explicit-local-smoke".into(),
        invocation: "one-turn".into(),
        nonce: nonce.clone(),
        daemon: engine.identity().clone(),
    };
    let mut sandbox = docker::Sandbox::new(
        ready,
        owner,
        docker::Limits::smoke(),
        docker::WorkDeadline::after(Duration::from_secs(240))?,
        Arc::new(Journal(root.clone())),
    )?;
    write_new(
        &root.join("seed.json"),
        &serde_json::to_vec_pretty(&json!({"/work/probe.txt":nonce}))?,
    )?;
    let result = conversation(
        &root,
        client,
        &mut sandbox,
        config(&lock, &image, &nonce)?,
        &nonce,
    )
    .await;
    let cleanup = sandbox
        .cleanup(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await;
    write_new(
        &root.join("workshop-cleanup.json"),
        &serde_json::to_vec_pretty(
            &json!({"cleanup":cleanup,"operations":sandbox.observations(),"proposals":sandbox.proposals(),"conversation":result.as_ref().map(|_|"passed").map_err(|e|e.to_string())}),
        )?,
    )?;
    if !cleanup.complete() {
        return Err("workshop residuals remain".into());
    }
    cancellation(&root, result?).await?;
    println!(
        "Tool round-trip and local cancellation passed; verify owned server idle/cleanup separately. Evidence: {}",
        root.display()
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: workshop-smoke LOOPBACK_ENDPOINT LOCK_JSON IMAGE_LOCK_JSON NEW_EVIDENCE_DIRECTORY".into());
    }
    run(&args).await
}
