use playtime_core::{RequestPurpose, RunId, RunMode, Sha256Digest, StopReason};
use playtime_engine::{
    chat::*,
    compaction::CompactionSettings,
    simulation::{Scenario, SequentialIds},
    *,
};
use serde_json::{Value, json};

fn config() -> ConversationConfig {
    let fixture: Scenario =
        serde_json::from_str(include_str!("fixtures/two-session.json")).unwrap();
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = 16384.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 10000.try_into().unwrap();
    parameters.limits.output_bytes = 2048.try_into().unwrap();
    ConversationConfig {
        schema_version: 1,
        mode: RunMode::Simulation,
        tool_policy: ToolPolicy::Simulation,
        model: Sha256Digest::new("1".repeat(64)).unwrap(),
        workspace: Sha256Digest::new("2".repeat(64)).unwrap(),
        instructions: "Keep the API stable. Answer normally when done.".into(),
        inventory: "Owned workshop, no network.".into(),
        parameters,
        compaction: CompactionSettings {
            allow_finish_run: false,
            summary_tokens: 256.try_into().unwrap(),
            ..Default::default()
        },
    }
}

struct Chat {
    controller: Controller<SequentialIds>,
    records: Vec<Record>,
    now: u64,
}
impl Chat {
    fn new(
        config: ConversationConfig,
        previous: Option<ConversationCheckpoint>,
        input: &str,
        now: u64,
    ) -> Self {
        let run_id = RunId::new(format!("turn-{now}")).unwrap();
        let controller = Controller::chat(
            config,
            ConversationTurn {
                input: input.into(),
                previous,
            },
            run_id,
            SequentialIds,
            now,
        )
        .unwrap();
        let mut chat = Self {
            controller,
            records: Vec::new(),
            now,
        };
        chat.collect();
        chat.complete(Outcome::WorkspaceStarted);
        chat
    }
    fn collect(&mut self) {
        self.records.extend(self.controller.take_records());
        self.controller.take_control_updates();
    }
    fn input(&mut self, input: Input) {
        self.controller.advance(self.now, input).unwrap();
        self.collect();
    }
    fn complete(&mut self, outcome: Outcome) {
        let effect_id = self.controller.pending().unwrap().id;
        self.input(Input::Dispatched { effect_id });
        self.now += 1;
        self.input(Input::Complete { effect_id, outcome });
    }
    fn request(&self) -> &ModelRequest {
        let EffectKind::Model(r) = &self.controller.pending().unwrap().kind else {
            panic!("expected model")
        };
        r
    }
    fn response(&mut self, message: Value, finish: &str) {
        self.complete(Outcome::ModelCompleted {
            response: ModelResponse {
                message,
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: None,
            },
            finish_reason: finish.into(),
        });
    }
    fn final_reply(&mut self, content: &str) -> ConversationCheckpoint {
        self.response(json!({"role":"assistant", "content":content}), "stop");
        assert!(self.controller.conversation_checkpoint().is_none());
        assert!(matches!(
            self.controller.pending().unwrap().kind,
            EffectKind::Cleanup
        ));
        self.complete(Outcome::Cleaned(Vec::new()));
        self.controller.conversation_checkpoint().unwrap()
    }
}

#[test]
fn final_reply_cleans_up_then_follow_up_gets_history_and_fresh_limits() {
    let config = config();
    let mut first = Chat::new(config.clone(), None, "Fix the parser.", 0);
    let first_id = first.request().request_id.clone();
    first.response(json!({"role":"assistant", "tool_calls":[{"function":{"name":"exec", "arguments":{"command":"test-parser"}}}]}), "tool_calls");
    first.complete(Outcome::Tool(
        json!({"exit_code":1,"stderr":"missing delimiter"}),
    ));
    let saved = first.final_reply("The failing parser test needs a delimiter fix.");
    assert_eq!(saved.history.exchanges.len(), 2);
    assert_eq!(
        saved.history.exchanges[0].failures.records[0]["result"]["exit_code"],
        1
    );
    config.validate_checkpoint(&saved).unwrap();
    let saved: ConversationCheckpoint =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    let next_sequence = saved.next_sequence;
    let next_request = saved.next_request;
    let mut next = Chat::new(
        config.clone(),
        Some(saved),
        "Fix that while preserving error messages.",
        1_000_000,
    );
    assert_eq!(next.records[0].sequence, next_sequence);
    assert_ne!(next.request().request_id, first_id);
    assert_eq!(
        next.request().request_id,
        SequentialIds.request(next_request)
    );
    assert_eq!(next.controller.usage().budget_charged_tokens(), 0);
    assert!(next.controller.pending().unwrap().deadline_ms > 1_000_000);
    let messages = &next.request().messages;
    assert_eq!(messages[1]["content"], "Fix the parser.");
    assert_eq!(
        messages.last().unwrap()["content"],
        "Fix that while preserving error messages."
    );
    assert!(messages.iter().any(
        |m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("missing delimiter")
    ));
    let saved = next.final_reply("Fixed; the parser tests pass.");
    config.validate_checkpoint(&saved).unwrap();
    assert!(next.records.iter().all(|r| !matches!(
        r.kind.as_str(),
        "controller_checkpoint" | "workspace_captured"
    )));
    assert!(next.controller.result().unwrap().last_capture.is_none());
}

#[test]
fn a_follow_up_can_compact_the_saved_prefix_without_summarizing_new_input() {
    let mut config = config();
    config.compaction.trigger_percent = 60;
    config.compaction.recent_exchanges = 0;
    let mut first = Chat::new(config.clone(), None, "Investigate the parser.", 0);
    let saved = first.final_reply(&"checked details ".repeat(600));
    let mut next = Chat::new(config.clone(), Some(saved), "Now fix the bug.", 1_000_000);
    assert_eq!(next.request().purpose, RequestPurpose::Compaction);
    assert!(
        !serde_json::to_string(&next.request().messages)
            .unwrap()
            .contains("Now fix the bug.")
    );
    next.response(
        json!({"role":"assistant", "content":"The parser rejects the missing delimiter."}),
        "stop",
    );
    assert_eq!(next.request().purpose, RequestPurpose::Work);
    assert_eq!(
        next.request().messages.last().unwrap()["content"],
        "Now fix the bug."
    );
    let saved = next.final_reply("Fixed and checked.");
    assert_eq!(saved.history.compactions, 1);
    assert!(saved.history.summary.is_some());
    config.validate_checkpoint(&saved).unwrap();
}

#[test]
fn earlier_follow_up_constraints_are_included_when_they_eventually_compact() {
    let mut config = config();
    config.parameters.limits.context_tokens = 32768.try_into().unwrap();
    config.compaction.recent_exchanges = 1;
    let mut first = Chat::new(config.clone(), None, "Investigate.", 0);
    let saved = first.final_reply(&"a".repeat(5000));
    let mut second = Chat::new(
        config.clone(),
        Some(saved),
        "Keep all error strings unchanged.",
        100_000,
    );
    assert_eq!(second.request().purpose, RequestPurpose::Work);
    let saved = second.final_reply(&"b".repeat(5000));
    let mut third = Chat::new(config.clone(), Some(saved), "Check the callers.", 200_000);
    assert_eq!(third.request().purpose, RequestPurpose::Work);
    let saved = third.final_reply(&"c".repeat(14000));
    let mut fourth = Chat::new(config.clone(), Some(saved), "Now finish the fix.", 300_000);
    assert_eq!(fourth.request().purpose, RequestPurpose::Compaction);
    let transcript = fourth.request().messages[1]["content"].as_str().unwrap();
    assert!(transcript.contains("[user or controller notice]\nKeep all error strings unchanged."));
    assert!(!transcript.contains("Now finish the fix."));
    fourth.response(
        json!({"role":"assistant", "content":"Preserve all existing error strings."}),
        "stop",
    );
    assert_eq!(fourth.request().purpose, RequestPurpose::Work);
    config
        .validate_checkpoint(&fourth.final_reply("Fixed; error strings are unchanged."))
        .unwrap();
}

#[test]
fn partial_or_empty_replies_and_uncertain_cleanup_are_not_resumable() {
    for (message, finish, reason) in [
        (
            json!({"role":"assistant", "content":"partial"}),
            "length",
            StopReason::IncompleteModelResponse,
        ),
        (
            json!({"role":"assistant", "content":""}),
            "stop",
            StopReason::InvalidCompletion,
        ),
        (
            json!({"role":"assistant", "tool_calls":[{"function":{"name":"exec", "arguments":{"command":"must-not-run"}}}]}),
            "length",
            StopReason::IncompleteModelResponse,
        ),
    ] {
        let mut chat = Chat::new(config(), None, "Fix it.", 0);
        chat.response(message, finish);
        assert_eq!(chat.controller.terminal_decision(), Some(reason));
        assert!(matches!(
            chat.controller.pending().unwrap().kind,
            EffectKind::Cleanup
        ));
        chat.complete(Outcome::Cleaned(vec![]));
        assert!(chat.controller.conversation_checkpoint().is_none());
        assert_eq!(chat.controller.usage().known_generated_tokens(), 10);
    }
    let mut chat = Chat::new(config(), None, "Fix it.", 0);
    chat.response(json!({"role":"assistant", "content":"Done."}), "stop");
    chat.complete(Outcome::Cleaned(vec!["process still running".into()]));
    assert!(chat.controller.conversation_checkpoint().is_none());
}

#[test]
fn interruption_keeps_unknown_usage_and_never_exports_pending_tools() {
    let mut chat = Chat::new(config(), None, "Fix it.", 0);
    let effect_id = chat.controller.pending().unwrap().id;
    chat.input(Input::Dispatched { effect_id });
    chat.input(Input::Cancel);
    chat.complete(Outcome::Cleaned(vec![]));
    assert_eq!(
        chat.controller.result().unwrap().reason,
        StopReason::Interrupted
    );
    assert_eq!(chat.controller.usage().unknown_requests(), 1);
    assert!(chat.controller.conversation_checkpoint().is_none());
}

#[test]
fn resume_rejects_configuration_changes_and_incomplete_saved_exchanges() {
    let config = config();
    let mut chat = Chat::new(config.clone(), None, "Fix it.", 0);
    let saved = chat.final_reply("Done.");
    let mut changed = config.clone();
    changed.model = Sha256Digest::new("3".repeat(64)).unwrap();
    assert!(changed.validate_checkpoint(&saved).is_err());
    changed = config.clone();
    changed.parameters.limits.generated_tokens = 4000.try_into().unwrap();
    assert!(changed.validate_checkpoint(&saved).is_err());
    changed = config.clone();
    changed.mode = RunMode::LiveSmoke;
    assert!(changed.validate().is_err());
    changed.tool_policy = ToolPolicy::ConfirmedExternal {
        return_overhead_ms: 100,
    };
    changed.validate().unwrap();
    assert!(changed.validate_checkpoint(&saved).is_err());
    changed.tool_policy = ToolPolicy::ConfirmedExternal {
        return_overhead_ms: 0,
    };
    assert!(changed.validate().is_err());
    let mut invalid = saved.clone();
    invalid.history.exchanges[0].messages[0] = json!({"role":"assistant","tool_calls":[{"function":{"name":"exec","arguments":{"command":"uncertain"}}}]});
    assert!(config.validate_checkpoint(&invalid).is_err());
    invalid = saved;
    invalid.schema_version = 2;
    assert!(config.validate_checkpoint(&invalid).is_err());
}
