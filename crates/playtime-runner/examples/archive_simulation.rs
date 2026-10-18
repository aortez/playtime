use std::{env, fs, path::PathBuf};

use playtime_core::{RunId, UtcTime};
use playtime_runner::{
    DriverOptions, VirtualClock, capture_demo_worker, prepare_simulation, run_prepared,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = None;
    let mut output = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--scenario" => scenario = arguments.next().map(PathBuf::from),
            "--output" => output = arguments.next().map(PathBuf::from),
            _ => {
                return Err(
                    "usage: archive_simulation --scenario <scenario.json> --output <new-directory>"
                        .into(),
                );
            }
        }
    }
    let scenario = scenario.ok_or("--scenario is required")?;
    let output = output.ok_or("--output is required")?;
    let worker = capture_demo_worker()?;
    let prepared = prepare_simulation(
        &output,
        RunId::new("simulation")?,
        &fs::read(scenario)?,
        worker,
    )?;
    let mut clock = VirtualClock::new(UtcTime::new("2026-10-17T00:00:00Z")?);
    let report = run_prepared(&output, &prepared, &mut clock, DriverOptions::default())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "archive":output,"mode":"simulation","terminal_committed":report.terminal_committed,
            "frontier":report.confirmed.frontier(),"result":report.result,"diagnostics":report.diagnostics,
            "dispatched_effects":report.dispatches.len(),"log_bytes":report.accounted_log_bytes,"peak_queued_bytes":report.peak_queued_bytes,
            "publication_unresolved":report.pending_publication.is_some()
        }))?
    );
    if !report.terminal_committed {
        return Err(
            "archive remains incomplete; inspect committed evidence and diagnostics".into(),
        );
    }
    Ok(())
}
