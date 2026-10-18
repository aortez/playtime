//! Shared producer/worker verification. Package authenticity/static ELF checks
//! remain the production bundle owner's job; these checks bind its advertised
//! profile and exact frozen inputs to this compiled controller implementation.
use crate::{Error, Result, prepare::reference};
use playtime_engine::simulation::Scenario;
use playtime_protocol::{
    archive::InputManifest,
    recovery::{self, ExecutionContractV1, RecoveryRecord},
    worker_bundle::{self, BuildRecord, BundleDescriptor},
};
use playtime_storage::{ArchiveReader, Snapshot, StorageLimits};

pub fn contract_for(
    manifest: &InputManifest,
    scenario: &Scenario,
    limits: &StorageLimits,
) -> ExecutionContractV1 {
    ExecutionContractV1 {
        schema_version: 1,
        rule_set: recovery::RULE_SET.into(),
        event_schema_version: 1,
        bridge_schema_version: 1,
        observation_schema_version: 1,
        required_event_kinds: recovery::REQUIRED_EVENTS
            .iter()
            .map(|s| (*s).into())
            .collect(),
        effect_kinds: recovery::EFFECT_KINDS.iter().map(|s| (*s).into()).collect(),
        worker_executable: manifest.worker.executable.clone(),
        configuration: manifest.configuration.clone(),
        generated_tokens: scenario.parameters.limits.generated_tokens,
        log_bytes: scenario.parameters.limits.log_bytes,
        storage_limits: limits.clone(),
        finalization_reserve_bytes: playtime_protocol::storage::FINALIZATION_RESERVE_BYTES,
        observation_accounting: recovery::OBSERVATION_ACCOUNTING.into(),
    }
}

pub fn verify_bundle_support(
    manifest: &InputManifest,
    mut read: impl FnMut(&playtime_protocol::archive::ContentRef) -> Result<Vec<u8>>,
) -> Result<()> {
    let locate = |path: &str| -> Result<&playtime_protocol::archive::ContentRef> {
        let mut found = manifest.files.iter().filter(|r| r.path.as_str() == path);
        let first = found.next().ok_or_else(|| {
            Error::Invalid("recovery requires frozen bundle capability metadata".into())
        })?;
        if found.next().is_some() {
            return Err(Error::Invalid("duplicate recovery capability input".into()));
        }
        Ok(first)
    };
    let descriptor_ref = locate(worker_bundle::DESCRIPTOR)?;
    let build_ref = locate(worker_bundle::BUILD)?;
    let descriptor_bytes = read(descriptor_ref)?;
    let build_bytes = read(build_ref)?;
    let descriptor: BundleDescriptor = serde_json::from_slice(&descriptor_bytes)?;
    let build: BuildRecord = serde_json::from_slice(&build_bytes)?;
    if reference(descriptor_ref.path.as_str(), &descriptor_bytes) != *descriptor_ref
        || reference(build_ref.path.as_str(), &build_bytes) != *build_ref
        || descriptor.schema_version != 1
        || build.schema_version != 1
        || descriptor.worker != manifest.worker
        || descriptor.build_record != *build_ref
        || !descriptor.capabilities.supported()
        || descriptor.capabilities.recovery_profile.as_deref() != Some(recovery::RULE_SET)
        || build.capabilities != descriptor.capabilities
        || build.provenance != manifest.worker.build
        || build.source_sha256 != manifest.worker.source.sha256
        || build.lock_sha256 != manifest.worker.cargo_lock.sha256
    {
        return Err(Error::Invalid(
            "unsupported or conflicting frozen recovery capability".into(),
        ));
    }
    Ok(())
}

/// Existing unadvertised archives can execute through their ordinary path. If
/// a contract is present it must pass independent verification before creation
/// of the controller. Missing/tampered manifest inputs already fail storage.
pub(crate) fn verify_worker_contract(
    reader: &ArchiveReader,
    snapshot: &Snapshot,
    scenario: &Scenario,
    limits: &StorageLimits,
) -> Result<()> {
    let mut contracts = reader
        .manifest()
        .files
        .iter()
        .filter(|r| r.path.as_str() == recovery::EXECUTION_CONTRACT);
    let Some(contract_ref) = contracts.next() else {
        return Ok(());
    };
    if contracts.next().is_some() {
        return Err(Error::Invalid("duplicate execution contract".into()));
    }
    let bytes = reader.read_content(snapshot, contract_ref)?;
    let contract: ExecutionContractV1 = recovery::decode(&bytes, limits.manifest_bytes)?;
    contract.validate()?;
    if contract != contract_for(reader.manifest(), scenario, limits) {
        return Err(Error::Invalid(
            "execution contract differs from frozen worker/configuration/limits".into(),
        ));
    }
    verify_bundle_support(reader.manifest(), |r| Ok(reader.read_content(snapshot, r)?))
}
