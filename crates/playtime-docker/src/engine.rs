use crate::{
    CancellationToken, Error, Failure, Result,
    transport::{Dispatch, Fault, LocalExit, Operation, Outcome, Transport},
    types::*,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::time::Instant;

#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    pub operation: &'static str,
    pub resource: Option<String>,
    pub acknowledged_resource: Option<String>,
    pub budget_ms: u128,
    pub elapsed_ms: u128,
    pub dispatch: Dispatch,
    pub local: LocalExit,
    pub fault: Option<Fault>,
    /// Successful CLI acknowledgement only; subsequent inspect establishes the
    /// requested state. No model text, raw argv or unrestricted diagnostics.
    pub acknowledged: bool,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}
#[derive(Clone)]
pub struct Engine {
    transport: Arc<dyn Transport>,
    identity: DaemonIdentity,
    observations: Arc<Mutex<VecDeque<Observation>>>,
}
#[derive(Clone)]
pub struct ReadyImage {
    pub(crate) engine: Engine,
    pub(crate) image: String,
}
impl ReadyImage {
    pub fn image_id(&self) -> &str {
        &self.image
    }
    pub fn daemon(&self) -> &DaemonIdentity {
        &self.engine.identity
    }
}
impl Engine {
    pub async fn connect(
        transport: Arc<dyn Transport>,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let version = transport
            .execute(Operation::Version, deadline, cancel.clone())
            .await;
        let info = transport.execute(Operation::Info, deadline, cancel).await;
        let version = parse_success(&version, "version")?;
        let info = parse_success(&info, "info")?;
        let identity = identity(&version, &info)?;
        Ok(Self {
            transport,
            identity,
            observations: Arc::new(Mutex::new(VecDeque::new())),
        })
    }
    pub fn identity(&self) -> &DaemonIdentity {
        &self.identity
    }
    pub fn observations(&self) -> Vec<Observation> {
        self.observations
            .lock()
            .expect("observations")
            .iter()
            .cloned()
            .collect()
    }
    pub(crate) async fn raw(
        &self,
        op: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Outcome {
        let start = Instant::now();
        let operation = op.name();
        let resource = match &op {
            Operation::CreateVolume(p) | Operation::CreateContainer(p) => Some(p.name.clone()),
            Operation::ImageInspect(id)
            | Operation::ContainerInspect(id)
            | Operation::VolumeInspect(id)
            | Operation::Start(id)
            | Operation::RemoveContainer(id)
            | Operation::RemoveVolume(id)
            | Operation::Probe(id)
            | Operation::Mounts(id)
            | Operation::Bridge { id, .. } => Some(id.clone()),
            _ => None,
        };
        let returns_resource = matches!(
            op,
            Operation::CreateVolume(_) | Operation::CreateContainer(_) | Operation::Start(_)
        );
        let result = self.transport.execute(op, deadline, cancel).await;
        let acknowledged_resource = if returns_resource && result.success() {
            std::str::from_utf8(&result.stdout)
                .ok()
                .map(str::trim)
                .filter(|s| token(s))
                .map(str::to_owned)
        } else {
            None
        };
        let mut observations = self.observations.lock().expect("observations");
        if observations.len() == 256 {
            observations.pop_front();
        }
        observations.push_back(Observation {
            operation,
            resource,
            acknowledged_resource,
            budget_ms: deadline.saturating_duration_since(start).as_millis(),
            elapsed_ms: start.elapsed().as_millis(),
            dispatch: result.dispatch,
            local: result.local,
            fault: result.fault,
            acknowledged: result.success(),
            stdout_bytes: result.stdout.len(),
            stderr_bytes: result.stderr.len(),
        });
        result
    }
    pub(crate) async fn check(&self, deadline: Instant, cancel: CancellationToken) -> Result<()> {
        let result = self.raw(Operation::Info, deadline, cancel).await;
        let info = parse_success(&result, "info")?;
        validate_info(&info)?;
        if bounded_string(&info["ID"])? != self.identity.id
            || bounded_string(&info["Name"])? != self.identity.name
            || bounded_string(&info["ServerVersion"])? != self.identity.server_version
            || bounded_string(&info["KernelVersion"])? != self.identity.kernel
        {
            return Err(Error::new(
                Failure::ChangedIdentity,
                "info",
                "selected daemon identity changed",
            ));
        }
        Ok(())
    }
    pub(crate) async fn call(
        &self,
        op: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Outcome> {
        self.check(deadline, cancel.clone()).await?;
        Ok(self.raw(op, deadline, cancel).await)
    }
    /// Read-only image check. The exact locked content ID binds the bridge and
    /// interpreter profile. Actual helper hashes are checked inside the owned
    /// holder before any model tool; preview creates/starts nothing.
    pub async fn readiness(
        &self,
        lock: &[u8],
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<ReadyImage> {
        let bad = || {
            Error::new(
                Failure::InvalidInput,
                "readiness",
                "unsupported or malformed image lock",
            )
        };
        if lock.len() > 65_536 {
            return Err(bad());
        }
        let lock: Value = serde_json::from_slice(lock).map_err(|_| bad())?;
        let id = lock["image_id"].as_str().ok_or_else(bad)?;
        if lock["schema"] != 1
            || lock["os"] != "linux"
            || lock["architecture"] != "amd64"
            || !id.strip_prefix("sha256:").is_some_and(|s| hex(s, 64))
            || lock["build_sources"]["tool.py"] != BRIDGE_SHA256
        {
            return Err(bad());
        }
        let outcome = self
            .call(Operation::ImageInspect(id.into()), deadline, cancel.clone())
            .await?;
        if missing(&outcome, "image") {
            self.check(deadline, cancel).await?;
            return Err(Error::new(
                Failure::MissingImage,
                "readiness",
                "locked image absent; explicit operator restoration required",
            ));
        }
        let image = one(&outcome, "image_inspect")?;
        if image["Id"] != id || image["Os"] != "linux" || image["Architecture"] != "amd64" {
            return Err(Error::new(
                Failure::ChangedIdentity,
                "readiness",
                "locked image/platform changed",
            ));
        }
        if !empty(&image["Config"]["Volumes"]) || !empty(&image["Config"]["OnBuild"]) {
            return Err(Error::new(
                Failure::Unsupported,
                "readiness",
                "unexpected image volumes or build hooks",
            ));
        }
        Ok(ReadyImage {
            engine: self.clone(),
            image: id.into(),
        })
    }
    pub(crate) async fn container(
        &self,
        name: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Option<Value>> {
        let result = self
            .call(
                Operation::ContainerInspect(name.into()),
                deadline,
                cancel.clone(),
            )
            .await?;
        if missing(&result, "container") {
            self.check(deadline, cancel).await?;
            Ok(None)
        } else {
            one(&result, "container_inspect").map(Some)
        }
    }
    pub(crate) async fn volume(
        &self,
        name: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Option<Value>> {
        let result = self
            .call(
                Operation::VolumeInspect(name.into()),
                deadline,
                cancel.clone(),
            )
            .await?;
        if missing(&result, "volume") {
            self.check(deadline, cancel).await?;
            Ok(None)
        } else {
            one(&result, "volume_inspect").map(Some)
        }
    }
    pub(crate) async fn probe(
        &self,
        id: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<()> {
        let result = self
            .call(Operation::Probe(id.into()), deadline, cancel)
            .await?;
        let probe = parse_success(&result, "helper_identity")?;
        if probe != json!({"python":PYTHON,"bridge_sha256":BRIDGE_SHA256,"uid":1000,"gid":1000}) {
            return Err(Error::new(
                Failure::ChangedIdentity,
                "helper_identity",
                "locked interpreter or bridge mismatch",
            ));
        }
        Ok(())
    }
}
pub(crate) fn outcome_error(result: &Outcome, operation: &'static str) -> Error {
    let failure = match result.fault {
        Some(Fault::Cancelled) => Failure::Cancelled,
        Some(Fault::Deadline) => Failure::Deadline,
        Some(Fault::SelectionChanged) => Failure::ChangedIdentity,
        _ => Failure::Transport,
    };
    Error::new(
        failure,
        operation,
        "Docker operation not acknowledged; daemon outcome may be uncertain",
    )
}
fn parse_success(result: &Outcome, operation: &'static str) -> Result<Value> {
    if !result.success() {
        if matches!(
            result.fault,
            Some(Fault::Cancelled | Fault::Deadline | Fault::SelectionChanged)
        ) {
            return Err(outcome_error(result, operation));
        }
        return Err(Error::new(
            Failure::DaemonUnavailable,
            operation,
            "selected daemon did not provide a successful response",
        ));
    }
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| Error::new(Failure::Malformed, operation, "response is not UTF-8"))?;
    serde_json::from_str(text)
        .map_err(|_| Error::new(Failure::Malformed, operation, "invalid Docker JSON"))
}
fn one(result: &Outcome, op: &'static str) -> Result<Value> {
    let value = parse_success(result, op)?;
    let array = value
        .as_array()
        .filter(|a| a.len() == 1 && a[0].is_object())
        .ok_or_else(|| Error::new(Failure::Malformed, op, "expected one inspected object"))?;
    Ok(array[0].clone())
}
fn missing(result: &Outcome, kind: &str) -> bool {
    result.fault.is_none()
        && result.local
            == LocalExit::Reaped {
                code: Some(1),
                signal: None,
            }
        && (result.stdout.iter().all(u8::is_ascii_whitespace)
            || serde_json::from_slice::<Value>(&result.stdout).ok() == Some(json!([])))
        && std::str::from_utf8(&result.stderr).is_ok_and(|s| {
            let lower = s.to_ascii_lowercase();
            lower.contains(&format!("no such {kind}"))
                || (kind == "container" && lower.contains("no such object"))
        })
}
fn bounded_string(value: &Value) -> Result<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::new(
                Failure::Malformed,
                "identity",
                "invalid bounded identity field",
            )
        })
}
fn identity(version: &Value, info: &Value) -> Result<DaemonIdentity> {
    validate_info(info)?;
    let client = &version["Client"];
    let server = &version["Server"];
    if client["Os"] != "linux"
        || client["Arch"] != "amd64"
        || server["Os"] != "linux"
        || server["Arch"] != "amd64"
    {
        return Err(Error::new(
            Failure::Unsupported,
            "version",
            "only local Linux amd64 Docker Engine is supported",
        ));
    }
    let api = bounded_string(&server["ApiVersion"])?;
    let supported = api
        .split_once('.')
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .is_some_and(|(major, minor)| major == 1 && minor >= 41);
    if !supported {
        return Err(Error::new(
            Failure::Unsupported,
            "version",
            "Docker API 1.41 or newer required",
        ));
    }
    let identity = DaemonIdentity {
        id: bounded_string(&info["ID"])?,
        name: bounded_string(&info["Name"])?,
        cli_version: bounded_string(&client["Version"])?,
        server_version: bounded_string(&server["Version"])?,
        api_version: api,
        kernel: bounded_string(&info["KernelVersion"])?,
    };
    if info["ServerVersion"] != identity.server_version {
        return Err(Error::new(
            Failure::ChangedIdentity,
            "version",
            "server identity changed during discovery",
        ));
    }
    Ok(identity)
}
fn validate_info(info: &Value) -> Result<()> {
    let security = info["SecurityOptions"]
        .as_array()
        .ok_or_else(|| Error::new(Failure::Malformed, "info", "missing security capabilities"))?;
    if info["OSType"] != "linux"
        || !matches!(info["Architecture"].as_str(), Some("x86_64" | "amd64"))
        || info["CgroupVersion"] != "2"
        || [
            "MemoryLimit",
            "SwapLimit",
            "CpuCfsQuota",
            "CpuCfsPeriod",
            "PidsLimit",
        ]
        .iter()
        .any(|k| info[k] != true)
        || security.iter().any(|s| {
            s.as_str()
                .is_none_or(|s| s.contains("rootless") || s.contains("userns"))
        })
        || !security
            .iter()
            .any(|s| s.as_str().is_some_and(|s| s.starts_with("name=seccomp")))
        || info["OperatingSystem"]
            .as_str()
            .is_none_or(|s| s.contains("Docker Desktop"))
        || !info["Plugins"]["Volume"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("local")))
    {
        return Err(Error::new(
            Failure::Unsupported,
            "info",
            "requires rootful Linux cgroup v2, seccomp and enforceable CPU/memory/swap/PID limits",
        ));
    }
    Ok(())
}
pub(crate) fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

pub(crate) fn verify_volume(value: &Value, p: &Proposal) -> Result<()> {
    if value["Name"] != p.name
        || value["Driver"] != "local"
        || value["Scope"] != "local"
        || value["Labels"] != json!(p.labels())
        || value["Options"] != json!(p.limits.volume_options())
    {
        return Err(Error::new(
            Failure::Conflict,
            "volume_inspect",
            "volume name, labels, driver or limits differ",
        ));
    }
    Ok(())
}

pub(crate) fn verify_container(value: &Value, p: &Proposal) -> Result<String> {
    let bad = || {
        Error::new(
            Failure::Conflict,
            "container_inspect",
            "container identity or effective sandbox configuration differs",
        )
    };
    let c = &value["Config"];
    let h = &value["HostConfig"];
    let l = &p.limits;
    let id = value["Id"]
        .as_str()
        .filter(|s| hex(s, 64))
        .ok_or_else(bad)?;
    let (memory, pids) = if p.role == Role::Holder {
        (64 * 1024 * 1024, 64)
    } else {
        (l.memory_bytes, l.pids)
    };
    if value["Name"] != format!("/{}", p.name)
        || value["Image"] != p.image
        || c["Image"] != p.image
        || c["Labels"] != json!(p.labels())
        || c["Entrypoint"] != json!([PYTHON])
        || c["Cmd"]
            != json!([
                "-I",
                "-S",
                "-B",
                "-c",
                TIMER,
                p.deadline_monotonic_ns.to_string()
            ])
        || c["User"] != "0:0"
        || c["WorkingDir"] != "/"
        || c["Healthcheck"]["Test"] != json!(["NONE"])
        || !empty(&c["Volumes"])
        || h["NetworkMode"] != "none"
        || h["ReadonlyRootfs"] != true
        || h["Privileged"] != false
        || h["CapDrop"] != json!(["ALL"])
        || !empty(&h["CapAdd"])
        || h["SecurityOpt"] != json!(["no-new-privileges"])
        || h["LogConfig"]["Type"] != "none"
        || !empty(&h["LogConfig"]["Config"])
        || h["RestartPolicy"]["Name"] != "no"
        || h["RestartPolicy"]["MaximumRetryCount"] != 0
        || h["NanoCpus"] != l.nano_cpus
        || h["Memory"] != memory
        || h["MemorySwap"] != memory
        || h["PidsLimit"] != pids
        || h["ShmSize"] != l.tmp_bytes
        || h["CgroupnsMode"] != "private"
        || h["IpcMode"] != "private"
        || h["PidMode"] != ""
        || h["UTSMode"] != ""
        || h["UsernsMode"] != ""
        || h["Tmpfs"] != json!({"/tmp":l.tmpfs()})
        || !empty(&h["Binds"])
        || !empty(&h["VolumesFrom"])
        || !empty(&h["Devices"])
        || !empty(&h["DeviceRequests"])
        || !empty(&h["DeviceCgroupRules"])
        || !empty(&h["GroupAdd"])
        || !empty(&h["ExtraHosts"])
        || !empty(&h["Sysctls"])
        || !empty(&h["PortBindings"])
        || !empty(&h["Links"])
        || h["PublishAllPorts"] != false
        || h["AutoRemove"] != false
        // Docker normalizes the unset pointer from false at create to null
        // after start. Both retain the OOM killer; true must be rejected.
        || !matches!(h["OomKillDisable"],Value::Null|Value::Bool(false))
    {
        return Err(bad());
    }
    let env = c["Env"].as_array().ok_or_else(bad)?;
    for key in ["PLAYTIME_SEED", "PYTHONHASHSEED"] {
        if env
            .iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|s| s.starts_with(&format!("{key}=")))
            })
            .collect::<Vec<_>>()
            != vec![&json!(format!("{key}={}", l.seed))]
        {
            return Err(bad());
        }
    }
    let mounts = h["Mounts"]
        .as_array()
        .filter(|a| a.len() == 1)
        .ok_or_else(bad)?;
    if mounts[0]["Type"] != "volume"
        || mounts[0]["Source"] != p.volume
        || mounts[0]["Target"] != "/work"
        || !matches!(mounts[0].get("ReadOnly"), None | Some(Value::Bool(false)))
        || mounts[0]["VolumeOptions"]["NoCopy"] != true
        || !empty(&mounts[0]["VolumeOptions"]["DriverConfig"])
        || !mounts[0].as_object().is_some_and(|o| {
            o.keys().all(|k| {
                ["Type", "Source", "Target", "ReadOnly", "VolumeOptions"].contains(&k.as_str())
            })
        })
        || !mounts[0]["VolumeOptions"].as_object().is_some_and(|o| {
            o.keys()
                .all(|k| ["NoCopy", "DriverConfig"].contains(&k.as_str()))
        })
    {
        return Err(bad());
    }
    let mounts = value["Mounts"]
        .as_array()
        .filter(|a| a.len() == 1)
        .ok_or_else(bad)?;
    if mounts[0]["Type"] != "volume"
        || mounts[0]["Name"] != p.volume
        || mounts[0]["Destination"] != "/work"
        || mounts[0]["Driver"] != "local"
        || mounts[0]["RW"] != true
    {
        return Err(bad());
    }
    // Running state is checked separately for each lifecycle operation.
    let networks = value["NetworkSettings"]["Networks"]
        .as_object()
        .ok_or_else(bad)?;
    if networks.len() != 1
        || !networks.contains_key("none")
        || !empty(&value["NetworkSettings"]["Ports"])
        || ["Gateway", "IPAddress", "IPv6Gateway", "GlobalIPv6Address"]
            .iter()
            .any(|k| networks["none"][k] != "")
    {
        return Err(bad());
    }
    if !value["State"]["Running"].is_boolean() || !value["State"]["Status"].is_string() {
        return Err(bad());
    }
    Ok(id.into())
}
