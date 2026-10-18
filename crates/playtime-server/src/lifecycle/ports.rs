use super::{Admission, AdmissionRequest, PreparedLaunch, Result, Selection, Stamp};
use async_trait::async_trait;
use playtime_core::UtcTime;
use playtime_protocol::worker::{Receipt, Status};
use playtime_worker_control::Endpoint;

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Stamp;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Stamp {
        let utc = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC format");
        Stamp {
            utc: UtcTime::new(utc).expect("valid UTC"),
            boot_ms: playtime_worker_control::boot_ms(),
        }
    }
}

#[async_trait]
pub trait ArchivePort: Send + Sync + 'static {
    async fn validate_preview(&self, request: &AdmissionRequest) -> Result<Selection>;
    async fn prepare(&self, admission: &Admission) -> Result<PreparedLaunch>;
    /// Validate a receipt's bytes and prefix extension; no discovered marker may
    /// construct a confirmation. The first slice supplies only a labeled fake.
    async fn verify_status(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> Result<()>;
}

#[async_trait]
pub trait Supervisor: Send + Sync + 'static {
    /// Called while holding Python-compatible run exclusion. Unknown legacy
    /// unit state must fail closed, including the launcher/worker handoff gap.
    async fn ensure_legacy_idle(&self, root: &std::path::Path) -> Result<()>;
    /// Exactly one submission after the ledger's start intent. Errors are
    /// ambiguous; the coordinator must query this same invocation, never retry.
    async fn start(&self, launch: &PreparedLaunch) -> Result<Endpoint>;
    /// Evidence comes from the supervisor, not self-reported worker JSON.
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint>;
}
