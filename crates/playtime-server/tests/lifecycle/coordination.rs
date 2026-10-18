use super::*;
use playtime_protocol::worker::{Receipt, Status};
use playtime_worker_control::Endpoint;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::sync::Notify;

#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}
impl Gate {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    async fn wait(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("fixture boundary reached");
    }
}
struct Archive {
    inner: Arc<FixtureArchive>,
    previews: AtomicUsize,
    prepare: Gate,
    verify_started: Gate,
}
impl Archive {
    fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: archive(root),
            previews: AtomicUsize::new(0),
            prepare: Gate::default(),
            verify_started: Gate::default(),
        })
    }
}
#[async_trait::async_trait]
impl ArchivePort for Archive {
    async fn validate_preview(&self, request: &AdmissionRequest) -> Result<Selection> {
        self.previews.fetch_add(1, Ordering::SeqCst);
        self.inner.validate_preview(request).await
    }
    async fn prepare(&self, record: &Admission) -> Result<PreparedLaunch> {
        self.prepare.wait().await;
        self.inner.prepare(record).await
    }
    async fn verify_status(
        &self,
        launch: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> Result<()> {
        // Block the post-Begin receipt, even if scheduling lets the prepared
        // refresh cooldown elapse before the test reaches Begin.
        if status
            .latest_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.sequence > 0)
        {
            self.verify_started.wait().await;
        }
        self.inner.verify_status(launch, previous, status).await
    }
}
#[derive(Default)]
struct SupervisorGate {
    inner: FixtureSupervisor,
    locate: Gate,
    locations: AtomicUsize,
    starts: AtomicUsize,
}
#[async_trait::async_trait]
impl Supervisor for SupervisorGate {
    async fn ensure_legacy_idle(&self, root: &Path) -> Result<()> {
        self.inner.ensure_legacy_idle(root).await
    }
    async fn start(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.inner.start(launch).await
    }
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        self.locations.fetch_add(1, Ordering::SeqCst);
        self.locate.wait().await;
        self.inner.locate(launch).await
    }
}

// Wait for the specified scheduler floor, not as evidence that any action has
// happened. Every race below then synchronizes on an explicit port boundary.
async fn after_refresh_floor() {
    tokio::time::sleep(Duration::from_millis(250)).await;
}
async fn quick_cancel(c: &Coordinator, id: &playtime_core::RunId) -> Admission {
    // The held gates prove independence. Allow the five-second control deadline
    // plus scheduling margin, including actual executable verification.
    tokio::time::timeout(Duration::from_secs(6), c.cancel(id.clone()))
        .await
        .expect("cancellation was blocked by unrelated work")
        .unwrap()
}

#[tokio::test]
async fn accepted_request_replays_while_preparation_holds_launch_control() {
    let root = Root::new();
    let archive = Archive::new(&root.0);
    let c = coordinator(
        &root.0,
        archive.clone(),
        Arc::new(FixtureSupervisor::default()),
    )
    .await;
    let admitted = admit(&c).await;
    archive.prepare.arm();
    let starting = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.start(id).await })
    };
    archive.prepare.entered().await;
    let replay = tokio::time::timeout(
        Duration::from_secs(2),
        c.admit(
            admitted.service_id.clone(),
            admitted.key.clone(),
            admitted.request.clone(),
        ),
    )
    .await
    .expect("replay waited for preparation")
    .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.admission, admitted);
    let mut changed = admitted.request.clone();
    changed.mode = RunMode::Experiment;
    assert!(matches!(
        c.admit(admitted.service_id.clone(), admitted.key.clone(), changed)
            .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        c.admit(
            random_id().unwrap(),
            admitted.key.clone(),
            admitted.request.clone()
        )
        .await,
        Err(Error::ServiceMismatch)
    ));
    assert!(matches!(
        c.admit(
            admitted.service_id.clone(),
            "competitor".into(),
            admitted.request.clone()
        )
        .await,
        Err(Error::Busy)
    ));
    assert_eq!(archive.previews.load(Ordering::SeqCst), 1);
    assert_eq!(
        c.ledger().trace(&admitted.run_id).await.unwrap(),
        vec!["reserved"]
    );
    quick_cancel(&c, &admitted.run_id).await;
    archive.prepare.release.notify_one();
    assert!(!starting.await.unwrap().unwrap().start_attempted);
}

#[tokio::test]
#[ignore = "subprocess entry; bounded by invalid_control_fifo_returns_cancellation_uncertainty"]
async fn fifo_cancellation_process() {
    struct UnsubmittedSupervisor;
    #[async_trait::async_trait]
    impl Supervisor for UnsubmittedSupervisor {
        async fn ensure_legacy_idle(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        async fn start(&self, _: &PreparedLaunch) -> Result<Endpoint> {
            Err(Error::Uncertain)
        }
        async fn locate(&self, _: &PreparedLaunch) -> Result<Endpoint> {
            panic!("invalid revocation must fail before control exchange")
        }
    }
    let root = PathBuf::from(std::env::var_os("PLAYTIME_FIFO_ROOT").unwrap());
    let c = coordinator(&root, archive(&root), Arc::new(UnsubmittedSupervisor)).await;
    let admitted = admit(&c).await;
    let started = c.start(admitted.run_id.clone()).await.unwrap();
    let launch = started.prepared.as_ref().unwrap();
    let revoked = launch.private_dir.join("revoked.json");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &revoked,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .unwrap();
    let cancelled = c.cancel(admitted.run_id.clone()).await.unwrap();
    assert!(cancelled.cancel_requested.is_some());
    assert_eq!(cancelled.cancel_delivery, Some(CancelDelivery::Uncertain));
    assert_eq!(cancelled.reason, Some(Reason::ControlUncertain));
    assert_eq!(cancelled.last_status, started.last_status);
    assert_eq!(c.ledger().get(&admitted.run_id).await.unwrap(), cancelled);
    assert_eq!(c.cancel(admitted.run_id.clone()).await.unwrap(), cancelled);
    assert!(matches!(
        c.admit(admitted.service_id, "competitor".into(), request())
            .await,
        Err(Error::Busy)
    ));
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::symlink_metadata(revoked).unwrap().file_type().is_fifo());
    assert!(!launch.private_dir.join("consumed.json").exists());
    write_once(&root.join("fifo-cancellation-checked"), b"ok").unwrap();
}

#[test]
fn invalid_control_fifo_returns_cancellation_uncertainty() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = Root::new();
    let mut child = ChildGuard(
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "coordination::fifo_cancellation_process",
                "--ignored",
                "--nocapture",
            ])
            .env("PLAYTIME_FIFO_ROOT", &root.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "FIFO cancellation fixture failed: {status}"
            );
            assert_eq!(
                fs::read(root.0.join("fifo-cancellation-checked")).unwrap(),
                b"ok"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "invalid FIFO blocked cancellation before the control deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
async fn cancellation_during_preparation_never_submits_a_worker() {
    let root = Root::new();
    let archive = Archive::new(&root.0);
    let supervisor = Arc::new(SupervisorGate::default());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let admitted = admit(&c).await;
    archive.prepare.arm();
    let starting = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.start(id).await })
    };
    archive.prepare.entered().await;
    let cancelled = quick_cancel(&c, &admitted.run_id).await;
    assert_eq!(cancelled.cancel_delivery, Some(CancelDelivery::Requested));
    archive.prepare.release.notify_one();
    let finished = starting.await.unwrap().unwrap();
    assert_eq!(finished.cancel_requested, cancelled.cancel_requested);
    assert_eq!(finished.phase, Phase::Cancelling);
    assert!(!finished.start_attempted);
    assert_eq!(supervisor.starts.load(Ordering::SeqCst), 0);
    let launch = finished.prepared.unwrap();
    assert!(launch.private_dir.join("revoked.json").is_file());
    assert!(!launch.private_dir.join("consumed.json").exists());
}

#[tokio::test]
async fn cancellation_during_begin_preflight_prevents_grant_consumption() {
    let root = Root::new();
    let supervisor = Arc::new(SupervisorGate::default());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let admitted = admit(&c).await;
    let p = prepared(&c, &admitted).await;
    supervisor.locate.arm();
    let beginning = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.begin(id).await })
    };
    supervisor.locate.entered().await;
    let cancelled = quick_cancel(&c, &admitted.run_id).await;
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    supervisor.locate.release.notify_one();
    let late = beginning.await.unwrap().unwrap();
    assert_eq!(late.cancel_requested, cancelled.cancel_requested);
    assert_eq!(late.phase, Phase::Cancelling);
    assert_eq!(
        client(&p)
            .request(Command::Status)
            .await
            .unwrap()
            .status
            .state,
        WorkerState::CancelledBeforeBegin
    );
    let launch = late.prepared.unwrap();
    assert!(!launch.private_dir.join("consumed.json").exists());
    assert!(launch.private_dir.join("revoked.json").is_file());
    assert_eq!(supervisor.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_does_not_wait_for_begin_receipt_verification_or_promote_its_reply() {
    let root = Root::new();
    let archive = Archive::new(&root.0);
    let supervisor = Arc::new(SupervisorGate::default());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let admitted = admit(&c).await;
    let p = prepared(&c, &admitted).await;
    archive.verify_started.arm();
    let beginning = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.begin(id).await })
    };
    archive.verify_started.entered().await;
    let waiting: Vec<_> = (0..20)
        .map(|_| {
            let c = c.clone();
            let id = admitted.run_id.clone();
            tokio::spawn(async move { c.refresh(id).await })
        })
        .collect();
    let cancelled = quick_cancel(&c, &admitted.run_id).await;
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(
        cancelled.last_status, p.last_status,
        "acknowledgment cannot advance unverified evidence"
    );
    assert_eq!(cancelled.phase, Phase::Cancelling);
    archive.verify_started.release.notify_one();
    let late = beginning.await.unwrap().unwrap();
    assert_eq!(late.cancel_requested, cancelled.cancel_requested);
    assert_eq!(
        late.last_status, p.last_status,
        "late verified reply cannot overwrite a newer decision"
    );
    for observer in waiting {
        observer.await.unwrap().unwrap();
    }
    after_refresh_floor().await;
    let refreshed = c.refresh(admitted.run_id.clone()).await.unwrap();
    assert_eq!(refreshed.phase, Phase::Cancelling);
    assert_eq!(
        refreshed.last_status.as_ref().unwrap().state,
        WorkerState::Cancelling
    );
    assert_eq!(
        refreshed
            .last_status
            .as_ref()
            .unwrap()
            .latest_receipt
            .as_ref()
            .unwrap()
            .sequence,
        1
    );
    let revision = refreshed.revision;
    let trace = c.ledger().trace(&admitted.run_id).await.unwrap();
    after_refresh_floor().await;
    let equal = c.refresh(admitted.run_id.clone()).await.unwrap();
    assert_eq!(
        equal.revision, revision,
        "identical confirmations do not append history"
    );
    assert_eq!(c.ledger().trace(&admitted.run_id).await.unwrap(), trace);
    assert_eq!(supervisor.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn one_slow_status_probe_is_shared_and_does_not_block_cancel() {
    let root = Root::new();
    let supervisor = Arc::new(SupervisorGate::default());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let admitted = admit(&c).await;
    prepared(&c, &admitted).await;
    after_refresh_floor().await;
    let before = supervisor.locations.load(Ordering::SeqCst);
    supervisor.locate.arm();
    let first = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.refresh(id).await })
    };
    supervisor.locate.entered().await;
    let followers: Vec<_> = (0..20)
        .map(|_| {
            let c = c.clone();
            let id = admitted.run_id.clone();
            tokio::spawn(async move { c.refresh(id).await })
        })
        .collect();
    let cancel = quick_cancel(&c, &admitted.run_id).await;
    assert_eq!(
        cancel.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(
        supervisor.locations.load(Ordering::SeqCst),
        before + 2,
        "one Status and one independent Cancel"
    );
    supervisor.locate.release.notify_one();
    assert_eq!(
        first.await.unwrap().unwrap().cancel_requested,
        cancel.cancel_requested
    );
    for follower in followers {
        follower.await.unwrap().unwrap();
    }
    assert_eq!(supervisor.locations.load(Ordering::SeqCst), before + 2);
    assert_eq!(
        c.ledger()
            .get(&admitted.run_id)
            .await
            .unwrap()
            .cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
}

#[tokio::test]
async fn timed_out_preflight_remains_bounded_and_cannot_later_publish() {
    let root = Root::new();
    let supervisor = Arc::new(SupervisorGate::default());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let admitted = admit(&c).await;
    let p = prepared(&c, &admitted).await;
    after_refresh_floor().await;
    let before = supervisor.locations.load(Ordering::SeqCst);
    supervisor.locate.arm();
    let first = {
        let c = c.clone();
        let id = admitted.run_id.clone();
        tokio::spawn(async move { c.refresh(id).await })
    };
    supervisor.locate.entered().await;
    let expired = tokio::time::timeout(Duration::from_secs(6), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(expired.phase, Phase::Reconciling);
    assert_eq!(expired.last_status, p.last_status);
    after_refresh_floor().await;
    c.refresh(admitted.run_id.clone()).await.unwrap();
    assert_eq!(
        supervisor.locations.load(Ordering::SeqCst),
        before + 1,
        "no abandoned preflight accumulation"
    );
    let cancelled = quick_cancel(&c, &admitted.run_id).await;
    supervisor.locate.release.notify_one();
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(
        c.ledger().get(&admitted.run_id).await.unwrap().last_status,
        p.last_status
    );
}

#[tokio::test]
async fn incomplete_socket_frame_cannot_serialize_all_worker_control() {
    let root = Root::new();
    let c = coordinator(
        &root.0,
        archive(&root.0),
        Arc::new(FixtureSupervisor::default()),
    )
    .await;
    let admitted = admit(&c).await;
    let p = prepared(&c, &admitted).await;
    let mut slow = tokio::net::UnixStream::connect(&p.prepared.as_ref().unwrap().socket)
        .await
        .unwrap();
    use tokio::io::AsyncWriteExt;
    slow.write_all(&[0]).await.unwrap();
    let cancelled = quick_cancel(&c, &admitted.run_id).await;
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(
        slow.try_read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "Cancel completed while the partial-frame connection remained open"
    );
    assert_eq!(
        client(&p)
            .request(Command::Status)
            .await
            .unwrap()
            .status
            .state,
        WorkerState::CancelledBeforeBegin
    );
}

#[tokio::test]
async fn v1_ledger_migration_preserves_identity_and_canonical_request() {
    let root = Root::new();
    let c = coordinator(
        &root.0,
        archive(&root.0),
        Arc::new(FixtureSupervisor::default()),
    )
    .await;
    let saved = admit(&c).await;
    drop(c);
    {
        let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
        let mut old = serde_json::to_value(&saved).unwrap();
        for field in ["owner_generation", "revision", "cancel_delivery"] {
            old.as_object_mut().unwrap().remove(field);
        }
        db.execute(
            "UPDATE runs SET record=?1",
            [serde_json::to_string(&old).unwrap()],
        )
        .unwrap();
        db.execute_batch("PRAGMA user_version=1;").unwrap();
    }
    let ledger = Ledger::open(&root.0).await.unwrap();
    let restored = ledger.lookup(&saved.key).await.unwrap().unwrap();
    assert_eq!(ledger.service_id(), &saved.service_id);
    assert_eq!(restored.request, saved.request);
    assert_eq!(restored.request_sha256, saved.request_sha256);
    assert_eq!(restored.selection, saved.selection);
    assert_eq!(restored.owner_generation, 1);
    assert!(restored.revision > 0);
    let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
    assert_eq!(
        db.query_row::<u32, _, _>("PRAGMA user_version", [], |row| row.get(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn v2_ledger_never_defaults_missing_ownership_or_revision() {
    for missing in ["owner_generation", "revision"] {
        let root = Root::new();
        let c = coordinator(
            &root.0,
            archive(&root.0),
            Arc::new(FixtureSupervisor::default()),
        )
        .await;
        let saved = admit(&c).await;
        drop(c);
        let db = rusqlite::Connection::open(root.0.join(".playtime/service.sqlite3")).unwrap();
        let mut damaged = serde_json::to_value(&saved).unwrap();
        damaged.as_object_mut().unwrap().remove(missing);
        db.execute(
            "UPDATE runs SET record=?1",
            [serde_json::to_string(&damaged).unwrap()],
        )
        .unwrap();
        drop(db);
        assert!(
            Ledger::open(&root.0).await.is_err(),
            "missing {missing} must fail closed"
        );
    }
}
