#![allow(dead_code)]
use playtime_backend_guard::{
    capability, digest,
    fixture::{self, FixtureCommand, FixtureResult},
    ports::{Clock, SystemClock},
    random_id,
    transport::Client,
    wire::*,
};
use playtime_core::RunId;
use playtime_local::PrivateDirectory;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command as ProcessCommand, Stdio},
    time::Duration,
};

pub struct Process(pub Child);
impl Process {
    pub fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}
pub struct Harness {
    pub root: PathBuf,
    pub identity: BackendIdentity,
    pub secret: Capability,
    pub remote: fixture::Remote,
    pub backend: Process,
    pub guards: Vec<Process>,
}
pub fn private(path: &Path) {
    PrivateDirectory::open(path, true).unwrap();
}
pub fn now() -> u64 {
    SystemClock::new().unwrap().now().ms
}
pub fn identity() -> BackendIdentity {
    BackendIdentity {
        backend_id: random_id().unwrap(),
        guard_id: random_id().unwrap(),
        boot_id: SystemClock::new().unwrap().now().boot_id,
        unit: "fixture.service".into(),
        invocation_id: random_id().unwrap(),
        cgroup: "/fixture/backend".into(),
        generation: 1,
        profile_sha256: digest(b"synthetic profile"),
    }
}
pub fn lease(identity: &BackendIdentity) -> LeaseSpec {
    LeaseSpec {
        operation_id: random_id().unwrap(),
        lease_id: random_id().unwrap(),
        backend: identity.clone(),
        service_id: random_id().unwrap(),
        run_id: RunId::new("synthetic-run").unwrap(),
        manifest_sha256: digest(b"manifest"),
        worker_id: random_id().unwrap(),
        grant_id: random_id().unwrap(),
        owner_revision: 1,
        execution_deadline_ms: now() + 60_000,
        cleanup_deadline_ms: now() + 180_000,
    }
}
pub fn request(lease: &LeaseSpec, ordinal: u64) -> RequestSpec {
    RequestSpec {
        lease_id: lease.lease_id.clone(),
        generation: lease.backend.generation,
        ordinal,
        request_id: random_id().unwrap(),
        effect_id: random_id().unwrap(),
        body_sha256: digest(b"x"),
        body_bytes: 1,
        original_max_tokens: 256,
        deadline_ms: lease.execution_deadline_ms,
    }
}
async fn spawn(mode: &str, config: &Path) -> Process {
    let mut child = Process(
        ProcessCommand::new(env!("CARGO_BIN_EXE_playtime-backend-guard-fixture"))
            .arg(mode)
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let line = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::BufReader::new(stdout)
                .read_line(&mut line)
                .unwrap();
            line
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(line.trim(), "ready");
    child
}
impl Harness {
    pub async fn new(mode: fixture::Mode) -> Self {
        Self::with_identity(mode, identity()).await
    }
    pub async fn with_identity(mode: fixture::Mode, identity: BackendIdentity) -> Self {
        let root = std::env::temp_dir().join(format!("g-{}", &random_id().unwrap().as_str()[..12]));
        private(&root);
        private(&root.join("backend"));
        let secret = capability().unwrap();
        let credential = capability().unwrap();
        let config = json!({"identity":identity,"credential":credential,"ipc":root.join("backend"),"state":root.join("unused"),"backend":null,"create":false,"mode":mode,"barrier":null});
        PrivateDirectory::open(&root, false)
            .unwrap()
            .write_new("backend.json", &serde_json::to_vec(&config).unwrap())
            .unwrap();
        let backend = spawn("backend", &root.join("backend.json")).await;
        let remote = fixture::Remote {
            endpoint: fixture::Endpoint {
                socket: root.join("backend/backend.sock"),
                pid: backend.0.id(),
                credential,
            },
        };
        Self {
            root,
            identity,
            secret,
            remote,
            backend,
            guards: Vec::new(),
        }
    }
    pub async fn start(&mut self, create: bool, barrier: Option<serde_json::Value>) -> Client {
        self.start_with_endpoint(create, barrier, self.remote.endpoint.clone())
            .await
    }
    pub async fn start_with_endpoint(
        &mut self,
        create: bool,
        barrier: Option<serde_json::Value>,
        endpoint: fixture::Endpoint,
    ) -> Client {
        let index = self.guards.len();
        let ipc = self.root.join(format!("ipc-{index}"));
        private(&ipc);
        let config = json!({"identity":self.identity,"credential":self.secret,"ipc":ipc,"state":self.root.join("state"),"backend":endpoint,"create":create,"mode":"terminal","barrier":barrier});
        let name = format!("guard-{index}.json");
        PrivateDirectory::open(&self.root, false)
            .unwrap()
            .write_new(&name, &serde_json::to_vec(&config).unwrap())
            .unwrap();
        self.guards
            .push(spawn("guard", &self.root.join(name)).await);
        Client::new(ipc, self.secret.clone())
    }
    pub async fn stats(&self) -> (Vec<RequestSpec>, Vec<StopOperation>, u32) {
        match self.remote.command(FixtureCommand::Stats).await.unwrap() {
            FixtureResult::Stats {
                sends,
                stops,
                alive,
            } => (sends, stops, alive),
            _ => panic!("wrong fixture reply"),
        }
    }
    pub async fn set_mode(&self, mode: fixture::Mode) {
        assert!(matches!(
            self.remote
                .command(FixtureCommand::SetMode { mode })
                .await
                .unwrap(),
            FixtureResult::Ok
        ));
    }
    pub async fn wait_sends(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.stats().await.0.len() == count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        for guard in &mut self.guards {
            guard.kill();
        }
        // Ask the independently owned fixture manager to kill AND reap both
        // children before terminating it. Never use a missing PID as proof.
        let shutdown = (|| -> std::io::Result<()> {
            use std::io::{Read, Write};
            let mut stream = std::os::unix::net::UnixStream::connect(&self.remote.endpoint.socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_secs(3)))?;
            let request = json!({"version":1,"nonce":random_id().unwrap(),"credential":self.remote.endpoint.credential,"command":{"command":"shutdown"}});
            let bytes = serde_json::to_vec(&request)?;
            stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
            stream.write_all(&bytes)?;
            let mut header = [0; 4];
            stream.read_exact(&mut header)?;
            let mut reply = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut reply)?;
            Ok(())
        })();
        if shutdown.is_ok() {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while self.backend.0.try_wait().unwrap().is_none()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.backend.kill();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
pub async fn status_until(client: &Client, predicate: impl Fn(&Status) -> bool) -> Status {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = client.control(Command::Query {}).await.unwrap();
            if predicate(&status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
pub async fn finish(mut stream: tokio::net::UnixStream) -> (Vec<u8>, Status) {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut all = Vec::new();
        loop {
            match stream.read_u8().await.unwrap() {
                1 => {
                    let n = stream.read_u32().await.unwrap() as usize;
                    assert!(n <= FRAME_BYTES);
                    let mut bytes = vec![0; n];
                    stream.read_exact(&mut bytes).await.unwrap();
                    all.extend(bytes);
                }
                2 => {
                    let reply: ControlReply =
                        playtime_backend_guard::transport::read_json(&mut stream)
                            .await
                            .unwrap();
                    if let Outcome::Status { status } = reply.outcome {
                        return (all, *status);
                    }
                    panic!("failed terminal {reply:?}");
                }
                other => panic!("bad stream tag {other}"),
            }
        }
    })
    .await
    .unwrap()
}
