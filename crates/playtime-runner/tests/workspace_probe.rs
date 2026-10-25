#![cfg(feature = "test-harness")]
mod common;
use common::{Temp, clock};
use playtime_core::{RequestPurpose, RunId, StopReason};
use playtime_engine::{
    EffectKind, ModelResponse,
    simulation::{ExecScript, LifecycleStep, ModelEnding, ModelScript, TimedCompletion},
};
use playtime_protocol::{archive_queries::ArchiveFact, execution::Fact};
use playtime_runner::{ArchiveQueries, DriverOptions, PreparedExecution, probe};
use playtime_storage::StorageLimits;
use serde_json::json;

fn response(tokens: Option<u64>, message: serde_json::Value) -> ModelScript {
    ModelScript {
        chunks: Vec::new(),
        completion: Some(TimedCompletion {
            after_ms: 10,
            outcome: ModelEnding::Response(ModelResponse {
                done: true,
                generated_tokens: tokens,
                prompt_tokens: None,
                message,
            }),
        }),
    }
}
fn scenario() -> probe::Scenario {
    probe::Scenario {
        schema_version: 2,
        profile: probe::PROFILE.into(),
        parameters: common::scenario().parameters.common(),
        protocol: "Perform the bounded test task.".into(),
        prompt: "Create output.txt.".into(),
        context_reserve: 0,
        initial_files: [("seed.txt".into(), "seed".into())].into(),
        models: vec![
            response(
                Some(8),
                json!({"role":"assistant","content":"Working", "tool_calls":[{"function":{"name":"exec","arguments":{"command":"write output"}}}]}),
            ),
            response(Some(5), json!({"role":"assistant","content":"Summary"})),
        ],
        execs: vec![ExecScript {
            command: "write output".into(),
            delay_ms: 5,
            never: false,
            result: json!({"exit_code":0,"stdout":"done","stderr":"","timed_out":false}),
            workspace_changes: [("output.txt".into(), Some("output".into()))].into(),
            tmp_changes: Default::default(),
            processes: Vec::new(),
        }],
        cancel_at_ms: None,
        fail_record_sequence: None,
        start: LifecycleStep::default(),
        capture: LifecycleStep::default(),
        publish: LifecycleStep::default(),
        cleanup: LifecycleStep::default(),
    }
}
fn run(temp: &Temp, scenario: &probe::Scenario) -> playtime_runner::RunReport {
    let prepared = probe::prepare(&temp.archive(), RunId::new("probe").unwrap(), scenario).unwrap();
    assert!(
        PreparedExecution::acquire(temp.archive(), &prepared).is_err(),
        "production worker must reject test contract before dispatch"
    );
    PreparedExecution::acquire_probe(temp.archive(), &prepared)
        .unwrap()
        .execute(&mut clock(), DriverOptions::default())
        .unwrap()
}
#[test]
fn workspace_capture_has_no_session_or_memory_and_preserves_purpose_accounting() {
    let temp = Temp::new();
    let report = run(&temp, &scenario());
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let result = report.result.as_ref().unwrap();
    assert_eq!(result.reason, StopReason::AgentFinished, "{result:?}");
    assert_eq!(result.model_calls, 2);
    assert!(result.last_checkpoint.is_none());
    assert!(result.last_capture.is_some());
    let requests: Vec<_> = report
        .dispatches
        .iter()
        .filter_map(|dispatch| match &dispatch.effect.kind {
            EffectKind::Model(request) => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(
        requests.iter().map(|r| r.purpose).collect::<Vec<_>>(),
        [RequestPurpose::Work, RequestPurpose::Compaction]
    );
    assert_eq!(requests[1].sampling_seed, requests[0].sampling_seed + 1);
    assert_eq!(requests[1].call, 2);
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[0].tools.len(), 1);
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert!(queries.checkpoints().unwrap().is_empty());
    let captures = queries.captures().unwrap();
    assert_eq!(captures.len(), 1);
    let expected_index: playtime_protocol::execution::CaptureIndex =
        serde_json::from_str(include_str!("fixtures/workspace-capture-v2.json")).unwrap();
    assert_eq!(captures[0].index, expected_index);
    assert_eq!(captures[0].index.files.len(), 2);
    let accounting = queries.request_accounting().unwrap();
    assert_eq!(accounting.model_calls, Some(2));
    assert_eq!(
        accounting.by_purpose[&RequestPurpose::Work]
            .usage
            .known_generated_tokens,
        8
    );
    assert_eq!(
        accounting.by_purpose[&RequestPurpose::Compaction]
            .usage
            .known_generated_tokens,
        5
    );
    assert_eq!(accounting.unclassified.settled_calls, 0);
    let run = queries.run().unwrap();
    assert_eq!(run.outcome.unwrap().reason, StopReason::AgentFinished);
    assert!(run.execution.unwrap().unsettled_model_intents.is_empty());
    let history = queries.history(0, 1000, true).unwrap();
    let contract: Vec<_> = history
        .entries
        .iter()
        .filter_map(|entry| match &entry.fact {
            ArchiveFact::Execution { event, .. } => Some(event.clone()),
            _ => None,
        })
        .collect();
    let expected: Vec<playtime_protocol::execution::Record> =
        serde_json::from_str(include_str!("fixtures/workspace-execution-v2.json")).unwrap();
    assert_eq!(
        contract, expected,
        "review the v2 contract; never rewrite a legacy golden"
    );

    for entry in &history.entries {
        assert!(!entry.jsonl.contains("MEMENTO.md"));
        assert!(!entry.jsonl.contains("end_session"));
        if let ArchiveFact::Execution { event, .. } = &entry.fact
            && let Fact::EffectIntent(intent) = &event.fact
            && let Some(request) = &intent.request
        {
            assert!(matches!(request.scope, playtime_core::RequestScope::Run));
        }
    }
}

#[test]
fn maintenance_unknown_usage_retains_the_same_remaining_allowance() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.parameters.limits.generated_tokens = 12.try_into().unwrap();
    scenario.models[1] = response(
        None,
        json!({"role":"assistant", "content":"partial summary"}),
    );
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let result = report.result.unwrap();
    assert_eq!(result.reason, StopReason::MissingTokenUsage);
    assert_eq!(result.usage.known_generated_tokens, 8);
    assert_eq!(result.usage.unknown_reserved_tokens, 4);
    assert_eq!(result.usage.budget_charged_tokens, 12);
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let accounting = queries.request_accounting().unwrap();
    assert_eq!(
        accounting.by_purpose[&RequestPurpose::Compaction]
            .usage
            .unknown_reserved_tokens,
        4
    );
    assert_eq!(
        accounting.by_purpose[&RequestPurpose::Compaction].admitted_calls,
        1
    );
    assert!(
        queries.captures().unwrap()[0]
            .index
            .files
            .iter()
            .any(|f| f.path == "output.txt")
    );
}

#[test]
fn maintenance_uses_shared_timeout_wall_deadline_and_cancellation() {
    for (cancel, wall, expected) in [
        (None, false, StopReason::ModelTimeout),
        (None, true, StopReason::WallLimit),
        (Some(30), false, StopReason::Interrupted),
    ] {
        let temp = Temp::new();
        let mut scenario = scenario();
        scenario.models[1] = ModelScript::default();
        scenario.cancel_at_ms = cancel;
        scenario.parameters.limits.model_seconds = 1.try_into().unwrap();
        if wall {
            scenario.parameters.limits.wall_seconds = 1.try_into().unwrap();
        }
        let report = run(&temp, &scenario);
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let result = report.result.unwrap();
        assert_eq!(result.reason, expected);
        assert_eq!(result.model_calls, 2);
        assert_eq!(result.usage.unknown_reserved_tokens, 100);
        assert!(result.last_capture.is_some());
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        assert_eq!(
            queries.request_accounting().unwrap().by_purpose[&RequestPurpose::Compaction]
                .usage
                .unknown_requests,
            1
        );
    }
}

struct CancelAtIntent {
    id: u64,
    cancel: playtime_runner::Cancellation,
}
impl playtime_runner::WriterHooks for CancelAtIntent {
    fn after(
        &mut self,
        context: &playtime_runner::CommitContext,
        _: &playtime_storage::CommittedFrontier,
    ) -> playtime_runner::ReceiptDelivery {
        if context.effect_ids.contains(&self.id) {
            self.cancel.cancel();
        }
        playtime_runner::ReceiptDelivery::Deliver
    }
}
#[test]
fn preflight_cancellation_counts_the_attempt_but_releases_its_reservation() {
    for (id, purpose, expected_calls) in [
        (2, RequestPurpose::Work, 1),
        (4, RequestPurpose::Compaction, 2),
    ] {
        let temp = Temp::new();
        let scenario = scenario();
        let prepared =
            probe::prepare(&temp.archive(), RunId::new("probe").unwrap(), &scenario).unwrap();
        let cancellation = playtime_runner::Cancellation::default();
        let report = PreparedExecution::acquire_probe(temp.archive(), &prepared)
            .unwrap()
            .execute(
                &mut clock(),
                DriverOptions {
                    cancellation: cancellation.clone(),
                    writer_hooks: Box::new(CancelAtIntent {
                        id,
                        cancel: cancellation,
                    }),
                    ..DriverOptions::default()
                },
            )
            .unwrap();
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let result = report.result.unwrap();
        assert_eq!(result.model_calls, expected_calls);
        assert_eq!(result.reason, StopReason::Interrupted);
        assert_eq!(result.usage.unknown_requests, 0);
        assert!(!report.dispatches.iter().any(|d| d.effect.id == id));
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        let accounting = queries.request_accounting().unwrap();
        assert_eq!(accounting.model_calls, Some(expected_calls));
        assert_eq!(accounting.by_purpose[&purpose].admitted_calls, 1);
        assert_eq!(accounting.by_purpose[&purpose].not_dispatched_calls, 1);
        assert_eq!(accounting.by_purpose[&purpose].settled_calls, 0);
    }
}

#[test]
fn maintenance_cannot_dispatch_tools_or_claim_a_separate_budget() {
    for tool_violation in [false, true] {
        let temp = Temp::new();
        let mut scenario = scenario();
        if tool_violation {
            scenario.models[1] = response(
                Some(5),
                json!({"role":"assistant", "tool_calls":[{"function":{"name":"exec","arguments":{"command":"escape"}}}]}),
            );
        } else {
            scenario.parameters.limits.generated_tokens = 8.try_into().unwrap();
        }
        let report = run(&temp, &scenario);
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let result = report.result.unwrap();
        assert_eq!(
            result.reason,
            if tool_violation {
                StopReason::InfrastructureFailure
            } else {
                StopReason::TokenLimit
            }
        );
        assert_eq!(result.model_calls, if tool_violation { 2 } else { 1 });
        assert_eq!(
            report
                .dispatches
                .iter()
                .filter(|d| matches!(d.effect.kind, EffectKind::Tool(_)))
                .count(),
            1
        );
    }
}

struct LoseCapture;
impl playtime_runner::WriterHooks for LoseCapture {
    fn after(
        &mut self,
        context: &playtime_runner::CommitContext,
        _: &playtime_storage::CommittedFrontier,
    ) -> playtime_runner::ReceiptDelivery {
        if context.capture_effect.is_some() {
            playtime_runner::ReceiptDelivery::Lose
        } else {
            playtime_runner::ReceiptDelivery::Deliver
        }
    }
}
#[test]
fn lost_capture_receipt_never_promotes_a_candidate_or_claims_success() {
    let temp = Temp::new();
    let prepared =
        probe::prepare(&temp.archive(), RunId::new("probe").unwrap(), &scenario()).unwrap();
    let report = PreparedExecution::acquire_probe(temp.archive(), &prepared)
        .unwrap()
        .execute(
            &mut clock(),
            DriverOptions {
                writer_hooks: Box::new(LoseCapture),
                ..DriverOptions::default()
            },
        )
        .unwrap();
    assert!(!report.terminal_committed);
    let result = report.result.unwrap();
    assert_eq!(result.reason, StopReason::InfrastructureFailure);
    assert!(result.last_capture.is_none());
    assert!(matches!(
        result.cleanup,
        playtime_protocol::events::CleanupOutcome::Complete
    ));
    let live =
        ArchiveQueries::live(temp.archive(), StorageLimits::default(), &report.confirmed).unwrap();
    assert!(live.captures().unwrap().is_empty());
    let offline = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    assert!(offline.run().unwrap().incomplete_evidence);
    assert_eq!(
        offline.captures().unwrap().len(),
        1,
        "offline discovery does not grant a writer receipt"
    );
}

#[test]
fn failed_capture_and_failed_publication_still_cleanup_without_success() {
    for capture_failure in [true, false] {
        let temp = Temp::new();
        let mut scenario = scenario();
        let step = if capture_failure {
            &mut scenario.capture
        } else {
            &mut scenario.publish
        };
        step.failure = Some("injected stopped-workspace failure".into());
        let report = run(&temp, &scenario);
        assert!(report.terminal_committed, "{:?}", report.diagnostics);
        let result = report.result.unwrap();
        assert_eq!(result.reason, StopReason::InfrastructureFailure);
        assert!(result.last_capture.is_none());
        assert!(matches!(
            result.cleanup,
            playtime_protocol::events::CleanupOutcome::Complete
        ));
    }
}

#[test]
fn log_omissions_keep_the_maintenance_purpose_and_unknown_charge() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.parameters.limits.log_bytes = 85_000.try_into().unwrap();
    scenario.models[1].chunks = vec![playtime_engine::simulation::TimedChunk {
        after_ms: 1,
        chunk: json!({"message":{"thinking":"x".repeat(30_000)}}),
    }];
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(report.result.as_ref().unwrap().reason, StopReason::LogLimit);
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let accounting = queries.request_accounting().unwrap();
    assert_eq!(accounting.model_calls, Some(2));
    assert_eq!(
        accounting.by_purpose[&RequestPurpose::Compaction]
            .usage
            .unknown_reserved_tokens,
        100
    );
    let history = queries.history(0, 1000, true).unwrap();
    assert!(history.entries.iter().any(|entry| matches!(&entry.fact, ArchiveFact::Execution { event, .. } if matches!(&event.fact, Fact::ObservationOmitted { request:Some(request), .. } if request.purpose == RequestPurpose::Compaction))));
}

#[test]
fn rejected_adapter_preflight_is_an_admitted_attempt_with_uncertain_usage() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.models[0].completion.as_mut().unwrap().outcome =
        ModelEnding::Error(playtime_engine::ModelFailure::Infrastructure(
            "metadata preflight failed before inference".into(),
        ));
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    assert_eq!(report.result.as_ref().unwrap().model_calls, 1);
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
    assert_eq!(
        queries.request_accounting().unwrap().by_purpose[&RequestPurpose::Work].admitted_calls,
        1
    );
}

#[test]
fn storage_rejects_changed_purpose_ordinal_allowance_and_legacy_terminal() {
    use playtime_storage::{ArchiveWriter, BatchClass};
    let records: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/workspace-execution-v2.json")).unwrap();
    let mut intent = records
        .iter()
        .find(|r| {
            r["fact"]["kind"] == "effect_intent" && r["fact"]["data"]["effect_kind"] == "model"
        })
        .unwrap()
        .clone();
    intent["fact"]["data"]["sources"] = json!([]);
    let settled = records
        .iter()
        .find(|r| r["fact"]["kind"] == "request_settled")
        .unwrap()
        .clone();
    for case in 0..5 {
        let temp = Temp::new();
        let prepared =
            probe::prepare(&temp.archive(), RunId::new("probe").unwrap(), &scenario()).unwrap();
        let mut writer = ArchiveWriter::open_prepared(
            temp.archive(),
            &prepared.identity,
            prepared.limits.clone(),
        )
        .unwrap();
        let wire = |sequence, kind, data| {
            format!("{}\n", json!({"schema_version":1, "run_id":"probe", "sequence":sequence, "time":"2026-10-17T00:00:00Z", "kind":kind, "data":data})).into_bytes()
        };
        let start = wire(
            1,
            "run_started",
            json!({"mode":"simulation","input_manifest":prepared.identity.input_manifest}),
        );
        writer.append(&[&start], BatchClass::Ordinary).unwrap();
        let (sequence, kind, invalid) = if case == 3 {
            let mut invalid = intent.clone();
            invalid["fact"]["data"]["request"]["call"] = json!(2);
            (2, "execution_v2", invalid)
        } else if case == 4 {
            (
                2,
                "run_finished",
                json!({"reason":"agent_finished", "usage":{"known_generated_tokens":0,"unknown_reserved_tokens":0,"unknown_requests":0,"budget_charged_tokens":0,"usage_complete":true},"cleanup":{"status":"complete"},"last_checkpoint":null}),
            )
        } else {
            writer
                .append(
                    &[&wire(2, "execution_v2", intent.clone())],
                    BatchClass::Ordinary,
                )
                .unwrap();
            let mut invalid = settled.clone();
            match case {
                0 => invalid["fact"]["data"]["request"]["purpose"] = json!("compaction"),
                1 => {
                    invalid["fact"]["data"]["request"]["scope"] =
                        json!({"kind":"session","session_id":"s1"})
                }
                _ => {
                    invalid["fact"]["data"]["usage"] =
                        json!({"status":"unknown","reserved_tokens":99})
                }
            }
            (3, "execution_v2", invalid)
        };
        let before = writer.confirmed_frontier().clone();
        assert!(
            writer
                .append(&[&wire(sequence, kind, invalid)], BatchClass::Finalization)
                .is_err(),
            "case {case}"
        );
        assert_eq!(writer.confirmed_frontier().frontier(), before.frontier());
    }
}

struct CaptureRace {
    clock: playtime_runner::VirtualClock,
    at_ms: u64,
    cancel: Option<playtime_runner::Cancellation>,
}
impl playtime_runner::WriterHooks for CaptureRace {
    fn after(
        &mut self,
        context: &playtime_runner::CommitContext,
        _: &playtime_storage::CommittedFrontier,
    ) -> playtime_runner::ReceiptDelivery {
        if context.capture_effect.is_some() {
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            self.clock.advance_to(self.at_ms);
        }
        playtime_runner::ReceiptDelivery::Deliver
    }
}
#[test]
fn late_capture_receipt_updates_evidence_without_rewriting_the_first_stop() {
    for (at_ms, cancel, expected) in [
        (20_024, false, StopReason::AgentFinished),
        (20_025, false, StopReason::InfrastructureFailure),
        (300_000, false, StopReason::WallLimit),
        (20_025, true, StopReason::Interrupted),
    ] {
        let temp = Temp::new();
        let prepared =
            probe::prepare(&temp.archive(), RunId::new("probe").unwrap(), &scenario()).unwrap();
        let mut clock = clock();
        let cancellation = playtime_runner::Cancellation::default();
        let hook = CaptureRace {
            clock: clock.clone(),
            at_ms,
            cancel: cancel.then_some(cancellation.clone()),
        };
        let report = PreparedExecution::acquire_probe(temp.archive(), &prepared)
            .unwrap()
            .execute(
                &mut clock,
                DriverOptions {
                    cancellation,
                    writer_hooks: Box::new(hook),
                    ..DriverOptions::default()
                },
            )
            .unwrap();
        assert!(
            report.terminal_committed,
            "at {at_ms}: {:?}",
            report.diagnostics
        );
        let result = report.result.unwrap();
        assert_eq!(result.reason, expected);
        assert!(result.last_capture.is_some());
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        assert_eq!(queries.run().unwrap().outcome.unwrap().reason, expected);
        assert_eq!(queries.captures().unwrap().len(), 1);
    }
}

#[test]
fn externalized_observations_resolve_exact_data_and_keep_request_attribution() {
    let temp = Temp::new();
    let mut scenario = scenario();
    let chunk = json!({"message":{"thinking":"retained".repeat(40_000)}});
    scenario.models[1].chunks = vec![playtime_engine::simulation::TimedChunk {
        after_ms: 1,
        chunk: chunk.clone(),
    }];
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let page = queries.history(0, 1000, true).unwrap();
    let (observation, data) = page
        .entries
        .iter()
        .find_map(|entry| match &entry.fact {
            ArchiveFact::Execution {
                event,
                resolved: Some(data),
            } => match &event.fact {
                Fact::Observation(observation) if observation.observation_kind == "model_chunk" => {
                    Some((observation, data))
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        observation.data,
        playtime_protocol::observations::ObservationData::Artifact { .. }
    ));
    assert_eq!(
        observation.request.as_ref().unwrap().purpose,
        RequestPurpose::Compaction
    );
    assert_eq!(data["chunk"], chunk);
    assert!(report.accounted_log_bytes > report.confirmed.frontier().committed_bytes + 300_000);
    assert_eq!(queries.request_accounting().unwrap().model_calls, Some(2));
}
