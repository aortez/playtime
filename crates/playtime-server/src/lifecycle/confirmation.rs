//! Authentication and archive verification are separate from cancellation.
//! Only the guarded ledger commit makes a confirmation visible to callers.

use super::{coordinator::Inner, *};
use playtime_core::RunId;
use playtime_protocol::worker::{self, Command, Reply, WorkerState};
use playtime_worker_control::{Bootstrap, Client, Endpoint};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

/// A worker whose unit is up but that hasn't answered yet, with no cancel
/// requested and its startup deadline still ahead.
fn starting(record: &Admission, now_boot_ms: u64) -> bool {
    record.last_status.is_none()
        && record.cancel_requested.is_none()
        && record
            .prepared
            .as_ref()
            .is_some_and(|p| now_boot_ms < p.startup_deadline_boot_ms)
        && record.supervisor.as_ref().is_some_and(|s| {
            !s.stopped && matches!(s.active_state.as_str(), "activating" | "active")
        })
}

impl Inner {
    pub(super) async fn shared_refresh(self: &Arc<Self>, run: RunId) -> Result<Admission> {
        let inner = self.clone();
        let id = run.clone();
        match self
            .refreshes
            .run(run.clone(), move || async move {
                let record = inner.ledger.get(&id).await?;
                if record.phase == Phase::Terminal || record.prepared.is_none() {
                    return Ok(record);
                }
                let record = inner.observe_supervisor(record).await?;
                // A worker the daemon stopped on purpose has nothing more to
                // report and stops answering once told to stop. Only its unit
                // is watched, so a slow stop isn't taken for a loss.
                if record.supervisor.as_ref().is_some_and(|s| s.stopped)
                    || super::supervision::normal_stop(&record)
                {
                    return Ok(record);
                }
                match inner.exchange(&record, Command::Status).await {
                    Ok((endpoint, reply)) => inner.accept(record, endpoint, reply).await,
                    // Its first answer can trail the unit's start. Until the
                    // startup deadline, that isn't uncertainty or loss.
                    Err(_) if starting(&record, inner.clock.now().boot_ms) => Ok(record),
                    Err(reason) => inner.uncertain(&record, reason).await,
                }
            })
            .await?
        {
            Some(record) => Ok(record),
            None => self.ledger.get(&run).await,
        }
    }
    pub(super) async fn uncertain(
        &self,
        expected: &Admission,
        reason: Reason,
    ) -> Result<Admission> {
        self.ledger
            .compare_update(expected, "ownership_uncertain", move |record| {
                record.phase = Phase::Reconciling;
                record.reason = Some(reason);
                Ok(())
            })
            .await
            .map(|(record, _)| record)
    }
    pub(super) async fn command_begin(self: &Arc<Self>, expected: Admission) -> Result<Admission> {
        match self.exchange(&expected, Command::Begin).await {
            Ok((endpoint, reply)) => self.accept(expected, endpoint, reply).await,
            Err(reason) => self.uncertain(&expected, reason).await,
        }
    }
    /// Three independent, single-operation lanes. If a preflight blocks beyond
    /// the caller's deadline, its task retains the lane until it really settles;
    /// repeated calls cannot accumulate abandoned filesystem operations.
    pub(super) async fn exchange(
        self: &Arc<Self>,
        expected: &Admission,
        command: Command,
    ) -> std::result::Result<(Endpoint, Reply), Reason> {
        let lane = match command {
            Command::Status => &self.status_io,
            Command::Begin => &self.begin_io,
            Command::Cancel => &self.cancel_io,
        };
        let permit = lane
            .clone()
            .try_acquire_owned()
            .map_err(|_| Reason::ControlUncertain)?;
        let inner = self.clone();
        let expected = expected.clone();
        let deadline = Instant::now() + Duration::from_secs(worker::REQUEST_SECONDS);
        let mut task = tokio::spawn(async move {
            let _permit = permit;
            let prepared = expected.prepared.as_ref().ok_or(Reason::ControlUncertain)?;
            let endpoint = inner
                .supervisor
                .locate(prepared)
                .await
                .map_err(|_| Reason::WorkerUnavailable)?;
            if endpoint.binding != prepared.binding {
                return Err(Reason::WorkerUnavailable);
            }
            if Instant::now() >= deadline {
                return Err(Reason::ControlUncertain);
            }
            let path = prepared.bootstrap.clone();
            let bootstrap: Bootstrap = tokio::task::spawn_blocking(move || -> Result<Bootstrap> {
                Ok(serde_json::from_slice(
                    &playtime_worker_control::read_private(&path, 65_536)?,
                )?)
            })
            .await
            .map_err(|_| Reason::ControlUncertain)?
            .map_err(|_| Reason::ControlUncertain)?;
            if bootstrap.version != worker::VERSION
                || bootstrap.binding != prepared.binding
                || bootstrap.wall_ms != expected.selection.wall_ms
                || bootstrap.startup_deadline_boot_ms != prepared.startup_deadline_boot_ms
                || Instant::now() >= deadline
            {
                return Err(Reason::ControlUncertain);
            }
            let current = inner
                .ledger
                .get(&expected.run_id)
                .await
                .map_err(|_| Reason::ControlUncertain)?;
            if Instant::now() >= deadline
                || current.owner_generation != expected.owner_generation
                || current.phase == Phase::Terminal
                || current.prepared.as_ref().map(|p| &p.binding) != Some(&prepared.binding)
                || (command == Command::Begin
                    && (current.cancel_requested.is_some()
                        || current.revision != expected.revision))
            {
                return Err(Reason::ControlUncertain);
            }
            let client = Client::new(endpoint.clone(), bootstrap.credential);
            let reply = timeout_at(deadline, client.request(command))
                .await
                .map_err(|_| Reason::ControlUncertain)?
                .map_err(|_| Reason::ControlUncertain)?;
            Ok((endpoint, reply))
        });
        // Dropping the JoinHandle after timeout detaches its bounded preflight;
        // it cannot publish evidence or send a command after its deadline.
        timeout_at(deadline, &mut task)
            .await
            .map_err(|_| Reason::ControlUncertain)?
            .map_err(|_| Reason::ControlUncertain)?
    }

    async fn accept(
        &self,
        expected: Admission,
        endpoint: Endpoint,
        reply: Reply,
    ) -> Result<Admission> {
        let _verification = self.evidence.lock().await;
        let current = self.ledger.get(&expected.run_id).await?;
        if current.revision != expected.revision
            || current.owner_generation != expected.owner_generation
            || current.phase == Phase::Terminal
        {
            return Ok(current);
        }
        let prepared = expected.prepared.as_ref().ok_or(Error::Uncertain)?;
        let status = reply.status;
        let prior = expected
            .last_status
            .as_ref()
            .and_then(|s| s.latest_receipt.as_ref());
        let shape_valid = status.wall_ms == expected.selection.wall_ms
            && match status.state {
                WorkerState::Prepared | WorkerState::CancelledBeforeBegin => {
                    status.work_started_boot_ms.is_none()
                        && status
                            .latest_receipt
                            .as_ref()
                            .is_some_and(|r| r.sequence == 0)
                }
                WorkerState::Running | WorkerState::Cancelling => {
                    status.work_started_boot_ms.is_some()
                        && status
                            .latest_receipt
                            .as_ref()
                            .is_some_and(|r| r.sequence > 0)
                }
                WorkerState::Reconciling => true,
            };
        // Only an execution that began can fail, and the worker reports why
        // once, in bounded printable text it never changes or withdraws.
        let failure_valid = status.failure.as_ref().is_none_or(|text| {
            status.state == WorkerState::Reconciling
                && status.work_started_boot_ms.is_some()
                && worker::valid_failure(text)
        }) && expected
            .last_status
            .as_ref()
            .and_then(|old| old.failure.as_ref())
            .is_none_or(|old| status.failure.as_ref() == Some(old));
        let clock_valid = expected
            .last_status
            .as_ref()
            .and_then(|old| old.work_started_boot_ms)
            .is_none_or(|old| status.work_started_boot_ms == Some(old))
            && status.work_started_boot_ms.is_none_or(|started| {
                expected.begin_authorized
                    && started < prepared.startup_deadline_boot_ms
                    && started <= self.clock.now().boot_ms
            });
        // Reject regressions before asking the adapter to validate exact prefix
        // extension. The adapter still verifies content, including equal replies.
        let receipt_valid = prior.is_none_or(|old| {
            status.latest_receipt.as_ref().is_some_and(|new| {
                new.sequence >= old.sequence
                    && new.bytes >= old.bytes
                    && (new.sequence != old.sequence || new == old)
            })
        });
        if !shape_valid || !failure_valid || !clock_valid || !receipt_valid {
            return self.uncertain(&expected, Reason::ControlUncertain).await;
        }
        let frontier = match self.archive.confirmed_read(prepared, prior, &status).await {
            Ok(frontier) => frontier,
            Err(_) => return self.uncertain(&expected, Reason::ControlUncertain).await,
        };
        self.ledger
            .compare_update(&expected, "worker_confirmed", move |record| {
                record.phase = if record.cancel_requested.is_some() {
                    Phase::Cancelling
                } else {
                    match status.state {
                        WorkerState::Prepared => Phase::Starting,
                        WorkerState::Running => Phase::Running,
                        WorkerState::CancelledBeforeBegin | WorkerState::Cancelling => {
                            Phase::Cancelling
                        }
                        WorkerState::Reconciling => Phase::Reconciling,
                    }
                };
                record.reason = if record.cancel_requested.is_some() {
                    Some(Reason::CancelRequested)
                } else {
                    None
                };
                record.endpoint = Some(endpoint);
                record.last_status = Some(status);
                if let Some(frontier) = frontier {
                    record.confirmed_read = Some(ConfirmedRead {
                        frontier,
                        owner_generation: record.owner_generation,
                        source: ConfirmationSource::Worker,
                    });
                }
                Ok(())
            })
            .await
            .map(|(record, _)| record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::ports::{ArchivePort, Clock, Supervisor};
    use async_trait::async_trait;
    use playtime_core::{RunMode, UtcTime};
    use playtime_protocol::worker::{Binding, Receipt, Status, WorkerKind};
    use playtime_worker_control::{boot_id, digest, random_id};
    use std::{
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::Notify;

    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[derive(Default)]
    struct Archive {
        calls: AtomicUsize,
        block: AtomicBool,
        complete: AtomicBool,
        entered: Notify,
        release: Notify,
    }
    #[async_trait]
    impl ArchivePort for Archive {
        async fn normal_completion(&self, _: &Admission) -> Result<bool> {
            Ok(self.complete.load(Ordering::SeqCst))
        }
        async fn validate_preview(&self, _: &AdmissionRequest) -> Result<Selection> {
            unreachable!()
        }
        async fn prepare(&self, _: &Admission) -> Result<PreparedLaunch> {
            unreachable!()
        }
        async fn verify_status(
            &self,
            _: &PreparedLaunch,
            previous: Option<&Receipt>,
            status: &Status,
        ) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let new = status.latest_receipt.as_ref().unwrap();
            let bytes = b"a\nb\n";
            let len = usize::try_from(new.bytes).map_err(|_| Error::Uncertain)?;
            if len > bytes.len() || new.sha256 != digest(&bytes[..len]) {
                return Err(Error::Uncertain);
            }
            if let Some(old) = previous
                && digest(&bytes[..old.bytes as usize]) != old.sha256
            {
                return Err(Error::Uncertain);
            }
            Ok(())
        }
    }
    struct NoSupervisor;
    #[async_trait]
    impl Supervisor for NoSupervisor {
        async fn ensure_legacy_idle(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        async fn start(&self, _: &PreparedLaunch) -> Result<Option<Endpoint>> {
            unreachable!()
        }
        async fn locate(&self, _: &PreparedLaunch) -> Result<Endpoint> {
            unreachable!()
        }
    }
    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> Stamp {
            Stamp {
                utc: UtcTime::new("2026-10-18T00:00:00Z").unwrap(),
                boot_ms: 1000,
            }
        }
    }
    fn receipt(sequence: u64) -> Receipt {
        Receipt {
            sequence,
            bytes: sequence * 2,
            sha256: digest(&b"a\nb\n"[..sequence as usize * 2]),
        }
    }
    async fn setup() -> (Root, Coordinator, Arc<Archive>, Admission, Endpoint) {
        let root = Root(
            std::env::temp_dir().join(format!("pt34-confirm-{}", random_id().unwrap().as_str())),
        );
        std::fs::create_dir(&root.0).unwrap();
        let ledger = Ledger::open(&root.0).await.unwrap();
        let selection = Selection {
            executable_sha256: digest(b"fixture"),
            wall_ms: 1000,
            token_limit: 10,
        };
        let record = ledger
            .reserve(
                "fixture-key".into(),
                AdmissionRequest {
                    target: None,
                    recipe_id: "fixture".into(),
                    mode: RunMode::Simulation,
                    preview_revision: "fixture".into(),
                },
                selection,
                FixedClock.now(),
            )
            .await
            .unwrap()
            .admission;
        let endpoint = Endpoint {
            socket: "/fixture/control.sock".into(),
            uid: 1000,
            pid: 123,
            binding: Binding {
                service_id: ledger.service_id().clone(),
                run_id: record.run_id.clone(),
                manifest_sha256: digest(b"manifest"),
                executable_sha256: digest(b"fixture"),
                grant_id: random_id().unwrap(),
                worker_id: random_id().unwrap(),
                invocation_id: random_id().unwrap(),
                boot_id: boot_id().unwrap(),
                unit: "fixture.service".into(),
                kind: WorkerKind::SupervisionFixture,
            },
        };
        let saved_endpoint = endpoint.clone();
        let record = ledger
            .update(&record.run_id, "test_prepared", move |record| {
                record.prepared = Some(PreparedLaunch {
                    binding: saved_endpoint.binding.clone(),
                    executable: "/fixture/worker".into(),
                    argv: vec![],
                    bootstrap: "/fixture/bootstrap".into(),
                    private_dir: "/fixture".into(),
                    socket: saved_endpoint.socket.clone(),
                    startup_deadline_boot_ms: 2000,
                });
                record.endpoint = Some(saved_endpoint);
                record.start_attempted = true;
                record.begin_authorized = true;
                record.phase = Phase::Running;
                record.last_status = Some(Status {
                    state: WorkerState::Running,
                    work_started_boot_ms: Some(10),
                    wall_ms: 1000,
                    latest_receipt: Some(receipt(1)),
                    publication_pending: false,
                    failure: None,
                });
                Ok(())
            })
            .await
            .unwrap();
        let archive = Arc::new(Archive::default());
        let c = Coordinator::new(
            ledger,
            archive.clone(),
            Arc::new(NoSupervisor),
            Arc::new(FixedClock),
        );
        (root, c, archive, record, endpoint)
    }
    fn reply(record: &Admission, endpoint: &Endpoint, receipt: Receipt) -> Reply {
        let mut status = record.last_status.clone().unwrap();
        status.latest_receipt = Some(receipt);
        Reply {
            version: worker::VERSION,
            nonce: random_id().unwrap(),
            binding: endpoint.binding.clone(),
            status,
        }
    }
    #[tokio::test]
    async fn regressions_conflicts_and_invalid_extensions_preserve_confirmed_evidence() {
        let (_root, c, archive, mut record, endpoint) = setup().await;
        let original = record.last_status.clone();
        let mut conflict = receipt(1);
        conflict.sha256 = digest(b"different");
        let mut invalid_extension = receipt(2);
        invalid_extension.sha256 = digest(b"not the prefix");
        for invalid in [receipt(0), conflict, invalid_extension] {
            let response = reply(&record, &endpoint, invalid);
            record = c
                .inner
                .accept(record, endpoint.clone(), response)
                .await
                .unwrap();
            assert_eq!(record.phase, Phase::Reconciling);
            assert_eq!(record.last_status, original);
        }
        assert_eq!(
            archive.calls.load(Ordering::SeqCst),
            1,
            "structural conflicts are rejected before archive I/O"
        );
        let response = reply(&record, &endpoint, receipt(2));
        record = c
            .inner
            .accept(record, endpoint.clone(), response)
            .await
            .unwrap();
        assert_eq!(record.phase, Phase::Running);
        let revision = record.revision;
        let response = reply(&record, &endpoint, receipt(2));
        record = c.inner.accept(record, endpoint, response).await.unwrap();
        assert_eq!(record.revision, revision);
        assert_eq!(
            archive.calls.load(Ordering::SeqCst),
            3,
            "equal confirmations still verify bytes"
        );
    }
    #[tokio::test]
    async fn ownership_generation_change_fences_a_verification_already_in_progress() {
        let (_root, c, archive, record, endpoint) = setup().await;
        archive.block.store(true, Ordering::SeqCst);
        let response = reply(&record, &endpoint, receipt(2));
        let pending = {
            let (inner, expected) = (c.inner.clone(), record.clone());
            tokio::spawn(async move { inner.accept(expected, endpoint, response).await })
        };
        archive.entered.notified().await;
        let superseded = c
            .ledger()
            .update(&record.run_id, "test_ownership_fence", |record| {
                record.owner_generation += 1;
                record.phase = Phase::Reconciling;
                record.reason = Some(Reason::WorkerUnavailable);
                Ok(())
            })
            .await
            .unwrap();
        archive.release.notify_one();
        let late = pending.await.unwrap().unwrap();
        assert_eq!(late, superseded);
        assert_eq!(late.last_status, record.last_status);
        assert_eq!(c.ledger().get(&record.run_id).await.unwrap(), superseded);
    }
    fn observation(launch: &PreparedLaunch, active_state: &str) -> SupervisorObservation {
        let stopped = active_state == "inactive";
        SupervisorObservation {
            manager_id: launch.binding.invocation_id.clone(),
            cgroup: "/synthetic-test".into(),
            active_state: active_state.into(),
            main_pid: if stopped { 0 } else { 123 },
            ready_monotonic_us: 0,
            stopping_monotonic_us: 0,
            result: "test".into(),
            stopped,
            exit: None,
        }
    }
    async fn confirm(ledger: &Ledger, run: &RunId) {
        let reference = playtime_storage::content_ref(
            playtime_core::ArchivePath::new("test").unwrap(),
            b"test",
        );
        let identity = playtime_protocol::storage::ArchiveIdentity {
            run_id: run.clone(),
            descriptor: reference.clone(),
            input_manifest: reference.clone(),
        };
        ledger
            .update(run, "test_confirmed", move |record| {
                record.confirmed_read = Some(ConfirmedRead {
                    frontier: playtime_protocol::storage::EventFrontier {
                        archive: identity,
                        marker: reference,
                        committed_sequence: 1,
                        committed_bytes: 2,
                        prefix_sha256: digest(b"a\n"),
                    },
                    owner_generation: 1,
                    source: ConfirmationSource::Worker,
                });
                Ok(())
            })
            .await
            .unwrap();
    }
    /// A running unit whose worker has no control endpoint yet.
    struct Unreachable;
    #[async_trait]
    impl Supervisor for Unreachable {
        async fn ensure_legacy_idle(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        async fn start(&self, _: &PreparedLaunch) -> Result<Option<Endpoint>> {
            unreachable!()
        }
        async fn locate(&self, _: &PreparedLaunch) -> Result<Endpoint> {
            Err(Error::Uncertain)
        }
        async fn observe(&self, launch: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
            Ok(Some(observation(launch, "activating")))
        }
    }
    struct FinishSupervisor {
        stopped: AtomicBool,
        stop_calls: AtomicUsize,
        ledger: Ledger,
        change_owner: bool,
        /// An accepted stop leaves the unit deactivating until the test stops it.
        linger: bool,
    }
    #[async_trait]
    impl Supervisor for FinishSupervisor {
        async fn ensure_legacy_idle(&self, _: &Path) -> Result<()> {
            Ok(())
        }
        async fn start(&self, _: &PreparedLaunch) -> Result<Option<Endpoint>> {
            unreachable!()
        }
        async fn locate(&self, _: &PreparedLaunch) -> Result<Endpoint> {
            unreachable!()
        }
        async fn observe(&self, launch: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
            let state = if self.stopped.load(Ordering::SeqCst) {
                "inactive"
            } else if self.stop_calls.load(Ordering::SeqCst) > 0 {
                "deactivating"
            } else {
                "active"
            };
            Ok(Some(observation(launch, state)))
        }
        async fn stop(&self, launch: &PreparedLaunch, _: &SupervisorObservation) -> Result<()> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            if !self.linger {
                self.stopped.store(true, Ordering::SeqCst);
            }
            if self.change_owner {
                self.ledger
                    .update(&launch.binding.run_id, "test_new_owner", |record| {
                        record.owner_generation += 1;
                        Ok(())
                    })
                    .await?;
            }
            Ok(())
        }
    }
    #[tokio::test]
    async fn normal_release_requires_settled_confirmed_terminal_and_original_owner_termination() {
        for change_owner in [false, true] {
            let (_root, old, archive, record, _) = setup().await;
            let ledger = old.ledger().clone();
            let supervisor = Arc::new(FinishSupervisor {
                stopped: AtomicBool::new(false),
                stop_calls: AtomicUsize::new(0),
                ledger: ledger.clone(),
                change_owner,
                linger: false,
            });
            let coordinator = Coordinator::new(
                ledger.clone(),
                archive.clone(),
                supervisor.clone(),
                Arc::new(FixedClock),
            );
            let run = record.run_id.clone();
            let record = coordinator.finish_normal(run.clone()).await.unwrap();
            assert_ne!(
                record.phase,
                Phase::Terminal,
                "missing trusted read boundary"
            );
            confirm(&ledger, &run).await;
            ledger
                .update(&run, "test_pending", |record| {
                    record.last_status.as_mut().unwrap().publication_pending = true;
                    Ok(())
                })
                .await
                .unwrap();
            archive.complete.store(true, Ordering::SeqCst);
            assert_ne!(
                coordinator.finish_normal(run.clone()).await.unwrap().phase,
                Phase::Terminal
            );
            assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 0);
            ledger
                .update(&run, "test_settled", |record| {
                    record.last_status.as_mut().unwrap().publication_pending = false;
                    Ok(())
                })
                .await
                .unwrap();
            archive.complete.store(false, Ordering::SeqCst);
            assert_ne!(
                coordinator.finish_normal(run.clone()).await.unwrap().phase,
                Phase::Terminal
            );
            assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 0);
            archive.complete.store(true, Ordering::SeqCst);
            let final_record = coordinator.finish_normal(run.clone()).await.unwrap();
            assert_eq!(final_record.phase == Phase::Terminal, !change_owner);
            let next = ledger
                .reserve(
                    "next".into(),
                    record.request.clone(),
                    record.selection.clone(),
                    FixedClock.now(),
                )
                .await;
            assert_eq!(
                next.is_ok(),
                !change_owner,
                "only proven normal completion releases admission"
            );
            let trace = ledger.trace(&run).await.unwrap();
            if !change_owner {
                assert_eq!(trace.last().unwrap(), "normal_completed");
                assert!(final_record.supervisor.unwrap().stopped);
                assert!(final_record.stop_accepted.is_some());
            }
        }
    }
    // Clients wait until a run is Terminal or reports worker loss (#93), so a
    // worker that is starting or finishing normally must not report its loss.
    #[tokio::test]
    async fn a_starting_worker_that_cannot_answer_yet_is_not_reported_lost() {
        // FixedClock is at boot 1000 ms. Past the startup deadline, or after a
        // cancel, an unreachable worker is lost as before.
        for (deadline, cancelled, still_starting) in [
            (2000, false, true),
            (1000, false, false),
            (2000, true, false),
        ] {
            let (_root, old, archive, record, _) = setup().await;
            let ledger = old.ledger().clone();
            ledger
                .update(&record.run_id, "test_starting", move |record| {
                    record.phase = Phase::Starting;
                    record.last_status = None;
                    record.prepared.as_mut().unwrap().startup_deadline_boot_ms = deadline;
                    if cancelled {
                        record.cancel_requested = Some(FixedClock.now());
                    }
                    Ok(())
                })
                .await
                .unwrap();
            let coordinator =
                Coordinator::new(ledger, archive, Arc::new(Unreachable), Arc::new(FixedClock));
            let refreshed = coordinator.refresh(record.run_id).await.unwrap();
            assert_eq!(
                (refreshed.phase, refreshed.reason),
                if still_starting {
                    (Phase::Starting, None)
                } else {
                    (Phase::Reconciling, Some(Reason::WorkerUnavailable))
                }
            );
        }
    }
    #[tokio::test]
    async fn stopping_a_normally_completed_worker_never_reports_its_loss() {
        for still_completes in [true, false] {
            let (_root, old, archive, record, _) = setup().await;
            let ledger = old.ledger().clone();
            let supervisor = Arc::new(FinishSupervisor {
                stopped: AtomicBool::new(false),
                stop_calls: AtomicUsize::new(0),
                ledger: ledger.clone(),
                change_owner: false,
                linger: true,
            });
            let coordinator = Coordinator::new(
                ledger.clone(),
                archive.clone(),
                supervisor.clone(),
                Arc::new(FixedClock),
            );
            let run = record.run_id.clone();
            confirm(&ledger, &run).await;
            archive.complete.store(true, Ordering::SeqCst);
            let stopping = coordinator.finish_normal(run.clone()).await.unwrap();
            assert_eq!(stopping.phase, Phase::Running);
            assert!(stopping.stop_accepted.is_some());
            // While the unit lingers, a refresh only watches it.
            let lingering = coordinator.refresh(run.clone()).await.unwrap();
            assert_eq!((lingering.phase, lingering.reason), (Phase::Running, None));
            // The next refresh sees the unit stopped. The run stays Running.
            tokio::time::sleep(crate::lifecycle::flights::REFRESH_FLOOR).await;
            supervisor.stopped.store(true, Ordering::SeqCst);
            let observed = coordinator.refresh(run.clone()).await.unwrap();
            assert_eq!((observed.phase, observed.reason), (Phase::Running, None));
            assert!(observed.supervisor.unwrap().stopped);
            archive.complete.store(still_completes, Ordering::SeqCst);
            let finished = coordinator.finish_normal(run.clone()).await.unwrap();
            if still_completes {
                assert_eq!((finished.phase, finished.reason), (Phase::Terminal, None));
            } else {
                // A completion that no longer qualifies is lost like any other.
                assert_eq!(
                    (finished.phase, finished.reason),
                    (Phase::Reconciling, Some(Reason::WorkerUnavailable))
                );
            }
            assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 1);
        }
    }
    #[tokio::test]
    async fn a_worker_that_stops_unasked_or_for_a_cancel_is_lost() {
        for cancel in [false, true] {
            let (_root, old, archive, record, _) = setup().await;
            let ledger = old.ledger().clone();
            let supervisor = Arc::new(FinishSupervisor {
                stopped: AtomicBool::new(!cancel),
                stop_calls: AtomicUsize::new(0),
                ledger: ledger.clone(),
                change_owner: false,
                linger: false,
            });
            let coordinator =
                Coordinator::new(ledger, archive, supervisor.clone(), Arc::new(FixedClock));
            let lost = if cancel {
                coordinator.cancel(record.run_id).await.unwrap()
            } else {
                coordinator.refresh(record.run_id).await.unwrap()
            };
            assert_eq!(
                (lost.phase, lost.reason),
                (Phase::Reconciling, Some(Reason::WorkerUnavailable))
            );
            assert_eq!(
                supervisor.stop_calls.load(Ordering::SeqCst),
                usize::from(cancel)
            );
        }
    }
    #[tokio::test]
    async fn a_failure_report_is_accepted_only_whole_and_never_withdrawn() {
        let (_root, c, _archive, mut record, endpoint) = setup().await;
        let failed = |record: &Admission, state, failure: Option<&str>| {
            let mut response = reply(record, &endpoint, receipt(2));
            response.status.state = state;
            response.status.failure = failure.map(str::to_owned);
            response
        };
        // Only a reconciling worker can report a failure, in printable text.
        for response in [
            failed(&record, WorkerState::Running, Some("lost publication")),
            failed(&record, WorkerState::Reconciling, Some("lost\npublication")),
        ] {
            record = c
                .inner
                .accept(record, endpoint.clone(), response)
                .await
                .unwrap();
            assert_eq!(record.reason, Some(Reason::ControlUncertain));
            assert_eq!(record.last_status.as_ref().unwrap().failure, None);
        }
        let response = failed(&record, WorkerState::Reconciling, Some("lost publication"));
        record = c
            .inner
            .accept(record, endpoint.clone(), response)
            .await
            .unwrap();
        assert_eq!((record.phase, record.reason), (Phase::Reconciling, None));
        assert_eq!(
            record.last_status.as_ref().unwrap().failure.as_deref(),
            Some("lost publication")
        );
        // Once reported, the failure can't change or disappear.
        for failure in [None, Some("another cause")] {
            let response = failed(&record, WorkerState::Reconciling, failure);
            record = c
                .inner
                .accept(record, endpoint.clone(), response)
                .await
                .unwrap();
            assert_eq!(record.reason, Some(Reason::ControlUncertain));
            assert_eq!(
                record.last_status.as_ref().unwrap().failure.as_deref(),
                Some("lost publication")
            );
        }
    }
    // #162: a worker whose execution failed has nothing left to publish, so it
    // is stopped at once rather than holding its locks until its runtime limit.
    #[tokio::test]
    async fn a_worker_that_reports_a_failed_execution_is_stopped_and_stays_reserved() {
        for (linger, cancelled) in [(false, false), (true, false), (false, true)] {
            let (_root, old, archive, record, _) = setup().await;
            let ledger = old.ledger().clone();
            let supervisor = Arc::new(FinishSupervisor {
                stopped: AtomicBool::new(false),
                stop_calls: AtomicUsize::new(0),
                ledger: ledger.clone(),
                change_owner: false,
                linger,
            });
            let coordinator = Coordinator::new(
                ledger.clone(),
                archive,
                supervisor.clone(),
                Arc::new(FixedClock),
            );
            let run = record.run_id.clone();
            // As a verified status saves it, with its publication unsettled.
            ledger
                .update(&run, "test_failed", move |record| {
                    record.phase = Phase::Reconciling;
                    if cancelled {
                        record.cancel_requested = Some(FixedClock.now());
                    }
                    let status = record.last_status.as_mut().unwrap();
                    status.state = WorkerState::Reconciling;
                    status.failure = Some("lost publication".into());
                    status.publication_pending = true;
                    Ok(())
                })
                .await
                .unwrap();
            let waiting = coordinator.finish_failed(run.clone()).await.unwrap();
            assert_eq!((waiting.phase, waiting.reason), (Phase::Reconciling, None));
            ledger
                .update(&run, "test_settled", |record| {
                    record.last_status.as_mut().unwrap().publication_pending = false;
                    Ok(())
                })
                .await
                .unwrap();
            let stopping = coordinator.finish_failed(run.clone()).await.unwrap();
            if cancelled {
                // The cancellation stops its worker on its own path.
                assert_eq!(stopping.reason, None);
                assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 0);
                continue;
            }
            assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 1);
            let failed = if linger {
                assert_eq!(
                    (stopping.phase, stopping.reason),
                    (Phase::Reconciling, None)
                );
                assert!(stopping.stop_accepted.is_some());
                // While the unit lingers, a refresh only watches it.
                let lingering = coordinator.refresh(run.clone()).await.unwrap();
                assert_eq!(
                    (lingering.phase, lingering.reason),
                    (Phase::Reconciling, None)
                );
                // The stop the daemon asked for isn't reported as a loss.
                tokio::time::sleep(crate::lifecycle::flights::REFRESH_FLOOR).await;
                supervisor.stopped.store(true, Ordering::SeqCst);
                let observed = coordinator.refresh(run.clone()).await.unwrap();
                assert!(observed.supervisor.unwrap().stopped);
                assert_eq!(observed.reason, None);
                coordinator.finish_failed(run.clone()).await.unwrap()
            } else {
                stopping
            };
            assert_eq!(
                (failed.phase, failed.reason),
                (Phase::Reconciling, Some(Reason::ExecutionFailed))
            );
            assert_eq!(supervisor.stop_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                ledger.trace(&run).await.unwrap().last().unwrap(),
                "execution_failed"
            );
            // Normal completion leaves it alone, and it keeps its reservation
            // until an operator releases it.
            let kept = coordinator.finish_normal(run.clone()).await.unwrap();
            assert_eq!(kept.reason, Some(Reason::ExecutionFailed));
            let next = ledger
                .reserve(
                    "next".into(),
                    record.request.clone(),
                    record.selection.clone(),
                    FixedClock.now(),
                )
                .await;
            assert!(next.is_err());
        }
    }
}
