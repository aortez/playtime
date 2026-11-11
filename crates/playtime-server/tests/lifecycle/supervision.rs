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
    diagnostics_failed: AtomicBool,
    lose_stop: AtomicBool,
    stops: AtomicUsize,
    /// Every stop request, including those the manager refuses.
    stop_calls: AtomicUsize,
    observes: AtomicUsize,
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
            diagnostics_failed: AtomicBool::new(false),
            lose_stop: AtomicBool::new(false),
            stops: AtomicUsize::new(0),
            stop_calls: AtomicUsize::new(0),
            observes: AtomicUsize::new(0),
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
        self.observes.fetch_add(1, Ordering::SeqCst);
        if self.unavailable.load(Ordering::SeqCst) || self.diagnostics_failed.load(Ordering::SeqCst)
        {
            return Err(Error::Uncertain);
        }
        Ok(Some(self.observation.lock().unwrap().clone()))
    }
    async fn stop(&self, _: &PreparedLaunch, authenticated: &SupervisorObservation) -> Result<()> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(Error::Uncertain);
        }
        {
            let mut observation = self.observation.lock().unwrap();
            if observation.manager_id != authenticated.manager_id
                || observation.cgroup != authenticated.cgroup
            {
                return Err(Error::Uncertain);
            }
            if observation.active_state != "deactivating" {
                self.stops.fetch_add(1, Ordering::SeqCst);
                observation.active_state = "deactivating".into();
                observation.stopping_monotonic_us = 456;
            }
        }
        if self.lose_stop.swap(false, Ordering::SeqCst) {
            return Err(Error::Uncertain);
        }
        Ok(())
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
    supervisor.diagnostics_failed.store(true, Ordering::SeqCst);
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
    assert_eq!(recovered.phase, Phase::Reconciling);
    supervisor.diagnostics_failed.store(false, Ordering::SeqCst);
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
async fn release_requires_the_original_invocation_to_be_stopped() {
    let root = Root::new();
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    let release = || c.release(ready.run_id.clone(), "operator test".into());
    let refused = release().await;
    assert!(
        matches!(&refused, Err(Error::Refused(remaining))
            if remaining.len() == 1 && remaining[0].ends_with("is active with main PID 1")),
        "a running unit keeps the slot: {refused:?}"
    );
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Accepted));
    assert!(
        matches!(release().await, Err(Error::Refused(_))),
        "a deactivating unit keeps the slot"
    );
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    {
        let mut observation = supervisor.observation.lock().unwrap();
        observation.active_state = "unloaded".into();
        observation.main_pid = 0;
        observation.stopped = true;
        observation.cgroup = "/replacement".into();
    }
    let refused = release().await;
    assert!(
        matches!(&refused, Err(Error::Refused(remaining))
            if remaining[0].ends_with("no longer matches its saved identity")),
        "a stopped replacement is not the original invocation: {refused:?}"
    );
    supervisor.observation.lock().unwrap().cgroup = "/synthetic-manager".into();
    let released = release().await.unwrap();
    assert_eq!(released.phase, Phase::Terminal);
    assert_eq!(released.reason, Some(Reason::OperatorReleased));
    assert_eq!(released.stop_accepted, cancelled.stop_accepted);
    let evidence = released.operator_release.unwrap().supervisor.unwrap();
    assert!(evidence.stopped);
    assert_eq!(evidence.active_state, "unloaded");
}

/// The host after a reboot: the same clock, on another boot.
struct Rebooted(playtime_protocol::worker::ControlId);
impl Clock for Rebooted {
    fn now(&self) -> Stamp {
        SystemClock.now()
    }
    fn boot_id(&self) -> Result<playtime_protocol::worker::ControlId> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn release_takes_a_later_boot_as_proof_that_the_worker_stopped() {
    let root = Root::new();
    let resource = "container playtime-fake-agent-1";
    let archive = Arc::new(ResidualArchive {
        inner: archive(&root.0),
        present: Mutex::new(vec![format!("{resource} is present")]),
    });
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    // A reboot ends the worker, and no supervisor can observe its unit after.
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    supervisor.unavailable.store(true, Ordering::SeqCst);
    let refused = c.release(ready.run_id.clone(), "rebooted".into()).await;
    assert!(
        matches!(&refused, Err(Error::Refused(remaining))
            if remaining.len() == 2 && remaining[0].contains("could not be observed")),
        "on the worker's own boot, only the supervisor proves it stopped: {refused:?}"
    );
    drop(c);
    let boot = random_id().unwrap();
    let c = coordinator_on(
        &root.0,
        archive.clone(),
        supervisor.clone(),
        Arc::new(Rebooted(boot.clone())),
    )
    .await;
    // Without the fixture's legacy compatibility pointer, only the ledger slot
    // can block admission.
    fs::remove_file(root.0.join(".playtime/active.json")).unwrap();
    assert!(matches!(
        c.admit(c.ledger().service_id().clone(), "second".into(), request())
            .await,
        Err(Error::Busy)
    ));
    let observed = supervisor.observes.load(Ordering::SeqCst);
    // The reboot proves the worker stopped, not that its resources are gone.
    let refused = c.release(ready.run_id.clone(), "rebooted".into()).await;
    assert!(
        matches!(&refused, Err(Error::Refused(remaining))
            if remaining == &vec![format!("{resource} is present")]),
        "a remaining resource keeps the slot after a reboot too: {refused:?}"
    );
    assert_ne!(
        c.ledger().get(&ready.run_id).await.unwrap().phase,
        Phase::Terminal
    );
    archive.present.lock().unwrap().clear();
    let released = c
        .release(ready.run_id.clone(), "rebooted".into())
        .await
        .unwrap();
    assert_eq!(
        supervisor.observes.load(Ordering::SeqCst),
        observed,
        "release doesn't ask the supervisor about a unit from an earlier boot"
    );
    assert_eq!(released.phase, Phase::Terminal);
    assert_eq!(released.reason, Some(Reason::OperatorReleased));
    let evidence = released.operator_release.unwrap();
    assert_eq!(evidence.later_boot_id, Some(boot));
    assert!(evidence.supervisor.is_none());
    assert_eq!(evidence.absent_resources, vec![resource.to_string()]);
    c.admit(c.ledger().service_id().clone(), "second".into(), request())
        .await
        .expect("the release frees the slot");
}

#[tokio::test]
async fn cancel_leaves_a_run_from_an_earlier_boot_waiting_for_release() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    supervisor.unavailable.store(true, Ordering::SeqCst);
    drop(c);
    let c = coordinator_on(
        &root.0,
        archive,
        supervisor.clone(),
        Arc::new(Rebooted(random_id().unwrap())),
    )
    .await;
    let restarted = c.ledger().get(&ready.run_id).await.unwrap();
    assert_eq!(restarted.phase, Phase::Reconciling);
    assert_eq!(restarted.reason, Some(Reason::CoordinatorRestarted));
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(cancelled, restarted, "nothing is left to cancel");
    assert_eq!(c.ledger().get(&ready.run_id).await.unwrap(), restarted);
    assert_eq!(
        supervisor.stop_calls.load(Ordering::SeqCst),
        0,
        "no supervisor can stop a unit from an earlier boot"
    );
    let released = c
        .release(ready.run_id.clone(), "rebooted".into())
        .await
        .unwrap();
    assert_eq!(released.phase, Phase::Terminal);
}

/// A host whose boot ID can't be read.
struct UnreadableBoot;
impl Clock for UnreadableBoot {
    fn now(&self) -> Stamp {
        SystemClock.now()
    }
    fn boot_id(&self) -> Result<playtime_protocol::worker::ControlId> {
        Err(Error::Uncertain)
    }
}

#[tokio::test]
async fn an_unreadable_boot_never_holds_back_a_stop() {
    let root = Root::new();
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator_on(
        &root.0,
        archive(&root.0),
        supervisor.clone(),
        Arc::new(UnreadableBoot),
    )
    .await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(
        cancelled.cancel_delivery,
        Some(CancelDelivery::WorkerAcknowledged)
    );
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Accepted));
    assert_eq!(supervisor.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_leaves_a_run_that_never_started_a_worker_waiting_for_release() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let accepted = admit(&c).await;
    // The daemon stops before preparation submits a worker.
    drop(c);
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    let restarted = c.ledger().get(&accepted.run_id).await.unwrap();
    assert_eq!(restarted.phase, Phase::Reconciling);
    assert!(!restarted.start_attempted);
    let cancelled = c.cancel(accepted.run_id.clone()).await.unwrap();
    assert_eq!(cancelled, restarted, "nothing is left to cancel");
    assert_eq!(c.ledger().get(&accepted.run_id).await.unwrap(), restarted);
    let released = c
        .release(accepted.run_id.clone(), "never started".into())
        .await
        .unwrap();
    assert_eq!(released.phase, Phase::Terminal);
}

#[tokio::test]
async fn cancel_leaves_a_run_whose_worker_stopped_waiting_for_release() {
    let root = Root::new();
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive(&root.0), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    c.begin(ready.run_id.clone()).await.unwrap();
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    {
        let mut observation = supervisor.observation.lock().unwrap();
        observation.active_state = "unloaded".into();
        observation.main_pid = 0;
        observation.stopped = true;
    }
    after_refresh_floor().await;
    let lost = c.refresh(ready.run_id.clone()).await.unwrap();
    assert_eq!(lost.phase, Phase::Reconciling);
    assert_eq!(lost.reason, Some(Reason::WorkerUnavailable));
    assert!(lost.supervisor.as_ref().is_some_and(|s| s.stopped));
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(cancelled, lost, "nothing is left to cancel");
    assert_eq!(c.ledger().get(&ready.run_id).await.unwrap(), lost);
}

#[tokio::test]
async fn diagnostic_failure_preserves_stop_acceptance_across_restart() {
    let root = Root::new();
    let archive = archive(&root.0);
    let supervisor = Arc::new(ManagerFixture::new());
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let accepted = admit(&c).await;
    let ready = prepared(&c, &accepted).await;
    assert!(ready.supervisor.is_some());
    supervisor.diagnostics_failed.store(true, Ordering::SeqCst);
    // Restart while diagnostics are unavailable: the saved authenticated
    // invocation still authorizes a fresh manager check and stop submission.
    drop(c);
    let c = coordinator(&root.0, archive.clone(), supervisor.clone()).await;
    let cancelled = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(cancelled.stop_delivery, Some(StopDelivery::Accepted));
    assert!(cancelled.stop_accepted.is_some());
    assert_eq!(cancelled.phase, Phase::Reconciling);
    assert_eq!(cancelled.supervisor, ready.supervisor);
    assert_eq!(supervisor.stops.load(Ordering::SeqCst), 1);
    // An unavailable manager after acceptance also cannot erase the accepted
    // stop. It only prevents a new termination observation.
    supervisor.unavailable.store(true, Ordering::SeqCst);
    drop(c);
    let c = coordinator(&root.0, archive, supervisor.clone()).await;
    let retry = c.cancel(ready.run_id.clone()).await.unwrap();
    assert_eq!(retry.stop_delivery, Some(StopDelivery::Accepted));
    assert_eq!(retry.stop_accepted, cancelled.stop_accepted);
    assert_eq!(retry.phase, Phase::Reconciling);
    assert!(!retry.supervisor.unwrap().stopped);
    assert_eq!(supervisor.stops.load(Ordering::SeqCst), 1);
    supervisor
        .child
        .terminate(ready.prepared.as_ref().unwrap())
        .unwrap();
    assert!(matches!(
        c.admit(c.ledger().service_id().clone(), "second".into(), request())
            .await,
        Err(Error::Busy)
    ));
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
    db.execute_batch("DROP TABLE model_acceptances; DROP TABLE model_registrations; DROP TABLE recipe_saves; DROP TABLE recipe_revisions; DROP TABLE recipe_heads; PRAGMA user_version=2;").unwrap();
    drop(db);
    let ledger = Ledger::open(&root.0).await.unwrap();
    let restored = ledger.get(&saved.run_id).await.unwrap();
    assert_eq!(restored.service_id, saved.service_id);
    assert_eq!(restored.selection, saved.selection);
    assert_eq!(restored.owner_generation, saved.owner_generation);
    assert!(restored.supervisor.is_none());
}
