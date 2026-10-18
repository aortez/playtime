use crate::{
    common::{Temp, clock, scenario},
    preparation,
    support::{Case, stored},
};
use playtime_engine::simulation::ModelEnding;
use playtime_protocol::{recovery::*, storage::EventQuery};
use playtime_runner::{DriverOptions, recovery::*, run_prepared};
use playtime_storage::ArchiveReader;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs};

fn reference_totals(
    reader: &ArchiveReader,
    events: &[playtime_storage::StoredEvent],
    root: &std::path::Path,
    plan: &playtime_protocol::archive::ContentRef,
) -> (u64, u64) {
    use playtime_protocol::{
        bridge::CheckpointIndex,
        events::{KnownEvent, MemoryRef, decode_event},
        observations::ObservationData,
    };
    let mut references = BTreeMap::new();
    let mut add = |r: &playtime_protocol::archive::ContentRef| {
        references.insert(r.path.clone(), r.clone());
    };
    add(&reader.identity().input_manifest);
    for r in reader.manifest().references() {
        add(r);
    }
    for e in events {
        let decoded = decode_event(std::str::from_utf8(&e.bytes).unwrap()).unwrap();
        match decoded.payload() {
            playtime_protocol::events::EventPayload::Observation(o) => {
                if let ObservationData::Artifact { content } = &o.observation().data {
                    add(content);
                }
            }
            playtime_protocol::events::EventPayload::Known(known) => match known.as_ref() {
                KnownEvent::SessionStarted {
                    workspace,
                    memento,
                    evidence,
                    delivery,
                    ..
                } => {
                    add(workspace);
                    if let MemoryRef::Available { content } = memento {
                        add(content);
                    }
                    if let Some(r) = evidence {
                        add(r);
                    }
                    if let Some(delivery) = delivery {
                        add(&delivery.memento.content);
                        if let Some(e) = &delivery.evidence {
                            add(&e.text);
                            add(&e.provenance);
                        }
                    }
                }
                KnownEvent::CheckpointPublished { checkpoint } => {
                    add(&checkpoint.workspace);
                    if let MemoryRef::Available { content } = &checkpoint.memento {
                        add(content);
                    }
                    if let Some(r) = &checkpoint.evidence {
                        add(r);
                    }
                    if let Some(r) = &checkpoint.index {
                        add(r);
                        let index: CheckpointIndex =
                            serde_json::from_slice(&fs::read(root.join(r.path.as_str())).unwrap())
                                .unwrap();
                        add(&index.source_map);
                        for r in index.content_leaves() {
                            add(r);
                        }
                    }
                }
                _ => (),
            },
            _ => (),
        }
    }
    add(plan);
    (
        references.len() as u64,
        references.values().map(|r| r.size_bytes).sum(),
    )
}

fn independent_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
type Oracle = (u64, u64, u64, Vec<(Value, Value, u64)>, u64);
fn oracle(events: &[playtime_storage::StoredEvent], root: &std::path::Path) -> Oracle {
    let (mut known, mut unknown, mut unknown_count, mut cost) = (0, 0, 0, 0);
    let mut pending: BTreeMap<String, (Value, Value, u64, u64)> = BTreeMap::new();
    for e in events {
        cost += e.bytes.len() as u64;
        assert_eq!(independent_hash(&e.bytes), e.sha256.as_str());
        let raw: Value = serde_json::from_slice(&e.bytes).unwrap();
        let d = &raw["data"];
        match raw["kind"].as_str().unwrap() {
            "effect_intent" if d["effect_kind"] == "model" => {
                pending.insert(
                    d["request_id"].as_str().unwrap().into(),
                    (
                        d["session_id"].clone(),
                        d["request_id"].clone(),
                        d["maximum_tokens"].as_u64().unwrap(),
                        raw["sequence"].as_u64().unwrap(),
                    ),
                );
            }
            "request_not_dispatched" => {
                pending.remove(d["request_id"].as_str().unwrap()).unwrap();
            }
            "request_finished" => {
                pending.remove(d["request_id"].as_str().unwrap()).unwrap();
                match d["usage"]["status"].as_str().unwrap() {
                    "known" => known += d["usage"]["generated_tokens"].as_u64().unwrap(),
                    "unknown" => {
                        unknown += d["usage"]["reserved_tokens"].as_u64().unwrap();
                        unknown_count += 1;
                    }
                    _ => panic!(),
                }
            }
            "controller_observation" if d["data"]["storage"] == "artifact" => {
                let r = &d["data"]["content"];
                let bytes = fs::read(root.join(r["path"].as_str().unwrap())).unwrap();
                assert_eq!(independent_hash(&bytes), r["sha256"].as_str().unwrap());
                assert_eq!(bytes.len() as u64, r["size_bytes"].as_u64().unwrap());
                cost += bytes.len() as u64;
            }
            _ => (),
        }
    }
    let mut pending: Vec<_> = pending.into_values().collect();
    pending.sort_by_key(|v| v.3);
    (
        known,
        unknown,
        unknown_count,
        pending.into_iter().map(|(s, r, c, _)| (s, r, c)).collect(),
        cost,
    )
}

#[test]
fn real_scripted_archive_matches_independent_accounting_byte_and_hash_oracle() {
    let temp = Temp::new();
    let mut scenario = scenario();
    scenario.parameters.limits.log_bytes = 8_388_608.try_into().unwrap();
    let ModelEnding::Response(response) =
        &mut scenario.models[0].completion.as_mut().unwrap().outcome
    else {
        panic!()
    };
    response.message["content"] = json!("🙂<&>".repeat(50_000)); // Forces retained observation bytes.
    let prepared = preparation::prepare(&temp, &scenario);
    let report = run_prepared(
        temp.archive(),
        &prepared,
        &mut clock(),
        DriverOptions::default(),
    )
    .unwrap();
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let reader = ArchiveReader::open(temp.archive(), prepared.limits.clone()).unwrap();
    let snapshot = reader.offline_snapshot().unwrap();
    let page = reader
        .page(
            &snapshot,
            &EventQuery {
                archive: reader.identity().clone(),
                after_sequence: 0,
                through_sequence: snapshot.frontier().committed_sequence,
                limit: 1000,
            },
        )
        .unwrap();
    assert!(!page.has_more);
    let actual_bytes = fs::read(temp.archive().join("events.jsonl")).unwrap();
    let events: Vec<_> = actual_bytes
        .split_inclusive(|b| *b == b'\n')
        .map(|b| stored(b.to_vec()))
        .collect();
    let (known, unknown, count, pending, cost) = oracle(&events, &temp.archive());
    assert_eq!((known, unknown, count), (26, 0, 0));
    assert!(pending.is_empty());
    assert_eq!(cost, report.accounted_log_bytes);
    assert!(cost > actual_bytes.len() as u64 + 300_000);
    let mut case = Case::golden();
    case.files.clear();
    for r in &snapshot.inspection().referenced_content {
        case.files.insert(
            r.path.as_str().into(),
            reader.read_content(&snapshot, r).unwrap(),
        );
    }
    for name in ["archive.json", "inputs/manifest.json"] {
        case.files
            .insert(name.into(), fs::read(temp.archive().join(name)).unwrap());
    }
    case.base = snapshot.frontier().clone();
    case.attempt.binding.archive = case.base.archive.clone();
    case.attempt.binding.worker.run_id = case.base.archive.run_id.clone();
    case.attempt.binding.worker.manifest_sha256 = case.base.archive.input_manifest.sha256.clone();
    case.attempt.binding.worker.executable_sha256 =
        reader.manifest().worker.executable.sha256.clone();
    case.limits = prepared.limits;
    case.replace_events(events.clone());
    assert_eq!(
        case.decision(),
        RecoveryDecision::PreserveTerminal(Box::new(case.base.clone()))
    );
    let before = case.files.clone();
    // Interrupted views are in-memory prefixes of real controller bytes; no
    // marker, log, or copied archive is rewritten to manufacture the result.
    let first_settlement = events
        .iter()
        .position(|e| e.envelope.kind == "request_finished")
        .unwrap();
    for length in [first_settlement, events.len() - 1] {
        case.replace_events(events[..length].to_vec());
        case.file_bytes = actual_bytes.len() as u64;
        let (known, unknown, count, pending, cost) = oracle(&case.events, &temp.archive());
        let plan = case.final_plan();
        assert_eq!(plan.base_accounted_log_bytes, cost);
        assert_eq!(plan.existing_usage.known_generated_tokens, known);
        assert_eq!(plan.existing_usage.unknown_reserved_tokens, unknown);
        let reserved: u64 = pending.iter().map(|p| p.2).sum();
        assert_eq!(
            plan.resulting_usage.budget_charged_tokens,
            known + unknown + reserved
        );
        assert_eq!(
            plan.resulting_usage.unknown_requests,
            count + pending.len() as u64
        );
        let patch = encode_patch(&plan).unwrap();
        let plan_hash = independent_hash(&patch.plan_bytes);
        assert_eq!(patch.origin.plan.sha256.as_str(), plan_hash);
        let origin = json!({"schema_version":1,"attempt_id":case.attempt.attempt_id,"plan":{"path":plan_path(&case.attempt.attempt_id),"sha256":plan_hash,"size_bytes":patch.plan_bytes.len()}});
        let mut expected = Vec::new();
        let mut sequence = case.base.committed_sequence;
        let mut append = |kind: &str, data: Value| {
            sequence += 1;
            expected.extend(format!("{{\"schema_version\":1,\"run_id\":{},\"sequence\":{},\"time\":{},\"kind\":{},\"data\":{}}}\n",serde_json::to_string(&case.base.archive.run_id).unwrap(),sequence,serde_json::to_string(&case.attempt.chosen_time).unwrap(),serde_json::to_string(kind).unwrap(),data).bytes());
        };
        for (session, request, cap) in &pending {
            append(
                "request_finished",
                json!({"session_id":session,"request_id":request,"usage":{"status":"unknown","reserved_tokens":cap},"recovery_origin":origin}),
            );
        }
        let first_stop = case
            .events
            .iter()
            .find(|e| e.envelope.kind == "stop_decided")
            .map_or(json!("interrupted"), |e| e.envelope.data["reason"].clone());
        let checkpoint = case
            .events
            .iter()
            .rev()
            .find(|e| e.envelope.kind == "checkpoint_published")
            .map_or(Value::Null, |e| e.envelope.data["checkpoint"].clone());
        append(
            "run_finished",
            json!({"reason":first_stop,"usage":{"known_generated_tokens":known,"unknown_reserved_tokens":unknown+reserved,"unknown_requests":count+pending.len() as u64,"budget_charged_tokens":known+unknown+reserved,"usage_complete":count==0&&pending.is_empty()},"cleanup":{"status":"unknown"},"last_checkpoint":checkpoint,"recovery_origin":origin}),
        );
        assert_eq!(patch.bytes, expected);
        let prefix: Vec<_> = case
            .events
            .iter()
            .flat_map(|e| e.bytes.clone())
            .chain(expected.clone())
            .collect();
        let digest = playtime_core::Sha256Digest::new(independent_hash(&prefix)).unwrap();
        let (reference_count, referenced_bytes) =
            reference_totals(&reader, &case.events, &temp.archive(), &patch.origin.plan);
        let (selection, marker) =
            selection_for_patch(&plan, &patch, digest, reference_count, referenced_bytes).unwrap();
        assert_eq!(
            selection.capacity.final_accounted_log_bytes,
            cost + expected.len() as u64
        );
        assert_eq!(selection.patch.sha256.as_str(), independent_hash(&expected));
        assert_eq!(
            selection.target_marker.sha256.as_str(),
            independent_hash(&marker)
        );
    }
    assert_eq!(before, case.files);
    assert_eq!(
        fs::read(temp.archive().join("events.jsonl")).unwrap(),
        actual_bytes
    );
}
