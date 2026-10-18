//! Worker half of the Linux user-service contract. These private operational
//! records are supervisor diagnostics, never archive receipts or recovery power.
use crate::{Bootstrap, Error, Result, boot_ms, read_private, write_once};
use playtime_protocol::worker::{Binding, ControlId};
use serde::{Deserialize, Serialize};
use std::{
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
    path::Path,
    time::Duration,
};

pub const IDENTITY_FILE: &str = "systemd-invocation.json";
pub const EXIT_FILE: &str = "systemd-exit.json";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub binding: Binding,
    /// Assigned by systemd, distinct from the preallocated launch identity.
    pub manager_id: ControlId,
    pub pid: u32,
    pub start_ticks: u64,
    pub cgroup: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exit {
    pub binding: Binding,
    pub manager_id: ControlId,
    pub cgroup: String,
    pub observed_boot_ms: u64,
    pub result: String,
    pub code: Option<String>,
    pub status: Option<String>,
}

pub fn process_start(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|n| n.parse().ok())
        .ok_or(Error::Invalid("invalid process identity"))
}
pub fn process_cgroup(pid: u32) -> Result<String> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    contents
        .lines()
        .find_map(|s| s.strip_prefix("0::"))
        .filter(|s| s.starts_with('/') && *s != "/")
        .map(str::to_owned)
        .ok_or(Error::Invalid("unified cgroup ownership unavailable"))
}
fn manager_id() -> Result<ControlId> {
    std::env::var("INVOCATION_ID")
        .ok()
        .and_then(|s| ControlId::new(s).ok())
        .ok_or(Error::Invalid("systemd invocation identity unavailable"))
}

/// Construct only in a systemd main process, after verifying the bootstrap.
/// No READY is sent until Worker has consumed its grant and committed Begin.
/// This API deliberately cannot extend any systemd deadline.
pub struct Notify {
    socket: UnixDatagram,
}
impl Notify {
    pub fn attach(bootstrap: &Bootstrap) -> Result<Self> {
        let path = std::env::var_os("NOTIFY_SOCKET")
            .ok_or(Error::Invalid("systemd notification socket unavailable"))?;
        let bytes = path.as_bytes();
        let address = if let Some(abstract_name) = bytes.strip_prefix(b"@") {
            SocketAddr::from_abstract_name(abstract_name)?
        } else if bytes.starts_with(b"/") {
            SocketAddr::from_pathname(&path)?
        } else {
            return Err(Error::Invalid("invalid systemd notification socket"));
        };
        let socket = UnixDatagram::unbound()?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        socket.connect_addr(&address)?;
        let pid = std::process::id();
        let identity = Invocation {
            binding: bootstrap.binding.clone(),
            manager_id: manager_id()?,
            pid,
            start_ticks: process_start(pid)?,
            cgroup: process_cgroup(pid)?,
        };
        write_once(
            &bootstrap.private_dir.join(IDENTITY_FILE),
            &serde_json::to_vec(&identity)?,
        )?;
        Ok(Self { socket })
    }
    pub(crate) fn ready(&self) -> Result<()> {
        self.socket.send(b"READY=1")?;
        Ok(())
    }
}

/// Call at the worker entry point before acquiring run/writer locks. systemd's
/// ExecStopPost invokes the same frozen executable and argv with SERVICE_RESULT.
/// A missing/failed record leaves ownership uncertain; it cannot free a slot.
pub fn record_exit_if_requested(bootstrap_path: &Path) -> Result<bool> {
    let Ok(result) = std::env::var("SERVICE_RESULT") else {
        return Ok(false);
    };
    let bootstrap = Bootstrap::read(bootstrap_path)?;
    let code = std::env::var("EXIT_CODE").ok();
    let status = std::env::var("EXIT_STATUS").ok();
    if [&result]
        .into_iter()
        .chain(code.iter())
        .chain(status.iter())
        .any(|s| s.len() > 128 || s.contains('\n'))
    {
        return Err(Error::Invalid("invalid systemd exit diagnostic"));
    }
    let record = Exit {
        binding: bootstrap.binding,
        manager_id: manager_id()?,
        cgroup: process_cgroup(std::process::id())?,
        observed_boot_ms: boot_ms(),
        result,
        code,
        status,
    };
    write_once(
        &bootstrap.private_dir.join(EXIT_FILE),
        &serde_json::to_vec(&record)?,
    )?;
    Ok(true)
}

pub fn read_invocation(directory: &Path) -> Result<Invocation> {
    Ok(serde_json::from_slice(&read_private(
        &directory.join(IDENTITY_FILE),
        65_536,
    )?)?)
}
pub fn read_exit(directory: &Path) -> Result<Option<Exit>> {
    match read_private(&directory.join(EXIT_FILE), 65_536) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Credential, FileLock, Grant, PreparedExecution, Worker, boot_id, create_private_dir,
        digest, random_id,
    };
    use playtime_protocol::worker::{self, Command, Receipt, Request, WorkerKind, WorkerState};

    struct Execution {
        directory: std::path::PathBuf,
        fail: bool,
    }
    impl PreparedExecution for Execution {
        fn prepared_receipt(&self) -> Option<Receipt> {
            Some(Receipt {
                sequence: 0,
                bytes: 0,
                sha256: digest(b""),
            })
        }
        fn begin(&mut self, _: u64) -> Result<Receipt> {
            // Initial publication is permitted only after durable consumption.
            assert!(self.directory.join("consumed.json").exists());
            if self.fail {
                return Err(Error::Invalid("injected initial publication failure"));
            }
            write_once(&self.directory.join("initial"), b"committed")?;
            Ok(Receipt {
                sequence: 1,
                bytes: 9,
                sha256: digest(b"committed"),
            })
        }
        fn cancel(&mut self, _: bool) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn ready_follows_consumption_and_initial_commit_and_is_never_repeated() {
        for fail in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("pt-notify-{}", random_id().unwrap().as_str()));
            create_private_dir(&directory).unwrap();
            let binding = Binding {
                service_id: random_id().unwrap(),
                run_id: playtime_core::RunId::new("notify-fixture").unwrap(),
                manifest_sha256: digest(b"manifest"),
                executable_sha256: digest(b"worker"),
                grant_id: random_id().unwrap(),
                worker_id: random_id().unwrap(),
                invocation_id: random_id().unwrap(),
                boot_id: boot_id().unwrap(),
                unit: "notify-fixture.service".into(),
                kind: WorkerKind::SupervisionFixture,
            };
            let bootstrap = Bootstrap {
                version: worker::VERSION,
                binding: binding.clone(),
                credential: Credential::generate().unwrap(),
                root: directory.clone(),
                private_dir: directory.clone(),
                socket: directory.join("control"),
                startup_deadline_boot_ms: boot_ms() + 60_000,
                wall_ms: 30_000,
            };
            let (sender, receiver) = UnixDatagram::pair().unwrap();
            receiver.set_nonblocking(true).unwrap();
            let lock = FileLock::acquire(&directory.join("run.lock")).unwrap();
            let mut owner = Worker::prepared(
                bootstrap,
                Execution {
                    directory: directory.clone(),
                    fail,
                },
                lock,
            )
            .unwrap()
            .with_systemd(Notify { socket: sender });
            let mut buffer = [0; 64];
            assert!(receiver.recv(&mut buffer).is_err());
            Grant::new(directory.clone(), binding.clone())
                .authorize()
                .unwrap();
            let request = Request {
                version: worker::VERSION,
                binding,
                nonce: random_id().unwrap(),
                command: Command::Begin,
            };
            let result = owner.handle(request.clone());
            if fail {
                assert!(result.is_err());
                assert_eq!(owner.status().state, WorkerState::Reconciling);
                assert!(receiver.recv(&mut buffer).is_err());
            } else {
                assert_eq!(result.unwrap().status.state, WorkerState::Running);
                assert!(directory.join("initial").exists());
                let size = receiver.recv(&mut buffer).unwrap();
                assert_eq!(&buffer[..size], b"READY=1");
            }
            owner.handle(request).unwrap();
            assert!(receiver.recv(&mut buffer).is_err());
            drop(owner);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}
