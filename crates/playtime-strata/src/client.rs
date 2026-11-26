use std::{future::Future, net::IpAddr, time::Duration};

use futures_util::StreamExt;
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};

use crate::{
    CallResult, CancellationToken, ChatDecoder, Completion, Decoder, Failure, FailureKind,
    Identity, Limits, ModelLock, RemoteState, Result, Service, StreamEvent, encode_request,
};

/// The initial profile reaches an owned local server or SSH forward. It does
/// not discover hosts, use ambient proxies, send credentials or follow redirects.
#[derive(Clone, Debug)]
pub struct Endpoint(reqwest::Url);
impl Endpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let mut url =
            reqwest::Url::parse(value).map_err(|e| Failure::new(FailureKind::Request, e))?;
        let loopback = url
            .host_str()
            .and_then(|h| {
                h.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if url.scheme() != "http"
            || !loopback
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/" | "/v1" | "/v1/")
        {
            return Err(Failure::new(
                FailureKind::Request,
                "Strata requires a loopback HTTP endpoint, optionally ending in /v1",
            ));
        }
        url.set_path("/");
        Ok(Self(url))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    fn url(&self, path: &str) -> reqwest::Url {
        self.0.join(path).expect("fixed absolute API path")
    }
}

/// What tells one server process from its restart. Matching values show only
/// that no restart was observed, never that the process is still running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Instance {
    /// The start time that Strata and Basalt report.
    Started(u64),
    /// llama-server reports no start time. Its media marker is random for each
    /// process, unless its environment pins one, which a lock's launch
    /// configuration excludes.
    Marker(String),
}

#[derive(Clone, Debug)]
pub struct Inspection {
    pub identity: Identity,
    pub instance: Instance,
    pub in_flight: u64,
    pub props: Value,
    /// Strata's and Basalt's /v1/status.
    pub status: Value,
    /// Strata's /settings.
    pub settings: Value,
    /// Basalt's and llama-server's /slots.
    pub slots: Value,
    /// llama-server's /v1/models.
    pub models: Value,
}
impl Inspection {
    /// The start time Strata and Basalt report, which llama-server doesn't.
    pub fn started(&self) -> Option<u64> {
        match self.instance {
            Instance::Started(started) => Some(started),
            Instance::Marker(_) => None,
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
    lock: ModelLock,
    limits: Limits,
    instance: Option<Instance>,
}
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .http1_only()
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| Failure::new(FailureKind::Connection, e))
}
pub(crate) async fn bounded<T>(
    deadline: Instant,
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {biased;
        _ = cancel.cancelled() => Err(Failure::new(FailureKind::Cancelled, "request cancelled locally; remote termination is unconfirmed")),
        _ = tokio::time::sleep_until(deadline) => Err(Failure::new(FailureKind::Timeout, "absolute model deadline expired")),
        value = future => value,
    }
}

async fn metadata(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    path: &str,
    limit: usize,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Value> {
    bounded(deadline, cancel, async {
        let response = http
            .get(endpoint.url(path))
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .map_err(|e| Failure::new(FailureKind::Connection, e))?;
        if !response.status().is_success() {
            return Err(Failure::new(
                FailureKind::Http,
                format!("{path}: HTTP {}", response.status()),
            ));
        }
        encoding(&response)?;
        let mut body = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(part) = body.next().await {
            let part = part.map_err(|e| Failure::new(FailureKind::Connection, e))?;
            if bytes.len().saturating_add(part.len()) > limit {
                return Err(Failure::new(
                    FailureKind::ByteLimit,
                    "metadata exceeds byte limit",
                ));
            }
            bytes.extend_from_slice(&part);
        }
        serde_json::from_slice(&bytes).map_err(|e| Failure::new(FailureKind::Protocol, e))
    })
    .await
}
fn encoding(response: &reqwest::Response) -> Result<()> {
    if response
        .headers()
        .get("content-encoding")
        .is_some_and(|v| v != "identity")
    {
        return Err(Failure::new(
            FailureKind::Protocol,
            "compressed responses are unsupported",
        ));
    }
    Ok(())
}
async fn inspect(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    limits: &Limits,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Inspection> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(15));
    let props = metadata(
        http,
        endpoint,
        "/props",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    // Strata and Basalt name themselves here. Whatever else answers must pass
    // llama-server's checks, and a lock's identity names its service.
    if !props["build_info"]
        .as_str()
        .is_some_and(|b| b.starts_with("Strata ") || b.starts_with("basalt "))
    {
        return inspect_llama(http, endpoint, limits, deadline, cancel, props).await;
    }
    let status = metadata(
        http,
        endpoint,
        "/v1/status",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    // Basalt has no /settings endpoint. Record its absence explicitly; never
    // manufacture Strata settings or accept a failed Strata metadata request.
    let settings = if status["service"] == "basalt" {
        Value::Null
    } else {
        metadata(
            http,
            endpoint,
            "/settings",
            limits.metadata_bytes,
            deadline,
            cancel,
        )
        .await?
    };
    let identity = Identity::inspect(&props, &status, &settings)?;
    let started = status["started"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| Failure::new(FailureKind::Identity, "server start identity is missing"))?;
    let mut in_flight = status["activity"]["in_flight"]
        .as_u64()
        .ok_or_else(|| Failure::new(FailureKind::Identity, "server activity is missing"))?;
    if identity.service == Service::Basalt {
        // Basalt 0.4.1 reports bool(busy) + queued here, not the number of
        // active batch slots. Count its slot table before admitting a peer.
        let slots = metadata(
            http,
            endpoint,
            "/slots",
            limits.metadata_bytes,
            deadline,
            cancel,
        )
        .await?;
        in_flight = in_flight.max(active_slots(
            &slots,
            &identity,
            "invalid Basalt slot activity",
        )?);
        return Ok(Inspection {
            identity,
            instance: Instance::Started(started),
            in_flight,
            props,
            status,
            settings,
            slots,
            models: Value::Null,
        });
    }
    Ok(Inspection {
        identity,
        instance: Instance::Started(started),
        in_flight,
        props,
        status,
        settings,
        slots: Value::Null,
        models: Value::Null,
    })
}

/// The number of processing slots in a table of exactly the identity's slots.
fn active_slots(slots: &Value, identity: &Identity, detail: &str) -> Result<u64> {
    let invalid = || Failure::new(FailureKind::Identity, detail);
    let slots = slots.as_array().ok_or_else(invalid)?;
    if slots.len() as u64 != identity.parallel {
        return Err(invalid());
    }
    let mut active = 0;
    for (id, slot) in slots.iter().enumerate() {
        if slot["id"].as_u64() != Some(id as u64)
            || slot["n_ctx"].as_u64() != Some(identity.context_tokens)
        {
            return Err(invalid());
        }
        active += u64::from(slot["is_processing"].as_bool().ok_or_else(invalid)?);
    }
    Ok(active)
}

/// `<__media_`, 32 random letters and digits, then `__>`.
fn random_marker(marker: &str) -> bool {
    marker
        .strip_prefix("<__media_")
        .and_then(|m| m.strip_suffix("__>"))
        .is_some_and(|m| m.len() == 32 && m.bytes().all(|b| b.is_ascii_alphanumeric()))
}

async fn inspect_llama(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    limits: &Limits,
    deadline: Instant,
    cancel: &CancellationToken,
    props: Value,
) -> Result<Inspection> {
    let slots = metadata(
        http,
        endpoint,
        "/slots",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    let models = metadata(
        http,
        endpoint,
        "/v1/models",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    let identity = Identity::inspect_llama(&props, &slots, &models)?;
    let marker = props["media_marker"]
        .as_str()
        .filter(|m| random_marker(m))
        .ok_or_else(|| {
            Failure::new(
                FailureKind::Identity,
                "llama-server's process marker is missing or pinned",
            )
        })?;
    let in_flight = active_slots(&slots, &identity, "invalid llama-server slot activity")?;
    Ok(Inspection {
        identity,
        instance: Instance::Marker(marker.into()),
        in_flight,
        props,
        status: Value::Null,
        settings: Value::Null,
        slots,
        models,
    })
}

/// Read-only discovery. The caller reviews/freezes this metadata together with
/// independently verified host artifacts; discovery does not acquire ownership.
pub async fn inspect_endpoint(
    endpoint: &Endpoint,
    limits: &Limits,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<Inspection> {
    limits.validate()?;
    inspect(&http_client()?, endpoint, limits, deadline, cancel).await
}

impl Client {
    pub fn new(endpoint: Endpoint, lock: ModelLock, limits: Limits) -> Result<Self> {
        lock.validate()?;
        limits.validate()?;
        Ok(Self {
            http: http_client()?,
            endpoint,
            lock,
            limits,
            instance: None,
        })
    }
    /// Bind the client to the server process a caller already verified. Its
    /// first check then refuses any other process, as every later check does.
    pub fn with_server_start(mut self, started: u64) -> Self {
        self.instance = Some(Instance::Started(started));
        self
    }
    pub fn lock(&self) -> &ModelLock {
        &self.lock
    }
    pub async fn verify(
        &mut self,
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<Inspection> {
        let seen = inspect(&self.http, &self.endpoint, &self.limits, deadline, cancel).await?;
        let llama = self.lock.identity.service == Service::LlamaServer;
        if seen.identity != self.lock.identity
            || self.instance.as_ref().is_some_and(|i| *i != seen.instance)
        {
            return Err(Failure::new(
                FailureKind::Identity,
                if llama {
                    "llama-server model, template, settings or process changed"
                } else {
                    "Strata model, template, settings or server process changed"
                },
            ));
        }
        // The external Basalt owner lends at most one client per serving slot.
        // Another admitted client may be generating while this one verifies.
        if (seen.identity.service != Service::Basalt && seen.in_flight != 0)
            || seen.in_flight >= seen.identity.parallel
        {
            return Err(Failure::new(
                FailureKind::Busy,
                if llama {
                    "llama-server has an active request"
                } else {
                    "Strata has active or queued requests"
                },
            ));
        }
        self.instance = Some(seen.instance.clone());
        Ok(seen)
    }

    pub async fn complete(
        &mut self,
        request: &playtime_engine::ModelRequest,
        deadline: Instant,
        cancel: &CancellationToken,
        events: mpsc::Sender<StreamEvent>,
    ) -> CallResult {
        let mut remote = RemoteState::NotDispatched;
        let identity = &self.lock.identity;
        let mut decoder = if identity.service == Service::LlamaServer {
            Stream::Chat(
                ChatDecoder::new(&identity.model, &identity.engine, self.limits.clone())
                    .expect("validated limits"),
            )
        } else {
            Stream::Responses(
                Decoder::new(&identity.model, self.limits.clone()).expect("validated limits"),
            )
        };
        let result = self
            .run(request, deadline, cancel, events, &mut decoder, &mut remote)
            .await;
        if decoder.terminal_observed() {
            remote = RemoteState::TerminalObserved;
        }
        let partial = decoder.partial();
        let received_bytes = decoder.received_bytes();
        let completion = result.and_then(|()| decoder.finish());
        CallResult {
            completion,
            remote,
            partial,
            received_bytes,
        }
    }
    async fn run(
        &mut self,
        request: &playtime_engine::ModelRequest,
        deadline: Instant,
        cancel: &CancellationToken,
        events: mpsc::Sender<StreamEvent>,
        decoder: &mut Stream,
        remote: &mut RemoteState,
    ) -> Result<()> {
        let bytes = encode_request(request, &self.lock, &self.limits)?;
        self.verify(deadline, cancel).await?;
        let (path, dialect) = match decoder {
            Stream::Chat(_) => ("/v1/chat/completions", "Chat Completions"),
            Stream::Responses(_) => ("/v1/responses", "Responses"),
        };
        bounded(deadline, cancel, async {
            // From this point a request may reach the server, even if HTTP
            // headers or the first SSE frame never arrive. Never retry it here.
            *remote = RemoteState::Uncertain;
            let response = self
                .http
                .post(self.endpoint.url(path))
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .header("Accept-Encoding", "identity")
                .body(bytes)
                .send()
                .await
                .map_err(|e| Failure::new(FailureKind::Connection, e))?;
            if !response.status().is_success() {
                let status = response.status();
                // llama-server explains a rejected request in its body.
                let reason = match decoder {
                    Stream::Chat(_) => error_message(response, self.limits.metadata_bytes).await,
                    Stream::Responses(_) => None,
                };
                return Err(Failure::new(
                    FailureKind::Http,
                    match reason {
                        Some(reason) => format!("{dialect} HTTP {status} ({reason}); not replayed"),
                        None => format!("{dialect} HTTP {status}; not replayed"),
                    },
                ));
            }
            encoding(&response)?;
            if response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_none_or(|s| s.split(';').next() != Some("text/event-stream"))
            {
                return Err(Failure::new(
                    FailureKind::Protocol,
                    format!("expected {dialect} event stream"),
                ));
            }
            let mut body = response.bytes_stream();
            while let Some(bytes) = body.next().await {
                let bytes = bytes.map_err(|e| Failure::new(FailureKind::Connection, e))?;
                let parsed = decoder.push(&bytes);
                for event in decoder.take_events() {
                    events.send(event).await.map_err(|_| {
                        Failure::new(FailureKind::Protocol, "event consumer disconnected")
                    })?;
                }
                parsed?;
                if decoder.is_finished() {
                    return Ok(());
                }
            }
            Err(Failure::new(
                FailureKind::Incomplete,
                format!("connection ended before a terminal {dialect} event"),
            ))
        })
        .await
    }
}

/// The `error.message` of a bounded JSON error body, if there is one.
async fn error_message(response: reqwest::Response, limit: usize) -> Option<String> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(part) = body.next().await {
        bytes.extend_from_slice(&part.ok()?);
        if bytes.len() > limit {
            return None;
        }
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["error"]["message"].as_str().map(str::to_owned)
}

/// The decoder for the lock's dialect.
pub(crate) enum Stream {
    Responses(Decoder),
    Chat(ChatDecoder),
}
impl Stream {
    fn push(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Responses(d) => d.push(bytes),
            Self::Chat(d) => d.push(bytes),
        }
    }
    fn take_events(&mut self) -> Vec<StreamEvent> {
        match self {
            Self::Responses(d) => d.take_events(),
            Self::Chat(d) => d.take_events(),
        }
    }
    fn is_finished(&self) -> bool {
        match self {
            Self::Responses(d) => d.is_finished(),
            Self::Chat(d) => d.is_finished(),
        }
    }
    fn terminal_observed(&self) -> bool {
        match self {
            Self::Responses(d) => d.terminal_observed(),
            Self::Chat(d) => d.terminal_observed(),
        }
    }
    fn partial(&self) -> Value {
        match self {
            Self::Responses(d) => d.partial(),
            Self::Chat(d) => d.partial(),
        }
    }
    fn received_bytes(&self) -> usize {
        match self {
            Self::Responses(d) => d.received_bytes(),
            Self::Chat(d) => d.received_bytes(),
        }
    }
    fn finish(self) -> Result<Completion> {
        match self {
            Self::Responses(d) => d.finish(),
            Self::Chat(d) => d.finish(),
        }
    }
}
