//! Opt-in real user-manager tests. No shortened startup/stop policy: these take
//! about three minutes, use unique nonce units, and require systemd >=250/cgroup v2.
use super::*;
use async_trait::async_trait;
use playtime_server::lifecycle::systemd::SystemdSupervisor;
use std::time::Instant;

struct FaultArchive {
    inner: FixtureArchive,
    ignore_stop: bool,
}
#[async_trait]
impl ArchivePort for FaultArchive {
    async fn validate_preview(&self, request: &AdmissionRequest) -> Result<Selection> {
        self.inner.validate_preview(request).await
    }
    async fn prepare(&self, admission: &Admission) -> Result<PreparedLaunch> {
        let launch = self.inner.prepare(admission).await?;
        if self.ignore_stop {
            write_once(
                &launch.private_dir.join("ignore-stop.fixture"),
                b"owned systemd fault fixture",
            )?;
        }
        Ok(launch)
    }
    async fn verify_status(
        &self,
        launch: &PreparedLaunch,
        prior: Option<&playtime_protocol::worker::Receipt>,
        status: &playtime_protocol::worker::Status,
    ) -> Result<()> {
        self.inner.verify_status(launch, prior, status).await
    }
}
struct Cleanup {
    root: Option<Root>,
    launch: Option<PreparedLaunch>,
    completed: bool,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.completed
            && let Some(launch) = &self.launch
        {
            fixture::cleanup_systemd_unit(&launch.binding.unit, true);
        }
        if !self.completed
            && let Some(launch) = &self.launch
        {
            // Test failure still requests the original uniquely named unit's
            // stop. Keep its bootstrap/executable until systemd's bound elapses.
            fixture::cleanup_systemd_unit(&launch.binding.unit, false);
            if let Some(root) = self.root.take() {
                std::mem::forget(root);
            }
        }
    }
}
async fn setup(
    wall_ms: u64,
    ignore_stop: bool,
) -> (Cleanup, Coordinator, Arc<SystemdSupervisor>, Admission) {
    let supervisor = Arc::new(
        SystemdSupervisor::connect()
            .await
            .expect("real user systemd/cgroup v2 required; no fixture fallback"),
    );
    let root = Root::new();
    let inner = FixtureArchive::new(
        &root.0,
        &std::env::current_exe().unwrap(),
        vec![
            "--exact".into(),
            "fixture_worker_process".into(),
            "--ignored".into(),
            "--nocapture".into(),
            "--skip".into(),
            "$HOME $(not-a-command); %n * \"quoted\"".into(),
        ],
    )
    .unwrap()
    .with_wall_ms(wall_ms);
    let revision = inner.preview_revision();
    let archive = Arc::new(FaultArchive { inner, ignore_stop });
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    let accepted = c
        .admit(
            c.ledger().service_id().clone(),
            "systemd".into(),
            AdmissionRequest {
                preview_revision: revision,
                ..request()
            },
        )
        .await
        .unwrap()
        .admission;
    let mut cleanup = Cleanup {
        root: Some(root),
        launch: None,
        completed: false,
    };
    c.start(accepted.run_id.clone()).await.unwrap();
    cleanup.launch = c.ledger().get(&accepted.run_id).await.unwrap().prepared;
    let ready = prepared(&c, &accepted).await;
    (cleanup, c, supervisor, ready)
}
async fn stopped(
    supervisor: &SystemdSupervisor,
    launch: &PreparedLaunch,
    limit: Duration,
) -> SupervisorObservation {
    tokio::time::timeout(limit, async {
        loop {
            if let Ok(Some(observation)) = supervisor.observe(launch).await
                && observation.stopped
                && observation.exit.is_some()
            {
                return observation;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("original systemd invocation must stop within its independent bound")
}
#[tokio::test]
#[ignore = "real user systemd; 60-second startup deadline"]
async fn systemd_startup_deadline_survives_coordinator_exit_without_begin() {
    let (mut cleanup, c, supervisor, ready) = setup(30_000, false).await;
    let launch = ready.prepared.unwrap();
    let observed = supervisor.observe(&launch).await.unwrap().unwrap();
    assert_eq!(observed.active_state, "activating");
    assert_eq!(observed.ready_monotonic_us, 0);
    drop(c);
    let exit = stopped(&supervisor, &launch, Duration::from_secs(75)).await;
    assert_eq!(exit.result, "timeout");
    assert!(!launch.private_dir.join("consumed.json").exists());
    assert!(!launch.private_dir.join("fixture.evidence").exists());
    assert!(playtime_worker_control::boot_ms() >= launch.startup_deadline_boot_ms);
    cleanup.completed = true;
}
#[tokio::test]
#[ignore = "real user systemd; frozen wall deadline"]
async fn systemd_wall_deadline_starts_at_ready_and_survives_coordinator_exit() {
    let (mut cleanup, c, supervisor, ready) = setup(1500, false).await;
    let launch = ready.prepared.clone().unwrap();
    let args: Vec<String> = serde_json::from_slice(
        &read_private(&launch.private_dir.join("argv.fixture.json"), 65_536).unwrap(),
    )
    .unwrap();
    assert_eq!(
        args.last().unwrap(),
        "$HOME $(not-a-command); %n * \"quoted\""
    );
    // Preparation time is not charged by RuntimeMaxSec.
    tokio::time::sleep(Duration::from_millis(1700)).await;
    assert_eq!(
        supervisor
            .observe(&launch)
            .await
            .unwrap()
            .unwrap()
            .active_state,
        "activating"
    );
    let running = c.begin(ready.run_id).await.unwrap();
    assert_eq!(running.phase, Phase::Running);
    let start = running.last_status.unwrap().work_started_boot_ms.unwrap();
    drop(c);
    let exit = stopped(&supervisor, &launch, Duration::from_secs(15)).await;
    assert_eq!(exit.result, "timeout");
    assert!(exit.exit.unwrap().observed_boot_ms >= start + 1500);
    assert_eq!(
        read_private(&launch.private_dir.join("signal.fixture.json"), 128).unwrap(),
        b"deadline"
    );
    cleanup.completed = true;
}
#[tokio::test]
#[ignore = "real user systemd; full 180-second ignored-stop/cgroup enforcement"]
async fn systemd_stop_retries_do_not_reset_grace_and_kill_descendants() {
    let (mut cleanup, c, supervisor, ready) = setup(300_000, true).await;
    let launch = ready.prepared.clone().unwrap();
    assert_eq!(
        c.begin(ready.run_id.clone()).await.unwrap().phase,
        Phase::Running
    );
    let began_stop = Instant::now();
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Accepted));
    let original = supervisor.observe(&launch).await.unwrap().unwrap();
    assert_eq!(original.active_state, "deactivating");
    assert!(original.stopping_monotonic_us > 0);
    drop(c);
    // Exercise actual repeated manager stops too (lost-ack recovery), not only
    // the coordinator's cached acknowledgment. Neither call resets the timer.
    tokio::time::sleep(Duration::from_secs(5)).await;
    for _ in 0..2 {
        let retry = supervisor.stop(&launch).await.unwrap().unwrap();
        assert_eq!(retry.stopping_monotonic_us, original.stopping_monotonic_us);
    }
    let exit = stopped(&supervisor, &launch, Duration::from_secs(190)).await;
    assert!(began_stop.elapsed() >= Duration::from_secs(179));
    assert!(began_stop.elapsed() < Duration::from_secs(190));
    assert_eq!(exit.exit.unwrap().status.as_deref(), Some("KILL"));
    let pid: u32 = std::str::from_utf8(
        &read_private(&launch.private_dir.join("descendant.fixture"), 32).unwrap(),
    )
    .unwrap()
    .parse()
    .unwrap();
    assert!(!PathBuf::from(format!("/proc/{pid}/exe")).exists());
    cleanup.completed = true;
}
#[tokio::test]
#[ignore = "real user systemd; cancellation before Begin"]
async fn systemd_cancellation_before_begin_preserves_zero_receipt() {
    let (mut cleanup, c, supervisor, ready) = setup(30_000, false).await;
    let launch = ready.prepared.clone().unwrap();
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert!(matches!(
        cancelled.stop_delivery,
        Some(StopDelivery::Accepted | StopDelivery::Stopped)
    ));
    stopped(&supervisor, &launch, Duration::from_secs(15)).await;
    assert!(!launch.private_dir.join("consumed.json").exists());
    assert!(!launch.private_dir.join("fixture.evidence").exists());
    assert_eq!(
        c.begin(ready.run_id)
            .await
            .unwrap()
            .last_status
            .unwrap()
            .latest_receipt
            .unwrap()
            .sequence,
        0
    );
    cleanup.completed = true;
}

#[tokio::test]
#[ignore = "real user systemd; killed coordinator and invocation mismatch"]
async fn systemd_killed_coordinator_reattaches_without_resetting_worker_clock() {
    let supervisor = Arc::new(SystemdSupervisor::connect().await.unwrap());
    let root = Root::new();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fixture_coordinator_process",
            "--ignored",
            "--nocapture",
        ])
        .env("PLAYTIME_COORDINATOR_ROOT", &root.0)
        .env("PLAYTIME_COORDINATOR_SYSTEMD", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        root: Some(root),
        launch: None,
        completed: false,
    };
    let root = cleanup.root.as_ref().unwrap().0.clone();
    let saved: Admission = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "coordinator exited early"
            );
            if let Ok(bytes) = read_private(&root.join("coordinator-ready.fixture.json"), 65_536) {
                return serde_json::from_slice(&bytes).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let launch = saved.prepared.as_ref().unwrap();
    cleanup.launch = Some(launch.clone());
    child.kill().unwrap();
    child.wait().unwrap();
    let c = coordinator(&root, archive(&root), supervisor.clone()).await;
    let live = c.refresh(saved.run_id.clone()).await.unwrap();
    assert_eq!(live.phase, Phase::Running);
    assert_eq!(live.endpoint, saved.endpoint);
    assert_eq!(live.last_status, saved.last_status);
    // A recycled/stale private invocation record cannot authorize observation
    // of a live endpoint or stop a different manager invocation.
    let path = launch
        .private_dir
        .join(playtime_worker_control::systemd::IDENTITY_FILE);
    let original = read_private(&path, 65_536).unwrap();
    let mut identity: playtime_worker_control::systemd::Invocation =
        serde_json::from_slice(&original).unwrap();
    identity.manager_id = random_id().unwrap();
    fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
    assert!(supervisor.locate(launch).await.is_err());
    assert!(supervisor.stop(launch).await.is_err());
    fs::write(&path, original).unwrap();
    c.cancel(saved.run_id).await.unwrap();
    stopped(&supervisor, launch, Duration::from_secs(15)).await;
    cleanup.completed = true;
}
