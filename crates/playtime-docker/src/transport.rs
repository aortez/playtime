//! Typed Docker CLI transport. Child exit is only local evidence, never proof of
//! daemon-side cessation. All inherited Docker/context/TLS configuration is gone.
use crate::{CancellationToken, Error, Failure, Result, bridge::*, types::*};
use async_trait::async_trait;
use serde::Serialize;
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    time::Instant,
};

const QUERY_BYTES: usize = 65_536;
pub const PROBE: &str = "import hashlib,json,os,sys\np='/opt/playtime/tool.py'\nwith open(p,'rb') as f: b=f.read(262145)\nprint(json.dumps({'python':os.path.realpath(sys.executable),'bridge_sha256':hashlib.sha256(b).hexdigest(),'uid':os.getuid(),'gid':os.getgid()}))\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Version,
    Info,
    ImageInspect(String),
    ContainerInspect(String),
    VolumeInspect(String),
    CreateVolume(Proposal),
    CreateContainer(Proposal),
    Start(String),
    RemoveContainer(String),
    RemoveVolume(String),
    Bridge {
        id: String,
        payload: Vec<u8>,
    },
    Probe(String),
    Export {
        id: String,
        profile: crate::capture::CaptureProfile,
    },
    Memory {
        id: String,
        limit: u64,
    },
    Mounts(String),
}
impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Info => "info",
            Self::ImageInspect(_) => "image_inspect",
            Self::ContainerInspect(_) => "container_inspect",
            Self::VolumeInspect(_) => "volume_inspect",
            Self::CreateVolume(_) => "volume_create",
            Self::CreateContainer(_) => "container_create",
            Self::Start(_) => "container_start",
            Self::RemoveContainer(_) => "container_remove",
            Self::RemoveVolume(_) => "volume_remove",
            Self::Bridge { .. } => "tool",
            Self::Probe(_) => "helper_identity",
            Self::Export { .. } => "workspace_export",
            Self::Memory { .. } => "outgoing_memory",
            Self::Mounts(_) => "volume_mounts",
        }
    }
    pub fn argv(&self) -> Vec<String> {
        let args: Vec<&str> = match self {
            Self::Version => vec!["version", "--format", "{{json .}}"],
            Self::Info => vec!["info", "--format", "{{json .}}"],
            Self::ImageInspect(id) => vec!["image", "inspect", id],
            Self::ContainerInspect(id) => vec!["container", "inspect", id],
            Self::VolumeInspect(name) => vec!["volume", "inspect", name],
            Self::Start(id) => vec!["container", "start", id],
            Self::RemoveContainer(id) => vec!["container", "rm", "--force", id],
            Self::RemoveVolume(name) => vec!["volume", "rm", name],
            Self::Bridge { id, .. } => vec![
                "container",
                "exec",
                "--user",
                "1000:1000",
                "--workdir",
                "/",
                "--interactive",
                id,
                PYTHON,
                "-I",
                "-S",
                "-B",
                BRIDGE,
            ],
            Self::Export { id, .. } | Self::Memory { id, .. } => vec![
                "container",
                "exec",
                "--user",
                "1000:1000",
                "--workdir",
                "/",
                "--interactive",
                id,
                PYTHON,
                "-I",
                "-S",
                "-B",
                "-c",
                if matches!(self, Self::Export { .. }) {
                    crate::capture::EXPORTER
                } else {
                    crate::capture::MEMORY_READER
                },
            ],
            Self::Probe(id) => vec![
                "container",
                "exec",
                "--user",
                "1000:1000",
                "--workdir",
                "/",
                id,
                PYTHON,
                "-I",
                "-S",
                "-B",
                "-c",
                PROBE,
            ],
            Self::Mounts(volume) => {
                return vec![
                    "container".into(),
                    "ls".into(),
                    "--all".into(),
                    "--no-trunc".into(),
                    "--filter".into(),
                    format!("volume={volume}"),
                    "--format".into(),
                    "{{.ID}}".into(),
                ];
            }
            Self::CreateVolume(p) => {
                let mut args = vec![
                    "volume".into(),
                    "create".into(),
                    "--driver".into(),
                    "local".into(),
                ];
                labels(&mut args, p);
                for (key, value) in p.limits.volume_options() {
                    args.extend(["--opt".into(), format!("{key}={value}")]);
                }
                args.push(p.name.clone());
                return args;
            }
            Self::CreateContainer(p) => return create_args(p),
        };
        args.into_iter().map(str::to_owned).collect()
    }
    fn input(&self) -> Vec<u8> {
        match self {
            Self::Bridge { payload, .. } => payload.clone(),
            Self::Export { profile, .. } => profile.payload(),
            Self::Memory { limit, .. } => limit.to_string().into_bytes(),
            _ => Vec::new(),
        }
    }
    fn stdout_limit(&self) -> usize {
        if let Self::Export { profile, .. } = self {
            profile.limits().tar_bytes as usize
        } else if matches!(self, Self::Bridge { .. } | Self::Memory { .. }) {
            RESPONSE_BYTES
        } else {
            QUERY_BYTES
        }
    }
}
fn labels(args: &mut Vec<String>, p: &Proposal) {
    for (key, value) in p.labels() {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
}
fn create_args(p: &Proposal) -> Vec<String> {
    let l = &p.limits;
    let (memory, pids) = if p.role == Role::Holder {
        (64 * 1024 * 1024, 64)
    } else {
        (l.memory_bytes, l.pids)
    };
    let mut args: Vec<String> = vec![
        "container",
        "create",
        "--pull",
        "never",
        "--platform",
        "linux/amd64",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "0:0",
        "--log-driver",
        "none",
        "--restart",
        "no",
        "--no-healthcheck",
        "--cgroupns",
        "private",
        "--ipc",
        "private",
        "--workdir",
        "/",
        "--hostname",
        "playtime-workshop",
        "--entrypoint",
        PYTHON,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.extend([
        "--name".into(),
        p.name.clone(),
        "--cpus".into(),
        format!(
            "{}.{:09}",
            l.nano_cpus / 1_000_000_000,
            l.nano_cpus % 1_000_000_000
        ),
        "--memory".into(),
        memory.to_string(),
        "--memory-swap".into(),
        memory.to_string(),
        "--pids-limit".into(),
        pids.to_string(),
        "--shm-size".into(),
        l.tmp_bytes.to_string(),
        "--tmpfs".into(),
        format!("/tmp:{}", l.tmpfs()),
        "--mount".into(),
        format!("type=volume,src={},dst=/work,volume-nocopy", p.volume),
        "--env".into(),
        format!("PLAYTIME_SEED={}", l.seed),
        "--env".into(),
        format!("PYTHONHASHSEED={}", l.seed),
    ]);
    labels(&mut args, p);
    args.extend([
        p.image.clone(),
        "-I".into(),
        "-S".into(),
        "-B".into(),
        "-c".into(),
        TIMER.into(),
        p.deadline_monotonic_ns.to_string(),
    ]);
    args
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dispatch {
    NotStarted,
    MayHaveReachedDaemon,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalExit {
    NotStarted,
    Reaped {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Reaping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    Deadline,
    Cancelled,
    Overflow,
    Io,
    SelectionChanged,
    Busy,
}
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub dispatch: Dispatch,
    pub local: LocalExit,
    pub fault: Option<Fault>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl Outcome {
    pub fn success(&self) -> bool {
        self.fault.is_none()
            && self.local
                == LocalExit::Reaped {
                    code: Some(0),
                    signal: None,
                }
    }
    pub fn completed(stdout: Vec<u8>) -> Self {
        Self {
            dispatch: Dispatch::MayHaveReachedDaemon,
            local: LocalExit::Reaped {
                code: Some(0),
                signal: None,
            },
            fault: None,
            stdout,
            stderr: vec![],
        }
    }
    fn failed(fault: Fault, dispatch: Dispatch, local: LocalExit) -> Self {
        Self {
            dispatch,
            local,
            fault: Some(fault),
            stdout: vec![],
            stderr: vec![],
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn execute(
        &self,
        operation: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Outcome;
}

#[derive(Clone, Debug, Serialize)]
pub struct Selection {
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub socket: PathBuf,
}
#[derive(Clone)]
pub struct Cli {
    selection: Selection,
    config: PathBuf,
    fingerprint: Fingerprint,
    config_fingerprint: Fingerprint,
    slots: Arc<Semaphore>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Fingerprint {
    fn read(path: &Path) -> std::io::Result<Self> {
        let m = std::fs::metadata(path)?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}
impl Cli {
    /// Resolves a trusted operator selection, never Docker's inherited context.
    /// `config` must be a new directory under a private host-owned task root.
    pub fn resolve(executable: &Path, socket: &Path, config: &Path) -> Result<Self> {
        let bad = || {
            Error::new(
                Failure::Unsupported,
                "selection",
                "requires a local Linux executable and root-owned Unix socket",
            )
        };
        if !cfg!(target_os = "linux")
            || !executable.is_absolute()
            || !socket.is_absolute()
            || !config.is_absolute()
        {
            return Err(bad());
        }
        let parent = config.parent().ok_or_else(bad)?;
        let parent = std::fs::metadata(parent).map_err(|_| bad())?;
        if !parent.is_dir()
            || parent.mode() & 0o077 != 0
            || parent.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(Error::new(
                Failure::InvalidInput,
                "selection",
                "configuration parent must be private and owned by this user",
            ));
        }
        let executable = std::fs::canonicalize(executable).map_err(|_| bad())?;
        let socket = std::fs::canonicalize(socket).map_err(|_| bad())?;
        let exe = std::fs::metadata(&executable).map_err(|_| bad())?;
        let sock = std::fs::metadata(&socket).map_err(|_| bad())?;
        if !exe.is_file()
            || exe.mode() & 0o111 == 0
            || exe.mode() & 0o022 != 0
            || !sock.file_type().is_socket()
            || sock.uid() != 0
            || exe.len() > 128 * 1024 * 1024
        {
            return Err(bad());
        }
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(config)
            .map_err(|_| {
                Error::new(
                    Failure::InvalidInput,
                    "selection",
                    "Docker config must be a new private directory",
                )
            })?;
        let fingerprint = Fingerprint::read(&executable).map_err(|_| bad())?;
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&executable)
            .map_err(|_| bad())?
            .take(128 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| bad())?;
        if bytes.len() > 128 * 1024 * 1024
            || Fingerprint::read(&executable).map_err(|_| bad())? != fingerprint
        {
            return Err(bad());
        }
        let config_fingerprint = Fingerprint::read(config).map_err(|_| bad())?;
        Ok(Self {
            selection: Selection {
                executable,
                executable_sha256: hash(&bytes),
                socket,
            },
            config: config.to_owned(),
            fingerprint,
            config_fingerprint,
            slots: Arc::new(Semaphore::new(4)),
        })
    }
    pub fn selection(&self) -> &Selection {
        &self.selection
    }
    fn command(&self, operation: &Operation) -> std::io::Result<Command> {
        if Fingerprint::read(&self.selection.executable)? != self.fingerprint
            || Fingerprint::read(&self.config)? != self.config_fingerprint
            || !std::fs::symlink_metadata(&self.config)?.is_dir()
            || std::fs::read_dir(&self.config)?.next().is_some()
            || std::fs::metadata(&self.config)?.permissions().mode() & 0o777 != 0o700
        {
            return Err(std::io::Error::other("frozen selection changed"));
        }
        let mut cmd = Command::new(&self.selection.executable);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        cmd.arg("--config")
            .arg(&self.config)
            .arg("--host")
            .arg(format!("unix://{}", self.selection.socket.display()))
            .args(operation.argv());
        Ok(cmd)
    }
}
#[async_trait]
impl Transport for Cli {
    async fn execute(
        &self,
        operation: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Outcome {
        if operation.input().len() > REQUEST_BYTES {
            return Outcome::failed(Fault::Overflow, Dispatch::NotStarted, LocalExit::NotStarted);
        }
        let Ok(permit) = self.slots.clone().try_acquire_owned() else {
            return Outcome::failed(Fault::Busy, Dispatch::NotStarted, LocalExit::NotStarted);
        };
        let selected = self.clone();
        let owned_cancel = cancel.child_token();
        let guard = owned_cancel.clone().drop_guard();
        let mut task = tokio::spawn(async move {
            let _permit = permit;
            // Selection checks may block on host I/O. The outer bound retains
            // this permit/task, and cancellation is checked before spawning.
            let prepared = tokio::task::spawn_blocking(move || {
                selected.command(&operation).map(|cmd| (cmd, operation))
            })
            .await;
            match prepared {
                Ok(Ok((cmd, operation))) => {
                    run(
                        cmd,
                        operation.input(),
                        operation.stdout_limit(),
                        deadline,
                        owned_cancel,
                    )
                    .await
                }
                _ => Outcome::failed(
                    Fault::SelectionChanged,
                    Dispatch::NotStarted,
                    LocalExit::NotStarted,
                ),
            }
        });
        // Reaping belongs to the owned task even if the controller drops its
        // future. No unbounded collection of abandoned CLI children is possible.
        let result = tokio::select! {
            biased;
            result=&mut task=>result.unwrap_or_else(|_|Outcome::failed(Fault::Io,Dispatch::MayHaveReachedDaemon,LocalExit::Reaping)),
            _=cancel.cancelled()=>Outcome::failed(Fault::Cancelled,Dispatch::MayHaveReachedDaemon,LocalExit::Reaping),
            _=tokio::time::sleep_until(deadline)=>Outcome::failed(Fault::Deadline,Dispatch::MayHaveReachedDaemon,LocalExit::Reaping),
        };
        drop(guard);
        result
    }
}

async fn read_bounded(mut pipe: impl AsyncRead + Unpin, limit: usize) -> (Vec<u8>, Option<Fault>) {
    let mut kept = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) => return (kept, None),
            Ok(n) => {
                let take = n.min(limit.saturating_sub(kept.len()));
                kept.extend_from_slice(&buffer[..take]);
                if take < n {
                    return (kept, Some(Fault::Overflow));
                }
            }
            Err(_) => return (kept, Some(Fault::Io)),
        }
    }
}
async fn run(
    mut command: Command,
    input: Vec<u8>,
    limit: usize,
    deadline: Instant,
    cancel: CancellationToken,
) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    if cancel.is_cancelled() {
        return Outcome::failed(
            Fault::Cancelled,
            Dispatch::NotStarted,
            LocalExit::NotStarted,
        );
    }
    if Instant::now() >= deadline {
        return Outcome::failed(Fault::Deadline, Dispatch::NotStarted, LocalExit::NotStarted);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Outcome::failed(Fault::Io, Dispatch::NotStarted, LocalExit::NotStarted),
    };
    let pid = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let write = async move {
        stdin.write_all(&input).await?;
        stdin.shutdown().await
    };
    let out = read_bounded(stdout, limit);
    let err = read_bounded(stderr, DIAGNOSTIC_BYTES);
    tokio::pin!(write, out, err);
    let mut written = false;
    let mut output = None;
    let mut errors = None;
    let mut status = None;
    let fault = loop {
        if written && output.is_some() && errors.is_some() && status.is_some() {
            break None;
        }
        tokio::select! {
            biased;
            _=cancel.cancelled()=>break Some(Fault::Cancelled),
            _=tokio::time::sleep_until(deadline)=>break Some(Fault::Deadline),
            result=&mut write,if !written=>{ written=true; if result.is_err() { break Some(Fault::Io); } },
            (bytes,fault)=&mut out,if output.is_none()=>{ output=Some(bytes); if fault.is_some() { break fault; } },
            (bytes,fault)=&mut err,if errors.is_none()=>{ errors=Some(bytes); if fault.is_some() { break fault; } },
            // Retain the unreaped leader until pipes settle. Its PID/process
            // group cannot be reused while a descendant holds a pipe open.
            result=child.wait(),if status.is_none() && written && output.is_some() && errors.is_some()=>match result { Ok(s)=>status=Some(s),Err(_)=>break Some(Fault::Io) },
        }
    };
    if fault.is_some() {
        if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.start_kill();
        if status.is_none() {
            status = child.wait().await.ok();
        }
    }
    Outcome {
        dispatch: Dispatch::MayHaveReachedDaemon,
        local: status
            .map(|s| LocalExit::Reaped {
                code: s.code(),
                signal: s.signal(),
            })
            .unwrap_or(LocalExit::Reaping),
        fault,
        stdout: output.unwrap_or_default(),
        stderr: errors.unwrap_or_default(),
    }
}

/// A deterministic CPU-only port: exact typed requests and FIFO replies. No
/// subprocesses or Docker discovery. Useful for independent adapter fixtures.
#[derive(Default)]
pub struct ScriptedTransport {
    steps: std::sync::Mutex<std::collections::VecDeque<(Operation, Outcome)>>,
    seen: std::sync::Mutex<Vec<Operation>>,
}
impl ScriptedTransport {
    pub fn push(&self, operation: Operation, outcome: Outcome) {
        self.steps
            .lock()
            .expect("steps")
            .push_back((operation, outcome));
    }
    pub fn seen(&self) -> Vec<Operation> {
        self.seen.lock().expect("seen").clone()
    }
    pub fn remaining(&self) -> usize {
        self.steps.lock().expect("steps").len()
    }
}
#[async_trait]
impl Transport for ScriptedTransport {
    async fn execute(
        &self,
        operation: Operation,
        _deadline: Instant,
        _cancel: CancellationToken,
    ) -> Outcome {
        self.seen.lock().expect("seen").push(operation.clone());
        let (expected, result) = self
            .steps
            .lock()
            .expect("steps")
            .pop_front()
            .expect("unexpected Docker operation");
        assert_eq!(operation, expected);
        result
    }
}

#[cfg(test)]
mod tests;
