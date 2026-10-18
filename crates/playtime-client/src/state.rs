//! Versioned CLI/UI recovery aids. The service ledger remains authoritative.
//! Synchronous private-file I/O belongs on a caller's blocking task.
use crate::{PendingLaunch, config::normalize_endpoint};
use playtime_core::RunId;
use playtime_local::{PrivateDirectory, lock_private, read_private, replace_private};
use playtime_protocol::api::{EventCursor, IdempotencyKey, LaunchRequest, ServiceId};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub enum StateError {
    Io,
    Invalid,
    Conflict,
    IdentityMismatch,
    EndpointChanged,
}
impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Io => "local receipt/cursor I/O failed",
            Self::Invalid => "invalid or unsupported client state",
            Self::Conflict => "key already has a different saved request or acknowledgment",
            Self::IdentityMismatch => "saved request belongs to a different service",
            Self::EndpointChanged => {
                "endpoint changed; explicitly select the new endpoint before retrying"
            }
        })
    }
}
impl std::error::Error for StateError {}
impl From<io::Error> for StateError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}
type Result<T> = std::result::Result<T, StateError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchReceipt {
    pub schema_version: u32,
    pub service_id: ServiceId,
    pub endpoint: String,
    pub key: IdempotencyKey,
    pub request: LaunchRequest,
    pub created_at_unix_ms: u64,
    pub run_id: Option<RunId>,
}
impl LaunchReceipt {
    pub fn pending(&self) -> PendingLaunch {
        PendingLaunch {
            schema_version: 1,
            service_id: self.service_id.clone(),
            key: self.key.clone(),
            request: self.request.clone(),
        }
    }
    pub fn check_target(&self, service: &ServiceId, endpoint: &str, explicit: bool) -> Result<()> {
        if *service != self.service_id {
            return Err(StateError::IdentityMismatch);
        }
        if normalize_endpoint(endpoint).map_err(|_| StateError::Invalid)? != self.endpoint
            && !explicit
        {
            return Err(StateError::EndpointChanged);
        }
        Ok(())
    }
    fn validate(&self, key: &IdempotencyKey) -> Result<()> {
        if self.schema_version != 1
            || self.key != *key
            || normalize_endpoint(&self.endpoint).map_err(|_| StateError::Invalid)? != self.endpoint
        {
            return Err(StateError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct ReceiptStore {
    directory: PathBuf,
}
impl ReceiptStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            directory: state_dir.join("launches"),
        }
    }
    pub fn path(&self, key: &IdempotencyKey) -> PathBuf {
        self.directory.join(format!("{key}.json"))
    }
    fn open(&self, create: bool) -> io::Result<PrivateDirectory> {
        if create {
            PrivateDirectory::open(self.directory.parent().unwrap(), true)?;
        }
        PrivateDirectory::open(&self.directory, create)
    }
    pub fn read(&self, key: &IdempotencyKey) -> Result<Option<LaunchReceipt>> {
        let directory = match self.open(false) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let _lock = directory.lock()?;
        read_receipt(&directory, key)
    }
    /// Exclusive publication, with durable bytes and parent sync before POST.
    /// Matching concurrent clients share the first timestamp/request/receipt.
    pub fn save(&self, pending: &PendingLaunch, endpoint: &str) -> Result<LaunchReceipt> {
        if pending.schema_version != 1 {
            return Err(StateError::Invalid);
        }
        let directory = self.open(true)?;
        let _lock = directory.lock()?;
        if let Some(existing) = read_receipt(&directory, &pending.key)? {
            if existing.pending() != *pending {
                return Err(StateError::Conflict);
            }
            return Ok(existing);
        }
        let receipt = LaunchReceipt {
            schema_version: 1,
            service_id: pending.service_id.clone(),
            endpoint: normalize_endpoint(endpoint).map_err(|_| StateError::Invalid)?,
            key: pending.key.clone(),
            request: pending.request.clone(),
            created_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| StateError::Invalid)?
                .as_millis()
                .try_into()
                .map_err(|_| StateError::Invalid)?,
            run_id: None,
        };
        directory.write_new(
            &format!("{}.json", pending.key),
            &serde_json::to_vec(&receipt).map_err(|_| StateError::Invalid)?,
        )?;
        Ok(receipt)
    }
    pub fn acknowledge(&self, saved: &LaunchReceipt, run: &RunId) -> Result<LaunchReceipt> {
        let directory = self.open(false)?;
        let _lock = directory.lock()?;
        let mut receipt = read_receipt(&directory, &saved.key)?.ok_or(StateError::Invalid)?;
        if receipt.pending() != saved.pending()
            || receipt.run_id.as_ref().is_some_and(|id| id != run)
        {
            return Err(StateError::Conflict);
        }
        receipt.run_id = Some(run.clone());
        directory.replace(
            &format!("{}.json", saved.key),
            &serde_json::to_vec(&receipt).map_err(|_| StateError::Invalid)?,
        )?;
        Ok(receipt)
    }
    pub fn list(&self, after: Option<&IdempotencyKey>, limit: usize) -> Result<ReceiptPage> {
        if !(1..=1000).contains(&limit) {
            return Err(StateError::Invalid);
        }
        let directory = match self.open(false) {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(ReceiptPage {
                    items: vec![],
                    next_cursor: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        let _lock = directory.lock()?;
        let mut items = Vec::new();
        let mut more = false;
        for name in directory.names()? {
            let key = IdempotencyKey::new(name.strip_suffix(".json").ok_or(StateError::Invalid)?)
                .map_err(|_| StateError::Invalid)?;
            if after.is_some_and(|after| key <= *after) {
                continue;
            }
            if items.len() == limit {
                more = true;
                break;
            }
            items.push(read_receipt(&directory, &key)?.ok_or(StateError::Invalid)?);
        }
        let next_cursor = more.then(|| items.last().unwrap().key.clone());
        Ok(ReceiptPage { items, next_cursor })
    }
}
#[derive(Debug, Serialize)]
pub struct ReceiptPage {
    pub items: Vec<LaunchReceipt>,
    pub next_cursor: Option<IdempotencyKey>,
}
fn read_receipt(
    directory: &PrivateDirectory,
    key: &IdempotencyKey,
) -> Result<Option<LaunchReceipt>> {
    match directory.read(&format!("{key}.json"), 16384) {
        Ok(bytes) => {
            let r: LaunchReceipt =
                serde_json::from_slice(&bytes).map_err(|_| StateError::Invalid)?;
            r.validate(key)?;
            Ok(Some(r))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CursorReceipt {
    pub schema_version: u32,
    pub service_id: ServiceId,
    pub run_id: RunId,
    pub cursor: String,
}
impl CursorReceipt {
    pub fn read(path: &Path, service: &ServiceId, run: &RunId) -> Result<Option<Self>> {
        let bytes = match read_private(path, 8192) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let receipt: Self = serde_json::from_slice(&bytes).map_err(|_| StateError::Invalid)?;
        receipt.check(service, run)?;
        Ok(Some(receipt))
    }
    fn check(&self, service: &ServiceId, run: &RunId) -> Result<()> {
        if self.service_id != *service {
            return Err(StateError::IdentityMismatch);
        }
        if self.schema_version != 1
            || self.run_id != *run
            || EventCursor::decode(&self.cursor)
                .map_err(|_| StateError::Invalid)?
                .frontier
                .run_id
                != *run
        {
            return Err(StateError::Invalid);
        }
        Ok(())
    }
    /// Invoke only after the caller's complete event frame is flushed. This
    /// records emission by a client, not downstream pipeline processing.
    pub fn write(&self, path: &Path) -> Result<()> {
        self.check(&self.service_id, &self.run_id)?;
        let mut lock_name = path.file_name().ok_or(StateError::Invalid)?.to_owned();
        lock_name.push(".lock");
        let _lock = lock_private(&path.with_file_name(lock_name))?;
        if let Some(previous) = Self::read(path, &self.service_id, &self.run_id)? {
            let old = EventCursor::decode(&previous.cursor).map_err(|_| StateError::Invalid)?;
            let new = EventCursor::decode(&self.cursor).map_err(|_| StateError::Invalid)?;
            if old.frontier.manifest_sha256 != new.frontier.manifest_sha256
                || old.after_sequence > new.after_sequence
            {
                return Err(StateError::Conflict);
            }
        }
        replace_private(
            path,
            &serde_json::to_vec(self).map_err(|_| StateError::Invalid)?,
        )?;
        Ok(())
    }
}
