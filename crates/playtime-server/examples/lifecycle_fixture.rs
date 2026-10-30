//! CPU-only admission/control walkthrough; not an experimental run.
use playtime_core::RunMode;
use playtime_protocol::worker::WorkerState;
use playtime_server::lifecycle::{
    fixture::{self, FixtureArchive, FixtureSupervisor},
    ports::{Supervisor, SystemClock},
    systemd::SystemdSupervisor,
    *,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("PLAYTIME_LIFECYCLE_BOOTSTRAP") {
        fixture::run_worker(Path::new(&path)).await?;
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(260), walkthrough()).await?
}

// Failed walkthroughs retain their private records and request only their own
// unit's stop. The service manager enforces the bound even if this process dies.
struct Cleanup(Option<String>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(unit) = &self.0 {
            fixture::cleanup_systemd_unit(unit, false);
        }
    }
}
async fn walkthrough() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    let systemd = arguments.last().is_some_and(|arg| arg == "--systemd");
    if systemd {
        arguments.pop();
    }
    if arguments.len() != 2 || arguments[0] != "--root" {
        return Err("usage: lifecycle_fixture --root <new-short-directory> [--systemd]".into());
    }
    let root = PathBuf::from(&arguments[1]);
    std::fs::create_dir(&root)?;
    let archive = Arc::new(FixtureArchive::new(
        &root,
        &std::env::current_exe()?,
        Vec::new(),
    )?);
    let subprocess = Arc::new(FixtureSupervisor::default());
    let supervisor: Arc<dyn Supervisor> = if systemd {
        Arc::new(SystemdSupervisor::connect().await?)
    } else {
        subprocess.clone()
    };
    let preview_revision = archive.preview_revision();
    let ledger = Ledger::open(&root).await?;
    let coordinator = Coordinator::new(
        ledger,
        archive.clone(),
        supervisor.clone(),
        Arc::new(SystemClock),
    );
    let accepted = coordinator
        .admit(
            coordinator.ledger().service_id().clone(),
            "fixture-launch".into(),
            AdmissionRequest {
                target: None,
                recipe_id: "lifecycle-fixture".into(),
                mode: RunMode::Simulation,
                preview_revision,
            },
        )
        .await?;
    let run = accepted.admission.run_id;
    let submitted = coordinator.start(run.clone()).await?;
    let mut cleanup = Cleanup(if systemd {
        submitted.prepared.as_ref().map(|p| p.binding.unit.clone())
    } else {
        None
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let record = coordinator.refresh(run.clone()).await?;
            if record
                .last_status
                .as_ref()
                .is_some_and(|status| status.state == WorkerState::Prepared)
            {
                return Result::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let running = coordinator.begin(run.clone()).await?;
    if running.phase != Phase::Running {
        return Err("fixture did not confirm running".into());
    }
    let original = running.endpoint.as_ref().ok_or("missing endpoint")?.pid;
    drop(coordinator);
    let restarted = Coordinator::new(
        Ledger::open(&root).await?,
        archive,
        supervisor.clone(),
        Arc::new(SystemClock),
    );
    let reattached = restarted.refresh(run.clone()).await?;
    if reattached.phase != Phase::Running
        || reattached.endpoint.as_ref().map(|endpoint| endpoint.pid) != Some(original)
    {
        return Err("fixture did not reattach to its original worker".into());
    }
    let mut cancelled = restarted.cancel(run.clone()).await?;
    if systemd {
        cancelled = tokio::time::timeout(Duration::from_secs(200), async {
            loop {
                let record = restarted.refresh(run.clone()).await?;
                if record.stop_delivery == Some(StopDelivery::Stopped) {
                    return Result::Ok(record);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await??;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "kind":"supervision_fixture",
            "service_id":restarted.ledger().service_id(),
            "run_id":run,
            "same_surviving_worker_pid":original,
            "phase":cancelled.phase,
            "cancel_delivery":cancelled.cancel_delivery,
            "stop_delivery":cancelled.stop_delivery,
            "supervisor":cancelled.supervisor,
            "worker_status":cancelled.last_status,
            "trace":restarted.ledger().trace(&run).await?,
            "limitation":"synthetic execution only; stopped processes do not authorize recovery or release admission"
        }))?
    );
    if !systemd {
        subprocess.terminate(cancelled.prepared.as_ref().ok_or("missing preparation")?)?;
    }
    cleanup.0 = None;
    Ok(())
}
