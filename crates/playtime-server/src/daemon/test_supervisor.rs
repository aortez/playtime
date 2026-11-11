//! Explicit CPU process supervision for scripted effects only. It establishes
//! owned-child termination, never systemd enforcement or arbitrary cgroup cleanup.
//! A restarted daemon adopts the worker an earlier daemon started on this boot
//! (#59), through a pidfd opened while that exact process runs: its PID and
//! kernel start time match the saved invocation, and `locate` checks its
//! executable. It never infers an exit from a missing or reused PID. An adopted
//! worker has exited only once its pidfd reports it.
use crate::lifecycle::{ports::Supervisor, *};
use async_trait::async_trait;
use playtime_protocol::worker::{Binding, WorkerKind};
use playtime_worker_control::{Endpoint, boot_id, digest, read_private, systemd::process_start};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal},
};
use std::{
    collections::HashMap,
    io::Read,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

const INVOCATION: &str = "cpu-test-invocation.json";

enum Process {
    /// Started by this daemon, which reaps it.
    Child(Child),
    /// Started by an earlier daemon on this boot. Its pidfd names that exact
    /// process and turns readable when it exits.
    Adopted(OwnedFd),
}
struct Owned {
    launch: PreparedLaunch,
    endpoint: Endpoint,
    start_ticks: u64,
    process: Process,
    stopped: bool,
}
fn exited(pidfd: &OwnedFd) -> Result<bool> {
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ready = poll(&mut fds, Some(&now)).map_err(std::io::Error::from)?;
    Ok(ready == 1 && fds[0].revents().contains(PollFlags::IN))
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
        let exit = match &mut owned.process {
            Process::Child(child) => child.try_wait()?.is_some(),
            Process::Adopted(pidfd) => exited(pidfd)?,
        };
        if exit {
            owned.stopped = true;
        }
        if !owned.stopped && process_start(owned.endpoint.pid)? != owned.start_ticks {
            return Err(Error::Uncertain);
        }
        Ok(())
    }
    /// The worker an earlier daemon started for this launch, while it runs.
    fn adopt(launch: &PreparedLaunch) -> Result<Owned> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Invocation {
            pid: u32,
            start_ticks: u64,
            binding: Binding,
        }
        if launch.binding.kind != WorkerKind::ControllerSimulation
            || launch.binding.boot_id != boot_id()?
        {
            return Err(Error::Uncertain);
        }
        let saved: Invocation =
            serde_json::from_slice(&read_private(&launch.private_dir.join(INVOCATION), 65_536)?)?;
        if saved.binding != launch.binding {
            return Err(Error::Uncertain);
        }
        let pid = Pid::from_raw(i32::try_from(saved.pid).map_err(|_| Error::Uncertain)?)
            .ok_or(Error::Uncertain)?;
        let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(|_| Error::Uncertain)?;
        // Checked after opening the pidfd, so the pidfd names the saved process.
        if process_start(saved.pid)? != saved.start_ticks {
            return Err(Error::Uncertain);
        }
        Ok(Owned {
            launch: launch.clone(),
            endpoint: Endpoint {
                socket: launch.socket.clone(),
                uid: std::fs::metadata(&launch.bootstrap)?.uid(),
                pid: saved.pid,
                binding: launch.binding.clone(),
            },
            start_ticks: saved.start_ticks,
            process: Process::Adopted(pidfd),
            stopped: false,
        })
    }
    /// The authenticated worker of this launch, adopting it on first use.
    fn owned<'a>(
        children: &'a mut HashMap<String, Owned>,
        launch: &PreparedLaunch,
    ) -> Result<&'a mut Owned> {
        let key = launch.binding.invocation_id.as_str();
        if !children.contains_key(key) {
            let mut adopted = Self::adopt(launch)?;
            Self::authenticate(&mut adopted, launch)?;
            children.insert(key.to_owned(), adopted);
        }
        let owned = children.get_mut(key).ok_or(Error::Uncertain)?;
        Self::authenticate(owned, launch)?;
        Ok(owned)
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
            // An earlier daemon's start leaves its invocation behind.
            if children.contains_key(&key) || launch.private_dir.join(INVOCATION).try_exists()? {
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
            if let Err(error) = playtime_worker_control::write_once(&launch.private_dir.join(INVOCATION), &serde_json::to_vec(&serde_json::json!({"pid":endpoint.pid,"start_ticks":start_ticks,"binding":launch.binding}))?) {
                let _ = child.kill(); let _ = child.wait(); return Err(error.into());
            }
            children.insert(
                key,
                Owned {
                    launch,
                    endpoint: endpoint.clone(),
                    start_ticks,
                    process: Process::Child(child),
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
                let owned = Self::owned(&mut children, &launch)?;
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
            let owned = Self::owned(&mut children, &launch)?;
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
        let owned = Self::owned(&mut children, launch)?;
        Ok(Some(SupervisorObservation {
            manager_id: launch.binding.invocation_id.clone(),
            cgroup: "cpu-test-owned-child-no-systemd-enforcement".into(),
            active_state: if owned.stopped { "inactive" } else { "active" }.into(),
            main_pid: owned.endpoint.pid,
            ready_monotonic_us: 0,
            stopping_monotonic_us: 0,
            result: match (owned.stopped, &owned.process) {
                (false, _) => "running",
                (true, Process::Child(_)) => "reaped",
                (true, Process::Adopted(_)) => "exited",
            }
            .into(),
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
        let owned = Self::owned(&mut children, launch)?;
        if authenticated.manager_id != launch.binding.invocation_id
            || authenticated.main_pid != owned.endpoint.pid
        {
            return Err(Error::Uncertain);
        }
        if !owned.stopped {
            match &owned.process {
                Process::Child(_) => rustix::process::kill_process(
                    Pid::from_raw(owned.endpoint.pid as i32).ok_or(Error::Uncertain)?,
                    Signal::TERM,
                ),
                Process::Adopted(pidfd) => pidfd_send_signal(pidfd, Signal::TERM),
            }
            .map_err(std::io::Error::from)?;
        }
        Ok(())
    }
}
