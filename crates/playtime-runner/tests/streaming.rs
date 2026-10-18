mod common;

use common::*;
use playtime_core::StopReason;
use playtime_engine::simulation::{ModelScript, TimedChunk};
use playtime_protocol::archive_queries::ArchiveFact;
use playtime_runner::{
    ArchiveQueries, CommitContext, DriverClock, DriverOptions, ReceiptDelivery, VirtualClock,
    WriterHooks, run_prepared,
};
use playtime_storage::{ArchiveWriter, CommittedFrontier, StorageLimits};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Seen {
    clock: VirtualClock,
    commits: Arc<Mutex<Vec<(u64, CommitContext)>>>,
}
impl WriterHooks for Seen {
    fn after(&mut self, context: &CommitContext, _: &CommittedFrontier) -> ReceiptDelivery {
        self.commits
            .lock()
            .unwrap()
            .push((self.clock.now_ms(), context.clone()));
        ReceiptDelivery::Deliver
    }
}

#[test]
fn streaming_batches_flush_at_time_or_size_and_large_lines_stand_alone() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.models = vec![ModelScript {
        chunks: vec![
            TimedChunk {
                after_ms: 10,
                chunk: json!({"message":{"content":"first"}}),
            },
            TimedChunk {
                after_ms: 10,
                chunk: json!({"message":{"content":"second"}}),
            },
            TimedChunk {
                after_ms: 280,
                chunk: json!({"message":{"content":"x".repeat(70_000)}}),
            },
        ],
        completion: None,
    }];
    let prepared = prepared(&temp, &scenario);
    let mut clock = clock();
    let seen = Seen {
        clock: clock.clone(),
        commits: Arc::new(Mutex::new(Vec::new())),
    };
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock,
        DriverOptions {
            writer_hooks: Box::new(seen.clone()),
            ..DriverOptions::default()
        },
    )
    .unwrap();
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        StopReason::ModelTimeout
    );
    assert_eq!(
        report
            .result
            .as_ref()
            .unwrap()
            .usage
            .unknown_reserved_tokens,
        100
    );
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let history = queries.history(0, 1000, true).unwrap();
    let chunks: Vec<_> = history.entries.iter().filter(|entry| matches!(&entry.fact, ArchiveFact::Observation { record:Some(record), .. } if record.kind == "model_chunk")).collect();
    assert_eq!(chunks.len(), 3);
    let commits = seen.commits.lock().unwrap();
    let small = commits
        .iter()
        .find(|(_, context)| {
            context.first_sequence <= chunks[0].envelope.sequence.get()
                && context.last_sequence >= chunks[0].envelope.sequence.get()
        })
        .unwrap();
    assert_eq!(small.0, 260);
    assert!(small.1.last_sequence >= chunks[1].envelope.sequence.get());
    let large = commits
        .iter()
        .find(|(_, context)| {
            context.first_sequence <= chunks[2].envelope.sequence.get()
                && context.last_sequence >= chunks[2].envelope.sequence.get()
        })
        .unwrap();
    assert_eq!(large.0, 300);
    assert_eq!(large.1.first_sequence, large.1.last_sequence);
    assert!(report.peak_queued_bytes < 1_048_576);
}

struct AdvanceDuringModelIntent {
    clock: VirtualClock,
}
impl WriterHooks for AdvanceDuringModelIntent {
    fn before(
        &mut self,
        context: &CommitContext,
        _: &mut ArchiveWriter,
    ) -> playtime_runner::Result<()> {
        if context.effect_ids.contains(&2)
            && context.kinds.iter().any(|kind| kind == "effect_intent")
        {
            self.clock.advance_to(30_000);
        }
        Ok(())
    }
}
#[test]
fn expired_intent_receipt_is_followed_by_non_dispatch_proof_and_never_an_adapter_call() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let mut clock = clock();
    let hook = AdvanceDuringModelIntent {
        clock: clock.clone(),
    };
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock,
        DriverOptions {
            writer_hooks: Box::new(hook),
            ..DriverOptions::default()
        },
    )
    .unwrap();
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        StopReason::ModelTimeout
    );
    assert_eq!(
        report.result.as_ref().unwrap().usage.budget_charged_tokens,
        0
    );
    assert!(
        !report
            .dispatches
            .iter()
            .any(|item| matches!(item.effect.kind, playtime_engine::EffectKind::Model(_)))
    );
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert!(queries.run().unwrap().unsettled_model_intents.is_empty());
    assert!(
        queries
            .history(0, 1000, true)
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.envelope.kind == "request_not_dispatched")
    );
}

#[test]
fn preparation_cannot_execute_tampered_inputs_or_silently_refresh_a_lock() {
    let temp = Temp::new();
    let prepared = prepared(&temp, &scenario());
    let manifest_before = std::fs::read(temp.archive().join("inputs/manifest.json")).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_before).unwrap();
    std::fs::write(
        temp.archive()
            .join(manifest["configuration"]["path"].as_str().unwrap()),
        b"{}",
    )
    .unwrap();
    assert!(
        run_prepared(
            temp.archive(),
            &prepared,
            &mut clock(),
            DriverOptions::default()
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(temp.archive().join("inputs/manifest.json")).unwrap(),
        manifest_before
    );
    assert!(
        std::fs::read(temp.archive().join("events.jsonl"))
            .unwrap()
            .is_empty()
    );
}
