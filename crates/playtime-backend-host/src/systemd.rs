//! Fixed-unit manager authority. Unit operations are serialized by the host
//! deadline owner, which holds the exclusion inode and original cgroup handles.
use crate::{Result, wire::*};
use playtime_backend_guard::ports::{Clock, SystemClock};
use playtime_protocol::worker::ControlId;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue},
};

const DEST: &str = "org.freedesktop.systemd1";
const MANAGER: &str = "/org/freedesktop/systemd1";
const IFACE: &str = "org.freedesktop.systemd1.Manager";
pub struct Manager {
    connection: Connection,
    unit: String,
    boot: ControlId,
    cgroup: File,
    events: File,
    kill: File,
    inode: u64,
    pub identity: BackendIdentity,
    stop: Option<StopOperation>,
    job: Option<String>,
    submitted: bool,
    saw_empty: bool,
    require_listener: bool,
}
#[derive(Debug)]
pub struct Snapshot {
    pub invocation: ControlId,
    pub cgroup: String,
    pub state: String,
    pub restart: String,
    pub kill_mode: String,
    pub stop_usec: u64,
    pub job: Option<String>,
}
async fn snapshot(connection: &Connection, unit: &str) -> Result<Snapshot> {
    let proxy = Proxy::new(connection, DEST, MANAGER, IFACE)
        .await
        .map_err(|_| Failure::Blocked)?;
    let path: OwnedObjectPath = proxy
        .call("GetUnit", &(unit,))
        .await
        .map_err(|_| Failure::Blocked)?;
    let props = Proxy::new(connection, DEST, path, "org.freedesktop.DBus.Properties")
        .await
        .map_err(|_| Failure::Blocked)?;
    let u: HashMap<String, OwnedValue> = props
        .call("GetAll", &("org.freedesktop.systemd1.Unit",))
        .await
        .map_err(|_| Failure::Blocked)?;
    let s: HashMap<String, OwnedValue> = props
        .call("GetAll", &("org.freedesktop.systemd1.Service",))
        .await
        .map_err(|_| Failure::Blocked)?;
    let text = |map: &HashMap<String, OwnedValue>, name| -> Result<String> {
        <&str>::try_from(map.get(name).ok_or(Failure::Blocked)?)
            .map(str::to_owned)
            .map_err(|_| Failure::Blocked)
    };
    let invocation: Vec<u8> = u
        .get("InvocationID")
        .ok_or(Failure::Blocked)?
        .try_clone()
        .map_err(|_| Failure::Blocked)?
        .try_into()
        .map_err(|_| Failure::Blocked)?;
    if invocation.len() != 16 {
        return Err(Failure::Blocked);
    }
    let job: (u32, OwnedObjectPath) = u
        .get("Job")
        .ok_or(Failure::Blocked)?
        .try_clone()
        .map_err(|_| Failure::Blocked)?
        .try_into()
        .map_err(|_| Failure::Blocked)?;
    Ok(Snapshot {
        invocation: ControlId::new(
            invocation
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        )
        .map_err(|_| Failure::Blocked)?,
        cgroup: text(&s, "ControlGroup")?,
        state: text(&u, "ActiveState")?,
        restart: text(&s, "Restart")?,
        kill_mode: text(&s, "KillMode")?,
        stop_usec: u64::try_from(s.get("TimeoutStopUSec").ok_or(Failure::Blocked)?)
            .map_err(|_| Failure::Blocked)?,
        job: (job.0 != 0).then(|| job.1.to_string()),
    })
}
impl Manager {
    /// The production caller supplies only its administrator-owned manifest.
    /// Session-bus selection is reserved for the explicitly invoked fixture.
    pub async fn capture(
        unit: &str,
        backend_id: ControlId,
        guard_id: ControlId,
        generation: u64,
        profile: playtime_core::Sha256Digest,
        session_fixture: bool,
    ) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(1), async {
            let connection = if session_fixture {
                Connection::session().await
            } else {
                Connection::system().await
            }
            .map_err(|_| Failure::Blocked)?;
            let proxy = Proxy::new(&connection, DEST, MANAGER, IFACE)
                .await
                .map_err(|_| Failure::Blocked)?;
            let version: String = proxy
                .get_property("Version")
                .await
                .map_err(|_| Failure::Blocked)?;
            if version
                .trim_start_matches('v')
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .is_none_or(|v| v < 255)
            {
                return Err(Failure::Blocked);
            }
            let snapshot = snapshot(&connection, unit).await?;
            if snapshot.state != "active"
                || snapshot.restart != "no"
                || snapshot.kill_mode != "control-group"
                || snapshot.stop_usec > 2_000_000
            {
                return Err(Failure::Blocked);
            }
            let boot = SystemClock::new()?.now().boot_id;
            let identity = BackendIdentity {
                backend_id,
                guard_id,
                boot_id: boot.clone(),
                unit: unit.into(),
                invocation_id: snapshot.invocation,
                cgroup: snapshot.cgroup.clone(),
                generation,
                profile_sha256: profile,
            };
            identity.validate().map_err(|_| Failure::Invalid)?;
            let fd = rustix::fs::open(
                Path::new("/sys/fs/cgroup").join(snapshot.cgroup.trim_start_matches('/')),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|_| Failure::Blocked)?;
            let cgroup = File::from(fd);
            let open = |name, flags| {
                rustix::fs::openat(
                    &cgroup,
                    name,
                    flags
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map(File::from)
                .map_err(|_| Failure::Blocked)
            };
            let events = open("cgroup.events", rustix::fs::OFlags::RDONLY)?;
            let kill = open("cgroup.kill", rustix::fs::OFlags::WRONLY)?;
            let inode = cgroup.metadata().map_err(|_| Failure::Blocked)?.ino();
            // Keep the original manager object referenced across collection.
            let _: () = proxy
                .call("RefUnit", &(unit,))
                .await
                .map_err(|_| Failure::Blocked)?;
            let manager = Self {
                connection,
                unit: unit.into(),
                boot,
                cgroup,
                events,
                kill,
                inode,
                identity,
                stop: None,
                job: None,
                submitted: false,
                saw_empty: false,
                require_listener: !session_fixture,
            };
            manager.verify().await?;
            Ok(manager)
        })
        .await
        .map_err(|_| Failure::Expired)?
    }
    pub async fn verify(&self) -> Result<()> {
        let fresh = snapshot(&self.connection, &self.unit).await?;
        if SystemClock::new()?.now().boot_id != self.boot
            || fresh.invocation != self.identity.invocation_id
            || fresh.restart != "no"
            || fresh.kill_mode != "control-group"
            || fresh.stop_usec > 2_000_000
            || (!fresh.cgroup.is_empty() && fresh.cgroup != self.identity.cgroup)
            || self.cgroup.metadata().map_err(|_| Failure::Blocked)?.ino() != self.inode
        {
            return Err(Failure::Conflict);
        }
        Ok(())
    }
    pub async fn live(&self) -> Result<()> {
        self.verify().await?;
        if snapshot(&self.connection, &self.unit).await?.state != "active" || self.stop.is_some() {
            return Err(Failure::Blocked);
        }
        if self.require_listener {
            self.verify_listener(11434).await?;
        }
        Ok(())
    }
    /// Bind the loopback listener to the authenticated unit process tree.
    /// Stop deliberately never depends on these read-only readiness files.
    pub async fn verify_listener(&self, port: u16) -> Result<()> {
        use std::collections::HashSet;
        let mut bytes = String::new();
        File::open("/proc/net/tcp")
            .map_err(|_| Failure::Blocked)?
            .take(1024 * 1024 + 1)
            .read_to_string(&mut bytes)
            .map_err(|_| Failure::Blocked)?;
        if bytes.len() > 1024 * 1024 {
            return Err(Failure::Limit);
        }
        let address = format!("0100007F:{port:04X}");
        let inodes: HashSet<_> = bytes
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.len() > 9 && fields[1] == address && fields[3] == "0A")
                    .then(|| fields[9].to_owned())
            })
            .collect();
        if inodes.len() != 1 {
            return Err(Failure::Blocked);
        }
        let proxy = Proxy::new(&self.connection, DEST, MANAGER, IFACE)
            .await
            .map_err(|_| Failure::Blocked)?;
        let processes: Vec<(String, u32, String)> = proxy
            .call("GetUnitProcesses", &(&self.unit,))
            .await
            .map_err(|_| Failure::Blocked)?;
        if processes.len() > 128 {
            return Err(Failure::Limit);
        }
        for (_, pid, _) in processes {
            let directory =
                std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(|_| Failure::Blocked)?;
            for (count, entry) in directory.enumerate() {
                if count >= 1024 {
                    return Err(Failure::Limit);
                }
                let entry = entry.map_err(|_| Failure::Blocked)?;
                if let Ok(target) = std::fs::read_link(entry.path())
                    && inodes.iter().any(|inode| {
                        target.as_os_str() == std::ffi::OsStr::new(&format!("socket:[{inode}]"))
                    })
                {
                    self.verify().await?;
                    return Ok(());
                }
            }
        }
        Err(Failure::Blocked)
    }
    pub async fn process_count(&self) -> Result<usize> {
        let proxy = Proxy::new(&self.connection, DEST, MANAGER, IFACE)
            .await
            .map_err(|_| Failure::Blocked)?;
        let processes: Vec<(String, u32, String)> = proxy
            .call("GetUnitProcesses", &(&self.unit,))
            .await
            .map_err(|_| Failure::Blocked)?;
        Ok(processes.len())
    }
    pub async fn next_generation(&mut self, session_fixture: bool) -> Result<Self> {
        if self.stop.is_none() || !self.terminated().await? {
            return Err(Failure::Blocked);
        }
        let proxy = Proxy::new(&self.connection, DEST, MANAGER, IFACE)
            .await
            .map_err(|_| Failure::Blocked)?;
        let _: OwnedObjectPath = proxy
            .call("StartUnit", &(&self.unit, "fail"))
            .await
            .map_err(|_| Failure::Blocked)?;
        let until = tokio::time::Instant::now() + Duration::from_millis(750);
        loop {
            if let Ok(next) = Self::capture(
                &self.unit,
                self.identity.backend_id.clone(),
                self.identity.guard_id.clone(),
                self.identity
                    .generation
                    .checked_add(1)
                    .ok_or(Failure::Limit)?,
                self.identity.profile_sha256.clone(),
                session_fixture,
            )
            .await
            {
                if next.identity.invocation_id == self.identity.invocation_id {
                    return Err(Failure::Conflict);
                }
                return Ok(next);
            }
            if tokio::time::Instant::now() >= until {
                return Err(Failure::Expired);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    pub async fn stop(&mut self, operation: &StopOperation) -> Result<ManagerObservation> {
        if operation.backend != self.identity {
            return Err(Failure::Conflict);
        }
        if let Some(original) = &self.stop {
            if original != operation {
                return Err(Failure::Conflict);
            }
        } else {
            self.stop = Some(operation.clone());
        }
        self.verify().await?;
        let fresh = snapshot(&self.connection, &self.unit).await?;
        if self.job.is_none() && fresh.state == "deactivating" {
            self.job = fresh.job.clone();
        }
        if self.submitted
            || fresh.state == "deactivating"
            || matches!(fresh.state.as_str(), "inactive" | "failed")
        {
            return Ok(self
                .job_id()
                .map_or(ManagerObservation::Unavailable {}, |job_id| {
                    ManagerObservation::Submitted { job_id }
                }));
        }
        let proxy = Proxy::new(&self.connection, DEST, MANAGER, IFACE)
            .await
            .map_err(|_| Failure::Blocked)?;
        // StopUnit is not compare-and-stop. All supported start/stop operations
        // require our fence. Root replacement is unsupported and closes the gate.
        self.verify().await?;
        self.submitted = true; // never issue a second submission after lost reply
        let job: OwnedObjectPath = proxy
            .call("StopUnit", &(&self.unit, "replace"))
            .await
            .map_err(|_| Failure::Blocked)?;
        self.job = Some(job.to_string());
        Ok(ManagerObservation::Submitted {
            job_id: self.job_id().ok_or(Failure::Blocked)?,
        })
    }
    pub fn job_path(&self) -> Option<&str> {
        self.job.as_deref()
    }
    pub fn job_id(&self) -> Option<ControlId> {
        self.job.as_ref().map(|job| {
            ControlId::new(
                playtime_backend_guard::digest(
                    format!(
                        "{}:{}:{}",
                        self.boot.as_str(),
                        self.identity.invocation_id.as_str(),
                        job
                    )
                    .as_bytes(),
                )
                .as_str()[..32]
                    .into(),
            )
            .expect("digest ID")
        })
    }
    pub fn kill_original(&mut self) -> Result<()> {
        // This handle targets the held original cgroup, never a reused path/unit.
        self.kill.write_all(b"1").map_err(|_| Failure::Blocked)
    }
    pub async fn terminated(&mut self) -> Result<bool> {
        self.verify().await?;
        let mut bytes = String::new();
        let read = self
            .events
            .seek(SeekFrom::Start(0))
            .and_then(|_| (&mut self.events).take(4097).read_to_string(&mut bytes));
        match read {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::NODEV.raw_os_error()) => {
                // The held original inode is now a dying cgroup. Kernel v2
                // removal requires no live processes/children and it cannot
                // revive. Combine this with fresh original manager identity
                // and an empty process tree; a missing *path* is never proof.
                self.saw_empty = true;
            }
            Err(_) => return Err(Failure::Blocked),
        }
        if bytes.len() > 4096 {
            return Err(Failure::Blocked);
        }
        if bytes.lines().any(|line| line == "populated 0") {
            self.saw_empty = true;
        }
        let state = snapshot(&self.connection, &self.unit).await?.state;
        let proxy = Proxy::new(&self.connection, DEST, MANAGER, IFACE)
            .await
            .map_err(|_| Failure::Blocked)?;
        let processes: Vec<(String, u32, String)> = proxy
            .call("GetUnitProcesses", &(&self.unit,))
            .await
            .map_err(|_| Failure::Blocked)?;
        Ok(self.saw_empty
            && processes.is_empty()
            && matches!(state.as_str(), "inactive" | "failed"))
    }
}
