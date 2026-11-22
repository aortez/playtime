use playtime_chat::{
    runtime::{self, Effects},
    worker,
};
use playtime_core::{RequestPurpose, StopReason};
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
    commands: Vec<String>,
    selection: Vec<usize>,
    output: Option<Value>,
    stale_reference: Option<String>,
    observed: Vec<Value>,
    summary_padding: usize,
    summaries: usize,
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
            EffectKind::Tool(_) => Outcome::Tool(self.output.clone().unwrap_or_else(||
                json!({"stdout":"ok","stderr":"","exit_code":0,"timed_out":false,"duration_seconds":0.01}))),
            EffectKind::Model(request) if request.purpose == RequestPurpose::Compaction => {
                self.summaries += 1;
                let reference = self.stale_reference.as_deref().expect("old reference for summary");
                assert!(serde_json::to_string(&request.messages).unwrap().contains(reference));
                Outcome::ModelCompleted {
                    response: ModelResponse { message: json!({"role":"assistant","content":format!("Earlier check used {reference}. Historical evidence only.")}), done: true, generated_tokens: Some(50), prompt_tokens: Some(200) },
                    finish_reason: "stop".into(),
                }
            }
            EffectKind::Model(request) => {
                self.calls += 1;
                let message = if self.calls % 2 == 1 {
                    if self.commands.is_empty() { self.commands.push("printf ok".into()); }
                    let calls: Vec<_> = self.commands.iter().enumerate().map(|(i, command)|
                        json!({"id":format!("call-{}-{i}",self.calls),"function":{"name":"exec","arguments":{"command":command}}})).collect();
                    json!({"role":"assistant","content":"","tool_calls":calls})
                } else {
                    self.observed = request.messages.iter().rev().filter(|v| v["role"] == "tool")
                        .take(self.commands.len()).map(|v| serde_json::from_str(v["content"].as_str().unwrap()).unwrap()).collect();
                    self.observed.reverse();
                    let selection = if self.selection.is_empty() { vec![0] } else { self.selection.clone() };
                    let checks: Vec<_> = selection.iter().map(|i| json!({"evidence_ref": self.observed[*i]["evidence_log_path"]})).collect();
                    let checks = if let Some(reference) = &self.stale_reference { vec![json!({"evidence_ref":reference})] } else { checks };
                    let report = json!({"status":"completed","summary":format!("Checked{}", " details".repeat(self.summary_padding)),"changes":[],"verification":checks,"remaining_work":[],"questions":[],"blockers":[]});
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
        execution: playtime_engine::execution::ExecutionConfig::workshop(
            parameters,
            ToolPolicy::Simulation,
        ),
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
    let mut cfg = config();
    cfg.compaction.trigger_percent = 60;
    cfg.compaction.recent_exchanges = 0;
    let mut session = Session::create(&state, cfg.clone()).unwrap();
    let mut previous_reference = None;
    for turn in 0..2 {
        let artifacts = root.0.join(format!("artifacts-{turn}"));
        let dir = PrivateDirectory::open(&artifacts, true).unwrap();
        let effects = Arc::new(Mutex::new(
            worker::EvidenceEffects::new(
                Fixture {
                    stale_reference: previous_reference.clone(),
                    summary_padding: if turn == 0 { 1000 } else { 0 },
                    ..Default::default()
                },
                artifacts,
                65536,
            )
            .unwrap(),
        ));
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
        let effects = effects.lock().await;
        let report = worker::validate_report(response.as_bytes(), &dir, effects.evidence());
        if turn == 0 {
            previous_reference = Some(report.unwrap().verification[0].log_path.clone());
        } else {
            assert!(report.is_err());
            assert_eq!(effects.inner.summaries, 1);
            assert_eq!(checkpoint.history.compactions, 1);
            assert!(
                serde_json::to_string(&checkpoint.history.summary)
                    .unwrap()
                    .contains(previous_reference.as_deref().unwrap())
            );
            let current = effects.inner.observed[0]["evidence_log_path"]
                .as_str()
                .unwrap();
            assert_ne!(current, previous_reference.as_deref().unwrap());
            worker::validate_report(
                &serde_json::to_vec(&reference_report(current)).unwrap(),
                &dir,
                effects.evidence(),
            )
            .unwrap();
        }
        assert!(effects.inner.cleanup > 0);
        drop(completed);
        session = Session::open(&state, &cfg).unwrap();
    }
}
#[tokio::test]
async fn references_carry_a_short_invocation_nonce() {
    let root = Root::new();
    PrivateDirectory::open(&root.0, true).unwrap();
    let mut effects =
        worker::EvidenceEffects::new(Fixture::default(), root.0.clone(), 65536).unwrap();
    let reference = capture(&mut effects, 1, "check").await;
    // The model copies each reference whole.
    let nonce = reference
        .strip_prefix("tool-")
        .and_then(|rest| rest.strip_suffix("-1.json"))
        .unwrap();
    assert_eq!(nonce.len(), 8);
    assert!(
        nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );
}
#[tokio::test]
async fn verification_must_match_captured_commands_and_exit_codes() {
    let root = Root::new();
    let dir = PrivateDirectory::open(&root.0, true).unwrap();
    let mut effects =
        worker::EvidenceEffects::new(Fixture::default(), root.0.clone(), 65536).unwrap();
    let reference = capture(&mut effects, 1, "check").await;
    let good = json!({"status":"completed","summary":"Checked","changes":[],"verification":[{"command":"check","exit_code":0,"log_path":reference}],"remaining_work":[],"questions":[],"blockers":[]});
    worker::validate_report(
        &serde_json::to_vec(&good).unwrap(),
        &dir,
        effects.evidence(),
    )
    .unwrap();
    let encoded = serde_json::to_string(&good).unwrap();
    for prefix in ["[", "true\n", "null\n", "\""] {
        assert!(
            worker::validate_report(
                format!("{prefix}{encoded}").as_bytes(),
                &dir,
                effects.evidence()
            )
            .is_err()
        );
    }
    worker::validate_report(
        format!("Checked the file.\n{encoded}").as_bytes(),
        &dir,
        effects.evidence(),
    )
    .unwrap();
    assert!(
        worker::validate_report(
            format!("{encoded}\n{encoded}").as_bytes(),
            &dir,
            effects.evidence()
        )
        .is_err()
    );
    assert!(
        worker::validate_report(
            format!("Checked.\n{}", &encoded[..encoded.len() - 1]).as_bytes(),
            &dir,
            effects.evidence()
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
        assert!(
            worker::validate_report(&serde_json::to_vec(&bad).unwrap(), &dir, effects.evidence())
                .is_err()
        );
    }
    let mut blocked = good;
    blocked["status"] = "blocked".into();
    assert!(
        worker::validate_report(
            &serde_json::to_vec(&blocked).unwrap(),
            &dir,
            effects.evidence()
        )
        .is_err()
    );
    blocked["blockers"] = json!(["Needs dependency"]);
    worker::validate_report(
        &serde_json::to_vec(&blocked).unwrap(),
        &dir,
        effects.evidence(),
    )
    .unwrap();
}
#[tokio::test]
async fn evidence_failure_stops_turn_and_preserves_cleanup() {
    for budget in [0, 65536] {
        let root = Root::new();
        let session = Session::create(&root.0.join("session"), config()).unwrap();
        let effects = Arc::new(Mutex::new(
            worker::EvidenceEffects::new(Fixture::default(), root.0.join("missing"), budget)
                .unwrap(),
        ));
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
        let effects = effects.lock().await;
        assert!(effects.inner.cleanup > 0);
        assert_eq!(effects.inner.calls, 1);
    }
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

async fn capture(effects: &mut worker::EvidenceEffects<Fixture>, id: u64, command: &str) -> String {
    let (tx, _rx) = mpsc::channel(1);
    let result = effects
        .execute(
            &Effect {
                id,
                deadline_ms: 1000,
                kind: EffectKind::Tool(ToolRequest {
                    name: "exec".into(),
                    arguments: json!({"command":command}),
                }),
            },
            Instant::now() + std::time::Duration::from_secs(1),
            CancellationToken::new(),
            tx,
        )
        .await;
    let Outcome::Tool(value) = result else {
        panic!("{result:?}")
    };
    value["evidence_log_path"].as_str().unwrap().into()
}

fn reference_report(reference: &str) -> Value {
    json!({"status":"completed","summary":"Checked","changes":[],"verification":[{"evidence_ref":reference}],"remaining_work":[],"questions":[],"blockers":[]})
}

#[tokio::test]
async fn references_preserve_exact_selected_commands_and_saved_report_digest() {
    use sha2::{Digest, Sha256};
    let root = Root::new();
    let records = PrivateDirectory::open(&root.0, true).unwrap();
    let artifacts = root.0.join("artifacts");
    let dir = PrivateDirectory::open(&artifacts, true).unwrap();
    let commands = vec![
        "cd /work && cat <<'EOF'\n  \"quoted\" '$value'\nEOF\n".into(),
        "uncited check".into(),
        "  printf '%s' \"last\"  ".into(),
    ];
    let effects = Arc::new(Mutex::new(
        worker::EvidenceEffects::new(
            Fixture {
                commands: commands.clone(),
                selection: vec![2, 0],
                output: Some(
                    json!({"stdout":"x".repeat(16000),"stderr":"","exit_code":0,"timed_out":false}),
                ),
                ..Default::default()
            },
            artifacts,
            65536,
        )
        .unwrap(),
    ));
    let session = Session::create(&root.0.join("session"), config()).unwrap();
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let completed = runtime::run_turn(
        session,
        "Run checks".into(),
        effects.clone(),
        tx,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    drain.await.unwrap();
    assert_eq!(completed.result.reason, StopReason::AgentFinished);
    let session = completed.session.unwrap();
    let response = session
        .checkpoint()
        .unwrap()
        .history
        .exchanges
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let effects = effects.lock().await;
    assert!(
        effects
            .inner
            .observed
            .iter()
            .all(|v| v["context_truncated"] == true)
    );
    let (report, digest) =
        worker::save_report(response, &records, &dir, effects.evidence()).unwrap();
    assert_eq!(
        report
            .verification
            .iter()
            .map(|v| &v.command)
            .collect::<Vec<_>>(),
        vec![&commands[2], &commands[0]]
    );
    assert_eq!(
        records
            .read("worker-report.raw.txt", worker::MAX_REPORT)
            .unwrap(),
        response.as_bytes()
    );
    let canonical = records
        .read("worker-report.json", worker::MAX_REPORT)
        .unwrap();
    assert_eq!(
        digest,
        Sha256::digest(&canonical)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let canonical: Value = serde_json::from_slice(&canonical).unwrap();
    let raw: Value = serde_json::from_str(response).unwrap();
    for (i, check) in report.verification.iter().enumerate() {
        assert_eq!(raw["verification"][i]["evidence_ref"], check.log_path);
        assert_eq!(canonical["verification"][i].as_object().unwrap().len(), 3);
        let log: Value =
            serde_json::from_slice(&dir.read(&check.log_path, worker::MAX_REPORT).unwrap())
                .unwrap();
        assert_eq!(log["command"], check.command);
        assert_eq!(log["result"]["exit_code"], check.exit_code);
        assert!(
            log["result"]["stdout"].as_str().unwrap().len()
                > effects.inner.observed[i]["stdout"].as_str().unwrap().len()
        );
    }
}

#[tokio::test]
async fn references_reject_unknown_mixed_duplicate_and_changed_evidence() {
    let root = Root::new();
    let dir = PrivateDirectory::open(&root.0, true).unwrap();
    let mut effects =
        worker::EvidenceEffects::new(Fixture::default(), root.0.clone(), 65536).unwrap();
    let reference = capture(&mut effects, 1, "check").await;
    let validate = |report: &Value| {
        worker::validate_report(
            &serde_json::to_vec(report).unwrap(),
            &dir,
            effects.evidence(),
        )
    };
    let good = reference_report(&reference);
    validate(&good).unwrap();
    for name in [
        "",
        "tool-1.json",
        "../tool-1.json",
        "/tmp/tool-1.json",
        "tool-%2e%2e.json",
        "tool-\\1.json",
        "tool-unknown.json",
    ] {
        assert!(validate(&reference_report(name)).is_err(), "{name}");
    }
    for checks in [
        json!([{ "evidence_ref":reference }, { "evidence_ref":reference }]),
        json!([{ "evidence_ref":reference, "command":"check", "exit_code":0, "log_path":reference }]),
        json!([{ "evidence_ref":reference }, { "command":"check", "exit_code":0, "log_path":reference }]),
        json!([{ "evidence_ref":1 }]),
    ] {
        let mut bad = good.clone();
        bad["verification"] = checks;
        assert!(validate(&bad).is_err());
    }
    let bytes = dir.read(&reference, worker::MAX_REPORT).unwrap();
    dir.write_new("tool-uncommitted.json", &bytes).unwrap();
    assert!(validate(&reference_report("tool-uncommitted.json")).is_err());
    dir.replace(&reference, b"{}").unwrap();
    assert!(validate(&good).is_err());
    dir.remove(&reference).unwrap();
    assert!(validate(&good).is_err());
    dir.write_new("target.json", &bytes).unwrap();
    std::os::unix::fs::symlink(root.0.join("target.json"), root.0.join(&reference)).unwrap();
    assert!(validate(&good).is_err());
    let records = PrivateDirectory::open(&root.0.join("report"), true).unwrap();
    let raw = format!("Checked.\n{good}");
    assert!(worker::save_report(&raw, &records, &dir, effects.evidence()).is_err());
    assert_eq!(
        records
            .read("worker-report.raw.txt", worker::MAX_REPORT)
            .unwrap(),
        raw.as_bytes()
    );
    assert!(!root.0.join("report/worker-report.json").exists());
}

#[tokio::test]
async fn unsuccessful_or_incomplete_results_never_complete() {
    for output in [
        json!({"exit_code":1,"timed_out":false}),
        json!({"exit_code":0,"timed_out":true}),
        json!({"exit_code":0}),
        json!({"timed_out":false}),
        json!({"exit_code":null,"timed_out":false}),
        json!({"exit_code":0,"timed_out":false,"error":"incomplete"}),
    ] {
        let root = Root::new();
        let dir = PrivateDirectory::open(&root.0, true).unwrap();
        let mut effects = worker::EvidenceEffects::new(
            Fixture {
                output: Some(output.clone()),
                ..Default::default()
            },
            root.0.clone(),
            65536,
        )
        .unwrap();
        let reference = capture(&mut effects, 1, "check").await;
        let mut report = reference_report(&reference);
        assert!(
            worker::validate_report(
                &serde_json::to_vec(&report).unwrap(),
                &dir,
                effects.evidence()
            )
            .is_err()
        );
        report["status"] = "checkpoint".into();
        assert_eq!(
            worker::validate_report(
                &serde_json::to_vec(&report).unwrap(),
                &dir,
                effects.evidence()
            )
            .is_ok(),
            output["exit_code"] == 1
        );
    }
}

#[tokio::test]
async fn repeated_effect_ids_and_copied_foreign_logs_do_not_alias() {
    let root = Root::new();
    let first = PrivateDirectory::open(&root.0.join("first"), true).unwrap();
    let second = PrivateDirectory::open(&root.0.join("second"), true).unwrap();
    let mut a =
        worker::EvidenceEffects::new(Fixture::default(), root.0.join("first"), 65536).unwrap();
    let mut b =
        worker::EvidenceEffects::new(Fixture::default(), root.0.join("second"), 65536).unwrap();
    let old = capture(&mut a, 1, "first check").await;
    let new = capture(&mut b, 1, "second check").await;
    assert_ne!(old, new);
    second
        .write_new(&old, &first.read(&old, worker::MAX_REPORT).unwrap())
        .unwrap();
    assert!(
        worker::validate_report(
            &serde_json::to_vec(&reference_report(&old)).unwrap(),
            &second,
            b.evidence()
        )
        .is_err()
    );
    worker::validate_report(
        &serde_json::to_vec(&reference_report(&new)).unwrap(),
        &second,
        b.evidence(),
    )
    .unwrap();
}

#[test]
fn private_worker_evidence_cannot_be_inside_project_tools_root() {
    let root = Root::new();
    let project = root.0.join("project");
    let state = root.0.join("state");
    PrivateDirectory::open(&project, true).unwrap();
    PrivateDirectory::open(&state, true).unwrap();
    let artifacts = project.join("run/artifacts");
    PrivateDirectory::open(&artifacts, true).unwrap();
    assert!(
        playtime_docker::project::ProjectDirectory::open(&project, &state, &[artifacts]).is_err()
    );
}

#[test]
fn changed_worker_instructions_require_explicit_fresh_reconstruction() {
    let root = Root::new();
    let mut previous = config();
    previous.instructions = "Old worker reporting instructions".into();
    drop(Session::create(&root.0.join("old"), previous.clone()).unwrap());
    assert!(Session::open(&root.0.join("old"), &config()).is_err());
    Session::open(&root.0.join("old"), &previous).unwrap();
    Session::create(&root.0.join("fresh"), config()).unwrap();
}

#[tokio::test]
async fn reference_expansion_still_obeys_canonical_report_limit() {
    let root = Root::new();
    let dir = PrivateDirectory::open(&root.0, true).unwrap();
    let mut effects =
        worker::EvidenceEffects::new(Fixture::default(), root.0.clone(), 2 * 1024 * 1024).unwrap();
    let command = "x".repeat(600_000);
    let a = capture(&mut effects, 1, &command).await;
    let b = capture(&mut effects, 2, &command).await;
    let mut report = reference_report(&a);
    report["verification"]
        .as_array_mut()
        .unwrap()
        .push(json!({"evidence_ref":b}));
    assert!(worker::save_report(&report.to_string(), &dir, &dir, effects.evidence()).is_err());
    assert!(root.0.join("worker-report.raw.txt").exists());
    assert!(!root.0.join("worker-report.json").exists());
}
