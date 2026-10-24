mod common;
use common::*;
use playtime_protocol::api::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn every_surface_requires_bearer_and_rejects_browser_origins() {
    let h = Harness::new().await;
    let routes = [
        ("GET", "capabilities"),
        ("GET", "openapi.json"),
        ("GET", "models"),
        ("POST", "models/inspect"),
        ("GET", "recipes"),
        ("POST", "recipes/fixture-example/preview"),
        ("POST", "runs"),
        ("GET", "launches/key"),
        ("GET", "runs"),
        ("GET", "runs/run"),
        ("POST", "runs/run/cancel"),
        ("GET", "runs/run/events"),
        ("GET", "runs/run/events/stream"),
        ("GET", "runs/run/checkpoints"),
        ("GET", "runs/run/checkpoints/s1"),
        ("GET", "runs/run/artifacts"),
        ("GET", "runs/run/artifacts/file"),
        ("GET", "runs/run/artifacts/file/content"),
    ];
    for (method, path) in routes {
        let url = format!("{}/api/v1/{path}", h.server.endpoint());
        let method: reqwest::Method = method.parse().unwrap();
        code(
            h.raw.request(method.clone(), &url).send().await.unwrap(),
            401,
            "unauthenticated",
        )
        .await;
        code(
            h.raw
                .request(method.clone(), &url)
                .header("cookie", format!("token={}", h.token.expose()))
                .send()
                .await
                .unwrap(),
            401,
            "unauthenticated",
        )
        .await;
        code(
            h.raw
                .request(method, &url)
                .bearer_auth(h.token.expose())
                .header("Origin", "null")
                .send()
                .await
                .unwrap(),
            403,
            "forbidden_origin",
        )
        .await;
    }
    code(
        h.raw
            .get(format!(
                "{}/api/v1/capabilities?token={}",
                h.server.endpoint(),
                h.token.expose()
            ))
            .send()
            .await
            .unwrap(),
        401,
        "unauthenticated",
    )
    .await;
    assert!(h.fixture.calls().is_empty());
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn host_and_mutation_service_identity_are_checked_before_application_calls() {
    let h = Harness::new().await;
    for host in [
        "evil.example:8766",
        "localhost.evil:8766",
        "127.0.0.1.evil:8766",
        "localhost",
        "127.0.0.1:0",
        "[::1]:65536",
        "user@localhost:8766",
    ] {
        code(
            h.get("capabilities")
                .header("host", host)
                .send()
                .await
                .unwrap(),
            403,
            "forbidden_host",
        )
        .await;
    }
    for host in ["localhost:12345", "127.0.0.1:9999", "[::1]:9999"] {
        let response = h
            .get("capabilities")
            .header("host", host)
            .header("x-forwarded-host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }
    code(
        h.raw
            .post(format!("{}/api/v1/runs", h.server.endpoint()))
            .bearer_auth(h.token.expose())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap(),
        400,
        "malformed_data",
    )
    .await;
    code(
        h.raw
            .post(format!("{}/api/v1/runs", h.server.endpoint()))
            .bearer_auth(h.token.expose())
            .header(SERVICE_HEADER, "wrong")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap(),
        409,
        "service_identity_mismatch",
    )
    .await;
    code(
        h.post("runs")
            .header(SERVICE_HEADER, "not a valid id")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap(),
        400,
        "malformed_data",
    )
    .await;
    assert!(h.fixture.calls().is_empty());
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn mutation_decoders_reject_duplicates_unknown_settings_and_wrong_media() {
    let h = Harness::new().await;
    for text in [
        r#"{"mode":"simulation","mode":"experiment"}"#,
        r#"{"mode":"simulation","temperature":0}"#,
        r#"{"mode":"future_mode"}"#,
        "[]",
        "null",
    ] {
        code(
            h.post("recipes/fixture-example/preview")
                .header("content-type", "application/json")
                .body(text)
                .send()
                .await
                .unwrap(),
            400,
            "malformed_data",
        )
        .await;
    }
    code(
        h.post("recipes/fixture-example/preview")
            .header("content-type", "text/plain")
            .body("{}")
            .send()
            .await
            .unwrap(),
        415,
        "unsupported_media_type",
    )
    .await;
    code(
        h.post("recipes/fixture-example/preview")
            .header("content-encoding", "gzip")
            .json(&serde_json::json!({"mode":"simulation"}))
            .send()
            .await
            .unwrap(),
        415,
        "unsupported_media_type",
    )
    .await;
    for key in ["", "a b", "../path", &"x".repeat(129)] {
        code(h.post("runs").header("idempotency-key", key).json(&serde_json::json!({"recipe_id":"fixture-example","mode":"simulation","preview_revision":"fixture-v1"})).send().await.unwrap(), 400, "malformed_data").await;
    }
    for path in [
        "recipes?limit=0",
        "recipes?limit=1001",
        "recipes?limit=1&limit=2",
        "capabilities?token=ignored",
        "recipes?offset=1",
    ] {
        code(h.get(path).send().await.unwrap(), 400, "malformed_data").await;
    }
    assert!(h.fixture.calls().is_empty());
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn actual_chunked_input_bytes_are_bounded_and_diagnostics_do_not_echo_payloads() {
    let h = Harness::new().await;
    let mut socket = tokio::net::TcpStream::connect(h.server.address)
        .await
        .unwrap();
    let head = format!(
        "POST /api/v1/runs HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nX-Playtime-Service-ID: service-test\r\nIdempotency-Key: chunked\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        h.server.address,
        h.token.expose()
    );
    socket.write_all(head.as_bytes()).await.unwrap();
    let chunk = "x".repeat((1 << 20) + 1);
    socket
        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(chunk.as_bytes()).await.unwrap();
    socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 413"));
    assert!(response.contains("limit_exceeded"));
    assert!(!response.contains(&h.token.expose()));
    let secret = format!("model payload {}", h.token.expose());
    let reply = h
        .post("recipes/fixture-example/preview")
        .json(&serde_json::json!({"mode":secret}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!reply.contains(&h.token.expose()));
    assert!(!reply.contains("model payload"));
    assert!(h.fixture.calls().is_empty());
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn artifact_paths_are_not_client_selected_and_downloads_have_safe_headers() {
    let h = Harness::new().await;
    let run = h.client.launch(&h.pending("content").await).await.unwrap();
    h.fixture
        .reveal(&run.run_id, 4, Some(StreamEndStatus::CommittedCompletion));
    let response = h
        .get(&format!(
            "runs/{}/artifacts/saved-image/content",
            run.run_id
        ))
        .header("range", "bytes=1-2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(response.headers()["accept-ranges"], "none");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"saved-image\""
    );
    assert!(response.headers().contains_key("content-digest"));
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        include_bytes!("fixtures/pixel.png")
    );
    code(
        h.get(&format!(
            "runs/{}/artifacts/saved-image/content?path=/etc/passwd",
            run.run_id
        ))
        .send()
        .await
        .unwrap(),
        400,
        "malformed_data",
    )
    .await;
    code(
        h.get(&format!(
            "runs/{}/artifacts/https%3A%2F%2Fevil/content",
            run.run_id
        ))
        .send()
        .await
        .unwrap(),
        400,
        "malformed_data",
    )
    .await;
    h.server.stop().await.unwrap();
}

#[tokio::test]
async fn listener_and_client_reject_non_loopback_and_do_not_follow_redirects() {
    let h = Harness::new().await;
    let server = playtime_server::http::Server::new(h.fixture.clone(), h.token.clone()).unwrap();
    assert!(server.listen("0.0.0.0:0".parse().unwrap()).await.is_err());
    for endpoint in [
        "http://example.com:8766",
        "http://192.168.1.1:8766",
        "http://127.0.0.1:8766/path",
        "http://user:secret@localhost:8766",
        "https://localhost:8766",
        "http://localhost:8766/?token=x",
    ] {
        assert!(matches!(
            playtime_client::Client::connect(endpoint, h.token.clone(), None).await,
            Err(playtime_client::Error::UnsupportedEndpoint)
        ));
    }
    h.server.stop().await.unwrap();
}
