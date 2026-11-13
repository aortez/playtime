use std::{future::Future, net::IpAddr, time::Duration};

use futures_util::StreamExt;
use serde_json::Value;
use tokio::{sync::mpsc, time::Instant};

use crate::{
    CallResult, CancellationToken, Decoder, Failure, FailureKind, Identity, Limits, ModelLock,
    RemoteState, Result, StreamEvent, encode_request,
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

#[derive(Clone, Debug)]
pub struct Inspection {
    pub identity: Identity,
    pub started: u64,
    pub in_flight: u64,
    pub props: Value,
    pub status: Value,
    pub settings: Value,
}

pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
    lock: ModelLock,
    limits: Limits,
    started: Option<u64>,
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
    let settings = metadata(
        http,
        endpoint,
        "/settings",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    let status = metadata(
        http,
        endpoint,
        "/v1/status",
        limits.metadata_bytes,
        deadline,
        cancel,
    )
    .await?;
    let identity = Identity::inspect(&props, &status, &settings)?;
    let started = status["started"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| Failure::new(FailureKind::Identity, "server start identity is missing"))?;
    let in_flight = status["activity"]["in_flight"]
        .as_u64()
        .ok_or_else(|| Failure::new(FailureKind::Identity, "server activity is missing"))?;
    Ok(Inspection {
        identity,
        started,
        in_flight,
        props,
        status,
        settings,
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
            started: None,
        })
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
        if seen.identity != self.lock.identity || self.started.is_some_and(|s| s != seen.started) {
            return Err(Failure::new(
                FailureKind::Identity,
                "Strata model, template, settings or server process changed",
            ));
        }
        if seen.in_flight != 0 {
            return Err(Failure::new(
                FailureKind::Busy,
                "Strata has active or queued requests",
            ));
        }
        self.started = Some(seen.started);
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
        let mut decoder =
            Decoder::new(&self.lock.identity.model, self.limits.clone()).expect("validated limits");
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
        decoder: &mut Decoder,
        remote: &mut RemoteState,
    ) -> Result<()> {
        let bytes = encode_request(request, &self.lock, &self.limits)?;
        self.verify(deadline, cancel).await?;
        bounded(deadline, cancel, async {
            // From this point a request may reach the server, even if HTTP
            // headers or the first SSE frame never arrive. Never retry it here.
            *remote = RemoteState::Uncertain;
            let response = self
                .http
                .post(self.endpoint.url("/v1/responses"))
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .header("Accept-Encoding", "identity")
                .body(bytes)
                .send()
                .await
                .map_err(|e| Failure::new(FailureKind::Connection, e))?;
            if !response.status().is_success() {
                return Err(Failure::new(
                    FailureKind::Http,
                    format!("Responses HTTP {}; not replayed", response.status()),
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
                    "expected Responses event stream",
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
                "connection ended before a terminal Responses event",
            ))
        })
        .await
    }
}
