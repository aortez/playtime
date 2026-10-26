use std::{fs::File, io::Read, path::Path};

use playtime_core::{ArchivePath, RunId, RunMode};
use playtime_engine::simulation::{Scenario, ScriptedAdapters};
use playtime_protocol::{
    archive::{BuildProvenance, ContentRef, InputManifest, PlatformRequirements, WorkerIdentity},
    storage::{ArchiveDescriptor, ArchiveIdentity},
};
use playtime_storage::{ArchiveWriter, FrozenInput, StorageLimits, content_ref};

use crate::{Error, Result};

const SOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/source.json"));
const LOCK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/Cargo.lock"));
const BUILD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/build.json"));

#[derive(Clone, Debug)]
pub struct PreparedSimulation {
    pub identity: ArchiveIdentity,
    pub limits: StorageLimits,
}

/// An explicitly supplied worker package. The preparing host validates its
/// authority/provenance; this layer freezes and verifies the exact supplied
/// bytes. It never substitutes the host executable for the designated worker.
pub struct WorkerBundle {
    pub executable: Vec<u8>,
    pub source: Vec<u8>,
    pub cargo_lock: Vec<u8>,
    pub build: BuildProvenance,
    pub platform: PlatformRequirements,
}

/// Demo-only self capture. Production preparation supplies an independently
/// validated worker bundle built from an isolated source snapshot (#20).
pub fn capture_demo_worker() -> Result<WorkerBundle> {
    Ok(WorkerBundle {
        executable: executable(StorageLimits::default().content_bytes)?,
        source: SOURCE.to_vec(),
        cargo_lock: LOCK.to_vec(),
        build: serde_json::from_slice::<BuildProvenance>(BUILD)?,
        platform: PlatformRequirements {
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            abi: env!("PLAYTIME_WORKER_TARGET").into(),
            requirements: vec![
                "CPU-only scripted simulation; Linux durable filesystem operations".into(),
            ],
        },
    })
}

pub(crate) fn reference(path: &str, bytes: &[u8]) -> ContentRef {
    content_ref(
        ArchivePath::new(path).expect("driver-generated archive path"),
        bytes,
    )
}

pub(crate) fn executable(bound: u64) -> Result<Vec<u8>> {
    // Reopening current_exe() can read a replacement pathname or fail after
    // unlink. /proc/self/exe holds the actual executing image on Linux.
    let file = File::open("/proc/self/exe")?;
    if file.metadata()?.len() > bound {
        return Err(Error::Capacity(
            "executing binary exceeds frozen content bound".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(bound.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > bound {
        return Err(Error::Capacity(
            "executing binary grew beyond content bound".into(),
        ));
    }
    Ok(bytes)
}

/// Freeze the scenario and explicitly supplied worker bundle. This prepares
/// an empty archive and releases its lock; it does not authorize or launch work.
pub fn prepare_simulation(
    path: impl AsRef<Path>,
    run_id: RunId,
    scenario_bytes: &[u8],
    bundle: WorkerBundle,
) -> Result<PreparedSimulation> {
    prepare_simulation_with_inputs(path, run_id, scenario_bytes, bundle, Vec::new())
}

/// Freeze additional manifest-bound inputs (for example, bundle provenance).
/// Storage verifies their identities, unique paths and aggregate limits along
/// with all other inputs before publishing the prepared archive.
pub fn prepare_simulation_with_inputs(
    path: impl AsRef<Path>,
    run_id: RunId,
    scenario_bytes: &[u8],
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
) -> Result<PreparedSimulation> {
    prepare(path.as_ref(), run_id, scenario_bytes, bundle, extra, false)
}

/// Explicit recovery opt-in. The caller supplies its validated package; its
/// frozen descriptor/build metadata must advertise this compiled profile.
/// Arbitrary old bundles and manually inserted contracts are rejected.
pub fn prepare_recoverable_simulation_with_inputs(
    path: impl AsRef<Path>,
    run_id: RunId,
    scenario_bytes: &[u8],
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
) -> Result<PreparedSimulation> {
    prepare(path.as_ref(), run_id, scenario_bytes, bundle, extra, true)
}

fn prepare(
    path: &Path,
    run_id: RunId,
    scenario_bytes: &[u8],
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
    recoverable: bool,
) -> Result<PreparedSimulation> {
    if extra
        .iter()
        .any(|i| i.reference.path.as_str() == playtime_protocol::recovery::EXECUTION_CONTRACT)
    {
        return Err(Error::Invalid(
            "execution contract is reserved for explicit recoverable preparation".into(),
        ));
    }
    if scenario_bytes.len() as u64 > StorageLimits::default().content_bytes {
        return Err(Error::Capacity(
            "scenario exceeds frozen content bound".into(),
        ));
    }
    if bundle.executable.is_empty() || bundle.source.is_empty() || bundle.cargo_lock.is_empty() {
        return Err(Error::Invalid(
            "worker bundle requires executable, source, and lock bytes".into(),
        ));
    }
    let scenario: Scenario = serde_json::from_slice(scenario_bytes)?;
    let adapters = ScriptedAdapters::new(&scenario)?;
    let limits = StorageLimits {
        event_log_bytes: scenario.parameters.limits.log_bytes.get(),
        ..StorageLimits::default()
    };
    freeze_archive(
        path,
        run_id,
        scenario_bytes,
        bundle,
        extra,
        recoverable,
        Some(&scenario),
        RunMode::Simulation,
        adapters.config().initial_workspace.clone(),
        adapters.artifacts()[adapters.config().initial_workspace.path.as_str()].clone(),
        limits,
    )
}

pub fn prepare_docker_with_inputs(
    path: &Path,
    run_id: RunId,
    profile: &crate::docker::Profile,
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
) -> Result<PreparedSimulation> {
    profile.validate()?;
    if extra
        .iter()
        .any(|i| i.reference.path.as_str() == playtime_protocol::recovery::EXECUTION_CONTRACT)
    {
        return Err(Error::Invalid(
            "simulation recovery contract cannot authorize Docker".into(),
        ));
    }
    freeze_archive(
        path,
        run_id,
        &serde_json::to_vec(profile)?,
        bundle,
        extra,
        false,
        Some(&profile.scenario),
        profile.mode,
        profile.config().initial_workspace,
        crate::docker::Profile::initial_bytes(),
        profile.storage.clone(),
    )
}

pub fn prepare_continuous_with_inputs(
    path: &Path,
    run_id: RunId,
    profile: &crate::docker::ContinuousProfile,
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
) -> Result<PreparedSimulation> {
    profile.validate()?;
    if extra
        .iter()
        .any(|i| i.reference.path.as_str() == playtime_protocol::recovery::EXECUTION_CONTRACT)
    {
        return Err(Error::Invalid(
            "simulation recovery cannot authorize continuous execution".into(),
        ));
    }
    freeze_archive(
        path,
        run_id,
        &serde_json::to_vec(profile)?,
        bundle,
        extra,
        false,
        None,
        profile.mode,
        profile.config.initial_workspace.clone(),
        crate::docker::Profile::initial_bytes(),
        profile.storage.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn freeze_archive(
    path: &Path,
    run_id: RunId,
    scenario_bytes: &[u8],
    bundle: WorkerBundle,
    extra: Vec<FrozenInput>,
    recoverable: bool,
    scenario: Option<&Scenario>,
    mode: RunMode,
    initial_workspace: ContentRef,
    initial_bytes: Vec<u8>,
    limits: StorageLimits,
) -> Result<PreparedSimulation> {
    let mut inputs = Vec::new();
    let mut freeze = |path: &str, bytes: Vec<u8>| {
        let reference = reference(path, &bytes);
        inputs.push(FrozenInput {
            reference: reference.clone(),
            bytes,
        });
        reference
    };
    let configuration = freeze("inputs/scenario.json", scenario_bytes.to_vec());
    freeze(initial_workspace.path.as_str(), initial_bytes);
    let worker = WorkerIdentity {
        executable: freeze("inputs/worker/executable", bundle.executable),
        source: freeze("inputs/worker/source.bundle", bundle.source),
        cargo_lock: freeze("inputs/worker/Cargo.lock", bundle.cargo_lock),
        build: bundle.build,
        platform: bundle.platform,
    };
    let files = extra.iter().map(|input| input.reference.clone()).collect();
    inputs.extend(extra);
    let mut manifest = InputManifest {
        schema_version: if scenario.is_some() { 1 } else { 2 },
        run_id: run_id.clone(),
        worker,
        configuration,
        initial_workspace,
        files,
    };
    if recoverable {
        crate::recovery::verify_bundle_support(&manifest, |r| {
            inputs
                .iter()
                .find(|i| &i.reference == r)
                .map(|i| i.bytes.clone())
                .ok_or_else(|| Error::Invalid("missing recovery capability bytes".into()))
        })?;
        let contract = crate::recovery::contract_for(
            &manifest,
            scenario
                .ok_or_else(|| Error::Invalid("recovery requires a memento scenario".into()))?,
            &limits,
        );
        use playtime_protocol::recovery::RecoveryRecord;
        contract.validate()?;
        let bytes = serde_json::to_vec(&contract)?;
        let reference = reference(playtime_protocol::recovery::EXECUTION_CONTRACT, &bytes);
        manifest.files.push(reference.clone());
        inputs.push(FrozenInput { reference, bytes });
    }
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let descriptor = ArchiveDescriptor {
        schema_version: 1,
        run_id,
        mode,
        input_manifest: reference("inputs/manifest.json", &manifest_bytes),
    };
    let writer = ArchiveWriter::create(path, descriptor, &manifest_bytes, &inputs, limits.clone())?;
    Ok(PreparedSimulation {
        identity: writer.confirmed_frontier().frontier().archive.clone(),
        limits,
    })
}
