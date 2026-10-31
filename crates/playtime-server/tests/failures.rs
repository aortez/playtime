mod common;
use common::{faults::Faults, *};
use playtime_client::{Client, Error, Observation, RetryPolicy};
use playtime_protocol::api::*;
use playtime_server::{auth::Token, fixture::Fixture, http::Server};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn commit_at_subscribe_and_read_boundaries_never_disappears() {
    for boundary in 0..3 {
        let h = Harness::new().await;
        let run = h
            .client
            .launch(&h.pending("join-race").await)
            .await
            .unwrap();
        let faults = Arc::new(Faults::new(h.fixture.clone()));
        let fixture = h.fixture.clone();
        let id = run.run_id.clone();
        let hook =
            Box::new(move || fixture.reveal(&id, 4, Some(StreamEndStatus::CommittedCompletion)));
        match boundary {
            0 => *faults.before_subscribe.lock().unwrap() = Some(hook),
            1 => *faults.after_subscribe.lock().unwrap() = Some(hook),
            _ => *faults.after_history.lock().unwrap() = Some(hook),
        }
        let server = Server::new(faults, h.token.clone())
            .unwrap()
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let client = Client::connect(&server.endpoint(), h.token.clone(), None)
            .await
            .unwrap();
        let mut stream = client
            .stream(&run.run_id, None, &CancellationToken::new())
            .await
            .unwrap();
        for sequence in 1..=4 {
            let Observation::Record(record) = next(&mut stream).await else {
                panic!("record required")
            };
            assert_eq!(record.event.sequence.get(), sequence);
            stream.acknowledge(&record).unwrap();
        }
        assert!(matches!(next(&mut stream).await, Observation::End(_)));
        server.stop().await.unwrap();
        h.server.stop().await.unwrap();
    }
}

#[tokio::test]
async fn handler_timeout_or_disconnection_does_not_cancel_admitted_operation() {
    let limits = ApiLimits {
        handler_seconds: 1,
        ..Default::default()
    };
    let fixture = Arc::new(Fixture::with_limits(
        ServiceId::new("service-delay").unwrap(),
        limits,
    ));
    let mut faults = Faults::new(fixture.clone());
    faults.admission_delay = Duration::from_millis(1300);
    let token = Token::generate().unwrap();
    let server = Server::new(Arc::new(faults), token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), token, None)
        .await
        .unwrap();
    let request = LaunchRequest {
        target: None,
        recipe_id: RecipeId::new("fixture-example").unwrap(),
        mode: playtime_core::RunMode::Simulation,
        preview_revision: PreviewRevision::new("fixture-v1").unwrap(),
    };
    let pending = client
        .prepare_launch(request, Some(IdempotencyKey::new("timeout").unwrap()))
        .unwrap();
    assert!(matches!(
        client.launch(&pending).await,
        Err(Error::AmbiguousMutation(_))
    ));
    let accepted = client.lookup(&pending).await.unwrap();
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|c| c.operation == "admit")
            .count(),
        1
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        client.lookup(&pending).await.unwrap().run_id,
        accepted.run_id
    );
    assert!(!fixture.calls().iter().any(|c| c.operation == "cancel"));
    server.stop().await.unwrap();
}

#[tokio::test]
async fn oversized_output_and_event_frames_fail_without_truncating_or_advancing() {
    let h = Harness::new().await;
    let pending = h.pending("large-event").await;
    let mut faults = Faults::new(h.fixture.clone());
    faults.preview_size = (4 << 20) + 1;
    let server = Server::new(Arc::new(faults), h.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), h.token.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        client
            .preview(&pending.request.recipe_id, pending.request.mode)
            .await
            .unwrap_err()
            .api_code(),
        Some("evidence_limit_exceeded")
    );
    let run = client.launch(&pending).await.unwrap();
    h.fixture.replace_pending_record(
        &run.run_id,
        1,
        serde_json::json!({"text":"x".repeat(1<<20)}),
    );
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let mut stream = client
        .stream(&run.run_id, None, &CancellationToken::new())
        .await
        .unwrap();
    let Observation::Record(first) = next(&mut stream).await else {
        panic!()
    };
    stream.acknowledge(&first).unwrap();
    let error = stream.next(&CancellationToken::new()).await.unwrap_err();
    assert_eq!(error.api_code(), Some("evidence_limit_exceeded"));
    assert_eq!(
        EventCursor::decode(stream.acknowledged_cursor().unwrap())
            .unwrap()
            .after_sequence,
        1
    );
    server.stop().await.unwrap();
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn tiny_slow_client_queue_disconnects_and_resumes_without_skipping() {
    let limits = ApiLimits {
        stream_pending_records: 1,
        ..Default::default()
    };
    let h = Harness::limits(limits).await;
    let run = h
        .client
        .launch(&h.pending("slow-reader").await)
        .await
        .unwrap();
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let mut follow = h.client.follow(
        &run.run_id,
        None,
        RetryPolicy {
            max_reconnects: 8,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            max_elapsed: Duration::from_secs(3),
        },
    );
    let cancel = CancellationToken::new();
    let mut sequences = vec![];
    loop {
        match tokio::time::timeout(Duration::from_secs(4), follow.next(&cancel))
            .await
            .unwrap()
            .unwrap()
        {
            Observation::Record(record) => {
                sequences.push(record.event.sequence.get());
                follow.acknowledge(&record).unwrap();
            }
            Observation::End(_) => break,
            Observation::Disconnected => panic!(),
        }
    }
    assert_eq!(sequences, vec![1, 2, 3, 4]);
    assert_eq!(
        h.fixture
            .calls()
            .iter()
            .filter(|c| c.operation == "admit")
            .count(),
        1
    );
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn download_capacity_is_separate_and_bad_content_is_never_published_locally() {
    let h = Harness::new().await;
    let run = h
        .client
        .launch(&h.pending("download-failure").await)
        .await
        .unwrap();
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let mut bad = Faults::new(h.fixture.clone());
    bad.bad_download = true;
    let server = Server::new(Arc::new(bad), h.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let client = Client::connect(&server.endpoint(), h.token.clone(), None)
        .await
        .unwrap();
    let artifact = client
        .artifact(&run.run_id, &ArtifactId::new("saved-image").unwrap())
        .await
        .unwrap();
    let temp = Temp::new();
    let destination = temp.0.join("bad.png");
    assert!(
        client
            .download_to_path(
                &run.run_id,
                &artifact,
                &destination,
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
    server.stop().await.unwrap();
    let mut stalled = Faults::new(h.fixture.clone());
    stalled.stalled_download = true;
    let server = Server::new(Arc::new(stalled), h.token.clone())
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let mut transfers = vec![];
    for _ in 0..4 {
        transfers.push(
            h.raw
                .get(format!(
                    "{}/api/v1/runs/{}/artifacts/saved-image/content",
                    server.endpoint(),
                    run.run_id
                ))
                .bearer_auth(h.token.expose())
                .send()
                .await
                .unwrap(),
        );
    }
    code(
        h.raw
            .get(format!(
                "{}/api/v1/runs/{}/artifacts/saved-image/content",
                server.endpoint(),
                run.run_id
            ))
            .bearer_auth(h.token.expose())
            .send()
            .await
            .unwrap(),
        429,
        "too_many_observers",
    )
    .await;
    let client = Client::connect(&server.endpoint(), h.token.clone(), None)
        .await
        .unwrap();
    assert!(client.run(&run.run_id).await.is_ok());
    assert!(client.cancel(&run.run_id).await.is_ok());
    drop(transfers);
    server.stop().await.unwrap();
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn streams_outlive_handler_deadline_and_transport_eof_is_not_completion() {
    let limits = ApiLimits {
        handler_seconds: 1,
        heartbeat_seconds: 1,
        ..Default::default()
    };
    let h = Harness::limits(limits).await;
    let run = h
        .client
        .launch(&h.pending("long-stream").await)
        .await
        .unwrap();
    let mut stream = h
        .client
        .stream(&run.run_id, None, &CancellationToken::new())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    h.fixture.reveal(&run.run_id, 1, None);
    let Observation::Record(record) = next(&mut stream).await else {
        panic!()
    };
    assert_eq!(record.event.sequence.get(), 1);
    stream.acknowledge(&record).unwrap();
    h.server.stop().await.unwrap();
    assert!(matches!(next(&mut stream).await, Observation::Disconnected));
    assert_eq!(
        EventCursor::decode(stream.acknowledged_cursor().unwrap())
            .unwrap()
            .after_sequence,
        1
    );
}
