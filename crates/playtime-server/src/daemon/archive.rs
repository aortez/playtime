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
}
/// The live model as the daemon reaches it: host bindings frozen at startup
/// and an adapter for read-only identity checks at admission.
pub(super) struct Live {
    pub host: playtime_runner::docker::LiveHost,
    endpoint: playtime_ollama::Endpoint,
    adapter: tokio::sync::Mutex<playtime_ollama::Adapter<playtime_ollama::NativeTransport>>,
}
impl Live {
    /// Same addresses as at startup, and the locked model still identifies
    /// itself exactly. Metadata only; never inference.
    async fn ready(&self) -> life::Result<()> {
        if resolve(&self.endpoint).await? != self.host.addresses {
            return Err(Error::Invalid("live model unavailable"));
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        self.adapter
            .lock()
            .await
            .inspect(false, deadline, &playtime_ollama::CancellationToken::new())
            .await
            .map(|_| ())
            .map_err(|_| Error::Invalid("live model unavailable"))
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
        let cli = Cli::resolve(executable, socket, &docker_config(&self.root)?)
            .map_err(|_| Error::Invalid("invalid Docker executable/socket selection"))?;
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
        if self.docker.is_none() {
            return Err(Error::Invalid("live runs need the Docker workshop"));
        }
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
        let addresses = resolve(&endpoint).await?;
        let limits = playtime_ollama::Limits::default();
        let transport = playtime_ollama::NativeTransport::with_resolution(
            endpoint.clone(),
            playtime_ollama::Trust::BundledWebPki,
            playtime_ollama::Resolution::Pinned {
                addresses: addresses.clone(),
            },
            &limits,
        )
        .map_err(|_| Error::Invalid("live model unavailable"))?;
        let lock = playtime_ollama::ModelLock::parse(playtime_runner::docker::MODEL_LOCK)
            .map_err(|_| Error::Invalid("invalid model lock"))?;
        let adapter = playtime_ollama::Adapter::new(transport, lock, limits)
            .map_err(|_| Error::Invalid("live model unavailable"))?;
        let live = Live {
            host: playtime_runner::docker::LiveHost {
                endpoint: endpoint.as_str().into(),
                addresses,
                live_lock: directory.join("live-model.lock"),
            },
            endpoint,
            adapter: tokio::sync::Mutex::new(adapter),
        };
        live.ready().await?;
        self.live = Some(Arc::new(live));
        Ok(())
    }
    /// Catalog recipes run on the live model, so this needs `enable_live` first.
    pub fn enable_recipes(
        &mut self,
        recipes: Vec<playtime_runner::docker::Recipe>,
    ) -> life::Result<()> {
        if recipes.is_empty() {
            return Ok(());
        }
        if self.live.is_none() {
            return Err(Error::Invalid("catalog recipes need the live model"));
        }
        for recipe in recipes {
            self.recipes.insert(recipe.id.clone(), recipe);
        }
        if self
            .recipes
            .keys()
            .any(|id| self.recipe_profile(id).is_none())
        {
            return Err(Error::Invalid("invalid catalog recipe"));
        }
        Ok(())
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
    /// The effective recipe: the profile frozen at admission, seeds included.
    pub fn recipe_preview(&self, id: &str) -> Option<Preview> {
        let profile = self.recipe_profile(id)?;
        let mut preview = self.model_preview(&profile, id, RunMode::Experiment)?;
        let seeds = &profile.recipe.as_ref()?.seeds;
        preview.seeds = SeedSummary {
            files: seeds.len() as u64,
            bytes: seeds.values().map(|s| s.len() as u64).sum(),
            digests: seeds
                .values()
                .map(|s| digest(s.as_bytes()).to_string())
                .collect(),
        };
        Some(preview)
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
        preview.parameters = profile.scenario.parameters.clone();
        preview.prompt = profile.scenario.prompt.clone();
        preview.protocol = profile.scenario.protocol.clone();
        let model = profile.model.as_ref().unwrap();
        preview.model_identity = serde_json::json!({"adapter":"ollama","name":model.lock["name"],
            "digest":model.lock["digest"],"ollama_version":model.lock["ollama_version"],
            "settings":model.settings,"endpoint":model.host.endpoint,"addresses":model.host.addresses});
        preview.warnings = vec!["Live model: real inference on the locked model, and tools execute in the real locked Docker workshop. One live run at a time across checkouts.".into()];
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
        let docker_ready = || async {
            self.docker_engine
                .as_ref()
                .ok_or(Error::Uncertain)?
                .readiness(
                    playtime_runner::docker::IMAGE_LOCK,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(20),
                    playtime_docker::CancellationToken::new(),
                )
                .await
                .map_err(|_| Error::Invalid("locked Docker readiness changed"))
        };
        // Busy while another live run holds the lock; then the workshop and
        // the model must still be what was frozen.
        let live_ready = || async {
            let live = self.live.as_ref().ok_or(Error::Uncertain)?;
            let path = live.host.live_lock.clone();
            tokio::task::spawn_blocking(move || probe_live_lock(&path))
                .await
                .map_err(|_| Error::Uncertain)??;
            docker_ready().await?;
            live.ready().await
        };
        let preview = if request.recipe_id == playtime_runner::docker::RECIPE
            && request.mode == RunMode::ScriptedSmoke
        {
            let preview = self
                .docker_preview()
                .ok_or(Error::Invalid("unsupported recipe or mode"))?;
            docker_ready().await?;
            preview
        } else if request.recipe_id == playtime_runner::docker::LIVE_RECIPE
            && request.mode == RunMode::LiveSmoke
        {
            let preview = self
                .live_preview()
                .ok_or(Error::Invalid("unsupported recipe or mode"))?;
            live_ready().await?;
            preview
        } else if request.mode == RunMode::Experiment
            && self.recipes.contains_key(&request.recipe_id)
        {
            let preview = self
                .recipe_preview(&request.recipe_id)
                .ok_or(Error::Invalid("unsupported recipe or mode"))?;
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
    async fn prepare(&self, admission: &Admission) -> life::Result<PreparedLaunch> {
        // A Docker mode never falls back to simulation.
        let docker = match launch(&admission.request)? {
            Launch::Simulation => None,
            Launch::ScriptedSmoke => Some(self.docker.clone()),
            Launch::LiveSmoke => Some(self.live_profile()),
            Launch::Catalog(id) => Some(self.recipe_profile(id)),
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
                Some(profile) if profile.model.is_some() => WorkerKind::DockerLive,
                Some(_) => WorkerKind::DockerScripted,
                None => WorkerKind::ControllerSimulation,
            };
            let prepared = if let Some(profile) = &docker {
                (*bundle)
                    .clone()
                    .prepare_docker(&path, admission.run_id.clone(), profile)
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
