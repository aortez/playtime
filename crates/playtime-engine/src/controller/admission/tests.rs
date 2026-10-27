//! Pause the real controller immediately before policy proposal application, so
//! deliberately invalid proposals exercise runtime enforcement independently of
//! the concrete memento policy. External adapters cannot use this entry point.

use super::*;
use crate::simulation::{Scenario, ScriptedAdapters, SequentialIds};

type TestController = Controller<SequentialIds>;

fn idle() -> TestController {
    let scenario: Scenario =
        serde_json::from_str(include_str!("../../../tests/fixtures/two-session.json")).unwrap();
    let adapters = ScriptedAdapters::new(&scenario).unwrap();
    let mut controller = Controller::new(
        adapters.config().clone(),
        RunId::new("policy-test").unwrap(),
        RunMode::Simulation,
        SequentialIds,
        0,
    )
    .unwrap();
    complete_idle(
        &mut controller,
        Outcome::Started(Memory::loaded(String::new())),
    );
    controller
}

fn complete_idle(controller: &mut TestController, outcome: Outcome) {
    controller.take_control_updates();
    let effect_id = controller.pending().unwrap().id;
    controller
        .advance(0, Input::Dispatched { effect_id })
        .unwrap();
    let effect = controller.pending.take().unwrap();
    assert!(TestController::matches(&effect.kind, &outcome));
    controller.complete(effect.kind, outcome);
}

fn inference(handoff: bool) -> Inference {
    Inference {
        purpose: if handoff {
            RequestPurpose::Handoff
        } else {
            RequestPurpose::Work
        },
        messages: vec![json!({"role": "user", "content": "Continue"})],
        tools: contract::tools(handoff),
    }
}

fn accept_calls(controller: &mut TestController, calls: Vec<Value>) {
    controller
        .apply_proposal(Proposal::Infer(inference(false)))
        .unwrap();
    complete_idle(
        controller,
        Outcome::Model(ModelResponse {
            message: json!({"role": "assistant", "content": "", "tool_calls": calls}),
            done: true,
            generated_tokens: Some(5),
            prompt_tokens: Some(10),
        }),
    );
}

fn call(request: &ToolRequest) -> Value {
    json!({"function": {"name": request.name, "arguments": request.arguments}})
}

#[test]
fn runtime_derives_allowance_and_keeps_completed_unknown_charges() {
    let mut controller = idle();
    let allocation = controller.config.parameters.limits.generated_tokens.get();
    let reserve = controller
        .config
        .memento
        .as_ref()
        .unwrap()
        .parameters
        .limits
        .handoff_tokens
        .get();
    controller
        .totals
        .record(GenerationUsage::Unknown {
            reserved_tokens: NonZeroU64::new(19).unwrap(),
        })
        .unwrap();
    controller
        .totals
        .record(GenerationUsage::Known {
            generated_tokens: allocation - reserve - 19 - 7,
        })
        .unwrap();
    let before = controller.usage();
    controller
        .apply_proposal(Proposal::Infer(inference(false)))
        .unwrap();
    let EffectKind::Model(request) = &controller.pending().unwrap().kind else {
        panic!()
    };
    assert_eq!(request.maximum_tokens, 7);
    assert_eq!(controller.reserved_tokens(), 7);
    assert_eq!(controller.usage(), before);
    assert_eq!(request.call, 1);
    assert_eq!(
        request.sampling_seed,
        controller.config.parameters.sampling_seed
    );
    assert_eq!(
        controller.pending().unwrap().deadline_ms,
        (controller.config.parameters.limits.model_seconds.get() * 1000)
            .min(controller.deadline_ms)
    );

    let mut controller = idle();
    controller.apply_proposal(Proposal::BeginHandoff).unwrap();
    controller
        .apply_proposal(Proposal::Infer(inference(true)))
        .unwrap();
    let EffectKind::Model(request) = &controller.pending().unwrap().kind else {
        panic!()
    };
    assert_eq!(
        request.maximum_tokens,
        controller
            .config
            .memento
            .as_ref()
            .unwrap()
            .parameters
            .limits
            .handoff_tokens
            .get()
    );
}

#[test]
fn frozen_turn_retry_and_tool_capabilities_cannot_be_overridden() {
    for case in 0..6 {
        let mut controller = idle();
        let mut proposal = inference(false);
        match case {
            0 => {
                controller.turns = controller
                    .config
                    .memento
                    .as_ref()
                    .unwrap()
                    .parameters
                    .limits
                    .work_turns_per_session
                    .get()
            }
            1 => {
                controller.parse_streak = controller
                    .config
                    .memento
                    .as_ref()
                    .unwrap()
                    .parameters
                    .limits
                    .tool_parse_retries
                    + 1
            }
            2 => {
                controller.session_parse_errors = contract::parse_ceiling(
                    &controller
                        .config
                        .memento
                        .as_ref()
                        .unwrap()
                        .parameters
                        .limits,
                )
            }
            3 => proposal
                .tools
                .push(json!({"function": {"name": "host_exec"}})),
            4 | 5 => {
                controller.apply_proposal(Proposal::BeginHandoff).unwrap();
                proposal = inference(true);
                if case == 4 {
                    controller.handoff_attempts = 2;
                } else {
                    proposal.tools = contract::tools(false);
                }
            }
            _ => unreachable!(),
        }
        assert!(
            controller
                .apply_proposal(Proposal::Infer(proposal))
                .is_err(),
            "case {case}"
        );
        assert!(controller.pending().is_none());
        assert_eq!(controller.model_calls, 0);
        assert_eq!(controller.reserved_tokens(), 0);
    }
}

#[test]
fn context_fit_uses_actual_proposed_messages_and_verified_measurements() {
    let mut controller = idle();
    let mut proposal = inference(false);
    proposal.messages = vec![json!({"role": "user", "content": "x".repeat(
        controller.config.parameters.limits.context_tokens.get() as usize)})];
    controller
        .apply_proposal(Proposal::Infer(proposal))
        .unwrap();
    assert!(controller.pending().is_none());
    assert_eq!(controller.model_calls, 0);
    assert_eq!(controller.reserved_tokens(), 0);
    assert_eq!(controller.phase(), RunPhase::HandingOff);
    assert!(
        controller
            .take_records()
            .iter()
            .any(|r| r.kind == "early_handoff" && r.data["reason"] == "context_guard")
    );

    let mut controller = idle();
    accept_calls(&mut controller, Vec::new());
    // Rewritten history cannot borrow the low server measurement of a different
    // prefix. The estimator must count these actual proposed bytes again.
    let mut proposal = inference(false);
    proposal.messages[0]["content"] = json!("changed prefix");
    let expected = encoded_size(&json!({"messages": proposal.messages, "tools": proposal.tools}));
    controller
        .apply_proposal(Proposal::Infer(proposal))
        .unwrap();
    let EffectKind::Model(request) = &controller.pending().unwrap().kind else {
        panic!()
    };
    assert_eq!(request.context_basis, "input_bytes");
    assert_eq!(request.context_input_bound, expected);

    let mut controller = idle();
    accept_calls(&mut controller, Vec::new());
    let mut proposal = inference(false);
    // The injected request differs from memento's stored conversation. Only the
    // actual admitted prefix can inherit the measured 10 input + 5 output tokens.
    proposal
        .messages
        .push(json!({"role": "assistant", "content": "", "tool_calls": []}));
    let extra = json!({"role": "user", "content": "More"});
    proposal.messages.push(extra.clone());
    controller
        .apply_proposal(Proposal::Infer(proposal))
        .unwrap();
    let EffectKind::Model(request) = &controller.pending().unwrap().kind else {
        panic!()
    };
    assert_eq!(request.context_basis, "server_usage_plus_new_bytes");
    assert_eq!(
        request.context_input_bound,
        15 + encoded_size(&json!([extra]))
    );
}

#[test]
fn tool_proposals_require_a_response_call_and_frozen_schema() {
    let mut controller = idle();
    let exec = ToolRequest {
        name: "exec".into(),
        arguments: json!({"command": "data only"}),
    };
    assert!(controller.apply_proposal(Proposal::Tool(exec)).is_err());
    assert!(controller.pending().is_none());

    for request in [
        ToolRequest {
            name: "host_exec".into(),
            arguments: json!({"command": "data only"}),
        },
        ToolRequest {
            name: "exec".into(),
            arguments: json!({"command": 42}),
        },
        ToolRequest {
            name: "end_session".into(),
            arguments: json!({"memento": "x".repeat(32769), "finish_run": true}),
        },
    ] {
        let mut controller = idle();
        accept_calls(&mut controller, vec![call(&request)]);
        controller.take_records();
        controller.apply_proposal(Proposal::Tool(request)).unwrap();
        assert!(controller.pending().is_none());
        assert!(!controller.ended && !controller.finished);
        let records = controller.take_records();
        assert!(
            records
                .iter()
                .any(|r| r.kind == "tool_result" && r.data["result"]["error"].is_string())
        );
        assert!(
            !records
                .iter()
                .any(|r| r.kind == "tool_dispatch" || r.kind == "memento_write")
        );
    }
}

#[test]
fn stop_and_dispatch_facts_survive_further_policy_proposals() {
    for dispatched in [false, true] {
        for stop in [
            Input::Cancel,
            Input::PersistenceFailed("writer stopped".into()),
            Input::Tick,
        ] {
            let mut controller = idle();
            controller
                .apply_proposal(Proposal::Infer(inference(false)))
                .unwrap();
            let allowance = controller.reserved_tokens();
            let effect_id = controller.pending().unwrap().id;
            if dispatched {
                controller
                    .advance(0, Input::Dispatched { effect_id })
                    .unwrap();
            }
            let now = if matches!(stop, Input::Tick) {
                controller.deadline_ms
            } else {
                0
            };
            controller.advance(now, stop).unwrap();
            let finalization = controller.pending().cloned();
            assert!(
                controller
                    .apply_proposal(Proposal::Infer(inference(false)))
                    .is_err()
            );
            assert_eq!(controller.pending(), finalization.as_ref());
            assert_eq!(controller.reserved_tokens(), 0);
            assert_eq!(
                UsageSummary::from(controller.usage()).unknown_reserved_tokens,
                if dispatched { allowance } else { 0 }
            );
        }
    }
}

#[test]
fn successful_finish_and_next_session_require_actual_publication() {
    for publication_fails in [false, true] {
        let mut controller = idle();
        let request = ToolRequest {
            name: "end_session".into(),
            arguments: json!({"memento": "done", "finish_run": true}),
        };
        accept_calls(&mut controller, vec![call(&request)]);
        controller.apply_proposal(Proposal::Tool(request)).unwrap();
        complete_idle(&mut controller, Outcome::Tool(json!({"bytes_written": 4})));
        assert!(controller.finished);
        assert!(
            controller
                .apply_proposal(Proposal::Finish(StopReason::AgentFinished))
                .is_err()
        );
        assert!(controller.apply_proposal(Proposal::StartSession).is_err());
        controller
            .apply_proposal(Proposal::Checkpoint {
                missing_handoff: false,
            })
            .unwrap();
        controller.pump();
        let snapshot = Snapshot {
            workspace: controller.config.initial_workspace.clone(),
            files: controller
                .config
                .memento
                .as_ref()
                .unwrap()
                .initial_files
                .clone(),
            memory: Memory::loaded("done".into()),
        };
        complete_idle(&mut controller, Outcome::Snapshot(snapshot));
        assert!(controller.last_checkpoint.is_none());
        assert!(
            controller
                .apply_proposal(Proposal::Finish(StopReason::AgentFinished))
                .is_err()
        );
        if publication_fails {
            complete_idle(&mut controller, Outcome::Failed("writer stopped".into()));
            assert!(
                controller
                    .apply_proposal(Proposal::Finish(StopReason::AgentFinished))
                    .is_err()
            );
            assert_eq!(
                controller.terminal_decision(),
                Some(StopReason::InfrastructureFailure)
            );
            assert!(controller.last_checkpoint.is_none());
        } else {
            complete_idle(&mut controller, Outcome::Published);
            controller
                .apply_proposal(Proposal::Finish(StopReason::AgentFinished))
                .unwrap();
            assert_eq!(
                controller.terminal_decision(),
                Some(StopReason::AgentFinished)
            );
            assert!(controller.last_checkpoint.is_some());
        }
    }
}

#[cfg(feature = "test-harness")]
fn compaction_idle(allow_finish_run: bool) -> TestController {
    use crate::compaction::{CompactionConfig, CompactionSettings};
    let scenario: Scenario =
        serde_json::from_str(include_str!("../../../tests/fixtures/two-session.json")).unwrap();
    let adapter = ScriptedAdapters::new(&scenario).unwrap();
    let mut parameters = scenario.parameters.common();
    parameters.limits.context_tokens = 65536.try_into().unwrap();
    let config = CompactionConfig {
        settings: CompactionSettings {
            summary_tokens: 20.try_into().unwrap(),
            allow_finish_run,
            ..CompactionSettings::default()
        },
        parameters,
        protocol: "system".into(),
        prompt: "task".into(),
        inventory: "workshop".into(),
        initial_workspace: adapter.config().initial_workspace.clone(),
        initial_files: Default::default(),
    };
    let mut controller =
        Controller::test_compaction(config, RunId::new("policy-test").unwrap(), SequentialIds, 0)
            .unwrap();
    complete_idle(&mut controller, Outcome::WorkspaceStarted);
    controller
}

#[cfg(feature = "test-harness")]
#[test]
fn compaction_proposals_cannot_grant_tokens_tools_or_completion() {
    let mut controller = compaction_idle(true);
    for purpose in [
        RequestPurpose::Work,
        RequestPurpose::Compaction,
        RequestPurpose::Handoff,
    ] {
        assert!(
            controller
                .apply_proposal(Proposal::Infer(Inference {
                    purpose,
                    messages: vec![json!({"role":"user","content":"request"})],
                    tools: vec![json!({"function":{"name":"host_exec"}})]
                }))
                .is_err()
        );
        assert!(controller.pending().is_none());
    }
    assert!(
        controller
            .apply_proposal(Proposal::Finish(StopReason::AgentFinished))
            .is_err()
    );
    assert!(
        controller
            .apply_proposal(Proposal::Stop(StopReason::AgentFinished))
            .is_err()
    );
    let work_cap = controller.config.parameters.limits.tokens_per_call.get();
    let total = controller.config.parameters.limits.generated_tokens.get();
    controller
        .totals
        .record(GenerationUsage::Known {
            generated_tokens: total - work_cap - 7,
        })
        .unwrap();
    controller
        .apply_proposal(Proposal::Infer(Inference {
            purpose: RequestPurpose::Compaction,
            messages: vec![json!({"role":"user","content":"summary"})],
            tools: Vec::new(),
        }))
        .unwrap();
    let EffectKind::Model(request) = &controller.pending().unwrap().kind else {
        panic!()
    };
    assert_eq!(
        request.maximum_tokens, 7,
        "runtime reserves continued work independently of policy"
    );
    assert_eq!(request.context_reserve, 3277);
    assert_eq!(controller.reserved_tokens(), 7);
}

#[cfg(feature = "test-harness")]
#[test]
fn compaction_work_cannot_exceed_the_consecutive_parse_retry_limit() {
    let mut controller = compaction_idle(true);
    let work = || {
        Proposal::Infer(Inference {
            purpose: RequestPurpose::Work,
            messages: vec![json!({"role":"user","content":"request"})],
            tools: crate::compaction::tools(true),
        })
    };
    controller.parse_streak = crate::compaction::TOOL_PARSE_RETRIES + 1;
    assert!(controller.apply_proposal(work()).is_err());
    assert!(controller.pending().is_none());
    assert_eq!(controller.reserved_tokens(), 0);
    controller.parse_streak = crate::compaction::TOOL_PARSE_RETRIES;
    controller.apply_proposal(work()).unwrap();
    assert!(controller.pending().is_some());
}

#[cfg(feature = "test-harness")]
#[test]
fn compaction_work_cannot_offer_a_withheld_finish_run() {
    let mut controller = compaction_idle(false);
    let work = |allow_finish_run| {
        Proposal::Infer(Inference {
            purpose: RequestPurpose::Work,
            messages: vec![json!({"role":"user","content":"request"})],
            tools: crate::compaction::tools(allow_finish_run),
        })
    };
    assert!(controller.apply_proposal(work(true)).is_err());
    assert!(controller.pending().is_none());
    controller.apply_proposal(work(false)).unwrap();
    assert!(controller.pending().is_some());
}
