use playtime_core::RunMode;
use playtime_protocol::worker::{Command, WorkerState};
use playtime_server::lifecycle::{
    fixture::{self, FixtureArchive, FixtureSupervisor},
    ports::{ArchivePort, Supervisor, SystemClock},
    *,
};
use playtime_worker_control::{
    Bootstrap, Client, FileLock, Grant, random_id, read_private, write_once,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/pt34-{}-{}",
            std::process::id(),
            &random_id().unwrap().as_str()[..8]
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn request() -> AdmissionRequest {
    static REVISION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let revision = REVISION.get_or_init(|| {
        FixtureArchive::new(
            Path::new("/tmp"),
            &std::env::current_exe().unwrap(),
            Vec::new(),
        )
        .unwrap()
        .preview_revision()
    });
    AdmissionRequest {
        recipe_id: "lifecycle-fixture".into(),
        mode: RunMode::Simulation,
        preview_revision: revision.clone(),
    }
}
fn archive(root: &Path) -> Arc<FixtureArchive> {
    Arc::new(
        FixtureArchive::new(
            root,
            &std::env::current_exe().unwrap(),
            vec![
                "--exact".into(),
                "fixture_worker_process".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        )
        .unwrap(),
    )
}
async fn coordinator(
    root: &Path,
    archive: Arc<dyn ArchivePort>,
    supervisor: Arc<dyn Supervisor>,
) -> Coordinator {
    Coordinator::new(
        Ledger::open(root).await.unwrap(),
        archive,
        supervisor,
        Arc::new(SystemClock),
    )
}
async fn admit(c: &Coordinator) -> Admission {
    c.admit(
        c.ledger().service_id().clone(),
        "launch-key".into(),
        request(),
    )
    .await
    .unwrap()
    .admission
}
async fn prepared(c: &Coordinator, admission: &Admission) -> Admission {
    c.start(admission.run_id.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let current = c.refresh(admission.run_id.clone()).await.unwrap();
            if current
                .last_status
                .as_ref()
                .is_some_and(|status| status.state == WorkerState::Prepared)
            {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture must report prepared")
}
fn client(admission: &Admission) -> Client {
    let prepared = admission.prepared.as_ref().unwrap();
    let bootstrap: Bootstrap =
        serde_json::from_slice(&read_private(&prepared.bootstrap, 65_536).unwrap()).unwrap();
    Client::new(admission.endpoint.clone().unwrap(), bootstrap.credential)
}

#[test]
#[ignore = "subprocess fixture entry; invoked by lifecycle integration tests"]
fn fixture_worker_process() {
    let path =
        std::env::var_os("PLAYTIME_LIFECYCLE_BOOTSTRAP").expect("private fixture bootstrap path");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(fixture::run_worker(Path::new(&path)))
        .unwrap();
}

#[tokio::test]
async fn durable_admission_replays_before_new_preview_and_blocks_competitors() {
    let root = Root::new();
    let original_archive = archive(&root.0);
    let supervisor = Arc::new(FixtureSupervisor::default());
    let c = coordinator(&root.0, original_archive.clone(), supervisor.clone()).await;
    let first = admit(&c).await;
    let mut changed = request();
    changed.mode = RunMode::Experiment;
    assert!(matches!(
        c.admit(
            c.ledger().service_id().clone(),
            "launch-key".into(),
            changed
        )
        .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        c.admit(
            c.ledger().service_id().clone(),
            "competing-key".into(),
            request()
        )
        .await,
        Err(Error::Busy)
    ));
    assert!(matches!(
        c.admit(random_id().unwrap(), "launch-key".into(), request())
            .await,
        Err(Error::ServiceMismatch)
    ));
    assert!(Ledger::open(&root.0).await.is_err(), "single service lock");
    let service = c.ledger().service_id().clone();
    drop(c);
    // A changed default refuses today's preview; accepted-key lookup must never
    // consult it or silently switch away from the originally selected bundle.
    struct RejectPreview;
    #[async_trait::async_trait]
    impl ArchivePort for RejectPreview {
        async fn validate_preview(&self, _: &AdmissionRequest) -> Result<Selection> {
            panic!("replay revalidated today's preview")
        }
        async fn prepare(&self, _: &Admission) -> Result<PreparedLaunch> {
            unreachable!()
        }
        async fn verify_status(
            &self,
            _: &PreparedLaunch,
            _: Option<&playtime_protocol::worker::Receipt>,
            _: &playtime_protocol::worker::Status,
        ) -> Result<()> {
            unreachable!()
        }
    }
    let restarted = coordinator(&root.0, Arc::new(RejectPreview), supervisor).await;
    assert_eq!(restarted.ledger().service_id(), &service);
    let replay = restarted
        .admit(service, "launch-key".into(), request())
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.admission.run_id, first.run_id);
    assert_eq!(replay.admission.selection, first.selection);
    assert_eq!(replay.admission.accepted, first.accepted);
    assert_eq!(replay.admission.phase, Phase::Reconciling);
}

#[tokio::test]
async fn lost_start_and_begin_replies_restart_and_duplicate_begin_keep_one_worker() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(FixtureSupervisor::default());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let admitted = admit(&c).await;
    supervisor
        .lose_next_start_reply
        .store(true, Ordering::SeqCst);
    let uncertain = c.start(admitted.run_id.clone()).await.unwrap();
    assert_eq!(uncertain.phase, Phase::Reconciling);
    let p = prepared(&c, &admitted).await; // start is queried, never submitted twice
    let launch = p.prepared.as_ref().unwrap();
    assert_eq!(
        p.last_status
            .as_ref()
            .unwrap()
            .latest_receipt
            .as_ref()
            .unwrap()
            .sequence,
        0
    );
    assert!(!launch.private_dir.join("fixture.evidence").exists());
    assert!(FileLock::acquire(&root.0.join(".playtime/run.lock")).is_err());
    assert!(FileLock::acquire(&launch.private_dir.join("writer.fixture.lock")).is_err());
    write_once(
        &launch.private_dir.join("lose-begin-reply.fixture"),
        b"one reply",
    )
    .unwrap();
    let uncertain = c.begin(admitted.run_id.clone()).await.unwrap();
    assert_eq!(uncertain.phase, Phase::Reconciling);
    assert_eq!(
        uncertain
            .last_status
            .as_ref()
            .unwrap()
            .latest_receipt
            .as_ref()
            .unwrap()
            .sequence,
        0,
        "lost receipt does not advance ledger"
    );
    let original_pid = p.endpoint.as_ref().unwrap().pid;
    let service = c.ledger().service_id().clone();
    drop(c);
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    assert_eq!(c.ledger().service_id(), &service);
    assert_eq!(
        c.ledger().get(&admitted.run_id).await.unwrap().phase,
        Phase::Reconciling
    );
    let running = c.refresh(admitted.run_id.clone()).await.unwrap();
    assert_eq!(running.phase, Phase::Running);
    assert_eq!(running.endpoint.as_ref().unwrap().pid, original_pid);
    let first = client(&running).request(Command::Begin).await.unwrap();
    let second = client(&running).request(Command::Begin).await.unwrap();
    assert_eq!(first.status, second.status);
    assert_eq!(first.status.latest_receipt.as_ref().unwrap().sequence, 1);
    let evidence = read_private(&launch.private_dir.join("fixture.evidence"), 4096).unwrap();
    assert_eq!(evidence.iter().filter(|&&b| b == b'\n').count(), 1);
    let trace = c.ledger().trace(&admitted.run_id).await.unwrap();
    assert_eq!(trace.iter().filter(|t| *t == "start_intent").count(), 1);
    assert_eq!(trace.iter().filter(|t| *t == "begin_authorized").count(), 1);
    // A replacement process cannot consume the grant a second time.
    supervisor.terminate(launch).unwrap();
    assert!(fixture::run_worker(&launch.bootstrap).await.is_err());
    assert_eq!(
        c.refresh(admitted.run_id.clone()).await.unwrap().phase,
        Phase::Reconciling
    );
    assert!(c.start(admitted.run_id).await.unwrap().start_attempted);
}

#[tokio::test]
async fn cancellation_before_begin_is_durable_and_never_creates_run_records() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(FixtureSupervisor::default());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let admitted = admit(&c).await;
    let p = prepared(&c, &admitted).await;
    let cancelled = c.cancel(admitted.run_id.clone()).await.unwrap();
    assert_eq!(
        cancelled.last_status.as_ref().unwrap().state,
        WorkerState::CancelledBeforeBegin
    );
    let again = c.cancel(admitted.run_id.clone()).await.unwrap();
    assert_eq!(again.cancel_requested, cancelled.cancel_requested);
    let late = client(&p).request(Command::Begin).await.unwrap();
    assert_eq!(late.status.state, WorkerState::CancelledBeforeBegin);
    assert!(late.status.work_started_boot_ms.is_none());
    drop(c);
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    let after = c.begin(admitted.run_id.clone()).await.unwrap();
    assert_eq!(after.cancel_requested, cancelled.cancel_requested);
    let directory = &p.prepared.as_ref().unwrap().private_dir;
    assert!(!directory.join("fixture.evidence").exists());
    assert!(!directory.join("consumed.json").exists());
    assert!(directory.join("revoked.json").exists());
    assert!(
        !root
            .0
            .join("runs")
            .join(admitted.run_id.as_str())
            .join("manifest.json")
            .exists()
    );
    assert!(
        c.admit(c.ledger().service_id().clone(), "new-key".into(), request())
            .await
            .is_err(),
        "cancellation alone does not release ownership"
    );
}

#[tokio::test]
async fn authentication_binds_peer_invocation_and_credentials_and_prevents_unpermitted_begin() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(FixtureSupervisor::default());
    let c = coordinator(&root.0, archive, supervisor).await;
    let admission = admit(&c).await;
    let p = prepared(&c, &admission).await;
    let endpoint = p.endpoint.clone().unwrap();
    let bootstrap: Bootstrap = serde_json::from_slice(
        &read_private(&p.prepared.as_ref().unwrap().bootstrap, 65_536).unwrap(),
    )
    .unwrap();
    assert!(
        Client::new(
            endpoint.clone(),
            playtime_worker_control::Credential::generate().unwrap()
        )
        .request(Command::Status)
        .await
        .is_err()
    );
    let mut wrong_peer = endpoint.clone();
    wrong_peer.pid = std::process::id();
    assert!(
        Client::new(wrong_peer, bootstrap.credential.clone())
            .request(Command::Status)
            .await
            .is_err()
    );
    let mut wrong_invocation = endpoint;
    wrong_invocation.binding.invocation_id = random_id().unwrap();
    assert!(
        Client::new(wrong_invocation, bootstrap.credential)
            .request(Command::Status)
            .await
            .is_err()
    );
    assert!(
        client(&p).request(Command::Begin).await.is_err(),
        "socket access is not Begin permission"
    );
    let status = client(&p).request(Command::Status).await.unwrap();
    assert_eq!(status.status.state, WorkerState::Reconciling);
    assert!(status.status.work_started_boot_ms.is_none());
    assert!(
        !p.prepared
            .unwrap()
            .private_dir
            .join("fixture.evidence")
            .exists()
    );
}

#[tokio::test]
async fn cancelled_admission_cannot_start_and_corrupt_ledger_is_never_recreated() {
    let root = Root::new();
    let c = coordinator(
        &root.0,
        archive(&root.0),
        Arc::new(FixtureSupervisor::default()),
    )
    .await;
    let admission = admit(&c).await;
    c.cancel(admission.run_id.clone()).await.unwrap();
    assert!(!c.start(admission.run_id).await.unwrap().start_attempted);
    drop(c);
    let path = root.0.join(".playtime/service.sqlite3");
    fs::write(&path, b"damaged-ledger-do-not-recreate").unwrap();
    assert!(Ledger::open(&root.0).await.is_err());
    assert_eq!(fs::read(path).unwrap(), b"damaged-ledger-do-not-recreate");
}

#[tokio::test]
async fn legacy_lock_and_startup_reservation_use_python_compatible_metadata() {
    struct GatedArchive(Arc<FixtureArchive>);
    #[async_trait::async_trait]
    impl ArchivePort for GatedArchive {
        async fn validate_preview(&self, request: &AdmissionRequest) -> Result<Selection> {
            self.0.validate_preview(request).await
        }
        async fn prepare(&self, admission: &Admission) -> Result<PreparedLaunch> {
            let launch = self.0.prepare(admission).await?;
            write_once(
                &launch.private_dir.join("hold-before-lock.fixture"),
                b"test gate",
            )?;
            Ok(launch)
        }
        async fn verify_status(
            &self,
            launch: &PreparedLaunch,
            previous: Option<&playtime_protocol::worker::Receipt>,
            status: &playtime_protocol::worker::Status,
        ) -> Result<()> {
            self.0.verify_status(launch, previous, status).await
        }
    }
    let root = Root::new();
    let c = coordinator(
        &root.0,
        Arc::new(GatedArchive(archive(&root.0))),
        Arc::new(FixtureSupervisor::default()),
    )
    .await;
    let admission = admit(&c).await;
    let p = c.start(admission.run_id.clone()).await.unwrap();
    let bootstrap = p.prepared.as_ref().unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !bootstrap
            .private_dir
            .join("waiting-before-lock.fixture")
            .exists()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let script = r#"
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[2])
from playtime.jobs import RunLock, ensure_idle, lock_held, RUN_ID
from playtime.common import PlaytimeError
root = Path(sys.argv[1])
assert not lock_held(root)
assert RUN_ID.fullmatch(sys.argv[3])
class StartingUnit:
    def state(self, unit):
        assert unit == 'playtime-' + sys.argv[3] + '.service'
        return 'activating'
try:
    with RunLock(root):
        ensure_idle(root, StartingUnit())
except PlaytimeError:
    pass
else:
    raise AssertionError('legacy reservation did not exclude startup')
"#;
    let output = ProcessCommand::new("python3")
        .args(["-c", script])
        .arg(&root.0)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src"))
        .arg(admission.run_id.as_str())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_file(bootstrap.private_dir.join("hold-before-lock.fixture")).unwrap();
    prepared(&c, &admission).await;
    assert!(FileLock::acquire(&root.0.join(".playtime/run.lock")).is_err());
    let grant = Grant::new(bootstrap.private_dir.clone(), bootstrap.binding.clone());
    grant.revoke().unwrap();
    assert!(grant.authorize().is_err());
}

#[tokio::test]
async fn preparation_image_change_keeps_reservation_and_does_not_start_worker() {
    let root = Root::new();
    let executable = root.0.join("bundle");
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let archive = Arc::new(FixtureArchive::new(&root.0, &executable, Vec::new()).unwrap());
    let c = coordinator(&root.0, archive, Arc::new(FixtureSupervisor::default())).await;
    let admission = admit(&c).await;
    fs::write(&executable, b"changed after preview").unwrap();
    assert!(c.start(admission.run_id.clone()).await.is_err());
    assert!(
        !c.ledger()
            .get(&admission.run_id)
            .await
            .unwrap()
            .start_attempted
    );
    assert!(
        c.admit(
            c.ledger().service_id().clone(),
            "second-key".into(),
            request()
        )
        .await
        .is_err()
    );
}

#[test]
#[ignore = "subprocess coordinator; killed by the restart test"]
fn fixture_coordinator_process() {
    let root = PathBuf::from(std::env::var_os("PLAYTIME_COORDINATOR_ROOT").unwrap());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let c = coordinator(
                &root,
                archive(&root),
                Arc::new(FixtureSupervisor::default()),
            )
            .await;
            let admission = admit(&c).await;
            prepared(&c, &admission).await;
            let running = c.begin(admission.run_id).await.unwrap();
            assert_eq!(running.phase, Phase::Running);
            write_once(
                &root.join("coordinator-ready.fixture.json"),
                &serde_json::to_vec(&running).unwrap(),
            )
            .unwrap();
            tokio::time::sleep(Duration::from_secs(40)).await;
        });
}

#[tokio::test]
async fn killed_coordinator_reattaches_to_original_surviving_process() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct WorkerGuard(rustix::fd::OwnedFd);
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            let _ = rustix::process::pidfd_send_signal(&self.0, rustix::process::Signal::KILL);
        }
    }
    let root = Root::new();
    let mut child = ChildGuard(
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fixture_coordinator_process",
                "--ignored",
                "--nocapture",
            ])
            .env("PLAYTIME_COORDINATOR_ROOT", &root.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let saved: Admission = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "fixture coordinator exited before readiness"
            );
            if let Ok(bytes) = read_private(&root.0.join("coordinator-ready.fixture.json"), 65_536)
                && let Ok(record) = serde_json::from_slice(&bytes)
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = saved.endpoint.as_ref().unwrap().pid;
    let worker = WorkerGuard(
        rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(pid as i32).unwrap(),
            rustix::process::PidfdFlags::empty(),
        )
        .unwrap(),
    );
    let supervisor = Arc::new(FixtureSupervisor::default());
    assert_eq!(
        supervisor
            .locate(saved.prepared.as_ref().unwrap())
            .await
            .unwrap()
            .pid,
        pid
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let restarted = coordinator(&root.0, archive(&root.0), supervisor).await;
    assert_eq!(restarted.ledger().service_id(), &saved.service_id);
    let uncertain = restarted.ledger().get(&saved.run_id).await.unwrap();
    assert_eq!(uncertain.phase, Phase::Reconciling);
    let live = restarted.refresh(saved.run_id.clone()).await.unwrap();
    assert_eq!(live.phase, Phase::Running);
    assert_eq!(live.endpoint.as_ref().unwrap().pid, pid);
    assert_eq!(
        live.last_status, saved.last_status,
        "allocation, original work clock and receipt survive API death"
    );
    assert!(FileLock::acquire(&root.0.join(".playtime/run.lock")).is_err());
    assert!(
        restarted
            .admit(saved.service_id, "second-key".into(), request())
            .await
            .is_err()
    );
    restarted.cancel(saved.run_id.clone()).await.unwrap();
    drop(worker);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if FileLock::acquire(&root.0.join(".playtime/run.lock")).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        restarted.refresh(saved.run_id).await.unwrap().phase,
        Phase::Reconciling,
        "worker death is never automatic resumption or completion"
    );
}

#[tokio::test]
async fn deleting_a_known_ledger_does_not_silently_create_a_new_service() {
    let root = Root::new();
    let ledger = Ledger::open(&root.0).await.unwrap();
    let identity = ledger.service_id().clone();
    drop(ledger);
    fs::remove_file(root.0.join(".playtime/service.sqlite3")).unwrap();
    assert!(Ledger::open(&root.0).await.is_err());
    assert_eq!(
        read_private(&root.0.join(".playtime/service.identity"), 32).unwrap(),
        identity.as_str().as_bytes()
    );
}
