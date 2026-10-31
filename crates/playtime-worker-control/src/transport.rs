use crate::{Error, PreparedExecution, Result, Worker};
use playtime_protocol::worker::{self, Binding, Command, Reply, Request};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{path::PathBuf, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Mutex,
    time::{Instant, timeout, timeout_at},
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    stream: UnixStream,
    worker: Arc<Mutex<Worker<E>>>,
    credential: &Credential,
    uid: u32,
    deliver: impl FnOnce(Command, &Reply) -> bool,
) -> Result<()> {
    serve_connection_until(
        stream,
        worker,
        credential,
        uid,
        deliver,
        Instant::now() + Duration::from_secs(worker::REQUEST_SECONDS),
    )
    .await
}

async fn serve_connection_until<E: PreparedExecution>(
    mut stream: UnixStream,
    worker: Arc<Mutex<Worker<E>>>,
    credential: &Credential,
    uid: u32,
    deliver: impl FnOnce(Command, &Reply) -> bool,
    deadline: Instant,
) -> Result<()> {
    timeout_at(deadline, async {
        if stream.peer_cred()?.uid() != uid {
            return Err(Error::Unauthenticated);
        }
        let authenticated: AuthenticatedRequest = read(&mut stream).await?;
        if !credential.matches(&authenticated.credential) {
            return Err(Error::Unauthenticated);
        }
        let command = authenticated.request.command;
        // Contention with Status or the timer is not a failed control request.
        // Waiting remains inside the request deadline and occupies no blocking
        // thread. Only the operation owning this guard may enter blocking I/O.
        let mut owner = worker.lock_owned().await;
        let reply = tokio::task::spawn_blocking(move || {
            // The blocking pool may have been busy after we acquired ownership.
            // A queued command must not start after its caller timed out.
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
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

    struct Execution;
    impl PreparedExecution for Execution {
        fn prepared_receipt(&self) -> Option<worker::Receipt> {
            Some(worker::Receipt {
                sequence: 0,
                bytes: 0,
                sha256: digest(&[]),
            })
        }
        fn begin(&mut self, _: u64) -> Result<worker::Receipt> {
            Ok(worker::Receipt {
                sequence: 1,
                bytes: 1,
                sha256: digest(b"x"),
            })
        }
        fn cancel(&mut self, _: bool) -> Result<()> {
            Ok(())
        }
    }
    struct ControlFixture {
        root: PathBuf,
        credential: Credential,
        binding: Binding,
        owner: Arc<Mutex<Worker<Execution>>>,
    }
    impl ControlFixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("pt-dispatch-{}", random_id().unwrap().as_str()));
            crate::create_private_dir(&root).unwrap();
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
            let credential = Credential::generate().unwrap();
            let bootstrap = crate::Bootstrap {
                version: worker::VERSION,
                binding: binding.clone(),
                credential: credential.clone(),
                root: root.clone(),
                private_dir: root.clone(),
                socket: root.join("control"),
                startup_deadline_boot_ms: crate::boot_ms() + 60_000,
                wall_ms: 30_000,
            };
            let lock = crate::FileLock::acquire(&root.join("run.lock")).unwrap();
            let owner = Arc::new(Mutex::new(
                Worker::prepared(bootstrap, Execution, lock).unwrap(),
            ));
            Self {
                root,
                credential,
                binding,
                owner,
            }
        }
        async fn cancel_connection(&self) -> (UnixStream, UnixStream) {
            let (mut client, server) = UnixStream::pair().unwrap();
            write(
                &mut client,
                &AuthenticatedRequest {
                    credential: self.credential.clone(),
                    request: Request {
                        version: worker::VERSION,
                        nonce: random_id().unwrap(),
                        binding: self.binding.clone(),
                        command: Command::Cancel,
                    },
                },
            )
            .await
            .unwrap();
            (client, server)
        }
        async fn assert_unexecuted(&self) {
            let owner = timeout(Duration::from_secs(1), self.owner.lock())
                .await
                .unwrap();
            assert_eq!(owner.status().state, WorkerState::Prepared);
            assert!(!self.root.join("revoked.json").exists());
        }
    }
    impl Drop for ControlFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[tokio::test]
    async fn brief_worker_contention_waits_then_acknowledges_cancel() {
        let fixture = ControlFixture::new();
        let held = fixture.owner.clone().lock_owned().await;
        let (mut client, server) = fixture.cancel_connection().await;
        let serving = serve_connection(
            server,
            fixture.owner.clone(),
            &fixture.credential,
            rustix::process::getuid().as_raw(),
        );
        tokio::pin!(serving);
        assert!(
            timeout(Duration::from_millis(50), &mut serving)
                .await
                .is_err(),
            "a busy worker must wait within the deadline, not reject Cancel"
        );
        drop(held);
        serving.await.unwrap();
        let reply: Reply = read(&mut client).await.unwrap();
        assert_eq!(reply.status.state, WorkerState::CancelledBeforeBegin);
        assert!(fixture.root.join("revoked.json").is_file());
    }

    #[tokio::test]
    async fn expired_request_waiting_for_worker_never_executes() {
        let fixture = ControlFixture::new();
        let held = fixture.owner.clone().lock_owned().await;
        let (_client, server) = fixture.cancel_connection().await;
        let result = serve_connection_until(
            server,
            fixture.owner.clone(),
            &fixture.credential,
            rustix::process::getuid().as_raw(),
            |_, _| true,
            Instant::now() + Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Err(Error::Deadline)));
        drop(held);
        fixture.assert_unexecuted().await;
    }

    #[test]
    fn expired_request_queued_in_blocking_pool_never_executes() {
        // A release guard keeps runtime shutdown bounded even if an assertion fails.
        struct Release(std::sync::mpsc::Sender<()>);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let fixture = ControlFixture::new();
                let (release, blocked) = std::sync::mpsc::channel();
                let release = Release(release);
                let entered = Arc::new(tokio::sync::Notify::new());
                let notification = entered.clone();
                let blocker = tokio::task::spawn_blocking(move || {
                    notification.notify_one();
                    blocked.recv().unwrap();
                });
                entered.notified().await;
                let (_client, server) = fixture.cancel_connection().await;
                let result = serve_connection_until(
                    server,
                    fixture.owner.clone(),
                    &fixture.credential,
                    rustix::process::getuid().as_raw(),
                    |_, _| true,
                    Instant::now() + Duration::from_millis(50),
                )
                .await;
                assert!(matches!(result, Err(Error::Deadline)));
                assert!(
                    fixture.owner.try_lock().is_err(),
                    "queued I/O retains ownership"
                );
                drop(release);
                blocker.await.unwrap();
                fixture.assert_unexecuted().await;
            });
    }

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
                        failure: None,
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
