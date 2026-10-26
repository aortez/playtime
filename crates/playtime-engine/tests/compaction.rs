#![cfg(feature = "test-harness")]

use playtime_core::{RequestPurpose, RunId, StopReason};
use playtime_engine::{
    compaction::{CompactionConfig, CompactionSettings},
    simulation::{Scenario, SequentialIds},
    *,
};
use playtime_protocol::events::CleanupOutcome;
use serde_json::{Value, json};

fn config() -> CompactionConfig {
    let scenario: Scenario =
        serde_json::from_str(include_str!("fixtures/two-session.json")).unwrap();
    let mut parameters = scenario.parameters.common();
    parameters.limits.context_tokens = 16384.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 10000.try_into().unwrap();
    parameters.limits.output_bytes = 2048.try_into().unwrap();
    CompactionConfig {
        parameters,
        protocol: "Keep the original instructions.".into(),
        prompt: "Investigate and report checked results.".into(),
        inventory: "Owned workshop; no network.".into(),
        settings: CompactionSettings {
            summary_tokens: 256.try_into().unwrap(),
            ..CompactionSettings::default()
        },
        initial_workspace: serde_json::from_value(json!({
            "path":"inputs/workspace.json", "size_bytes":2,
            "sha256":"44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        }))
        .unwrap(),
        initial_files: Default::default(),
    }
}

struct Run {
    controller: Controller<SequentialIds>,
    config: CompactionConfig,
    now: u64,
    records: Vec<Record>,
    observations: Vec<BoundRecord>,
    dispatches: Vec<Effect>,
}
impl Run {
    fn new(config: CompactionConfig) -> Self {
        let controller = Controller::test_compaction(
            config.clone(),
            RunId::new("compaction-test").unwrap(),
            SequentialIds,
            0,
        )
        .unwrap();
        let mut run = Self {
            controller,
            config,
            now: 0,
            records: Vec::new(),
            observations: Vec::new(),
            dispatches: Vec::new(),
        };
        run.collect();
        run.complete(Outcome::WorkspaceStarted);
        run
    }
    fn collect(&mut self) {
        let observations = self.controller.take_observations();
        self.records
            .extend(observations.iter().map(|entry| entry.record.clone()));
        self.observations.extend(observations);
        self.controller.take_control_updates();
    }
    fn input(&mut self, input: Input) {
        self.controller.advance(self.now, input).unwrap();
        self.collect();
    }
    fn dispatch(&mut self) -> Effect {
        let effect = self.controller.pending().unwrap().clone();
        self.input(Input::Dispatched {
            effect_id: effect.id,
        });
        self.dispatches.push(effect.clone());
        effect
    }
    fn complete(&mut self, outcome: Outcome) {
        let effect = self.dispatch();
        self.now += 1;
        self.input(Input::Complete {
            effect_id: effect.id,
            outcome,
        });
    }
    fn request(&self) -> &ModelRequest {
        let EffectKind::Model(request) = &self.controller.pending().expect("pending model").kind
        else {
            panic!(
                "expected inference, stopped {:?}",
                self.controller.terminal_decision()
            )
        };
        request
    }
    fn response(&mut self, message: Value) {
        self.complete(Outcome::ModelCompleted {
            response: ModelResponse {
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: None,
                message,
            },
            finish_reason: "stop".into(),
        });
    }
    fn until_summary(&mut self) {
        for n in 0..20 {
            if self.request().purpose == RequestPurpose::Compaction {
                return;
            }
            self.response(
                json!({"role":"assistant","content":format!("Exchange {n}: {}", "x".repeat(3000))}),
            );
        }
        panic!("no summary request");
    }
    fn finalize(&mut self) -> RunResult {
        for _ in 0..4 {
            if let Some(result) = self.controller.result() {
                return result.clone();
            }
            match &self.controller.pending().unwrap().kind {
                EffectKind::CaptureWorkspace => {
                    self.complete(Outcome::WorkspaceCaptured(WorkspaceSnapshot {
                        workspace: self.config.initial_workspace.clone(),
                        files: Default::default(),
                    }))
                }
                EffectKind::PublishCapture(_) => {
                    let effect = self.dispatch();
                    // Explicit simulated writer acknowledgment, never an adapter
                    // completion or proof of real storage/native cleanup.
                    self.input(Input::PublicationCommitted {
                        effect_id: effect.id,
                    });
                }
                EffectKind::Cleanup => self.complete(Outcome::Cleaned(Vec::new())),
                other => panic!("unexpected finalization effect: {other:?}"),
            }
        }
        self.controller.result().unwrap().clone()
    }
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"function":{"name":name,"arguments":arguments}})
}
fn finish(summary: Value) -> Value {
    json!({"role":"assistant","tool_calls":[call("finish_run",json!({"summary":summary}))]})
}

fn contract_projection(run: &Run) -> Value {
    use sha2::{Digest, Sha256};
    let digest = |value: &Value| {
        Sha256::digest(serde_json::to_vec(value).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let requests:Vec<_> = run.dispatches.iter().filter_map(|effect| {
        let EffectKind::Model(request) = &effect.kind else {return None};
        Some(json!({
            "request_id":request.request_id,"purpose":request.purpose,"call":request.call,
            "sampling_seed":request.sampling_seed,"maximum_tokens":request.maximum_tokens,
            "context_input_bound":request.context_input_bound,"context_reserve":request.context_reserve,
            "context_basis":request.context_basis,"messages_sha256":digest(&json!(request.messages)),
            "tools_sha256":digest(&json!(request.tools))
        }))
    }).collect();
    let transitions: Vec<_> = run
        .records
        .iter()
        .filter(|r| r.kind.starts_with("compaction_") || r.kind == "completion_declared")
        .map(|record| {
            let mut data = record.data.clone();
            for field in ["before_messages", "messages"] {
                if let Some(messages) = data.as_object_mut().unwrap().remove(field) {
                    data[format!("{field}_sha256")] = json!(digest(&messages));
                }
            }
            json!({"sequence":record.sequence,"kind":record.kind,"data":data})
        })
        .collect();
    json!({"default_settings":CompactionSettings::default(),"requests":requests,"transitions":transitions,
        "tools":run.dispatches.iter().find_map(|effect|if let EffectKind::Model(r)=&effect.kind {Some(&r.tools)} else {None}),
        "result":run.controller.result()})
}

#[test]
fn repeated_compaction_preserves_root_recent_exchanges_previous_summary_and_global_accounting() {
    let mut run = Run::new(config());
    let root = run.request().messages[..2].to_vec();
    for ordinal in 1..=3 {
        run.until_summary();
        let request = run.request().clone();
        assert!(request.tools.is_empty());
        assert_eq!(request.messages[..2], root);
        assert_eq!(request.maximum_tokens, 256);
        assert_eq!(request.context_reserve, 1024);
        let started = &run
            .records
            .iter()
            .rev()
            .find(|r| r.kind == "compaction_planned")
            .unwrap()
            .data;
        assert_eq!(started["ordinal"], ordinal);
        assert_eq!(started["retained_requests"].as_array().unwrap().len(), 2);
        let before = started["before_messages"].as_array().unwrap().clone();
        let recent = before[before.len() - 2..].to_vec();
        if ordinal > 1 {
            assert!(
                serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains(&format!("Summary {}", ordinal - 1))
            );
        }
        run.response(json!({"role":"assistant","thinking":"private reasoning is retained only in evidence","content":format!("Summary {ordinal}: checked results, next action.")}));
        let next = run.request();
        assert_eq!(next.purpose, RequestPurpose::Work);
        assert_eq!(next.context_basis, "input_bytes");
        assert_eq!(next.messages[..2], root);
        assert_eq!(&next.messages[next.messages.len() - 2..], recent);
        assert!(
            !serde_json::to_string(&next.messages)
                .unwrap()
                .contains("private reasoning")
        );
    }
    run.response(finish(json!("Done; evidence is in the workshop.")));
    assert!(
        run.controller.terminal_decision().is_none(),
        "declaration is not a publication receipt"
    );
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::AgentFinished);
    assert_eq!(result.cleanup, CleanupOutcome::Complete);
    assert!(result.last_capture.is_some());
    assert!(result.last_checkpoint.is_none());
    assert_eq!(result.sessions_started, 0);
    let requests: Vec<_> = run
        .dispatches
        .iter()
        .filter_map(|effect| {
            if let EffectKind::Model(request) = &effect.kind {
                Some(request)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(requests.len() as u64, result.model_calls);
    assert_eq!(result.usage.known_generated_tokens, 10 * result.model_calls);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.purpose == RequestPurpose::Compaction)
            .count(),
        3
    );
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.call, index as u64 + 1);
        assert_eq!(
            request.sampling_seed,
            run.config.parameters.sampling_seed + index as u32
        );
    }
    assert_eq!(
        run.dispatches
            .iter()
            .filter(|e| matches!(e.kind, EffectKind::StartWorkspace))
            .count(),
        1
    );
    assert_eq!(
        run.dispatches
            .iter()
            .filter(|e| matches!(e.kind, EffectKind::CaptureWorkspace))
            .count(),
        1
    );
    assert!(run.records.iter().all(|r| r.session_id.is_none()));
    for entry in &run.observations {
        match entry.record.kind.as_str() {
            "compaction_planned" => {
                assert!(entry.request.is_none());
                assert!(entry.identity.request_id.is_none());
            }
            "compaction_applied" => assert_eq!(
                entry.request.as_ref().unwrap().purpose,
                RequestPurpose::Compaction
            ),
            "completion_declared" => assert_eq!(
                entry.request.as_ref().unwrap().purpose,
                RequestPurpose::Work
            ),
            _ => {}
        }
    }
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/compaction-policy-v1.json")).unwrap();
    assert_eq!(
        contract_projection(&run),
        expected,
        "review this separate compaction contract; never rewrite memento goldens"
    );
}

#[test]
fn reported_usage_compacts_before_the_next_summary_outgrows_context() {
    for retained in [1, 2] {
        let mut config = config();
        config.settings.recent_exchanges = retained;
        config.parameters.limits.generated_tokens = 50000.try_into().unwrap();
        config.parameters.limits.log_bytes = 8000000.try_into().unwrap();
        let context = config.parameters.limits.context_tokens.get();
        let threshold = (context * u64::from(config.settings.trigger_percent)).div_ceil(100);
        let mut run = Run::new(config);
        let root = run.request().messages.clone();
        let reserve = run.request().context_reserve;
        let mut work_calls = 0;
        let mut summaries = 0;
        for _ in 0..40 {
            assert_eq!(run.controller.terminal_decision(), None);
            let request = run.request().clone();
            let prompt_tokens =
                (serde_json::to_vec(&json!({"messages":request.messages,"tools":request.tools}))
                    .unwrap()
                    .len() as u64)
                    .div_ceil(8);
            let (message, generated_tokens) = if request.purpose == RequestPurpose::Work {
                work_calls += 1;
                (
                    json!({"role":"assistant","content":format!("Exchange {work_calls}: {}", "x".repeat(3000))}),
                    400,
                )
            } else {
                assert_eq!(request.purpose, RequestPurpose::Compaction);
                assert_eq!(request.context_basis, "input_bytes");
                assert!(
                    request.context_input_bound + request.maximum_tokens + request.context_reserve
                        < context
                );
                let plan = &run
                    .records
                    .iter()
                    .rev()
                    .find(|r| r.kind == "compaction_planned")
                    .unwrap()
                    .data;
                // Measured work occupancy still permits more work; the summary
                // fit, not the ordinary work trigger or reserve, must stop it.
                assert_eq!(plan["estimate_basis"], "server_usage_plus_new_bytes");
                let projected = plan["projected_tokens"].as_u64().unwrap();
                assert!(projected < threshold && projected + reserve < context);
                assert_eq!(
                    plan["retained_requests"].as_array().unwrap().len(),
                    usize::from(retained)
                );
                // One more response would move the oldest retained exchange
                // into the summary and make its byte estimate exceed the bound.
                // With retention one this is the entire current prefix, which
                // must not borrow today's measurement for tomorrow's summary.
                let mut deferred = plan["before_messages"].as_array().unwrap().clone();
                deferred.truncate(deferred.len() - usize::from(retained) + 1);
                deferred.push(json!({"role":"user","content":run.config.settings.instructions}));
                let deferred_bytes = serde_json::to_vec(&json!({"messages":deferred,"tools":[]}))
                    .unwrap()
                    .len() as u64;
                assert!(
                    deferred_bytes + request.maximum_tokens + request.context_reserve >= context
                );
                assert_eq!(&request.messages[..root.len()], &root);
                for n in work_calls - u64::from(retained) + 1..=work_calls {
                    assert!(!request.messages.iter().any(|m| {
                        m["content"]
                            .as_str()
                            .is_some_and(|s| s.starts_with(&format!("Exchange {n}:")))
                    }));
                }
                if summaries > 0 {
                    assert!(request.messages.iter().any(|m| {
                        m["content"]
                            .as_str()
                            .is_some_and(|s| s.contains(&format!("Summary {summaries}")))
                    }));
                }
                summaries += 1;
                (
                    json!({"role":"assistant","content":format!("Summary {summaries}: preserve the checked findings.")}),
                    10,
                )
            };
            run.complete(Outcome::ModelCompleted {
                response: ModelResponse {
                    done: true,
                    generated_tokens: Some(generated_tokens),
                    prompt_tokens: Some(prompt_tokens),
                    message,
                },
                finish_reason: "stop".into(),
            });
            if summaries == 3 {
                break;
            }
        }
        assert_eq!(summaries, 3);
        assert_eq!(run.request().purpose, RequestPurpose::Work);
        assert_eq!(
            run.records
                .iter()
                .filter(|r| r.kind == "compaction_applied")
                .count(),
            3
        );
        assert!(!run.records.iter().any(|r| r.kind == "compaction_rejected"));
        run.response(finish(json!("done after repeated measured compaction")));
        assert_eq!(run.finalize().reason, StopReason::AgentFinished);
    }
}

#[test]
fn summary_requires_normal_completion_final_content_and_progress() {
    for case in 0..8 {
        let mut run = Run::new(config());
        run.until_summary();
        let mut response = ModelResponse {
            done: true,
            generated_tokens: Some(10),
            prompt_tokens: None,
            message: json!({"role":"assistant","content":"summary"}),
        };
        let outcome = match case {
            0 => Outcome::Model(response),
            1 => Outcome::ModelCompleted {
                response,
                finish_reason: "length".into(),
            },
            2 => {
                response.message = json!({"role":"assistant","thinking":"thinking only"});
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
            3 => {
                response.message["content"] = json!("x".repeat(20000));
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
            4 => {
                response.generated_tokens = None;
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
            5 => {
                response.done = false;
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
            6 => {
                response.message["tool_calls"] =
                    json!([call("exec", json!({"command":"must not execute"}))]);
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
            _ => {
                response.generated_tokens = Some(run.request().maximum_tokens + 1);
                Outcome::ModelCompleted {
                    response,
                    finish_reason: "stop".into(),
                }
            }
        };
        let allowance = run.request().maximum_tokens;
        run.complete(outcome);
        let result = run.finalize();
        assert_eq!(
            result.reason,
            match case {
                0..=2 => StopReason::CompactionFailed,
                3 => StopReason::CompactionNoProgress,
                4 => StopReason::MissingTokenUsage,
                5 => StopReason::IncompleteModelResponse,
                6 => StopReason::CompactionFailed,
                _ => StopReason::TokenLimitExceededByServer,
            }
        );
        assert!(!run.records.iter().any(|r| r.kind == "compaction_applied"));
        if matches!(case, 4 | 5) {
            assert_eq!(result.usage.unknown_reserved_tokens, allowance);
        }
        assert_eq!(run.dispatches.iter().filter(|e| matches!(&e.kind, EffectKind::Model(r) if r.purpose == RequestPurpose::Compaction)).count(),1);
    }
}

#[test]
fn invalid_or_mixed_finish_calls_are_rejected_before_any_tool_side_effect() {
    let exec = call("exec", json!({"command":"never execute"}));
    for message in [
        finish(json!("")),
        finish(json!(" \n\t")),
        finish(json!("é".repeat(4097))),
        finish(json!(42)),
        json!({"role":"assistant","tool_calls":[call("finish_run",json!({}))]}),
        json!({"role":"assistant","tool_calls":[call("finish_run",json!({"summary":"done","extra":true}))]}),
        json!({"role":"assistant","tool_calls":[exec.clone(),call("finish_run",json!({"summary":"done"}))]}),
        json!({"role":"assistant","tool_calls":[call("finish_run",json!({"summary":"done"})),exec]}),
    ] {
        let mut run = Run::new(config());
        run.response(message);
        assert_eq!(run.finalize().reason, StopReason::InvalidCompletion);
        assert!(
            !run.dispatches
                .iter()
                .any(|e| matches!(e.kind, EffectKind::Tool(_)))
        );
        assert!(!run.records.iter().any(|r| r.kind == "completion_declared"));
    }
    let mut run = Run::new(config());
    run.response(finish(json!("é".repeat(4096))));
    assert_eq!(run.finalize().reason, StopReason::AgentFinished);
}

#[test]
fn tool_pairs_share_an_aggregate_allowance_and_plain_text_does_not_finish() {
    let mut run = Run::new(config());
    run.response(json!({"role":"assistant","content":"I am done."}));
    assert_eq!(run.request().purpose, RequestPurpose::Work);
    run.response(json!({"role":"assistant","tool_calls":[call("exec",json!({"command":"first"})),call("exec",json!({"command":"second"}))]}));
    for _ in 0..2 {
        run.complete(Outcome::Tool(
            json!({"exit_code":0,"stdout":"é".repeat(10000),"stderr":"","timed_out":false}),
        ));
    }
    let request = run.request();
    let results: Vec<_> = request
        .messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    // ASCII output after canonical escaping makes these serialized lengths exact.
    assert!(
        results
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap().len())
            .sum::<usize>()
            <= 2048
    );
    assert!(
        results
            .iter()
            .all(|m| m["content"].as_str().unwrap().contains("context_truncated"))
    );
    assert_eq!(
        run.records
            .iter()
            .filter(|r| r.kind == "tool_result" && !r.data["unabridged_result"].is_null())
            .count(),
        2
    );
    run.response(finish(json!("done")));
    assert_eq!(run.finalize().reason, StopReason::AgentFinished);
}

#[test]
fn cancellation_deadline_and_publication_failure_keep_the_first_stop() {
    for (dispatched, cause) in [(false, 0), (true, 0), (true, 1), (true, 2)] {
        let mut run = Run::new(config());
        run.until_summary();
        let allowance = run.request().maximum_tokens;
        if dispatched {
            run.dispatch();
            run.input(Input::Chunk {
                effect_id: run.controller.pending().unwrap().id,
                chunk: json!({"message":{"content":"partial summary"}}),
            });
        }
        let input = if cause == 0 {
            Input::Cancel
        } else {
            run.now = if cause == 1 {
                run.config.parameters.limits.wall_seconds.get() * 1000
            } else {
                run.controller.pending().unwrap().deadline_ms
            };
            Input::Tick
        };
        run.input(input);
        let expected = match cause {
            0 => StopReason::Interrupted,
            1 => StopReason::WallLimit,
            _ => StopReason::ModelTimeout,
        };
        let result = run.finalize();
        assert_eq!(result.reason, expected);
        assert_eq!(
            result.usage.unknown_reserved_tokens,
            if dispatched { allowance } else { 0 }
        );
        assert!(!run.records.iter().any(|r| r.kind == "compaction_applied"));
        if dispatched {
            assert!(run.records.iter().any(|r| r.kind == "model_chunk"
                && r.data["chunk"]["message"]["content"] == "partial summary"));
        }
    }
    let mut run = Run::new(config());
    run.response(finish(json!("done")));
    run.complete(Outcome::WorkspaceCaptured(WorkspaceSnapshot {
        workspace: run.config.initial_workspace.clone(),
        files: Default::default(),
    }));
    run.complete(Outcome::Failed("publication failed".into()));
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::InfrastructureFailure);
    assert!(result.last_capture.is_none());
}

#[test]
fn settings_reject_impossible_or_unsupported_values_without_changing_allocations() {
    let defaults = CompactionSettings::default();
    assert_eq!(defaults.trigger_percent, 75);
    assert_eq!(defaults.recent_exchanges, 2);
    assert_eq!(defaults.summary_tokens.get(), 2048);
    assert_eq!(CompactionSettings::safety_margin(20480), 1024);
    assert_eq!(CompactionSettings::safety_margin(20481), 1025);
    for case in 0..9 {
        let mut config = config();
        match case {
            0 => config.settings.schema_version = 2,
            1 => config.settings.trigger_percent = 0,
            2 => config.settings.trigger_percent = 100,
            3 => config.settings.recent_exchanges = 33,
            4 => config.settings.summary_tokens = 513.try_into().unwrap(),
            5 => config.settings.instructions = " \n".into(),
            6 => config.settings.instructions = "x".repeat(65537),
            7 => config.prompt = "large".repeat(5000),
            _ => config.settings.trigger_percent = 1,
        }
        assert!(
            Controller::test_compaction(config, RunId::new("invalid").unwrap(), SequentialIds, 0)
                .is_err(),
            "case {case}"
        );
    }
    let mut value = json!(defaults);
    value["summary_tokens"] = json!(0);
    assert!(serde_json::from_value::<CompactionSettings>(value).is_err());
}

#[test]
fn exact_trigger_and_earlier_reserve_edges_use_the_next_request_allowance() {
    // Retain zero only for this boundary test, so even one small exchange may
    // be compacted. Server usage determines this exact unchanged-prefix bound.
    for trigger in [50, 99] {
        let mut config = config();
        config.settings.trigger_percent = trigger;
        config.settings.recent_exchanges = 0;
        let context = config.parameters.limits.context_tokens.get();
        let mut initial = Run::new(config.clone());
        let reserve = initial.request().context_reserve;
        let cap = initial.request().maximum_tokens;
        let threshold = (context * u64::from(trigger)).div_ceil(100);
        let boundary = threshold.min(context - reserve);
        initial.input(Input::Cancel);
        for delta in [-1_i64, 0, 1] {
            let mut run = Run::new(config.clone());
            let projected = boundary.checked_add_signed(delta).unwrap();
            // ContextWindow adds the two bytes for the empty appended array.
            let prompt = projected - cap - 10 - 2;
            run.complete(Outcome::ModelCompleted {
                response: ModelResponse {
                    done: true,
                    generated_tokens: Some(10),
                    prompt_tokens: Some(prompt),
                    message: json!({"role":"assistant","content":"one complete exchange"}),
                },
                finish_reason: "stop".into(),
            });
            assert_eq!(
                run.request().purpose,
                if delta < 0 {
                    RequestPurpose::Work
                } else {
                    RequestPurpose::Compaction
                },
                "trigger {trigger}, delta {delta}"
            );
            if delta >= 0 {
                let decision = &run
                    .records
                    .iter()
                    .find(|r| r.kind == "compaction_planned")
                    .unwrap()
                    .data;
                assert_eq!(decision["estimate_basis"], "server_usage_plus_new_bytes");
                assert_eq!(decision["projected_tokens"], projected);
            }
        }
    }
}

#[test]
fn retained_tool_exchange_stays_paired_and_older_tools_enter_the_summary_together() {
    let mut run = Run::new(config());
    let mut completed = Vec::new();
    for n in 0..20 {
        if run.request().purpose == RequestPurpose::Compaction {
            break;
        }
        run.response(json!({"role":"assistant","content":format!("Exchange {n}: {}", "x".repeat(1800)),"tool_calls":[call("exec",json!({"command":format!("command-{n}")}))]}));
        run.complete(Outcome::Tool(
            json!({"exit_code":0,"stdout":format!("result-{n}"),"stderr":"","timed_out":false}),
        ));
        completed.push(n);
    }
    let summary = run.request().clone();
    assert_eq!(summary.purpose, RequestPurpose::Compaction);
    let old = &completed[..completed.len() - 2];
    let recent = &completed[completed.len() - 2..];
    let summary_text = serde_json::to_string(&summary.messages).unwrap();
    for n in old {
        assert!(summary_text.contains(&format!("command-{n}")));
        assert!(summary_text.contains(&format!("result-{n}")));
    }
    for n in recent {
        assert!(!summary_text.contains(&format!("command-{n}")));
        assert!(!summary_text.contains(&format!("result-{n}")));
    }
    run.response(
        json!({"role":"assistant","content":"Older commands and their results were checked."}),
    );
    let next = run.request();
    for (offset, n) in recent.iter().enumerate() {
        let index = next.messages.len() - 4 + offset * 2;
        assert_eq!(
            next.messages[index]["tool_calls"][0]["function"]["arguments"]["command"],
            format!("command-{n}")
        );
        assert_eq!(next.messages[index + 1]["role"], "tool");
        assert!(
            next.messages[index + 1]["content"]
                .as_str()
                .unwrap()
                .contains(&format!("result-{n}"))
        );
    }
}

#[test]
fn impossible_preserved_input_budget_and_tool_batches_stop_without_maintenance() {
    for case in 0..4 {
        let mut config = config();
        if case == 1 {
            config.parameters.limits.generated_tokens = 700.try_into().unwrap();
        }
        let mut run = Run::new(config);
        match case {
            0 => run.response(json!({"role":"assistant","content":"huge".repeat(4000)})),
            1 => {
                for _ in 0..10 {
                    if run.controller.terminal_decision().is_some() {break;}
                    run.response(json!({"role":"assistant","content":"growing".repeat(450)}));
                }
            }
            2 => run.response(json!({"role":"assistant","tool_calls":(0..5).map(|_|call("exec",json!({"command":"never execute"}))).collect::<Vec<_>>()})),
            _ => run.response(json!({"role":"assistant","tool_calls":[call("exec",json!({"command":42}))]})),
        }
        let result = run.finalize();
        assert_eq!(
            result.reason,
            match case {
                1 => StopReason::TokenLimit,
                3 => StopReason::ToolParseRetryLimit,
                _ => StopReason::ContextLimit,
            }
        );
        assert!(!run.dispatches.iter().any(
            |e| matches!(&e.kind,EffectKind::Model(r) if r.purpose==RequestPurpose::Compaction)
        ));
        assert!(
            !run.dispatches
                .iter()
                .any(|e| matches!(e.kind, EffectKind::Tool(_)))
        );
    }
}

#[test]
fn finish_receipt_races_preserve_cancellation_and_failed_cleanup_evidence() {
    for cause in 0..4 {
        let mut run = Run::new(config());
        run.response(finish(json!("done")));
        run.complete(Outcome::WorkspaceCaptured(WorkspaceSnapshot {
            workspace: run.config.initial_workspace.clone(),
            files: Default::default(),
        }));
        let publication = run.dispatch();
        match cause {
            0 => run.input(Input::Cancel),
            1 => {
                run.now = publication.deadline_ms;
                run.input(Input::Tick);
            }
            2 => {
                run.now = run.config.parameters.limits.wall_seconds.get() * 1000;
                run.input(Input::Tick);
            }
            _ => {}
        }
        run.input(Input::PublicationCommitted {
            effect_id: publication.id,
        });
        if cause == 3 {
            run.complete(Outcome::Cleaned(vec!["cleanup failed".into()]));
        }
        let result = run.finalize();
        assert_eq!(
            result.reason,
            match cause {
                0 => StopReason::Interrupted,
                1 => StopReason::InfrastructureFailure,
                2 => StopReason::WallLimit,
                _ => StopReason::AgentFinished,
            }
        );
        assert!(
            result.last_capture.is_some(),
            "late receipt remains evidence"
        );
        if cause == 3 {
            assert!(matches!(result.cleanup, CleanupOutcome::Failed { .. }));
        }
        assert_eq!(result.model_calls, 1);
    }
}

#[test]
fn fitting_summary_still_must_reduce_input_on_the_same_estimation_basis() {
    let mut config = config();
    config.settings.recent_exchanges = 0;
    let mut run = Run::new(config);
    run.complete(Outcome::ModelCompleted {
        response: ModelResponse {
            done: true,
            generated_tokens: Some(10),
            prompt_tokens: Some(12000),
            message: json!({"role":"assistant","content":"tiny"}),
        },
        finish_reason: "stop".into(),
    });
    assert_eq!(run.request().purpose, RequestPurpose::Compaction);
    run.response(json!({"role":"assistant","content":"This summary is larger than the original tiny exchange."}));
    assert_eq!(run.finalize().reason, StopReason::CompactionNoProgress);
    assert!(!run.records.iter().any(|r| r.kind == "compaction_applied"));
}

#[test]
fn invalid_summary_fit_never_dispatches_a_maintenance_request() {
    let mut config = config();
    config.settings.recent_exchanges = 0;
    let mut run = Run::new(config);
    run.response(json!({"role":"assistant","content":"x".repeat(20000)}));
    assert_eq!(run.finalize().reason, StopReason::ContextLimit);
    assert_eq!(run.controller.result().unwrap().model_calls, 1);
    assert!(run.records.iter().any(|r| r.kind == "compaction_rejected"
        && r.data["detail"] == "summary request cannot fit with its allowance and safety margin"));
}

#[test]
fn completion_requires_explicit_normal_termination_metadata() {
    for reason in [None, Some("length"), Some("unknown")] {
        let mut run = Run::new(config());
        let response = ModelResponse {
            done: true,
            generated_tokens: Some(10),
            prompt_tokens: None,
            message: finish(json!("done")),
        };
        let outcome = match reason {
            Some(reason) => Outcome::ModelCompleted {
                response,
                finish_reason: reason.into(),
            },
            None => Outcome::Model(response),
        };
        run.complete(outcome);
        assert_eq!(run.finalize().reason, StopReason::InvalidCompletion);
    }
}
