//! Scripted application replies for HTTP conformance. Nothing in this module
//! executes a controller, writes an archive, or establishes production durability.

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use playtime_core::{ArchivePath, RunId, RunMode, SessionId, Sha256Digest, StopReason, UtcTime};
use playtime_protocol::{api::*, archive::ContentRef, events::EventEnvelope, v1::UsageSummary};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Cursor, sync::Mutex};
use tokio::sync::watch;

use crate::application::{ApiError, Application, Download, History, LiveState, Result};

pub fn digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::new(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    )
    .expect("SHA-256")
}
fn reference(path: &str, bytes: &[u8]) -> ContentRef {
    ContentRef {
        path: ArchivePath::new(path).unwrap(),
        sha256: digest(bytes),
        size_bytes: bytes.len() as u64,
    }
}
fn time() -> UtcTime {
    UtcTime::new("2026-10-17T12:00:00Z").unwrap()
}
fn identity() -> BackendIdentity {
    BackendIdentity {
        kind: BackendKind::Fixture,
        description: "Scripted HTTP fixture; no experimental execution or durable admission".into(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub operation: &'static str,
    pub id: String,
}

struct Run {
    view: RunView,
    records: Vec<EventEnvelope>,
    visible: usize,
    live: watch::Sender<LiveState>,
    artifacts: Vec<(Artifact, Vec<u8>)>,
    checkpoint: Checkpoint,
    evidence_ready: bool,
    ownership_resolved: bool,
}
impl Run {
    fn new(id: RunId, request: &LaunchRequest) -> Self {
        let manifest = reference("inputs/manifest.json", b"synthetic HTTP fixture manifest\n");
        let (live, _) = watch::channel(LiveState::default());
        let session = SessionId::new("s1").unwrap();
        let content = [
            (
                "incoming-memory",
                "incoming.txt",
                "incoming_memory",
                b"fixture incoming memory\n".as_slice(),
            ),
            (
                "outgoing-memory",
                "memento.txt",
                "outgoing_memory",
                b"fixture saved memory\n".as_slice(),
            ),
            (
                "evidence",
                "evidence.txt",
                "controller_evidence",
                b"fixture evidence, observation 7\n".as_slice(),
            ),
            (
                "workspace",
                "workspace.json",
                "workspace",
                b"{\"synthetic\":true}\n".as_slice(),
            ),
            (
                "saved-image",
                "image.png",
                "file_version",
                include_bytes!("../tests/fixtures/pixel.png").as_slice(),
            ),
        ];
        let mut artifacts: Vec<_> = content
            .into_iter()
            .map(|(artifact_id, name, kind, bytes)| {
                (
                    Artifact {
                        artifact_id: ArtifactId::new(artifact_id).unwrap(),
                        name: name.into(),
                        kind: kind.into(),
                        content: reference(&format!("objects/{name}"), bytes),
                        session_id: Some(session.clone()),
                        published_sequence: 3,
                        frozen_input: false,
                    },
                    bytes.to_vec(),
                )
            })
            .collect();
        let available = |n: usize| ContentAvailability::Available {
            artifact_id: artifacts[n].0.artifact_id.clone(),
            content: artifacts[n].0.content.clone(),
        };
        let checkpoint = Checkpoint {
            index: None,
            checkpoint: CheckpointSummary {
                session_id: session,
                published_sequence: 3,
            },
            incoming_memory: available(0),
            outgoing_memory: available(1),
            evidence: available(2),
            workspace: available(3),
            files: vec![
                FileVersion {
                    name: "saved <image>.png".into(),
                    kind: "regular".into(),
                    content: available(4),
                },
                FileVersion {
                    name: "unretained.txt".into(),
                    kind: "regular".into(),
                    content: ContentAvailability::Unavailable {
                        reason: "not retained in this fixture".into(),
                    },
                },
            ],
        };
        let prompt = b"Scripted fixture prompt; no model is invoked.\n";
        artifacts.push((
            Artifact {
                artifact_id: ArtifactId::new("frozen-prompt").unwrap(),
                name: "prompt.txt".into(),
                kind: "prompt".into(),
                content: reference("inputs/prompt.txt", prompt),
                session_id: None,
                published_sequence: 0,
                frozen_input: true,
            },
            prompt.to_vec(),
        ));
        // Deliberately synthetic, not #42's observation wrapper. Only this
        // explicit ContentRef is catalogued; path-shaped payload strings are data.
        let payload = br#"{"synthetic":true,"text":"fixture retained payload","unknown":{"path":"/not/a/fetch/permission"}}"#;
        let payload_ref = reference("observations/fixture.json", payload);
        artifacts.push((
            Artifact {
                artifact_id: ArtifactId::new("observation-payload").unwrap(),
                name: "fixture.json".into(),
                kind: "observation_payload".into(),
                content: payload_ref.clone(),
                session_id: Some(SessionId::new("s1").unwrap()),
                published_sequence: 2,
                frozen_input: false,
            },
            payload.to_vec(),
        ));
        let raw = [
            (
                "run_started",
                serde_json::json!({"mode":"simulation", "input_manifest": manifest}),
            ),
            (
                "future_fixture_observation",
                serde_json::json!({"text":"scripted output", "unknown_field":{"retained":true}, "synthetic_payload_ref":payload_ref}),
            ),
            (
                "fixture_checkpoint_published",
                serde_json::json!({"session_id":"s1", "provenance":"fixture"}),
            ),
            (
                "run_finished",
                serde_json::json!({"reason":"agent_finished", "usage":{"known_generated_tokens":0,"unknown_reserved_tokens":100,"unknown_requests":1,"budget_charged_tokens":100,"usage_complete":false},"cleanup":{"status":"unknown"},"last_checkpoint":null}),
            ),
        ];
        let mut records: Vec<_> = raw
            .into_iter()
            .enumerate()
            .map(|(i, (kind, data))| EventEnvelope {
                schema_version: 1,
                run_id: id.clone(),
                sequence: ((i + 1) as u64).try_into().unwrap(),
                time: time(),
                kind: kind.into(),
                data,
            })
            .collect();
        if request.recipe_id.as_str() == "fixture-empty" {
            records.clear();
        }
        let view = RunView {
            accounting_coverage: None,
            run_id: id,
            mode: request.mode,
            accepted_at: time(),
            updated_at: time(),
            recipe_id: request.recipe_id.clone(),
            preview_revision: request.preview_revision.clone(),
            backend: identity(),
            orchestration: Orchestration {
                state: OrchestrationState::Running,
                reason: None,
            },
            controller_phase: None,
            outcome: None,
            usage: None,
            checkpoint: None,
            cancellation: None,
            cleanup: Cleanup::Unknown,
            evidence: EvidenceAvailability::Available,
            input_manifest: Some(manifest),
            worker: None,
            last_confirmed_frontier: None,
            event_anchors: None,
            contention: None,
        };
        Self {
            view,
            records,
            visible: 0,
            live,
            artifacts,
            checkpoint,
            evidence_ready: true,
            ownership_resolved: true,
        }
    }
    fn frontier(&self, sequence: usize) -> Frontier {
        let mut hasher = Sha256::new();
        let mut byte_offset = 0;
        for event in &self.records[..sequence] {
            let mut bytes = serde_json::to_vec(event).unwrap();
            bytes.push(b'\n');
            byte_offset += bytes.len() as u64;
            hasher.update(bytes);
        }
        Frontier {
            run_id: self.view.run_id.clone(),
            manifest_sha256: self.view.input_manifest.as_ref().unwrap().sha256.clone(),
            prefix: Prefix {
                sequence: sequence as u64,
                byte_offset,
                sha256: Sha256Digest::new(
                    hasher
                        .finalize()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                )
                .unwrap(),
            },
        }
    }
    fn validate(&self, cursor: &EventCursor) -> Result<()> {
        if cursor.frontier.run_id != self.view.run_id
            || cursor.frontier.manifest_sha256 != self.view.input_manifest.as_ref().unwrap().sha256
        {
            return Err(ApiError::new("invalid_cursor"));
        }
        if cursor.frontier.prefix.sequence > self.visible as u64 {
            return Err(ApiError::new("cursor_ahead"));
        }
        if cursor.frontier != self.frontier(cursor.frontier.prefix.sequence as usize) {
            return Err(ApiError::new("invalid_cursor"));
        }
        Ok(())
    }
    fn readable(&self, cursor: Option<&EventCursor>) -> Result<()> {
        if !self.evidence_ready {
            return Err(ApiError::new("evidence_not_ready"));
        }
        if let Some(cursor) = cursor {
            self.validate(cursor)?;
        }
        // A previously confirmed requested prefix remains serveable, even if
        // the fixture scripts loss of ownership of newer commits.
        if !self.ownership_resolved && cursor.is_none() {
            return Err(ApiError::new("ownership_unresolved"));
        }
        Ok(())
    }
    fn view(&self) -> RunView {
        let mut view = self.view.clone();
        if !self.evidence_ready {
            view.evidence = EvidenceAvailability::Unavailable {
                reason: "fixture preparation pending".into(),
            };
            return view;
        }
        let frontier = self.frontier(self.visible);
        view.last_confirmed_frontier = Some(frontier.clone());
        view.event_anchors = Some(EventAnchors::new(frontier).unwrap());
        if !self.ownership_resolved {
            view.evidence = EvidenceAvailability::OwnershipUnresolved {
                reason: "scripted ownership uncertainty".into(),
            };
        }
        if self.visible >= 3 {
            view.checkpoint = Some(self.checkpoint.checkpoint.clone());
        }
        if !self.records.is_empty() && self.visible == self.records.len() {
            view.outcome = Some(ExperimentalOutcome {
                reason: StopReason::AgentFinished,
                event_sequence: 4,
            });
            view.usage = Some(UsageSummary {
                known_generated_tokens: 0,
                unknown_reserved_tokens: 100,
                unknown_requests: 1,
                budget_charged_tokens: 100,
                usage_complete: false,
            });
        }
        view
    }
}

struct State {
    revision: PreviewRevision,
    runs: BTreeMap<RunId, Run>,
    launches: BTreeMap<IdempotencyKey, (LaunchRequest, RunId)>,
    calls: Vec<Call>,
    next_run: u64,
}

pub struct Fixture {
    catalog: Option<crate::catalog::Catalog>,
    models: Option<std::sync::Arc<crate::models::ModelBrowser>>,
    registrations: Option<crate::registrations::Registry>,
    service: ServiceId,
    limits: ApiLimits,
    state: Mutex<State>,
}
impl Fixture {
    pub fn new(service: ServiceId) -> Self {
        Self::with_limits(service, ApiLimits::default())
    }
    pub fn with_limits(service: ServiceId, limits: ApiLimits) -> Self {
        Self {
            catalog: None,
            models: None,
            registrations: None,
            service,
            limits,
            state: Mutex::new(State {
                revision: PreviewRevision::new("fixture-v1").unwrap(),
                runs: BTreeMap::new(),
                launches: BTreeMap::new(),
                calls: vec![],
                next_run: 1,
            }),
        }
    }
    /// Actual catalog persistence behind explicitly non-executing HTTP fixtures.
    pub fn with_catalog(service: ServiceId, catalog: crate::catalog::Catalog) -> Self {
        let mut fixture = Self::new(service);
        fixture.catalog = Some(catalog);
        fixture
    }
    /// Connect metadata reads to an explicitly supplied fake model server.
    pub fn with_model_browser(mut self, browser: crate::models::ModelBrowser) -> Self {
        self.models = Some(std::sync::Arc::new(browser));
        self
    }
    pub fn with_model_registry(mut self, registry: crate::registrations::Registry) -> Self {
        self.models = registry.browser();
        self.registrations = Some(registry);
        self
    }
    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }
    pub fn change_revision(&self, value: PreviewRevision) {
        self.state.lock().unwrap().revision = value;
    }
    pub fn set_evidence(&self, run: &RunId, ready: bool, owned: bool) {
        let mut state = self.state.lock().unwrap();
        let run = state.runs.get_mut(run).unwrap();
        run.evidence_ready = ready;
        run.ownership_resolved = owned;
    }
    /// Reveal the next canned records. This is an explicit test action, not
    /// execution or a storage implementation. A terminal notice is independent.
    pub fn reveal(&self, run: &RunId, through: usize, end: Option<StreamEndStatus>) {
        let mut state = self.state.lock().unwrap();
        let run = state.runs.get_mut(run).unwrap();
        assert!(through >= run.visible && through <= run.records.len());
        run.visible = through;
        let end = end.map(|status| StreamEnd {
            status,
            frontier: run.frontier(through),
        });
        if end.is_some() {
            run.view.orchestration.state = OrchestrationState::Terminal;
        }
        run.live.send_modify(|live| {
            live.revision += 1;
            live.end = end;
        });
    }
    /// Test-only script editing before publication; already visible bytes are immutable.
    pub fn replace_pending_record(&self, run: &RunId, index: usize, data: serde_json::Value) {
        let mut state = self.state.lock().unwrap();
        let run = state.runs.get_mut(run).unwrap();
        assert!(index >= run.visible);
        run.records[index].data = data;
    }
    fn record(state: &mut State, operation: &'static str, id: impl ToString) {
        state.calls.push(Call {
            operation,
            id: id.to_string(),
        });
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCursor {
    version: u32,
    kind: String,
    scope: String,
    after: String,
    cutoff: String,
    frontier: Option<Frontier>,
}
fn list_cursor(query: &PageQuery, kind: &str, scope: &str) -> Result<Option<ListCursor>> {
    query
        .cursor
        .as_ref()
        .map(|cursor| {
            if cursor.len() > 2048 {
                return Err(ApiError::new("invalid_cursor"));
            }
            let decoded = URL_SAFE_NO_PAD
                .decode(cursor)
                .map_err(|_| ApiError::new("invalid_cursor"))?;
            let value: ListCursor =
                serde_json::from_slice(&decoded).map_err(|_| ApiError::new("invalid_cursor"))?;
            if value.version != 1
                || value.kind != kind
                || value.scope != scope
                || value.after.len() > 256
                || value.cutoff.len() > 256
            {
                return Err(ApiError::new("invalid_cursor"));
            }
            Ok(value)
        })
        .transpose()
}
fn page<T: Serialize>(
    items: Vec<(String, T)>,
    query: &PageQuery,
    mut cursor: ListCursor,
    limits: &ApiLimits,
) -> Result<Page<T>> {
    let limit = query
        .limit(limits)
        .map_err(|_| ApiError::new("malformed_data"))? as usize;
    let mut selected = vec![];
    let mut bytes = 1600; // Bounded cursor/frontier envelope; final transport checks actual bytes.
    let mut more = false;
    for (key, item) in items {
        let size = serde_json::to_vec(&item)
            .map_err(|_| ApiError::new("internal_error"))?
            .len()
            + 1;
        if selected.len() == limit || bytes + size > limits.json_bytes {
            if selected.is_empty() {
                return Err(ApiError::new("evidence_limit_exceeded"));
            }
            more = true;
            break;
        }
        bytes += size;
        cursor.after = key;
        selected.push(item);
    }
    let next_cursor = more.then(|| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap()));
    Ok(Page {
        items: selected,
        next_cursor,
        frontier: cursor.frontier,
    })
}

#[async_trait]
impl Application for Fixture {
    async fn model_review(&self, request: ModelInspectRequest) -> Result<ModelRegistrationReview> {
        self.registrations
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .review(request)
            .await
    }
    async fn model_registrations(&self) -> Result<ModelRegistrations> {
        self.registrations
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .list()
            .await
    }
    async fn model_registration(&self, id: ModelRegistrationId) -> Result<RegisteredModel> {
        self.registrations
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .get(id)
            .await
    }
    async fn register_model(
        &self,
        key: IdempotencyKey,
        request: ModelRegistrationRequest,
    ) -> Result<ModelRegistrationResponse> {
        self.registrations
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .register(key, request)
            .await
    }
    async fn model_registration_lookup(
        &self,
        key: IdempotencyKey,
    ) -> Result<ModelRegistrationResponse> {
        self.registrations
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .lookup(key)
            .await
    }

    async fn model_inventory(&self) -> Result<ModelInventory> {
        self.models
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .inventory()
            .await
    }
    async fn inspect_model(&self, request: ModelInspectRequest) -> Result<ModelInspection> {
        self.models
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .inspect(request)
            .await
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            api_versions: vec![1],
            archive_versions: vec![1],
            event_versions: vec![1],
            service_id: self.service.clone(),
            service_build: env!("CARGO_PKG_VERSION").into(),
            backend: identity(),
            modes: vec![RunMode::Simulation],
            features: [
                self.models.as_ref().map(|_| MODEL_DISCOVERY.into()),
                self.registrations
                    .as_ref()
                    .map(|_| MODEL_REGISTRATION.into()),
                Some("scripted_fixture".into()),
                Some("events".into()),
                Some("downloads".into()),
                self.catalog.as_ref().map(|_| RECIPE_AUTHORING.into()),
                self.catalog.as_ref().map(|_| MODEL_SETTINGS.into()),
                self.catalog.as_ref().map(|_| RECIPE_MODEL_SELECTION.into()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            unsupported_adapters: vec![
                "controller".into(),
                "model".into(),
                "workshop".into(),
                "systemd".into(),
            ],
            limits: self.limits.clone(),
        }
    }
    async fn recipe_model_settings(&self, id: RecipeId) -> Result<RecipeSettingsView> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .model_settings(&id)
            .await
    }
    async fn recipe_definition(&self, id: RecipeId) -> Result<RecipeDefinition> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .definition(&id)
            .await
    }
    async fn recipe_save(
        &self,
        key: IdempotencyKey,
        request: RecipeMutation,
    ) -> Result<RecipeSaveResponse> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .save(key, request)
            .await
    }
    async fn recipe_save_lookup(&self, key: IdempotencyKey) -> Result<RecipeSaveResponse> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .lookup(&key)
            .await
    }
    async fn saved_recipes(
        &self,
        query: PageQuery,
        history: Option<RecipeId>,
    ) -> Result<Page<Recipe>> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .page(query, history, None)
            .await
    }
    async fn saved_recipe(&self, id: RecipeId) -> Result<SavedRecipe> {
        self.catalog
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .head(&id)
            .await
    }
    async fn recipes(&self, query: PageQuery) -> Result<Page<Recipe>> {
        if let Some(catalog) = &self.catalog {
            return catalog.page(query, None, Some(vec![])).await;
        }
        let cursor = list_cursor(&query, "recipes", self.service.as_str())?.unwrap_or(ListCursor {
            version: 1,
            kind: "recipes".into(),
            scope: self.service.to_string(),
            after: String::new(),
            cutoff: String::new(),
            frontier: None,
        });
        let items = ["fixture-empty", "fixture-example"]
            .into_iter()
            .filter(|id| *id > cursor.after.as_str())
            .map(|id| {
                (
                    id.into(),
                    Recipe {
                        saved: None,
                        recipe_id: RecipeId::new(id).unwrap(),
                        name: format!("Scripted {id}"),
                        modes: vec![RunMode::Simulation],
                    },
                )
            })
            .collect();
        page(items, &query, cursor, &self.limits)
    }
    async fn preview(&self, recipe: RecipeId, request: PreviewRequest) -> Result<Preview> {
        if let Some(catalog) = &self.catalog
            && request.mode == RunMode::Experiment
        {
            catalog.definition(&recipe).await?;
            return Err(ApiError::new("execution_unavailable"));
        }
        if !matches!(recipe.as_str(), "fixture-empty" | "fixture-example") {
            return Err(ApiError::new("not_found"));
        }
        if request.mode != RunMode::Simulation {
            return Err(ApiError::new("unsupported_capability"));
        }
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "preview", &recipe);
        let config: serde_json::Value = serde_json::from_str(include_str!(
            "../../playtime-protocol/tests/fixtures/two-session/inputs/configuration.json"
        ))
        .unwrap();
        Ok(Preview {
            recipe_id: recipe,
            mode: request.mode,
            preview_revision: state.revision.clone(),
            parameters: serde_json::from_value(config.clone()).unwrap(),
            effective_configuration: config,
            prompt: "Scripted fixture prompt; no model is invoked.".into(),
            protocol: "Scripted fixture protocol.".into(),
            model_identity: serde_json::json!({"kind":"fixture"}),
            workshop_identity: serde_json::json!({"kind":"fixture"}),
            seeds: SeedSummary {
                files: 0,
                bytes: 0,
                digests: vec![],
            },
            readiness: Readiness {
                ready: true,
                reasons: vec![],
            },
            warnings: vec!["Scripted replies do not establish real execution or durability".into()],
            backend: identity(),
        })
    }
    async fn admit(&self, key: IdempotencyKey, request: LaunchRequest) -> Result<LaunchResponse> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "admit", &key);
        if let Some((original, run)) = state.launches.get(&key) {
            if *original != request {
                return Err(ApiError::new("idempotency_conflict"));
            }
            let run = state.runs[run].view();
            return Ok(LaunchResponse {
                run_id: run.run_id.clone(),
                accepted_at: run.accepted_at.clone(),
                replayed: true,
                run,
            });
        }
        if !matches!(
            request.recipe_id.as_str(),
            "fixture-empty" | "fixture-example"
        ) {
            return Err(ApiError::new("not_found"));
        }
        if request.mode != RunMode::Simulation {
            return Err(ApiError::new("unsupported_capability"));
        }
        if request.preview_revision != state.revision {
            return Err(ApiError::new("stale_preview"));
        }
        if state
            .runs
            .values()
            .any(|r| r.view.orchestration.state != OrchestrationState::Terminal)
        {
            return Err(ApiError::new("run_busy"));
        }
        let id = RunId::new(format!("fixture-run-{:08}", state.next_run)).unwrap();
        state.next_run += 1;
        let run = Run::new(id.clone(), &request);
        let view = run.view();
        state.runs.insert(id.clone(), run);
        state.launches.insert(key, (request, id.clone()));
        Ok(LaunchResponse {
            run_id: id,
            accepted_at: view.accepted_at.clone(),
            replayed: false,
            run: view,
        })
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "launch", &key);
        let (_, run) = state
            .launches
            .get(&key)
            .ok_or_else(|| ApiError::new("not_found"))?;
        let run = state.runs[run].view();
        Ok(LaunchResponse {
            run_id: run.run_id.clone(),
            accepted_at: run.accepted_at.clone(),
            replayed: true,
            run,
        })
    }
    async fn runs(&self, query: PageQuery) -> Result<Page<RunView>> {
        let state = self.state.lock().unwrap();
        let key = |r: &Run| format!("{}|{}", r.view.accepted_at, r.view.run_id);
        let mut items: Vec<_> = state.runs.values().map(|r| (key(r), r.view())).collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        let cursor = list_cursor(&query, "runs", self.service.as_str())?.unwrap_or(ListCursor {
            version: 1,
            kind: "runs".into(),
            scope: self.service.to_string(),
            after: String::new(),
            cutoff: items.first().map(|i| i.0.clone()).unwrap_or_default(),
            frontier: None,
        });
        items
            .retain(|(k, _)| *k <= cursor.cutoff && (cursor.after.is_empty() || *k < cursor.after));
        page(items, &query, cursor, &self.limits)
    }
    async fn run(&self, run: RunId) -> Result<RunView> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "run", &run);
        Ok(state
            .runs
            .get(&run)
            .ok_or_else(|| ApiError::new("not_found"))?
            .view())
    }
    async fn cancel(&self, run: RunId) -> Result<CancelResponse> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "cancel", &run);
        let run = state
            .runs
            .get_mut(&run)
            .ok_or_else(|| ApiError::new("not_found"))?;
        let pending = run.view.orchestration.state != OrchestrationState::Terminal;
        if pending && run.view.cancellation.is_none() {
            run.view.cancellation = Some(Cancellation {
                requested_at: time(),
                enforcement: Enforcement::Uncertain,
                stop_accepted_at: None,
                grace_deadline: None,
                uncertainty: Some("Scripted supervisor unavailable".into()),
            });
            run.view.orchestration.state = OrchestrationState::Cancelling;
        }
        Ok(CancelResponse {
            pending,
            intent: run.view.cancellation.clone(),
            run: run.view(),
        })
    }
    async fn history(
        &self,
        id: RunId,
        cursor: Option<EventCursor>,
        follow: bool,
        limit: u32,
        byte_limit: usize,
    ) -> Result<History> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "history", &id);
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        run.readable(cursor.as_ref())?;
        if follow && !run.ownership_resolved {
            return Err(ApiError::new("ownership_unresolved"));
        }
        let after = cursor.as_ref().map_or(0, |c| c.after_sequence);
        let through = if follow {
            run.visible
        } else {
            cursor
                .as_ref()
                .map_or(run.visible, |c| c.frontier.prefix.sequence as usize)
        };
        let mut events = vec![];
        let mut bytes = 0;
        for event in run.records[after as usize..through]
            .iter()
            .take(limit as usize)
        {
            let size = serde_json::to_vec(event).unwrap().len() + 1;
            if bytes + size > byte_limit {
                if events.is_empty() {
                    return Err(ApiError::new("evidence_limit_exceeded"));
                }
                break;
            }
            events.push(event.clone());
            bytes += size;
        }
        let has_more = after + (events.len() as u64) < through as u64;
        Ok(History {
            events,
            frontier: run.frontier(through),
            after_sequence: after,
            has_more,
        })
    }
    async fn subscribe(&self, run: RunId) -> Result<watch::Receiver<LiveState>> {
        let mut state = self.state.lock().unwrap();
        Self::record(&mut state, "subscribe", &run);
        let run = state
            .runs
            .get(&run)
            .ok_or_else(|| ApiError::new("not_found"))?;
        if !run.evidence_ready {
            return Err(ApiError::new("evidence_not_ready"));
        }
        Ok(run.live.subscribe())
    }
    async fn checkpoints(&self, id: RunId, query: PageQuery) -> Result<Page<Checkpoint>> {
        let state = self.state.lock().unwrap();
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        let cursor = content_cursor(run, &query, "checkpoints")?;
        let items =
            if cursor.frontier.as_ref().unwrap().prefix.sequence >= 3 && cursor.after.is_empty() {
                vec![("s1".into(), run.checkpoint.clone())]
            } else {
                vec![]
            };
        page(items, &query, cursor, &self.limits)
    }
    async fn checkpoint(&self, id: RunId, session: SessionId) -> Result<Checkpoint> {
        let state = self.state.lock().unwrap();
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        run.readable(None)?;
        if run.visible < 3 || session != run.checkpoint.checkpoint.session_id {
            return Err(ApiError::new("not_found"));
        }
        Ok(run.checkpoint.clone())
    }
    async fn artifacts(&self, id: RunId, query: PageQuery) -> Result<Page<Artifact>> {
        let state = self.state.lock().unwrap();
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        let cursor = content_cursor(run, &query, "artifacts")?;
        let through = cursor.frontier.as_ref().unwrap().prefix.sequence;
        let mut items: Vec<_> = run
            .artifacts
            .iter()
            .filter(|(a, _)| {
                a.published_sequence <= through && a.artifact_id.as_str() > cursor.after.as_str()
            })
            .map(|(a, _)| (a.artifact_id.to_string(), a.clone()))
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        page(items, &query, cursor, &self.limits)
    }
    async fn artifact(&self, id: RunId, artifact: ArtifactId) -> Result<Artifact> {
        let state = self.state.lock().unwrap();
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        run.readable(None)?;
        run.artifacts
            .iter()
            .find(|(a, _)| a.artifact_id == artifact && a.published_sequence <= run.visible as u64)
            .map(|(a, _)| a.clone())
            .ok_or_else(|| ApiError::new("not_found"))
    }
    async fn download(&self, id: RunId, artifact: ArtifactId) -> Result<Download> {
        let state = self.state.lock().unwrap();
        let run = state
            .runs
            .get(&id)
            .ok_or_else(|| ApiError::new("not_found"))?;
        run.readable(None)?;
        let (artifact, bytes) = run
            .artifacts
            .iter()
            .find(|(a, _)| a.artifact_id == artifact && a.published_sequence <= run.visible as u64)
            .ok_or_else(|| ApiError::new("not_found"))?;
        if artifact.content.size_bytes != bytes.len() as u64
            || artifact.content.sha256 != digest(bytes)
        {
            return Err(ApiError::new("content_mismatch"));
        }
        Ok(Download {
            artifact: artifact.clone(),
            reader: Box::pin(Cursor::new(bytes.clone())),
        })
    }
}
fn content_cursor(run: &Run, query: &PageQuery, kind: &str) -> Result<ListCursor> {
    let cursor = list_cursor(query, kind, run.view.run_id.as_str())?;
    if let Some(cursor) = cursor {
        let position = EventCursor::new(
            cursor
                .frontier
                .clone()
                .ok_or_else(|| ApiError::new("invalid_cursor"))?,
            0,
        )
        .map_err(|_| ApiError::new("invalid_cursor"))?;
        run.readable(Some(&position))?;
        Ok(cursor)
    } else {
        run.readable(None)?;
        Ok(ListCursor {
            version: 1,
            kind: kind.into(),
            scope: run.view.run_id.to_string(),
            after: String::new(),
            cutoff: String::new(),
            frontier: Some(run.frontier(run.visible)),
        })
    }
}
