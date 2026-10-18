//! Trusted host ports, deliberately separate from deserializable observations.
//! Implementers must not interpret a PID, wire boolean or closed socket as exit.
use crate::{Result, wire::*};
use playtime_protocol::worker::ControlId;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct Stamp {
    pub boot_id: ControlId,
    pub ms: u64,
}
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Stamp;
}
pub struct SystemClock {
    boot_id: ControlId,
}
impl SystemClock {
    pub fn new() -> Result<Self> {
        let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| Failure::Blocked)?;
        Ok(Self {
            boot_id: ControlId::new(text.trim().replace('-', "")).map_err(|_| Failure::Blocked)?,
        })
    }
}
impl Clock for SystemClock {
    fn now(&self) -> Stamp {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
        Stamp {
            boot_id: self.boot_id.clone(),
            ms: (t.tv_sec as u64)
                .saturating_mul(1000)
                .saturating_add(t.tv_nsec as u64 / 1_000_000),
        }
    }
}

/// A held, trusted binding; no Serialize/Deserialize implementation. The
/// adapter owns the verification and preserves the authority's lifetime.
pub trait BackendAuthority: Send + Sync + 'static {
    fn identity(&self) -> &BackendIdentity;
}
pub struct VerifiedBackend(Arc<dyn BackendAuthority>);
impl VerifiedBackend {
    pub fn from_trusted_port(authority: Arc<dyn BackendAuthority>) -> Result<Self> {
        authority
            .identity()
            .validate()
            .map_err(|_| Failure::Invalid)?;
        Ok(Self(authority))
    }
    pub fn identity(&self) -> &BackendIdentity {
        self.0.identity()
    }
}
pub trait TerminationAuthority: BackendAuthority {
    fn operation_id(&self) -> &ControlId;
}
pub struct HeldTermination(Arc<dyn TerminationAuthority>);
impl HeldTermination {
    pub fn from_supervisor(authority: Arc<dyn TerminationAuthority>) -> Self {
        Self(authority)
    }
    pub fn matches(&self, op: &StopOperation) -> bool {
        self.0.identity() == &op.backend && self.0.operation_id() == &op.operation_id
    }
}
/// Only a trusted protocol implementation may construct this value after
/// validating its terminal boundary; arbitrary model content cannot do so.
pub struct VerifiedTerminal {
    backend: BackendIdentity,
    request: RequestSpec,
}
impl VerifiedTerminal {
    pub fn from_trusted_port(backend: BackendIdentity, request: RequestSpec) -> Self {
        Self { backend, request }
    }
    pub(crate) fn matches(&self, backend: &BackendIdentity, request: &RequestSpec) -> bool {
        &self.backend == backend && &self.request == request
    }
}

#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Read-only, bounded exact instance/profile verification. This method must
    /// never load/unload/create a model or submit inference. Caller caps it at 1s.
    async fn verify_read_only(&self, expected: &BackendIdentity) -> Result<VerifiedBackend>;
    /// One call per durable intent. No retries inside this port. Check cancel
    /// before sending; dropping this future never proves remote termination.
    async fn execute(
        &self,
        backend: BackendIdentity,
        request: RequestSpec,
        body: Vec<u8>,
        observations: ObservationSink,
        cancel: CancellationToken,
    ) -> Result<VerifiedTerminal>;
}
#[async_trait::async_trait]
pub trait Supervisor: Send + Sync + 'static {
    /// Check fresh manager identity and submit/recover this exact operation.
    /// Must not depend on private worker diagnostics or response forwarding.
    async fn stop(&self, operation: &StopOperation) -> Result<ManagerObservation>;
    /// Retain real authority (e.g. owned/reaped children and an exclusion fence).
    /// A serialized manager observation is not sufficient to return Some.
    async fn terminated(&self, operation: &StopOperation) -> Result<Option<HeldTermination>>;
}

/// Production-only crash-independent enforcement. The CPU fixture constructor
/// does not install this port. Acknowledgments bind the exact immutable input;
/// a lost reply must never renew its deadlines.
#[async_trait::async_trait]
pub trait Enforcement: Send + Sync + 'static {
    /// Adopt an earlier crash-independent host stop if one already exists.
    /// The local gate is already closed; no host reply can reopen it.
    async fn select_stop(&self, proposed: &StopOperation) -> Result<StopOperation>;
    async fn arm_lease(&self, lease: &LeaseSpec) -> Result<()>;
    async fn arm_request(&self, request: &RequestSpec) -> Result<()>;
    async fn terminal(&self, request: &RequestSpec) -> Result<()>;
    async fn release(&self, lease: &ControlId) -> Result<()>;
}

pub struct ObservationFrame {
    pub bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}
pub struct ObservationSink {
    sender: mpsc::Sender<ObservationFrame>,
    capacity: Arc<Semaphore>,
    total: usize,
    clock: Arc<dyn Clock>,
    boot: ControlId,
    deadline: u64,
    cancel: CancellationToken,
}
impl ObservationSink {
    pub(crate) fn new(
        clock: Arc<dyn Clock>,
        deadline: u64,
        boot: ControlId,
        cancel: CancellationToken,
        capacity: Arc<Semaphore>,
    ) -> (Self, mpsc::Receiver<ObservationFrame>) {
        let (sender, receiver) = mpsc::channel(64);
        (
            Self {
                sender,
                capacity,
                total: 0,
                clock,
                boot,
                deadline,
                cancel,
            },
            receiver,
        )
    }
    /// Permits remain held through downstream writes, including the frame being
    /// written. Control requests use a different socket and never these permits.
    pub async fn send(&mut self, bytes: Vec<u8>) -> Result<()> {
        let next = self.total.checked_add(bytes.len()).ok_or(Failure::Limit)?;
        if bytes.is_empty() || bytes.len() > FRAME_BYTES || next > RESPONSE_BYTES {
            return Err(Failure::Limit);
        }
        loop {
            let now = self.clock.now();
            if now.boot_id != self.boot || now.ms >= self.deadline {
                return Err(Failure::Expired);
            }
            let wait = async {
                let permit = self
                    .capacity
                    .clone()
                    .acquire_many_owned(bytes.len() as u32)
                    .await
                    .map_err(|_| Failure::Blocked)?;
                let slot = self.sender.reserve().await.map_err(|_| Failure::Blocked)?;
                Ok::<_, Failure>((permit, slot))
            };
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(Failure::Blocked),
                result = wait => {
                    let (permit, slot) = result?;
                    slot.send(ObservationFrame { bytes, _permit: permit });
                    self.total = next;
                    return Ok(());
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }
}
