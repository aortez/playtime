use crate::{
    AssistantMessage, CancellationToken, Decoder, EffectiveSettings, ExecutionModel, Failure,
    FailureKind, HttpResponse, Inspection, Limits, ModelLock, Observation, ObservationKind,
    ObservationSender, Operation, Profile, Request, Result, Transport, json,
};
use futures_util::StreamExt;
use playtime_engine::{ModelRequest, ModelResponse};
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dispatch {
    NotDispatched,
    PossiblySent,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOutcome {
    NotStarted,
    TerminalObserved,
    Uncertain,
}
/// One adapter result, not a canonical settlement or backend termination receipt.
#[derive(Clone, Debug)]
pub struct ChatResult {
    pub outcome: Result<ModelResponse>,
    pub finish_reason: Option<String>,
    pub dispatch: Dispatch,
    pub remote: RemoteOutcome,
    pub partial: AssistantMessage,
    pub pending_fragment: Vec<u8>,
    pub consumed_bytes: usize,
    pub records: u64,
}
pub struct Adapter<T: Transport> {
    transport: T,
    lock: ExecutionModel,
    limits: Limits,
    verified: bool,
    recheck_dispatch: bool,
}
impl<T: Transport> Adapter<T> {
    pub fn new(transport: T, lock: ModelLock, limits: Limits) -> Result<Self> {
        let lock = ExecutionModel::locked(&lock)?;
        Self::build(transport, lock, limits, false)
    }
    /// Explicit selected identities are checked again before every inference.
    pub fn with_model(transport: T, model: ExecutionModel, limits: Limits) -> Result<Self> {
        Self::build(transport, model, limits, true)
    }
    fn build(
        transport: T,
        lock: ExecutionModel,
        limits: Limits,
        recheck_dispatch: bool,
    ) -> Result<Self> {
        lock.validate()?;
        limits.validate()?;
        Ok(Self {
            transport,
            lock,
            limits,
            verified: false,
            recheck_dispatch,
        })
    }
    /// Metadata only; success belongs to this immutable transport/lock pair.
    /// This is checked metadata, not an atomic lease on a mutable model name.
    pub async fn inspect(
        &mut self,
        loaded: bool,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Inspection> {
        self.verified = false;
        let aggregate = deadline.min(Instant::now() + Duration::from_secs(45));
        let version = self.metadata(Operation::Version, aggregate, cancel).await?;
        if version.get("version").and_then(serde_json::Value::as_str) != Some(Profile::VERSION) {
            return Err(Failure::new(
                FailureKind::Identity,
                "unsupported or changed Ollama version",
            ));
        }
        let tags = self.metadata(Operation::Tags, aggregate, cancel).await?;
        let show = self.metadata(Operation::Show, aggregate, cancel).await?;
        let loaded = if loaded {
            Some(self.metadata(Operation::Loaded, aggregate, cancel).await?)
        } else {
            None
        };
        let inspection = crate::identity::verify(
            &self.lock,
            version,
            tags,
            show,
            loaded,
            self.limits.metadata_bytes,
        )?;
        check(aggregate, cancel)?;
        self.verified = true;
        Ok(inspection)
    }
    async fn metadata(
        &self,
        operation: Operation,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(15));
        let body = (operation == Operation::Show).then(|| {
            serde_json::to_vec(&serde_json::json!({"model": self.lock.name}))
                .expect("model name JSON")
        });
        let mut response = bounded(
            self.transport.send(Request { operation, body }),
            deadline,
            cancel,
        )
        .await?;
        encoding(&response)?;
        let mut bytes = Vec::new();
        let limit = if response.status == 200 {
            self.limits.metadata_bytes
        } else {
            self.limits.metadata_bytes.min(self.limits.error_bytes)
        };
        read_body(&mut response, &mut bytes, limit, deadline, cancel).await?;
        if response.status != 200 {
            return Err(Failure::new(
                // On the supported Ollama profile, show returns 404 for a
                // missing model, including removal after the tags response.
                if operation == Operation::Show && response.status == 404 {
                    FailureKind::Identity
                } else {
                    FailureKind::Http
                },
                format!(
                    "metadata {} returned HTTP {}",
                    operation.path(),
                    response.status
                ),
            ));
        }
        let value = json::parse(&bytes, self.limits.metadata_bytes, true)?;
        check(deadline, cancel)?;
        Ok(value)
    }
    /// No adapter background receive task is spawned. The owner polls this
    /// future and drains the bounded observation channel independently; queue
    /// persistence never blocks cancel/deadline servicing. Dropping this future
    /// after dispatch leaves an uncertain external effect for the owner/O2.
    pub async fn chat(
        &mut self,
        request: &ModelRequest,
        settings: &EffectiveSettings,
        deadline: Instant,
        cancel: &CancellationToken,
        observations: &ObservationSender,
    ) -> ChatResult {
        let before = async {
            check(deadline, cancel)?;
            let body = crate::request::encode_for_model(
                request,
                settings,
                &self.lock,
                self.limits.request_bytes,
            )?;
            if self.recheck_dispatch || !self.verified {
                self.inspect(false, deadline, cancel).await?;
            }
            check(deadline, cancel)?;
            Ok(body)
        }
        .await;
        let body = match before {
            Ok(body) => body,
            Err(error) => {
                return ChatResult {
                    outcome: Err(error),
                    finish_reason: None,
                    dispatch: Dispatch::NotDispatched,
                    remote: RemoteOutcome::NotStarted,
                    partial: AssistantMessage::default(),
                    pending_fragment: vec![],
                    consumed_bytes: 0,
                    records: 0,
                };
            }
        };
        let mut decoder = Decoder::for_model(self.limits.clone(), self.lock.name.clone())
            .expect("validated adapter identity and limits");
        let response = bounded(
            self.transport.send(Request {
                operation: Operation::Chat,
                body: Some(body),
            }),
            deadline,
            cancel,
        )
        .await;
        let outcome = match response {
            Err(error) => Err(error),
            Ok(mut response) => {
                if let Err(error) = encoding(&response) {
                    Err(error)
                } else if response.status != 200 {
                    return self
                        .http_error(response, deadline, cancel, observations)
                        .await;
                } else {
                    'receive: loop {
                        tokio::task::yield_now().await;
                        match bounded(
                            async { response.body.next().await.transpose() },
                            deadline,
                            cancel,
                        )
                        .await
                        {
                            Err(error) => break Err(error),
                            Ok(None) => {
                                let result = decoder.eof(observations).expect("no prior terminal");
                                break check(deadline, cancel).and(result);
                            }
                            Ok(Some(bytes)) => {
                                // Bound uninterrupted framing work even when a transport
                                // supplies a large immediately-ready chunk.
                                for part in bytes.chunks(16 * 1024) {
                                    tokio::task::yield_now().await;
                                    if let Err(error) = check(deadline, cancel) {
                                        break 'receive Err(error);
                                    }
                                    if let Some(result) = decoder.feed(part, observations) {
                                        break 'receive check(deadline, cancel).and(result);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        };
        if outcome
            .as_ref()
            .is_err_and(|error| error.kind == FailureKind::Identity)
        {
            self.verified = false;
        }
        ChatResult {
            outcome,
            finish_reason: decoder.finish_reason().map(str::to_owned),
            dispatch: Dispatch::PossiblySent,
            remote: if decoder.terminal_observed() {
                RemoteOutcome::TerminalObserved
            } else {
                RemoteOutcome::Uncertain
            },
            partial: decoder.partial().clone(),
            pending_fragment: decoder.pending_fragment().to_vec(),
            consumed_bytes: decoder.consumed_bytes(),
            records: decoder.records(),
        }
    }
    async fn http_error(
        &self,
        mut response: HttpResponse,
        deadline: Instant,
        cancel: &CancellationToken,
        observations: &ObservationSender,
    ) -> ChatResult {
        let mut bytes = Vec::new();
        let read = read_body(
            &mut response,
            &mut bytes,
            self.limits.error_bytes,
            deadline,
            cancel,
        )
        .await;
        let received = bytes.len();
        let truncated = read.is_err();
        bytes.truncate(self.limits.error_bytes);
        let recorded = observations.record(Observation {
            kind: ObservationKind::HttpError {
                status: response.status,
                truncated,
            },
            bytes: bytes.clone(),
        });
        let error = match read {
            Err(error) => error,
            Ok(()) => {
                let detail = json::parse(&bytes, self.limits.error_bytes, false).ok();
                let error = detail
                    .as_ref()
                    .and_then(|d| d.get("error"))
                    .and_then(serde_json::Value::as_str);
                if let Some(error) = error.filter(|s| Profile::tool_parse_error(s)) {
                    Failure::new(FailureKind::BackendToolParse, error)
                } else {
                    Failure::new(
                        FailureKind::Http,
                        format!("Ollama chat HTTP {}", response.status),
                    )
                }
            }
        };
        let error = check(deadline, cancel).err().unwrap_or(error);
        ChatResult {
            outcome: Err(if let Err(queue) = recorded {
                queue
            } else {
                error
            }),
            finish_reason: None,
            dispatch: Dispatch::PossiblySent,
            remote: RemoteOutcome::Uncertain,
            partial: AssistantMessage::default(),
            pending_fragment: bytes,
            consumed_bytes: received,
            records: 0,
        }
    }
}
pub(crate) fn encoding(response: &HttpResponse) -> Result<()> {
    if response
        .content_encoding
        .as_deref()
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        Err(Failure::new(
            FailureKind::Protocol,
            "unexpected HTTP content encoding",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn check(deadline: Instant, cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Failure::new(
            FailureKind::Cancelled,
            "local request cancelled; no backend termination receipt",
        ))
    } else if Instant::now() >= deadline {
        Err(Failure::new(
            FailureKind::Timeout,
            "absolute request deadline expired",
        ))
    } else {
        Ok(())
    }
}
pub(crate) async fn bounded<F: Future<Output = Result<T>>, T>(
    future: F,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<T> {
    check(deadline, cancel)?;
    tokio::select! {biased;_ = cancel.cancelled()=>Err(Failure::new(FailureKind::Cancelled,"local request cancelled; remote completion may be unknown")),
    _ = tokio::time::sleep_until(deadline)=>Err(Failure::new(FailureKind::Timeout,"absolute request deadline expired")),result=future=>result}
}
pub(crate) async fn read_body(
    response: &mut HttpResponse,
    bytes: &mut Vec<u8>,
    limit: usize,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<()> {
    loop {
        tokio::task::yield_now().await;
        match bounded(
            async { response.body.next().await.transpose() },
            deadline,
            cancel,
        )
        .await?
        {
            None => return Ok(()),
            Some(chunk) => {
                let take = chunk
                    .len()
                    .min(limit.saturating_add(1).saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..take]);
                if bytes.len() > limit {
                    return Err(Failure::new(
                        FailureKind::ByteLimit,
                        "HTTP response byte bound exceeded",
                    ));
                }
            }
        }
    }
}
