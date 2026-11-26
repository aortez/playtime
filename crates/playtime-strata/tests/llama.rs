//! llama-server's Chat Completions profile. Fixtures are synthetic, in the
//! shapes llama-server v1.0.0 streamed on 2026-11-25; see README.md.
use playtime_core::{RequestId, RequestPurpose, Sha256Digest};
use playtime_engine::{ModelFailure, ModelRequest, Outcome};
use playtime_strata::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{Duration, Instant},
};

const MODEL: &str = "fixture-qwen3";
const ENGINE: &str = "b1-b1b770d";
const COMMIT: &str = "b1b770d90d94cad9e9ea5b993bdbc383a522fb28";
/// 32 random letters and digits, as llama-server draws them for each process.
const MARKER: &str = "<__media_Qx5rYWJzBUUEQOALnZGUF7B3mLkf3mKb__>";

fn props() -> Value {
    json!({"default_generation_settings":{"n_ctx":40960,"params":{"temperature":0.8,"seed":4294967295u32}},
        "total_slots":1,"model_alias":MODEL,"model_path":"/fixture/qwen3.gguf",
        "model_ftype":"Q4_K - Medium","chat_template":"synthetic template",
        "chat_template_caps":{"supports_tools":true,"supports_tool_calls":true,
            "supports_parallel_tool_calls":true,"reasoning_effort_levels":[]},
        "bos_token":"<|endoftext|>","eos_token":"<|im_end|>","build_info":ENGINE,
        "is_sleeping":false,"modalities":{"vision":false,"video":false,"audio":false},
        "endpoint_slots":true,"endpoint_props":false,"endpoint_metrics":false,"ui":false,
        "ui_settings":{},"cors_proxy_enabled":false,"media_marker":MARKER})
}
fn slots(processing: bool) -> Value {
    json!([{"id":0,"n_ctx":40960,"speculative":false,"is_processing":processing}])
}
fn models() -> Value {
    json!({"object":"list","data":[{"id":MODEL,"object":"model",
        "meta":{"n_ctx":40960,"n_params":2031739904u64,"ftype":"Q4_K - Medium"}}]})
}
fn lock() -> ModelLock {
    let hash = || Sha256Digest::new("1".repeat(64)).unwrap();
    ModelLock {
        schema_version: 3,
        identity: Identity::inspect_llama(&props(), &slots(false), &models()).unwrap(),
        host: HostIdentity {
            backend: "cpu".into(),
            quantization: "Q4_K_M".into(),
            engine_sha256: hash(),
            artifacts_manifest_sha256: hash(),
            launch_configuration_sha256: hash(),
            responses_source_sha256: None,
            server_release_manifest_sha256: None,
            source_commit: Some(COMMIT.into()),
            build_manifest_sha256: Some(hash()),
        },
        settings: Settings {
            thinking: Thinking::On,
            ..Settings::default()
        },
    }
}
fn request() -> ModelRequest {
    ModelRequest {
        request_id: RequestId::new("request-1").unwrap(),
        call: 1,
        purpose: RequestPurpose::Work,
        sampling_seed: 42,
        maximum_tokens: 512,
        context_input_bound: 1000,
        context_reserve: 256,
        context_basis: "fixture".into(),
        messages: vec![
            json!({"role":"system","content":"You are a coding agent."}),
            json!({"role":"user","content":"Inspect the file."}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read_file","description":"Read file",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        ],
    }
}
fn body(request: &ModelRequest, lock: &ModelLock) -> Result<Value> {
    Ok(serde_json::from_slice(&encode_request(request, lock, &Limits::default())?).unwrap())
}

/// Chunks in llama-server's order: the role, one delta per chunk, the finish
/// chunk, the usage chunk and `[DONE]`.
struct Chunks(Vec<String>);
impl Chunks {
    fn new() -> Self {
        let mut chunks = Self(vec![]);
        chunks.delta(json!({"role":"assistant","content":null}));
        chunks
    }
    fn envelope(&mut self, mut extra: Value) {
        let mut chunk = json!({"created":1795656790,"id":"chatcmpl-fixture","model":MODEL,
            "system_fingerprint":ENGINE,"object":"chat.completion.chunk"});
        chunk
            .as_object_mut()
            .unwrap()
            .append(extra.as_object_mut().unwrap());
        self.0.push(chunk.to_string());
    }
    fn delta(&mut self, delta: Value) {
        self.envelope(json!({"choices":[{"finish_reason":null,"index":0,"delta":delta}]}));
    }
    fn text(&mut self, field: &str, text: &str) {
        for c in text.chars() {
            self.delta(json!({field: c.to_string()}));
        }
    }
    fn call(&mut self, index: u64, name: &str, arguments: &str) {
        let (first, rest) = arguments.split_at(1);
        self.delta(
            json!({"tool_calls":[{"index":index,"id":format!("call{index}"),
            "type":"function","function":{"name":name,"arguments":first}}]}),
        );
        for c in rest.chars() {
            self.delta(
                json!({"tool_calls":[{"index":index,"function":{"arguments":c.to_string()}}]}),
            );
        }
    }
    fn end(&mut self, reason: &str) {
        self.envelope(json!({"choices":[{"finish_reason":reason,"index":0,"delta":{}}]}));
        self.envelope(
            json!({"choices":[],"usage":{"completion_tokens":40,"prompt_tokens":200,
            "total_tokens":240,"prompt_tokens_details":{"cached_tokens":12}},
            "timings":{"predicted_n":40}}),
        );
        self.0.push("[DONE]".into());
    }
    fn error(&mut self, message: &str) {
        self.0.push(
            json!({"error":{"code":500,"message":message,"type":"server_error"}}).to_string(),
        );
    }
    fn bytes(&self) -> Vec<u8> {
        self.0
            .iter()
            .map(|data| format!("data: {data}\n\n"))
            .collect::<String>()
            .into_bytes()
    }
}
fn decoder() -> ChatDecoder {
    ChatDecoder::new(MODEL, ENGINE, Limits::default()).unwrap()
}
fn decode(bytes: &[u8]) -> Result<Completion> {
    let mut decoder = decoder();
    decoder.push(bytes)?;
    decoder.finish()
}
fn protocol_error(bytes: &[u8]) -> bool {
    matches!(
        decode(bytes),
        Err(Failure {
            kind: FailureKind::Protocol,
            ..
        })
    )
}

#[test]
fn reasoning_text_and_parallel_calls_round_trip_into_a_stateless_follow_up() {
    let mut chunks = Chunks::new();
    chunks.text("reasoning_content", "Check café 🐦.");
    chunks.text("content", "Reading both.");
    chunks.call(0, "read_file", r#"{"path": "a"}"#);
    chunks.call(1, "read_file", r#"{"path": "b"}"#);
    chunks.end("tool_calls");
    let bytes = chunks.bytes();
    for size in [1, 2, 3, 7, 1024, usize::MAX] {
        let mut decoder = decoder();
        let mut observed = Vec::new();
        for part in bytes.chunks(size.min(bytes.len())) {
            decoder.push(part).unwrap();
            observed.extend(
                decoder
                    .take_events()
                    .iter()
                    .filter_map(StreamEvent::controller_chunk),
            );
        }
        assert_eq!(observed[0], json!({"item_id":"reasoning","thinking":"C"}));
        assert!(observed.contains(&json!({"item_id":"message","content":"R"})));
        assert!(observed.contains(&json!({"item_id":"tool_call_1","arguments_delta":"{"})));
        assert!(decoder.terminal_observed());
        let done = decoder.finish().unwrap();
        assert_eq!(done.finish_reason, "tool_calls");
        assert_eq!(done.response_id, "chatcmpl-fixture");
        assert_eq!(done.response.generated_tokens, Some(40));
        assert_eq!(done.response.prompt_tokens, Some(200));
        assert_eq!(done.cached_input_tokens, Some(12));
        assert_eq!(done.reasoning_tokens, None);
        assert_eq!(done.timings, Some(json!({"predicted_n":40})));
        let message = &done.response.message;
        assert_eq!(message["thinking"], "Check café 🐦.");
        assert_eq!(message["content"], "Reading both.");
        assert_eq!(message["tool_calls"][1]["id"], "call1");
        assert_eq!(
            message["tool_calls"][1]["function"]["arguments"]["path"],
            "b"
        );

        let mut follow_up = request();
        follow_up.messages.push(done.response.message);
        for output in ["first", "second"] {
            follow_up
                .messages
                .push(json!({"role":"tool","tool_name":"read_file","content":output}));
        }
        let sent = body(&follow_up, &lock()).unwrap();
        let assistant = &sent["messages"][2];
        assert_eq!(assistant["reasoning_content"], "Check café 🐦.");
        assert_eq!(assistant["content"], "Reading both.");
        assert_eq!(
            assistant["tool_calls"][0],
            json!({"id":"call0","type":"function",
                "function":{"name":"read_file","arguments":r#"{"path":"a"}"#}})
        );
        assert_eq!(
            sent["messages"][4],
            json!({"role":"tool","tool_call_id":"call1","content":"second"})
        );
    }
}

#[test]
fn requests_name_llama_samplers_and_switch_thinking_by_purpose() {
    let sent = body(&request(), &lock()).unwrap();
    assert_eq!(sent["model"], MODEL);
    assert_eq!(sent["max_tokens"], 512);
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"], json!({"include_usage":true}));
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["parallel_tool_calls"], true);
    assert_eq!(sent["reasoning_format"], "deepseek");
    assert_eq!(
        sent["chat_template_kwargs"],
        json!({"enable_thinking":true})
    );
    assert_eq!(sent["repeat_penalty"], 1.0);
    assert_eq!(sent["repeat_last_n"], 64);
    assert_eq!(sent["seed"], 42);
    assert_eq!(sent["tools"][0], request().tools[0]);
    assert!(sent.get("repetition_penalty").is_none() && sent.get("input").is_none());

    let mut summary = request();
    summary.purpose = RequestPurpose::Compaction;
    assert_eq!(
        body(&summary, &lock()).unwrap()["chat_template_kwargs"],
        json!({"enable_thinking":false})
    );
}

#[test]
fn tools_are_written_in_their_usual_order() {
    // llama-server renders a schema's keys in the order they arrive. Property
    // names and the body's other fields keep their sorted order.
    let mut chat = request();
    chat.tools = vec![json!({"type":"function","function":{"name":"write_file",
        "description":"Write a file","parameters":{"type":"object","additionalProperties":false,
        "required":["path"],"properties":{"path":{"type":"string","description":"Where"},
        "type":{"type":"string","enum":["text","binary"]}}}}})];
    let sent = encode_request(&chat, &lock(), &Limits::default()).unwrap();
    let sent = String::from_utf8(sent).unwrap();
    let expected = concat!(
        r#""tool_choice":"auto","tools":[{"type":"function","function":{"name":"write_file","#,
        r#""description":"Write a file","parameters":{"type":"object","properties":{"#,
        r#""path":{"type":"string","description":"Where"},"#,
        r#""type":{"type":"string","enum":["text","binary"]}},"required":["path"],"#,
        r#""additionalProperties":false}}}],"top_k":"#,
    );
    assert!(sent.contains(expected), "{sent}");
}

#[test]
fn requests_llama_server_would_misread_are_refused() {
    let refused = |request: &ModelRequest, lock: &ModelLock| {
        matches!(
            encode_request(request, lock, &Limits::default()),
            Err(Failure {
                kind: FailureKind::Request,
                ..
            })
        )
    };
    // A trailing assistant message would be continued as a prefill.
    let mut prefill = request();
    prefill
        .messages
        .push(json!({"role":"assistant","content":"Partial"}));
    assert!(refused(&prefill, &lock()));
    let mut random = request();
    random.sampling_seed = u32::MAX;
    assert!(refused(&random, &lock()));
    let mut developer = request();
    developer.messages[0]["role"] = json!("developer");
    assert!(refused(&developer, &lock()));
    // Without tool support, llama-server would drop the tools.
    let mut toolless = lock();
    toolless.identity.template_capabilities["supports_tool_calls"] = json!(false);
    assert!(refused(&request(), &toolless));
    let mut chat = request();
    chat.tools.clear();
    assert!(!refused(&chat, &toolless));
}

#[test]
fn llama_locks_pin_the_reviewed_release_and_its_thinking_switch() {
    assert!(lock().validate().is_ok());
    assert_eq!(lock().identity.service, Service::LlamaServer);
    let rejected = |change: &dyn Fn(&mut ModelLock)| {
        let mut lock = lock();
        change(&mut lock);
        lock.validate().is_err()
    };
    assert!(rejected(&|l| l.settings.thinking = Thinking::Medium));
    assert!(rejected(&|l| l.settings.summary_thinking = Thinking::High));
    assert!(rejected(&|l| l.schema_version = 1));
    assert!(rejected(&|l| l.host.backend = "cuda".into()));
    assert!(rejected(&|l| l.host.source_commit = Some("0".repeat(40))));
    assert!(rejected(&|l| l.host.source_commit = None));
    assert!(rejected(&|l| l.host.build_manifest_sha256 = None));
    assert!(rejected(&|l| l.identity.engine = "b1-0000000".into()));
    assert!(rejected(&|l| l.identity.engine = "1.0.0".into()));
    assert!(rejected(&|l| l.identity.parallel = 2));
    assert!(rejected(&|l| {
        l.host.responses_source_sha256 = Some(Sha256Digest::new("1".repeat(64)).unwrap())
    }));
    // A rebuild from a full clone numbers the same commit differently.
    assert!(!rejected(&|l| l.identity.engine = "b7311-b1b770d".into()));

    // Strata and Basalt take effort levels, and their locks keep their bytes.
    let strata: ModelLock = serde_json::from_str(include_str!(
        "../../../models/framework-strata-gyro-m-fast-v2.lock.json"
    ))
    .unwrap();
    assert!(strata.validate().is_ok());
    let saved: Value = serde_json::from_str(include_str!(
        "../../../models/framework-strata-gyro-m-fast-v2.lock.json"
    ))
    .unwrap();
    assert_eq!(serde_json::to_value(&strata).unwrap(), saved);
    let mut on = strata.clone();
    on.settings.thinking = Thinking::On;
    assert!(on.validate().is_err());
    let mut commit = strata;
    commit.host.source_commit = Some(COMMIT.into());
    assert!(commit.validate().is_err());
}

#[test]
fn only_an_awake_text_only_single_slot_server_with_fixed_properties_qualifies() {
    let inspect = |change: &dyn Fn(&mut Value, &mut Value, &mut Value)| {
        let (mut props, mut slots, mut models) = (props(), slots(false), models());
        change(&mut props, &mut slots, &mut models);
        Identity::inspect_llama(&props, &slots, &models)
    };
    let identity = inspect(&|_, _, _| {}).unwrap();
    assert_eq!(identity.context_tokens, 40960);
    assert_eq!(identity.server["model_meta"]["n_params"], 2031739904u64);
    assert!(identity.server.get("media_marker").is_none());
    type Change = Box<dyn Fn(&mut Value, &mut Value, &mut Value)>;
    let changes: Vec<Change> = vec![
        Box::new(|p, _, _| p["is_sleeping"] = json!(true)),
        Box::new(|p, _, _| p["total_slots"] = json!(2)),
        Box::new(|p, _, _| p["modalities"]["vision"] = json!(true)),
        Box::new(|p, _, _| p["endpoint_props"] = json!(true)),
        Box::new(|p, _, _| p["endpoint_slots"] = json!(false)),
        Box::new(|p, _, _| p["model_alias"] = json!("")),
        Box::new(|p, _, _| {
            p.as_object_mut().unwrap().remove("ui");
        }),
        Box::new(|_, s, _| *s = json!([])),
        Box::new(|_, _, m| m["data"][0]["meta"]["n_ctx"] = json!(512)),
        Box::new(|_, _, m| m["data"][0]["id"] = json!("other")),
        Box::new(|_, _, m| m["data"] = json!([])),
    ];
    for (number, change) in changes.iter().enumerate() {
        assert!(inspect(change.as_ref()).is_err(), "change {number}");
    }
}

#[test]
fn length_and_unparsed_calls_never_execute() {
    // A length cut-off drops its calls, whatever the lenient parser kept.
    let mut cut = Chunks::new();
    cut.call(0, "read_file", r#"{"path": "a"#);
    cut.end("length");
    let done = decode(&cut.bytes()).unwrap();
    assert_eq!(done.finish_reason, "length");
    assert_eq!(done.response.message["tool_calls"], json!([]));

    // llama-server reports tool_calls for arguments a stop string cut off.
    let mut broken = Chunks::new();
    broken.call(0, "read_file", r#"{"path": "a"#);
    broken.end("tool_calls");
    let error = decode(&broken.bytes()).unwrap_err();
    assert_eq!(
        (error.kind, error.generated_tokens),
        (FailureKind::ToolParse, Some(40))
    );
    let result = CallResult {
        completion: Err(error),
        remote: RemoteState::TerminalObserved,
        partial: Value::Null,
        received_bytes: 0,
    };
    assert!(matches!(
        result.into_outcome(),
        Outcome::ModelParseError {
            generated_tokens: 40,
            ..
        }
    ));
    let mut array = Chunks::new();
    array.call(0, "read_file", "[1]");
    array.end("tool_calls");
    assert_eq!(
        decode(&array.bytes()).unwrap_err().kind,
        FailureKind::ToolParse
    );
}

#[test]
fn streamed_errors_end_the_stream_and_unparsed_output_is_a_parse_failure() {
    let mut unparsed = Chunks::new();
    unparsed.text("content", "ok");
    unparsed.error("The model produced output that does not match the expected peg-native format");
    let mut decoder = decoder();
    decoder.push(&unparsed.bytes()).unwrap();
    assert!(decoder.is_finished() && decoder.terminal_observed());
    assert_eq!(decoder.partial()["content"], "ok");
    let error = decoder.finish().unwrap_err();
    assert_eq!(
        (error.kind, error.generated_tokens),
        (FailureKind::ToolParse, None)
    );
    let result = CallResult {
        completion: Err(error),
        remote: RemoteState::TerminalObserved,
        partial: Value::Null,
        received_bytes: 0,
    };
    assert!(matches!(
        result.into_outcome(),
        Outcome::ModelError(ModelFailure::Parse(_))
    ));

    let mut other = Chunks::new();
    other.error("context shift is disabled");
    assert_eq!(
        decode(&other.bytes()).unwrap_err().kind,
        FailureKind::Remote
    );
    // Nothing may follow a streamed error.
    let mut after = Chunks::new();
    after.error("context shift is disabled");
    after.end("stop");
    assert!(protocol_error(&after.bytes()));
}

#[test]
fn chunks_outside_llama_servers_order_or_shape_are_rejected() {
    let valid = || {
        let mut chunks = Chunks::new();
        chunks.text("content", "hi");
        chunks
    };
    let mut ended = valid();
    ended.end("stop");
    assert_eq!(
        decode(&ended.bytes()).unwrap().response.message["content"],
        "hi"
    );
    // A final first result has no role chunk.
    let mut roleless = Chunks(vec![]);
    roleless.text("content", "hi");
    roleless.end("stop");
    assert!(decode(&roleless.bytes()).is_ok());

    type Edit = Box<dyn Fn(&mut Chunks)>;
    let edits: Vec<Edit> = vec![
        Box::new(|c| c.delta(json!({"role":"assistant","content":null}))),
        Box::new(|c| c.delta(json!({"content":"a","reasoning_content":"b"}))),
        Box::new(|c| c.delta(json!({"content":""}))),
        Box::new(|c| c.delta(json!({"refusal":"no"}))),
        Box::new(|c| {
            c.delta(json!({"tool_calls":[{"index":1,"id":"x","type":"function","function":{"name":"read_file"}}]}))
        }),
        Box::new(|c| {
            c.call(0, "read_file", "{}");
            c.delta(json!({"tool_calls":[{"index":0,"id":"call0","function":{"arguments":"x"}}]}));
        }),
        Box::new(|c| {
            c.call(0, "read_file", "{}");
            c.call(1, "read_file", "{}");
            c.0.retain(|d| !d.contains(r#""id":"call1""#));
        }),
        Box::new(|c| {
            c.call(0, "read_file", "{}");
            c.0.push(c.0.last().unwrap().replace(r#""index":0"#, r#""index":1"#));
        }),
        Box::new(|c| {
            c.0.push(c.0[1].replace("chatcmpl-fixture", "chatcmpl-other"))
        }),
        Box::new(|c| c.0.push(c.0[1].replace(MODEL, "other-model"))),
        Box::new(|c| c.0.push(c.0[1].replace(ENGINE, "b2-b1b770d"))),
        Box::new(|c| c.0.push(c.0[1].replace(r#""index":0"#, r#""index":1"#))),
        Box::new(|c| {
            let mut verbose: Value = serde_json::from_str(&c.0[1]).unwrap();
            verbose["__verbose"] = json!({});
            c.0.push(verbose.to_string());
        }),
        Box::new(|c| c.0.push("[DONE]".into())),
    ];
    for (number, edit) in edits.iter().enumerate() {
        let mut chunks = valid();
        edit(&mut chunks);
        chunks.end("stop");
        assert!(protocol_error(&chunks.bytes()), "edit {number}");
    }

    let finished = |reason: &str, calls: bool| {
        let mut chunks = valid();
        if calls {
            chunks.call(0, "read_file", "{}");
        }
        chunks.end(reason);
        chunks
    };
    assert!(protocol_error(&finished("stop", true).bytes()));
    assert!(protocol_error(&finished("tool_calls", false).bytes()));
    assert!(protocol_error(&finished("content_filter", false).bytes()));
    // Usage must follow the finish chunk, and [DONE] the usage chunk.
    let mut unmeasured = finished("stop", false);
    unmeasured.0.remove(unmeasured.0.len() - 2);
    assert!(protocol_error(&unmeasured.bytes()));
    let mut early = valid();
    early.0.push("[DONE]".into());
    assert!(protocol_error(&early.bytes()));
    let mut trailing = finished("stop", false);
    trailing.text("content", "late");
    assert!(protocol_error(&trailing.bytes()));
    let mut mismatched = finished("stop", false);
    let usage = mismatched.0.len() - 2;
    mismatched.0[usage] = mismatched.0[usage].replace("240", "241");
    assert!(protocol_error(&mismatched.bytes()));
    let mut named = valid().bytes();
    named.splice(0..0, b"event: chunk\n".iter().copied());
    assert!(protocol_error(&named));

    let mut open = valid();
    open.0.pop();
    let mut decoder = decoder();
    decoder.push(&open.bytes()).unwrap();
    decoder.push(b"data: {").unwrap();
    assert_eq!(decoder.finish().unwrap_err().kind, FailureKind::Protocol);
    assert_eq!(
        decode(&valid().bytes()).unwrap_err().kind,
        FailureKind::Incomplete
    );
}

#[derive(Clone)]
struct Fixture {
    props: Value,
    slots: Value,
    status: u16,
    stream: Vec<u8>,
    posts: Vec<Value>,
}
struct Server {
    endpoint: Endpoint,
    state: Arc<Mutex<Fixture>>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(stream: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Endpoint::parse(&format!("http://{}/v1", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(Fixture {
            props: props(),
            slots: slots(false),
            status: 200,
            stream,
            posts: vec![],
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let shared = shared.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut chunk = [0; 4096];
                    let (header, length) = loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let header = String::from_utf8(bytes[..at + 4].to_vec()).unwrap();
                            let length = header
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            bytes.drain(..at + 4);
                            break (header, length);
                        }
                    };
                    while bytes.len() < length {
                        let n = socket.read(&mut chunk).await.unwrap();
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    let path = header.split_whitespace().nth(1).unwrap().to_owned();
                    let (status, content_type, body) = {
                        let mut state = shared.lock().unwrap();
                        match path.as_str() {
                            "/v1/chat/completions" => {
                                state.posts.push(serde_json::from_slice(&bytes).unwrap());
                                let streamed = state.status == 200;
                                (
                                    state.status,
                                    if streamed {
                                        "text/event-stream"
                                    } else {
                                        "application/json"
                                    },
                                    state.stream.clone(),
                                )
                            }
                            path => {
                                let value = match path {
                                    "/props" => state.props.clone(),
                                    "/slots" => state.slots.clone(),
                                    "/v1/models" => models(),
                                    _ => panic!("unexpected path {path}"),
                                };
                                (200, "application/json", serde_json::to_vec(&value).unwrap())
                            }
                        }
                    };
                    let head = format!(
                        "HTTP/1.1 {status} fixture\r\nContent-Type: {content_type}\r\n\
                        Connection: close\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
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
    fn posts(&self) -> Vec<Value> {
        self.state.lock().unwrap().posts.clone()
    }
}
async fn complete(client: &mut Client) -> CallResult {
    let (sender, mut receiver) = events();
    let drain = tokio::spawn(async move {
        let mut seen = vec![];
        while let Some(event) = receiver.recv().await {
            seen.push(event.data);
        }
        seen
    });
    let result = client
        .complete(
            &request(),
            Instant::now() + Duration::from_secs(10),
            &CancellationToken::new(),
            sender,
        )
        .await;
    drain.await.unwrap();
    result
}

#[tokio::test]
async fn discovery_binds_llama_servers_process_marker_and_dispatches_chat_completions() {
    let mut chunks = Chunks::new();
    chunks.call(0, "read_file", r#"{"path":"a"}"#);
    chunks.end("tool_calls");
    let server = Server::new(chunks.bytes()).await;
    let seen = inspect_endpoint(
        &server.endpoint,
        &Limits::default(),
        Instant::now() + Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(seen.identity, lock().identity);
    assert_eq!(seen.instance, Instance::Marker(MARKER.into()));
    assert_eq!((seen.started(), seen.in_flight), (None, 0));
    assert_eq!(seen.slots, slots(false));

    let mut client = server.client();
    let result = complete(&mut client).await;
    assert_eq!(result.remote, RemoteState::TerminalObserved);
    let done = result.completion.unwrap();
    assert_eq!(done.response.message["tool_calls"][0]["id"], "call0");
    let posts = server.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["messages"][1]["content"], "Inspect the file.");

    // A restart on the same port draws a new marker.
    server.state.lock().unwrap().props["media_marker"] =
        json!(format!("<__media_{}__>", "Z".repeat(32)));
    let error = complete(&mut client).await.completion.unwrap_err();
    assert_eq!(error.kind, FailureKind::Identity);
    assert_eq!(server.posts().len(), 1);
}

#[tokio::test]
async fn busy_pinned_or_changed_servers_get_no_request() {
    let server = Server::new(vec![]).await;
    server.state.lock().unwrap().slots = slots(true);
    let result = complete(&mut server.client()).await;
    assert_eq!(result.completion.unwrap_err().kind, FailureKind::Busy);
    assert_eq!(result.remote, RemoteState::NotDispatched);

    server.state.lock().unwrap().slots = slots(false);
    for marker in [json!("<__media_pinned__>"), Value::Null] {
        server.state.lock().unwrap().props["media_marker"] = marker;
        let error = complete(&mut server.client()).await.completion.unwrap_err();
        assert_eq!(error.kind, FailureKind::Identity);
    }
    server.state.lock().unwrap().props = props();
    server.state.lock().unwrap().props["chat_template"] = json!("changed");
    let error = complete(&mut server.client()).await.completion.unwrap_err();
    assert_eq!(error.kind, FailureKind::Identity);
    assert!(server.posts().is_empty());
}

#[tokio::test]
async fn a_rejected_request_reports_llama_servers_reason_and_stays_uncertain() {
    let server = Server::new(
        br#"{"error":{"code":400,"message":"request (50413 tokens) exceeds the available context size (40960 tokens), try increasing it","type":"exceed_context_size_error"}}"#
            .to_vec(),
    )
    .await;
    server.state.lock().unwrap().status = 400;
    let result = complete(&mut server.client()).await;
    assert_eq!(result.remote, RemoteState::Uncertain);
    let error = result.completion.unwrap_err();
    assert_eq!(error.kind, FailureKind::Http);
    assert!(
        error
            .detail
            .contains("Chat Completions HTTP 400 Bad Request (request (50413 tokens) exceeds")
    );
}
