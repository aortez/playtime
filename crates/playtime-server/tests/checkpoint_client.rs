//! Deliberately synthetic HTTP peers for versioned checkpoint download handling.
use axum::{Router, body::Body, response::Response, routing::get};
use base64::{Engine, engine::general_purpose::STANDARD};
use playtime_client::{CheckpointIndexRead, Client, Error};
use playtime_core::{ArchivePath, RunId, SessionId};
use playtime_protocol::api::*;
use playtime_server::{application::Application, auth::Token, fixture::Fixture};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn unsupported_index_bytes_remain_inspectable_only_after_verified_download() {
    for corrupt in [false, true] {
        let bytes = br#"{"schema_version":2,"future":{"retained":"exact bytes"}}"#.to_vec();
        let content = playtime_storage::content_ref(
            ArchivePath::new("sessions/s1/index.json").unwrap(),
            &bytes,
        );
        let artifact = Artifact {
            artifact_id: ArtifactId::new("index").unwrap(),
            name: "index.json".into(),
            kind: "checkpoint_index".into(),
            content: content.clone(),
            session_id: Some(SessionId::new("s1").unwrap()),
            published_sequence: 5,
            frozen_input: false,
        };
        let unavailable = ContentAvailability::Unavailable {
            reason: "test".into(),
        };
        let mut checkpoint = Checkpoint {
            checkpoint: CheckpointSummary {
                session_id: SessionId::new("s1").unwrap(),
                published_sequence: 5,
            },
            incoming_memory: unavailable.clone(),
            outgoing_memory: unavailable.clone(),
            evidence: unavailable.clone(),
            workspace: unavailable,
            files: vec![],
            index: None,
        };
        let caps = Fixture::new(ServiceId::new("index-peer").unwrap()).capabilities();
        let counter = Arc::new(AtomicUsize::new(0));
        let requests = counter.clone();
        let metadata = artifact.clone();
        let body = if corrupt {
            vec![b'x'; bytes.len()]
        } else {
            bytes.clone()
        };
        let hash = content.sha256.clone();
        let raw: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&hash.as_str()[i..i + 2], 16).unwrap())
            .collect();
        let digest = format!("sha-256=:{}:", STANDARD.encode(raw));
        let app = Router::new()
            .route(
                "/api/v1/capabilities",
                get(move || async move { json(caps) }),
            )
            .route(
                "/api/v1/runs/run/artifacts/index",
                get(move || {
                    requests.fetch_add(1, Ordering::SeqCst);
                    let metadata = metadata.clone();
                    async move { json(metadata) }
                }),
            )
            .route(
                "/api/v1/runs/run/artifacts/index/content",
                get(move || {
                    let (body, hash, digest) = (body.clone(), hash.clone(), digest.clone());
                    async move {
                        Response::builder()
                            .header(SERVICE_HEADER, "index-peer")
                            .header("content-type", "application/octet-stream")
                            .header("content-length", body.len())
                            .header("x-playtime-sha256", hash.as_str())
                            .header("content-digest", digest)
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
                .unwrap()
        });
        let client = Client::connect(&endpoint, Token::generate().unwrap(), None)
            .await
            .unwrap();
        let run = RunId::new("run").unwrap();
        assert!(
            matches!(client.checkpoint_index(&run,&checkpoint,&CancellationToken::new()).await.unwrap(),CheckpointIndexRead::Unavailable{reason} if reason=="not_recorded")
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        checkpoint.index = Some(ContentAvailability::Available {
            artifact_id: artifact.artifact_id,
            content,
        });
        let result = client
            .checkpoint_index(&run, &checkpoint, &CancellationToken::new())
            .await;
        if corrupt {
            assert!(matches!(result, Err(Error::ContentMismatch)));
        } else {
            assert!(
                matches!(result.unwrap(),CheckpointIndexRead::UnsupportedSchema{received:2,bytes:actual} if actual==bytes)
            );
        }
        checkpoint.checkpoint.session_id = SessionId::new("other").unwrap();
        assert!(matches!(
            client
                .checkpoint_index(&run, &checkpoint, &CancellationToken::new())
                .await,
            Err(Error::ContentMismatch)
        ));
        stop.cancel();
        server.await.unwrap();
    }
}
fn json(value: impl serde::Serialize) -> Response {
    Response::builder()
        .header(SERVICE_HEADER, "index-peer")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap()
}
