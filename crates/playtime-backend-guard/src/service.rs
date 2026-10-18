use crate::{Journal, Result, digest, machine::Machine, ports::*, wire::*};
use playtime_protocol::worker::ControlId;
use std::{
    sync::{Arc, Mutex, mpsc as blocking},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

const IO_GRACE: Duration = Duration::from_millis(100);
const PORT_WAIT: Duration = Duration::from_secs(1);
const RESERVE_RECORDS: u64 = 8;
const RESERVE_BYTES: u64 = RESERVE_RECORDS * CONTROL_BYTES as u64;

#[derive(Clone, Copy)]
struct Progress {
    sequence: u64,
    failed: bool,
}
struct Inner {
    machine: Machine,
    sequence: u64,
    bytes_left: u64,
    records_left: u64,
    writer: blocking::SyncSender<Event>,
    verified: Option<VerifiedBackend>,
    termination: Option<HeldTermination>,
    poisoned: bool,
    stop_started: bool,
    cancel: CancellationToken,
    armed_lease: Option<ControlId>,
}
impl Inner {
    fn enqueue(&mut self, event: Event, admission: bool) -> Result<u64> {
        let sequence = self.sequence.checked_add(1).ok_or(Failure::Limit)?;
        let bytes = serde_json::to_vec(&JournalRecord {
            version: VERSION,
            sequence,
            previous_sha256: digest(&[]),
            event: event.clone(),
        })
        .map_err(|_| Failure::Invalid)?
        .len() as u64;
        let reserve = if admission {
            (RESERVE_BYTES, RESERVE_RECORDS)
        } else {
            (0, 0)
        };
        if bytes > CONTROL_BYTES as u64
            || self.bytes_left < bytes + reserve.0
            || self.records_left <= reserve.1
        {
            return Err(Failure::Limit);
        }
        let mut next = self.machine.clone();
        next.apply(&event)?;
        self.writer.try_send(event).map_err(|_| {
            self.poisoned = true;
            Failure::Storage
        })?;
        self.machine = next;
        self.sequence = sequence;
        self.bytes_left -= bytes;
        self.records_left -= 1;
        Ok(sequence)
    }
    fn authenticate(&self, lease_id: &ControlId, capability: &Capability) -> Result<&LeaseRecord> {
        let lease = self
            .machine
            .leases
            .get(lease_id.as_str())
            .ok_or(Failure::Unauthorized)?;
        let actual = digest(capability.expose().as_bytes());
        if !bool::from(
            lease
                .capability_sha256
                .as_str()
                .as_bytes()
                .ct_eq(actual.as_str().as_bytes()),
        ) {
            return Err(Failure::Unauthorized);
        }
        Ok(lease)
    }
}
struct Shared {
    inner: Mutex<Inner>,
    progress: watch::Receiver<Progress>,
    clock: Arc<dyn Clock>,
    backend: Arc<dyn Backend>,
    supervisor: Arc<dyn Supervisor>,
    operations: Arc<Semaphore>,
    forwarding: Arc<Semaphore>,
    enforcement: Option<Arc<dyn Enforcement>>,
}
#[derive(Clone)]
pub struct Guard(Arc<Shared>);

pub enum Dispatch {
    /// Retries within a reserved lease are queries, including uncertain requests.
    Existing(Box<Status>),
    Stream(OwnerStream),
}
pub struct OwnerStream {
    pub frames: mpsc::Receiver<ObservationFrame>,
    pub completion: watch::Receiver<Option<Result<Status>>>,
    guard: Guard,
    lease_id: ControlId,
    request_id: ControlId,
}
impl Drop for OwnerStream {
    fn drop(&mut self) {
        self.guard.owner_lost(&self.lease_id, &self.request_id);
    }
}

impl Guard {
    /// `initial` is accepted only for an explicitly created empty journal.
    /// Reopening requires saved identity; it can never create a new generation.
    pub async fn start(
        journal: Journal,
        initial: Option<BackendIdentity>,
        backend: Arc<dyn Backend>,
        supervisor: Arc<dyn Supervisor>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        Self::start_enforced(journal, initial, backend, supervisor, clock, None).await
    }
    pub async fn start_enforced(
        mut journal: Journal,
        initial: Option<BackendIdentity>,
        backend: Arc<dyn Backend>,
        supervisor: Arc<dyn Supervisor>,
        clock: Arc<dyn Clock>,
        enforcement: Option<Arc<dyn Enforcement>>,
    ) -> Result<Self> {
        if journal.machine.backend.is_none() {
            let identity = initial.ok_or(Failure::Blocked)?;
            identity.validate().map_err(|_| Failure::Invalid)?;
            if clock.now().boot_id != identity.boot_id {
                return Err(Failure::Blocked);
            }
            let verified = tokio::time::timeout(PORT_WAIT, backend.verify_read_only(&identity))
                .await
                .map_err(|_| Failure::Blocked)??;
            if verified.identity() != &identity {
                return Err(Failure::Blocked);
            }
            journal = tokio::task::spawn_blocking(move || {
                journal.append(Event::Prepared { backend: identity })?;
                Ok::<_, Failure>(journal)
            })
            .await
            .map_err(|_| Failure::Storage)??;
        } else if initial.is_some() {
            return Err(Failure::Conflict);
        }
        let (bytes_left, records_left) = journal.remaining();
        let machine = journal.machine.clone();
        let sequence = journal.sequence;
        let (writer, commands) = blocking::sync_channel(16);
        let (published, progress) = watch::channel(Progress {
            sequence,
            failed: false,
        });
        std::thread::Builder::new()
            .name("backend-guard-journal".into())
            .spawn(move || {
                while let Ok(event) = commands.recv() {
                    match journal.append(event) {
                        Ok(sequence) => {
                            published.send_replace(Progress {
                                sequence,
                                failed: false,
                            });
                        }
                        Err(_) => {
                            published.send_replace(Progress {
                                sequence: journal.sequence,
                                failed: true,
                            });
                            // Retain the lock even after failure. A live failed guard
                            // must not let a second process authorize this backend.
                            while commands.recv().is_ok() {}
                            break;
                        }
                    }
                }
            })
            .map_err(|_| Failure::Storage)?;
        let guard = Self(Arc::new(Shared {
            inner: Mutex::new(Inner {
                machine,
                sequence,
                bytes_left,
                records_left,
                writer,
                verified: None,
                termination: None,
                poisoned: false,
                stop_started: false,
                cancel: CancellationToken::new(),
                armed_lease: None,
            }),
            progress,
            clock,
            backend,
            supervisor,
            operations: Arc::new(Semaphore::new(16)),
            forwarding: Arc::new(Semaphore::new(QUEUE_BYTES)),
            enforcement,
        }));
        let needs_stop = {
            let inner = guard.0.inner.lock().unwrap();
            // A prior process may have closed its gate but lost the stop write.
            // Every surviving lease needs reconciliation, even after a terminal.
            inner.machine.lease().is_some_and(|l| !l.released) || inner.machine.stop.is_some()
        };
        if needs_stop {
            guard.select_stop(StopReason::Restart, None)?;
        } else {
            guard.refresh_identity().await;
        }
        let weak = Arc::downgrade(&guard.0);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let Some(shared) = weak.upgrade() else {
                    break;
                };
                Guard(shared).tick();
            }
        });
        Ok(guard)
    }
    async fn refresh_identity(&self) {
        let identity = self
            .0
            .inner
            .lock()
            .unwrap()
            .machine
            .backend
            .clone()
            .unwrap();
        let result =
            tokio::time::timeout(PORT_WAIT, self.0.backend.verify_read_only(&identity)).await;
        let mut inner = self.0.inner.lock().unwrap();
        if self.0.clock.now().boot_id != identity.boot_id {
            inner.poisoned = true;
            return;
        }
        match result {
            Ok(Ok(held))
                if held.identity() == &identity
                    && inner.machine.backend.as_ref() == Some(&identity)
                    && inner.machine.stop.is_none() =>
            {
                inner.verified = Some(held)
            }
            _ => inner.poisoned = true,
        }
    }
    async fn durable(&self, sequence: u64) -> Result<()> {
        let mut progress = self.0.progress.clone();
        loop {
            let current = *progress.borrow_and_update();
            if current.failed {
                return Err(Failure::Storage);
            }
            if current.sequence >= sequence {
                return Ok(());
            }
            progress.changed().await.map_err(|_| Failure::Storage)?;
        }
    }
    pub fn status(&self) -> Status {
        let inner = self.0.inner.lock().unwrap();
        let progress = *self.0.progress.borrow();
        let machine = &inner.machine;
        let gate = if inner.poisoned
            || progress.failed
            || self.0.clock.now().boot_id != machine.backend.as_ref().unwrap().boot_id
        {
            Gate::Blocked
        } else if machine.terminated {
            if inner.termination.is_some() && progress.sequence >= inner.sequence {
                Gate::Terminated
            } else {
                Gate::Preparing
            }
        } else if inner.stop_started {
            if machine
                .stop
                .as_ref()
                .is_some_and(|s| self.0.clock.now().ms >= s.observation_deadline_ms)
            {
                Gate::Blocked
            } else {
                Gate::Stopping
            }
        } else if progress.sequence < inner.sequence || inner.verified.is_none() {
            Gate::Preparing
        } else if machine.pending_request() {
            Gate::InFlight
        } else {
            Gate::Ready
        };
        Status {
            version: VERSION,
            backend: machine.backend.clone().unwrap(),
            lease: machine.lease().cloned(),
            request: machine.request().cloned(),
            stop: machine.stop.clone(),
            observation: machine.observation.clone(),
            termination_recorded: machine.terminated && progress.sequence >= inner.sequence,
            sequence: inner.sequence,
            durable_sequence: progress.sequence,
            gate,
        }
    }
    /// The detached operation survives cancellation of the API caller.
    pub async fn acquire(&self, spec: LeaseSpec, capability: Capability) -> Result<Status> {
        let permit = self
            .0
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let guard = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            guard.acquire_inner(spec, capability).await
        })
        .await
        .map_err(|_| Failure::Blocked)?
    }
    async fn acquire_inner(&self, spec: LeaseSpec, capability: Capability) -> Result<Status> {
        spec.validate().map_err(|_| Failure::Invalid)?;
        let requested_lease = spec.lease_id.clone();
        let verifier = digest(capability.expose().as_bytes());
        let sequence = {
            let mut inner = self.0.inner.lock().unwrap();
            if let Some(event) = inner.machine.operations.get(spec.operation_id.as_str()) {
                match event {
                    Event::Acquired { lease }
                        if lease.spec == spec && lease.capability_sha256 == verifier =>
                    {
                        inner.sequence
                    }
                    _ => return Err(Failure::Conflict),
                }
            } else {
                self.check_gate(&inner)?;
                if inner.machine.lease().is_some_and(|l| !l.released) {
                    return Err(Failure::Busy);
                }
                if self.0.clock.now().ms >= spec.execution_deadline_ms {
                    return Err(Failure::Expired);
                }
                inner.enqueue(
                    Event::Acquired {
                        lease: LeaseRecord {
                            spec: spec.clone(),
                            capability_sha256: verifier,
                            last_ordinal: 0,
                            released: false,
                        },
                    },
                    true,
                )?
            }
        };
        let needs_arm = {
            let inner = self.0.inner.lock().unwrap();
            !inner.stop_started
                && inner
                    .machine
                    .lease()
                    .is_some_and(|lease| !lease.released && lease.spec == spec)
        };
        if needs_arm && let Some(enforcement) = &self.0.enforcement {
            if tokio::time::timeout(PORT_WAIT, enforcement.arm_lease(&spec))
                .await
                .map_or(true, |r| r.is_err())
            {
                let _ = self.select_stop(StopReason::BackendUncertain, None);
                return Err(Failure::Blocked);
            }
            let mut inner = self.0.inner.lock().unwrap();
            if inner
                .machine
                .lease()
                .is_some_and(|lease| !lease.released && lease.spec == spec)
            {
                inner.armed_lease = Some(spec.lease_id.clone());
            }
        }
        self.durable(sequence).await?;
        let mut status = self.status();
        status.lease = self
            .0
            .inner
            .lock()
            .unwrap()
            .machine
            .leases
            .get(requested_lease.as_str())
            .cloned();
        Ok(status)
    }
    fn check_gate(&self, inner: &Inner) -> Result<()> {
        let now = self.0.clock.now();
        if inner.poisoned
            || self.0.progress.borrow().failed
            || inner.machine.stop.is_some()
            || inner.stop_started
            || inner.verified.is_none()
            || now.boot_id != inner.machine.backend.as_ref().unwrap().boot_id
        {
            return Err(Failure::Blocked);
        }
        if self.0.progress.borrow().sequence < inner.sequence {
            return Err(Failure::Busy);
        }
        Ok(())
    }
    pub async fn dispatch(
        &self,
        spec: RequestSpec,
        capability: Capability,
        body: Vec<u8>,
    ) -> Result<Dispatch> {
        // Caller cancellation must not cancel a committed dispatch. Returning an
        // unreceived stream drops its owner and independently initiates stop.
        let permit = self
            .0
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let guard = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            guard.dispatch_inner(spec, capability, body).await
        })
        .await
        .map_err(|_| Failure::Blocked)?
    }
    async fn dispatch_inner(
        &self,
        spec: RequestSpec,
        capability: Capability,
        body: Vec<u8>,
    ) -> Result<Dispatch> {
        spec.validate().map_err(|_| Failure::Invalid)?;
        if body.len() as u64 != spec.body_bytes || digest(&body) != spec.body_sha256 {
            return Err(Failure::Conflict);
        }
        let (sequence, existing, backend) = {
            let mut inner = self.0.inner.lock().unwrap();
            let lease = inner.authenticate(&spec.lease_id, &capability)?;
            if lease.released || inner.machine.lease.as_deref() != Some(spec.lease_id.as_str()) {
                return Err(Failure::Unauthorized);
            }
            if let Some(request) = inner.machine.requests.get(spec.request_id.as_str()) {
                if request.spec != spec {
                    return Err(Failure::Conflict);
                }
                (inner.sequence, true, inner.machine.backend.clone().unwrap())
            } else {
                self.check_gate(&inner)?;
                if self.0.enforcement.is_some()
                    && inner.armed_lease.as_ref() != Some(&spec.lease_id)
                {
                    return Err(Failure::Busy);
                }
                if inner.machine.pending_request() {
                    return Err(Failure::Busy);
                }
                let now = self.0.clock.now().ms;
                if now >= spec.deadline_ms {
                    return Err(Failure::Expired);
                }
                let backend = inner.machine.backend.clone().unwrap();
                let sequence = inner.enqueue(
                    Event::DispatchMarked {
                        request: RequestRecord {
                            spec: spec.clone(),
                            marked_ms: now,
                            state: RequestState::MayHaveDispatched {},
                        },
                    },
                    true,
                )?;
                (sequence, false, backend)
            }
        };
        if let Err(e) = self.durable(sequence).await {
            let _ = self.select_stop(StopReason::StorageUncertain, None);
            return Err(e);
        }
        if existing {
            let mut status = self.status();
            status.request = self
                .0
                .inner
                .lock()
                .unwrap()
                .machine
                .requests
                .get(spec.request_id.as_str())
                .cloned();
            return Ok(Dispatch::Existing(Box::new(status)));
        }
        if let Some(enforcement) = &self.0.enforcement
            && tokio::time::timeout(PORT_WAIT, enforcement.arm_request(&spec))
                .await
                .map_or(true, |r| r.is_err())
        {
            let _ = self.select_stop(StopReason::BackendUncertain, None);
            return Err(Failure::Blocked);
        }
        let cancel = {
            let inner = self.0.inner.lock().unwrap();
            self.check_gate(&inner)?;
            if self.0.clock.now().ms >= spec.deadline_ms {
                drop(inner);
                let _ = self.select_stop(StopReason::Deadline, None);
                return Err(Failure::Expired);
            }
            inner.cancel.clone()
        };
        let (sink, frames) = ObservationSink::new(
            self.0.clock.clone(),
            spec.deadline_ms,
            backend.boot_id.clone(),
            cancel.clone(),
            self.0.forwarding.clone(),
        );
        let (completed, completion) = watch::channel(None);
        let stream = OwnerStream {
            frames,
            completion,
            guard: self.clone(),
            lease_id: spec.lease_id.clone(),
            request_id: spec.request_id.clone(),
        };
        let guard = self.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Failure::Blocked),
                result = guard.0.backend.execute(backend.clone(), spec.clone(), body, sink, cancel.clone()) => result,
            };
            let result = match result {
                Ok(terminal) if terminal.matches(&backend, &spec) => guard.complete(&spec).await,
                _ => {
                    let _ = guard.select_stop(StopReason::BackendUncertain, None);
                    Err(Failure::Blocked)
                }
            };
            completed.send_replace(Some(result));
        });
        Ok(Dispatch::Stream(stream))
    }
    async fn complete(&self, request: &RequestSpec) -> Result<Status> {
        let sequence = {
            let mut inner = self.0.inner.lock().unwrap();
            self.check_gate(&inner)?;
            let now = self.0.clock.now().ms;
            if now >= request.deadline_ms {
                drop(inner);
                self.select_stop(StopReason::Deadline, None)?;
                return Err(Failure::Expired);
            }
            inner.enqueue(
                Event::Terminal {
                    request_id: request.request_id.clone(),
                    completed_ms: now,
                },
                false,
            )?
        };
        self.durable(sequence).await?;
        if let Some(enforcement) = &self.0.enforcement
            && tokio::time::timeout(PORT_WAIT, enforcement.terminal(request))
                .await
                .map_or(true, |r| r.is_err())
        {
            let _ = self.select_stop(StopReason::BackendUncertain, None);
            return Err(Failure::Blocked);
        }
        Ok(self.status())
    }
    fn owner_lost(&self, lease_id: &ControlId, request_id: &ControlId) {
        let pending = {
            let inner = self.0.inner.lock().unwrap();
            inner
                .machine
                .lease()
                .is_some_and(|l| &l.spec.lease_id == lease_id)
                && inner.machine.pending_request()
                && inner
                    .machine
                    .request()
                    .is_some_and(|r| &r.spec.request_id == request_id)
        };
        if pending {
            let _ = self.select_stop(StopReason::OwnerLost, None);
        }
    }
    pub async fn revoke(
        &self,
        lease: ControlId,
        capability: Capability,
        operation: ControlId,
    ) -> Result<Status> {
        {
            let inner = self.0.inner.lock().unwrap();
            let record = inner.authenticate(&lease, &capability)?;
            if record.released || inner.machine.lease.as_deref() != Some(lease.as_str()) {
                return Err(Failure::Unauthorized);
            }
            if let Some(event) = inner.machine.operations.get(operation.as_str())
                && !matches!(event, Event::StopSelected { operation: old } if old.lease_id == lease)
            {
                return Err(Failure::Conflict);
            }
        }
        self.select_stop(StopReason::Revoked, Some(operation))?;
        tokio::time::timeout(IO_GRACE, async {
            loop {
                let selected = {
                    let inner = self.0.inner.lock().unwrap();
                    inner.machine.stop.as_ref().map(|_| inner.sequence)
                };
                if let Some(sequence) = selected {
                    return self.durable(sequence).await;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .map_err(|_| Failure::Storage)??;
        Ok(self.status())
    }
    fn select_stop(&self, reason: StopReason, operation_id: Option<ControlId>) -> Result<()> {
        let (operation, sequence) = {
            let mut inner = self.0.inner.lock().unwrap();
            inner.cancel.cancel(); // close the dispatch gate before any I/O
            if inner.stop_started {
                return Ok(());
            }
            let Some(lease) = inner
                .machine
                .lease()
                .cloned()
                .filter(|l| !l.released || inner.machine.stop.is_some())
            else {
                return Ok(());
            };
            let operation = if let Some(operation) = &inner.machine.stop {
                operation.clone()
            } else {
                let now = self.0.clock.now();
                // Restart after an unacknowledged stop selection may only use
                // the earlier durable request origin, never a renewed grace.
                let decided_ms = if reason == StopReason::Restart {
                    inner
                        .machine
                        .request()
                        .map_or(lease.spec.execution_deadline_ms.min(now.ms), |r| {
                            r.marked_ms
                        })
                } else {
                    now.ms
                };
                let automatic = digest(format!("stop:{}", lease.spec.lease_id.as_str()).as_bytes());
                StopOperation {
                    operation_id: operation_id.unwrap_or_else(|| {
                        ControlId::new(automatic.as_str()[..32].into()).unwrap()
                    }),
                    backend: lease.spec.backend.clone(),
                    lease_id: lease.spec.lease_id.clone(),
                    reason,
                    decided_ms,
                    supervisor_deadline_ms: decided_ms
                        .checked_add(SUPERVISOR_STOP_MS)
                        .ok_or(Failure::Invalid)?
                        .min(lease.spec.cleanup_deadline_ms),
                    observation_deadline_ms: decided_ms
                        .checked_add(STOP_OBSERVATION_MS)
                        .ok_or(Failure::Invalid)?
                        .min(lease.spec.cleanup_deadline_ms),
                }
            };
            inner.stop_started = true;
            let sequence = if inner.machine.stop.is_some() {
                Some(inner.sequence)
            } else if self.0.enforcement.is_some() {
                None
            } else {
                match inner.enqueue(
                    Event::StopSelected {
                        operation: operation.clone(),
                    },
                    false,
                ) {
                    Ok(n) => Some(n),
                    Err(_) => {
                        inner.poisoned = true;
                        inner.machine.stop = Some(operation.clone());
                        None
                    }
                }
            };
            (operation, sequence)
        };
        let guard = self.clone();
        tokio::spawn(async move {
            let (operation, sequence) = if sequence.is_none()
                && let Some(enforcement) = &guard.0.enforcement
            {
                let result =
                    tokio::time::timeout(PORT_WAIT, enforcement.select_stop(&operation)).await;
                let chosen = match result {
                    Ok(Ok(chosen))
                        if chosen.backend == operation.backend
                            && chosen.lease_id == operation.lease_id =>
                    {
                        chosen
                    }
                    _ => {
                        guard.0.inner.lock().unwrap().poisoned = true;
                        operation
                    }
                };
                let sequence = {
                    let mut inner = guard.0.inner.lock().unwrap();
                    match inner.enqueue(
                        Event::StopSelected {
                            operation: chosen.clone(),
                        },
                        false,
                    ) {
                        Ok(n) => Some(n),
                        Err(_) => {
                            inner.poisoned = true;
                            inner.machine.stop = Some(chosen.clone());
                            None
                        }
                    }
                };
                (chosen, sequence)
            } else {
                (operation, sequence)
            };
            guard.service_stop(operation, sequence).await;
        });
        Ok(())
    }
    async fn service_stop(&self, operation: StopOperation, sequence: Option<u64>) {
        if let Some(sequence) = sequence
            && !matches!(
                tokio::time::timeout(IO_GRACE, self.durable(sequence)).await,
                Ok(Ok(()))
            )
        {
            self.0.inner.lock().unwrap().poisoned = true;
        }
        // Stop is independent of failed/stalled private persistence. The trusted
        // supervisor must match the original identity using fresh evidence.
        if self.0.clock.now().boot_id != operation.backend.boot_id {
            self.0.inner.lock().unwrap().poisoned = true;
            return;
        }
        let already_recorded = self.0.inner.lock().unwrap().machine.terminated;
        let mut retry_submission = !already_recorded && self.submit_stop(&operation).await;
        loop {
            let now = self.0.clock.now();
            if now.boot_id != operation.backend.boot_id {
                self.0.inner.lock().unwrap().poisoned = true;
                return;
            }
            // A saved termination is reauthenticated even after its deadline;
            // this cannot submit stop or grant extra execution time.
            let remaining = operation.observation_deadline_ms.saturating_sub(now.ms);
            if remaining == 0 && !already_recorded {
                return;
            }
            let wait = if already_recorded {
                PORT_WAIT
            } else {
                PORT_WAIT.min(Duration::from_millis(remaining))
            };
            if let Ok(Ok(Some(held))) =
                tokio::time::timeout(wait, self.0.supervisor.terminated(&operation)).await
            {
                if !already_recorded && self.0.clock.now().ms >= operation.observation_deadline_ms {
                    return;
                }
                if !held.matches(&operation) {
                    self.0.inner.lock().unwrap().poisoned = true;
                    return;
                }
                let sequence = {
                    let mut inner = self.0.inner.lock().unwrap();
                    inner.termination = Some(held);
                    if already_recorded {
                        None
                    } else if !inner.poisoned {
                        inner
                            .enqueue(
                                Event::TerminationConfirmed {
                                    operation_id: operation.operation_id.clone(),
                                },
                                false,
                            )
                            .ok()
                    } else {
                        None
                    }
                };
                if let Some(sequence) = sequence {
                    let _ = self.durable(sequence).await;
                }
                return;
            }
            if already_recorded {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            if retry_submission && self.0.clock.now().ms < operation.observation_deadline_ms {
                retry_submission = self.submit_stop(&operation).await;
            }
        }
    }
    async fn submit_stop(&self, operation: &StopOperation) -> bool {
        let remaining = operation
            .observation_deadline_ms
            .saturating_sub(self.0.clock.now().ms);
        // Revocation is still submitted after expiry, carrying the *expired*
        // original deadlines; this bounded exchange cannot create new grace.
        let wait = PORT_WAIT.min(Duration::from_millis(remaining.max(1)));
        let observation = tokio::time::timeout(wait, self.0.supervisor.stop(operation))
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .unwrap_or(ManagerObservation::Unavailable {});
        let retry = observation == (ManagerObservation::Unavailable {});
        let mut inner = self.0.inner.lock().unwrap();
        let should_record = inner.machine.observation.is_none()
            || (inner.machine.observation == Some(ManagerObservation::Unavailable {})
                && matches!(observation, ManagerObservation::Submitted { .. }));
        if observation == (ManagerObservation::IdentityMismatch {}) {
            inner.poisoned = true;
        }
        if should_record
            && !inner.poisoned
            && observation != (ManagerObservation::Terminated {})
            && inner
                .enqueue(
                    Event::StopObserved {
                        operation_id: operation.operation_id.clone(),
                        observation,
                    },
                    false,
                )
                .is_err()
        {
            inner.poisoned = true;
        }
        retry
    }
    /// Independent timer; virtual-clock fixtures call this directly.
    pub fn enforcement_failed(&self) {
        let _ = self.select_stop(StopReason::BackendUncertain, None);
    }
    pub fn tick(&self) {
        let reason = {
            let inner = self.0.inner.lock().unwrap();
            if inner.machine.lease().is_none_or(|l| l.released) || inner.stop_started {
                return;
            }
            let now = self.0.clock.now();
            if inner.poisoned
                || self.0.progress.borrow().failed
                || now.boot_id != inner.machine.backend.as_ref().unwrap().boot_id
            {
                Some(StopReason::StorageUncertain)
            } else if now.ms >= inner.machine.lease().unwrap().spec.execution_deadline_ms
                || inner.machine.request().is_some_and(|r| {
                    r.state == (RequestState::MayHaveDispatched {}) && now.ms >= r.spec.deadline_ms
                })
            {
                Some(StopReason::Deadline)
            } else {
                None
            }
        };
        if let Some(reason) = reason {
            let _ = self.select_stop(reason, None);
        }
    }
    pub async fn release(
        &self,
        lease_id: ControlId,
        capability: Capability,
        operation_id: ControlId,
    ) -> Result<Status> {
        let permit = self
            .0
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let guard = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let sequence = {
                let mut inner = guard.0.inner.lock().unwrap();
                inner.authenticate(&lease_id, &capability)?;
                let event = Event::Released {
                    lease_id: lease_id.clone(),
                    operation_id: operation_id.clone(),
                };
                if let Some(old) = inner.machine.operations.get(operation_id.as_str()) {
                    if old != &event {
                        return Err(Failure::Conflict);
                    }
                    inner.sequence
                } else {
                    if inner.poisoned || guard.0.progress.borrow().failed {
                        return Err(Failure::Blocked);
                    }
                    if let Some(op) = &inner.machine.stop
                        && !inner
                            .termination
                            .as_ref()
                            .is_some_and(|held| held.matches(op))
                    {
                        return Err(Failure::Blocked);
                    }
                    inner.enqueue(event, false)?
                }
            };
            guard.durable(sequence).await?;
            if let Some(enforcement) = &guard.0.enforcement {
                tokio::time::timeout(PORT_WAIT, enforcement.release(&lease_id))
                    .await
                    .map_err(|_| Failure::Blocked)??;
            }
            Ok(guard.status())
        })
        .await
        .map_err(|_| Failure::Blocked)?
    }
    /// Trusted provisioning only, after held original termination and release.
    /// This is not a private client command or an automatic restart behavior.
    pub async fn prepare_generation(&self, verified: VerifiedBackend) -> Result<Status> {
        let sequence = {
            let mut inner = self.0.inner.lock().unwrap();
            let old = inner.machine.stop.as_ref().ok_or(Failure::Blocked)?;
            if inner.poisoned
                || !inner.termination.as_ref().is_some_and(|h| h.matches(old))
                || self.0.clock.now().boot_id != verified.identity().boot_id
            {
                return Err(Failure::Blocked);
            }
            let sequence = inner.enqueue(
                Event::Prepared {
                    backend: verified.identity().clone(),
                },
                true,
            )?;
            inner.verified = Some(verified);
            inner.termination = None;
            inner.stop_started = false;
            inner.armed_lease = None;
            inner.cancel = CancellationToken::new();
            sequence
        };
        self.durable(sequence).await?;
        Ok(self.status())
    }
}
