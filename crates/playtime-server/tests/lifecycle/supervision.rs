use super::*;
use async_trait::async_trait;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize},
};

struct ManagerFixture {
    child: FixtureSupervisor,
    observation: Mutex<SupervisorObservation>,
    unavailable: AtomicBool,
    lose_stop: AtomicBool,
    stops: AtomicUsize,
}
impl ManagerFixture {
    fn new() -> Self {
        Self {
            child: FixtureSupervisor::default(),
            observation: Mutex::new(SupervisorObservation {
                manager_id: random_id().unwrap(),
                cgroup: "/synthetic-manager".into(),
                active_state: "active".into(),
                main_pid: 1,
                ready_monotonic_us: 123,
                stopping_monotonic_us: 0,
                result: "success".into(),
                stopped: false,
                exit: None,
            }),
            unavailable: AtomicBool::new(false),
            lose_stop: AtomicBool::new(false),
            stops: AtomicUsize::new(0),
        }
    }
}
#[async_trait]
impl Supervisor for ManagerFixture {
    async fn ensure_legacy_idle(&self, root: &Path) -> Result<()> {
        self.child.ensure_legacy_idle(root).await
    }
    async fn start(
        &self,
        launch: &PreparedLaunch,
    ) -> Result<Option<playtime_worker_control::Endpoint>> {
        self.child.start(launch).await
    }
    async fn locate(&self, launch: &PreparedLaunch) -> Result<playtime_worker_control::Endpoint> {
        self.child.locate(launch).await
    }
    async fn observe(&self, _: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(Error::Uncertain);
        }
        Ok(Some(self.observation.lock().unwrap().clone()))
    }
    async fn stop(&self, launch: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
        self.observe(launch).await?;
        {
            let mut observation = self.observation.lock().unwrap();
            if observation.active_state != "deactivating" {
                self.stops.fetch_add(1, Ordering::SeqCst);
                observation.active_state = "deactivating".into();
                observation.stopping_monotonic_us = 456;
            }
        }
        if self.lose_stop.swap(false, Ordering::SeqCst) {
            return Err(Error::Uncertain);
        }
        self.observe(launch).await
    }
}
#[tokio::test]
async fn lost_stop_ack_and_restart_recover_original_job_and_grace() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    supervisor.lose_stop.store(true, Ordering::SeqCst);
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Uncertain));
    drop(c);
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    let recovered = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(recovered.stop_delivery, Some(StopDelivery::Accepted));
    assert_eq!(recovered.cancel_requested, cancelled.cancel_requested);
    let accepted_at = recovered.stop_accepted.clone();
    for _ in 0..3 {
        after_refresh_floor().await;
        let retry = c.cancel(ready.run_id.clone()).await.unwrap();
        assert_eq!(retry.stop_accepted, accepted_at);
        assert_eq!(retry.supervisor.unwrap().stopping_monotonic_us, 456);
    }
    assert_eq!(supervisor.stops.load(Ordering::SeqCst), 1);
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    {
        let mut observation = supervisor.observation.lock().unwrap();
        observation.active_state = "unloaded".into();
        observation.main_pid = 0;
        observation.stopped = true;
        observation.ready_monotonic_us = 0;
        observation.stopping_monotonic_us = 0;
    }
    after_refresh_floor().await;
    let stopped = c.refresh(ready.run_id.clone()).await.unwrap();
    assert_eq!(stopped.stop_delivery, Some(StopDelivery::Stopped));
    assert_eq!(stopped.supervisor.unwrap().stopping_monotonic_us, 456);
    assert_eq!(stopped.phase, Phase::Reconciling);
    // No worker/cgroup does not prove archive or external-resource ownership.
    assert!(matches!(
        c.admit(c.ledger().service_id().clone(), "second".into(), request())
            .await,
        Err(Error::Busy)
    ));
}
#[tokio::test]
async fn manager_outage_does_not_upgrade_cancel_ack_to_enforced_stop() {
    let root = Root::new();
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    supervisor.unavailable.store(true, Ordering::SeqCst);
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Uncertain));
    assert!(cancelled.stop_accepted.is_none());
    assert_eq!(supervisor.stops.load(Ordering::SeqCst), 0);
    assert!(
        !ready
            .prepared
            .unwrap()
            .private_dir
            .join("fixture.evidence")
            .exists()
    );
    supervisor.unavailable.store(false, Ordering::SeqCst);
    after_refresh_floor().await;
    let retried = c.cancel(ready.run_id).await.unwrap();
    assert_eq!(retried.stop_delivery, Some(StopDelivery::Accepted));
    assert_eq!(retried.cancel_requested, cancelled.cancel_requested);
}
#[tokio::test]
async fn v2_migration_preserves_history_and_adds_supervision_evidence() {
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
    let mut value = serde_json::to_value(&saved).unwrap();
    for key in ["stop_delivery", "stop_accepted", "supervisor"] {
        value.as_object_mut().unwrap().remove(key);
    }
    db.execute(
        "UPDATE runs SET record=?1",
        [serde_json::to_string(&value).unwrap()],
    )
    .unwrap();
    db.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(db);
    let ledger = Ledger::open(&root.0).await.unwrap();
    let restored = ledger.get(&saved.run_id).await.unwrap();
    assert_eq!(restored.service_id, saved.service_id);
    assert_eq!(restored.selection, saved.selection);
    assert_eq!(restored.owner_generation, saved.owner_generation);
    assert!(restored.supervisor.is_none());
}
