mod common;
use common::*;
use playtime_backend_guard::{capability, fixture::Mode, random_id, wire::*};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn barrier(listener: &tokio::net::UnixListener) -> tokio::net::UnixStream {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "at barrier\n");
        reader.into_inner()
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn synchronized_crashes_never_resend_possibly_dispatched_requests() {
    for (event, point, sends, may_have_dispatched, terminal) in [
        ("dispatch_marked", "BeforeWrite", 0, false, false),
        ("dispatch_marked", "AfterWrite", 0, true, false),
        ("dispatch_marked", "AfterFileSync", 0, true, false),
        ("dispatch_marked", "AfterDirectorySync", 0, true, false),
        ("terminal", "BeforeWrite", 1, true, false),
        ("terminal", "AfterWrite", 1, true, true),
        ("terminal", "AfterFileSync", 1, true, true),
        ("terminal", "AfterDirectorySync", 1, true, true),
    ] {
        let mut h = Harness::new(Mode::Terminal).await;
        let socket = h.root.join("barrier.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let client = h
            .start(
                true,
                Some(json!({"socket":socket,"event":event,"point":point,"fail":false})),
            )
            .await;
        let lease = lease(&h.identity);
        let cap = capability().unwrap();
        client
            .control(Command::Acquire {
                lease: lease.clone(),
                capability: cap.clone(),
            })
            .await
            .unwrap();
        let request = request(&lease, 1);
        let dispatch = tokio::spawn({
            let client = client.clone();
            let request = request.clone();
            let cap = cap.clone();
            async move {
                if let Ok((stream, _)) = client.dispatch(request, cap, b"x").await {
                    finish(stream).await;
                }
            }
        });
        let _blocked = barrier(&listener).await;
        assert_eq!(h.stats().await.0.len(), sends);
        h.guards[0].kill();
        dispatch.abort();
        let client = h.start(false, None).await;
        let status = client.control(Command::Query {}).await.unwrap();
        if !may_have_dispatched {
            assert!(
                status.request.is_none(),
                "no intent and no bytes before marking"
            );
            assert!(client.dispatch(request.clone(), cap, b"x").await.is_err());
            status_until(&client, |s| s.termination_recorded).await;
            assert!(h.stats().await.0.is_empty());
        } else {
            let (_closed, retry) = client
                .dispatch(request.clone(), cap.clone(), b"x")
                .await
                .unwrap();
            if terminal {
                assert!(matches!(
                    retry.request.unwrap().state,
                    RequestState::Terminal { .. }
                ));
                assert!(
                    retry.stop.is_some(),
                    "restart fences even a lease whose terminal acknowledgment was lost"
                );
            } else {
                assert_eq!(
                    retry.request.unwrap().state,
                    RequestState::MayHaveDispatched {}
                );
                status_until(&client, |s| s.termination_recorded).await;
            }
            assert_eq!(h.stats().await.0.len(), sends);
        }
    }
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn crash_after_upstream_submission_retains_reservation_and_deadlines() {
    let mut h = Harness::new(Mode::Hang).await;
    let client = h.start(true, None).await;
    let lease = lease(&h.identity);
    let cap = capability().unwrap();
    client
        .control(Command::Acquire {
            lease: lease.clone(),
            capability: cap.clone(),
        })
        .await
        .unwrap();
    let request = request(&lease, 1);
    let (stream, _) = client
        .dispatch(request.clone(), cap.clone(), b"x")
        .await
        .unwrap();
    h.wait_sends(1).await;
    h.guards[0].kill();
    assert_eq!(h.stats().await.2, 2, "owned backend outlives guard process");
    let client = h.start(false, None).await;
    let done = status_until(&client, |s| s.termination_recorded).await;
    assert_eq!(done.lease.unwrap().spec, lease);
    let original_stop = done.stop.unwrap();
    assert_eq!(original_stop.reason, StopReason::Restart);
    assert!(original_stop.observation_deadline_ms <= original_stop.decided_ms + 120_000);
    let (_retry, _) = client.dispatch(request.clone(), cap, b"x").await.unwrap();
    assert_eq!(h.stats().await.0, vec![request]);
    drop(stream);
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn stalled_journal_cannot_delay_stop_and_failed_handle_keeps_exclusion() {
    let mut h = Harness::new(Mode::Hang).await;
    let socket = h.root.join("barrier.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let client = h.start(true, Some(json!({"socket":socket,"event":"dispatch_marked","point":"BeforeWrite","fail":true}))).await;
    let lease = lease(&h.identity);
    let cap = capability().unwrap();
    client
        .control(Command::Acquire {
            lease: lease.clone(),
            capability: cap.clone(),
        })
        .await
        .unwrap();
    let dispatch = tokio::spawn({
        let client = client.clone();
        let cap = cap.clone();
        let request = request(&lease, 1);
        async move { client.dispatch(request, cap, b"x").await }
    });
    let mut blocked = barrier(&listener).await;
    assert_eq!(
        client
            .control(Command::Revoke {
                lease_id: lease.lease_id,
                capability: cap,
                operation_id: random_id().unwrap(),
            })
            .await,
        Err(Failure::Storage)
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.stats().await.2 == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.stats().await.0.len(), 0);
    assert_eq!(
        client.control(Command::Query {}).await.unwrap().gate,
        Gate::Blocked
    );
    // Resume with an I/O error. The old guard remains blocked; it cannot yield
    // its lock to a second guard merely because its journal thread failed.
    blocked.write_all(b"x").await.unwrap();
    assert!(dispatch.await.unwrap().is_err());
    let state = h.root.join("state");
    let competing =
        tokio::task::spawn_blocking(move || playtime_backend_guard::Journal::open(&state));
    assert!(competing.await.unwrap().is_err());
}

#[tokio::test]
#[ignore = "guard gate: ./scripts/test-rust-backend-guard.sh"]
async fn accepted_acquisition_retry_waits_for_same_durable_record() {
    let mut h = Harness::new(Mode::Terminal).await;
    let socket = h.root.join("barrier.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let client = h
        .start(
            true,
            Some(json!({"socket":socket,"event":"acquired","point":"AfterWrite","fail":false})),
        )
        .await;
    let lease = lease(&h.identity);
    let cap = capability().unwrap();
    let command = Command::Acquire {
        lease: lease.clone(),
        capability: cap,
    };
    let first = tokio::spawn({
        let client = client.clone();
        let command = command.clone();
        async move { client.control(command).await }
    });
    let mut blocked = barrier(&listener).await;
    let second = tokio::spawn({
        let client = client.clone();
        async move { client.control(command).await }
    });
    assert_eq!(
        client.control(Command::Query {}).await.unwrap().gate,
        Gate::Preparing
    );
    blocked.write_all(b"x").await.unwrap();
    assert_eq!(first.await.unwrap().unwrap().lease.unwrap().spec, lease);
    assert_eq!(second.await.unwrap().unwrap().lease.unwrap().spec, lease);
    assert_eq!(
        std::fs::read_dir(h.root.join("state"))
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("0000"))
            .count(),
        2
    );
}
