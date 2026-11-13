use playtime_core::{CommonLimits, StopReason};
use playtime_engine::{
    compaction::{CompactionSettings, summary_transcript},
    conversation::{Budget, ContextPolicy, ContextWindow, History, Plan},
};
use serde_json::{Value, json};

/// A standalone caller's history, with no experiment configuration or archive.
struct Chat {
    root: Vec<Value>,
    exchanges: Vec<Vec<Value>>,
    summary: Option<String>,
    follow_up: String,
}

impl History for Chat {
    fn complete_exchanges(&self) -> usize {
        self.exchanges.len()
    }

    fn work_messages(&self) -> Vec<Value> {
        let mut messages = self.root.clone();
        if let Some(summary) = &self.summary {
            messages.push(json!({"role":"assistant", "content":summary}));
        }
        messages.extend(self.exchanges.iter().flatten().cloned());
        messages.push(json!({"role":"user", "content":self.follow_up}));
        messages
    }

    fn summary_messages(&self, older: usize) -> Vec<Value> {
        summary_transcript(
            self.root[1]["content"].as_str().unwrap(),
            self.summary.as_deref(),
            &self.exchanges[..older]
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>(),
            "Keep task constraints and distinguish checked results from guesses.",
        )
    }
}

fn policy() -> ContextPolicy {
    ContextPolicy::new(
        CommonLimits {
            wall_seconds: 120.try_into().unwrap(),
            generated_tokens: 4096.try_into().unwrap(),
            tokens_per_call: 512.try_into().unwrap(),
            context_tokens: 16384.try_into().unwrap(),
            model_seconds: 30.try_into().unwrap(),
            command_seconds: 10.try_into().unwrap(),
            output_bytes: 1024.try_into().unwrap(),
            log_bytes: 65536.try_into().unwrap(),
        },
        CompactionSettings {
            trigger_percent: 50,
            recent_exchanges: 1,
            summary_tokens: 128.try_into().unwrap(),
            ..CompactionSettings::default()
        },
        vec![json!({"type":"function", "function":{"name":"read_file"}})],
    )
    .unwrap()
}

fn budget() -> Budget {
    Budget {
        remaining_tokens: 4096,
        remaining_ms: 120000,
        model_ms: 0,
        known_generated_tokens: 0,
    }
}

fn chat() -> Chat {
    Chat {
        root: vec![
            json!({"role":"system", "content":"Use only the selected workspace."}),
            json!({"role":"user", "content":"Fix the parser without changing its public API."}),
        ],
        exchanges: (0..3).map(|index| vec![
            json!({"role":"assistant", "tool_calls":[{
                "function":{"name":"read_file", "arguments":{"path":format!("part-{index}")}}
            }]}),
            json!({"role":"tool", "tool_name":"read_file", "content":"x".repeat(2800)}),
        ]).collect(),
        summary: None,
        follow_up: "Preserve the existing error messages too.".into(),
    }
}

#[test]
fn a_standalone_caller_compacts_then_continues_the_same_conversation() {
    let policy = policy();
    let mut chat = chat();
    let context = ContextWindow::default();
    assert!(policy.initial_fits(&chat.root));
    let Plan::Summarize(plan) = policy.plan(&chat, &context, budget()) else {
        panic!("expected a summary of the older complete exchanges");
    };
    assert_eq!(plan.older_exchanges, 2);
    assert_eq!(plan.before_messages, chat.work_messages());
    let transcript = plan.messages[1]["content"].as_str().unwrap();
    assert!(transcript.contains("part-0"));
    assert!(transcript.contains("part-1"));
    assert!(!transcript.contains("part-2"));
    assert!(!transcript.contains(&chat.follow_up));

    // Validate before accepting the summary or discarding any exchange.
    assert!(
        policy
            .replacement(plan.before_bytes, &plan.before_messages, None)
            .is_err()
    );
    let retained = chat.exchanges[plan.older_exchanges..].to_vec();
    let candidate = Chat {
        root: chat.root.clone(),
        exchanges: retained.clone(),
        summary: Some("Read the first two parts; keep the parser's public API.".into()),
        follow_up: chat.follow_up.clone(),
    };
    let replacement = candidate.work_messages();
    policy
        .replacement(plan.before_bytes, &replacement, None)
        .unwrap();
    chat = candidate;
    let Plan::Work {
        messages,
        deferred: None,
    } = policy.plan(&chat, &context, budget())
    else {
        panic!("the compacted conversation must have room to continue");
    };
    assert_eq!(messages, replacement);
    assert_eq!(&messages[..2], chat.root);
    assert_eq!(&messages[3..5], retained[0]);
    assert_eq!(messages.last().unwrap()["content"], chat.follow_up);
}

#[test]
fn a_caller_cannot_plan_a_summary_without_budget_for_continued_work() {
    let chat = chat();
    let before = chat.work_messages();
    let mut remaining = budget();
    remaining.remaining_tokens = 128 + 512 - 1;
    assert!(matches!(
        policy().plan(&chat, &ContextWindow::default(), remaining),
        Plan::Stop {
            reason: StopReason::TokenLimit,
            detail: Some(_)
        }
    ));
    assert_eq!(chat.work_messages(), before);
}
