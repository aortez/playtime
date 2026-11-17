use playtime_chat::{
    host::{HostEffects, Workspace},
    runtime::{self, Effects, Event},
};
use playtime_core::{RequestPurpose, RunMode, Sha256Digest, StopReason};
use playtime_engine::{
    chat::ConversationConfig, compaction::CompactionSettings, execution::ToolProfile,
    simulation::Scenario, *,
};
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
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
        let root =
            Self(std::env::temp_dir().join(format!("pth-{}", Token::generate().unwrap().expose())));
        PrivateDirectory::open(&root.0, true).unwrap();
        fs::create_dir(root.0.join("project")).unwrap();
        PrivateDirectory::open(&root.0.join("state"), true).unwrap();
        root
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn config(workspace: &Workspace) -> ConversationConfig {
    let fixture: Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap();
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = 32768.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 4096.try_into().unwrap();
    parameters.limits.output_bytes = 4096.try_into().unwrap();
    ConversationConfig {
        schema_version: 2,
        model: Sha256Digest::new("1".repeat(64)).unwrap(),
        workspace: Sha256Digest::new("2".repeat(64)).unwrap(),
        instructions: "Use the host tools and return when done.".into(),
        inventory: workspace.inventory(),
        mode: RunMode::LiveSmoke,
        execution: playtime_engine::execution::ExecutionConfig {
            tool_profile: ToolProfile::HostV1,
            workshop: None,
            ..playtime_engine::execution::ExecutionConfig::workshop(
                parameters,
                ToolPolicy::ConfirmedExternal {
                    return_overhead_ms: 1000,
                },
            )
        },
        compaction: CompactionSettings {
            allow_finish_run: false,
            trigger_percent: 50,
            recent_exchanges: 1,
            summary_tokens: 256.try_into().unwrap(),
            ..Default::default()
        },
    }
}
#[derive(Default)]
struct ModelFixture {
    calls: usize,
    compactions: usize,
}
#[async_trait::async_trait]
impl Effects for ModelFixture {
    async fn execute(
        &mut self,
        effect: &Effect,
        _: Instant,
        _: CancellationToken,
        _: mpsc::Sender<Value>,
    ) -> Outcome {
        let EffectKind::Model(request) = &effect.kind else {
            panic!("model adapter received tool effect")
        };
        let message = if request.purpose == RequestPurpose::Compaction {
            self.compactions += 1;
            json!({"role":"assistant","content":"We started a persistent host process. Use poll_process to inspect current handles and continue the user's task."})
        } else {
            assert!(
                request
                    .tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "start_process")
            );
            self.calls += 1;
            if self.calls.is_multiple_of(2) {
                json!({"role":"assistant","content":"Host process verified."})
            } else {
                let (name, args) = if self.calls == 1 {
                    ("start_process", json!({"command":"sleep 300"}))
                } else {
                    ("poll_process", json!({}))
                };
                json!({"role":"assistant","content":"","tool_calls":[{"id":format!("host-{}",self.calls),"function":{"name":name,"arguments":args}}]})
            }
        };
        Outcome::ModelCompleted {
            response: ModelResponse {
                message,
                done: true,
                generated_tokens: Some(40),
                prompt_tokens: None,
            },
            finish_reason: "stop".into(),
        }
    }
    async fn cleanup(&mut self, _: Instant) -> Vec<String> {
        vec![]
    }
}

#[test]
fn host_admission_does_not_scan_build_trees_and_keeps_shared_lease_and_fence() {
    use std::os::unix::net::UnixListener;
    let root = Root::new();
    let project = root.0.join("project");
    let state = root.0.join("state");
    let _socket = UnixListener::bind(project.join("control.sock")).unwrap();
    let host = Workspace::open(&project, &state, 4096).unwrap();
    assert!(Workspace::open(&project, &state, 4096).is_err());
    assert!(
        host.inventory()
            .contains(&project.to_string_lossy().to_string())
    );
    assert_eq!(host.binding()["profile"], "host-coding-v1");
    let cfg = config(&host);
    let mut docker = cfg.clone();
    docker.execution.tool_profile = ToolProfile::Workshop;
    docker.execution.workshop = Some(cfg.execution.controller_parameters().workshop);
    assert_ne!(cfg.fingerprint().unwrap(), docker.fingerprint().unwrap());
    drop(host);
    let lease = fs::read_dir(&state)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    PrivateDirectory::open(&lease, false)
        .unwrap()
        .write_new("active", b"unresolved")
        .unwrap();
    assert!(Workspace::open(&project, &state, 4096).is_err());
}

#[tokio::test]
#[ignore = "explicit same-user systemd host gate"]
async fn host_process_survives_real_harness_turns_compaction_and_stops_at_exit() {
    let root = Root::new();
    let workspace = Workspace::open(&root.0.join("project"), &root.0.join("state"), 4096).unwrap();
    let cfg = config(&workspace);
    let workspace = Arc::new(Mutex::new(workspace));
    let effects = Arc::new(Mutex::new(HostEffects {
        inner: ModelFixture::default(),
        workspace: workspace.clone(),
    }));
    let saved = root.0.join("conversation");
    let mut session = Session::create(&saved, cfg.clone()).unwrap();
    for index in 0..8 {
        let (tx, mut rx) = runtime::events();
        let drain = tokio::spawn(async move {
            let mut results = vec![];
            while let Some(event) = rx.recv().await {
                if let Event::ToolResult { value } = event {
                    results.push(value);
                }
            }
            results
        });
        let done = runtime::run_turn(
            session,
            format!(
                "Inspect the running host process. {}",
                "Context fixture material. ".repeat(150)
            ),
            effects.clone(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.result.reason, StopReason::AgentFinished);
        let results = drain.await.unwrap();
        assert_eq!(results.len(), 1);
        if index > 0 {
            assert_eq!(results[0]["processes"][0]["running"], true);
        }
        drop(done.session.unwrap());
        session = Session::open(&saved, &cfg).unwrap();
    }
    assert!(
        effects.lock().await.inner.compactions > 0,
        "must exercise actual controller compaction"
    );
    drop(session);
    workspace.lock().await.close().await.unwrap();
    drop(effects);
    drop(workspace);
    let mut reopened =
        Workspace::open(&root.0.join("project"), &root.0.join("state"), 4096).unwrap();
    let processes = reopened
        .call(
            "poll_process",
            json!({}),
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(processes["processes"], json!([]));
    drop(Session::open(&saved, &cfg).unwrap());
    reopened.close().await.unwrap();
}

#[tokio::test]
#[ignore = "explicit same-user systemd host gate"]
async fn host_command_cancel_is_acknowledged_before_next_tool() {
    let root = Root::new();
    let mut workspace =
        Workspace::open(&root.0.join("project"), &root.0.join("state"), 4096).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let reply = workspace
        .call(
            "exec",
            json!({"command":"sleep 60"}),
            Instant::now() + Duration::from_secs(30),
            cancel,
        )
        .await
        .unwrap();
    assert!(reply["error"].as_str().unwrap().contains("cancelled"));
    let reply = workspace
        .call(
            "exec",
            json!({"command":"printf next"}),
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply["stdout"], "next");
    workspace.close().await.unwrap();
}

#[tokio::test]
#[ignore = "explicit same-user systemd host gate"]
async fn interrupted_host_process_call_remains_inspectable_without_replay() {
    let root = Root::new();
    let workspace = Workspace::open(&root.0.join("project"), &root.0.join("state"), 4096).unwrap();
    let cfg = config(&workspace);
    let workspace = Arc::new(Mutex::new(workspace));
    let effects = Arc::new(Mutex::new(HostEffects {
        inner: ModelFixture::default(),
        workspace: workspace.clone(),
    }));
    let saved = root.0.join("interrupted");
    let session = Session::create(&saved, cfg.clone()).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, Event::ToolResult { .. }) {
                trigger.cancel();
            }
        }
    });
    let done = runtime::run_turn(session, "Start a process".into(), effects, tx, cancel)
        .await
        .unwrap();
    drain.await.unwrap();
    assert_eq!(done.result.reason, StopReason::Interrupted);
    assert!(done.session.is_none());
    workspace.lock().await.close().await.unwrap();
    let report = Session::inspect_interrupted(&saved, &cfg).unwrap();
    assert!(
        serde_json::to_string(&report)
            .unwrap()
            .contains("start_process")
    );
    let recovered =
        Session::recover_interrupted(&saved, &root.0.join("recovered"), cfg, false).unwrap();
    assert!(recovered.pending_recovery().is_some());
    assert!(
        workspace
            .lock()
            .await
            .call(
                "poll_process",
                json!({}),
                Instant::now() + Duration::from_secs(10),
                CancellationToken::new()
            )
            .await
            .unwrap()["processes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    workspace.lock().await.close().await.unwrap();
}
