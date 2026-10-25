mod common;
use common::*;
use playtime_core::RequestPurpose;
use playtime_runner::ArchiveQueries;
use playtime_storage::{ArchiveWriter, BatchClass, StorageLimits};
use serde_json::json;

#[test]
fn memento_purposes_reconcile_with_unchanged_aggregate_and_admitted_count() {
    let temp = Temp::new();
    let mut scenario = scenario();
    // One work turn forces a dedicated handoff in this fixture.
    scenario.parameters.limits.work_turns_per_session = 1.try_into().unwrap();
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let result = report.result.unwrap();
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let accounting = queries.request_accounting().unwrap();
    assert_eq!(accounting.model_calls, Some(result.model_calls));
    assert!(accounting.by_purpose.contains_key(&RequestPurpose::Handoff));
    assert_eq!(accounting.unclassified.admitted_calls, 0);
    assert_eq!(accounting.unclassified.settled_calls, 0);
    assert_eq!(
        accounting
            .by_purpose
            .values()
            .map(|t| t.usage.budget_charged_tokens)
            .sum::<u64>(),
        result.usage.budget_charged_tokens
    );
    assert_eq!(
        accounting
            .by_purpose
            .values()
            .map(|t| t.admitted_calls)
            .sum::<u64>(),
        result.model_calls
    );
}

#[test]
fn missing_legacy_purpose_is_unclassified_and_missing_admission_count_is_unavailable() {
    for admission in [true, false] {
        let temp = Temp::new();
        let prepared = prepared(&temp, &scenario());
        let mut writer = ArchiveWriter::open_prepared(
            temp.archive(),
            &prepared.identity,
            prepared.limits.clone(),
        )
        .unwrap();
        let wire = |sequence, kind, data| {
            format!("{}\n", json!({"schema_version":1,"run_id":"simulation","sequence":sequence,"time":"2026-10-17T00:00:00Z","kind":kind,"data":data})).into_bytes()
        };
        let mut lines = vec![wire(
            1,
            "run_started",
            json!({"mode":"simulation","input_manifest":prepared.identity.input_manifest}),
        )];
        if admission {
            lines.push(wire(2, "effect_intent", json!({"schema_version":1,"effect_id":2,"effect_kind":"model","session_id":"s1","request_id":"r1","tool_request_sequence":null,"maximum_tokens":100,"deadline_ms":1000,"sources":[]})));
        }
        lines.push(wire(lines.len() + 1, "request_finished", json!({"session_id":"s1","request_id":"r1","usage":{"status":"unknown","reserved_tokens":100}})));
        writer
            .append(
                &lines.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                BatchClass::Ordinary,
            )
            .unwrap();
        drop(writer);
        let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
        let accounting = queries.request_accounting().unwrap();
        assert_eq!(accounting.model_calls, admission.then_some(1));
        assert!(
            accounting.by_purpose.is_empty(),
            "do not invent per-purpose zeroes"
        );
        assert_eq!(accounting.unclassified.settled_calls, 1);
        assert_eq!(accounting.unclassified.usage.unknown_reserved_tokens, 100);
        assert!(!accounting.unclassified.usage.usage_complete);
    }
}
