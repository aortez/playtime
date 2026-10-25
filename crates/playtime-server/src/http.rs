//! Reusable Axum/Tower transport with one error and authentication boundary.
mod schema;
mod stream;
pub use schema::openapi;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use playtime_core::{RequestId, RunId, SessionId};
use playtime_protocol::api::*;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{self, Write},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    application::{ApiError, Application, History, Result},
    auth::{Token, authenticate, check_native_headers, single_header},
};

pub struct Server {
    application: Arc<dyn Application>,
    capabilities: Capabilities,
    token: Token,
    streams: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
    mutations: Arc<Semaphore>,
    shutdown: CancellationToken,
    request_sequence: AtomicU64,
}
impl Server {
    pub fn new(application: Arc<dyn Application>, token: Token) -> Result<Arc<Self>> {
        let capabilities = application.capabilities();
        let limits = &capabilities.limits;
        let maximum = ApiLimits::default();
        if limits.request_bytes == 0
            || limits.request_bytes > maximum.request_bytes
            || limits.json_bytes < 4096
            || limits.json_bytes > maximum.json_bytes
            || limits.event_bytes == 0
            || limits.event_bytes > maximum.event_bytes
            || limits.sse_framing_bytes != maximum.sse_framing_bytes
            || limits.artifact_bytes == 0
            || limits.artifact_bytes > maximum.artifact_bytes
            || limits.cursor_bytes != maximum.cursor_bytes
            || limits.default_page_items == 0
            || limits.default_page_items > limits.max_page_items
            || limits.max_page_items > maximum.max_page_items
            || limits.event_streams == 0
            || limits.event_streams > maximum.event_streams
            || limits.artifact_transfers == 0
            || limits.artifact_transfers > maximum.artifact_transfers
            || limits.stream_pending_records == 0
            || limits.stream_pending_records > maximum.stream_pending_records
            || limits.stream_pending_bytes < limits.event_bytes + limits.sse_framing_bytes
            || limits.stream_pending_bytes > maximum.stream_pending_bytes
            || limits.handler_seconds == 0
            || limits.handler_seconds > 30
            || limits.connect_seconds != 5
            || limits.heartbeat_seconds == 0
            || limits.heartbeat_seconds > 15
            || limits.stream_idle_seconds != 45
        {
            return Err(ApiError::new("invalid_parameters"));
        }
        Ok(Arc::new(Self {
            streams: Arc::new(Semaphore::new(limits.event_streams)),
            downloads: Arc::new(Semaphore::new(limits.artifact_transfers)),
            mutations: Arc::new(Semaphore::new(16)),
            application,
            capabilities,
            token,
            shutdown: CancellationToken::new(),
            request_sequence: AtomicU64::new(1),
        }))
    }
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .fallback(dispatch)
            .with_state(self.clone())
            .layer(middleware::from_fn_with_state(self.clone(), guard))
    }
    pub async fn listen(self: &Arc<Self>, address: SocketAddr) -> io::Result<RunningServer> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only loopback listeners are supported",
            ));
        }
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        let stop = self.shutdown.clone();
        let router = self.router();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
        Ok(RunningServer {
            address,
            shutdown: self.shutdown.clone(),
            task: Some(task),
        })
    }
    fn limits(&self) -> &ApiLimits {
        &self.capabilities.limits
    }
}
pub struct RunningServer {
    pub address: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
}
impl RunningServer {
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }
    pub async fn stop(mut self) -> io::Result<()> {
        self.shutdown.cancel();
        let mut task = self.task.take().unwrap();
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => result.map_err(|_| io::Error::other("server task failed"))?,
            Err(_) => {
                task.abort();
                let _ = task.await;
                Ok(())
            }
        }
    }
}
impl Drop for RunningServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn guard(State(server): State<Arc<Server>>, mut request: Request, next: Next) -> Response {
    let request_id = RequestId::new(format!(
        "http-{}-{}",
        std::process::id(),
        server.request_sequence.fetch_add(1, Ordering::Relaxed)
    ))
    .unwrap();
    let authenticated = authenticate(request.headers(), &server.token);
    let permitted = authenticated
        .clone()
        .and_then(|_| check_native_headers(request.headers()))
        .and_then(|_| {
            if request.method() == Method::POST {
                let id = single_header(request.headers(), SERVICE_HEADER)?
                    .ok_or_else(|| ApiError::new("malformed_data"))?;
                let id = ServiceId::new(id).map_err(|_| ApiError::new("malformed_data"))?;
                if id != server.capabilities.service_id {
                    return Err(ApiError::new("service_identity_mismatch"));
                }
            }
            Ok(())
        });
    request.extensions_mut().insert(request_id.clone());
    let mut response = match permitted {
        Err(error) => error_response(error, Some(request_id.clone())),
        Ok(()) => {
            tokio::select! {
                _ = server.shutdown.cancelled() => error_response(ApiError::new("backend_unavailable"), Some(request_id.clone())),
                result = tokio::time::timeout(Duration::from_secs(server.limits().handler_seconds), next.run(request)) => {
                    result.unwrap_or_else(|_| error_response(ApiError::new("request_timeout"), Some(request_id.clone())))
                }
            }
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        REQUEST_HEADER,
        HeaderValue::from_str(request_id.as_str()).unwrap(),
    );
    if authenticated.is_ok() {
        headers.insert(
            SERVICE_HEADER,
            HeaderValue::from_str(server.capabilities.service_id.as_str()).unwrap(),
        );
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

pub(crate) fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit - self.bytes.len() {
                return Err(io::Error::other("JSON bound exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| ApiError::new("evidence_limit_exceeded"))?;
    Ok(writer.bytes)
}
fn json(value: &impl Serialize, status: u16, limit: usize) -> Result<Response> {
    let bytes = bounded_json(value, limit)?;
    Ok(Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap())
}
fn safe_detail(value: &serde_json::Value, depth: usize) -> bool {
    if depth > 2 {
        return false;
    }
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
        serde_json::Value::String(s) => valid_id(s),
        serde_json::Value::Array(a) => a.len() <= 32 && a.iter().all(|v| safe_detail(v, depth + 1)),
        _ => false,
    }
}
pub(crate) fn error_response(error: ApiError, request_id: Option<RequestId>) -> Response {
    let status = error.status();
    let mut safe = ApiError::new(if valid_id(error.code()) {
        error.code()
    } else {
        "internal_error"
    })
    .0;
    safe.request_id = request_id;
    safe.details = error
        .0
        .details
        .into_iter()
        .filter(|(key, value)| valid_id(key) && safe_detail(value, 0))
        .take(16)
        .collect();
    // Individually bounded fields can exceed the whole response budget together.
    // Optional diagnostics must not prevent delivery of the original error.
    if bounded_json(&safe, 16_384).is_err() {
        safe.details.clear();
    }
    json(&safe, status, 16384).expect("bounded safe error")
}
async fn body<T: DeserializeOwned>(request: Request, limits: &ApiLimits) -> Result<T> {
    let content_type = single_header(request.headers(), "content-type")?
        .ok_or_else(|| ApiError::new("unsupported_media_type"))?;
    if !content_type
        .split(';')
        .next()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
        || request.headers().contains_key(header::CONTENT_ENCODING)
    {
        return Err(ApiError::new("unsupported_media_type"));
    }
    if single_header(request.headers(), "content-length")?.is_some_and(|s| {
        s.parse::<u64>()
            .is_ok_and(|n| n > limits.request_bytes as u64)
    }) {
        return Err(ApiError::new("limit_exceeded"));
    }
    let bytes = to_bytes(request.into_body(), limits.request_bytes)
        .await
        .map_err(|error| {
            if std::error::Error::source(&error)
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
            {
                ApiError::new("limit_exceeded")
            } else {
                ApiError::new("malformed_data")
            }
        })?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::new("malformed_data"))
}
fn query(request: &Request, limits: &ApiLimits) -> Result<PageQuery> {
    let query: PageQuery = serde_urlencoded::from_str(request.uri().query().unwrap_or(""))
        .map_err(|_| ApiError::new("malformed_data"))?;
    if query
        .cursor
        .as_ref()
        .is_some_and(|c| c.len() > limits.cursor_bytes)
    {
        return Err(ApiError::new("invalid_cursor"));
    }
    query
        .limit(limits)
        .map_err(|_| ApiError::new("malformed_data"))?;
    Ok(query)
}
fn no_query(request: &Request) -> Result<()> {
    if request.uri().query().is_some_and(|s| !s.is_empty()) {
        return Err(ApiError::new("malformed_data"));
    }
    Ok(())
}
fn run_id(id: &str) -> Result<RunId> {
    RunId::new(id).map_err(|_| ApiError::new("malformed_data"))
}
fn artifact_id(id: &str) -> Result<ArtifactId> {
    ArtifactId::new(id).map_err(|_| ApiError::new("malformed_data"))
}

async fn dispatch(State(server): State<Arc<Server>>, request: Request) -> Response {
    let request_id = request.extensions().get::<RequestId>().cloned();
    match handle(server, request).await {
        Ok(response) => response,
        Err(error) => error_response(error, request_id),
    }
}
async fn handle(server: Arc<Server>, request: Request) -> Result<Response> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    let limits = server.limits();
    let app = &server.application;
    let status = 200;
    if parts.first() == Some(&"api") && parts.get(1) != Some(&"v1") {
        return Err(ApiError::new("unsupported_version"));
    }
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["api", "v1", "capabilities"]) => {
            no_query(&request)?;
            json(&server.capabilities, status, limits.json_bytes)
        }
        ("GET", ["api", "v1", "openapi.json"]) => {
            no_query(&request)?;
            json(&openapi(), status, limits.json_bytes)
        }
        ("POST", ["api", "v1", "models", "review"]) => {
            no_query(&request)?;
            json(
                &app.model_review(body(request, limits).await?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "models", "registrations"]) => {
            no_query(&request)?;
            json(&app.model_registrations().await?, 200, limits.json_bytes)
        }
        ("GET", ["api", "v1", "models", "registrations", id]) => {
            no_query(&request)?;
            let id = ModelRegistrationId::new(*id).map_err(|_| ApiError::new("malformed_data"))?;
            json(&app.model_registration(id).await?, 200, limits.json_bytes)
        }
        ("GET", ["api", "v1", "model-registration-requests", key]) => {
            no_query(&request)?;
            let key = IdempotencyKey::new(*key).map_err(|_| ApiError::new("malformed_data"))?;
            json(
                &app.model_registration_lookup(key).await?,
                200,
                limits.json_bytes,
            )
        }
        ("POST", ["api", "v1", "models", "registrations"]) => {
            no_query(&request)?;
            let key = save_key(&request)?;
            let request: ModelRegistrationRequest = body(request, limits).await?;
            let permit = server
                .mutations
                .clone()
                .try_acquire_owned()
                .map_err(|_| ApiError::new("backend_unavailable"))?;
            let app = server.application.clone();
            let reply = tokio::spawn(async move {
                let _permit = permit;
                app.register_model(key, request).await
            })
            .await
            .map_err(|_| ApiError::new("internal_error"))??;
            json(
                &reply,
                if reply.replayed { 200 } else { 201 },
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "models"]) => {
            no_query(&request)?;
            json(&app.model_inventory().await?, 200, limits.json_bytes)
        }
        ("POST", ["api", "v1", "models", "inspect"]) => {
            no_query(&request)?;
            json(
                &app.inspect_model(body(request, limits).await?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "recipes"]) => json(
            &app.recipes(query(&request, limits)?).await?,
            status,
            limits.json_bytes,
        ),
        ("POST", ["api", "v1", "recipes", id, "preview"]) => {
            no_query(&request)?;
            let recipe = RecipeId::new(*id).map_err(|_| ApiError::new("malformed_data"))?;
            json(
                &app.preview(recipe, body(request, limits).await?).await?,
                status,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "recipes", id, "model-settings"]) => {
            no_query(&request)?;
            json(
                &app.recipe_model_settings(recipe_id(id)?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "harnesses"]) => {
            no_query(&request)?;
            json(&app.harnesses().await?, 200, limits.json_bytes)
        }
        ("GET", ["api", "v1", "recipes", id, "harness"]) => {
            no_query(&request)?;
            json(
                &app.recipe_harness(recipe_id(id)?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "recipes", id, "definition"]) => {
            no_query(&request)?;
            json(
                &app.recipe_definition(recipe_id(id)?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "saved-recipes"]) => json(
            &app.saved_recipes(query(&request, limits)?, None).await?,
            200,
            limits.json_bytes,
        ),
        ("GET", ["api", "v1", "saved-recipes", id]) => {
            no_query(&request)?;
            json(
                &app.saved_recipe(recipe_id(id)?).await?,
                200,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "saved-recipes", id, "revisions"]) => json(
            &app.saved_recipes(query(&request, limits)?, Some(recipe_id(id)?))
                .await?,
            200,
            limits.json_bytes,
        ),
        ("POST", ["api", "v1", "saved-recipes"]) => {
            no_query(&request)?;
            let key = save_key(&request)?;
            let mutation = RecipeMutation::Clone {
                request: body(request, limits).await?,
            };
            save_recipe(server.clone(), key, mutation).await
        }
        ("POST", ["api", "v1", "saved-recipes", id, "revisions"]) => {
            no_query(&request)?;
            let key = save_key(&request)?;
            let mutation = RecipeMutation::Revise {
                recipe_id: recipe_id(id)?,
                request: body(request, limits).await?,
            };
            save_recipe(server.clone(), key, mutation).await
        }
        ("GET", ["api", "v1", "recipe-saves", key]) => {
            no_query(&request)?;
            let key = IdempotencyKey::new(*key).map_err(|_| ApiError::new("malformed_data"))?;
            json(&app.recipe_save_lookup(key).await?, 200, limits.json_bytes)
        }
        ("POST", ["api", "v1", "runs"]) => {
            no_query(&request)?;
            let key = IdempotencyKey::new(
                single_header(request.headers(), "idempotency-key")?
                    .ok_or_else(|| ApiError::new("malformed_data"))?,
            )
            .map_err(|_| ApiError::new("malformed_data"))?;
            let value = body::<LaunchRequest>(request, limits).await?;
            let permit = server
                .mutations
                .clone()
                .try_acquire_owned()
                .map_err(|_| ApiError::new("backend_unavailable"))?;
            let app = app.clone();
            // Dropping the handler/JoinHandle detaches. The application owns the
            // admitted operation, and a bounded permit survives with the task.
            let reply = tokio::spawn(async move {
                let _permit = permit;
                app.admit(key, value).await
            })
            .await
            .map_err(|_| ApiError::new("internal_error"))??;
            json(
                &reply,
                if reply.replayed { 200 } else { 202 },
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "launches", key]) => {
            no_query(&request)?;
            json(
                &app.launch(
                    IdempotencyKey::new(*key).map_err(|_| ApiError::new("malformed_data"))?,
                )
                .await?,
                status,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "runs"]) => json(
            &app.runs(query(&request, limits)?).await?,
            status,
            limits.json_bytes,
        ),
        ("GET", ["api", "v1", "runs", id]) => {
            no_query(&request)?;
            json(&app.run(run_id(id)?).await?, status, limits.json_bytes)
        }
        ("POST", ["api", "v1", "runs", id, "cancel"]) => {
            no_query(&request)?;
            let id = run_id(id)?;
            body::<CancelRequest>(request, limits).await?;
            let permit = server
                .mutations
                .clone()
                .try_acquire_owned()
                .map_err(|_| ApiError::new("backend_unavailable"))?;
            let app = app.clone();
            let reply = tokio::spawn(async move {
                let _permit = permit;
                app.cancel(id).await
            })
            .await
            .map_err(|_| ApiError::new("internal_error"))??;
            json(
                &reply,
                if reply.pending { 202 } else { 200 },
                limits.json_bytes,
            )
        }
        ("POST", ["api", "v1", "runs", id, "release"]) => {
            no_query(&request)?;
            let id = run_id(id)?;
            let value = body::<ReleaseRequest>(request, limits).await?;
            let permit = server
                .mutations
                .clone()
                .try_acquire_owned()
                .map_err(|_| ApiError::new("backend_unavailable"))?;
            let app = app.clone();
            let reply = tokio::spawn(async move {
                let _permit = permit;
                app.release(id, value).await
            })
            .await
            .map_err(|_| ApiError::new("internal_error"))??;
            json(&reply, status, limits.json_bytes)
        }
        ("GET", ["api", "v1", "runs", id, "events"]) => {
            let query = query(&request, limits)?;
            let cursor = decode_cursor(query.cursor.as_deref())?;
            let history = app
                .history(
                    run_id(id)?,
                    cursor,
                    false,
                    query.limit(limits).unwrap(),
                    limits.json_bytes,
                )
                .await?;
            json(&event_page(history, limits)?, status, limits.json_bytes)
        }
        ("GET", ["api", "v1", "runs", id, "events", "stream"]) => {
            let query = query(&request, limits)?;
            if query.limit.is_some() {
                return Err(ApiError::new("malformed_data"));
            }
            let header = single_header(request.headers(), "last-event-id")?;
            if let (Some(query), Some(header)) = (query.cursor.as_deref(), header)
                && query != header
            {
                return Err(ApiError::new("invalid_cursor"));
            }
            let cursor = decode_cursor(query.cursor.as_deref().or(header))?;
            let request_id = request.extensions().get::<RequestId>().cloned();
            stream::events(server.clone(), run_id(id)?, cursor, request_id).await
        }
        ("GET", ["api", "v1", "runs", id, "checkpoints"]) => json(
            &app.checkpoints(run_id(id)?, query(&request, limits)?)
                .await?,
            status,
            limits.json_bytes,
        ),
        ("GET", ["api", "v1", "runs", id, "checkpoints", session]) => {
            no_query(&request)?;
            json(
                &app.checkpoint(
                    run_id(id)?,
                    SessionId::new(*session).map_err(|_| ApiError::new("malformed_data"))?,
                )
                .await?,
                status,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "runs", id, "artifacts"]) => json(
            &app.artifacts(run_id(id)?, query(&request, limits)?).await?,
            status,
            limits.json_bytes,
        ),
        ("GET", ["api", "v1", "runs", id, "artifacts", artifact]) => {
            no_query(&request)?;
            json(
                &app.artifact(run_id(id)?, artifact_id(artifact)?).await?,
                status,
                limits.json_bytes,
            )
        }
        ("GET", ["api", "v1", "runs", id, "artifacts", artifact, "content"]) => {
            no_query(&request)?;
            stream::download(server.clone(), run_id(id)?, artifact_id(artifact)?).await
        }
        _ => Err(ApiError::new("not_found")),
    }
}

fn recipe_id(id: &str) -> Result<RecipeId> {
    RecipeId::new(id).map_err(|_| ApiError::new("malformed_data"))
}
fn save_key(request: &Request) -> Result<IdempotencyKey> {
    IdempotencyKey::new(
        single_header(request.headers(), "idempotency-key")?
            .ok_or_else(|| ApiError::new("malformed_data"))?,
    )
    .map_err(|_| ApiError::new("malformed_data"))
}
async fn save_recipe(
    server: Arc<Server>,
    key: IdempotencyKey,
    request: RecipeMutation,
) -> Result<Response> {
    let permit = server
        .mutations
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new("backend_unavailable"))?;
    let app = server.application.clone();
    let reply = tokio::spawn(async move {
        let _permit = permit;
        app.recipe_save(key, request).await
    })
    .await
    .map_err(|_| ApiError::new("internal_error"))??;
    json(
        &reply,
        if reply.replayed { 200 } else { 201 },
        server.limits().json_bytes,
    )
}
fn decode_cursor(value: Option<&str>) -> Result<Option<EventCursor>> {
    value
        .map(|c| EventCursor::decode(c).map_err(|_| ApiError::new("invalid_cursor")))
        .transpose()
}
fn event_page(history: History, limits: &ApiLimits) -> Result<EventPage> {
    let mut page = EventPage {
        events: vec![],
        frontier: history.frontier.clone(),
        has_more: history.has_more,
        next_cursor: None,
        resume_cursor: EventCursor::new(history.frontier.clone(), history.after_sequence)
            .map_err(|_| ApiError::new("archive_corrupt"))?
            .encode()
            .map_err(|_| ApiError::new("archive_corrupt"))?,
    };
    let mut sequence = history.after_sequence;
    for event in history.events {
        if event.run_id != history.frontier.run_id
            || Some(event.sequence.get()) != sequence.checked_add(1)
            || event.sequence.get() > history.frontier.prefix.sequence
        {
            return Err(ApiError::new("archive_corrupt"));
        }
        bounded_json(&event, limits.event_bytes)?;
        let previous_cursor = page.resume_cursor.clone();
        page.resume_cursor = EventCursor::new(history.frontier.clone(), event.sequence.get())
            .unwrap()
            .encode()
            .unwrap();
        page.next_cursor = Some(page.resume_cursor.clone());
        page.events.push(event);
        if bounded_json(&page, limits.json_bytes).is_err() {
            page.events.pop();
            page.resume_cursor = previous_cursor;
            if page.events.is_empty() {
                return Err(ApiError::new("evidence_limit_exceeded"));
            }
            page.has_more = true;
            break;
        }
        sequence += 1;
    }
    page.has_more |= sequence < history.frontier.prefix.sequence;
    page.next_cursor = page.has_more.then(|| page.resume_cursor.clone());
    if page.has_more && page.events.is_empty() {
        return Err(ApiError::new("archive_corrupt"));
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_safe_details_preserve_the_bounded_error_envelope() {
        let mut error = ApiError::new("backend_unavailable");
        for index in 0..16 {
            error.0.details.insert(
                format!("detail_{index}"),
                serde_json::json!(vec!["a".repeat(128); 32]),
            );
        }
        let request_id = RequestId::new("bounded-error-test").unwrap();
        let response = error_response(error, Some(request_id.clone()));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 16_384).await.unwrap();
        let body: playtime_protocol::v1::ErrorResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.code, "backend_unavailable");
        assert_eq!(body.request_id, Some(request_id));
        assert!(body.details.is_empty());
    }
}
