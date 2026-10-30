//! The production adapter (#20) owns admission, lifecycle and archive trust.
//! Handlers never infer a durable fact from a file, timer, or process status.

use async_trait::async_trait;
use playtime_core::{RunId, SessionId};
use playtime_protocol::{api::*, events::EventEnvelope, v1::ErrorResponse};
use std::pin::Pin;
use tokio::{io::AsyncRead, sync::watch};

pub type Result<T> = std::result::Result<T, ApiError>;

/// Messages and details must be safe for disclosure. Adapter diagnostics with
/// paths, credentials, or experimental payloads belong outside this response.
#[derive(Clone, Debug)]
pub struct ApiError(pub ErrorResponse);
impl ApiError {
    pub fn new(code: &str) -> Self {
        let message = match code {
            "malformed_data" => "Malformed request",
            "invalid_cursor" => "Invalid read cursor",
            "unauthenticated" => "Bearer authentication required",
            "forbidden_origin" => "Browser origins are not supported",
            "forbidden_host" => "A loopback Host header is required",
            "not_found" => "Item not found",
            "unsupported_version" => "Unsupported API or data version",
            "stale_preview" => "Preview revision is no longer current",
            "idempotency_conflict" => "Key already identifies a different request",
            "service_identity_mismatch" => "Service identity does not match",
            "run_busy" => "The data root already has active work",
            "cursor_ahead" => "Cursor is ahead of confirmed evidence",
            "evidence_not_ready" => "Archive identity is not prepared",
            "limit_exceeded" => "Request exceeds the advertised limit",
            "unsupported_media_type" => "Use application/json without content encoding",
            "unsupported_capability" => "Capability is not supported by this backend",
            "invalid_parameters" => "Invalid requested parameters",
            "model_metadata_changed" => {
                "Model metadata changed or could not be verified; refresh and inspect again"
            }
            "model_discovery_busy" => {
                "Model metadata requests are busy; retry after the current reads finish"
            }
            "source_conflict" => "The source definition changed; reload before cloning",
            "head_conflict" => "A newer revision was saved; keep your edits and reload",
            "catalog_full" => "The saved recipe catalog is full",
            "model_registry_full" => "Model registration storage is full",
            "model_registry_corrupt" => "Stored model registrations failed verification",
            "model_registration_busy" => {
                "A model acceptance is pending; look up or retry the same request"
            }
            "catalog_corrupt" => "Saved recipe data failed verification",
            "execution_unavailable" => {
                "Execution is unavailable; saved recipes can still be edited"
            }
            "too_many_observers" => {
                "Observer capacity is exhausted; reconnect with the saved cursor"
            }
            "archive_corrupt" => "Committed archive evidence failed verification",
            "content_mismatch" => "Content failed length or digest verification",
            "ownership_unresolved" => "Live archive ownership has not been established",
            "backend_unavailable" => "Application backend is unavailable",
            "evidence_limit_exceeded" => "Evidence exceeds the advertised transport limit",
            "request_timeout" => "Request deadline exceeded",
            "release_refused" => "The run still owns a worker or resources; see details",
            _ => "Internal service error",
        };
        Self(ErrorResponse {
            code: code.into(),
            message: message.into(),
            request_id: None,
            details: Default::default(),
        })
    }
    pub fn code(&self) -> &str {
        &self.0.code
    }
    pub fn status(&self) -> u16 {
        match self.code() {
            "malformed_data" | "invalid_cursor" => 400,
            "unauthenticated" => 401,
            "forbidden_origin" | "forbidden_host" => 403,
            "not_found" | "unsupported_version" => 404,
            "stale_preview"
            | "idempotency_conflict"
            | "service_identity_mismatch"
            | "run_busy"
            | "cursor_ahead"
            | "evidence_not_ready"
            | "release_refused" => 409,
            "source_conflict" | "head_conflict" | "model_metadata_changed" => 409,
            "catalog_full" | "model_registry_full" => 413,
            "catalog_corrupt" | "execution_unavailable" | "model_registry_corrupt" => 503,
            "limit_exceeded" => 413,
            "unsupported_media_type" => 415,
            "unsupported_capability" | "invalid_parameters" => 422,
            "too_many_observers" | "model_discovery_busy" | "model_registration_busy" => 429,
            "ownership_unresolved" | "backend_unavailable" | "evidence_limit_exceeded" => 503,
            "request_timeout" => 504,
            _ => 500,
        }
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0.code, self.0.message)
    }
}
impl std::error::Error for ApiError {}

pub struct History {
    pub events: Vec<EventEnvelope>,
    pub frontier: Frontier,
    pub after_sequence: u64,
    pub has_more: bool,
}

/// A bounded notification, never a queue of events. Receivers page confirmed
/// evidence after subscribing. Coalescing notifications cannot discard records.
#[derive(Clone, Debug, Default)]
pub struct LiveState {
    pub revision: u64,
    /// Only the lifecycle owner may declare that no more commits can occur.
    pub end: Option<StreamEnd>,
}

/// The adapter verifies reference membership, containment, file type, length
/// and digest before returning this stream. Transport also checks streamed
/// length/digest and clients verify them before accepting their destination.
pub struct Download {
    pub artifact: Artifact,
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
}

#[async_trait]
pub trait Application: Send + Sync + 'static {
    /// Stable identity and cached capabilities; no remote probes here.
    fn capabilities(&self) -> Capabilities;
    async fn inference_targets(&self) -> Result<InferenceTargets> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn model_inventory_on(&self, target: Option<String>) -> Result<ModelInventory> {
        if target.is_some() {
            return Err(ApiError::new("unsupported_capability"));
        }
        self.model_inventory().await
    }
    async fn inspect_model_on(
        &self,
        target: Option<String>,
        request: ModelInspectRequest,
    ) -> Result<ModelInspection> {
        if target.is_some() {
            return Err(ApiError::new("unsupported_capability"));
        }
        self.inspect_model(request).await
    }
    async fn model_review_on(
        &self,
        target: Option<String>,
        request: ModelInspectRequest,
    ) -> Result<ModelRegistrationReview> {
        if target.is_some() {
            return Err(ApiError::new("unsupported_capability"));
        }
        self.model_review(request).await
    }
    async fn model_review(&self, _request: ModelInspectRequest) -> Result<ModelRegistrationReview> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn model_registrations(&self) -> Result<ModelRegistrations> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn model_registration(&self, _id: ModelRegistrationId) -> Result<RegisteredModel> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn register_model(
        &self,
        _key: IdempotencyKey,
        _request: ModelRegistrationRequest,
    ) -> Result<ModelRegistrationResponse> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn model_registration_lookup(
        &self,
        _key: IdempotencyKey,
    ) -> Result<ModelRegistrationResponse> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn model_inventory(&self) -> Result<ModelInventory> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn inspect_model(&self, _request: ModelInspectRequest) -> Result<ModelInspection> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipes(&self, query: PageQuery) -> Result<Page<Recipe>>;
    async fn harnesses(&self) -> Result<HarnessCatalog> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipe_harness(&self, _id: RecipeId) -> Result<RecipeHarnessView> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipe_definition(&self, _id: RecipeId) -> Result<RecipeDefinition> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipe_model_settings(&self, _id: RecipeId) -> Result<RecipeSettingsView> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipe_save(
        &self,
        _key: IdempotencyKey,
        _request: RecipeMutation,
    ) -> Result<RecipeSaveResponse> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn recipe_save_lookup(&self, _key: IdempotencyKey) -> Result<RecipeSaveResponse> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn saved_recipes(
        &self,
        _query: PageQuery,
        _history: Option<RecipeId>,
    ) -> Result<Page<Recipe>> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn saved_recipe(&self, _id: RecipeId) -> Result<SavedRecipe> {
        Err(ApiError::new("unsupported_capability"))
    }
    async fn preview(&self, recipe: RecipeId, request: PreviewRequest) -> Result<Preview>;
    /// Must look up a prior key before today's preview/busy validation. A
    /// returned acceptance is durable in production. Own accepted work beyond
    /// the request lifetime. HTTP timeouts never revoke an admission.
    async fn admit(&self, key: IdempotencyKey, request: LaunchRequest) -> Result<LaunchResponse>;
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse>;
    async fn runs(&self, query: PageQuery) -> Result<Page<RunView>>;
    /// Frontier and both event anchors must come from one consistent read.
    async fn run(&self, run: RunId) -> Result<RunView>;
    /// Save the first intent before acknowledgment; only the lifecycle owner
    /// can change enforcement state or declare the operation terminal.
    async fn cancel(&self, run: RunId) -> Result<CancelResponse>;
    /// Operator release of a non-terminal run. The adapter itself verifies that
    /// the worker stopped and its external resources are absent; the archive is
    /// never modified.
    async fn release(&self, _run: RunId, _request: ReleaseRequest) -> Result<ReleaseResponse> {
        Err(ApiError::new("unsupported_capability"))
    }
    /// Validate run/manifest identity AND the claimed historical prefix against
    /// trusted committed evidence, including old frontiers whose marker was
    /// overwritten. `follow` advances the frontier after validating the cursor;
    /// ordinary history retains exactly its selected prefix. Limit both items
    /// and JSON bytes before returning, with no skipped record.
    async fn history(
        &self,
        run: RunId,
        cursor: Option<EventCursor>,
        follow: bool,
        limit: u32,
        byte_limit: usize,
    ) -> Result<History>;
    /// Register before the initial history read. The sender must remain alive
    /// until explicit reconciliation or service shutdown, even for empty logs.
    async fn subscribe(&self, run: RunId) -> Result<watch::Receiver<LiveState>>;
    async fn checkpoints(&self, run: RunId, query: PageQuery) -> Result<Page<Checkpoint>>;
    async fn checkpoint(&self, run: RunId, session: SessionId) -> Result<Checkpoint>;
    async fn artifacts(&self, run: RunId, query: PageQuery) -> Result<Page<Artifact>>;
    async fn artifact(&self, run: RunId, artifact: ArtifactId) -> Result<Artifact>;
    async fn download(&self, run: RunId, artifact: ArtifactId) -> Result<Download>;
}
