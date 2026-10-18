//! Linux user-manager adapter. All commands are typed D-Bus values, with
//! environment expansion explicitly disabled. A lost submission is never retried.
use super::{ports::Supervisor, *};
use async_trait::async_trait;
use playtime_protocol::worker::ControlId;
use playtime_worker_control::{
    Bootstrap, Endpoint, boot_id, boot_ms, digest, read_private, systemd as worker,
};
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const DESTINATION: &str = "org.freedesktop.systemd1";
const MANAGER: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_IFACE: &str = "org.freedesktop.systemd1.Service";
const DEADLINE: Duration = Duration::from_secs(5);
type Properties = Vec<(String, OwnedValue)>;

pub struct SystemdSupervisor {
    connection: Connection,
    filesystem: Arc<tokio::sync::Semaphore>,
}
impl SystemdSupervisor {
    /// No fallback to a subprocess: unavailable managers/capabilities are explicit.
    pub async fn connect() -> Result<Self> {
        bounded(async {
            let connection = Connection::session().await?;
            let manager = Proxy::new(&connection, DESTINATION, MANAGER, MANAGER_IFACE).await?;
            let version: String = manager.get_property("Version").await?;
            let major: u32 = version
                .trim_start_matches('v')
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(Error::Invalid("unknown systemd version"))?;
            if major < 250 || !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
                return Err(Error::Invalid(
                    "systemd >= 250 with unified cgroups is required",
                ));
            }
            Ok(Self {
                connection,
                filesystem: Arc::new(tokio::sync::Semaphore::new(4)),
            })
        })
        .await
    }
    async fn filesystem<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self
            .filesystem
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn manager(&self) -> Result<Proxy<'_>> {
        Ok(Proxy::new(&self.connection, DESTINATION, MANAGER, MANAGER_IFACE).await?)
    }
    async fn properties(
        &self,
        launch: &PreparedLaunch,
    ) -> Result<Option<(HashMap<String, OwnedValue>, HashMap<String, OwnedValue>)>> {
        validate_unit(launch)?;
        let path: std::result::Result<OwnedObjectPath, zbus::Error> = self
            .manager()
            .await?
            .call("GetUnit", &(&launch.binding.unit,))
            .await;
        let path = match path {
            Ok(path) => path,
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit" =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let proxy = Proxy::new(
            &self.connection,
            DESTINATION,
            path,
            "org.freedesktop.DBus.Properties",
        )
        .await?;
        let unit = proxy.call("GetAll", &(UNIT_IFACE,)).await?;
        let service = proxy.call("GetAll", &(SERVICE_IFACE,)).await?;
        Ok(Some((unit, service)))
    }
    async fn observation(&self, launch: &PreparedLaunch) -> Result<SupervisorObservation> {
        let properties = self.properties(launch).await?;
        let launch = launch.clone();
        self.filesystem(move || observe_files(&launch, properties))
            .await
    }
    async fn endpoint(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        let observed = self.observation(launch).await?;
        if observed.stopped
            || !matches!(
                observed.active_state.as_str(),
                "activating" | "active" | "deactivating"
            )
        {
            return Err(Error::Uncertain);
        }
        let launch = launch.clone();
        self.filesystem(move || {
            let identity = worker::read_invocation(&launch.private_dir)?;
            if identity.binding != launch.binding
                || identity.manager_id != observed.manager_id
                || identity.pid != observed.main_pid
                || worker::process_start(identity.pid)? != identity.start_ticks
                || worker::process_cgroup(identity.pid)? != identity.cgroup
            {
                return Err(Error::Uncertain);
            }
            let mut bytes = Vec::new();
            File::open(format!("/proc/{}/exe", identity.pid))?
                .take(256 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 256 * 1024 * 1024
                || digest(&bytes) != launch.binding.executable_sha256
                || worker::process_start(identity.pid)? != identity.start_ticks
            {
                return Err(Error::Uncertain);
            }
            Ok(Endpoint {
                socket: launch.socket,
                uid: std::fs::metadata(&launch.bootstrap)?.uid(),
                pid: identity.pid,
                binding: launch.binding,
            })
        })
        .await
    }
}
#[async_trait]
impl Supervisor for SystemdSupervisor {
    async fn ensure_legacy_idle(&self, root: &Path) -> Result<()> {
        // Do not retire a legacy reservation based only on a manager observation.
        if root.join(".playtime/active.json").try_exists()? {
            return Err(Error::Busy);
        }
        Ok(())
    }
    async fn start(&self, launch: &PreparedLaunch) -> Result<Option<Endpoint>> {
        bounded(async {
            validate_unit(launch)?;
            let copy = launch.clone();
            let properties = self.filesystem(move || launch_properties(&copy)).await?;
            let aux: Vec<(String, Properties)> = Vec::new();
            // StartTransientUnit returns its job path without waiting for READY.
            // "fail" forbids replacing a preexisting job. Existing units fail too.
            let _: OwnedObjectPath = self
                .manager()
                .await?
                .call(
                    "StartTransientUnit",
                    &(&launch.binding.unit, "fail", properties, aux),
                )
                .await?;
            // A submission acknowledgment is not worker readiness. Query its
            // endpoint only after the launcher releases run exclusion.
            Ok(None)
        })
        .await
    }
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        bounded(self.endpoint(launch)).await
    }
    async fn observe(&self, launch: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
        bounded(async { self.observation(launch).await.map(Some) }).await
    }
    async fn stop(
        &self,
        launch: &PreparedLaunch,
        authenticated: &SupervisorObservation,
    ) -> Result<()> {
        bounded(async {
            // Do not read private diagnostics/bootstrap or acquire a filesystem
            // permit here. Those can fail or stall while the worker is alive.
            let (unit, service) = self.properties(launch).await?.ok_or(Error::Uncertain)?;
            let manager_id = verify_manager(launch, &unit, &service)?;
            let cgroup = string(&service, "ControlGroup")?;
            let state = string(&unit, "ActiveState")?;
            if manager_id != authenticated.manager_id
                || (cgroup != authenticated.cgroup
                    && !(cgroup.is_empty() && matches!(state.as_str(), "inactive" | "failed")))
            {
                return Err(Error::Uncertain);
            }
            if state == "deactivating" {
                // A lost acknowledgement must not restart the original grace.
                return Ok(());
            }
            // Unit names contain the immutable grant/launch nonce and are never
            // reused. Verify systemd's invocation immediately before stopping it.
            let _: OwnedObjectPath = self
                .manager()
                .await?
                .call("StopUnit", &(&launch.binding.unit, "replace"))
                .await?;
            // Stop acceptance is sufficient even if the unit disappears now.
            Ok(())
        })
        .await
    }
}
async fn bounded<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(DEADLINE, future)
        .await
        .map_err(|_| Error::Uncertain)?
}
fn validate_unit(launch: &PreparedLaunch) -> Result<()> {
    // A fresh cryptographic launch nonce prevents accidental name reuse across
    // service roots and makes queries/stops name an exact owned invocation.
    if launch.binding.unit != format!("playtime-{}.service", launch.binding.invocation_id.as_str())
        || launch.binding.boot_id != boot_id()?
    {
        return Err(Error::Invalid("systemd unit/boot identity mismatch"));
    }
    Ok(())
}
fn property<'a>(name: &str, value: impl Into<Value<'a>>) -> (String, OwnedValue) {
    (
        name.to_owned(),
        value.into().try_to_owned().expect("owned D-Bus property"),
    )
}
fn launch_properties(launch: &PreparedLaunch) -> Result<Properties> {
    let bootstrap: Bootstrap = serde_json::from_slice(&read_private(&launch.bootstrap, 65_536)?)?;
    if bootstrap.binding != launch.binding
        || bootstrap.private_dir != launch.private_dir
        || bootstrap.socket != launch.socket
        || bootstrap.startup_deadline_boot_ms != launch.startup_deadline_boot_ms
        || !(1..=86_400_000).contains(&bootstrap.wall_ms)
    {
        return Err(Error::Invalid("systemd bootstrap binding mismatch"));
    }
    let startup_ms = launch
        .startup_deadline_boot_ms
        .saturating_sub(boot_ms())
        .min(60_000);
    if startup_ms == 0 {
        return Err(Error::Invalid("startup deadline expired before submission"));
    }
    let executable = launch
        .executable
        .to_str()
        .filter(|_| launch.executable.is_absolute())
        .ok_or(Error::Invalid("absolute UTF-8 worker path required"))?;
    let bootstrap_path = launch
        .bootstrap
        .to_str()
        .filter(|_| launch.bootstrap.is_absolute())
        .ok_or(Error::Invalid("absolute UTF-8 bootstrap path required"))?;
    let argv: Vec<&str> = std::iter::once(executable)
        .chain(launch.argv.iter().map(String::as_str))
        .collect();
    let command = vec![(executable, argv, vec!["no-env-expand"])];
    Ok(vec![
        property(
            "Description",
            format!(
                "Playtime {} {}",
                launch.binding.service_id.as_str(),
                launch.binding.invocation_id.as_str()
            ),
        ),
        property("Type", "notify"),
        property("NotifyAccess", "main"),
        property("Restart", "no"),
        property("UMask", 0o077u32),
        property("TimeoutStartUSec", startup_ms * 1000),
        property("TimeoutStopUSec", 180_000_000u64),
        property("RuntimeMaxUSec", bootstrap.wall_ms * 1000),
        property("RuntimeRandomizedExtraUSec", 0u64),
        property("KillMode", "mixed"),
        property("SendSIGKILL", true),
        property("ExecStartEx", command.clone()),
        property("ExecStopPostEx", command),
        property(
            "Environment",
            vec![
                format!("PLAYTIME_LIFECYCLE_BOOTSTRAP={bootstrap_path}"),
                "PLAYTIME_SYSTEMD_WORKER=1".into(),
            ],
        ),
        property("StandardInput", "null"),
        property("StandardOutput", "null"),
        property("StandardError", "journal"),
    ])
}
fn string(properties: &HashMap<String, OwnedValue>, key: &str) -> Result<String> {
    properties
        .get(key)
        .and_then(|v| <&str>::try_from(v).ok())
        .map(str::to_owned)
        .ok_or(Error::Invalid("missing systemd string property"))
}
fn number<T>(properties: &HashMap<String, OwnedValue>, key: &str) -> Result<T>
where
    for<'a> T: TryFrom<&'a OwnedValue>,
{
    properties
        .get(key)
        .and_then(|v| T::try_from(v).ok())
        .ok_or(Error::Invalid("missing systemd numeric property"))
}
fn verify_manager(
    launch: &PreparedLaunch,
    unit: &HashMap<String, OwnedValue>,
    service: &HashMap<String, OwnedValue>,
) -> Result<ControlId> {
    if string(unit, "Id")? != launch.binding.unit
        || string(unit, "Description")?
            != format!(
                "Playtime {} {}",
                launch.binding.service_id.as_str(),
                launch.binding.invocation_id.as_str()
            )
    {
        return Err(Error::Uncertain);
    }
    if string(service, "Type")? != "notify"
        || string(service, "NotifyAccess")? != "main"
        || string(service, "Restart")? != "no"
        || string(service, "KillMode")? != "mixed"
        || !number::<bool>(service, "SendSIGKILL")?
        || number::<u32>(service, "UMask")? != 0o077
        || number::<u64>(service, "TimeoutStopUSec")? != 180_000_000
        || !(1..=60_000_000).contains(&number::<u64>(service, "TimeoutStartUSec")?)
    {
        return Err(Error::Invalid(
            "systemd enforcement policy changed or unsupported",
        ));
    }
    let bytes: Vec<u8> = unit
        .get("InvocationID")
        .ok_or(Error::Uncertain)?
        .try_clone()?
        .try_into()?;
    ControlId::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .map_err(|_| Error::Uncertain)
}
fn observe_files(
    launch: &PreparedLaunch,
    properties: Option<(HashMap<String, OwnedValue>, HashMap<String, OwnedValue>)>,
) -> Result<SupervisorObservation> {
    let exit = worker::read_exit(&launch.private_dir)?;
    if let Some(exit) = &exit
        && exit.binding != launch.binding
    {
        return Err(Error::Uncertain);
    }
    let identity = match worker::read_invocation(&launch.private_dir) {
        Ok(identity) if identity.binding == launch.binding => Some(identity),
        Err(playtime_worker_control::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            None
        }
        _ => return Err(Error::Uncertain),
    };
    let (
        manager_id,
        cgroup,
        active_state,
        main_pid,
        ready_monotonic_us,
        stopping_monotonic_us,
        result,
    ) = match properties {
        Some((unit, service)) => {
            let id = verify_manager(launch, &unit, &service)?;
            let bootstrap: Bootstrap =
                serde_json::from_slice(&read_private(&launch.bootstrap, 65_536)?)?;
            if bootstrap.binding != launch.binding
                || number::<u64>(&service, "RuntimeMaxUSec")? != bootstrap.wall_ms * 1000
            {
                return Err(Error::Invalid("systemd wall allocation changed"));
            }
            (
                id,
                string(&service, "ControlGroup")?,
                string(&unit, "ActiveState")?,
                number(&service, "MainPID")?,
                number(&unit, "ActiveEnterTimestampMonotonic")?,
                number(&unit, "ActiveExitTimestampMonotonic")?,
                string(&service, "Result")?,
            )
        }
        None => {
            let saved = exit.as_ref().ok_or(Error::Uncertain)?;
            (
                saved.manager_id.clone(),
                saved.cgroup.clone(),
                "unloaded".into(),
                0,
                0,
                0,
                saved.result.clone(),
            )
        }
    };
    if identity
        .as_ref()
        .is_some_and(|i| i.manager_id != manager_id || (!cgroup.is_empty() && i.cgroup != cgroup))
        || exit.as_ref().is_some_and(|e| {
            e.manager_id != manager_id || (!cgroup.is_empty() && e.cgroup != cgroup)
        })
    {
        return Err(Error::Uncertain);
    }
    // ControlGroup can clear once stopped; retain the verified invocation's path.
    let cgroup = if cgroup.is_empty() {
        identity
            .as_ref()
            .map(|i| i.cgroup.clone())
            .or_else(|| exit.as_ref().map(|e| e.cgroup.clone()))
            .ok_or(Error::Uncertain)?
    } else {
        cgroup
    };
    let stopped = matches!(active_state.as_str(), "failed" | "inactive" | "unloaded")
        && main_pid == 0
        && cgroup_empty(&cgroup)?;
    Ok(SupervisorObservation {
        manager_id,
        cgroup,
        active_state,
        main_pid,
        ready_monotonic_us,
        stopping_monotonic_us,
        result,
        stopped,
        exit,
    })
}
fn cgroup_empty(cgroup: &str) -> Result<bool> {
    let path = Path::new(cgroup);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        || path == Path::new("/")
    {
        return Err(Error::Uncertain);
    }
    match std::fs::read_to_string(
        Path::new("/sys/fs/cgroup")
            .join(path.strip_prefix("/").map_err(|_| Error::Uncertain)?)
            .join("cgroup.events"),
    ) {
        Ok(events) => Ok(events.lines().any(|line| line == "populated 0")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e.into()),
    }
}
