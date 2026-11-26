//! Strata targets, which the daemon holds through the chat launcher's hold mode
//! for one run at a time. The launcher owns the host: it refuses a busy one,
//! starts or reuses the server, verifies it against the reviewed lock and
//! forwards a loopback endpoint. The daemon checks what the hold lends, freezes
//! it for the worker and ends the hold once the worker has stopped.
use crate::lifecycle::{self as life, Error};
use playtime_core::RunId;
use playtime_protocol::api::InferenceTarget;
use playtime_runner::docker::StrataHost;
use playtime_strata::{CancellationToken, Endpoint, Limits, ModelLock};
use serde_json::Value;
use std::{
    collections::HashMap,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::Instant,
};

const PROTOCOL: &str = "hold_v1";
/// The largest event line a hold may write. `held` carries the verifier's
/// whole report.
const EVENT_BYTES: u64 = 4 << 20;
/// Added to the launcher's own budgets, for the wrapper's work around them.
const MARGIN: Duration = Duration::from_secs(60);
/// The launcher's read-only status, including its own 8-second SSH bound.
const STATUS_DEADLINE: Duration = Duration::from_secs(15);
/// Reading the package's hold description touches no host.
const DESCRIBE_DEADLINE: Duration = Duration::from_secs(10);
/// How long a hold has to exit after SIGTERM before the daemon leaves it.
const TERM_GRACE: Duration = Duration::from_secs(30);
/// The launcher runs on the system's Python with the system's commands, such
/// as `ssh`, whatever the daemon's own PATH holds.
const PYTHON: &str = "/usr/bin/python3";
const LAUNCHER_PATH: &str = "/usr/bin:/bin";

/// Writes one line to the daemon's log. A log that can't be written mustn't
/// fail the hold, as a panicking eprintln would; tests capture eprintln.
fn log(message: std::fmt::Arguments) {
    #[cfg(test)]
    eprintln!("playtimed: {message}");
    #[cfg(not(test))]
    {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "playtimed: {message}");
    }
}

/// One Strata target: its launcher profile and the compiled lock it holds.
pub(super) struct StrataTarget {
    pub config: InferenceTarget,
    program: PathBuf,
    pub profile: String,
    pub lock_name: String,
    lock: ModelLock,
    lock_file: String,
    lock_sha256: String,
    pub live_lock: PathBuf,
    /// From the launch until `held` or `refused`.
    acquire: Duration,
    /// From the end of a hold until `ended` or `refused`.
    cleanup: Duration,
    /// One status read at a time, so concurrent previews don't stack SSH sessions.
    status: tokio::sync::Mutex<()>,
}
/// The helper's read-only report on its host.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HostStatus {
    pub state: String,
    pub message: Option<String>,
}
impl HostStatus {
    /// A hold reads the same status first and refuses any other state.
    pub fn holdable(&self) -> bool {
        matches!(self.state.as_str(), "ready" | "starts_on_launch")
    }
}
impl StrataTarget {
    /// Reads the launcher package's hold description, which runs nothing on the
    /// host, and checks the package's lock against the compiled reviewed one.
    pub async fn describe(
        config: InferenceTarget,
        launcher: &Path,
        live_lock: PathBuf,
    ) -> life::Result<Self> {
        let profile = config
            .profile
            .clone()
            .ok_or(Error::Invalid("a Strata target needs a launcher profile"))?;
        if !launcher.is_absolute() {
            return Err(Error::Invalid(
                "the Strata launcher must be an absolute path",
            ));
        }
        let program = launcher.join("run-strata.py");
        let output = run(&program, &[&profile, "--describe-hold"], DESCRIBE_DEADLINE)
            .await
            .ok_or_else(|| {
                log(format_args!(
                    "{} {profile} --describe-hold failed",
                    program.display()
                ));
                Error::Invalid("Strata launcher unavailable")
            })?;
        let description: Value = serde_json::from_slice(&output)
            .map_err(|_| Error::Invalid("Strata launcher unavailable"))?;
        let seconds = |key: &str| {
            description[key]
                .as_u64()
                .filter(|s| (1..=3600).contains(s))
                .map(Duration::from_secs)
                .ok_or(Error::Invalid("Strata launcher unavailable"))
        };
        if description["schema_version"] != 1
            || description["protocol"] != PROTOCOL
            || description["profile"] != profile.as_str()
        {
            return Err(Error::Invalid("Strata launcher unavailable"));
        }
        let lock_file = description["model_lock"]
            .as_str()
            .ok_or(Error::Invalid("Strata launcher unavailable"))?
            .to_owned();
        let lock_name = lock_file
            .strip_suffix(".lock.json")
            .filter(|name| playtime_runner::docker::strata_lock_names().any(|n| n == *name))
            .ok_or(Error::Invalid(
                "the Strata target's lock isn't a reviewed one",
            ))?
            .to_owned();
        let lock = playtime_runner::docker::strata_lock(&lock_name)
            .map_err(|_| Error::Invalid("the Strata target's lock isn't a reviewed one"))?;
        let lock_sha256 = playtime_runner::docker::strata_lock_sha256(&lock_name)
            .map_err(|_| Error::Invalid("the Strata target's lock isn't a reviewed one"))?;
        // The hold reports this digest of the lock it verified against.
        let path = launcher.join(&lock_file);
        let bytes = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(std::fs::File::open(path)?, 1 << 20),
                &mut bytes,
            )
            .map(|_| bytes)
        })
        .await
        .map_err(|_| Error::Uncertain)?
        .map_err(|_| Error::Invalid("Strata launcher unavailable"))?;
        if playtime_storage::sha256(&bytes).as_str() != lock_sha256 {
            return Err(Error::Invalid(
                "the Strata launcher's lock differs from the reviewed one",
            ));
        }
        Ok(Self {
            acquire: seconds("startup_timeout")?
                + seconds("verify_timeout")?
                + STATUS_DEADLINE
                + MARGIN,
            cleanup: seconds("cleanup_timeout")? + MARGIN,
            config,
            program,
            profile,
            lock_name,
            lock,
            lock_file,
            lock_sha256,
            live_lock,
            status: tokio::sync::Mutex::new(()),
        })
    }
    /// The host a preview freezes: everything except what a hold lends.
    pub fn template(&self) -> StrataHost {
        StrataHost {
            target: Some(Box::new(self.config.clone())),
            profile: self.profile.clone(),
            endpoint: String::new(),
            started: 0,
            verification: Value::Null,
            live_lock: self.live_lock.clone(),
        }
    }
    /// The helper's read-only status, such as `ready`, `starts_on_launch` or
    /// `busy`, and `unknown` when the launcher can't say.
    pub async fn status(&self) -> HostStatus {
        let _one = self.status.lock().await;
        let report = run(&self.program, &["--status", &self.profile], STATUS_DEADLINE)
            .await
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        match report {
            Some(report) if report["state"].is_string() => HostStatus {
                state: report["state"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(64)
                    .collect(),
                message: report["message"]
                    .as_str()
                    .map(|m| m.chars().filter(|c| !c.is_control()).take(512).collect()),
            },
            _ => HostStatus {
                state: "unknown".into(),
                message: Some("the launcher's status read failed".into()),
            },
        }
    }
    /// The served model, process and activity, if they're the lock's.
    async fn inspect(
        &self,
        endpoint: &Endpoint,
        cancel: &CancellationToken,
    ) -> Option<playtime_strata::Inspection> {
        let deadline = Instant::now() + Duration::from_secs(15);
        playtime_strata::inspect_endpoint(endpoint, &Limits::default(), deadline, cancel)
            .await
            .ok()
            .filter(|seen| seen.identity == self.lock.identity)
    }
}

/// Runs a short launcher command and returns its standard output, or `None`
/// if it fails or outlasts `deadline`. Nothing it does owns a host.
async fn run(program: &Path, args: &[&str], deadline: Duration) -> Option<Vec<u8>> {
    let child = launcher(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(deadline, async move {
        let mut child = child;
        let mut stdout = child.stdout.take()?;
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(1 << 20)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        child.wait().await.ok()?.success().then_some(bytes)
    });
    output.await.ok().flatten()
}
/// The launcher runs isolated from the daemon's Python environment, from its
/// own package directory.
fn launcher(program: &Path) -> Command {
    let mut command = Command::new(PYTHON);
    command.arg("-I").arg(program).env("PATH", LAUNCHER_PATH);
    if let Some(package) = program.parent() {
        command.current_dir(package);
    }
    command
}

/// The daemon's holds by run. A hold outlives neither its run nor the daemon:
/// when the daemon exits, the hold reads EOF and the helper stops the server.
#[derive(Default)]
pub(super) struct Holds {
    entries: Mutex<HashMap<RunId, Entry>>,
    /// Notified as each entry is removed, for `end_all`.
    removed: tokio::sync::Notify,
    /// Set under the `entries` lock once the daemon stops; no hold starts after.
    closing: AtomicBool,
}
enum Entry {
    Acquiring(Arc<StrataTarget>, CancellationToken),
    Held(Box<Hold>),
    /// Being ended or abandoned. It's still the run's until its helper exits.
    Ending(Arc<StrataTarget>),
}
impl Entry {
    fn target(&self) -> &Arc<StrataTarget> {
        match self {
            Self::Acquiring(target, _) | Self::Ending(target) => target,
            Self::Held(hold) => &hold.target,
        }
    }
}
/// Removes the run's entry when its hold has settled, or if the future that
/// was settling it is dropped, so that `end_all` never waits on it.
struct Settle<'a>(&'a Holds, &'a RunId);
impl Drop for Settle<'_> {
    fn drop(&mut self) {
        self.0.entries.lock().unwrap().remove(self.1);
        self.0.removed.notify_waiters();
    }
}
impl Holds {
    /// Holds the target's host for `run` and returns what the hold lent.
    /// Cancelling the run abandons a hold that isn't held yet, which stops
    /// the server. The hold's own messages go to `log`.
    pub async fn acquire(
        &self,
        run: &RunId,
        target: Arc<StrataTarget>,
        log_path: &Path,
    ) -> life::Result<StrataHost> {
        let cancel = CancellationToken::new();
        {
            let mut entries = self.entries.lock().unwrap();
            if self.closing.load(Ordering::SeqCst) {
                return Err(Error::Invalid("Strata hold cancelled"));
            }
            if entries.contains_key(run) {
                return Err(Error::Busy);
            }
            entries.insert(
                run.clone(),
                Entry::Acquiring(target.clone(), cancel.clone()),
            );
        }
        let settle = Settle(self, run);
        let late = match Hold::acquire(target.clone(), log_path, &cancel).await {
            Err(error) => return Err(error),
            Ok(hold) => {
                let mut entries = self.entries.lock().unwrap();
                if !cancel.is_cancelled() {
                    let host = hold.host.clone();
                    entries.insert(run.clone(), Entry::Held(Box::new(hold)));
                    std::mem::forget(settle);
                    return Ok(host);
                }
                entries.insert(run.clone(), Entry::Ending(target));
                hold
            }
        };
        // The run ended or was cancelled as the host was held.
        late.end(false).await;
        drop(settle);
        Err(Error::Invalid("Strata hold cancelled"))
    }
    /// Abandons a hold that isn't held yet. A held one ends with its run.
    pub fn cancel(&self, run: &RunId) {
        if let Some(Entry::Acquiring(_, cancel)) = self.entries.lock().unwrap().get(run) {
            cancel.cancel();
        }
    }
    /// The launcher profile of the run's hold, until its helper has exited.
    pub fn active(&self, run: &RunId) -> Option<String> {
        let entries = self.entries.lock().unwrap();
        entries.get(run).map(|entry| entry.target().profile.clone())
    }
    /// Whether any run has a hold, including one still ending.
    pub fn busy(&self) -> bool {
        !self.entries.lock().unwrap().is_empty()
    }
    /// Ends the run's hold, once its worker has stopped or can't start. Only a
    /// run that completed normally parks the warm model, and only while the
    /// same server is idle; every other ending stops the server.
    pub async fn end(&self, run: &RunId, completed: bool) {
        if let Some(hold) = self.take(run) {
            let settle = Settle(self, run);
            hold.end(completed).await;
            drop(settle);
        }
    }
    /// Takes the run's held hold, leaving it ending. An acquiring hold is
    /// cancelled instead; its acquisition abandons it.
    fn take(&self, run: &RunId) -> Option<Box<Hold>> {
        let mut entries = self.entries.lock().unwrap();
        match entries.remove(run)? {
            Entry::Held(hold) => {
                entries.insert(run.clone(), Entry::Ending(hold.target.clone()));
                Some(hold)
            }
            other => {
                if let Entry::Acquiring(_, cancel) = &other {
                    cancel.cancel();
                }
                entries.insert(run.clone(), other);
                None
            }
        }
    }
    /// Ends every hold without parking, as the daemon stops, and waits for
    /// each helper to exit, including those that runs were already ending.
    pub async fn end_all(&self) {
        let runs: Vec<_> = {
            let entries = self.entries.lock().unwrap();
            self.closing.store(true, Ordering::SeqCst);
            entries.keys().cloned().collect()
        };
        futures_util::future::join_all(runs.iter().map(|run| self.end(run, false))).await;
        // Acquiring holds abandon themselves, and other endings finish theirs.
        loop {
            let removed = self.removed.notified();
            if !self.busy() {
                return;
            }
            removed.await;
        }
    }
}

/// One running `--hold` and what it lent.
struct Hold {
    target: Arc<StrataTarget>,
    child: Child,
    /// Writing `release` parks the model; closing it stops the server.
    stdin: Option<ChildStdin>,
    events: BufReader<ChildStdout>,
    host: StrataHost,
}
impl Hold {
    async fn acquire(
        target: Arc<StrataTarget>,
        log_path: &Path,
        cancel: &CancellationToken,
    ) -> life::Result<Self> {
        let log_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log_path)?;
        // Its own process group, so a terminal's interrupt reaches only the
        // daemon, which then ends the hold through its stdin.
        let mut child = launcher(&target.program)
            .args([&target.profile, "--hold"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log_file)
            .process_group(0)
            .spawn()
            .map_err(|error| {
                log(format_args!(
                    "Strata hold for {} didn't start: {error}",
                    target.profile
                ));
                Error::Invalid("Strata launcher unavailable")
            })?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let mut hold = Self {
            host: target.template(),
            target,
            child,
            stdin,
            events: BufReader::new(stdout.ok_or(Error::Uncertain)?),
        };
        let deadline = Instant::now() + hold.target.acquire;
        let first = tokio::select! {
            event = hold.event() => event,
            () = cancel.cancelled() => {
                hold.abandon("the run was cancelled").await;
                return Err(Error::Invalid("Strata hold cancelled"));
            }
            () = tokio::time::sleep_until(deadline) => {
                hold.abandon("its acquisition deadline passed").await;
                return Err(Error::Invalid("Strata host unavailable"));
            }
        };
        match first {
            Some(event) if event["event"] == "held" => match hold.check(&event, cancel).await {
                Ok(host) => {
                    log(format_args!(
                        "Strata hold of {} lent {} (server {})",
                        hold.target.profile, host.endpoint, host.started
                    ));
                    hold.host = host;
                    Ok(hold)
                }
                Err((reason, error)) => {
                    log(format_args!(
                        "Strata hold of {} refused by the daemon: {reason}",
                        hold.target.profile
                    ));
                    hold.end(false).await;
                    Err(error)
                }
            },
            Some(event) if event["event"] == "refused" => {
                let reason = event["reason"].as_str().unwrap_or_default().to_owned();
                log(format_args!(
                    "Strata hold of {} refused: {reason}: {}; inference owner {}",
                    hold.target.profile,
                    event["message"].as_str().unwrap_or_default(),
                    event["inference_owner"].as_str().unwrap_or("unknown")
                ));
                hold.stdin = None;
                hold.reap().await;
                Err(if reason == "busy" {
                    Error::Busy
                } else {
                    Error::Invalid("Strata host unavailable")
                })
            }
            _ => {
                hold.abandon("it wrote no valid first event").await;
                Err(Error::Invalid("Strata host unavailable"))
            }
        }
    }
    /// The next valid event, or `None` at EOF or after an invalid line.
    async fn event(&mut self) -> Option<Value> {
        let mut line = Vec::new();
        (&mut self.events)
            .take(EVENT_BYTES)
            .read_until(b'\n', &mut line)
            .await
            .ok()?;
        let event: Value = serde_json::from_slice(line.strip_suffix(b"\n")?).ok()?;
        (event["schema_version"] == 1
            && event["protocol"] == PROTOCOL
            && event["profile"] == self.target.profile.as_str()
            && matches!(event["event"].as_str(), Some("held" | "refused" | "ended")))
        .then_some(event)
    }
    /// What `held` lent, if it's the target's reviewed lock on an idle server.
    async fn check(
        &self,
        held: &Value,
        cancel: &CancellationToken,
    ) -> Result<StrataHost, (&'static str, Error)> {
        let unavailable = |reason| (reason, Error::Invalid("Strata host unavailable"));
        if held["model_lock"] != self.target.lock_file.as_str()
            || held["model_lock_sha256"] != self.target.lock_sha256.as_str()
        {
            return Err(unavailable("it verified another lock"));
        }
        let host = serde_json::to_value(&self.target.lock.host).map_err(|_| unavailable("lock"))?;
        if held["report"]["host"] != host {
            return Err(unavailable("its verified host isn't the lock's"));
        }
        let endpoint = held["endpoint"].as_str().unwrap_or_default();
        let parsed =
            Endpoint::parse(endpoint).map_err(|_| unavailable("its endpoint isn't loopback"))?;
        // The server the verifier checked. A worker accepts only this process.
        let seen = self.target.inspect(&parsed, cancel).await.ok_or_else(|| {
            if cancel.is_cancelled() {
                (
                    "the run was cancelled",
                    Error::Invalid("Strata hold cancelled"),
                )
            } else {
                unavailable("its server isn't the lock's model")
            }
        })?;
        if seen.in_flight != 0 {
            return Err(("its server already has work", Error::Busy));
        }
        let started = seen
            .started()
            .ok_or_else(|| unavailable("its server reports no start time"))?;
        Ok(StrataHost {
            endpoint: endpoint.into(),
            started,
            verification: held["report"].clone(),
            ..self.target.template()
        })
    }
    /// Ends the hold. A normally completed run parks the model only while the
    /// same server is idle, so no request it may have left can still run.
    async fn end(mut self, completed: bool) {
        let release = completed && self.idle().await;
        log(format_args!(
            "{} the Strata hold of {}",
            if release { "releasing" } else { "stopping" },
            self.target.profile
        ));
        let mut stdin = self.stdin.take();
        if release && let Some(input) = stdin.as_mut() {
            let _ = input.write_all(b"release\n").await;
            let _ = input.flush().await;
        }
        // EOF changes nothing after `release`; otherwise it stops the server.
        drop(stdin);
        let deadline = Instant::now() + self.target.cleanup;
        let ended = tokio::time::timeout_at(deadline, self.event())
            .await
            .ok()
            .flatten();
        match ended {
            Some(event) if event["event"] == "ended" => log(format_args!(
                "Strata hold of {} ended: {}, released {}, inference owner {}",
                self.target.profile,
                event["ending"].as_str().unwrap_or("unknown"),
                event["released"],
                event["inference_owner"].as_str().unwrap_or("unknown")
            )),
            _ => log(format_args!(
                "Strata hold of {} wrote no ending; its inference owner is unresolved",
                self.target.profile
            )),
        }
        self.reap().await;
    }
    /// The frozen server, idle. A failed read counts as busy.
    async fn idle(&self) -> bool {
        let Ok(endpoint) = Endpoint::parse(&self.host.endpoint) else {
            return false;
        };
        self.target
            .inspect(&endpoint, &CancellationToken::new())
            .await
            .is_some_and(|seen| seen.started() == Some(self.host.started) && seen.in_flight == 0)
    }
    /// Abandons a hold that hasn't lent its host: EOF before `held` stops it.
    async fn abandon(mut self, why: &str) {
        log(format_args!(
            "abandoning the Strata hold of {}: {why}",
            self.target.profile
        ));
        self.stdin = None;
        let deadline = Instant::now() + self.target.cleanup;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.event()).await {
            if matches!(event["event"].as_str(), Some("refused" | "ended")) {
                log(format_args!(
                    "Strata hold of {} ended before use; inference owner {}",
                    self.target.profile,
                    event["inference_owner"].as_str().unwrap_or("unknown")
                ));
                break;
            }
        }
        self.reap().await;
    }
    /// Waits for the hold to exit, then signals it. A hold that still runs is
    /// left to finish its own cleanup: killing it could orphan its SSH session.
    async fn reap(mut self) {
        let deadline = Instant::now() + self.target.cleanup;
        if tokio::time::timeout_at(deadline, self.child.wait())
            .await
            .is_ok()
        {
            return;
        }
        if let Some(pid) = self
            .child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32))
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        if tokio::time::timeout(TERM_GRACE, self.child.wait())
            .await
            .is_err()
        {
            log(format_args!(
                "Strata hold of {} didn't exit; it's left to finish its cleanup",
                self.target.profile
            ));
            let mut child = self.child;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}
#[cfg(test)]
#[path = "strata_tests.rs"]
mod tests;
