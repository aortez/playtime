//! Real HTTP + CLI process cases, reused in Cargo tests and the process wrapper.
use crate::fixture::Script;
use crate::*;
use playtime_local::Credentials;
use playtime_protocol::api::*;
use playtime_server::http::{RunningServer, Server};
use std::{
    net::SocketAddr,
    os::unix::fs::{PermissionsExt, symlink},
    sync::Arc,
};

macro_rules! ensure {
    ($condition:expr, $message:expr) => {
        if !$condition {
            return Err($message.into());
        }
    };
}
enum Running {
    Process(Service),
    InProcess {
        server: RunningServer,
        script: Arc<Script>,
    },
}
struct Harness<'a> {
    cli: &'a Path,
    fixture: Option<&'a Path>,
    root: TestRoot,
    running: Option<Running>,
    connection: PathBuf,
    endpoint: String,
    records: Vec<Value>,
}
impl<'a> Harness<'a> {
    async fn new(cli: &'a Path, fixture: Option<&'a Path>, scenario: &str) -> Result<Self> {
        let root = TestRoot::new()?;
        let mut h = Self {
            cli,
            fixture,
            root,
            running: None,
            connection: PathBuf::new(),
            endpoint: String::new(),
            records: vec![],
        };
        h.start("service", scenario, "cli-fixture", None).await?;
        Ok(h)
    }
    async fn start(
        &mut self,
        name: &str,
        scenario: &str,
        identity: &str,
        address: Option<SocketAddr>,
    ) -> Result<()> {
        if let Some(fixture) = self.fixture {
            let service = Service::start(
                fixture,
                &self.root,
                name,
                scenario,
                identity,
                address.map(|a| a.to_string()).as_deref(),
            )
            .await?;
            self.connection = service.connection.clone();
            self.endpoint = service.endpoint.clone();
            self.running = Some(Running::Process(service));
        } else {
            let credentials = Credentials::initialize(&self.root.join(name))?;
            let script = Arc::new(Script::new(ServiceId::new(identity)?, scenario.into()));
            let server = Server::new(script.clone(), credentials.token.clone())?
                .listen(address.unwrap_or("127.0.0.1:0".parse()?))
                .await?;
            self.endpoint = server.endpoint();
            self.connection = credentials.connection_path.clone();
            credentials.publish(&Connection {
                schema_version: 1,
                endpoint: self.endpoint.clone(),
                service_id: ServiceId::new(identity)?,
                token_file: credentials.token_path.to_string_lossy().into(),
            })?;
            self.running = Some(Running::InProcess { server, script });
        }
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        match self.running.take() {
            Some(Running::Process(s)) => s.stop().await?,
            Some(Running::InProcess { server, .. }) => server.stop().await?,
            None => (),
        }
        Ok(())
    }
    async fn run(&mut self, args: &[&str]) -> Result<Captured> {
        run_command(
            self.cli,
            &self.root,
            Some(&self.connection),
            args,
            &mut self.records,
        )
        .await
    }
    async fn offline(&mut self, args: &[&str]) -> Result<Captured> {
        run_command(self.cli, &self.root, None, args, &mut self.records).await
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = isolated(self.cli, &self.root);
        c.arg("--connection-file").arg(&self.connection).args(args);
        c
    }
    async fn calls(&self) -> Result<Vec<Value>> {
        if let Some(Running::InProcess { script, .. }) = &self.running {
            return Ok(script
                .fixture
                .calls()
                .iter()
                .map(|c| json!({"operation":c.operation,"id":c.id}))
                .collect());
        }
        let path = self
            .connection
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("requests.json");
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(value) = serde_json::from_slice(&bytes)
            {
                return Ok(value);
            }
            if Instant::now() >= end {
                return Err("request trace unavailable".into());
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    async fn launched(&mut self, key: &str, wait: bool) -> Result<String> {
        let mut args = vec![
            "run",
            "fixture-example",
            "--mode",
            "simulation",
            "--idempotency-key",
            key,
            "--format",
            "json",
        ];
        if wait {
            args.push("--wait");
        }
        let r = self.run(&args).await?;
        r.expect(0)?;
        let v = r.json()?;
        Ok(if wait {
            v["data"]["run"]["run_id"].as_str()
        } else {
            v["data"]["acceptance"]["run_id"].as_str()
        }
        .ok_or("missing run ID")?
        .into())
    }
    async fn finish(&mut self, name: &str, result: Result<()>) -> Result<()> {
        if let Err(error) = result {
            let trace = self.calls().await.unwrap_or_default();
            self.records.push(json!({"requests":trace}));
            let bundle = failure_bundle(&self.root, &self.records, &error.to_string())?;
            return Err(format!("{name}: {error}; failure evidence: {}", bundle.display()).into());
        }
        self.stop().await?;
        Ok(())
    }
}

pub async fn run(cli: &Path, fixture: Option<&Path>) -> Result<usize> {
    timeout(Duration::from_secs(180), cases(cli, fixture))
        .await
        .map_err(|_| "CLI suite exceeded 180 seconds")?
}
async fn cases(cli: &Path, fixture: Option<&Path>) -> Result<usize> {
    let mut count = 0;
    macro_rules! case {
        ($name:literal, $scenario:literal, $h:ident, $body:block) => {{
            let mut $h = Harness::new(cli, fixture, $scenario).await?;
            let result = timeout(Duration::from_secs(30), async { $body Ok(()) }).await.map_err(|_| "case exceeded 30 seconds".into()).and_then(|r| r);
            $h.finish($name, result).await?; count += 1;
        }};
    }
    case!("commands and verified content", "normal", h, {
        let status = h.run(&["service", "status", "--format", "json"]).await?;
        status.expect(0)?;
        ensure!(
            status.json()?["data"]["capabilities"]["backend"]["kind"] == "fixture",
            "fixture provenance missing"
        );
        let recipes = h
            .run(&["recipes", "list", "--limit", "1", "--format", "json"])
            .await?;
        recipes.expect(0)?;
        ensure!(
            recipes.json()?["data"]["items"].as_array().unwrap().len() == 1,
            "page limit ignored"
        );
        h.run(&[
            "recipes",
            "preview",
            "fixture-example",
            "--mode",
            "simulation",
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        let run = h.launched("commands", true).await?;
        let result = h.run(&["runs", "show", &run, "--format", "json"]).await?;
        result.expect(0)?;
        let view = result.json()?;
        ensure!(
            view["data"]["usage"]["known_generated_tokens"] == 10
                && view["data"]["usage"]["unknown_reserved_tokens"] == 100
                && view["data"]["usage"]["budget_charged_tokens"] == 110
                && view["data"]["usage"]["usage_complete"] == false,
            "unknown usage accounting lost"
        );
        for args in [
            vec!["runs", "list"],
            vec!["runs", "wait", &run],
            vec!["checkpoints", "list", &run],
            vec!["checkpoints", "show", &run, "s1"],
            vec!["artifacts", "list", &run],
            vec!["artifacts", "show", &run, "saved-image"],
            vec!["launches", "show", "commands"],
            vec!["launches", "list"],
        ] {
            let mut args = args;
            args.extend(["--format", "json"]);
            let result = h.run(&args).await?;
            result.expect(0)?;
            ensure!(
                result.json()?["schema_version"] == 1,
                "output schema missing"
            );
        }
        let file = h.root.join("image.png");
        let path = file.to_str().unwrap();
        h.run(&[
            "artifacts",
            "download",
            &run,
            "saved-image",
            "--output",
            path,
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        ensure!(
            std::fs::read(&file)?
                == include_bytes!("../../playtime-server/tests/fixtures/pixel.png"),
            "download byte mismatch"
        );
        h.run(&[
            "artifacts",
            "download",
            &run,
            "saved-image",
            "--output",
            path,
            "--format",
            "json",
        ])
        .await?
        .expect(7)?;
        std::fs::write(&file, b"existing")?;
        h.run(&[
            "artifacts",
            "download",
            &run,
            "saved-image",
            "--output",
            path,
            "--force",
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        let linked = h.root.join("symlink");
        symlink(&file, &linked)?;
        h.run(&[
            "artifacts",
            "download",
            &run,
            "saved-image",
            "--output",
            linked.to_str().unwrap(),
            "--force",
            "--format",
            "json",
        ])
        .await?
        .expect(7)?;
        let events = h
            .run(&["events", &run, "--all", "--limit", "1", "--format", "jsonl"])
            .await?;
        events.expect(0)?;
        let frames = events.frames()?;
        let events: Vec<_> = frames.iter().filter(|f| f["kind"] == "event").collect();
        ensure!(
            events.len() == 4
                && events[1]["data"]["event"]["data"]["unknown_field"]["retained"] == true,
            "unknown event or history record lost"
        );
        let retry = h
            .run(&["launches", "retry", "commands", "--format", "json"])
            .await?;
        retry.expect(0)?;
        ensure!(
            retry.json()?["data"]["acceptance"]["run_id"] == run
                && retry.json()?["data"]["acceptance"]["replayed"] == true,
            "retry dispatched a new run"
        );
        h.run(&[
            "run",
            "fixture-example",
            "--mode",
            "simulation",
            "--idempotency-key",
            "commands",
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        let calls = h.calls().await?;
        ensure!(
            calls.iter().filter(|c| c["operation"] == "preview").count() == 2,
            "saved request incorrectly obtained another preview"
        );
    });
    case!("launch-follow output", "normal", h, {
        let result = h
            .run(&[
                "run",
                "fixture-example",
                "--mode",
                "simulation",
                "--follow",
                "--timeout",
                "5s",
                "--format",
                "jsonl",
            ])
            .await?;
        result.expect(0)?;
        let frames = result.frames()?;
        ensure!(
            frames.first().unwrap()["kind"] == "launch"
                && frames.last().unwrap()["kind"] == "result",
            "launch-follow framing changed"
        );
        ensure!(
            frames.iter().filter(|f| f["kind"] == "event").count() == 4,
            "follow lost records"
        );
    });
    case!("lost admission response", "lost-launch", h, {
        let lost = h
            .run(&[
                "run",
                "fixture-example",
                "--mode",
                "simulation",
                "--idempotency-key",
                "lost",
                "--format",
                "json",
            ])
            .await?;
        lost.expect(3)?;
        let errors: Vec<Value> = lost
            .stderr
            .split(|b| *b == b'\n')
            .filter_map(|b| serde_json::from_slice(b).ok())
            .collect();
        ensure!(
            errors
                .last()
                .is_some_and(|e| e["kind"] == "error" && e["data"]["recovery"]["key"] == "lost"),
            "machine error lost recovery identity"
        );
        ensure!(
            String::from_utf8_lossy(&lost.stderr).contains("receipt_path"),
            "ambiguous result omitted receipt"
        );
        let listing = h.offline(&["launches", "list", "--format", "json"]).await?;
        listing.expect(0)?;
        ensure!(
            listing.json()?["data"]["items"][0]["run_id"].is_null(),
            "lost reply was locally acknowledged"
        );
        let retry = h
            .run(&[
                "launches",
                "retry",
                "lost",
                "--wait",
                "--timeout",
                "5s",
                "--format",
                "json",
            ])
            .await?;
        retry.expect(0)?;
        ensure!(
            retry.json()?["data"]["launch"]["acceptance"]["replayed"] == true,
            "lost reply caused duplicate admission"
        );
        let runs = h.run(&["runs", "list", "--format", "json"]).await?;
        ensure!(
            runs.json()?["data"]["items"].as_array().unwrap().len() == 1,
            "duplicate dispatch after retry"
        );
    });
    case!("signals deadlines cancellation and pipes", "hold", h, {
        let run = h.launched("hold", false).await?;
        h.run(&[
            "runs",
            "wait",
            &run,
            "--timeout",
            "50ms",
            "--format",
            "json",
        ])
        .await?
        .expect(8)?;
        for (signal, exit) in [
            (rustix::process::Signal::INT, 130),
            (rustix::process::Signal::TERM, 143),
        ] {
            let deadline = Instant::now() + Duration::from_secs(3);
            // Wait for an authenticated observer request, not a fixed startup sleep.
            let before = h
                .calls()
                .await?
                .iter()
                .filter(|c| c["operation"] == "subscribe")
                .count();
            let child = h
                .command(&["events", &run, "--follow", "--format", "jsonl"])
                .spawn()?;
            loop {
                if h.calls()
                    .await?
                    .iter()
                    .filter(|c| c["operation"] == "subscribe")
                    .count()
                    > before
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("observer did not attach".into());
                }
                sleep(Duration::from_millis(10)).await;
            }
            signal_child(&child, signal)?;
            capture(child).await?.expect(exit)?;
        }
        let mut broken = h.command(&["events", &run, "--format", "jsonl"]).spawn()?;
        drop(broken.stdout.take());
        let stderr = broken.stderr.take().unwrap();
        let status = timeout(Duration::from_secs(3), broken.wait()).await??;
        let bytes = bounded_read(stderr).await?;
        ensure!(
            status.code() == Some(7) && !String::from_utf8_lossy(&bytes).contains("panicked"),
            "broken pipe panicked or wrong exit"
        );
        let view = h
            .run(&["runs", "show", &run, "--format", "json"])
            .await?
            .json()?;
        ensure!(
            view["data"]["cancellation"].is_null()
                && view["data"]["orchestration"]["state"] == "running",
            "observer termination cancelled the run"
        );
        h.run(&[
            "runs",
            "cancel",
            &run,
            "--wait",
            "--timeout",
            "5s",
            "--format",
            "json",
        ])
        .await?
        .expect(0)?;
        h.run(&["runs", "wait", &run, "--format", "json"])
            .await?
            .expect(10)?;
        h.run(&["runs", "show", &run, "--format", "json"])
            .await?
            .expect(0)?;
    });
    case!("cursor publication and redelivery", "normal", h, {
        let run = h.launched("cursor", true).await?;
        let cursor = h.root.join("cursor.json");
        let first = h
            .run(&[
                "events",
                &run,
                "--limit",
                "1",
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--format",
                "jsonl",
            ])
            .await?;
        first.expect(0)?;
        let original = std::fs::read(&cursor)?;
        let resumed = h
            .run(&[
                "events",
                &run,
                "--all",
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--format",
                "jsonl",
            ])
            .await?;
        resumed.expect(0)?;
        let frames = resumed.frames()?;
        ensure!(
            frames
                .iter()
                .filter(|f| f["kind"] == "event")
                .map(|f| f["data"]["event"]["sequence"].as_u64().unwrap())
                .collect::<Vec<_>>()
                == vec![2, 3, 4],
            "cursor silently skipped or replayed committed position"
        );
        std::fs::write(&cursor, &original)?;
        let redelivered = h
            .run(&[
                "events",
                &run,
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--limit",
                "1",
                "--format",
                "jsonl",
            ])
            .await?;
        ensure!(
            redelivered.frames()?[0]["data"]["event"]["sequence"] == 2,
            "crash-window redelivery lost"
        );
        ensure!(
            std::fs::metadata(&cursor)?.permissions().mode() & 0o777 == 0o600,
            "cursor file not private"
        );
        let before = std::fs::read(&cursor)?;
        let mut broken = h
            .command(&[
                "events",
                &run,
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--format",
                "jsonl",
            ])
            .spawn()?;
        drop(broken.stdout.take());
        let stderr = broken.stderr.take().unwrap();
        ensure!(
            timeout(Duration::from_secs(3), broken.wait())
                .await??
                .code()
                == Some(7),
            "closed event output did not fail"
        );
        drop(bounded_read(stderr).await?);
        ensure!(
            std::fs::read(&cursor)? == before,
            "failed output advanced saved cursor"
        );
    });
    case!(
        "conflicting frontier preserves saved position",
        "conflicting-frontier",
        h,
        {
            let run = h.launched("conflicting", true).await?;
            let cursor = h.root.join("cursor.json");
            let observed = h
                .run(&[
                    "events",
                    &run,
                    "--follow",
                    "--cursor-file",
                    cursor.to_str().unwrap(),
                    "--timeout",
                    "5s",
                    "--format",
                    "jsonl",
                ])
                .await?;
            observed.expect(6)?;
            let frames = observed.frames()?;
            ensure!(
                frames.len() == 1
                    && frames[0]["kind"] == "event"
                    && frames[0]["data"]["event"]["sequence"] == 1,
                "conflicting frontier emitted another event or completion"
            );
            let saved: playtime_client::state::CursorReceipt =
                serde_json::from_slice(&std::fs::read(&cursor)?)?;
            ensure!(
                frames[0]["data"]["cursor"] == saved.cursor,
                "conflicting frontier advanced the saved cursor"
            );
            ensure!(
                EventCursor::decode(&saved.cursor)?.after_sequence == 1,
                "saved cursor did not retain the last valid event"
            );
            ensure!(
                !h.calls().await?.iter().any(|c| c["operation"] == "cancel"),
                "observation failure cancelled the run"
            );
        }
    );
    case!("killed observer reconnects with no gap", "normal", h, {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let run = h.launched("crash", false).await?;
        let cursor = h.root.join("crash-cursor.json");
        let mut child = h
            .command(&[
                "events",
                &run,
                "--follow",
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--format",
                "jsonl",
            ])
            .spawn()?;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = timeout(Duration::from_secs(3), async {
            loop {
                let line = lines
                    .next_line()
                    .await?
                    .ok_or("observer ended before event")?;
                let frame: Value = serde_json::from_str(&line)?;
                if frame["kind"] == "event" {
                    return Ok::<Value, Box<dyn std::error::Error + Send + Sync>>(frame);
                }
            }
        })
        .await??;
        let first_sequence = first["data"]["event"]["sequence"].as_u64().unwrap();
        child.kill().await?;
        child.wait().await?;
        let resumed = h
            .run(&[
                "events",
                &run,
                "--follow",
                "--cursor-file",
                cursor.to_str().unwrap(),
                "--timeout",
                "5s",
                "--format",
                "jsonl",
            ])
            .await?;
        resumed.expect(0)?;
        let mut observed = std::collections::BTreeSet::from([first_sequence]);
        for frame in resumed.frames()? {
            if frame["kind"] == "event" {
                observed.insert(frame["data"]["event"]["sequence"].as_u64().unwrap());
            }
        }
        ensure!(
            observed == [1, 2, 3, 4].into(),
            "killed observer lost evidence across reconnect"
        );
        let view = h
            .run(&["runs", "show", &run, "--format", "json"])
            .await?
            .json()?;
        ensure!(
            view["data"]["cancellation"].is_null(),
            "killing observer requested cancellation"
        );
    });
    case!("validation authentication and error exits", "normal", h, {
        h.offline(&["--help"]).await?.expect(0)?;
        h.offline(&["--version"]).await?.expect(0)?;
        let empty = h.offline(&["launches", "list", "--format", "json"]).await?;
        let empty_golden: Value = serde_json::from_str(include_str!(
            "../../playtime-cli/tests/fixtures/empty-page.json"
        ))?;
        ensure!(
            empty.json()? == empty_golden,
            "offline receipt page contract changed"
        );
        for args in [
            vec!["run", "fixture-example", "--follow", "--format", "json"],
            vec!["events", "run", "--all", "--format", "json"],
            vec!["run", "recipe", "--timeout", "0s"],
            vec!["recipes", "list", "--limit", "0"],
        ] {
            h.offline(&args).await?.expect(2)?;
        }
        h.offline(&["smoke", "experiments/pilot.toml", "--format", "json"])
            .await?
            .expect(6)?;
        let missing = h
            .run(&["runs", "show", "missing", "--format", "json"])
            .await?;
        missing.expect(5)?;
        let mut error: Value = serde_json::from_slice(
            missing
                .stderr
                .split(|b| *b == b'\n')
                .rfind(|b| !b.is_empty())
                .unwrap(),
        )?;
        error["data"]["request_id"] = json!("<request-id>");
        let golden: Value = serde_json::from_str(include_str!(
            "../../playtime-cli/tests/fixtures/not-found.json"
        ))?;
        ensure!(
            error == golden,
            "server error code/details contract changed"
        );
        h.run(&["run", "fixture-example", "--format", "json"])
            .await?
            .expect(6)?;
        h.run(&[
            "run",
            "fixture-example",
            "--mode",
            "simulation",
            "--preview-revision",
            "stale",
            "--format",
            "json",
        ])
        .await?
        .expect(5)?;
        let config = h.root.join("invalid.toml");
        std::fs::write(&config, "unknown = 1")?;
        h.offline(&[
            "--config",
            config.to_str().unwrap(),
            "service",
            "status",
            "--format",
            "json",
        ])
        .await?
        .expect(2)?;
        let wrong = Credentials::initialize(&h.root.join("wrong-credential"))?;
        let result = h
            .offline(&[
                "--endpoint",
                &h.endpoint.clone(),
                "--token-file",
                wrong.token_path.to_str().unwrap(),
                "service",
                "status",
                "--format",
                "json",
            ])
            .await?;
        result.expect(4)?;
        ensure!(
            !String::from_utf8_lossy(&result.stderr).contains(&wrong.token.expose()),
            "credential leaked to diagnostics"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let unavailable = format!("http://{}", listener.local_addr()?);
        drop(listener);
        h.offline(&[
            "--endpoint",
            &unavailable,
            "--token-file",
            wrong.token_path.to_str().unwrap(),
            "service",
            "status",
            "--format",
            "json",
        ])
        .await?
        .expect(3)?;
    });
    case!(
        "receipt protects service identity after rebind",
        "normal",
        h,
        {
            h.launched("rebind", true).await?;
            let address: SocketAddr = h.endpoint.strip_prefix("http://").unwrap().parse()?;
            h.stop().await?;
            h.start("replacement", "normal", "another-service", Some(address))
                .await?;
            h.run(&["launches", "retry", "rebind", "--format", "json"])
                .await?
                .expect(5)?;
            let runs = h.run(&["runs", "list", "--format", "json"]).await?.json()?;
            ensure!(
                runs["data"]["items"].as_array().unwrap().is_empty(),
                "saved request rebound to another service"
            );
        }
    );
    case!(
        "integrity failure never publishes download",
        "corrupt",
        h,
        {
            let run = h.launched("corrupt", true).await?;
            let path = h.root.join("result.png");
            let result = h
                .run(&[
                    "artifacts",
                    "download",
                    &run,
                    "saved-image",
                    "--output",
                    path.to_str().unwrap(),
                    "--format",
                    "json",
                ])
                .await?;
            ensure!(
                matches!(result.code, 3 | 6),
                "unexpected integrity/transport failure classification"
            );
            ensure!(!path.exists(), "corrupt artifact published");
            ensure!(
                !std::fs::read_dir(&h.root.0)?.any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")),
                "failed download left temporary bytes"
            );
        }
    );
    for scenario in ["unclean", "abnormal"] {
        let mut h = Harness::new(cli, fixture, scenario).await?;
        let result = timeout(Duration::from_secs(30), async {
            h.run(&[
                "run",
                "fixture-example",
                "--mode",
                "simulation",
                "--wait",
                "--timeout",
                "5s",
                "--format",
                "json",
            ])
            .await?
            .expect(10)?;
            Ok(())
        })
        .await
        .map_err(|_| "outcome case exceeded 30 seconds".into())
        .and_then(|r| r);
        h.finish("experimental outcome differs from client failure", result)
            .await?;
        count += 1;
    }
    Ok(count)
}
