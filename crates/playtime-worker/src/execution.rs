use crate::{Error, PrivateDirectory, Result, bundle::VerifiedBundle};
use playtime_core::UtcTime;
use playtime_engine::simulation::Scenario;
use playtime_protocol::worker::{Receipt, Status, WorkerKind, WorkerState};
use playtime_runner::{
    Cancellation, DriverClock, DriverOptions, LatestReceipt, PreparedExecution, PreparedSimulation,
};
use playtime_storage::{ArchiveReader, CommittedFrontier, StorageLimits, content_ref};
use playtime_worker_control::{Bootstrap, PreparedExecution as ControlExecution, boot_ms};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Execution {
    prepared: Option<PreparedExecution>,
    latest: LatestReceipt,
    cancel: Cancellation,
    finished: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    pending: Arc<AtomicBool>,
    wall_ms: u64,
    work_deadline: Arc<AtomicU64>,
    /// Held for the worker's whole life so only one live run uses the model.
    _live_lock: Option<playtime_worker_control::FileLock>,
}
impl Execution {
    pub async fn acquire(bootstrap: &Bootstrap) -> Result<Self> {
        if !matches!(
            bootstrap.binding.kind,
            WorkerKind::ControllerSimulation | WorkerKind::DockerScripted | WorkerKind::DockerLive
        ) {
            return Err(Error::UnsupportedCapability);
        }
        let path = bootstrap
            .root
            .join("runs")
            .join(bootstrap.binding.run_id.as_str());
        let limits = StorageLimits::default();
        let reader = ArchiveReader::open(&path, limits.clone())?;
        if reader.identity().run_id != bootstrap.binding.run_id
            || reader.identity().input_manifest.sha256 != bootstrap.binding.manifest_sha256
            || reader.manifest().worker.executable.sha256 != bootstrap.binding.executable_sha256
        {
            return Err(Error::Invalid("admitted archive identity mismatch"));
        }
        let configuration = &reader.manifest().configuration;
        let bytes = PrivateDirectory::open(&path)?
            .read(configuration.path.as_str(), limits.content_bytes)?;
        if content_ref(configuration.path.clone(), &bytes) != *configuration {
            return Err(Error::Invalid("configuration changed"));
        }
        let live = bootstrap.binding.kind == WorkerKind::DockerLive;
        let native = live || bootstrap.binding.kind == WorkerKind::DockerScripted;
        let profile = if native {
            Some(serde_json::from_slice::<playtime_runner::docker::Profile>(
                &bytes,
            )?)
        } else {
            None
        };
        let scenario: Scenario = if let Some(profile) = &profile {
            profile.validate()?;
            if profile.model.is_some() != live {
                return Err(Error::Invalid("worker kind and frozen profile differ"));
            }
            if std::env::var_os("PLAYTIME_SYSTEMD_WORKER").is_none() {
                return Err(Error::Invalid(
                    "real Docker effects require systemd supervision",
                ));
            }
            profile.scenario.clone()
        } else {
            serde_json::from_slice(&bytes)?
        };
        if scenario
            .parameters
            .limits
            .wall_seconds
            .get()
            .checked_mul(1000)
            != Some(bootstrap.wall_ms)
        {
            return Err(Error::Invalid(
                "bootstrap wall allocation differs from frozen configuration",
            ));
        }
        let prepared = PreparedSimulation {
            identity: reader.identity().clone(),
            limits: StorageLimits {
                event_log_bytes: scenario.parameters.limits.log_bytes.get(),
                ..limits
            },
        };
        // Take the machine-wide lock before any Docker or model work. Without
        // it this worker fails before Begin.
        let live_lock = profile
            .as_ref()
            .and_then(|profile| profile.model.as_ref())
            .map(|model| {
                playtime_worker_control::FileLock::acquire(&model.host.live_lock)
                    .map_err(|_| Error::Invalid("another live run holds the live-model lock"))
            })
            .transpose()?;
        let owned = if let Some(profile) = profile {
            let effects = crate::docker::NativeEffects::prepare(profile, bootstrap).await?;
            PreparedExecution::acquire_docker(&path, &prepared, Box::new(effects))?
        } else {
            PreparedExecution::acquire(&path, &prepared)?
        };
        let snapshot = reader.live_snapshot(owned.confirmed())?;
        let bundle = VerifiedBundle::from_archive(&reader, &snapshot)?;
        bundle.verify_embedded(crate::EMBEDDED_BUILD)?;
        // A bundle never advertises a profile its own worker does not implement.
        let capabilities = &bundle.descriptor().capabilities;
        let profiles = playtime_runner::docker::digest()?;
        if (live && !capabilities.implements_live(&profiles))
            || (native && !capabilities.implements_docker(&profiles))
        {
            return Err(Error::UnsupportedCapability);
        }
        let latest = LatestReceipt::default();
        Ok(Self {
            prepared: Some(owned),
            latest,
            cancel: Cancellation::default(),
            finished: Arc::default(),
            failed: Arc::default(),
            pending: Arc::default(),
            wall_ms: bootstrap.wall_ms,
            work_deadline: Arc::default(),
            _live_lock: live_lock,
        })
    }
}
pub fn receipt(confirmed: &CommittedFrontier) -> Receipt {
    let f = confirmed.frontier();
    Receipt {
        sequence: f.committed_sequence,
        bytes: f.committed_bytes,
        sha256: f.prefix_sha256.clone(),
    }
}
impl ControlExecution for Execution {
    fn prepared_receipt(&self) -> Option<Receipt> {
        self.prepared
            .as_ref()
            .map(|owned| receipt(owned.confirmed()))
            .or_else(|| self.latest.latest().as_ref().map(receipt))
    }
    fn begin(&mut self, started_boot_ms: u64) -> playtime_worker_control::Result<Receipt> {
        self.work_deadline.store(
            started_boot_ms.saturating_add(self.wall_ms),
            Ordering::Release,
        );
        let owned = self
            .prepared
            .take()
            .ok_or(playtime_worker_control::Error::Invalid(
                "execution already consumed",
            ))?;
        let options = DriverOptions {
            confirmed: self.latest.clone(),
            cancellation: self.cancel.clone(),
            ..DriverOptions::default()
        };
        let (finished, failed, pending) = (
            self.finished.clone(),
            self.failed.clone(),
            self.pending.clone(),
        );
        pending.store(true, Ordering::Release);
        // The grant is already durably consumed. This owned thread outlives lost
        // replies and retains a late publication until its operation settles.
        std::thread::spawn(move || {
            let result = owned.execute(&mut WorkClock::new(started_boot_ms), options);
            let success = match result {
                Ok(report) => {
                    let mut terminal = report.terminal_committed
                        && report.result.as_ref().is_some_and(|result| {
                            !matches!(
                                result.reason,
                                playtime_core::StopReason::InfrastructureFailure
                                    | playtime_core::StopReason::Interrupted
                            ) && result.cleanup
                                == playtime_protocol::events::CleanupOutcome::Complete
                        });
                    if let Some(publication) = report.pending_publication {
                        loop {
                            if let Some(result) = publication.poll() {
                                // A late receipt preserves evidence, but does not
                                // turn an interrupted driver into completed work.
                                terminal &= result.is_ok();
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    terminal
                }
                Err(_) => false,
            };
            failed.store(!success, Ordering::Release);
            pending.store(false, Ordering::Release);
            finished.store(true, Ordering::Release);
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(receipt) = self.prepared_receipt()
                && receipt.sequence > 0
            {
                return Ok(receipt);
            }
            if Instant::now() >= deadline || self.finished.load(Ordering::Acquire) {
                return Err(playtime_worker_control::Error::Deadline);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn cancel(&mut self, _before_begin: bool) -> playtime_worker_control::Result<()> {
        self.cancel.cancel();
        // Keep an unused prepared writer until this invocation is gone.
        Ok(())
    }
    fn refresh_status(&self, status: &mut Status) {
        // Acquire completion before sampling its receipt, so a settled status
        // cannot accidentally pair completion with a pre-completion sample.
        status.publication_pending = self.pending.load(Ordering::Acquire);
        status.latest_receipt = self.prepared_receipt();
        if self.failed.load(Ordering::Acquire) {
            status.state = WorkerState::Reconciling;
        } else if status.state == WorkerState::Reconciling
            && status.work_started_boot_ms.is_some()
            && status
                .latest_receipt
                .as_ref()
                .is_some_and(|r| r.sequence > 0)
        {
            status.state = WorkerState::Running;
        }
    }
}

struct WorkClock {
    first: std::cell::Cell<Option<u64>>,
}
impl WorkClock {
    fn new(start: u64) -> Self {
        Self {
            first: std::cell::Cell::new(Some(start)),
        }
    }
}
impl DriverClock for WorkClock {
    fn now_ms(&self) -> u64 {
        self.first.take().unwrap_or_else(boot_ms)
    }
    fn utc(&self) -> UtcTime {
        UtcTime::new(
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("UTC formatting"),
        )
        .expect("system UTC")
    }
    fn idle(&mut self, next_ms: u64, _storage_pending: bool) {
        std::thread::sleep(Duration::from_millis(
            next_ms.saturating_sub(boot_ms()).clamp(1, 5),
        ));
    }
}

pub async fn serve(path: &Path) -> Result<()> {
    if playtime_worker_control::systemd::record_exit_if_requested(path)? {
        return Ok(());
    }
    // Install before preparation. SIGTERM requests controller cancellation; it
    // cannot drop a pending storage owner. systemd independently bounds grace.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let supervised = std::env::var_os("PLAYTIME_SYSTEMD_WORKER").is_some();
    let bootstrap = Bootstrap::read(path)?;
    let reservation: playtime_protocol::worker::Binding =
        serde_json::from_slice(&playtime_worker_control::read_private(
            &bootstrap.private_dir.join("reservation.json"),
            65_536,
        )?)?;
    if reservation != bootstrap.binding {
        return Err(Error::Invalid("reservation binding mismatch"));
    }
    let run_lock = loop {
        match playtime_worker_control::FileLock::acquire(&bootstrap.root.join(".playtime/run.lock"))
        {
            Ok(lock) => break lock,
            Err(playtime_worker_control::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && boot_ms() < bootstrap.startup_deadline_boot_ms =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
            Err(error) => return Err(error.into()),
        }
    };
    let execution = Execution::acquire(&bootstrap).await?;
    let cancellation = execution.cancel.clone();
    let finished = execution.finished.clone();
    let work_deadline = execution.work_deadline.clone();
    let notify = if supervised {
        Some(playtime_worker_control::systemd::Notify::attach(
            &bootstrap,
        )?)
    } else {
        None
    };
    let mut worker =
        playtime_worker_control::Worker::prepared(bootstrap.clone(), execution, run_lock)?;
    if let Some(notify) = notify {
        worker = worker.with_systemd(notify);
    }
    let owner = Arc::new(tokio::sync::Mutex::new(worker));
    let listener = tokio::net::UnixListener::bind(&bootstrap.socket)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bootstrap.socket, std::fs::Permissions::from_mode(0o600))?;
    let mut connections = tokio::task::JoinSet::new();
    let mut stopping = false;
    loop {
        // The #34 independent supervisor owns startup/hard-stop enforcement.
        // This process keeps control/receipts available through finalization.
        tokio::select! {
            _ = terminate.recv() => {
                stopping = true;
                // At the original hard wall let the controller's next clock
                // observation establish WallLimit. A manager signal must not
                // manufacture an earlier cancellation or rewrite a first stop.
                let deadline = work_deadline.load(Ordering::Acquire);
                if deadline == 0 || boot_ms() < deadline { cancellation.cancel(); }
            }
            connected = listener.accept(), if !stopping => {
                let (stream, _) = connected?;
                if connections.len() >= 8 { continue; }
                let (owner, credential) = (owner.clone(), bootstrap.credential.clone());
                connections.spawn(async move { let _ = playtime_worker_control::serve_connection(stream, owner, &credential, rustix::process::getuid().as_raw()).await; });
            }
            _ = connections.join_next(), if !connections.is_empty() => (),
            _ = tokio::time::sleep(Duration::from_millis(20)), if stopping => {
                if finished.load(Ordering::Acquire)
                    || owner.try_lock().is_ok_and(|w| w.status().work_started_boot_ms.is_none()) {
                    return Ok(());
                }
            }
        }
    }
}
