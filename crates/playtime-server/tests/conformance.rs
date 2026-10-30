mod common;
use common::*;
use playtime_client::{Client, Error, Observation};
use playtime_core::{RunMode, SessionId};
use playtime_protocol::api::*;
use playtime_server::{application::Application, fixture::Fixture, http::Server};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn retained_observation_content_joins_the_catalog_at_its_publication_frontier() {
    let h = Harness::new().await;
    let run = h
        .client
        .launch(&h.pending("retained-payload").await)
        .await
        .unwrap();
    h.fixture.reveal(&run.run_id, 1, None);
    let older = h
        .client
        .artifacts(&run.run_id, &PageQuery::default())
        .await
        .unwrap();
    assert_eq!(older.frontier.unwrap().prefix.sequence, 1);
    assert!(!older.items.iter().any(|a| a.kind == "observation_payload"));
    h.fixture.reveal(&run.run_id, 2, None);
    let page = h
        .client
        .history(&run.run_id, &PageQuery::default())
        .await
        .unwrap();
    let reference: playtime_protocol::archive::ContentRef =
        serde_json::from_value(page.events[1].data["synthetic_payload_ref"].clone()).unwrap();
    let catalog = h
        .client
        .artifacts(&run.run_id, &PageQuery::default())
        .await
        .unwrap();
    assert_eq!(catalog.frontier, Some(page.frontier));
    let artifact = catalog
        .items
        .iter()
        .find(|a| a.content == reference)
        .unwrap();
    assert_eq!(artifact.published_sequence, 2);
    let mut bytes = Vec::new();
    h.client
        .download(&run.run_id, artifact, &mut bytes, &CancellationToken::new())
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(payload["unknown"]["path"], "/not/a/fetch/permission");
    assert!(
        !catalog
            .items
            .iter()
            .any(|a| a.content.path.as_str() == "/not/a/fetch/permission")
    );
}

#[tokio::test]
async fn real_http_preview_launch_inspect_stream_download_and_schema() {
    let h = Harness::new().await;
    assert_eq!(h.client.capabilities().backend.kind, BackendKind::Fixture);
    assert!(
        h.client
            .capabilities()
            .unsupported_adapters
            .contains(&"controller".into())
    );
    let recipes = h
        .client
        .recipes(&PageQuery {
            cursor: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(recipes.items.len(), 1);
    let second = h
        .client
        .recipes(&PageQuery {
            cursor: recipes.next_cursor,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(second.items[0].recipe_id.as_str(), "fixture-example");
    let pending = h.pending("walkthrough").await;
    let run = h.client.launch(&pending).await.unwrap();
    assert!(!run.replayed);
    assert!(run.run.outcome.is_none());
    assert!(run.run.usage.is_none());
    let anchors = run.run.event_anchors.unwrap();
    assert_eq!(anchors.frontier.prefix.sequence, 0);
    assert_eq!(anchors.history_cursor, anchors.live_cursor);
    assert_eq!(h.client.lookup(&pending).await.unwrap().run_id, run.run_id);
    assert!(h.client.launch(&pending).await.unwrap().replayed);
    let mut stream = h
        .client
        .stream(
            &run.run_id,
            Some(&anchors.live_cursor),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    for sequence in 1..=4 {
        let Observation::Record(record) = next(&mut stream).await else {
            panic!("record expected")
        };
        assert_eq!(record.event.sequence.get(), sequence);
        if sequence == 2 {
            assert_eq!(record.event.data["unknown_field"]["retained"], true);
        }
        assert!(matches!(
            stream.next(&CancellationToken::new()).await,
            Err(Error::UnacknowledgedRecord)
        ));
        stream.acknowledge(&record).unwrap();
    }
    assert!(matches!(
        next(&mut stream).await,
        Observation::End(StreamEnd {
            status: StreamEndStatus::CommittedCompletion,
            ..
        })
    ));
    let completed = h.client.run(&run.run_id).await.unwrap();
    assert_eq!(completed.usage.unwrap().unknown_requests, 1);
    let cp = h
        .client
        .checkpoint(&run.run_id, &SessionId::new("s1").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        cp.files[1].content,
        ContentAvailability::Unavailable { .. }
    ));
    let artifacts = h
        .client
        .artifacts(&run.run_id, &PageQuery::default())
        .await
        .unwrap();
    assert_eq!(artifacts.frontier.unwrap().prefix.sequence, 4);
    let artifact = artifacts
        .items
        .iter()
        .find(|a| a.kind == "file_version")
        .unwrap();
    let temp = Temp::new();
    let destination = temp.0.join("image.png");
    let size = h
        .client
        .download_to_path(
            &run.run_id,
            artifact,
            &destination,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(size, artifact.content.size_bytes);
    assert_eq!(
        std::fs::read(destination).unwrap(),
        include_bytes!("fixtures/pixel.png")
    );
    let document = h.client.schema().await.unwrap();
    assert_eq!(document["security"][0]["bearerAuth"], serde_json::json!([]));
    assert_eq!(document["paths"].as_object().unwrap().len(), 31);
    for (route, value) in [
        (
            "capabilities",
            serde_json::to_value(h.client.capabilities()).unwrap(),
        ),
        (
            "runs/{run_id}",
            serde_json::to_value(h.client.run(&run.run_id).await.unwrap()).unwrap(),
        ),
        (
            "runs/{run_id}/artifacts",
            serde_json::to_value(
                h.client
                    .artifacts(&run.run_id, &PageQuery::default())
                    .await
                    .unwrap(),
            )
            .unwrap(),
        ),
    ] {
        let response_schema = document["paths"][format!("/api/v1/{route}")]["get"]["responses"]["200"]["content"]["application/json"]["schema"].clone();
        let schema = serde_json::json!({"$ref": response_schema["$ref"], "components":document["components"]});
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(
            validator.is_valid(&value),
            "{route}: {:?}",
            validator
                .iter_errors(&value)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
    }
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn lost_reply_same_key_stale_preview_and_restart_recover_original_admission() {
    let h = Harness::new().await;
    let pending = h.pending("lost-reply").await;
    // Discard the accepted response without decoding it.
    drop(
        h.post("runs")
            .header("idempotency-key", pending.key.as_str())
            .json(&pending.request)
            .send()
            .await
            .unwrap(),
    );
    let original = h.client.lookup(&pending).await.unwrap();
    h.fixture
        .change_revision(PreviewRevision::new("fixture-v2").unwrap());
    let replay = h.client.launch(&pending).await.unwrap();
    assert_eq!(original.run_id, replay.run_id);
    assert!(replay.replayed);
    let mut different = pending.clone();
    different.request.mode = RunMode::Experiment;
    assert_eq!(
        h.client.launch(&different).await.unwrap_err().api_code(),
        Some("idempotency_conflict")
    );
    let mut stale = pending.clone();
    stale.key = IdempotencyKey::new("unconsumed").unwrap();
    assert_eq!(
        h.client.launch(&stale).await.unwrap_err().api_code(),
        Some("stale_preview")
    );
    assert_eq!(
        h.client.lookup(&stale).await.unwrap_err().api_code(),
        Some("not_found")
    );
    h.fixture.reveal(
        &original.run_id,
        4,
        Some(StreamEndStatus::CommittedCompletion),
    );
    let address = h.server.address;
    h.server.stop().await.unwrap();
    let restarted = Server::new(h.fixture.clone(), h.token.clone())
        .unwrap()
        .listen(address)
        .await
        .unwrap();
    let recovered = Client::connect(
        &restarted.endpoint(),
        h.token,
        Some(pending.service_id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered.launch(&pending).await.unwrap().run_id,
        original.run_id
    );
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn endpoint_rebinding_cannot_retarget_a_saved_launch() {
    let h = Harness::new().await;
    let pending = h.pending("rebind").await;
    let address = h.server.address;
    h.server.stop().await.unwrap();
    let other = Arc::new(Fixture::new(ServiceId::new("other-history").unwrap()));
    let replacement = Server::new(other.clone(), h.token.clone())
        .unwrap()
        .listen(address)
        .await
        .unwrap();
    assert!(matches!(
        h.client.launch(&pending).await,
        Err(Error::IdentityMismatch)
    ));
    assert!(other.calls().is_empty());
    let new_client = Client::connect(&replacement.endpoint(), h.token, None)
        .await
        .unwrap();
    assert!(matches!(
        new_client.launch(&pending).await,
        Err(Error::IdentityMismatch)
    ));
    assert!(other.calls().is_empty());
    replacement.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_preserves_first_intent_and_exposes_uncertain_enforcement() {
    let h = Harness::new().await;
    let run = h.client.launch(&h.pending("cancel").await).await.unwrap();
    let first = h.client.cancel(&run.run_id).await.unwrap();
    assert!(first.pending);
    assert_eq!(
        first.intent.as_ref().unwrap().enforcement,
        Enforcement::Uncertain
    );
    let second = h.client.cancel(&run.run_id).await.unwrap();
    assert_eq!(first.intent, second.intent);
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let terminal = h.client.cancel(&run.run_id).await.unwrap();
    assert!(!terminal.pending);
    assert_eq!(first.intent, terminal.intent);
    assert!(terminal.run.outcome.is_some());
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn paired_anchors_fixed_history_and_live_observation_cover_every_boundary() {
    for split in 0..=4 {
        let h = Harness::new().await;
        let run = h.client.launch(&h.pending("anchors").await).await.unwrap();
        h.fixture.reveal(&run.run_id, split, None);
        let anchors = h
            .client
            .run(&run.run_id)
            .await
            .unwrap()
            .event_anchors
            .unwrap();
        // Commit between acquiring paired anchors and opening either reader.
        h.fixture
            .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
        let mut live = h
            .client
            .stream(
                &run.run_id,
                Some(&anchors.live_cursor),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let history_client = h.client.clone();
        let id = run.run_id.clone();
        let history = tokio::spawn(async move {
            let mut cursor = Some(anchors.history_cursor);
            let mut sequences = vec![];
            while let Some(position) = cursor {
                let page = history_client
                    .history(
                        &id,
                        &PageQuery {
                            cursor: Some(position),
                            limit: Some(1),
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(page.frontier.prefix.sequence, split as u64);
                sequences.extend(page.events.into_iter().map(|e| e.sequence.get()));
                cursor = page.next_cursor;
            }
            sequences
        });
        let mut live_sequences = vec![];
        loop {
            match next(&mut live).await {
                Observation::Record(record) => {
                    live_sequences.push(record.event.sequence.get());
                    live.acknowledge(&record).unwrap();
                }
                Observation::End(_) => break,
                Observation::Disconnected => panic!("no reconciled end"),
            }
        }
        let mut all = history.await.unwrap();
        all.extend(live_sequences);
        assert_eq!(all, vec![1, 2, 3, 4]);
        h.server.stop().await.unwrap();
    }
}

#[tokio::test]
async fn disconnect_before_ack_redelivers_and_server_restart_retains_cursor_position() {
    let h = Harness::new().await;
    let run = h
        .client
        .launch(&h.pending("redelivery").await)
        .await
        .unwrap();
    h.fixture.reveal(&run.run_id, 2, None);
    let cancel = CancellationToken::new();
    let mut stream = h.client.stream(&run.run_id, None, &cancel).await.unwrap();
    let Observation::Record(first) = next(&mut stream).await else {
        panic!()
    };
    stream.acknowledge(&first).unwrap();
    let Observation::Record(second) = next(&mut stream).await else {
        panic!()
    };
    let saved = stream.acknowledged_cursor().unwrap().to_owned();
    drop(stream);
    let address = h.server.address;
    h.server.stop().await.unwrap();
    let restarted = Server::new(h.fixture.clone(), h.token)
        .unwrap()
        .listen(address)
        .await
        .unwrap();
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let mut resumed = h
        .client
        .stream(&run.run_id, Some(&saved), &cancel)
        .await
        .unwrap();
    let Observation::Record(redelivered) = next(&mut resumed).await else {
        panic!()
    };
    assert_eq!(redelivered.event, second.event);
    assert_eq!(resumed.acknowledged_cursor(), Some(saved.as_str()));
    resumed.acknowledge(&redelivered).unwrap();
    assert_eq!(
        next(&mut resumed).await,
        Observation::Record(playtime_client::ObservedRecord {
            event: h
                .fixture
                .history(run.run_id.clone(), None, false, 4, 1 << 20)
                .await
                .unwrap()
                .events[2]
                .clone(),
            cursor: EventCursor::new(
                h.client
                    .run(&run.run_id)
                    .await
                    .unwrap()
                    .event_anchors
                    .unwrap()
                    .frontier,
                3
            )
            .unwrap()
            .encode()
            .unwrap(),
        })
    );
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn wrong_forged_ahead_and_conflicting_cursors_are_explicit() {
    let h = Harness::new().await;
    let run = h
        .client
        .launch(&h.pending("cursor-errors").await)
        .await
        .unwrap();
    h.fixture.reveal(&run.run_id, 2, None);
    let frontier = h
        .client
        .run(&run.run_id)
        .await
        .unwrap()
        .event_anchors
        .unwrap()
        .frontier;
    let mut forged = EventCursor::new(frontier.clone(), 1).unwrap();
    forged.frontier.prefix.sha256 = playtime_core::Sha256Digest::new("00".repeat(32)).unwrap();
    code(
        h.get(&format!("runs/{}/events", run.run_id))
            .query(&[("cursor", forged.encode().unwrap())])
            .send()
            .await
            .unwrap(),
        400,
        "invalid_cursor",
    )
    .await;
    let mut ahead = EventCursor::new(frontier.clone(), 1).unwrap();
    ahead.frontier.prefix.sequence += 1;
    code(
        h.get(&format!("runs/{}/events", run.run_id))
            .query(&[("cursor", ahead.encode().unwrap())])
            .send()
            .await
            .unwrap(),
        409,
        "cursor_ahead",
    )
    .await;
    let mut wrong = EventCursor::new(frontier.clone(), 0).unwrap();
    wrong.frontier.run_id = playtime_core::RunId::new("wrong-run").unwrap();
    code(
        h.get(&format!("runs/{}/events", run.run_id))
            .query(&[("cursor", wrong.encode().unwrap())])
            .send()
            .await
            .unwrap(),
        400,
        "invalid_cursor",
    )
    .await;
    code(
        h.get(&format!("runs/{}/events/stream", run.run_id))
            .query(&[(
                "cursor",
                EventCursor::new(frontier.clone(), 0)
                    .unwrap()
                    .encode()
                    .unwrap(),
            )])
            .header(
                "last-event-id",
                EventCursor::new(frontier, 1).unwrap().encode().unwrap(),
            )
            .send()
            .await
            .unwrap(),
        400,
        "invalid_cursor",
    )
    .await;
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn evidence_readiness_and_ownership_never_fabricate_usage_or_completion() {
    let h = Harness::new().await;
    let run = h.client.launch(&h.pending("evidence").await).await.unwrap();
    h.fixture.set_evidence(&run.run_id, false, false);
    let view = h.client.run(&run.run_id).await.unwrap();
    assert!(view.event_anchors.is_none());
    assert!(view.usage.is_none());
    assert_eq!(
        h.client
            .history(&run.run_id, &PageQuery::default())
            .await
            .unwrap_err()
            .api_code(),
        Some("evidence_not_ready")
    );
    h.fixture.set_evidence(&run.run_id, true, true);
    let confirmed = h
        .client
        .run(&run.run_id)
        .await
        .unwrap()
        .event_anchors
        .unwrap();
    h.fixture.set_evidence(&run.run_id, true, false);
    assert_eq!(
        h.client
            .history(&run.run_id, &PageQuery::default())
            .await
            .unwrap_err()
            .api_code(),
        Some("ownership_unresolved")
    );
    assert!(
        h.client
            .history(
                &run.run_id,
                &PageQuery {
                    cursor: Some(confirmed.history_cursor),
                    limit: None
                }
            )
            .await
            .unwrap()
            .events
            .is_empty()
    );
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn run_pages_fix_admission_cutoff_and_artifact_pages_fix_frontier() {
    let h = Harness::new().await;
    let one = h.client.launch(&h.pending("one").await).await.unwrap();
    h.fixture
        .reveal(&one.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let two = h.client.launch(&h.pending("two").await).await.unwrap();
    h.fixture.reveal(&two.run_id, 3, None);
    let first = h
        .client
        .runs(&PageQuery {
            cursor: None,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(first.items[0].run_id, two.run_id);
    let artifacts = h
        .client
        .artifacts(
            &two.run_id,
            &PageQuery {
                cursor: None,
                limit: Some(1),
            },
        )
        .await
        .unwrap();
    h.fixture
        .reveal(&two.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let three = h.client.launch(&h.pending("three").await).await.unwrap();
    let second = h
        .client
        .runs(&PageQuery {
            cursor: first.next_cursor,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(second.items[0].run_id, one.run_id);
    assert!(second.next_cursor.is_none());
    assert_ne!(three.run_id, one.run_id);
    let more = h
        .client
        .artifacts(
            &two.run_id,
            &PageQuery {
                cursor: artifacts.next_cursor,
                limit: Some(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(more.frontier, artifacts.frontier);
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn full_stream_capacity_preserves_control_and_detachment_does_not_cancel() {
    let h = Harness::new().await;
    let run = h
        .client
        .launch(&h.pending("observers").await)
        .await
        .unwrap();
    let mut streams = vec![];
    for _ in 0..8 {
        streams.push(
            h.client
                .stream(&run.run_id, None, &CancellationToken::new())
                .await
                .unwrap(),
        );
    }
    let error = h
        .client
        .stream(&run.run_id, None, &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error.api_code(), Some("too_many_observers"));
    assert!(h.client.run(&run.run_id).await.is_ok());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        streams[0].next(&cancel).await,
        Err(Error::Cancelled)
    ));
    drop(streams);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!h.fixture.calls().iter().any(|c| c.operation == "cancel"));
    assert!(h.client.cancel(&run.run_id).await.unwrap().pending);
    h.server.stop().await.unwrap();
}
