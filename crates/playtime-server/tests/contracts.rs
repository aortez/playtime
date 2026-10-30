mod common;
use common::*;
use playtime_client::Observation;
use playtime_core::RunMode;
use playtime_protocol::api::*;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn validate(document: &Value, name: &str, value: &Value) {
    let schema =
        json!({"$ref":format!("#/components/schemas/{name}"), "components":document["components"]});
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{name}: {errors:?}");
}
fn check_references(document: &Value, value: &Value) {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref") {
                let reference = reference.as_str().unwrap();
                assert!(reference.starts_with("#/"));
                assert!(
                    document.pointer(&reference[1..]).is_some(),
                    "missing {reference}"
                );
            }
            for value in map.values() {
                check_references(document, value);
            }
        }
        Value::Array(values) => {
            for value in values {
                check_references(document, value);
            }
        }
        _ => (),
    }
}

#[tokio::test]
async fn reviewed_http_goldens_match_actual_responses_and_generated_schema() {
    let h = Harness::new().await;
    let document = h.client.schema().await.unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/openapi.json")).unwrap();
    assert_eq!(document, expected);
    check_references(&document, &document);
    let golden: Value = serde_json::from_str(include_str!("fixtures/http-contract.json")).unwrap();
    let request: LaunchRequest = serde_json::from_value(golden["launch_request"].clone()).unwrap();
    let pending = h
        .client
        .prepare_launch(request, Some(IdempotencyKey::new("golden").unwrap()))
        .unwrap();
    let launch = h.client.launch(&pending).await.unwrap();
    assert_eq!(
        serde_json::to_value(&launch).unwrap(),
        golden["launch_response"]
    );
    assert_eq!(
        serde_json::to_value(&launch.run.event_anchors).unwrap(),
        golden["empty_anchors"]
    );
    h.fixture.reveal(
        &launch.run_id,
        4,
        Some(StreamEndStatus::CommittedCompletion),
    );
    let completed = h.client.run(&launch.run_id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&completed).unwrap(),
        golden["completed_run"]
    );
    let page = h
        .client
        .history(
            &launch.run_id,
            &PageQuery {
                cursor: None,
                limit: Some(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&page).unwrap(), golden["event_page"]);
    let cursor = EventCursor::decode(golden["cursor_encoded"].as_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(&cursor).unwrap(),
        golden["cursor_json"]
    );
    assert_eq!(cursor.encode().unwrap(), golden["cursor_encoded"]);
    assert_eq!(
        serde_json::to_value(
            h.client
                .checkpoints(&launch.run_id, &PageQuery::default())
                .await
                .unwrap()
        )
        .unwrap(),
        golden["checkpoints"]
    );
    assert_eq!(
        serde_json::to_value(
            h.client
                .artifacts(&launch.run_id, &PageQuery::default())
                .await
                .unwrap()
        )
        .unwrap(),
        golden["artifacts"]
    );
    for (name, field) in [
        ("LaunchRequest", "launch_request"),
        ("LaunchResponse", "launch_response"),
        ("RunView", "completed_run"),
        ("EventAnchors", "empty_anchors"),
        ("EventPage", "event_page"),
        ("EventCursor", "cursor_json"),
        ("Page_Checkpoint", "checkpoints"),
        ("Page_Artifact", "artifacts"),
    ] {
        validate(&document, name, &golden[field]);
    }
    let preview = h
        .client
        .preview(
            &RecipeId::new("fixture-example").unwrap(),
            RunMode::Simulation,
        )
        .await
        .unwrap();
    validate(
        &document,
        "Preview",
        &serde_json::to_value(preview).unwrap(),
    );
    let recipes = h.client.recipes(&PageQuery::default()).await.unwrap();
    validate(
        &document,
        "Page_Recipe",
        &serde_json::to_value(recipes).unwrap(),
    );
    let cancelled = h.client.cancel(&launch.run_id).await.unwrap();
    validate(
        &document,
        "CancelResponse",
        &serde_json::to_value(cancelled).unwrap(),
    );
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn empty_reconciled_prefix_has_valid_anchors_and_no_invented_outcome() {
    let h = Harness::new().await;
    let preview = h
        .client
        .preview(
            &RecipeId::new("fixture-empty").unwrap(),
            RunMode::Simulation,
        )
        .await
        .unwrap();
    let pending = h
        .client
        .prepare_launch(
            LaunchRequest {
                target: None,
                recipe_id: preview.recipe_id,
                mode: preview.mode,
                preview_revision: preview.preview_revision,
            },
            None,
        )
        .unwrap();
    let launch = h.client.launch(&pending).await.unwrap();
    h.fixture
        .reveal(&launch.run_id, 0, Some(StreamEndStatus::Incomplete));
    let run = h.client.run(&launch.run_id).await.unwrap();
    assert!(run.outcome.is_none());
    assert!(run.usage.is_none());
    let anchors = run.event_anchors.unwrap();
    assert_eq!(anchors.history_cursor, anchors.live_cursor);
    let page = h
        .client
        .history(&launch.run_id, &PageQuery::default())
        .await
        .unwrap();
    assert!(page.events.is_empty());
    assert!(!page.has_more);
    assert!(page.next_cursor.is_none());
    assert_eq!(
        EventCursor::decode(&page.resume_cursor)
            .unwrap()
            .after_sequence,
        0
    );
    let mut stream = h
        .client
        .stream(
            &launch.run_id,
            Some(&anchors.live_cursor),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        next(&mut stream).await,
        Observation::End(StreamEnd {
            status: StreamEndStatus::Incomplete,
            ..
        })
    ));
    let artifacts = h
        .client
        .artifacts(&launch.run_id, &PageQuery::default())
        .await
        .unwrap();
    assert_eq!(artifacts.items.len(), 1);
    assert!(artifacts.items[0].frozen_input);
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn actual_page_byte_cap_stops_before_record_without_skipping_it() {
    let limits = ApiLimits {
        json_bytes: 4096,
        event_bytes: 2048,
        ..Default::default()
    };
    let h = Harness::limits(limits).await;
    let launch = h
        .client
        .launch(&h.pending("page-bytes").await)
        .await
        .unwrap();
    for index in [1, 2] {
        h.fixture
            .replace_pending_record(&launch.run_id, index, json!({"text":"é".repeat(750)}));
    }
    h.fixture.reveal(
        &launch.run_id,
        4,
        Some(StreamEndStatus::CommittedCompletion),
    );
    let mut cursor = None;
    let mut sequences = vec![];
    let mut pages = 0;
    loop {
        let page = h
            .client
            .history(
                &launch.run_id,
                &PageQuery {
                    cursor,
                    limit: Some(1000),
                },
            )
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 4096);
        sequences.extend(page.events.into_iter().map(|e| e.sequence.get()));
        pages += 1;
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(sequences, vec![1, 2, 3, 4]);
    h.server.stop().await.unwrap();
}
