//! Private Unix IPC. Control and inference have separate listeners, task quotas
//! and buffers. Neither the stream reader nor caller lifetime owns stop service.
use crate::{Dispatch, Guard, Result, ports::Clock, wire::*};
use playtime_local::PrivateDirectory;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

pub async fn read_json<R: AsyncRead + Unpin, T: DeserializeOwned>(stream: &mut R) -> Result<T> {
    let length = stream.read_u32().await.map_err(|_| Failure::Invalid)? as usize;
    if length == 0 || length > CONTROL_BYTES {
        return Err(Failure::Limit);
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| Failure::Invalid)?;
    serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)
}
pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|_| Failure::Invalid)?;
    if bytes.len() > CONTROL_BYTES {
        return Err(Failure::Limit);
    }
    stream
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| Failure::Invalid)?;
    stream.write_all(&bytes).await.map_err(|_| Failure::Invalid)
}
fn matches(expected: &Capability, supplied: &Capability) -> bool {
    expected
        .expose()
        .as_bytes()
        .ct_eq(supplied.expose().as_bytes())
        .into()
}
fn peer(stream: &UnixStream) -> Result<()> {
    if stream.peer_cred().map_err(|_| Failure::Unauthorized)?.uid()
        != rustix::process::geteuid().as_raw()
    {
        return Err(Failure::Unauthorized);
    }
    Ok(())
}
fn reply(nonce: playtime_protocol::worker::ControlId, result: Result<Status>) -> ControlReply {
    ControlReply {
        version: VERSION,
        nonce,
        outcome: match result {
            Ok(status) => Outcome::Status {
                status: Box::new(status),
            },
            Err(failure) => Outcome::Rejected { failure },
        },
    }
}
pub struct Server {
    control: UnixListener,
    inference: UnixListener,
}
impl Server {
    /// Endpoints must be new paths inside a private root. Removing stale sockets
    /// is a provisioning action under the journal lock, never a client fallback.
    pub fn bind(root: &Path) -> Result<Self> {
        PrivateDirectory::open(root, false).map_err(|_| Failure::Invalid)?;
        let control =
            UnixListener::bind(root.join("control.sock")).map_err(|_| Failure::Invalid)?;
        let inference =
            UnixListener::bind(root.join("inference.sock")).map_err(|_| Failure::Invalid)?;
        Ok(Self { control, inference })
    }
    pub async fn serve(
        self,
        guard: Guard,
        credential: Capability,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let control_quota = Arc::new(Semaphore::new(16));
        let inference_quota = Arc::new(Semaphore::new(2));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                accepted = self.control.accept() => {
                    let (stream, _) = accepted.map_err(|_| Failure::Blocked)?;
                    if let Ok(permit) = control_quota.clone().try_acquire_owned() {
                        let guard = guard.clone(); let credential = credential.clone();
                        tokio::spawn(async move { let _permit = permit; let _ = tokio::time::timeout(Duration::from_secs(2), control(stream, guard, credential)).await; });
                    }
                }
                accepted = self.inference.accept() => {
                    let (stream, _) = accepted.map_err(|_| Failure::Blocked)?;
                    if let Ok(permit) = inference_quota.clone().try_acquire_owned() {
                        let guard = guard.clone(); let credential = credential.clone(); let clock = clock.clone();
                        tokio::spawn(async move { let _permit = permit; let _ = inference(stream, guard, credential, clock).await; });
                    }
                }
            }
        }
    }
}
async fn authenticated(stream: &mut UnixStream, credential: &Capability) -> Result<ControlRequest> {
    peer(stream)?;
    let request: ControlRequest = read_json(stream).await?;
    if request.version != VERSION || !matches(credential, &request.credential) {
        return Err(Failure::Unauthorized);
    }
    Ok(request)
}
async fn control(mut stream: UnixStream, guard: Guard, credential: Capability) -> Result<()> {
    let request = authenticated(&mut stream, &credential).await?;
    let result = match request.command {
        Command::Query {} => Ok(guard.status()),
        Command::Acquire { lease, capability } => guard.acquire(lease, capability).await,
        Command::Revoke {
            lease_id,
            capability,
            operation_id,
        } => guard.revoke(lease_id, capability, operation_id).await,
        Command::Release {
            lease_id,
            capability,
            operation_id,
        } => guard.release(lease_id, capability, operation_id).await,
        Command::Dispatch { .. } => Err(Failure::Invalid),
    };
    write_json(&mut stream, &reply(request.nonce, result)).await
}
async fn inference(
    mut stream: UnixStream,
    guard: Guard,
    credential: Capability,
    clock: Arc<dyn Clock>,
) -> Result<()> {
    let request = tokio::time::timeout(
        Duration::from_secs(2),
        authenticated(&mut stream, &credential),
    )
    .await
    .map_err(|_| Failure::Expired)??;
    let Command::Dispatch {
        request: spec,
        capability,
    } = request.command
    else {
        return Err(Failure::Invalid);
    };
    spec.validate().map_err(|_| Failure::Invalid)?;
    let boot = guard.status().backend.boot_id;
    let stamp = clock.now();
    if stamp.boot_id != boot || stamp.ms >= spec.deadline_ms {
        return Err(Failure::Expired);
    }
    let mut body = vec![0; spec.body_bytes as usize];
    tokio::time::timeout(
        Duration::from_millis(spec.deadline_ms - stamp.ms).min(Duration::from_secs(2)),
        stream.read_exact(&mut body),
    )
    .await
    .map_err(|_| Failure::Expired)?
    .map_err(|_| Failure::Invalid)?;
    let dispatch = guard.dispatch(spec.clone(), capability, body).await;
    match dispatch {
        Err(e) => write_json(&mut stream, &reply(request.nonce, Err(e))).await,
        Ok(Dispatch::Existing(status)) => {
            write_json(&mut stream, &reply(request.nonce, Ok(*status))).await
        }
        Ok(Dispatch::Stream(mut owner)) => {
            let (mut reader, mut writer) = stream.split();
            let initial = reply(request.nonce.clone(), Ok(guard.status()));
            tokio::time::timeout(Duration::from_secs(1), write_json(&mut writer, &initial))
                .await
                .map_err(|_| Failure::Expired)??;
            let mut frames_open = true;
            loop {
                let now = clock.now();
                if now.boot_id != boot || now.ms >= spec.deadline_ms {
                    return Err(Failure::Expired);
                }
                let remaining =
                    Duration::from_millis(spec.deadline_ms - now.ms).min(Duration::from_millis(50));
                let mut extra = [0; 1];
                tokio::select! {
                    biased;
                    _ = reader.read(&mut extra) => return Err(Failure::Blocked),
                    frame = owner.frames.recv(), if frames_open => {
                        if let Some(frame) = frame {
                            // Keep the queue permit while the socket is blocked.
                            let write = async { writer.write_u8(1).await?; writer.write_u32(frame.bytes.len() as u32).await?; writer.write_all(&frame.bytes).await };
                            tokio::time::timeout(Duration::from_millis(spec.deadline_ms - now.ms), write).await.map_err(|_| Failure::Expired)?.map_err(|_| Failure::Blocked)?;
                        } else { frames_open = false; }
                    }
                    changed = owner.completion.changed() => {
                        if changed.is_err() { return Err(Failure::Blocked); }
                    }
                    _ = tokio::time::sleep(remaining) => {}
                }
                if !frames_open {
                    let final_result = owner.completion.borrow().clone();
                    if let Some(result) = final_result {
                        let final_reply = reply(request.nonce.clone(), result);
                        tokio::time::timeout(Duration::from_secs(1), async {
                            writer.write_u8(2).await.map_err(|_| Failure::Blocked)?;
                            write_json(&mut writer, &final_reply).await
                        })
                        .await
                        .map_err(|_| Failure::Expired)??;
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    root: PathBuf,
    credential: Capability,
}
impl Client {
    pub fn new(root: PathBuf, credential: Capability) -> Self {
        Self { root, credential }
    }
    pub async fn control(&self, command: Command) -> Result<Status> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = UnixStream::connect(self.root.join("control.sock"))
                .await
                .map_err(|_| Failure::Blocked)?;
            peer(&stream)?;
            let request = ControlRequest {
                version: VERSION,
                nonce: crate::random_id()?,
                credential: self.credential.clone(),
                command,
            };
            write_json(&mut stream, &request).await?;
            let response: ControlReply = read_json(&mut stream).await?;
            checked_reply(&request.nonce, response)
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
    /// Returns the authenticated socket to the caller, whose lifetime is the
    /// inference owner's lifetime. There is deliberately no reconnect/resend.
    pub async fn dispatch(
        &self,
        request: RequestSpec,
        capability: Capability,
        body: &[u8],
    ) -> Result<(UnixStream, Status)> {
        request.validate().map_err(|_| Failure::Invalid)?;
        if body.len() as u64 != request.body_bytes {
            return Err(Failure::Invalid);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = UnixStream::connect(self.root.join("inference.sock"))
                .await
                .map_err(|_| Failure::Blocked)?;
            peer(&stream)?;
            let envelope = ControlRequest {
                version: VERSION,
                nonce: crate::random_id()?,
                credential: self.credential.clone(),
                command: Command::Dispatch {
                    request,
                    capability,
                },
            };
            write_json(&mut stream, &envelope).await?;
            stream.write_all(body).await.map_err(|_| Failure::Blocked)?;
            let response: ControlReply = read_json(&mut stream).await?;
            Ok((stream, checked_reply(&envelope.nonce, response)?))
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
}
fn checked_reply(
    nonce: &playtime_protocol::worker::ControlId,
    response: ControlReply,
) -> Result<Status> {
    if response.version != VERSION || &response.nonce != nonce {
        return Err(Failure::Unauthorized);
    }
    match response.outcome {
        Outcome::Status { status } if status.version == VERSION => Ok(*status),
        Outcome::Rejected { failure } => Err(failure),
        _ => Err(Failure::Invalid),
    }
}
