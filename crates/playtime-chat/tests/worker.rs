use playtime_chat::{
    runtime::{self, Effects},
    worker,
};
use playtime_core::StopReason;
use playtime_engine::*;
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "worker-test-{}",
            Token::generate().unwrap().expose()
        )))
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Fixture {
    calls: usize,
    cleanup: usize,
}
#[async_trait::async_trait]
impl Effects for Fixture {
    async fn execute(
        &mut self,
        effect: &Effect,
        _: Instant,
        _: CancellationToken,
        _: mpsc::Sender<Value>,
    ) -> Outcome {
        match &effect.kind {
            EffectKind::StartWorkspace => Outcome::WorkspaceStarted,
            EffectKind::Tool(_) => Outcome::Tool(
                json!({"stdout":"ok","stderr":"","exit_code":0,"timed_out":false,"duration_seconds":0.01}),
            ),
            EffectKind::Model(request) => {
                self.calls += 1;
                let message = if self.calls % 2 == 1 {
                    json!({"role":"assistant","content":"","tool_calls":[{"id":format!("call-{}",self.calls),"function":{"name":"exec","arguments":{"command":"printf ok"}}}]})
                } else {
                    let tool = request
                        .messages
                        .iter()
                        .rev()
                        .find(|v| v["role"] == "tool")
                        .unwrap();
                    let value: Value =
                        serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                    let report = json!({"status":"completed","summary":"Checked","changes":[],"verification":[{"command":"printf ok","exit_code":0,"log_path":value["evidence_log_path"]}],"remaining_work":[],"questions":[],"blockers":[]});
                    json!({"role":"assistant","content":report.to_string()})
                };
                Outcome::ModelCompleted {
                    response: ModelResponse {
                        message,
                        done: true,
                        generated_tokens: Some(50),
                        prompt_tokens: Some(200),
                    },
                    finish_reason: "stop".into(),
                }
            }
            EffectKind::Cleanup => Outcome::Cleaned(self.cleanup(Instant::now()).await),
            _ => panic!("unexpected effect: {effect:?}"),
        }
    }
    async fn cleanup(&mut self, _: Instant) -> Vec<String> {
        self.cleanup += 1;
        vec![]
    }
}
fn config() -> chat::ConversationConfig {
    let fixture: simulation::Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap();
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = 16384.try_into().unwrap();
    parameters.limits.output_bytes = 2048.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 4096.try_into().unwrap();
    chat::ConversationConfig {
        schema_version: 1,
        model: playtime_core::Sha256Digest::new("1".repeat(64)).unwrap(),
        workspace: playtime_core::Sha256Digest::new("2".repeat(64)).unwrap(),
        instructions: worker::INSTRUCTIONS.into(),
        inventory: "fixture".into(),
        mode: playtime_core::RunMode::Simulation,
        tool_policy: ToolPolicy::Simulation,
        parameters,
        compaction: compaction::CompactionSettings {
            allow_finish_run: false,
            summary_tokens: 256.try_into().unwrap(),
            ..Default::default()
        },
    }
}
#[tokio::test]
async fn shared_driver_retains_evidence_and_resumes_worker_history() {
    let root = Root::new();
    let state = root.0.join("session");
    let cfg = config();
    let mut session = Session::create(&state, cfg.clone()).unwrap();
    let mut previous_exchanges = 0;
    for turn in 0..2 {
        let artifacts = root.0.join(format!("artifacts-{turn}"));
        let dir = PrivateDirectory::open(&artifacts, true).unwrap();
        let effects = Arc::new(Mutex::new(worker::EvidenceEffects {
            inner: Fixture::default(),
            artifacts,
            remaining_bytes: 65536,
        }));
        let (tx, mut rx) = runtime::events();
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = runtime::run_turn(
            session,
            "Run the check".into(),
            effects.clone(),
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        drain.await.unwrap();
        assert_eq!(result.result.reason, StopReason::AgentFinished);
        let completed = result.session.unwrap();
        let checkpoint = completed.checkpoint().unwrap();
        let response = checkpoint
            .history
            .exchanges
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()["content"]
            .as_str()
            .unwrap();
        worker::validate_report(response.as_bytes(), &dir).unwrap();
        assert!(effects.lock().await.inner.cleanup > 0);
        assert!(checkpoint.history.exchanges.len() > previous_exchanges);
        previous_exchanges = checkpoint.history.exchanges.len();
        drop(completed);
        session = Session::open(&state, &cfg).unwrap();
    }
}
#[test]
fn verification_must_match_captured_commands_and_exit_codes() {
    let root = Root::new();
    let dir = PrivateDirectory::open(&root.0, true).unwrap();
    dir.write_new(
        "tool-1.json",
        br#"{"schema_version":1,"command":"check","result":{"exit_code":0,"timed_out":false}}"#,
    )
    .unwrap();
    let good = json!({"status":"completed","summary":"Checked","changes":[],"verification":[{"command":"check","exit_code":0,"log_path":"tool-1.json"}],"remaining_work":[],"questions":[],"blockers":[]});
    worker::validate_report(&serde_json::to_vec(&good).unwrap(), &dir).unwrap();
    let encoded = serde_json::to_string(&good).unwrap();
    for prefix in ["[", "true\n", "null\n", "\""] {
        assert!(worker::validate_report(format!("{prefix}{encoded}").as_bytes(), &dir).is_err());
    }
    worker::validate_report(format!("Checked the file.\n{encoded}").as_bytes(), &dir).unwrap();
    assert!(worker::validate_report(format!("{encoded}\n{encoded}").as_bytes(), &dir).is_err());
    assert!(
        worker::validate_report(
            format!("Checked.\n{}", &encoded[..encoded.len() - 1]).as_bytes(),
            &dir
        )
        .is_err()
    );
    for (field, value) in [
        ("command", json!("other check")),
        ("exit_code", json!(1)),
        ("log_path", json!("../tool-1.json")),
    ] {
        let mut bad = good.clone();
        bad["verification"][0][field] = value;
        assert!(worker::validate_report(&serde_json::to_vec(&bad).unwrap(), &dir).is_err());
    }
    let mut blocked = good;
    blocked["status"] = "blocked".into();
    assert!(worker::validate_report(&serde_json::to_vec(&blocked).unwrap(), &dir).is_err());
    blocked["blockers"] = json!(["Needs dependency"]);
    worker::validate_report(&serde_json::to_vec(&blocked).unwrap(), &dir).unwrap();
}
#[tokio::test]
async fn evidence_failure_stops_turn_and_preserves_cleanup() {
    let root = Root::new();
    let session = Session::create(&root.0.join("session"), config()).unwrap();
    let effects = Arc::new(Mutex::new(worker::EvidenceEffects {
        inner: Fixture::default(),
        artifacts: root.0.join("missing"),
        remaining_bytes: 0,
    }));
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = runtime::run_turn(
        session,
        "Check".into(),
        effects.clone(),
        tx,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    drain.await.unwrap();
    assert_ne!(result.result.reason, StopReason::AgentFinished);
    assert!(effects.lock().await.inner.cleanup > 0);
}

#[test]
fn local_input_check_needs_no_endpoint_and_rejects_writable_ancestors() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let root = Root::new();
    let workspace = root.0.join("workspace");
    let run = root.0.join("run");
    PrivateDirectory::open(&workspace, true).unwrap();
    let records = PrivateDirectory::open(&run, true).unwrap();
    records.write_new("prompt.txt", b"Run the check").unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let check = || {
        Command::new(env!("CARGO_BIN_EXE_playtime-task"))
            .arg("--check-inputs")
            .arg("--state-dir")
            .arg(root.0.join("state"))
            .arg("--model-lock")
            .arg(repository.join("models/framework-strata-gyro-m-fast-v1.lock.json"))
            .arg("--image-lock")
            .arg(repository.join("environments/general.lock.json"))
            .arg("--workspace")
            .arg(&workspace)
            .arg("--run-dir")
            .arg(&run)
            .arg("--request")
            .arg(run.join("prompt.txt"))
            .output()
            .unwrap()
    };
    let result = check();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["inputs_valid"],
        true
    );
    assert!(!run.join("worker-invoked.json").exists());
    std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(!check().status.success());
}
