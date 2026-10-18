mod common;
use common::*;
use futures_util::StreamExt;
use playtime_ollama::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

const SECOND: Duration = Duration::from_secs(1);
#[derive(Default)]
struct Seen {
    head: String,
    body: Vec<u8>,
}
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Seen {
    let mut bytes = vec![];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 16384);
        bytes.push(stream.read_u8().await.unwrap());
    }
    Seen {
        head: String::from_utf8(bytes).unwrap(),
        body: vec![],
    }
}
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Seen {
    let mut seen = read_head(stream).await;
    let len = seen
        .head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|v| v.parse::<usize>().ok())
        })
        .unwrap_or(0);
    seen.body.resize(len, 0);
    stream.read_exact(&mut seen.body).await.unwrap();
    seen
}
async fn head<S: AsyncWrite + Unpin>(s: &mut S, status: u16, extra: &str) -> std::io::Result<()> {
    s.write_all(format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Type: application/json\r\n{extra}\r\n").as_bytes()).await
}
struct Reply {
    status: u16,
    extra: String,
    headers_delay: Duration,
    chunks: Vec<(Duration, Vec<u8>)>,
    hold: Duration,
    drop_reply: bool,
}
impl Reply {
    fn body(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            extra: String::new(),
            headers_delay: Duration::ZERO,
            chunks: vec![(Duration::ZERO, body.into())],
            hold: Duration::ZERO,
            drop_reply: false,
        }
    }
    fn delayed(mut self, delay: Duration) -> Self {
        self.chunks[0].0 = delay;
        self
    }
}
struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    job: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.job.abort();
    }
}
impl Server {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/base", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let record = seen.clone();
        let job = tokio::spawn(async move {
            for reply in replies {
                let (mut stream, _) = timeout(5 * SECOND, listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let request = read_request(&mut stream).await;
                record.lock().unwrap().push(request);
                if reply.drop_reply {
                    continue;
                }
                sleep(reply.headers_delay).await;
                if head(&mut stream, reply.status, &reply.extra).await.is_err() {
                    continue;
                }
                for (delay, chunk) in reply.chunks {
                    sleep(delay).await;
                    if stream.write_all(&chunk).await.is_err() {
                        break;
                    }
                }
                sleep(reply.hold).await;
            }
            // Keep the listener alive briefly to detect unexpected replay.
            assert!(
                timeout(Duration::from_millis(80), listener.accept())
                    .await
                    .is_err(),
                "unexpected replay/redirect"
            );
        });
        Self { url, seen, job }
    }
    async fn finish(&mut self) {
        timeout(5 * SECOND, &mut self.job).await.unwrap().unwrap();
    }
    fn transport(&self, limits: &Limits) -> NativeTransport {
        NativeTransport::new(
            Endpoint::parse(&self.url).unwrap(),
            Trust::BundledWebPki,
            limits,
        )
        .unwrap()
    }
    fn adapter(&self, limits: Limits) -> Adapter<NativeTransport> {
        Adapter::new(self.transport(&limits), lock(), limits).unwrap()
    }
}
fn metadata_replies() -> Vec<Reply> {
    metadata()
        .into_iter()
        .map(|v| Reply::body(serde_json::to_vec(&v).unwrap()))
        .collect()
}

#[tokio::test]
async fn native_inspection_exact_paths_headers_bodies_and_optional_ps() {
    let mut replies = metadata_replies();
    replies.push(Reply::body(b"{\"models\":[]}"));
    let mut server = Server::start(replies).await;
    let mut adapter = server.adapter(Limits::default());
    adapter
        .inspect(true, deadline(), &CancellationToken::new())
        .await
        .unwrap();
    server.finish().await;
    let requests = server.seen.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for (index, line) in [
        "GET /base/api/version HTTP/1.1",
        "GET /base/api/tags HTTP/1.1",
        "POST /base/api/show HTTP/1.1",
        "GET /base/api/ps HTTP/1.1",
    ]
    .iter()
    .enumerate()
    {
        let request = &requests[index];
        assert_eq!(request.head.lines().next().unwrap(), *line);
        assert!(
            request
                .head
                .to_ascii_lowercase()
                .contains("accept-encoding: identity\r\n")
        );
        assert!(!request.head.to_ascii_lowercase().contains("authorization"));
    }
    assert_eq!(requests[2].body, br#"{"model":"gpt-oss:20b"}"#);
}
#[tokio::test]
async fn native_redirect_and_compression_are_rejected_without_followup() {
    for (status, extra, kind) in [
        (
            302,
            "Location: http://127.0.0.1:1/forbidden\r\n",
            FailureKind::Http,
        ),
        (200, "Content-Encoding: gzip\r\n", FailureKind::Protocol),
    ] {
        let mut reply = Reply::body(b"{}");
        reply.status = status;
        reply.extra = extra.into();
        let mut server = Server::start(vec![reply]).await;
        let mut adapter = server.adapter(Limits::default());
        assert_eq!(
            adapter
                .inspect(false, deadline(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            kind
        );
        server.finish().await;
        assert_eq!(server.seen.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn quiet_generation_exceeds_connect_timeout_and_stops_on_done_before_eof() {
    let mut replies = metadata_replies();
    let mut reply =
        Reply::body(include_bytes!("fixtures/success.ndjson")).delayed(Duration::from_millis(150));
    reply.hold = Duration::from_millis(350);
    replies.push(reply);
    let mut server = Server::start(replies).await;
    let limits = Limits {
        connect_timeout: Duration::from_millis(40),
        ..Default::default()
    };
    let mut adapter = server.adapter(limits.clone());
    let (tx, _rx) = observations(&limits).unwrap();
    let start = Instant::now();
    let result = adapter
        .chat(
            &request(),
            &EffectiveSettings::default(),
            deadline(),
            &CancellationToken::new(),
            &tx,
        )
        .await;
    assert_eq!(result.outcome.unwrap().generated_tokens, Some(32));
    assert_eq!(result.remote, RemoteOutcome::TerminalObserved);
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(start.elapsed() < Duration::from_millis(450));
    server.finish().await;
}
#[tokio::test]
async fn native_header_deadline_and_dribbling_success_or_error_share_absolute_budget() {
    for case in 0..3 {
        let mut reply = Reply::body(b"{}");
        if case == 0 {
            reply.headers_delay = Duration::from_millis(180);
        } else {
            reply.status = if case == 1 { 200 } else { 500 };
            reply.chunks = vec![(Duration::from_millis(30), vec![b' ']); 20];
        }
        let mut replies = metadata_replies();
        replies.push(reply);
        let mut server = Server::start(replies).await;
        let mut adapter = server.adapter(Limits::default());
        adapter
            .inspect(false, deadline(), &CancellationToken::new())
            .await
            .unwrap();
        let (tx, mut rx) = observations(&Limits::default()).unwrap();
        let start = Instant::now();
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings::default(),
                Instant::now() + Duration::from_millis(100),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        assert_eq!(
            result.outcome.unwrap_err().kind,
            FailureKind::Timeout,
            "case {case}"
        );
        assert_eq!(result.remote, RemoteOutcome::Uncertain);
        assert!(start.elapsed() < Duration::from_millis(400));
        if case == 1 {
            assert!(!result.pending_fragment.is_empty());
        }
        if case == 2 {
            assert!(matches!(
                rx.try_recv().unwrap().kind,
                ObservationKind::HttpError {
                    truncated: true,
                    ..
                }
            ));
        }
        server.finish().await;
    }
}
#[tokio::test]
async fn lost_reply_is_possibly_sent_and_never_replayed() {
    let mut replies = metadata_replies();
    let mut reply = Reply::body(b"");
    reply.drop_reply = true;
    replies.push(reply);
    let mut server = Server::start(replies).await;
    let mut adapter = server.adapter(Limits::default());
    let (tx, _rx) = observations(&Limits::default()).unwrap();
    let result = adapter
        .chat(
            &request(),
            &EffectiveSettings::default(),
            deadline(),
            &CancellationToken::new(),
            &tx,
        )
        .await;
    assert_eq!(result.outcome.unwrap_err().kind, FailureKind::Transport);
    assert_eq!(result.dispatch, Dispatch::PossiblySent);
    assert_eq!(result.remote, RemoteOutcome::Uncertain);
    server.finish().await;
    assert_eq!(server.seen.lock().unwrap().len(), 4);
}
#[tokio::test]
async fn cancellation_during_upload_headers_body_and_before_send() {
    for phase in ["upload", "headers", "body", "before"] {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(8).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let server = tokio::spawn(async move {
            if phase == "before" {
                assert!(
                    timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err()
                );
                return;
            }
            for value in metadata() {
                let (mut s, _) = listener.accept().await.unwrap();
                read_request(&mut s).await;
                head(&mut s, 200, "").await.unwrap();
                s.write_all(&serde_json::to_vec(&value).unwrap())
                    .await
                    .unwrap();
            }
            let (mut s, _) = listener.accept().await.unwrap();
            if phase == "upload" {
                let seen = read_head(&mut s).await;
                assert!(seen.head.to_ascii_lowercase().contains("content-length:"));
            } else {
                read_request(&mut s).await;
                if phase == "body" {
                    head(&mut s, 200, "").await.unwrap();
                    s.write_all(b"{\"message\":{\"content\":\"partial\"}}\n{")
                        .await
                        .unwrap();
                    sleep(Duration::from_millis(30)).await;
                }
            }
            trigger.cancel();
            // Deliberately keep doing server work after client cancellation.
            sleep(Duration::from_millis(120)).await;
            if phase == "upload" {
                let mut received = vec![];
                timeout(SECOND, s.read_to_end(&mut received))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    received.len() < 3 * 1024 * 1024,
                    "upload unexpectedly finished"
                );
            }
        });
        let transport = NativeTransport::new(
            Endpoint::parse(&url).unwrap(),
            Trust::BundledWebPki,
            &Limits::default(),
        )
        .unwrap();
        let mut adapter = Adapter::new(transport, lock(), Limits::default()).unwrap();
        let (tx, _rx) = observations(&Limits::default()).unwrap();
        let mut req = request();
        if phase == "upload" {
            req.messages[0]["content"] = serde_json::json!("x".repeat(3 * 1024 * 1024));
        }
        if phase == "before" {
            cancel.cancel();
        }
        let result = adapter
            .chat(
                &req,
                &EffectiveSettings::default(),
                deadline(),
                &cancel,
                &tx,
            )
            .await;
        assert_eq!(
            result.outcome.unwrap_err().kind,
            FailureKind::Cancelled,
            "{phase}"
        );
        assert_eq!(
            result.remote,
            if phase == "before" {
                RemoteOutcome::NotStarted
            } else {
                RemoteOutcome::Uncertain
            }
        );
        if phase == "body" {
            assert_eq!(result.partial.content.as_deref(), Some("partial"));
            assert_eq!(result.pending_fragment, b"{");
        }
        assert!(
            !server.is_finished(),
            "local result must not pretend to stop server work"
        );
        timeout(3 * SECOND, server).await.unwrap().unwrap();
    }
}
#[tokio::test]
async fn connect_setup_timeout_covers_a_silent_tls_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        sleep(Duration::from_millis(200)).await;
    });
    let limits = Limits {
        connect_timeout: Duration::from_millis(40),
        ..Default::default()
    };
    let transport = NativeTransport::new(
        Endpoint::parse(&url).unwrap(),
        Trust::BundledWebPki,
        &limits,
    )
    .unwrap();
    let mut adapter = Adapter::new(transport, lock(), limits).unwrap();
    let start = Instant::now();
    assert!(
        adapter
            .inspect(false, deadline(), &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(180));
    timeout(SECOND, server).await.unwrap().unwrap();
}
#[tokio::test]
async fn https_rejects_untrusted_certificate_and_accepts_only_frozen_root_and_name() {
    let certified =
        rcgen::generate_simple_self_signed(vec!["locked-ollama.invalid".into()]).unwrap();
    let pem = certified.cert.pem();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certified.cert.der().clone()], key.into())
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    for (trust, host, ok) in [
        (Trust::BundledWebPki, "locked-ollama.invalid", false),
        (
            Trust::PemOnly {
                certificates: vec![pem.clone()],
            },
            "locked-ollama.invalid",
            true,
        ),
        (
            Trust::PemOnly {
                certificates: vec![pem.clone()],
            },
            "wrong-ollama.invalid",
            false,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Endpoint::parse(&format!(
            "https://{host}:{}/tls",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let acceptor = acceptor.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            if let Ok(mut stream) = acceptor.accept(socket).await {
                let req = read_request(&mut stream).await;
                assert!(req.head.starts_with("GET /tls/api/version "));
                head(&mut stream, 200, "").await.unwrap();
                stream.write_all(b"{\"version\":\"0.32.3\"}").await.unwrap();
                stream.shutdown().await.unwrap();
            } else {
                assert!(!ok);
            }
        });
        let transport = NativeTransport::with_resolution(
            endpoint,
            trust,
            Resolution::Pinned {
                addresses: vec!["127.0.0.1".parse().unwrap()],
            },
            &Limits::default(),
        )
        .unwrap();
        let result = timeout(
            SECOND,
            transport.send(Request {
                operation: Operation::Version,
                body: None,
            }),
        )
        .await
        .unwrap();
        if ok {
            let mut response = result.unwrap();
            let mut body = vec![];
            while let Some(chunk) = response.body.next().await {
                body.extend(chunk.unwrap());
            }
            assert_eq!(body, b"{\"version\":\"0.32.3\"}");
        } else {
            assert!(result.is_err());
        }
        timeout(SECOND, server).await.unwrap().unwrap();
    }
}
#[tokio::test]
async fn inherited_proxy_variables_do_not_change_endpoint() {
    let mut server = Server::start(metadata_replies()).await;
    let endpoint = server.url.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proxy_environment_child", "--nocapture"])
            .env("PLAYTIME_OLLAMA_PROXY_TEST_ENDPOINT", endpoint)
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("http_proxy", "http://127.0.0.1:1")
            .env("https_proxy", "http://127.0.0.1:1")
            .env("all_proxy", "http://127.0.0.1:1")
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.finish().await;
    assert_eq!(server.seen.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn proxy_environment_child() {
    let Ok(endpoint) = std::env::var("PLAYTIME_OLLAMA_PROXY_TEST_ENDPOINT") else {
        return;
    };
    let transport = NativeTransport::new(
        Endpoint::parse(&endpoint).unwrap(),
        Trust::BundledWebPki,
        &Limits::default(),
    )
    .unwrap();
    let mut adapter = Adapter::new(transport, lock(), Limits::default()).unwrap();
    adapter
        .inspect(false, deadline(), &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn explicit_frozen_resolution_preserves_the_configured_hostname_and_path() {
    let mut server = Server::start(metadata_replies()).await;
    let url = server.url.replace("127.0.0.1", "locked-ollama.invalid");
    let endpoint = Endpoint::parse(&url).unwrap();
    let resolution = Resolution::Pinned {
        addresses: vec!["127.0.0.1".parse().unwrap()],
    };
    let transport = NativeTransport::with_resolution(
        endpoint,
        Trust::BundledWebPki,
        resolution.clone(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(transport.endpoint().as_str(), url);
    assert_eq!(transport.resolution(), &resolution);
    let mut adapter = Adapter::new(transport, lock(), Limits::default()).unwrap();
    adapter
        .inspect(false, deadline(), &CancellationToken::new())
        .await
        .unwrap();
    server.finish().await;
    for req in server.seen.lock().unwrap().iter() {
        assert!(
            req.head
                .to_ascii_lowercase()
                .contains("host: locked-ollama.invalid:")
        );
        assert!(req.head.lines().next().unwrap().contains(" /base/api/"));
    }
}
