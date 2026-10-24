mod common;
use common::*;
use playtime_ollama::{discovery::Discovery, *};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
fn fake(values: Vec<Value>) -> Fake {
    Fake::new(
        values
            .into_iter()
            .map(|v| response(200, serde_json::to_vec(&v).unwrap())),
    )
}
#[tokio::test]
async fn discovery_is_metadata_only_preserves_aliases_and_escapes_names() {
    let name = "a/quoted\"name:latest";
    let tags = json!({"models":[{"name":name,"digest":Profile::DIGEST},{"name":"alias","digest":Profile::DIGEST}]});
    let transport = fake(vec![
        json!({"version":Profile::VERSION}),
        tags.clone(),
        json!({"models":[]}),
        json!({"version":Profile::VERSION}),
        tags.clone(),
        lock().definition,
        tags,
        json!({"version":Profile::VERSION}),
    ]);
    let seen = transport.seen.clone();
    let reader = Discovery::new(transport, Limits::default()).unwrap();
    let inventory = reader
        .inventory(deadline(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(inventory.entries.len(), 2);
    assert!(inventory.loaded.unwrap().is_empty());
    let result = reader
        .inspect(
            name,
            Some(Profile::DIGEST),
            deadline(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        result.definition_sha256.as_deref(),
        Some(Profile::DEFINITION)
    );
    let calls = seen.lock().unwrap();
    assert_eq!(
        calls.iter().map(|r| r.operation).collect::<Vec<_>>(),
        [
            Operation::Version,
            Operation::Tags,
            Operation::Loaded,
            Operation::Version,
            Operation::Tags,
            Operation::Show,
            Operation::Tags,
            Operation::Version
        ]
    );
    assert_eq!(
        serde_json::from_slice::<Value>(calls[5].body.as_ref().unwrap()).unwrap(),
        json!({"model":name})
    );
}
#[tokio::test]
async fn missing_remote_ambiguous_changed_and_unverified_metadata_never_become_candidates() {
    for case in 0..8 {
        let mut values = metadata();
        match case {
            0 => values[1] = json!({"models":[]}),
            1 => values[1]["models"][0]["remote_host"] = json!("cloud.example"),
            2 => values[1]["models"][0]["digest"] = Value::Null,
            3 => values[2]["remote_model"] = json!("remote"),
            4 => values[2]["model_info"] = Value::Null,
            5 => values[1]["models"]
                .as_array_mut()
                .unwrap()
                .push(metadata()[1]["models"][0].clone()),
            6 => values[1]["models"][0]["digest"] = json!("f".repeat(64)),
            _ => values[2]["remote_host"] = json!(false),
        }
        let tags = values[1].clone();
        values.extend([tags, json!({"version":Profile::VERSION})]);
        let transport = fake(values);
        let seen = transport.seen.clone();
        let reader = Discovery::new(transport, Limits::default()).unwrap();
        let result = reader
            .inspect(
                Profile::MODEL,
                Some(Profile::DIGEST),
                deadline(),
                &CancellationToken::new(),
            )
            .await;
        if case == 5 {
            assert!(result.is_err());
        } else {
            let result = result.unwrap();
            assert!(result.definition_sha256.is_none() || result.rejected);
        }
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|c| c.operation != Operation::Chat)
        );
        if matches!(case, 0..=2 | 5 | 6) {
            assert!(
                seen.lock()
                    .unwrap()
                    .iter()
                    .all(|c| c.operation != Operation::Show)
            );
        }
    }
}
#[tokio::test]
async fn loaded_errors_are_unknown_and_limits_are_explicit() {
    let transport = Fake::new([
        response(200, serde_json::to_vec(&metadata()[0]).unwrap()),
        response(200, serde_json::to_vec(&metadata()[1]).unwrap()),
        response(503, b"unavailable".to_vec()),
    ]);
    let result = Discovery::new(transport, Limits::default())
        .unwrap()
        .inventory(deadline(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.entries.len(), 1);
    assert!(result.loaded.is_none());
    for count in [256, 257] {
        let tags = json!({"models":(0..count).map(|n|json!({"name":format!("model{n}"),"digest":Profile::DIGEST})).collect::<Vec<_>>()});
        let reader = Discovery::new(
            fake(vec![metadata()[0].clone(), tags, json!({"models":[]})]),
            Limits::default(),
        )
        .unwrap();
        let result = reader
            .inventory(deadline(), &CancellationToken::new())
            .await;
        if count == 256 {
            assert_eq!(result.unwrap().entries.len(), 256);
        } else {
            assert!(matches!(
                result,
                Err(Failure {
                    kind: FailureKind::ByteLimit,
                    ..
                })
            ));
        }
    }
    let reader = Discovery::new(
        fake(vec![json!({"version":"x".repeat(300)})]),
        Limits {
            metadata_bytes: 64,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        reader
            .inventory(deadline(), &CancellationToken::new())
            .await,
        Err(Failure {
            kind: FailureKind::ByteLimit,
            ..
        })
    ));
}
#[tokio::test]
async fn tag_and_backend_drift_during_show_are_refused() {
    for backend in [false, true] {
        let mut values = metadata();
        values.push(if backend {
            metadata()[1].clone()
        } else {
            json!({"models":[]})
        });
        values.push(json!({"version":"changed"}));
        let result = Discovery::new(fake(values), Limits::default())
            .unwrap()
            .inspect(Profile::MODEL, None, deadline(), &CancellationToken::new())
            .await;
        assert!(matches!(
            result,
            Err(Failure {
                kind: FailureKind::Identity,
                ..
            })
        ));
    }
}
struct Hung;
#[async_trait::async_trait]
impl Transport for Hung {
    async fn send(&self, _: Request) -> Result<HttpResponse> {
        std::future::pending().await
    }
}
#[tokio::test(start_paused = true)]
async fn timeout_and_cancellation_bound_metadata_work() {
    let reader = Discovery::new(Hung, Limits::default()).unwrap();
    let start = Instant::now();
    assert!(matches!(
        reader
            .inventory(start + Duration::from_secs(300), &CancellationToken::new())
            .await,
        Err(Failure {
            kind: FailureKind::Timeout,
            ..
        })
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(15));
    let cancel = CancellationToken::new();
    let copy = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        copy.cancel();
    });
    let start = Instant::now();
    assert!(matches!(
        reader.inventory(deadline(), &cancel).await,
        Err(Failure {
            kind: FailureKind::Cancelled,
            ..
        })
    ));
    assert_eq!(start.elapsed(), Duration::from_millis(25));
}

struct Slow(Fake);
#[async_trait::async_trait]
impl Transport for Slow {
    async fn send(&self, request: Request) -> Result<HttpResponse> {
        tokio::time::sleep(Duration::from_secs(14)).await;
        self.0.send(request).await
    }
}
#[tokio::test(start_paused = true)]
async fn inspection_has_an_aggregate_deadline_across_metadata_operations() {
    let mut values = metadata();
    values.extend([metadata()[1].clone(), metadata()[0].clone()]);
    let reader = Discovery::new(Slow(fake(values)), Limits::default()).unwrap();
    let start = Instant::now();
    assert!(matches!(
        reader
            .inspect(
                Profile::MODEL,
                None,
                start + Duration::from_secs(300),
                &CancellationToken::new()
            )
            .await,
        Err(Failure {
            kind: FailureKind::Timeout,
            ..
        })
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(45));
}
