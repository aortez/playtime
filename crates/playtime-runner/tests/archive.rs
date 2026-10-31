mod common;

use common::*;
use playtime_core::{MemoryCondition, StopReason};
use playtime_engine::simulation::{ModelEnding, ModelScript, TimedCompletion};
use playtime_protocol::{
    archive_queries::{ArchiveFact, ObservationResolution},
    bridge::FileContent,
    observations::ObservationData,
};
use playtime_runner::ArchiveQueries;
use playtime_storage::{StorageLimits, sha256};
use serde_json::{Value, json};
use std::{fs, num::NonZeroU64};

#[test]
fn real_controller_archive_survives_folder_only_copy_with_exact_transcript_and_individual_content()
{
    let temp = Temp::new();
    let scenario = scenario();
    let expected = playtime_engine::simulation::run(scenario.clone()).unwrap();
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert!(report.pending_publication.is_none());
    assert_eq!(report.result.as_ref().unwrap(), &expected.result);
    assert_eq!(report.dispatches, expected.dispatches);
    let copied = temp.path.join("copied");
    copy(&temp.archive(), &copied);
    fs::remove_dir_all(temp.archive()).unwrap();
    let queries = ArchiveQueries::offline(&copied, StorageLimits::default()).unwrap();
    let run = queries.run().unwrap();
    // A v1 archive's overview has the same run, and no accounting or captures.
    let mut visited = 0;
    let overview = queries
        .overview(|_| {
            visited += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(overview.run, run);
    assert!(overview.request_accounting.is_none() && overview.captures.is_empty());
    assert_eq!(visited, run.frontier.committed_sequence);
    assert_eq!(run.usage.known_generated_tokens, 26);
    assert_eq!(run.first_stop, Some(StopReason::AgentFinished));
    assert!(!run.incomplete_evidence);
    assert_eq!(run.frontier, *report.confirmed.frontier());
    let history = queries.history(0, 1000, true).unwrap();
    assert!(!history.has_more);
    let originals: Vec<_> = history
        .entries
        .iter()
        .filter_map(|entry| match &entry.fact {
            ArchiveFact::Observation {
                record: Some(record),
                ..
            } => Some(serde_json::to_value(record).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(
        originals,
        expected
            .records
            .iter()
            .map(|record| serde_json::to_value(record).unwrap())
            .collect::<Vec<_>>()
    );
    assert!(history.entries.len() > originals.len());
    let inputs = queries.inputs();
    let worker_bytes = queries.content(&inputs.worker.executable).unwrap();
    assert_eq!(worker_bytes, fs::read("/proc/self/exe").unwrap());
    let source: Value =
        serde_json::from_slice(&queries.content(&inputs.worker.source).unwrap()).unwrap();
    assert!(
        source["files"]["crates/playtime-runner/src/driver.rs"]["text"]
            .as_str()
            .unwrap()
            .contains("run_prepared")
    );
    assert!(!inputs.worker.build.rustc_version.is_empty());
    for reference in &queries.snapshot().inspection().referenced_content {
        let bytes = queries.content(reference).unwrap();
        assert_eq!(bytes.len() as u64, reference.size_bytes);
        assert_eq!(sha256(&bytes), reference.sha256);
    }
    let checkpoints = queries.checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 2);
    for (checkpoint, expected) in checkpoints.iter().zip(&expected.checkpoints) {
        let index = checkpoint.index.as_ref().unwrap();
        assert_eq!(
            queries.content(&index.outgoing_memory.content).unwrap(),
            expected.snapshot.memory.content.as_bytes()
        );
        assert_eq!(
            queries
                .content(&index.outgoing_evidence.as_ref().unwrap().text)
                .unwrap(),
            expected.evidence.as_ref().unwrap().content.as_bytes()
        );
        let persistent = index
            .files
            .iter()
            .find(|file| file.path == "persistent.txt")
            .unwrap();
        let FileContent::Retained { content } = &persistent.content else {
            panic!("missing individual file")
        };
        assert_eq!(queries.content(content).unwrap(), b"survives");
    }
    let first = checkpoints[0].index.as_ref().unwrap();
    let second = checkpoints[1].index.as_ref().unwrap();
    assert_eq!(
        queries.content(&first.outgoing_memory.content).unwrap(),
        queries.content(&second.incoming_memory.content).unwrap()
    );
    assert_eq!(
        queries
            .content(&first.outgoing_evidence.as_ref().unwrap().text)
            .unwrap(),
        queries
            .content(&second.incoming_evidence.as_ref().unwrap().text)
            .unwrap()
    );
    let source = expected.checkpoints[0]
        .evidence
        .as_ref()
        .unwrap()
        .command_sources[0]
        .dispatch;
    let ObservationResolution::Recorded { location, record } = queries
        .observation(NonZeroU64::new(source).unwrap())
        .unwrap()
    else {
        panic!("missing original evidence source")
    };
    assert_eq!(record.kind, "tool_dispatch");
    assert!(location.archive_sequence.get() > source);
    assert_eq!(
        location.record_sha256,
        history.entries[(location.archive_sequence.get() - 1) as usize].record_sha256
    );
}

fn finishing(content: String, tokens: Option<u64>) -> ModelScript {
    ModelScript {
        chunks: Vec::new(),
        completion: Some(TimedCompletion {
            after_ms: 0,
            outcome: ModelEnding::Response(playtime_engine::ModelResponse {
                done: true,
                generated_tokens: tokens,
                prompt_tokens: None,
                message: json!({"role":"assistant","content":content,"tool_calls":[{"function":{"name":"end_session","arguments":{"memento":"finished","finish_run":true}}}]}),
            }),
        }),
    }
}

#[test]
fn large_observations_externalize_exact_data_and_still_consume_log_allowance() {
    for limit in [1_048_576, 200_000] {
        let temp = Temp::new();
        let mut scenario = scenario();
        scenario.parameters.limits.log_bytes = limit.try_into().unwrap();
        scenario.parameters.memory_condition = MemoryCondition::SelfWritten;
        scenario.models = vec![finishing("🙂<&>".repeat(50_000), Some(0))];
        let report = run(&temp, &scenario);
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        let history = queries.history(0, 1000, true).unwrap();
        if limit == 1_048_576 {
            assert_eq!(
                report.result.as_ref().unwrap().reason,
                StopReason::AgentFinished
            );
            let external = history
                .entries
                .iter()
                .find_map(|entry| match &entry.fact {
                    ArchiveFact::Observation {
                        observation,
                        record: Some(record),
                    } if observation.observation_kind == "model_response" => {
                        Some((observation, record))
                    }
                    _ => None,
                })
                .unwrap();
            assert!(matches!(external.0.data, ObservationData::Artifact { .. }));
            assert_eq!(
                external.1.data["response"]["message"]["content"],
                "🙂<&>".repeat(50_000)
            );
            assert!(
                report.accounted_log_bytes > report.confirmed.frontier().committed_bytes + 300_000
            );
        } else {
            assert_eq!(report.result.as_ref().unwrap().reason, StopReason::LogLimit);
            assert!(
                history
                    .entries
                    .iter()
                    .any(|entry| entry.envelope.kind == "observation_omitted")
            );
        }
        assert_eq!(
            queries.run().unwrap().usage,
            report.result.as_ref().unwrap().usage
        );
        assert_eq!(
            report.result.as_ref().unwrap().usage.known_generated_tokens,
            0
        );
        assert_eq!(report.result.as_ref().unwrap().usage.unknown_requests, 0);
        assert!(report.accounted_log_bytes <= limit);
    }
}

#[test]
fn missing_usage_and_partial_output_remain_unknown_while_zero_is_measured() {
    for tokens in [None, Some(0)] {
        let temp = Temp::new();
        let mut scenario = scenario();
        scenario.models = vec![finishing("answer".into(), tokens)];
        scenario.models[0].chunks = vec![playtime_engine::simulation::TimedChunk {
            after_ms: 1,
            chunk: json!({"message":{"content":"partial", "thinking":"uncompleted"}}),
        }];
        let report = run(&temp, &scenario);
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        assert_eq!(
            queries.run().unwrap().usage.unknown_requests,
            u64::from(tokens.is_none())
        );
        assert_eq!(
            queries.run().unwrap().usage,
            report.result.as_ref().unwrap().usage
        );
        let history = queries.history(0, 1000, true).unwrap();
        assert!(history.entries.iter().any(|entry| matches!(&entry.fact, ArchiveFact::Observation { record:Some(record), .. } if record.kind == "model_chunk" && record.data["chunk"]["message"]["content"] == "partial")));
    }
}

#[test]
fn required_facts_that_cannot_fit_leave_incomplete_evidence_without_external_work() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.parameters.limits.log_bytes = 1.try_into().unwrap();
    let report = run(&temp, &scenario);
    assert!(!report.terminal_committed);
    assert_eq!(report.confirmed.frontier().committed_sequence, 0);
    assert_eq!(report.result.as_ref().unwrap().reason, StopReason::LogLimit);
    assert!(
        report
            .dispatches
            .iter()
            .all(|dispatch| matches!(dispatch.effect.kind, playtime_engine::EffectKind::Cleanup))
    );
    assert!(
        ArchiveQueries::offline(temp.archive(), StorageLimits::default())
            .unwrap()
            .run()
            .unwrap()
            .incomplete_evidence
    );
}

#[test]
fn failed_startup_keeps_a_partial_checkpoint_without_inventing_delivered_memory() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.lifecycle.start = vec![playtime_engine::simulation::LifecycleStep {
        failure: Some("partial startup failed".into()),
        ..Default::default()
    }];
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        StopReason::InfrastructureFailure
    );
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let checkpoints = queries.checkpoints().unwrap();
    assert_eq!(checkpoints.len(), 1);
    let index = checkpoints[0].index.as_ref().unwrap();
    assert_eq!(index.incoming_memory.status, "not_delivered");
    assert!(
        queries
            .content(&index.incoming_memory.content)
            .unwrap()
            .is_empty()
    );
    assert!(index.incoming_evidence.is_none());
    assert!(
        !queries
            .history(0, 1000, true)
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.envelope.kind == "session_started")
    );
}
