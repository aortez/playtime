//! Explicit real-Docker gate; no model, image mutation or production authority.
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
async fn sandbox_checks(sandbox: &mut Sandbox) -> GateResult<()> {
    sandbox
        .initialize(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
    sandbox
        .start_session(
            1,
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
    let output = success(
        exec(
            sandbox,
            r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
import os, pathlib, socket
assert os.getuid()==os.getgid()==1000
assert not pathlib.Path('/var/run/docker.sock').exists()
assert list(pathlib.Path('/sys/class/net').iterdir()) == [pathlib.Path('/sys/class/net/lo')]
assert 'NoNewPrivs:\t1' in pathlib.Path('/proc/self/status').read_text()
assert 'CapEff:\t0000000000000000' in pathlib.Path('/proc/self/status').read_text()
assert pathlib.Path('/sys/fs/cgroup/memory.max').read_text().strip()=='1073741824'
assert pathlib.Path('/sys/fs/cgroup/memory.swap.max').read_text().strip()=='0'
assert pathlib.Path('/sys/fs/cgroup/pids.max').read_text().strip()=='64'
quota,period=map(int,pathlib.Path('/sys/fs/cgroup/cpu.max').read_text().split()); assert quota==period
assert os.statvfs('/work').f_files==16384
try: pathlib.Path('/etc/playtime-escape').write_text('no')
except OSError: pass
else: raise AssertionError('writable root')
pathlib.Path('/work/persistent').write_text('survives')
pathlib.Path('/tmp/transient').write_text('fresh-session-only')
print('uid-network-rootfs-ok')
PY"#,
        )
        .await?,
    )?;
    expect(
        output.contains("uid-network-rootfs-ok"),
        "missing isolation proof",
    )?;
    // Poison every imported bridge module and user-site hooks. Fixed helpers
    // must never import these workspace files; no workshop source is changed.
    success(exec(sandbox,r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
from pathlib import Path
poison="raise RuntimeError('workspace import executed')\n"
for name in ('codecs','json','os','selectors','signal','stat','subprocess','sys','time','pathlib','sitecustomize','usercustomize'):
 Path('/work/'+name+'.py').write_text(poison)
site=Path('/work/.local/lib/python3.12/site-packages'); site.mkdir(parents=True)
(site/'sitecustomize.py').write_text(poison); (site/'usercustomize.py').write_text(poison)
PY"#).await?)?;
    let reply = sandbox
        .tool(
            Tool::WriteFile {
                path: "zero".into(),
                content: String::new(),
                append: false,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(
            reply,
            ToolReply::Write {
                bytes_written: 0,
                ..
            }
        ),
        "zero-byte write lost",
    )?;
    let reply = sandbox
        .tool(
            Tool::ReadFile {
                path: "/etc/os-release".into(),
                offset_bytes: 0,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(
            reply,
            ToolReply::Read {
                truncated: false,
                ..
            }
        ),
        "image-file read rejected",
    )?;
    let reply = sandbox
        .tool(
            Tool::WriteFile {
                path: "/etc/escape".into(),
                content: "no".into(),
                append: false,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(reply, ToolReply::Error { .. }),
        "escaping write accepted",
    )?;
    success(exec(sandbox, "printf '\\000' > /work/binary").await?)?;
    let reply = sandbox
        .tool(
            Tool::ReadFile {
                path: "binary".into(),
                offset_bytes: 0,
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(
            reply,
            ToolReply::Error {
                size_bytes: Some(1),
                ..
            }
        ),
        "binary-read status lost",
    )?;
    let reply = exec(
        sandbox,
        "/usr/local/bin/python3.12 -I -S -B -c 'print(\"x\"*100000)' ",
    )
    .await?;
    expect(
        matches!(reply,ToolReply::Exec{output_truncated:true,ref stdout,ref stderr,..} if stdout.len()+stderr.len()<=4096),
        "output not bounded",
    )?;
    let reply = sandbox
        .tool(
            Tool::Exec {
                command: "sleep 2".into(),
                cwd: None,
                timeout_seconds: Some(0.1),
            },
            ToolLimits {
                command_seconds: 5.0,
                output_bytes: 4096,
            },
            Instant::now() + Duration::from_secs(10),
            CancellationToken::new(),
        )
        .await?;
    expect(
        matches!(
            reply,
            ToolReply::Exec {
                timed_out: true,
                ..
            }
        ) && !sandbox.is_fenced(),
        "valid command timeout treated as transport uncertainty",
    )?;
    let reply = exec(
        sandbox,
        "dd if=/dev/zero of=/work/full bs=1M count=32; result=$?; rm /work/full; exit $result",
    )
    .await?;
    expect(
        matches!(reply,ToolReply::Exec{exit_code,ref stderr,..} if exit_code!=0 && stderr.contains("No space left on device")),
        "workspace capacity unenforced",
    )?;
    let reply = exec(
        sandbox,
        "dd if=/dev/zero of=/tmp/full bs=1M count=32; result=$?; rm /tmp/full; exit $result",
    )
    .await?;
    expect(
        matches!(reply,ToolReply::Exec{exit_code,ref stderr,..} if exit_code!=0 && stderr.contains("No space left on device")),
        "tmpfs capacity unenforced",
    )?;
    success(
        exec(
            sandbox,
            r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
import errno,os
from pathlib import Path
root=Path('/work/inodes'); root.mkdir()
try:
 for i in range(20000): (root/str(i)).touch()
except OSError as error: assert error.errno==errno.ENOSPC
else: raise AssertionError('workspace inode capacity unenforced')
finally:
 for entry in root.iterdir(): entry.unlink()
 root.rmdir()
PY"#,
        )
        .await?,
    )?;
    success(
        exec(
            sandbox,
            r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
import subprocess
children=[]
try:
 for i in range(100): children.append(subprocess.Popen(['/usr/bin/sleep','5']))
except OSError: print('pid-limit-ok')
else: raise AssertionError('PID limit not enforced')
finally:
 for child in children: child.kill()
 for child in children: child.wait()
PY"#,
        )
        .await?,
    )?;
    success(exec(sandbox,r#"/usr/local/bin/python3.12 -I -S -B -c 'import time; from pathlib import Path; [(Path("/work/counter").write_text(str(i)),time.sleep(.03)) for i in range(10000)]' >/work/background.log 2>&1 &"#).await?)?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let quiescent = sandbox
            .quiesce(
                Instant::now() + Duration::from_secs(80),
                CancellationToken::new(),
            )
            .await?;
        expect(!quiescent.holder_id().is_empty(), "missing retained holder")?;
        // D1 only: no snapshot/capture or writer receipt is claimed here.
    }
    sandbox
        .start_session(
            2,
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await?;
    success(
        exec(
            sandbox,
            r#"/usr/local/bin/python3.12 -I -S -B - <<'PY'
import time
from pathlib import Path
assert Path('/work/persistent').read_text()=='survives'
assert not Path('/tmp/transient').exists()
before=Path('/work/counter').read_text(); time.sleep(.2)
assert Path('/work/counter').read_text()==before
print('persistence-process-reset-ok')
PY"#,
        )
        .await?,
    )?;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        trigger.cancel();
    });
    let result = sandbox
        .tool(
            Tool::Exec {
                command: "sleep 30".into(),
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
        result.is_err() && sandbox.is_fenced(),
        "cancelled transport did not fence agent",
    )?;
    sandbox
        .quiesce(
            Instant::now() + Duration::from_secs(80),
            CancellationToken::new(),
        )
        .await?;
    Ok(())
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
    let journal = Arc::new(SyntheticJournal {
        root: root.to_owned(),
    });
    let owner = Ownership {
        service: "synthetic-docker-gate".into(),
        run: "sandbox".into(),
        grant: "synthetic-grant".into(),
        invocation: "synthetic-invocation".into(),
        nonce: Ownership::random_nonce()?,
        daemon: engine.identity().clone(),
    };
    let mut sandbox = Sandbox::new(
        ready.clone(),
        owner.clone(),
        Limits::smoke(),
        WorkDeadline::after(Duration::from_secs(120))?,
        journal.clone(),
    )?;
    let result = sandbox_checks(&mut sandbox).await;
    let cleanup = sandbox
        .cleanup(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await;
    std::fs::write(
        root.join("sandbox-evidence.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"mode":"real Docker; scripted tool calls; no model","selection":transport.selection(),"daemon":engine.identity(),"proposals":sandbox.proposals(),"check":result.as_ref().map(|_|"passed").map_err(|e|e.to_string()),"operations":sandbox.observations(),"cleanup":cleanup}),
        )?,
    )?;
    expect(
        cleanup.complete(),
        "owned sandbox residuals remain; see evidence",
    )?;
    result?;
    // Fresh namespace: prove the frozen root timer stops the agent without any
    // controller polling/cancellation, while the holder survives for capture.
    let mut timer = Sandbox::new(
        ready,
        Ownership {
            nonce: Ownership::random_nonce()?,
            ..owner
        },
        Limits::smoke(),
        WorkDeadline::after(Duration::from_secs(4))?,
        journal,
    )?;
    let timer_result = async {
        timer
            .initialize(
                Instant::now() + Duration::from_secs(20),
                CancellationToken::new(),
            )
            .await?;
        timer
            .start_session(
                1,
                Instant::now() + Duration::from_secs(20),
                CancellationToken::new(),
            )
            .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Inspect the exact owned ID, without stopping it first.
        let proposal = timer
            .proposals()
            .into_iter()
            .find(|p| p.role == Role::Agent)
            .expect("timer agent");
        use playtime_docker::transport::{Operation, Transport};
        let out = transport
            .execute(
                Operation::ContainerInspect(proposal.name),
                Instant::now() + Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await;
        expect(out.success(), "cannot inspect expired timer")?;
        let value: serde_json::Value = serde_json::from_slice(&out.stdout)?;
        expect(
            value[0]["State"]["Running"] == false,
            "independent lifetime timer did not stop agent",
        )?;
        timer
            .quiesce(
                Instant::now() + Duration::from_secs(80),
                CancellationToken::new(),
            )
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let cleanup = timer
        .cleanup(
            Instant::now() + Duration::from_secs(20),
            CancellationToken::new(),
        )
        .await;
    std::fs::write(
        root.join("timer-evidence.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"operations":timer.observations(),"cleanup":cleanup}),
        )?,
    )?;
    expect(
        cleanup.complete(),
        "owned timer resources remain; see evidence",
    )?;
    timer_result?;
    println!(
        "sandbox: real locked Docker image; isolation, bridge, limits, session reset, cancellation, absolute timer and exact cleanup passed"
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 4 {
        eprintln!("usage: sandbox-e2e PRIVATE_ROOT DOCKER_EXECUTABLE UNIX_SOCKET IMAGE_LOCK");
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
            "sandbox gate failed: {error}; bounded evidence retained in {}",
            args[0].to_string_lossy()
        );
        std::process::exit(1);
    }
}
