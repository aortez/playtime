//! O1 is the only metadata/NDJSON implementation. The host accepts only the
//! canonical fixed chat operation and sends its original admitted bytes once.
use crate::{Result, wire::*};
use playtime_backend_guard::ports::*;
use playtime_core::RequestId;
use playtime_engine::ModelRequest;
use playtime_ollama::{self as o1, Transport};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    model: String,
    stream: bool,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
    keep_alive: String,
    think: Option<o1::Thinking>,
    options: Options,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    num_predict: u64,
    seed: u32,
    num_ctx: Option<u64>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
    repeat_penalty: Option<f64>,
    use_mmap: Option<bool>,
}
pub fn validate_body(
    body: &[u8],
    request: &RequestSpec,
    settings: &o1::EffectiveSettings,
) -> Result<ModelRequest> {
    if body.len() as u64 != request.body_bytes
        || playtime_backend_guard::digest(body) != request.body_sha256
    {
        return Err(Failure::Conflict);
    }
    let parsed = o1::json::parse(body, BODY_BYTES, true).map_err(|_| Failure::Invalid)?;
    let chat: Chat = serde_json::from_value(parsed).map_err(|_| Failure::Invalid)?;
    let actual = o1::EffectiveSettings {
        context_tokens: chat.options.num_ctx,
        temperature: chat.options.temperature,
        top_p: chat.options.top_p,
        top_k: chat.options.top_k,
        repeat_penalty: chat.options.repeat_penalty,
        thinking: chat.think,
        use_mmap: chat.options.use_mmap,
        keep_alive: chat.keep_alive,
    };
    if chat.model != o1::Profile::MODEL
        || !chat.stream
        || &actual != settings
        || chat.options.num_predict != request.original_max_tokens
    {
        return Err(Failure::Invalid);
    }
    let model = ModelRequest {
        request_id: RequestId::new(request.request_id.as_str()).map_err(|_| Failure::Invalid)?,
        call: request.ordinal,
        handoff: false,
        sampling_seed: chat.options.seed,
        maximum_tokens: request.original_max_tokens,
        messages: chat.messages,
        tools: chat.tools,
        context_input_bound: 0,
        context_basis: String::new(),
        context_reserve: 0,
    };
    if o1::encode_request(&model, settings, BODY_BYTES).map_err(|_| Failure::Invalid)? != body {
        return Err(Failure::Invalid);
    }
    Ok(model)
}

#[async_trait::async_trait]
pub trait ManagerAuthority: Send + Sync + 'static {
    async fn verify(&self, expected: &BackendIdentity) -> Result<VerifiedBackend>;
}
pub struct OllamaBackend<T: Transport + 'static> {
    adapter: tokio::sync::Mutex<o1::Adapter<T>>,
    identity: BackendIdentity,
    manager: Arc<dyn ManagerAuthority>,
    settings: o1::EffectiveSettings,
    clock: Arc<dyn Clock>,
}
impl<T: Transport + Clone + 'static> OllamaBackend<T> {
    /// Full metadata inspection is explicit preparation, outside the one-second
    /// fast verify port. The prepared profile never silently changes.
    pub async fn prepare(
        transport: T,
        lock: o1::ModelLock,
        identity: BackendIdentity,
        settings: o1::EffectiveSettings,
        manager: Arc<dyn ManagerAuthority>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let mut adapter = o1::Adapter::new(transport.clone(), lock, o1::Limits::default())
            .map_err(|_| Failure::Invalid)?;
        manager.verify(&identity).await?;
        adapter
            .inspect(
                false,
                tokio::time::Instant::now() + Duration::from_secs(45),
                &CancellationToken::new(),
            )
            .await
            .map_err(|_| Failure::Blocked)?;
        manager.verify(&identity).await?;
        Ok(Self {
            adapter: tokio::sync::Mutex::new(adapter),
            identity,
            manager,
            settings,
            clock,
        })
    }
}
#[async_trait::async_trait]
impl<T: Transport + 'static> Backend for OllamaBackend<T> {
    async fn verify_read_only(&self, expected: &BackendIdentity) -> Result<VerifiedBackend> {
        if expected != &self.identity {
            return Err(Failure::Conflict);
        }
        self.manager.verify(expected).await
    }
    async fn execute(
        &self,
        backend: BackendIdentity,
        request: RequestSpec,
        body: Vec<u8>,
        mut observations: ObservationSink,
        cancel: CancellationToken,
    ) -> Result<VerifiedTerminal> {
        let model = validate_body(&body, &request, &self.settings)?;
        self.verify_read_only(&backend).await?;
        let now = self.clock.now();
        if now.boot_id != backend.boot_id || now.ms >= request.deadline_ms || cancel.is_cancelled()
        {
            return Err(Failure::Expired);
        }
        let limits = o1::Limits {
            queue_bytes: QUEUE_BYTES,
            queue_records: 64,
            ..Default::default()
        };
        let (tx, mut rx) = o1::observations(&limits).map_err(|_| Failure::Invalid)?;
        let work = async {
            let mut adapter = self.adapter.lock().await;
            let chat = adapter.chat(
                &model,
                &self.settings,
                tokio::time::Instant::now() + Duration::from_millis(request.deadline_ms - now.ms),
                &cancel,
                &tx,
            );
            tokio::pin!(chat);
            let result = loop {
                tokio::select! {biased;
                    raw=rx.recv()=>{
                        if let Some(raw)=raw {observations.send(raw.bytes).await?;}else{return Err(Failure::Blocked);}
                    },
                    result=&mut chat=>break result,
                }
            };
            while let Some(raw) = rx.try_recv() {
                observations.send(raw.bytes).await?;
            }
            if result.remote != o1::RemoteOutcome::TerminalObserved || result.outcome.is_err() {
                return Err(Failure::Blocked);
            }
            Ok(VerifiedTerminal::from_trusted_port(
                backend.clone(),
                request.clone(),
            ))
        };
        tokio::select! { biased;
            _ = cancel.cancelled() => Err(Failure::Blocked),
            _ = tokio::time::sleep(Duration::from_millis(request.deadline_ms - now.ms)) => Err(Failure::Expired),
            result = work => result,
        }
    }
}
