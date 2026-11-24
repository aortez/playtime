//! A scripted launcher package lends the Strata fixture server, whose metadata
//! is the runner's synthetic fixture lock. No host, SSH or model is involved.
use super::*;
#[path = "../../../playtime-strata/tests/support/mod.rs"]
mod strata_fixture;
use serde_json::json;
use strata_fixture::server::Server;

const LOCK: &str = "fixture-strata-gyro-m";
const PROFILE: &str = "fixture-gyro-m";
/// The launcher's hold protocol as the fixture plays it: `fixture.json` says
/// what each command reports, and every hold appends what its caller did to
/// `calls.jsonl`. Like the real hold, it reads stdin to EOF; `linger` then
/// delays its ending, as stopping a server does.
const LAUNCHER: &str = r#"
import json, os, sys, time
from pathlib import Path
here = Path(__file__).resolve().parent
script = json.loads((here / "fixture.json").read_text())
def record(entry):
    with (here / "calls.jsonl").open("a") as calls:
        calls.write(json.dumps(entry) + "\n")
args = sys.argv[1:]
if args[:1] == ["--status"]:
    print(json.dumps(script["status"]))
    sys.exit(0)
profile, command = args[0], args[1:]
if command == ["--describe-hold"]:
    print(json.dumps(script["describe"]))
    sys.exit(0)
assert command == ["--hold"], command
def emit(event, **fields):
    print(json.dumps({"schema_version": 1, "protocol": "hold_v1", "event": event,
                      "profile": profile, **fields}), flush=True)
hold = script["hold"]
record({"call": "hold", "path": os.environ.get("PATH"), "python": sys.executable})
if hold["first"] == "refused":
    emit("refused", reason=hold["reason"], message="fixture refusal", inference_owner="not_acquired")
    sys.exit(1)
if hold["first"] == "wait":
    data = sys.stdin.buffer.read()
    record({"call": "abandoned", "input": data.decode()})
    emit("refused", reason="abandoned", message="ended before held", inference_owner="confirmed")
    sys.exit(1)
emit("held", endpoint=hold["endpoint"], model_lock=hold["model_lock"],
     model_lock_sha256=hold["model_lock_sha256"], ready={}, report=hold["report"],
     helper_logs="/fixture", evidence=str(here), timings={})
data = sys.stdin.buffer.read()
ending = {b"release\n": "released", b"": "closed"}.get(data, "protocol_error")
time.sleep(hold.get("linger", 0))
record({"call": "ended", "ending": ending})
emit("ended", ending=ending, released=ending == "released", inference_owner="confirmed", owner_exit=0)
"#;

struct Package {
    root: PathBuf,
    server: Server,
}
impl Package {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pt-strata-hold-{}",
            playtime_worker_control::random_id().unwrap().as_str()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("run-strata.py"), LAUNCHER).unwrap();
        std::fs::write(
            root.join(format!("{LOCK}.lock.json")),
            include_bytes!("../../../playtime-runner/src/docker/fixture-strata-lock.json"),
        )
        .unwrap();
        let package = Self {
            root,
            server: Server::new([]),
        };
        package.script(|_| {});
        package
    }
    /// The fixture's script: a hold that lends the fixture server, edited by `change`.
    fn script(&self, change: impl FnOnce(&mut Value)) {
        let lock = playtime_runner::docker::strata_lock(LOCK).unwrap();
        let mut script = json!({
            "describe": {"schema_version": 1, "protocol": "hold_v1", "profile": PROFILE,
                "model_lock": format!("{LOCK}.lock.json"), "startup_timeout": 30,
                "verify_timeout": 30, "cleanup_timeout": 5},
            "status": {"state": "ready"},
            "hold": {"first": "held", "endpoint": self.server.endpoint,
                "model_lock": format!("{LOCK}.lock.json"),
                "model_lock_sha256": playtime_runner::docker::strata_lock_sha256(LOCK).unwrap(),
                "report": {"host": lock.host, "verification": {"hashed_files": 1}}},
        });
        change(&mut script);
        std::fs::write(self.root.join("fixture.json"), script.to_string()).unwrap();
    }
    fn config(&self) -> InferenceTarget {
        InferenceTarget {
            id: "fixture-strata".into(),
            name: "Fixture Strata".into(),
            provider: playtime_protocol::api::InferenceProvider::Strata,
            endpoint: String::new(),
            profile: Some(PROFILE.into()),
        }
    }
    async fn target(&self) -> life::Result<Arc<StrataTarget>> {
        StrataTarget::describe(self.config(), &self.root, self.root.join("live-model.lock"))
            .await
            .map(Arc::new)
    }
    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn log(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.log"))
    }
    fn busy(&self, in_flight: u64) {
        self.server.state.lock().unwrap().status["activity"]["in_flight"] = json!(in_flight);
    }
}
impl Drop for Package {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn run(name: &str) -> RunId {
    RunId::new(name).unwrap()
}

#[tokio::test]
async fn description_needs_the_reviewed_lock_in_the_launcher_package() {
    let package = Package::new();
    let target = package.target().await.unwrap();
    assert_eq!(target.lock_name, LOCK);
    assert_eq!(target.profile, PROFILE);
    // The launcher's own budgets, plus the daemon's margins.
    assert_eq!(
        target.acquire,
        Duration::from_secs(30 + 30) + STATUS_DEADLINE + MARGIN
    );
    assert_eq!(target.cleanup, Duration::from_secs(5) + MARGIN);
    let template = target.template();
    assert_eq!(template.target.as_deref(), Some(&package.config()));
    assert_eq!((template.endpoint.as_str(), template.started), ("", 0));
    for (change, reason) in [
        (
            json!({"protocol": "hold_v2"}),
            "Strata launcher unavailable",
        ),
        (json!({"profile": "other"}), "Strata launcher unavailable"),
        (json!({"cleanup_timeout": 0}), "Strata launcher unavailable"),
        (
            json!({"model_lock": "bird-strata-gyro-m-fast-v2.lock.json"}),
            "the Strata target's lock isn't a reviewed one",
        ),
    ] {
        package.script(|script| {
            for (key, value) in change.as_object().unwrap() {
                script["describe"][key] = value.clone();
            }
        });
        assert!(
            matches!(package.target().await, Err(Error::Invalid(r)) if r == reason),
            "{change}"
        );
    }
    package.script(|_| {});
    let path = package.root.join(format!("{LOCK}.lock.json"));
    let mut lock = std::fs::read(&path).unwrap();
    lock.push(b'\n');
    std::fs::write(&path, lock).unwrap();
    assert!(matches!(
        package.target().await,
        Err(Error::Invalid(
            "the Strata launcher's lock differs from the reviewed one"
        ))
    ));
}

#[tokio::test]
async fn status_reads_the_helpers_report() {
    let package = Package::new();
    let target = package.target().await.unwrap();
    assert!(target.status().await.holdable());
    package.script(|script| {
        script["status"] = json!({"state": "busy", "message": "held by\u{7}another client"});
    });
    let status = target.status().await;
    assert!(!status.holdable());
    assert_eq!(status.state, "busy");
    assert_eq!(status.message.as_deref(), Some("held byanother client"));
    package.script(|script| script["status"] = json!("not a report"));
    assert_eq!(target.status().await.state, "unknown");
}

#[tokio::test]
async fn a_hold_lends_the_verified_server_and_parks_it_only_when_a_completed_run_left_it_idle() {
    let package = Package::new();
    let target = package.target().await.unwrap();
    let holds = Holds::default();
    let first = run("run-1");
    let host = holds
        .acquire(&first, target.clone(), &package.log("run-1"))
        .await
        .unwrap();
    assert_eq!(host.endpoint, package.server.endpoint);
    // The fixture server's start identity, read after the hold verified it.
    assert_eq!(host.started, 100);
    assert_eq!(host.target.as_deref(), Some(&package.config()));
    assert_eq!(host.verification["verification"]["hashed_files"], 1);
    assert_eq!(holds.active(&first).as_deref(), Some(PROFILE));
    let profile = playtime_runner::docker::Profile::freeze_strata_live(
        playtime_docker::transport::Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        },
        playtime_docker::DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        },
        LOCK,
        host,
    )
    .unwrap();
    profile.validate().unwrap();
    holds.end(&first, true).await;
    assert_eq!(holds.active(&first), None);
    // Every other ending stops the server: a run that didn't complete, and a
    // completed one whose server still has work.
    let second = run("run-2");
    holds
        .acquire(&second, target.clone(), &package.log("run-2"))
        .await
        .unwrap();
    holds.end(&second, false).await;
    let third = run("run-3");
    holds
        .acquire(&third, target.clone(), &package.log("run-3"))
        .await
        .unwrap();
    package.busy(1);
    holds.end(&third, true).await;
    let endings: Vec<_> = package
        .calls()
        .into_iter()
        .filter(|c| c["call"] == "ended")
        .map(|c| c["ending"].clone())
        .collect();
    assert_eq!(endings, ["released", "closed", "closed"]);
    // The hold's own messages stay in its private log.
    assert!(package.log("run-1").is_file());
    // The launcher gets the system's Python and commands, whatever the
    // daemon's own PATH.
    let started = &package.calls()[0];
    assert_eq!(started["call"], "hold");
    assert_eq!(
        (started["path"].as_str(), started["python"].as_str()),
        (Some(LAUNCHER_PATH), Some(PYTHON))
    );
}

#[tokio::test]
async fn a_refused_or_unverifiable_hold_lends_nothing_and_stops_what_it_held() {
    let package = Package::new();
    let target = package.target().await.unwrap();
    let holds = Holds::default();
    package.script(|script| {
        script["hold"] = json!({"first": "refused", "reason": "busy"});
    });
    assert!(matches!(
        holds
            .acquire(&run("busy"), target.clone(), &package.log("busy"))
            .await,
        Err(Error::Busy)
    ));
    package.script(|script| {
        script["hold"] = json!({"first": "refused", "reason": "verification_failed"});
    });
    assert!(matches!(
        holds
            .acquire(
                &run("unverified"),
                target.clone(),
                &package.log("unverified")
            )
            .await,
        Err(Error::Invalid("Strata host unavailable"))
    ));
    // A hold the daemon can't use is closed, so the helper stops the server.
    for (name, change) in [
        ("other-lock", json!({"model_lock_sha256": "2".repeat(64)})),
        (
            "other-host",
            json!({"report": {"host": {"backend": "other"}}}),
        ),
        ("remote", json!({"endpoint": "http://192.0.2.1:8080/v1"})),
        ("unreachable", json!({"endpoint": "http://127.0.0.1:9/v1"})),
    ] {
        package.script(|script| {
            for (key, value) in change.as_object().unwrap() {
                script["hold"][key] = value.clone();
            }
        });
        assert!(
            matches!(
                holds
                    .acquire(&run(name), target.clone(), &package.log(name))
                    .await,
                Err(Error::Invalid("Strata host unavailable"))
            ),
            "{name}"
        );
    }
    package.script(|_| {});
    package.busy(1);
    assert!(matches!(
        holds
            .acquire(&run("working"), target.clone(), &package.log("working"))
            .await,
        Err(Error::Busy)
    ));
    let endings: Vec<_> = package
        .calls()
        .into_iter()
        .filter(|c| c["call"] == "ended")
        .map(|c| c["ending"].clone())
        .collect();
    assert_eq!(endings, ["closed"; 5]);
    assert!(holds.entries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelling_a_run_abandons_a_hold_that_isnt_held_yet() {
    let package = Package::new();
    let target = package.target().await.unwrap();
    package.script(|script| script["hold"] = json!({"first": "wait"}));
    let holds = Arc::new(Holds::default());
    let id = run("cancelled");
    let acquiring = {
        let (holds, id, log) = (holds.clone(), id.clone(), package.log("cancelled"));
        tokio::spawn(async move { holds.acquire(&id, target, &log).await })
    };
    let started = Instant::now();
    while !package.calls().iter().any(|c| c["call"] == "hold") {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the hold didn't start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(holds.active(&id).as_deref(), Some(PROFILE));
    holds.cancel(&id);
    assert!(matches!(
        acquiring.await.unwrap(),
        Err(Error::Invalid("Strata hold cancelled"))
    ));
    assert_eq!(holds.active(&id), None);
    let abandoned = package.calls();
    assert_eq!(
        abandoned.last().unwrap(),
        &json!({"call": "abandoned", "input": ""})
    );
}

#[tokio::test]
async fn the_daemon_stopping_waits_for_every_hold_and_starts_no_other() {
    let package = Package::new();
    package.script(|script| script["hold"]["linger"] = json!(0.5));
    let target = package.target().await.unwrap();
    let holds = Arc::new(Holds::default());
    for name in ["one", "two"] {
        holds
            .acquire(&run(name), target.clone(), &package.log(name))
            .await
            .unwrap();
    }
    // A run's own ending is still under way as the daemon stops. Until its
    // helper exits, the hold is still the run's.
    let ending = {
        let holds = holds.clone();
        tokio::spawn(async move { holds.end(&run("one"), true).await })
    };
    let started = Instant::now();
    while !matches!(
        holds.entries.lock().unwrap().get(&run("one")),
        Some(Entry::Ending(_))
    ) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the ending didn't start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(holds.active(&run("one")).as_deref(), Some(PROFILE));
    assert!(holds.busy());
    holds.end_all().await;
    assert!(!holds.busy());
    assert!(ending.is_finished());
    let mut endings: Vec<_> = package
        .calls()
        .into_iter()
        .filter(|c| c["call"] == "ended")
        .map(|c| c["ending"].as_str().unwrap().to_owned())
        .collect();
    endings.sort();
    assert_eq!(endings, ["closed", "released"]);
    assert!(matches!(
        holds
            .acquire(&run("three"), target, &package.log("three"))
            .await,
        Err(Error::Invalid("Strata hold cancelled"))
    ));
    assert_eq!(
        package
            .calls()
            .iter()
            .filter(|c| c["call"] == "hold")
            .count(),
        2
    );
}
