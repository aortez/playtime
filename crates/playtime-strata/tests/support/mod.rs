#![allow(dead_code)]
use playtime_core::{RequestId, RequestPurpose, Sha256Digest};
use playtime_engine::ModelRequest;
use playtime_strata::{HostIdentity, Identity, ModelLock, Settings};
use serde_json::{Value, json};

pub const MODEL: &str = "fixture-gyro-m";
pub fn metadata() -> (Value, Value, Value) {
    (
        json!({"default_generation_settings":{"n_ctx":163840,"params":{"temperature":1.0}},
        "total_slots":1,"model_alias":MODEL,"chat_template":"synthetic template",
        "chat_template_caps":{"supports_tools":true},"is_sleeping":false,
        "model_path":"/fixture/gyro.gguf","build_info":"Strata 0.1.58"}),
        json!({"service":"strata","model":MODEL,"engine":"0.1.58","loaded":true,
        "context":{"native":163840,"max_positions":163840},"concurrency":{"serving":1},
        "dialects":["/v1/responses"],"started":100,"activity":{"in_flight":0}}),
        json!({"shared":false,"defaults":{}}),
    )
}
pub fn lock() -> ModelLock {
    let (props, status, settings) = metadata();
    let hash = || Sha256Digest::new("1".repeat(64)).unwrap();
    ModelLock {
        schema_version: 1,
        identity: Identity::inspect(&props, &status, &settings).unwrap(),
        host: HostIdentity {
            backend: "hip".into(),
            quantization: "fixture".into(),
            engine_sha256: hash(),
            artifacts_manifest_sha256: hash(),
            launch_configuration_sha256: hash(),
            responses_source_sha256: Sha256Digest::new(
                "aba98b4aba933fe55cf699985add2e1014a66dacb55ed768ca9aa404c3fcb29b",
            )
            .unwrap(),
        },
        settings: Settings::default(),
    }
}
pub fn request() -> ModelRequest {
    ModelRequest {
        request_id: RequestId::new("request-1").unwrap(),
        call: 1,
        purpose: RequestPurpose::Work,
        sampling_seed: 42,
        maximum_tokens: 512,
        context_input_bound: 1000,
        context_reserve: 256,
        context_basis: "fixture".into(),
        messages: vec![json!({"role":"user","content":"Inspect the file."})],
        tools: vec![
            json!({"type":"function","function":{"name":"read_file","description":"Read file",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}),
        ],
    }
}

/// Synthetic events transcribed from the installed Responses assembler's shape;
/// deliberately no model output, production IDs or third-party source code.
pub struct Stream {
    pub events: Vec<Value>,
    pub output: Vec<Value>,
}
impl Stream {
    pub fn new() -> Self {
        let mut s = Self {
            events: vec![],
            output: vec![],
        };
        s.event(json!({"type":"response.created","response":s.response("in_progress")}));
        s.event(json!({"type":"response.in_progress","response":s.response("in_progress")}));
        s
    }
    pub fn response(&self, status: &str) -> Value {
        json!({"id":"resp_fixture","object":"response","model":MODEL,"status":status,"output":self.output})
    }
    pub fn event(&mut self, mut event: Value) {
        event["sequence_number"] = json!(self.events.len());
        self.events.push(event);
    }
    pub fn item(&mut self, kind: &str, content: &str, status: &str) {
        let index = self.output.len();
        let id = format!("item_{index}");
        let (delta, done) = match kind {
            "message" => ("response.output_text.delta", "response.output_text.done"),
            "reasoning" => (
                "response.reasoning_text.delta",
                "response.reasoning_text.done",
            ),
            _ => (
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
            ),
        };
        let mut item = json!({"id":id,"type":kind,"status":"in_progress"});
        if kind == "function_call" {
            item["call_id"] = json!(format!("call_{index}"));
            item["name"] = json!("read_file");
            item["arguments"] = json!("");
        } else {
            item["content"] = json!([]);
            if kind == "message" {
                item["role"] = json!("assistant");
            } else {
                item["summary"] = json!([]);
            }
        }
        self.event(json!({"type":"response.output_item.added","output_index":index,"item":item}));
        if kind == "message" {
            self.event(json!({"type":"response.content_part.added","item_id":id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":""}}));
        }
        for c in content.chars() {
            self.event(json!({"type":delta,"item_id":id,"output_index":index,"content_index":0,"delta":c.to_string()}));
        }
        let mut finished = json!({"type":done,"item_id":id,"output_index":index,"content_index":0});
        if kind == "function_call" {
            finished["arguments"] = json!(content);
            item["arguments"] = json!(content);
        } else {
            finished["text"] = json!(content);
            let part = json!({"type":if kind=="message" {"output_text"} else {"reasoning_text"},"text":content});
            item["content"] = json!([part]);
        }
        self.event(finished);
        if kind == "message" {
            self.event(json!({"type":"response.content_part.done","item_id":id,"output_index":index,"content_index":0,"part":item["content"][0]}));
        }
        item["status"] = json!(status);
        self.event(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        self.output.push(item);
    }
    pub fn terminal(&mut self, state: &str) {
        let mut response = self.response(state);
        response["usage"] = json!({"input_tokens":200,"output_tokens":40,"total_tokens":240,"input_tokens_details":{"cached_tokens":12},"output_tokens_details":{"reasoning_tokens":20}});
        if state == "incomplete" {
            response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        if state == "failed" {
            response["error"] = json!({"code":"server_error","message":"fixture failure"});
            response["usage"] = Value::Null;
        }
        self.event(json!({"type":format!("response.{state}"),"response":response,"timings":{"predicted_n":40}}));
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.events
            .iter()
            .map(|e| {
                format!(
                    "event: {}\r\ndata: {}\r\n\r\n",
                    e["type"].as_str().unwrap(),
                    e
                )
            })
            .collect::<String>()
            .into_bytes()
    }
}
pub fn answer() -> Stream {
    let mut s = Stream::new();
    s.item("message", "héllo", "completed");
    s.terminal("completed");
    s
}
