use super::{
    Acceptance, Admission, AdmissionRequest, Error, Phase, Reason, Result, Selection, Stamp,
};
use playtime_core::RunId;
use playtime_protocol::worker::ControlId;
use playtime_worker_control::{
    FileLock, create_private_dir, digest, random_id, read_private, write_once,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

struct Inner {
    connection: Mutex<Connection>,
    capacity: Arc<Semaphore>,
    _service_lock: FileLock,
}
#[derive(Clone)]
pub struct Ledger {
    inner: Arc<Inner>,
    service_id: ControlId,
    root: PathBuf,
}

impl Ledger {
    pub async fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        tokio::task::spawn_blocking(move || Self::open_blocking(root))
            .await
            .map_err(|_| Error::Invalid("ledger task failed"))?
    }
    fn open_blocking(root: PathBuf) -> Result<Self> {
        let private = root.join(".playtime");
        create_private_dir(&private)?;
        let service_lock = FileLock::acquire(&private.join("service.lock"))?;
        let path = private.join("service.sqlite3");
        let identity_path = private.join("service.identity");
        let fresh = match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if !meta.is_file() || meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
                    return Err(Error::Invalid("ledger file is not private and regular"));
                }
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if identity_path.try_exists()?
                    || private.join("service.sqlite3-wal").try_exists()?
                    || private.join("service.sqlite3-shm").try_exists()?
                {
                    return Err(Error::Invalid(
                        "missing ledger with retained service evidence; refusing to replace history",
                    ));
                }
                write_once(&path, &[])?;
                true
            }
            Err(e) => return Err(e.into()),
        };
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_secs(1))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
        )?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        let sync: u32 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        let foreign_keys: bool =
            connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        if journal != "wal" || sync != 2 || !foreign_keys {
            return Err(Error::Invalid(
                "required ledger durability settings unavailable",
            ));
        }
        if fresh {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE service (singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL);
                CREATE TABLE runs (id TEXT PRIMARY KEY, key TEXT NOT NULL UNIQUE, active INTEGER UNIQUE CHECK(active=1 OR active IS NULL), record TEXT NOT NULL);
                CREATE TABLE trace (seq INTEGER PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), transition TEXT NOT NULL);
                PRAGMA user_version=1;")?;
            tx.execute(
                "INSERT INTO service VALUES (1, ?1)",
                [random_id()?.as_str()],
            )?;
            tx.commit()?;
            File::open(&private)?.sync_all()?;
        } else if version != 1 {
            return Err(Error::Invalid(
                "unsupported or incomplete service ledger; refusing to replace admission history",
            ));
        }
        let check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if check != "ok" {
            return Err(Error::Invalid("damaged service ledger"));
        }
        let id: String =
            connection.query_row("SELECT id FROM service WHERE singleton=1", [], |row| {
                row.get(0)
            })?;
        let service_id =
            ControlId::new(id).map_err(|_| Error::Invalid("damaged service identity"))?;
        if fresh {
            write_once(&identity_path, service_id.as_str().as_bytes())?;
        }
        if read_private(&identity_path, 32)? != service_id.as_str().as_bytes() {
            return Err(Error::Invalid(
                "ledger and retained service identity disagree",
            ));
        }
        // Saved status remains evidence only; a new coordinator must authenticate
        // the same surviving invocation before asserting current liveness.
        let active: Option<String> = connection
            .query_row("SELECT record FROM runs WHERE active=1", [], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(json) = active {
            let mut admission: Admission = serde_json::from_str(&json)?;
            if admission.service_id != service_id {
                return Err(Error::Invalid("ledger service identity conflict"));
            }
            admission.phase = Phase::Reconciling;
            admission.reason = Some(Reason::CoordinatorRestarted);
            save(&mut connection, &admission, "coordinator_restarted")?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(connection),
                capacity: Arc::new(Semaphore::new(16)),
                _service_lock: service_lock,
            }),
            service_id,
            root,
        })
    }
    pub fn service_id(&self) -> &ControlId {
        &self.service_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// At most 16 admitted blocking operations; cancelled callers cannot drop a
    /// transaction or release the service lock underneath a pending commit.
    async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .inner
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut connection = inner
                .connection
                .lock()
                .map_err(|_| Error::Invalid("ledger owner failed"))?;
            operation(&mut connection)
        })
        .await
        .map_err(|_| Error::Invalid("ledger task failed"))?
    }
    pub async fn lookup(&self, key: &str) -> Result<Option<Admission>> {
        let key = key.to_owned();
        self.call(move |db| {
            let json: Option<String> = db
                .query_row("SELECT record FROM runs WHERE key=?1", [key], |row| {
                    row.get(0)
                })
                .optional()?;
            json.map(|value| serde_json::from_str(&value).map_err(Error::from))
                .transpose()
        })
        .await
    }
    pub async fn get(&self, run_id: &RunId) -> Result<Admission> {
        let id = run_id.clone();
        self.call(move |db| load(db, &id)).await
    }
    pub(crate) async fn reserve(
        &self,
        key: String,
        request: AdmissionRequest,
        selection: Selection,
        now: Stamp,
    ) -> Result<Acceptance> {
        let service_id = self.service_id.clone();
        self.call(move |db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let old: Option<String> = tx
                .query_row("SELECT record FROM runs WHERE key=?1", [&key], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(old) = old {
                let admission: Admission = serde_json::from_str(&old)?;
                if admission.request != request {
                    return Err(Error::Conflict);
                }
                return Ok(Acceptance {
                    replayed: true,
                    admission,
                });
            }
            let busy: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE active=1)",
                [],
                |row| row.get(0),
            )?;
            if busy {
                return Err(Error::Busy);
            }
            let utc = now.utc.as_str();
            // Match the pinned Python timestamp-eight-hex ID convention.
            let timestamp = utc[..19].replace(['-', ':'], "");
            let run_id = RunId::new(format!("{timestamp}Z-{}", &random_id()?.as_str()[..8]))
                .map_err(|_| Error::Invalid("invalid run ID"))?;
            let request_sha256 = digest(&serde_json::to_vec(&request)?);
            let admission = Admission {
                service_id,
                run_id,
                key,
                request,
                request_sha256,
                selection,
                accepted: now,
                phase: Phase::Preparing,
                reason: None,
                prepared: None,
                start_attempted: false,
                endpoint: None,
                begin_authorized: false,
                cancel_requested: None,
                last_status: None,
            };
            tx.execute(
                "INSERT INTO runs VALUES (?1, ?2, 1, ?3)",
                params![
                    admission.run_id.as_str(),
                    admission.key,
                    serde_json::to_string(&admission)?
                ],
            )?;
            tx.execute(
                "INSERT INTO trace(run_id,transition) VALUES (?1,'reserved')",
                [admission.run_id.as_str()],
            )?;
            tx.commit()?;
            Ok(Acceptance {
                replayed: false,
                admission,
            })
        })
        .await
    }
    pub(crate) async fn update(
        &self,
        id: &RunId,
        transition: &'static str,
        change: impl FnOnce(&mut Admission) -> Result<()> + Send + 'static,
    ) -> Result<Admission> {
        let id = id.clone();
        self.call(move |db| {
            let mut admission = load(db, &id)?;
            change(&mut admission)?;
            save(db, &admission, transition)?;
            Ok(admission)
        })
        .await
    }
    pub async fn trace(&self, id: &RunId) -> Result<Vec<String>> {
        let id = id.clone();
        self.call(move |db| {
            let mut statement =
                db.prepare("SELECT transition FROM trace WHERE run_id=?1 ORDER BY seq LIMIT 256")?;
            Ok(statement
                .query_map([id.as_str()], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }
}
fn load(db: &Connection, id: &RunId) -> Result<Admission> {
    let json: String = db.query_row(
        "SELECT record FROM runs WHERE id=?1",
        [id.as_str()],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&json)?)
}
fn save(db: &mut Connection, admission: &Admission, transition: &str) -> Result<()> {
    let json = serde_json::to_string(admission)?;
    if json.len() > 65_536 {
        return Err(Error::Invalid("ledger record exceeds bound"));
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE runs SET record=?2 WHERE id=?1",
        params![admission.run_id.as_str(), json],
    )?;
    tx.execute(
        "INSERT INTO trace(run_id,transition) VALUES (?1,?2)",
        params![admission.run_id.as_str(), transition],
    )?;
    tx.commit()?;
    Ok(())
}
