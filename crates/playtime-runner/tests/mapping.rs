mod common;

use common::*;
use playtime_engine::simulation::Scenario;
use playtime_runner::ArchiveQueries;
use playtime_storage::StorageLimits;
use serde_json::{Value, json};

#[test]
fn reviewed_zero_usage_rejected_tool_and_unknown_command_mapping_stays_exact() {
    let temp = Temp::new();
    let scenario: Scenario =
        serde_json::from_str(include_str!("fixtures/known-zero-rejected.json")).unwrap();
    let report = run(&temp, &scenario);
    assert!(report.terminal_committed, "{:?}", report.diagnostics);
    let queries = ArchiveQueries::offline(temp.archive(), StorageLimits::default()).unwrap();
    let page = queries.history(0, 1000, true).unwrap();
    let selected: Vec<_> = page.entries.iter().filter(|entry| {
        matches!(entry.envelope.kind.as_str(), "request_finished" | "command_finished" | "stop_decided")
        || (entry.envelope.kind == "controller_observation" && matches!(entry.envelope.data["observation_kind"].as_str(), Some("model_chunk" | "model_response" | "tool_request" | "tool_dispatch" | "memento_write" | "tool_result")))
    }).map(|entry| json!({"archive_sequence":entry.envelope.sequence,"kind":entry.envelope.kind,"data":entry.envelope.data,"record_sha256":entry.record_sha256})).collect();
    let expected: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/known-zero-rejected-mapping.json")).unwrap();
    assert_eq!(selected, expected);
    assert!(queries.checkpoints().unwrap().iter().all(|checkpoint| {
        checkpoint
            .index
            .as_ref()
            .unwrap()
            .outgoing_evidence
            .is_none()
    }));
    assert_eq!(queries.run().unwrap().usage.budget_charged_tokens, 0);
    assert_eq!(queries.run().unwrap().usage.unknown_requests, 0);
}
