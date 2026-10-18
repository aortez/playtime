//! Explicit CPU-only owned-child backend/supervisor and authenticated local
//! bridge. No Ollama compatibility logic, SSH, systemd or production capability.
use crate::{
    Result, digest,
    ports::*,
    transport::{read_json, write_json},
    wire::*,
};
use playtime_protocol::worker::ControlId;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{Mutex, Notify},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub socket: PathBuf,
    pub pid: u32,
    pub credential: Capability,
}
#[derive(Clone)]
pub struct Remote {
    pub endpoint: Endpoint,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Terminal,
    Hang,
    Flood,
    Error,
    HoldTerminal,
    Unavailable,
    Descendant,
    LostStopReply,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureCommand {
    Verify {
        expected: BackendIdentity,
    },
    Execute {
        backend: BackendIdentity,
        request: RequestSpec,
    },
    Stop {
        operation: StopOperation,
    },
    Terminated {
        operation: StopOperation,
    },
    Stats,
    SetMode {
        mode: Mode,
    },
    Finish,
    Cleanup,
    Shutdown,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    nonce: ControlId,
    credential: Capability,
    command: FixtureCommand,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureResult {
    Verified {
        backend: BackendIdentity,
    },
    Terminal {
        backend: BackendIdentity,
        request: RequestSpec,
    },
    Observation {
        observation: ManagerObservation,
    },
    Terminated {
        operation: StopOperation,
    },
    Pending,
    Stats {
        sends: Vec<RequestSpec>,
        stops: Vec<StopOperation>,
        alive: u32,
    },
    Ok,
    Failed,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    nonce: ControlId,
    result: FixtureResult,
}
impl Remote {
    async fn connect(&self, command: FixtureCommand) -> Result<(UnixStream, ControlId)> {
        let mut stream = UnixStream::connect(&self.endpoint.socket)
            .await
            .map_err(|_| Failure::Blocked)?;
        let peer = stream.peer_cred().map_err(|_| Failure::Unauthorized)?;
        if peer.uid() != rustix::process::geteuid().as_raw()
            || peer.pid().and_then(|n| u32::try_from(n).ok()) != Some(self.endpoint.pid)
        {
            return Err(Failure::Unauthorized);
        }
        let nonce = crate::random_id()?;
        write_json(
            &mut stream,
            &Envelope {
                version: VERSION,
                nonce: nonce.clone(),
                credential: self.endpoint.credential.clone(),
                command,
            },
        )
        .await?;
        Ok((stream, nonce))
    }
    async fn response(stream: &mut UnixStream, nonce: &ControlId) -> Result<FixtureResult> {
        let response: Response = read_json(stream).await?;
        if response.version != VERSION || &response.nonce != nonce {
            return Err(Failure::Unauthorized);
        }
        Ok(response.result)
    }
    pub async fn command(&self, command: FixtureCommand) -> Result<FixtureResult> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut stream, nonce) = self.connect(command).await?;
            Self::response(&mut stream, &nonce).await
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
}
struct HeldFixture {
    backend: BackendIdentity,
    operation: ControlId,
    _connection: std::sync::Mutex<UnixStream>,
}
impl BackendAuthority for HeldFixture {
    fn identity(&self) -> &BackendIdentity {
        &self.backend
    }
}
impl TerminationAuthority for HeldFixture {
    fn operation_id(&self) -> &ControlId {
        &self.operation
    }
}
#[async_trait::async_trait]
impl Backend for Remote {
    async fn verify_read_only(&self, expected: &BackendIdentity) -> Result<VerifiedBackend> {
        let (mut stream, nonce) = self
            .connect(FixtureCommand::Verify {
                expected: expected.clone(),
            })
            .await?;
        match Self::response(&mut stream, &nonce).await? {
            FixtureResult::Verified { backend } if &backend == expected => {
                VerifiedBackend::from_trusted_port(Arc::new(HeldFixture {
                    backend,
                    operation: nonce,
                    _connection: std::sync::Mutex::new(stream),
                }))
            }
            _ => Err(Failure::Blocked),
        }
    }
    async fn execute(
        &self,
        backend: BackendIdentity,
        request: RequestSpec,
        body: Vec<u8>,
        mut observations: ObservationSink,
        cancel: CancellationToken,
    ) -> Result<VerifiedTerminal> {
        if cancel.is_cancelled() {
            return Err(Failure::Blocked);
        }
        let (mut stream, nonce) = self
            .connect(FixtureCommand::Execute {
                backend: backend.clone(),
                request: request.clone(),
            })
            .await?;
        if cancel.is_cancelled() {
            return Err(Failure::Blocked);
        }
        stream
            .write_all(&body)
            .await
            .map_err(|_| Failure::Blocked)?;
        loop {
            match stream.read_u8().await.map_err(|_| Failure::Blocked)? {
                1 => {
                    let len = stream.read_u32().await.map_err(|_| Failure::Blocked)? as usize;
                    if len == 0 || len > FRAME_BYTES {
                        return Err(Failure::Limit);
                    }
                    let mut bytes = vec![0; len];
                    stream
                        .read_exact(&mut bytes)
                        .await
                        .map_err(|_| Failure::Blocked)?;
                    observations.send(bytes).await?;
                }
                2 => match Self::response(&mut stream, &nonce).await? {
                    FixtureResult::Terminal {
                        backend: actual,
                        request: actual_request,
                    } if actual == backend && actual_request == request => {
                        return Ok(VerifiedTerminal::from_trusted_port(actual, actual_request));
                    }
                    _ => return Err(Failure::Blocked),
                },
                _ => return Err(Failure::Invalid),
            }
        }
    }
}
#[async_trait::async_trait]
impl Supervisor for Remote {
    async fn stop(&self, operation: &StopOperation) -> Result<ManagerObservation> {
        match self
            .command(FixtureCommand::Stop {
                operation: operation.clone(),
            })
            .await?
        {
            FixtureResult::Observation { observation } => Ok(observation),
            _ => Err(Failure::Blocked),
        }
    }
    async fn terminated(&self, operation: &StopOperation) -> Result<Option<HeldTermination>> {
        let (mut stream, nonce) = self
            .connect(FixtureCommand::Terminated {
                operation: operation.clone(),
            })
            .await?;
        match Self::response(&mut stream, &nonce).await? {
            FixtureResult::Terminated { operation: actual } if &actual == operation => Ok(Some(
                HeldTermination::from_supervisor(Arc::new(HeldFixture {
                    backend: actual.backend,
                    operation: actual.operation_id,
                    _connection: std::sync::Mutex::new(stream),
                })),
            )),
            FixtureResult::Pending => Ok(None),
            _ => Err(Failure::Blocked),
        }
    }
}

struct OwnedState {
    children: Vec<Child>,
    mode: Mode,
    sends: Vec<RequestSpec>,
    stops: Vec<StopOperation>,
    reaped: bool,
    finished: bool,
}
/// The manager owns *both* child handles independently of guard/client life.
/// It never creates replacements. A remaining second child defeats termination.
pub async fn serve_owned(
    listener: UnixListener,
    identity: BackendIdentity,
    credential: Capability,
    executable: PathBuf,
    mode: Mode,
) -> Result<()> {
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            Command::new(&executable)
                .arg("runner")
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .map_err(|_| Failure::Blocked)?,
        );
    }
    let state = Arc::new(Mutex::new(OwnedState {
        children,
        mode,
        sends: Vec::new(),
        stops: Vec::new(),
        reaped: false,
        finished: false,
    }));
    let changed = Arc::new(Notify::new());
    let shutdown = CancellationToken::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|_| Failure::Blocked)?;
                let state = state.clone(); let changed = changed.clone(); let shutdown = shutdown.clone(); let credential = credential.clone(); let identity = identity.clone();
                tokio::spawn(async move { let _ = handle_owned(stream, identity, credential, state, changed, shutdown).await; });
            }
        }
    }
    cleanup(&mut *state.lock().await, true).await;
    Ok(())
}
async fn cleanup(state: &mut OwnedState, all: bool) {
    let count = if all { state.children.len() } else { 1 };
    for child in state.children.iter_mut().take(count) {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    state.reaped = all;
}
async fn handle_owned(
    mut stream: UnixStream,
    identity: BackendIdentity,
    credential: Capability,
    state: Arc<Mutex<OwnedState>>,
    changed: Arc<Notify>,
    shutdown: CancellationToken,
) -> Result<()> {
    if stream.peer_cred().map_err(|_| Failure::Unauthorized)?.uid()
        != rustix::process::geteuid().as_raw()
    {
        return Err(Failure::Unauthorized);
    }
    let envelope: Envelope = tokio::time::timeout(Duration::from_secs(2), read_json(&mut stream))
        .await
        .map_err(|_| Failure::Expired)??;
    use subtle::ConstantTimeEq;
    if envelope.version != VERSION
        || !bool::from(
            credential
                .expose()
                .as_bytes()
                .ct_eq(envelope.credential.expose().as_bytes()),
        )
    {
        return Err(Failure::Unauthorized);
    }
    let response = |result| Response {
        version: VERSION,
        nonce: envelope.nonce.clone(),
        result,
    };
    let mut hold = false;
    let result = match envelope.command {
        FixtureCommand::Verify { expected }
            if expected == identity && !state.lock().await.reaped =>
        {
            hold = true;
            FixtureResult::Verified {
                backend: identity.clone(),
            }
        }
        FixtureCommand::Execute { backend, request } if backend == identity => {
            request.validate().map_err(|_| Failure::Invalid)?;
            let mut body = vec![0; request.body_bytes as usize];
            stream
                .read_exact(&mut body)
                .await
                .map_err(|_| Failure::Invalid)?;
            if digest(&body) != request.body_sha256 {
                return Err(Failure::Conflict);
            }
            let mode = {
                let mut state = state.lock().await;
                if state.reaped || state.sends.len() >= 100 {
                    return Err(Failure::Blocked);
                }
                state.sends.push(request.clone());
                state.mode
            };
            changed.notify_waiters();
            let count = if mode == Mode::Flood { 16 } else { 1 };
            let bytes = if mode == Mode::Flood {
                vec![b'x'; FRAME_BYTES]
            } else {
                b"synthetic partial".to_vec()
            };
            for _ in 0..count {
                stream.write_u8(1).await.map_err(|_| Failure::Blocked)?;
                stream
                    .write_u32(bytes.len() as u32)
                    .await
                    .map_err(|_| Failure::Blocked)?;
                stream
                    .write_all(&bytes)
                    .await
                    .map_err(|_| Failure::Blocked)?;
            }
            if matches!(
                mode,
                Mode::Hang
                    | Mode::Flood
                    | Mode::Unavailable
                    | Mode::Descendant
                    | Mode::LostStopReply
            ) {
                std::future::pending::<()>().await;
            }
            if mode == Mode::HoldTerminal {
                loop {
                    let notified = changed.notified();
                    if state.lock().await.finished {
                        break;
                    }
                    notified.await;
                }
            }
            stream.write_u8(2).await.map_err(|_| Failure::Blocked)?;
            if mode == Mode::Error {
                FixtureResult::Failed
            } else {
                FixtureResult::Terminal { backend, request }
            }
        }
        FixtureCommand::Stop { operation } => {
            let mut state = state.lock().await;
            if operation.backend != identity {
                FixtureResult::Observation {
                    observation: ManagerObservation::IdentityMismatch {},
                }
            } else if state.mode == Mode::Unavailable {
                FixtureResult::Observation {
                    observation: ManagerObservation::Unavailable {},
                }
            } else {
                if let Some(old) = state.stops.first() {
                    if old != &operation {
                        return Err(Failure::Conflict);
                    }
                } else {
                    state.stops.push(operation.clone());
                }
                let all = state.mode != Mode::Descendant;
                cleanup(&mut state, all).await;
                if state.mode == Mode::LostStopReply {
                    return Err(Failure::Blocked);
                }
                FixtureResult::Observation {
                    observation: ManagerObservation::Submitted {
                        job_id: operation.operation_id,
                    },
                }
            }
        }
        FixtureCommand::Terminated { operation } => {
            let state = state.lock().await;
            if operation.backend == identity
                && state.reaped
                && state.stops.first() == Some(&operation)
            {
                hold = true;
                FixtureResult::Terminated { operation }
            } else {
                FixtureResult::Pending
            }
        }
        FixtureCommand::Stats => {
            let mut state = state.lock().await;
            let alive = state
                .children
                .iter_mut()
                .filter_map(|child| child.try_wait().ok())
                .filter(Option::is_none)
                .count() as u32;
            FixtureResult::Stats {
                sends: state.sends.clone(),
                stops: state.stops.clone(),
                alive,
            }
        }
        FixtureCommand::SetMode { mode } => {
            state.lock().await.mode = mode;
            FixtureResult::Ok
        }
        FixtureCommand::Finish => {
            state.lock().await.finished = true;
            changed.notify_waiters();
            FixtureResult::Ok
        }
        FixtureCommand::Cleanup => {
            cleanup(&mut *state.lock().await, true).await;
            FixtureResult::Ok
        }
        FixtureCommand::Shutdown => {
            shutdown.cancel();
            FixtureResult::Ok
        }
        _ => FixtureResult::Failed,
    };
    write_json(&mut stream, &response(result)).await?;
    if hold {
        // This authenticated, manager-owned connection retains the fixture's
        // authority. No wire reply can cause a different supervisor to mint it.
        let mut byte = [0];
        let _ = stream.read(&mut byte).await;
    }
    Ok(())
}
