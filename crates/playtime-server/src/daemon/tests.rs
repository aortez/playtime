//! Adapter vectors use labeled synthetic storage inputs and actual writer
//! receipts. The separate service gate executes the real controller/worker.
use super::projection;
use crate::lifecycle::*;
use playtime_core::{RunMode, UtcTime};
use playtime_protocol::{api::*, worker::ControlId};
use playtime_storage::{ArchiveReader, ArchiveWriter, BatchClass, StorageLimits, fixture};
use serde_json::json;
struct Root(std::path::PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pt20-adapter-{}",
            playtime_worker_control::random_id().unwrap().as_str()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn admission(fixture: &fixture::Fixture) -> Admission {
    Admission {
        service_id: ControlId::new("0123456789abcdef0123456789abcdef".into()).unwrap(),
        run_id: fixture.descriptor.run_id.clone(),
        key: "test".into(),
        request: AdmissionRequest {
            recipe_id: "test".into(),
            mode: RunMode::Simulation,
            preview_revision: "test".into(),
        },
        request_sha256: playtime_storage::sha256(b"test"),
        selection: Selection {
            executable_sha256: playtime_storage::sha256(b"test"),
            wall_ms: 1000,
            token_limit: 1000,
        },
        accepted: Stamp {
            utc: UtcTime::new("2026-10-18T00:00:00Z").unwrap(),
            boot_ms: 0,
        },
        phase: Phase::Running,
        reason: None,
        prepared: None,
        start_attempted: true,
        endpoint: None,
        begin_authorized: true,
        cancel_requested: None,
        cancel_delivery: None,
        stop_delivery: None,
        stop_accepted: None,
        supervisor: None,
        owner_generation: 1,
        revision: 0,
        last_status: None,
        confirmed_read: None,
    }
}
fn record(f: &fixture::Fixture, seq: u64, kind: &str, data: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({"schema_version":1,"run_id":f.descriptor.run_id,"sequence":seq,"time":"2026-10-18T00:00:00Z","kind":kind,"data":data})).unwrap();
    bytes.push(b'\n');
    bytes
}
fn intent(f: &fixture::Fixture, seq: u64, effect: u64, request: &str, cap: u64) -> Vec<u8> {
    record(
        f,
        seq,
        "effect_intent",
        json!({"schema_version":1,"effect_id":effect,"effect_kind":"model","session_id":"s1","request_id":request,"tool_request_sequence":null,"maximum_tokens":cap,"deadline_ms":1000,"sources":[]}),
    )
}
fn view(root: &Root, writer: &ArchiveWriter, f: &fixture::Fixture) -> projection::Evidence {
    let queries = playtime_runner::ArchiveQueries::live(
        root.0.join("archive"),
        StorageLimits::default(),
        writer.confirmed_frontier(),
    )
    .unwrap();
    projection::project(&admission(f), &queries).unwrap()
}
#[test]
fn pending_allowances_nondispatch_and_unknown_settlements_are_separate_at_each_frontier() {
    let root = Root::new();
    let f = fixture::two_session().unwrap();
    let mut writer = ArchiveWriter::create(
        root.0.join("archive"),
        f.descriptor.clone(),
        f.manifest,
        &f.inputs,
        StorageLimits::default(),
    )
    .unwrap();
    writer
        .append(
            &[f.events[0], &intent(&f, 2, 1, "r1", 100)],
            BatchClass::Ordinary,
        )
        .unwrap();
    let first = view(&root, &writer, &f).view;
    assert_eq!(first.usage.unwrap().known_generated_tokens, 0);
    let coverage = first.accounting_coverage.unwrap();
    assert_eq!(
        (coverage.unsettled_requests, coverage.unsettled_allowance),
        (1, 100)
    );
    assert_eq!(Some(coverage.frontier), first.last_confirmed_frontier);
    let settlement = record(
        &f,
        3,
        "request_finished",
        json!({"session_id":"s1","request_id":"r1","usage":{"status":"known","generated_tokens":7}}),
    );
    writer
        .append(
            &[&settlement, &intent(&f, 4, 2, "r2", 40)],
            BatchClass::Ordinary,
        )
        .unwrap();
    let next = view(&root, &writer, &f).view;
    assert_eq!(next.usage.unwrap().budget_charged_tokens, 7);
    assert_eq!(next.accounting_coverage.unwrap().unsettled_allowance, 40);
    let nondispatch = record(
        &f,
        5,
        "request_not_dispatched",
        json!({"schema_version":1,"effect_id":2,"session_id":"s1","request_id":"r2","released_tokens":40}),
    );
    writer
        .append(
            &[&nondispatch, &intent(&f, 6, 3, "r3", 80)],
            BatchClass::Ordinary,
        )
        .unwrap();
    assert_eq!(
        view(&root, &writer, &f)
            .view
            .accounting_coverage
            .unwrap()
            .unsettled_allowance,
        80
    );
    let unknown = record(
        &f,
        7,
        "request_finished",
        json!({"session_id":"s1","request_id":"r3","usage":{"status":"unknown","reserved_tokens":80}}),
    );
    writer.append(&[&unknown], BatchClass::Ordinary).unwrap();
    let last = view(&root, &writer, &f).view;
    let usage = last.usage.unwrap();
    assert_eq!(
        (
            usage.known_generated_tokens,
            usage.unknown_reserved_tokens,
            usage.budget_charged_tokens
        ),
        (7, 80, 87)
    );
    assert!(!usage.usage_complete);
    assert_eq!(last.accounting_coverage.unwrap().unsettled_allowance, 0);
}
#[test]
fn exact_checkpoint_index_preserves_missing_empty_and_metadata_only_files() {
    let root = Root::new();
    let f = fixture::two_session().unwrap();
    let mut writer = ArchiveWriter::create(
        root.0.join("archive"),
        f.descriptor.clone(),
        f.manifest,
        &f.inputs,
        StorageLimits::default(),
    )
    .unwrap();
    for a in &f.artifacts {
        writer.publish_artifact(&a.reference, &a.bytes).unwrap();
    }
    writer.append(&f.events[..7], BatchClass::Ordinary).unwrap();
    let publish = |writer: &mut ArchiveWriter, path: &str, bytes: &[u8]| {
        let content =
            playtime_storage::content_ref(playtime_core::ArchivePath::new(path).unwrap(), bytes);
        writer.publish_artifact(&content, bytes).unwrap();
        content
    };
    let empty = publish(&mut writer, "sessions/s1/empty.txt", b"");
    let map = publish(
        &mut writer,
        "sessions/s1/map.json",
        &serde_json::to_vec(&json!({"schema_version":1,"run_id":f.descriptor.run_id,"entries":[]}))
            .unwrap(),
    );
    let mut event: serde_json::Value = serde_json::from_slice(f.events[7]).unwrap();
    let checkpoint = &event["data"]["checkpoint"];
    let provenance = publish(&mut writer, "sessions/s1/provenance.json", b"{}");
    let index = json!({"schema_version":1,"run_id":f.descriptor.run_id,"session_id":"s1","ordinal":1,"workspace":checkpoint["workspace"],"files":[{"path":"metadata.txt","size_bytes":123,"mode":420,"entry_type":"0","sha256":null,"link":null,"content":{"availability":"unavailable","reason":"metadata_only"}}],"changes":[{"path":"metadata.txt","change":"modified"}],"incoming_memory":{"status":"missing","content":empty},"outgoing_memory":{"status":"loaded","content":empty},"incoming_evidence":null,"outgoing_evidence":{"text":checkpoint["evidence"],"provenance":provenance},"source_map":map});
    let index_bytes = serde_json::to_vec(&index).unwrap();
    let reference = publish(
        &mut writer,
        "sessions/s1/checkpoint-index.json",
        &index_bytes,
    );
    event["data"]["checkpoint"]["index"] = json!(reference);
    event["data"]["checkpoint"]["memento"] = json!({"status":"available","content":empty});
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    writer.append(&[&bytes], BatchClass::Ordinary).unwrap();
    let projected = view(&root, &writer, &f);
    let checkpoint = &projected.checkpoints[0];
    assert_eq!(
        checkpoint.incoming_memory,
        ContentAvailability::Unavailable {
            reason: "missing".into()
        }
    );
    assert!(
        matches!(&checkpoint.outgoing_memory,ContentAvailability::Available{content,..} if content.size_bytes==0)
    );
    assert_eq!(
        checkpoint.files[0].content,
        ContentAvailability::Unavailable {
            reason: "metadata_only".into()
        }
    );
    assert!(projected.artifacts.iter().any(|a| a.content == empty));
    let ContentAvailability::Available { content, .. } = checkpoint.index.as_ref().unwrap() else {
        panic!("index unavailable")
    };
    let reader = ArchiveReader::open(root.0.join("archive"), StorageLimits::default()).unwrap();
    assert_eq!(
        reader.read_content(&projected.snapshot, content).unwrap(),
        index_bytes
    );
}

#[tokio::test]
async fn cpu_supervisor_refuses_real_workshop_kind_before_any_files_or_processes() {
    use crate::daemon::test_supervisor::CpuTestSupervisor;
    use crate::lifecycle::ports::Supervisor;
    use playtime_protocol::worker::{Binding, WorkerKind};
    let id = || ControlId::new("0123456789abcdef0123456789abcdef".into()).unwrap();
    let launch = PreparedLaunch {
        binding: Binding {
            service_id: id(),
            run_id: playtime_core::RunId::new("synthetic-docker").unwrap(),
            manifest_sha256: playtime_storage::sha256(b"manifest"),
            executable_sha256: playtime_storage::sha256(b"worker"),
            grant_id: id(),
            worker_id: id(),
            invocation_id: id(),
            boot_id: id(),
            unit: "synthetic.service".into(),
            kind: WorkerKind::DockerScripted,
        },
        executable: "/must-not-be-opened/worker".into(),
        argv: vec![],
        bootstrap: "/must-not-be-opened/bootstrap".into(),
        private_dir: "/must-not-be-opened".into(),
        socket: "/must-not-be-opened/control.sock".into(),
        startup_deadline_boot_ms: 1,
    };
    assert!(matches!(
        CpuTestSupervisor::default().start(&launch).await,
        Err(Error::Invalid(
            "CPU test supervisor accepts only controller simulation"
        ))
    ));
}

#[test]
fn live_admission_is_busy_while_another_worker_holds_the_shared_lock() {
    let root = std::env::temp_dir().join(format!("pt72-lock-{}", std::process::id()));
    playtime_worker_control::create_private_dir(&root).unwrap();
    let path = root.join("live-model.lock");
    super::archive::probe_live_lock(&path).unwrap();
    let held = playtime_worker_control::FileLock::acquire(&path).unwrap();
    assert!(matches!(
        super::archive::probe_live_lock(&path),
        Err(Error::Busy)
    ));
    drop(held);
    // The probe never keeps the lock.
    super::archive::probe_live_lock(&path).unwrap();
    playtime_worker_control::FileLock::acquire(&path).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn live_endpoints_resolve_to_frozen_addresses_and_pin_ip_literals() {
    let local = playtime_ollama::Endpoint::parse("http://localhost:11434").unwrap();
    let addresses = super::archive::resolve(&local).await.unwrap();
    assert!(!addresses.is_empty() && addresses.iter().all(|ip| ip.is_loopback()));
    for (url, ip) in [
        ("http://192.0.2.10:11434", "192.0.2.10"),
        ("http://[2001:db8::10]:11434", "2001:db8::10"),
    ] {
        let literal = playtime_ollama::Endpoint::parse(url).unwrap();
        let ip: std::net::IpAddr = ip.parse().unwrap();
        assert_eq!(super::archive::resolve(&literal).await.unwrap(), [ip]);
    }
    // The worker can't carry a link-local scope.
    let link_local = playtime_ollama::Endpoint::parse("http://[fe80::1]:11434").unwrap();
    assert!(super::archive::resolve(&link_local).await.is_err());
}
