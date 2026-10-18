//! Real stopped-workspace capture gate. No model or production publication.
use playtime_docker::{transport::Cli, *};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

struct SyntheticJournal {
    root: PathBuf,
}
#[async_trait::async_trait]
impl OwnershipJournal for SyntheticJournal {
    async fn commit(&self, p: &Proposal) -> Result<()> {
        use std::io::Write;
        let path = self.root.join(format!("{}.json", p.name));
        let bytes = serde_json::to_vec_pretty(p).expect("proposal");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| journal_error())?;
        f.write_all(&bytes)
            .and_then(|()| f.sync_all())
            .map_err(|_| journal_error())?;
        std::fs::File::open(&self.root)
            .and_then(|f| f.sync_all())
            .map_err(|_| journal_error())
    }
}
fn journal_error() -> Error {
    Error {
        failure: Failure::Journal,
        operation: "synthetic_journal",
        detail: "cannot commit gate ownership",
    }
}
type GateResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn expect(condition: bool, detail: &str) -> GateResult<()> {
    if condition {
        Ok(())
    } else {
        Err(detail.into())
    }
}

async fn exec(sandbox: &mut Sandbox, command: &str) -> GateResult<ToolReply> {
    Ok(sandbox
        .tool(
            Tool::Exec {
                command: command.into(),
                cwd: None,
                timeout_seconds: None,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?)
}
fn success(reply: ToolReply) -> GateResult<String> {
    match reply {
        ToolReply::Exec {
            exit_code: 0,
            timed_out: false,
            stdout,
            ..
        } => Ok(stdout),
        other => Err(format!("synthetic command failed: {other:?}").into()),
    }
}
fn capture_profile(memory_bytes: u64) -> GateResult<capture::CaptureProfile> {
    Ok(capture::CaptureProfile::prepare(
        capture::CaptureLimits {
            memory_bytes,
            ..Default::default()
        },
        8 << 20,
        &playtime_protocol::storage::StorageLimits::default(),
        capture::ArchiveUsage::default(),
    )?)
}
async fn capture(
    sandbox: &mut Sandbox,
    memory_bytes: u64,
) -> GateResult<capture::SnapshotCandidate> {
    let mut stopped = sandbox
        .quiesce(
            Instant::now() + Duration::from_secs(80),
            CancellationToken::new(),
        )
        .await?;
    Ok(stopped
        .capture(&capture_profile(memory_bytes)?, CancellationToken::new())
        .await?)
}
async fn session(sandbox: &mut Sandbox, n: u32) -> GateResult<()> {
    Ok(sandbox
        .start_session(
            n,
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?)
}
async fn checks(sandbox: &mut Sandbox, root: &Path) -> GateResult<()> {
    sandbox
        .initialize(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
    session(sandbox, 1).await?;
    success(
        exec(
            sandbox,
            r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
import os
from pathlib import Path
p=Path('/work')
(p/'a.txt').write_bytes(b'alpha\n'); (p/'a.txt').chmod(0o640)
os.link(p/'a.txt',p/'alias')
(p/'dir').mkdir(mode=0o750)
(p/'dir/é.txt').write_bytes(b'beta\n'); (p/'dir/é.txt').chmod(0o644)
(p/'outside').symlink_to('../../outside')
(p/'run.sh').write_bytes(b'#!/bin/sh\nexit 0\n'); (p/'run.sh').chmod(0o755)
PY"#,
        )
        .await?,
    )?;
    let first = capture(sandbox, 65536).await?;
    expect(
        first.workspace().tar() == include_bytes!("../tests/fixtures/capture/reference.tar"),
        "locked image tar differs from reviewed reference",
    )?;
    std::fs::write(root.join("reference.tar"), first.workspace().tar())?;
    session(sandbox, 2).await?;
    success(exec(sandbox,r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
from pathlib import Path
p=Path('/work')
poison="raise RuntimeError('workspace import executed')\n"
for name in ('codecs','json','os','stat','sys','tarfile','io','struct','copy','re','time','pwd','grp','shutil','sitecustomize','usercustomize'):
 (p/(name+'.py')).write_text(poison)
site=p/'.local/lib/python3.12/site-packages'; site.mkdir(parents=True)
(site/'sitecustomize.py').write_text(poison); (site/'usercustomize.py').write_text(poison)
(p/'MEMENTO.md').write_text('actual outgoing memory\n'); (p/'MEMENTO.md').chmod(0o600)
(p/'empty').write_bytes(b''); (p/'empty-dir').mkdir(mode=0o711)
(p/('é'*100)).write_bytes(b'unicode name\n')
(p/'absolute-link').symlink_to('/etc/passwd')
PY
(while true; do printf x >> /work/ticks; sleep 0.02; done) >/dev/null 2>&1 &
sleep 0.15"#).await?)?;
    let candidate = capture(sandbox, 65536).await?;
    expect(
        candidate.memory().status == "loaded"
            && candidate.memory().content == "actual outgoing memory\n",
        "actual memento mismatch",
    )?;
    let entries = candidate.workspace().entries();
    let marker = entries
        .iter()
        .find(|e| e.path == "ticks")
        .ok_or("missing background writer marker")?;
    expect(marker.size_bytes > 0, "background writer never ran")?;
    expect(
        entries
            .iter()
            .find(|e| e.path == "MEMENTO.md")
            .unwrap()
            .mode
            == 0o600,
        "owner-only mode changed",
    )?;
    expect(
        entries.iter().any(|e| {
            e.path == "absolute-link"
                && e.link.as_deref() == Some("/etc/passwd")
                && e.content_slot.is_none()
        }),
        "inert link mismatch",
    )?;
    expect(
        entries
            .iter()
            .any(|e| e.path == "empty" && e.size_bytes == 0 && e.content_slot.is_some()),
        "empty file became unavailable",
    )?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let repeated = capture(sandbox, 65536).await?;
    expect(
        candidate.workspace().tar() == repeated.workspace().tar(),
        "background writer survived stop",
    )?;
    std::fs::write(root.join("workspace.tar"), candidate.workspace().tar())?;
    std::fs::write(
        root.join("capture-index.json"),
        candidate.workspace().index(),
    )?;
    std::fs::write(
        root.join("memory.json"),
        serde_json::to_vec(candidate.memory())?,
    )?;
    session(sandbox, 3).await?;
    let command = format!(
        "cd /work && PYTHONPATH=/work PYTHONHOME=/work PYTHONUSERBASE=/work/.local /usr/local/bin/python3.12 -I -S -B -c {} <<'DATA'\n65536\nDATA",
        shell_quote(capture::MEMORY_READER)
    );
    let out = success(exec(sandbox, &command).await?)?;
    let memory: serde_json::Value = serde_json::from_str(&out)?;
    expect(
        memory["status"] == "loaded" && memory["content"] == "actual outgoing memory\n",
        "locked helper imported poisoned code",
    )?;
    // Run the exporter too from poisoned cwd/env, discarding its tar inside
    // the workshop. Actual capture still uses the trusted holder and / cwd.
    let command = format!(
        "cd /work && PYTHONPATH=/work PYTHONHOME=/work PYTHONUSERBASE=/work/.local /usr/local/bin/python3.12 -I -S -B -c {} >/dev/null <<'DATA'\n{{\"entries\":4096,\"file_bytes\":8388608,\"retained_bytes\":8388608,\"tar_bytes\":67108864}}\nDATA",
        shell_quote(capture::EXPORTER)
    );
    success(exec(sandbox, &command).await?)?;
    // Verify permission denial in the locked image as uid 1000. The exporter
    // must fail; it may not chmod the entry or quietly omit it.
    success(exec(sandbox, "chmod 000 /work/MEMENTO.md").await?)?;
    let command = format!(
        "/usr/local/bin/python3.12 -I -S -B -c {} <<'DATA'\n65536\nDATA",
        shell_quote(capture::MEMORY_READER)
    );
    let denied: serde_json::Value =
        serde_json::from_str(&success(exec(sandbox, &command).await?)?)?;
    expect(
        denied["status"] == "unreadable" && denied["content"] == "",
        "unreadable memory status lost",
    )?;
    let command = format!(
        "/usr/local/bin/python3.12 -I -S -B -c {} >/dev/null <<'DATA'\n{{\"entries\":4096,\"file_bytes\":8388608,\"retained_bytes\":8388608,\"tar_bytes\":67108864}}\nDATA",
        shell_quote(capture::EXPORTER)
    );
    expect(
        matches!(
            exec(sandbox, &command).await?,
            ToolReply::Exec {
                exit_code: 1,
                timed_out: false,
                ..
            }
        ),
        "unreadable entry was skipped or privileged",
    )?;
    success(exec(sandbox, "chmod 600 /work/MEMENTO.md").await?)?;
    let oversized = capture(sandbox, 8).await?;
    expect(
        oversized.memory().status == "oversized" && oversized.memory().content.is_empty(),
        "oversized memory became empty loaded",
    )?;
    session(sandbox, 4).await?;
    success(exec(sandbox, "printf '\\000' > /work/MEMENTO.md").await?)?;
    let binary = capture(sandbox, 65536).await?;
    expect(
        binary.memory().status == "binary",
        "binary memory status lost",
    )?;
    session(sandbox, 5).await?;
    success(exec(sandbox, "mkfifo /work/unsupported-fifo").await?)?;
    expect(
        capture(sandbox, 65536).await.is_err(),
        "FIFO capture falsely succeeded",
    )?;
    expect(
        std::fs::read(root.join("workspace.tar"))? == candidate.workspace().tar(),
        "failed capture replaced prior evidence",
    )?;
    std::fs::write(
        root.join("helpers.json"),
        serde_json::to_vec_pretty(candidate.helpers())?,
    )?;
    Ok(())
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
async fn run(root: &Path, cli: &Path, socket: &Path, lock: &Path) -> GateResult<()> {
    let transport = Arc::new(Cli::resolve(cli, socket, &root.join("docker-config"))?);
    let engine = Engine::connect(
        transport.clone(),
        Instant::now() + Duration::from_secs(20),
        CancellationToken::new(),
    )
    .await?;
    let ready = engine
        .readiness(
            &std::fs::read(lock)?,
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
    let owner = Ownership {
        service: "synthetic-docker-capture-gate".into(),
        run: "checkpoint".into(),
        grant: "synthetic-grant".into(),
        invocation: "synthetic-invocation".into(),
        nonce: Ownership::random_nonce()?,
        daemon: engine.identity().clone(),
    };
    let mut sandbox = Sandbox::new(
        ready,
        owner,
        Limits::smoke(),
        WorkDeadline::after(Duration::from_secs(150))?,
        Arc::new(SyntheticJournal {
            root: root.to_owned(),
        }),
    )?;
    let result = checks(&mut sandbox, root).await;
    let cleanup = sandbox
        .cleanup(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await;
    std::fs::write(
        root.join("checkpoint-evidence.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"mode":"real locked Docker; synthetic tools; candidates only", "check":result.as_ref().map(|_|"passed").map_err(|e|e.to_string()),"selection":transport.selection(),"daemon":engine.identity(),"proposals":sandbox.proposals(),"operations":sandbox.observations(),"cleanup":cleanup}),
        )?,
    )?;
    expect(
        cleanup.complete(),
        "owned checkpoint resources remain; see evidence",
    )?;
    result?;
    println!(
        "checkpoint: locked-image golden, stopped background writer, exact files/modes/links/memory, isolated helpers, failed capture preservation and exact cleanup passed"
    );
    Ok(())
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 {
        eprintln!("usage: checkpoint-e2e PRIVATE_ROOT DOCKER_EXECUTABLE UNIX_SOCKET IMAGE_LOCK");
        std::process::exit(2);
    }
    if let Err(error) = run(
        Path::new(&args[0]),
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
    )
    .await
    {
        eprintln!(
            "checkpoint gate failed: {error}; bounded evidence retained in {}",
            args[0].to_string_lossy()
        );
        std::process::exit(1);
    }
}
