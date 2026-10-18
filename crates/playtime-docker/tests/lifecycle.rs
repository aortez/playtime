mod support;
use playtime_docker::{transport::*, *};
use serde_json::json;
use support::*;

#[tokio::test]
async fn ownership_precedes_mutations_and_session_reset_keeps_absolute_deadlines() {
    let mut f = Fixture::running().await;
    let proposals = f.sandbox.proposals();
    assert_eq!(proposals.len(), 3);
    for p in &proposals {
        assert_eq!(p.labels()["io.playtime.service"], "synthetic-service");
        assert_eq!(p.labels()["io.playtime.run"], "synthetic-run");
        assert_eq!(p.labels()["io.playtime.grant"], "synthetic-grant");
        assert_eq!(p.labels()["io.playtime.invocation"], "synthetic-invocation");
        assert_eq!(
            p.labels()["io.playtime.nonce"],
            "0123456789abcdef0123456789abcdef"
        );
        assert!(p.name.len() < 64);
    }
    let original = proposals
        .iter()
        .find(|p| p.role == Role::Agent)
        .unwrap()
        .deadline_monotonic_ns;
    assert_eq!(
        proposals
            .iter()
            .find(|p| p.role == Role::Holder)
            .unwrap()
            .deadline_monotonic_ns,
        original + 180_000_000_000
    );
    assert!(matches!(
        f.tool().await.unwrap(),
        ToolReply::Exec {
            exit_code: 0,
            timed_out: false,
            output_truncated: false,
            duration_seconds: 0.0,
            ..
        }
    ));
    {
        let stopped = f
            .sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .unwrap();
        assert!(!stopped.holder_id().is_empty());
        assert!(!stopped.volume_name().is_empty());
    }
    assert_eq!(
        f.fake.resources(),
        2,
        "holder and volume retained for capture"
    );
    f.sandbox
        .start_session(2, deadline(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        f.sandbox
            .proposals()
            .iter()
            .find(|p| p.role == Role::Agent)
            .unwrap()
            .deadline_monotonic_ns,
        original
    );
    assert!(f.cleanup().await.complete());
    assert_eq!(f.fake.resources(), 0);
}

#[tokio::test]
async fn lost_create_and_start_replies_are_reconciled_without_replay() {
    for op in ["volume_create", "container_create", "container_start"] {
        for injected in [Fault::Io, Fault::Deadline, Fault::Cancelled] {
            let mut f = Fixture::new().await;
            f.fake.inject(op, Injection::Lost(injected));
            assert!(
                f.sandbox
                    .initialize(deadline(), CancellationToken::new())
                    .await
                    .is_err()
            );
            let count = f.fake.seen().len();
            assert!(
                f.sandbox
                    .initialize(deadline(), CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(f.fake.seen().len(), count);
            assert!(f.cleanup().await.complete(), "{op} {injected:?}");
            assert_eq!(f.fake.resources(), 0);
            assert_eq!(f.fake.seen().iter().filter(|o| o.name() == op).count(), 1);
        }
    }
    for op in ["container_create", "container_start"] {
        let mut f = Fixture::initialized().await;
        f.fake.inject(op, Injection::Lost(Fault::Io));
        assert!(
            f.sandbox
                .start_session(1, deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        assert!(f.sandbox.is_fenced());
        assert!(f.tool().await.is_err());
        assert!(f.cleanup().await.complete());
        assert_eq!(f.fake.resources(), 0);
    }
}

#[tokio::test]
async fn uncertain_or_malformed_tool_reply_fences_until_confirmed_agent_absence() {
    for injection in [
        Injection::Lost(Fault::Deadline),
        Injection::Lost(Fault::Cancelled),
        Injection::Lost(Fault::Io),
        Injection::Malformed,
    ] {
        let mut f = Fixture::running().await;
        f.fake.inject("tool", injection);
        assert!(f.tool().await.is_err());
        assert!(f.sandbox.is_fenced());
        assert!(f.tool().await.is_err());
        assert_eq!(
            f.fake.seen().iter().filter(|o| o.name() == "tool").count(),
            1
        );
        assert!(
            f.sandbox
                .start_session(2, deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        f.sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(f.fake.resources(), 2);
        f.sandbox
            .start_session(2, deadline(), CancellationToken::new())
            .await
            .unwrap();
        assert!(f.tool().await.is_ok());
        assert!(f.cleanup().await.complete());
    }
}

#[tokio::test]
async fn lost_remove_reply_needs_positive_absence_and_failed_daemon_is_never_empty() {
    let mut f = Fixture::running().await;
    f.fake
        .inject("container_remove", Injection::Lost(Fault::Io));
    f.sandbox
        .quiesce(deadline(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(f.fake.resources(), 2);
    assert!(f.cleanup().await.complete());
    for changed in [false, true] {
        let mut f = Fixture::running().await;
        {
            let mut s = f.fake.state.lock().unwrap();
            if changed {
                s.changed = true;
            } else {
                s.available = false;
            }
        }
        let report = f.cleanup().await;
        assert!(!report.complete());
        assert_eq!(report.resources.len(), 3);
        assert!(
            report
                .resources
                .iter()
                .all(|r| matches!(r, ResourceOutcome::Residual { .. }))
        );
        assert_eq!(f.fake.resources(), 3);
        assert!(!f.fake.seen().iter().any(|o| matches!(
            o,
            Operation::RemoveContainer(_) | Operation::RemoveVolume(_)
        )));
    }
}

#[tokio::test]
async fn same_name_foreign_resource_is_never_reused_or_removed() {
    let mut f = Fixture::new().await;
    // Obtain the deterministic proposal through a cancelled acknowledgement;
    // a second independently constructed fixture has the same synthetic owner.
    f.fake.inject("volume_create", Injection::Lost(Fault::Io));
    assert!(
        f.sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    let name = f.sandbox.proposals()[0].name.clone();
    let mut other = Fixture::new().await;
    other
        .fake
        .state
        .lock()
        .unwrap()
        .volumes
        .insert(name, json!({"Labels":{"unrelated":"resource"}}));
    assert_eq!(
        other
            .sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap_err()
            .failure,
        Failure::Conflict
    );
    assert!(other.cleanup().await.complete());
    assert_eq!(other.fake.resources(), 1);
    assert!(
        !other
            .fake
            .seen()
            .iter()
            .any(|o| matches!(o, Operation::CreateVolume(_) | Operation::RemoveVolume(_)))
    );
    assert!(f.cleanup().await.complete());
}

#[tokio::test]
async fn every_effective_sandbox_requirement_is_checked_before_model_work() {
    let mutations = vec![
        ("/Image", json!("sha256:other")),
        ("/Config/User", json!("1000:1000")),
        ("/Config/Entrypoint", json!(["/bin/sh"])),
        ("/Config/Cmd", json!(["sleep", "infinity"])),
        ("/Config/WorkingDir", json!("/work")),
        ("/Config/Healthcheck/Test", json!(["CMD", "evil"])),
        ("/Config/Volumes", json!({"/host":{}})),
        ("/Config/Labels/io.playtime.nonce", json!("other")),
        ("/HostConfig/NetworkMode", json!("host")),
        ("/HostConfig/ReadonlyRootfs", json!(false)),
        ("/HostConfig/Privileged", json!(true)),
        ("/HostConfig/CapDrop", json!([])),
        ("/HostConfig/CapAdd", json!(["SYS_ADMIN"])),
        ("/HostConfig/SecurityOpt", json!([])),
        ("/HostConfig/LogConfig/Type", json!("json-file")),
        (
            "/HostConfig/LogConfig/Config",
            json!({"unexpected":"option"}),
        ),
        ("/HostConfig/RestartPolicy/MaximumRetryCount", json!(1)),
        ("/HostConfig/RestartPolicy/Name", json!("always")),
        ("/HostConfig/NanoCpus", json!(0)),
        ("/HostConfig/Memory", json!(0)),
        ("/HostConfig/MemorySwap", json!(-1)),
        ("/HostConfig/PidsLimit", json!(-1)),
        ("/HostConfig/ShmSize", json!(0)),
        ("/HostConfig/CgroupnsMode", json!("host")),
        ("/HostConfig/IpcMode", json!("host")),
        ("/HostConfig/PidMode", json!("host")),
        ("/HostConfig/UTSMode", json!("host")),
        ("/HostConfig/UsernsMode", json!("host")),
        ("/HostConfig/Tmpfs", json!({})),
        ("/HostConfig/Binds", json!(["/:/host"])),
        ("/HostConfig/VolumesFrom", json!(["foreign"])),
        ("/HostConfig/Devices", json!([{}])),
        ("/HostConfig/DeviceRequests", json!([{}])),
        ("/HostConfig/DeviceCgroupRules", json!(["a *:* rwm"])),
        ("/HostConfig/GroupAdd", json!(["0"])),
        ("/HostConfig/ExtraHosts", json!(["host-gateway"])),
        ("/HostConfig/OomKillDisable", json!(true)),
        ("/HostConfig/PortBindings", json!({"80/tcp":[{}]})),
        ("/HostConfig/Links", json!(["foreign"])),
        ("/HostConfig/PublishAllPorts", json!(true)),
        ("/HostConfig/AutoRemove", json!(true)),
        ("/HostConfig/Mounts/0/Source", json!("foreign")),
        ("/HostConfig/Mounts/0/VolumeOptions/NoCopy", json!(false)),
        ("/Mounts/0/Type", json!("bind")),
        ("/Mounts/0/Destination", json!("/host")),
        ("/Mounts/0/RW", json!(false)),
        ("/NetworkSettings/Networks", json!({"none":{},"bridge":{}})),
        ("/NetworkSettings/Networks/none/Gateway", json!("192.0.2.1")),
        (
            "/HostConfig/Mounts/0/VolumeOptions/DriverConfig",
            json!({"Name":"foreign"}),
        ),
        (
            "/Config/Env",
            json!(["PYTHONHASHSEED=0", "PLAYTIME_SEED=1042"]),
        ),
    ];
    for (pointer, value) in mutations {
        let mut f = Fixture::running().await;
        f.fake.state.lock().unwrap().mutate = Some((pointer, value));
        assert_eq!(
            f.tool().await.unwrap_err().failure,
            Failure::Conflict,
            "{pointer}"
        );
        assert!(
            !f.fake
                .seen()
                .iter()
                .any(|o| matches!(o, Operation::Bridge { .. })),
            "{pointer}"
        );
        let report = f.cleanup().await;
        assert!(!report.complete(), "{pointer}");
        assert!(
            !f.fake
                .seen()
                .iter()
                .any(|o| matches!(o, Operation::RemoveContainer(_))),
            "{pointer}"
        );
    }
}

#[tokio::test]
async fn failure_to_commit_ownership_sends_no_mutations() {
    let mut f = Fixture::new().await;
    *f.journal.fail.lock().unwrap() = true;
    assert_eq!(
        f.sandbox
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap_err()
            .failure,
        Failure::Journal
    );
    assert!(!f.fake.seen().iter().any(|o| matches!(
        o,
        Operation::CreateVolume(_) | Operation::CreateContainer(_)
    )));
    assert!(f.cleanup().await.complete());
}

#[tokio::test]
async fn failed_agent_stop_retains_holder_and_blocks_new_session() {
    let mut f = Fixture::running().await;
    f.fake.inject("container_remove", Injection::RefuseRemove);
    assert!(
        f.sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(f.sandbox.is_fenced());
    assert_eq!(f.fake.resources(), 3);
    assert!(
        f.sandbox
            .start_session(2, deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(f.cleanup().await.complete());
}

#[tokio::test]
async fn partial_cleanup_reports_each_residual_even_when_holder_removal_succeeds() {
    let mut f = Fixture::running().await;
    f.fake.inject("container_remove", Injection::RefuseRemove);
    let report = f.cleanup().await;
    assert!(!report.complete());
    assert!(matches!(
        &report.resources[0],
        ResourceOutcome::Residual {
            role: Role::Agent,
            ..
        }
    ));
    assert!(matches!(
        &report.resources[1],
        ResourceOutcome::ConfirmedAbsent {
            role: Role::Holder,
            ..
        }
    ));
    assert!(matches!(
        &report.resources[2],
        ResourceOutcome::Residual {
            role: Role::Volume,
            ..
        }
    ));
    assert_eq!(f.fake.resources(), 2);
    assert!(f.cleanup().await.complete());
}

#[tokio::test]
async fn retries_do_not_renew_capture_quiescence_or_final_cleanup_budgets() {
    use std::time::Duration;
    use tokio::time::Instant;
    let mut f = Fixture::running().await;
    let before = Instant::now();
    let first = f
        .sandbox
        .quiesce(before + Duration::from_secs(300), CancellationToken::new())
        .await
        .unwrap()
        .deadline();
    assert!(first <= Instant::now() + Duration::from_secs(80));
    f.fake.state.lock().unwrap().deadlines.clear();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = f
        .sandbox
        .quiesce(
            Instant::now() + Duration::from_secs(300),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .deadline();
    assert_eq!(first, second);
    assert!(
        f.fake
            .state
            .lock()
            .unwrap()
            .deadlines
            .iter()
            .all(|d| *d < before + Duration::from_secs(21))
    );
    f.fake.inject("container_remove", Injection::RefuseRemove);
    f.fake.state.lock().unwrap().deadlines.clear();
    assert!(!f.cleanup().await.complete());
    let first = f.fake.state.lock().unwrap().deadlines[0];
    f.fake.state.lock().unwrap().deadlines.clear();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(f.cleanup().await.complete());
    assert!(
        f.fake
            .state
            .lock()
            .unwrap()
            .deadlines
            .iter()
            .all(|d| *d <= first)
    );
}

#[tokio::test]
async fn volume_identity_options_and_unexpected_mounts_block_cleanup_or_capture() {
    for (key, value) in [
        ("Driver", json!("remote")),
        ("Options", json!({})),
        ("Labels", json!({"foreign":"owner"})),
    ] {
        let mut f = Fixture::initialized().await;
        {
            let mut s = f.fake.state.lock().unwrap();
            s.volumes.values_mut().next().unwrap()[key] = value;
        }
        assert!(
            f.sandbox
                .quiesce(deadline(), CancellationToken::new())
                .await
                .is_err()
        );
        let report = f.cleanup().await;
        assert!(!report.complete());
        assert!(
            !f.fake
                .seen()
                .iter()
                .any(|op| matches!(op, Operation::RemoveVolume(_)))
        );
    }
    let mut f = Fixture::initialized().await;
    {
        let mut s = f.fake.state.lock().unwrap();
        let mut foreign = s.containers.values().next().unwrap().clone();
        foreign["Name"] = json!("/foreign");
        foreign["Id"] = json!("f".repeat(64));
        s.containers.insert("f".repeat(64), foreign);
    }
    assert_eq!(
        f.sandbox
            .quiesce(deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .failure,
        Failure::Conflict
    );
    assert!(
        f.sandbox
            .start_session(1, deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(!f.cleanup().await.complete());
    assert!(
        !f.fake
            .seen()
            .iter()
            .any(|op| matches!(op,Operation::RemoveContainer(id) if id==&"f".repeat(64)))
    );
}

#[tokio::test]
async fn complete_argv_uses_fixed_helpers_and_no_host_shell() {
    let mut f = Fixture::running().await;
    f.tool().await.unwrap();
    let operations = f.fake.seen();
    let agent = operations
        .iter()
        .find_map(|o| match o {
            Operation::CreateContainer(p) if p.role == Role::Agent => Some(p),
            _ => None,
        })
        .unwrap();
    let args = Operation::CreateContainer(agent.clone()).argv();
    for (flag, value) in [
        ("--pull", "never"),
        ("--platform", "linux/amd64"),
        ("--network", "none"),
        ("--cap-drop", "ALL"),
        ("--security-opt", "no-new-privileges"),
        ("--user", "0:0"),
        ("--log-driver", "none"),
        ("--restart", "no"),
        ("--cgroupns", "private"),
        ("--ipc", "private"),
        ("--workdir", "/"),
        ("--entrypoint", "/usr/local/bin/python3.12"),
        ("--cpus", "1.000000000"),
        ("--memory", "1073741824"),
        ("--memory-swap", "1073741824"),
        ("--pids-limit", "64"),
        ("--shm-size", "16777216"),
    ] {
        assert_eq!(args.iter().filter(|s| *s == flag).count(), 1);
        let i = args.iter().position(|s| s == flag).unwrap();
        assert_eq!(args[i + 1], value);
    }
    for flag in ["--read-only", "--no-healthcheck"] {
        assert_eq!(args.iter().filter(|s| *s == flag).count(), 1);
    }
    assert!(args.contains(&format!(
        "type=volume,src={},dst=/work,volume-nocopy",
        agent.volume
    )));
    assert!(args.contains(&"/tmp:rw,nosuid,nodev,size=16777216,mode=1777,nr_inodes=16384".into()));
    assert_eq!(
        &args[args.len() - 7..],
        &[
            agent.image.clone(),
            "-I".into(),
            "-S".into(),
            "-B".into(),
            "-c".into(),
            TIMER.into(),
            agent.deadline_monotonic_ns.to_string()
        ]
    );
    let bridge = operations
        .iter()
        .find(|o| matches!(o, Operation::Bridge { .. }))
        .unwrap();
    let Operation::Bridge { id, payload } = bridge else {
        unreachable!()
    };
    assert_eq!(
        bridge.argv(),
        vec![
            "container",
            "exec",
            "--user",
            "1000:1000",
            "--workdir",
            "/",
            "--interactive",
            id,
            "/usr/local/bin/python3.12",
            "-I",
            "-S",
            "-B",
            "/opt/playtime/tool.py"
        ]
    );
    assert!(!bridge.argv().iter().any(|s| s.contains("model text")));
    assert!(
        std::str::from_utf8(payload)
            .unwrap()
            .contains("model text is data only")
    );
    assert!(f.cleanup().await.complete());
}
