use crate::{Error, PrivateDirectory, Result, bundle::VerifiedBundle};
use playtime_core::{StopReason, UtcTime};
use playtime_engine::simulation::Scenario;
use playtime_protocol::{
    events::CleanupOutcome,
    worker::{self, Receipt, Status, WorkerKind, WorkerState},
};
use playtime_runner::{
    Cancellation, DriverClock, DriverOptions, LatestReceipt, PreparedExecution, PreparedSimulation,
};
use playtime_storage::{ArchiveReader, CommittedFrontier, StorageLimits, content_ref};
use playtime_worker_control::{Bootstrap, PreparedExecution as ControlExecution, boot_ms};
use std::{
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct Execution {
    prepared: Option<PreparedExecution>,
    latest: LatestReceipt,
    cancel: Cancellation,
    finished: Arc<AtomicBool>,
    /// Set once when the execution ends without a normal outcome: why it
    /// failed, or None for an interruption that was asked for and ended
    /// cleanly.
    ended: Arc<OnceLock<Option<String>>>,
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
            Some(playtime_runner::docker::NativeProfile::decode(&bytes)?)
        } else {
            None
        };
        let (wall_seconds, storage) = if let Some(profile) = &profile {
            if profile.model().is_some() != live {
                return Err(Error::Invalid("worker kind and frozen profile differ"));
            }
            if std::env::var_os("PLAYTIME_SYSTEMD_WORKER").is_none() {
                return Err(Error::Invalid(
                    "real Docker effects require systemd supervision",
                ));
            }
            (profile.wall_seconds(), profile.storage().clone())
        } else {
            let scenario: Scenario = serde_json::from_slice(&bytes)?;
            (
                scenario.parameters.limits.wall_seconds.get(),
                StorageLimits {
                    event_log_bytes: scenario.parameters.limits.log_bytes.get(),
                    ..limits
                },
            )
        };
        if wall_seconds.checked_mul(1000) != Some(bootstrap.wall_ms) {
            return Err(Error::Invalid(
                "bootstrap wall allocation differs from frozen configuration",
            ));
        }
        let prepared = PreparedSimulation {
            identity: reader.identity().clone(),
            limits: storage,
        };
        // Take the machine-wide lock before any Docker or model work. Without
        // it this worker fails before Begin.
        let live_lock = profile
            .as_ref()
            .and_then(|profile| profile.model())
            .map(|model| {
                playtime_worker_control::FileLock::acquire(&model.host.live_lock)
                    .map_err(|_| Error::Invalid("another live run holds the live-model lock"))
            })
            .transpose()?;
        let owned = if let Some(profile) = profile {
            let effects = crate::docker::NativeEffects::prepare_native(profile, bootstrap).await?;
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
            ended: Arc::default(),
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
        let (finished, ended, pending) = (
            self.finished.clone(),
            self.ended.clone(),
            self.pending.clone(),
        );
        pending.store(true, Ordering::Release);
        // The grant is already durably consumed. This owned thread outlives lost
        // replies and retains a late publication until its operation settles.
        std::thread::spawn(move || {
            let result = owned.execute(&mut WorkClock::new(started_boot_ms), options);
            let (outcome, settled) = match result {
                Ok(mut report) => {
                    for diagnostic in &report.diagnostics {
                        eprintln!("worker execution: {diagnostic}");
                    }
                    let late = report.pending_publication.take().map(|publication| {
                        loop {
                            if let Some(result) = publication.poll() {
                                let late = result.map(drop).map_err(|error| error.to_string());
                                eprintln!("worker execution: late publication {late:?}");
                                break late;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    });
                    let result = report
                        .result
                        .as_ref()
                        .map(|result| (&result.reason, &result.cleanup));
                    let outcome =
                        ending(result, report.terminal_committed, &report.diagnostics, late);
                    (outcome, true)
                }
                Err(error) => {
                    eprintln!("worker execution failed: {error}");
                    // A write the driver began may still run on its own thread,
                    // so publication never counts as settled.
                    let text = format!("worker execution failed: {error}");
                    (Ending::Failed(text), false)
                }
            };
            // Set before publication settles, so a settled status carries it.
            match outcome {
                Ending::Normal => {}
                Ending::Interrupted => {
                    let _ = ended.set(None);
                }
                Ending::Failed(text) => {
                    let _ = ended.set(Some(worker::failure_text(&text)));
                }
            }
            if settled {
                pending.store(false, Ordering::Release);
            }
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
        if let Some(failure) = self.ended.get() {
            status.state = WorkerState::Reconciling;
            status.failure = failure.clone();
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

/// How an execution ended.
#[derive(Debug, PartialEq)]
enum Ending {
    /// A committed outcome the daemon can confirm as normal completion.
    Normal,
    /// A cancellation or stop that was asked for and ended cleanly. It isn't
    /// normal completion, but nothing failed.
    Interrupted,
    /// Anything else, and why: the driver's diagnostics come first.
    Failed(String),
}

/// Classifies an execution from its outcome and cleanup, when the controller
/// recorded them, whether the driver saw that outcome committed, the driver's
/// diagnostics, and the result of a publication still pending when the driver
/// returned, if there was one.
fn ending(
    result: Option<(&StopReason, &CleanupOutcome)>,
    terminal_committed: bool,
    diagnostics: &[String],
    late: Option<std::result::Result<(), String>>,
) -> Ending {
    // A late receipt preserves evidence, but does not turn an interrupted
    // driver into completed work.
    let committed = terminal_committed && late.as_ref().is_none_or(|late| late.is_ok());
    match result {
        Some((StopReason::Interrupted, CleanupOutcome::Complete))
            if committed && diagnostics.is_empty() =>
        {
            return Ending::Interrupted;
        }
        Some((reason, CleanupOutcome::Complete))
            if committed
                && !matches!(
                    reason,
                    StopReason::InfrastructureFailure | StopReason::Interrupted
                ) =>
        {
            return Ending::Normal;
        }
        _ => {}
    }
    // The same snake_case name the archive records.
    let name = |reason: &StopReason| {
        serde_json::to_value(reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{reason:?}"))
    };
    let mut text = match result {
        _ if !diagnostics.is_empty() => diagnostics.join("; "),
        None => "the controller recorded no outcome".into(),
        _ if !terminal_committed => "the outcome was not committed".into(),
        Some((reason, CleanupOutcome::Complete)) => format!("stopped with {}", name(reason)),
        Some((reason, _)) => format!("stopped with {} and incomplete cleanup", name(reason)),
    };
    match late {
        Some(Ok(())) => text.push_str("; the late publication then committed"),
        Some(Err(error)) => text.push_str(&format!("; the late publication then failed: {error}")),
        None => {}
    }
    Ending::Failed(text)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_committed_normal_outcome_is_normal_and_a_clean_interruption_is_no_failure() {
        let complete = CleanupOutcome::Complete;
        let done = Some((&StopReason::AgentFinished, &complete));
        let interrupted = Some((&StopReason::Interrupted, &complete));
        assert_eq!(ending(done, true, &[], None), Ending::Normal);
        assert_eq!(ending(interrupted, true, &[], None), Ending::Interrupted);
        let failed = |text: &str| Ending::Failed(text.into());
        assert_eq!(
            ending(done, false, &[], None),
            failed("the outcome was not committed")
        );
        assert_eq!(
            ending(None, false, &[], None),
            failed("the controller recorded no outcome")
        );
        let infrastructure = Some((&StopReason::InfrastructureFailure, &complete));
        assert_eq!(
            ending(infrastructure, true, &[], None),
            failed("stopped with infrastructure_failure")
        );
        let incomplete = CleanupOutcome::Unknown;
        assert_eq!(
            ending(
                Some((&StopReason::Interrupted, &incomplete)),
                true,
                &[],
                None
            ),
            failed("stopped with interrupted and incomplete cleanup")
        );
        // An interruption the driver reported trouble with is a failure.
        let diagnostics = ["retired 3 queued events".to_owned()];
        assert_eq!(
            ending(interrupted, true, &diagnostics, None),
            failed("retired 3 queued events")
        );
    }
    #[test]
    fn a_late_publication_says_how_it_settled_and_never_completes_the_run() {
        let complete = CleanupOutcome::Complete;
        let done = Some((&StopReason::AgentFinished, &complete));
        let diagnostics = ["filesystem publication remains unresolved".to_owned()];
        assert_eq!(
            ending(done, false, &diagnostics, Some(Ok(()))),
            Ending::Failed(
                "filesystem publication remains unresolved; the late publication then committed"
                    .into()
            )
        );
        assert_eq!(
            ending(done, false, &diagnostics, Some(Err("disk".into()))),
            Ending::Failed(
                "filesystem publication remains unresolved; the late publication then failed: disk"
                    .into()
            )
        );
    }
}
