mod common;
use common::*;
use playtime_ollama::*;
use serde_json::{Value, json};

fn model() -> ExecutionModel {
    let lock: Value =
        serde_json::from_slice(include_bytes!("../../../models/qwen3-1.7b.lock.json")).unwrap();
    ExecutionModel {
        name: lock["name"].as_str().unwrap().into(),
        digest: lock["digest"].as_str().unwrap().into(),
        backend_version: lock["ollama_version"].as_str().unwrap().into(),
        definition_sha256: lock["definition_sha256"].as_str().unwrap().into(),
        definition: lock["definition"].clone(),
        reasoning: vec![
            None,
            Some(Thinking::Boolean(true)),
            Some(Thinking::Boolean(false)),
        ],
    }
}
fn selected_metadata() -> Vec<Value> {
    let m = model();
    vec![
        json!({"version": m.backend_version}),
        json!({"models":[{"name":m.name,"digest":m.digest}]}),
        m.definition,
    ]
}
fn replies(values: Vec<Value>) -> Vec<HttpResponse> {
    values
        .into_iter()
        .map(|v| response(200, serde_json::to_vec(&v).unwrap()))
        .collect()
}
fn terminal() -> HttpResponse {
    response(200, b"{\"model\":\"qwen3:1.7b\",\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":true,\"eval_count\":2}\n".to_vec())
}

#[tokio::test]
async fn selected_identity_is_rechecked_before_each_chat_and_booleans_are_not_levels() {
    let mut pending = replies(selected_metadata());
    for _ in 0..3 {
        pending.extend(replies(selected_metadata()));
        pending.push(terminal());
    }
    let fake = Fake::new(pending);
    let seen = fake.seen.clone();
    let mut adapter = Adapter::with_model(fake, model(), Limits::default()).unwrap();
    adapter
        .inspect(false, deadline(), &CancellationToken::new())
        .await
        .unwrap();
    for choice in [
        None,
        Some(Thinking::Boolean(true)),
        Some(Thinking::Boolean(false)),
    ] {
        let (tx, _rx) = observations(&Limits::default()).unwrap();
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings {
                    thinking: choice,
                    ..Default::default()
                },
                deadline(),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        assert_eq!(result.remote, RemoteOutcome::TerminalObserved);
        assert_eq!(result.outcome.unwrap().generated_tokens, Some(2));
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 15);
    for (i, think) in [Value::Null, json!(true), json!(false)]
        .into_iter()
        .enumerate()
    {
        let chunk = &seen[3 + i * 4..7 + i * 4];
        assert_eq!(
            chunk.iter().map(|r| r.operation).collect::<Vec<_>>(),
            [
                Operation::Version,
                Operation::Tags,
                Operation::Show,
                Operation::Chat
            ]
        );
        assert_eq!(
            serde_json::from_slice::<Value>(chunk[2].body.as_ref().unwrap()).unwrap(),
            json!({"model":"qwen3:1.7b"})
        );
        let body: Value = serde_json::from_slice(chunk[3].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["model"], "qwen3:1.7b");
        assert_eq!(body["think"], think);
        assert_eq!(body["options"]["num_predict"], request().maximum_tokens);
    }
}

#[tokio::test]
async fn cached_verification_never_hides_selected_identity_drift() {
    for case in 0..7 {
        let mut changed = selected_metadata();
        match case {
            0 => changed[0]["version"] = json!("changed"),
            1 => changed[1]["models"] = json!([]),
            2 => changed[1]["models"][0]["digest"] = json!("changed"),
            3 => changed[2]["template"] = json!("changed"),
            4 => changed[2]["capabilities"] = json!(["completion"]),
            5 => changed[2]["remote_host"] = json!("remote"),
            _ => changed[1]["models"][0]["name"] = json!("gpt-oss:20b"),
        }
        let fake = Fake::new(
            replies(selected_metadata())
                .into_iter()
                .chain(replies(changed)),
        );
        let seen = fake.seen.clone();
        let mut adapter = Adapter::with_model(fake, model(), Limits::default()).unwrap();
        adapter
            .inspect(false, deadline(), &CancellationToken::new())
            .await
            .unwrap();
        let (tx, _rx) = observations(&Limits::default()).unwrap();
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings {
                    thinking: Some(Thinking::Boolean(false)),
                    ..Default::default()
                },
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
async fn missing_show_is_an_identity_failure_but_other_http_failures_are_not() {
    for (metadata_count, status, kind) in [
        (2, 404, FailureKind::Identity),
        (2, 503, FailureKind::Http),
        (1, 404, FailureKind::Http),
        (0, 404, FailureKind::Http),
    ] {
        // Tags still contain the selected identity: it can disappear before show.
        let fake = Fake::new(
            replies(selected_metadata())
                .into_iter()
                .take(metadata_count)
                .chain([response(
                    status,
                    br#"{"error":"metadata unavailable"}"#.to_vec(),
                )]),
        );
        let seen = fake.seen.clone();
        let mut adapter = Adapter::with_model(fake, model(), Limits::default()).unwrap();
        let (tx, _rx) = observations(&Limits::default()).unwrap();
        let result = adapter
            .chat(
                &request(),
                &EffectiveSettings {
                    thinking: None,
                    ..Default::default()
                },
                deadline(),
                &CancellationToken::new(),
                &tx,
            )
            .await;
        assert_eq!(
            result.outcome.unwrap_err().kind,
            kind,
            "step {metadata_count}, HTTP {status}"
        );
        assert_eq!(result.dispatch, Dispatch::NotDispatched);
        assert_eq!(result.remote, RemoteOutcome::NotStarted);
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|r| r.operation != Operation::Chat)
        );
    }
}

#[tokio::test]
async fn unsupported_reasoning_never_falls_back_or_reads_the_backend() {
    let fake = Fake::new([]);
    let seen = fake.seen.clone();
    let mut adapter = Adapter::with_model(fake, model(), Limits::default()).unwrap();
    let (tx, _rx) = observations(&Limits::default()).unwrap();
    let result = adapter
        .chat(
            &request(),
            &EffectiveSettings {
                thinking: Some(Thinking::Level("high".into())),
                ..Default::default()
            },
            deadline(),
            &CancellationToken::new(),
            &tx,
        )
        .await;
    assert!(result.outcome.is_err());
    assert_eq!(result.dispatch, Dispatch::NotDispatched);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_response_identity_and_cut_stream_keep_dispatch_uncertain() {
    for body in [
        b"{\"model\":\"gpt-oss:20b\",\"message\":{\"role\":\"assistant\",\"content\":\"wrong\"},\"done\":true,\"eval_count\":2}\n".as_slice(),
        b"{\"model\":\"qwen3:1.7b\",\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n".as_slice(),
    ] {
        let fake = Fake::new(replies(selected_metadata()).into_iter().chain([response(200, body.to_vec())]));
        let mut adapter = Adapter::with_model(fake, model(), Limits::default()).unwrap();
        let (tx, mut rx) = observations(&Limits::default()).unwrap();
        let result = adapter.chat(&request(), &EffectiveSettings { thinking: None, ..Default::default() }, deadline(), &CancellationToken::new(), &tx).await;
        assert!(result.outcome.is_err());
        assert_eq!(result.dispatch, Dispatch::PossiblySent);
        assert_eq!(result.remote, RemoteOutcome::Uncertain);
        assert!(rx.try_recv().is_some(), "original received evidence survives failure");
    }
}
