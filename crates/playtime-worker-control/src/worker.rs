use crate::{
    Credential, Error, FileLock, Result, boot_id, boot_ms, digest, read_private, write_once,
};
use playtime_protocol::worker::{
    self, Binding, Command, Receipt, Reply, Request, Status, WorkerState,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub version: u32,
    pub binding: Binding,
    pub credential: Credential,
    pub root: PathBuf,
    pub private_dir: PathBuf,
    pub socket: PathBuf,
    pub startup_deadline_boot_ms: u64,
    pub wall_ms: u64,
}
impl Bootstrap {
    pub fn read(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&read_private(path, worker::FRAME_BYTES as u64)?)?;
        if value.version != worker::VERSION
            || value.binding.boot_id != boot_id()?
            || value.wall_ms == 0
        {
            return Err(Error::Invalid(
                "bootstrap version, boot or allocation mismatch",
            ));
        }
        let mut executable = Vec::new();
        File::open("/proc/self/exe")?
            .take(256 * 1024 * 1024 + 1)
            .read_to_end(&mut executable)?;
        if executable.len() > 256 * 1024 * 1024
            || digest(&executable) != value.binding.executable_sha256
        {
            return Err(Error::Invalid(
                "actual executing image does not match bootstrap",
            ));
        }
        Ok(value)
    }
}

/// Immutable, one-use permission files. All mutations serialize on grant.lock.
/// A replacement worker cannot reuse consumed authority, even with the same IDs.
pub struct Grant {
    directory: PathBuf,
    binding: Binding,
}
impl Grant {
    pub fn new(directory: PathBuf, binding: Binding) -> Self {
        Self { directory, binding }
    }
    fn lock(&self) -> Result<FileLock> {
        FileLock::acquire(&self.directory.join("grant.lock"))
    }
    fn matches(&self, name: &str) -> Result<bool> {
        let bytes = match read_private(&self.directory.join(name), worker::FRAME_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if serde_json::from_slice::<Binding>(&bytes)? != self.binding {
            return Err(Error::Invalid("grant binding mismatch"));
        }
        Ok(true)
    }
    fn record(&self, name: &str) -> Result<()> {
        write_once(
            &self.directory.join(name),
            &serde_json::to_vec(&self.binding)?,
        )
    }
    fn confirm(&self, name: &str) -> Result<()> {
        // A prior caller may have lost an acknowledgment. Reconfirm durability
        // before acknowledging the same immutable permission/revocation.
        crate::files::sync_private(&self.directory.join(name))
    }
    pub fn authorize(&self) -> Result<()> {
        let _lock = self.lock()?;
        if self.matches("revoked.json")? {
            return Err(Error::Invalid("launch grant revoked"));
        }
        if !self.matches("authorized.json")? {
            self.record("authorized.json")?;
        }
        self.confirm("authorized.json")
    }
    pub fn revoke(&self) -> Result<()> {
        let _lock = self.lock()?;
        if !self.matches("revoked.json")? {
            self.record("revoked.json")?;
        }
        self.confirm("revoked.json")
    }
    fn unused(&self) -> Result<()> {
        let _lock = self.lock()?;
        if self.matches("consumed.json")? || self.matches("revoked.json")? {
            return Err(Error::Invalid("launch grant already consumed or revoked"));
        }
        Ok(())
    }
    fn consume(&self) -> Result<()> {
        let _lock = self.lock()?;
        if !self.matches("authorized.json")? || self.matches("revoked.json")? {
            return Err(Error::Invalid("launch grant not authorized"));
        }
        self.record("consumed.json")
    }
}

/// Implementations retain the same prepared writer from construction through
/// Begin/finalization. Returned receipts, never discovered markers, are exposed.
pub trait PreparedExecution: Send + 'static {
    fn prepared_receipt(&self) -> Option<Receipt>;
    fn begin(&mut self, started_boot_ms: u64) -> Result<Receipt>;
    fn cancel(&mut self, before_begin: bool) -> Result<()>;
    /// Coalesced in-memory observations only. Must not wait on filesystem work.
    /// Default preserves existing synchronous fixture implementations.
    fn refresh_status(&self, _status: &mut Status) {}
}

pub struct Worker<E> {
    bootstrap: Bootstrap,
    grant: Grant,
    execution: E,
    status: Status,
    // Ownership is retained even when control publication fails.
    _run_lock: FileLock,
    notify: Option<crate::systemd::Notify>,
}
impl<E: PreparedExecution> Worker<E> {
    pub fn status(&self) -> &Status {
        &self.status
    }
    pub fn prepared(bootstrap: Bootstrap, execution: E, run_lock: FileLock) -> Result<Self> {
        let grant = Grant::new(bootstrap.private_dir.clone(), bootstrap.binding.clone());
        grant.unused()?;
        if boot_ms() >= bootstrap.startup_deadline_boot_ms {
            return Err(Error::Invalid("startup deadline expired"));
        }
        let status = Status {
            state: WorkerState::Prepared,
            work_started_boot_ms: None,
            wall_ms: bootstrap.wall_ms,
            latest_receipt: execution.prepared_receipt(),
            publication_pending: false,
            failure: None,
        };
        Ok(Self {
            bootstrap,
            grant,
            execution,
            status,
            _run_lock: run_lock,
            notify: None,
        })
    }
    /// Keep the same prepared owner; notification cannot precede initial commit.
    pub fn with_systemd(mut self, notify: crate::systemd::Notify) -> Self {
        self.notify = Some(notify);
        self
    }
    pub fn handle(&mut self, request: Request) -> Result<Reply> {
        if request.version != worker::VERSION || request.binding != self.bootstrap.binding {
            return Err(Error::Unauthenticated);
        }
        self.execution.refresh_status(&mut self.status);
        match request.command {
            Command::Status => (),
            Command::Begin if self.status.state == WorkerState::Prepared => {
                if boot_ms() >= self.bootstrap.startup_deadline_boot_ms {
                    return Err(Error::Invalid("startup deadline expired"));
                }
                // Set uncertainty before any potentially ambiguous persistence.
                self.status.state = WorkerState::Reconciling;
                self.grant.consume()?;
                let started = boot_ms();
                if started >= self.bootstrap.startup_deadline_boot_ms {
                    return Err(Error::Invalid(
                        "startup deadline expired during grant consumption",
                    ));
                }
                self.status.work_started_boot_ms = Some(started);
                self.status.publication_pending = true;
                let receipt = self.execution.begin(started)?;
                self.status.latest_receipt = Some(receipt);
                self.status.publication_pending = false;
                if let Some(notify) = &self.notify {
                    notify.ready()?;
                }
                self.status.state = WorkerState::Running;
            }
            Command::Begin => (), // Duplicate Begin never repeats consumption or work.
            Command::Cancel => {
                self.grant.revoke()?;
                let before = self.status.work_started_boot_ms.is_none();
                if !matches!(
                    self.status.state,
                    WorkerState::CancelledBeforeBegin | WorkerState::Cancelling
                ) {
                    self.execution.cancel(before)?;
                    self.status.state = if before {
                        WorkerState::CancelledBeforeBegin
                    } else {
                        WorkerState::Cancelling
                    };
                }
            }
        }
        self.execution.refresh_status(&mut self.status);
        Ok(Reply {
            version: worker::VERSION,
            nonce: request.nonce,
            binding: self.bootstrap.binding.clone(),
            status: self.status.clone(),
        })
    }
}
