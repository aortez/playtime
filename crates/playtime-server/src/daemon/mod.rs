//! Production application: ordinary HTTP routes over the admission coordinator,
//! detached packaged worker and verified archive queries. No fixture responses.
mod archive;
pub mod catalog;
mod paging;
mod projection;
pub mod test_supervisor;

use crate::{
    application::{ApiError, Application, Download, History, LiveState, Result},
    lifecycle::{
        self as life, Admission, Coordinator, Ledger, Phase,
        ports::{Supervisor, SystemClock},
    },
};
use archive::Archive;
use async_trait::async_trait;
use playtime_core::{RunId, RunMode, SessionId};
use playtime_protocol::{api::*, worker::WorkerState};
use playtime_runner::ArchiveQueries;
use playtime_storage::{ArchiveReader, ReadPrefix, StorageLimits};
use playtime_worker::bundle::VerifiedBundle;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::sync::{RwLock, Semaphore, watch};

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<Inner>,
}
struct Inner {
    archive: Arc<Archive>,
    coordinator: Coordinator,
    published: RwLock<BTreeMap<RunId, Published>>,
    reads: Arc<Semaphore>,
    test_supervision: bool,
    catalog: crate::catalog::Catalog,
    models: Option<Arc<crate::models::ModelBrowser>>,
    target_browsers: BTreeMap<String, Arc<crate::models::ModelBrowser>>,
    registrations: crate::registrations::Registry,
}
#[derive(Clone)]
struct Published {
    record: Admission,
    evidence: Option<Arc<projection::Evidence>>,
    live: watch::Sender<LiveState>,
    error: bool,
}
impl Daemon {
    pub async fn open(
        root: &Path,
        bundle: &Path,
        supervisor: Arc<dyn Supervisor>,
        test_supervision: bool,
    ) -> life::Result<Self> {
        Self::open_with_docker(
            root,
            bundle,
            supervisor,
            test_supervision,
            None,
            None,
            vec![],
        )
        .await
    }
    /// `live` names the Ollama endpoint for live runs, which also need `docker`.
    /// Catalog `recipes` run on the live model.
    pub async fn open_with_docker(
        root: &Path,
        bundle: &Path,
        supervisor: Arc<dyn Supervisor>,
        test_supervision: bool,
        docker: Option<(&Path, &Path)>,
        live: Option<&str>,
        recipes: Vec<playtime_runner::docker::Recipe>,
    ) -> life::Result<Self> {
        Self::open_with_targets(
            root,
            bundle,
            supervisor,
            test_supervision,
            docker,
            live,
            recipes,
            InferenceTargets {
                schema_version: 1,
                targets: vec![],
            },
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn open_with_targets(
        root: &Path,
        bundle: &Path,
        supervisor: Arc<dyn Supervisor>,
        test_supervision: bool,
        docker: Option<(&Path, &Path)>,
        live: Option<&str>,
        recipes: Vec<playtime_runner::docker::Recipe>,
        targets: InferenceTargets,
    ) -> life::Result<Self> {
        crate::targets::validate(&targets).map_err(life::Error::Invalid)?;
        if !targets.targets.is_empty() && (live.is_some() || docker.is_none()) {
            return Err(life::Error::Invalid(
                "named targets require Docker and replace --ollama-endpoint",
            ));
        }
        if test_supervision && (docker.is_some() || live.is_some() || !targets.targets.is_empty()) {
            return Err(life::Error::Invalid(
                "CPU test supervisor rejects real Docker effects",
            ));
        }
        let ledger = Ledger::open(root).await?;
        let root = ledger.root().to_path_buf();
        let bundle = bundle.to_owned();
        let bundle = tokio::task::spawn_blocking(move || {
            VerifiedBundle::open(&bundle)
                .map_err(|_| life::Error::Invalid("invalid frozen worker bundle"))
        })
        .await
        .map_err(|_| life::Error::Uncertain)??;
        let mut archive = Archive::new(root, bundle)?;
        let catalog = crate::catalog::Catalog::new(ledger.clone(), recipes.clone())
            .map_err(|_| life::Error::Invalid("invalid recipe catalog"))?;
        catalog
            .check()
            .await
            .map_err(|_| life::Error::Invalid("damaged recipe catalog"))?;
        archive.authoring = Some(catalog.clone());
        archive.docker_options = docker.map(|(e, s)| (e.to_path_buf(), s.to_path_buf()));
        archive.live_endpoint = live.map(str::to_owned);
        archive.enable_recipes(recipes)?;
        if let Some((executable, socket)) = docker {
            tolerate_unavailable(archive.enable_docker(executable, socket).await)?;
        }
        if let Some(endpoint) = live {
            tolerate_unavailable(archive.enable_live(endpoint).await)?;
        }
        for target in targets.targets {
            archive.add_target(target).await?;
        }
        let archive = Arc::new(archive);
        let models = live
            .map(crate::models::ModelBrowser::new)
            .transpose()
            .map_err(|_| life::Error::Invalid("invalid discovery endpoint"))?
            .map(Arc::new);
        let target_browsers = archive
            .targets
            .iter()
            .map(|(id, (target, _))| {
                crate::models::ModelBrowser::new(&target.endpoint)
                    .map(|b| (id.clone(), Arc::new(b)))
            })
            .collect::<Result<BTreeMap<_, _>>>()
            .map_err(|_| life::Error::Invalid("invalid discovery endpoint"))?;
        let registrations = crate::registrations::Registry::new(ledger.clone(), models.clone());
        registrations.check().await.map_err(|error| {
            life::Error::Invalid(if error.code() == "model_registry_corrupt" {
                "damaged model registry"
            } else {
                "model registry unavailable"
            })
        })?;
        let coordinator =
            Coordinator::new(ledger, archive.clone(), supervisor, Arc::new(SystemClock));
        let this = Self {
            inner: Arc::new(Inner {
                archive,
                coordinator,
                published: RwLock::new(BTreeMap::new()),
                reads: Arc::new(Semaphore::new(4)),
                test_supervision,
                models,
                target_browsers,
                registrations,
                catalog,
            }),
        };
        for record in this.inner.coordinator.ledger().list().await? {
            // A saved boundary is historical confirmation, never a fresh claim
            // of worker liveness. Active restart/recovery remains a later slice.
            this.publish(record)
                .await
                .map_err(|_| life::Error::Uncertain)?;
        }
        Ok(this)
    }
    fn browser_on(&self, target: Option<&str>) -> Result<Arc<crate::models::ModelBrowser>> {
        match target {
            Some(id) => self
                .inner
                .target_browsers
                .get(id)
                .cloned()
                .ok_or_else(|| ApiError::new("invalid_parameters")),
            None => self
                .inner
                .models
                .clone()
                .ok_or_else(|| ApiError::new("backend_unavailable")),
        }
    }
    pub fn coordinator(&self) -> &Coordinator {
        &self.inner.coordinator
    }
    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .inner
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::new("backend_unavailable"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation()
        })
        .await
        .map_err(|_| ApiError::new("backend_unavailable"))?
    }
    async fn publish(&self, record: Admission) -> Result<()> {
        let old = self
            .inner
            .published
            .read()
            .await
            .get(&record.run_id)
            .cloned();
        if old.as_ref().is_some_and(|old| {
            old.record == record
                && !old.error
                && old.evidence.as_ref().map(|e| e.snapshot.frontier())
                    == record.confirmed_read.as_ref().map(|c| &c.frontier)
        }) {
            return Ok(());
        }
        let (archive, saved) = (self.inner.archive.clone(), record.clone());
        let projected = if record.confirmed_read.is_none() {
            Ok(None)
        } else {
            self.blocking(move || {
                archive
                    .saved_snapshot(&saved)
                    .map_err(projection::evidence)?
                    .map(|(reader, snapshot)| {
                        let query = ArchiveQueries::from_snapshot(reader, snapshot)
                            .map_err(projection::evidence)?;
                        projection::project(&saved, &query).map(Arc::new)
                    })
                    .transpose()
            })
            .await
        };
        let mut published = self.inner.published.write().await;
        if published
            .get(&record.run_id)
            .is_some_and(|old| old.record.revision > record.revision)
        {
            return Ok(());
        }
        let live = published
            .get(&record.run_id)
            .map(|old| old.live.clone())
            .unwrap_or_else(|| watch::channel(LiveState::default()).0);
        let (evidence, error) = match projected {
            Ok(value) => (value, false),
            Err(_) => (
                published
                    .get(&record.run_id)
                    .and_then(|old| old.evidence.clone()),
                true,
            ),
        };
        let end = if record.phase == Phase::Terminal && !error {
            evidence
                .as_ref()
                .and_then(|e| e.view.last_confirmed_frontier.clone())
                .map(|frontier| StreamEnd {
                    // A release ends observation without claiming completion.
                    status: if record.operator_release.is_some() {
                        StreamEndStatus::Incomplete
                    } else {
                        StreamEndStatus::CommittedCompletion
                    },
                    frontier,
                })
        } else {
            None
        };
        live.send_replace(LiveState {
            revision: record.revision,
            end,
        });
        published.insert(
            record.run_id.clone(),
            Published {
                record,
                evidence,
                live,
                error,
            },
        );
        if error {
            Err(ApiError::new("archive_corrupt"))
        } else {
            Ok(())
        }
    }
    async fn owned_run(self, run: RunId) {
        let coordinator = &self.inner.coordinator;
        if coordinator.start(run.clone()).await.is_err() {
            if let Ok(record) = coordinator.ledger().get(&run).await {
                let _ = self.publish(record).await;
            }
            return;
        }
        loop {
            let result = async {
                let mut record = coordinator.refresh(run.clone()).await.map_err(lifecycle)?;
                if record
                    .last_status
                    .as_ref()
                    .is_some_and(|s| s.state == WorkerState::Prepared)
                    && record.cancel_requested.is_none()
                {
                    record = coordinator.begin(run.clone()).await.map_err(lifecycle)?;
                }
                self.publish(record).await?;
                let record = coordinator
                    .finish_normal(run.clone())
                    .await
                    .map_err(lifecycle)?;
                let terminal = record.phase == Phase::Terminal;
                // A worker stopped for normal completion stays monitored until
                // finish_normal records Terminal or the loss.
                let lost = record.phase == Phase::Reconciling
                    && record.supervisor.as_ref().is_some_and(|s| s.stopped);
                self.publish(record).await?;
                Ok::<_, ApiError>(terminal || lost)
            }
            .await;
            if matches!(result, Ok(true)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    async fn published(&self, run: &RunId) -> Result<Published> {
        self.inner
            .published
            .read()
            .await
            .get(run)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found"))
    }
    /// Cancellation publishes control metadata using the existing read boundary.
    /// It neither waits for hashing/download capacity nor promotes a receipt.
    async fn publish_control(&self, record: Admission) {
        let mut published = self.inner.published.write().await;
        if let Some(old) = published.get_mut(&record.run_id)
            && old.record.revision <= record.revision
        {
            old.record = record;
            old.live
                .send_modify(|state| state.revision = old.record.revision);
        }
    }
    async fn selected(
        &self,
        run: &RunId,
        selected: Option<Frontier>,
    ) -> Result<Arc<projection::Evidence>> {
        let published = self.published(run).await?;
        let current = published
            .evidence
            .ok_or_else(|| ApiError::new("evidence_not_ready"))?;
        let Some(frontier) = selected else {
            return Ok(current);
        };
        let path = self.inner.archive.path(run);
        self.blocking(move || {
            let reader = ArchiveReader::open(path, StorageLimits::default())
                .map_err(projection::evidence)?;
            let snapshot = historical(&reader, &current.snapshot, &frontier)?;
            let query =
                ArchiveQueries::from_snapshot(reader, snapshot).map_err(projection::evidence)?;
            projection::project(&published.record, &query).map(Arc::new)
        })
        .await
    }
    async fn launch_response(&self, record: Admission, replayed: bool) -> Result<LaunchResponse> {
        let run = self.run(record.run_id.clone()).await?;
        Ok(LaunchResponse {
            run_id: record.run_id,
            accepted_at: record.accepted.utc,
            replayed,
            run,
        })
    }
}
fn lifecycle(error: life::Error) -> ApiError {
    match error {
        life::Error::Busy => ApiError::new("run_busy"),
        life::Error::Conflict => ApiError::new("idempotency_conflict"),
        life::Error::ServiceMismatch => ApiError::new("service_identity_mismatch"),
        life::Error::Invalid("stale preview") => ApiError::new("stale_preview"),
        life::Error::Invalid("unsupported recipe or mode") => {
            ApiError::new("unsupported_capability")
        }
        error @ life::Error::Invalid(
            "inference target required"
            | "unknown inference target"
            | "target requires live execution"
            | "live model unavailable"
            | "selected model unsupported"
            | "selected model identity changed"
            | "execution unavailable"
            | "Docker readiness unavailable"
            | "locked Docker image unavailable"
            | "locked Docker readiness changed"
            | "invalid Docker executable/socket selection"
            | "live runs need the Docker workshop"
            | "bundle does not implement this daemon's frozen Docker profile"
            | "bundle does not implement this daemon's live model profile",
        ) => execution_unavailable(error),
        life::Error::Invalid("invalid saved recipe") => ApiError::new("catalog_corrupt"),
        _ => ApiError::new("ownership_unresolved"),
    }
}
fn historical(
    reader: &ArchiveReader,
    covering: &playtime_storage::Snapshot,
    f: &Frontier,
) -> Result<playtime_storage::Snapshot> {
    if f.run_id != reader.identity().run_id
        || f.manifest_sha256 != reader.identity().input_manifest.sha256
    {
        return Err(ApiError::new("invalid_cursor"));
    }
    reader
        .sub_snapshot(
            covering,
            ReadPrefix {
                sequence: f.prefix.sequence,
                bytes: f.prefix.byte_offset,
                sha256: f.prefix.sha256.clone(),
            },
        )
        .map_err(|e| {
            ApiError::new(if e.code == playtime_storage::ErrorCode::CursorAhead {
                "cursor_ahead"
            } else {
                "invalid_cursor"
            })
        })
}
#[async_trait]
impl Application for Daemon {
    async fn inference_targets(&self) -> Result<InferenceTargets> {
        Ok(InferenceTargets {
            schema_version: 1,
            targets: self
                .inner
                .archive
                .targets
                .values()
                .map(|(t, _)| t.clone())
                .collect(),
        })
    }
    async fn model_inventory_on(&self, target: Option<String>) -> Result<ModelInventory> {
        self.browser_on(target.as_deref())?.inventory().await
    }
    async fn inspect_model_on(
        &self,
        target: Option<String>,
        request: ModelInspectRequest,
    ) -> Result<ModelInspection> {
        self.browser_on(target.as_deref())?.inspect(request).await
    }
    async fn model_review_on(
        &self,
        target: Option<String>,
        request: ModelInspectRequest,
    ) -> Result<ModelRegistrationReview> {
        self.browser_on(target.as_deref())?
            .review(&self.capabilities().service_id, request)
            .await
    }
    async fn model_review(&self, request: ModelInspectRequest) -> Result<ModelRegistrationReview> {
        self.inner.registrations.review(request).await
    }
    async fn model_registrations(&self) -> Result<ModelRegistrations> {
        self.inner.registrations.list().await
    }
    async fn model_registration(&self, id: ModelRegistrationId) -> Result<RegisteredModel> {
        self.inner.registrations.get(id).await
    }
    async fn register_model(
        &self,
        key: IdempotencyKey,
        request: ModelRegistrationRequest,
    ) -> Result<ModelRegistrationResponse> {
        let service = self.capabilities().service_id;
        let browser = self
            .inner
            .target_browsers
            .values()
            .cloned()
            .chain(self.inner.models.clone())
            .find(|b| b.binding(&service) == request.endpoint_binding);
        self.inner
            .registrations
            .with_browser(browser)
            .register(key, request)
            .await
    }
    async fn model_registration_lookup(
        &self,
        key: IdempotencyKey,
    ) -> Result<ModelRegistrationResponse> {
        self.inner.registrations.lookup(key).await
    }

    async fn model_inventory(&self) -> Result<ModelInventory> {
        self.inner
            .models
            .as_ref()
            .ok_or_else(|| ApiError::new("unsupported_capability"))?
            .inventory()
            .await
    }
    async fn inspect_model(&self, request: ModelInspectRequest) -> Result<ModelInspection> {
        self.inner
            .models
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
            service_id: ServiceId::new(self.inner.coordinator.ledger().service_id().as_str())
                .unwrap(),
            service_build: env!("CARGO_PKG_VERSION").into(),
            backend: if self.inner.archive.live.is_some() || !self.inner.archive.targets.is_empty()
            {
                projection::docker_and_live_backend()
            } else if self.inner.archive.docker.is_some() {
                projection::docker_backend()
            } else {
                projection::backend()
            },
            modes: [
                Some(RunMode::Simulation),
                self.inner
                    .archive
                    .docker_options
                    .is_some()
                    .then_some(RunMode::ScriptedSmoke),
                (self.inner.archive.live_endpoint.is_some()
                    || !self.inner.archive.targets.is_empty())
                .then_some(RunMode::LiveSmoke),
                Some(RunMode::Experiment),
            ]
            .into_iter()
            .flatten()
            .collect(),
            features: [
                (self.inner.models.is_some() || !self.inner.target_browsers.is_empty())
                    .then(|| MODEL_DISCOVERY.to_owned()),
            ]
            .into_iter()
            .flatten()
            .chain(vec![
                INFERENCE_TARGETS.into(),
                MODEL_REGISTRATION.into(),
                RECIPE_AUTHORING.into(),
                RECIPE_HARNESS_SELECTION.into(),
                RECIPE_COMPACTION_SETTINGS.into(),
                RECIPE_COMPACTION_FINISH.into(),
                MODEL_SETTINGS.into(),
                RECIPE_MODEL_SELECTION.into(),
                RECIPE_MODEL_EXECUTION.into(),
                "keyed_launch".into(),
                "event_stream".into(),
                "verified_artifacts".into(),
                "accounting_coverage_v1".into(),
                "checkpoint_index_v1".into(),
                "operator_release".into(),
                if self.inner.test_supervision {
                    "cpu_test_supervision"
                } else {
                    "systemd_supervision"
                }
                .into(),
            ])
            .collect(),
            unsupported_adapters: [
                (self.inner.archive.live.is_none() && self.inner.archive.targets.is_empty())
                    .then(|| "ollama".into()),
                self.inner.archive.docker.is_none().then(|| "docker".into()),
                Some("restart_and_worker_loss_recovery".into()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            limits: ApiLimits::default(),
        }
    }
    async fn recipes(&self, query: PageQuery) -> Result<Page<Recipe>> {
        let mut recipes: Vec<(String, Recipe)> = vec![(
            "two-session".into(),
            Recipe {
                recipe_id: RecipeId::new("two-session").unwrap(),
                name: "Two-session controller simulation".into(),
                modes: vec![RunMode::Simulation],
                saved: None,
            },
        )];
        if self.inner.archive.docker_options.is_some() || self.inner.archive.docker.is_some() {
            recipes.push((
                playtime_runner::docker::RECIPE.into(),
                Recipe {
                    recipe_id: RecipeId::new(playtime_runner::docker::RECIPE).unwrap(),
                    name: "Two-session real Docker / scripted model".into(),
                    modes: vec![RunMode::ScriptedSmoke],
                    saved: None,
                },
            ));
        }
        if self.inner.archive.live_endpoint.is_some()
            || self.inner.archive.live.is_some()
            || !self.inner.archive.targets.is_empty()
        {
            recipes.push((
                playtime_runner::docker::LIVE_RECIPE.into(),
                Recipe {
                    recipe_id: RecipeId::new(playtime_runner::docker::LIVE_RECIPE).unwrap(),
                    name: "Two-session real Docker / live model".into(),
                    modes: vec![RunMode::LiveSmoke],
                    saved: None,
                },
            ));
        }
        self.inner
            .catalog
            .page(
                query,
                None,
                Some(recipes.into_iter().map(|(_, recipe)| recipe).collect()),
            )
            .await
    }
    async fn recipe_model_settings(&self, id: RecipeId) -> Result<RecipeSettingsView> {
        self.inner.catalog.model_settings(&id).await
    }
    async fn harnesses(&self) -> Result<HarnessCatalog> {
        playtime_runner::docker::harness_catalog().map_err(|_| ApiError::new("internal_error"))
    }
    async fn recipe_harness(&self, id: RecipeId) -> Result<RecipeHarnessView> {
        self.inner.catalog.harness(&id).await
    }
    async fn recipe_definition(&self, id: RecipeId) -> Result<RecipeDefinition> {
        self.inner.catalog.definition(&id).await
    }
    async fn recipe_save(
        &self,
        key: IdempotencyKey,
        request: RecipeMutation,
    ) -> Result<RecipeSaveResponse> {
        self.inner.catalog.save(key, request).await
    }
    async fn recipe_save_lookup(&self, key: IdempotencyKey) -> Result<RecipeSaveResponse> {
        self.inner.catalog.lookup(&key).await
    }
    async fn saved_recipes(
        &self,
        query: PageQuery,
        history: Option<RecipeId>,
    ) -> Result<Page<Recipe>> {
        self.inner.catalog.page(query, history, None).await
    }
    async fn saved_recipe(&self, id: RecipeId) -> Result<SavedRecipe> {
        self.inner.catalog.head(&id).await
    }
    async fn preview(&self, recipe: RecipeId, request: PreviewRequest) -> Result<Preview> {
        if request.target.is_some()
            && !matches!(request.mode, RunMode::LiveSmoke | RunMode::Experiment)
        {
            return Err(ApiError::new("invalid_parameters"));
        }
        if recipe.as_str() == "two-session" {
            return if request.mode == RunMode::Simulation {
                Ok(self.inner.archive.preview())
            } else {
                Err(ApiError::new("unsupported_capability"))
            };
        }
        if request.mode == RunMode::Experiment {
            self.inner.catalog.definition(&recipe).await?;
            self.inner.catalog.check_context(&recipe).await?;
        } else if !matches!(
            (recipe.as_str(), request.mode),
            (playtime_runner::docker::RECIPE, RunMode::ScriptedSmoke)
                | (playtime_runner::docker::LIVE_RECIPE, RunMode::LiveSmoke)
        ) {
            return Err(ApiError::new("unsupported_capability"));
        }
        let archive = self
            .inner
            .archive
            .execution_on(request.mode, request.target.as_deref())
            .await
            .map_err(execution_unavailable)?;
        match request.mode {
            RunMode::Experiment => archive
                .authored_preview(recipe.as_str())
                .await
                .map_err(execution_unavailable),
            RunMode::ScriptedSmoke => archive
                .docker_preview()
                .ok_or_else(|| execution_unavailable(life::Error::Uncertain)),
            RunMode::LiveSmoke => archive
                .live_preview()
                .ok_or_else(|| execution_unavailable(life::Error::Uncertain)),
            _ => Err(ApiError::new("unsupported_capability")),
        }
    }
    async fn admit(&self, key: IdempotencyKey, request: LaunchRequest) -> Result<LaunchResponse> {
        let this = self.clone();
        // Own the entire reservation -> start scheduling chain across HTTP loss.
        tokio::spawn(async move {
            let accepted = this
                .inner
                .coordinator
                .admit(
                    this.inner.coordinator.ledger().service_id().clone(),
                    key.to_string(),
                    life::AdmissionRequest {
                        target: request.target,
                        recipe_id: request.recipe_id.to_string(),
                        mode: request.mode,
                        preview_revision: request.preview_revision.to_string(),
                    },
                )
                .await
                .map_err(lifecycle)?;
            this.publish(accepted.admission.clone()).await?;
            if !accepted.replayed {
                tokio::spawn(this.clone().owned_run(accepted.admission.run_id.clone()));
            }
            this.launch_response(accepted.admission, accepted.replayed)
                .await
        })
        .await
        .map_err(|_| ApiError::new("backend_unavailable"))?
    }
    async fn launch(&self, key: IdempotencyKey) -> Result<LaunchResponse> {
        let record = self
            .inner
            .coordinator
            .ledger()
            .lookup(key.as_str())
            .await
            .map_err(lifecycle)?
            .ok_or_else(|| ApiError::new("not_found"))?;
        self.launch_response(record, true).await
    }
    async fn runs(&self, query: PageQuery) -> Result<Page<RunView>> {
        let ids: Vec<_> = self.inner.published.read().await.keys().cloned().collect();
        let mut views = Vec::new();
        for id in ids {
            views.push((id.to_string(), self.run(id).await?));
        }
        paging::page(
            views,
            &query,
            "runs",
            self.capabilities().service_id.as_str(),
            None,
        )
    }
    async fn run(&self, run: RunId) -> Result<RunView> {
        let published = self.published(&run).await?;
        let mut view = published
            .evidence
            .as_ref()
            .map(|e| e.view.clone())
            .unwrap_or_else(|| projection::unprepared(&published.record));
        // Control state may advance without a newer experimental boundary.
        let control = projection::unprepared(&published.record);
        view.orchestration = control.orchestration;
        view.cancellation = control.cancellation;
        if published.error {
            view.evidence = EvidenceAvailability::OwnershipUnresolved {
                reason: "newer confirmation could not be published; retaining verified evidence"
                    .into(),
            };
        }
        Ok(view)
    }
    async fn cancel(&self, run: RunId) -> Result<CancelResponse> {
        let record = self
            .inner
            .coordinator
            .cancel(run.clone())
            .await
            .map_err(lifecycle)?;
        self.publish_control(record).await;
        let run = self.run(run).await?;
        Ok(CancelResponse {
            pending: run.orchestration.state != OrchestrationState::Terminal,
            intent: run.cancellation.clone(),
            run,
        })
    }
    async fn release(&self, run: RunId, request: ReleaseRequest) -> Result<ReleaseResponse> {
        self.published(&run).await?;
        let record = self
            .inner
            .coordinator
            .release(run.clone(), request.reason)
            .await
            .map_err(|error| match error {
                life::Error::Refused(remaining) => {
                    let mut refused = ApiError::new("release_refused");
                    refused
                        .0
                        .details
                        .insert("remaining".into(), serde_json::json!(remaining));
                    refused
                }
                life::Error::Invalid("release reason must be 1 to 512 printable bytes") => {
                    ApiError::new("invalid_parameters")
                }
                other => lifecycle(other),
            })?;
        // The release is already durable. A projection failure is reported by
        // the run view rather than by this response.
        let _ = self.publish(record).await;
        Ok(ReleaseResponse {
            run: self.run(run).await?,
        })
    }
    async fn history(
        &self,
        run: RunId,
        cursor: Option<EventCursor>,
        follow: bool,
        limit: u32,
        byte_limit: usize,
    ) -> Result<History> {
        let evidence = self.selected(&run, None).await?;
        let path = self.inner.archive.path(&run);
        self.blocking(move || {
            let reader = ArchiveReader::open(path, StorageLimits::default())
                .map_err(projection::evidence)?;
            let mut snapshot = evidence.snapshot.clone();
            let after = if let Some(cursor) = cursor {
                let older = historical(&reader, &snapshot, &cursor.frontier)?;
                if !follow {
                    snapshot = older;
                }
                cursor.after_sequence
            } else {
                0
            };
            let page = reader
                .page(&snapshot, &snapshot.query(after, limit))
                .map_err(projection::evidence)?;
            let mut events = Vec::new();
            let mut bytes = 0;
            for event in page.events {
                let size = serde_json::to_vec(&event.envelope)
                    .map_err(projection::evidence)?
                    .len()
                    + 1;
                if bytes + size > byte_limit {
                    if events.is_empty() {
                        return Err(ApiError::new("evidence_limit_exceeded"));
                    }
                    break;
                }
                bytes += size;
                events.push(event.envelope);
            }
            let last = events.last().map_or(after, |e| e.sequence.get());
            Ok(History {
                frontier: projection::frontier(snapshot.frontier()),
                has_more: last < snapshot.frontier().committed_sequence,
                events,
                after_sequence: after,
            })
        })
        .await
    }
    async fn subscribe(&self, run: RunId) -> Result<watch::Receiver<LiveState>> {
        Ok(self.published(&run).await?.live.subscribe())
    }
    async fn checkpoints(&self, run: RunId, query: PageQuery) -> Result<Page<Checkpoint>> {
        let cursor = paging::decode(&query, "checkpoints", run.as_str())?;
        let evidence = self.selected(&run, cursor.and_then(|c| c.frontier)).await?;
        paging::page(
            evidence
                .checkpoints
                .iter()
                .cloned()
                .map(|c| (format!("{:020}", c.checkpoint.published_sequence), c))
                .collect(),
            &query,
            "checkpoints",
            run.as_str(),
            evidence.view.last_confirmed_frontier.clone(),
        )
    }
    async fn checkpoint(&self, run: RunId, session: SessionId) -> Result<Checkpoint> {
        self.selected(&run, None)
            .await?
            .checkpoints
            .iter()
            .find(|c| c.checkpoint.session_id == session)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found"))
    }
    async fn artifacts(&self, run: RunId, query: PageQuery) -> Result<Page<Artifact>> {
        let cursor = paging::decode(&query, "artifacts", run.as_str())?;
        let evidence = self.selected(&run, cursor.and_then(|c| c.frontier)).await?;
        paging::page(
            evidence
                .artifacts
                .iter()
                .cloned()
                .map(|a| (a.artifact_id.to_string(), a))
                .collect(),
            &query,
            "artifacts",
            run.as_str(),
            evidence.view.last_confirmed_frontier.clone(),
        )
    }
    async fn artifact(&self, run: RunId, artifact: ArtifactId) -> Result<Artifact> {
        self.selected(&run, None)
            .await?
            .artifacts
            .iter()
            .find(|a| a.artifact_id == artifact)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found"))
    }
    async fn download(&self, run: RunId, artifact: ArtifactId) -> Result<Download> {
        let evidence = self.selected(&run, None).await?;
        let metadata = evidence
            .artifacts
            .iter()
            .find(|a| a.artifact_id == artifact)
            .cloned()
            .ok_or_else(|| ApiError::new("not_found"))?;
        let path = self.inner.archive.path(&run);
        self.blocking(move || {
            let reader = ArchiveReader::open(path, StorageLimits::default())
                .map_err(projection::evidence)?;
            let f = evidence.snapshot.frontier();
            let snapshot = reader
                .sub_snapshot(
                    &evidence.snapshot,
                    ReadPrefix {
                        sequence: f.committed_sequence,
                        bytes: f.committed_bytes,
                        sha256: f.prefix_sha256.clone(),
                    },
                )
                .map_err(projection::evidence)?;
            let bytes = reader
                .read_content(&snapshot, &metadata.content)
                .map_err(|_| ApiError::new("content_mismatch"))?;
            Ok(Download {
                artifact: metadata,
                reader: Box::pin(std::io::Cursor::new(bytes)),
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests;

fn tolerate_unavailable(result: life::Result<()>) -> life::Result<()> {
    match result {
        Err(life::Error::Invalid(
            "Docker readiness unavailable"
            | "locked Docker image unavailable"
            | "invalid Docker executable/socket selection"
            | "live model unavailable"
            | "live runs need the Docker workshop"
            | "bundle does not implement this daemon's frozen Docker profile"
            | "bundle does not implement this daemon's live model profile",
        )) => Ok(()),
        other => other,
    }
}
fn execution_unavailable(error: life::Error) -> ApiError {
    let reason = match error {
        life::Error::Busy => return ApiError::new("run_busy"),
        life::Error::Invalid("invalid saved recipe") => return ApiError::new("catalog_corrupt"),
        life::Error::Invalid("unsupported recipe or mode") => {
            return ApiError::new("unsupported_capability");
        }
        life::Error::Invalid("inference target required") => "target_required",
        life::Error::Invalid("unknown inference target") => "target_unknown",
        life::Error::Invalid("target requires live execution") => "target_requires_live_execution",
        life::Error::Invalid("live model unavailable") => "model_unavailable",
        life::Error::Invalid("selected model unsupported") => "selected_model_unsupported",
        life::Error::Invalid("selected model identity changed") => {
            "selected_model_missing_or_changed"
        }
        life::Error::Invalid("locked Docker image unavailable") => "workshop_image_unavailable",
        life::Error::Invalid(
            "Docker readiness unavailable"
            | "locked Docker readiness changed"
            | "invalid Docker executable/socket selection"
            | "live runs need the Docker workshop",
        ) => "workshop_unavailable",
        life::Error::Invalid(
            "bundle does not implement this daemon's frozen Docker profile"
            | "bundle does not implement this daemon's live model profile",
        ) => "worker_profile_unavailable",
        life::Error::Invalid("execution unavailable") => "execution_configuration_unavailable",
        _ => return ApiError::new("backend_unavailable"),
    };
    let mut error = ApiError::new("execution_unavailable");
    error
        .0
        .details
        .insert("reasons".into(), serde_json::json!([reason]));
    error
}
