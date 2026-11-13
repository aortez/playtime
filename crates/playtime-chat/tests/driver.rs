//! CPU adapter fixtures exercise the real controller, persistence and async loop.
use playtime_chat::runtime::{self, Effects, Event};
use playtime_core::{RunMode, Sha256Digest, StopReason};
use playtime_engine::{
    chat::ConversationConfig, compaction::CompactionSettings, simulation::Scenario, *,
};
use playtime_harness::Session;
use playtime_local::Token;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "playtime-driver-{}",
            Token::generate().unwrap().expose()
        )))
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn config() -> ConversationConfig {
    let fixture: Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap();
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = 16384.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 4096.try_into().unwrap();
    parameters.limits.output_bytes = 2048.try_into().unwrap();
    ConversationConfig {
        schema_version: 1,
        model: Sha256Digest::new("1".repeat(64)).unwrap(),
        workspace: Sha256Digest::new("2".repeat(64)).unwrap(),
        instructions: "Reply when finished.".into(),
        inventory: "Synthetic project tools.".into(),
        mode: RunMode::Simulation,
        tool_policy: ToolPolicy::Simulation,
        parameters,
        compaction: CompactionSettings {
            allow_finish_run: false,
            summary_tokens: 256.try_into().unwrap(),
            ..Default::default()
        },
    }
}
#[derive(Default)]
struct Fixture {
    calls: u32,
    cleanup: u32,
    requests: Vec<ModelRequest>,
    block_model: bool,
    fail_cleanup: bool,
    break_storage: Option<PathBuf>,
}
#[async_trait::async_trait]
impl Effects for Fixture {
    async fn execute(
        &mut self,
        effect: &Effect,
        _deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<Value>,
    ) -> Outcome {
        match &effect.kind {
            EffectKind::StartWorkspace => {
                if let Some(root) = &self.break_storage {
                    // Occupy the next immutable record name to fail publication.
                    let head: Value =
                        serde_json::from_slice(&fs::read(root.join("head.json")).unwrap()).unwrap();
                    let turn = head["active_turn"].as_str().unwrap();
                    let mut next = 1;
                    for entry in fs::read_dir(root).unwrap() {
                        let path = entry.unwrap().path();
                        if path.extension().is_some_and(|s| s == "jsonl") {
                            for line in fs::read_to_string(path).unwrap().lines() {
                                let record: Record = serde_json::from_str(line).unwrap();
                                next = next.max(record.sequence + 1);
                            }
                        }
                    }
                    fs::write(
                        root.join(format!("events-{turn}-{next:020}.jsonl")),
                        "existing evidence",
                    )
                    .unwrap();
                }
                Outcome::WorkspaceStarted
            }
            EffectKind::Model(request) => {
                self.requests.push(request.clone());
                self.calls += 1;
                chunks.send(json!({"content":"partial"})).await.unwrap();
                if self.block_model {
                    cancel.cancelled().await;
                    return Outcome::ModelError(ModelFailure::Incomplete(
                        "cancelled fixture".into(),
                    ));
                }
                let message = if self.calls % 2 == 1 {
                    json!({"role":"assistant","content":"","tool_calls":[{"id":format!("call-{}",self.calls),"function":{"name":"read_file","arguments":{"path":"/work/file"}}}]})
                } else {
                    json!({"role":"assistant","content":"done"})
                };
                Outcome::ModelCompleted {
                    response: ModelResponse {
                        message,
                        done: true,
                        generated_tokens: Some(10),
                        prompt_tokens: Some(100),
                    },
                    finish_reason: "stop".into(),
                }
            }
            EffectKind::Tool(_) => Outcome::Tool(
                json!({"content":"actual fixture file","offset_bytes":0,"truncated":false}),
            ),
            EffectKind::Cleanup => Outcome::Cleaned(self.cleanup(_deadline).await),
            _ => panic!("unexpected chat effect"),
        }
    }
    async fn cleanup(&mut self, _deadline: Instant) -> Vec<String> {
        self.cleanup += 1;
        if self.fail_cleanup {
            vec!["injected unresolved container".into()]
        } else {
            vec![]
        }
    }
}
#[tokio::test]
async fn tool_roundtrip_reopens_and_retains_followup_history() {
    let root = Root::new();
    let config = config();
    let mut session = Session::create(&root.0, config.clone()).unwrap();
    let effects = Arc::new(Mutex::new(Fixture::default()));
    for input in ["Read the file", "What did it contain?"] {
        let (tx, mut rx) = runtime::events();
        let drain = tokio::spawn(async move {
            let mut all = vec![];
            while let Some(event) = rx.recv().await {
                all.push(event);
            }
            all
        });
        let result = runtime::run_turn(
            session,
            input.into(),
            effects.clone(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.result.reason, StopReason::AgentFinished);
        assert!(
            drain
                .await
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::ToolResult { .. }))
        );
        drop(result.session.unwrap());
        session = Session::open(&root.0, &config).unwrap();
    }
    let effects = effects.lock().await;
    assert!(
        serde_json::to_string(&effects.requests[2].messages)
            .unwrap()
            .contains("actual fixture file")
    );
    assert!(effects.cleanup >= 2);
}
#[tokio::test]
async fn cancellation_stops_effect_and_cleans_without_resuming_unknown_request() {
    let root = Root::new();
    let config = config();
    let session = Session::create(&root.0, config.clone()).unwrap();
    let effects = Arc::new(Mutex::new(Fixture {
        block_model: true,
        ..Default::default()
    }));
    let (tx, mut rx) = runtime::events();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, Event::Chunk { .. }) {
                trigger.cancel();
            }
        }
    });
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        runtime::run_turn(session, "Work".into(), effects.clone(), tx, cancel),
    )
    .await
    .unwrap()
    .unwrap();
    drain.await.unwrap();
    assert_eq!(result.result.reason, StopReason::Interrupted);
    assert_eq!(result.result.usage.unknown_requests, 1);
    assert!(result.session.is_none());
    assert!(effects.lock().await.cleanup > 0);
    assert!(Session::open(&root.0, &config).is_err());
}
#[tokio::test]
async fn disconnected_frontend_stops_before_model_dispatch_and_still_cleans() {
    let root = Root::new();
    let session = Session::create(&root.0, config()).unwrap();
    let effects = Arc::new(Mutex::new(Fixture::default()));
    let (tx, rx) = runtime::events();
    drop(rx);
    let result = runtime::run_turn(
        session,
        "Work".into(),
        effects.clone(),
        tx,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.result.reason, StopReason::Interrupted);
    assert_eq!(effects.lock().await.calls, 0);
    assert!(effects.lock().await.cleanup > 0);
}
#[tokio::test]
async fn uncertain_cleanup_never_publishes_ready_head() {
    let root = Root::new();
    let config = config();
    let session = Session::create(&root.0, config.clone()).unwrap();
    let effects = Arc::new(Mutex::new(Fixture {
        fail_cleanup: true,
        ..Default::default()
    }));
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    assert!(
        runtime::run_turn(
            session,
            "Work".into(),
            effects,
            tx,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    drain.await.unwrap();
    assert!(Session::open(&root.0, &config).is_err());
}
#[tokio::test]
async fn failed_record_publication_cleans_without_sending_model_request() {
    let root = Root::new();
    let config = config();
    let session = Session::create(&root.0, config).unwrap();
    let effects = Arc::new(Mutex::new(Fixture {
        break_storage: Some(root.0.clone()),
        ..Default::default()
    }));
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    assert!(
        runtime::run_turn(
            session,
            "Work".into(),
            effects.clone(),
            tx,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    drain.await.unwrap();
    assert_eq!(effects.lock().await.calls, 0);
    assert!(effects.lock().await.cleanup > 0);
}
