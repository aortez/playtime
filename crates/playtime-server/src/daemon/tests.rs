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
        operator_release: None,
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
fn continuous_accounting_uses_v2_intents_without_inventing_sessions() {
    let root = Root::new();
    let mut f = fixture::two_session().unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(f.manifest).unwrap();
    manifest["schema_version"] = json!(2);
    let bytes = serde_json::to_vec(&manifest).unwrap();
    f.descriptor.input_manifest =
        playtime_storage::content_ref(f.descriptor.input_manifest.path.clone(), &bytes);
    let mut writer = ArchiveWriter::create(
        root.0.join("archive"),
        f.descriptor.clone(),
        &bytes,
        &f.inputs,
        StorageLimits::default(),
    )
    .unwrap();
    let request = json!({"effect_id":1,"request_id":"r1","call":1,"scope":{"kind":"run"},"purpose":"compaction"});
    let pending = record(
        &f,
        2,
        "execution_v2",
        json!({"schema_version":2,"fact":{"kind":"effect_intent","data":{"effect_id":1,"effect_kind":"model","request":request,"tool_request_sequence":null,"maximum_tokens":512,"deadline_ms":1000,"sources":[]}}}),
    );
    let started = record(
        &f,
        1,
        "run_started",
        json!({"mode":f.descriptor.mode,"input_manifest":f.descriptor.input_manifest}),
    );
    writer
        .append(&[&started, &pending], BatchClass::Ordinary)
        .unwrap();
    let first = view(&root, &writer, &f).view;
    let coverage = first.accounting_coverage.unwrap();
    assert_eq!(
        (coverage.unsettled_requests, coverage.unsettled_allowance),
        (1, 512)
    );
    let accounting = first.request_accounting.unwrap();
    assert_eq!(
        accounting.by_purpose[&playtime_core::RequestPurpose::Compaction].admitted_calls,
        1
    );
    assert!(first.checkpoint.is_none());
    let settled = record(
        &f,
        3,
        "execution_v2",
        json!({"schema_version":2,"fact":{"kind":"request_settled","data":{"request":request,"usage":{"status":"unknown","reserved_tokens":512}}}}),
    );
    writer.append(&[&settled], BatchClass::Ordinary).unwrap();
    let last = view(&root, &writer, &f).view;
    assert_eq!(last.accounting_coverage.unwrap().unsettled_allowance, 0);
    let accounting = last.request_accounting.unwrap();
    let usage = &accounting.by_purpose[&playtime_core::RequestPurpose::Compaction].usage;
    assert_eq!(usage.unknown_requests, 1);
    assert_eq!(usage.unknown_reserved_tokens, 512);
    assert!(!usage.usage_complete);
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

#[test]
fn daemon_restarts_replace_one_docker_config_directory() {
    let root = Root::new();
    let private = root.0.join(".playtime");
    playtime_worker_control::create_private_dir(&private).unwrap();
    for _ in 0..3 {
        // Each start's `Cli::resolve` creates the directory new.
        let config = super::archive::docker_config(&root.0).unwrap();
        playtime_worker_control::create_private_dir(&config).unwrap();
    }
    let names: Vec<_> = std::fs::read_dir(&private)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["docker-config"]);
    let stray = private.join("docker-config/stray");
    std::fs::write(&stray, b"").unwrap();
    assert!(super::archive::docker_config(&root.0).is_err());
    assert!(stray.exists());
}

#[test]
fn preparation_is_keyed_on_the_recipe_as_well_as_the_mode() {
    use super::archive::{Launch, launch};
    use playtime_runner::docker::{LIVE_RECIPE, RECIPE};
    let request = |recipe: &str, mode| AdmissionRequest {
        recipe_id: recipe.into(),
        mode,
        preview_revision: "test".into(),
    };
    for (recipe, mode, expected) in [
        ("two-session", RunMode::Simulation, Launch::Simulation),
        (RECIPE, RunMode::ScriptedSmoke, Launch::ScriptedSmoke),
        (LIVE_RECIPE, RunMode::LiveSmoke, Launch::LiveSmoke),
        (
            "curiosity",
            RunMode::Experiment,
            Launch::Catalog("curiosity"),
        ),
    ] {
        assert_eq!(launch(&request(recipe, mode)).unwrap(), expected);
    }
    // A Docker recipe never prepares a simulation, and no mode takes another
    // mode's recipe.
    for (recipe, mode) in [
        (RECIPE, RunMode::Simulation),
        ("two-session", RunMode::ScriptedSmoke),
        (LIVE_RECIPE, RunMode::ScriptedSmoke),
        (RECIPE, RunMode::LiveSmoke),
        ("curiosity", RunMode::LiveSmoke),
    ] {
        assert!(matches!(
            launch(&request(recipe, mode)),
            Err(Error::Invalid("unsupported recipe or mode"))
        ));
    }
}

fn repository() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
#[test]
fn the_reviewed_catalog_loads_the_owner_confirmed_curiosity_recipe() {
    let recipes = super::catalog::load(&repository().join("recipes")).unwrap();
    assert_eq!(recipes.len(), 1);
    let recipe = &recipes[0];
    assert_eq!(recipe.id, "curiosity");
    let (limits, workshop) = (&recipe.parameters.limits, &recipe.parameters.workshop);
    assert_eq!(
        (
            limits.sessions.get(),
            limits.work_turns_per_session.get(),
            limits.wall_seconds.get(),
            limits.generated_tokens.get(),
            limits.tokens_per_call.get(),
            limits.handoff_tokens.get(),
            limits.context_tokens.get(),
        ),
        (10, 10, 1800, 300_000, 8192, 8192, 65_536)
    );
    assert_eq!(
        (
            limits.model_seconds.get(),
            limits.command_seconds.get(),
            limits.output_bytes.get(),
            limits.memento_bytes.get(),
            limits.log_bytes.get(),
            limits.tool_parse_retries,
            limits.tool_parse_error_limit.map(|n| n.get()),
        ),
        (400, 60, 16_384, 8192, 64 << 20, 5, Some(8))
    );
    assert_eq!(
        (
            workshop.cpu_cores,
            workshop.memory_bytes.get(),
            workshop.workspace_bytes.get(),
            workshop.tmp_bytes.get(),
            workshop.pids.get(),
            recipe.workspace_entries,
        ),
        (8.0, 10_000 << 20, 16 << 20, 1 << 30, 256, 900)
    );
    assert_eq!(recipe.parameters.sampling_seed, 1042);
    assert_eq!(recipe.model.thinking, "high");
    // The seed pack is frozen exactly, by path under /work/seeds.
    let pack = repository().join("seeds/assortment-01");
    let mut names: Vec<_> = std::fs::read_dir(&pack)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(recipe.seeds.keys().cloned().collect::<Vec<_>>(), names);
    for (name, content) in &recipe.seeds {
        assert_eq!(content.as_bytes(), std::fs::read(pack.join(name)).unwrap());
    }
    assert_eq!(
        recipe.prompt,
        std::fs::read_to_string(repository().join("prompts/inspiration.txt")).unwrap()
    );
    assert!(recipe.protocol.starts_with("You woke up one day"));
    assert!(recipe.protocol.contains("Max memento size is 8k."));
    assert!(
        recipe.inventory.starts_with(
            "Installed collections: core, computation, browser, systems\n\n# Workshop\n"
        )
    );
    assert!(recipe.inventory.ends_with(
        "\nYour workspace /work holds 16 MiB and 900 files and directories, and persists \
         between sessions; /tmp holds 1 GiB and is cleared.\n"
    ));
}
#[test]
fn invalid_catalogs_fail_before_launch_with_the_file_and_reason() {
    let reviewed = std::fs::read_to_string(repository().join("recipes/curiosity.toml")).unwrap();
    let load = |name: &str, text: &str, seeds: &[(&str, &[u8])]| {
        let root = Root::new();
        std::fs::create_dir(root.0.join("recipes")).unwrap();
        std::fs::write(root.0.join("recipes").join(name), text).unwrap();
        for (path, bytes) in seeds {
            let path = root.0.join("seeds/assortment-01").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        super::catalog::load(&root.0.join("recipes"))
    };
    let seed: &[(&str, &[u8])] = &[("ABOUT.txt", b"About.\n")];
    load("curiosity.toml", &reviewed, seed).unwrap();
    for (name, text, seeds, reason) in [
        ("other.toml", reviewed.clone(), seed, "match its file name"),
        (
            "curiosity.toml",
            reviewed.replace("sessions = 10\n", "sessions = 21\n"),
            seed,
            "exceeds its bound",
        ),
        (
            "curiosity.toml",
            reviewed.replace("workspace_bytes = 16777216", "workspace_bytes = 1073741824"),
            seed,
            "exceeds its bound",
        ),
        (
            "curiosity.toml",
            reviewed.replace("pids = 256", "pids = 256\ninodes = 900"),
            seed,
            "unknown field",
        ),
        (
            "curiosity.toml",
            reviewed.replace("\"seeds/assortment-01\"", "\"../seeds\""),
            seed,
            "plain path",
        ),
        ("curiosity.toml", reviewed.clone(), &[], "No such file"),
        (
            "curiosity.toml",
            reviewed.clone(),
            &[("binary.dat", b"\xff\xfe")],
            "not UTF-8",
        ),
    ] {
        let error = load(name, &text, seeds).unwrap_err();
        assert!(
            error.contains(name) || error.contains("assortment-01"),
            "{error}"
        );
        assert!(error.contains(reason), "{reason}: {error}");
    }
    // Links and other non-regular entries are refused, not followed.
    let root = Root::new();
    std::fs::create_dir(root.0.join("recipes")).unwrap();
    std::fs::write(root.0.join("recipes/curiosity.toml"), &reviewed).unwrap();
    std::fs::create_dir_all(root.0.join("seeds/assortment-01")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", root.0.join("seeds/assortment-01/link")).unwrap();
    let error = super::catalog::load(&root.0.join("recipes")).unwrap_err();
    assert!(error.contains("without links"), "{error}");
    assert!(super::catalog::load(&root.0.join("seeds")).is_err());
}
#[test]
fn run_views_name_the_backend_each_mode_runs_on() {
    let mut record = admission(&fixture::two_session().unwrap());
    for (mode, backend) in [
        (RunMode::Simulation, projection::backend()),
        (RunMode::ScriptedSmoke, projection::docker_backend()),
        (RunMode::LiveSmoke, projection::live_backend()),
        (RunMode::Experiment, projection::live_backend()),
    ] {
        record.request.mode = mode;
        assert_eq!(projection::unprepared(&record).backend, backend, "{mode:?}");
    }
}

#[test]
fn docker_ownership_journal_binds_records_to_the_exact_launch() {
    use playtime_docker::{DaemonIdentity, Limits, Ownership, Proposal, Role};
    use playtime_protocol::worker::{Binding, WorkerKind};
    use playtime_worker_control::{create_private_dir, write_once};
    let root = Root::new();
    let private = root.0.join("worker");
    create_private_dir(&private).unwrap();
    let id = || ControlId::new("0123456789abcdef0123456789abcdef".into()).unwrap();
    let binding = Binding {
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
    };
    let launch = PreparedLaunch {
        binding: binding.clone(),
        executable: "/must-not-be-opened/worker".into(),
        argv: vec![],
        bootstrap: "/must-not-be-opened/bootstrap".into(),
        private_dir: private.clone(),
        socket: "/must-not-be-opened/control.sock".into(),
        startup_deadline_boot_ms: 1,
    };
    let proposal = |role: Role, session: Option<u32>, name: &str| Proposal {
        schema: 1,
        ownership: Ownership {
            service: "service".into(),
            run: "synthetic-docker".into(),
            grant: "grant".into(),
            invocation: "invocation".into(),
            nonce: "0".repeat(32),
            daemon: DaemonIdentity {
                id: "synthetic-daemon".into(),
                name: "synthetic-host".into(),
                cli_version: "28.4.0".into(),
                server_version: "28.4.0".into(),
                api_version: "1.51".into(),
                kernel: "synthetic-linux".into(),
            },
        },
        role,
        session,
        name: name.into(),
        image: format!("sha256:{}", "a".repeat(64)),
        volume: "playtime-synthetic-volume-0".into(),
        limits: Limits::smoke(),
        deadline_monotonic_ns: 1,
    };
    let write = |file: &str, binding: &Binding, proposal: &Proposal| {
        let record =
            json!({"version":1,"binding":binding,"intent":{"effect":1},"proposal":proposal});
        write_once(&private.join(file), &serde_json::to_vec(&record).unwrap()).unwrap();
    };
    write(
        "docker-volume-0.json",
        &binding,
        &proposal(Role::Volume, None, "playtime-synthetic-volume-0"),
    );
    write(
        "docker-agent-1.json",
        &binding,
        &proposal(Role::Agent, Some(1), "playtime-synthetic-agent-1"),
    );
    // Cleanup diagnostics and other names are not ownership records.
    write_once(&private.join("docker-cleanup.json"), b"{}").unwrap();
    write_once(&private.join("docker-agent-x.json"), b"{}").unwrap();
    let mut names: Vec<_> = super::archive::ownership_journal(&launch)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["playtime-synthetic-agent-1", "playtime-synthetic-volume-0"]
    );
    // A record naming another launch fails the whole check; it is never skipped.
    let mut other = binding.clone();
    other.invocation_id = ControlId::new("fedcba9876543210fedcba9876543210".into()).unwrap();
    write(
        "docker-holder-0.json",
        &other,
        &proposal(Role::Holder, None, "playtime-synthetic-holder-0"),
    );
    assert!(super::archive::ownership_journal(&launch).is_err());
}

#[test]
fn authoring_startup_tolerates_only_execution_limitations() {
    for reason in [
        "Docker readiness unavailable",
        "locked Docker image unavailable",
        "live model unavailable",
        "live runs need the Docker workshop",
    ] {
        assert!(super::tolerate_unavailable(Err(Error::Invalid(reason))).is_ok());
        let error = super::execution_unavailable(Error::Invalid(reason));
        assert_eq!(error.code(), "execution_unavailable");
        assert_eq!(error.0.details["reasons"].as_array().unwrap().len(), 1);
    }
    for reason in [
        "invalid recipe catalog",
        "damaged recipe catalog",
        "invalid Docker CLI config directory",
        "live endpoint must be a plain HTTP URL",
    ] {
        assert!(super::tolerate_unavailable(Err(Error::Invalid(reason))).is_err());
        assert_ne!(
            super::execution_unavailable(Error::Invalid(reason)).code(),
            "execution_unavailable"
        );
    }
    assert_eq!(
        super::execution_unavailable(Error::Invalid("invalid saved recipe")).code(),
        "catalog_corrupt"
    );
}
#[test]
fn remembered_snapshots_serve_their_exact_frontier_for_at_most_the_refreshed_runs() {
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
    for artifact in &f.artifacts {
        writer
            .publish_artifact(&artifact.reference, &artifact.bytes)
            .unwrap();
    }
    writer.append(&f.events[..1], BatchClass::Ordinary).unwrap();
    let first_receipt = writer.confirmed_frontier().clone();
    writer
        .append(&f.events[1..], BatchClass::Finalization)
        .unwrap();
    let reader = ArchiveReader::open(root.0.join("archive"), StorageLimits::default()).unwrap();
    let first = reader.live_snapshot(&first_receipt).unwrap();
    let last = reader.live_snapshot(writer.confirmed_frontier()).unwrap();
    let run = &f.descriptor.run_id;
    let snapshots = super::archive::Snapshots::default();
    assert!(snapshots.remembered(run, first.frontier()).is_none());
    snapshots.remember(run, &first);
    let hit = snapshots.remembered(run, first.frontier()).unwrap();
    assert_eq!(hit.frontier(), first.frontier());
    assert_eq!(hit.inspection(), first.inspection());
    // Only the run's last verified frontier is served, and only to that run.
    assert!(snapshots.remembered(run, last.frontier()).is_none());
    snapshots.remember(run, &last);
    assert!(snapshots.remembered(run, first.frontier()).is_none());
    assert_eq!(
        snapshots
            .remembered(run, last.frontier())
            .unwrap()
            .frontier(),
        last.frontier()
    );
    let others: Vec<_> = (0..crate::lifecycle::RUNS)
        .map(|n| playtime_core::RunId::new(format!("other-{n}")).unwrap())
        .collect();
    assert!(snapshots.remembered(&others[0], last.frontier()).is_none());
    // At most the runs the daemon refreshes at once keep a snapshot, so one
    // of these seventeen loses its own and the rest keep theirs.
    for other in &others {
        snapshots.remember(other, &last);
    }
    let kept = std::iter::once(run)
        .chain(&others)
        .filter(|run| snapshots.remembered(run, last.frontier()).is_some())
        .count();
    assert_eq!(kept, crate::lifecycle::RUNS);
}
