use crate::{Error, PreparedExecution, Result, Worker};
use playtime_protocol::worker::{self, Binding, Command, Reply, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Credential([u8; 32]);
impl Credential {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| Error::Invalid("OS randomness unavailable"))?;
        Ok(Self(bytes))
    }
    fn matches(&self, other: &Self) -> bool {
        bool::from(self.0.ct_eq(&other.0))
    }
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential([redacted])")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub socket: PathBuf,
    pub uid: u32,
    /// Obtained from the supervisor, never accepted from a Status body.
    pub pid: u32,
    pub binding: Binding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthenticatedRequest {
    credential: Credential,
    request: Request,
}

async fn read<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let size = stream.read_u32().await? as usize;
    if size == 0 || size > worker::FRAME_BYTES {
        return Err(Error::Invalid("invalid control frame length"));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
async fn write<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > worker::FRAME_BYTES {
        return Err(Error::Invalid("control frame exceeds limit"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    credential: Credential,
}
impl Client {
    pub fn new(endpoint: Endpoint, credential: Credential) -> Self {
        Self {
            endpoint,
            credential,
        }
    }
    pub async fn request(&self, command: Command) -> Result<Reply> {
        timeout(Duration::from_secs(worker::REQUEST_SECONDS), async {
            let mut stream = UnixStream::connect(&self.endpoint.socket).await?;
            let peer = stream.peer_cred()?;
            if peer.uid() != self.endpoint.uid
                || peer.pid().and_then(|pid| u32::try_from(pid).ok()) != Some(self.endpoint.pid)
            {
                return Err(Error::Unauthenticated);
            }
            let request = Request {
                version: worker::VERSION,
                nonce: crate::random_id()?,
                binding: self.endpoint.binding.clone(),
                command,
            };
            write(
                &mut stream,
                &AuthenticatedRequest {
                    credential: self.credential.clone(),
                    request: request.clone(),
                },
            )
            .await?;
            let reply: Reply = read(&mut stream).await?;
            if reply.version != worker::VERSION
                || reply.nonce != request.nonce
                || reply.binding != request.binding
            {
                return Err(Error::Unauthenticated);
            }
            Ok(reply)
        })
        .await
        .map_err(|_| Error::Deadline)?
    }
}

/// One request per connection; the caller bounds accepted connections. A timed
/// out blocking operation retains its worker/locks until it actually returns.
pub async fn serve_connection<E: PreparedExecution>(
    stream: UnixStream,
    worker: Arc<Mutex<Worker<E>>>,
    credential: &Credential,
    uid: u32,
) -> Result<()> {
    serve_connection_with_delivery(stream, worker, credential, uid, |_, _| true).await
}

/// Fault harnesses may lose a reply after the operation has completed. Delivery
/// never controls whether a grant was consumed or a receipt was returned.
pub async fn serve_connection_with_delivery<E: PreparedExecution>(
    mut stream: UnixStream,
    worker: Arc<Mutex<Worker<E>>>,
    credential: &Credential,
    uid: u32,
    deliver: impl FnOnce(Command, &Reply) -> bool,
) -> Result<()> {
    timeout(Duration::from_secs(worker::REQUEST_SECONDS), async {
        if stream.peer_cred()?.uid() != uid {
            return Err(Error::Unauthenticated);
        }
        let authenticated: AuthenticatedRequest = read(&mut stream).await?;
        if !credential.matches(&authenticated.credential) {
            return Err(Error::Unauthenticated);
        }
        let command = authenticated.request.command;
        let reply = tokio::task::spawn_blocking(move || {
            let mut owner = worker
                .try_lock()
                .map_err(|_| Error::Invalid("worker control operation pending"))?;
            owner.handle(authenticated.request)
        })
        .await
        .map_err(|_| Error::Invalid("worker control task failed"))??;
        if deliver(command, &reply) {
            write(&mut stream, &reply).await
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|_| Error::Deadline)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{boot_id, digest, random_id};
    use playtime_core::RunId;
    use playtime_protocol::worker::{Status, WorkerKind, WorkerState};

    #[tokio::test]
    async fn oversized_length_is_rejected_without_waiting_for_a_body() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender
            .write_u32(worker::FRAME_BYTES as u32 + 1)
            .await
            .unwrap();
        let result = timeout(Duration::from_secs(1), read::<Request>(&mut receiver))
            .await
            .unwrap();
        assert!(matches!(
            result,
            Err(Error::Invalid("invalid control frame length"))
        ));
    }

    #[tokio::test]
    async fn fresh_nonce_and_reply_binding_are_required_even_from_expected_peer() {
        let directory =
            std::env::temp_dir().join(format!("pt-control-{}", random_id().unwrap().as_str()));
        crate::create_private_dir(&directory).unwrap();
        let socket = directory.join("control");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let binding = Binding {
            service_id: random_id().unwrap(),
            run_id: RunId::new("fixture").unwrap(),
            manifest_sha256: digest(b"manifest"),
            executable_sha256: digest(b"worker"),
            grant_id: random_id().unwrap(),
            worker_id: random_id().unwrap(),
            invocation_id: random_id().unwrap(),
            boot_id: boot_id().unwrap(),
            unit: "fixture.service".into(),
            kind: WorkerKind::SupervisionFixture,
        };
        let endpoint = Endpoint {
            socket,
            uid: rustix::process::getuid().as_raw(),
            pid: std::process::id(),
            binding,
        };
        let client = Client::new(endpoint, Credential::generate().unwrap());
        let server = tokio::spawn(async move {
            let mut previous = None;
            for wrong_nonce in [true, false] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let received: AuthenticatedRequest = read(&mut stream).await.unwrap();
                assert_ne!(previous.as_ref(), Some(&received.request.nonce));
                previous = Some(received.request.nonce.clone());
                let mut reply = Reply {
                    version: worker::VERSION,
                    nonce: received.request.nonce,
                    binding: received.request.binding,
                    status: Status {
                        state: WorkerState::Prepared,
                        work_started_boot_ms: None,
                        wall_ms: 1000,
                        latest_receipt: None,
                        publication_pending: false,
                    },
                };
                if wrong_nonce {
                    reply.nonce = random_id().unwrap();
                } else {
                    reply.binding.invocation_id = random_id().unwrap();
                }
                write(&mut stream, &reply).await.unwrap();
            }
        });
        for _ in 0..2 {
            assert!(matches!(
                client.request(Command::Status).await,
                Err(Error::Unauthenticated)
            ));
        }
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn credentials_are_redacted_from_diagnostics() {
        assert_eq!(
            format!("{:?}", Credential::generate().unwrap()),
            "Credential([redacted])"
        );
    }
}
