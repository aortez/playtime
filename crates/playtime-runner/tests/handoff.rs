mod common;

use common::*;
use playtime_core::RunId;
use playtime_runner::{
    ArchiveQueries, CommitContext, DriverOptions, LatestReceipt, PreparedSimulation,
    ReceiptDelivery, WriterHooks, capture_demo_worker, prepare_simulation, run_prepared,
};
use playtime_storage::{ArchiveWriter, CommittedFrontier, StorageLimits, sha256};
use std::{
    fs,
    process::Command,
    sync::{Arc, Mutex},
};

struct HandoffLock {
    path: std::path::PathBuf,
    prepared: PreparedSimulation,
}
impl WriterHooks for HandoffLock {
    fn before(&mut self, _: &CommitContext, _: &mut ArchiveWriter) -> playtime_runner::Result<()> {
        let error = ArchiveWriter::open_prepared(
            &self.path,
            &self.prepared.identity,
            self.prepared.limits.clone(),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, playtime_storage::ErrorCode::WriterBusy);
        Ok(())
    }
}

#[test]
fn prepared_owner_keeps_one_writer_through_initial_commit_without_starting_work_early() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let owner = playtime_runner::PreparedExecution::acquire(temp.archive(), &prepared).unwrap();
    assert_eq!(owner.confirmed().frontier().committed_sequence, 0);
    assert_eq!(
        fs::metadata(temp.archive().join("events.jsonl"))
            .unwrap()
            .len(),
        0
    );
    assert!(ArchiveQueries::offline(temp.archive(), prepared.limits.clone()).is_err());
    let error =
        ArchiveWriter::open_prepared(temp.archive(), &prepared.identity, prepared.limits.clone())
            .err()
            .unwrap();
    assert_eq!(error.code, playtime_storage::ErrorCode::WriterBusy);
    let report = owner
        .execute(
            &mut clock(),
            DriverOptions {
                writer_hooks: Box::new(HandoffLock {
                    path: temp.archive(),
                    prepared,
                }),
                ..DriverOptions::default()
            },
        )
        .unwrap();
    assert!(report.terminal_committed);
}

#[test]
fn dropping_unused_preparation_creates_no_experimental_events() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let owner = playtime_runner::PreparedExecution::acquire(temp.archive(), &prepared).unwrap();
    drop(owner);
    let queries = ArchiveQueries::offline(temp.archive(), prepared.limits).unwrap();
    assert_eq!(queries.snapshot().frontier().committed_sequence, 0);
    assert!(queries.run().unwrap().outcome.is_none());
}

#[test]
#[ignore = "subprocess helper invoked by demo_capture_reads_the_executing_image_after_unlink"]
fn unlinked_image_child() {
    let path = std::env::var_os("PLAYTIME_RUNNER_IMAGE_COPY").unwrap();
    let expected = std::env::var("PLAYTIME_RUNNER_IMAGE_HASH").unwrap();
    fs::remove_file(path).unwrap();
    let bundle = capture_demo_worker().unwrap();
    assert_eq!(sha256(&bundle.executable).as_str(), expected);
    let source: serde_json::Value = serde_json::from_slice(&bundle.source).unwrap();
    assert_eq!(
        source["capture_method"],
        "demo_cargo_build_script_live_workspace_not_isolated"
    );
}

#[test]
fn demo_capture_reads_the_executing_image_after_unlink() {
    let temp = Temp::new();
    let image = temp.path.join("worker-copy");
    fs::copy("/proc/self/exe", &image).unwrap();
    let expected = sha256(&fs::read(&image).unwrap());
    let child = Command::new(&image)
        .args(["--ignored", "--exact", "unlinked_image_child"])
        .env("PLAYTIME_RUNNER_IMAGE_COPY", &image)
        .env("PLAYTIME_RUNNER_IMAGE_HASH", expected.as_str())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(!image.exists());
}

#[test]
#[ignore = "subprocess helper invoked by a_separate_worker_must_match_the_explicit_prepared_bundle"]
fn prepared_worker_child() {
    let path = std::path::PathBuf::from(std::env::var_os("PLAYTIME_RUNNER_PREPARED_PATH").unwrap());
    let identity =
        serde_json::from_str(&std::env::var("PLAYTIME_RUNNER_PREPARED_IDENTITY").unwrap()).unwrap();
    let mut limits = StorageLimits::default();
    limits.event_log_bytes = scenario().parameters.limits.log_bytes.get();
    let prepared = PreparedSimulation { identity, limits };
    let result = run_prepared(path, &prepared, &mut clock(), DriverOptions::default());
    if std::env::var("PLAYTIME_RUNNER_EXPECT_MATCH").unwrap() == "yes" {
        assert!(result.unwrap().terminal_committed);
    } else {
        assert!(result.is_err());
    }
}

#[test]
fn a_separate_worker_must_match_the_explicit_prepared_bundle() {
    for matches in [false, true] {
        let temp = Temp::new();
        let mut bundle = capture_demo_worker().unwrap();
        if !matches {
            bundle.executable = b"a different explicitly supplied worker".to_vec();
        }
        let prepared = prepare_simulation(
            temp.archive(),
            RunId::new("simulation").unwrap(),
            &serde_json::to_vec(&scenario()).unwrap(),
            bundle,
        )
        .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "prepared_worker_child"])
            .env("PLAYTIME_RUNNER_PREPARED_PATH", temp.archive())
            .env(
                "PLAYTIME_RUNNER_PREPARED_IDENTITY",
                serde_json::to_string(&prepared.identity).unwrap(),
            )
            .env(
                "PLAYTIME_RUNNER_EXPECT_MATCH",
                if matches { "yes" } else { "no" },
            )
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{} {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        assert_eq!(queries.run().unwrap().incomplete_evidence, !matches);
        if !matches {
            assert_eq!(queries.snapshot().frontier().committed_sequence, 0);
        }
    }
}

struct Visibility {
    receipt: LatestReceipt,
    observations: Arc<Mutex<Vec<(u64, u64)>>>,
    lose_terminal: bool,
}
impl WriterHooks for Visibility {
    fn before(
        &mut self,
        _: &CommitContext,
        writer: &mut ArchiveWriter,
    ) -> playtime_runner::Result<()> {
        // This runs on the storage thread. Sampling sees only memory and never
        // waits on the filesystem operation or on this callback's return.
        assert_eq!(
            self.receipt.latest().unwrap().frontier(),
            writer.confirmed_frontier().frontier()
        );
        Ok(())
    }
    fn after(&mut self, context: &CommitContext, actual: &CommittedFrontier) -> ReceiptDelivery {
        let visible = self.receipt.latest().unwrap();
        assert!(visible.frontier().committed_sequence < actual.frontier().committed_sequence);
        self.observations.lock().unwrap().push((
            visible.frontier().committed_sequence,
            actual.frontier().committed_sequence,
        ));
        if self.lose_terminal && context.kinds.iter().any(|kind| kind == "run_finished") {
            ReceiptDelivery::Lose
        } else {
            ReceiptDelivery::Deliver
        }
    }
}
#[test]
fn latest_receipt_is_coalesced_and_advances_only_after_the_returned_publication_boundary() {
    for lose_terminal in [false, true] {
        let temp = Temp::new();
        let prepared = prepared(&temp, &scenario());
        let latest = LatestReceipt::default();
        assert!(latest.latest().is_none());
        let observations = Arc::new(Mutex::new(Vec::new()));
        let hook = Visibility {
            receipt: latest.clone(),
            observations: observations.clone(),
            lose_terminal,
        };
        let report = run_prepared(
            temp.archive(),
            &prepared,
            &mut clock(),
            DriverOptions {
                confirmed: latest.clone(),
                writer_hooks: Box::new(hook),
                ..DriverOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            latest.latest().unwrap().frontier(),
            report.confirmed.frontier()
        );
        assert_eq!(report.terminal_committed, !lose_terminal);
        let observations = observations.lock().unwrap();
        assert_eq!(observations[0].0, 0);
        for pair in observations.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
        if lose_terminal {
            assert!(
                observations.last().unwrap().1
                    > latest.latest().unwrap().frontier().committed_sequence
            );
        } else {
            assert_eq!(
                observations.last().unwrap().1,
                latest.latest().unwrap().frontier().committed_sequence
            );
        }
    }
}
