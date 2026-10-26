//! Metadata-only by default. `--live` sends exactly one bounded synthetic chat.
//! This is adapter compatibility evidence, not a controller/run-and-view gate.
use playtime_core::RequestId;
use playtime_engine::ModelRequest;
use playtime_ollama::*;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut endpoint = None;
    let mut live = false;
    let mut addresses = vec![];
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--endpoint" => endpoint = Some(args.next().ok_or("missing endpoint")?),
            "--live" => live = true,
            "--resolve-ip" => addresses.push(args.next().ok_or("missing IP address")?.parse()?),
            _ => {
                return Err(
                    "usage: compatibility --endpoint HTTP(S)_URL [--resolve-ip IP] [--live]".into(),
                );
            }
        }
    }
    let endpoint =
        Endpoint::parse(&endpoint.ok_or("--endpoint is required; no ambient endpoint selection")?)?;
    let limits = Limits::default();
    let resolution = if addresses.is_empty() {
        Resolution::System
    } else {
        Resolution::Pinned { addresses }
    };
    let transport = NativeTransport::with_resolution(
        endpoint.clone(),
        Trust::BundledWebPki,
        resolution.clone(),
        &limits,
    )?;
    let lock = ModelLock::parse(include_bytes!("../../../models/gpt-oss-20b.lock.json"))?;
    let mut adapter = Adapter::new(transport, lock, limits.clone())?;
    let cancel = CancellationToken::new();
    let inspection = adapter
        .inspect(true, Instant::now() + Duration::from_secs(45), &cancel)
        .await?;
    println!(
        "{}",
        json!({"kind":"inspection","endpoint":endpoint.as_str(),"resolution":resolution,"version":inspection.version["version"],"model":Profile::MODEL,"digest":Profile::DIGEST,"definition_sha256":inspection.definition_sha256,"definition_bytes":inspection.canonical_definition.len(),"loaded":inspection.loaded})
    );
    if !live {
        return Ok(());
    }
    let request = ModelRequest {
        request_id: RequestId::new("ollama-compatibility-55")?,
        call: 1,
        purpose: playtime_core::RequestPurpose::Work,
        sampling_seed: 55,
        maximum_tokens: 256,
        messages: vec![
            json!({"role":"user","content":"Call emit_result once with text set to ready. This is a synthetic compatibility check. Do not do any other work."}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"emit_result","description":"Return a compatibility marker. No tool will be executed.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}}),
        ],
        context_input_bound: 1024,
        context_basis: "standalone synthetic compatibility probe; no controller dispatch".into(),
        context_reserve: 256,
    };
    let settings = EffectiveSettings {
        temperature: Some(0.0),
        ..Default::default()
    };
    println!(
        "{}",
        json!({"kind":"effective_request","maximum_seconds":60,"parse_error_retries":0,"settings":settings,"request":request,"body_sha256":json::hash(&encode_request(&request,&settings,limits.request_bytes)?)})
    );
    let (tx, mut rx) = observations(&limits)?;
    let chat = adapter.chat(
        &request,
        &settings,
        Instant::now() + Duration::from_secs(60),
        &cancel,
        &tx,
    );
    tokio::pin!(chat);
    let result = loop {
        tokio::select! {result=&mut chat=>break result,observation=rx.recv()=>if let Some(observation)=observation{print_observation(observation);}}
    };
    while let Some(observation) = rx.try_recv() {
        print_observation(observation);
    }
    println!(
        "{}",
        json!({"kind":"result","dispatch":result.dispatch,"remote":result.remote,"partial":result.partial,"consumed_bytes":result.consumed_bytes,"records":result.records,"pending_fragment":String::from_utf8_lossy(&result.pending_fragment),"outcome":match &result.outcome{Ok(response)=>json!({"response":response}),Err(error)=>json!({"error":error})}})
    );
    if let Err(error) = result.outcome {
        return Err(error.into());
    }
    Ok(())
}
fn print_observation(observation: Observation) {
    println!(
        "{}",
        json!({"kind":"observation","framing":format!("{:?}",observation.kind),"raw_utf8":String::from_utf8_lossy(&observation.bytes),"raw_sha256":json::hash(&observation.bytes)})
    );
}
