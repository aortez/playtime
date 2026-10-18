use super::{
    ports::{ArchivePort, Clock, Supervisor},
    *,
};
use playtime_core::RunId;
use playtime_protocol::worker::{Command, ControlId, Reply, WorkerState};
use playtime_worker_control::{Bootstrap, Client, FileLock, Grant};
use std::{future::Future, sync::Arc};
use tokio::sync::Mutex;

struct Inner {
    ledger: Ledger,
    archive: Arc<dyn ArchivePort>,
    supervisor: Arc<dyn Supervisor>,
    clock: Arc<dyn Clock>,
    operation: Arc<Mutex<()>>,
}
#[derive(Clone)]
pub struct Coordinator {
    inner: Arc<Inner>,
}
impl Coordinator {
    pub fn new(
        ledger: Ledger,
        archive: Arc<dyn ArchivePort>,
        supervisor: Arc<dyn Supervisor>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                ledger,
                archive,
                supervisor,
                clock,
                operation: Arc::new(Mutex::new(())),
            }),
        }
    }
    pub fn ledger(&self) -> &Ledger {
        &self.inner.ledger
    }
    /// Once admitted to this bounded actor, work belongs to the coordinator.
    /// Dropping a caller's response future cannot cancel a reservation/Begin.
    async fn owned<T: Send + 'static, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _guard = inner
                .operation
                .clone()
                .try_lock_owned()
                .map_err(|_| Error::Busy)?;
            operation(inner).await
        })
        .await
        .map_err(|_| Error::Invalid("coordinator operation failed"))?
    }
    pub async fn admit(
        &self,
        service_id: ControlId,
        key: String,
        request: AdmissionRequest,
    ) -> Result<Acceptance> {
        self.owned(move |inner| async move {
            if service_id != *inner.ledger.service_id() {
                return Err(Error::ServiceMismatch);
            }
            request.validate()?;
            if key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                return Err(Error::Invalid("invalid idempotency key"));
            }
            // Replay precedes today's preview, worker selection and busy checks.
            if let Some(admission) = inner.ledger.lookup(&key).await? {
                if admission.request != request {
                    return Err(Error::Conflict);
                }
                return Ok(Acceptance {
                    replayed: true,
                    admission,
                });
            }
            let root = inner.ledger.root().to_path_buf();
            let _run_lock = tokio::task::spawn_blocking(move || {
                FileLock::acquire(&root.join(".playtime/run.lock"))
            })
            .await
            .map_err(|_| Error::Uncertain)??;
            inner
                .supervisor
                .ensure_legacy_idle(inner.ledger.root())
                .await?;
            let selection = inner.archive.validate_preview(&request).await?;
            if selection.wall_ms == 0
                || selection.wall_ms > 86_400_000
                || selection.token_limit == 0
            {
                return Err(Error::Invalid("invalid frozen allocation"));
            }
            inner
                .ledger
                .reserve(key, request, selection, inner.clock.now())
                .await
        })
        .await
    }
    pub async fn start(&self, run: RunId) -> Result<Admission> {
        self.owned(move |inner| async move {
            let admission = inner.ledger.get(&run).await?;
            if admission.start_attempted {
                return inner.refresh(&run, Command::Status).await;
            }
            if admission.cancel_requested.is_some() {
                return Ok(admission);
            }
            if admission.phase != Phase::Preparing {
                return Err(Error::Uncertain);
            }
            let root = inner.ledger.root().to_path_buf();
            let run_lock = tokio::task::spawn_blocking(move || {
                FileLock::acquire(&root.join(".playtime/run.lock"))
            })
            .await
            .map_err(|_| Error::Uncertain)??;
            inner
                .supervisor
                .ensure_legacy_idle(inner.ledger.root())
                .await?;
            let prepared = match inner.archive.prepare(&admission).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    inner.uncertain(&run, Reason::PreparationFailed).await?;
                    return Err(error);
                }
            };
            if prepared.binding.run_id != run
                || prepared.binding.service_id != admission.service_id
                || prepared.binding.executable_sha256 != admission.selection.executable_sha256
                || !prepared.executable.is_absolute()
                || !prepared.bootstrap.is_absolute()
                || prepared.startup_deadline_boot_ms <= inner.clock.now().boot_ms
            {
                inner.uncertain(&run, Reason::PreparationFailed).await?;
                return Err(Error::Invalid("preparation identity or path mismatch"));
            }
            let saved = prepared.clone();
            inner
                .ledger
                .update(&run, "start_intent", move |record| {
                    if record.start_attempted || record.cancel_requested.is_some() {
                        return Err(Error::Uncertain);
                    }
                    record.prepared = Some(saved);
                    record.start_attempted = true;
                    record.phase = Phase::Starting;
                    Ok(())
                })
                .await?;
            let submitted = inner.supervisor.start(&prepared).await;
            // Start submission never waits for preparedness while holding this.
            drop(run_lock);
            match submitted {
                Ok(endpoint) if endpoint.binding == prepared.binding => {
                    inner
                        .ledger
                        .update(&run, "start_acknowledged", move |record| {
                            record.endpoint = Some(endpoint);
                            Ok(())
                        })
                        .await
                }
                _ => inner.uncertain(&run, Reason::StartUncertain).await,
            }
        })
        .await
    }
    pub async fn refresh(&self, run: RunId) -> Result<Admission> {
        self.owned(move |inner| async move { inner.refresh(&run, Command::Status).await })
            .await
    }
    pub async fn begin(&self, run: RunId) -> Result<Admission> {
        self.owned(move |inner| async move {
            let before = inner.ledger.get(&run).await?;
            if before.cancel_requested.is_some() {
                return Ok(before);
            }
            let admission = inner.refresh(&run, Command::Status).await?;
            if admission.phase == Phase::Reconciling {
                return Ok(admission);
            }
            if admission
                .last_status
                .as_ref()
                .is_none_or(|s| s.state != WorkerState::Prepared)
            {
                return Ok(admission);
            }
            let prepared = admission.prepared.clone().ok_or(Error::Uncertain)?;
            if inner.clock.now().boot_ms >= prepared.startup_deadline_boot_ms {
                return inner.uncertain(&run, Reason::BeginUncertain).await;
            }
            inner
                .ledger
                .update(&run, "begin_authorized", |record| {
                    if record.cancel_requested.is_some() {
                        return Err(Error::Uncertain);
                    }
                    record.begin_authorized = true;
                    Ok(())
                })
                .await?;
            let authorization = tokio::task::spawn_blocking(move || {
                Grant::new(prepared.private_dir, prepared.binding).authorize()
            })
            .await
            .map_err(|_| Error::Uncertain)?;
            if authorization.is_err() {
                return inner.uncertain(&run, Reason::BeginUncertain).await;
            }
            inner.refresh(&run, Command::Begin).await
        })
        .await
    }
    pub async fn cancel(&self, run: RunId) -> Result<Admission> {
        self.owned(move |inner| async move {
            let now = inner.clock.now();
            let admission = inner
                .ledger
                .update(&run, "cancel_requested", move |record| {
                    if record.phase == Phase::Terminal {
                        return Ok(());
                    }
                    record.cancel_requested.get_or_insert(now);
                    record.phase = Phase::Cancelling;
                    record.reason = Some(Reason::CancelRequested);
                    Ok(())
                })
                .await?;
            if admission.phase == Phase::Terminal {
                return Ok(admission);
            }
            if let Some(prepared) = admission.prepared {
                let revoked = tokio::task::spawn_blocking(move || {
                    Grant::new(prepared.private_dir, prepared.binding).revoke()
                })
                .await
                .map_err(|_| Error::Uncertain)?;
                if revoked.is_err() {
                    return inner.uncertain(&run, Reason::ControlUncertain).await;
                }
                return inner.refresh(&run, Command::Cancel).await;
            }
            Ok(admission)
        })
        .await
    }
}
impl Inner {
    async fn uncertain(&self, run: &RunId, reason: Reason) -> Result<Admission> {
        self.ledger
            .update(run, "ownership_uncertain", move |record| {
                record.phase = Phase::Reconciling;
                record.reason = Some(reason);
                Ok(())
            })
            .await
    }
    async fn refresh(&self, run: &RunId, command: Command) -> Result<Admission> {
        let admission = self.ledger.get(run).await?;
        let Some(prepared) = &admission.prepared else {
            return Ok(admission);
        };
        let endpoint = match self.supervisor.locate(prepared).await {
            Ok(endpoint) if endpoint.binding == prepared.binding => endpoint,
            _ => return self.uncertain(run, Reason::WorkerUnavailable).await,
        };
        let bootstrap_path = prepared.bootstrap.clone();
        // The coordinator reads credentials without invoking worker-only image
        // verification: its executing image need not be the selected worker.
        let bootstrap: Bootstrap = tokio::task::spawn_blocking(move || -> Result<Bootstrap> {
            Ok(serde_json::from_slice(
                &playtime_worker_control::read_private(&bootstrap_path, 65_536)?,
            )?)
        })
        .await
        .map_err(|_| Error::Uncertain)??;
        if bootstrap.binding != prepared.binding {
            return self.uncertain(run, Reason::ControlUncertain).await;
        }
        let client = Client::new(endpoint.clone(), bootstrap.credential);
        let reply = match client.request(command).await {
            Ok(reply) => reply,
            Err(_) => return self.uncertain(run, Reason::ControlUncertain).await,
        };
        self.accept(admission, endpoint, reply).await
    }
    async fn accept(
        &self,
        admission: Admission,
        endpoint: playtime_worker_control::Endpoint,
        reply: Reply,
    ) -> Result<Admission> {
        let prepared = admission.prepared.as_ref().ok_or(Error::Uncertain)?;
        let prior = admission
            .last_status
            .as_ref()
            .and_then(|s| s.latest_receipt.as_ref());
        let status = reply.status;
        let shape_valid = status.wall_ms == admission.selection.wall_ms
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
        let clock_valid = admission
            .last_status
            .as_ref()
            .and_then(|old| old.work_started_boot_ms)
            .is_none_or(|old| status.work_started_boot_ms == Some(old))
            && status.work_started_boot_ms.is_none_or(|started| {
                admission.begin_authorized
                    && started < prepared.startup_deadline_boot_ms
                    && started <= self.clock.now().boot_ms
            });
        if !shape_valid
            || !clock_valid
            || self
                .archive
                .verify_status(prepared, prior, &status)
                .await
                .is_err()
        {
            return self
                .uncertain(&admission.run_id, Reason::ControlUncertain)
                .await;
        }
        self.ledger
            .update(&admission.run_id, "worker_confirmed", move |record| {
                if record.prepared.as_ref().map(|p| &p.binding) != Some(&endpoint.binding) {
                    return Err(Error::Uncertain);
                }
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
    }
}
