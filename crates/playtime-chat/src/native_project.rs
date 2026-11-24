//! Opt-in host tools behind a fixed privileged helper and Bubblewrap policy.
use crate::{Result, runtime::Effects};
use async_trait::async_trait;
use playtime_engine::{Effect, EffectKind, Outcome, ToolRequest};
use playtime_local::PrivateDirectory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub const CAPABILITY: &str = "project-native-v1";
const HELPER: &[u8] = include_bytes!("../../../scripts/chat/native_tool.py");
pub fn helper_sha256() -> String {
    digest(HELPER)
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub command_seconds: u32,
    pub memory_bytes: u64,
    pub processes: u32,
    pub output_bytes: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub controller_uid: u32,
    pub agent_uid: u32,
    pub agent_gid: u32,
    pub workspace: PathBuf,
    pub cache: PathBuf,
    pub toolchain: PathBuf,
    pub helper: PathBuf,
    pub mode: String,
    pub limits: Limits,
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("native configuration file exceeds bound".into());
    }
    Ok(bytes)
}

fn trusted(path: &Path, directory: bool) -> Result<()> {
    if !path.is_absolute() || path.canonicalize()? != path {
        return Err("native trusted path must be canonical".into());
    }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("native policy/helper/toolchain must be root-owned and protected".into());
        }
    }
    let metadata = fs::metadata(path)?;
    if if directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    } {
        return Err("invalid native trusted path type".into());
    }
    Ok(())
}

pub struct Workspace {
    policy: Policy,
    path: PathBuf,
    policy_bytes: Vec<u8>,
    identity: Value,
    cache_identity: Value,
    git_identity: Value,
    state: Arc<PrivateDirectory>,
    _lease: File,
    active: bool,
    #[cfg(test)]
    helper_fixture: bool,
}
impl Workspace {
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn open(
        path: &Path,
        workspace: &Path,
        state: &Path,
        protected: &[PathBuf],
    ) -> Result<Self> {
        trusted(path, false)?;
        let policy_bytes = read_limited(path, 65536)?;
        if policy_bytes.len() > 65536 {
            return Err("native policy exceeds 64 KiB".into());
        }
        let policy: Policy = serde_json::from_slice(&policy_bytes)?;
        let uid = rustix::process::geteuid().as_raw();
        if policy.schema_version != 1
            || uid == 0
            || policy.controller_uid != uid
            || policy.agent_uid == uid
            || policy.agent_uid == 0
            || policy.agent_uid == u32::MAX
            || policy.agent_gid == u32::MAX
            || policy.agent_gid == 0
            || !["edit", "read_only"].contains(&policy.mode.as_str())
        {
            return Err("invalid native policy identity or mode".into());
        }
        let limits = &policy.limits;
        if !(1..=3600).contains(&limits.command_seconds)
            || !(1024..=65536).contains(&limits.output_bytes)
            || !(1024 * 1024 * 1024..=128 * 1024 * 1024 * 1024).contains(&limits.memory_bytes)
            || !(16..=4096).contains(&limits.processes)
        {
            return Err("invalid native development limits".into());
        }
        trusted(&policy.helper, false)?;
        if fs::metadata(&policy.helper)?.mode() & 0o100 == 0 {
            return Err("native helper is not executable".into());
        }
        if read_limited(&policy.helper, HELPER.len() as u64)? != HELPER {
            return Err("native helper differs from this runtime build".into());
        }
        trusted(&policy.toolchain, true)?;
        // The helper gives edit commands their network through this binary.
        if policy.mode == "edit" && trusted(Path::new("/usr/bin/slirp4netns"), false).is_err() {
            return Err("native edit policies require a root-owned /usr/bin/slirp4netns".into());
        }
        if policy.workspace != workspace.canonicalize()? {
            return Err("native policy belongs to another workspace".into());
        }
        let roots = [&policy.workspace, &policy.cache, &policy.toolchain];
        for (i, root) in roots.iter().enumerate() {
            if root.as_path() == Path::new("/")
                || root.canonicalize()? != **root
                || !root.is_dir()
                || roots
                    .iter()
                    .enumerate()
                    .any(|(j, other)| i != j && root.starts_with(other))
            {
                return Err("native roots must be canonical, disjoint directories".into());
            }
        }
        for root in [&policy.workspace, &policy.cache] {
            if fs::metadata(root)?.mode() & 0o002 != 0 {
                return Err("world-writable native root".into());
            }
        }
        if fs::metadata(&policy.cache)?.uid() != policy.agent_uid {
            return Err("native cache must belong to agent".into());
        }
        cache_provisioned(&policy)?;
        let mut protected = protected.to_vec();
        protected.extend([
            state.to_owned(),
            std::env::current_exe()?,
            path.to_owned(),
            policy.helper.clone(),
        ]);
        for protected in protected {
            let protected = protected.canonicalize()?;
            if roots.iter().any(|root| protected.starts_with(root)) {
                return Err("native mounts expose trusted state or runtime".into());
            }
        }
        let git = policy.workspace.join(".git");
        if !git.is_dir() || git.canonicalize()? != git {
            return Err("native workspace must have a standalone .git directory".into());
        }
        // Tools can read both; Git reads the second when worktree config is enabled.
        for name in ["config", "config.worktree"] {
            let path = git.join(name);
            if path.exists() {
                let text = String::from_utf8_lossy(&read_limited(&path, 1024 * 1024)?).into_owned();
                if let Some(reason) = git_config_error(&text) {
                    return Err(format!("native workspace refused: {reason}").into());
                }
            }
        }
        ptrace_contained()?;
        agent_account(
            &policy,
            &String::from_utf8_lossy(&read_limited(Path::new("/etc/passwd"), 4 << 20)?),
        )?;
        let git = fs::metadata(git)?;
        let git_identity = json!({"device":git.dev(),"inode":git.ino()});
        let metadata = fs::metadata(workspace)?;
        let identity = json!({"device":metadata.dev(),"inode":metadata.ino()});
        // Share the Docker project's device/inode lease and active fence name.
        // Never interpret or clear an existing Docker or uncertain native fence.
        let directory = PrivateDirectory::open(
            &state.join(format!("project-{}-{}", metadata.dev(), metadata.ino())),
            true,
        )?;
        let lease = directory.try_lock()?;
        if directory.names()?.iter().any(|name| name == "active") {
            return Err("project has unresolved owned tools; no automatic recovery".into());
        }
        let cache = fs::metadata(&policy.cache)?;
        let cache_identity = json!({"device":cache.dev(),"inode":cache.ino()});
        Ok(Self {
            policy,
            path: path.into(),
            policy_bytes,
            identity,
            cache_identity,
            git_identity,
            state: Arc::new(directory),
            _lease: lease,
            active: false,
            #[cfg(test)]
            helper_fixture: false,
        })
    }
    pub fn binding(&self) -> Value {
        json!({"capability":CAPABILITY,"policy_path":self.path,"policy_sha256":digest(&self.policy_bytes),
            "policy":self.policy,"workspace_identity":self.identity,"cache_identity":self.cache_identity,"git_identity":self.git_identity,"helper_sha256":helper_sha256()})
    }
    pub async fn tool(
        &mut self,
        request: &ToolRequest,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if self.active {
            return Err("prior native command has unresolved cleanup".into());
        }
        if self.policy.mode == "read_only" && request.name == "write_file" {
            return Ok(json!({"error":"write_file denied by read-only policy"}));
        }
        if let Some(error) = argument_error(&request.arguments) {
            return Ok(json!({ "error": error }));
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_secs(1));
        let timeout_ms = remaining
            .as_millis()
            .min(u128::from(self.policy.limits.command_seconds) * 1000);
        if timeout_ms == 0 || cancel.is_cancelled() {
            return Err("native command cancelled before dispatch".into());
        }
        let request = json!({"schema_version":1,"policy_sha256":digest(&self.policy_bytes),"helper_sha256":helper_sha256(),"workspace_identity":self.identity,"cache_identity":self.cache_identity,"git_identity":self.git_identity,
            "tool":request.name,"arguments":request.arguments,"timeout_ms":timeout_ms as u64,"output_bytes":self.policy.limits.output_bytes});
        let mut input = serde_json::to_vec(&request)?;
        input.push(b'\n');
        if input.len() > 262144 {
            return Ok(json!({"error":"native tool request exceeds 256 KiB"}));
        }
        let marker = serde_json::to_vec(
            &json!({"capability":CAPABILITY,"binding":self.binding(),"request":request}),
        )?;
        let state = self.state.clone();
        let path = self.path.clone();
        let policy = self.policy.clone();
        let policy_bytes = self.policy_bytes.clone();
        let identity = self.identity.clone();
        let cache_identity = self.cache_identity.clone();
        let git_identity = self.git_identity.clone();
        #[cfg(test)]
        let fixture = self.helper_fixture;
        tokio::task::spawn_blocking(move || -> Result<()> {
            #[cfg(not(test))]
            verify_binding(
                &path,
                &policy,
                &policy_bytes,
                &identity,
                &cache_identity,
                &git_identity,
            )?;
            #[cfg(test)]
            if !fixture {
                verify_binding(
                    &path,
                    &policy,
                    &policy_bytes,
                    &identity,
                    &cache_identity,
                    &git_identity,
                )?;
            }
            state.write_new("active", &marker)?;
            Ok(())
        })
        .await??;
        self.active = true;
        let result = if cancel.is_cancelled() || Instant::now() >= deadline {
            // No dispatch occurred after the durable intent; it is safe to release.
            let state = self.state.clone();
            tokio::task::spawn_blocking(move || state.remove("active")).await??;
            self.active = false;
            return Err("native command cancelled before dispatch".into());
        } else {
            self.invoke(input, deadline, cancel).await
        };
        if result.is_ok() {
            let state = self.state.clone();
            tokio::task::spawn_blocking(move || state.remove("active")).await??;
            self.active = false;
        }
        result
    }
    async fn invoke(
        &self,
        input: Vec<u8>,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let mut command = tokio::process::Command::new("/usr/bin/sudo");
        command
            .args(["-n", "--"])
            .arg(&self.policy.helper)
            .arg("--policy")
            .arg(&self.path)
            .current_dir("/")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(test)]
        if self.helper_fixture {
            command = tokio::process::Command::new("/usr/bin/python3");
            command
                .args(["-I", "-S", "-c", FIXTURE_HELPER])
                .arg(env!("CARGO_MANIFEST_DIR"))
                .arg(serde_json::to_string(&self.policy)?)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
        }
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or("native helper stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("native helper stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("native helper stderr unavailable")?;
        let output = tokio::spawn(read_bounded(
            stdout,
            12 * self.policy.limits.output_bytes as usize + 16384,
        ));
        let errors = tokio::spawn(read_bounded(stderr, 16384));
        let write = tokio::select! {
            result = stdin.write_all(&input) => result,
            _ = cancel.cancelled() => Err(std::io::Error::other("native startup cancelled")),
            _ = tokio::time::sleep_until(deadline) => Err(std::io::Error::other("native startup deadline")),
        };
        if write.is_err() {
            drop(stdin);
        } else {
            tokio::select! {
                _ = cancel.cancelled() => (),
                _ = tokio::time::sleep_until(deadline) => (),
                result = child.wait() => { result?; },
            }
            // EOF is the helper's cancellation/lost-controller signal.
            drop(stdin);
        }
        let status = tokio::time::timeout(Duration::from_secs(7), child.wait()).await??;
        let stdout = tokio::time::timeout(Duration::from_secs(1), output).await???;
        let stderr = tokio::time::timeout(Duration::from_secs(1), errors).await???;
        if !status.success() {
            return Err(
                format!("native helper failed: {}", String::from_utf8_lossy(&stderr)).into(),
            );
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reply {
            schema_version: u32,
            cleanup: String,
            result: Value,
        }
        let reply: Reply = serde_json::from_slice(&stdout)?;
        if reply.schema_version != 1 || reply.cleanup != "complete" || !reply.result.is_object() {
            return Err("native helper did not confirm cleanup".into());
        }
        Ok(reply.result)
    }
    pub fn cleanup(&self) -> Vec<String> {
        if self.active {
            vec!["native tool cleanup remains uncertain; private active fence retained".into()]
        } else {
            vec![]
        }
    }
}

/// Model-chosen values the helper would refuse. Refusing them here dispatches
/// nothing, so the model gets an error result and no fence is written.
fn argument_error(arguments: &Value) -> Option<&'static str> {
    if ["command", "cwd", "path"].iter().any(|field| {
        arguments[field]
            .as_str()
            .is_some_and(|text| text.contains('\0'))
    }) {
        return Some("tool text arguments cannot contain NUL characters");
    }
    if arguments
        .get("offset_bytes")
        .is_some_and(|offset| offset.as_u64().is_none())
    {
        return Some("offset_bytes must be a non-negative integer");
    }
    if arguments.get("timeout_seconds").is_some_and(|seconds| {
        !seconds
            .as_f64()
            .is_some_and(|seconds| seconds.is_finite() && seconds > 0.0)
    }) {
        return Some("timeout_seconds must be a positive number");
    }
    None
}

/// Tools can read the Git config and reach the network, so credentials there
/// could leave with them. A line check for the usual places, not a parser:
/// whole-line comments are skipped and nothing is unquoted or joined.
fn git_config_error(config: &str) -> Option<&'static str> {
    for line in config.lines() {
        let line = line.trim().to_ascii_lowercase();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with("[include") {
            return Some("its Git config includes other files");
        }
        // A helper can be an inline script that prints a password.
        if line.starts_with("[credential") {
            return Some("its Git config configures credentials");
        }
        if line
            .split_once('=')
            .is_some_and(|(key, _)| key.trim() == "extraheader")
        {
            return Some("its Git config sets HTTP headers, which can carry credentials");
        }
        let mut rest = line.as_str();
        while let Some(index) = rest.find("://") {
            let scheme = rest[..index]
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))
                .next()
                .unwrap_or("");
            let authority = rest[index + 3..]
                .split(|c: char| c == '/' || c == '"' || c.is_whitespace())
                .next()
                .unwrap_or("");
            // An SSH login name is not a secret; any HTTP user or password may be.
            if authority
                .rsplit_once('@')
                .is_some_and(|(user, _)| scheme.starts_with("http") || user.contains(':'))
            {
                return Some("a Git remote URL carries credentials");
            }
            rest = &rest[index + 3..];
        }
    }
    None
}

/// Every tool runs as the same agent UID, so none may attach to a process it
/// did not start, such as the sandbox's own init.
fn ptrace_contained() -> Result<()> {
    let scope = fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_default();
    if scope.trim().parse::<u8>().is_ok_and(|scope| scope >= 1) {
        Ok(())
    } else {
        Err("native tools require kernel.yama.ptrace_scope of 1 or higher".into())
    }
}

const NOLOGIN_SHELLS: [&str; 4] = [
    "/usr/sbin/nologin",
    "/sbin/nologin",
    "/usr/bin/false",
    "/bin/false",
];

/// A tool must not be able to start processes outside its sandbox by logging
/// in. The helper also checks the locked password, which only root can read.
fn agent_account(policy: &Policy, passwd: &str) -> Result<()> {
    let uid = policy.agent_uid.to_string();
    let entries: Vec<Vec<&str>> = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .filter(|entry| entry.len() == 7 && entry[2] == uid)
        .collect();
    let [entry] = entries.as_slice() else {
        return Err("native agent account needs exactly one local passwd entry".into());
    };
    if !NOLOGIN_SHELLS.contains(&entry[6]) {
        return Err("native agent account must not have a login shell".into());
    }
    let home = Path::new(entry[5]);
    for path in [resolved(home), resolved(&home.join(".ssh"))] {
        if [&policy.workspace, &policy.cache, &policy.toolchain]
            .iter()
            .any(|root| path.starts_with(root))
        {
            return Err("native agent home and SSH keys must lie outside the tool mounts".into());
        }
    }
    Ok(())
}

/// Like realpath: links in the existing part are resolved, a missing tail is kept.
fn resolved(path: &Path) -> PathBuf {
    match (path.canonicalize(), path.parent(), path.file_name()) {
        (Ok(path), _, _) => path,
        (Err(_), Some(parent), Some(name)) => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

async fn read_bounded(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0; 8192];
    let mut overflow = false;
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let room = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..n.min(room)]);
        overflow |= n > room;
    }
    if overflow {
        return Err("native helper output exceeded bound".into());
    }
    Ok(retained)
}

pub struct ProjectEffects<E> {
    pub inner: E,
    pub workspace: Workspace,
}
#[async_trait]
impl<E: Effects> Effects for ProjectEffects<E> {
    async fn execute(
        &mut self,
        effect: &Effect,
        deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<Value>,
    ) -> Outcome {
        match &effect.kind {
            EffectKind::StartWorkspace => {
                let probe = ToolRequest {
                    name: "exec".into(),
                    arguments: json!({"command":"/usr/bin/id -u; /usr/bin/id -g; /usr/bin/id -G"}),
                };
                match self.workspace.tool(&probe, deadline, cancel).await {
                    Ok(value)
                        if value["exit_code"] == 0
                            && value["stdout"]
                                == format!(
                                    "{}\n{}\n{}\n",
                                    self.workspace.policy.agent_uid,
                                    self.workspace.policy.agent_gid,
                                    self.workspace.policy.agent_gid
                                ) =>
                    {
                        Outcome::WorkspaceStarted
                    }
                    Ok(_) => Outcome::Failed("native account/sandbox probe failed".into()),
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }
            EffectKind::Tool(request) => match self.workspace.tool(request, deadline, cancel).await
            {
                Ok(value) => Outcome::Tool(value),
                Err(error) => Outcome::Failed(error.to_string()),
            },
            EffectKind::Model(_) => self.inner.execute(effect, deadline, cancel, chunks).await,
            EffectKind::Cleanup => Outcome::Cleaned(self.cleanup(deadline).await),
            _ => Outcome::Failed("effect unsupported by native project tools".into()),
        }
    }
    async fn cleanup(&mut self, deadline: Instant) -> Vec<String> {
        let mut residuals = self.workspace.cleanup();
        residuals.extend(self.inner.cleanup(deadline).await);
        residuals
    }
}

/// Each command's HOME and CARGO_HOME are private, so only Cargo's downloads
/// (`cargo/git`, `cargo/registry`) and the edit TMPDIR live in the shared cache.
fn cache_provisioned(policy: &Policy) -> Result<()> {
    for name in ["cargo", "cargo/git", "cargo/registry", "tmp"] {
        let directory = policy.cache.join(name);
        let owned = directory.canonicalize().is_ok_and(|path| path == directory)
            && fs::metadata(&directory)
                .is_ok_and(|info| info.is_dir() && info.uid() == policy.agent_uid);
        if !owned {
            return Err(
                "native cache needs agent-owned cargo/, cargo/git/, cargo/registry/ and tmp/ directories"
                    .into(),
            );
        }
    }
    Ok(())
}

fn verify_binding(
    path: &Path,
    policy: &Policy,
    policy_bytes: &[u8],
    identity: &Value,
    cache_identity: &Value,
    git_identity: &Value,
) -> Result<()> {
    trusted(path, false)?;
    trusted(&policy.helper, false)?;
    if fs::metadata(&policy.helper)?.mode() & 0o100 == 0 {
        return Err("native helper is not executable".into());
    }
    if read_limited(path, 65536)? != policy_bytes
        || read_limited(&policy.helper, HELPER.len() as u64)? != HELPER
    {
        return Err("native policy/helper changed after admission".into());
    }
    let info = fs::symlink_metadata(&policy.workspace)?;
    if !info.is_dir()
        || policy.workspace.canonicalize()? != policy.workspace
        || *identity != json!({"device":info.dev(),"inode":info.ino()})
    {
        return Err("native workspace changed after admission".into());
    }
    let cache = fs::symlink_metadata(&policy.cache)?;
    if !cache.is_dir() || *cache_identity != json!({"device":cache.dev(),"inode":cache.ino()}) {
        return Err("native cache changed after admission".into());
    }
    let git = fs::symlink_metadata(policy.workspace.join(".git"))?;
    if !git.is_dir() || *git_identity != json!({"device":git.dev(),"inode":git.ino()}) {
        return Err("native Git directory changed after admission".into());
    }
    Ok(())
}

// This runner exists only in the unit-test binary, never in an installed worker.
// It qualifies namespaces/process handling, not sudo or dedicated-account access.
#[cfg(test)]
const FIXTURE_HELPER: &str = r#"
import importlib.util, json, pathlib, sys
source = pathlib.Path(sys.argv[1]).parent.parent / 'scripts/chat/native_tool.py'
spec = importlib.util.spec_from_file_location('native_tool', source)
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)
request = native.read_request(0)
# This unprivileged fixture cannot clear host supplementary groups. Substitute
# only the group probe; dedicated-account launch/identity is a separate gate.
if request['tool'] == 'exec' and request['arguments']['command'] == '/usr/bin/id -u; /usr/bin/id -g; /usr/bin/id -G':
    request['arguments']['command'] = '/usr/bin/id -u; /usr/bin/id -g; /usr/bin/id -g'
print(json.dumps(native.run_tool(json.loads(sys.argv[2]), request, 0, drop_identity=False)), flush=True)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime, worker};
    use playtime_core::StopReason;
    use playtime_engine::{ModelResponse, ToolPolicy};
    use playtime_harness::Session;
    use playtime_local::Token;
    use tokio::sync::Mutex;

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "playtime-native-rust-{}",
                Token::generate().unwrap().expose()
            ));
            PrivateDirectory::open(&root, true).unwrap();
            for name in [
                "workspace/src",
                "cache/cargo/git",
                "cache/cargo/registry",
                "cache/tmp",
            ] {
                fs::create_dir_all(root.join(name)).unwrap();
            }
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", &root)
                    .args([
                        "-c",
                        "init.templateDir=",
                        "init",
                        "--quiet",
                        "--initial-branch=main"
                    ])
                    .arg(root.join("workspace"))
                    .status()
                    .unwrap()
                    .success()
            );
            Self(root)
        }
        fn workspace(&self) -> Workspace {
            let root = self.0.join("workspace");
            let cache = self.0.join("cache");
            let toolchain = std::process::Command::new("rustc")
                .args(["--print", "sysroot"])
                .output()
                .unwrap();
            assert!(toolchain.status.success());
            let policy = Policy {
                schema_version: 1,
                controller_uid: rustix::process::geteuid().as_raw(),
                agent_uid: rustix::process::geteuid().as_raw(),
                agent_gid: rustix::process::getegid().as_raw(),
                workspace: root.clone(),
                cache: cache.clone(),
                toolchain: PathBuf::from(String::from_utf8(toolchain.stdout).unwrap().trim()),
                helper: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../scripts/chat/native_tool.py"),
                mode: "edit".into(),
                limits: Limits {
                    command_seconds: 30,
                    memory_bytes: 16 * 1024 * 1024 * 1024,
                    processes: 512,
                    output_bytes: 4096,
                },
            };
            let info = fs::metadata(&root).unwrap();
            let cache = fs::metadata(cache).unwrap();
            let git = fs::metadata(root.join(".git")).unwrap();
            let state = PrivateDirectory::open(&self.0.join("state"), true).unwrap();
            let lease = state.try_lock().unwrap();
            Workspace {
                policy_bytes: serde_json::to_vec(&policy).unwrap(),
                policy,
                path: self.0.join("fixture-policy.json"),
                identity: json!({"device":info.dev(),"inode":info.ino()}),
                cache_identity: json!({"device":cache.dev(),"inode":cache.ino()}),
                git_identity: json!({"device":git.dev(),"inode":git.ino()}),
                state: Arc::new(state),
                _lease: lease,
                active: false,
                helper_fixture: true,
            }
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct ScriptedModel(usize);
    #[async_trait]
    impl Effects for ScriptedModel {
        async fn execute(
            &mut self,
            effect: &Effect,
            _: Instant,
            _: CancellationToken,
            _: mpsc::Sender<Value>,
        ) -> Outcome {
            let EffectKind::Model(request) = &effect.kind else {
                panic!("non-model effect reached fixture")
            };
            self.0 += 1;
            let message = if self.0 == 1 {
                json!({"role":"assistant","content":"","tool_calls":[
                    {"id":"manifest","function":{"name":"write_file","arguments":{"path":"Cargo.toml","content":"[package]\nname=\"native-driver-fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n"}}},
                    {"id":"source","function":{"name":"write_file","arguments":{"path":"src/lib.rs","content":"#[test] fn checked() { assert_eq!(2 + 2, 4); }\n"}}},
                    {"id":"check","function":{"name":"exec","arguments":{"command":"cargo test --offline"}}}
                ]})
            } else {
                let tool = request
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "tool")
                    .unwrap();
                let result: Value =
                    serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["exit_code"], 0, "{result}");
                let report = json!({"status":"completed","summary":"Rust test passed","changes":["src/lib.rs"],"verification":[{"evidence_ref":result["evidence_log_path"]}],"remaining_work":[],"questions":[],"blockers":[]});
                json!({"role":"assistant","content":report.to_string()})
            };
            Outcome::ModelCompleted {
                response: ModelResponse {
                    message,
                    done: true,
                    generated_tokens: Some(50),
                    prompt_tokens: Some(200),
                },
                finish_reason: "stop".into(),
            }
        }
        async fn cleanup(&mut self, _: Instant) -> Vec<String> {
            vec![]
        }
    }

    #[tokio::test]
    #[ignore = "explicit native namespace gate; does not qualify a dedicated account"]
    async fn native_driver_edits_builds_reports_and_continues() {
        let root = Root::new();
        let mut workspace = root.workspace();
        let lock: playtime_strata::ModelLock = serde_json::from_str(include_str!(
            "../../../models/framework-strata-gyro-m-fast-v1.lock.json"
        ))
        .unwrap();
        let mut config = worker::native_config(&lock, &workspace, 60).unwrap();
        assert!(config.instructions.contains("task dependency downloads"));
        assert!(
            !config
                .instructions
                .contains("Project paths are inside /work")
        );
        config.mode = playtime_core::RunMode::Simulation;
        config.execution.tool_policy = ToolPolicy::Simulation;
        workspace.policy.mode = "read_only".into();
        let mut altered = worker::native_config(&lock, &workspace, 60).unwrap();
        altered.mode = playtime_core::RunMode::Simulation;
        altered.execution.tool_policy = ToolPolicy::Simulation;
        assert_ne!(
            config.fingerprint().unwrap(),
            altered.fingerprint().unwrap()
        );
        drop(workspace);
        let path = root.0.join("session");
        let mut previous_reference = None;
        for turn in 0..2 {
            let session = if turn == 0 {
                Session::create(&path, config.clone()).unwrap()
            } else {
                Session::open(&path, &config).unwrap()
            };
            let run = root.0.join(format!("run-{turn}"));
            let records = PrivateDirectory::open(&run, true).unwrap();
            let artifacts = PrivateDirectory::open(&run.join("artifacts"), true).unwrap();
            let effects = Arc::new(Mutex::new(
                worker::EvidenceEffects::new(
                    ProjectEffects {
                        inner: ScriptedModel(0),
                        workspace: root.workspace(),
                    },
                    run.join("artifacts"),
                    1024 * 1024,
                )
                .unwrap(),
            ));
            let (tx, mut rx) = runtime::events();
            let drain = tokio::spawn(async move {
                let mut events = Vec::new();
                while let Some(event) = rx.recv().await {
                    events.push(event);
                }
                events
            });
            let done = runtime::run_turn(
                session,
                "Implement and test".into(),
                effects.clone(),
                tx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let events = drain.await.unwrap();
            assert_eq!(done.result.reason, StopReason::AgentFinished, "{events:?}");
            let session = done.session.unwrap();
            let response = session
                .checkpoint()
                .unwrap()
                .history
                .exchanges
                .last()
                .unwrap()
                .messages
                .last()
                .unwrap()["content"]
                .as_str()
                .unwrap();
            let effects = effects.lock().await;
            let (report, digest) =
                worker::save_report(response, &records, &artifacts, effects.evidence()).unwrap();
            assert_eq!(report.verification[0].command, "cargo test --offline");
            assert_ne!(
                previous_reference,
                Some(report.verification[0].log_path.clone())
            );
            previous_reference = Some(report.verification[0].log_path.clone());
            assert_eq!(
                digest,
                super::digest(
                    &records
                        .read("worker-report.json", worker::MAX_REPORT)
                        .unwrap()
                )
            );
            assert!(effects.inner.workspace.cleanup().is_empty());
            assert!(!root.0.join("state/active").exists());
            drop(session);
            assert!(Session::open(&path, &altered).is_err());
        }
        assert!(root.0.join("workspace/target").is_dir());
    }

    #[tokio::test]
    #[ignore = "explicit native namespace gate; does not qualify a dedicated account"]
    async fn native_control_eof_cancels_and_missing_receipt_fences() {
        let root = Root::new();
        let mut workspace = root.workspace();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            trigger.cancel();
        });
        let request = ToolRequest {
            name: "exec".into(),
            arguments: json!({"command":"setsid sh -c 'sleep .5; touch escaped' & wait"}),
        };
        let result = workspace
            .tool(&request, Instant::now() + Duration::from_secs(10), cancel)
            .await
            .unwrap();
        assert_eq!(result["error"], "native tool cancelled");
        assert!(workspace.cleanup().is_empty());
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!root.0.join("workspace/escaped").exists());
        workspace.policy.limits.output_bytes = 0;
        assert!(
            workspace
                .tool(
                    &request,
                    Instant::now() + Duration::from_secs(10),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert!(!workspace.cleanup().is_empty());
        assert!(root.0.join("state/active").exists());
        assert!(
            workspace
                .tool(
                    &request,
                    Instant::now() + Duration::from_secs(10),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "explicit native namespace gate; does not qualify a dedicated account"]
    async fn maximum_output_remains_bounded_after_json_escaping() {
        let root = Root::new();
        let mut workspace = root.workspace();
        workspace.policy.limits.output_bytes = 65536;
        let request = ToolRequest {
            name: "exec".into(),
            arguments: json!({"command":"head -c 100000 /dev/zero; head -c 100000 /dev/zero >&2"}),
        };
        let result = workspace
            .tool(
                &request,
                Instant::now() + Duration::from_secs(10),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"].as_str().unwrap().len(), 65536);
        assert_eq!(result["stderr"].as_str().unwrap().len(), 65536);
        assert_eq!(result["output_truncated"], true);
        assert!(workspace.cleanup().is_empty());
    }

    #[tokio::test]
    async fn model_argument_refusals_dispatch_nothing_and_leave_no_fence() {
        let root = Root::new();
        let mut workspace = root.workspace();
        for (name, arguments) in [
            ("read_file", json!({"path":"x","offset_bytes":-1})),
            (
                "exec",
                json!({"command":"touch should-not-run","timeout_seconds":0}),
            ),
            (
                "exec",
                json!({"command":"touch should-not-run","timeout_seconds":-0.5}),
            ),
            ("exec", json!({"command":"touch should-not-run\u{0}"})),
            (
                "exec",
                json!({"command":"touch should-not-run","cwd":"src\u{0}"}),
            ),
            ("write_file", json!({"path":"x\u{0}","content":"x"})),
            (
                "write_file",
                json!({"path":"should-not-run","content":"x".repeat(300_000)}),
            ),
        ] {
            let request = ToolRequest {
                name: name.into(),
                arguments,
            };
            let result = workspace
                .tool(
                    &request,
                    Instant::now() + Duration::from_secs(10),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(result["error"].is_string(), "{result}");
            assert!(workspace.cleanup().is_empty());
            assert!(!root.0.join("state/active").exists());
        }
        assert!(!root.0.join("workspace/should-not-run").exists());
    }

    #[test]
    fn git_config_credentials_are_refused_but_ssh_logins_are_not() {
        for clean in [
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = ssh://git@github.com/o/r.git\n",
            "[remote \"origin\"]\n\turl = git@github.com:o/r.git\n\tpushurl = https://github.com/o/r\n",
            "# url = https://user:token@example.com/\n",
        ] {
            assert_eq!(git_config_error(clean), None, "{clean}");
        }
        for leaking in [
            "[remote \"origin\"]\n\turl = https://x-access-token:secret@github.com/o/r\n",
            "[remote \"origin\"]\n\turl = https://ghp_secret@github.com/o/r\n",
            "[remote \"origin\"]\n\tpushUrl = ssh://user:secret@host/r\n",
            "[url \"https://user:secret@github.com/\"]\n\tinsteadOf = https://github.com/\n",
            "[http]\n\textraHeader = AUTHORIZATION: basic c2VjcmV0\n",
            "[http \"https://github.com/\"]\n\textraheader=Authorization: bearer x\n",
            "[include]\n\tpath = secrets\n",
            "[includeIf \"gitdir:/w/\"]\n\tpath = secrets\n",
            "[credential]\n\thelper = \"!f() { echo password=secret; }; f\"\n",
            "[credential \"https://github.com\"]\n\tusername = x\n",
        ] {
            assert!(git_config_error(leaking).is_some(), "{leaking}");
        }
    }

    #[test]
    fn agent_account_cannot_log_in_or_expose_its_keys() {
        let root = Root::new();
        let workspace = root.workspace();
        let policy = workspace.policy();
        let account = |home: &Path, shell: &str| {
            agent_account(
                policy,
                &format!(
                    "root:x:0:0::/root:/bin/bash\nagent:x:{}:{}::{}:{shell}\n",
                    policy.agent_uid,
                    policy.agent_gid,
                    home.display()
                ),
            )
        };
        let outside = root.0.join("agent-home");
        account(&outside, "/usr/sbin/nologin").unwrap();
        account(&outside, "/bin/false").unwrap();
        assert!(account(&outside, "/bin/bash").is_err());
        assert!(account(&policy.cache.join("home"), "/usr/sbin/nologin").is_err());
        assert!(account(&policy.workspace, "/usr/sbin/nologin").is_err());
        assert!(agent_account(policy, "root:x:0:0::/root:/bin/bash\n").is_err());
    }

    #[test]
    fn native_tools_are_described_for_the_sandbox_and_bind_no_host_cpu_count() {
        let root = Root::new();
        let workspace = root.workspace();
        let lock = serde_json::from_str(include_str!(
            "../../../models/framework-strata-gyro-m-fast-v1.lock.json"
        ))
        .unwrap();
        let config = worker::native_config(&lock, &workspace, 60).unwrap();
        assert_eq!(
            config.execution.tool_profile,
            playtime_engine::execution::ToolProfile::NativeV1
        );
        assert!((config.execution.workshop.as_ref().unwrap().cpu_cores - 1.0).abs() < f64::EPSILON);
        let tools = serde_json::to_string(&config.tools()).unwrap();
        assert!(tools.contains("project checkout") && !tools.contains("/work"));
    }

    #[test]
    fn cache_must_provide_the_shared_cargo_downloads_and_temporary_directory() {
        let root = Root::new();
        let workspace = root.workspace();
        let policy = workspace.policy();
        cache_provisioned(policy).unwrap();
        let registry = policy.cache.join("cargo/registry");
        fs::remove_dir(&registry).unwrap();
        assert!(cache_provisioned(policy).is_err());
        std::os::unix::fs::symlink(policy.cache.join("tmp"), &registry).unwrap();
        assert!(cache_provisioned(policy).is_err());
    }

    #[test]
    fn untrusted_policy_is_rejected_without_any_tool_dispatch() {
        let root = Root::new();
        let path = root.0.join("policy.json");
        fs::write(&path, b"{}").unwrap();
        assert!(
            Workspace::open(&path, &root.0.join("workspace"), &root.0.join("state"), &[]).is_err()
        );
        assert!(!root.0.join("state/active").exists());
    }
    struct ReadonlyModel {
        calls: usize,
        malformed: bool,
        cancel: bool,
        cache: PathBuf,
    }
    #[async_trait]
    impl Effects for ReadonlyModel {
        async fn execute(
            &mut self,
            effect: &Effect,
            _: Instant,
            _: CancellationToken,
            _: mpsc::Sender<Value>,
        ) -> Outcome {
            let EffectKind::Model(request) = &effect.kind else {
                panic!("model fixture only")
            };
            self.calls += 1;
            let message = if self.calls == 1 {
                assert!(
                    !request
                        .messages
                        .iter()
                        .any(|m| m["role"] == "assistant" || m["role"] == "tool"),
                    "every invocation starts fresh"
                );
                let command = if self.cancel {
                    "sleep 30".to_string()
                } else {
                    format!(
                        "touch forbidden; touch .git/forbidden; touch {}/forbidden",
                        self.cache.display()
                    )
                };
                json!({"role":"assistant","content":"","tool_calls":[
                    {"id":"deny","function":{"name":"write_file","arguments":{"path":"source.txt","content":"overwritten"}}},
                    {"id":"inspect","function":{"name":"read_file","arguments":{"path":"source.txt"}}},
                    {"id":"boundary","function":{"name":"exec","arguments":{"command":command}}}
                ]})
            } else {
                let tools: Vec<Value> = request
                    .messages
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
                    .collect();
                assert_eq!(tools.len(), 3);
                assert!(tools[0]["error"].as_str().unwrap().contains("denied"));
                assert!(tools[1].to_string().contains("original source"));
                assert_ne!(tools[2]["exit_code"], 0);
                for tool in &tools {
                    assert!(tool["evidence_log_path"].is_string());
                }
                json!({"role":"assistant","content":if self.malformed {"not JSON"} else {r#"{"decision":"ready"}"#}})
            };
            Outcome::ModelCompleted {
                response: ModelResponse {
                    message,
                    done: true,
                    generated_tokens: Some(50),
                    prompt_tokens: Some(200),
                },
                finish_reason: "stop".into(),
            }
        }
        async fn cleanup(&mut self, _: Instant) -> Vec<String> {
            vec![]
        }
    }

    #[tokio::test]
    #[ignore = "explicit native namespace gate; does not qualify a dedicated account"]
    async fn readonly_roles_inspect_validate_retain_failures_and_cancel() {
        use crate::readonly::{self, Contract, Role};
        let root = Root::new();
        fs::write(root.0.join("workspace/source.txt"), "original source").unwrap();
        let lock = serde_json::from_str(include_str!(
            "../../../models/framework-strata-gyro-m-fast-v1.lock.json"
        ))
        .unwrap();
        let bytes = br#"{"type":"object","properties":{"decision":{"type":"string","enum":["ready","blocked"]}},"required":["decision"],"additionalProperties":false}"#.to_vec();
        let contract = Contract::parse(bytes.clone(), &readonly::digest(&bytes)).unwrap();
        for (index, role) in [
            Role::Setup,
            Role::Supervisor,
            Role::Reviewer,
            Role::Reviewer,
        ]
        .into_iter()
        .enumerate()
        {
            let mut workspace = root.workspace();
            assert!(readonly::config(&lock, &workspace, 60, role, &contract).is_err());
            workspace.policy.mode = "read_only".into();
            let mut config = readonly::config(&lock, &workspace, 60, role, &contract).unwrap();
            config.mode = playtime_core::RunMode::Simulation;
            config.execution.tool_policy = ToolPolicy::Simulation;
            let session_path = root.0.join(format!("readonly-session-{index}"));
            let session = Session::create(&session_path, config.clone()).unwrap();
            let records =
                PrivateDirectory::open(&root.0.join(format!("readonly-run-{index}")), true)
                    .unwrap();
            let artifacts_path = root.0.join(format!("readonly-artifacts-{index}"));
            let artifacts = PrivateDirectory::open(&artifacts_path, true).unwrap();
            let effects = Arc::new(Mutex::new(
                worker::EvidenceEffects::new(
                    ProjectEffects {
                        inner: ReadonlyModel {
                            calls: 0,
                            malformed: index == 2,
                            cancel: index == 3,
                            cache: workspace.policy.cache.clone(),
                        },
                        workspace,
                    },
                    artifacts_path,
                    1024 * 1024,
                )
                .unwrap()
                .with_all_tools(),
            ));
            let cancel = CancellationToken::new();
            let (tx, mut rx) = runtime::events();
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
            if index == 3 {
                let trigger = cancel.clone();
                let active = root.0.join("state/active");
                tokio::spawn(async move {
                    // Wait for the command itself, after the startup and read probes.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        if fs::read_to_string(&active).is_ok_and(|s| s.contains("sleep 30")) {
                            break;
                        }
                        assert!(Instant::now() < deadline);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    trigger.cancel();
                });
            }
            let done = runtime::run_turn(
                session,
                "Inspect source using supplied evidence only".into(),
                effects.clone(),
                tx,
                cancel,
            )
            .await
            .unwrap();
            drain.await.unwrap();
            assert_eq!(
                done.result.cleanup,
                playtime_protocol::events::CleanupOutcome::Complete
            );
            assert!(effects.lock().await.inner.workspace.cleanup().is_empty());
            assert!(!root.0.join("state/active").exists());
            if index == 3 {
                assert_eq!(done.result.reason, StopReason::Interrupted);
                assert!(
                    !root
                        .0
                        .join(format!("readonly-run-{index}/response.json"))
                        .exists()
                );
            } else {
                assert_eq!(done.result.reason, StopReason::AgentFinished);
                let session = done.session.unwrap();
                let response = session
                    .checkpoint()
                    .unwrap()
                    .history
                    .exchanges
                    .last()
                    .unwrap()
                    .messages
                    .last()
                    .unwrap()["content"]
                    .as_str()
                    .unwrap();
                let saved =
                    readonly::save_response(response, &records, "response.raw.txt", &contract)
                        .unwrap();
                assert_eq!(saved.is_err(), index == 2);
                assert_eq!(
                    records.read("response.raw.txt", 1024).unwrap(),
                    response.as_bytes()
                );
                if let Ok(hash) = saved {
                    assert_eq!(
                        hash,
                        readonly::digest(&records.read("response.json", 1024).unwrap())
                    );
                }
                assert_eq!(artifacts.names().unwrap().len(), 3);
                drop(session);
                let mut different_role = readonly::configure(
                    config.clone(),
                    if role == Role::Setup {
                        Role::Reviewer
                    } else {
                        Role::Setup
                    },
                    &contract,
                )
                .unwrap();
                assert!(Session::open(&session_path, &different_role).is_err());
                let changed_bytes = bytes.iter().copied().chain([b' ']).collect::<Vec<_>>();
                let changed =
                    Contract::parse(changed_bytes.clone(), &readonly::digest(&changed_bytes))
                        .unwrap();
                different_role = readonly::configure(config.clone(), role, &changed).unwrap();
                assert!(Session::open(&session_path, &different_role).is_err());
                let mut execution_change = config;
                execution_change.workspace =
                    playtime_core::Sha256Digest::new("0".repeat(64)).unwrap();
                assert!(Session::open(&session_path, &execution_change).is_err());
            }
            assert_eq!(
                fs::read_to_string(root.0.join("workspace/source.txt")).unwrap(),
                "original source"
            );
            for path in [
                "workspace/forbidden",
                "workspace/.git/forbidden",
                "cache/forbidden",
            ] {
                assert!(!root.0.join(path).exists());
            }
        }
    }
}
