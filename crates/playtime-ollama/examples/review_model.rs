//! Inspect a candidate and optionally probe its native tools. This grants no
//! production execution approval and never executes model-controlled commands.
use playtime_core::{RequestId, RequestPurpose};
use playtime_engine::{ModelRequest, ModelResponse};
use playtime_ollama::{discovery::Discovery, *};
use serde_json::{Value, json};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path, time::Duration};
use tokio::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn retain(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    Ok(())
}

fn request(
    call: u64,
    messages: Vec<Value>,
    tools: Vec<Value>,
    maximum_tokens: u64,
) -> Result<ModelRequest> {
    Ok(ModelRequest {
        request_id: RequestId::new(format!("candidate-{call}"))?,
        call,
        purpose: RequestPurpose::Work,
        sampling_seed: 1042,
        maximum_tokens,
        messages,
        tools,
        context_input_bound: 4096,
        context_basis: "standalone candidate probe; no controller dispatch".into(),
        context_reserve: 1024,
    })
}

async fn chat(
    adapter: &mut Adapter<NativeTransport>,
    request: &ModelRequest,
    settings: &EffectiveSettings,
    evidence: &Path,
    cancel_after_chunk: bool,
) -> Result<Option<ModelResponse>> {
    let limits = Limits::default();
    let (tx, mut rx) = observations(&limits)?;
    let cancel = CancellationToken::new();
    let mut records = Vec::new();
    retain(
        &evidence.join(format!("request-{}.json", request.call)),
        &json!({"request":request,"settings":settings,"cancel_after_chunk":cancel_after_chunk}),
    )?;
    let chat = adapter.chat(
        request,
        settings,
        Instant::now() + Duration::from_secs(180),
        &cancel,
        &tx,
    );
    tokio::pin!(chat);
    let result = loop {
        tokio::select! {
            result = &mut chat => break result,
            observation = rx.recv() => if let Some(observation) = observation {
                if cancel_after_chunk && observation.kind == ObservationKind::NativeChunk {
                    cancel.cancel();
                }
                records.push(String::from_utf8_lossy(&observation.bytes).into_owned());
            }
        }
    };
    while let Some(observation) = rx.try_recv() {
        records.push(String::from_utf8_lossy(&observation.bytes).into_owned());
    }
    retain(
        &evidence.join(format!("response-{}.json", request.call)),
        &json!({"dispatch":result.dispatch,"remote":result.remote,"records":records,"outcome":result.outcome,"partial":result.partial}),
    )?;
    if cancel_after_chunk {
        if result.outcome.as_ref().err().map(|e| e.kind) != Some(FailureKind::Cancelled) {
            return Err("probe did not observe cancellation after a streamed record".into());
        }
        // Adapter cancellation does not prove that the backend stopped. Inspect
        // the model host separately before admitting another inference request.
        return Ok(None);
    }
    let response = result.outcome?;
    if response.generated_tokens.is_none() {
        return Err("probe completed without known usage".into());
    }
    Ok(Some(response))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(args.len() == 5 || ((args.len() == 6 || args.len() == 7) && args[5] == "--live")) {
        return Err("usage: review_model ENDPOINT MODEL EXPECTED_DIGEST REASONING NEW_EVIDENCE_DIR [--live [MAX_TOKENS]] (reasoning: default, off, on, low, medium, high; tokens: 1..8192, default 512)".into());
    }
    let maximum_tokens = args
        .get(6)
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(512);
    if !(1..=8192).contains(&maximum_tokens) {
        return Err("probe token allowance must be 1..8192".into());
    }
    let thinking = match args[3].as_str() {
        "default" => None,
        "off" => Some(Thinking::Boolean(false)),
        "on" => Some(Thinking::Boolean(true)),
        level @ ("low" | "medium" | "high") => Some(Thinking::Level(level.into())),
        _ => return Err("unknown reasoning choice".into()),
    };
    let evidence = Path::new(&args[4]);
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(evidence)?;
    let limits = Limits::default();
    let endpoint = Endpoint::parse(&args[0])?;
    let transport = || {
        NativeTransport::with_resolution(
            endpoint.clone(),
            Trust::BundledWebPki,
            Resolution::System,
            &limits,
        )
    };
    let detail = Discovery::new(transport()?, limits.clone())?
        .inspect(
            &args[1],
            Some(&args[2]),
            Instant::now() + Duration::from_secs(45),
            &CancellationToken::new(),
        )
        .await?;
    if detail.rejected {
        return Err("remote or rejected candidate".into());
    }
    let model = ExecutionModel {
        name: args[1].clone(),
        digest: args[2].clone(),
        backend_version: detail.inventory.version,
        definition_sha256: detail
            .definition_sha256
            .ok_or("missing candidate identity")?,
        definition: detail.definition.ok_or("missing candidate definition")?,
        reasoning: vec![thinking.clone()],
    };
    model.validate()?;
    retain(
        &evidence.join("model-lock.json"),
        &json!({"name":model.name,"digest":model.digest,"ollama_version":model.backend_version,"definition_sha256":model.definition_sha256,"definition":model.definition,"modelfile":""}),
    )?;
    retain(&evidence.join("show.json"), &detail.show)?;
    if args.len() == 5 {
        return Ok(());
    }
    let mut adapter = Adapter::with_model(transport()?, model, limits.clone())?;
    let settings = EffectiveSettings {
        context_tokens: Some(65536),
        thinking,
        temperature: Some(0.0),
        ..Default::default()
    };
    let tools = vec![
        json!({"type":"function","function":{"name":"record_number","description":"Record the requested integer. This synthetic probe executes no command.","parameters":{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}}}),
    ];
    let messages = vec![
        json!({"role":"user","content":"Call record_number exactly once with number 7. Do not write prose."}),
    ];
    let response = chat(
        &mut adapter,
        &request(1, messages.clone(), tools.clone(), maximum_tokens)?,
        &settings,
        evidence,
        false,
    )
    .await?
    .ok_or("missing response")?;
    let calls = response.message["tool_calls"]
        .as_array()
        .ok_or("missing tool call")?;
    if calls.len() != 1
        || calls[0]["function"]["name"] != "record_number"
        || calls[0]["function"]["arguments"]["number"] != 7
    {
        return Err("wrong synthetic tool call".into());
    }
    let mut messages = messages;
    messages.push(response.message.clone());
    messages.push(json!({"role":"tool","tool_name":"record_number","content":"{\"recorded\":7,\"receipt\":\"cobalt-731\"}"}));
    messages.push(json!({"role":"user","content":"Return only the receipt string from the tool result. Do not call tools."}));
    let response = chat(
        &mut adapter,
        &request(2, messages, tools, maximum_tokens)?,
        &settings,
        evidence,
        false,
    )
    .await?
    .ok_or("missing round-trip response")?;
    if response.message["tool_calls"]
        .as_array()
        .is_some_and(|calls| !calls.is_empty())
        || !response.message["content"]
            .as_str()
            .is_some_and(|s| s.contains("cobalt-731"))
    {
        return Err("tool result did not survive round trip".into());
    }
    let request = request(
        3,
        vec![
            json!({"role":"user","content":"Write a detailed tutorial about sorting algorithms, with many examples."}),
        ],
        vec![],
        maximum_tokens,
    )?;
    chat(&mut adapter, &request, &settings, evidence, true).await?;
    println!(
        "Identity, 64K streaming/tool round trip, known completed usage and client cancellation checked; backend stop requires separate observation."
    );
    Ok(())
}
