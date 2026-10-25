//! Reusable native-client test support: owned processes, port-zero discovery,
//! isolated client state, bounded output, and credential-free failure evidence.
pub mod fixture;
pub mod service;
pub mod service_loss;
pub mod suite;
use playtime_client::Client;
use playtime_local::{Token, read_connection};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    time::{Instant, sleep, timeout},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub struct TestRoot(pub PathBuf);
impl TestRoot {
    pub fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "playtime-cli-{}-{}",
            std::process::id(),
            &Token::generate()?.expose()[..16]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&root)?;
        Ok(Self(root))
    }
    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        if self.0.join(".retain-evidence").exists() {
            eprintln!(
                "Retained incomplete gate evidence and ownership: {}",
                self.0.display()
            );
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn isolated(program: &Path, root: &TestRoot) -> Command {
    let mut command = Command::new(program);
    for name in [
        "PLAYTIME_CONNECTION_FILE",
        "PLAYTIME_ENDPOINT",
        "PLAYTIME_TOKEN_FILE",
        "PLAYTIME_CONFIG",
        "PLAYTIME_STATE_DIR",
    ] {
        command.env_remove(name);
    }
    command
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .current_dir(&root.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}
#[derive(Debug)]
pub struct Captured {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl Captured {
    pub fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&self.stdout)?)
    }
    pub fn frames(&self) -> Result<Vec<Value>> {
        self.stdout
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| Ok(serde_json::from_slice(b)?))
            .collect()
    }
    pub fn expect(&self, code: i32) -> Result<()> {
        if self.code != code {
            return Err(format!(
                "exit {}, expected {code}; stderr: {}",
                self.code,
                String::from_utf8_lossy(&self.stderr)
            )
            .into());
        }
        Ok(())
    }
}
async fn bounded_read<R: tokio::io::AsyncRead + Unpin>(reader: R) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((1 << 20) + 1).read_to_end(&mut bytes).await?;
    if bytes.len() > 1 << 20 {
        return Err("subprocess output exceeds 1 MiB".into());
    }
    Ok(bytes)
}
pub async fn capture(mut child: Child) -> Result<Captured> {
    timeout(Duration::from_secs(30), async {
        let stdout = child.stdout.take().ok_or("missing stdout")?;
        let stderr = child.stderr.take().ok_or("missing stderr")?;
        let (status, stdout, stderr) = tokio::try_join!(
            async { Ok::<_, Box<dyn std::error::Error + Send + Sync>>(child.wait().await?) },
            bounded_read(stdout),
            bounded_read(stderr)
        )?;
        Ok(Captured {
            code: status.code().unwrap_or(-1),
            stdout,
            stderr,
        })
    })
    .await
    .map_err(|_| "owned subprocess exceeded 30 seconds")?
}
pub struct Service {
    child: Child,
    pub connection: PathBuf,
    pub root: PathBuf,
    pub endpoint: String,
}
impl Service {
    pub async fn start(
        fixture: &Path,
        root: &TestRoot,
        name: &str,
        scenario: &str,
        identity: &str,
        listen: Option<&str>,
    ) -> Result<Self> {
        let service_root = root.join(name);
        let mut command = isolated(fixture, root);
        command
            .arg("--root")
            .arg(&service_root)
            .arg("--scenario")
            .arg(scenario)
            .arg("--service-id")
            .arg(identity);
        if let Some(listen) = listen {
            command.arg("--listen").arg(listen);
        }
        let mut child = command.spawn()?;
        let connection = service_root.join(".playtime/connection.json");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!("fixture exited during readiness: {status}").into());
            }
            if let Ok(client) = Client::from_connection(&connection).await {
                return Ok(Self {
                    child,
                    connection,
                    root: service_root,
                    endpoint: client.endpoint().to_owned(),
                });
            }
            if Instant::now() >= deadline {
                return Err("fixture readiness deadline exceeded".into());
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn stop(mut self) -> Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        Ok(())
    }
}
pub fn signal_child(child: &Child, signal: rustix::process::Signal) -> Result<()> {
    let id = child.id().ok_or("owned child already exited")?;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(id as i32).ok_or("invalid child PID")?,
        signal,
    )?;
    Ok(())
}
/// Saves only allowlisted diagnostics. Credential files and client receipts are
/// never copied; request logs contain operation names and opaque IDs only.
pub fn failure_bundle(root: &TestRoot, records: &[Value], message: &str) -> Result<PathBuf> {
    let bundle = std::env::temp_dir().join(format!(
        "playtime-cli-failure-{}-{}",
        std::process::id(),
        &Token::generate()?.expose()[..8]
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&bundle)?;
    let mut redacted =
        serde_json::to_string_pretty(&json!({"failure":message,"commands":records}))?;
    for entry in std::fs::read_dir(&root.0)? {
        let entry = entry?;
        let descriptor = entry.path().join(".playtime/connection.json");
        if let Ok((_, token)) = read_connection(&descriptor) {
            redacted = redacted.replace(&token.expose(), "[REDACTED]");
        }
        let trace = entry.path().join("requests.json");
        if let Ok(bytes) = std::fs::read(&trace) {
            std::fs::write(
                bundle.join(format!(
                    "{}-requests.json",
                    entry.file_name().to_string_lossy()
                )),
                &bytes[..bytes.len().min(256 << 10)],
            )?;
        }
    }
    std::fs::write(
        bundle.join("commands.json"),
        &redacted.as_bytes()[..redacted.len().min(4 << 20)],
    )?;
    Ok(bundle)
}
pub async fn run_command(
    cli: &Path,
    root: &TestRoot,
    connection: Option<&Path>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    records: &mut Vec<Value>,
) -> Result<Captured> {
    let mut command = isolated(cli, root);
    if let Some(connection) = connection {
        command.arg("--connection-file").arg(connection);
    }
    let args: Vec<_> = args.into_iter().map(|s| s.as_ref().to_owned()).collect();
    command.args(&args);
    let captured = capture(command.spawn()?).await?;
    records.push(json!({"args":args.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>(),"code":captured.code,"stdout":String::from_utf8_lossy(&captured.stdout),"stderr":String::from_utf8_lossy(&captured.stderr)}));
    Ok(captured)
}

/// Bounded metadata-only fake; never performs inference.
pub mod model_metadata;

/// Synthetic retained registrations for authoring tests, never native model evidence.
pub mod recipe_models;
