use std::{ffi::OsStr, path::PathBuf};

use playtime_storage::{ArchiveReader, StorageLimits, fixture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(OsStr::new("--output")) {
        return Err("usage: archive_fixture --output <new-directory>".into());
    }
    let output = PathBuf::from(arguments.next().ok_or("missing output directory")?);
    if arguments.next().is_some() {
        return Err("unexpected argument".into());
    }
    fixture::write_two_session(&output)?;
    let reader = ArchiveReader::open(&output, StorageLimits::default())?;
    let snapshot = reader.offline_snapshot()?;
    let inspection = snapshot.inspection();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "operation": "fixture_replay",
            "controller_executed": false,
            "worker_executed": false,
            "archive": output,
            "run_id": reader.identity().run_id,
            "committed_events": snapshot.frontier().committed_sequence,
            "prefix_sha256": snapshot.frontier().prefix_sha256,
            "usage": inspection.usage,
            "checkpoints": inspection.checkpoints.len(),
            "incomplete_evidence": inspection.incomplete_evidence,
            "uncommitted_event_bytes": inspection.uncommitted_event_bytes,
            "terminal": inspection.terminal.as_ref().map(|event| &event.envelope),
            "worker_execution_checked": inspection.worker.execution_checked,
            "platform_requirements": inspection.worker.platform,
        }))?
    );
    Ok(())
}
