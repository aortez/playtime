mod common;
#[path = "recovery/preparation.rs"]
mod preparation;
#[path = "recovery/real_archive.rs"]
mod real_archive;
#[path = "recovery/support.rs"]
mod support;
use playtime_protocol::recovery::*;
use playtime_runner::recovery::*;
use playtime_storage::{content_ref, sha256};
use serde_json::{Value, json};
use support::*;

#[test]
fn planner_matches_independent_exact_plan_patch_marker_selection_goldens() {
    let case = Case::golden();
    let before = case.files.clone();
    let plan = case.final_plan();
    let encoded = encode_patch(&plan).unwrap();
    assert_eq!(
        encoded.plan_bytes,
        bytes(&format!("artifacts/recovery/{ATTEMPT}/plan.json"))
    );
    assert_eq!(
        encoded.bytes,
        bytes(&format!("recovery/{ATTEMPT}/patch.jsonl"))
    );
    // Expected totals/hashes come from independently authored Python fixtures,
    // not a planner projection or ArchiveQueries::run.
    assert_eq!(plan.base_accounted_log_bytes, 2272);
    assert_eq!(encoded.bytes.len(), 1425);
    assert_eq!(plan.resulting_usage.known_generated_tokens, 10);
    assert_eq!(plan.resulting_usage.unknown_reserved_tokens, 100);
    let expected: RecoverySelectionV1 = decode(&bytes("recovery/intent.json"), 65536).unwrap();
    let all = [bytes("events.jsonl"), encoded.bytes.clone()].concat();
    let (selection, marker) = selection_for_patch(
        &plan,
        &encoded,
        sha256(&all),
        14,
        expected.capacity.referenced_content_bytes,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&selection).unwrap(),
        bytes("recovery/intent.json")
    );
    assert_eq!(marker, bytes(&format!("recovery/{ATTEMPT}/marker.json")));
    assert_eq!(
        selection.plan,
        content_ref(encoded.origin.plan.path.clone(), &encoded.plan_bytes)
    );
    assert_eq!(selection.patch.sha256, sha256(&encoded.bytes));
    assert_eq!(selection.target_marker.sha256, sha256(&marker));
    assert_eq!(
        case.files, before,
        "planning must not mutate supplied evidence"
    );
}
#[test]
fn all_fifteen_accounting_review_vectors_are_executable() {
    let vectors: Value = serde_json::from_slice(&bytes("review-vectors.json")).unwrap();
    for vector in vectors["accounting"].as_array().unwrap() {
        let mut case = Case::golden();
        case.facts(&vector["facts"]);
        if vector.get("blocked").is_some() {
            blocked(case.decision(), RecoveryBlockedReasonV1::EvidenceConflict);
            continue;
        }
        let plan = case.final_plan();
        let expected = &vector["expected"];
        let mut usage = serde_json::to_value(&plan.resulting_usage).unwrap();
        usage["append_unknown"] = json!(
            plan.unresolved
                .iter()
                .map(|i| json!([i.session_id, i.request_id, i.maximum_tokens]))
                .collect::<Vec<_>>()
        );
        assert_eq!(&usage, expected, "{}", vector["name"]);
    }
}
#[test]
fn empty_log_decisions_require_empty_file_and_proven_no_begin() {
    let mut case = Case::golden();
    case.replace_events(vec![]);
    blocked(case.decision(), RecoveryBlockedReasonV1::Incomplete);
    case.begin = BeginHistory::ProvenNeverAuthorized;
    assert_eq!(case.decision(), RecoveryDecision::NeverStarted);
    case.file_bytes = 1;
    blocked(case.decision(), RecoveryBlockedReasonV1::Incomplete);
}
#[test]
fn first_stop_and_last_committed_checkpoint_are_preserved_beyond_worker_ack() {
    let case = Case::golden();
    let plan = case.final_plan();
    assert_eq!(
        plan.first_stop.unwrap().reason,
        playtime_core::StopReason::AgentFinished
    );
    assert_eq!(
        plan.last_checkpoint.unwrap().checkpoint.session_id.as_str(),
        "s2"
    );
    assert_eq!(plan.prior_bounds.worker.unwrap().committed_sequence, 4);
    assert_eq!(plan.base.committed_sequence, 7);
    let patch = encode_patch(&case.final_plan()).unwrap();
    let terminal: Value = serde_json::from_slice(
        patch
            .bytes
            .split_inclusive(|b| *b == b'\n')
            .next_back()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(terminal["data"]["cleanup"], json!({"status":"unknown"}));
    assert!(terminal["data"].get("recovery_origin").is_some());
}
#[test]
fn saved_boundaries_exact_records_and_content_are_independently_checked() {
    for mutation in 0..6 {
        let mut case = Case::golden();
        match mutation {
            0 => case.bounds.worker.as_mut().unwrap().prefix_sha256 = sha256(b"wrong prefix"),
            1 => {
                case.bounds
                    .public
                    .as_mut()
                    .unwrap()
                    .frontier
                    .committed_bytes += 1
            }
            2 => case.events[1].sha256 = sha256(b"not its LF-terminated record"),
            3 => case.events[1].envelope.data["maximum_tokens"] = json!(99),
            4 => {
                case.files
                    .insert("sessions/s2/memento.txt".into(), b"changed".to_vec());
            }
            5 => {
                case.events.pop();
            }
            _ => unreachable!(),
        }
        blocked(case.decision(), RecoveryBlockedReasonV1::EvidenceConflict);
    }
}

#[test]
fn recovery_terminal_requires_exact_plan_and_patch_and_preserves_suffix() {
    let mut case = Case::golden();
    let patch = bytes(&format!("recovery/{ATTEMPT}/patch.jsonl"));
    let mut events = case.events.clone();
    events.extend(
        patch
            .split_inclusive(|b| *b == b'\n')
            .map(|line| stored(line.to_vec())),
    );
    case.replace_events(events);
    case.file_bytes += 123; // Uncommitted suffix is not read or removed.
    case.bounds.public = Some(PublishedFrontierV1 {
        frontier: case.base.clone(),
        provenance: FrontierProvenanceV1::Recovery {
            attempt_id: case.attempt.attempt_id.clone(),
        },
    });
    assert_eq!(
        case.decision(),
        RecoveryDecision::PreserveTerminal(Box::new(case.base.clone()))
    );
    let before = case.files.clone();
    let _ = case.decision();
    assert_eq!(before, case.files);
    for mutation in 0..3 {
        let mut bad = Case::golden();
        let mut events = case.events.clone();
        match mutation {
            0 => {
                events.pop();
            }
            1 => {
                let last = events.last_mut().unwrap();
                last.envelope.time = playtime_core::UtcTime::new("2026-10-19T00:00:00Z").unwrap();
                let mut bytes = serde_json::to_vec(&last.envelope).unwrap();
                bytes.push(b'\n');
                *last = stored(bytes);
            }
            2 => {
                bad.files.insert(
                    format!("artifacts/recovery/{ATTEMPT}/plan.json"),
                    b"{}".to_vec(),
                );
            }
            _ => unreachable!(),
        }
        bad.replace_events(events);
        blocked(bad.decision(), RecoveryBlockedReasonV1::EvidenceConflict);
    }
}

#[test]
fn older_terminal_is_preserved_but_missing_or_unsupported_contract_never_finalizes() {
    let mut case = Case::golden();
    case.facts(&json!([
        ["intent", 1, "s1", "r1", 40],
        ["known", "s1", "r1", 0]
    ]));
    let mut manifest: playtime_protocol::archive::InputManifest =
        serde_json::from_slice(&case.files["inputs/manifest.json"]).unwrap();
    manifest
        .files
        .retain(|r| r.path.as_str() != EXECUTION_CONTRACT);
    case.rebind_manifest(manifest);
    blocked(
        case.decision(),
        RecoveryBlockedReasonV1::UnsupportedContract,
    );
    let mut events = case.events.clone();
    events.push(event(events.len() as u64+1,"run_finished",json!({"reason":"interrupted","usage":{"known_generated_tokens":0,"unknown_reserved_tokens":0,"unknown_requests":0,"budget_charged_tokens":0,"usage_complete":true},"cleanup":{"status":"unknown"},"last_checkpoint":null})));
    case.replace_events(events);
    case.file_bytes += 17;
    assert_eq!(
        case.decision(),
        RecoveryDecision::PreserveTerminal(Box::new(case.base.clone()))
    );
    for mutation in 0..4 {
        let mut case = Case::golden();
        case.change_contract(|c| match mutation {
            0 => c["schema_version"] = json!(2),
            1 => c["rule_set"] = json!("future_rules"),
            2 => c["generated_tokens"] = json!(1),
            3 => c["storage_limits"]
                .as_object_mut()
                .map(|m| m.remove("events"))
                .map(|_| ())
                .unwrap(),
            _ => unreachable!(),
        });
        blocked(
            case.decision(),
            RecoveryBlockedReasonV1::UnsupportedContract,
        );
    }
}

#[test]
fn future_optional_observations_are_inspectable_but_unknown_control_is_blocked() {
    let mut case = Case::golden();
    let mut events = case.events.clone();
    events.push(event(8,"controller_observation",json!({"schema_version":1,"controller_sequence":1,"elapsed_ms":0,"session_id":null,"observation_kind":"future_optional","identity":{"effect_id":null,"request_id":null,"tool_request_sequence":null},"data":{"storage":"inline","value":{"future_value":42}},"extra_optional":true})));
    case.replace_events(events);
    assert_eq!(case.final_plan().resulting_usage.budget_charged_tokens, 110);
    for mutation in 0..3 {
        let mut bad = Case::golden();
        let mut events = bad.events.clone();
        match mutation {
            0 => events.push(event(8, "future_required_control", json!({}))),
            1 => {
                let mut data = events[1].envelope.data.clone();
                data["effect_kind"] = json!("future_effect");
                data["maximum_tokens"] = Value::Null;
                events[1] = event(2, "effect_intent", data);
            }
            2 => {
                let mut data = events[1].envelope.data.clone();
                data["future_control"] = json!(true);
                events[1] = event(2, "effect_intent", data);
            }
            _ => unreachable!(),
        }
        bad.replace_events(events);
        blocked(bad.decision(), RecoveryBlockedReasonV1::UnsupportedContract);
    }
}

#[test]
fn six_reentry_review_vectors_only_classify_exact_selected_evidence() {
    let vectors: Value = serde_json::from_slice(&bytes("review-vectors.json")).unwrap();
    let selection = bytes("recovery/intent.json");
    let plan = bytes(&format!("artifacts/recovery/{ATTEMPT}/plan.json"));
    let inventory = bytes(&format!("recovery/{ATTEMPT}/diagnostics.json"));
    let patch = bytes(&format!("recovery/{ATTEMPT}/patch.jsonl"));
    let target = bytes(&format!("recovery/{ATTEMPT}/marker.json"));
    let decoded: RecoverySelectionV1 = decode(&selection, 65536).unwrap();
    for v in vectors["reentry"].as_array().unwrap() {
        let files = v["selection_valid"]
            .as_bool()
            .unwrap()
            .then_some(SelectedFiles {
                selection: &selection,
                plan: &plan,
                inventory: &inventory,
                patch: &patch,
                target_marker: &target,
            });
        let mut marker = decoded.base.clone();
        if v["marker"] == "target" {
            marker = decoded.final_frontier.clone();
        }
        if v["marker"] == "intermediate" {
            marker.committed_sequence += 1;
        }
        let suffix = match v["suffix"].as_str() {
            Some("exact_patch_prefix") => &patch[..37],
            Some("exact_full_patch") => patch.as_slice(),
            _ => b"conflicting",
        };
        let decision = inspect_reentry(
            files,
            &marker,
            suffix,
            v["destructive_work_possible"].as_bool().unwrap_or(false),
        );
        let expected = match v["expected"].as_str().unwrap() {
            "preserve_then_rewrite_same_patch" => ReentryDecision::PreserveSelectedPatch,
            "verify_and_adopt" => ReentryDecision::VerifySelectedTerminal,
            _ => ReentryDecision::Blocked(RecoveryBlockedReasonV1::EvidenceConflict),
        };
        assert_eq!(decision, expected, "{}", v["name"]);
    }
}

#[test]
fn actual_patch_and_each_independent_capacity_bound_are_enforced() {
    for mutation in 0..8 {
        let mut case = Case::golden();
        match mutation {
            0 => case.limits.batch_bytes = 1,
            1 => case.limits.record_bytes = 100,
            2 => case.limits.events = 7,
            3 => case.limits.references = 13,
            4 => case.limits.verified_content_bytes = 1,
            5 => case.limits.content_bytes = 100,
            6 => case.attempt.maintenance_limits.plan_bytes = 100,
            7 => case.attempt.maintenance_limits.selection_bytes = 1,
            _ => unreachable!(),
        }
        let reason = if mutation == 7 {
            RecoveryBlockedReasonV1::PreservationLimit
        } else {
            RecoveryBlockedReasonV1::FinalizationLimit
        };
        blocked(case.decision(), reason);
    }
}

#[test]
fn retained_payload_is_charged_per_occurrence_and_omissions_are_not_retained_output() {
    let mut case = Case::golden();
    let payload = serde_json::to_vec(&"a".repeat(299_998)).unwrap();
    assert_eq!(payload.len(), 300_000);
    let reference = content_ref(
        playtime_core::ArchivePath::new("artifacts/observations/repeated.json").unwrap(),
        &payload,
    );
    case.files.insert(reference.path.as_str().into(), payload);
    let mut events = case.events.clone();
    for sequence in 1..=2 {
        events.push(event(events.len() as u64+1,"controller_observation",json!({"schema_version":1,"controller_sequence":sequence,"elapsed_ms":0,"session_id":null,"observation_kind":"future_optional","identity":{"effect_id":null,"request_id":null,"tool_request_sequence":null},"data":{"storage":"artifact","content":reference}})));
    }
    events.push(event(events.len() as u64+1,"observation_omitted",json!({"schema_version":1,"controller_sequence":3,"observation_kind":"future_optional","elapsed_ms":0,"session_id":null,"identity":{"effect_id":null,"request_id":null,"tool_request_sequence":null},"size_bytes":9_000_000,"reason":"not retained"})));
    case.replace_events(events);
    let plan = case.final_plan();
    assert_eq!(
        plan.base_accounted_log_bytes,
        case.base.committed_bytes + 600_000
    );
    let patch = encode_patch(&plan).unwrap();
    assert_eq!(
        plan.available_log_bytes,
        1_048_576 - case.base.committed_bytes - 600_000
    );
    case.limits.event_log_bytes = plan.base_accounted_log_bytes + patch.bytes.len() as u64 - 1;
    blocked(case.decision(), RecoveryBlockedReasonV1::FinalizationLimit);
    let mut input = case.input();
    input.inventory_entries = 10_001;
    blocked(
        plan_recovery(&input, case.events.iter().cloned().map(Ok)).unwrap(),
        RecoveryBlockedReasonV1::PreservationLimit,
    );
}

#[test]
fn attempt_time_and_proposed_bytes_survive_a_higher_acquired_generation() {
    let case = Case::golden();
    let expected = case.decision();
    let mut input = case.input();
    input.acquired_generation = 99;
    assert_eq!(
        plan_recovery(&input, case.events.iter().cloned().map(Ok)).unwrap(),
        expected
    );
    input.acquired_generation = 1;
    blocked(
        plan_recovery(&input, case.events.iter().cloned().map(Ok)).unwrap(),
        RecoveryBlockedReasonV1::OwnershipUnresolved,
    );
    let error: playtime_storage::StorageError = std::io::Error::other("bounded scan failed").into();
    blocked(
        plan_recovery(&case.input(), [Err(error)].into_iter()).unwrap(),
        RecoveryBlockedReasonV1::IoUncertain,
    );
}

#[test]
fn generic_non_dispatch_unknown_settlement_is_valid_only_in_the_exact_recovery_patch() {
    let mut case = Case::golden();
    case.facts(&json!([
        ["intent", 1, "s1", "r1", 40],
        ["effect_not_dispatched", 1, "s1", "r1"]
    ]));
    let patch = encode_patch(&case.final_plan()).unwrap();
    case.files.insert(
        patch.origin.plan.path.as_str().into(),
        patch.plan_bytes.clone(),
    );
    let mut events = case.events.clone();
    events.extend(
        patch
            .bytes
            .split_inclusive(|b| *b == b'\n')
            .map(|b| stored(b.to_vec())),
    );
    case.replace_events(events);
    assert_eq!(
        case.decision(),
        RecoveryDecision::PreserveTerminal(Box::new(case.base.clone()))
    );
    let mut events = case.events.clone();
    let last = events.len() - 2;
    let mut data = events[last].envelope.data.clone();
    data.as_object_mut().unwrap().remove("recovery_origin");
    events[last] = event(last as u64 + 1, "request_finished", data);
    case.replace_events(events);
    blocked(case.decision(), RecoveryBlockedReasonV1::EvidenceConflict);
}

#[test]
fn frozen_input_bounds_and_occupied_plan_paths_cannot_be_bypassed() {
    let mut case = Case::golden();
    case.change_contract(|c| {
        c["storage_limits"]["manifest_bytes"] = json!(1);
        c["storage_limits"]["content_bytes"] = json!(1);
    });
    blocked(case.decision(), RecoveryBlockedReasonV1::FinalizationLimit);
    let mut case = Case::golden();
    let path = playtime_core::ArchivePath::new(plan_path(&case.attempt.attempt_id)).unwrap();
    let payload =
        serde_json::to_vec(&json!({"existing":"worker observation".repeat(20_000)})).unwrap();
    let reference = content_ref(path.clone(), &payload);
    case.files.insert(path.as_str().into(), payload);
    let mut events = case.events.clone();
    events.push(event(8,"controller_observation",json!({"schema_version":1,"controller_sequence":1,"elapsed_ms":0,"session_id":null,"observation_kind":"future_optional","identity":{"effect_id":null,"request_id":null,"tool_request_sequence":null},"data":{"storage":"artifact","content":reference}})));
    case.replace_events(events);
    blocked(case.decision(), RecoveryBlockedReasonV1::EvidenceConflict);
}
