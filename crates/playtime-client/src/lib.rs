//! Shared native HTTP client. Observation cancellation only detaches the client.
pub mod config;
mod downloads;
mod events;
pub mod state;
pub use events::{EventStream, Follow, Observation, ObservedRecord, RetryPolicy};
pub use playtime_local::{Token, read_connection};

use base64::{Engine, engine::general_purpose::STANDARD};
use playtime_core::{RunId, SessionId};
use playtime_protocol::{api::*, events::decode_event, v1::ErrorResponse};
use reqwest::{
    Method, Response, Url,
    header::{self, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, Error>;
pub enum Error {
    Api {
        status: u16,
        response: ErrorResponse,
    },
    Transport,
    Timeout,
    InvalidResponse,
    LimitExceeded,
    IdentityMismatch,
    UnsupportedEndpoint,
    Cancelled,
    RetryBudgetExhausted,
    UnacknowledgedRecord,
    ContentMismatch,
    Io,
    /// The caller retains its PendingLaunch and resolves this by lookup or by
    /// resubmitting that exact service/key/request. Never generate a fresh key.
    AmbiguousMutation(Box<Error>),
}
impl Error {
    pub fn api_code(&self) -> Option<&str> {
        match self {
            Self::Api { response, .. } => Some(&response.code),
            _ => None,
        }
    }
    /// Whether a read/observation may retry this failure. Mutations still require
    /// their original identity and ambiguity handling; this is not permission to
    /// resubmit a launch or cancellation.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Transport | Self::Timeout)
            || matches!(
                self.api_code(),
                Some(
                    "backend_unavailable"
                        | "too_many_observers"
                        | "request_timeout"
                        | "ownership_unresolved"
                )
            )
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Api { status, response } => {
                return write!(
                    f,
                    "HTTP {status}: {}",
                    if valid_id(&response.code) {
                        &response.code
                    } else {
                        "invalid_error_code"
                    }
                );
            }
            Self::Transport => "HTTP transport failed",
            Self::Timeout => "HTTP deadline exceeded",
            Self::InvalidResponse => "Invalid API response",
            Self::LimitExceeded => "Response exceeded the client limit",
            Self::IdentityMismatch => "Service identity changed",
            Self::UnsupportedEndpoint => "Only loopback HTTP endpoints are supported",
            Self::Cancelled => "Observation detached",
            Self::RetryBudgetExhausted => "Reconnect budget exhausted",
            Self::UnacknowledgedRecord => {
                "Apply and acknowledge the delivered record before reading another"
            }
            Self::ContentMismatch => "Downloaded content failed verification",
            Self::Io => "Local I/O failed",
            Self::AmbiguousMutation(_) => {
                "Mutation result is uncertain; retain the original service and request identity"
            }
        };
        f.write_str(text)
    }
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for Error {}
fn transport(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Transport
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingLaunch {
    pub schema_version: u32,
    pub service_id: ServiceId,
    pub key: IdempotencyKey,
    pub request: LaunchRequest,
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Url,
    service: ServiceId,
    capabilities: Capabilities,
    limits: ApiLimits,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}
impl Client {
    pub async fn from_connection(path: &Path) -> Result<Self> {
        let (connection, token) = read_connection(path).map_err(|_| Error::Io)?;
        Self::connect(&connection.endpoint, token, Some(connection.service_id)).await
    }
    pub async fn connect(
        endpoint: &str,
        token: Token,
        expected_service: Option<ServiceId>,
    ) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|_| Error::UnsupportedEndpoint)?;
        if url.scheme() != "http"
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::UnsupportedEndpoint);
        }
        let host = url.host_str().ok_or(Error::UnsupportedEndpoint)?;
        if !matches!(host, "127.0.0.1" | "[::1]" | "::1" | "localhost") {
            return Err(Error::UnsupportedEndpoint);
        }
        let port = url
            .port_or_known_default()
            .ok_or(Error::UnsupportedEndpoint)?;
        if port == 0 {
            return Err(Error::UnsupportedEndpoint);
        }
        let addresses: Vec<SocketAddr> = timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host.trim_matches(['[', ']']), port)),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Transport)?
        .collect();
        if addresses.is_empty() || addresses.iter().any(|a| !a.ip().is_loopback()) {
            return Err(Error::UnsupportedEndpoint);
        }
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .map_err(|_| Error::InvalidResponse)?;
        auth.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(
            header::HOST,
            HeaderValue::from_str(&format!("{host}:{port}"))
                .map_err(|_| Error::UnsupportedEndpoint)?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(45))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(transport)?;
        let response = http
            .get(url.join("api/v1/capabilities").unwrap())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(transport)?;
        let service_header = response
            .headers()
            .get(SERVICE_HEADER)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let capabilities: Capabilities =
            read_json(response, ApiLimits::default().json_bytes).await?;
        if !capabilities.api_versions.contains(&1) {
            return Err(Error::InvalidResponse);
        }
        if service_header.as_deref() != Some(capabilities.service_id.as_str())
            || expected_service.is_some_and(|id| id != capabilities.service_id)
        {
            return Err(Error::IdentityMismatch);
        }
        let mut limits = capabilities.limits.clone();
        // The peer cannot increase client allocation or deadlines.
        let defaults = ApiLimits::default();
        limits.json_bytes = limits.json_bytes.min(defaults.json_bytes);
        limits.event_bytes = limits.event_bytes.min(defaults.event_bytes);
        limits.artifact_bytes = limits.artifact_bytes.min(defaults.artifact_bytes);
        limits.cursor_bytes = limits.cursor_bytes.min(defaults.cursor_bytes);
        limits.handler_seconds = limits.handler_seconds.clamp(1, 30);
        limits.sse_framing_bytes = limits.sse_framing_bytes.min(defaults.sse_framing_bytes);
        limits.stream_pending_bytes = limits
            .stream_pending_bytes
            .min(defaults.stream_pending_bytes);
        limits.stream_idle_seconds = limits.stream_idle_seconds.clamp(1, 45);
        if limits.json_bytes == 0 || limits.event_bytes == 0 || limits.cursor_bytes == 0 {
            return Err(Error::InvalidResponse);
        }
        Ok(Self {
            http,
            endpoint: url,
            service: capabilities.service_id.clone(),
            capabilities,
            limits,
        })
    }
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str().trim_end_matches('/')
    }
    pub fn service_id(&self) -> &ServiceId {
        &self.service
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(
                method,
                self.endpoint.join(&format!("api/v1/{path}")).unwrap(),
            )
            .header(SERVICE_HEADER, self.service.as_str())
    }
    fn check_response(&self, response: &Response) -> Result<()> {
        if response.status().as_u16() != 401
            && response
                .headers()
                .get(SERVICE_HEADER)
                .and_then(|h| h.to_str().ok())
                != Some(self.service.as_str())
        {
            return Err(Error::IdentityMismatch);
        }
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            return Err(Error::InvalidResponse);
        }
        Ok(())
    }
    async fn json<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request
            .timeout(Duration::from_secs(self.limits.handler_seconds))
            .send()
            .await
            .map_err(transport)?;
        self.check_response(&response)?;
        read_json(response, self.limits.json_bytes).await
    }
    pub async fn schema(&self) -> Result<serde_json::Value> {
        self.json(self.request(Method::GET, "openapi.json")).await
    }
    pub async fn recipes(&self, query: &PageQuery) -> Result<Page<Recipe>> {
        self.json(self.request(Method::GET, "recipes").query(query))
            .await
    }
    pub async fn preview(
        &self,
        recipe: &RecipeId,
        mode: playtime_core::RunMode,
    ) -> Result<Preview> {
        let preview: Preview = self
            .json(
                self.request(Method::POST, &format!("recipes/{recipe}/preview"))
                    .json(&PreviewRequest { mode }),
            )
            .await?;
        preview
            .parameters
            .validate()
            .map_err(|_| Error::InvalidResponse)?;
        if preview.recipe_id != *recipe || preview.mode != mode {
            return Err(Error::InvalidResponse);
        }
        Ok(preview)
    }
    pub fn prepare_launch(
        &self,
        request: LaunchRequest,
        key: Option<IdempotencyKey>,
    ) -> Result<PendingLaunch> {
        let key = match key {
            Some(key) => key,
            None => {
                let mut bytes = [0; 24];
                getrandom::fill(&mut bytes).map_err(|_| Error::Io)?;
                IdempotencyKey::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
                    .unwrap()
            }
        };
        Ok(PendingLaunch {
            schema_version: 1,
            service_id: self.service.clone(),
            key,
            request,
        })
    }
    fn check_pending(&self, pending: &PendingLaunch) -> Result<()> {
        if pending.schema_version != 1 {
            return Err(Error::InvalidResponse);
        }
        if pending.service_id != self.service {
            return Err(Error::IdentityMismatch);
        }
        Ok(())
    }
    pub async fn launch(&self, pending: &PendingLaunch) -> Result<LaunchResponse> {
        self.check_pending(pending)?;
        let result = self
            .json(
                self.request(Method::POST, "runs")
                    .header("idempotency-key", pending.key.as_str())
                    .json(&pending.request),
            )
            .await;
        let reply: LaunchResponse = ambiguous(result)?;
        ambiguous(validate_launch(&reply, Some(pending)))?;
        Ok(reply)
    }
    pub async fn lookup(&self, pending: &PendingLaunch) -> Result<LaunchResponse> {
        self.check_pending(pending)?;
        let reply = self.lookup_key(&pending.key).await?;
        validate_launch(&reply, Some(pending))?;
        Ok(reply)
    }
    pub async fn lookup_key(&self, key: &IdempotencyKey) -> Result<LaunchResponse> {
        let reply = self
            .json(self.request(Method::GET, &format!("launches/{key}")))
            .await?;
        validate_launch(&reply, None)?;
        Ok(reply)
    }
    pub async fn runs(&self, query: &PageQuery) -> Result<Page<RunView>> {
        let page: Page<RunView> = self
            .json(self.request(Method::GET, "runs").query(query))
            .await?;
        for run in &page.items {
            validate_run(run)?;
        }
        Ok(page)
    }
    pub async fn run(&self, id: &RunId) -> Result<RunView> {
        let value: RunView = self
            .json(self.request(Method::GET, &format!("runs/{id}")))
            .await?;
        if value.run_id != *id {
            return Err(Error::InvalidResponse);
        }
        validate_run(&value)?;
        Ok(value)
    }
    pub async fn cancel(&self, id: &RunId) -> Result<CancelResponse> {
        let reply: CancelResponse = ambiguous(
            self.json(
                self.request(Method::POST, &format!("runs/{id}/cancel"))
                    .json(&CancelRequest {}),
            )
            .await,
        )?;
        ambiguous(validate_run(&reply.run))?;
        if reply.run.run_id != *id || reply.intent != reply.run.cancellation {
            return Err(Error::AmbiguousMutation(Box::new(Error::InvalidResponse)));
        }
        Ok(reply)
    }
    pub async fn history(&self, id: &RunId, query: &PageQuery) -> Result<EventPage> {
        let page: EventPage = self
            .json(
                self.request(Method::GET, &format!("runs/{id}/events"))
                    .query(query),
            )
            .await?;
        let position = query
            .cursor
            .as_deref()
            .map(EventCursor::decode)
            .transpose()
            .map_err(|_| Error::InvalidResponse)?;
        if page.frontier.run_id != *id
            || position
                .as_ref()
                .is_some_and(|p| p.frontier != page.frontier)
        {
            return Err(Error::InvalidResponse);
        }
        let mut sequence = position.as_ref().map_or(0, |p| p.after_sequence);
        for event in &page.events {
            if event.run_id != *id || Some(event.sequence.get()) != sequence.checked_add(1) {
                return Err(Error::InvalidResponse);
            }
            let encoded = serde_json::to_string(event).map_err(|_| Error::InvalidResponse)?;
            if encoded.len() > self.limits.event_bytes {
                return Err(Error::LimitExceeded);
            }
            decode_event(&encoded).map_err(|_| Error::InvalidResponse)?;
            sequence += 1;
        }
        let expected = EventCursor::new(page.frontier.clone(), sequence)
            .map_err(|_| Error::InvalidResponse)?
            .encode()
            .map_err(|_| Error::InvalidResponse)?;
        if page.resume_cursor != expected
            || page.has_more != (sequence < page.frontier.prefix.sequence)
            || page.next_cursor != page.has_more.then_some(expected)
            || (page.has_more && page.events.is_empty())
        {
            return Err(Error::InvalidResponse);
        }
        Ok(page)
    }
    pub async fn checkpoints(&self, id: &RunId, query: &PageQuery) -> Result<Page<Checkpoint>> {
        self.json(
            self.request(Method::GET, &format!("runs/{id}/checkpoints"))
                .query(query),
        )
        .await
    }
    pub async fn checkpoint(&self, id: &RunId, session: &SessionId) -> Result<Checkpoint> {
        let checkpoint: Checkpoint = self
            .json(self.request(Method::GET, &format!("runs/{id}/checkpoints/{session}")))
            .await?;
        if checkpoint.checkpoint.session_id != *session {
            return Err(Error::InvalidResponse);
        }
        Ok(checkpoint)
    }
    pub async fn artifacts(&self, id: &RunId, query: &PageQuery) -> Result<Page<Artifact>> {
        self.json(
            self.request(Method::GET, &format!("runs/{id}/artifacts"))
                .query(query),
        )
        .await
    }
    pub async fn artifact(&self, id: &RunId, artifact: &ArtifactId) -> Result<Artifact> {
        let result: Artifact = self
            .json(self.request(Method::GET, &format!("runs/{id}/artifacts/{artifact}")))
            .await?;
        if result.artifact_id != *artifact {
            return Err(Error::InvalidResponse);
        }
        Ok(result)
    }

    /// Writes verified bytes incrementally. On any error the destination is
    /// incomplete and must be discarded; download_to_path handles that staging.
    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        id: &RunId,
        artifact: &Artifact,
        destination: &mut W,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        if artifact.content.size_bytes > self.limits.artifact_bytes {
            return Err(Error::LimitExceeded);
        }
        let deadline = Instant::now() + Duration::from_secs(self.limits.handler_seconds);
        let task = async {
            let mut response = self
                .request(
                    Method::GET,
                    &format!("runs/{id}/artifacts/{}/content", artifact.artifact_id),
                )
                .send()
                .await
                .map_err(transport)?;
            self.check_response(&response)?;
            if !response.status().is_success() {
                return read_json::<u64>(response, self.limits.json_bytes).await;
            }
            let hash = artifact.content.sha256.as_str();
            let digest_bytes: Vec<_> = (0..64)
                .step_by(2)
                .map(|i| u8::from_str_radix(&hash[i..i + 2], 16).unwrap())
                .collect();
            let expected_digest = format!("sha-256=:{}:", STANDARD.encode(digest_bytes));
            if response.content_length() != Some(artifact.content.size_bytes)
                || response
                    .headers()
                    .get("x-playtime-sha256")
                    .and_then(|v| v.to_str().ok())
                    != Some(hash)
                || response
                    .headers()
                    .get("content-digest")
                    .and_then(|v| v.to_str().ok())
                    != Some(expected_digest.as_str())
                || response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    != Some("application/octet-stream")
            {
                return Err(Error::ContentMismatch);
            }
            let mut count = 0u64;
            let mut hasher = Sha256::new();
            while let Some(bytes) = response.chunk().await.map_err(transport)? {
                count = count
                    .checked_add(bytes.len() as u64)
                    .ok_or(Error::LimitExceeded)?;
                if count > self.limits.artifact_bytes || count > artifact.content.size_bytes {
                    return Err(Error::LimitExceeded);
                }
                hasher.update(&bytes);
                destination.write_all(&bytes).await.map_err(|_| Error::Io)?;
            }
            if count != artifact.content.size_bytes
                || hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
                    != hash
            {
                return Err(Error::ContentMismatch);
            }
            destination.flush().await.map_err(|_| Error::Io)?;
            Ok(count)
        };
        tokio::select! { _ = cancel.cancelled() => Err(Error::Cancelled), result = timeout_at(deadline, task) => result.map_err(|_| Error::Timeout)? }
    }
}
fn ambiguous<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if matches!(
            error,
            Error::Transport | Error::Timeout | Error::InvalidResponse | Error::LimitExceeded
        ) || error.api_code() == Some("request_timeout")
        {
            Error::AmbiguousMutation(Box::new(error))
        } else {
            error
        }
    })
}
async fn read_json<T: DeserializeOwned>(mut response: Response, limit: usize) -> Result<T> {
    let status = response.status();
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(';').next())
        != Some("application/json")
    {
        return Err(Error::InvalidResponse);
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Error::LimitExceeded);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if chunk.len() > limit - bytes.len() {
            return Err(Error::LimitExceeded);
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let response: ErrorResponse =
            serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)?;
        return Err(Error::Api {
            status: status.as_u16(),
            response,
        });
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidResponse)
}
fn validate_run(run: &RunView) -> Result<()> {
    if let Some(usage) = &run.usage {
        usage.validate().map_err(|_| Error::InvalidResponse)?;
    }
    match (&run.last_confirmed_frontier, &run.event_anchors) {
        (Some(frontier), Some(anchors)) => {
            if frontier.run_id != run.run_id
                || *anchors
                    != EventAnchors::new(frontier.clone()).map_err(|_| Error::InvalidResponse)?
            {
                return Err(Error::InvalidResponse);
            }
        }
        (None, None) if !matches!(run.evidence, EvidenceAvailability::Available) => (),
        _ => return Err(Error::InvalidResponse),
    }
    Ok(())
}

fn validate_launch(reply: &LaunchResponse, pending: Option<&PendingLaunch>) -> Result<()> {
    validate_run(&reply.run)?;
    if reply.run_id != reply.run.run_id
        || reply.accepted_at != reply.run.accepted_at
        || pending.is_some_and(|p| {
            p.request.recipe_id != reply.run.recipe_id
                || p.request.mode != reply.run.mode
                || p.request.preview_revision != reply.run.preview_revision
        })
    {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}
