use super::{
    Admission, AdmissionRequest, ExternalResources, PreparedLaunch, Result, Selection, Stamp,
};
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

    /// Authenticated-reply validation, before a guarded ledger commit. A
    /// returned frontier may be published only after that transaction succeeds.
    async fn confirmed_read(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> Result<Option<playtime_protocol::storage::EventFrontier>> {
        self.verify_status(prepared, previous, status).await?;
        Ok(None)
    }

    /// Normal completion only. Must validate the saved confirmed boundary,
    /// terminal record, settled publication and successful resource cleanup.
    async fn normal_completion(&self, _admission: &Admission) -> Result<bool> {
        Ok(false)
    }

    /// Operator release only: check every external resource the run's worker
    /// may own, by exact identity. An error means the check was not possible.
    async fn external_resources(&self, _admission: &Admission) -> Result<ExternalResources> {
        Err(super::Error::Invalid(
            "external resource verification is unsupported",
        ))
    }
}

#[async_trait]
pub trait Supervisor: Send + Sync + 'static {
    /// Called while holding Python-compatible run exclusion. Unknown legacy
    /// unit state must fail closed, including the launcher/worker handoff gap.
    async fn ensure_legacy_idle(&self, root: &std::path::Path) -> Result<()>;
    /// Exactly one submission after the ledger's start intent. Errors are
    /// ambiguous; the coordinator must query this same invocation, never retry.
    /// Ok(None) acknowledges submission while fork/exec or preparation is pending.
    async fn start(&self, launch: &PreparedLaunch) -> Result<Option<Endpoint>>;
    /// Evidence comes from the supervisor, not self-reported worker JSON.
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint>;
    /// None explicitly means this fixture port has no independent enforcement.
    async fn observe(
        &self,
        _launch: &PreparedLaunch,
    ) -> Result<Option<super::SupervisorObservation>> {
        Ok(None)
    }
    /// Submit or recover the same stop job using previously authenticated
    /// identity and fresh manager checks, independently of private-file I/O.
    /// Success acknowledges enforcement only; it does not prove termination.
    async fn stop(
        &self,
        _launch: &PreparedLaunch,
        _authenticated: &super::SupervisorObservation,
    ) -> Result<()> {
        Err(super::Error::Invalid("supervisor stop is unsupported"))
    }
}
