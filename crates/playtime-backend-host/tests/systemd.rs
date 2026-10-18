//! Opt-in real user-manager fixture; never names or changes ollama.service.
use playtime_backend_guard::{
    digest,
    ports::{Clock, SystemClock},
    random_id,
    wire::*,
};
use playtime_backend_host::systemd::Manager;
use std::{process::Command, time::Duration};
struct Unit(String);
impl Drop for Unit {
    fn drop(&mut self) {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .output();
        let _ = Command::new("systemctl")
            .args(["--user", "reset-failed", &self.0])
            .output();
    }
}

#[tokio::test]
#[ignore = "explicit real systemd listener ownership fixture"]
async fn listener_must_belong_to_original_unit() {
    assert_eq!(
        std::env::var("PLAYTIME_BACKEND_SYSTEMD_TESTS").as_deref(),
        Ok("1")
    );
    let root = std::env::temp_dir().join(format!("pt-listener-{}", random_id().unwrap().as_str()));
    playtime_local::PrivateDirectory::open(&root, true).unwrap();
    let unit = Unit(format!(
        "playtime-backend-fixture-{}.service",
        random_id().unwrap().as_str()
    ));
    let port_file = root.join("port");
    assert!(Command::new("systemd-run").args(["--user","--quiet","--unit",&unit.0,"--property=Restart=no","--property=TimeoutStopSec=1s","--property=KillMode=control-group","/usr/bin/python3","-c","import socket,sys,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); open(sys.argv[1],'w').write(str(s.getsockname()[1])); time.sleep(300)"]).arg(&port_file).status().unwrap().success());
    let manager = Manager::capture(
        &unit.0,
        random_id().unwrap(),
        random_id().unwrap(),
        1,
        digest(b"listener-fixture"),
        true,
    )
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&port_file)
                && let Ok(port) = text.parse::<u16>()
            {
                break port;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    manager.verify_listener(port).await.unwrap();
    let impostor = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    assert!(
        manager
            .verify_listener(impostor.local_addr().unwrap().port())
            .await
            .is_err()
    );
    drop(unit);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "explicit real systemd/cgroup gate; requires a working delegated user manager"]
async fn original_cgroup_and_all_descendants_exit() {
    assert_eq!(
        std::env::var("PLAYTIME_BACKEND_SYSTEMD_TESTS").as_deref(),
        Ok("1")
    );
    let unit = Unit(format!(
        "playtime-backend-fixture-{}.service",
        random_id().unwrap().as_str()
    ));
    let status = Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--unit",
            &unit.0,
            "--property=Restart=no",
            "--property=TimeoutStopSec=1s",
            "--property=KillMode=control-group",
            "/usr/bin/python3",
            "-c",
            "import subprocess,time; subprocess.Popen(['/usr/bin/sleep','300']); time.sleep(300)",
        ])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "real user manager prerequisite is unavailable"
    );
    let clock = SystemClock::new().unwrap();
    let mut manager = Manager::capture(
        &unit.0,
        random_id().unwrap(),
        random_id().unwrap(),
        1,
        digest(b"systemd-fixture"),
        true,
    )
    .await
    .expect("real cgroup handles and systemd 255+ are required");
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.process_count().await.unwrap() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let now = clock.now().ms;
    let op = StopOperation {
        operation_id: random_id().unwrap(),
        backend: manager.identity.clone(),
        lease_id: random_id().unwrap(),
        reason: StopReason::Revoked,
        decided_ms: now,
        supervisor_deadline_ms: now + 90_000,
        observation_deadline_ms: now + 120_000,
    };
    manager.stop(&op).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager.terminated().await.unwrap() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("must establish original-cgroup emptiness and manager termination");
    let replacement = manager
        .next_generation(true)
        .await
        .expect("explicit generation after held original termination");
    assert_eq!(replacement.identity.generation, 2);
    assert_ne!(
        replacement.identity.invocation_id,
        manager.identity.invocation_id
    );
    assert!(
        manager.stop(&op).await.is_err(),
        "old invocation must never stop its replacement"
    );
    replacement.live().await.unwrap();
}

#[tokio::test]
#[ignore = "explicit real systemd deployment rollback fixture"]
async fn deployment_rollback_requires_real_cgroup_cleanup() {
    assert_eq!(
        std::env::var("PLAYTIME_BACKEND_SYSTEMD_TESTS").as_deref(),
        Ok("1")
    );
    let unit = Unit(format!(
        "playtime-backend-fixture-{}.service",
        random_id().unwrap().as_str()
    ));
    assert!(Command::new("systemd-run").args(["--user","--quiet","--unit",&unit.0,"--property=Restart=no","--property=TimeoutStopSec=1s","--property=KillMode=control-group","/usr/bin/python3","-c","import subprocess,time; subprocess.Popen(['/usr/bin/sleep','300']); time.sleep(300)"]).status().unwrap().success());
    let held = Manager::capture(
        &unit.0,
        random_id().unwrap(),
        random_id().unwrap(),
        1,
        digest(b"deployment-fixture"),
        true,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while held.process_count().await.unwrap() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let result=Command::new("python3").current_dir(root).env("PYTHONPATH","tests").env("PLAYTIME_DEPLOY_FIXTURE_UNIT",&unit.0)
        .args(["-m","unittest","test_backend_host_deploy.HostDeploymentTests.test_real_systemd_rollback_waits_for_original_process_tree","-v"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test]
#[ignore = "subprocess entry point for explicitly invoked fixture"]
async fn owner_entry() {
    use playtime_backend_host::host::{Config, OwnerPaths, notify, serve_owner};
    let path = std::env::var_os("PLAYTIME_HOST_FIXTURE_CONFIG").expect("fixture config path");
    let bytes = playtime_local::read_private(std::path::Path::new(&path), 65536).unwrap();
    let (config, unit, state, socket): (Config, String, std::path::PathBuf, std::path::PathBuf) =
        serde_json::from_slice(&bytes).unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    let manager = loop {
        notify(b"WATCHDOG=1").unwrap();
        if let Ok(manager) = Manager::capture(
            &unit,
            config.backend_id.clone(),
            config.guard_id.clone(),
            1,
            config.profile_sha256.clone(),
            true,
        )
        .await
        {
            break manager;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "fixture backend failed to become active"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    serve_owner(manager, config, OwnerPaths { state, socket }, true)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "explicit real systemd owner watchdog and deadline gate"]
async fn independent_owner_loss_pause_and_original_deadline() {
    use playtime_backend_guard::ports::Enforcement;
    use playtime_backend_host::host::{Config, Remote};
    assert_eq!(
        std::env::var("PLAYTIME_BACKEND_SYSTEMD_TESTS").as_deref(),
        Ok("1")
    );
    for case in ["deadline", "guard-loss", "owner-kill", "owner-pause"] {
        let nonce = random_id().unwrap();
        let root = std::env::temp_dir().join(format!("pt-host-{}", nonce.as_str()));
        playtime_local::PrivateDirectory::open(&root, true).unwrap();
        let state = root.join("state");
        playtime_local::PrivateDirectory::open(&state, true).unwrap();
        let socket = root.join("owner.sock");
        let owner = Unit(format!("playtime-owner-fixture-{}.service", nonce.as_str()));
        let backend = Unit(format!(
            "playtime-backend-fixture-{}.service",
            nonce.as_str()
        ));
        let config = Config {
            version: 1,
            backend_id: random_id().unwrap(),
            guard_id: random_id().unwrap(),
            generation: 1,
            uid: rustix::process::geteuid().as_raw(),
            profile_sha256: digest(b"fixture"),
            credential: playtime_backend_guard::capability().unwrap(),
        };
        let path = root.join("config.json");
        playtime_local::replace_private(
            &path,
            &serde_json::to_vec(&(&config, &backend.0, &state, &socket)).unwrap(),
        )
        .unwrap();
        let environment = format!("--setenv=PLAYTIME_HOST_FIXTURE_CONFIG={}", path.display());
        assert!(
            Command::new("systemd-run")
                .args([
                    "--user",
                    "--quiet",
                    "--unit",
                    &owner.0,
                    "--property=Type=exec",
                    "--property=WatchdogSec=2s",
                    "--property=NotifyAccess=main",
                    "--property=TimeoutAbortSec=1s",
                    "--property=TimeoutStopSec=1s",
                    &environment
                ])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "owner_entry", "--ignored", "--nocapture"])
                .status()
                .unwrap()
                .success()
        );
        assert!(Command::new("systemd-run").args(["--user","--quiet","--unit",&backend.0,"--property=Restart=no","--property=TimeoutStopSec=1s","--property=KillMode=control-group"])
            .arg(format!("--property=BindsTo={}",&owner.0)).arg(format!("--property=After={}",&owner.0))
            .args(["/usr/bin/python3","-c","import subprocess,time; subprocess.Popen(['/usr/bin/sleep','300']); time.sleep(300)"]).status().unwrap().success());
        // Exercise the staged guard's dependency policy with a disposable
        // process: backend stop must leave status/release serving available.
        let guard_unit = Unit(format!("playtime-guard-fixture-{}.service", nonce.as_str()));
        assert!(
            Command::new("systemd-run")
                .args(["--user", "--quiet", "--unit", &guard_unit.0])
                .arg(format!("--property=BindsTo={}", owner.0))
                .arg(format!("--property=After={} {}", owner.0, backend.0))
                .args(["/usr/bin/sleep", "300"])
                .status()
                .unwrap()
                .success()
        );
        let remote = Remote {
            socket,
            credential: config.credential,
            owner_uid: config.uid,
        };
        let clock = SystemClock::new().unwrap();
        let identity = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if let Ok(state) = remote.inspect().await {
                    break state.backend;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fixture owner failed to become ready");
        let mut held = Manager::capture(
            &backend.0,
            identity.backend_id.clone(),
            identity.guard_id.clone(),
            1,
            identity.profile_sha256.clone(),
            true,
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while held.process_count().await.unwrap() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let now = clock.now().ms;
        let lease = LeaseSpec {
            operation_id: random_id().unwrap(),
            lease_id: random_id().unwrap(),
            backend: identity,
            service_id: random_id().unwrap(),
            run_id: playtime_core::RunId::new("systemd-fixture").unwrap(),
            manifest_sha256: digest(b"m"),
            worker_id: random_id().unwrap(),
            grant_id: random_id().unwrap(),
            owner_revision: 1,
            execution_deadline_ms: now + 20_000,
            cleanup_deadline_ms: now + 200_000,
        };
        remote.arm_lease(&lease).await.unwrap();
        if case == "deadline" {
            let request = RequestSpec {
                lease_id: lease.lease_id.clone(),
                generation: 1,
                ordinal: 1,
                request_id: random_id().unwrap(),
                effect_id: random_id().unwrap(),
                body_sha256: digest(b"x"),
                body_bytes: 1,
                original_max_tokens: 1,
                deadline_ms: now + 500,
            };
            remote.arm_request(&request).await.unwrap();
        } else if case == "owner-kill" || case == "owner-pause" {
            assert!(
                Command::new("systemctl")
                    .args([
                        "--user",
                        "kill",
                        "--kill-whom=main",
                        if case == "owner-kill" {
                            "--signal=KILL"
                        } else {
                            "--signal=STOP"
                        },
                        &owner.0
                    ])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        // No caller heartbeat: disconnect/guard death is independent of the
        // original lease timer. Owner SIGSTOP is enforced by real systemd.
        tokio::time::timeout(Duration::from_secs(7), async {
            loop {
                if held.terminated().await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{case} did not stop the original process tree"));
        if case == "deadline" {
            assert!(
                clock.now().ms < now + 2000,
                "request bound must not become lease bound"
            );
            assert!(
                Command::new("systemctl")
                    .args(["--user", "is-active", "--quiet", &guard_unit.0])
                    .status()
                    .unwrap()
                    .success(),
                "guard must survive the backend stop for status and release"
            );
        }
        drop(backend);
        drop(owner);
        assert!(
            !Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", &guard_unit.0])
                .status()
                .unwrap()
                .success(),
            "guard must close when the deadline owner is gone"
        );
        drop(guard_unit);
        std::fs::remove_dir_all(root).unwrap();
    }
}
