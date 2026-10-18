//! CPU-only admission/control walkthrough; not an experimental run.
use playtime_core::RunMode;
use playtime_protocol::worker::WorkerState;
use playtime_server::lifecycle::{
    fixture::{self, FixtureArchive, FixtureSupervisor},
    ports::SystemClock,
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
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 2 || arguments[0] != "--root" {
        return Err("usage: lifecycle_fixture --root <new-short-directory>; subprocess fixture only, systemd is a later slice".into());
    }
    let root = PathBuf::from(&arguments[1]);
    std::fs::create_dir(&root)?;
    let archive = Arc::new(FixtureArchive::new(
        &root,
        &std::env::current_exe()?,
        Vec::new(),
    )?);
    let supervisor = Arc::new(FixtureSupervisor::default());
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
                recipe_id: "lifecycle-fixture".into(),
                mode: RunMode::Simulation,
                preview_revision,
            },
        )
        .await?;
    let run = accepted.admission.run_id;
    coordinator.start(run.clone()).await?;
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
    let cancelled = restarted.cancel(run.clone()).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "kind":"supervision_fixture",
            "service_id":restarted.ledger().service_id(),
            "run_id":run,
            "same_surviving_worker_pid":original,
            "phase":cancelled.phase,
            "worker_status":cancelled.last_status,
            "trace":restarted.ledger().trace(&run).await?,
            "limitation":"fixture cancellation is acknowledged; full stop enforcement and recovery remain in #34"
        }))?
    );
    supervisor.terminate(cancelled.prepared.as_ref().ok_or("missing preparation")?)?;
    Ok(())
}
