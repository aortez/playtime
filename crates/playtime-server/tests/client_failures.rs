//! Adversarial real HTTP peers test the shared client independently of handlers.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    response::Response,
};
use playtime_client::{Client, Error, Observation, RetryPolicy};
use playtime_core::{RunId, Sha256Digest};
use playtime_protocol::api::*;
use playtime_server::{application::Application, auth::Token, fixture::Fixture};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Behavior {
    Redirect,
    MalformedError,
    LargeJson,
    Stall,
    PartialSse,
    LargeSse,
    BadVersion,
    Unauthenticated,
    LostLaunch,
    EmptySse,
    CutDownload,
    WrongCancel,
}
struct StateData {
    behavior: Behavior,
    calls: AtomicUsize,
    caps: Capabilities,
    token: String,
}
struct Peer {
    endpoint: String,
    state: Arc<StateData>,
    token: Token,
    shutdown: CancellationToken,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
impl Peer {
    async fn start(behavior: Behavior) -> Self {
        let token = Token::generate().unwrap();
        let mut caps = Fixture::new(ServiceId::new("peer-fixture").unwrap()).capabilities();
        caps.limits.handler_seconds = 1;
        let state = Arc::new(StateData {
            behavior,
            calls: AtomicUsize::new(0),
            caps,
            token: token.expose(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let app = Router::new().fallback(reply).with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            endpoint,
            state,
            token,
            shutdown,
        }
    }
    async fn client(&self) -> Client {
        Client::connect(&self.endpoint, self.token.clone(), None)
            .await
            .unwrap()
    }
}
fn json_response(status: u16, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(status)
        .header(SERVICE_HEADER, "peer-fixture")
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .unwrap()
}
fn record(sequence: u64, version: u32) -> (String, String) {
    let frontier = Frontier {
        run_id: RunId::new("run-peer").unwrap(),
        manifest_sha256: Sha256Digest::new("11".repeat(32)).unwrap(),
        prefix: Prefix {
            sequence: 2,
            byte_offset: 512,
            sha256: Sha256Digest::new("22".repeat(32)).unwrap(),
        },
    };
    let cursor = EventCursor::new(frontier, sequence)
        .unwrap()
        .encode()
        .unwrap();
    let event = serde_json::json!({"schema_version":version,"run_id":"run-peer","sequence":sequence,"time":"2026-10-17T12:00:00Z","kind":"future_record","data":{"text":"é💥"}});
    (
        format!("event: record\nid: {cursor}\ndata: {event}\n\n"),
        cursor,
    )
}
async fn reply(State(state): State<Arc<StateData>>, request: Request) -> Response {
    assert_eq!(
        request.headers()["authorization"],
        format!("Bearer {}", state.token)
    );
    if request.uri().path() == "/api/v1/capabilities" {
        return json_response(200, serde_json::to_vec(&state.caps).unwrap());
    }
    let call = state.calls.fetch_add(1, Ordering::SeqCst);
    match state.behavior {
        Behavior::WrongCancel => {
            let fixture = Fixture::new(state.caps.service_id.clone());
            let launch = fixture
                .admit(
                    IdempotencyKey::new("wrong-run").unwrap(),
                    LaunchRequest {
                        recipe_id: RecipeId::new("fixture-example").unwrap(),
                        mode: playtime_core::RunMode::Simulation,
                        preview_revision: PreviewRevision::new("fixture-v1").unwrap(),
                    },
                )
                .await
                .unwrap();
            let reply = fixture.cancel(launch.run_id).await.unwrap();
            json_response(202, serde_json::to_vec(&reply).unwrap())
        }
        Behavior::EmptySse => Response::builder()
            .header(SERVICE_HEADER, "peer-fixture")
            .header("content-type", "text/event-stream")
            .body(Body::empty())
            .unwrap(),
        Behavior::CutDownload => {
            use base64::Engine;
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(vec![b'x'; 100_000]);
            let hash = digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 16_000]));
                tokio::time::sleep(Duration::from_millis(20)).await;
                yield Err(std::io::Error::other("scripted connection loss"));
            };
            Response::builder()
                .header(SERVICE_HEADER, "peer-fixture")
                .header("content-type", "application/octet-stream")
                .header("content-length", 100_000)
                .header("x-playtime-sha256", hash)
                .header(
                    "content-digest",
                    format!(
                        "sha-256=:{}:",
                        base64::engine::general_purpose::STANDARD.encode(digest)
                    ),
                )
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Behavior::Redirect => Response::builder()
            .status(302)
            .header(SERVICE_HEADER, "peer-fixture")
            .header("location", "/redirect-target")
            .body(Body::empty())
            .unwrap(),
        Behavior::MalformedError => json_response(
            500,
            format!("{{\"message\":\"{}\"}}", state.token).into_bytes(),
        ),
        Behavior::LargeJson => {
            let chunks = futures_util::stream::iter(
                (0..80).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 64 << 10]))),
            );
            Response::builder()
                .header(SERVICE_HEADER, "peer-fixture")
                .header("content-type", "application/json")
                .body(Body::from_stream(chunks))
                .unwrap()
        }
        Behavior::Stall => {
            let stream = async_stream::stream! { tokio::time::sleep(Duration::from_secs(2)).await; yield Ok::<_, std::io::Error>(Bytes::from_static(b"{}")); };
            Response::builder()
                .header(SERVICE_HEADER, "peer-fixture")
                .header("content-type", "application/json")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Behavior::LostLaunch => {
            let stream = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other(
                "scripted lost reply",
            ))]);
            Response::builder()
                .header(SERVICE_HEADER, "peer-fixture")
                .header("content-type", "application/json")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Behavior::Unauthenticated => json_response(
            401,
            serde_json::to_vec(&playtime_server::application::ApiError::new("unauthenticated").0)
                .unwrap(),
        ),
        Behavior::PartialSse | Behavior::LargeSse | Behavior::BadVersion => {
            let bytes = match state.behavior {
                Behavior::LargeSse => {
                    format!("event: record\ndata: {}", "x".repeat((1 << 20) + (8 << 10)))
                }
                Behavior::BadVersion => record(1, 2).0,
                _ if call == 0 => format!(
                    "{}event: record\nid: {}\ndata: {{\"partial\":",
                    record(1, 1).0,
                    record(2, 1).1
                ),
                _ => {
                    assert_eq!(request.headers()["last-event-id"], record(1, 1).1);
                    let end = StreamEnd {
                        status: StreamEndStatus::Incomplete,
                        frontier: EventCursor::decode(&record(2, 1).1).unwrap().frontier,
                    };
                    format!(
                        "{}event: stream_end\ndata: {}\n\n",
                        record(2, 1).0,
                        serde_json::to_string(&end).unwrap()
                    )
                }
            };
            // Small transport chunks exercise UTF-8 and frame boundaries on a
            // real socket, independently from the parser's unit checks.
            let chunks: Vec<_> = bytes
                .as_bytes()
                .chunks(13)
                .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
                .collect();
            Response::builder()
                .header(SERVICE_HEADER, "peer-fixture")
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(futures_util::stream::iter(chunks)))
                .unwrap()
        }
    }
}

#[tokio::test]
async fn cancellation_reply_for_another_run_remains_unresolved() {
    let peer = Peer::start(Behavior::WrongCancel).await;
    assert!(matches!(
        peer.client()
            .await
            .cancel(&RunId::new("run-peer").unwrap())
            .await,
        Err(Error::AmbiguousMutation(_))
    ));
    assert_eq!(peer.state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn redirects_malformed_errors_actual_response_bytes_and_stalls_are_bounded() {
    for behavior in [
        Behavior::Redirect,
        Behavior::MalformedError,
        Behavior::LargeJson,
        Behavior::Stall,
    ] {
        let peer = Peer::start(behavior).await;
        let client = peer.client().await;
        let error = client
            .run(&RunId::new("run-peer").unwrap())
            .await
            .unwrap_err();
        match behavior {
            Behavior::LargeJson => assert!(matches!(error, Error::LimitExceeded)),
            Behavior::Stall => assert!(matches!(error, Error::Timeout)),
            _ => assert!(matches!(error, Error::InvalidResponse)),
        }
        assert!(!format!("{error:?}").contains(&peer.token.expose()));
        assert_eq!(peer.state.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn partial_frame_eof_reconnects_from_applied_cursor_only() {
    let peer = Peer::start(Behavior::PartialSse).await;
    let client = peer.client().await;
    let cancel = CancellationToken::new();
    let mut follow = client.follow(
        &RunId::new("run-peer").unwrap(),
        None,
        RetryPolicy {
            initial_backoff: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let Observation::Record(first) = follow.next(&cancel).await.unwrap() else {
        panic!()
    };
    assert_eq!(first.event.sequence.get(), 1);
    follow.acknowledge(&first).unwrap();
    let Observation::Record(second) = follow.next(&cancel).await.unwrap() else {
        panic!()
    };
    assert_eq!(second.event.sequence.get(), 2);
    assert_eq!(follow.acknowledged_cursor(), Some(first.cursor.as_str()));
    follow.acknowledge(&second).unwrap();
    assert!(matches!(
        follow.next(&cancel).await.unwrap(),
        Observation::End(StreamEnd {
            status: StreamEndStatus::Incomplete,
            ..
        })
    ));
    assert_eq!(peer.state.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn oversized_corrupt_and_unauthorized_streams_do_not_retry_or_advance() {
    for behavior in [
        Behavior::LargeSse,
        Behavior::BadVersion,
        Behavior::Unauthenticated,
    ] {
        let peer = Peer::start(behavior).await;
        let client = peer.client().await;
        let mut follow = client.follow(
            &RunId::new("run-peer").unwrap(),
            None,
            RetryPolicy::default(),
        );
        let error = follow.next(&CancellationToken::new()).await.unwrap_err();
        match behavior {
            Behavior::LargeSse => assert!(matches!(error, Error::LimitExceeded)),
            Behavior::BadVersion => assert!(matches!(error, Error::InvalidResponse)),
            _ => assert_eq!(error.api_code(), Some("unauthenticated")),
        }
        assert!(follow.acknowledged_cursor().is_none());
        assert_eq!(peer.state.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn ambiguous_mutations_never_retry_in_the_transport() {
    let peer = Peer::start(Behavior::LostLaunch).await;
    let client = peer.client().await;
    let pending = client
        .prepare_launch(
            LaunchRequest {
                recipe_id: RecipeId::new("example").unwrap(),
                mode: playtime_core::RunMode::Simulation,
                preview_revision: PreviewRevision::new("v1").unwrap(),
            },
            None,
        )
        .unwrap();
    assert!(matches!(
        client.launch(&pending).await,
        Err(Error::AmbiguousMutation(_))
    ));
    assert_eq!(peer.state.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reconnect_count_and_backoff_cancellation_are_bounded() {
    let peer = Peer::start(Behavior::EmptySse).await;
    let client = peer.client().await;
    let mut follow = client.follow(
        &RunId::new("run-peer").unwrap(),
        None,
        RetryPolicy {
            max_reconnects: 2,
            initial_backoff: Duration::from_millis(1),
            ..Default::default()
        },
    );
    assert!(matches!(
        follow.next(&CancellationToken::new()).await,
        Err(Error::RetryBudgetExhausted)
    ));
    assert_eq!(peer.state.calls.load(Ordering::SeqCst), 3);
    let mut follow = client.follow(
        &RunId::new("run-peer").unwrap(),
        None,
        RetryPolicy {
            initial_backoff: Duration::from_secs(2),
            ..Default::default()
        },
    );
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        stop.cancel();
    });
    assert!(matches!(follow.next(&cancel).await, Err(Error::Cancelled)));
    assert_eq!(peer.state.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn interrupted_download_keeps_partial_bytes_out_of_the_final_destination() {
    let peer = Peer::start(Behavior::CutDownload).await;
    let client = peer.client().await;
    let artifact = Artifact {
        artifact_id: ArtifactId::new("cut-file").unwrap(),
        name: "cut.bin".into(),
        kind: "file_version".into(),
        content: playtime_protocol::archive::ContentRef {
            path: playtime_core::ArchivePath::new("objects/cut.bin").unwrap(),
            sha256: playtime_server::fixture::digest(&vec![b'x'; 100_000]),
            size_bytes: 100_000,
        },
        session_id: None,
        published_sequence: 1,
        frozen_input: false,
    };
    let id = RunId::new("run-peer").unwrap();
    let cancel = CancellationToken::new();
    let mut bytes = vec![];
    assert!(
        client
            .download(&id, &artifact, &mut bytes, &cancel)
            .await
            .is_err()
    );
    assert_eq!(bytes.len(), 16_000);
    let directory =
        std::env::temp_dir().join(format!("playtime-cut-download-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("output.bin");
    assert!(
        client
            .download_to_path(&id, &artifact, &path, &cancel)
            .await
            .is_err()
    );
    assert!(!path.exists());
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir(directory).unwrap();
}
