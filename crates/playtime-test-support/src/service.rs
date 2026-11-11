//! Production-service process gates. Normal uses simulated workshop effects and
//! an explicitly selected CPU supervisor; Docker uses real tools and systemd,
//! with scripted model replies or the live locked model. Lifecycle is the normal
//! gate with the daemon killed and restarted while the run's worker is paused.
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

#[path = "service_diagnostics.rs"]
mod diagnostics;

/// A cancelled run keeps the data root's slot until the operator releases it.
/// Cancel a first run through the shared client, which avoids the CLI's
/// process start-up before the cancel, then release it through the CLI.
async fn cancel_and_release(
    cli: &Path,
    root: &TestRoot,
    connection: &Path,
    client: &Client,
    records: &mut Vec<serde_json::Value>,
) -> Result<RunView> {
    let key = "released-service-gate";
    let launched = run_command(
        cli,
        root,
        Some(connection),
        [
            "run",
            "two-session",
            "--mode",
            "simulation",
            "--idempotency-key",
            key,
            "--format",
            "json",
        ],
        records,
    )
    .await?;
    launched.expect(0)?;
    let run = client.lookup_key(&IdempotencyKey::new(key)?).await?.run_id;
    // A cancelled run stops but stays reserved. --wait returns once the daemon
    // stops monitoring it, with exit 9 and the release command.
    let cancelled = run_command(
        cli,
        root,
        Some(connection),
        [
            "runs",
            "cancel",
            run.as_str(),
            "--wait",
            "--timeout",
            "30s",
            "--format",
            "json",
        ],
        records,
    )
    .await?;
    cancelled.expect(9)?;
    let result = cancelled.json()?;
    if result["data"]["next"].as_str()
        != Some(format!("playtime runs release {run} --reason TEXT").as_str())
    {
        return Err(format!("cancel --wait did not name the release: {result}").into());
    }
    let released = run_command(
        cli,
        root,
        Some(connection),
        [
            "runs",
            "release",
            run.as_str(),
            "--reason",
            "service gate: cancelled run",
            "--format",
            "json",
        ],
        records,
    )
    .await?;
    released.expect(0)?;
    let view = client.run(&run).await?;
    if view.orchestration.state != OrchestrationState::Terminal
        || view.orchestration.reason.as_deref() != Some("operator_released")
    {
        return Err("a released run must report an operator release".into());
    }
    Ok(view)
}

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
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("service"))?;
        self.serve(daemon, root, bundle, "127.0.0.1:0", false).await
    }
    /// Start a daemon on the root's existing service directory. A simulation
    /// uses the CPU test supervisor unless `systemd` selects the default one.
    async fn serve(
        self,
        daemon: &Path,
        root: &TestRoot,
        bundle: &Path,
        listen: &str,
        systemd: bool,
    ) -> Result<(Child, PathBuf, Client)> {
        let service = root.join("service");
        let mut command = isolated(daemon, root);
        match self {
            Self::Simulation if systemd => &mut command,
            Self::Simulation => command.arg("--cpu-test-supervisor"),
            Self::DockerScripted => command.arg("--docker-scripted"),
            Self::Live(endpoint) => {
                command.args(["--docker-scripted", "--ollama-endpoint", endpoint])
            }
        };
        command
            .args(["--listen", listen, "--root"])
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
    /// The cancelled run released before the normal launch.
    pub released: Option<RunView>,
    /// What the lifecycle gate saw across its daemon restart.
    pub restart: Option<serde_json::Value>,
    /// The simulation runs under user systemd, not the CPU test supervisor.
    systemd: bool,
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
        let released = if docker {
            None
        } else {
            Some(cancel_and_release(cli, &root, &connection, &client, &mut records).await?)
        };
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
                target: None,
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
            released,
            restart: None,
            systemd: false,
        };
        if !docker {
            this.verify(cli, &mut records).await?;
        }
        Ok(this)
    }
    /// The normal simulation, except that the daemon is killed while the run's
    /// worker is paused, then restarted on the same root, port and bundle before
    /// the worker resumes. The restarted daemon must follow that same worker to
    /// normal completion, without preparing or starting another (#59). With
    /// `systemd`, the workers are user systemd units, not CPU test children.
    pub async fn start_restarted(
        cli: &Path,
        daemon: &Path,
        bundle: &Path,
        systemd: bool,
    ) -> Result<Self> {
        let backend = Backend::Simulation;
        // Adopt the worker when its daemon dies. The kernel ends a paused
        // process whose group that death orphans (SIGHUP), so the gate keeps
        // the group attached by becoming the worker's parent.
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
        let directory = std::env::temp_dir().join(format!(
            "pt59-{}",
            &playtime_local::Token::generate()?.expose()[..12]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let root = TestRoot(directory);
        let cleanup = WorkerCleanup(root.join("service"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("service"))?;
        let (mut child, connection, client) = backend
            .serve(daemon, &root, bundle, "127.0.0.1:0", systemd)
            .await?;
        let mut records = Vec::new();
        run_command(
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
        .await?
        .expect(0)?;
        let key = IdempotencyKey::new("normal-service-gate")?;
        let run = client.lookup_key(&key).await?.run_id;
        let workers = root.join("service/.playtime/workers");
        let worker = PausedWorker::pause(&workers.join(run.as_str()), false, systemd).await?;
        // Paused before its control socket listened, the worker can't have had
        // its Begin, so only the restarted daemon can begin it. The CPU test
        // worker listens about 180 ms after its start is recorded. A systemd
        // worker records itself just before it listens, so under systemd the
        // gate reports whether it paused the worker in time.
        let before_listening =
            !PausedWorker::listening(&workers.join(run.as_str()).join("control.sock"));
        if !before_listening && !systemd {
            return Err("the worker listened before the gate paused it".into());
        }
        let (endpoint, _) = playtime_local::read_connection(&connection)?;
        let listen = endpoint
            .endpoint
            .strip_prefix("http://")
            .ok_or("unexpected daemon endpoint")?
            .to_owned();
        child.kill().await?;
        child.wait().await?;
        let (child, connection, restarted) = backend
            .serve(daemon, &root, bundle, &listen, systemd)
            .await?;
        // The client from before the restart reaches the new daemon, which
        // waits on the paused original worker rather than replacing it. The
        // run stays reconciling until that worker authenticates again, with
        // control being resolved, so clients don't ask for a release.
        let paused = client.run(&run).await?;
        if paused.orchestration.state != OrchestrationState::Reconciling
            || paused.orchestration.reason.as_deref() != Some("control_uncertain")
        {
            return Err(format!(
                "the restarted daemon isn't resolving the paused worker's run: {:?}",
                paused.orchestration
            )
            .into());
        }
        let acceptance = restarted.lookup_key(&key).await?;
        if acceptance.run_id != run {
            return Err("the launch key changed runs across the restart".into());
        }
        worker.resume()?;
        let mut this = Self {
            child,
            _cleanup: cleanup,
            root,
            connection,
            client: restarted,
            run,
            view: acceptance.run,
            backend,
            artifacts: vec![],
            released: None,
            restart: Some(json!({
                "worker_pid": worker.pid,
                "paused_before_listening": before_listening,
                "orchestration_while_paused": paused.orchestration,
            })),
            systemd,
        };
        this.verify(cli, &mut records).await?;
        worker.exited().await?;
        if worker.saved(&workers.join(this.run.as_str()))? != worker.pid {
            return Err("the run finished on a different worker".into());
        }
        if std::fs::read_dir(&workers)?.count() != 1 {
            return Err("the restarted daemon prepared another worker".into());
        }
        // Normal completion freed the slot without an operator release.
        run_command(
            cli,
            &this.root,
            Some(&this.connection),
            [
                "run",
                backend.recipe(),
                "--mode",
                backend.mode(),
                "--idempotency-key",
                "after-restart-service-gate",
                "--wait",
                "--format",
                "json",
            ],
            &mut records,
        )
        .await?
        .expect(0)?;
        this.cancel_across_restart(cli, daemon, bundle, &listen, &mut records)
            .await?;
        Ok(this)
    }
    /// A cancellation saved before the daemon dies is delivered again by the
    /// restarted daemon, which stops the original worker. The run then waits
    /// for a release, as any cancelled run does.
    async fn cancel_across_restart(
        &mut self,
        cli: &Path,
        daemon: &Path,
        bundle: &Path,
        listen: &str,
        records: &mut Vec<serde_json::Value>,
    ) -> Result<()> {
        let key = "cancelled-across-restart";
        run_command(
            cli,
            &self.root,
            Some(&self.connection),
            [
                "run",
                self.backend.recipe(),
                "--mode",
                self.backend.mode(),
                "--idempotency-key",
                key,
                "--format",
                "json",
            ],
            records,
        )
        .await?
        .expect(0)?;
        let run = self
            .client
            .lookup_key(&IdempotencyKey::new(key)?)
            .await?
            .run_id;
        let directory = self
            .root
            .join("service/.playtime/workers")
            .join(run.as_str());
        // Paused only once it listens, so control requests wait on it.
        let worker = PausedWorker::pause(&directory, true, self.systemd).await?;
        let (client, id) = (self.client.clone(), run.clone());
        let cancel = tokio::spawn(async move { client.cancel(&id).await });
        // Delivery to the paused worker waits out its five-second control
        // deadline before the daemon asks the supervisor to stop it, so this
        // daemon dies once its ledger holds the request, before that stop.
        let ledger = self.root.join("service/.playtime/service.sqlite3");
        let deadline = Instant::now() + Duration::from_secs(4);
        while !cancellation_saved(&ledger, &run)? {
            if Instant::now() >= deadline {
                return Err("the daemon did not save the cancellation".into());
            }
            sleep(Duration::from_millis(10)).await;
        }
        self.child.kill().await?;
        self.child.wait().await?;
        cancel.abort();
        let (child, connection, client) = self
            .backend
            .serve(daemon, &self.root, bundle, listen, self.systemd)
            .await?;
        (self.child, self.connection, self.client) = (child, connection, client);
        let saved = self.client.run(&run).await?.cancellation;
        if saved.as_ref().map(|c| c.enforcement) != Some(Enforcement::Requested) {
            return Err(format!("the dead daemon left this cancellation: {saved:?}").into());
        }
        // So only the restarted daemon can ask for this stop.
        let deadline = Instant::now() + Duration::from_secs(20);
        while self
            .client
            .run(&run)
            .await?
            .cancellation
            .map(|c| c.enforcement)
            != Some(Enforcement::StopAccepted)
        {
            if Instant::now() >= deadline {
                return Err("the restarted daemon did not resume the cancellation".into());
            }
            sleep(Duration::from_millis(50)).await;
        }
        // systemd's stop sends SIGCONT after SIGTERM, which resumes it.
        if !self.systemd {
            worker.resume()?;
        }
        worker.exited().await?;
        if worker.saved(&directory)? != worker.pid {
            return Err("the cancelled run had a different worker".into());
        }
        let waited = run_command(
            cli,
            &self.root,
            Some(&self.connection),
            [
                "runs",
                "wait",
                run.as_str(),
                "--timeout",
                "30s",
                "--format",
                "json",
            ],
            records,
        )
        .await?;
        waited.expect(9)?;
        run_command(
            cli,
            &self.root,
            Some(&self.connection),
            [
                "runs",
                "release",
                run.as_str(),
                "--reason",
                "lifecycle gate: cancelled across a daemon restart",
                "--format",
                "json",
            ],
            records,
        )
        .await?
        .expect(0)?;
        let view = self.client.run(&run).await?;
        if view.orchestration.state != OrchestrationState::Terminal
            || view.orchestration.reason.as_deref() != Some("operator_released")
            || view.cancellation.as_ref().map(|c| c.enforcement) != Some(Enforcement::Stopped)
        {
            return Err(format!("cancelled run ended wrongly: {view:?}").into());
        }
        println!(
            "{}",
            json!({"case":"lifecycle-cancel","run_id":run,"worker_pid":worker.pid,"orchestration":view.orchestration,"cancellation":view.cancellation})
        );
        Ok(())
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
        let diagnostics = json!({
            "run_id": self.run,
            "outcome": self.view.outcome,
            "usage": usage,
            "accounting_coverage": self.view.accounting_coverage,
            "observations": diagnostics::summarize(observed.values()),
        });
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
            return Err(
                format!("normal terminal/accounting projection mismatch: {diagnostics}").into(),
            );
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
            return Err(format!("expected two actual controller sessions: {diagnostics}").into());
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
            .ok_or_else(|| format!("saved file absent: {diagnostics}"))?;
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
            return Err(format!("downloaded saved file differs: {diagnostics}").into());
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
        let runs = self.client.runs(&PageQuery::default()).await?.items.len();
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
        if self.client.runs(&PageQuery::default()).await?.items.len() != runs {
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
        if self.backend == Backend::DockerScripted {
            // Starting and finishing never look like a run that needs a
            // release, so a waiting launch of a normal run returns 0 (#93).
            run_command(
                cli,
                &self.root,
                Some(&self.connection),
                [
                    "run",
                    self.backend.recipe(),
                    "--mode",
                    self.backend.mode(),
                    "--idempotency-key",
                    "waiting-service-gate",
                    "--wait",
                    "--format",
                    "json",
                ],
                records,
            )
            .await?
            .expect(0)?;
        }
        if let Backend::Live(_) = self.backend {
            live_streaming_and_timing(&history, &self.view)?;
        }
        self.artifacts = artifacts.clone();
        println!(
            "{}",
            json!({"case":match self.backend {
                Backend::Simulation if self.restart.is_some() => "lifecycle",
                Backend::Simulation => "normal",
                Backend::DockerScripted => "run-and-view",
                Backend::Live(_) => "live-run-and-view",
            },"run_id":self.run,"frontier":final_frontier,"sessions":2,"known_tokens":usage.known_generated_tokens,"unknown_reserved_tokens":usage.unknown_reserved_tokens,"verified_artifacts":artifacts.len(),"copied_archive":true,"supervision":if self.backend.docker() || self.systemd { "real user systemd" } else { "CPU test; no systemd enforcement" },"released":self.released.as_ref().map(|r| json!({"run_id":r.run_id,"orchestration":r.orchestration,"cancellation":r.cancellation})),"restart":self.restart})
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
            .filter(|e| {
                e.kind == "controller_observation"
                    && e.data["session_id"] == "s2"
                    && e.data["observation_kind"] == kind
            })
            .map(|e| &e.data["data"]["value"])
            .find(|v| accept(v))
            .cloned()
            .ok_or_else(|| format!("missing {kind}"))
    };
    let delivered = value("memory_received", &|v| v["ordinal"] == 2)?;
    let memento = delivered["memory"]["content"].as_str().unwrap_or_default();
    // Prompt text may describe both sessions. The archive's session identity
    // selects the first request; later requests must not hide a stale context.
    let request = value("model_request", &|_| true)?;
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
fn observations<'a>(
    history: &'a [playtime_protocol::events::EventEnvelope],
    kind: &'static str,
) -> impl Iterator<Item = &'a playtime_protocol::events::EventEnvelope> {
    history
        .iter()
        .filter(move |e| e.kind == "controller_observation" && e.data["observation_kind"] == kind)
}
fn live_streaming_and_timing(
    history: &[playtime_protocol::events::EventEnvelope],
    view: &RunView,
) -> Result<()> {
    let (long, calls_only) = long_responses_streamed(history)?;
    println!(
        "Live streaming: {long} responses with text generated for 1.5 seconds or more; \
         {calls_only} more held only tool calls"
    );
    let contention = view
        .contention
        .as_ref()
        .ok_or("live run recorded no request timing")?;
    if contention.timed_requests != observations(history, "model_request").count() as u64 {
        return Err("a live request recorded no timing".into());
    }
    if contention.possible {
        return Err(format!("possible GPU contention during the live run: {contention:?}").into());
    }
    Ok(())
}
/// Every response streamed a chunk, and each one generated for 1.5 seconds or
/// more showed text at least 300 ms before it ended. Returns how many long
/// responses streamed text and how many held only tool calls.
fn long_responses_streamed(
    history: &[playtime_protocol::events::EventEnvelope],
) -> Result<(usize, usize)> {
    let value = |e: &playtime_protocol::events::EventEnvelope| e.data["data"]["value"].clone();
    let elapsed = |e: &playtime_protocol::events::EventEnvelope| e.data["elapsed_ms"].as_u64();
    let (mut first_chunk, mut first_text, mut generated) =
        (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for e in observations(history, "model_chunk") {
        let chunk = value(e);
        let call = chunk["call"].as_u64();
        if chunk["chunk"]["observation"] == "timing" {
            generated.insert(call, chunk["chunk"]["eval_duration"].as_u64());
            continue;
        }
        first_chunk.entry(call).or_insert(elapsed(e));
        let message = &chunk["chunk"]["message"];
        if ["thinking", "content"]
            .iter()
            .any(|key| message[key].as_str().is_some_and(|t| !t.is_empty()))
        {
            first_text.entry(call).or_insert(elapsed(e));
        }
    }
    let (mut long, mut calls_only) = (0, 0);
    for e in observations(history, "model_response") {
        let call = value(e)["call"].as_u64();
        first_chunk
            .get(&call)
            .copied()
            .flatten()
            .ok_or("a live response had no streamed chunk")?;
        if generated.get(&call).copied().flatten().unwrap_or(0) < 1_500_000_000 {
            continue;
        }
        // Ollama sends a tool call whole once it parses, so a response with
        // no thinking or content has nothing to show early.
        let Some(first) = first_text.get(&call).copied() else {
            calls_only += 1;
            continue;
        };
        long += 1;
        let first = first.ok_or("a live chunk recorded no elapsed time")?;
        if elapsed(e).unwrap_or(0) < first + 300 {
            return Err("a long live response showed no partial output first".into());
        }
    }
    Ok((long, calls_only))
}
/// Whether the daemon's ledger holds the run's cancellation. A read-only
/// connection takes no write lock, so the daemon's own writes go on.
fn cancellation_saved(ledger: &Path, run: &RunId) -> Result<bool> {
    let connection =
        rusqlite::Connection::open_with_flags(ledger, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let record: String = connection.query_row(
        "SELECT record FROM runs WHERE id = ?1",
        [run.as_str()],
        |row| row.get(0),
    )?;
    let record: serde_json::Value = serde_json::from_str(&record)?;
    Ok(!record["cancel_requested"].is_null())
}
/// A simulation worker the harness paused through a pidfd, which names that
/// exact process.
struct PausedWorker {
    pid: u32,
    pidfd: rustix::fd::OwnedFd,
    /// Its invocation record, from the CPU test supervisor or under systemd.
    record: &'static str,
}
impl PausedWorker {
    /// Pause the worker as soon as its invocation is recorded or, with
    /// `listening`, once its control socket accepts connections.
    async fn pause(directory: &Path, listening: bool, systemd: bool) -> Result<Self> {
        let record = if systemd {
            playtime_worker_control::systemd::IDENTITY_FILE
        } else {
            "cpu-test-invocation.json"
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(pid) = Self::saved_pid(&directory.join(record))
                && (!listening || Self::listening(&directory.join("control.sock")))
            {
                let (pid, start) = pid;
                let pidfd = rustix::process::pidfd_open(
                    rustix::process::Pid::from_raw(i32::try_from(pid)?).ok_or("invalid PID")?,
                    rustix::process::PidfdFlags::empty(),
                )?;
                if playtime_worker_control::systemd::process_start(pid)? != start {
                    return Err("the worker exited before the harness paused it".into());
                }
                rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::STOP)?;
                // The signal lands asynchronously; the kernel reports when it has.
                let stopped = Instant::now() + Duration::from_secs(5);
                while !Self::stopped(pid)? {
                    if Instant::now() >= stopped {
                        return Err("the worker did not stop".into());
                    }
                    sleep(Duration::from_millis(1)).await;
                }
                return Ok(Self { pid, pidfd, record });
            }
            if Instant::now() >= deadline {
                return Err("worker start deadline".into());
            }
            sleep(Duration::from_millis(1)).await;
        }
    }
    /// Whether the kernel reports this process stopped by a signal.
    fn stopped(pid: u32) -> Result<bool> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, tail)| tail.split_whitespace().next());
        Ok(state == Some("T"))
    }
    /// A listening Unix socket has the accepting flag in the kernel's table.
    fn listening(socket: &Path) -> bool {
        let Ok(socket) = std::fs::canonicalize(socket) else {
            return false;
        };
        std::fs::read_to_string("/proc/net/unix").is_ok_and(|table| {
            table.lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.len() == 8 && fields[3] == "00010000" && Path::new(fields[7]) == socket
            })
        })
    }
    fn saved_pid(record: &Path) -> Result<(u32, u64)> {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(record)?)?;
        Ok((
            u32::try_from(value["pid"].as_u64().ok_or("missing PID")?)?,
            value["start_ticks"].as_u64().ok_or("missing start")?,
        ))
    }
    /// The PID recorded for the worker of this directory.
    fn saved(&self, directory: &Path) -> Result<u32> {
        Ok(Self::saved_pid(&directory.join(self.record))?.0)
    }
    fn resume(&self) -> Result<()> {
        rustix::process::pidfd_send_signal(&self.pidfd, rustix::process::Signal::CONT)?;
        Ok(())
    }
    /// The pidfd turns readable once this exact process exits.
    async fn exited(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut fds = [rustix::event::PollFd::new(
                &self.pidfd,
                rustix::event::PollFlags::IN,
            )];
            let now = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if rustix::event::poll(&mut fds, Some(&now))? == 1 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("the original worker did not exit after completion".into());
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}
struct WorkerCleanup(PathBuf);
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        // Only invocation files under this owned test root; authenticate kernel
        // start ticks before signaling. The production supervisor never uses this.
        let Ok(entries) = std::fs::read_dir(self.0.join(".playtime/workers")) else {
            return;
        };
        let records = [
            "cpu-test-invocation.json",
            playtime_worker_control::systemd::IDENTITY_FILE,
        ];
        for entry in entries.flatten() {
            let Some(bytes) = records
                .iter()
                .find_map(|record| std::fs::read(entry.path().join(record)).ok())
            else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_conversation_uses_session_identity_and_checks_its_first_request() {
        let event = |sequence, session, kind, value| {
            serde_json::from_value(json!({
                "schema_version": 1, "run_id": "fresh-session-test", "sequence": sequence,
                "time": "2026-10-23T00:00:00Z", "kind": "controller_observation",
                "data": {"session_id": session, "observation_kind": kind,
                    "data": {"storage": "inline", "value": value}}
            }))
            .unwrap()
        };
        let mut history = vec![
            event(
                1,
                "s1",
                "model_request",
                json!({"messages": [
                    {"role": "system", "content": "protocol"},
                    {"role": "user", "content": "Instructions for Session 2/2. also appear here."}
                ]}),
            ),
            event(
                2,
                "s2",
                "memory_received",
                json!({"ordinal": 2,
                "memory": {"content": "verify existing files"}}),
            ),
            event(
                3,
                "s2",
                "model_request",
                json!({"messages": [
                    {"role": "system", "content": "protocol"},
                    {"role": "user", "content": "Session 2/2.\nverify existing files"}
                ]}),
            ),
        ];
        fresh_second_session(&history).unwrap();
        // A later clean-looking request must not hide a stale first conversation.
        history.push(history[2].clone());
        history[2].data["data"]["value"]["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "old private context"}));
        assert!(fresh_second_session(&history).is_err());
        history[2] = history[3].clone();
        history[2].data["data"]["value"]["messages"][1]["content"] = json!("wrong memento");
        assert!(fresh_second_session(&history).is_err());
    }

    #[test]
    fn long_responses_show_text_early_unless_they_hold_only_tool_calls() {
        let event = |sequence, kind, elapsed: u64, value| {
            serde_json::from_value(json!({
                "schema_version": 1, "run_id": "streaming-test", "sequence": sequence,
                "time": "2026-10-30T00:00:00Z", "kind": "controller_observation",
                "data": {"session_id": "s1", "observation_kind": kind, "elapsed_ms": elapsed,
                    "data": {"storage": "inline", "value": value}}
            }))
            .unwrap()
        };
        // One call: a chunk at `chunk_ms`, its response at 2000 ms and the
        // server's generation time.
        let call = |history: &mut Vec<_>, call: u64, message, chunk_ms, eval_ns: u64| {
            let next = history.len() as u64 + 1;
            history.push(event(
                next,
                "model_chunk",
                chunk_ms,
                json!({"call": call, "chunk": {"message": message, "done": false, "records": 3}}),
            ));
            history.push(event(
                next + 1,
                "model_response",
                2000,
                json!({"call": call}),
            ));
            history.push(event(
                next + 2,
                "model_chunk",
                2000,
                json!({"call": call, "chunk": {"observation": "timing", "eval_duration": eval_ns}}),
            ));
        };
        let thinking = json!({"role": "assistant", "thinking": "Reading the notes"});
        let tool_call = json!({"role": "assistant",
            "tool_calls": [{"function": {"name": "shell", "arguments": {"command": "ls"}}}]});
        let mut history = Vec::new();
        call(&mut history, 1, thinking.clone(), 100, 1_600_000_000);
        // Ollama sends a parsed tool call whole, just before the response.
        call(&mut history, 2, tool_call, 1950, 1_520_000_000);
        // A short response needn't show anything early.
        call(&mut history, 3, thinking.clone(), 1990, 400_000_000);
        assert_eq!(long_responses_streamed(&history).unwrap(), (1, 1));
        let mut late = history.clone();
        call(&mut late, 4, thinking.clone(), 1900, 1_600_000_000);
        assert!(long_responses_streamed(&late).is_err());
        let mut silent = history;
        silent.push(event(99, "model_response", 2000, json!({"call": 5})));
        assert!(long_responses_streamed(&silent).is_err());
    }
}
