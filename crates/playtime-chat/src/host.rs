//! Explicitly trusted same-user host execution. Scopes own processes, not access.
//! The shared controller still owns dispatch, history and compaction.
use crate::{Result, runtime::Effects};
use playtime_engine::{Effect, EffectKind, Outcome};
use playtime_local::PrivateDirectory;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const HELPER: &str = include_str!("../../../scripts/chat/host_tools.py");
pub const PROFILE: &str = "host-coding-v1";

pub struct Workspace {
    pub root: PathBuf,
    state: PathBuf,
    device: u64,
    inode: u64,
    _lease: File,
    supervisor: Option<Supervisor>,
    failed: bool,
    output_bytes: u32,
}
struct Supervisor {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Supervisor {
    async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        // The fixed helper bounds each reply; reject protocol loss rather than retrying a tool.
        self.output.read_line(&mut line).await?;
        if line.len() > 2 * 1024 * 1024 || line.is_empty() {
            return Err("host helper lost its control stream".into());
        }
        Ok(serde_json::from_str(&line)?)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }
}
impl Workspace {
    pub fn open(root: &Path, private: &Path, output_bytes: u32) -> Result<Self> {
        if !(1..=65536).contains(&output_bytes) {
            return Err("host output cap must be between 1 and 65536 bytes per stream".into());
        }
        let root = root.canonicalize()?;
        let metadata = root.metadata()?;
        if !metadata.is_dir() {
            return Err("host workspace must be a directory".into());
        }
        let state = private.join(format!("project-{}-{}", metadata.dev(), metadata.ino()));
        let directory = PrivateDirectory::open(&state, true)?;
        let lease = directory
            .try_lock()
            .map_err(|e| format!("workspace is already in use: {e}"))?;
        if directory.names()?.iter().any(|name| name == "active") {
            return Err(format!(
                "workspace has unresolved tool cleanup; inspect {} before launching again",
                state.join("active").display()
            )
            .into());
        }
        Ok(Self {
            root,
            state,
            device: metadata.dev(),
            inode: metadata.ino(),
            _lease: lease,
            supervisor: None,
            failed: false,
            output_bytes,
        })
    }
    pub fn binding(&self) -> Value {
        json!({"profile":PROFILE,"path":self.root,"device":self.device,"inode":self.inode,
            "uid":rustix::process::geteuid().as_raw(),"gid":rustix::process::getegid().as_raw(),
            "helper_sha256":Sha256::digest(HELPER).iter().map(|b| format!("{b:02x}")).collect::<String>(),"shell":"/bin/bash",
            "process_owner":"systemd-user-scope-v1","output_bytes":self.output_bytes})
    }
    pub fn inventory(&self) -> String {
        format!(
            "Host coding tools run directly on this Linux workstation as uid {} with the caller's environment and normal filesystem, network, USB/serial and IPC permissions. The starting directory is {}. Relative tool paths use that directory; absolute paths are ordinary host paths. Commands use Bash with inherited PATH and HOME. No Docker filesystem or /work remapping is used. There is no additional filesystem/network sandbox or privilege escalation tool. Use the installed toolchains; discover them rather than guessing. Each exec owns a transient systemd user scope and stops its descendants when that command ends. Explicit start_process jobs (optionally a PTY) survive turns and compaction in this running CLI, until stop_process or CLI exit. Conversation resume starts a new process registry; use poll_process without an ID to list current handles. Never assume a saved process handle is still running. Session records live outside the checkout but same-user host tools are not isolated from them; do not edit harness records or the running installation.",
            rustix::process::geteuid().as_raw(),
            self.root.display()
        )
    }
    fn verify(&self) -> Result<()> {
        let metadata = self.root.metadata()?;
        if metadata.dev() != self.device || metadata.ino() != self.inode {
            return Err("host workspace directory was replaced".into());
        }
        Ok(())
    }
    async fn start(&mut self) -> Result<()> {
        self.verify()?;
        if self.failed {
            return Err("host helper is fenced after a control failure".into());
        }
        if self.supervisor.is_some() {
            return Ok(());
        }
        let status = Command::new("/usr/bin/systemctl")
            .args(["--user", "show", "--property=Version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        if !tokio::time::timeout(Duration::from_secs(5), status)
            .await??
            .success()
        {
            return Err("host coding needs a running systemd user manager".into());
        }
        let mut child = Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-u", "-c", HELPER])
            .arg(&self.root)
            .arg(&self.state)
            .arg(self.output_bytes.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take().ok_or("missing host control input")?;
        let output = BufReader::new(child.stdout.take().ok_or("missing host control output")?);
        self.supervisor = Some(Supervisor {
            child,
            input,
            output,
        });
        let supervisor = self.supervisor.as_mut().unwrap();
        let ready = tokio::time::timeout(Duration::from_secs(5), supervisor.read()).await??;
        if ready["ready"] != true {
            return Err("host helper did not become ready".into());
        }
        Ok(())
    }
    pub async fn call(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Value> {
        self.start().await?;
        if cancel.is_cancelled() || Instant::now() >= deadline {
            return Ok(json!({"error":"host tool cancelled before dispatch"}));
        }
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_secs(2))
            .as_secs_f64();
        let supervisor = self.supervisor.as_mut().unwrap();
        self.failed = true;
        supervisor
            .send(json!({"name":name,"arguments":arguments,"timeout_seconds":timeout}))
            .await?;
        // Do not cancel read_line: a partial JSON reply must remain in this one reader.
        let input = &mut supervisor.input;
        let output = &mut supervisor.output;
        let mut line = String::new();
        let read = output.read_line(&mut line);
        tokio::pin!(read);
        tokio::select! {
            result = &mut read => { result?; },
            _ = async { tokio::select! { _=cancel.cancelled()=>(), _=tokio::time::sleep_until(deadline)=>() } } => {
                input.write_all(b"{\"op\":\"cancel\"}\n").await?;
                input.flush().await?;
                tokio::time::timeout(Duration::from_secs(20), &mut read).await??;
            }
        }
        if line.len() > 2 * 1024 * 1024 || line.is_empty() {
            return Err("host tool reply missing or oversized".into());
        }
        let reply: Value = serde_json::from_str(&line)?;
        if reply.get("closed").is_some() {
            return Err(format!("host tool supervisor stopped: {reply}").into());
        }
        self.failed = false;
        Ok(reply)
    }
    pub async fn close(&mut self) -> Result<()> {
        let Some(mut supervisor) = self.supervisor.take() else {
            return Ok(());
        };
        // Closing control input also covers failure midway through a request.
        supervisor.input.shutdown().await?;
        drop(supervisor.input);
        let status =
            tokio::time::timeout(Duration::from_secs(30), supervisor.child.wait()).await??;
        if !status.success() || self.state.join("active").exists() {
            return Err(format!(
                "host process cleanup unresolved; inspect {}",
                self.state.join("active").display()
            )
            .into());
        }
        self.failed = false;
        Ok(())
    }
}

pub struct HostEffects<E> {
    pub inner: E,
    pub workspace: Arc<Mutex<Workspace>>,
}
#[async_trait::async_trait]
impl<E: Effects> Effects for HostEffects<E> {
    async fn execute(
        &mut self,
        effect: &Effect,
        deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<Value>,
    ) -> Outcome {
        match &effect.kind {
            EffectKind::StartWorkspace => match self.workspace.lock().await.start().await {
                Ok(()) => Outcome::WorkspaceStarted,
                Err(error) => Outcome::Failed(error.to_string()),
            },
            EffectKind::Tool(request) => match self
                .workspace
                .lock()
                .await
                .call(&request.name, request.arguments.clone(), deadline, cancel)
                .await
            {
                Ok(value) => Outcome::Tool(value),
                Err(error) => Outcome::Failed(error.to_string()),
            },
            EffectKind::Cleanup => Outcome::Cleaned(self.cleanup(deadline).await),
            _ => self.inner.execute(effect, deadline, cancel, chunks).await,
        }
    }
    async fn cleanup(&mut self, _: Instant) -> Vec<String> {
        // exec owns its complete scope. Explicit persistent jobs belong to the
        // live CLI session and are closed by Workspace::close on every exit path.
        if self.workspace.lock().await.failed {
            vec!["host tool cleanup/control is unresolved".into()]
        } else {
            vec![]
        }
    }
}
