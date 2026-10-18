//! Authentication and archive verification are separate from cancellation.
//! Only the guarded ledger commit makes a confirmation visible to callers.

use super::{coordinator::Inner, *};
use playtime_core::RunId;
use playtime_protocol::worker::{self, Command, Reply, WorkerState};
use playtime_worker_control::{Bootstrap, Client, Endpoint};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

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
                match inner.exchange(&record, Command::Status).await {
                    Ok((endpoint, reply)) => inner.accept(record, endpoint, reply).await,
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
        if !shape_valid
            || !clock_valid
            || !receipt_valid
            || self
                .archive
                .verify_status(prepared, prior, &status)
                .await
                .is_err()
        {
            return self.uncertain(&expected, Reason::ControlUncertain).await;
        }
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
        entered: Notify,
        release: Notify,
    }
    #[async_trait]
    impl ArchivePort for Archive {
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
        async fn start(&self, _: &PreparedLaunch) -> Result<Endpoint> {
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
}
