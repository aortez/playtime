//! Explicit native preparation/worker gate, not a catalog or HTTP launch route.
use playtime_core::{RequestPurpose, RunId, StopReason};
use playtime_docker::{CancellationToken, Engine, Proposal, transport::Cli};
use playtime_protocol::{
    bridge::FileContent,
    compaction::EvidenceEntry,
    worker::{self, Binding, Command, WorkerKind},
};
use playtime_runner::{
    ArchiveQueries,
    docker::{ContinuousProfile, LiveHost, LiveModel},
};
use playtime_server::lifecycle::{PreparedLaunch, ports::Supervisor, systemd::SystemdSupervisor};
use playtime_storage::ArchiveReader;
use playtime_worker::bundle::VerifiedBundle;
use playtime_worker_control::{
    Bootstrap, Client, Credential, Grant, boot_id, boot_ms, create_private_dir, random_id,
    write_once,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
#[path = "../../playtime-runner/tests/common/registered.rs"]
mod registered;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn live_model(endpoint: &str, root: &Path) -> Result<LiveModel> {
    // Explicit reviewed identity, not a fixture backend. The real worker checks
    // its metadata before every call. The service registration API is step C.
    let registered = registered::qwen();
    let id = &registered.summary.identity;
    let address = playtime_ollama::Endpoint::parse(endpoint)?;
    let host = LiveHost {
        endpoint: endpoint.into(),
        addresses: vec![
            address
                .ip_literal()
                .ok_or("native smoke endpoint requires an IP literal")?,
        ],
        live_lock: root.to_path_buf(),
    };
    Ok(LiveModel {
        lock: json!({"name":id.name,"digest":id.digest,"ollama_version":id.backend_version,
            "definition_sha256":id.definition_sha256,"definition":registered.definition,"modelfile":""}),
        settings: serde_json::to_value(playtime_ollama::EffectiveSettings {
            context_tokens: Some(16384),
            thinking: Some(playtime_ollama::Thinking::Boolean(false)),
            ..Default::default()
        })?,
        host,
        registration: Some(registered.summary),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if !(3..=4).contains(&args.len()) {
        return Err("usage: continuous-native-e2e BUNDLE EVIDENCE_ROOT [OLLAMA_ENDPOINT]".into());
    }
    let root = PathBuf::from(&args[2]);
    for directory in [
        root.clone(),
        root.join("runs"),
        root.join("private"),
        root.join("executables"),
        root.join(".playtime"),
    ] {
        create_private_dir(&directory)?;
    }
    let cli = Cli::resolve(
        Path::new("/usr/bin/docker"),
        Path::new("/var/run/docker.sock"),
        &root.join("docker-config"),
    )?;
    let selection = cli.selection().clone();
    let engine = Engine::connect(
        Arc::new(cli),
        Instant::now() + Duration::from_secs(20),
        CancellationToken::new(),
    )
    .await?;
    let live = if let Some(endpoint) = args.get(3) {
        let runtime = PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join("playtime");
        create_private_dir(&runtime)?;
        Some(live_model(endpoint, &runtime.join("live-model.lock"))?)
    } else {
        None
    };
    let profile = ContinuousProfile::freeze(selection, engine.identity().clone(), live)?;
    profile.validate()?;
    let run_id = RunId::new(format!("continuous-{}", &random_id()?.as_str()[..12]))?;
    let archive = root.join("runs").join(run_id.as_str());
    let prepared = VerifiedBundle::open(Path::new(&args[1]))?.prepare_continuous(
        &archive,
        run_id.clone(),
        &profile,
    )?;
    let reader = ArchiveReader::open(&archive, prepared.limits.clone())?;
    let bundle = VerifiedBundle::from_archive(&reader, &reader.offline_snapshot()?)?;
    let executable = bundle.materialize(&root.join("executables"), &run_id)?;
    let invocation = random_id()?;
    let binding = Binding {
        service_id: random_id()?,
        run_id,
        manifest_sha256: prepared.identity.input_manifest.sha256.clone(),
        executable_sha256: reader.manifest().worker.executable.sha256.clone(),
        grant_id: random_id()?,
        worker_id: random_id()?,
        unit: format!("playtime-{}.service", invocation.as_str()),
        invocation_id: invocation,
        boot_id: boot_id()?,
        kind: if profile.model.is_some() {
            WorkerKind::DockerLive
        } else {
            WorkerKind::DockerScripted
        },
    };
    let bootstrap = Bootstrap {
        version: worker::VERSION,
        binding: binding.clone(),
        credential: Credential::generate()?,
        root: root.clone(),
        private_dir: root.join("private"),
        socket: root.join("private/control.sock"),
        startup_deadline_boot_ms: boot_ms() + 60000,
        wall_ms: profile.config.parameters.limits.wall_seconds.get() * 1000,
    };
    let bootstrap_path = root.join("private/bootstrap.json");
    write_once(&bootstrap_path, &serde_json::to_vec(&bootstrap)?)?;
    write_once(
        &root.join("private/reservation.json"),
        &serde_json::to_vec(&binding)?,
    )?;
    let launch = PreparedLaunch {
        binding: binding.clone(),
        executable,
        argv: vec![
            "--bootstrap".into(),
            bootstrap_path.to_str().unwrap().into(),
        ],
        bootstrap: bootstrap_path,
        private_dir: bootstrap.private_dir.clone(),
        socket: bootstrap.socket.clone(),
        startup_deadline_boot_ms: bootstrap.startup_deadline_boot_ms,
    };
    let supervisor = SystemdSupervisor::connect().await?;
    supervisor.ensure_legacy_idle(&root).await?;
    supervisor.start(&launch).await?;
    let result = exercise(&root, &archive, &profile, &launch, &bootstrap, &supervisor).await;
    // Stop only this authenticated original invocation; retain all evidence.
    let observed = supervisor
        .observe(&launch)
        .await?
        .ok_or("missing systemd observation")?;
    if !observed.stopped {
        supervisor.stop(&launch, &observed).await?;
    }
    let deadline = Instant::now() + Duration::from_secs(190);
    loop {
        if let Some(observed) = supervisor.observe(&launch).await?
            && observed.stopped
        {
            write_once(
                &root.join("supervisor.json"),
                &serde_json::to_vec_pretty(&observed)?,
            )?;
            break;
        }
        if Instant::now() >= deadline {
            return Err("original worker did not stop".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut absent = Vec::new();
    for entry in std::fs::read_dir(&bootstrap.private_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("docker-") && name != "docker-cleanup.json" && name.ends_with(".json") {
            let record: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            if record["binding"] != serde_json::to_value(&binding)? {
                return Err("ownership binding changed".into());
            }
            let proposal: Proposal = serde_json::from_value(record["proposal"].clone())?;
            if engine
                .residual(
                    &proposal,
                    Instant::now() + Duration::from_secs(10),
                    CancellationToken::new(),
                )
                .await?
                .is_some()
            {
                return Err("owned resource remains after finalization".into());
            }
            absent.push(proposal.name);
        }
    }
    write_once(
        &root.join("absent.json"),
        &serde_json::to_vec_pretty(&absent)?,
    )?;
    result?;
    if absent.len() != 3 {
        return Err("expected one volume, holder and run-scoped agent".into());
    }
    println!("PASS continuous native gate: {}", root.display());
    Ok(())
}

async fn exercise(
    root: &Path,
    archive: &Path,
    profile: &ContinuousProfile,
    launch: &PreparedLaunch,
    bootstrap: &Bootstrap,
    supervisor: &SystemdSupervisor,
) -> Result<()> {
    let endpoint = loop {
        if let Ok(endpoint) = supervisor.locate(launch).await {
            break endpoint;
        }
        if boot_ms() >= bootstrap.startup_deadline_boot_ms {
            return Err("worker readiness deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let client = Client::new(endpoint, bootstrap.credential.clone());
    loop {
        if let Ok(reply) = client.request(Command::Status).await
            && reply.status.latest_receipt.is_some()
        {
            break;
        }
        if boot_ms() >= bootstrap.startup_deadline_boot_ms {
            return Err("prepared worker deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Grant::new(bootstrap.private_dir.clone(), bootstrap.binding.clone()).authorize()?;
    let begun = client.request(Command::Begin).await?.status;
    let deadline = Instant::now() + Duration::from_millis(bootstrap.wall_ms + 180000);
    let terminal = loop {
        let status = client.request(Command::Status).await?.status;
        if status.work_started_boot_ms != begun.work_started_boot_ms {
            return Err("worker restarted".into());
        }
        if !status.publication_pending {
            break status;
        }
        if Instant::now() >= deadline {
            return Err("continuous run exceeded finalization deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    write_once(
        &root.join("terminal-status.json"),
        &serde_json::to_vec_pretty(&terminal)?,
    )?;
    // The completed driver has released its writer. Offline inspection is
    // explicitly recovered read evidence, never a manufactured writer receipt.
    let queries = ArchiveQueries::offline(archive, profile.storage.clone())?;
    let frontier = queries.snapshot().frontier();
    let receipt = terminal
        .latest_receipt
        .as_ref()
        .ok_or("missing final receipt")?;
    if (
        frontier.committed_sequence,
        frontier.committed_bytes,
        &frontier.prefix_sha256,
    ) != (receipt.sequence, receipt.bytes, &receipt.sha256)
    {
        return Err("offline evidence exceeds authenticated final prefix".into());
    }
    let run = queries.run()?;
    write_once(&root.join("run.json"), &serde_json::to_vec_pretty(&run)?)?;
    let totals = queries.request_accounting()?;
    write_once(
        &root.join("accounting.json"),
        &serde_json::to_vec_pretty(&totals)?,
    )?;
    let mut after = 0;
    let mut evidence = Vec::new();
    loop {
        let page = queries.compaction_history(after, 100)?;
        evidence.extend(page.entries);
        if !page.has_more {
            break;
        }
        after = page.next_after_sequence;
    }
    write_once(
        &root.join("compaction.json"),
        &serde_json::to_vec_pretty(&evidence)?,
    )?;
    let applied = evidence
        .iter()
        .filter(
            |e| matches!(e, EvidenceEntry::Recorded { kind, .. } if kind == "compaction_applied"),
        )
        .count();
    let finished = run.outcome.as_ref().is_some_and(|outcome| {
        outcome.reason == StopReason::AgentFinished
            && matches!(
                outcome.cleanup,
                playtime_protocol::events::CleanupOutcome::Complete
            )
    });
    if !finished || applied < 2 || run.usage.unknown_requests != 0 {
        return Err(format!(
            "continuous outcome failed: {:?}, {applied} compactions",
            run.outcome
        )
        .into());
    }
    if totals.by_purpose[&RequestPurpose::Compaction].settled_calls != applied as u64 {
        return Err("compaction accounting mismatch".into());
    }
    let captures = queries.captures()?;
    if !queries.checkpoints()?.is_empty() || captures.len() != 1 {
        return Err("wrong final capture/session count".into());
    }
    let result = captures[0]
        .index
        .files
        .iter()
        .find(|f| f.path == "result.txt")
        .ok_or("missing continuity result")?;
    let FileContent::Retained { content } = &result.content else {
        return Err("continuity result not retained".into());
    };
    if queries.content(content)? != b"continuity-ok\n" {
        return Err("wrong continuity result".into());
    }
    println!(
        "{}: {applied} compactions, {} known tokens, final capture verified",
        run.run_id, run.usage.known_generated_tokens
    );
    Ok(())
}
