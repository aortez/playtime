//! Explicit Docker gate for the worker contract invocation, and for the
//! version-one worker on the same task path: the real task binary, the locked
//! workshop image and a scripted loopback model. No live inference.
#[path = "../../playtime-strata/tests/support/mod.rs"]
mod strata;

use playtime_chat::readonly::digest;
use playtime_local::{PrivateDirectory, Token};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const COMMAND: &str = "printf checked > /work/result.txt && cat /work/result.txt";
const REQUEST: &[u8] = b"Run the check and report it.";

/// The newest evidence reference the request carries, as the model would copy it.
fn reference(body: &str) -> String {
    body.match_indices("tool-")
        .filter_map(|(at, _)| {
            let rest = &body[at..];
            let name = rest.get(..rest.find(".json")? + 5)?;
            name.get(5..13)?
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then(|| name.to_owned())
        })
        .last()
        .unwrap_or_default()
}

#[derive(Clone, Copy)]
enum Script {
    /// A check, then a fenced result that must be rejected, then a clean one.
    FencedThenClean,
    /// A check, then a complete version-one worker report.
    VersionOne,
}

fn scripted(script: Script, call: usize, body: &str) -> Vec<u8> {
    let mut result =
        json!({"status":"completed","verification":[{"evidence_ref":reference(body)}]});
    if let Script::VersionOne = script {
        result["summary"] = json!("Checked");
        for key in ["changes", "remaining_work", "questions", "blockers"] {
            result[key] = json!([]);
        }
    }
    let result = result.to_string();
    let mut stream = strata::Stream::new();
    match call {
        0 => {
            stream.item(
                "function_call",
                &json!({"command":COMMAND}).to_string(),
                "completed",
            );
            for item in stream
                .events
                .iter_mut()
                .map(|event| &mut event["item"])
                .chain(stream.output.iter_mut())
            {
                if item["name"] == "read_file" {
                    item["name"] = json!("exec");
                }
            }
        }
        1 if matches!(script, Script::FencedThenClean) => stream.item(
            "message",
            &format!("Done.\n```json\n{result}\n```"),
            "completed",
        ),
        _ => stream.item("message", &result, "completed"),
    }
    stream.terminal("completed");
    stream.bytes()
}

async fn serve(listener: TcpListener, script: Script) {
    let calls = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let calls = calls.clone();
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            let header_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            let (props, status, settings) = strata::metadata();
            let path = header.split_whitespace().nth(1).unwrap();
            let (kind, body) = match path {
                "/v1/responses" => (
                    "text/event-stream",
                    scripted(
                        script,
                        calls.fetch_add(1, Ordering::SeqCst),
                        &String::from_utf8_lossy(&bytes[header_end..]),
                    ),
                ),
                "/props" => ("application/json", serde_json::to_vec(&props).unwrap()),
                "/v1/status" => ("application/json", serde_json::to_vec(&status).unwrap()),
                "/settings" => ("application/json", serde_json::to_vec(&settings).unwrap()),
                "/slots" => ("application/json", b"null".to_vec()),
                _ => panic!("unexpected path {path}"),
            };
            let head = format!(
                "HTTP/1.1 200 fixture\r\nContent-Type: {kind}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            if socket.write_all(head.as_bytes()).await.is_ok() {
                let _ = socket.write_all(&body).await;
            }
        });
    }
}

struct Launch {
    root: PathBuf,
    workspace: PathBuf,
    run: PathBuf,
    output: std::process::Output,
}
impl Launch {
    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.output.stdout.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
}

/// A private root with an empty workspace, and a run directory holding the
/// request, the model lock and `inputs`. Returns the root, workspace and run.
fn prepare(inputs: &[(&str, &[u8])]) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "contract-docker-{}",
        Token::generate().unwrap().expose()
    ));
    PrivateDirectory::open(&root, true).unwrap();
    let (workspace, run) = (root.join("workspace"), root.join("run"));
    PrivateDirectory::open(&workspace, true).unwrap();
    let records = PrivateDirectory::open(&run, true).unwrap();
    records.write_new("prompt.txt", REQUEST).unwrap();
    records
        .write_new(
            "model.lock.json",
            &serde_json::to_vec(&strata::lock()).unwrap(),
        )
        .unwrap();
    for (name, bytes) in inputs {
        records.write_new(name, bytes).unwrap();
    }
    (root, workspace, run)
}

/// The task binary's command line for one Docker run of `prepare`'s files.
fn task(root: &Path, run: &Path, endpoint: &str, image_lock: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_playtime-task"));
    command
        .current_dir(run)
        .arg("--endpoint")
        .arg(endpoint)
        .args(["--model-lock", "model.lock.json"])
        .arg("--image-lock")
        .arg(image_lock)
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--workspace")
        .arg(root.join("workspace"))
        .arg("--run-dir")
        .arg(run)
        .args(["--request", "prompt.txt", "--wall-seconds", "120"])
        .stdin(std::process::Stdio::null());
    command
}

/// Run the task binary once against Docker and the scripted model. `inputs` are
/// extra private files for the run directory; `arguments` may name them.
async fn launch(
    script: Script,
    succeeds: bool,
    inputs: &[(&str, &[u8])],
    arguments: &[&str],
) -> Launch {
    let (root, workspace, run) = prepare(inputs);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(serve(listener, script));
    let image_lock = repository().join("environments/general.lock.json");
    let output = tokio::time::timeout(
        Duration::from_secs(240),
        task(&root, &run, &endpoint, &image_lock)
            .args(arguments)
            .output(),
    )
    .await
    .expect("task deadline")
    .unwrap();
    server.abort();
    assert_eq!(
        output.status.success(),
        succeeds,
        "evidence retained in {}: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    // Every turn confirmed cleanup, so no project reservation remains.
    assert!(!root.join("state").join("active").exists());
    assert_eq!(
        std::fs::read(workspace.join("result.txt")).unwrap(),
        b"checked"
    );
    Launch {
        root,
        workspace,
        run,
        output,
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn verification_contract() -> Vec<u8> {
    serde_json::to_vec(&json!({"type":"object","additionalProperties":false,
        "required":["status","verification"],
        "properties":{"status":{"type":"string","enum":["checkpoint","completed"]},
            "verification":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,
                "required":["command","exit_code","log_path"],
                "properties":{"command":{"type":"string"},"exit_code":{"type":"integer"},"log_path":{"type":"string"}}}}}}))
    .unwrap()
}

/// The wall-clock time `after` from now, in Unix milliseconds.
fn unix_ms_after(after: Duration) -> u64 {
    (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + after).as_millis() as u64
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit Docker gate; needs the locked workshop image and no live model"]
async fn rejected_report_is_repaired_in_a_second_owned_container_and_bound_in_receipts() {
    let contract = verification_contract();
    // Far enough away that the repair turn could use its whole budget.
    let deadline = unix_ms_after(Duration::from_secs(3600));
    let launched = launch(
        Script::FencedThenClean,
        true,
        &[("schema.json", &contract)],
        &[
            "--output-contract",
            "schema.json",
            "--output-contract-sha256",
            &digest(&contract),
            "--request-sha256",
            &digest(REQUEST),
            "--evidence-property",
            "verification",
            "--repair-turns",
            "1",
            "--caller-deadline-unix-ms",
            &deadline.to_string(),
        ],
    )
    .await;
    let run = &launched.run;

    let result = read(&run.join("invocation-result.json"));
    let written = std::fs::read(run.join("response.json")).unwrap();
    assert_eq!(result["type"], "invocation_finished");
    assert_eq!(result["cleanup"]["status"], "complete");
    assert_eq!(result["response_sha256"], digest(&written));
    assert_eq!(result["repair_turns_used"], 1);
    assert_eq!(result["rejected"][0]["reason"], "invalid_output");
    // The rejected turn's own cleanup is receipt evidence, not an inference.
    assert_eq!(result["rejected"][0]["cleanup"], result["cleanup"]);
    assert_ne!(result["rejected"][0]["turn_id"], result["turn_id"]);
    assert!(result["session_id"].as_str().unwrap().starts_with("ptc-"));
    assert_eq!(
        result["invocation"],
        json!({"capability":"playtime-worker-contract-v1","role":"worker",
            "output_contract_sha256":digest(&contract),"request_sha256":digest(REQUEST),
            "evidence_property":"verification","repair_turns":1,
            "caller_deadline_unix_ms":deadline,"execution":{"capability":"project-docker-v1"}})
    );
    assert_eq!(
        read(&run.join("invocation.json"))["invocation"],
        result["invocation"]
    );
    assert_eq!(std::fs::read(run.join("request.txt")).unwrap(), REQUEST);
    assert_eq!(
        std::fs::read(run.join("output-contract.json")).unwrap(),
        contract
    );

    // The written result carries what the harness recorded, not model transcription.
    let written: Value = serde_json::from_slice(&written).unwrap();
    let check = &written["verification"][0];
    assert_eq!(written["status"], "completed");
    assert_eq!(check["command"], COMMAND);
    assert_eq!(check["exit_code"], 0);
    let log = read(
        &run.join("artifacts")
            .join(check["log_path"].as_str().unwrap()),
    );
    assert_eq!(log["command"], COMMAND);
    assert_eq!(log["result"]["stdout"], "checked");
    let rejected = std::fs::read_to_string(run.join("response.raw.txt")).unwrap();
    assert!(rejected.starts_with("Done.\n```json"));
    let repaired = std::fs::read_to_string(run.join("response.repair-1.raw.txt")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&repaired).unwrap()["verification"][0]["evidence_ref"],
        check["log_path"]
    );

    // The stream uses only the invocation family and ends at its terminal event.
    let events = launched.events();
    assert_eq!(events[0]["type"], "invocation_started");
    assert_eq!(events[0]["invocation"], result["invocation"]);
    assert_eq!(events.last().unwrap(), &result);
    assert!(
        events[1..events.len() - 1]
            .iter()
            .all(|event| event["type"] == "runtime" && event["schema_version"] == 1)
    );
    assert!(launched.workspace.join("result.txt").exists());
    std::fs::remove_dir_all(&launched.root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit Docker gate; needs the locked workshop image and no live model"]
async fn repair_turn_that_would_outlast_the_caller_deadline_never_starts() {
    let contract = verification_contract();
    // The caller would stop the worker before a whole 120-second turn could run.
    let deadline = unix_ms_after(Duration::from_secs(100));
    let launched = launch(
        Script::FencedThenClean,
        false,
        &[("schema.json", &contract)],
        &[
            "--output-contract",
            "schema.json",
            "--output-contract-sha256",
            &digest(&contract),
            "--request-sha256",
            &digest(REQUEST),
            "--evidence-property",
            "verification",
            "--repair-turns",
            "1",
            "--caller-deadline-unix-ms",
            &deadline.to_string(),
        ],
    )
    .await;
    let run = &launched.run;
    let result = read(&run.join("invocation-result.json"));
    assert_eq!(result["type"], "invocation_failed");
    assert_eq!(result["reason"], "invalid_output");
    assert_eq!(result["repair_skipped"], "caller_deadline");
    assert_eq!(result["repair_turns_used"], 0);
    assert_eq!(result["cleanup"]["status"], "complete");
    assert_eq!(result["invocation"]["caller_deadline_unix_ms"], deadline);
    assert_eq!(
        read(&run.join("invocation.json"))["invocation"],
        result["invocation"]
    );
    let events = launched.events();
    assert_eq!(events[0]["invocation"], result["invocation"]);
    assert_eq!(events.last().unwrap(), &result);
    // Only the rejected turn ran, and its completed session stays resumable.
    assert!(run.join("response.raw.txt").exists());
    assert!(!run.join("response.repair-1.raw.txt").exists());
    assert!(!run.join("response.json").exists());
    assert!(String::from_utf8_lossy(&launched.output.stderr).contains("repair turn skipped"));
    std::fs::remove_dir_all(&launched.root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit Docker gate; needs the locked workshop image and no live model"]
async fn version_one_worker_keeps_its_stream_files_and_report_on_the_shared_path() {
    let launched = launch(Script::VersionOne, true, &[], &[]).await;
    let run = &launched.run;
    let result = read(&run.join("worker-result.json"));
    let report = std::fs::read(run.join("worker-report.json")).unwrap();
    assert_eq!(
        result
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "cleanup",
            "report_sha256",
            "schema_version",
            "session_id",
            "status",
            "turn_id",
            "type",
            "usage"
        ]
    );
    assert_eq!(result["type"], "worker_finished");
    assert_eq!(result["status"], "completed");
    assert_eq!(result["report_sha256"], digest(&report));
    assert!(result["session_id"].as_str().unwrap().starts_with("ptw-"));
    let report: Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["verification"][0]["command"], COMMAND);
    assert_eq!(report["verification"][0]["exit_code"], 0);
    assert_eq!(
        read(&run.join("worker-invoked.json"))
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "configuration",
            "execution",
            "resume",
            "schema_version",
            "session_id"
        ]
    );
    let events = launched.events();
    assert_eq!(events[0]["type"], "worker_started");
    assert_eq!(events.last().unwrap(), &result);
    assert!(
        events[1..events.len() - 1]
            .iter()
            .all(|event| event["type"] == "runtime")
    );
    // Nothing from the contract invocation appears in a version-one run directory.
    let mut names: Vec<_> = std::fs::read_dir(run)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ".lock",
            "artifacts",
            "model.lock.json",
            "prompt.txt",
            "worker-events.jsonl",
            "worker-invoked.json",
            "worker-report.json",
            "worker-report.raw.txt",
            "worker-result.json"
        ]
    );
    std::fs::remove_dir_all(&launched.root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "explicit Docker gate; needs a reachable Docker daemon and no live model"]
async fn absent_image_stops_before_any_model_request_or_session() {
    let mut lock = read(&repository().join("environments/general.lock.json"));
    lock["image_id"] = json!(format!("sha256:{}", "0".repeat(64)));
    let (root, _, run) = prepare(&[("absent.lock.json", &serde_json::to_vec(&lock).unwrap())]);
    // Bound but never served: any model request would wait in its backlog.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        task(&root, &run, &endpoint, &run.join("absent.lock.json")).output(),
    )
    .await
    .expect("task deadline")
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("locked image absent"),
        "{stderr}"
    );
    // Admission finished; Docker then failed before the model or a session.
    assert!(run.join("worker-invoked.json").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    let sessions = std::fs::read_dir(root.join("state"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("ptw-"))
        .count();
    assert_eq!(sessions, 0);
    std::fs::remove_dir_all(&root).unwrap();
}
