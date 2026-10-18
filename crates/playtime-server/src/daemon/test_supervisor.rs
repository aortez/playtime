//! Explicit CPU process supervision for scripted effects only. It establishes
//! owned-child termination, never systemd enforcement or arbitrary cgroup cleanup.
use crate::lifecycle::{ports::Supervisor, *};
use async_trait::async_trait;
use playtime_protocol::worker::WorkerKind;
use playtime_worker_control::{Endpoint, boot_id, digest, systemd::process_start};
use std::{
    collections::HashMap,
    io::Read,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

struct Owned {
    launch: PreparedLaunch,
    endpoint: Endpoint,
    start_ticks: u64,
    child: Child,
    stopped: bool,
}
#[derive(Default)]
pub struct CpuTestSupervisor {
    children: Arc<Mutex<HashMap<String, Owned>>>,
}
impl CpuTestSupervisor {
    fn authenticate(owned: &mut Owned, launch: &PreparedLaunch) -> Result<()> {
        if &owned.launch != launch
            || launch.binding.kind != WorkerKind::ControllerSimulation
            || launch.binding.boot_id != boot_id()?
        {
            return Err(Error::Uncertain);
        }
        if owned.child.try_wait()?.is_some() {
            owned.stopped = true;
        }
        if !owned.stopped && process_start(owned.endpoint.pid)? != owned.start_ticks {
            return Err(Error::Uncertain);
        }
        Ok(())
    }
}
#[async_trait]
impl Supervisor for CpuTestSupervisor {
    async fn ensure_legacy_idle(&self, root: &Path) -> Result<()> {
        if root.join(".playtime/active.json").try_exists()? {
            return Err(Error::Busy);
        }
        Ok(())
    }
    async fn start(&self, launch: &PreparedLaunch) -> Result<Option<Endpoint>> {
        if launch.binding.kind != WorkerKind::ControllerSimulation {
            return Err(Error::Invalid(
                "CPU test supervisor accepts only controller simulation",
            ));
        }
        let (children, launch) = (self.children.clone(), launch.clone());
        tokio::task::spawn_blocking(move || {
            let mut children = children.lock().map_err(|_| Error::Uncertain)?;
            let key = launch.binding.invocation_id.as_str().to_owned();
            if children.contains_key(&key) {
                return Err(Error::Uncertain);
            }
            let mut child = Command::new(&launch.executable)
                .args(&launch.argv)
                .env_clear()
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let endpoint = Endpoint {
                socket: launch.socket.clone(),
                uid: std::fs::metadata(&launch.bootstrap)?.uid(),
                pid: child.id(),
                binding: launch.binding.clone(),
            };
            let start_ticks = match process_start(child.id()) {
                Ok(start) => start,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            };
            if let Err(error) = playtime_worker_control::write_once(&launch.private_dir.join("cpu-test-invocation.json"), &serde_json::to_vec(&serde_json::json!({"pid":endpoint.pid,"start_ticks":start_ticks,"binding":launch.binding}))?) {
                let _ = child.kill(); let _ = child.wait(); return Err(error.into());
            }
            children.insert(
                key,
                Owned {
                    launch,
                    endpoint: endpoint.clone(),
                    start_ticks,
                    child,
                    stopped: false,
                },
            );
            Ok(Some(endpoint))
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn locate(&self, launch: &PreparedLaunch) -> Result<Endpoint> {
        let (children, launch) = (self.children.clone(), launch.clone());
        tokio::task::spawn_blocking(move || {
            let endpoint = {
                let mut children = children.lock().map_err(|_| Error::Uncertain)?;
                let owned = children
                    .get_mut(launch.binding.invocation_id.as_str())
                    .ok_or(Error::Uncertain)?;
                Self::authenticate(owned, &launch)?;
                if owned.stopped {
                    return Err(Error::Uncertain);
                }
                owned.endpoint.clone()
            };
            let mut bytes = Vec::new();
            std::fs::File::open(format!("/proc/{}/exe", endpoint.pid))?
                .take(64 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 * 1024 || digest(&bytes) != launch.binding.executable_sha256
            {
                return Err(Error::Uncertain);
            }
            let mut children = children.lock().map_err(|_| Error::Uncertain)?;
            let owned = children
                .get_mut(launch.binding.invocation_id.as_str())
                .ok_or(Error::Uncertain)?;
            Self::authenticate(owned, &launch)?;
            if owned.stopped {
                return Err(Error::Uncertain);
            }
            Ok(endpoint)
        })
        .await
        .map_err(|_| Error::Uncertain)?
    }
    async fn observe(&self, launch: &PreparedLaunch) -> Result<Option<SupervisorObservation>> {
        let mut children = self.children.lock().map_err(|_| Error::Uncertain)?;
        let owned = children
            .get_mut(launch.binding.invocation_id.as_str())
            .ok_or(Error::Uncertain)?;
        Self::authenticate(owned, launch)?;
        Ok(Some(SupervisorObservation {
            manager_id: launch.binding.invocation_id.clone(),
            cgroup: "cpu-test-owned-child-no-systemd-enforcement".into(),
            active_state: if owned.stopped { "inactive" } else { "active" }.into(),
            main_pid: owned.endpoint.pid,
            ready_monotonic_us: 0,
            stopping_monotonic_us: 0,
            result: if owned.stopped { "reaped" } else { "running" }.into(),
            stopped: owned.stopped,
            exit: None,
        }))
    }
    async fn stop(
        &self,
        launch: &PreparedLaunch,
        authenticated: &SupervisorObservation,
    ) -> Result<()> {
        let mut children = self.children.lock().map_err(|_| Error::Uncertain)?;
        let owned = children
            .get_mut(launch.binding.invocation_id.as_str())
            .ok_or(Error::Uncertain)?;
        Self::authenticate(owned, launch)?;
        if authenticated.manager_id != launch.binding.invocation_id
            || authenticated.main_pid != owned.endpoint.pid
        {
            return Err(Error::Uncertain);
        }
        if !owned.stopped {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(owned.endpoint.pid as i32)
                    .ok_or(Error::Uncertain)?,
                rustix::process::Signal::TERM,
            )
            .map_err(std::io::Error::from)?;
        }
        Ok(())
    }
}
