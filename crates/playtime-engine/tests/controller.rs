use std::{collections::BTreeMap, num::NonZeroU64};

use playtime_core::{MemoryCondition, RunId, RunMode, StopReason};
use playtime_engine::{simulation::*, *};
use playtime_protocol::events::CleanupOutcome;
use proptest::{prelude::*, test_runner::RngSeed};
use serde_json::{Value, json};

fn fixture() -> Scenario {
    serde_json::from_str(include_str!("fixtures/two-session.json")).unwrap()
}

fn base() -> Scenario {
    let mut scenario = fixture();
    scenario.parameters.memory_condition = MemoryCondition::SelfWritten;
    scenario.parameters.limits.work_turns_per_session = 1.try_into().unwrap();
    scenario.parameters.limits.tool_parse_error_limit = None;
    scenario.initial_files = BTreeMap::from([("MEMENTO.md".into(), String::new())]);
    scenario.models.clear();
    scenario.execs.clear();
    scenario
}

fn tool(name: &str, arguments: Value) -> Value {
    json!({"function": {"name": name, "arguments": arguments}})
}

fn response(tokens: u64, tools: Vec<Value>) -> ModelScript {
    ModelScript {
        chunks: Vec::new(),
        completion: Some(TimedCompletion {
            after_ms: 0,
            outcome: ModelEnding::Response(ModelResponse {
                message: json!({"role": "assistant", "content": "continuing", "tool_calls": tools}),
                done: true,
                generated_tokens: Some(tokens),
                prompt_tokens: None,
            }),
        }),
    }
}

fn ending(finish_run: bool) -> ModelScript {
    response(
        5,
        vec![tool(
            "end_session",
            json!({"memento": "The project is in progress.", "finish_run": finish_run}),
        )],
    )
}

fn error(error: ModelFailure) -> ModelScript {
    ModelScript {
        chunks: vec![TimedChunk {
            after_ms: 0,
            chunk: json!({"message": {"thinking": "Before the failure", "tool_calls": [tool("exec", json!({"command": "must-not-execute"}))]}}),
        }],
        completion: Some(TimedCompletion {
            after_ms: 0,
            outcome: ModelEnding::Error(error),
        }),
    }
}

fn requests(simulation: &Simulation) -> Vec<&ModelRequest> {
    simulation
        .dispatches
        .iter()
        .filter_map(|dispatch| {
            if let EffectKind::Model(request) = &dispatch.effect.kind {
                Some(request)
            } else {
                None
            }
        })
        .collect()
}

fn tools(simulation: &Simulation) -> Vec<&ToolRequest> {
    simulation
        .dispatches
        .iter()
        .filter_map(|dispatch| {
            if let EffectKind::Tool(request) = &dispatch.effect.kind {
                Some(request)
            } else {
                None
            }
        })
        .collect()
}

fn records<'a>(simulation: &'a Simulation, kind: &'a str) -> impl Iterator<Item = &'a Record> {
    simulation
        .records
        .iter()
        .filter(move |record| record.kind == kind)
}

#[test]
fn two_sessions_persist_files_and_exact_memory_but_reset_conversation_and_transients() {
    let simulation = run(fixture()).unwrap();
    assert_eq!(simulation, run(fixture()).unwrap());
    assert_eq!(simulation.mode, RunMode::Simulation);
    assert_eq!(simulation.result.mode, RunMode::Simulation);
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert_eq!(simulation.result.usage.known_generated_tokens, 26);
    assert_eq!(simulation.result.elapsed_ms, 400);
    assert_eq!(simulation.session_states.len(), 2);
    let next = &simulation.session_states[1];
    assert_eq!(next.workspace["persistent.txt"], "survives");
    assert_eq!(next.workspace["partial.txt"], "kept");
    assert!(next.temporary_files.is_empty());
    assert!(next.processes.is_empty());
    let requests = requests(&simulation);
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[2].messages.len(), 3);
    assert_eq!(requests[2].context_basis, "input_bytes");
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains("First-session private thinking")
    );
    assert!(
        !serde_json::to_string(&requests[2].messages)
            .unwrap()
            .contains("First-session private thinking")
    );
    let received = records(&simulation, "memory_received").nth(1).unwrap();
    assert_eq!(
        received.data["memory"]["content"],
        "Read persistent.txt in the next session."
    );
    let evidence = simulation.checkpoints[0].evidence.as_ref().unwrap();
    assert_eq!(received.data["evidence"]["content"], evidence.content);
    assert_eq!(
        requests[2].messages[2],
        json!({"role": "user", "content": evidence.content})
    );
    assert!(evidence.content.len() <= 4096);
    assert!(
        evidence
            .content
            .contains("process completion, not artifact or test correctness")
    );
    assert_eq!(
        simulation.workspace["MEMENTO.md"],
        "The two-session fixture is complete."
    );
    let normalized: Vec<_> = simulation.records.iter().map(|record| json!({
        "sequence": record.sequence, "elapsed_ms": record.elapsed_ms, "session_id": record.session_id,
        "kind": record.kind, "phase": record.data.get("phase"), "reason": record.data.get("reason")
    })).collect();
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/two-session.trace.json")).unwrap();
    assert_eq!(json!(normalized), golden);
}

#[test]
fn recovery_matches_the_committed_python_baseline() {
    let mut scenario = base();
    scenario.models = vec![
        error(ModelFailure::Parse("invalid tool JSON".into())),
        ending(false),
        ending(true),
    ];
    let simulation = run(scenario).unwrap();
    let usage = &simulation.result.usage;
    let normalized = json!({
        "result": {"stop_reason": simulation.result.reason, "generated_tokens": usage.known_generated_tokens,
            "token_budget_charged": usage.budget_charged_tokens, "unaccounted_token_reserve": usage.unknown_reserved_tokens,
            "unaccounted_model_calls": usage.unknown_requests, "token_usage_complete": usage.usage_complete,
            "model_calls": simulation.result.model_calls, "tool_parse_errors": simulation.result.tool_parse_errors},
        "requests": requests(&simulation).into_iter().map(|request| json!({"call": request.call, "handoff": request.handoff,
            "seed": request.sampling_seed, "num_predict": request.maximum_tokens, "message_count": request.messages.len(),
            "tools": request.tools.iter().map(|tool| &tool["function"]["name"]).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "session_starts": records(&simulation, "memory_received").map(|record| json!({"kind": "session_start",
            "session": record.data["ordinal"], "memory": record.data["memory"]})).collect::<Vec<_>>(),
        "handoffs": records(&simulation, "tool_request").filter(|record| record.data["name"] == "end_session").enumerate()
            .map(|(i, record)| json!({"kind": "tool_request", "session": i + 1, "arguments": record.data["arguments"]})).collect::<Vec<_>>(),
        "model_errors": records(&simulation, "model_error").map(|record| json!({"kind": "model_error", "reason": "tool_call_parse_error",
            "recovery_allowed": record.data["recovery_allowed"], "token_budget_charge": record.data["token_budget_charge"],
            "unaccounted_model_calls": record.data["usage"]["unknown_requests"],
            "unaccounted_token_reserve": record.data["usage"]["unknown_reserved_tokens"]})).collect::<Vec<_>>()
    });
    let expected: Value = serde_json::from_str(include_str!(
        "../../playtime-protocol/tests/fixtures/legacy/recovery.json"
    ))
    .unwrap();
    assert_eq!(normalized, expected);
    assert!(
        tools(&simulation)
            .iter()
            .all(|tool| tool.name == "write_file")
    );
    assert!(
        records(&simulation, "model_chunk")
            .any(|record| record.data["chunk"]["message"]["thinking"] == "Before the failure")
    );
}

#[test]
fn empty_evidence_matches_python_and_self_memory_stays_separate() {
    let simulation = run({
        let mut scenario = base();
        scenario.models = vec![ending(false), ending(true)];
        scenario
    })
    .unwrap();
    assert_eq!(
        requests(&simulation)
            .iter()
            .map(|request| request.messages.len())
            .collect::<Vec<_>>(),
        [2, 2]
    );
    assert!(
        simulation
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.evidence.is_none())
    );
    let reference = simulation.checkpoints[0].snapshot.workspace.clone();
    let evidence = evidence::build(
        1,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &[],
        reference.clone(),
        reference,
    );
    let expected: Value = serde_json::from_str(include_str!(
        "../../playtime-protocol/tests/fixtures/legacy/evidence.json"
    ))
    .unwrap();
    assert_eq!(evidence.content, expected["received_evidence"]);
    assert_eq!(
        evidence.content.len(),
        expected["evidence_bytes"].as_u64().unwrap() as usize
    );
}

#[test]
fn bounded_unicode_evidence_matches_the_independent_python_oracle() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/evidence.json")).unwrap();
    let before: FileIndex = serde_json::from_value(fixture["before"].clone()).unwrap();
    let after: FileIndex = serde_json::from_value(fixture["after"].clone()).unwrap();
    assert_eq!(before["changed.txt"].sha256, after["changed.txt"].sha256);
    assert_ne!(before["changed.txt"].mode, after["changed.txt"].mode);
    let commands: Vec<evidence::CommandObservation> =
        serde_json::from_value(fixture["commands"].clone()).unwrap();
    let simulation = run({
        let mut scenario = base();
        scenario.models = vec![ending(true)];
        scenario
    })
    .unwrap();
    let reference = simulation.checkpoints[0].snapshot.workspace.clone();
    let evidence = evidence::build(2, &before, &after, &commands, reference.clone(), reference);
    assert_eq!(evidence.content, fixture["expected_content"]);
    assert!(evidence.content.len() <= 4096);
    assert_eq!(
        evidence
            .command_sources
            .iter()
            .map(|source| source.dispatch)
            .collect::<Vec<_>>(),
        [35, 32, 2]
    );
    assert_eq!(evidence.command_sources[0].result, None);
}

#[test]
fn token_handoff_and_session_limits_do_not_create_new_allocations() {
    let mut scenario = base();
    scenario.parameters.limits.generated_tokens = 101.try_into().unwrap();
    scenario.models = vec![response(51, vec![]), response(50, vec![])];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::TokenLimit);
    assert_eq!(
        requests(&simulation)
            .iter()
            .map(|request| request.maximum_tokens)
            .collect::<Vec<_>>(),
        [51, 50]
    );
    assert_eq!(simulation.result.usage.budget_charged_tokens, 101);

    let mut scenario = base();
    scenario.parameters.limits.sessions = 1.try_into().unwrap();
    scenario.initial_files.clear();
    scenario.models = vec![response(0, vec![]); 3];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::SessionLimit);
    assert_eq!(simulation.result.model_calls, 3);
    assert_eq!(
        requests(&simulation)
            .iter()
            .map(|request| request.handoff)
            .collect::<Vec<_>>(),
        [false, true, true]
    );
    assert_eq!(records(&simulation, "handoff_feedback").count(), 1);
    assert_eq!(records(&simulation, "handoff_missing").count(), 1);
    assert_eq!(simulation.checkpoints[0].snapshot.memory.status, "missing");
    assert!(!simulation.checkpoints[0].handoff_saved);
    assert!(simulation.result.usage.usage_complete);
}

#[test]
fn a_model_that_never_yields_is_bounded_by_virtual_time() {
    let mut scenario = base();
    scenario.models = vec![ModelScript::default()];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::ModelTimeout);
    assert_eq!(simulation.result.elapsed_ms, 30_000);
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
    assert_eq!(simulation.result.usage.unknown_requests, 1);
    assert!(tools(&simulation).is_empty());
}

#[test]
fn cancellation_then_wall_then_operation_deadline_win_ties_with_completion() {
    for (cancel, wall, expected) in [
        (true, true, StopReason::Interrupted),
        (false, true, StopReason::WallLimit),
        (false, false, StopReason::ModelTimeout),
    ] {
        let mut scenario = base();
        scenario.parameters.limits.model_seconds = 1.try_into().unwrap();
        if wall {
            scenario.parameters.limits.wall_seconds = 1.try_into().unwrap();
        }
        scenario.cancel_at_ms = cancel.then_some(1000);
        let mut completed = ending(true);
        completed.completion.as_mut().unwrap().after_ms = 1000;
        scenario.models = vec![completed];
        let simulation = run(scenario).unwrap();
        assert_eq!(simulation.result.reason, expected);
        assert_eq!(simulation.result.elapsed_ms, 1000);
        assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
        assert_eq!(simulation.result.usage.known_generated_tokens, 0);
        assert!(tools(&simulation).is_empty());
    }
}

#[test]
fn cancellation_during_an_exec_preserves_partial_workspace_and_unknown_result() {
    let mut scenario = base();
    scenario.parameters.memory_condition = MemoryCondition::SelfPlusEvidence;
    scenario.cancel_at_ms = Some(500);
    scenario.models = vec![response(
        5,
        vec![tool("exec", json!({"command": "partial operation"}))],
    )];
    scenario.execs = vec![ExecScript {
        command: "partial operation".into(),
        delay_ms: 0,
        never: true,
        result: json!({"exit_code": 0}),
        workspace_changes: BTreeMap::from([("partial.txt".into(), Some("unfinished".into()))]),
        tmp_changes: BTreeMap::new(),
        processes: vec!["still-running".into()],
    }];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::Interrupted);
    assert_eq!(simulation.workspace["partial.txt"], "unfinished");
    assert_eq!(records(&simulation, "tool_result").count(), 0);
    let evidence = &simulation.checkpoints[0].evidence.as_ref().unwrap().content;
    assert!(evidence.contains("\"result\":\"unknown\""));
    assert!(evidence.contains("\"exit_code\":null"));
    assert_eq!(simulation.result.usage.known_generated_tokens, 5);
}

#[test]
fn checkpoint_cancellation_preserves_one_attempt_and_wall_timer_still_fires() {
    for cancel in [true, false] {
        let mut scenario = base();
        scenario.models = vec![ending(true)];
        scenario.parameters.limits.wall_seconds = 1.try_into().unwrap();
        scenario.cancel_at_ms = cancel.then_some(500);
        scenario.lifecycle.snapshot = vec![LifecycleStep {
            delay_ms: 1500,
            ..Default::default()
        }];
        let simulation = run(scenario).unwrap();
        assert_eq!(
            simulation.result.reason,
            if cancel {
                StopReason::Interrupted
            } else {
                StopReason::WallLimit
            }
        );
        assert_eq!(simulation.result.elapsed_ms, 1500);
        assert_eq!(
            simulation
                .dispatches
                .iter()
                .filter(|dispatch| matches!(dispatch.effect.kind, EffectKind::Snapshot { .. }))
                .count(),
            1
        );
        assert!(simulation.checkpoints[0].interrupted);
        assert_eq!(
            records(&simulation, "controller_stop")
                .next()
                .unwrap()
                .elapsed_ms,
            if cancel { 500 } else { 1000 }
        );
    }
}

#[test]
fn failed_snapshot_or_publication_keeps_the_last_published_checkpoint() {
    for publish in [false, true] {
        let mut scenario = base();
        scenario.models = vec![ending(false), ending(true)];
        let steps = vec![
            LifecycleStep::default(),
            LifecycleStep {
                failure: Some("disk unavailable".into()),
                ..Default::default()
            },
        ];
        if publish {
            scenario.lifecycle.publish = steps;
        } else {
            scenario.lifecycle.snapshot = steps;
        }
        let simulation = run(scenario).unwrap();
        assert_eq!(simulation.result.reason, StopReason::InfrastructureFailure);
        assert_eq!(
            simulation.result.last_checkpoint.as_ref().unwrap().ordinal,
            1
        );
        assert_eq!(simulation.checkpoints.len(), 1);
        assert_eq!(simulation.result.cleanup, CleanupOutcome::Complete);
        assert!(
            simulation
                .result
                .diagnostics
                .iter()
                .any(|text| text.contains("disk unavailable"))
        );
    }
}

#[test]
fn cleanup_failure_and_stall_do_not_erase_the_primary_reason() {
    for never in [false, true] {
        let mut scenario = base();
        scenario.models = vec![ending(true)];
        scenario.lifecycle.cleanup = LifecycleStep {
            never,
            failure: Some("cannot remove workshop".into()),
            ..Default::default()
        };
        let simulation = run(scenario).unwrap();
        assert_eq!(simulation.result.reason, StopReason::AgentFinished);
        assert!(matches!(
            simulation.result.cleanup,
            CleanupOutcome::Failed { .. }
        ));
        assert_eq!(simulation.result.elapsed_ms, if never { 20_000 } else { 0 });
    }
}

#[test]
fn too_many_tools_or_server_budget_overrun_stop_before_dispatch() {
    for (tokens, calls, reason) in [
        (
            5,
            vec![tool("exec", json!({"command": "no"})); 9],
            StopReason::TooManyToolsInOneResponse,
        ),
        (
            1001,
            vec![tool("exec", json!({"command": "no"}))],
            StopReason::TokenLimitExceededByServer,
        ),
    ] {
        let mut scenario = base();
        scenario.models = vec![response(tokens, calls)];
        let simulation = run(scenario).unwrap();
        assert_eq!(simulation.result.reason, reason);
        assert!(tools(&simulation).is_empty());
        assert_eq!(simulation.result.usage.known_generated_tokens, tokens);
    }
}

#[test]
fn malformed_tool_arguments_and_tools_after_end_session_are_rejected() {
    let mut scenario = base();
    scenario.models = vec![response(
        5,
        vec![
            tool("exec", json!({"command": 7})),
            tool("exec", json!({"command": "bad", "unexpected": true})),
            tool(
                "end_session",
                json!({"memento": "done", "finish_run": true}),
            ),
            tool("exec", json!({"command": "too late"})),
        ],
    )];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert_eq!(tools(&simulation).len(), 1);
    assert_eq!(tools(&simulation)[0].name, "write_file");
    assert_eq!(
        records(&simulation, "tool_result")
            .filter(|record| record.data["result"].get("error").is_some())
            .count(),
        3
    );
}

#[test]
fn oversized_utf8_memento_cannot_finish_and_does_not_get_written() {
    let mut scenario = base();
    scenario.parameters.limits.work_turns_per_session = 2.try_into().unwrap();
    scenario.models = vec![
        response(
            5,
            vec![tool(
                "end_session",
                json!({"memento": "😀".repeat(4096), "finish_run": true}),
            )],
        ),
        ending(true),
    ];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert_eq!(tools(&simulation).len(), 1);
    assert_eq!(simulation.result.model_calls, 2);
    assert_eq!(
        simulation.workspace["MEMENTO.md"],
        "The project is in progress."
    );
    assert!(
        records(&simulation, "tool_result").next().unwrap().data["result"]["error"]
            .as_str()
            .unwrap()
            .contains("Memento exceeds")
    );
}

#[test]
fn parse_error_ceiling_ends_only_the_session_and_keeps_its_charges() {
    let mut scenario = base();
    scenario.parameters.limits.work_turns_per_session = 5.try_into().unwrap();
    scenario.parameters.limits.tool_parse_retries = 2;
    scenario.parameters.limits.tool_parse_error_limit = Some(2.try_into().unwrap());
    scenario.models = vec![
        error(ModelFailure::Parse("first".into())),
        response(5, vec![]),
        // The streak reset, but this session's second error hands off.
        error(ModelFailure::Parse("second".into())),
        ending(false),
        // The next session starts with a fresh ceiling.
        error(ModelFailure::Parse("third".into())),
        ending(true),
    ];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert_eq!(simulation.result.tool_parse_errors, 3);
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 300);
    assert_eq!(simulation.result.usage.known_generated_tokens, 15);
    assert_eq!(simulation.checkpoints.len(), 2);
    let handoffs: Vec<_> = records(&simulation, "early_handoff").collect();
    assert_eq!(handoffs.len(), 1);
    assert_eq!(
        handoffs[0].data,
        json!({"reason": "tool_parse_error_limit", "session_tool_parse_errors": 2})
    );
    let errors: Vec<_> = records(&simulation, "model_error").collect();
    assert_eq!(errors[1].data["consecutive_errors"], 1);
    assert_eq!(errors[2].data["tool_parse_errors"], 3);
    assert!(errors.iter().all(|e| e.data["recovery_allowed"] == true));
    assert!(
        requests(&simulation)[3].handoff,
        "the request after the ceiling is the handoff"
    );

    let mut scenario = base();
    scenario.parameters.limits.tool_parse_retries = 0;
    scenario.models = vec![error(ModelFailure::Parse("no recovery".into()))];
    assert_eq!(
        run(scenario).unwrap().result.reason,
        StopReason::ToolParseRetryLimit
    );
}

#[test]
fn partial_disconnect_missing_usage_and_incomplete_response_never_execute_partial_tools() {
    for failure in [
        ModelFailure::Connection("disconnected".into()),
        ModelFailure::MissingUsage("no count".into()),
        ModelFailure::Incomplete("no done".into()),
    ] {
        let mut scenario = base();
        scenario.models = vec![error(failure.clone())];
        let simulation = run(scenario).unwrap();
        let reason = match failure {
            ModelFailure::Connection(_) => StopReason::ModelConnectionError,
            ModelFailure::MissingUsage(_) => StopReason::MissingTokenUsage,
            _ => StopReason::IncompleteModelResponse,
        };
        assert_eq!(simulation.result.reason, reason);
        assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
        assert!(tools(&simulation).is_empty());
        assert_eq!(records(&simulation, "model_chunk").count(), 1);
    }
    for incomplete in [true, false] {
        let mut scenario = base();
        let mut script = ending(true);
        if let ModelEnding::Response(response) = &mut script.completion.as_mut().unwrap().outcome {
            if incomplete {
                response.done = false;
            } else {
                response.generated_tokens = None;
            }
        }
        scenario.models = vec![script];
        let simulation = run(scenario).unwrap();
        assert_eq!(
            simulation.result.reason,
            if incomplete {
                StopReason::IncompleteModelResponse
            } else {
                StopReason::MissingTokenUsage
            }
        );
        assert!(tools(&simulation).is_empty());
    }
}

#[test]
fn context_guard_rejects_before_dispatch_and_seed_wraps_without_resetting_per_session() {
    let mut scenario = base();
    scenario.parameters.sampling_seed = i32::MAX as u32;
    scenario.models = vec![ending(false), ending(true)];
    let simulation = run(scenario).unwrap();
    let expected: Value = serde_json::from_str(include_str!(
        "../../playtime-protocol/tests/fixtures/legacy/context-seed.json"
    ))
    .unwrap();
    assert_eq!(
        json!(
            requests(&simulation)
                .iter()
                .map(|request| request.sampling_seed)
                .collect::<Vec<_>>()
        ),
        expected["request_seeds"]
    );
    assert!(
        requests(&simulation)
            .iter()
            .all(|request| request.context_basis == "input_bytes")
    );
    let mut scenario = base();
    scenario.prompt = "large input".repeat(4000);
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.model_calls, 0);
    assert_eq!(simulation.result.reason, StopReason::SessionLimit);
    assert_eq!(records(&simulation, "early_handoff").count(), 3);
    assert_eq!(records(&simulation, "handoff_skipped").count(), 3);
}

#[test]
fn an_unrepresentable_server_count_is_observable_and_keeps_a_conservative_charge() {
    let mut scenario = base();
    scenario.parameters.limits.work_turns_per_session = 2.try_into().unwrap();
    scenario.models = vec![
        response(5, vec![]),
        response(u64::MAX, vec![tool("exec", json!({"command": "no"}))]),
    ];
    let simulation = run(scenario).unwrap();
    assert_eq!(
        simulation.result.reason,
        StopReason::TokenLimitExceededByServer
    );
    assert_eq!(simulation.result.usage.known_generated_tokens, 5);
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
    assert_eq!(
        records(&simulation, "model_usage_overflow")
            .next()
            .unwrap()
            .data["reported_generated_tokens"],
        json!(u64::MAX)
    );
    assert!(tools(&simulation).is_empty());
}

#[test]
fn log_exhaustion_stops_admission_and_retains_result_and_inflight_charge() {
    let mut scenario = base();
    scenario.parameters.limits.log_bytes = 100_000.try_into().unwrap();
    scenario.models = vec![ModelScript {
        chunks: vec![TimedChunk {
            after_ms: 0,
            chunk: json!({"message": {"thinking": "x".repeat(100_000)}}),
        }],
        ..Default::default()
    }];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::LogLimit);
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
    assert!(tools(&simulation).is_empty());
    assert_eq!(records(&simulation, "controller_finished").count(), 1);
}

#[test]
fn persistence_failure_prevents_the_next_dispatch() {
    let mut scenario = base();
    scenario.models = vec![ending(true)];
    let reference = run(scenario.clone()).unwrap();
    scenario.fail_record_sequence = Some(
        records(&reference, "model_request")
            .next()
            .unwrap()
            .sequence,
    );
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::InfrastructureFailure);
    assert!(requests(&simulation).is_empty());
    assert!(tools(&simulation).is_empty());
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 0);
    assert!(simulation.result.usage.usage_complete);
    assert_eq!(records(&simulation, "request_not_dispatched").count(), 1);
}

#[test]
fn a_tool_abandoned_before_dispatch_is_not_included_as_evidence() {
    let mut scenario = fixture();
    let reference = run(scenario.clone()).unwrap();
    scenario.fail_record_sequence = Some(
        records(&reference, "tool_dispatch")
            .find(|record| record.data["name"] == "exec")
            .unwrap()
            .sequence,
    );
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::InfrastructureFailure);
    assert!(tools(&simulation).iter().all(|tool| tool.name != "exec"));
    assert_eq!(records(&simulation, "tool_not_dispatched").count(), 1);
    assert!(
        simulation.checkpoints[0]
            .evidence
            .as_ref()
            .unwrap()
            .content
            .contains("\"commands_total\":0")
    );
}

#[test]
fn command_timeout_keeps_unknown_exit_and_allows_the_next_bounded_turn() {
    let mut scenario = base();
    scenario.parameters.limits.work_turns_per_session = 2.try_into().unwrap();
    scenario.models = vec![
        response(
            5,
            vec![tool(
                "exec",
                json!({"command": "stall", "timeout_seconds": 0.1}),
            )],
        ),
        ending(true),
    ];
    scenario.execs = vec![ExecScript {
        command: "stall".into(),
        delay_ms: 0,
        never: true,
        result: json!({"exit_code": 0}),
        workspace_changes: BTreeMap::new(),
        tmp_changes: BTreeMap::new(),
        processes: Vec::new(),
    }];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert_eq!(simulation.result.elapsed_ms, 100);
    let result = &records(&simulation, "tool_result").next().unwrap().data["result"];
    assert_eq!(result["timed_out"], true);
    assert!(result.get("exit_code").is_none());
    assert_eq!(simulation.result.model_calls, 2);
}

#[test]
fn oversized_incoming_memory_and_oversized_stream_chunks_are_explicit() {
    let mut scenario = base();
    scenario
        .initial_files
        .insert("MEMENTO.md".into(), "😀".repeat(3000));
    scenario.models = vec![ending(true)];
    let simulation = run(scenario).unwrap();
    let received = &records(&simulation, "memory_received").next().unwrap().data["memory"];
    assert_eq!(received["content"], "");
    assert!(
        received["status"]
            .as_str()
            .unwrap()
            .starts_with("oversized;")
    );

    let mut scenario = base();
    scenario.models = vec![ModelScript {
        chunks: vec![
            TimedChunk {
                after_ms: 1,
                chunk: json!({"message": {"thinking": "visible partial"}}),
            },
            TimedChunk {
                after_ms: 1,
                chunk: json!({"message": {"content": "x".repeat(512 * 1024)}}),
            },
        ],
        ..Default::default()
    }];
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::ModelResponseByteLimit);
    assert_eq!(simulation.result.usage.unknown_reserved_tokens, 100);
    assert_eq!(records(&simulation, "model_chunk").count(), 1);
    assert_eq!(records(&simulation, "model_chunk_rejected").count(), 1);
    assert!(tools(&simulation).is_empty());
}

fn controller() -> Controller<SequentialIds> {
    let scenario = base();
    let reference: playtime_protocol::archive::InputManifest =
        playtime_protocol::archive::decode_input_manifest(include_str!(
            "../../playtime-protocol/tests/fixtures/two-session/inputs/manifest.json"
        ))
        .unwrap();
    Controller::new(
        ControllerConfig {
            parameters: scenario.parameters,
            prompt: scenario.prompt,
            protocol: scenario.protocol,
            inventory: scenario.inventory,
            initial_workspace: reference.initial_workspace,
            initial_files: BTreeMap::new(),
        },
        RunId::new("test").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap()
}

#[test]
fn duplicate_wrong_type_and_stale_completions_cannot_change_accounting() {
    let mut controller = controller();
    let start = controller.pending().unwrap().id;
    assert!(
        controller
            .advance(
                0,
                Input::Complete {
                    effect_id: start,
                    outcome: Outcome::Published
                }
            )
            .is_err()
    );
    controller
        .advance(0, Input::Dispatched { effect_id: start })
        .unwrap();
    controller
        .advance(
            1,
            Input::Complete {
                effect_id: start,
                outcome: Outcome::Started(Memory::loaded(String::new())),
            },
        )
        .unwrap();
    assert_eq!(controller.reserved_tokens(), 100);
    let pending = controller.pending().unwrap().clone();
    controller
        .advance(
            1,
            Input::Dispatched {
                effect_id: pending.id,
            },
        )
        .unwrap();
    assert!(
        controller
            .advance(
                1,
                Input::Complete {
                    effect_id: start,
                    outcome: Outcome::Started(Memory::loaded(String::new()))
                }
            )
            .is_err()
    );
    assert!(controller.advance(0, Input::Tick).is_err());
    assert_eq!(controller.pending(), Some(&pending));
    controller.advance(2, Input::Cancel).unwrap();
    assert_eq!(controller.usage().unknown_reserved_tokens(), 100);
    assert_eq!(controller.reserved_tokens(), 0);
    assert!(
        controller
            .advance(
                2,
                Input::Complete {
                    effect_id: pending.id,
                    outcome: Outcome::ModelError(ModelFailure::Connection("late".into()))
                }
            )
            .is_err()
    );
    assert_eq!(controller.usage().unknown_requests(), 1);
}

#[test]
fn preflight_cancellation_and_terminal_persistence_failure_remain_truthful() {
    let mut controller = controller();
    let start = controller.pending().unwrap().id;
    controller
        .advance(0, Input::Dispatched { effect_id: start })
        .unwrap();
    controller
        .advance(
            0,
            Input::Complete {
                effect_id: start,
                outcome: Outcome::Started(Memory::loaded(String::new())),
            },
        )
        .unwrap();
    assert_eq!(controller.reserved_tokens(), 100);
    controller.advance(1, Input::Cancel).unwrap();
    assert_eq!(controller.usage().unknown_requests(), 0);
    assert_eq!(controller.reserved_tokens(), 0);
    assert!(
        controller
            .take_records()
            .iter()
            .any(|record| record.kind == "request_not_dispatched")
    );

    let mut scenario = base();
    scenario.models = vec![ending(true)];
    let reference = run(scenario.clone()).unwrap();
    scenario.fail_record_sequence = Some(
        records(&reference, "controller_finished")
            .next()
            .unwrap()
            .sequence,
    );
    let simulation = run(scenario).unwrap();
    assert_eq!(simulation.result.reason, StopReason::AgentFinished);
    assert!(
        simulation
            .result
            .diagnostics
            .iter()
            .any(|text| text.contains("terminal event persistence"))
    );
}

#[test]
fn simulation_rejects_wrong_version_mode_and_host_paths() {
    let mut scenario = base();
    scenario.mode = RunMode::Experiment;
    assert!(run(scenario).is_err());
    let mut scenario = base();
    scenario.schema_version = 2;
    assert!(run(scenario).is_err());
    for name in ["../outside", "/etc/passwd", "a/../../outside"] {
        let mut scenario = base();
        scenario.initial_files.insert(name.into(), "no".into());
        assert!(run(scenario).is_err());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, rng_seed: RngSeed::Fixed(0x18_2026), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn accounting_admission_and_terminal_invariants(
        outcomes in prop::collection::vec((0_u8..4, 0_u64..250), 1..20),
        tokens in 51_u64..1000, turns in 1_u32..6, sessions in 1_u32..4,
        cancel_at in prop::option::of(0_u64..100), seed in 0_u32..=i32::MAX as u32,
    ) {
        let mut scenario = base();
        scenario.parameters.sampling_seed = seed;
        scenario.parameters.limits.generated_tokens = NonZeroU64::new(tokens).unwrap();
        scenario.parameters.limits.work_turns_per_session = turns.try_into().unwrap();
        scenario.parameters.limits.sessions = sessions.try_into().unwrap();
        scenario.cancel_at_ms = cancel_at;
        scenario.models = outcomes.into_iter().map(|(kind, count)| {
            let mut script = match kind {
                0 => response(count, vec![]),
                1 => error(ModelFailure::Parse("scripted parse error".into())),
                2 => response(count, vec![tool("end_session", json!({"memento": "next", "finish_run": count % 2 == 0}))]),
                _ => error(ModelFailure::Connection("scripted disconnect".into())),
            };
            script.completion.as_mut().unwrap().after_ms = 10;
            script
        }).collect();
        let simulation = run(scenario).unwrap();
        let mut previous = [0_u64; 4];
        let mut settled = 0_u64;
        let mut terminal = false;
        for record in &simulation.records {
            if record.kind == "request_settled" {
                let totals = &record.data["totals"];
                let current = [totals["known_generated_tokens"].as_u64().unwrap(), totals["unknown_reserved_tokens"].as_u64().unwrap(),
                    totals["unknown_requests"].as_u64().unwrap(), totals["budget_charged_tokens"].as_u64().unwrap()];
                prop_assert!(current.iter().zip(previous).all(|(new, old)| *new >= old));
                prop_assert_eq!(current[0] + current[1], current[3]);
                settled = current[3];
                previous = current;
            }
            if record.kind == "model_request" {
                let allowance = record.data["maximum_tokens"].as_u64().unwrap();
                let reserve = if record.data["handoff"] == true { 0 } else { 50 };
                prop_assert!(allowance > 0);
                prop_assert!(settled + allowance + reserve <= tokens);
            }
            if record.kind == "controller_stop" { terminal = true; }
            if terminal { prop_assert!(!["model_request", "tool_dispatch", "memento_write"].contains(&record.kind.as_str())); }
        }
        prop_assert!(terminal);
        prop_assert!(simulation.result.usage.validate().is_ok());
        prop_assert!(simulation.result.model_calls <= u64::from(sessions) * u64::from(turns + 2));
    }
}

fn external_tool_controller() -> Controller<SequentialIds> {
    let adapters = ScriptedAdapters::new(&fixture()).unwrap();
    let mut c = Controller::with_tool_policy(
        adapters.config().clone(),
        RunId::new("external").unwrap(),
        RunMode::ScriptedSmoke,
        SequentialIds,
        0,
        ToolPolicy::ConfirmedExternal {
            return_overhead_ms: 1000,
        },
    )
    .unwrap();
    let id = c.pending().unwrap().id;
    c.advance(0, Input::Dispatched { effect_id: id }).unwrap();
    c.advance(
        0,
        Input::Complete {
            effect_id: id,
            outcome: Outcome::Started(Memory::loaded(String::new())),
        },
    )
    .unwrap();
    let id = c.pending().unwrap().id;
    c.advance(0, Input::Dispatched { effect_id: id }).unwrap();
    c.advance(0, Input::Complete { effect_id: id, outcome: Outcome::Model(ModelResponse {
        done: true, generated_tokens: Some(32), prompt_tokens: None,
        message: json!({"role":"assistant","content":"","tool_calls":[tool("exec",json!({"command":"data-only","timeout_seconds":0.1}))]}),
    }) }).unwrap();
    c
}
#[test]
fn confirmed_bridge_timeout_continues_but_transport_expiry_never_fabricates_a_result() {
    let mut confirmed = external_tool_controller();
    let effect = confirmed.pending().unwrap().clone();
    assert_eq!(effect.deadline_ms, 5000);
    confirmed
        .advance(
            0,
            Input::Dispatched {
                effect_id: effect.id,
            },
        )
        .unwrap();
    confirmed
        .advance(
            200,
            Input::Complete {
                effect_id: effect.id,
                outcome: Outcome::Tool(
                    json!({"exit_code":-9,"timed_out":true,"stdout":"","stderr":""}),
                ),
            },
        )
        .unwrap();
    assert!(confirmed.terminal_decision().is_none());
    assert!(matches!(
        confirmed.pending().unwrap().kind,
        EffectKind::Model(_)
    ));

    for late_reply in [false, true] {
        let mut expired = external_tool_controller();
        let effect = expired.pending().unwrap().clone();
        expired
            .advance(
                0,
                Input::Dispatched {
                    effect_id: effect.id,
                },
            )
            .unwrap();
        expired.take_records();
        expired
            .advance(
                effect.deadline_ms,
                if late_reply {
                    Input::Complete {
                        effect_id: effect.id,
                        outcome: Outcome::Tool(json!({"exit_code":0,"timed_out":false})),
                    }
                } else {
                    Input::Tick
                },
            )
            .unwrap();
        assert_eq!(
            expired.terminal_decision(),
            Some(StopReason::InfrastructureFailure)
        );
        assert!(matches!(
            expired.pending().unwrap().kind,
            EffectKind::Snapshot { .. }
        ));
        assert!(
            !expired
                .take_records()
                .iter()
                .any(|r| r.kind == "tool_result" && r.data["result"]["timed_out"] == true)
        );
    }
}
