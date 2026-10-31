//! CPU-only package gate: actual bundled worker/controller/storage, scripted
//! external effects, private transport. No production daemon or systemd claim.
use playtime_core::RunId;
use playtime_protocol::{
    worker::{self, Binding, Command, Status, WorkerKind, WorkerState},
    worker_bundle::{self as wire, BuildRecord, BundleDescriptor},
};
use playtime_runner::{ArchiveQueries, PreparedSimulation};
use playtime_storage::{ArchiveReader, ArchiveWriter, ErrorCode, StorageLimits};
use playtime_worker::{
    PrivateDirectory,
    bundle::{EXECUTABLE, LOCK, SOURCE, VerifiedBundle, reference},
};
use playtime_worker_control::{
    Bootstrap, Client, Credential, Endpoint, Grant, boot_id, boot_ms, create_private_dir,
    random_id, write_once,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn copy(source: &Path, target: &Path) -> Result<()> {
    create_private_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy(&entry.path(), &target.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        }
    }
    Ok(())
}
fn overwrite(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    fs::write(path, bytes)?;
    Ok(())
}
struct Case {
    root: PathBuf,
    archive: PathBuf,
    image: PathBuf,
    prepared: PreparedSimulation,
    bootstrap: Bootstrap,
    child: Option<Child>,
}
impl Drop for Case {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Case {
    fn new(bundle: &Path, forge_build: bool) -> Result<Self> {
        let root = PathBuf::from(format!("/tmp/pt-w-{}", &random_id()?.as_str()[..12]));
        create_private_dir(&root)?;
        for dir in ["runs", ".playtime", "private", "executables"] {
            create_private_dir(&root.join(dir))?;
        }
        let original = root.join("source-bundle");
        copy(bundle, &original)?;
        if forge_build {
            let mut descriptor: BundleDescriptor =
                serde_json::from_slice(&fs::read(original.join("bundle.json"))?)?;
            let mut build: BuildRecord =
                serde_json::from_slice(&fs::read(original.join(wire::BUILD))?)?;
            build.provenance.dirty = !build.provenance.dirty;
            descriptor.worker.build = build.provenance.clone();
            let bytes = serde_json::to_vec(&build)?;
            descriptor.build_record = reference(wire::BUILD, &bytes);
            overwrite(&original.join(wire::BUILD), &bytes)?;
            overwrite(
                &original.join("bundle.json"),
                &serde_json::to_vec(&descriptor)?,
            )?;
        }
        let run_id = RunId::new("simulation")?;
        let archive = root.join("runs/simulation");
        let prepared = VerifiedBundle::open(&original)?.prepare_recoverable(
            &archive,
            run_id.clone(),
            include_bytes!("../../playtime-engine/tests/fixtures/two-session.json"),
        )?;
        fs::remove_dir_all(original)?;
        // Materialize only from independently verifiable archived bytes.
        let reader = ArchiveReader::open(&archive, prepared.limits.clone())?;
        let frozen = VerifiedBundle::from_archive(&reader, &reader.offline_snapshot()?)?;
        let image = frozen.materialize(&root.join("executables"), &run_id)?;
        assert_eq!(fs::metadata(archive.join(EXECUTABLE))?.mode() & 0o111, 0);
        assert_eq!(fs::metadata(&image)?.mode() & 0o777, 0o500);
        let binding = Binding {
            service_id: random_id()?,
            run_id,
            manifest_sha256: prepared.identity.input_manifest.sha256.clone(),
            executable_sha256: reader.manifest().worker.executable.sha256.clone(),
            grant_id: random_id()?,
            worker_id: random_id()?,
            invocation_id: random_id()?,
            boot_id: boot_id()?,
            unit: "package-gate.service".into(),
            kind: WorkerKind::ControllerSimulation,
        };
        let bootstrap = Bootstrap {
            version: worker::VERSION,
            binding,
            credential: Credential::generate()?,
            root: root.clone(),
            private_dir: root.join("private"),
            socket: root.join("private/control.sock"),
            startup_deadline_boot_ms: boot_ms() + 60_000,
            wall_ms: 300_000,
        };
        write_once(
            &bootstrap.private_dir.join("reservation.json"),
            &serde_json::to_vec(&bootstrap.binding)?,
        )?;
        write_once(
            &bootstrap.private_dir.join("bootstrap.json"),
            &serde_json::to_vec(&bootstrap)?,
        )?;
        Ok(Self {
            root,
            archive,
            image,
            prepared,
            bootstrap,
            child: None,
        })
    }
    fn start(&mut self) -> Result<Client> {
        let child = Process::new(&self.image)
            .arg("--bootstrap")
            .arg(self.bootstrap.private_dir.join("bootstrap.json"))
            .env_clear()
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let client = self.client(child.id());
        self.child = Some(child);
        Ok(client)
    }
    fn client(&self, pid: u32) -> Client {
        Client::new(
            Endpoint {
                socket: self.bootstrap.socket.clone(),
                uid: rustix::process::getuid().as_raw(),
                pid,
                binding: self.bootstrap.binding.clone(),
            },
            self.bootstrap.credential.clone(),
        )
    }
    fn grant(&self) -> Grant {
        Grant::new(
            self.bootstrap.private_dir.clone(),
            self.bootstrap.binding.clone(),
        )
    }
    fn writer_busy(&self) {
        let error = ArchiveWriter::open_prepared(
            &self.archive,
            &self.prepared.identity,
            self.prepared.limits.clone(),
        )
        .err()
        .expect("competing writer acquired ownership");
        assert_eq!(error.code, ErrorCode::WriterBusy);
    }
    fn zero(&self) {
        assert_eq!(
            fs::metadata(self.archive.join("events.jsonl"))
                .unwrap()
                .len(),
            0
        );
    }
    async fn ready(&mut self, client: &Client) -> Result<Status> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(reply) = client.request(Command::Status).await {
                return Ok(reply.status);
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err("worker exited before readiness".into());
            }
            if Instant::now() >= deadline {
                return Err("worker readiness deadline".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
async fn waiting_and_handoff(bundle: &Path) -> Result<()> {
    let mut case = Case::new(bundle, false)?;
    let client = case.start()?;
    let status = case.ready(&client).await?;
    assert_eq!(status.state, WorkerState::Prepared);
    assert!(status.work_started_boot_ms.is_none());
    assert_eq!(status.latest_receipt.unwrap().sequence, 0);
    for _ in 0..10 {
        case.writer_busy();
        case.zero();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A reattached control client addresses the original process/identity.
    drop(client);
    let pid = case.child.as_ref().unwrap().id();
    let client = case.client(pid);
    assert!(
        client
            .request(Command::Status)
            .await?
            .status
            .work_started_boot_ms
            .is_none()
    );
    // Replace the launch pathname after exec. /proc/PID/exe remains the original
    // image even though neither the build, source bundle nor launch path exists.
    fs::remove_file(&case.image)?;
    fs::write(
        &case.image,
        b"replacement pathname is not the executing file",
    )?;
    assert_eq!(
        playtime_storage::sha256(&fs::read(format!("/proc/{pid}/exe"))?),
        case.bootstrap.binding.executable_sha256
    );
    case.grant().authorize()?;
    let begun = client.request(Command::Begin).await?.status;
    assert!(begun.latest_receipt.as_ref().unwrap().sequence > 0);
    let duplicate = client.request(Command::Begin).await?.status;
    assert_eq!(begun.work_started_boot_ms, duplicate.work_started_boot_ms);
    let deadline = Instant::now() + Duration::from_secs(15);
    let terminal = loop {
        let status = client.request(Command::Status).await?.status;
        if !status.publication_pending {
            break status;
        }
        if Instant::now() > deadline {
            return Err("controller completion deadline".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_ne!(terminal.state, WorkerState::Reconciling);
    let queries = ArchiveQueries::offline(&case.archive, case.prepared.limits.clone())?;
    assert!(queries.run()?.outcome.is_some());
    assert_eq!(queries.checkpoints()?.len(), 2);
    let events = queries.history(0, 1000, true)?;
    assert!(!events.has_more);
    assert_eq!(
        events
            .entries
            .iter()
            .filter(|entry| entry.envelope.kind == "run_started")
            .count(),
        1
    );
    assert_eq!(
        terminal.latest_receipt.unwrap().sequence,
        queries.snapshot().frontier().committed_sequence
    );
    let usage = queries.run()?.usage;
    let copied = case.root.join("copied-run");
    copy(&case.archive, &copied)?;
    let child = case.child.as_mut().unwrap();
    child.kill()?;
    child.wait()?;
    fs::remove_dir_all(&case.archive)?;
    fs::remove_dir_all(case.root.join("executables"))?;
    let copied = ArchiveQueries::offline(copied, StorageLimits::default())?;
    assert_eq!(copied.run()?.usage, usage);
    println!(
        "PASS frozen process: two sessions, {} events, duplicate Begin, reattachment, unlinked executable, copied archive",
        copied.snapshot().frontier().committed_sequence
    );
    Ok(())
}
async fn cancelled_or_revoked(bundle: &Path, cancel: bool) -> Result<()> {
    let mut case = Case::new(bundle, false)?;
    let client = case.start()?;
    case.ready(&client).await?;
    case.grant().authorize()?;
    if cancel {
        assert_eq!(
            client.request(Command::Cancel).await?.status.state,
            WorkerState::CancelledBeforeBegin
        );
    } else {
        case.grant().revoke()?;
    }
    let begin = client.request(Command::Begin).await;
    if cancel {
        assert_eq!(begin?.status.state, WorkerState::CancelledBeforeBegin);
    } else {
        assert!(begin.is_err());
    }
    let status = client.request(Command::Status).await?.status;
    assert!(status.work_started_boot_ms.is_none());
    case.writer_busy();
    case.zero();
    assert!(!case.bootstrap.private_dir.join("consumed.json").exists());
    println!(
        "PASS before Begin: {}",
        if cancel {
            "cancellation"
        } else {
            "revoked grant"
        }
    );
    Ok(())
}
async fn forged_embedded_identity(bundle: &Path) -> Result<()> {
    let mut case = Case::new(bundle, true)?;
    case.start()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = case.child.as_mut().unwrap().try_wait()? {
            assert!(!status.success());
            break;
        }
        if Instant::now() > deadline {
            return Err("forged worker did not refuse startup".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    case.zero();
    assert!(!case.bootstrap.socket.exists());
    println!("PASS embedded build mismatch refuses readiness");
    Ok(())
}

async fn forged_execution_contract(bundle: &Path) -> Result<()> {
    use playtime_protocol::{recovery::EXECUTION_CONTRACT, storage::ArchiveDescriptor};
    use playtime_storage::FrozenInput;
    for mutation in ["profile", "limits", "worker", "configuration", "version"] {
        let mut case = Case::new(bundle, false)?;
        let reader = ArchiveReader::open(&case.archive, case.prepared.limits.clone())?;
        let snapshot = reader.offline_snapshot()?;
        let mut manifest = reader.manifest().clone();
        let mut inputs = manifest
            .references()
            .map(|r| {
                Ok(FrozenInput {
                    reference: r.clone(),
                    bytes: reader.read_content(&snapshot, r)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        drop(reader);
        let input = inputs
            .iter_mut()
            .find(|i| i.reference.path.as_str() == EXECUTION_CONTRACT)
            .unwrap();
        let mut contract: serde_json::Value = serde_json::from_slice(&input.bytes)?;
        match mutation {
            "profile" => contract["rule_set"] = "future_rules".into(),
            "limits" => contract["storage_limits"]["events"] = 100_001.into(),
            "worker" => {
                contract["worker_executable"]["sha256"] =
                    serde_json::to_value(playtime_storage::sha256(b"wrong image"))?
            }
            "configuration" => {
                contract["configuration"]["sha256"] =
                    serde_json::to_value(playtime_storage::sha256(b"wrong configuration"))?
            }
            _ => contract["schema_version"] = 2.into(),
        }
        input.bytes = serde_json::to_vec(&contract)?;
        input.reference = reference(EXECUTION_CONTRACT, &input.bytes);
        *manifest
            .files
            .iter_mut()
            .find(|r| r.path.as_str() == EXECUTION_CONTRACT)
            .unwrap() = input.reference.clone();
        let bytes = serde_json::to_vec(&manifest)?;
        let descriptor = ArchiveDescriptor {
            schema_version: 1,
            run_id: manifest.run_id.clone(),
            mode: playtime_core::RunMode::Simulation,
            input_manifest: reference("inputs/manifest.json", &bytes),
        };
        // Test-only reconstruction: all hashes and the private binding agree,
        // so only the executing worker's independent verifier can reject it.
        fs::remove_dir_all(&case.archive)?;
        let writer = ArchiveWriter::create(
            &case.archive,
            descriptor,
            &bytes,
            &inputs,
            case.prepared.limits.clone(),
        )?;
        case.prepared.identity = writer.confirmed_frontier().frontier().archive.clone();
        drop(writer);
        case.bootstrap.binding.manifest_sha256 =
            case.prepared.identity.input_manifest.sha256.clone();
        overwrite(
            &case.bootstrap.private_dir.join("reservation.json"),
            &serde_json::to_vec(&case.bootstrap.binding)?,
        )?;
        overwrite(
            &case.bootstrap.private_dir.join("bootstrap.json"),
            &serde_json::to_vec(&case.bootstrap)?,
        )?;
        case.start()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = case.child.as_mut().unwrap().try_wait()? {
                assert!(!status.success());
                break;
            }
            if Instant::now() >= deadline {
                return Err("invalid execution contract did not fail before readiness".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        case.zero();
        assert!(!case.bootstrap.socket.exists());
        assert!(!case.bootstrap.private_dir.join("consumed.json").exists());
        println!("PASS frozen execution contract {mutation} mismatch refuses readiness");
    }
    Ok(())
}

/// Copy the bundle with edited capabilities and a matching build record.
fn rewritten(
    case: &Case,
    bundle: &Path,
    name: &str,
    edit: impl FnOnce(&mut wire::Capabilities),
) -> Result<PathBuf> {
    let path = case.root.join(name);
    copy(bundle, &path)?;
    let mut descriptor: BundleDescriptor =
        serde_json::from_slice(&fs::read(path.join("bundle.json"))?)?;
    let mut build: BuildRecord = serde_json::from_slice(&fs::read(path.join(wire::BUILD))?)?;
    edit(&mut build.capabilities);
    descriptor.capabilities = build.capabilities.clone();
    let bytes = serde_json::to_vec(&build)?;
    descriptor.build_record = reference(wire::BUILD, &bytes);
    overwrite(&path.join(wire::BUILD), &bytes)?;
    overwrite(&path.join("bundle.json"), &serde_json::to_vec(&descriptor)?)?;
    Ok(path)
}

fn older_bundle_cannot_opt_in(bundle: &Path) -> Result<()> {
    let case = Case::new(bundle, false)?;
    let path = rewritten(&case, bundle, "older-bundle", |capabilities| {
        capabilities.recovery_profile = None;
        capabilities.real_workshop = false;
        capabilities.docker_profile = None;
    })?;
    let archive = case.root.join("older-archive");
    assert!(
        VerifiedBundle::open(&path)?
            .prepare_recoverable(
                &archive,
                RunId::new("older")?,
                include_bytes!("../../playtime-engine/tests/fixtures/two-session.json")
            )
            .is_err()
    );
    assert!(!archive.exists());
    for profile in [docker_profile()?, live_profile()?] {
        assert!(
            VerifiedBundle::open(&path)?
                .prepare_docker(&archive, RunId::new("older")?, &profile)
                .is_err()
        );
        assert!(!archive.exists());
    }
    let base = docker_profile()?;
    let continuous =
        playtime_runner::docker::ContinuousProfile::freeze(base.selection, base.daemon, None)?;
    assert!(
        VerifiedBundle::open(&path)?
            .prepare_continuous(&archive, RunId::new("older")?, &continuous)
            .is_err()
    );
    assert!(!archive.exists());
    // The older capability set remains usable for ordinary simulation.
    VerifiedBundle::open(&path)?.prepare(
        &archive,
        RunId::new("older")?,
        include_bytes!("../../playtime-engine/tests/fixtures/two-session.json"),
    )?;
    println!("PASS older simulation bundle cannot opt in to recovery or Docker");
    Ok(())
}

fn other_docker_profile_is_refused(bundle: &Path) -> Result<()> {
    let case = Case::new(bundle, false)?;
    let built = playtime_runner::docker::digest()?;
    let path = rewritten(&case, bundle, "other-profile-bundle", |capabilities| {
        assert_eq!(capabilities.docker_profile.as_ref(), Some(&built));
        capabilities.docker_profile = Some(playtime_storage::sha256(b"another frozen profile"));
    })?;
    let archive = case.root.join("other-profile-archive");
    for profile in [docker_profile()?, live_profile()?] {
        assert!(
            VerifiedBundle::open(&path)?
                .prepare_docker(&archive, RunId::new("other")?, &profile)
                .is_err()
        );
        assert!(!archive.exists());
    }
    let base = docker_profile()?;
    let continuous =
        playtime_runner::docker::ContinuousProfile::freeze(base.selection, base.daemon, None)?;
    assert!(
        VerifiedBundle::open(&path)?
            .prepare_continuous(&archive, RunId::new("other")?, &continuous)
            .is_err()
    );
    assert!(!archive.exists());
    println!("PASS bundle advertising other Docker profiles cannot prepare Docker or live runs");
    // Without the real-model capability, the same profiles allow only scripted runs.
    let path = rewritten(&case, bundle, "scripted-only-bundle", |capabilities| {
        capabilities.real_model = false;
    })?;
    let live = case.root.join("scripted-only-live");
    assert!(
        VerifiedBundle::open(&path)?
            .prepare_docker(&live, RunId::new("live")?, &live_profile()?)
            .is_err()
    );
    assert!(!live.exists());
    VerifiedBundle::open(&path)?.prepare_docker(
        &case.root.join("scripted-only-scripted"),
        RunId::new("scripted")?,
        &docker_profile()?,
    )?;
    println!("PASS bundle without the real-model capability refuses only live runs");
    Ok(())
}

fn live_profile() -> Result<playtime_runner::docker::Profile> {
    let scripted = docker_profile()?;
    Ok(playtime_runner::docker::Profile::freeze_live(
        scripted.selection,
        scripted.daemon,
        playtime_runner::docker::LiveHost {
            target: None,
            endpoint: "http://model-host.local:11434".into(),
            addresses: vec!["192.0.2.10".parse()?],
            live_lock: "/nonexistent/package-gate/live-model.lock".into(),
        },
    )?)
}
fn docker_profile() -> Result<playtime_runner::docker::Profile> {
    Ok(playtime_runner::docker::Profile::freeze(
        playtime_docker::transport::Selection {
            executable: "/nonexistent/package-gate/docker".into(),
            executable_sha256: "a".repeat(64),
            socket: "/nonexistent/package-gate/docker.sock".into(),
        },
        playtime_docker::DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        },
    )?)
}

async fn docker_startup_rejections(bundle: &Path) -> Result<()> {
    use playtime_protocol::storage::ArchiveDescriptor;
    use playtime_storage::FrozenInput;
    for mutation in ["version", "limits", "mode", "systemd"] {
        let mut case = Case::new(bundle, false)?;
        let mut profile = docker_profile()?;
        match mutation {
            "version" => profile.version += 1,
            "limits" => profile.storage.events += 1,
            "mode" => profile.mode = playtime_core::RunMode::Simulation,
            _ => (),
        }
        if mutation != "systemd" {
            let rejected = case.root.join("rejected");
            assert!(
                VerifiedBundle::open(bundle)?
                    .prepare_docker(&rejected, RunId::new("rejected")?, &profile)
                    .is_err()
            );
            assert!(!rejected.exists());
        }
        // Rebuild a digest-consistent archive without the producer validator,
        // proving the executing packaged worker independently refuses it.
        let reader = ArchiveReader::open(&case.archive, case.prepared.limits.clone())?;
        let snapshot = reader.offline_snapshot()?;
        let mut manifest = reader.manifest().clone();
        let mut inputs = manifest
            .references()
            .map(|r| {
                Ok(FrozenInput {
                    reference: r.clone(),
                    bytes: reader.read_content(&snapshot, r)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        drop(reader);
        let input = inputs
            .iter_mut()
            .find(|i| i.reference == manifest.configuration)
            .unwrap();
        input.bytes = serde_json::to_vec(&profile)?;
        input.reference = reference(input.reference.path.as_str(), &input.bytes);
        manifest.configuration = input.reference.clone();
        let bytes = serde_json::to_vec(&manifest)?;
        let descriptor = ArchiveDescriptor {
            schema_version: 1,
            run_id: manifest.run_id.clone(),
            mode: playtime_core::RunMode::ScriptedSmoke,
            input_manifest: reference("inputs/manifest.json", &bytes),
        };
        fs::remove_dir_all(&case.archive)?;
        let writer = ArchiveWriter::create(
            &case.archive,
            descriptor,
            &bytes,
            &inputs,
            case.prepared.limits.clone(),
        )?;
        case.bootstrap.binding.manifest_sha256 = writer
            .confirmed_frontier()
            .frontier()
            .archive
            .input_manifest
            .sha256
            .clone();
        drop(writer);
        case.bootstrap.binding.kind = WorkerKind::DockerScripted;
        case.bootstrap.wall_ms = 120_000;
        overwrite(
            &case.bootstrap.private_dir.join("reservation.json"),
            &serde_json::to_vec(&case.bootstrap.binding)?,
        )?;
        overwrite(
            &case.bootstrap.private_dir.join("bootstrap.json"),
            &serde_json::to_vec(&case.bootstrap)?,
        )?;
        case.start()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = case.child.as_mut().unwrap().try_wait()? {
                assert!(!status.success());
                break;
            }
            if Instant::now() >= deadline {
                return Err("unsupported Docker preparation reached readiness".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        case.zero();
        assert!(!case.bootstrap.socket.exists());
        assert!(!case.bootstrap.private_dir.join("docker-config").exists());
        assert!(!case.bootstrap.private_dir.join("consumed.json").exists());
        println!("PASS Docker {mutation} rejection before metadata/effects");
    }
    Ok(())
}
fn tamper(bundle: &Path) -> Result<()> {
    for name in [EXECUTABLE, SOURCE, LOCK, wire::BUILD] {
        let case = Case::new(bundle, false)?;
        let copy_path = case.root.join("tampered");
        copy(bundle, &copy_path)?;
        overwrite(&copy_path.join(name), b"tampered")?;
        assert!(
            VerifiedBundle::open(&copy_path).is_err(),
            "accepted mutated {name}"
        );
    }
    for mutation in ["platform", "capability", "oversized"] {
        let case = Case::new(bundle, false)?;
        let path = case.root.join("tampered");
        copy(bundle, &path)?;
        let mut descriptor: BundleDescriptor =
            serde_json::from_slice(&fs::read(path.join("bundle.json"))?)?;
        match mutation {
            "platform" => descriptor.worker.platform.architecture = "aarch64".into(),
            "capability" => descriptor.capabilities.private_protocol += 1,
            _ => descriptor.worker.executable.size_bytes = u64::MAX,
        }
        overwrite(&path.join("bundle.json"), &serde_json::to_vec(&descriptor)?)?;
        assert!(VerifiedBundle::open(&path).is_err());
    }
    // Every package is static, independently of the descriptor's target string.
    let bytes =
        PrivateDirectory::open(bundle)?.read(EXECUTABLE, StorageLimits::default().content_bytes)?;
    playtime_worker::verify_static_elf(&bytes)?;
    let build: BuildRecord = serde_json::from_slice(&fs::read(bundle.join(wire::BUILD))?)?;
    assert_eq!(
        build.capture_helpers,
        playtime_docker::capture::helper_digests()
    );
    for helper in [
        playtime_docker::capture::EXPORTER,
        playtime_docker::capture::MEMORY_READER,
    ] {
        assert!(
            bytes
                .windows(helper.len())
                .any(|window| window == helper.as_bytes()),
            "helper source missing from frozen executable"
        );
    }
    let verified = VerifiedBundle::open(bundle)?;
    verified.verify_embedded(&serde_json::to_vec(&build)?)?;
    let mut wrong = build;
    wrong
        .capture_helpers
        .insert("workspace-exporter-v1".into(), "0".repeat(64));
    assert!(
        verified
            .verify_embedded(&serde_json::to_vec(&wrong)?)
            .is_err()
    );
    println!("PASS capture helpers embedded and digest-bound to frozen worker build");
    println!("PASS bundle tamper/platform/capability/size and ELF gates");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != "--bundle" {
        return Err("usage: package-e2e --bundle <verified-directory>".into());
    }
    let bundle = Path::new(&args[1]);
    tamper(bundle)?;
    waiting_and_handoff(bundle).await?;
    cancelled_or_revoked(bundle, true).await?;
    cancelled_or_revoked(bundle, false).await?;
    forged_embedded_identity(bundle).await?;
    older_bundle_cannot_opt_in(bundle)?;
    other_docker_profile_is_refused(bundle)?;
    forged_execution_contract(bundle).await?;
    docker_startup_rejections(bundle).await?;
    Ok(())
}
