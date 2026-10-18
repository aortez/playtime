use crate::common::{Temp, scenario};
use playtime_core::{ArchivePath, RunId};
use playtime_protocol::{
    archive::{InputManifest, WorkerIdentity},
    recovery::EXECUTION_CONTRACT,
    storage::ArchiveDescriptor,
    worker_bundle::{self as wire, BuildRecord, BundleDescriptor, Capabilities},
};
use playtime_runner::{
    PreparedExecution, PreparedSimulation, WorkerBundle, capture_demo_worker,
    prepare_recoverable_simulation_with_inputs, prepare_simulation_with_inputs,
};
use playtime_storage::{ArchiveReader, ArchiveWriter, FrozenInput, content_ref};
use serde_json::{Value, json};
use std::fs;

fn reference(path: &str, bytes: &[u8]) -> playtime_protocol::archive::ContentRef {
    content_ref(ArchivePath::new(path).unwrap(), bytes)
}
/// Explicit demo provenance: this is the executing test image, not a validated
/// production static package. The production gate exercises VerifiedBundle.
pub fn demo_inputs(bundle: &WorkerBundle, capabilities: Capabilities) -> Vec<FrozenInput> {
    let worker = WorkerIdentity {
        executable: reference("inputs/worker/executable", &bundle.executable),
        source: reference("inputs/worker/source.bundle", &bundle.source),
        cargo_lock: reference("inputs/worker/Cargo.lock", &bundle.cargo_lock),
        build: bundle.build.clone(),
        platform: bundle.platform.clone(),
    };
    let build = BuildRecord {
        capture_helpers: Default::default(),
        schema_version: 1,
        source_sha256: worker.source.sha256.clone(),
        lock_sha256: worker.cargo_lock.sha256.clone(),
        provenance: worker.build.clone(),
        cargo_version: "explicit demo fixture".into(),
        linker_version: "explicit demo fixture".into(),
        features: vec![],
        rustflags: vec![],
        capabilities: capabilities.clone(),
    };
    let bytes = serde_json::to_vec(&build).unwrap();
    let build = FrozenInput {
        reference: reference(wire::BUILD, &bytes),
        bytes,
    };
    let descriptor = BundleDescriptor {
        schema_version: 1,
        worker,
        build_record: build.reference.clone(),
        capabilities,
    };
    let bytes = serde_json::to_vec(&descriptor).unwrap();
    vec![
        build,
        FrozenInput {
            reference: reference(wire::DESCRIPTOR, &bytes),
            bytes,
        },
    ]
}
pub fn prepare(
    temp: &Temp,
    scenario: &playtime_engine::simulation::Scenario,
) -> PreparedSimulation {
    let bundle = capture_demo_worker().unwrap();
    let extra = demo_inputs(&bundle, Capabilities::current());
    prepare_recoverable_simulation_with_inputs(
        temp.archive(),
        RunId::new("simulation").unwrap(),
        &serde_json::to_vec(scenario).unwrap(),
        bundle,
        extra,
    )
    .unwrap()
}

#[test]
fn opt_in_requires_advertised_capability_and_reserves_the_contract_path() {
    for mutation in 0..5 {
        let temp = Temp::new();
        let bundle = capture_demo_worker().unwrap();
        let mut caps = Capabilities::current();
        if mutation == 1 {
            caps.recovery_profile = None;
        }
        if mutation == 2 {
            caps.recovery_profile = Some("future_profile".into());
        }
        let mut extra = demo_inputs(&bundle, caps);
        if mutation == 0 {
            extra.clear();
        }
        if mutation == 3 {
            extra.push(FrozenInput {
                reference: reference(EXECUTION_CONTRACT, b"{}"),
                bytes: b"{}".to_vec(),
            });
        }
        if mutation == 4 {
            extra.push(FrozenInput {
                reference: extra[0].reference.clone(),
                bytes: extra[0].bytes.clone(),
            });
        }
        assert!(
            prepare_recoverable_simulation_with_inputs(
                temp.archive(),
                RunId::new("simulation").unwrap(),
                &serde_json::to_vec(&scenario()).unwrap(),
                bundle,
                extra
            )
            .is_err(),
            "mutation {mutation}"
        );
        assert!(
            !temp.archive().exists(),
            "rejected producer must not publish an archive"
        );
    }
    let temp = Temp::new();
    let bundle = capture_demo_worker().unwrap();
    let extra = vec![FrozenInput {
        reference: reference(EXECUTION_CONTRACT, b"{}"),
        bytes: b"{}".to_vec(),
    }];
    assert!(
        prepare_simulation_with_inputs(
            temp.archive(),
            RunId::new("simulation").unwrap(),
            &serde_json::to_vec(&scenario()).unwrap(),
            bundle,
            extra
        )
        .is_err()
    );
}

#[test]
fn generic_and_older_capable_bundles_remain_inspectable_without_recovery_advertising() {
    let temp = Temp::new();
    let bundle = capture_demo_worker().unwrap();
    let extra = demo_inputs(&bundle, Capabilities::default());
    let prepared = prepare_simulation_with_inputs(
        temp.archive(),
        RunId::new("simulation").unwrap(),
        &serde_json::to_vec(&scenario()).unwrap(),
        bundle,
        extra,
    )
    .unwrap();
    let reader = ArchiveReader::open(temp.archive(), prepared.limits.clone()).unwrap();
    assert!(
        !reader
            .manifest()
            .files
            .iter()
            .any(|r| r.path.as_str() == EXECUTION_CONTRACT)
    );
    assert_eq!(
        reader
            .offline_snapshot()
            .unwrap()
            .frontier()
            .committed_sequence,
        0
    );
    let owned = PreparedExecution::acquire(temp.archive(), &prepared).unwrap();
    assert_eq!(owned.confirmed().frontier().committed_sequence, 0);
}

#[test]
fn executing_image_independently_rejects_forged_contracts_before_any_events() {
    let source = Temp::new();
    let original = prepare(&source, &scenario());
    let reader = ArchiveReader::open(source.archive(), original.limits.clone()).unwrap();
    let snapshot = reader.offline_snapshot().unwrap();
    for mutation in 0..9 {
        let temp = Temp::new();
        let mut manifest: InputManifest = reader.manifest().clone();
        let mut inputs: Vec<_> = manifest
            .references()
            .map(|r| FrozenInput {
                reference: r.clone(),
                bytes: reader.read_content(&snapshot, r).unwrap(),
            })
            .collect();
        let input = inputs
            .iter_mut()
            .find(|i| i.reference.path.as_str() == EXECUTION_CONTRACT)
            .unwrap();
        let mut contract: Value = serde_json::from_slice(&input.bytes).unwrap();
        match mutation {
            0 => {
                contract["worker_executable"]["sha256"] =
                    json!(playtime_storage::sha256(b"wrong executable"))
            }
            1 => {
                contract["configuration"]["sha256"] =
                    json!(playtime_storage::sha256(b"wrong configuration"))
            }
            2 => contract["storage_limits"]["events"] = json!(100_001),
            3 => contract["rule_set"] = json!("future_rules"),
            4 => contract["schema_version"] = json!(2),
            5 => contract["generated_tokens"] = json!(1),
            6 => contract["log_bytes"] = json!(1),
            7 => {
                contract["storage_limits"]
                    .as_object_mut()
                    .unwrap()
                    .remove("events");
            }
            8 => {}
            _ => unreachable!(),
        }
        input.bytes = serde_json::to_vec(&contract).unwrap();
        if mutation == 8 {
            input.bytes = String::from_utf8(input.bytes.clone())
                .unwrap()
                .replace("\"events\":100000", "\"events\":100000,\"events\":100000")
                .into_bytes();
        }
        input.reference = content_ref(input.reference.path.clone(), &input.bytes);
        *manifest
            .files
            .iter_mut()
            .find(|r| r.path.as_str() == EXECUTION_CONTRACT)
            .unwrap() = input.reference.clone();
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let descriptor = ArchiveDescriptor {
            schema_version: 1,
            run_id: manifest.run_id.clone(),
            mode: playtime_core::RunMode::Simulation,
            input_manifest: reference("inputs/manifest.json", &manifest_bytes),
        };
        let writer = ArchiveWriter::create(
            temp.archive(),
            descriptor,
            &manifest_bytes,
            &inputs,
            original.limits.clone(),
        )
        .unwrap();
        let prepared = PreparedSimulation {
            identity: writer.confirmed_frontier().frontier().archive.clone(),
            limits: original.limits.clone(),
        };
        drop(writer);
        // All storage hashes are internally consistent. Rejection must come
        // from the worker's execution/profile check, before controller creation.
        assert!(
            PreparedExecution::acquire(temp.archive(), &prepared).is_err(),
            "mutation {mutation}"
        );
        assert_eq!(fs::read(temp.archive().join("events.jsonl")).unwrap(), b"");
        let reader = ArchiveReader::open(temp.archive(), prepared.limits).unwrap();
        assert_eq!(
            reader
                .offline_snapshot()
                .unwrap()
                .frontier()
                .committed_sequence,
            0
        );
    }
}

#[test]
fn recovered_preparation_does_not_substitute_or_trust_the_launch_path_image() {
    let temp = Temp::new();
    let mut bundle = capture_demo_worker().unwrap();
    bundle.executable.extend_from_slice(b"different image");
    let extra = demo_inputs(&bundle, Capabilities::current());
    let prepared = prepare_recoverable_simulation_with_inputs(
        temp.archive(),
        RunId::new("simulation").unwrap(),
        &serde_json::to_vec(&scenario()).unwrap(),
        bundle,
        extra,
    )
    .unwrap();
    assert!(PreparedExecution::acquire(temp.archive(), &prepared).is_err());
    assert!(
        fs::read(temp.archive().join("events.jsonl"))
            .unwrap()
            .is_empty()
    );
}
