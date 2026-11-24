//! The Strata hold gate: the real daemon, with Docker but without a model or
//! worker, drives a scripted chat launcher package through a Strata target's
//! previews, admission and refusals. The package carries the reviewed
//! Framework lock. The daemon refuses every host a hold lends, so no worker
//! starts and no host is reached.
use crate::{Captured, Result, STRATA_LAUNCHER, TestRoot, isolated, run_command};
use playtime_client::Client;
use playtime_core::{RunId, RunMode};
use playtime_protocol::api::*;
use serde_json::{Value, json};
use std::{
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    process::Child,
    time::{Instant, sleep},
};

#[path = "../../playtime-strata/tests/support/mod.rs"]
mod strata_fixture;
use strata_fixture::server::Server;

const LOCK: &str = "framework-strata-gyro-m-fast-v2";
const PROFILE: &str = "scripted-gyro-m";
const TARGET: &str = "scripted-strata";
const LIVE: &str = playtime_runner::docker::LIVE_RECIPE;
const RECIPE: &str = "curiosity";

/// Runs the gate with the catalog in `recipes`, then stops the daemon as
/// systemd would. A failed gate keeps its root, with the daemon's stderr.
pub async fn run(cli: &Path, daemon: &Path, bundle: &Path, recipes: &Path) -> Result<()> {
    let root = TestRoot::new()?;
    std::fs::write(
        root.join(".retain-evidence"),
        b"Incomplete Strata hold gate: retain the daemon's records",
    )?;
    let package = root.join("launcher");
    std::fs::DirBuilder::new().mode(0o700).create(&package)?;
    std::fs::write(package.join("run-strata.py"), STRATA_LAUNCHER)?;
    std::fs::write(
        package.join(format!("{LOCK}.lock.json")),
        include_bytes!(
            "../../playtime-runner/src/docker/framework-strata-gyro-m-fast-v2-lock.json"
        ),
    )?;
    // A loopback Strata server with the fixture's model, not the lock's.
    let server = Server::new([]);
    script(&package, &server, |_| {})?;
    let (mut child, connection, client) = serve(daemon, &root, &package, bundle, recipes).await?;
    let mut gate = Gate {
        cli: cli.into(),
        root,
        package,
        server,
        connection,
        client: client.with_target(Some(TARGET.into()))?,
        records: Vec::new(),
    };
    let checked = gate.check().await;
    let stopped = stop(&mut child, &gate.root).await;
    checked?;
    stopped?;
    std::fs::remove_file(gate.root.join(".retain-evidence"))?;
    println!(
        "{}",
        json!({"case": "strata-holds", "commands": gate.records.len(), "holds": 4,
            "supervision": "no worker; scripted launcher"})
    );
    Ok(())
}

/// Writes the package's `fixture.json`: a host the launcher would start,
/// whose hold lends `server`, edited by `change`.
fn script(package: &Path, server: &Server, change: impl FnOnce(&mut Value)) -> Result<()> {
    let lock = playtime_runner::docker::strata_lock(LOCK)?;
    let mut script = json!({
        "describe": {"schema_version": 1, "protocol": "hold_v1", "profile": PROFILE,
            "model_lock": format!("{LOCK}.lock.json"), "startup_timeout": 30,
            "verify_timeout": 30, "cleanup_timeout": 5},
        "status": {"state": "starts_on_launch"},
        "hold": {"first": "held", "endpoint": server.endpoint,
            "model_lock": format!("{LOCK}.lock.json"),
            "model_lock_sha256": playtime_runner::docker::strata_lock_sha256(LOCK)?,
            "report": {"host": lock.host}},
    });
    change(&mut script);
    std::fs::write(package.join("fixture.json"), script.to_string())?;
    Ok(())
}

async fn serve(
    daemon: &Path,
    root: &TestRoot,
    package: &Path,
    bundle: &Path,
    recipes: &Path,
) -> Result<(Child, PathBuf, Client)> {
    let service = root.join("service");
    std::fs::DirBuilder::new().mode(0o700).create(&service)?;
    let targets = root.join("targets.json");
    std::fs::write(
        &targets,
        serde_json::to_vec(&json!({"schema_version": 1, "targets": [{"id": TARGET,
            "name": "Scripted Strata", "provider": "strata", "profile": PROFILE}]}))?,
    )?;
    let mut command = isolated(daemon, root);
    command
        .args(["--docker-scripted", "--targets"])
        .arg(&targets)
        .arg("--strata-launcher")
        .arg(package)
        .arg("--recipes")
        .arg(recipes)
        .args(["--listen", "127.0.0.1:0", "--root"])
        .arg(&service)
        .arg("--bundle")
        .arg(bundle)
        .env("PATH", "/no-runtime-cargo")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(root.join("daemon-stderr.log"))?);
    let mut child = command.spawn()?;
    let connection = service.join(".playtime/connection.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("daemon failed readiness: {status}").into());
        }
        if let Ok(client) = Client::from_connection(&connection).await {
            return Ok((child, connection, client));
        }
        if Instant::now() >= deadline {
            return Err("daemon readiness deadline".into());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Stops the daemon with SIGTERM, then checks that it lent no host and that
/// every hold it took ended.
async fn stop(child: &mut Child, root: &TestRoot) -> Result<()> {
    let pid = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32))
        .ok_or("the daemon already exited")?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    if tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .is_err()
    {
        child.kill().await?;
        child.wait().await?;
        return Err("the daemon didn't stop within 2 minutes".into());
    }
    // Each hold in turn: no host lent, and every ending observed.
    let stderr = std::fs::read_to_string(root.join("daemon-stderr.log"))?;
    let holds: Vec<_> = stderr
        .lines()
        .filter(|line| line.contains("Strata hold of"))
        .collect();
    let closed = format!(
        "playtimed: Strata hold of {PROFILE} ended: closed, released false, inference owner confirmed"
    );
    let expected = [
        format!(
            "playtimed: Strata hold of {PROFILE} refused: busy: fixture refusal; inference owner not_acquired"
        ),
        format!(
            "playtimed: Strata hold of {PROFILE} refused by the daemon: its server isn't the lock's model"
        ),
        format!("playtimed: stopping the Strata hold of {PROFILE}"),
        closed.clone(),
        format!(
            "playtimed: Strata hold of {PROFILE} refused by the daemon: it verified another lock"
        ),
        format!("playtimed: stopping the Strata hold of {PROFILE}"),
        closed,
        format!("playtimed: abandoning the Strata hold of {PROFILE}: the run was cancelled"),
        format!("playtimed: Strata hold of {PROFILE} ended before use; inference owner confirmed"),
    ];
    ensure(holds == expected, || {
        format!("the daemon's holds went otherwise:\n{stderr}")
    })
}

struct Gate {
    cli: PathBuf,
    root: TestRoot,
    package: PathBuf,
    server: Server,
    connection: PathBuf,
    client: Client,
    records: Vec<Value>,
}
impl Gate {
    async fn check(&mut self) -> Result<()> {
        // A host the launcher would start is ready for the live smoke and a
        // catalog recipe. The recipe's sampling goes over the lock's other
        // settings, and no host is frozen before a hold lends one.
        let live = self.preview(LIVE, RunMode::LiveSmoke).await?;
        ensure(live.readiness.ready, || format!("{:?}", live.readiness))?;
        let recipe = self.preview(RECIPE, RunMode::Experiment).await?;
        ensure(recipe.readiness.ready, || format!("{:?}", recipe.readiness))?;
        let mut settings =
            serde_json::to_value(playtime_runner::docker::strata_lock(LOCK)?.settings)?;
        for (name, value) in [
            ("thinking", json!("high")),
            ("temperature", json!(0.8)),
            ("top_p", json!(0.95)),
            ("top_k", json!(40)),
            ("repetition_penalty", json!(1.0)),
        ] {
            settings[name] = value;
        }
        let model = &recipe.model_identity;
        ensure(
            model["adapter"] == "strata"
                && model["lock"] == LOCK
                && model["launcher_profile"] == PROFILE
                && model["settings"] == settings
                && recipe.effective_configuration["model"]["host"]["endpoint"] == ""
                && recipe.target.as_ref().is_some_and(|t| t.id == TARGET),
            || format!("the recipe's Strata preview: {model}"),
        )?;
        // A busy host isn't ready, and admission refuses it as busy. A host the
        // launcher can't hold for another reason is unavailable.
        self.script(|s| s["status"] = json!({"state": "busy", "message": "a chat holds it"}))?;
        self.not_ready("Strata host busy: a chat holds it").await?;
        self.refused("busy-host", "run_busy").await?;
        self.script(|s| s["status"] = json!({"state": "unreachable"}))?;
        self.not_ready("Strata host unreachable").await?;
        self.refused("unreachable-host", "model_unavailable")
            .await?;
        // A hold the launcher refuses fails the run's preparation.
        self.script(|s| {
            s["status"] = json!({"state": "ready"});
            s["hold"] = json!({"first": "refused", "reason": "busy"});
        })?;
        let run = self.accepted("refused-hold", LIVE, "live_smoke").await?;
        self.preparation_failed(&run).await?;
        // The daemon refuses a lent server that isn't the lock's model, and a
        // hold that verified another lock, and closes each hold.
        self.script(|_| {})?;
        let run = self.accepted("other-model", RECIPE, "experiment").await?;
        self.preparation_failed(&run).await?;
        self.script(|s| s["hold"]["model_lock_sha256"] = json!("0".repeat(64)))?;
        let run = self.accepted("other-lock", LIVE, "live_smoke").await?;
        self.preparation_failed(&run).await?;
        let endings: Vec<_> = self
            .calls()?
            .into_iter()
            .filter(|c| c["call"] == "ended")
            .map(|c| c["ending"].clone())
            .collect();
        ensure(endings == [json!("closed"), json!("closed")], || {
            format!("refused holds ended {endings:?}")
        })?;
        let inspected = self.server.state.lock().unwrap().seen.len();
        ensure(inspected > 0, || "no hold's server was inspected".into())?;
        // While a hold is pending, this daemon's host is busy. Cancelling the
        // run abandons the hold before it lends anything, and frees the host.
        self.script(|s| s["hold"] = json!({"first": "wait"}))?;
        let run = self
            .accepted("cancelled-hold", RECIPE, "experiment")
            .await?;
        self.wait_for(|calls| calls.iter().filter(|c| c["call"] == "hold").count() == 4)
            .await?;
        self.not_ready("Strata host still held for an earlier run")
            .await?;
        self.refused("while-held", "run_busy").await?;
        self.command(["runs", "cancel", run.as_str(), "--format", "json"])
            .await?
            .expect(0)?;
        self.wait_for(|calls| {
            calls
                .iter()
                .any(|c| c["call"] == "abandoned" && c["input"] == "")
        })
        .await?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self
            .preview(LIVE, RunMode::LiveSmoke)
            .await?
            .readiness
            .ready
        {
            if Instant::now() >= deadline {
                return Err("the abandoned hold didn't free the host".into());
            }
            sleep(Duration::from_millis(100)).await;
        }
        // The cancelled run never submitted a worker and waits for a release.
        self.release(&run).await
    }
    fn script(&self, change: impl FnOnce(&mut Value)) -> Result<()> {
        script(&self.package, &self.server, change)
    }
    async fn preview(&self, recipe: &str, mode: RunMode) -> Result<Preview> {
        Ok(self.client.preview(&RecipeId::new(recipe)?, mode).await?)
    }
    /// Neither the live smoke nor a recipe is ready, for this reason only.
    async fn not_ready(&self, reason: &str) -> Result<()> {
        for (recipe, mode) in [(LIVE, RunMode::LiveSmoke), (RECIPE, RunMode::Experiment)] {
            let preview = self.preview(recipe, mode).await?;
            ensure(
                !preview.readiness.ready && preview.readiness.reasons == [reason],
                || format!("{recipe}: expected {reason:?}, got {:?}", preview.readiness),
            )?;
        }
        Ok(())
    }
    async fn command<const N: usize>(&mut self, args: [&str; N]) -> Result<Captured> {
        run_command(
            &self.cli,
            &self.root,
            Some(&self.connection),
            args,
            &mut self.records,
        )
        .await
    }
    async fn launch(&mut self, key: &str, recipe: &str, mode: &str) -> Result<Captured> {
        self.command([
            "--target",
            TARGET,
            "run",
            recipe,
            "--mode",
            mode,
            "--idempotency-key",
            key,
            "--format",
            "json",
        ])
        .await
    }
    async fn refused(&mut self, key: &str, code: &str) -> Result<()> {
        let refused = self.launch(key, LIVE, "live_smoke").await?;
        let output =
            String::from_utf8_lossy(&refused.stdout) + String::from_utf8_lossy(&refused.stderr);
        ensure(refused.code != 0 && output.contains(code), || {
            format!("launch {key} wasn't refused with {code}: {output}")
        })
    }
    async fn accepted(&mut self, key: &str, recipe: &str, mode: &str) -> Result<RunId> {
        self.launch(key, recipe, mode).await?.expect(0)?;
        Ok(self
            .client
            .lookup_key(&IdempotencyKey::new(key)?)
            .await?
            .run_id)
    }
    /// The run's preparation failed before any worker, and a release ends it.
    async fn preparation_failed(&mut self, run: &RunId) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let view = self.client.run(run).await?;
            if view.orchestration.reason.as_deref() == Some("preparation_failed") {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("{run} stayed {:?}", view.orchestration).into());
            }
            sleep(Duration::from_millis(100)).await;
        }
        self.release(run).await
    }
    async fn release(&mut self, run: &RunId) -> Result<()> {
        self.command([
            "runs",
            "release",
            run.as_str(),
            "--reason",
            "Strata hold gate",
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        let view = self.client.run(run).await?;
        ensure(
            view.orchestration.state == OrchestrationState::Terminal
                && view.orchestration.reason.as_deref() == Some("operator_released"),
            || format!("{run} wasn't released: {:?}", view.orchestration),
        )
    }
    fn calls(&self) -> Result<Vec<Value>> {
        let calls = match std::fs::read_to_string(self.package.join("calls.jsonl")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            calls => calls?,
        };
        Ok(calls
            .lines()
            .map(serde_json::from_str)
            .collect::<serde_json::Result<_>>()?)
    }
    async fn wait_for(&self, done: impl Fn(&[Value]) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done(&self.calls()?) {
            if Instant::now() >= deadline {
                return Err(format!("the launcher recorded {:?}", self.calls()?).into());
            }
            sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

fn ensure(ok: bool, why: impl FnOnce() -> String) -> Result<()> {
    if ok { Ok(()) } else { Err(why().into()) }
}
