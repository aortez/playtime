//! CPU fake-port tests of the production native adapter + real archive driver.
//! Commands remain JSON data; no shell, Docker, image or model is used.
use super::*;
use playtime_docker::transport::{Fault, Operation, Outcome as TransportOutcome, Transport};
use playtime_protocol::worker::{ControlId, WorkerKind};
use playtime_runner::{
    ArchiveQueries, Cancellation, DriverClock, DriverOptions, PreparedExecution,
};
use std::sync::atomic::{AtomicU32, Ordering};
#[path = "../../playtime-docker/tests/support/mod.rs"]
mod fake;

#[derive(Clone, Copy, PartialEq)]
enum FaultCase {
    None,
    SecondCapture,
    ToolDeadline,
    DaemonLost,
    ChangedDaemon,
    CancelTool,
    CancelCapture,
    CancelPublication,
    LostOwner,
}
struct Port {
    fake: fake::Fake,
    private: PathBuf,
    archive: PathBuf,
    case: FaultCase,
    exports: AtomicU32,
    bridges: AtomicU32,
    cancellation: Cancellation,
}
#[async_trait::async_trait]
impl Transport for Port {
    async fn execute(
        &self,
        op: Operation,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> TransportOutcome {
        if let Operation::CreateVolume(p) | Operation::CreateContainer(p) = &op {
            let file = self.private.join(format!(
                "docker-{}-{}.json",
                match p.role {
                    playtime_docker::Role::Volume => "volume",
                    playtime_docker::Role::Holder => "holder",
                    playtime_docker::Role::Agent => "agent",
                },
                p.session.unwrap_or(0)
            ));
            let saved: serde_json::Value = serde_json::from_slice(
                &playtime_worker_control::read_private(&file, 32768).unwrap(),
            )
            .unwrap();
            assert_eq!(saved["proposal"], serde_json::to_value(p).unwrap());
            let prefix = &saved["intent"]["frontier"];
            let raw = std::fs::read(self.archive.join("events.jsonl")).unwrap();
            let bytes = prefix["committed_bytes"].as_u64().unwrap() as usize;
            assert_eq!(
                playtime_storage::sha256(&raw[..bytes]).as_str(),
                prefix["prefix_sha256"].as_str().unwrap()
            );
            let effect = saved["intent"]["effect"].as_u64().unwrap();
            assert!(
                raw[..bytes]
                    .split(|b| *b == b'\n')
                    .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
                    .any(|e| e["kind"] == "effect_intent" && e["data"]["effect_id"] == effect)
            );
            self.fake.journal.proposals.lock().unwrap().push(p.clone());
        }
        if let Operation::Export { .. } = &op {
            let number = self.exports.fetch_add(1, Ordering::SeqCst) + 1;
            if self.case == FaultCase::SecondCapture && number == 2 {
                return fake::lost(Fault::Deadline);
            }
            if self.case == FaultCase::CancelCapture {
                self.cancellation.cancel();
                cancel.cancelled().await;
                return fake::lost(Fault::Cancelled);
            }
        }
        if let Operation::Bridge { payload, .. } = &op {
            self.bridges.fetch_add(1, Ordering::SeqCst);
            match self.case {
                FaultCase::LostOwner => panic!("injected native effect owner loss"),
                FaultCase::ToolDeadline => return fake::lost(Fault::Deadline),
                FaultCase::DaemonLost => {
                    self.fake.state.lock().unwrap().available = false;
                    return fake::lost(Fault::Io);
                }
                FaultCase::ChangedDaemon => {
                    self.fake.state.lock().unwrap().changed = true;
                    return fake::lost(Fault::Io);
                }
                FaultCase::CancelTool => {
                    self.cancellation.cancel();
                    cancel.cancelled().await;
                    return fake::lost(Fault::Cancelled);
                }
                _ => (),
            }
            let value: serde_json::Value = serde_json::from_slice(payload).unwrap();
            if value["arguments"]["timeout_seconds"] == 0.1 {
                return fake::response(
                    serde_json::json!({"stdout":"","stderr":"","exit_code":-9,"timed_out":true,"output_truncated":false,"duration_seconds":0.1}),
                );
            }
        }
        self.fake.execute(op, deadline, cancel).await
    }
}
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Clock;
impl DriverClock for Clock {
    fn now_ms(&self) -> u64 {
        boot_ms()
    }
    fn utc(&self) -> playtime_core::UtcTime {
        playtime_core::UtcTime::new("2026-10-18T00:00:00Z").unwrap()
    }
    fn idle(&mut self, _: u64, _: bool) {
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn execute(
    case: FaultCase,
    hooks: Option<Box<dyn playtime_runner::WriterHooks>>,
) -> (Temp, Arc<Port>, playtime_runner::RunReport) {
    let root = std::env::temp_dir().join(format!(
        "pt66-native-{}",
        Ownership::random_nonce().unwrap()
    ));
    playtime_worker_control::create_private_dir(&root).unwrap();
    let temp = Temp(root.clone());
    let archive = root.join("archive");
    let cancel = Cancellation::default();
    let port = Arc::new(Port {
        fake: fake::Fake {
            state: Mutex::new(fake::State::default()),
            journal: Arc::new(fake::Journal::default()),
        },
        private: root.clone(),
        archive: archive.clone(),
        case,
        exports: AtomicU32::new(0),
        bridges: AtomicU32::new(0),
        cancellation: cancel.clone(),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (engine, ready) = runtime.block_on(async {
        let engine = Engine::connect(port.clone(), fake::deadline(), CancellationToken::new())
            .await
            .unwrap();
        let ready = engine
            .readiness(
                playtime_runner::docker::IMAGE_LOCK,
                fake::deadline(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        (engine, ready)
    });
    let profile = Profile::freeze(
        playtime_docker::transport::Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        },
        engine.identity().clone(),
    )
    .unwrap();
    let prepared = playtime_runner::prepare_docker_with_inputs(
        &archive,
        playtime_core::RunId::new("synthetic-native").unwrap(),
        &profile,
        playtime_runner::capture_demo_worker().unwrap(),
        vec![],
    )
    .unwrap();
    let id = || ControlId::new("0123456789abcdef0123456789abcdef".into()).unwrap();
    let binding = Binding {
        service_id: id(),
        run_id: prepared.identity.run_id.clone(),
        manifest_sha256: prepared.identity.input_manifest.sha256.clone(),
        executable_sha256: playtime_storage::ArchiveReader::open(&archive, profile.storage.clone())
            .unwrap()
            .manifest()
            .worker
            .executable
            .sha256
            .clone(),
        grant_id: id(),
        worker_id: id(),
        invocation_id: id(),
        boot_id: id(),
        unit: "synthetic-native.service".into(),
        kind: WorkerKind::DockerScripted,
    };
    let journal = Arc::new(Journal {
        directory: root.clone(),
        binding,
        intent: Mutex::new(None),
        slot: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    let owner = Ownership {
        service: "synthetic-service".into(),
        run: "synthetic-native".into(),
        grant: "synthetic-grant".into(),
        invocation: "synthetic-invocation".into(),
        nonce: Ownership::random_nonce().unwrap(),
        daemon: engine.identity().clone(),
    };
    let effects = NativeEffects {
        prepared: Some((ready, owner, journal, root)),
        profile,
        sender: None,
        receiver: None,
        active: None,
        retiring: false,
        input: None,
        candidate: None,
    };
    let owned = PreparedExecution::acquire_docker(&archive, &prepared, Box::new(effects)).unwrap();
    let mut options = DriverOptions {
        cancellation: cancel.clone(),
        ..DriverOptions::default()
    };
    if case == FaultCase::CancelPublication {
        options.writer_hooks = Box::new(CancelPublication(cancel));
    }
    if hooks.is_some() {
        options.publication_wait = Duration::from_millis(50);
    }
    if let Some(hooks) = hooks {
        options.writer_hooks = hooks;
    }
    let report = owned.execute(&mut Clock, options).unwrap();
    (temp, port, report)
}
#[test]
fn native_ports_use_real_controller_writer_and_durable_proposals() {
    let (temp, port, report) = execute(FaultCase::None, None);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let result = report.result.unwrap();
    assert_eq!(result.reason, playtime_core::StopReason::AgentFinished);
    assert_eq!(result.usage.known_generated_tokens, 160);
    assert_eq!(result.usage.unknown_reserved_tokens, 0);
    assert_eq!(result.sessions_started, 2);
    assert_eq!(port.exports.load(Ordering::SeqCst), 2);
    assert_eq!(port.bridges.load(Ordering::SeqCst), 5);
    assert_eq!(port.fake.journal.proposals.lock().unwrap().len(), 4);
    assert!(port.fake.state.lock().unwrap().containers.is_empty());
    assert!(port.fake.state.lock().unwrap().volumes.is_empty());
    let archive = ArchiveQueries::offline(temp.0.join("archive"), Default::default()).unwrap();
    assert_eq!(archive.checkpoints().unwrap().len(), 2);
    assert!(
        report.confirmed.usage().content_bytes
            > playtime_storage::ArchiveReader::open(temp.0.join("archive"), Default::default())
                .unwrap()
                .manifest()
                .worker
                .executable
                .size_bytes
    );
}
#[test]
fn capture_failure_after_second_handoff_preserves_160_tokens_and_checkpoint_one() {
    let (temp, port, report) = execute(FaultCase::SecondCapture, None);
    assert!(report.terminal_committed);
    let result = report.result.unwrap();
    assert_eq!(
        result.reason,
        playtime_core::StopReason::InfrastructureFailure
    );
    assert_eq!(result.usage.known_generated_tokens, 160);
    assert_eq!(result.last_checkpoint.unwrap().ordinal, 1);
    assert_eq!(
        result.cleanup,
        playtime_protocol::events::CleanupOutcome::Complete
    );
    assert!(port.fake.state.lock().unwrap().containers.is_empty());
    let archive = ArchiveQueries::offline(temp.0.join("archive"), Default::default()).unwrap();
    assert_eq!(archive.checkpoints().unwrap().len(), 1);
    let raw = std::fs::read_to_string(temp.0.join("archive/events.jsonl")).unwrap();
    assert!(raw.contains("Two sessions complete:"));
    assert!(!raw.contains("sessions/s2/checkpoint-index.json"));
}
#[test]
fn ambiguous_tools_and_daemon_loss_never_dispatch_another_model_or_tool() {
    for case in [
        FaultCase::ToolDeadline,
        FaultCase::DaemonLost,
        FaultCase::ChangedDaemon,
    ] {
        let (_temp, port, report) = execute(case, None);
        let result = report.result.unwrap();
        assert_eq!(
            result.reason,
            playtime_core::StopReason::InfrastructureFailure
        );
        assert_eq!(result.model_calls, 1);
        assert_eq!(port.bridges.load(Ordering::SeqCst), 1);
        if case != FaultCase::ToolDeadline {
            assert!(matches!(
                result.cleanup,
                playtime_protocol::events::CleanupOutcome::Failed { .. }
            ));
        }
    }
}
#[test]
fn cancellation_during_tool_and_capture_retains_owner_until_bounded_cleanup() {
    for case in [FaultCase::CancelTool, FaultCase::CancelCapture] {
        let (_temp, port, report) = execute(case, None);
        assert_eq!(
            report.result.unwrap().reason,
            playtime_core::StopReason::Interrupted
        );
        assert!(port.fake.state.lock().unwrap().containers.is_empty());
        assert!(port.fake.state.lock().unwrap().volumes.is_empty());
        assert_eq!(port.fake.journal.proposals.lock().unwrap().len(), 3);
    }
}

struct CancelPublication(Cancellation);
impl playtime_runner::WriterHooks for CancelPublication {
    fn before(
        &mut self,
        c: &playtime_runner::CommitContext,
        _: &mut playtime_storage::ArchiveWriter,
    ) -> Result<()> {
        if c.checkpoint_effect.is_some() {
            self.0.cancel();
            std::thread::sleep(Duration::from_millis(15));
        }
        Ok(())
    }
}
#[test]
fn cancellation_during_publication_keeps_receipt_and_does_not_start_session_two() {
    let (_temp, port, report) = execute(FaultCase::CancelPublication, None);
    assert_eq!(
        report.result.as_ref().unwrap().reason,
        playtime_core::StopReason::Interrupted
    );
    assert_eq!(report.result.unwrap().usage.known_generated_tokens, 96);
    assert_eq!(port.exports.load(Ordering::SeqCst), 1);
    assert!(report.confirmed.frontier().committed_sequence > 0);
    assert!(port.fake.state.lock().unwrap().containers.is_empty());
}
struct PublicationFault {
    count: u32,
    late: bool,
}
impl playtime_runner::WriterHooks for PublicationFault {
    fn before(
        &mut self,
        c: &playtime_runner::CommitContext,
        _: &mut playtime_storage::ArchiveWriter,
    ) -> Result<()> {
        if c.checkpoint_effect.is_some() {
            self.count += 1;
            if self.count == 2 {
                if self.late {
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    return Err(Error::Uncertain(
                        "injected second publication failure".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
#[test]
fn native_publication_failure_and_late_receipt_keep_prior_frontier_and_original_writer() {
    for late in [false, true] {
        let (temp, port, report) = execute(
            FaultCase::None,
            Some(Box::new(PublicationFault { count: 0, late })),
        );
        assert!(!report.terminal_committed);
        assert_eq!(report.result.unwrap().usage.known_generated_tokens, 160);
        assert!(port.fake.state.lock().unwrap().containers.is_empty());
        let reader =
            playtime_storage::ArchiveReader::open(temp.0.join("archive"), Default::default())
                .unwrap();
        let snapshot = reader.live_snapshot(&report.confirmed).unwrap();
        assert_eq!(snapshot.inspection().checkpoints.len(), 1);
        if late {
            let pending = report
                .pending_publication
                .expect("late write owner retained");
            assert!(playtime_storage::WriterLock::try_acquire(temp.0.join("archive")).is_err());
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let receipt = loop {
                if let Some(r) = pending.poll() {
                    break r.unwrap();
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(
                reader
                    .live_snapshot(&receipt)
                    .unwrap()
                    .inspection()
                    .checkpoints
                    .len(),
                2
            );
        }
    }
}
#[test]
fn cumulative_capture_reservation_uses_confirmed_counts_and_exhausts_without_reset() {
    let (_temp, _port, report) = execute(FaultCase::None, None);
    let mut profile = Profile::freeze(
        playtime_docker::transport::Selection {
            executable: "/synthetic/docker".into(),
            socket: "/synthetic/socket".into(),
            executable_sha256: "a".repeat(64),
        },
        playtime_docker::DaemonIdentity {
            id: "synthetic".into(),
            name: "synthetic".into(),
            cli_version: "28".into(),
            server_version: "28".into(),
            api_version: "1.51".into(),
            kernel: "synthetic".into(),
        },
    )
    .unwrap();
    let usage = report.confirmed.usage();
    let first = capture_profile(&profile, &report.confirmed).unwrap();
    let actual: serde_json::Value = serde_json::to_value(&first).unwrap();
    assert_eq!(
        actual["remaining_references"],
        profile.storage.references - usage.references - 16
    );
    assert_eq!(
        actual["remaining_bytes"],
        profile.storage.verified_content_bytes
            - usage.content_bytes
            - profile.storage.manifest_bytes * 2
            - profile.capture.memory_bytes * 2
    );
    profile.storage.references = usage.references + 18;
    assert!(capture_profile(&profile, &report.confirmed).is_err());
    assert!(
        CaptureProfile::prepare(
            profile.capture.clone(),
            profile.resources.workspace_bytes,
            &profile.storage,
            ArchiveUsage::default()
        )
        .is_ok(),
        "a fresh zero would wrongly admit this capture"
    );
    profile.storage.references = u64::MAX;
    profile.storage.manifest_bytes = u64::MAX;
    assert!(
        capture_profile(&profile, &report.confirmed).is_err(),
        "overflow must reject"
    );
}

#[test]
fn lost_native_owner_cannot_replace_itself_or_replay_tools_and_keeps_proposals() {
    let (temp, port, report) = execute(FaultCase::LostOwner, None);
    let result = report.result.unwrap();
    assert_eq!(
        result.reason,
        playtime_core::StopReason::InfrastructureFailure
    );
    assert_eq!(result.model_calls, 1);
    assert_eq!(port.bridges.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.cleanup,
        playtime_protocol::events::CleanupOutcome::Failed { .. }
    ));
    assert_eq!(port.fake.journal.proposals.lock().unwrap().len(), 3);
    assert_eq!(
        std::fs::read_dir(&temp.0)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("docker-"))
            .count(),
        3
    );
    assert_eq!(
        port.fake.state.lock().unwrap().containers.len(),
        2,
        "uncertainty is not cleanup evidence"
    );
}
