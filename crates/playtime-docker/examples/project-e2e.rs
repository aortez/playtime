//! Real local-project editing gate; only the disposable test project is mounted.
use playtime_docker::{project::*, transport::Cli, *};
use playtime_local::PrivateDirectory;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;
type GateResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Journal(PathBuf);
#[async_trait::async_trait]
impl ProjectJournal for Journal {
    async fn commit(&self, proposal: &ProjectProposal) -> Result<()> {
        let root = self.0.clone();
        let p = proposal.clone();
        tokio::task::spawn_blocking(move || {
            let error = |_| Error {
                failure: Failure::Journal,
                operation: "project_gate_journal",
                detail: "cannot commit ownership",
            };
            PrivateDirectory::open(&root, false)
                .map_err(error)?
                .write_new(
                    &format!("{}.json", p.container.name),
                    &serde_json::to_vec_pretty(&p).expect("proposal"),
                )
                .map_err(error)
        })
        .await
        .expect("journal worker")
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(20)
}
fn expect(value: bool, detail: &str) -> GateResult<()> {
    if value {
        Ok(())
    } else {
        Err(detail.to_owned().into())
    }
}
async fn exec(workspace: &mut ProjectWorkspace, command: &str) -> GateResult<ToolReply> {
    Ok(workspace
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
            deadline(),
            CancellationToken::new(),
        )
        .await?)
}
async fn checks(workspace: &mut ProjectWorkspace, project: &Path) -> GateResult<()> {
    workspace
        .start_workspace(deadline(), CancellationToken::new())
        .await?;
    let reply = exec(
        workspace,
        r#"python3 -I - <<'PY'
import os, pathlib, socket
assert os.getuid() == 1000
assert pathlib.Path('/work/source.txt').read_text() == 'before'
pathlib.Path('/work/source.txt').write_text('after')
assert not pathlib.Path('/var/run/docker.sock').exists()
assert not pathlib.Path('/work/outside-link').exists()
try:
    pathlib.Path('/etc/host-write-test').write_text('bad')
except OSError:
    pass
else:
    raise AssertionError('root filesystem writable')
try:
    socket.create_connection(('1.1.1.1',80), timeout=.2)
except OSError:
    pass
else:
    raise AssertionError('external network reachable')
print('project-boundaries-ok')
PY"#,
    )
    .await?;
    expect(
        matches!(&reply,ToolReply::Exec {exit_code:0,timed_out:false,stdout,..} if stdout.contains("project-boundaries-ok")),
        &format!("boundary check failed: {reply:?}"),
    )?;
    expect(
        std::fs::read_to_string(project.join("source.txt"))? == "after",
        "host edit was not visible",
    )?;
    let reply = workspace
        .tool(
            Tool::ReadFile {
                path: "/work/source.txt".into(),
                offset_bytes: 0,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            deadline(),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(reply,ToolReply::Read {content,..} if content=="after"),
        "readback differed",
    )?;
    let reply = workspace
        .tool(
            Tool::WriteFile {
                path: "/work/written.txt".into(),
                content: "persistent".into(),
                append: false,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            deadline(),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(
            reply,
            ToolReply::Write {
                bytes_written: 10,
                ..
            }
        ),
        "write_file result differed",
    )?;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        trigger.cancel();
    });
    let result = workspace
        .tool(
            Tool::Exec {
                command: "sleep 30; printf late > /work/late".into(),
                cwd: None,
                timeout_seconds: None,
            },
            ToolLimits {
                command_seconds: 30.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(35),
            cancel,
        )
        .await;
    expect(
        result.is_err() && workspace.is_fenced(),
        "cancelled tool was not fenced",
    )?;
    expect(
        exec(workspace, "touch /work/replayed").await.is_err(),
        "fenced tool was replayed",
    )?;
    Ok(())
}
async fn run(root: PathBuf, cli: PathBuf, socket: PathBuf, lock: PathBuf) -> GateResult<()> {
    let private = root.join("private");
    PrivateDirectory::open(&private, true)?;
    let project = root.join("project");
    std::fs::create_dir(&project)?;
    std::fs::write(project.join("source.txt"), "before")?;
    std::fs::write(root.join("secret"), "outside")?;
    std::os::unix::fs::symlink(root.join("secret"), project.join("outside-link"))?;
    let directory = {
        let p = project.clone();
        let s = private.clone();
        tokio::task::spawn_blocking(move || ProjectDirectory::open(&p, &s, &[])).await??
    };
    let transport = Arc::new(Cli::resolve(&cli, &socket, &private.join("docker-config"))?);
    let engine = Engine::connect(transport, deadline(), CancellationToken::new()).await?;
    let ready = engine
        .readiness(&std::fs::read(lock)?, deadline(), CancellationToken::new())
        .await?;
    let owner = Ownership {
        service: "project-gate".into(),
        run: "synthetic-project".into(),
        grant: "synthetic".into(),
        invocation: "synthetic".into(),
        nonce: Ownership::random_nonce()?,
        daemon: engine.identity().clone(),
    };
    let mut workspace = tokio::task::spawn_blocking(move || {
        ProjectWorkspace::new(
            ready,
            owner,
            Arc::new(directory),
            ProjectLimits {
                nano_cpus: 1_000_000_000,
                memory_bytes: 1024 * 1024 * 1024,
                tmp_bytes: 16 * 1024 * 1024,
                tmp_inodes: 16384,
                pids: 64,
                seed: 1042,
            },
            WorkDeadline::after(Duration::from_secs(60))?,
            Arc::new(Journal(private)),
        )
    })
    .await??;
    let result = checks(&mut workspace, &project).await;
    let cleanup = workspace
        .cleanup(deadline(), CancellationToken::new())
        .await;
    std::fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"schema":1,"case":"project","checks":result.as_ref().map(|_|"passed").map_err(|e|e.to_string()),"cleanup":cleanup,"observations":engine.observations()}),
        )?,
    )?;
    if !cleanup.complete() {
        return Err(
            "project cleanup unresolved; retained proposal identifies exact resource".into(),
        );
    }
    result?;
    assert_eq!(
        std::fs::read_to_string(project.join("written.txt"))?,
        "persistent"
    );
    assert_eq!(std::fs::read_to_string(root.join("secret"))?, "outside");
    assert!(!project.join("late").exists() && !project.join("replayed").exists());
    drop(workspace);
    ProjectDirectory::open(&project, &root.join("private"), &[])?;
    println!(
        "Project edit/read/write, uid/network/root boundary, cancellation fence and exact cleanup passed."
    );
    Ok(())
}
#[tokio::main]
async fn main() -> GateResult<()> {
    let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if args.len() != 4 {
        return Err("usage: project-e2e ROOT DOCKER SOCKET IMAGE_LOCK".into());
    }
    run(
        args[0].clone(),
        args[1].clone(),
        args[2].clone(),
        args[3].clone(),
    )
    .await
}
