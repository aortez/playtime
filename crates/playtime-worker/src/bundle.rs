use crate::{Error, PrivateDirectory, Result, verify_static_elf};
use playtime_core::{ArchivePath, RunId, Sha256Digest};
use playtime_protocol::{
    archive::ContentRef,
    worker_bundle::{self as wire, BuildRecord, BundleDescriptor},
};
use playtime_runner::{PreparedSimulation, WorkerBundle, prepare_simulation_with_inputs};
use playtime_storage::{ArchiveReader, FrozenInput, StorageLimits, content_ref, sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const EXECUTABLE: &str = "inputs/worker/executable";
pub const SOURCE: &str = "inputs/worker/source.bundle";
pub const LOCK: &str = "inputs/worker/Cargo.lock";

/// In-memory validated bytes. No mutable bundle pathname is retained as identity.
#[derive(Clone)]
pub struct VerifiedBundle {
    descriptor: BundleDescriptor,
    descriptor_bytes: Vec<u8>,
    build: BuildRecord,
    files: BTreeMap<String, Vec<u8>>,
}
impl VerifiedBundle {
    pub fn open(path: &Path) -> Result<Self> {
        let root = PrivateDirectory::open(path)?;
        let limits = StorageLimits::default();
        let descriptor = root.read("bundle.json", limits.manifest_bytes)?;
        Self::read(descriptor, |reference| {
            root.read(reference.path.as_str(), limits.content_bytes)
        })
    }
    /// Input bytes are already storage-validated. This does not confer archive
    /// read or execution authority, and does not rediscover a commit marker.
    pub fn from_archive(
        reader: &ArchiveReader,
        snapshot: &playtime_storage::Snapshot,
    ) -> Result<Self> {
        let reference = reader
            .manifest()
            .files
            .iter()
            .find(|r| r.path.as_str() == wire::DESCRIPTOR)
            .ok_or(Error::Invalid("missing frozen bundle descriptor"))?;
        let descriptor = reader.read_content(snapshot, reference)?;
        let value = Self::read(descriptor, |reference| {
            Ok(reader.read_content(snapshot, reference)?)
        })?;
        if value.descriptor.worker != reader.manifest().worker {
            return Err(Error::Invalid("manifest and bundle worker differ"));
        }
        Ok(value)
    }
    fn read(
        descriptor_bytes: Vec<u8>,
        mut read: impl FnMut(&ContentRef) -> Result<Vec<u8>>,
    ) -> Result<Self> {
        let descriptor: BundleDescriptor = serde_json::from_slice(&descriptor_bytes)?;
        if descriptor.schema_version != wire::VERSION {
            return Err(Error::UnsupportedCapability);
        }
        let worker = &descriptor.worker;
        if worker.platform.os != "linux"
            || worker.platform.architecture != "x86_64"
            || worker.platform.abi != wire::TARGET
            || std::env::consts::OS != "linux"
            || std::env::consts::ARCH != "x86_64"
        {
            return Err(Error::UnsupportedPlatform);
        }
        if !descriptor.capabilities.supported() {
            return Err(Error::UnsupportedCapability);
        }
        let refs = [
            (&worker.executable, EXECUTABLE),
            (&worker.source, SOURCE),
            (&worker.cargo_lock, LOCK),
            (&descriptor.build_record, wire::BUILD),
        ];
        let limits = StorageLimits::default();
        let mut total = descriptor_bytes.len() as u64;
        let mut files = BTreeMap::new();
        for (reference, path) in refs {
            if reference.path.as_str() != path {
                return Err(Error::Invalid("unexpected bundle object path"));
            }
            total = total
                .checked_add(reference.size_bytes)
                .ok_or(Error::Capacity)?;
            if reference.size_bytes == 0
                || reference.size_bytes > limits.content_bytes
                || total > limits.verified_content_bytes
            {
                return Err(Error::Capacity);
            }
            let bytes = read(reference)?;
            if content_ref(reference.path.clone(), &bytes) != *reference {
                return Err(Error::Invalid("bundle object digest/length mismatch"));
            }
            files.insert(path.into(), bytes);
        }
        verify_static_elf(&files[EXECUTABLE])?;
        let build: BuildRecord = serde_json::from_slice(&files[wire::BUILD])?;
        if build.schema_version != wire::VERSION
            || build.source_sha256 != worker.source.sha256
            || build.lock_sha256 != worker.cargo_lock.sha256
            || build.provenance != worker.build
            || build.provenance.target_triple != wire::TARGET
            || build.provenance.cargo_profile != "release"
            || build.capabilities != descriptor.capabilities
        {
            return Err(Error::Invalid("bundle build identity mismatch"));
        }
        Ok(Self {
            descriptor,
            descriptor_bytes,
            build,
            files,
        })
    }
    pub fn id(&self) -> Sha256Digest {
        sha256(&self.descriptor_bytes)
    }
    pub fn descriptor(&self) -> &BundleDescriptor {
        &self.descriptor
    }
    pub fn verify_embedded(&self, bytes: &[u8]) -> Result<()> {
        let build: Option<BuildRecord> = serde_json::from_slice(bytes)?;
        if build.as_ref() != Some(&self.build)
            || self.build.capture_helpers != playtime_docker::capture::helper_digests()
        {
            return Err(Error::Unbundled);
        }
        Ok(())
    }
    pub fn prepare(
        self,
        path: &Path,
        run_id: RunId,
        scenario: &[u8],
    ) -> Result<PreparedSimulation> {
        self.prepare_inner(path, run_id, scenario, false)
    }
    /// Advertise recovery only for this validated bundle's supported capability.
    pub fn prepare_recoverable(
        self,
        path: &Path,
        run_id: RunId,
        scenario: &[u8],
    ) -> Result<PreparedSimulation> {
        self.prepare_inner(path, run_id, scenario, true)
    }
    fn prepare_inner(
        self,
        path: &Path,
        run_id: RunId,
        scenario: &[u8],
        recoverable: bool,
    ) -> Result<PreparedSimulation> {
        let Self {
            descriptor,
            descriptor_bytes,
            mut files,
            ..
        } = self;
        let extra = vec![
            frozen(wire::DESCRIPTOR, descriptor_bytes),
            frozen(wire::BUILD, files.remove(wire::BUILD).unwrap()),
        ];
        let prepare = if recoverable {
            playtime_runner::prepare_recoverable_simulation_with_inputs
        } else {
            prepare_simulation_with_inputs
        };
        Ok(prepare(
            path,
            run_id,
            scenario,
            WorkerBundle {
                executable: files.remove(EXECUTABLE).unwrap(),
                source: files.remove(SOURCE).unwrap(),
                cargo_lock: files.remove(LOCK).unwrap(),
                build: descriptor.worker.build,
                platform: descriptor.worker.platform,
            },
            extra,
        )?)
    }
    /// Caller retains this owned copy until its invocation is confirmed gone.
    /// There is deliberately no drop-time cleanup that might remove a live image.
    pub fn materialize(&self, parent: &Path, run_id: &RunId) -> Result<PathBuf> {
        let parent_dir = PrivateDirectory::open(parent)?;
        parent_dir.require_private()?;
        let directory = parent_dir.create(run_id.as_str())?;
        directory.write_new("playtime-worker", &self.files[EXECUTABLE], true)?;
        Ok(std::path::absolute(parent)?
            .join(run_id.as_str())
            .join("playtime-worker"))
    }
}
pub fn reference(path: &str, bytes: &[u8]) -> ContentRef {
    content_ref(ArchivePath::new(path).expect("fixed package path"), bytes)
}
fn frozen(path: &str, bytes: Vec<u8>) -> FrozenInput {
    FrozenInput {
        reference: reference(path, &bytes),
        bytes,
    }
}
