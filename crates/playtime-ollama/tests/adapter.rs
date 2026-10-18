mod common;
use common::*;
use playtime_core::{GenerationUsage, UsageTotals};
use playtime_ollama::*;
use serde_json::{Value, json};
use std::{num::NonZeroU64, time::Duration};
use tokio::time::Instant;

#[tokio::test]
async fn inspection_is_exactly_allowlisted_and_keeps_optional_diagnostics() {
    for loaded in [false, true] {
        let mut values = metadata();
        values[2]["diagnostic"] = json!("extra retained");
        if loaded {
            values.push(json!({"models":[]}));
        }
        let transport = Fake::new(
            values
                .into_iter()
                .map(|v| response(200, serde_json::to_vec(&v).unwrap())),
        );
        let seen = transport.seen.clone();
        let mut adapter = Adapter::new(transport, lock(), Limits::default()).unwrap();
        let info = adapter
            .inspect(loaded, deadline(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            info.canonical_definition,
            include_bytes!("fixtures/definition.canonical.json")
        );
        assert_eq!(info.show["diagnostic"], "extra retained");
        assert_eq!(info.loaded.is_some(), loaded);
        let seen = seen.lock().unwrap();
        let mut allowed = vec![Operation::Version, Operation::Tags, Operation::Show];
        if loaded {
            allowed.push(Operation::Loaded);
        }
        assert_eq!(
            seen.iter().map(|r| r.operation).collect::<Vec<_>>(),
            allowed
        );
        assert_eq!(
            seen[2].body.as_deref(),
            Some(br#"{"model":"gpt-oss:20b"}"#.as_slice())
        );
        assert_eq!(seen[0].body, None);
        assert_eq!(seen[1].body, None);
    }
}
#[tokio::test]
async fn identity_changes_fail_before_any_inference() {
    for case in 0..12 {
        let mut values = metadata();
        match case {
            0 => values[0] = json!({}),
            1 => values[0]["version"] = json!("0.32.4"),
            2 => values[1]["models"] = json!([]),
            3 => values[1]["models"][0]["digest"] = json!("changed"),
            4 => values[2]["template"] = json!("changed"),
            5 => values[2]["capabilities"] = json!(["completion"]),
            6 => values[2]["remote_host"] = json!("remote"),
            7 => values[1]["models"][0]["remote_model"] = json!("remote"),
            8 => {
                let m = values[1]["models"][0].clone();
                values[1]["models"].as_array_mut().unwrap().push(m);
            }
            9 => {
                values[2].as_object_mut().unwrap().remove("parameters");
            }
            10 => values[1]["models"][0]["name"] = json!("other"),
            _ => values[2]["remote_host"] = Value::Null,
        }
        let transport = Fake::new(
            values
                .into_iter()
                .map(|v| response(200, serde_json::to_vec(&v).unwrap())),
        );
        let seen = transport.seen.clone();
        let mut adapter = Adapter::new(transport, lock(), Limits::default()).unwrap();
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
        assert_eq!(
            result.outcome.unwrap_err().kind,
            FailureKind::Identity,
            "case {case}"
        );
        assert_eq!(result.dispatch, Dispatch::NotDispatched);
        assert_eq!(result.remote, RemoteOutcome::NotStarted);
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.operation == Operation::Chat)
        );
    }
}
#[tokio::test]
async fn inspection_http_encoding_size_and_json_failures_never_chat() {
    for (status, encoding, body, limits, expected) in [
        (
            302,
            None,
            b"{}".to_vec(),
            Limits::default(),
            FailureKind::Http,
        ),
        (
            500,
            None,
            b"{}".to_vec(),
            Limits::default(),
            FailureKind::Http,
        ),
        (
            200,
            Some("gzip"),
            b"{}".to_vec(),
            Limits::default(),
            FailureKind::Protocol,
        ),
        (
            200,
            None,
            b"abc".to_vec(),
            Limits {
                metadata_bytes: 2,
                ..Default::default()
            },
            FailureKind::ByteLimit,
        ),
        (
            200,
            None,
            b"{\"version\":\"0.32.3\",\"version\":\"0.32.3\"}".to_vec(),
            Limits::default(),
            FailureKind::Protocol,
        ),
    ] {
        let mut reply = response(status, body);
        reply.content_encoding = encoding.map(str::to_owned);
        let fake = Fake::new([reply]);
        let seen = fake.seen.clone();
        let mut adapter = Adapter::new(fake, lock(), limits).unwrap();
        assert_eq!(
            adapter
                .inspect(false, deadline(), &CancellationToken::new())
                .await
                .unwrap_err()
                .kind,
            expected
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn combined_http_stream_errors_do_not_erase_original_allowances_or_retry() {
    let fake = Fake::ready([
        response(
            500,
            include_bytes!("fixtures/http-parse-error.json").to_vec(),
        ),
        response(200, include_bytes!("fixtures/success.ndjson").to_vec()),
        response(
            200,
            include_bytes!("fixtures/stream-parse-error.ndjson").to_vec(),
        ),
        response(200, include_bytes!("fixtures/success.ndjson").to_vec()),
    ]);
    let seen = fake.seen.clone();
    let mut adapter = Adapter::new(fake, lock(), Limits::default()).unwrap();
    let mut totals = UsageTotals::default();
    let mut observations_seen = vec![];
    for call in 0..4 {
        let mut req = request();
        req.call = call + 1;
        req.sampling_seed = 17 + call as u32;
        let (tx, mut rx) = observations(&Limits::default()).unwrap();
        let result = adapter
            .chat(
                &req,
                &EffectiveSettings::default(),
                deadline(),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        let usage = if call % 2 == 0 {
            assert_eq!(
                result.outcome.unwrap_err().kind,
                FailureKind::BackendToolParse
            );
            assert_eq!(result.remote, RemoteOutcome::Uncertain);
            GenerationUsage::Unknown {
                reserved_tokens: NonZeroU64::new(req.maximum_tokens).unwrap(),
            }
        } else {
            assert_eq!(result.remote, RemoteOutcome::TerminalObserved);
            GenerationUsage::Known {
                generated_tokens: result.outcome.unwrap().generated_tokens.unwrap(),
            }
        };
        totals.record(usage).unwrap();
        while let Some(record) = rx.try_recv() {
            observations_seen.push(record);
        }
    }
    assert_eq!(
        (
            totals.known_generated_tokens(),
            totals.unknown_reserved_tokens(),
            totals.budget_charged_tokens(),
            totals.unknown_requests()
        ),
        (64, 1024, 1088, 2)
    );
    assert_eq!(observations_seen.len(), 5);
    assert!(matches!(
        observations_seen[0].kind,
        ObservationKind::HttpError {
            status: 500,
            truncated: false
        }
    ));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 7);
    for (index, r) in seen[3..].iter().enumerate() {
        assert_eq!(r.operation, Operation::Chat);
        let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["options"]["num_predict"], 512);
        assert_eq!(body["options"]["seed"], 17 + index);
    }
}
#[tokio::test]
async fn http_errors_require_complete_exact_prefix_and_report_truncation() {
    for (body, limit, expected, truncated) in [
        (
            br#"{"error":"error parsing tool call: fixture"}"#.as_slice(),
            512,
            FailureKind::BackendToolParse,
            false,
        ),
        (
            br#"{"error":"Error parsing tool call: fixture"}"#,
            512,
            FailureKind::Http,
            false,
        ),
        (
            br#"{"error":"error parsing tool call: fixture""#,
            512,
            FailureKind::Http,
            false,
        ),
        (
            br#"{"error":"error parsing tool call: fixture"}"#,
            40,
            FailureKind::ByteLimit,
            true,
        ),
    ] {
        let limits = Limits {
            error_bytes: limit,
            ..Default::default()
        };
        let fake = Fake::ready([response(500, body.to_vec())]);
        let mut adapter = Adapter::new(fake, lock(), limits.clone()).unwrap();
        let (tx, mut rx) = observations(&limits).unwrap();
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings::default(),
                deadline(),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        assert_eq!(result.outcome.unwrap_err().kind, expected);
        assert_eq!(result.remote, RemoteOutcome::Uncertain);
        let record = rx.try_recv().unwrap();
        assert_eq!(
            record.kind,
            ObservationKind::HttpError {
                status: 500,
                truncated
            }
        );
        assert_eq!(record.bytes, &body[..body.len().min(limit)]);
    }
}
#[tokio::test]
async fn cancellation_timeout_and_oversize_before_dispatch_are_explicit() {
    let fake = Fake::new([]);
    let seen = fake.seen.clone();
    let mut adapter = Adapter::new(fake, lock(), Limits::default()).unwrap();
    let (tx, _rx) = observations(&Limits::default()).unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    for (req, deadline, cancel, kind) in [
        (request(), deadline(), cancelled, FailureKind::Cancelled),
        (
            request(),
            Instant::now(),
            CancellationToken::new(),
            FailureKind::Timeout,
        ),
        (
            {
                let mut r = request();
                r.messages[0]["content"] = json!("x".repeat(4 * 1024 * 1024));
                r
            },
            deadline(),
            CancellationToken::new(),
            FailureKind::ByteLimit,
        ),
    ] {
        let result = adapter
            .chat(&req, &EffectiveSettings::default(), deadline, &cancel, &tx)
            .await;
        assert_eq!(result.outcome.unwrap_err().kind, kind);
        assert_eq!(result.dispatch, Dispatch::NotDispatched);
        assert_eq!(result.remote, RemoteOutcome::NotStarted);
    }
    assert!(seen.lock().unwrap().is_empty());
}
#[tokio::test]
async fn quiet_stream_deadline_preserves_partial_and_drops_owned_receiver() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Pending {
        sent: bool,
        dropped: Arc<AtomicBool>,
    }
    impl futures_util::Stream for Pending {
        type Item = Result<Vec<u8>>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if self.sent {
                std::task::Poll::Pending
            } else {
                self.sent = true;
                std::task::Poll::Ready(Some(Ok(
                    b"{\"message\":{\"thinking\":\"partial\"}}\n{".to_vec()
                )))
            }
        }
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let fake = Fake::ready([HttpResponse {
        status: 200,
        content_encoding: None,
        body: Box::pin(Pending {
            sent: false,
            dropped: dropped.clone(),
        }),
    }]);
    let mut adapter = Adapter::new(fake, lock(), Limits::default()).unwrap();
    let (tx, _rx) = observations(&Limits::default()).unwrap();
    let result = adapter
        .chat(
            &request(),
            &EffectiveSettings::default(),
            Instant::now() + Duration::from_millis(100),
            &CancellationToken::new(),
            &tx,
        )
        .await;
    assert_eq!(result.outcome.unwrap_err().kind, FailureKind::Timeout);
    assert_eq!(result.remote, RemoteOutcome::Uncertain);
    assert_eq!(result.partial.thinking.as_deref(), Some("partial"));
    assert_eq!(result.pending_fragment, b"{");
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn metadata_request_aggregate_and_caller_deadlines_include_body_reads() {
    for (delay, caller_seconds, expected_seconds) in [(16, 90, 15), (14, 90, 45), (14, 5, 5)] {
        let fake = Fake::new(
            metadata()
                .into_iter()
                .chain([json!({"models":[]})])
                .map(|value| HttpResponse {
                    status: 200,
                    content_encoding: None,
                    body: Box::pin(futures_util::stream::once(async move {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        Ok(serde_json::to_vec(&value).unwrap())
                    })),
                }),
        );
        let mut adapter = Adapter::new(fake, lock(), Limits::default()).unwrap();
        let start = Instant::now();
        assert_eq!(
            adapter
                .inspect(
                    true,
                    start + Duration::from_secs(caller_seconds),
                    &CancellationToken::new()
                )
                .await
                .unwrap_err()
                .kind,
            FailureKind::Timeout
        );
        assert_eq!(start.elapsed(), Duration::from_secs(expected_seconds));
    }
}

#[tokio::test(start_paused = true)]
async fn caller_deadline_bounds_unfinished_resolution_or_setup_before_headers() {
    struct Setup;
    #[async_trait::async_trait]
    impl Transport for Setup {
        async fn send(&self, _: Request) -> Result<HttpResponse> {
            std::future::pending().await
        }
    }
    let mut adapter = Adapter::new(Setup, lock(), Limits::default()).unwrap();
    let start = Instant::now();
    assert_eq!(
        adapter
            .inspect(
                false,
                start + Duration::from_secs(2),
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .kind,
        FailureKind::Timeout
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
}

#[tokio::test]
async fn observed_response_identity_change_invalidates_checked_identity() {
    let fake = Fake::ready([
        response(
            200,
            b"{\"model\":\"other\",\"done\":true,\"eval_count\":0}\n".to_vec(),
        ),
        response(200, b"{\"version\":\"changed\"}".to_vec()),
    ]);
    let seen = fake.seen.clone();
    let mut adapter = Adapter::new(fake, lock(), Limits::default()).unwrap();
    let (tx, _rx) = observations(&Limits::default()).unwrap();
    for expected_dispatch in [Dispatch::PossiblySent, Dispatch::NotDispatched] {
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings::default(),
                deadline(),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        assert_eq!(result.outcome.unwrap_err().kind, FailureKind::Identity);
        assert_eq!(result.dispatch, expected_dispatch);
    }
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .map(|r| r.operation)
            .collect::<Vec<_>>(),
        [
            Operation::Version,
            Operation::Tags,
            Operation::Show,
            Operation::Chat,
            Operation::Version
        ]
    );
}
