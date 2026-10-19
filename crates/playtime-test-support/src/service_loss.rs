//! Explicit real-Docker/systemd loss probe. Its disposable admission remains
//! reserved, including after harness cleanup; this implements no recovery policy.
use crate::{Result, TestRoot, isolated, run_command};
use playtime_client::Client;
use playtime_docker::{
    CancellationToken, Engine, Proposal, Role,
    transport::{Cli, LocalExit, Operation, Transport},
};
use playtime_protocol::api::{IdempotencyKey, OrchestrationState, PageQuery};
use playtime_server::lifecycle::{PreparedLaunch, ports::Supervisor, systemd::SystemdSupervisor};
use playtime_worker_control::{Bootstrap, systemd};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::time::{Instant, sleep};

pub async fn worker_loss(cli: &Path, daemon: &Path, bundle: &Path, evidence: &Path) -> Result<()> {
    let path = std::env::temp_dir().join(format!(
        "p66-{}",
        &playtime_local::Token::generate()?.expose()[..8]
    ));
    playtime_worker_control::create_private_dir(&path)?;
    let root = TestRoot(path);
    std::fs::write(
        root.join(".retain-evidence"),
        b"Worker-loss admission remains reserved. Test cleanup is not recovery or release.",
    )?;
    let service = root.join("service");
    playtime_worker_control::create_private_dir(&service)?;
    let mut command = isolated(daemon, &root);
    command
        .args(["--listen", "127.0.0.1:0", "--docker-scripted", "--root"])
        .arg(&service)
        .arg("--bundle")
        .arg(bundle)
        .env("PATH", "/no-runtime-cargo");
    let mut child = command.spawn()?;
    let connection = service.join(".playtime/connection.json");
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut client = loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("loss daemon exited: {status}").into());
        }
        if let Ok(c) = Client::from_connection(&connection).await {
            break c;
        }
        if Instant::now() >= deadline {
            return Err("loss daemon readiness deadline".into());
        }
        sleep(Duration::from_millis(5)).await;
    };
    let args = [
        "run",
        playtime_runner::docker::RECIPE,
        "--mode",
        "scripted_smoke",
        "--idempotency-key",
        "worker-loss-gate",
        "--format",
        "json",
    ];
    let mut records = vec![];
    run_command(cli, &root, Some(&connection), args, &mut records)
        .await?
        .expect(0)?;
    let accepted = client
        .lookup_key(&IdempotencyKey::new("worker-loss-gate")?)
        .await?;
    let private = service
        .join(".playtime/workers")
        .join(accepted.run_id.as_str());
    let log = service
        .join("runs")
        .join(accepted.run_id.as_str())
        .join("events.jsonl");
    // A returned first tool result establishes that all volume/holder/agent
    // creates completed. Kill well before another session/create is possible.
    loop {
        let page = client
            .history(
                &accepted.run_id,
                &PageQuery {
                    limit: Some(1000),
                    ..Default::default()
                },
            )
            .await;
        if page.is_ok_and(|page| {
            page.events.iter().any(|event| {
                let v = serde_json::to_value(event).expect("wire event");
                v["kind"] == "controller_observation"
                    && v["data"]["observation_kind"] == "tool_result"
                    && v["data"]["data"]["value"]["name"] == "exec"
            })
        }) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("worker never reached first confirmed tool result".into());
        }
        sleep(Duration::from_millis(2)).await;
    }
    let bootstrap: Bootstrap = serde_json::from_slice(&playtime_worker_control::read_private(
        &private.join("bootstrap.json"),
        65536,
    )?)?;
    let invocation = systemd::read_invocation(&private)?;
    if invocation.binding != bootstrap.binding
        || systemd::process_start(invocation.pid)? != invocation.start_ticks
    {
        return Err("original worker identity changed before loss probe".into());
    }
    let pid = rustix::process::Pid::from_raw(invocation.pid as i32).ok_or("invalid owned PID")?;
    let held = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
    if systemd::process_start(invocation.pid)? != invocation.start_ticks {
        return Err("worker changed while opening pidfd".into());
    }
    rustix::process::pidfd_send_signal(&held, rustix::process::Signal::KILL)?;
    let launch = PreparedLaunch {
        binding: bootstrap.binding.clone(),
        executable: private.join("playtime-worker"),
        argv: vec![
            "--bootstrap".into(),
            private
                .join("bootstrap.json")
                .to_string_lossy()
                .into_owned(),
        ],
        bootstrap: private.join("bootstrap.json"),
        private_dir: private.clone(),
        socket: bootstrap.socket.clone(),
        startup_deadline_boot_ms: bootstrap.startup_deadline_boot_ms,
    };
    let supervisor = SystemdSupervisor::connect().await?;
    let stopped = loop {
        if let Ok(Some(observed)) = supervisor.observe(&launch).await
            && observed.stopped
        {
            break observed;
        }
        if Instant::now() >= deadline {
            return Err("original systemd worker termination not established".into());
        }
        sleep(Duration::from_millis(10)).await;
    };
    let view = loop {
        let view = client.run(&accepted.run_id).await?;
        if view.orchestration.state == OrchestrationState::Reconciling {
            break view;
        }
        if Instant::now() >= deadline {
            return Err("worker loss was not kept Reconciling".into());
        }
        sleep(Duration::from_millis(25)).await;
    };
    // Restart inspection may restore saved confirmation, but must not launch,
    // finalize or release an owner that was lost. Recovery is a later slice.
    child.kill().await?;
    child.wait().await?;
    let mut restart = isolated(daemon, &root);
    restart
        .args(["--listen", "127.0.0.1:0", "--docker-scripted", "--root"])
        .arg(&service)
        .arg("--bundle")
        .arg(bundle)
        .env("PATH", "/no-runtime-cargo");
    child = restart.spawn()?;
    client = loop {
        if let Some(status) = child.try_wait()? {
            return Err(format!("restarted daemon exited: {status}").into());
        }
        if let Ok(c) = Client::from_connection(&connection).await {
            break c;
        }
        if Instant::now() >= deadline {
            return Err("restart inspection deadline".into());
        }
        sleep(Duration::from_millis(10)).await;
    };
    if client.run(&accepted.run_id).await?.orchestration.state != OrchestrationState::Reconciling {
        return Err("daemon restart released or resumed lost worker".into());
    }
    let before = std::fs::read(&log)?;
    run_command(cli, &root, Some(&connection), args, &mut records)
        .await?
        .expect(0)?;
    let rejected = run_command(
        cli,
        &root,
        Some(&connection),
        [
            "run",
            playtime_runner::docker::RECIPE,
            "--mode",
            "scripted_smoke",
            "--idempotency-key",
            "must-remain-blocked",
            "--format",
            "json",
        ],
        &mut records,
    )
    .await?;
    if rejected.code == 0 || !String::from_utf8_lossy(&rejected.stderr).contains("busy") {
        return Err("worker loss allowed new admission or returned an unrelated refusal".into());
    }
    sleep(Duration::from_millis(300)).await;
    if std::fs::read(&log)? != before
        || client.runs(&PageQuery::default()).await?.items.len() != 1
        || std::fs::read_dir(service.join(".playtime/workers"))?.count() != 1
    {
        return Err("worker loss replayed execution or created a replacement".into());
    }
    let mut proposals = Vec::new();
    for entry in std::fs::read_dir(&private)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("docker-")
            && entry.path().extension().is_some_and(|s| s == "json")
        {
            let record: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            if let Some(p) = record.get("proposal") {
                proposals.push(serde_json::from_value::<Proposal>(p.clone())?);
            }
        }
    }
    if proposals.len() != 3 {
        return Err("loss probe missed its first-session ownership window".into());
    }
    let cleanup = fixture_cleanup(&root, &proposals).await?;
    // The service has no recovery join: even independent test cleanup and
    // original-worker termination must not release its admission.
    let still = client.run(&accepted.run_id).await?;
    if still.orchestration.state != OrchestrationState::Reconciling {
        return Err("test cleanup released admission".into());
    }
    std::fs::create_dir_all(evidence)?;
    std::fs::write(
        evidence.join("worker-loss.json"),
        serde_json::to_vec_pretty(&json!({"case":"worker-loss",
        "run":view,"original_worker_stopped":stopped,"proposals":proposals,"test_cleanup":cleanup,
        "no_replacement_or_new_admission":true,"daemon_restart_remains_reserved":true,"reserved_service_root":service}))?,
    )?;
    child.kill().await?;
    child.wait().await?;
    println!(
        "Worker-loss gate: original invocation terminated; no replay or new admission; reserved ledger retained at {}",
        service.display()
    );
    Ok(())
}

/// Test teardown only, after proven original-worker death and acknowledged
/// first-session creates. It neither writes archive evidence nor releases the
/// retained admission. Every remove binds an exact inspected proposal identity.
async fn fixture_cleanup(root: &TestRoot, proposals: &[Proposal]) -> Result<Vec<Value>> {
    let cli = Arc::new(Cli::resolve(
        Path::new("/usr/bin/docker"),
        Path::new("/var/run/docker.sock"),
        &root.join("test-cleanup-config"),
    )?);
    let deadline = Instant::now() + Duration::from_secs(20);
    let engine = Engine::connect(cli.clone(), deadline, CancellationToken::new()).await?;
    let mut result = vec![];
    for role in [Role::Agent, Role::Holder, Role::Volume] {
        for p in proposals.iter().filter(|p| p.role == role) {
            if &p.ownership.daemon != engine.identity() {
                return Err("test cleanup daemon identity changed".into());
            }
            let volume = role == Role::Volume;
            let inspect = if volume {
                Operation::VolumeInspect(p.name.clone())
            } else {
                Operation::ContainerInspect(p.name.clone())
            };
            let observed = cli
                .execute(inspect.clone(), deadline, CancellationToken::new())
                .await;
            if !observed.success() {
                return Err("test cleanup could not establish exact resource identity".into());
            }
            let value: Value = serde_json::from_slice(&observed.stdout)?;
            let v = value.get(0).ok_or("missing exact inspection")?;
            if volume {
                if v["Name"] != p.name
                    || v["Labels"] != serde_json::to_value(p.labels())?
                    || v["Driver"] != "local"
                {
                    return Err("foreign volume refused by test cleanup".into());
                }
            } else if v["Name"] != format!("/{}", p.name)
                || v["Config"]["Labels"] != serde_json::to_value(p.labels())?
                || v["Image"] != p.image
                || v["Mounts"][0]["Name"] != p.volume
            {
                return Err("foreign container refused by test cleanup".into());
            }
            let remove = if volume {
                Operation::RemoveVolume(p.name.clone())
            } else {
                Operation::RemoveContainer(v["Id"].as_str().ok_or("missing exact ID")?.into())
            };
            if !cli
                .execute(remove, deadline, CancellationToken::new())
                .await
                .success()
            {
                return Err("test cleanup remove uncertain".into());
            }
            let absent = cli
                .execute(inspect, deadline, CancellationToken::new())
                .await;
            if absent.fault.is_some()
                || absent.local
                    != (LocalExit::Reaped {
                        code: Some(1),
                        signal: None,
                    })
                || serde_json::from_slice::<Value>(&absent.stdout).ok() != Some(json!([]))
                || !String::from_utf8_lossy(&absent.stderr)
                    .to_ascii_lowercase()
                    .contains("no such")
            {
                return Err("test cleanup absence uncertain".into());
            }
            result.push(json!({"name":p.name,"role":role,"confirmed_absent":true}));
        }
    }
    if Engine::connect(cli, deadline, CancellationToken::new())
        .await?
        .identity()
        != engine.identity()
    {
        return Err("test cleanup final daemon identity changed".into());
    }
    Ok(result)
}
