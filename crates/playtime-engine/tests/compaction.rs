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
    /// A response the server measured: its prompt counts the request's bytes,
    /// so a later estimate can extend a measured prefix.
    fn measured(&mut self, message: Value) {
        let request = self.request();
        let prompt = serde_json::to_vec(&json!({"messages":request.messages,"tools":request.tools}))
            .unwrap()
            .len() as u64;
        self.complete(Outcome::ModelCompleted {
            response: ModelResponse {
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: Some(prompt),
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
        // The conversation arrives as text under the summarizer's own system
        // message, with the task but not the agent's protocol.
        let [system, transcript] = &request.messages[..] else {
            panic!("a summary request is one transcript")
        };
        assert_eq!(
            *system,
            json!({"role":"system","content":compaction::SUMMARY_SYSTEM})
        );
        let transcript = transcript["content"].as_str().unwrap();
        assert!(transcript.starts_with(&format!("The agent's task:\n{}", run.config.prompt)));
        assert!(transcript.ends_with(&format!("\n\n{}", run.config.settings.instructions)));
        assert!(!transcript.contains(&run.config.protocol));
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
        for _ in 0..120 {
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
                assert_eq!(request.context_basis, "calibrated_bytes");
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
                // The plan's work input is the root, any earlier summary, the
                // exchanges and the next request's notice. Each exchange but
                // the run's first is a continue notice and a reply.
                let before = plan["before_messages"].as_array().unwrap();
                let earlier = before[root.len()]["content"]
                    .as_str()
                    .filter(|s| s.starts_with("Derived memory"));
                let start = root.len() + usize::from(earlier.is_some());
                let mut deferred = before[start..before.len() - 1].to_vec();
                deferred.truncate(deferred.len() - 2 * (usize::from(retained) - 1));
                let deferred = compaction::summary_transcript(
                    &run.config.prompt,
                    earlier,
                    &deferred,
                    &run.config.settings.instructions,
                );
                let deferred_bytes = serde_json::to_vec(&json!({"messages":deferred,"tools":[]}))
                    .unwrap()
                    .len() as u64;
                // Estimated at the measured eight bytes per token, assumed
                // half again as dense: three tokens per sixteen bytes.
                let calibration = &plan["calibration"];
                let (rate_tokens, rate_bytes) = (
                    calibration["tokens"].as_u64().unwrap(),
                    calibration["bytes"].as_u64().unwrap(),
                );
                assert!((rate_tokens * 8).abs_diff(rate_bytes) * 100 <= rate_bytes);
                let deferred_tokens = (deferred_bytes * rate_tokens * 3)
                    .div_ceil(rate_bytes * 2)
                    .min(deferred_bytes);
                assert!(
                    deferred_tokens + request.maximum_tokens + request.context_reserve >= context
                );
                assert!(
                    deferred_bytes + request.maximum_tokens + request.context_reserve >= context
                );
                let transcript = serde_json::to_string(&request.messages).unwrap();
                assert!(transcript.contains(&run.config.prompt));
                for n in work_calls - u64::from(retained) + 1..=work_calls {
                    assert!(!transcript.contains(&format!("Exchange {n}:")));
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

impl Run {
    /// A measured response that took `ms` of model time and generated
    /// `generated` tokens, as the controller's pace facts will see it.
    fn measured_slow(&mut self, message: Value, ms: u64, generated: u64) {
        let request = self.request().clone();
        let prompt = serde_json::to_vec(&json!({"messages":request.messages,"tools":request.tools}))
            .unwrap()
            .len() as u64;
        let effect = self.dispatch();
        self.now += ms;
        self.input(Input::Complete {
            effect_id: effect.id,
            outcome: Outcome::ModelCompleted {
                response: ModelResponse {
                    done: true,
                    generated_tokens: Some(generated),
                    prompt_tokens: Some(prompt.div_ceil(8)),
                    message,
                },
                finish_reason: "stop".into(),
            },
        });
    }
}

/// The Framework's second 120B run died here: the summary succeeded, but the
/// replacement's bytes, counted as tokens, left no room for continued work,
/// although its real size was a small fraction of the context.
#[test]
fn calibrated_replacement_keeps_working_where_bytes_would_stop() {
    let mut config = config();
    config.parameters.limits.generated_tokens = 50000.try_into().unwrap();
    config.parameters.limits.log_bytes = 8000000.try_into().unwrap();
    let context = config.parameters.limits.context_tokens.get();
    let threshold = (context * u64::from(config.settings.trigger_percent)).div_ceil(100);
    let allowance = config.parameters.limits.tokens_per_call.get();
    let mut run = Run::new(config);
    let reserve = run.request().context_reserve;
    // Sparse prose measured at eight bytes per token; two retained exchanges
    // alone outweigh the room the byte basis would leave.
    let mut calls = 0;
    while run.request().purpose == RequestPurpose::Work {
        calls += 1;
        assert!(calls < 60, "no compaction was planned");
        run.measured_slow(
            json!({"role":"assistant","content":format!("Exchange {calls}: {}", "x".repeat(6000))}),
            1,
            10,
        );
    }
    run.measured_slow(
        json!({"role":"assistant","content":"Summary 1: checked results, next action."}),
        1,
        10,
    );
    let applied = run
        .records
        .iter()
        .find(|r| r.kind == "compaction_applied")
        .expect("compaction applied")
        .data
        .clone();
    assert_eq!(applied["estimate_basis"], "calibrated_bytes");
    let bytes = applied["input_bytes"].as_u64().unwrap();
    let tokens = applied["input_bound"].as_u64().unwrap();
    assert!(
        bytes + allowance >= threshold,
        "bytes {bytes} would have failed the trigger"
    );
    assert!(tokens + allowance < threshold && tokens + allowance + reserve < context);
    let expected = (bytes * 3).div_ceil(16);
    assert!(
        tokens.abs_diff(expected) * 100 <= expected,
        "{tokens} vs {expected}"
    );
    assert!(!run.records.iter().any(|r| r.kind == "compaction_rejected"));
    assert_eq!(run.controller.terminal_decision(), None);
    assert_eq!(run.request().purpose, RequestPurpose::Work);
    assert_eq!(run.request().context_basis, "calibrated_bytes");
    run.response(finish(json!("done after a calibrated compaction")));
    assert_eq!(run.finalize().reason, StopReason::AgentFinished);
}

/// A summary planned with less wall time than it needs is cut by the deadline
/// and produces nothing, so a run whose work still fits keeps working instead.
#[test]
fn compaction_is_deferred_when_the_summary_cannot_finish_before_the_deadline() {
    for late in [false, true] {
        let mut config = config();
        config.parameters.limits.generated_tokens = 50000.try_into().unwrap();
        config.parameters.limits.log_bytes = 8000000.try_into().unwrap();
        if late {
            // A 100 s run. Every call below takes 2 s for 20 tokens, so by the
            // time the summary check fires, a summary of the transcript at ten
            // times that pace, plus its whole cap, no longer fits the rest.
            config.parameters.limits.wall_seconds = 100.try_into().unwrap();
        }
        let mut run = Run::new(config);
        let mut calls = 0;
        let deferred = loop {
            calls += 1;
            assert!(calls < 60, "neither a compaction nor a deferral happened");
            let deferred = run
                .records
                .iter()
                .find(|r| r.kind == "compaction_deferred")
                .map(|r| r.data.clone());
            if deferred.is_some() || run.request().purpose == RequestPurpose::Compaction {
                break deferred;
            }
            run.measured_slow(
                json!({"role":"assistant","content":format!("Exchange {calls}: {}", "x".repeat(3000))}),
                2000,
                20,
            );
        };
        assert_eq!(run.controller.terminal_decision(), None);
        if late {
            let deferred = deferred.expect("late compaction is deferred");
            assert_eq!(run.request().purpose, RequestPurpose::Work);
            let remaining = deferred["remaining_ms"].as_u64().unwrap();
            let expected = deferred["expected_summary_ms"].as_u64().unwrap();
            assert!(remaining < 100_000 && expected > remaining, "{deferred}");
            assert_eq!(deferred["summary_estimate_basis"], "calibrated_bytes");
            assert_eq!(deferred["ordinal"], 1);
            assert!(deferred["model_ms"].as_u64().unwrap() >= 10_000);
            assert!(!run.records.iter().any(|r| r.kind == "compaction_planned"));
        } else {
            assert!(deferred.is_none(), "{deferred:?}");
            assert_eq!(run.request().purpose, RequestPurpose::Compaction);
            run.measured_slow(
                json!({"role":"assistant","content":"Summary 1: checked results, next action."}),
                2000,
                20,
            );
            assert_eq!(run.request().purpose, RequestPurpose::Work);
        }
        run.response(finish(json!("done")));
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
        let summary = run.request().clone();
        run.complete(outcome);
        if case == 6 {
            // A summary's tool calls never execute; the compaction is planned
            // again instead of stopping.
            assert!(
                !run.dispatches
                    .iter()
                    .any(|e| matches!(e.kind, EffectKind::Tool(_)))
            );
            assert_eq!(run.request().purpose, RequestPurpose::Compaction);
            assert_eq!(run.request().messages, summary.messages);
            assert!(!run.records.iter().any(|r| r.kind == "compaction_applied"));
            continue;
        }
        let result = run.finalize();
        assert_eq!(
            result.reason,
            match case {
                0..=2 => StopReason::CompactionFailed,
                3 => StopReason::CompactionNoProgress,
                4 => StopReason::MissingTokenUsage,
                5 => StopReason::IncompleteModelResponse,
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
        // ContextWindow adds the appended continue notice's bytes.
        let appended = serde_json::to_vec(&json!([continue_notice()]))
            .unwrap()
            .len() as u64;
        for delta in [-1_i64, 0, 1] {
            let mut run = Run::new(config.clone());
            let projected = boundary.checked_add_signed(delta).unwrap();
            let prompt = projected - cap - 10 - appended;
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
    for case in 0..3 {
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
            _ => run.response(json!({"role":"assistant","tool_calls":(0..5).map(|_|call("exec",json!({"command":"never execute"}))).collect::<Vec<_>>()})),
        }
        let result = run.finalize();
        assert_eq!(
            result.reason,
            match case {
                1 => StopReason::TokenLimit,
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
fn invalid_tool_batches_return_errors_without_running_any_call() {
    let mut run = Run::new(config());
    run.response(json!({"role":"assistant","tool_calls":[
        call("exec",json!({"command":"never execute"})),
        call("write_file",json!({"path":"notes.txt","patch":"@@ -1 +1 @@"})),
        call("python",json!({"code":"print(1)"})),
    ]}));
    let request = run.request();
    assert_eq!(request.purpose, RequestPurpose::Work);
    let results: Vec<Value> = request
        .messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(
        results,
        [
            json!({"error":"Not run because another call in this response was invalid"}),
            json!({"error":"Unexpected or missing tool arguments"}),
            json!({"error":"Tool is unavailable for this request purpose"}),
        ]
    );
    assert!(
        !run.dispatches
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Tool(_)))
    );
    assert!(
        !run.records
            .iter()
            .any(|r| r.kind == "tool_dispatch" || r.kind == "tool_batch_rejected")
    );
    run.response(
        json!({"role":"assistant","tool_calls":[call("exec",json!({"command":"echo fixed"}))]}),
    );
    run.complete(Outcome::Tool(
        json!({"exit_code":0,"stdout":"fixed\n","stderr":"","timed_out":false}),
    ));
    run.response(finish(json!("Corrected the call and checked its output.")));
    assert_eq!(run.finalize().reason, StopReason::AgentFinished);
    assert_eq!(
        run.dispatches
            .iter()
            .filter(|e| matches!(e.kind, EffectKind::Tool(_)))
            .count(),
        1
    );
}

fn notice() -> Value {
    json!({"role":"user","content":"The server rejected your last response because a tool call could not be parsed. None of that response's tools ran. Try again with valid JSON object arguments."})
}
fn continue_notice() -> Value {
    json!({"role":"user","content":"Your last response did not call a tool, so the run continues. Continue independently, or call finish_run alone if the task is finished; ordinary prose does not finish the run."})
}
fn parse_failure() -> Outcome {
    Outcome::ModelError(ModelFailure::Parse("error parsing tool call".into()))
}

#[test]
fn tool_free_responses_are_followed_by_a_continue_notice() {
    let mut run = Run::new(config());
    let empty = json!({"role":"assistant","content":""});
    run.response(empty.clone());
    let request = run.request().clone();
    assert_eq!(
        request.messages[request.messages.len() - 2..],
        [empty, continue_notice()]
    );
    run.response(
        json!({"role":"assistant","tool_calls":[call("exec",json!({"command":"echo ok"}))]}),
    );
    run.complete(Outcome::Tool(
        json!({"exit_code":0,"stdout":"ok\n","stderr":"","timed_out":false}),
    ));
    let request = run.request();
    assert_eq!(request.messages.last().unwrap()["role"], "tool");
    let notices = request.messages.iter().filter(|m| **m == continue_notice());
    assert_eq!(notices.count(), 1);
}

#[test]
fn work_parse_failures_retry_with_a_notice_until_the_consecutive_limit() {
    let mut run = Run::new(config());
    let root = run.request().messages.clone();
    run.complete(parse_failure());
    let retry = run.request().clone();
    assert_eq!(retry.purpose, RequestPurpose::Work);
    assert_eq!(retry.messages, [root.clone(), vec![notice()]].concat());
    // The notice opens the exchange of the response that follows it.
    let prose = json!({"role":"assistant","content":"Retrying with valid arguments."});
    run.response(prose.clone());
    let next = run.request();
    assert_eq!(next.messages[..retry.messages.len()], retry.messages[..]);
    assert_eq!(
        next.messages[retry.messages.len()..],
        [prose, continue_notice()]
    );
    for n in 1..=compaction::TOOL_PARSE_RETRIES + 1 {
        run.complete(parse_failure());
        if n <= compaction::TOOL_PARSE_RETRIES {
            let notices = run.request().messages.iter().filter(|m| **m == notice());
            assert_eq!(notices.count() as u32, n + 1);
        }
    }
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::ToolParseRetryLimit);
    assert_eq!(result.usage.unknown_requests, 7);
    assert_eq!(result.usage.unknown_reserved_tokens, 7 * 512);
    assert!(!result.usage.usage_complete);
    let errors: Vec<_> = run
        .records
        .iter()
        .filter(|r| r.kind == "model_error")
        .map(|r| {
            (
                r.data["recovery_allowed"].clone(),
                r.data["consecutive_errors"].clone(),
            )
        })
        .collect();
    let expected: Vec<_> = [1, 1, 2, 3, 4, 5, 6]
        .into_iter()
        .map(|streak| (json!(streak <= 5), json!(streak)))
        .collect();
    assert_eq!(errors, expected);
}

#[test]
fn a_retried_exchange_fits_its_tool_results_like_any_other() {
    let results = |retried: bool| {
        let mut run = Run::new(config());
        if retried {
            run.complete(parse_failure());
        }
        run.response(json!({"role":"assistant","tool_calls":[call("exec",json!({"command":"first"})),call("exec",json!({"command":"second"}))]}));
        for _ in 0..2 {
            run.complete(Outcome::Tool(
                json!({"exit_code":0,"stdout":"é".repeat(10000),"stderr":"","timed_out":false}),
            ));
        }
        let request = run.request();
        request
            .messages
            .iter()
            .filter(|m| m["role"] == "tool")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(results(true), results(false));
}

/// Alternates parse failures and short responses until compaction is planned
/// while a retry notice waits for its exchange.
fn compaction_with_pending_notice() -> Run {
    let mut config = config();
    config.parameters.limits.generated_tokens = 100_000.try_into().unwrap();
    let mut run = Run::new(config);
    for n in 0..200 {
        run.complete(parse_failure());
        if run.request().purpose == RequestPurpose::Compaction {
            return run;
        }
        run.response(
            json!({"role":"assistant","content":format!("Exchange {n}: {}", "x".repeat(150))}),
        );
        if run.request().purpose == RequestPurpose::Compaction {
            run.response(json!({"role":"assistant","content":format!("Summary {n}.")}));
        }
    }
    panic!("no compaction while a notice was pending");
}

#[test]
fn a_pending_retry_notice_stays_out_of_the_summary_and_opens_the_next_exchange() {
    let mut run = compaction_with_pending_notice();
    let summary = run.request().clone();
    let transcript = summary.messages[1]["content"].as_str().unwrap();
    // The summary ends with the agent's last older reply, not the notice.
    let last = transcript.rsplit("\n\n[").next().unwrap();
    assert!(last.starts_with("agent]\n"), "{last}");
    let plan = run
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "compaction_planned")
        .unwrap()
        .data
        .clone();
    assert_eq!(
        plan["before_messages"].as_array().unwrap().last(),
        Some(&notice())
    );
    run.response(json!({"role":"assistant","content":"Summary: checked results."}));
    let applied = &run
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "compaction_applied")
        .unwrap()
        .data;
    assert_eq!(applied["ordinal"], plan["ordinal"]);
    assert_eq!(
        applied["messages"].as_array().unwrap().last(),
        Some(&notice())
    );
    let work = run.request().clone();
    assert_eq!(work.purpose, RequestPurpose::Work);
    assert_eq!(work.messages.last(), Some(&notice()));
    run.response(json!({"role":"assistant","content":"Retried after compaction."}));
    assert_eq!(
        run.request().messages[..work.messages.len()],
        work.messages[..]
    );

    // A retried summary leaves the pending notice where it was and adds none,
    // so the next work request is the same as without the retry.
    let mut run = compaction_with_pending_notice();
    let summary = run.request().clone();
    run.complete(parse_failure());
    assert_eq!(run.request().messages, summary.messages);
    run.response(json!({"role":"assistant","content":"Summary: checked results."}));
    assert_eq!(run.request().messages, work.messages);
}

#[test]
fn summary_parse_failures_plan_the_same_compaction_again_until_the_consecutive_limit() {
    let plans = |run: &Run| {
        run.records
            .iter()
            .filter(|r| r.kind == "compaction_planned")
            .map(|r| r.data.clone())
            .collect::<Vec<_>>()
    };
    let mut run = Run::new(config());
    run.until_summary();
    let summary = run.request().clone();
    run.complete(parse_failure());
    let retry = run.request().clone();
    assert_eq!(retry.purpose, RequestPurpose::Compaction);
    assert_eq!(retry.messages, summary.messages);
    let [first, again] = &plans(&run)[..] else {
        panic!("the retry plans the compaction once more")
    };
    assert_eq!(again["ordinal"], first["ordinal"]);
    assert_eq!(again["before_messages"], first["before_messages"]);
    run.response(json!({"role":"assistant","content":"Summary: checked results."}));
    let applied = &run
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "compaction_applied")
        .unwrap()
        .data;
    assert_eq!(applied["ordinal"], first["ordinal"]);
    let work = run.request();
    assert_eq!(work.purpose, RequestPurpose::Work);
    assert!(!work.messages.contains(&notice()));

    // Summaries stop after as many consecutive failures as work requests.
    let mut run = Run::new(config());
    run.until_summary();
    for _ in 0..=compaction::TOOL_PARSE_RETRIES {
        assert_eq!(run.request().purpose, RequestPurpose::Compaction);
        run.complete(parse_failure());
    }
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::ToolParseRetryLimit);
    assert_eq!(result.usage.unknown_requests, 6);
    let allowed: Vec<_> = run
        .records
        .iter()
        .filter(|r| r.kind == "model_error")
        .map(|r| r.data["recovery_allowed"].clone())
        .collect();
    assert_eq!(allowed, [vec![json!(true); 5], vec![json!(false)]].concat());
    assert_eq!(plans(&run).len(), 6);
    assert!(
        plans(&run)
            .iter()
            .all(|plan| plan["ordinal"] == plans(&run)[0]["ordinal"])
    );
}

#[test]
fn summary_requests_carry_the_older_conversation_as_text_without_thinking_or_tools() {
    let mut run = Run::new(config());
    // A prose reply is followed by a continue notice, which opens the next
    // exchange.
    run.response(json!({"role":"assistant","thinking":"hidden plan","content":"Looking around."}));
    for n in 1..20 {
        if run.request().purpose == RequestPurpose::Compaction {
            break;
        }
        run.response(
            json!({"role":"assistant","thinking":format!("hidden plan {n} {}", "y".repeat(1500)),
            "tool_calls":[call("exec",json!({"command":format!("echo \"step {n}\"")}))]}),
        );
        run.complete(Outcome::Tool(
            json!({"exit_code":0,"stdout":format!("step {n}\n"),"stderr":"","timed_out":false}),
        ));
    }
    let request = run.request().clone();
    assert_eq!(request.purpose, RequestPurpose::Compaction);
    assert!(request.tools.is_empty());
    let [system, transcript] = &request.messages[..] else {
        panic!("a summary request is one transcript")
    };
    assert_eq!(
        *system,
        json!({"role":"system","content":compaction::SUMMARY_SYSTEM})
    );
    assert_eq!(transcript.as_object().unwrap().len(), 2);
    let text = transcript["content"].as_str().unwrap();
    assert!(!text.contains("hidden plan"));
    assert!(!text.contains(&run.config.protocol) && !text.contains(&run.config.inventory));
    let plan = &run
        .records
        .iter()
        .find(|r| r.kind == "compaction_planned")
        .unwrap()
        .data;
    let before = plan["before_messages"].as_array().unwrap();
    let result = before.iter().find(|m| m["role"] == "tool").unwrap();
    let opening = format!(
        "The agent's task:\n{}\n\nThe older conversation, oldest first:\n\n[agent]\nLooking around.\n\n[controller]\n{}\n\n[agent calls exec]\n{{\"command\":\"echo \\\"step 1\\\"\"}}\n\n[exec result]\n{}\n\n[agent calls exec]\n",
        run.config.prompt,
        continue_notice()["content"].as_str().unwrap(),
        result["content"].as_str().unwrap()
    );
    assert!(text.starts_with(&opening), "{text}");

    // The next summary carries the previous one in the message that replaced
    // it.
    run.response(json!({"role":"assistant","content":"Summary one."}));
    for n in 20..40 {
        if run.request().purpose == RequestPurpose::Compaction {
            break;
        }
        run.response(
            json!({"role":"assistant","content":format!("Exchange {n}: {}", "x".repeat(1500))}),
        );
    }
    let request = run.request();
    assert_eq!(request.purpose, RequestPurpose::Compaction);
    let text = request.messages[1]["content"].as_str().unwrap();
    assert!(text.starts_with(&format!(
        "The agent's task:\n{}\n\nDerived memory from earlier exchanges; retain uncertainty and verify claims:\nSummary one.\n\nThe older conversation, oldest first:\n\n",
        run.config.prompt
    )));
    assert!(text.ends_with(&format!("\n\n{}", run.config.settings.instructions)));
}

#[test]
fn without_retained_exchanges_a_summary_extends_the_measured_work_prefix() {
    let mut config = config();
    config.settings.recent_exchanges = 0;
    let mut run = Run::new(config);
    let work = run.request().clone();
    let reply = json!({"role":"assistant","thinking":"plan","content":"tiny"});
    run.complete(Outcome::ModelCompleted {
        response: ModelResponse {
            done: true,
            generated_tokens: Some(10),
            prompt_tokens: Some(12000),
            message: reply.clone(),
        },
        finish_reason: "stop".into(),
    });
    let summary = run.request();
    assert_eq!(summary.purpose, RequestPurpose::Compaction);
    let instructions = json!({"role":"user","content":run.config.settings.instructions});
    assert_eq!(
        summary.messages,
        [work.messages, vec![reply, instructions]].concat()
    );
    let plan = &run
        .records
        .iter()
        .find(|r| r.kind == "compaction_planned")
        .unwrap()
        .data;
    assert_eq!(
        plan["summary_estimate_basis"],
        "server_usage_plus_new_bytes"
    );
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

fn continue_to_limit_notice() -> Value {
    json!({"role":"user","content":"Your last response did not call a tool, so the run continues. Continue independently; the run ends when its time or token budget runs out."})
}

#[test]
fn without_finish_run_a_run_continues_until_its_budget_runs_out() {
    let mut config = config();
    config.settings.allow_finish_run = false;
    config.parameters.limits.generated_tokens = 40.try_into().unwrap();
    let mut run = Run::new(config);
    let names: Vec<&str> = run
        .request()
        .tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["exec", "read_file", "write_file"]);
    // finish_run is then an ordinary unavailable tool. Even beside a valid
    // call it doesn't stop the run, and nothing runs.
    run.response(json!({"role":"assistant","tool_calls":[
        call("exec",json!({"command":"never execute"})),
        call("finish_run",json!({"summary":"Done."})),
    ]}));
    let results: Vec<Value> = run
        .request()
        .messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(
        results,
        [
            json!({"error":"Not run because another call in this response was invalid"}),
            json!({"error":"Tool is unavailable for this request purpose"}),
        ]
    );
    assert!(
        !run.dispatches
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Tool(_)))
    );
    assert!(
        !run.records
            .iter()
            .any(|r| r.kind == "completion_declared" || r.kind == "tool_batch_rejected")
    );
    let prose = json!({"role":"assistant","content":"I am done."});
    run.response(prose.clone());
    let messages = run.request().messages.clone();
    assert_eq!(
        messages[messages.len() - 2..],
        [prose, continue_to_limit_notice()]
    );
    run.response(json!({"role":"assistant","content":"Still done."}));
    run.response(json!({"role":"assistant","content":"Nothing more."}));
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::TokenLimit);
    assert_eq!(result.usage.known_generated_tokens, 40);
    assert!(run.records.iter().any(|r| r.kind == "workspace_captured"));
}

fn tool_call_summary() -> Value {
    json!({"role":"assistant","content":"","tool_calls":[{"function":{"name":"exec","arguments":{"command":"ls -lh random_walk.png"}}}]})
}

#[test]
fn tool_call_summaries_plan_the_same_compaction_again_until_the_consecutive_limit() {
    let plans = |run: &Run| {
        run.records
            .iter()
            .filter(|r| r.kind == "compaction_planned")
            .map(|r| r.data.clone())
            .collect::<Vec<_>>()
    };
    let errors = |run: &Run| {
        run.records
            .iter()
            .filter(|r| r.kind == "model_error")
            .map(|r| r.data.clone())
            .collect::<Vec<_>>()
    };
    let mut run = Run::new(config());
    for n in 0..20 {
        if run.request().purpose == RequestPurpose::Compaction {
            break;
        }
        run.measured(
            json!({"role":"assistant","content":format!("Exchange {n}: {}", "x".repeat(3000))}),
        );
    }
    let summary = run.request().clone();
    assert_eq!(summary.purpose, RequestPurpose::Compaction);
    run.measured(tool_call_summary());
    // The settled attempt is charged its known usage, not an allowance, and
    // the failure is not a tool parse error.
    let error = errors(&run).pop().unwrap();
    assert_eq!(error["error"]["kind"], "summary_tools");
    assert_eq!(error["recovery_allowed"], true);
    assert_eq!(error["token_budget_charge"], 0);
    assert_eq!(error["consecutive_errors"], 1);
    assert_eq!(error["tool_parse_errors"], 0);
    assert!(
        run.records
            .iter()
            .filter(|r| r.kind == "request_settled")
            .all(|r| r.data["usage"]["status"] == "known")
    );
    let retry = run.request().clone();
    assert_eq!(retry.purpose, RequestPurpose::Compaction);
    assert_eq!(retry.messages, summary.messages);
    let [first, again] = &plans(&run)[..] else {
        panic!("the retry plans the compaction once more")
    };
    assert_eq!(again["ordinal"], first["ordinal"]);
    assert_eq!(again["before_messages"], first["before_messages"]);
    // The attempt leaves the measured context alone, so the retry is planned
    // from the basis the first attempt had.
    assert_eq!(first["estimate_basis"], "server_usage_plus_new_bytes");
    for field in [
        "estimate_basis",
        "input_bound",
        "summary_estimate_basis",
        "summary_input_bound",
    ] {
        assert_eq!(again[field], first[field], "{field}");
    }
    run.response(json!({"role":"assistant","content":"Summary: checked results."}));
    let applied = &run
        .records
        .iter()
        .rev()
        .find(|r| r.kind == "compaction_applied")
        .unwrap()
        .data;
    assert_eq!(applied["ordinal"], first["ordinal"]);
    let work = run.request();
    assert_eq!(work.purpose, RequestPurpose::Work);
    assert!(!work.messages.contains(&notice()));
    // An accepted summary ends the streak: a work request's parse failure
    // right after it, and the next compaction's failure, each open a new one.
    run.complete(parse_failure());
    let error = errors(&run).pop().unwrap();
    assert_eq!(error["error"]["kind"], "parse");
    assert_eq!(error["consecutive_errors"], 1);
    assert_eq!(error["recovery_allowed"], true);
    run.until_summary();
    run.response(tool_call_summary());
    let error = errors(&run).pop().unwrap();
    assert_eq!(error["consecutive_errors"], 1);
    assert_eq!(error["recovery_allowed"], true);
    assert_eq!(run.request().purpose, RequestPurpose::Compaction);

    // Unparseable and tool-call summaries share one streak, and a sixth
    // consecutive failure stops the run with the last failure's reason.
    let mut run = Run::new(config());
    run.until_summary();
    for n in 0..=compaction::TOOL_PARSE_RETRIES {
        assert_eq!(run.request().purpose, RequestPurpose::Compaction);
        if n % 2 == 0 {
            run.complete(parse_failure());
        } else {
            run.response(tool_call_summary());
        }
    }
    let result = run.finalize();
    assert_eq!(result.reason, StopReason::CompactionFailed);
    assert_eq!(result.usage.unknown_requests, 3);
    let errors = errors(&run);
    assert_eq!(
        errors
            .iter()
            .map(|e| e["recovery_allowed"].clone())
            .collect::<Vec<_>>(),
        [vec![json!(true); 5], vec![json!(false)]].concat()
    );
    assert_eq!(
        errors
            .iter()
            .map(|e| e["consecutive_errors"].clone())
            .collect::<Vec<_>>(),
        (1..=6).map(|n| json!(n)).collect::<Vec<_>>()
    );
    assert_eq!(
        errors
            .iter()
            .map(|e| e["error"]["kind"].clone())
            .collect::<Vec<_>>(),
        ["parse", "summary_tools"].repeat(3)
    );
    assert_eq!(plans(&run).len(), 6);
    assert!(
        plans(&run)
            .iter()
            .all(|plan| plan["ordinal"] == plans(&run)[0]["ordinal"])
    );
}
