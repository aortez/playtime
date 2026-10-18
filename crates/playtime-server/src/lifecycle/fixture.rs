//! Explicitly synthetic supervision fixture. Its receipt bytes are not a
//! Playtime experimental archive and cannot be used by production adapters.

use super::{
    ports::{ArchivePort, Supervisor},
    *,
};
use async_trait::async_trait;
use playtime_core::RunMode;
use playtime_protocol::worker::{self, Binding, Command, Receipt, Status, WorkerKind};
use playtime_worker_control::{
    Bootstrap, Credential, Endpoint, FileLock, PreparedExecution, Worker, boot_id, boot_ms,
    create_private_dir, digest, random_id, read_private, serve_connection_with_delivery,
    write_once,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const BUNDLE_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
pub struct FixtureArchive {
    root: PathBuf,
    executable: PathBuf,
    argv: Vec<String>,
    selection: Selection,
}
impl FixtureArchive {
    pub fn new(root: &Path, executable: &Path, argv: Vec<String>) -> Result<Self> {
        let bytes = bounded_executable(executable)?;
        Ok(Self {
            root: fs::canonicalize(root)?,
            executable: fs::canonicalize(executable)?,
            argv,
            selection: Selection {
                executable_sha256: digest(&bytes),
                wall_ms: 30_000,
                token_limit: 100,
            },
        })
    }
    pub fn selection(&self) -> &Selection {
        &self.selection
    }
    pub fn preview_revision(&self) -> String {
        format!(
            "fixture-v1-{}",
            digest(&serde_json::to_vec(&self.selection).expect("typed fixture selection"))
        )
    }
}
#[async_trait]
impl ArchivePort for FixtureArchive {
    async fn validate_preview(&self, request: &AdmissionRequest) -> Result<Selection> {
        if request.recipe_id != "lifecycle-fixture"
            || request.preview_revision != self.preview_revision()
            || request.mode != RunMode::Simulation
        {
            return Err(Error::Invalid("unsupported fixture preview"));
        }
        Ok(self.selection.clone())
    }
    async fn prepare(&self, admission: &Admission) -> Result<PreparedLaunch> {
        let this = self.clone();
        let admission = admission.clone();
        tokio::task::spawn_blocking(move || {
            let workers = this.root.join(".playtime/workers");
            create_private_dir(&workers)?;
            let directory = workers.join(admission.run_id.as_str());
            create_private_dir(&directory)?;
            let manifest = serde_json::to_vec(&serde_json::json!({ "kind": "supervision_fixture", "request": admission.request, "selection": admission.selection }))?;
            write_once(&directory.join("manifest.fixture.json"), &manifest)?;
            let executable = directory.join("worker");
            let bytes = bounded_executable(&this.executable)?;
            if digest(&bytes) != admission.selection.executable_sha256 { return Err(Error::Invalid("fixture bundle changed after preview")); }
            write_once(&executable, &bytes)?;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
            File::open(&executable)?.sync_all()?;
            let binding = Binding { service_id: admission.service_id.clone(), run_id: admission.run_id.clone(),
                manifest_sha256: digest(&manifest), executable_sha256: admission.selection.executable_sha256.clone(),
                grant_id: random_id()?, worker_id: random_id()?, invocation_id: random_id()?, boot_id: boot_id()?,
                unit: format!("playtime-{}.service", admission.run_id), kind: WorkerKind::SupervisionFixture };
            let socket = directory.join("control.sock");
            // Linux sockaddr_un is short. Refuse rather than truncate identity.
            if socket.as_os_str().len() >= 108 { return Err(Error::Invalid("fixture root too long for private socket")); }
            let bootstrap_path = directory.join("bootstrap.json");
            let bootstrap = Bootstrap { version: worker::VERSION, binding: binding.clone(), credential: Credential::generate()?,
                root: this.root.clone(), private_dir: directory.clone(), socket: socket.clone(),
                startup_deadline_boot_ms: boot_ms() + 60_000, wall_ms: admission.selection.wall_ms };
            write_once(&bootstrap_path, &serde_json::to_vec(&bootstrap)?)?;
            write_once(&directory.join("reservation.json"), &serde_json::to_vec(&binding)?)?;
            // Compatibility pointers only. Never manufacture Python manifest.json.
            let runs = this.root.join("runs");
            create_private_dir(&runs)?;
            let run_dir = runs.join(admission.run_id.as_str());
            create_private_dir(&run_dir)?;
            write_once(&run_dir.join("job.json"), &serde_json::to_vec(&serde_json::json!({"mode":"service","state":"starting","unit":binding.unit,"kind":"supervision_fixture"}))?)?;
            write_once(&this.root.join(".playtime/active.json"), &serde_json::to_vec(&serde_json::json!({"id":admission.run_id}))?)?;
            Ok(PreparedLaunch { binding, executable, argv: this.argv, bootstrap: bootstrap_path, private_dir: directory,
                socket, startup_deadline_boot_ms: bootstrap.startup_deadline_boot_ms })
        }).await.map_err(|_| Error::Uncertain)?
    }
    async fn verify_status(
        &self,
        prepared: &PreparedLaunch,
        previous: Option<&Receipt>,
        status: &Status,
    ) -> Result<()> {
        let directory = prepared.private_dir.clone();
        let receipt = status.latest_receipt.clone().ok_or(Error::Uncertain)?;
        let previous = previous.cloned();
        tokio::task::spawn_blocking(move || {
            let bytes = match read_private(&directory.join("fixture.evidence"), 4096) {
                Ok(bytes) => bytes,
                Err(playtime_worker_control::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::NotFound =>
                {
                    Vec::new()
                }
                Err(e) => return Err(e.into()),
            };
            let n = usize::try_from(receipt.bytes).map_err(|_| Error::Uncertain)?;
            if n > bytes.len()
                || receipt.sequence > 1
                || digest(&bytes[..n]) != receipt.sha256
                || (receipt.sequence == 0) != (n == 0)
            {
                return Err(Error::Invalid("fixture receipt mismatch"));
            }
            if let Some(old) = previous
                && (receipt.sequence < old.sequence
                    || receipt.bytes < old.bytes
                    || (receipt.sequence == old.sequence && receipt != old)
                    || digest(&bytes[..old.bytes as usize]) != old.sha256)
            {
                return Err(Error::Invalid(
                    "fixture prefix does not extend confirmation",
                ));
            }
            Ok(())
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
}

#[derive(Serialize, Deserialize)]
struct Invocation {
    endpoint: Endpoint,
    start_ticks: u64,
}

/// A subprocess fixture, not a systemd implementation. Its private invocation
/// record and kernel process identity supply an independent authentication port.
#[derive(Default)]
pub struct FixtureSupervisor {
    children: Arc<Mutex<HashMap<String, Child>>>,
    pub lose_next_start_reply: AtomicBool,
}
impl FixtureSupervisor {
    pub fn terminate(&self, launch: &PreparedLaunch) -> Result<()> {
        if let Some(mut child) = self
            .children
            .lock()
            .map_err(|_| Error::Uncertain)?
            .remove(launch.binding.invocation_id.as_str())
        {
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            child.wait()?;
        }
        Ok(())
    }
}
impl Drop for FixtureSupervisor {
    fn drop(&mut self) {
        // Fixture harness cleanup only. Dropping a Coordinator does not drop
        // this external supervisor; tests keep it alive across API restarts.
        if let Ok(mut children) = self.children.lock() {
            for (_, mut child) in children.drain() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
#[async_trait]
impl Supervisor for FixtureSupervisor {
    async fn ensure_legacy_idle(&self, root: &Path) -> Result<()> {
        // This fixture cannot establish that an arbitrary legacy systemd unit
        // is inactive. Existing compatibility reservations therefore block it.
        if root.join(".playtime/active.json").try_exists()? {
            return Err(Error::Busy);
        }
        Ok(())
    }
    async fn start(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        let launch = launch.clone();
        let children = self.children.clone();
        let lose = self.lose_next_start_reply.swap(false, Ordering::SeqCst);
        tokio::task::spawn_blocking(move || {
            let mut child = ProcessCommand::new(&launch.executable)
                .args(&launch.argv)
                .env("PLAYTIME_LIFECYCLE_BOOTSTRAP", &launch.bootstrap)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let pid = child.id();
            let endpoint = Endpoint {
                socket: launch.socket.clone(),
                uid: fs::metadata(&launch.bootstrap)?.uid(),
                pid,
                binding: launch.binding.clone(),
            };
            let publication = (|| -> Result<()> {
                let invocation = Invocation {
                    endpoint: endpoint.clone(),
                    start_ticks: process_start(pid)?,
                };
                write_once(
                    &launch.private_dir.join("invocation.fixture.json"),
                    &serde_json::to_vec(&invocation)?,
                )?;
                Ok(())
            })();
            if publication.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                publication?;
            }
            children
                .lock()
                .map_err(|_| Error::Uncertain)?
                .insert(launch.binding.invocation_id.as_str().to_owned(), child);
            if lose {
                Err(Error::Uncertain)
            } else {
                Ok(endpoint)
            }
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        let launch = launch.clone();
        tokio::task::spawn_blocking(move || {
            let record: Invocation = serde_json::from_slice(&read_private(
                &launch.private_dir.join("invocation.fixture.json"),
                65_536,
            )?)?;
            if record.endpoint.binding != launch.binding
                || launch.binding.boot_id != boot_id()?
                || process_start(record.endpoint.pid)? != record.start_ticks
            {
                return Err(Error::Uncertain);
            }
            let executing =
                bounded_executable(Path::new(&format!("/proc/{}/exe", record.endpoint.pid)))?;
            if digest(&executing) != launch.binding.executable_sha256 {
                return Err(Error::Uncertain);
            }
            Ok(record.endpoint)
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
}
fn process_start(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rest = stat.rsplit_once(')').ok_or(Error::Uncertain)?.1;
    rest.split_whitespace()
        .nth(19)
        .ok_or(Error::Uncertain)?
        .parse()
        .map_err(|_| Error::Uncertain)
}
fn bounded_executable(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(BUNDLE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > BUNDLE_LIMIT {
        return Err(Error::Invalid("fixture executable exceeds bound"));
    }
    Ok(bytes)
}

struct FixtureExecution {
    directory: PathBuf,
    _writer: FileLock,
}
impl PreparedExecution for FixtureExecution {
    fn prepared_receipt(&self) -> Option<Receipt> {
        Some(Receipt {
            sequence: 0,
            bytes: 0,
            sha256: digest(&[]),
        })
    }
    fn begin(&mut self, started_boot_ms: u64) -> playtime_worker_control::Result<Receipt> {
        let mut bytes = serde_json::to_vec(
            &serde_json::json!({"kind":"supervision_fixture_begin","started_boot_ms":started_boot_ms}),
        )?;
        bytes.push(b'\n');
        write_once(&self.directory.join("fixture.evidence"), &bytes)?;
        Ok(Receipt {
            sequence: 1,
            bytes: bytes.len() as u64,
            sha256: digest(&bytes),
        })
    }
    fn cancel(&mut self, before_begin: bool) -> playtime_worker_control::Result<()> {
        write_once(
            &self.directory.join("cancel.fixture.json"),
            &serde_json::to_vec(
                &serde_json::json!({"kind":"supervision_fixture_cancel","before_begin":before_begin}),
            )?,
        )
    }
}

/// Call only in an explicitly selected fixture child process. The parent passes
/// a private bootstrap pathname, never a credential, in the environment.
pub async fn run_worker(bootstrap_path: &Path) -> Result<()> {
    let path = bootstrap_path.to_owned();
    let (bootstrap, owner) = tokio::task::spawn_blocking(move || -> Result<_> {
        let bootstrap = Bootstrap::read(&path)?;
        let reservation: Binding = serde_json::from_slice(&read_private(
            &bootstrap.private_dir.join("reservation.json"),
            65_536,
        )?)?;
        let manifest = read_private(&bootstrap.private_dir.join("manifest.fixture.json"), 65_536)?;
        let manifest_value: serde_json::Value = serde_json::from_slice(&manifest)?;
        let selection: Selection = serde_json::from_value(
            manifest_value
                .get("selection")
                .ok_or(Error::Uncertain)?
                .clone(),
        )?;
        if reservation != bootstrap.binding
            || digest(&manifest) != bootstrap.binding.manifest_sha256
            || selection.wall_ms != bootstrap.wall_ms
            || selection.executable_sha256 != bootstrap.binding.executable_sha256
        {
            return Err(Error::Uncertain);
        }
        let gate = bootstrap.private_dir.join("hold-before-lock.fixture");
        if gate.exists() {
            write_once(
                &bootstrap.private_dir.join("waiting-before-lock.fixture"),
                b"fixture startup boundary",
            )?;
            while gate.exists() {
                if boot_ms() >= bootstrap.startup_deadline_boot_ms {
                    return Err(Error::Invalid("fixture startup gate timed out"));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let run_lock = loop {
            match FileLock::acquire(&bootstrap.root.join(".playtime/run.lock")) {
                Ok(lock) => break lock,
                Err(playtime_worker_control::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        && boot_ms() < bootstrap.startup_deadline_boot_ms =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(e.into()),
            }
        };
        let execution = FixtureExecution {
            directory: bootstrap.private_dir.clone(),
            _writer: FileLock::acquire(&bootstrap.private_dir.join("writer.fixture.lock"))?,
        };
        let owner = Worker::prepared(bootstrap.clone(), execution, run_lock)?;
        Ok((bootstrap, Arc::new(Mutex::new(owner))))
    })
    .await
    .map_err(|_| Error::Uncertain)??;
    let listener = tokio::net::UnixListener::bind(&bootstrap.socket)?;
    fs::set_permissions(&bootstrap.socket, fs::Permissions::from_mode(0o600))?;
    let uid = fs::metadata(&bootstrap.private_dir)?.uid();
    loop {
        let deadline = {
            let owner = owner.lock().map_err(|_| Error::Uncertain)?;
            owner
                .status()
                .work_started_boot_ms
                .map(|start| start + bootstrap.wall_ms)
                .unwrap_or(bootstrap.startup_deadline_boot_ms)
        };
        if boot_ms() >= deadline {
            return Ok(());
        }
        tokio::select! {
            connected = listener.accept() => {
                let (stream, _) = connected?;
                let lost_reply = bootstrap.private_dir.join("lose-begin-reply.fixture");
                let _ = serve_connection_with_delivery(stream, owner.clone(), &bootstrap.credential, uid, |command, _| {
                    if command == Command::Begin && lost_reply.exists() { let _ = fs::remove_file(lost_reply); false } else { true }
                }).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => (),
        }
    }
}
