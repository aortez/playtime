mod common;
use common::*;
use playtime_backend_guard::{
    Journal, capability,
    fixture::{FixtureCommand, Mode},
    random_id,
    transport::Client,
    wire::*,
};
use std::time::Duration;

#[tokio::test]
async fn two_services_race_and_normal_terminal_allows_unique_next_dispatch() {
    let mut h = Harness::new(Mode::Terminal).await;
    let client = h.start(true, None).await;
    let first = lease(&h.identity);
    let second = lease(&h.identity);
    let cap = capability().unwrap();
    let cap2 = capability().unwrap();
    let (a, b) = tokio::join!(
        client.control(Command::Acquire {
            lease: first.clone(),
            capability: cap.clone()
        }),
        client.control(Command::Acquire {
            lease: second.clone(),
            capability: cap2.clone()
        })
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let (lease, cap) = if a.is_ok() {
        (first, cap)
    } else {
        (second, cap2)
    };
    assert_eq!(
        client
            .control(Command::Acquire {
                lease: lease.clone(),
                capability: cap.clone()
            })
            .await
            .unwrap()
            .lease
            .unwrap()
            .spec,
        lease
    );
    let mut conflict = lease.clone();
    conflict.cleanup_deadline_ms += 1;
    assert_eq!(
        client
            .control(Command::Acquire {
                lease: conflict,
                capability: cap.clone()
            })
            .await,
        Err(Failure::Conflict)
    );
    let request = request(&lease, 1);
    let (stream, _) = client
        .dispatch(request.clone(), cap.clone(), b"x")
        .await
        .unwrap();
    let (partial, terminal) = finish(stream).await;
    assert_eq!(partial, b"synthetic partial");
    assert!(matches!(
        terminal.request.unwrap().state,
        RequestState::Terminal { .. }
    ));
    let (_retry, status) = client
        .dispatch(request.clone(), cap.clone(), b"x")
        .await
        .unwrap();
    assert!(matches!(
        status.request.unwrap().state,
        RequestState::Terminal { .. }
    ));
    let mut conflict = request.clone();
    conflict.original_max_tokens += 1;
    assert!(matches!(
        client.dispatch(conflict, cap.clone(), b"x").await,
        Err(Failure::Conflict)
    ));
    assert!(
        client
            .dispatch(common::request(&lease, 1), cap.clone(), b"x")
            .await
            .is_err()
    );
    let (stream, _) = client
        .dispatch(common::request(&lease, 2), cap.clone(), b"x")
        .await
        .unwrap();
    finish(stream).await;
    let release = Command::Release {
        lease_id: lease.lease_id.clone(),
        capability: cap.clone(),
        operation_id: random_id().unwrap(),
    };
    client.control(release.clone()).await.unwrap();
    client.control(release).await.unwrap();
    assert!(
        client
            .dispatch(common::request(&lease, 3), cap, b"x")
            .await
            .is_err()
    );
    let replacement = common::lease(&h.identity);
    client
        .control(Command::Acquire {
            lease: replacement,
            capability: capability().unwrap(),
        })
        .await
        .unwrap();
    let (sends, stops, alive) = h.stats().await;
    assert_eq!(sends.len(), 2);
    assert!(stops.is_empty());
    assert_eq!(alive, 2, "normal residency needs no restart");
}

#[tokio::test]
async fn disconnected_owner_retains_uncertainty_until_owned_children_are_reaped() {
    let mut h = Harness::new(Mode::Unavailable).await;
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
    drop(stream);
    let stopped = status_until(&client, |s| s.stop.is_some()).await;
    assert_eq!(stopped.stop.as_ref().unwrap().reason, StopReason::OwnerLost);
    assert_eq!(
        h.stats().await.2,
        2,
        "fake upstream survives socket closure"
    );
    assert!(
        client
            .control(Command::Acquire {
                lease: common::lease(&h.identity),
                capability: capability().unwrap()
            })
            .await
            .is_err()
    );
    assert!(
        client
            .control(Command::Release {
                lease_id: lease.lease_id.clone(),
                capability: cap.clone(),
                operation_id: random_id().unwrap()
            })
            .await
            .is_err()
    );
    h.set_mode(Mode::Hang).await;
    // Restart resumes the same stop against fresh owned-manager evidence.
    h.guards[0].kill();
    let client = h.start(false, None).await;
    let done = status_until(&client, |s| s.termination_recorded).await;
    assert_eq!(done.stop, stopped.stop);
    assert_eq!(
        done.request.unwrap().state,
        RequestState::MayHaveDispatched {}
    );
    assert_eq!(h.stats().await.2, 0);
    client
        .control(Command::Release {
            lease_id: lease.lease_id,
            capability: cap,
            operation_id: random_id().unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(h.stats().await.0, vec![request]);
}

#[tokio::test]
async fn full_stream_and_cancelled_caller_do_not_delay_control_stop() {
    let mut h = Harness::new(Mode::Flood).await;
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
    let (_unread, _) = client
        .dispatch(request(&lease, 1), cap.clone(), b"x")
        .await
        .unwrap();
    h.wait_sends(1).await;
    let command = Command::Revoke {
        lease_id: lease.lease_id,
        capability: cap,
        operation_id: random_id().unwrap(),
    };
    let task = tokio::spawn({
        let client = client.clone();
        let command = command.clone();
        async move { client.control(command).await }
    });
    // Query proves cancellation reached the guard before losing its caller.
    status_until(&client, |s| s.stop.is_some()).await;
    task.abort();
    tokio::time::timeout(
        Duration::from_secs(3),
        status_until(&client, |s| s.termination_recorded),
    )
    .await
    .unwrap();
    client.control(command).await.unwrap();
    assert_eq!(h.stats().await.2, 0);
}

#[tokio::test]
async fn stop_reply_loss_and_remaining_descendant_are_not_termination_claims() {
    for mode in [Mode::LostStopReply, Mode::Descendant] {
        let mut h = Harness::new(mode).await;
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
        let (stream, _) = client
            .dispatch(request(&lease, 1), cap.clone(), b"x")
            .await
            .unwrap();
        h.wait_sends(1).await;
        client
            .control(Command::Revoke {
                lease_id: lease.lease_id.clone(),
                capability: cap.clone(),
                operation_id: random_id().unwrap(),
            })
            .await
            .unwrap();
        if mode == Mode::Descendant {
            status_until(&client, |s| s.observation.is_some()).await;
            assert_eq!(h.stats().await.2, 1);
            assert!(
                !client
                    .control(Command::Query {})
                    .await
                    .unwrap()
                    .termination_recorded
            );
            h.remote.command(FixtureCommand::Cleanup).await.unwrap();
        }
        let done = status_until(&client, |s| s.termination_recorded).await;
        h.guards[0].kill();
        drop(stream);
        let client = h.start(false, None).await;
        status_until(&client, |s| s.gate == Gate::Terminated).await;
        client
            .control(Command::Release {
                lease_id: lease.lease_id,
                capability: cap,
                operation_id: random_id().unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(
            client.control(Command::Query {}).await.unwrap().stop,
            done.stop
        );
        assert_eq!(h.stats().await.1.len(), 1);
    }
}

#[tokio::test]
async fn malformed_private_frames_and_missing_or_damaged_state_fail_closed() {
    use tokio::io::AsyncWriteExt;
    let mut h = Harness::new(Mode::Terminal).await;
    let client = h.start(true, None).await;
    for length in [0, CONTROL_BYTES as u32 + 1, u32::MAX] {
        let mut stream = tokio::net::UnixStream::connect(h.root.join("ipc-0/control.sock"))
            .await
            .unwrap();
        stream.write_u32(length).await.unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(1),
            playtime_backend_guard::transport::read_json::<_, ControlReply>(&mut stream),
        )
        .await
        .unwrap();
        assert!(reply.is_err());
    }
    let wrong = Client::new(h.root.join("ipc-0"), capability().unwrap());
    assert!(wrong.control(Command::Query {}).await.is_err());
    assert_eq!(
        client.control(Command::Query {}).await.unwrap().gate,
        Gate::Ready
    );
    h.guards[0].kill();
    std::fs::remove_file(h.root.join("state/00000001.json")).unwrap();
    assert!(Journal::open(&h.root.join("state")).is_err());
    assert!(Journal::open(&h.root.join("missing")).is_err());
}

#[tokio::test]
async fn replacement_process_with_same_unit_is_never_targeted() {
    let mut original = Harness::new(Mode::Hang).await;
    let client = original.start(true, None).await;
    let lease = lease(&original.identity);
    let cap = capability().unwrap();
    client
        .control(Command::Acquire {
            lease: lease.clone(),
            capability: cap.clone(),
        })
        .await
        .unwrap();
    let (stream, _) = client
        .dispatch(request(&lease, 1), cap.clone(), b"x")
        .await
        .unwrap();
    original.wait_sends(1).await;
    original.guards[0].kill();
    let mut identity = original.identity.clone();
    identity.invocation_id = random_id().unwrap();
    let replacement = Harness::with_identity(Mode::Hang, identity).await;
    let client = original
        .start_with_endpoint(false, None, replacement.remote.endpoint.clone())
        .await;
    status_until(&client, |s| s.gate == Gate::Blocked).await;
    let (_, stops, alive) = replacement.stats().await;
    assert!(stops.is_empty());
    assert_eq!(alive, 2);
    assert_eq!(original.stats().await.2, 2);
    assert!(
        client
            .control(Command::Release {
                lease_id: lease.lease_id,
                capability: cap,
                operation_id: random_id().unwrap()
            })
            .await
            .is_err()
    );
    drop(stream);
}

#[tokio::test]
async fn manager_return_resumes_the_original_stop_without_renewing_deadlines() {
    let mut h = Harness::new(Mode::Unavailable).await;
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
    let command = Command::Revoke {
        lease_id: lease.lease_id,
        capability: cap,
        operation_id: random_id().unwrap(),
    };
    let first = client.control(command.clone()).await.unwrap().stop;
    status_until(&client, |s| {
        s.observation == Some(ManagerObservation::Unavailable {})
    })
    .await;
    assert_eq!(h.stats().await.2, 2);
    h.set_mode(Mode::Hang).await;
    let done = status_until(&client, |s| s.termination_recorded).await;
    assert_eq!(done.stop, first);
    assert_eq!(client.control(command).await.unwrap().stop, first);
    assert_eq!(h.stats().await.2, 0);
}
