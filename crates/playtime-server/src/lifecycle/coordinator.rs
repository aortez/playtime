use super::flights::Flights;
use super::{
    ports::{ArchivePort, Clock, Supervisor},
    *,
};
use playtime_core::RunId;
use playtime_protocol::worker::{Command, ControlId, WorkerState};
use playtime_worker_control::{FileLock, Grant};
use std::{future::Future, sync::Arc};
use tokio::sync::{Mutex, Semaphore};

pub(super) struct Inner {
    pub(super) ledger: Ledger,
    pub(super) archive: Arc<dyn ArchivePort>,
    pub(super) supervisor: Arc<dyn Supervisor>,
    pub(super) clock: Arc<dyn Clock>,
    launch: Arc<Mutex<()>>,
    pub(super) evidence: Mutex<()>,
    pub(super) refreshes: Flights,
    cancellations: Flights,
    pub(super) status_io: Arc<Semaphore>,
    pub(super) begin_io: Arc<Semaphore>,
    pub(super) cancel_io: Arc<Semaphore>,
    revocation_io: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct Coordinator {
    pub(super) inner: Arc<Inner>,
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
                launch: Arc::new(Mutex::new(())),
                evidence: Mutex::new(()),
                refreshes: Flights::refreshes(),
                cancellations: Flights::cancellations(),
                status_io: Arc::new(Semaphore::new(1)),
                begin_io: Arc::new(Semaphore::new(1)),
                cancel_io: Arc::new(Semaphore::new(1)),
                revocation_io: Arc::new(Semaphore::new(1)),
            }),
        }
    }
    pub fn ledger(&self) -> &Ledger {
        &self.inner.ledger
    }
    /// Once admitted to this bounded actor, work belongs to the coordinator.
    /// Dropping a caller's response future cannot cancel a reservation/Begin.
    async fn owned_launch<T: Send + 'static, F, Fut>(&self, operation: F) -> Result<T>
    where
        F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _guard = inner
                .launch
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
        if service_id != *self.inner.ledger.service_id() {
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
        // Accepted requests remain readable while preparation or Begin owns
        // launch control. Only a new reservation needs that ownership.
        if let Some(accepted) = self.inner.replay(&key, &request).await? {
            return Ok(accepted);
        }
        let (retry_key, retry_request) = (key.clone(), request.clone());
        let result = self
            .owned_launch(move |inner| async move {
                // Another admission may have committed after the first lookup.
                if let Some(accepted) = inner.replay(&key, &request).await? {
                    return Ok(accepted);
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
            .await;
        if matches!(result, Err(Error::Busy)) {
            // Include a commit racing the attempted acquisition of launch
            // control, even if preparation already owns it again.
            return self
                .inner
                .replay(&retry_key, &retry_request)
                .await?
                .ok_or(Error::Busy);
        }
        result
    }
    pub async fn start(&self, run: RunId) -> Result<Admission> {
        self.owned_launch(move |inner| async move {
            let admission = inner.ledger.get(&run).await?;
            if admission.start_attempted {
                return inner.shared_refresh(run.clone()).await;
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
                    inner
                        .uncertain(&admission, Reason::PreparationFailed)
                        .await?;
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
                inner
                    .uncertain(&admission, Reason::PreparationFailed)
                    .await?;
                return Err(Error::Invalid("preparation identity or path mismatch"));
            }
            let saved = prepared.clone();
            inner
                .ledger
                .update(&run, "start_intent", move |record| {
                    if record.start_attempted {
                        return Err(Error::Uncertain);
                    }
                    record.prepared = Some(saved);
                    if record.cancel_requested.is_none() {
                        record.start_attempted = true;
                        record.phase = Phase::Starting;
                    }
                    Ok(())
                })
                .await?;
            let latest = inner.ledger.get(&run).await?;
            if latest.cancel_requested.is_some() {
                // Preparation may finish after cancellation. Retain its identity
                // for cleanup, revoke its grant and never submit a new worker.
                tokio::task::spawn_blocking(move || {
                    Grant::new(prepared.private_dir, prepared.binding).revoke()
                })
                .await
                .map_err(|_| Error::Uncertain)??;
                return Ok(latest);
            }
            let submitted = inner.supervisor.start(&prepared).await;
            // Start submission never waits for preparedness while holding this.
            drop(run_lock);
            match submitted {
                Ok(Some(endpoint)) if endpoint.binding == prepared.binding => inner
                    .ledger
                    .compare_update(&latest, "start_acknowledged", move |record| {
                        record.endpoint = Some(endpoint);
                        Ok(())
                    })
                    .await
                    .map(|(record, _)| record),
                Ok(None) => inner.ledger.get(&run).await,
                _ => inner.uncertain(&latest, Reason::StartUncertain).await,
            }
        })
        .await
    }
    pub async fn refresh(&self, run: RunId) -> Result<Admission> {
        self.inner.shared_refresh(run).await
    }
    /// Normal execution can release admission only after a persisted terminal
    /// confirmation, settled publication, successful cleanup and original-owner
    /// termination. Worker loss and interrupted evidence remain reserved.
    pub async fn finish_normal(&self, run: RunId) -> Result<Admission> {
        let expected = self.inner.ledger.get(&run).await?;
        if expected.phase == Phase::Terminal
            || expected.confirmed_read.is_none()
            || expected.last_status.as_ref().is_none_or(|s| {
                s.publication_pending
                    || !matches!(s.state, WorkerState::Running | WorkerState::Cancelling)
            })
            || !self.inner.archive.normal_completion(&expected).await?
        {
            return Ok(expected);
        }
        let stopped = self.inner.stop_supervisor(expected.clone()).await?;
        // Stop/verification did not hold a transaction. A later recovery owner
        // or different boundary must run its own finalization protocol.
        if stopped.owner_generation != expected.owner_generation
            || stopped.prepared != expected.prepared
            || stopped.confirmed_read != expected.confirmed_read
            || stopped.last_status != expected.last_status
        {
            return Ok(stopped);
        }
        if !stopped.supervisor.as_ref().is_some_and(|s| s.stopped)
            || stopped.stop_accepted.is_none()
        {
            return Ok(stopped);
        }
        self.inner
            .ledger
            .compare_update(&stopped, "normal_completed", |record| {
                record.phase = Phase::Terminal;
                record.reason = None;
                record.stop_delivery = Some(StopDelivery::Stopped);
                Ok(())
            })
            .await
            .map(|(record, _)| record)
    }
    pub async fn begin(&self, run: RunId) -> Result<Admission> {
        self.owned_launch(move |inner| async move {
            let before = inner.ledger.get(&run).await?;
            if before.cancel_requested.is_some() {
                return Ok(before);
            }
            let admission = inner.shared_refresh(run.clone()).await?;
            if admission.cancel_requested.is_some() || admission.phase == Phase::Reconciling {
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
                return inner.uncertain(&admission, Reason::BeginUncertain).await;
            }
            let authorized = inner
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
                return inner.uncertain(&authorized, Reason::BeginUncertain).await;
            }
            inner.command_begin(authorized).await
        })
        .await
    }
    pub async fn cancel(&self, run: RunId) -> Result<Admission> {
        let inner = self.inner.clone();
        let id = run.clone();
        let result = self
            .inner
            .cancellations
            .run(run.clone(), move || async move {
                let now = inner.clock.now();
                let admission = inner
                    .ledger
                    .update(&id, "cancel_requested", move |record| {
                        if record.phase == Phase::Terminal || record.cancel_requested.is_some() {
                            return Ok(());
                        }
                        record.cancel_requested = Some(now);
                        record.cancel_delivery = Some(CancelDelivery::Requested);
                        record.stop_delivery = Some(StopDelivery::Requested);
                        record.phase = Phase::Cancelling;
                        record.reason = Some(Reason::CancelRequested);
                        Ok(())
                    })
                    .await?;
                if admission.phase == Phase::Terminal {
                    return Ok(admission);
                }
                if admission.cancel_delivery == Some(CancelDelivery::WorkerAcknowledged) {
                    return inner.stop_supervisor(admission).await;
                }
                let Some(prepared) = admission.prepared.clone() else {
                    return Ok(admission);
                };
                // A stuck private filesystem cannot indefinitely delay the
                // independent manager stop. Unsettled revocation retains both
                // service ownership and its single I/O slot after timeout.
                let revoked = if let Ok(permit) = inner.revocation_io.clone().try_acquire_owned() {
                    let retained = inner.clone();
                    let task = tokio::task::spawn_blocking(move || {
                        let (_permit, _owner) = (permit, retained);
                        Grant::new(prepared.private_dir, prepared.binding).revoke()
                    });
                    matches!(
                        tokio::time::timeout(std::time::Duration::from_secs(5), task).await,
                        Ok(Ok(Ok(())))
                    )
                } else {
                    false
                };
                let delivered = if revoked {
                    inner
                        .exchange(&admission, Command::Cancel)
                        .await
                        .ok()
                        .is_some_and(|(_, reply)| {
                            matches!(
                                reply.status.state,
                                WorkerState::CancelledBeforeBegin | WorkerState::Cancelling
                            )
                        })
                } else {
                    false
                };
                // A Cancel acknowledgment is control evidence, not an archive
                // confirmation. Never wait for verification or advance its receipt.
                let recorded = inner
                    .ledger
                    .update(&id, "cancel_delivery", move |record| {
                        if record.owner_generation != admission.owner_generation
                            || record.phase == Phase::Terminal
                            || record.prepared.as_ref().map(|p| &p.binding)
                                != admission.prepared.as_ref().map(|p| &p.binding)
                        {
                            return Ok(());
                        }
                        record.cancel_delivery = Some(if delivered {
                            CancelDelivery::WorkerAcknowledged
                        } else {
                            CancelDelivery::Uncertain
                        });
                        record.reason = Some(if delivered {
                            Reason::CancelRequested
                        } else {
                            Reason::ControlUncertain
                        });
                        Ok(())
                    })
                    .await?;
                inner.stop_supervisor(recorded).await
            })
            .await?;
        match result {
            Some(record) => Ok(record),
            None => self.inner.ledger.get(&run).await,
        }
    }
    /// Explicit operator release of a run that cannot reach normal completion.
    /// The daemon verifies that the original invocation stopped and that every
    /// external resource is absent. It never modifies the archive: the last
    /// published checkpoint stays authoritative and unknown usage stays unknown.
    pub async fn release(&self, run: RunId, reason: String) -> Result<Admission> {
        if reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
            return Err(Error::Invalid(
                "release reason must be 1 to 512 printable bytes",
            ));
        }
        self.owned_launch(move |inner| async move {
            let admission = inner.ledger.get(&run).await?;
            if admission.phase == Phase::Terminal {
                return Err(Error::Refused(vec!["run is already terminal".into()]));
            }
            let mut present = Vec::new();
            // Start intent is durable before any submission, so without it no
            // worker or worker-owned resource can exist for this run.
            let (supervisor, absent_resources) = if admission.start_attempted {
                let prepared = admission.prepared.as_ref().ok_or(Error::Uncertain)?;
                let unit = &prepared.binding.unit;
                let observation = match inner.supervisor.observe(prepared).await {
                    Ok(Some(observation)) => {
                        if admission.supervisor.as_ref().is_some_and(|saved| {
                            saved.manager_id != observation.manager_id
                                || saved.cgroup != observation.cgroup
                        }) {
                            present.push(format!("{unit} no longer matches its saved identity"));
                        } else if !observation.stopped {
                            present.push(format!(
                                "{unit} is {} with main PID {}",
                                observation.active_state, observation.main_pid
                            ));
                        }
                        Some(observation)
                    }
                    Ok(None) => {
                        present.push(format!("the supervisor cannot verify that {unit} stopped"));
                        None
                    }
                    Err(error) => {
                        present.push(format!("{unit} could not be observed: {error}"));
                        None
                    }
                };
                let resources = match inner.archive.external_resources(&admission).await {
                    Ok(resources) => {
                        present.extend(resources.present);
                        resources.absent
                    }
                    Err(error) => {
                        present.push(format!("external resources could not be verified: {error}"));
                        Vec::new()
                    }
                };
                (observation, resources)
            } else {
                (None, Vec::new())
            };
            if !present.is_empty() {
                return Err(Error::Refused(present));
            }
            let release = OperatorRelease {
                released: inner.clock.now(),
                reason,
                supervisor,
                absent_resources,
            };
            let (record, applied) = inner
                .ledger
                .compare_update(&admission, "operator_released", move |record| {
                    record.phase = Phase::Terminal;
                    record.reason = Some(Reason::OperatorReleased);
                    record.operator_release = Some(release);
                    Ok(())
                })
                .await?;
            if !applied {
                return Err(Error::Refused(vec![
                    "the run changed during verification; retry".into(),
                ]));
            }
            Ok(record)
        })
        .await
    }
}

impl Inner {
    async fn replay(&self, key: &str, request: &AdmissionRequest) -> Result<Option<Acceptance>> {
        let Some(admission) = self.ledger.lookup(key).await? else {
            return Ok(None);
        };
        if admission.request != *request {
            return Err(Error::Conflict);
        }
        Ok(Some(Acceptance {
            replayed: true,
            admission,
        }))
    }
}
