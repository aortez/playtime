mod support;
use playtime_docker::{transport::*, *};
use serde_json::json;
use std::sync::Arc;
use support::*;

#[tokio::test]
async fn readiness_is_inspection_only_and_distinguishes_absence_from_unavailability() {
    for (outcome, expected) in [
        (absent("image"), Failure::MissingImage),
        (lost(Fault::Io), Failure::DaemonUnavailable),
        (Outcome::completed(b"invalid".to_vec()), Failure::Malformed),
    ] {
        let port = Arc::new(ScriptedTransport::default());
        port.push(Operation::Version, response(version()));
        port.push(Operation::Info, response(info()));
        let engine = Engine::connect(port.clone(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        port.push(Operation::Info, response(info()));
        port.push(
            Operation::ImageInspect(format!("sha256:{}", "a".repeat(64))),
            outcome,
        );
        if expected == Failure::MissingImage {
            port.push(Operation::Info, response(info()));
        }
        let error = engine
            .readiness(&lock(), deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.failure, expected);
        assert!(port.seen().iter().all(|o| matches!(
            o,
            Operation::Version | Operation::Info | Operation::ImageInspect(_)
        )));
        assert_eq!(port.remaining(), 0);
    }
}

#[tokio::test]
async fn rootless_remote_platforms_and_unenforceable_limits_are_rejected() {
    for (pointer, value) in [
        ("/SecurityOptions", json!(["name=seccomp", "name=rootless"])),
        ("/SecurityOptions", json!(["name=seccomp", "name=userns"])),
        ("/SecurityOptions", json!([])),
        ("/CgroupVersion", json!("1")),
        ("/OSType", json!("windows")),
        ("/Architecture", json!("aarch64")),
        ("/OperatingSystem", json!("Docker Desktop")),
        ("/MemoryLimit", json!(false)),
        ("/SwapLimit", json!(false)),
        ("/CpuCfsQuota", json!(false)),
        ("/CpuCfsPeriod", json!(false)),
        ("/PidsLimit", json!(false)),
        ("/Plugins/Volume", json!([])),
    ] {
        let port = Arc::new(ScriptedTransport::default());
        let mut v = info();
        *v.pointer_mut(pointer).unwrap() = value;
        port.push(Operation::Version, response(version()));
        port.push(Operation::Info, response(v));
        assert_eq!(
            Engine::connect(port, deadline(), CancellationToken::new())
                .await
                .err()
                .unwrap()
                .failure,
            Failure::Unsupported,
            "{pointer}"
        );
    }
    let port = Arc::new(ScriptedTransport::default());
    let mut v = version();
    v["Server"]["ApiVersion"] = json!("1.40");
    port.push(Operation::Version, response(v));
    port.push(Operation::Info, response(info()));
    assert_eq!(
        Engine::connect(port, deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .failure,
        Failure::Unsupported
    );
}

#[tokio::test]
async fn absence_from_a_different_daemon_is_not_proof_of_missing_image() {
    let port = Arc::new(ScriptedTransport::default());
    port.push(Operation::Version, response(version()));
    port.push(Operation::Info, response(info()));
    let engine = Engine::connect(port.clone(), deadline(), CancellationToken::new())
        .await
        .unwrap();
    port.push(Operation::Info, response(info()));
    port.push(
        Operation::ImageInspect(format!("sha256:{}", "a".repeat(64))),
        absent("image"),
    );
    let mut changed = info();
    changed["ID"] = json!("replacement");
    port.push(Operation::Info, response(changed));
    assert_eq!(
        engine
            .readiness(&lock(), deadline(), CancellationToken::new())
            .await
            .err()
            .unwrap()
            .failure,
        Failure::ChangedIdentity
    );
}

#[tokio::test]
async fn unexpected_image_mounts_identity_and_bridge_profile_fail_before_creation() {
    for (property, value, failure) in [
        (
            "Id",
            json!(format!("sha256:{}", "b".repeat(64))),
            Failure::ChangedIdentity,
        ),
        ("Os", json!("windows"), Failure::ChangedIdentity),
        ("Architecture", json!("arm64"), Failure::ChangedIdentity),
        (
            "Config",
            json!({"Volumes":{"/unexpected":{}}}),
            Failure::Unsupported,
        ),
    ] {
        let port = Arc::new(ScriptedTransport::default());
        port.push(Operation::Version, response(version()));
        port.push(Operation::Info, response(info()));
        let engine = Engine::connect(port.clone(), deadline(), CancellationToken::new())
            .await
            .unwrap();
        let id = format!("sha256:{}", "a".repeat(64));
        let mut image = json!({"Id":id,"Os":"linux","Architecture":"amd64","Config":{"Volumes":null,"OnBuild":null}});
        image[property] = value;
        port.push(Operation::Info, response(info()));
        port.push(Operation::ImageInspect(id), response(json!([image])));
        assert_eq!(
            engine
                .readiness(&lock(), deadline(), CancellationToken::new())
                .await
                .err()
                .unwrap()
                .failure,
            failure
        );
    }
    let f = Fixture::new().await;
    let mut bad: serde_json::Value = serde_json::from_slice(&lock()).unwrap();
    bad["build_sources"]["tool.py"] = json!("unreviewed");
    let count = f.fake.seen().len();
    assert!(
        f.engine
            .readiness(
                &serde_json::to_vec(&bad).unwrap(),
                deadline(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(f.fake.seen().len(), count);
}
