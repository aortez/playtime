//! Normal production-service process gate. The only simulated components are
//! model/workshop effects and the explicitly selected CPU supervisor.
use crate::{Result, TestRoot, isolated, run_command};
use playtime_client::{CheckpointIndexRead, Client, Observation, RetryPolicy};
use playtime_core::{RunId, StopReason};
use playtime_protocol::api::*;
use playtime_runner::ArchiveQueries;
use playtime_storage::StorageLimits;
use serde_json::json;
use std::{
    collections::BTreeMap,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    process::Child,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;

pub struct NormalService {
    child: Child,
    _cleanup: WorkerCleanup,
    pub root: TestRoot,
    pub connection: PathBuf,
    pub client: Client,
    pub run: RunId,
    pub view: RunView,
}
impl NormalService {
    pub async fn start(cli: &Path, daemon: &Path, bundle: &Path) -> Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "pt20-{}",
            &playtime_local::Token::generate()?.expose()[..12]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let root = TestRoot(directory);
        let service = root.join("service");
        std::fs::DirBuilder::new().mode(0o700).create(&service)?;
        let cleanup = WorkerCleanup(service.clone());
        let mut command = isolated(daemon, &root);
        command
            .args(["--listen", "127.0.0.1:0", "--cpu-test-supervisor", "--root"])
            .arg(&service)
            .arg("--bundle")
            .arg(bundle)
            .env("PATH", "/no-runtime-cargo");
        let mut child = command.spawn()?;
        let connection = service.join(".playtime/connection.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        let client = loop {
            if let Some(status) = child.try_wait()? {
                return Err(format!("daemon failed readiness: {status}").into());
            }
            if let Ok(client) = Client::from_connection(&connection).await {
                break client;
            }
            if Instant::now() >= deadline {
                return Err("daemon readiness deadline".into());
            }
            sleep(Duration::from_millis(10)).await;
        };
        let mut records = Vec::new();
        let launched = run_command(
            cli,
            &root,
            Some(&connection),
            [
                "run",
                "two-session",
                "--mode",
                "simulation",
                "--idempotency-key",
                "normal-service-gate",
                "--format",
                "json",
            ],
            &mut records,
        )
        .await?;
        launched.expect(0)?;
        let key = IdempotencyKey::new("normal-service-gate")?;
        let acceptance = client.lookup_key(&key).await?;
        let run = acceptance.run_id.clone();
        // Construct the cleanup guard before any assertions against a worker.
        let mut this = Self {
            child,
            _cleanup: cleanup,
            root,
            connection,
            client,
            run,
            view: acceptance.run,
        };
        this.verify(cli, &mut records).await?;
        Ok(this)
    }
    async fn verify(&mut self, cli: &Path, records: &mut Vec<serde_json::Value>) -> Result<()> {
        if self.view.backend.kind != BackendKind::Controller {
            return Err("service substituted fixture output".into());
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let initial = loop {
            let view = self.client.run(&self.run).await?;
            if view.event_anchors.is_some() {
                break view;
            }
            if Instant::now() >= deadline {
                return Err("first worker confirmation deadline".into());
            }
            sleep(Duration::from_millis(10)).await;
        };
        let anchors = initial.event_anchors.unwrap();
        let mut observed = BTreeMap::new();
        let mut stream = self.client.follow(
            &self.run,
            Some(anchors.history_cursor.clone()),
            RetryPolicy::default(),
        );
        let cancel = CancellationToken::new();
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                match stream.next(&cancel).await? {
                    Observation::Record(record) => {
                        observed.insert(record.event.sequence.get(), record.event.clone());
                        stream.acknowledge(&record)?;
                    }
                    Observation::End(end) => {
                        if end.status != StreamEndStatus::CommittedCompletion {
                            return Err("unexpected stream end".into());
                        }
                        break;
                    }
                    _ => (),
                }
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await
        .map_err(|_| "service event deadline")??;
        self.view = self.client.run(&self.run).await?;
        if self.view.orchestration.state != OrchestrationState::Terminal
            || self.view.outcome.as_ref().map(|o| o.reason) != Some(StopReason::AgentFinished)
            || self.view.usage.as_ref().map(|u| u.known_generated_tokens) != Some(26)
            || self
                .view
                .accounting_coverage
                .as_ref()
                .is_none_or(|c| c.unsettled_allowance != 0)
        {
            return Err("normal terminal/accounting projection mismatch".into());
        }
        let final_frontier = self
            .view
            .last_confirmed_frontier
            .as_ref()
            .ok_or("missing final frontier")?;
        if observed.len() as u64 != final_frontier.prefix.sequence {
            return Err("SSE history/live coverage gap".into());
        }
        // Old anchors remain valid after the marker has advanced.
        let old = self
            .client
            .history(
                &self.run,
                &PageQuery {
                    cursor: Some(anchors.history_cursor),
                    limit: Some(1000),
                },
            )
            .await?;
        if old.frontier != anchors.frontier {
            return Err("historical cursor advanced".into());
        }
        let mut cursor = self
            .view
            .event_anchors
            .as_ref()
            .map(|a| a.history_cursor.clone());
        let mut history = Vec::new();
        loop {
            let page = self
                .client
                .history(
                    &self.run,
                    &PageQuery {
                        cursor,
                        limit: Some(7),
                    },
                )
                .await?;
            history.extend(page.events);
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }
        if history != observed.into_values().collect::<Vec<_>>() {
            return Err("paged/SSE evidence differs".into());
        }
        let checkpoints = self
            .client
            .checkpoints(&self.run, &PageQuery::default())
            .await?;
        if checkpoints.items.len() != 2 {
            return Err("expected two actual controller sessions".into());
        }
        for checkpoint in &checkpoints.items {
            let CheckpointIndexRead::Supported { index, .. } = self
                .client
                .checkpoint_index(&self.run, checkpoint, &cancel)
                .await?
            else {
                return Err("missing exact checkpoint index".into());
            };
            if index.session_id.as_str() == "s2"
                && (index.incoming_memory.status != "loaded"
                    || index.incoming_evidence.is_none()
                    || index.outgoing_evidence.is_none())
            {
                return Err("second session provenance missing".into());
            }
        }
        let mut cursor = None;
        let mut artifacts = Vec::new();
        loop {
            let page = self
                .client
                .artifacts(
                    &self.run,
                    &PageQuery {
                        cursor,
                        limit: Some(3),
                    },
                )
                .await?;
            artifacts.extend(page.items);
            if page.next_cursor.is_none() {
                break;
            }
            cursor = page.next_cursor;
        }
        let saved = artifacts
            .iter()
            .find(|a| a.kind == "file_version" && a.name == "persistent.txt")
            .ok_or("saved file absent")?;
        let mut bytes = Vec::new();
        self.client
            .download(&self.run, saved, &mut bytes, &cancel)
            .await?;
        if bytes != b"survives" {
            return Err("downloaded saved file differs".into());
        }
        for artifact in &artifacts {
            let mut bytes = Vec::new();
            self.client
                .download(&self.run, artifact, &mut bytes, &cancel)
                .await?;
            if playtime_storage::sha256(&bytes) != artifact.content.sha256
                || bytes.len() as u64 != artifact.content.size_bytes
            {
                return Err("artifact verification failed".into());
            }
        }
        let response = run_command(
            cli,
            &self.root,
            Some(&self.connection),
            ["runs", "show", self.run.as_str(), "--format", "json"],
            records,
        )
        .await?;
        response.expect(0)?;
        // A same-key replay remains the original admission after completion.
        let replay = run_command(
            cli,
            &self.root,
            Some(&self.connection),
            [
                "run",
                "two-session",
                "--mode",
                "simulation",
                "--idempotency-key",
                "normal-service-gate",
                "--format",
                "json",
            ],
            records,
        )
        .await?;
        replay.expect(0)?;
        if self.client.runs(&PageQuery::default()).await?.items.len() != 1 {
            return Err("replay created another run".into());
        }
        let source = self.root.join("service/runs").join(self.run.as_str());
        let copied = self.root.join("copied-archive");
        copy(&source, &copied)?;
        let offline = ArchiveQueries::offline(&copied, StorageLimits::default())?;
        let actual = offline.run()?;
        if actual.usage != self.view.usage.clone().ok_or("missing usage")?
            || actual.frontier.committed_sequence != final_frontier.prefix.sequence
            || actual.frontier.prefix_sha256 != final_frontier.prefix.sha256
            || offline.checkpoints()?.len() != 2
        {
            return Err("independent copied archive disagrees".into());
        }
        // Exact raw JSONL preimage, independently of HTTP reserialization.
        let raw = std::fs::read(copied.join("events.jsonl"))?;
        if playtime_storage::sha256(&raw) != final_frontier.prefix.sha256
            || raw.len() as u64 != final_frontier.prefix.byte_offset
        {
            return Err("copied exact prefix hash differs".into());
        }
        for checkpoint in offline.checkpoints()? {
            let index = checkpoint.index.ok_or("offline index absent")?;
            let outgoing = offline.content(&index.outgoing_memory.content)?;
            let expected = if index.ordinal.get() == 1 {
                "Read persistent.txt in the next session."
            } else {
                "The two-session fixture is complete."
            };
            if outgoing != expected.as_bytes() {
                return Err("exact memory mismatch".into());
            }
            let source_map: serde_json::Value =
                serde_json::from_slice(&offline.content(&index.source_map)?)?;
            for entry in source_map["entries"]
                .as_array()
                .ok_or("invalid source map")?
            {
                let sequence = entry["archive_sequence"]
                    .as_u64()
                    .ok_or("invalid sequence")?;
                let line = raw
                    .split_inclusive(|b| *b == b'\n')
                    .nth(sequence as usize - 1)
                    .ok_or("missing source record")?;
                if playtime_storage::sha256(line).as_str()
                    != entry["record_sha256"].as_str().ok_or("invalid digest")?
                {
                    return Err("source-map hash mismatch".into());
                }
            }
        }
        println!(
            "{}",
            json!({"case":"normal","run_id":self.run,"frontier":final_frontier,"sessions":2,"known_tokens":26,"verified_artifacts":artifacts.len(),"copied_archive":true,"supervision":"CPU test; no systemd enforcement"})
        );
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        std::fs::remove_dir_all(self.root.join("service"))?;
        let offline =
            ArchiveQueries::offline(self.root.join("copied-archive"), StorageLimits::default())?;
        if offline.run()?.usage.known_generated_tokens != 26 {
            return Err("folder-only inspection failed after original service root removal".into());
        }
        Ok(())
    }
}
struct WorkerCleanup(PathBuf);
impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        // Only invocation files under this owned test root; authenticate kernel
        // start ticks before signaling. The production supervisor never uses this.
        let Ok(entries) = std::fs::read_dir(self.0.join(".playtime/workers")) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path().join("cpu-test-invocation.json")) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(pid) = value["pid"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let start = stat
                .rsplit_once(')')
                .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                .and_then(|s| s.parse::<u64>().ok());
            if start == value["start_ticks"].as_u64()
                && let Some(pid) = rustix::process::Pid::from_raw(pid as i32)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}
fn copy(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            return Err("unexpected archive link".into());
        }
    }
    Ok(())
}
