//! Real preparation and confirmation verification. No runtime compilation.
use super::projection;
use crate::lifecycle::{self as life, ports::ArchivePort, *};
use async_trait::async_trait;
use playtime_core::{RunMode, StopReason};
use playtime_engine::simulation::Scenario;
use playtime_protocol::{
    api::*,
    storage::{ArchiveIdentity, EventFrontier},
    worker::{self, Binding, Receipt, Status, WorkerKind},
};
use playtime_storage::{ArchiveReader, ConfirmationAuthority, ReadPrefix, Snapshot, StorageLimits};
use playtime_worker::bundle::VerifiedBundle;
use playtime_worker_control::{
    Bootstrap, Credential, FileLock, boot_id, boot_ms, create_private_dir, digest, random_id,
    read_private, write_once,
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub(super) struct Archive {
    pub root: PathBuf,
    pub bundle: Arc<VerifiedBundle>,
    pub scenario: Scenario,
    bytes: Vec<u8>,
    pub revision: PreviewRevision,
    pub docker: Option<playtime_runner::docker::Profile>,
    docker_engine: Option<playtime_docker::Engine>,
    pub live: Option<Arc<Live>>,
    /// Reviewed catalog recipes, by id. They run on the live model.
    pub recipes: std::collections::BTreeMap<String, playtime_runner::docker::Recipe>,
    pub authoring: Option<crate::catalog::Catalog>,
    pub docker_options: Option<(PathBuf, PathBuf)>,
    pub live_endpoint: Option<String>,
    docker_transport: Arc<tokio::sync::Mutex<Option<playtime_docker::transport::Cli>>>,
    execution_gate: Arc<tokio::sync::Mutex<()>>,
}
/// The live model as the daemon reaches it: host bindings frozen at startup
/// and an adapter for read-only identity checks at admission.
pub(super) struct Live {
    pub host: playtime_runner::docker::LiveHost,
    endpoint: playtime_ollama::Endpoint,
    reads: tokio::sync::Mutex<()>,
}
impl Live {
    /// Recheck frozen addresses and the requested execution identity. A missing
    /// default model must not block an explicitly selected different model.
    async fn ready(&self, model: Option<&playtime_runner::docker::LiveModel>) -> life::Result<()> {
        let _guard = self.reads.lock().await;
        if resolve(&self.endpoint).await? != self.host.addresses {
            return Err(Error::Invalid("live model unavailable"));
        }
        let limits = playtime_ollama::Limits::default();
        let transport = playtime_ollama::NativeTransport::with_resolution(
            self.endpoint.clone(),
            playtime_ollama::Trust::BundledWebPki,
            playtime_ollama::Resolution::Pinned {
                addresses: self.host.addresses.clone(),
            },
            &limits,
        )
        .map_err(|_| Error::Invalid("live model unavailable"))?;
        let mut adapter = if let Some(model) = model.filter(|m| m.registration.is_some()) {
            let identity = model
                .execution_model()
                .map_err(|_| Error::Invalid("selected model unsupported"))?;
            playtime_ollama::Adapter::with_model(transport, identity, limits)
        } else {
            let lock = playtime_ollama::ModelLock::parse(playtime_runner::docker::MODEL_LOCK)
                .map_err(|_| Error::Invalid("invalid model lock"))?;
            playtime_ollama::Adapter::new(transport, lock, limits)
        }
        .map_err(|_| Error::Invalid("selected model unsupported"))?;
        adapter
            .inspect(
                false,
                tokio::time::Instant::now() + std::time::Duration::from_secs(10),
                &playtime_ollama::CancellationToken::new(),
            )
            .await
            .map(|_| ())
            .map_err(|e| {
                if e.kind == playtime_ollama::FailureKind::Identity {
                    Error::Invalid("selected model identity changed")
                } else {
                    Error::Invalid("live model unavailable")
                }
            })
    }
}
#[cfg(test)]
#[path = "../../../playtime-runner/tests/common/registered.rs"]
mod registered;
#[cfg(test)]
mod model_readiness_tests {
    use super::*;

    #[tokio::test]
    async fn selected_readiness_requires_its_own_identity_and_not_the_default_model() {
        let fake = playtime_test_support::model_metadata::FakeOllama::start().await;
        let selected = registered::qwen();
        let id = &selected.summary.identity;
        let lock = serde_json::json!({"name":id.name,"digest":id.digest,"ollama_version":id.backend_version,
            "definition_sha256":id.definition_sha256,"definition":selected.definition,"modelfile":""});
        let model = playtime_runner::docker::LiveModel {
            registration: Some(selected.summary.clone()),
            lock,
            settings: serde_json::json!({}),
            host: playtime_runner::docker::LiveHost {
                endpoint: fake.endpoint.clone(),
                addresses: vec!["127.0.0.1".parse().unwrap()],
                live_lock: "/synthetic/lock".into(),
            },
        };
        let live = Live {
            endpoint: playtime_ollama::Endpoint::parse(&fake.endpoint).unwrap(),
            host: model.host.clone(),
            reads: tokio::sync::Mutex::new(()),
        };
        let tags = serde_json::json!({"models":[{"name":id.name,"digest":id.digest}]});
        {
            let mut state = fake.metadata.0.lock().unwrap();
            state.tags = tags.clone();
            state.show = selected.definition.clone();
        }
        live.ready(Some(&model)).await.unwrap();
        assert!(
            live.ready(None).await.is_err(),
            "no default GPT-OSS on this host"
        );
        for case in 0..5 {
            {
                let mut state = fake.metadata.0.lock().unwrap();
                state.tags = tags.clone();
                state.show = selected.definition.clone();
                state.version = id.backend_version.clone();
                match case {
                    // The HTTP fixture returns 404 from show for an absent tag,
                    // just as Ollama does; this must not become endpoint outage.
                    0 => state.tags = serde_json::json!({"models":[]}),
                    1 => state.tags["models"][0]["digest"] = serde_json::json!("changed"),
                    2 => state.show["template"] = serde_json::json!("changed"),
                    3 => state.version = "0.32.4".into(),
                    _ => state.show["capabilities"] = serde_json::json!(["completion"]),
                }
            }
            assert!(
                matches!(
                    live.ready(Some(&model)).await,
                    Err(Error::Invalid("selected model identity changed"))
                ),
                "case {case}"
            );
        }
        {
            let mut state = fake.metadata.0.lock().unwrap();
            state.tags = tags;
            state.show = selected.definition;
            state.version = id.backend_version.clone();
            state.loaded = serde_json::json!({"models":[{"name":"unrelated-loaded-model"}]});
        }
        live.ready(Some(&model)).await.unwrap();
        assert!(
            !fake
                .metadata
                .0
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|c| c == "ps" || c.starts_with("unexpected:"))
        );
        fake.stop();
        assert!(matches!(
            live.ready(Some(&model)).await,
            Err(Error::Invalid("live model unavailable"))
        ));
    }
}
/// Busy while any checkout's live worker holds the lock. This probe releases
/// it at once; only the worker holds it, for its whole lifetime.
pub(super) fn probe_live_lock(path: &std::path::Path) -> life::Result<()> {
    FileLock::acquire(path)
        .map(drop)
        .map_err(|error| match error {
            playtime_worker_control::Error::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Error::Busy
            }
            _ => Error::Invalid("live model unavailable"),
        })
}
/// The static worker can't resolve mDNS names, so the daemon does. An IP
/// literal is pinned as it is, without a lookup. Link-local IPv6 addresses
/// need a scope the worker can't carry.
pub(super) async fn resolve(
    endpoint: &playtime_ollama::Endpoint,
) -> life::Result<Vec<std::net::IpAddr>> {
    let found: Vec<_> = if let Some(ip) = endpoint.ip_literal() {
        vec![ip]
    } else {
        let url = endpoint.url(playtime_ollama::Operation::Version);
        let (Some(host), Some(port)) = (url.domain(), url.port_or_known_default()) else {
            return Err(Error::Invalid("live endpoint needs a host"));
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| Error::Invalid("live model unavailable"))?
        .map_err(|_| Error::Invalid("live model unavailable"))?
        .map(|address| address.ip())
        .collect()
    };
    let mut addresses: Vec<_> = found
        .into_iter()
        .filter(|ip| !matches!(ip, std::net::IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80))
        .collect();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() || addresses.len() > 16 {
        return Err(Error::Invalid("live model unavailable"));
    }
    Ok(addresses)
}
/// The Docker CLI's `--config` directory. `Cli::resolve` creates it new and it
/// stays empty. The service lock makes this daemon the root's only user, so the
/// previous start's directory is removed instead of accumulating. Anything but
/// an empty directory there is refused, never deleted.
pub(super) fn docker_config(root: &std::path::Path) -> life::Result<PathBuf> {
    let path = root.join(".playtime/docker-config");
    match std::fs::remove_dir(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::Invalid(
            "previous Docker config is not an empty directory",
        )),
        _ => Ok(path),
    }
}
/// What an admission prepares, keyed on its recipe as well as its mode.
#[derive(Debug, PartialEq)]
pub(super) enum Launch<'a> {
    Simulation,
    ScriptedSmoke,
    LiveSmoke,
    Catalog(&'a str),
}
pub(super) fn launch(request: &AdmissionRequest) -> life::Result<Launch<'_>> {
    let recipe = request.recipe_id.as_str();
    Ok(match request.mode {
        RunMode::Simulation if recipe == "two-session" => Launch::Simulation,
        RunMode::ScriptedSmoke if recipe == playtime_runner::docker::RECIPE => {
            Launch::ScriptedSmoke
        }
        RunMode::LiveSmoke if recipe == playtime_runner::docker::LIVE_RECIPE => Launch::LiveSmoke,
        RunMode::Experiment => Launch::Catalog(recipe),
        _ => return Err(Error::Invalid("unsupported recipe or mode")),
    })
}
impl Archive {
    pub fn new(root: PathBuf, bundle: VerifiedBundle) -> life::Result<Self> {
        let bytes = include_bytes!("two-session.json").to_vec();
        let scenario: Scenario = serde_json::from_slice(&bytes)?;
        let revision = digest(&serde_json::to_vec(&(digest(&bytes), bundle.id()))?);
        Ok(Self {
            root,
            docker: None,
            docker_engine: None,
            live: None,
            recipes: Default::default(),
            authoring: None,
            docker_options: None,
            live_endpoint: None,
            docker_transport: Arc::new(tokio::sync::Mutex::new(None)),
            execution_gate: Arc::new(tokio::sync::Mutex::new(())),
            bundle: Arc::new(bundle),
            scenario,
            bytes,
            revision: PreviewRevision::new(revision.as_str()).map_err(|_| Error::Uncertain)?,
        })
    }
    pub async fn enable_docker(
        &mut self,
        executable: &std::path::Path,
        socket: &std::path::Path,
    ) -> life::Result<()> {
        use playtime_docker::{CancellationToken, Engine, transport::Cli};
        let profile_digest = playtime_runner::docker::digest()
            .map_err(|_| Error::Invalid("invalid Docker profile"))?;
        // The worker accepts only its own compiled profile. Refuse a bundle
        // with a different one here, before any admission is reserved.
        if !self
            .bundle
            .descriptor()
            .capabilities
            .implements_docker(&profile_digest)
        {
            return Err(Error::Invalid(
                "bundle does not implement this daemon's frozen Docker profile",
            ));
        }
        let cli = {
            let mut cached = self.docker_transport.lock().await;
            if let Some(cli) = cached.as_ref() {
                cli.clone()
            } else {
                let cli = Cli::resolve(executable, socket, &docker_config(&self.root)?)
                    .map_err(|_| Error::Invalid("invalid Docker executable/socket selection"))?;
                *cached = Some(cli.clone());
                cli
            }
        };
        let selection = cli.selection().clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let engine = Engine::connect(Arc::new(cli), deadline, CancellationToken::new())
            .await
            .map_err(|_| Error::Invalid("Docker readiness unavailable"))?;
        engine
            .readiness(
                playtime_runner::docker::IMAGE_LOCK,
                deadline,
                CancellationToken::new(),
            )
            .await
            .map_err(|_| Error::Invalid("locked Docker image unavailable"))?;
        let profile =
            playtime_runner::docker::Profile::freeze(selection, engine.identity().clone())
                .map_err(|_| Error::Invalid("invalid Docker profile"))?;
        self.docker = Some(profile);
        self.docker_engine = Some(engine);
        Ok(())
    }
    /// Live runs reuse the Docker workshop, so this needs `enable_docker` first.
    pub async fn enable_live(&mut self, endpoint: &str) -> life::Result<()> {
        let profiles = playtime_runner::docker::digest()
            .map_err(|_| Error::Invalid("invalid Docker profile"))?;
        if !self
            .bundle
            .descriptor()
            .capabilities
            .implements_live(&profiles)
        {
            return Err(Error::Invalid(
                "bundle does not implement this daemon's live model profile",
            ));
        }
        // Every checkout's daemon and worker share this lock, so at most one
        // live run uses the model at a time.
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(Error::Invalid("live runs need XDG_RUNTIME_DIR"))?;
        let directory = runtime.join("playtime");
        create_private_dir(&directory)?;
        let endpoint = playtime_ollama::Endpoint::parse(endpoint)
            .ok()
            .filter(|e| e.as_str().starts_with("http://"))
            .ok_or(Error::Invalid("live endpoint must be a plain HTTP URL"))?;
        if self.docker.is_none() {
            return Err(Error::Invalid("live runs need the Docker workshop"));
        }
        let addresses = resolve(&endpoint).await?;
        let live = Live {
            host: playtime_runner::docker::LiveHost {
                endpoint: endpoint.as_str().into(),
                addresses,
                live_lock: directory.join("live-model.lock"),
            },
            endpoint,
            reads: tokio::sync::Mutex::new(()),
        };
        self.live = Some(Arc::new(live));
        Ok(())
    }
    /// Catalog definitions load independently of current execution readiness.
    pub fn enable_recipes(
        &mut self,
        recipes: Vec<playtime_runner::docker::Recipe>,
    ) -> life::Result<()> {
        if recipes.is_empty() {
            return Ok(());
        }
        for recipe in recipes {
            recipe
                .check()
                .map_err(|_| Error::Invalid("invalid catalog recipe"))?;
            self.recipes.insert(recipe.id.clone(), recipe);
        }
        Ok(())
    }
    pub async fn execution(&self, mode: RunMode) -> life::Result<Self> {
        let _guard = self.execution_gate.lock().await;
        let mut ready = self.clone();
        if ready.docker.is_none()
            && let Some((executable, socket)) = &self.docker_options
        {
            ready.enable_docker(executable, socket).await?;
        }
        if mode != RunMode::ScriptedSmoke
            && ready.live.is_none()
            && let Some(endpoint) = &self.live_endpoint
        {
            ready.enable_live(endpoint).await?;
        }
        if let Some(engine) = &ready.docker_engine {
            engine
                .readiness(
                    playtime_runner::docker::IMAGE_LOCK,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(10),
                    playtime_docker::CancellationToken::new(),
                )
                .await
                .map_err(|_| Error::Invalid("locked Docker readiness changed"))?;
        }
        if ready.docker.is_none() {
            return Err(Error::Invalid("execution unavailable"));
        }
        if mode == RunMode::LiveSmoke {
            ready
                .live
                .as_ref()
                .ok_or(Error::Invalid("execution unavailable"))?
                .ready(None)
                .await?;
        }
        Ok(ready)
    }
    async fn authored_profile(
        &self,
        id: &str,
    ) -> life::Result<playtime_runner::docker::NativeProfile> {
        if !id.starts_with("saved-") {
            return self
                .recipe_profile(id)
                .map(|p| playtime_runner::docker::NativeProfile::Memento(Box::new(p)))
                .ok_or(Error::Invalid("execution unavailable"));
        }
        let (definition, _) = self
            .authoring
            .as_ref()
            .ok_or(Error::Invalid("execution unavailable"))?
            .frozen(id)
            .await
            .map_err(|error| {
                Error::Invalid(if error.code() == "not_found" {
                    "unsupported recipe or mode"
                } else {
                    "invalid saved recipe"
                })
            })?;
        definition.executable().map_err(|_| {
            Error::Invalid(if definition.model_registration.is_some() {
                "selected model unsupported"
            } else {
                "execution unavailable"
            })
        })?;
        let (docker, live) = (
            self.docker
                .as_ref()
                .ok_or(Error::Invalid("execution unavailable"))?,
            self.live
                .as_ref()
                .ok_or(Error::Invalid("execution unavailable"))?,
        );
        let profile = playtime_runner::docker::NativeProfile::freeze_definition(
            definition,
            docker.selection.clone(),
            docker.daemon.clone(),
            live.host.clone(),
        )
        .map_err(|_| Error::Invalid("invalid saved recipe"))?;
        profile
            .validate()
            .map_err(|_| Error::Invalid("invalid saved recipe"))?;
        Ok(profile)
    }
    pub async fn authored_preview(&self, id: &str) -> life::Result<Preview> {
        let profile = self.authored_profile(id).await?;
        self.live
            .as_ref()
            .ok_or(Error::Invalid("execution unavailable"))?
            .ready(profile.model())
            .await?;
        let (mut preview, seeds) = match &profile {
            playtime_runner::docker::NativeProfile::Memento(p) => (
                self.model_preview(p, id, RunMode::Experiment),
                &p.recipe.as_ref().ok_or(Error::Uncertain)?.seeds,
            ),
            playtime_runner::docker::NativeProfile::Continuous(p) => {
                (self.continuous_preview(p, id), &p.seeds)
            }
        };
        let mut preview = preview
            .take()
            .ok_or(Error::Invalid("execution unavailable"))?;
        preview.seeds = SeedSummary {
            files: seeds.len() as u64,
            bytes: seeds.values().map(|s| s.len() as u64).sum(),
            digests: seeds
                .values()
                .map(|s| digest(s.as_bytes()).to_string())
                .collect(),
        };
        Ok(preview)
    }
    fn continuous_preview(
        &self,
        profile: &playtime_runner::docker::ContinuousProfile,
        recipe: &str,
    ) -> Option<Preview> {
        let mut preview = self.docker_preview()?;
        preview.recipe_id = RecipeId::new(recipe).ok()?;
        preview.mode = RunMode::Experiment;
        preview.preview_revision = PreviewRevision::new(
            digest(
                &serde_json::to_vec(&(
                    profile,
                    self.bundle.id(),
                    &self.bundle.descriptor().capabilities.docker_profile,
                ))
                .ok()?,
            )
            .as_str(),
        )
        .ok()?;
        preview.effective_configuration = serde_json::to_value(profile).ok()?;
        preview.harness = HarnessView::from_configuration(&preview.effective_configuration).ok()?;
        // Compatibility field retains the source recipe's inactive memento
        // allocations. Only config.parameters and the selected harness execute.
        preview.parameters = profile.definition.as_ref()?.recipe.parameters.clone();
        preview.prompt = profile.config.prompt.clone();
        preview.protocol = profile.config.protocol.clone();
        let model = profile.model.as_ref()?;
        preview.model_identity = serde_json::json!({"adapter":"ollama","name":model.lock["name"], "digest":model.lock["digest"],"ollama_version":model.lock["ollama_version"], "settings":model.settings,"endpoint":model.host.endpoint,"addresses":model.host.addresses});
        if let Some(registration) = &model.registration {
            preview.model_identity["registration"] = serde_json::to_value(registration).ok()?;
            preview.model_identity["definition_sha256"] = model.lock["definition_sha256"].clone();
        }
        preview.warnings = vec!["One continuous workshop. Compaction uses the same model and run budget. Only the final stopped workspace is captured; intermediate captures are unavailable.".into()];
        preview.backend = projection::live_backend();
        Some(preview)
    }
    fn recipe_profile(&self, id: &str) -> Option<playtime_runner::docker::Profile> {
        let (docker, live) = (self.docker.as_ref()?, self.live.as_ref()?);
        playtime_runner::docker::Profile::freeze_recipe(
            self.recipes.get(id)?.clone(),
            docker.selection.clone(),
            docker.daemon.clone(),
            live.host.clone(),
        )
        .ok()
    }
    fn live_profile(&self) -> Option<playtime_runner::docker::Profile> {
        let (docker, live) = (self.docker.as_ref()?, self.live.as_ref()?);
        playtime_runner::docker::Profile::freeze_live(
            docker.selection.clone(),
            docker.daemon.clone(),
            live.host.clone(),
        )
        .ok()
    }
    pub fn live_preview(&self) -> Option<Preview> {
        let profile = self.live_profile()?;
        self.model_preview(
            &profile,
            playtime_runner::docker::LIVE_RECIPE,
            RunMode::LiveSmoke,
        )
    }
    /// A live-model run's preview. The frozen profile is its effective
    /// configuration, and the revision covers it.
    fn model_preview(
        &self,
        profile: &playtime_runner::docker::Profile,
        recipe: &str,
        mode: RunMode,
    ) -> Option<Preview> {
        let mut preview = self.docker_preview()?;
        let advertised = &self.bundle.descriptor().capabilities.docker_profile;
        preview.recipe_id = RecipeId::new(recipe).ok()?;
        preview.mode = mode;
        preview.preview_revision = PreviewRevision::new(
            digest(&serde_json::to_vec(&(profile, self.bundle.id(), advertised)).unwrap()).as_str(),
        )
        .unwrap();
        preview.effective_configuration = serde_json::to_value(profile).unwrap();
        preview.harness = Some(HarnessView::project(
            profile.harness.as_deref()?,
            &profile.scenario.parameters,
        ));
        preview.parameters = profile.scenario.parameters.clone();
        preview.prompt = profile.scenario.prompt.clone();
        preview.protocol = profile.scenario.protocol.clone();
        let model = profile.model.as_ref().unwrap();
        preview.model_identity = serde_json::json!({"adapter":"ollama","name":model.lock["name"],
            "digest":model.lock["digest"],"ollama_version":model.lock["ollama_version"],
            "settings":model.settings,"endpoint":model.host.endpoint,"addresses":model.host.addresses});
        if let Some(registration) = &model.registration {
            preview.model_identity["registration"] = serde_json::to_value(registration).unwrap();
            preview.model_identity["definition_sha256"] = model.lock["definition_sha256"].clone();
        }
        preview.warnings = vec![if model.registration.is_some() {
            "Live model: real inference on the selected registered identity. Tools execute in the locked Docker workshop. One live run at a time across checkouts."
        } else {
            "Live model: real inference on the locked model, and tools execute in the real locked Docker workshop. One live run at a time across checkouts."
        }.into()];
        preview.backend = projection::live_backend();
        Some(preview)
    }
    pub fn docker_preview(&self) -> Option<Preview> {
        let profile = self.docker.as_ref()?;
        let mut preview = self.preview();
        preview.recipe_id = RecipeId::new(playtime_runner::docker::RECIPE).unwrap();
        preview.mode = RunMode::ScriptedSmoke;
        let advertised = &self.bundle.descriptor().capabilities.docker_profile;
        preview.preview_revision = PreviewRevision::new(
            digest(&serde_json::to_vec(&(profile, self.bundle.id(), advertised)).unwrap()).as_str(),
        )
        .unwrap();
        preview.effective_configuration = serde_json::to_value(profile).unwrap();
        preview.harness = Some(HarnessView::project(
            profile.harness.as_deref()?,
            &profile.scenario.parameters,
        ));
        preview.parameters = profile.scenario.parameters.clone();
        preview.prompt = profile.scenario.prompt.clone();
        preview.protocol = profile.scenario.protocol.clone();
        preview.workshop_identity = serde_json::json!({"adapter":"docker","image":profile.image_lock,"daemon":profile.daemon,"helpers":profile.helpers});
        preview.seeds = SeedSummary {
            files: 0,
            bytes: 0,
            digests: vec![],
        };
        preview.warnings = vec!["Scripted model responses and synthetic token counts; tools execute in the real locked Docker workshop.".into()];
        preview.backend = projection::docker_backend();
        Some(preview)
    }
    pub fn preview(&self) -> Preview {
        Preview {
            harness: None,
            recipe_id: RecipeId::new("two-session").unwrap(),
            mode: RunMode::Simulation,
            preview_revision: self.revision.clone(),
            effective_configuration: serde_json::to_value(&self.scenario).unwrap(),
            parameters: self.scenario.parameters.clone(),
            prompt: self.scenario.prompt.clone(),
            protocol: self.scenario.protocol.clone(),
            model_identity: serde_json::json!({"adapter":"scripted","frozen":true}),
            workshop_identity: serde_json::json!({"adapter":"scripted","commands_execute":false}),
            seeds: SeedSummary {
                files: self.scenario.initial_files.len() as u64,
                bytes: self
                    .scenario
                    .initial_files
                    .values()
                    .map(|s| s.len() as u64)
                    .sum(),
                digests: self
                    .scenario
                    .initial_files
                    .values()
                    .map(|s| digest(s.as_bytes()).to_string())
                    .collect(),
            },
            readiness: Readiness {
                ready: true,
                reasons: vec![],
            },
            warnings: vec![
                "Simulation: model and workshop effects are scripted; commands are never executed."
                    .into(),
            ],
            backend: projection::backend(),
        }
    }
    pub fn path(&self, id: &playtime_core::RunId) -> PathBuf {
        self.root.join("runs").join(id.as_str())
    }
    pub fn saved_snapshot(
        &self,
        record: &Admission,
    ) -> life::Result<Option<(ArchiveReader, Snapshot)>> {
        let Some(confirmation) = &record.confirmed_read else {
            return Ok(None);
        };
        if confirmation.owner_generation != record.owner_generation
            || confirmation.source != ConfirmationSource::Worker
        {
            return Err(Error::Uncertain);
        }
        let prepared = record.prepared.as_ref().ok_or(Error::Uncertain)?;
        let reader = self.reader(prepared)?;
        let authorized = Authority {
            identity: confirmation.frontier.archive.clone(),
            prefix: ReadPrefix {
                sequence: confirmation.frontier.committed_sequence,
                bytes: confirmation.frontier.committed_bytes,
                sha256: confirmation.frontier.prefix_sha256.clone(),
            },
        };
        let snapshot = reader.confirmed_snapshot(&authorized).map_err(storage)?;
        if snapshot.frontier() != &confirmation.frontier {
            return Err(Error::Uncertain);
        }
        Ok(Some((reader, snapshot)))
    }
    fn reader(&self, prepared: &PreparedLaunch) -> life::Result<ArchiveReader> {
        let reader = ArchiveReader::open(
            self.path(&prepared.binding.run_id),
            StorageLimits::default(),
        )
        .map_err(storage)?;
        let expected: ArchiveIdentity = serde_json::from_slice(&read_private(
            &prepared.private_dir.join("archive-identity.json"),
            65_536,
        )?)?;
        if reader.identity() != &expected
            || expected.run_id != prepared.binding.run_id
            || expected.input_manifest.sha256 != prepared.binding.manifest_sha256
            || reader.manifest().worker.executable.sha256 != prepared.binding.executable_sha256
        {
            return Err(Error::Uncertain);
        }
        Ok(reader)
    }
    fn verify(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> life::Result<EventFrontier> {
        let reader = self.reader(prepared)?;
        let receipt = status.latest_receipt.as_ref().ok_or(Error::Uncertain)?;
        let authority = Authority {
            identity: reader.identity().clone(),
            prefix: prefix(receipt),
        };
        let snapshot = reader.confirmed_snapshot(&authority).map_err(storage)?;
        if let Some(previous) = previous {
            if receipt.sequence < previous.sequence
                || (receipt.sequence > previous.sequence && receipt.bytes <= previous.bytes)
                || (receipt.sequence == previous.sequence && receipt != previous)
            {
                return Err(Error::Uncertain);
            }
            reader
                .sub_snapshot(&snapshot, prefix(previous))
                .map_err(storage)?;
        }
        Ok(snapshot.frontier().clone())
    }
}
fn storage(_: playtime_storage::StorageError) -> Error {
    Error::Invalid("archive confirmation verification failed")
}
fn prefix(receipt: &Receipt) -> ReadPrefix {
    ReadPrefix {
        sequence: receipt.sequence,
        bytes: receipt.bytes,
        sha256: receipt.sha256.clone(),
    }
}
/// Only authenticated reply verification and saved-ledger publication construct
/// this private port. Public HTTP cursors never reach it.
struct Authority {
    identity: ArchiveIdentity,
    prefix: ReadPrefix,
}
impl ConfirmationAuthority for Authority {
    fn authorize(&self, archive: &ArchiveIdentity) -> playtime_storage::Result<ReadPrefix> {
        if archive != &self.identity {
            return Err(playtime_storage::StorageError {
                code: playtime_storage::ErrorCode::InvalidCursor,
                message: "confirmation identity mismatch".into(),
                outcome_uncertain: false,
                io_kind: None,
            });
        }
        Ok(self.prefix.clone())
    }
}
#[async_trait]
impl ArchivePort for Archive {
    async fn validate_preview(&self, request: &AdmissionRequest) -> life::Result<Selection> {
        let resolved;
        let current = if request.mode == RunMode::Simulation {
            self
        } else {
            resolved = self.execution(request.mode).await?;
            &resolved
        };
        current.validate_current_preview(request).await
    }
    async fn prepare(&self, admission: &Admission) -> life::Result<PreparedLaunch> {
        let resolved;
        let current = if admission.request.mode == RunMode::Simulation {
            self
        } else {
            resolved = self.execution(admission.request.mode).await?;
            &resolved
        };
        // Recheck the exact admission binding before creating the prepared writer.
        current.validate_current_preview(&admission.request).await?;
        current.prepare_current(admission).await
    }

    async fn verify_status(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> life::Result<()> {
        self.confirmed_read(prepared, previous, status)
            .await
            .map(|_| ())
    }
    async fn confirmed_read(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> life::Result<Option<EventFrontier>> {
        // This port is called in a coordinator-owned lane. Blocking archive work
        // stays off Tokio threads and outside ledger transactions.
        let (archive, prepared, previous, status) = (
            self.clone(),
            prepared.clone(),
            previous.cloned(),
            status.clone(),
        );
        tokio::task::spawn_blocking(move || {
            archive
                .verify(&prepared, previous.as_ref(), &status)
                .map(Some)
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn normal_completion(&self, admission: &Admission) -> life::Result<bool> {
        let (archive, admission) = (self.clone(), admission.clone());
        tokio::task::spawn_blocking(move || {
            let Some((reader, snapshot)) = archive.saved_snapshot(&admission)? else {
                return Ok(false);
            };
            let query = playtime_runner::ArchiveQueries::from_snapshot(reader, snapshot)
                .map_err(|_| Error::Uncertain)?;
            let run = query.run().map_err(|_| Error::Uncertain)?;
            Ok(!admission
                .last_status
                .as_ref()
                .is_none_or(|s| s.publication_pending)
                && run.unsettled_model_intents.is_empty()
                && run.outcome.is_some_and(|outcome| {
                    outcome.reason != StopReason::Interrupted
                        && outcome.reason != StopReason::InfrastructureFailure
                        && outcome.cleanup == playtime_protocol::events::CleanupOutcome::Complete
                }))
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn external_resources(&self, admission: &Admission) -> life::Result<ExternalResources> {
        match admission.request.mode {
            // Controller simulation workers create no external resources.
            RunMode::Simulation => Ok(ExternalResources::default()),
            // Every Docker mode's worker keeps the same ownership journal.
            RunMode::ScriptedSmoke | RunMode::LiveSmoke | RunMode::Experiment => {
                let engine = self.docker_engine.clone().ok_or(Error::Invalid(
                    "Docker is not enabled, so its resources cannot be verified",
                ))?;
                let prepared = admission.prepared.clone().ok_or(Error::Uncertain)?;
                let proposals = tokio::task::spawn_blocking(move || ownership_journal(&prepared))
                    .await
                    .map_err(|_| Error::Uncertain)??;
                let mut resources = ExternalResources::default();
                for proposal in proposals {
                    let kind = if proposal.role == playtime_docker::Role::Volume {
                        "volume"
                    } else {
                        "container"
                    };
                    let name = format!("{kind} {}", proposal.name);
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    let cancel = playtime_docker::CancellationToken::new();
                    match engine.residual(&proposal, deadline, cancel).await {
                        Ok(None) => resources.absent.push(name),
                        Ok(Some(true)) => resources.present.push(format!("{name} is present")),
                        Ok(Some(false)) => resources
                            .present
                            .push(format!("{name} is present with different labels")),
                        Err(error) => resources
                            .present
                            .push(format!("{name} could not be inspected: {error}")),
                    }
                }
                Ok(resources)
            }
        }
    }
}

/// The worker publishes one ownership record per proposed Docker resource
/// before any create, so these records bound everything it may have created.
pub(super) fn ownership_journal(
    prepared: &PreparedLaunch,
) -> life::Result<Vec<playtime_docker::Proposal>> {
    #[derive(serde::Deserialize)]
    struct Record {
        version: u32,
        binding: Binding,
        proposal: playtime_docker::Proposal,
    }
    let mut proposals = Vec::new();
    for entry in std::fs::read_dir(&prepared.private_dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let owned = name
            .strip_prefix("docker-")
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.rsplit_once('-'))
            .is_some_and(|(role, session)| {
                matches!(role, "volume" | "holder" | "agent")
                    && !session.is_empty()
                    && session.bytes().all(|b| b.is_ascii_digit())
            });
        if !owned {
            continue;
        }
        if proposals.len() == 64 {
            return Err(Error::Invalid("Docker ownership journal exceeds its bound"));
        }
        let record: Record =
            serde_json::from_slice(&read_private(&prepared.private_dir.join(name), 32_768)?)?;
        if record.version != 1 || record.binding != prepared.binding {
            return Err(Error::Invalid(
                "Docker ownership record does not match this launch",
            ));
        }
        proposals.push(record.proposal);
    }
    Ok(proposals)
}

impl Archive {
    async fn validate_current_preview(
        &self,
        request: &AdmissionRequest,
    ) -> life::Result<Selection> {
        // Busy while another live run holds the lock; then the workshop and
        // the model must still be what was frozen.
        let live_ready = || async {
            let live = self.live.as_ref().ok_or(Error::Uncertain)?;
            let path = live.host.live_lock.clone();
            tokio::task::spawn_blocking(move || probe_live_lock(&path))
                .await
                .map_err(|_| Error::Uncertain)??;
            Ok::<_, Error>(())
        };
        let preview = if request.recipe_id == playtime_runner::docker::RECIPE
            && request.mode == RunMode::ScriptedSmoke
        {
            self.docker_preview()
                .ok_or(Error::Invalid("unsupported recipe or mode"))?
        } else if request.recipe_id == playtime_runner::docker::LIVE_RECIPE
            && request.mode == RunMode::LiveSmoke
        {
            let preview = self
                .live_preview()
                .ok_or(Error::Invalid("unsupported recipe or mode"))?;
            live_ready().await?;
            preview
        } else if request.mode == RunMode::Experiment
            && (self.recipes.contains_key(&request.recipe_id)
                || request.recipe_id.starts_with("saved-"))
        {
            let preview = self.authored_preview(&request.recipe_id).await?;
            live_ready().await?;
            preview
        } else if request.recipe_id == "two-session" && request.mode == RunMode::Simulation {
            self.preview()
        } else {
            return Err(Error::Invalid("unsupported recipe or mode"));
        };
        if request.preview_revision != preview.preview_revision.as_str() {
            return Err(Error::Invalid("stale preview"));
        }
        Ok(Selection {
            executable_sha256: self.bundle.descriptor().worker.executable.sha256.clone(),
            wall_ms: preview.parameters.limits.wall_seconds.get() * 1000,
            token_limit: preview.parameters.limits.generated_tokens.get(),
        })
    }
    async fn prepare_current(&self, admission: &Admission) -> life::Result<PreparedLaunch> {
        // A Docker mode never falls back to simulation.
        let docker = match launch(&admission.request)? {
            Launch::Simulation => None,
            Launch::ScriptedSmoke => Some(
                self.docker
                    .clone()
                    .map(|p| playtime_runner::docker::NativeProfile::Memento(Box::new(p))),
            ),
            Launch::LiveSmoke => Some(
                self.live_profile()
                    .map(|p| playtime_runner::docker::NativeProfile::Memento(Box::new(p))),
            ),
            Launch::Catalog(id) => Some(Some(self.authored_profile(id).await?)),
        }
        .map(|profile| profile.ok_or(Error::Invalid("unsupported recipe or mode")))
        .transpose()?;
        let (root, bundle, bytes, admission) = (
            self.root.clone(),
            self.bundle.clone(),
            self.bytes.clone(),
            admission.clone(),
        );
        tokio::task::spawn_blocking(move || {
            create_private_dir(&root.join("runs"))?;
            let private = root.join(".playtime/workers");
            create_private_dir(&private)?;
            let executable = bundle
                .materialize(&private, &admission.run_id)
                .map_err(|_| Error::Invalid("worker materialization failed"))?;
            let directory = executable.parent().ok_or(Error::Uncertain)?.to_path_buf();
            let path = root.join("runs").join(admission.run_id.as_str());
            let kind = match &docker {
                Some(profile) if profile.model().is_some() => WorkerKind::DockerLive,
                Some(_) => WorkerKind::DockerScripted,
                None => WorkerKind::ControllerSimulation,
            };
            let prepared = if let Some(profile) = &docker {
                match profile {
                    playtime_runner::docker::NativeProfile::Memento(p) => (*bundle)
                        .clone()
                        .prepare_docker(&path, admission.run_id.clone(), p),
                    playtime_runner::docker::NativeProfile::Continuous(p) => (*bundle)
                        .clone()
                        .prepare_continuous(&path, admission.run_id.clone(), p),
                }
            } else {
                (*bundle)
                    .clone()
                    .prepare(&path, admission.run_id.clone(), &bytes)
            }
            .map_err(|_| Error::Invalid("archive preparation failed"))?;
            let invocation_id = random_id()?;
            let binding = Binding {
                service_id: admission.service_id,
                run_id: admission.run_id,
                manifest_sha256: prepared.identity.input_manifest.sha256.clone(),
                executable_sha256: admission.selection.executable_sha256,
                grant_id: random_id()?,
                worker_id: random_id()?,
                unit: format!("playtime-{}.service", invocation_id.as_str()),
                invocation_id,
                boot_id: boot_id()?,
                kind,
            };
            let socket = directory.join("control.sock");
            if socket.as_os_str().len() >= 108 {
                return Err(Error::Invalid("private socket path exceeds Unix limit"));
            }
            let bootstrap_path = directory.join("bootstrap.json");
            let bootstrap = Bootstrap {
                version: worker::VERSION,
                binding: binding.clone(),
                credential: Credential::generate()?,
                root,
                private_dir: directory.clone(),
                socket: socket.clone(),
                startup_deadline_boot_ms: boot_ms() + 60_000,
                wall_ms: admission.selection.wall_ms,
            };
            write_once(
                &directory.join("archive-identity.json"),
                &serde_json::to_vec(&prepared.identity)?,
            )?;
            write_once(
                &directory.join("reservation.json"),
                &serde_json::to_vec(&binding)?,
            )?;
            write_once(&bootstrap_path, &serde_json::to_vec(&bootstrap)?)?;
            let argv = vec![
                "--bootstrap".into(),
                bootstrap_path
                    .to_str()
                    .ok_or(Error::Invalid("UTF-8 root required"))?
                    .into(),
            ];
            Ok(PreparedLaunch {
                binding,
                executable,
                argv,
                bootstrap: bootstrap_path,
                private_dir: directory,
                socket,
                startup_deadline_boot_ms: bootstrap.startup_deadline_boot_ms,
            })
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
}
