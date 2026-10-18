//! Operator release checks exact proposed names without changing Docker state.
mod support;
use playtime_docker::{transport::*, *};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use support::*;
use tokio::time::Instant;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn proposal(role: Role, name: &str) -> Proposal {
    Proposal {
        schema: 1,
        ownership: Ownership {
            service: "service".into(),
            run: "run".into(),
            grant: "grant".into(),
            invocation: "invocation".into(),
            nonce: "0".repeat(32),
            daemon: DaemonIdentity {
                id: "synthetic-daemon".into(),
                name: "synthetic-host".into(),
                cli_version: "28.4.0".into(),
                server_version: "28.4.0".into(),
                api_version: "1.51".into(),
                kernel: "synthetic-linux".into(),
            },
        },
        role,
        session: (role == Role::Agent).then_some(1),
        name: name.into(),
        image: format!("sha256:{}", "a".repeat(64)),
        volume: "playtime-synthetic-volume-0".into(),
        limits: Limits::smoke(),
        deadline_monotonic_ns: 1,
    }
}

#[tokio::test]
async fn residual_checks_exact_names_and_reports_label_mismatches() {
    let transport = Arc::new(ScriptedTransport::default());
    transport.push(Operation::Version, response(version()));
    transport.push(Operation::Info, response(info()));
    let engine = Engine::connect(transport.clone(), deadline(), CancellationToken::new())
        .await
        .unwrap();
    let agent = proposal(Role::Agent, "playtime-synthetic-agent-1");
    transport.push(Operation::Info, response(info()));
    transport.push(
        Operation::ContainerInspect(agent.name.clone()),
        response(json!([container(&agent, &"b".repeat(64), false)])),
    );
    let present = engine.residual(&agent, deadline(), CancellationToken::new());
    assert_eq!(present.await.unwrap(), Some(true));
    // Absence is confirmed against the same daemon identity.
    transport.push(Operation::Info, response(info()));
    transport.push(
        Operation::ContainerInspect(agent.name.clone()),
        absent("container"),
    );
    transport.push(Operation::Info, response(info()));
    let gone = engine.residual(&agent, deadline(), CancellationToken::new());
    assert_eq!(gone.await.unwrap(), None);
    let volume = proposal(Role::Volume, "playtime-synthetic-volume-0");
    let mut foreign = support::volume(&volume);
    foreign["Labels"] = json!({"io.playtime.run": "another-run"});
    transport.push(Operation::Info, response(info()));
    transport.push(
        Operation::VolumeInspect(volume.name.clone()),
        response(json!([foreign])),
    );
    let conflict = engine.residual(&volume, deadline(), CancellationToken::new());
    assert_eq!(conflict.await.unwrap(), Some(false));
    // A failed inspection is an error, never absence.
    transport.push(Operation::Info, response(info()));
    transport.push(
        Operation::ContainerInspect(agent.name.clone()),
        lost(Fault::Deadline),
    );
    assert!(
        engine
            .residual(&agent, deadline(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(transport.remaining(), 0);
}
