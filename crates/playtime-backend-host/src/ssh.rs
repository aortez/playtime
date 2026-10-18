//! Fixed forced-command transport. No remote command, shell, arbitrary endpoint,
//! forwarding or inference retry. A reconnect can only query recorded state.
use crate::{Result, protocol, wire::*};
use playtime_backend_guard::transport::{read_json, write_json};
use playtime_protocol::worker::ControlId;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub const CONTROL: u8 = 1;
pub const INFERENCE: u8 = 2;
pub const PROFILE: u8 = 3;
pub struct Channel {
    child: Child,
    pub input: ChildStdin,
    pub output: ChildStdout,
}
impl Drop for Channel {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
pub struct Client {
    identity: PathBuf,
    known_hosts: PathBuf,
    credential: Capability,
    identity_sha256: playtime_core::Sha256Digest,
    hosts_sha256: playtime_core::Sha256Digest,
}
impl Client {
    pub fn new(identity: PathBuf, known_hosts: PathBuf, credential: Capability) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        for path in [&identity, &known_hosts] {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| Failure::Invalid)?;
            if !path.is_absolute()
                || !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o077 != 0
            {
                return Err(Failure::Invalid);
            }
        }
        let key = playtime_local::read_private(&identity, 65536).map_err(|_| Failure::Invalid)?;
        let hosts =
            playtime_local::read_private(&known_hosts, 65536).map_err(|_| Failure::Invalid)?;
        let text = std::str::from_utf8(&hosts).map_err(|_| Failure::Invalid)?;
        let fields: Vec<_> = text.split_whitespace().collect();
        if fields.len() != 3
            || fields[0] != "oldman-bird.local"
            || fields[1] != "ssh-ed25519"
            || fields[2].is_empty()
            || !fields[2]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b))
        {
            return Err(Failure::Invalid);
        }
        let executable = std::fs::metadata("/usr/bin/ssh").map_err(|_| Failure::Invalid)?;
        if !executable.is_file() || executable.uid() != 0 || executable.mode() & 0o022 != 0 {
            return Err(Failure::Invalid);
        }
        Ok(Self {
            identity,
            known_hosts,
            credential,
            identity_sha256: playtime_backend_guard::digest(&key),
            hosts_sha256: playtime_backend_guard::digest(&hosts),
        })
    }
    pub fn argv(&self) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = [
            "-F",
            "/dev/null",
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oIdentitiesOnly=yes",
            "-oIdentityAgent=none",
            "-oForwardAgent=no",
            "-oForwardX11=no",
            "-oClearAllForwardings=yes",
            "-oPermitLocalCommand=no",
            "-oRequestTTY=no",
            "-oControlMaster=no",
            "-oControlPath=none",
            "-oProxyCommand=none",
            "-oProxyJump=none",
            "-oConnectionAttempts=1",
            "-oConnectTimeout=3",
            "-oServerAliveInterval=1",
            "-oServerAliveCountMax=2",
            "-oGlobalKnownHostsFile=/dev/null",
            "-oHostKeyAlgorithms=ssh-ed25519",
            "-oUpdateHostKeys=no",
            "-oSendEnv=-*",
            "-i",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        args.push(self.identity.clone().into_os_string());
        args.push("-o".into());
        let mut known = std::ffi::OsString::from("UserKnownHostsFile=");
        known.push(&self.known_hosts);
        args.push(known);
        args.push("playtime-bridge@oldman-bird.local".into());
        args
    }
    async fn channel(&self, kind: u8) -> Result<Channel> {
        let paths = [self.identity.clone(), self.known_hosts.clone()];
        let expected = [self.identity_sha256.clone(), self.hosts_sha256.clone()];
        tokio::task::spawn_blocking(move || {
            for (path, hash) in paths.iter().zip(expected.iter()) {
                let bytes =
                    playtime_local::read_private(path, 65536).map_err(|_| Failure::Invalid)?;
                if playtime_backend_guard::digest(&bytes) != *hash {
                    return Err(Failure::Conflict);
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| Failure::Blocked)??;
        let mut child = Command::new("/usr/bin/ssh")
            .args(self.argv())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Failure::Blocked)?;
        let mut input = child.stdin.take().ok_or(Failure::Blocked)?;
        let output = child.stdout.take().ok_or(Failure::Blocked)?;
        input.write_u8(kind).await.map_err(|_| Failure::Blocked)?;
        Ok(Channel {
            child,
            input,
            output,
        })
    }
    pub async fn profile(
        &self,
        stamp: playtime_backend_guard::ports::Stamp,
        nonce: ControlId,
    ) -> Result<protocol::ProfileReply> {
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut channel = self.channel(PROFILE).await?;
            let request = protocol::Challenge {
                version: protocol::VERSION,
                nonce: nonce.clone(),
                credential: self.credential.clone(),
                local_boot: stamp.boot_id,
                local_send_ms: stamp.ms,
            };
            write_json(&mut channel.input, &request).await?;
            let reply: protocol::ProfileReply = read_json(&mut channel.output).await?;
            if reply.version != protocol::VERSION
                || reply.nonce != nonce
                || reply.local_boot != request.local_boot
                || reply.local_send_ms != stamp.ms
            {
                return Err(Failure::Unauthorized);
            }
            Ok(reply)
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
    pub async fn control(&self, command: crate::wire::Command) -> Result<Status> {
        if matches!(command, crate::wire::Command::Dispatch { .. }) {
            return Err(Failure::Invalid);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut channel = self.channel(CONTROL).await?;
            let nonce = playtime_backend_guard::random_id()?;
            write_json(
                &mut channel.input,
                &ControlRequest {
                    version: VERSION,
                    nonce: nonce.clone(),
                    credential: self.credential.clone(),
                    command,
                },
            )
            .await?;
            checked(&nonce, read_json(&mut channel.output).await?)
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
    pub async fn dispatch(
        &self,
        request: RequestSpec,
        capability: Capability,
        body: &[u8],
    ) -> Result<(Channel, ControlId, Status)> {
        request.validate().map_err(|_| Failure::Invalid)?;
        if body.len() as u64 != request.body_bytes
            || playtime_backend_guard::digest(body) != request.body_sha256
        {
            return Err(Failure::Conflict);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut channel = self.channel(INFERENCE).await?;
            let nonce = playtime_backend_guard::random_id()?;
            write_json(
                &mut channel.input,
                &ControlRequest {
                    version: VERSION,
                    nonce: nonce.clone(),
                    credential: self.credential.clone(),
                    command: crate::wire::Command::Dispatch {
                        request,
                        capability,
                    },
                },
            )
            .await?;
            channel
                .input
                .write_all(body)
                .await
                .map_err(|_| Failure::Blocked)?;
            let status = checked(&nonce, read_json(&mut channel.output).await?)?;
            Ok((channel, nonce, status))
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
}
pub fn checked(nonce: &ControlId, reply: ControlReply) -> Result<Status> {
    if reply.version != VERSION || &reply.nonce != nonce {
        return Err(Failure::Unauthorized);
    }
    match reply.outcome {
        Outcome::Status { status } if status.version == VERSION => Ok(*status),
        Outcome::Rejected { failure } => Err(failure),
        _ => Err(Failure::Invalid),
    }
}
/// Uses fixed sockets only; auth and framing still terminate inside the guard.
pub async fn bridge<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut input: R,
    mut output: W,
    root: &Path,
) -> Result<()> {
    let channel = tokio::time::timeout(Duration::from_secs(2), input.read_u8())
        .await
        .map_err(|_| Failure::Expired)?
        .map_err(|_| Failure::Invalid)?;
    let name = match channel {
        CONTROL => "control.sock",
        INFERENCE => "inference.sock",
        PROFILE => "profile.sock",
        _ => return Err(Failure::Invalid),
    };
    let mut socket = UnixStream::connect(root.join(name))
        .await
        .map_err(|_| Failure::Blocked)?;
    if socket.peer_cred().map_err(|_| Failure::Unauthorized)?.uid()
        != rustix::process::geteuid().as_raw()
    {
        return Err(Failure::Unauthorized);
    }
    let (mut reader, mut writer) = socket.split();
    // Whichever direction closes first closes the inference-owner socket too.
    // Fixed copy buffers cannot consume unbounded memory on blocked stdout.
    tokio::select! {
        result=tokio::io::copy(&mut input,&mut writer)=>result.map(|_|()).map_err(|_|Failure::Blocked),
        result=tokio::io::copy(&mut reader,&mut output)=>result.map(|_|()).map_err(|_|Failure::Blocked),
    }
}
