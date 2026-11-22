use playtime_chat::{
    contract::{self, Spec},
    readonly::{Contract, digest},
    runtime::{self, Effects},
    worker,
};
use playtime_core::StopReason;
use playtime_engine::*;
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "contract-test-{}",
            Token::generate().unwrap().expose()
        ));
        PrivateDirectory::open(&root, true).unwrap();
        Self(root)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

enum Step {
    Exec(&'static str),
    /// The final text, given the newest evidence reference this model has seen.
    Reply(fn(&str) -> String),
}
#[derive(Default)]
struct Scripted {
    steps: VecDeque<Step>,
    reference: String,
    exit_code: i64,
}
#[async_trait::async_trait]
impl Effects for Scripted {
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
                json!({"stdout":"ok","stderr":"","exit_code":self.exit_code,"timed_out":false,"duration_seconds":0.01}),
            ),
            EffectKind::Model(request) => {
                if let Some(tool) = request.messages.last().filter(|m| m["role"] == "tool") {
                    let result: Value =
                        serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                    self.reference = result["evidence_log_path"].as_str().unwrap().into();
                }
                let message = match self.steps.pop_front().expect("scripted model step") {
                    Step::Exec(command) => json!({"role":"assistant","content":"","tool_calls":[
                        {"id":"check","function":{"name":"exec","arguments":{"command":command}}}]}),
                    Step::Reply(reply) => {
                        json!({"role":"assistant","content":reply(&self.reference)})
                    }
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
            EffectKind::Cleanup => Outcome::Cleaned(vec![]),
            _ => panic!("unexpected effect: {effect:?}"),
        }
    }
    async fn cleanup(&mut self, _: Instant) -> Vec<String> {
        vec![]
    }
}

fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["status","verification"],
        "properties":{"status":{"type":"string","enum":["checkpoint","completed"]},
            "verification":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":["command","exit_code","log_path"],
                "properties":{"command":{"type":"string"},"exit_code":{"type":"integer","maximum":0},"log_path":{"type":"string"}}}}}})
}
fn spec(schema: &Value, evidence: Option<&str>) -> Spec {
    let bytes = serde_json::to_vec(schema).unwrap();
    let hash = digest(&bytes);
    Spec::new(
        Contract::parse(bytes, &hash).unwrap(),
        evidence.map(Into::into),
    )
    .unwrap()
}
fn worker_config() -> chat::ConversationConfig {
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
type Shared = Arc<Mutex<worker::EvidenceEffects<Scripted>>>;
fn invocation(artifacts: &Path, steps: Vec<Step>, exit_code: i64) -> Shared {
    PrivateDirectory::open(artifacts, true).unwrap();
    Arc::new(Mutex::new(
        worker::EvidenceEffects::new(
            Scripted {
                steps: steps.into(),
                exit_code,
                ..Default::default()
            },
            artifacts.to_path_buf(),
            65536,
        )
        .unwrap(),
    ))
}
/// One completed turn: the reusable session and the model's final text.
async fn turn(session: Session, input: &str, effects: &Shared) -> (Session, String) {
    let (tx, mut rx) = runtime::events();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let completed = runtime::run_turn(
        session,
        input.into(),
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
        .unwrap()
        .to_owned();
    (session, response)
}
fn cite(reference: &str) -> String {
    json!({"status":"completed","verification":[{"evidence_ref":reference}]}).to_string()
}

#[tokio::test]
async fn resolved_result_is_written_under_the_callers_contract() {
    let root = Root::new();
    let records = PrivateDirectory::open(&root.0.join("run"), true).unwrap();
    let artifacts = root.0.join("run/artifacts");
    let spec = spec(&schema(), Some("verification"));
    let config = contract::configure(worker_config(), None, &spec).unwrap();
    assert!(config.instructions.contains("In the verification array"));
    assert!(config.instructions.contains(&spec.contract().sha256()));
    let effects = invocation(
        &artifacts,
        vec![Step::Exec("cd /work && make test"), Step::Reply(cite)],
        0,
    );
    let session = Session::create(&root.0.join("session"), config).unwrap();
    let (_session, response) = turn(session, "Run the check", &effects).await;
    let dir = PrivateDirectory::open(&artifacts, false).unwrap();
    let effects = effects.lock().await;
    let sha = contract::save_response(
        &response,
        &records,
        "response.raw.txt",
        &spec,
        &dir,
        effects.evidence(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        records.read("response.raw.txt", 65536).unwrap(),
        response.as_bytes()
    );
    let written = records.read("response.json", 65536).unwrap();
    assert_eq!(sha, digest(&written));
    let written: Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(
        written,
        json!({"status":"completed","verification":[
            {"command":"cd /work && make test","exit_code":0,"log_path":effects.inner.reference}]})
    );
    // The written result is what the caller's own contract accepts.
    spec.contract()
        .validate(&serde_json::to_vec(&written).unwrap())
        .unwrap();
}

#[tokio::test]
async fn rejected_output_is_kept_and_one_repair_turn_cites_the_same_evidence() {
    let root = Root::new();
    let records = PrivateDirectory::open(&root.0.join("run"), true).unwrap();
    let artifacts = root.0.join("run/artifacts");
    let spec = spec(&schema(), Some("verification"));
    let config = contract::configure(worker_config(), None, &spec).unwrap();
    let fenced = |reference: &str| format!("Done.\n```json\n{}\n```", cite(reference));
    let effects = invocation(
        &artifacts,
        vec![
            Step::Exec("make test"),
            Step::Reply(fenced),
            Step::Reply(cite),
        ],
        0,
    );
    let state = root.0.join("session");
    let session = Session::create(&state, config.clone()).unwrap();
    let (session, response) = turn(session, "Run the check", &effects).await;
    let dir = PrivateDirectory::open(&artifacts, false).unwrap();
    let rejection = contract::save_response(
        &response,
        &records,
        "response.raw.txt",
        &spec,
        &dir,
        effects.lock().await.evidence(),
    )
    .unwrap()
    .unwrap_err();
    assert_eq!(rejection.reason, "invalid_output");
    assert!(!root.0.join("run/response.json").exists());

    // The repair turn runs in the same invocation, so its registry is still live.
    let (session, repaired) = turn(session, &contract::repair_request(&rejection), &effects).await;
    let reference = effects.lock().await.inner.reference.clone();
    contract::save_response(
        &repaired,
        &records,
        "response.repair-1.raw.txt",
        &spec,
        &dir,
        effects.lock().await.evidence(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        records.read("response.raw.txt", 65536).unwrap(),
        response.as_bytes()
    );
    assert_eq!(
        records.read("response.repair-1.raw.txt", 65536).unwrap(),
        repaired.as_bytes()
    );
    let written: Value =
        serde_json::from_slice(&records.read("response.json", 65536).unwrap()).unwrap();
    assert_eq!(written["verification"][0]["command"], "make test");
    assert_eq!(written["verification"][0]["log_path"], reference.as_str());

    // A later invocation resumes the completed session but cannot cite old evidence.
    drop(session);
    let later = invocation(&root.0.join("later/artifacts"), vec![], 0);
    {
        let mut later = later.lock().await;
        later.inner.reference = reference;
        later.inner.steps.push_back(Step::Reply(cite));
    }
    let session = Session::open(&state, &config).unwrap();
    let (_session, stale) = turn(session, "Send the result again", &later).await;
    let later_records = PrivateDirectory::open(&root.0.join("later"), false).unwrap();
    let later_dir = PrivateDirectory::open(&root.0.join("later/artifacts"), false).unwrap();
    let rejection = contract::save_response(
        &stale,
        &later_records,
        "response.raw.txt",
        &spec,
        &later_dir,
        later.lock().await.evidence(),
    )
    .unwrap()
    .unwrap_err();
    assert_eq!(rejection.reason, "evidence_unresolved");
    assert_eq!(rejection.detail, "unknown worker evidence reference");
}

#[tokio::test]
async fn shape_and_evidence_failures_are_typed_rejections() {
    let root = Root::new();
    let artifacts = root.0.join("run/artifacts");
    let spec = spec(&schema(), Some("verification"));
    let config = contract::configure(worker_config(), None, &spec).unwrap();
    // The check fails here, and this caller's contract admits only exit code zero.
    let effects = invocation(
        &artifacts,
        vec![Step::Exec("make test"), Step::Reply(cite)],
        1,
    );
    let session = Session::create(&root.0.join("session"), config).unwrap();
    let (_session, response) = turn(session, "Run the check", &effects).await;
    let dir = PrivateDirectory::open(&artifacts, false).unwrap();
    let effects = effects.lock().await;
    let reference = effects.inner.reference.clone();
    let twice =
        json!({"status":"completed","verification":[{"evidence_ref":reference},{"evidence_ref":reference}]})
            .to_string();
    let transcribed = json!({"status":"completed","verification":[
        {"command":"make test","exit_code":0,"log_path":reference}]})
    .to_string();
    for (name, text, reason) in [
        ("failed-check", response.as_str(), "invalid_output"),
        ("duplicate", twice.as_str(), "evidence_unresolved"),
        ("transcribed", transcribed.as_str(), "invalid_output"),
        (
            "status",
            r#"{"status":"published","verification":[]}"#,
            "invalid_output",
        ),
        (
            "unknown",
            r#"{"status":"completed","verification":[{"evidence_ref":"tool-0-1.json"}]}"#,
            "evidence_unresolved",
        ),
        (
            "trailing",
            r#"{"status":"checkpoint","verification":[]} {}"#,
            "invalid_output",
        ),
    ] {
        let records = PrivateDirectory::open(&root.0.join(name), true).unwrap();
        let rejection = contract::save_response(
            text,
            &records,
            "response.raw.txt",
            &spec,
            &dir,
            effects.evidence(),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(rejection.reason, reason, "{name}: {}", rejection.detail);
        assert_eq!(
            records.read("response.raw.txt", 65536).unwrap(),
            text.as_bytes()
        );
        assert!(!root.0.join(name).join("response.json").exists(), "{name}");
    }
    // Without an evidence property the contract is validated as written.
    let plain = self::spec(&schema(), None);
    let records = PrivateDirectory::open(&root.0.join("plain"), true).unwrap();
    contract::save_response(
        &transcribed,
        &records,
        "response.raw.txt",
        &plain,
        &dir,
        effects.evidence(),
    )
    .unwrap()
    .unwrap();
}

#[test]
fn contract_sessions_are_bound_to_their_contract_and_never_adopt_version_one() {
    let root = Root::new();
    let with_evidence = contract::configure(
        worker_config(),
        None,
        &spec(&schema(), Some("verification")),
    )
    .unwrap();
    let plain = contract::configure(worker_config(), None, &spec(&schema(), None)).unwrap();
    let mut other_schema = schema();
    other_schema["properties"]["status"]["enum"] = json!(["checkpoint"]);
    let other = contract::configure(
        worker_config(),
        None,
        &spec(&other_schema, Some("verification")),
    )
    .unwrap();
    let fingerprints: std::collections::BTreeSet<_> =
        [&worker_config(), &with_evidence, &plain, &other]
            .iter()
            .map(|config| config.fingerprint().unwrap())
            .collect();
    assert_eq!(fingerprints.len(), 4);
    drop(Session::create(&root.0.join("v1"), worker_config()).unwrap());
    assert!(Session::open(&root.0.join("v1"), &with_evidence).is_err());
    drop(Session::create(&root.0.join("contract"), with_evidence.clone()).unwrap());
    assert!(Session::open(&root.0.join("contract"), &worker_config()).is_err());
    assert!(Session::open(&root.0.join("contract"), &plain).is_err());
    Session::open(&root.0.join("contract"), &with_evidence).unwrap();
}

fn task() -> Command {
    Command::new(env!("CARGO_BIN_EXE_playtime-task"))
}
fn describe(arguments: &[&str]) -> Value {
    let output = task()
        .args(arguments)
        .arg("--describe-protocol")
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn version_one_worker_and_read_only_probes_keep_their_pinned_values() {
    use sha2::{Digest, Sha256};
    // Saved version-one worker sessions are bound to these exact bytes.
    assert_eq!(
        Sha256::digest(worker::INSTRUCTIONS.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "b236b07a3828c0f2bef37e1e5bce86b66107122d9e966c544cd062f79f8a2f0b"
    );
    let mut worker = describe(&["--tool-backend", "native"]);
    let added = worker
        .as_object_mut()
        .unwrap()
        .remove("contract_invocation")
        .unwrap();
    assert_eq!(
        added,
        json!({"protocol":"playtime-worker-contract","version":1,
            "capability":"playtime-worker-contract-v1","roles":["worker"],
            "report":"structured-result-v1","output_contract":"json-schema-subset-v1",
            "evidence":"evidence-ref-v1","continuation":"completed-session-only","repair_turns":1,
            "caller_deadline":"unix-ms-v1","transport":"strata-responses-v1",
            "tool_backends":{"docker":"project-docker-v1","native":"project-native-v1"}})
    );
    let helper = worker["native_helper_sha256"].clone();
    let readonly = worker
        .as_object_mut()
        .unwrap()
        .remove("readonly_invocation")
        .unwrap();
    assert_eq!(
        worker,
        json!({"protocol":"playtime-worker","version":1,"roles":["worker"],
            "tools":"project-native-v1",
            "tool_backends":{"docker":"project-docker-v1","native":"project-native-v1"},
            "native_policy_schema":1,"native_helper_sha256":helper,
            "report":"worker-report-v1","continuation":"completed-session-only"})
    );
    assert_eq!(
        readonly["roles"],
        json!(["setup", "supervisor", "reviewer"])
    );
    let reviewer = describe(&["--role", "reviewer"]);
    assert_eq!(
        reviewer,
        json!({"protocol":"playtime-readonly","version":1,"capability":"playtime-readonly-v1",
            "roles":["setup","supervisor","reviewer"],"report":"structured-result-v1",
            "output_contract":"json-schema-subset-v1","continuation":"none",
            "transport":"strata-responses-v1","tools":"project-native-v1",
            "tool_backends":{"native":"project-native-v1"},
            "native_policy_schema":1,"native_helper_sha256":helper})
    );
}

#[test]
fn contract_input_check_binds_request_contract_and_evidence_without_an_endpoint() {
    let root = Root::new();
    let workspace = root.0.join("workspace");
    let run = root.0.join("run");
    PrivateDirectory::open(&workspace, true).unwrap();
    let records = PrivateDirectory::open(&run, true).unwrap();
    let request = b"Run the check";
    let contract = serde_json::to_vec(&schema()).unwrap();
    records.write_new("prompt.txt", request).unwrap();
    records.write_new("schema.json", &contract).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let check = |extra: &[&str]| {
        let mut command = task();
        command
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
            .args(extra);
        command.output().unwrap()
    };
    let (schema_path, schema_hash, request_hash) = (
        run.join("schema.json").display().to_string(),
        digest(&contract),
        digest(request),
    );
    let selected = [
        "--output-contract",
        schema_path.as_str(),
        "--output-contract-sha256",
        schema_hash.as_str(),
        "--request-sha256",
        request_hash.as_str(),
    ];
    let with = |extra: &[&str]| check(&[&selected[..], extra].concat());
    let accepted = with(&["--evidence-property", "verification", "--repair-turns", "1"]);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let accepted: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(accepted["protocol"], "playtime-worker-contract");
    assert_eq!(accepted["inputs_valid"], true);
    assert_eq!(
        accepted["invocation"],
        json!({"capability":"playtime-worker-contract-v1","role":"worker",
            "output_contract_sha256":schema_hash,"request_sha256":request_hash,
            "evidence_property":"verification","repair_turns":1,
            "execution":{"capability":"project-docker-v1"}})
    );
    assert!(!run.join("invocation.json").exists());
    assert!(with(&[]).status.success());
    // A caller deadline is recorded as given, and only when supplied.
    let deadline = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000)
        .to_string();
    let timed = with(&[
        "--repair-turns",
        "1",
        "--caller-deadline-unix-ms",
        &deadline,
    ]);
    assert!(
        timed.status.success(),
        "{}",
        String::from_utf8_lossy(&timed.stderr)
    );
    let timed: Value = serde_json::from_slice(&timed.stdout).unwrap();
    assert_eq!(
        timed["invocation"]["caller_deadline_unix_ms"],
        deadline.parse::<u64>().unwrap()
    );

    let version_one_session = format!("ptw-{}", "a".repeat(64));
    for (name, rejected) in [
        ("two repair turns", with(&["--repair-turns", "2"])),
        (
            "unknown evidence property",
            with(&["--evidence-property", "status"]),
        ),
        (
            "version-one session",
            with(&["--worker-session", version_one_session.as_str()]),
        ),
        (
            "changed request",
            check(&[&selected[..4], &["--request-sha256", schema_hash.as_str()]].concat()),
        ),
        ("missing request hash", check(&selected[..4])),
        (
            "evidence without a contract",
            check(&["--evidence-property", "verification"]),
        ),
        ("repair without a contract", check(&["--repair-turns", "1"])),
        (
            "passed caller deadline",
            with(&["--caller-deadline-unix-ms", "1"]),
        ),
        (
            "caller deadline without a contract",
            check(&["--caller-deadline-unix-ms", deadline.as_str()]),
        ),
        (
            "hash without a contract",
            check(&["--request-sha256", request_hash.as_str()]),
        ),
    ] {
        assert!(!rejected.status.success(), "{name}");
    }
    // The version-one worker still checks the same inputs without any of this.
    let plain = check(&[]);
    assert!(plain.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&plain.stdout).unwrap(),
        json!({"protocol":"playtime-worker","version":1,"inputs_valid":true,
            "execution":{"capability":"project-docker-v1"}})
    );
}
