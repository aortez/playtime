//! Production-service process gates. Normal uses simulated workshop effects and
//! an explicitly selected CPU supervisor; Docker uses real tools and systemd,
//! with scripted model replies or the live locked model.
use crate::{Result, TestRoot, isolated, run_command};
use playtime_client::{CheckpointIndexRead, Client, Observation, RetryPolicy};
use playtime_core::{RunId, StopReason};
use playtime_protocol::api::*;
use playtime_runner::ArchiveQueries;
use playtime_storage::StorageLimits;
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    process::Child,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;

/// The production backend a gate exercises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Simulation,
    DockerScripted,
    /// Live model at this endpoint, with real Docker.
    Live(&'static str),
}
impl Backend {
    pub fn docker(self) -> bool {
        self != Self::Simulation
    }
    pub fn recipe(self) -> &'static str {
        match self {
            Self::Simulation => "two-session",
            Self::DockerScripted => playtime_runner::docker::RECIPE,
            Self::Live(_) => playtime_runner::docker::LIVE_RECIPE,
        }
    }
    pub fn mode(self) -> &'static str {
        match self {
            Self::Simulation => "simulation",
            Self::DockerScripted => "scripted_smoke",
            Self::Live(_) => "live_smoke",
        }
    }
    /// Start a daemon on its own new root, ready for the CLI.
    pub async fn daemon(
        self,
        daemon: &Path,
        root: &TestRoot,
        bundle: &Path,
    ) -> Result<(Child, PathBuf, Client)> {
        let service = root.join("service");
        std::fs::DirBuilder::new().mode(0o700).create(&service)?;
        let mut command = isolated(daemon, root);
        match self {
            Self::Simulation => command.arg("--cpu-test-supervisor"),
            Self::DockerScripted => command.arg("--docker-scripted"),
            Self::Live(endpoint) => {
                command.args(["--docker-scripted", "--ollama-endpoint", endpoint])
            }
        };
        command
            .args(["--listen", "127.0.0.1:0", "--root"])
            .arg(&service)
            .arg("--bundle")
            .arg(bundle)
            .env("PATH", "/no-runtime-cargo");
        let mut child = command.spawn()?;
        let connection = service.join(".playtime/connection.json");
        let deadline = Instant::now() + Duration::from_secs(if self.docker() { 30 } else { 15 });
        let client = loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!("daemon failed readiness: {status}").into());
            }
            if let Ok(client) = Client::from_connection(&connection).await {
                break client;
            }
            if Instant::now() >= deadline {
                return Err("daemon readiness deadline".into());
            }
            sleep(Duration::from_millis(10)).await;
        };
        Ok((child, connection, client))
    }
}
pub struct NormalService {
    child: Child,
    _cleanup: WorkerCleanup,
    pub root: TestRoot,
    pub connection: PathBuf,
    pub client: Client,
    pub run: RunId,
    pub view: RunView,
    pub backend: Backend,
    pub artifacts: Vec<Artifact>,
}
impl NormalService {
    pub async fn start(cli: &Path, daemon: &Path, bundle: &Path) -> Result<Self> {
        Self::start_profile(cli, daemon, bundle, Backend::Simulation).await
    }
    pub async fn start_docker(cli: &Path, daemon: &Path, bundle: &Path) -> Result<Self> {
        Self::start_profile(cli, daemon, bundle, Backend::DockerScripted).await
    }
    pub async fn start_live(
        cli: &Path,
        daemon: &Path,
        bundle: &Path,
        endpoint: &'static str,
    ) -> Result<Self> {
        Self::start_profile(cli, daemon, bundle, Backend::Live(endpoint)).await
    }
    async fn start_profile(
        cli: &Path,
        daemon: &Path,
        bundle: &Path,
        backend: Backend,
    ) -> Result<Self> {
        let docker = backend.docker();
        let directory = std::env::temp_dir().join(format!(
            "pt20-{}",
            &playtime_local::Token::generate()?.expose()[..12]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let root = TestRoot(directory);
        if docker {
            std::fs::write(
                root.join(".retain-evidence"),
                b"Incomplete Docker gate: retain original ownership records",
            )?;
        }
        let cleanup = WorkerCleanup(root.join("service"));
        let owned_bundle = root.join("service-bundle");
        if docker {
            copy(bundle, &owned_bundle)?;
        }
        let selected_bundle = if docker {
            owned_bundle.as_path()
        } else {
            bundle
        };
        let (child, connection, client) = backend.daemon(daemon, &root, selected_bundle).await?;
        if docker {
            std::fs::remove_dir_all(&owned_bundle)?;
        }
        let mut records = Vec::new();
        let launched = run_command(
            cli,
            &root,
            Some(&connection),
            [
                "run",
                backend.recipe(),
                "--mode",
                backend.mode(),
                "--idempotency-key",
                "normal-service-gate",
                "--format",
                "json",
            ],
            &mut records,
        )
        .await?;
        launched.expect(0)?;
        let key = IdempotencyKey::new("normal-service-gate")?;
        let acceptance = client.lookup_key(&key).await?;
        let run = acceptance.run_id.clone();
        // A real HTTP POST replay while the original may still be preparing
        // must retain one worker, not merely reuse a client-side receipt.
        let pending = client.prepare_launch(
            LaunchRequest {
                recipe_id: acceptance.run.recipe_id.clone(),
                mode: acceptance.run.mode,
                preview_revision: acceptance.run.preview_revision.clone(),
            },
            Some(key),
        )?;
        let replay = client.launch(&pending).await?;
        if !replay.replayed || replay.run_id != run {
            return Err("in-flight launch replay changed ownership".into());
        }
        // Construct the cleanup guard before any assertions against a worker.
        let mut this = Self {
            child,
            _cleanup: cleanup,
            root,
            connection,
            client,
            run,
            view: acceptance.run,
            backend,
            artifacts: vec![],
        };
        if !docker {
            this.verify(cli, &mut records).await?;
        }
        Ok(this)
    }
    /// While this run's worker holds the machine-wide live-model lock, a live
    /// admission through another daemon root, as from another checkout, gets
    /// Busy before any worker starts.
    pub async fn second_live_admission(
        &self,
        cli: &Path,
        daemon: &Path,
        bundle: &Path,
    ) -> Result<()> {
        // The worker takes the lock before Begin; anchors appear only after Begin.
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.client.run(&self.run).await?.event_anchors.is_none() {
            if Instant::now() >= deadline {
                return Err("live worker did not begin".into());
            }
            sleep(Duration::from_millis(50)).await;
        }
        let root = TestRoot::new()?;
        let (mut child, connection, _) = self.backend.daemon(daemon, &root, bundle).await?;
        let refused = run_command(
            cli,
            &root,
            Some(&connection),
            [
                "run",
                self.backend.recipe(),
                "--mode",
                self.backend.mode(),
                "--idempotency-key",
                "other-checkout",
                "--format",
                "json",
            ],
            &mut Vec::new(),
        )
        .await?;
        child.kill().await?;
        child.wait().await?;
        let output =
            String::from_utf8_lossy(&refused.stdout) + String::from_utf8_lossy(&refused.stderr);
        if refused.code == 0 || !output.contains("run_busy") {
            return Err(format!("second live admission was not refused as busy: {output}").into());
        }
        Ok(())
    }
    /// Copy another run's archive from this service, as evidence.
    pub fn retain_run(&self, run: &RunId, destination: &Path) -> Result<()> {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        copy(
            &self.root.join("service/runs").join(run.as_str()),
            destination,
        )
    }
    pub async fn await_completion(&mut self, cli: &Path) -> Result<()> {
        self.verify(cli, &mut Vec::new()).await
    }
    async fn verify(&mut self, cli: &Path, records: &mut Vec<serde_json::Value>) -> Result<()> {
        if self.view.backend.kind != BackendKind::Controller {
            return Err("service substituted fixture output".into());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let initial = loop {
            let view = self.client.run(&self.run).await?;
            if view.event_anchors.is_some() {
                break view;
            }
            if Instant::now() >= deadline {
                return Err("first worker confirmation deadline".into());
            }
            sleep(Duration::from_millis(10)).await;
        };
        let anchors = initial.event_anchors.unwrap();
        let mut observed = BTreeMap::new();
        let mut stream = self.client.follow(
            &self.run,
            Some(anchors.history_cursor.clone()),
            RetryPolicy::default(),
        );
        let cancel = CancellationToken::new();
        tokio::time::timeout(
            Duration::from_secs(match self.backend {
                Backend::Simulation => 25,
                Backend::DockerScripted => 180,
                Backend::Live(_) => 300,
            }),
            async {
                loop {
                    match stream.next(&cancel).await? {
                        Observation::Record(record) => {
                            observed.insert(record.event.sequence.get(), record.event.clone());
                            stream.acknowledge(&record)?;
                        }
                        Observation::End(end) => {
                            if end.status != StreamEndStatus::CommittedCompletion {
                                return Err("unexpected stream end".into());
                            }
                            break;
                        }
                        _ => (),
                    }
                }
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            },
        )
        .await
        .map_err(|_| "service event deadline")??;
        self.view = self.client.run(&self.run).await?;
        let usage = self.view.usage.clone().ok_or("missing usage")?;
        // Live usage varies, and a recovered tool-parse error leaves an
        // unknown charge, so only scripted runs have exact accounting.
        let exact = match self.backend {
            Backend::Simulation => Some(26),
            Backend::DockerScripted => Some(160),
            Backend::Live(_) => None,
        };
        if self.view.orchestration.state != OrchestrationState::Terminal
            || self.view.outcome.as_ref().map(|o| o.reason) != Some(StopReason::AgentFinished)
            || usage.known_generated_tokens == 0
            || exact.is_some_and(|known| {
                usage.known_generated_tokens != known
                    || usage.unknown_requests != 0
                    || usage.unknown_reserved_tokens != 0
                    || !usage.usage_complete
            })
            || self
                .view
                .accounting_coverage
                .as_ref()
                .is_none_or(|c| c.unsettled_allowance != 0)
        {
            return Err("normal terminal/accounting projection mismatch".into());
        }
        let final_frontier = self
            .view
            .last_confirmed_frontier
            .as_ref()
            .ok_or("missing final frontier")?;
        if observed.len() as u64 != final_frontier.prefix.sequence {
            return Err("SSE history/live coverage gap".into());
        }
        // Old anchors remain valid after the marker has advanced.
        let old = self
            .client
            .history(
                &self.run,
                &PageQuery {
                    cursor: Some(anchors.history_cursor),
                    limit: Some(1000),
                },
            )
            .await?;
        if old.frontier != anchors.frontier {
            return Err("historical cursor advanced".into());
        }
        let mut cursor = self
            .view
            .event_anchors
            .as_ref()
            .map(|a| a.history_cursor.clone());
        let mut history = Vec::new();
        loop {
            let page = self
                .client
                .history(
                    &self.run,
                    &PageQuery {
                        cursor,
                        limit: Some(7),
                    },
                )
                .await?;
            history.extend(page.events);
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }
        if history != observed.into_values().collect::<Vec<_>>() {
            return Err("paged/SSE evidence differs".into());
        }
        let checkpoints = self
            .client
            .checkpoints(&self.run, &PageQuery::default())
            .await?;
        if checkpoints.items.len() != 2 {
            return Err("expected two actual controller sessions".into());
        }
        for checkpoint in &checkpoints.items {
            let CheckpointIndexRead::Supported { index, .. } = self
                .client
                .checkpoint_index(&self.run, checkpoint, &cancel)
                .await?
            else {
                return Err("missing exact checkpoint index".into());
            };
            if index.session_id.as_str() == "s2"
                && (index.incoming_memory.status != "loaded"
                    || (self.backend == Backend::Simulation
                        && (index.incoming_evidence.is_none()
                            || index.outgoing_evidence.is_none())))
            {
                return Err("second session provenance missing".into());
            }
        }
        let mut cursor = None;
        let mut artifacts = Vec::new();
        loop {
            let page = self
                .client
                .artifacts(
                    &self.run,
                    &PageQuery {
                        cursor,
                        limit: Some(3),
                    },
                )
                .await?;
            artifacts.extend(page.items);
            if page.next_cursor.is_none() {
                break;
            }
            cursor = page.next_cursor;
        }
        let saved = artifacts
            .iter()
            .find(|a| {
                a.kind == "file_version"
                    && a.name
                        == if self.backend.docker() {
                            "result.txt"
                        } else {
                            "persistent.txt"
                        }
            })
            .ok_or("saved file absent")?;
        let mut bytes = Vec::new();
        self.client
            .download(&self.run, saved, &mut bytes, &cancel)
            .await?;
        // A live model may end the file with a newline.
        let saved = match self.backend {
            Backend::Simulation => bytes.as_slice(),
            Backend::DockerScripted => bytes.as_slice(),
            Backend::Live(_) => bytes.trim_ascii(),
        };
        if saved
            != if self.backend.docker() {
                b"persistence-ok".as_slice()
            } else {
                b"survives".as_slice()
            }
        {
            return Err("downloaded saved file differs".into());
        }
        for artifact in &artifacts {
            let mut bytes = Vec::new();
            self.client
                .download(&self.run, artifact, &mut bytes, &cancel)
                .await?;
            // Live images vary; the model was asked only for a 32x32 PNG.
            if matches!(self.backend, Backend::Live(_))
                && artifact.name == "probe.png"
                && (bytes.get(..8) != Some(b"\x89PNG\r\n\x1a\n".as_slice())
                    || bytes.get(16..24) != Some([0, 0, 0, 32, 0, 0, 0, 32].as_slice()))
            {
                return Err("live probe.png is not a 32x32 PNG".into());
            }
            if self.backend == Backend::DockerScripted
                && artifact.name == "probe.png"
                && (bytes.len() != 102
                    || playtime_storage::sha256(&bytes).as_str()
                        != "20647f03edd42b248b8e05de3ca738bd1a1bb1cdc59b101126be8d35d63dd9e1")
            {
                return Err(
                    "deterministic PNG differs from independently pinned locked-Pillow reference"
                        .into(),
                );
            }
            if playtime_storage::sha256(&bytes) != artifact.content.sha256
                || bytes.len() as u64 != artifact.content.size_bytes
            {
                return Err("artifact verification failed".into());
            }
        }
        let response = run_command(
            cli,
            &self.root,
            Some(&self.connection),
            ["runs", "show", self.run.as_str(), "--format", "json"],
            records,
        )
        .await?;
        response.expect(0)?;
        // A same-key replay remains the original admission after completion.
        let replay = run_command(
            cli,
            &self.root,
            Some(&self.connection),
            [
                "run",
                self.backend.recipe(),
                "--mode",
                self.backend.mode(),
                "--idempotency-key",
                "normal-service-gate",
                "--format",
                "json",
            ],
            records,
        )
        .await?;
        replay.expect(0)?;
        if self.client.runs(&PageQuery::default()).await?.items.len() != 1 {
            return Err("replay created another run".into());
        }
        let source = self.root.join("service/runs").join(self.run.as_str());
        let copied = self.root.join("copied-archive");
        copy(&source, &copied)?;
        let offline = ArchiveQueries::offline(&copied, StorageLimits::default())?;
        let actual = offline.run()?;
        if actual.usage != self.view.usage.clone().ok_or("missing usage")?
            || actual.frontier.committed_sequence != final_frontier.prefix.sequence
            || actual.frontier.prefix_sha256 != final_frontier.prefix.sha256
            || offline.checkpoints()?.len() != 2
        {
            return Err("independent copied archive disagrees".into());
        }
        // Exact raw JSONL preimage, independently of HTTP reserialization.
        let raw = std::fs::read(copied.join("events.jsonl"))?;
        if playtime_storage::sha256(&raw) != final_frontier.prefix.sha256
            || raw.len() as u64 != final_frontier.prefix.byte_offset
        {
            return Err("copied exact prefix hash differs".into());
        }
        for checkpoint in offline.checkpoints()? {
            let index = checkpoint.index.ok_or("offline index absent")?;
            let outgoing = offline.content(&index.outgoing_memory.content)?;
            if matches!(self.backend, Backend::Live(_)) {
                if outgoing.is_empty() {
                    return Err("live session saved no memento".into());
                }
            } else if outgoing
                != if self.backend == Backend::DockerScripted {
                if index.ordinal.get() == 1 {
                    "Inspect probe.txt, probe.png and the stopped counter in session two."
                } else {
                    "Two sessions complete: result.txt is persistence-ok; probe.png is the retained 32x32 image."
                }
                } else if index.ordinal.get() == 1 {
                    "Read persistent.txt in the next session."
                } else {
                    "The two-session fixture is complete."
                }
                .as_bytes()
            {
                return Err("exact memory mismatch".into());
            }
            let source_map: serde_json::Value =
                serde_json::from_slice(&offline.content(&index.source_map)?)?;
            for entry in source_map["entries"]
                .as_array()
                .ok_or("invalid source map")?
            {
                let sequence = entry["archive_sequence"]
                    .as_u64()
                    .ok_or("invalid sequence")?;
                let line = raw
                    .split_inclusive(|b| *b == b'\n')
                    .nth(sequence as usize - 1)
                    .ok_or("missing source record")?;
                if playtime_storage::sha256(line).as_str()
                    != entry["record_sha256"].as_str().ok_or("invalid digest")?
                {
                    return Err("source-map hash mismatch".into());
                }
            }
        }
        if self.backend == Backend::DockerScripted
            && !history.iter().any(|e| {
                e.kind == "controller_observation"
                    && e.data["observation_kind"] == "tool_result"
                    && e.data["data"]["value"]["result"]["timed_out"] == true
                    && e.data["data"]["value"]["result"]["exit_code"] == -9
            })
        {
            return Err("confirmed bridge timeout not present".into());
        }
        if self.backend.docker() {
            fresh_second_session(&history)?;
            self.verify_docker_absence().await?;
        }
        if let Backend::Live(_) = self.backend {
            live_streaming_and_timing(&history, &self.view)?;
        }
        self.artifacts = artifacts.clone();
        println!(
            "{}",
            json!({"case":match self.backend {
                Backend::Simulation => "normal",
                Backend::DockerScripted => "run-and-view",
                Backend::Live(_) => "live-run-and-view",
            },"run_id":self.run,"frontier":final_frontier,"sessions":2,"known_tokens":usage.known_generated_tokens,"unknown_reserved_tokens":usage.unknown_reserved_tokens,"verified_artifacts":artifacts.len(),"copied_archive":true,"supervision":if self.backend.docker() { "real user systemd" } else { "CPU test; no systemd enforcement" }})
        );
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        std::fs::remove_dir_all(self.root.join("service"))?;
        let offline =
            ArchiveQueries::offline(self.root.join("copied-archive"), StorageLimits::default())?;
        if Some(offline.run()?.usage) != self.view.usage {
            return Err("folder-only inspection failed after original service root removal".into());
        }
        if self.backend.docker() {
            std::fs::remove_file(self.root.join(".retain-evidence"))?;
        }
        Ok(())
    }
    pub fn retain_artifact_evidence(&self, destination: &Path) -> Result<()> {
        std::fs::create_dir_all(destination)?;
        copy(
            &self.root.join("copied-archive"),
            &destination.join("archive"),
        )?;
        std::fs::write(
            destination.join("artifacts.json"),
            serde_json::to_vec_pretty(&self.artifacts)?,
        )?;
        std::fs::write(
            destination.join("run.json"),
            serde_json::to_vec_pretty(&self.view)?,
        )?;
        if self.backend.docker() {
            std::fs::copy(
                self.root.join("docker-absence.json"),
                destination.join("docker-absence.json"),
            )?;
        }
        Ok(())
    }
    async fn verify_docker_absence(&self) -> Result<()> {
        use playtime_docker::{
            Engine,
            transport::{Cli, LocalExit, Operation, Transport},
        };
        let workers = self.root.join("service/.playtime/workers");
        let private = std::fs::read_dir(&workers)?
            .next()
            .ok_or("missing worker ownership")??
            .path();
        let mut proposals = Vec::new();
        for entry in std::fs::read_dir(&private)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("docker-")
                && name.ends_with(".json")
                && name != "docker-cleanup.json"
            {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(entry.path())?)?;
                proposals.push(serde_json::from_value::<playtime_docker::Proposal>(
                    value["proposal"].clone(),
                )?);
            }
        }
        if proposals.len() != 4 {
            return Err("expected volume, holder and exactly two agent proposals".into());
        }
        let cli = std::sync::Arc::new(Cli::resolve(
            Path::new("/usr/bin/docker"),
            Path::new("/var/run/docker.sock"),
            &self.root.join("docker-independent-inspection"),
        )?);
        let deadline = Instant::now() + Duration::from_secs(20);
        let engine = Engine::connect(cli.clone(), deadline, CancellationToken::new()).await?;
        let mut evidence = Vec::new();
        for proposal in proposals {
            if &proposal.ownership.daemon != engine.identity() {
                return Err("inspection daemon differs from proposed owner".into());
            }
            let volume = proposal.role == playtime_docker::Role::Volume;
            let operation = if volume {
                Operation::VolumeInspect(proposal.name.clone())
            } else {
                Operation::ContainerInspect(proposal.name.clone())
            };
            let result = cli
                .execute(operation, deadline, CancellationToken::new())
                .await;
            let diagnostic = String::from_utf8(result.stderr)?.to_ascii_lowercase();
            if result.fault.is_some()
                || result.local
                    != (LocalExit::Reaped {
                        code: Some(1),
                        signal: None,
                    })
                || serde_json::from_slice::<serde_json::Value>(&result.stdout).ok()
                    != Some(json!([]))
                || !(diagnostic.contains(if volume {
                    "no such volume"
                } else {
                    "no such container"
                }) || (!volume && diagnostic.contains("no such object")))
            {
                return Err("exact proposed resource absence not independently confirmed".into());
            }
            evidence.push(json!({"proposal":proposal,"independently_confirmed_absent":true}));
        }
        let after = Engine::connect(cli, deadline, CancellationToken::new()).await?;
        if after.identity() != engine.identity() {
            return Err("daemon changed during independent absence verification".into());
        }
        std::fs::write(
            self.root.join("docker-absence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        Ok(())
    }
}
/// Session two's first request is a new conversation that carries the memento
/// actually delivered to it. This was a fixture check in the production actor.
fn fresh_second_session(history: &[playtime_protocol::events::EventEnvelope]) -> Result<()> {
    let value = |kind: &str, accept: &dyn Fn(&serde_json::Value) -> bool| {
        history
            .iter()
            .filter(|e| e.kind == "controller_observation" && e.data["observation_kind"] == kind)
            .map(|e| &e.data["data"]["value"])
            .find(|v| accept(v))
            .cloned()
            .ok_or_else(|| format!("missing {kind}"))
    };
    let delivered = value("memory_received", &|v| v["ordinal"] == 2)?;
    let memento = delivered["memory"]["content"].as_str().unwrap_or_default();
    let request = value("model_request", &|v| {
        v["messages"].as_array().is_some_and(|m| {
            m.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Session 2/2."))
            })
        })
    })?;
    let messages = request["messages"].as_array().ok_or("invalid request")?;
    if memento.is_empty()
        || messages.len() != 2
        || messages
            .iter()
            .any(|m| !matches!(m["role"].as_str(), Some("system" | "user")))
        || !messages[1]["content"]
            .as_str()
            .is_some_and(|text| text.ends_with(memento))
    {
        return Err("second session lacked a fresh conversation and the delivered memento".into());
    }
    Ok(())
}
/// Every live response follows streamed chunks, and any request that generated
/// for 1.5 seconds or more streamed one at least 300 ms earlier. Batches commit
/// every 250 ms, so that chunk was readable before the response. Every request
/// recorded timing, with no sign of contention.
fn live_streaming_and_timing(
    history: &[playtime_protocol::events::EventEnvelope],
    view: &RunView,
) -> Result<()> {
    let observations = |kind: &'static str| {
        history.iter().filter(move |e| {
            e.kind == "controller_observation" && e.data["observation_kind"] == kind
        })
    };
    let value = |e: &playtime_protocol::events::EventEnvelope| e.data["data"]["value"].clone();
    let elapsed = |e: &playtime_protocol::events::EventEnvelope| e.data["elapsed_ms"].as_u64();
    let (mut first_chunk, mut generated) = (BTreeMap::new(), BTreeMap::new());
    for e in observations("model_chunk") {
        let chunk = value(e);
        if chunk["chunk"]["observation"] == "timing" {
            generated.insert(
                chunk["call"].as_u64(),
                chunk["chunk"]["eval_duration"].as_u64(),
            );
        } else {
            first_chunk
                .entry(chunk["call"].as_u64())
                .or_insert(elapsed(e));
        }
    }
    let mut long = 0;
    for e in observations("model_response") {
        let call = value(e)["call"].as_u64();
        let first = first_chunk
            .get(&call)
            .copied()
            .flatten()
            .ok_or("a live response had no streamed chunk")?;
        if generated.get(&call).copied().flatten().unwrap_or(0) >= 1_500_000_000 {
            long += 1;
            if elapsed(e).unwrap_or(0) < first + 300 {
                return Err("a long live response showed no partial output first".into());
            }
        }
    }
    println!("Live streaming: {long} responses generated for 1.5 seconds or more");
    let contention = view
        .contention
        .as_ref()
        .ok_or("live run recorded no request timing")?;
    if contention.timed_requests != observations("model_request").count() as u64 {
        return Err("a live request recorded no timing".into());
    }
    if contention.possible {
        return Err(format!("possible GPU contention during the live run: {contention:?}").into());
    }
    Ok(())
}
struct WorkerCleanup(PathBuf);
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        // Only invocation files under this owned test root; authenticate kernel
        // start ticks before signaling. The production supervisor never uses this.
        let Ok(entries) = std::fs::read_dir(self.0.join(".playtime/workers")) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path().join("cpu-test-invocation.json")) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(pid) = value["pid"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let start = stat
                .rsplit_once(')')
                .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                .and_then(|s| s.parse::<u64>().ok());
            if start == value["start_ticks"].as_u64()
                && let Some(pid) = rustix::process::Pid::from_raw(pid as i32)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}
fn copy(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            return Err("unexpected archive link".into());
        }
    }
    Ok(())
}
