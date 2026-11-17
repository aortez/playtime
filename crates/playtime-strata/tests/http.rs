mod support;
use playtime_strata::*;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
    time::Instant,
};

#[derive(Clone)]
enum Reply {
    Complete,
    Eof,
    Stall,
    Drop,
    Status(u16),
    WrongType,
}
struct State {
    status: Value,
    props: Value,
    slots: Value,
    reply: Reply,
    posts: Vec<Value>,
}
struct Server {
    endpoint: Endpoint,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Endpoint::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let (props, status, _) = metadata();
        let state = Arc::new(Mutex::new(State {
            status,
            props,
            slots: Value::Null,
            reply,
            posts: vec![],
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (mut socket,_)=accepted.unwrap();let shared=shared.clone();
                        connections.spawn(async move {
                            let mut bytes=Vec::new();let mut chunk=[0;4096];
                            let header_end=loop {
                                let n=socket.read(&mut chunk).await.unwrap();if n==0{return;}
                                bytes.extend_from_slice(&chunk[..n]);assert!(bytes.len()<65536);
                                if let Some(at)=bytes.windows(4).position(|w|w==b"\r\n\r\n"){break at+4;}
                            };
                            let header=String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                            let length=header.lines().find_map(|l|l.to_ascii_lowercase().strip_prefix("content-length:").map(|v|v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                            while bytes.len()<header_end+length {
                                let n=socket.read(&mut chunk).await.unwrap();if n==0{return;}bytes.extend_from_slice(&chunk[..n]);
                            }
                            let path=header.split_whitespace().nth(1).unwrap();
                            let (body,reply)= {
                                let mut state=shared.lock().unwrap();
                                if path=="/v1/responses" {
                                    state.posts.push(serde_json::from_slice(&bytes[header_end..header_end+length]).unwrap());
                                    (answer().bytes(),state.reply.clone())
                                } else {
                                    let value=match path {"/props"=>state.props.clone(),"/v1/status"=>state.status.clone(),"/settings"=>metadata().2,"/slots"=>state.slots.clone(),_=>panic!("unexpected path")};
                                    let reply = if path == "/settings" && state.status["service"] == "basalt" { Reply::Status(404) } else { Reply::Complete };
                                    (serde_json::to_vec(&value).unwrap(),reply)
                                }
                            };
                            if matches!(reply,Reply::Drop){return;}
                            let mut body=body;
                            if matches!(reply,Reply::Eof|Reply::Stall) {
                                let mut stream=answer();stream.events.truncate(5);body=stream.bytes();
                            }
                            let status=if let Reply::Status(code)=reply {code}else{200};
                            let content_type=if path=="/v1/responses" && !matches!(reply,Reply::WrongType){"text/event-stream"}else{"application/json"};
                            let length=if matches!(reply,Reply::Stall){String::new()}else{format!("Content-Length: {}\r\n",body.len())};
                            if socket.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Type: {content_type}\r\nConnection: close\r\n{length}\r\n").as_bytes()).await.is_err(){return;}
                            if socket.write_all(&body).await.is_err(){return;}
                            if matches!(reply,Reply::Stall){let _=socket.read(&mut chunk).await;}
                        });
                    },
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Self {
            endpoint,
            state,
            task,
        }
    }
    fn client(&self) -> Client {
        Client::new(self.endpoint.clone(), lock(), Limits::default()).unwrap()
    }
    fn posts(&self) -> usize {
        self.state.lock().unwrap().posts.len()
    }
}

#[tokio::test]
async fn basalt_has_no_settings_endpoint_and_shares_only_its_locked_capacity() {
    let server = Server::new(Reply::Complete).await;
    let mut model = lock();
    {
        let mut state = server.state.lock().unwrap();
        state.status["service"] = json!("basalt");
        state.status["engine"] = json!("0.4.1");
        state.status["concurrency"]["serving"] = json!(2);
        state.status["activity"]["in_flight"] = json!(1);
        state.props["build_info"] = json!("basalt 0.4.1");
        state.props["total_slots"] = json!(2);
        state.slots = json!([{ "id": 0, "n_ctx": model.identity.context_tokens, "is_processing": true },
                             { "id": 1, "n_ctx": model.identity.context_tokens, "is_processing": false }]);
        model.identity = Identity::inspect(&state.props, &state.status, &Value::Null).unwrap();
    }
    model.schema_version = 2;
    model.host.backend = "cuda".into();
    model.host.engine_sha256 = playtime_core::Sha256Digest::new(
        "5a15f95f8bf41fbd61c8e7299910d08049243eb32b26699b0b7a6f7976ad92e8",
    )
    .unwrap();
    model.host.responses_source_sha256 = playtime_core::Sha256Digest::new(
        "c1ba29dd88ac98f052b34e75de46106737a559871c75eb565fb852930e7bcf23",
    )
    .unwrap();
    let mut client =
        Client::new(server.endpoint.clone(), model.clone(), Limits::default()).unwrap();
    assert!(call(&mut client).await.completion.is_ok());
    assert_eq!(
        server.state.lock().unwrap().posts[0]["relaxed_drafting"],
        false
    );
    // The real Basalt status remains 1 even when both batch slots are busy.
    server.state.lock().unwrap().slots[1]["is_processing"] = json!(true);
    assert_eq!(
        call(&mut client).await.completion.unwrap_err().kind,
        FailureKind::Busy
    );
    assert_eq!(server.posts(), 1);
    server.state.lock().unwrap().slots[1]["is_processing"] = Value::Null;
    assert_eq!(
        call(&mut client).await.completion.unwrap_err().kind,
        FailureKind::Identity
    );
    assert_eq!(server.posts(), 1);
    server.state.lock().unwrap().props["total_slots"] = json!(1);
    assert_eq!(
        call(&mut client).await.completion.unwrap_err().kind,
        FailureKind::Identity
    );
    assert_eq!(server.posts(), 1);
    model.host.responses_source_sha256 = playtime_core::Sha256Digest::new("1".repeat(64)).unwrap();
    assert!(Client::new(server.endpoint.clone(), model, Limits::default()).is_err());
    let old = serde_json::to_value(lock()).unwrap();
    assert!(old["identity"].get("service").is_none());
    assert!(old["identity"].get("parallel").is_none());
}
async fn call(client: &mut Client) -> CallResult {
    let (tx, mut rx) = events();
    let req = request();
    let cancel = CancellationToken::new();
    let future = client.complete(&req, Instant::now() + Duration::from_secs(2), &cancel, tx);
    let (result, ()) = tokio::join!(future, async { while rx.recv().await.is_some() {} });
    result
}

#[test]
fn endpoint_does_not_accept_ambient_hosts_credentials_or_routes() {
    for url in ["http://127.0.0.1:80/v1", "http://[::1]:8080/"] {
        assert!(Endpoint::parse(url).is_ok());
    }
    for url in [
        "https://127.0.0.1",
        "http://localhost:8080",
        "http://192.168.1.2:8080",
        "http://u:p@127.0.0.1",
        "http://127.0.0.1/?key=x",
        "http://127.0.0.1/responses",
    ] {
        assert!(Endpoint::parse(url).is_err());
    }
}

#[tokio::test]
async fn metadata_and_process_identity_are_checked_before_every_dispatch() {
    let server = Server::new(Reply::Complete).await;
    let mut client = server.client();
    assert!(call(&mut client).await.completion.is_ok());
    assert_eq!(server.posts(), 1);
    server.state.lock().unwrap().status["started"] = json!(101);
    let result = call(&mut client).await;
    assert_eq!(result.remote, RemoteState::NotDispatched);
    assert_eq!(result.completion.unwrap_err().kind, FailureKind::Identity);
    assert_eq!(server.posts(), 1);
    let mut client = server.client();
    server.state.lock().unwrap().status["activity"]["in_flight"] = json!(1);
    assert_eq!(
        call(&mut client).await.completion.unwrap_err().kind,
        FailureKind::Busy
    );
    assert_eq!(server.posts(), 1);
    server.state.lock().unwrap().status["activity"]["in_flight"] = json!(0);
    server.state.lock().unwrap().props["chat_template"] = json!("changed");
    assert_eq!(
        call(&mut client).await.completion.unwrap_err().kind,
        FailureKind::Identity
    );
    assert_eq!(server.posts(), 1);
}

#[tokio::test]
async fn a_dispatched_failure_is_never_retried_or_reported_as_zero_usage() {
    for reply in [
        Reply::Eof,
        Reply::Drop,
        Reply::Status(503),
        Reply::Status(307),
        Reply::WrongType,
    ] {
        let server = Server::new(reply).await;
        let result = call(&mut server.client()).await;
        assert!(result.completion.is_err());
        assert_eq!(result.completion.unwrap_err().generated_tokens, None);
        assert_eq!(result.remote, RemoteState::Uncertain);
        assert_eq!(server.posts(), 1);
    }
}

#[tokio::test]
async fn cancellation_retains_partial_output_without_claiming_remote_termination() {
    let server = Server::new(Reply::Stall).await;
    let mut client = server.client();
    let (tx, mut rx) = events();
    let req = request();
    let cancel = CancellationToken::new();
    let future = client.complete(&req, Instant::now() + Duration::from_secs(2), &cancel, tx);
    let (result, ()) = tokio::join!(future, async {
        while let Some(event) = rx.recv().await {
            if event.controller_chunk().is_some() {
                cancel.cancel();
            }
        }
    });
    assert_eq!(result.completion.unwrap_err().kind, FailureKind::Cancelled);
    assert_eq!(result.remote, RemoteState::Uncertain);
    assert_eq!(result.partial["items"][0]["text"], "h");
    assert_eq!(server.posts(), 1);
    let mut client = server.client();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, _rx) = events();
    let result = client
        .complete(&req, Instant::now() + Duration::from_secs(2), &cancel, tx)
        .await;
    assert_eq!(result.remote, RemoteState::NotDispatched);
    assert_eq!(server.posts(), 1);
}

#[tokio::test]
async fn absolute_deadline_survives_a_stall_and_event_consumer_backpressure() {
    for reply in [Reply::Stall, Reply::Complete] {
        let server = Server::new(reply).await;
        let mut client = server.client();
        let (tx, _rx) = events();
        let result = client
            .complete(
                &request(),
                Instant::now() + Duration::from_millis(250),
                &CancellationToken::new(),
                tx,
            )
            .await;
        assert_eq!(result.completion.unwrap_err().kind, FailureKind::Timeout);
        assert_eq!(server.posts(), 1);
    }
}

#[tokio::test]
async fn terminal_http_exchange_preserves_measured_usage() {
    let server = Server::new(Reply::Complete).await;
    let result = call(&mut server.client()).await;
    assert_eq!(result.remote, RemoteState::TerminalObserved);
    assert_eq!(
        result.completion.unwrap().response.generated_tokens,
        Some(40)
    );
}
