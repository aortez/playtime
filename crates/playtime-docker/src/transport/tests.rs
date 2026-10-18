use super::*;
use std::time::Duration;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!(
            "playtime-docker-cpu-{}",
            crate::Ownership::random_nonce().unwrap()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn cli(&self, source: &str) -> Cli {
        use std::os::unix::fs::DirBuilderExt;
        let path = self.0.join("synthetic-cli");
        std::fs::write(&path, format!("#!/usr/bin/python3\n{source}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = self.0.join("empty-config");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&config)
            .unwrap();
        // CPU-only executable, no Docker socket and no production discovery.
        Cli {
            selection: Selection {
                executable: path.clone(),
                executable_sha256: hash(&std::fs::read(&path).unwrap()),
                socket: PathBuf::from("/synthetic-explicit.sock"),
            },
            fingerprint: Fingerprint::read(&path).unwrap(),
            config_fingerprint: Fingerprint::read(&config).unwrap(),
            config,
            slots: Arc::new(Semaphore::new(4)),
        }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn fixture(source: &str) -> Command {
    let mut command = Command::new("/usr/bin/python3");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(["-I", "-S", "-B", "-c", source]);
    command
}
#[tokio::test]
async fn partial_writes_and_simultaneously_full_pipes_do_not_deadlock() {
    let source = "import os,sys\ndata=b''\nwhile len(data)<262144:\n part=os.read(0,min(17,262144-len(data)))\n if not part: break\n data+=part\n if len(data)%256==0: os.write(2,b'diagnostic')\nos.write(1,str(len(data)).encode())\n";
    let result = run(
        fixture(source),
        vec![b'x'; REQUEST_BYTES],
        QUERY_BYTES,
        Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await;
    assert!(result.success(), "{result:?}");
    assert_eq!(result.stdout, b"262144");
    let source =
        "import os\nos.write(1,b'a'*60000)\nos.write(2,b'b'*16000)\nwhile os.read(0,17): pass\n";
    let result = run(
        fixture(source),
        vec![b'x'; REQUEST_BYTES],
        QUERY_BYTES,
        Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await;
    assert!(result.success(), "{result:?}");
    assert_eq!(result.stdout.len(), 60000);
    assert_eq!(result.stderr.len(), 16000);
}
#[tokio::test]
async fn overflow_stderr_and_stdout_kills_and_reaps_the_owned_child() {
    for (fd, bound) in [(1, QUERY_BYTES), (2, DIAGNOSTIC_BYTES)] {
        let source = format!(
            "import os,time\nos.write({fd},b'x'*{})\ntime.sleep(60)",
            bound + 1
        );
        let result = run(
            fixture(&source),
            vec![],
            QUERY_BYTES,
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.fault, Some(Fault::Overflow));
        assert!(matches!(result.local, LocalExit::Reaped { .. }));
        assert!(result.stdout.len() <= QUERY_BYTES && result.stderr.len() <= DIAGNOSTIC_BYTES);
    }
}
#[tokio::test]
async fn blocked_stdin_and_inherited_open_pipes_obey_deadline_and_cancel() {
    for source in [
        "import time; time.sleep(60)",
        "import os,time\nif os.fork()==0: time.sleep(60)\nelse: os._exit(0)",
    ] {
        let start = Instant::now();
        let result = run(
            fixture(source),
            vec![b'x'; REQUEST_BYTES],
            QUERY_BYTES,
            start + Duration::from_millis(100),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(result.fault, Some(Fault::Deadline | Fault::Io)));
        assert!(matches!(result.local, LocalExit::Reaped { .. }));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let result = run(
        fixture("import time; time.sleep(60)"),
        vec![0; REQUEST_BYTES],
        QUERY_BYTES,
        Instant::now() + Duration::from_secs(10),
        token,
    )
    .await;
    assert_eq!(result.fault, Some(Fault::Cancelled));
    assert!(matches!(result.local, LocalExit::Reaped { .. }));
}
#[tokio::test]
async fn deadline_before_dispatch_and_nonzero_exit_are_distinct() {
    let result = run(
        fixture("raise RuntimeError('must not execute')"),
        vec![],
        QUERY_BYTES,
        Instant::now(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.dispatch, Dispatch::NotStarted);
    let result = run(
        fixture("import sys; sys.exit(7)"),
        vec![],
        QUERY_BYTES,
        Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        result.local,
        LocalExit::Reaped {
            code: Some(7),
            signal: None
        }
    );
    assert_eq!(result.fault, None);
}

#[tokio::test]
async fn inherited_docker_overrides_are_cleared_and_selection_changes_fail_closed() {
    if std::env::var_os("PLAYTIME_DOCKER_CPU_CHILD").is_none() {
        let status=Command::new(std::env::current_exe().unwrap()).args(["--exact","transport::tests::inherited_docker_overrides_are_cleared_and_selection_changes_fail_closed","--nocapture"])
            .env("PLAYTIME_DOCKER_CPU_CHILD","1").env("DOCKER_HOST","tcp://unexpected:2375").env("DOCKER_CONTEXT","wrong").env("DOCKER_TLS_VERIFY","1").env("DOCKER_CERT_PATH","/bad").env("DOCKER_CONFIG","/bad").env("DOCKER_API_VERSION","1.1").env("PYTHONPATH","/bad").status().await.unwrap();
        assert!(status.success());
        return;
    }
    let root = Root::new();
    let cli = root.cli(
        "import os,sys,json\nprint(json.dumps({'env':dict(os.environ),'args':sys.argv[1:]}))\n",
    );
    let result = cli
        .execute(
            Operation::Info,
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await;
    assert!(result.success(), "{result:?}");
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    for key in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
        "DOCKER_CONFIG",
        "DOCKER_API_VERSION",
        "PYTHONPATH",
    ] {
        assert!(value["env"].get(key).is_none());
    }
    assert_eq!(
        value["args"],
        serde_json::json!([
            "--config",
            cli.config,
            "--host",
            "unix:///synthetic-explicit.sock",
            "info",
            "--format",
            "{{json .}}"
        ])
    );
    std::fs::write(cli.config.join("config.json"), b"{}").unwrap();
    let result = cli
        .execute(
            Operation::Info,
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.fault, Some(Fault::SelectionChanged));
    assert_eq!(result.dispatch, Dispatch::NotStarted);
    let other = Root::new();
    let cli = other.cli("print('original')\n");
    std::fs::write(
        &cli.selection.executable,
        b"#!/usr/bin/python3\nprint('replacement')\n",
    )
    .unwrap();
    let result = cli
        .execute(
            Operation::Info,
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.fault, Some(Fault::SelectionChanged));
    assert_eq!(result.dispatch, Dispatch::NotStarted);
}

#[tokio::test]
async fn dropping_caller_cancels_and_reaps_retained_cli_task() {
    let root = Root::new();
    let pid_path = root.0.join("pid");
    let cli = root.cli(&format!(
        "import os,time\nwith open({},'w') as f: f.write(str(os.getpid()))\ntime.sleep(60)\n",
        serde_json::to_string(&pid_path).unwrap()
    ));
    let task_cli = cli.clone();
    let task = tokio::spawn(async move {
        task_cli
            .execute(
                Operation::Info,
                Instant::now() + Duration::from_secs(60),
                CancellationToken::new(),
            )
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_path.exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(&pid_path).unwrap();
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    task.abort();
    assert!(task.await.is_err());
    while cli.slots.available_permits() != 4 || Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "owned CLI not reaped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn oversized_stdin_is_rejected_without_spawning() {
    let root = Root::new();
    let cli = root.cli("raise RuntimeError('must not be launched')\n");
    let result = cli
        .execute(
            Operation::Bridge {
                id: "synthetic".into(),
                payload: vec![0; REQUEST_BYTES + 1],
            },
            Instant::now() + Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.fault, Some(Fault::Overflow));
    assert_eq!(result.dispatch, Dispatch::NotStarted);
}
