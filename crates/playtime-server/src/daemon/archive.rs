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
    Bootstrap, Credential, boot_id, boot_ms, create_private_dir, digest, random_id, read_private,
    write_once,
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
        if !self.bundle.descriptor().capabilities.real_workshop
            || self.bundle.descriptor().capabilities.real_model
        {
            return Err(Error::Invalid(
                "bundle does not support Docker scripted execution",
            ));
        }
        let cli = Cli::resolve(
            executable,
            socket,
            &self.root.join(format!(
                ".playtime/docker-catalog-{}",
                random_id()?.as_str()
            )),
        )
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
    pub fn docker_preview(&self) -> Option<Preview> {
        let profile = self.docker.as_ref()?;
        let mut preview = self.preview();
        preview.recipe_id = RecipeId::new(playtime_runner::docker::RECIPE).unwrap();
        preview.mode = RunMode::ScriptedSmoke;
        preview.preview_revision = PreviewRevision::new(
            digest(&serde_json::to_vec(&(profile, self.bundle.id())).unwrap()).as_str(),
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
        let preview = if request.recipe_id == playtime_runner::docker::RECIPE
            && request.mode == RunMode::ScriptedSmoke
        {
            let preview = self
                .docker_preview()
                .ok_or(Error::Invalid("unsupported recipe or mode"))?;
            self.docker_engine
                .as_ref()
                .ok_or(Error::Uncertain)?
                .readiness(
                    playtime_runner::docker::IMAGE_LOCK,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(20),
                    playtime_docker::CancellationToken::new(),
                )
                .await
                .map_err(|_| Error::Invalid("locked Docker readiness changed"))?;
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
        let (root, bundle, bytes, admission, docker) = (
            self.root.clone(),
            self.bundle.clone(),
            self.bytes.clone(),
            admission.clone(),
            if admission.request.mode == RunMode::ScriptedSmoke {
                self.docker.clone()
            } else {
                None
            },
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
            let kind = if docker.is_some() {
                WorkerKind::DockerScripted
            } else {
                WorkerKind::ControllerSimulation
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
}
