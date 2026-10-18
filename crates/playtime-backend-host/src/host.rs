//! Production process boundary. The root deadline owner exposes a typed fixed
//! policy, not D-Bus, unit-name, path or executable forwarding.
use crate::{
    Result,
    deadline::{AbsoluteTimer, Register},
    ollama::ManagerAuthority,
    protocol,
    systemd::Manager,
    wire::*,
};
use playtime_backend_guard::{
    Guard, Journal,
    ports::*,
    transport::{read_json, write_json},
};
use playtime_local::PrivateDirectory;
use playtime_protocol::worker::ControlId;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

pub const CONFIG: &str = "/etc/playtime-backend/host.json";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub backend_id: ControlId,
    pub guard_id: ControlId,
    pub generation: u64,
    pub uid: u32,
    pub profile_sha256: playtime_core::Sha256Digest,
    pub credential: Capability,
}
impl Config {
    pub fn load() -> Result<Self> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
            )
            .open(CONFIG)
            .map_err(|_| Failure::Invalid)?;
        let m = file.metadata().map_err(|_| Failure::Invalid)?;
        if !m.is_file()
            || m.uid() != 0
            || m.mode() & 0o027 != 0
            || m.nlink() != 1
            || m.len() > CONTROL_BYTES as u64
        {
            return Err(Failure::Invalid);
        }
        let mut bytes = vec![];
        file.take(CONTROL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::Invalid)?;
        let config: Self = serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
        if config.version != 1 || config.uid == 0 || config.generation == 0 {
            return Err(Failure::Invalid);
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostCommand {
    PrepareGeneration {},
    Inspect {},
    ArmLease { lease: Box<LeaseSpec> },
    ArmRequest { request: RequestSpec },
    Terminal { request: RequestSpec },
    Release { lease: ControlId },
    Stop { stop: Box<StopOperation> },
    Termination { stop: Box<StopOperation> },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    nonce: ControlId,
    credential: Capability,
    command: HostCommand,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u32,
    nonce: ControlId,
    result: std::result::Result<HostState, Failure>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostState {
    pub backend: BackendIdentity,
    pub host_ms: u64,
    pub stop: Option<StopOperation>,
    pub terminated: bool,
    pub live: bool,
    pub job_id: Option<ControlId>,
}
pub struct OwnerPaths {
    pub state: PathBuf,
    pub socket: PathBuf,
}
struct Message {
    command: HostCommand,
    reply: oneshot::Sender<Result<HostState>>,
}
fn state_budget(path: &Path, publication_bytes: u64) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut bytes = publication_bytes;
    let mut entries = 2u64;
    for entry in std::fs::read_dir(path).map_err(|_| Failure::Storage)? {
        let entry = entry.map_err(|_| Failure::Storage)?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|_| Failure::Storage)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(Failure::Storage);
        }
        bytes = bytes.checked_add(metadata.len()).ok_or(Failure::Limit)?;
        entries = entries.checked_add(1).ok_or(Failure::Limit)?;
        if bytes > STATE_BYTES || entries > STATE_RECORDS {
            return Err(Failure::Limit);
        }
    }
    Ok(())
}
fn maintenance_lock(path: &Path, owner: u32) -> Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)
        .map_err(|_| Failure::Blocked)?;
    let metadata = file.metadata().map_err(|_| Failure::Blocked)?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(Failure::Blocked);
    }
    file.try_lock().map_err(|_| Failure::Busy)?;
    Ok(file)
}
fn authorized(a: &Capability, b: &Capability) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(a.expose().as_bytes().ct_eq(b.expose().as_bytes()))
}
pub fn notify(message: &[u8]) -> Result<()> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    if path.as_encoded_bytes().starts_with(b"@") {
        return Err(Failure::Invalid);
    }
    let socket = std::os::unix::net::UnixDatagram::unbound().map_err(|_| Failure::Blocked)?;
    socket.set_nonblocking(true).map_err(|_| Failure::Blocked)?;
    socket
        .send_to(message, PathBuf::from(path))
        .map_err(|_| Failure::Blocked)?;
    Ok(())
}
/// A stopped owner is not auto-restarted. Reopening this state requires explicit
/// reconciliation; missing/corrupt files never become an empty new generation.
pub async fn serve_owner(
    mut manager: Manager,
    config: Config,
    paths: OwnerPaths,
    initialize: bool,
) -> Result<()> {
    let root = PrivateDirectory::open(&paths.state, false).map_err(|_| Failure::Storage)?;
    let lock = root.lock().map_err(|_| Failure::Busy)?;
    state_budget(&paths.state, CONTROL_BYTES as u64)?;
    let mut register = if initialize {
        let state = Register::new(manager.identity.clone());
        root.write_new(
            "deadline.json",
            &serde_json::to_vec(&state).map_err(|_| Failure::Invalid)?,
        )
        .map_err(|_| Failure::Storage)?;
        state
    } else {
        let mut state: Register = serde_json::from_slice(
            &root
                .read("deadline.json", STATE_BYTES)
                .map_err(|_| Failure::Storage)?,
        )
        .map_err(|_| Failure::Storage)?;
        if state.backend != manager.identity {
            return Err(Failure::Conflict);
        }
        state.closed = true;
        state
    };
    let mut retired_managers: Vec<(Register, Manager)> = Vec::new();
    let listener = UnixListener::bind(&paths.socket).map_err(|_| Failure::Blocked)?;
    std::fs::set_permissions(
        &paths.socket,
        std::os::unix::fs::PermissionsExt::from_mode(0o660),
    )
    .map_err(|_| Failure::Blocked)?;
    // Runtime directory group is provisioned to the dedicated bridge UID.
    let (tx, mut rx) = mpsc::channel::<Message>(16);
    let quota = Arc::new(Semaphore::new(16));
    let credential = config.credential.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let Ok(permit) = quota.clone().try_acquire_owned() else {
                continue;
            };
            let tx = tx.clone();
            let credential = credential.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let result = tokio::time::timeout(Duration::from_secs(2), async {
                    let peer = stream.peer_cred().map_err(|_| Failure::Unauthorized)?.uid();
                    if peer != config.uid && peer != 0 {
                        return Err(Failure::Unauthorized);
                    }
                    let input: Envelope = read_json(&mut stream).await?;
                    if input.version != 1 || !authorized(&credential, &input.credential) {
                        return Err(Failure::Unauthorized);
                    }
                    if matches!(input.command, HostCommand::PrepareGeneration {}) && peer != 0 {
                        return Err(Failure::Unauthorized);
                    }
                    let hold = matches!(
                        input.command,
                        HostCommand::Inspect {} | HostCommand::Termination { .. }
                    );
                    let (reply, wait) = oneshot::channel();
                    tx.try_send(Message {
                        command: input.command,
                        reply,
                    })
                    .map_err(|_| Failure::Busy)?;
                    let result = wait.await.map_err(|_| Failure::Blocked)?;
                    write_json(
                        &mut stream,
                        &Reply {
                            version: 1,
                            nonce: input.nonce,
                            result,
                        },
                    )
                    .await?;
                    Ok::<_, Failure>(hold)
                })
                .await;
                if !matches!(result, Ok(Ok(true))) {
                    return;
                }
                // Keeping this authenticated peer alive holds the root owner,
                // which itself retains the manager connection/cgroup/exclusion.
                let mut byte = [0u8; 1];
                use tokio::io::AsyncReadExt;
                let _ = stream.read(&mut byte).await;
            });
        }
    });
    let clock = SystemClock::new()?;
    let timer = AbsoluteTimer::new()?;
    let (writes, commands) =
        std::sync::mpsc::sync_channel::<(Register, oneshot::Sender<Result<()>>)>(16);
    std::thread::spawn(move || {
        let _lock: File = lock;
        while let Ok((state, reply)) = commands.recv() {
            let result = serde_json::to_vec(&state)
                .map_err(|_| Failure::Storage)
                .and_then(|bytes| {
                    if bytes.len() as u64 > STATE_BYTES {
                        return Err(Failure::Limit);
                    }
                    state_budget(&paths.state, bytes.len() as u64)?;
                    root.replace("deadline.json", &bytes)
                        .map_err(|_| Failure::Storage)
                });
            let failed = result.is_err();
            let _ = reply.send(result);
            if failed {
                while let Ok((_, reply)) = commands.recv() {
                    let _ = reply.send(Err(Failure::Storage));
                }
                break;
            }
        }
    });
    let mut last_guard = clock.now().ms;
    let startup_deadline = last_guard.checked_add(50_000).ok_or(Failure::Invalid)?;
    let mut guard_seen = false;
    let mut terminated = false;
    let mut stopping = false;
    notify(b"READY=1")?;
    loop {
        let now = clock.now().ms;
        if !guard_seen && now >= startup_deadline {
            register.closed = true;
        }
        let wake = if stopping {
            now.saturating_add(250)
        } else {
            register
                .deadline()
                .unwrap_or(now.saturating_add(250))
                .min(now.saturating_add(250))
        };
        timer.arm(wake.max(1))?;
        let message = tokio::select! { biased;
            _ = timer.expired() => None,
            message = rx.recv() => message,
        };
        if let Some(message) = message {
            let now = clock.now().ms;
            if let HostCommand::Termination { stop } | HostCommand::Stop { stop } = &message.command
                && let Some((old, held)) = retired_managers
                    .iter()
                    .find(|(r, _)| r.stop.as_ref() == Some(stop.as_ref()))
            {
                let _ = message.reply.send(Ok(HostState {
                    backend: old.backend.clone(),
                    host_ms: now,
                    stop: old.stop.clone(),
                    terminated: true,
                    live: false,
                    job_id: held.job_id(),
                }));
            } else {
                let mut next = register.clone();
                let mut changed = true;
                let result = match &message.command {
                    HostCommand::PrepareGeneration {} => {
                        if register.lease.is_some() || !terminated || !stopping {
                            Err(Failure::Blocked)
                        } else if let Ok(_maintenance) = maintenance_lock(
                            Path::new("/var/lib/playtime-backend-deployment/.lock"),
                            0,
                        ) {
                            match tokio::time::timeout(
                                Duration::from_millis(900),
                                manager.next_generation(false),
                            )
                            .await
                            {
                                Ok(Ok(replacement)) => {
                                    next.backend = replacement.identity.clone();
                                    next.retired.push(next.stop.take().ok_or(Failure::Blocked)?);
                                    next.request = None;
                                    next.closed = false;
                                    next.job_path = None;
                                    let old = std::mem::replace(&mut manager, replacement);
                                    retired_managers.push((register.clone(), old));
                                    terminated = false;
                                    stopping = false;
                                    last_guard = now;
                                    Ok(())
                                }
                                _ => return Err(Failure::Blocked),
                            }
                        } else {
                            Err(Failure::Busy)
                        }
                    }
                    HostCommand::Inspect {} => {
                        changed = false;
                        let result = if register.stop.is_some() {
                            tokio::time::timeout(Duration::from_millis(400), manager.verify()).await
                        } else {
                            tokio::time::timeout(Duration::from_millis(400), manager.live()).await
                        }
                        .map_err(|_| Failure::Expired)
                        .and_then(|r| r);
                        if result.is_ok() {
                            last_guard = now;
                            guard_seen = true;
                        }
                        result
                    }
                    HostCommand::ArmLease { lease } => next.arm_lease(lease, now),
                    HostCommand::ArmRequest { request } => next.arm_request(request, now),
                    HostCommand::Terminal { request } => next.terminal(request, now),
                    HostCommand::Release { lease } => next.release(lease, terminated),
                    HostCommand::Stop { stop } => {
                        next.select_stop(stop.reason, now, Some(stop)).map(|_| ())
                    }
                    HostCommand::Termination { stop } => {
                        changed = false;
                        if register.stop.as_ref() != Some(stop.as_ref()) {
                            Err(Failure::Conflict)
                        } else {
                            Ok(())
                        }
                    }
                };
                if let Err(error) = result {
                    let _ = message.reply.send(Err(error));
                } else {
                    register = next;
                    // Arm in this independently supervised process before a disk
                    // write or acknowledgment can stall.
                    if let Some(deadline) = register.deadline() {
                        timer.arm(deadline.max(1))?;
                    }
                    let status = HostState {
                        backend: register.backend.clone(),
                        host_ms: now,
                        stop: register.stop.clone(),
                        terminated,
                        live: !register.closed && !stopping,
                        job_id: manager.job_id(),
                    };
                    if changed {
                        let (done, wait) = oneshot::channel();
                        if writes.try_send((register.clone(), done)).is_err() {
                            register.closed = true;
                            let _ = message.reply.send(Err(Failure::Storage));
                        } else {
                            tokio::spawn(async move {
                                let result = wait
                                    .await
                                    .map_err(|_| Failure::Storage)
                                    .and_then(|r| r)
                                    .map(|()| status);
                                let _ = message.reply.send(result);
                            });
                        }
                    } else {
                        let _ = message.reply.send(Ok(status));
                    }
                }
            }
        }
        let now = clock.now().ms;
        let expired = register.deadline().is_some_and(|d| now >= d);
        let lost = if guard_seen {
            now.saturating_sub(last_guard) >= 2000
        } else {
            now >= startup_deadline
        };
        if expired || lost || register.closed {
            let was_stopped = register.stop.is_some();
            let _ = register.select_stop(
                if expired {
                    StopReason::Deadline
                } else {
                    StopReason::OwnerLost
                },
                now,
                None,
            );
            if !was_stopped && register.stop.is_some() {
                let (done, _wait) = oneshot::channel();
                let _ = writes.try_send((register.clone(), done));
            }
        }
        if let Some(stop) = &register.stop {
            stopping = true;
            let stop = stop.clone();
            let _ = tokio::time::timeout(Duration::from_millis(400), manager.stop(&stop)).await;
            if register.job_path.as_deref() != manager.job_path() {
                register.job_path = manager.job_path().map(str::to_owned);
                let (done, _wait) = oneshot::channel();
                let _ = writes.try_send((register.clone(), done));
            }
            if now >= stop.supervisor_deadline_ms {
                let _ = manager.kill_original();
            }
            if let Ok(Ok(exited)) =
                tokio::time::timeout(Duration::from_millis(400), manager.terminated()).await
            {
                terminated = exited;
            }
        } else if lost || register.closed {
            // Even before a first lease, owner failure closes the owned backend.
            manager.kill_original()?;
            return Err(Failure::Blocked);
        }
        // Only this enforcing event loop feeds systemd. No helper thread can
        // conceal a blocked manager, timer, storage enqueue or enforcement path.
        notify(b"WATCHDOG=1")?;
    }
}

#[derive(Clone)]
pub struct Remote {
    pub socket: PathBuf,
    pub credential: Capability,
    pub owner_uid: u32,
}
struct Authority {
    identity: BackendIdentity,
    operation: Option<ControlId>,
    _connection: std::sync::Mutex<UnixStream>,
}
impl BackendAuthority for Authority {
    fn identity(&self) -> &BackendIdentity {
        &self.identity
    }
}
impl TerminationAuthority for Authority {
    fn operation_id(&self) -> &ControlId {
        self.operation.as_ref().expect("termination binding")
    }
}
impl Remote {
    async fn call(&self, command: HostCommand) -> Result<(HostState, UnixStream)> {
        tokio::time::timeout(Duration::from_millis(900), async {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|_| Failure::Blocked)?;
            if stream.peer_cred().map_err(|_| Failure::Unauthorized)?.uid() != self.owner_uid {
                return Err(Failure::Unauthorized);
            }
            let nonce = playtime_backend_guard::random_id()?;
            write_json(
                &mut stream,
                &Envelope {
                    version: 1,
                    nonce: nonce.clone(),
                    credential: self.credential.clone(),
                    command,
                },
            )
            .await?;
            let response: Reply = read_json(&mut stream).await?;
            if response.version != 1 || response.nonce != nonce {
                return Err(Failure::Unauthorized);
            }
            Ok((response.result?, stream))
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
    pub async fn inspect(&self) -> Result<HostState> {
        self.call(HostCommand::Inspect {}).await.map(|r| r.0)
    }
    pub async fn prepare_generation(&self) -> Result<HostState> {
        self.call(HostCommand::PrepareGeneration {})
            .await
            .map(|r| r.0)
    }
}
#[async_trait::async_trait]
impl ManagerAuthority for Remote {
    async fn verify(&self, expected: &BackendIdentity) -> Result<VerifiedBackend> {
        let (state, connection) = self.call(HostCommand::Inspect {}).await?;
        if &state.backend != expected || !state.live {
            return Err(Failure::Conflict);
        }
        VerifiedBackend::from_trusted_port(Arc::new(Authority {
            identity: state.backend,
            operation: None,
            _connection: std::sync::Mutex::new(connection),
        }))
    }
}
#[async_trait::async_trait]
impl Enforcement for Remote {
    async fn select_stop(&self, proposed: &StopOperation) -> Result<StopOperation> {
        let (state, _) = self
            .call(HostCommand::Stop {
                stop: Box::new(proposed.clone()),
            })
            .await?;
        state
            .stop
            .filter(|s| s.backend == proposed.backend && s.lease_id == proposed.lease_id)
            .ok_or(Failure::Blocked)
    }
    async fn arm_lease(&self, lease: &LeaseSpec) -> Result<()> {
        self.call(HostCommand::ArmLease {
            lease: Box::new(lease.clone()),
        })
        .await
        .map(|_| ())
    }
    async fn arm_request(&self, request: &RequestSpec) -> Result<()> {
        self.call(HostCommand::ArmRequest {
            request: request.clone(),
        })
        .await
        .map(|_| ())
    }
    async fn terminal(&self, request: &RequestSpec) -> Result<()> {
        self.call(HostCommand::Terminal {
            request: request.clone(),
        })
        .await
        .map(|_| ())
    }
    async fn release(&self, lease: &ControlId) -> Result<()> {
        self.call(HostCommand::Release {
            lease: lease.clone(),
        })
        .await
        .map(|_| ())
    }
}
#[async_trait::async_trait]
impl Supervisor for Remote {
    async fn stop(&self, stop: &StopOperation) -> Result<ManagerObservation> {
        let (state, _) = self
            .call(HostCommand::Stop {
                stop: Box::new(stop.clone()),
            })
            .await?;
        Ok(state
            .job_id
            .map_or(ManagerObservation::Unavailable {}, |job_id| {
                ManagerObservation::Submitted { job_id }
            }))
    }
    async fn terminated(&self, stop: &StopOperation) -> Result<Option<HeldTermination>> {
        let (state, connection) = self
            .call(HostCommand::Termination {
                stop: Box::new(stop.clone()),
            })
            .await?;
        if state.backend != stop.backend || state.stop.as_ref() != Some(stop) {
            return Err(Failure::Conflict);
        }
        Ok(state.terminated.then(|| {
            HeldTermination::from_supervisor(Arc::new(Authority {
                identity: state.backend,
                operation: Some(stop.operation_id.clone()),
                _connection: std::sync::Mutex::new(connection),
            }))
        }))
    }
}

#[derive(Clone)]
pub struct SharedTransport(Arc<playtime_ollama::NativeTransport>);
#[async_trait::async_trait]
impl playtime_ollama::Transport for SharedTransport {
    async fn send(
        &self,
        request: playtime_ollama::Request,
    ) -> playtime_ollama::Result<playtime_ollama::HttpResponse> {
        self.0.send(request).await
    }
}
struct Switchable(std::sync::RwLock<Arc<dyn Backend>>);
#[async_trait::async_trait]
impl Backend for Switchable {
    async fn verify_read_only(&self, expected: &BackendIdentity) -> Result<VerifiedBackend> {
        let port = self.0.read().map_err(|_| Failure::Blocked)?.clone();
        port.verify_read_only(expected).await
    }
    async fn execute(
        &self,
        backend: BackendIdentity,
        request: RequestSpec,
        body: Vec<u8>,
        observations: ObservationSink,
        cancel: CancellationToken,
    ) -> Result<VerifiedTerminal> {
        let port = self.0.read().map_err(|_| Failure::Blocked)?.clone();
        port.execute(backend, request, body, observations, cancel)
            .await
    }
}
async fn prepared(
    transport: SharedTransport,
    lock: playtime_ollama::ModelLock,
    identity: BackendIdentity,
    settings: playtime_ollama::EffectiveSettings,
    remote: Arc<Remote>,
    clock: Arc<SystemClock>,
) -> Result<Arc<dyn Backend>> {
    let backend = crate::ollama::OllamaBackend::prepare(
        transport,
        lock,
        identity,
        settings,
        remote.clone(),
        clock,
    );
    tokio::pin!(backend);
    loop {
        tokio::select! {
            result=&mut backend=>return result.map(|b|Arc::new(b) as Arc<dyn Backend>),
            _=tokio::time::sleep(Duration::from_millis(250))=>{remote.inspect().await?;notify(b"WATCHDOG=1")?;}
        }
    }
}
pub async fn serve_guard(config: Config, initialize: bool) -> Result<()> {
    if rustix::process::geteuid().as_raw() != config.uid {
        return Err(Failure::Unauthorized);
    }
    let remote = Arc::new(Remote {
        socket: "/run/playtime-backend-owner/owner.sock".into(),
        credential: config.credential.clone(),
        owner_uid: 0,
    });
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    let state = loop {
        match remote.inspect().await {
            Ok(state) => break state,
            Err(_) => {
                if tokio::time::Instant::now() >= until {
                    return Err(Failure::Expired);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    if state.backend.profile_sha256 != config.profile_sha256 {
        return Err(Failure::Conflict);
    }
    let lock_bytes =
        std::fs::read("/etc/playtime-backend/model.json").map_err(|_| Failure::Invalid)?;
    let lock = playtime_ollama::ModelLock::parse(&lock_bytes).map_err(|_| Failure::Invalid)?;
    let lock_sha256 = playtime_backend_guard::digest(&lock_bytes);
    let settings = playtime_ollama::EffectiveSettings::default();
    let settings_sha256 = playtime_backend_guard::digest(
        &serde_json::to_vec(&settings).map_err(|_| Failure::Invalid)?,
    );
    if playtime_backend_guard::digest(
        format!("{}:{}", lock_sha256.as_str(), settings_sha256.as_str()).as_bytes(),
    ) != config.profile_sha256
    {
        return Err(Failure::Conflict);
    }
    let clock = Arc::new(SystemClock::new()?);
    let transport = SharedTransport(Arc::new(
        playtime_ollama::NativeTransport::new(
            playtime_ollama::Endpoint::parse("http://127.0.0.1:11434")
                .map_err(|_| Failure::Invalid)?,
            playtime_ollama::Trust::BundledWebPki,
            &playtime_ollama::Limits::default(),
        )
        .map_err(|_| Failure::Invalid)?,
    ));
    // Preparation has its own 45-second bound. Heartbeats continue only while
    // the awaited metadata task is making bounded progress; no model warmup.
    let backend = Arc::new(Switchable(std::sync::RwLock::new(
        prepared(
            transport.clone(),
            lock.clone(),
            state.backend.clone(),
            settings.clone(),
            remote.clone(),
            clock.clone(),
        )
        .await?,
    )));
    let root = Path::new("/var/lib/playtime-backend/guard");
    let journal = if initialize {
        Journal::create(root)?
    } else {
        Journal::open(root)?
    };
    let guard = Guard::start_enforced(
        journal,
        initialize.then_some(state.backend),
        backend.clone(),
        remote.clone(),
        clock.clone(),
        Some(remote.clone()),
    )
    .await?;
    let server =
        playtime_backend_guard::transport::Server::bind(Path::new("/run/playtime-backend"))?;
    let credential = config.credential.clone();
    let clock2 = clock.clone();
    let serving = guard.clone();
    tokio::spawn(async move {
        server
            .serve(serving, credential, clock2, CancellationToken::new())
            .await
    });
    let readiness =
        UnixListener::bind("/run/playtime-backend/profile.sock").map_err(|_| Failure::Blocked)?;
    let quota = Arc::new(Semaphore::new(4));
    notify(b"READY=1")?;
    loop {
        tokio::select! {
            _=tokio::time::sleep(Duration::from_millis(250))=>{
                guard.tick(); let state=remote.inspect().await?;
                if !state.live {guard.enforcement_failed();}
                else if state.backend!=guard.status().backend {
                    if state.backend.profile_sha256!=config.profile_sha256{return Err(Failure::Conflict);}
                    let replacement=prepared(transport.clone(),lock.clone(),state.backend.clone(),settings.clone(),remote.clone(),clock.clone()).await?;
                    let verified=replacement.verify_read_only(&state.backend).await?;
                    *backend.0.write().map_err(|_|Failure::Blocked)?=replacement;
                    guard.prepare_generation(verified).await?;
                }
                notify(b"WATCHDOG=1")?;
            }
            accepted=readiness.accept()=>{
                let (mut stream,_)=accepted.map_err(|_|Failure::Blocked)?;
                let Ok(permit)=quota.clone().try_acquire_owned()else{continue;};
                let credential=config.credential.clone();let guard=guard.clone();let clock=clock.clone();let lock=lock_sha256.clone();let settings=settings_sha256.clone();let remote=remote.clone();
                tokio::spawn(async move {let _permit=permit;let _=tokio::time::timeout(Duration::from_secs(1),async {
                    if stream.peer_cred().map_err(|_|Failure::Unauthorized)?.uid()!=rustix::process::geteuid().as_raw(){return Err(Failure::Unauthorized);}
                    let challenge:protocol::Challenge=read_json(&mut stream).await?;
                    if challenge.version!=protocol::VERSION||!authorized(&credential,&challenge.credential){return Err(Failure::Unauthorized);}
                    let fresh=remote.inspect().await?;
                    if !fresh.live||fresh.backend!=guard.status().backend||guard.status().gate==Gate::Blocked{return Err(Failure::Blocked);}
                    write_json(&mut stream,&protocol::ProfileReply{version:protocol::VERSION,nonce:challenge.nonce,local_boot:challenge.local_boot,local_send_ms:challenge.local_send_ms,host_ms:clock.now().ms,backend:fresh.backend,lock_sha256:lock,settings_sha256:settings}).await
                }).await;});
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generation_and_deployment_share_one_nonblocking_fence() {
        let root = std::env::temp_dir().join(format!(
            "pt-maintenance-{}",
            playtime_backend_guard::random_id().unwrap().as_str()
        ));
        let directory = PrivateDirectory::open(&root, true).unwrap();
        directory.write_new("fence", b"").unwrap();
        let path = root.join("fence");
        let uid = rustix::process::geteuid().as_raw();
        let held = maintenance_lock(&path, uid).unwrap();
        let start = std::time::Instant::now();
        assert!(matches!(maintenance_lock(&path, uid), Err(Failure::Busy)));
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(held);
        drop(maintenance_lock(&path, uid).unwrap());
        let fifo = root.join("fifo");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert!(matches!(
            maintenance_lock(&fifo, uid),
            Err(Failure::Blocked)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
