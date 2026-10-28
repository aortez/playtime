use playtime_core::{RequestId, RequestPurpose};
use playtime_protocol::{
    compaction::{EvidenceLinks, RequestSource},
    observations::ObservationLocation,
};

#[test]
fn versioned_source_links_preserve_missing_evidence_explicitly() {
    let links = EvidenceLinks {
        schema_version: 1,
        requests: vec![
            RequestSource {
                request_id: RequestId::new("r1").unwrap(),
                purpose: RequestPurpose::Work,
                observation: Some(ObservationLocation {
                    controller_sequence: 2.try_into().unwrap(),
                    archive_sequence: 4.try_into().unwrap(),
                    record_sha256: playtime_core::Sha256Digest::new("a".repeat(64)).unwrap(),
                }),
            },
            RequestSource {
                request_id: RequestId::new("r2").unwrap(),
                purpose: RequestPurpose::Compaction,
                observation: None,
            },
        ],
        plan: None,
    };
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/compaction-links-v1.json")).unwrap();
    assert_eq!(serde_json::to_value(&links).unwrap(), golden);
    assert_eq!(
        serde_json::from_value::<EvidenceLinks>(golden).unwrap(),
        links
    );
}

fn applied_record(basis: &str) -> (playtime_protocol::execution::Observation, serde_json::Value) {
    use playtime_core::RequestScope;
    use playtime_protocol::{
        execution::{Observation, RequestIdentity},
        observations::{ObservationData, ObservationIdentity},
    };
    let request_id = RequestId::new("r5").unwrap();
    let observation = Observation {
        controller_sequence: 26.try_into().unwrap(),
        elapsed_ms: 1000,
        scope: RequestScope::Run,
        observation_kind: "compaction_applied".into(),
        identity: ObservationIdentity {
            effect_id: Some(5.try_into().unwrap()),
            request_id: Some(request_id.clone()),
            tool_request_sequence: None,
        },
        request: Some(RequestIdentity {
            effect_id: 5.try_into().unwrap(),
            request_id,
            call: 5.try_into().unwrap(),
            scope: RequestScope::Run,
            purpose: RequestPurpose::Compaction,
        }),
        data: ObservationData::Inline {
            value: serde_json::Value::Null,
        },
    };
    let value = serde_json::json!({
        "ordinal": 1, "summary_request_id": "r5", "summary": "Summary 1: checked results.",
        "messages": [], "input_bound": 8461, "input_bytes": 8461, "estimate_basis": basis,
        "projected_tokens": 8973, "before_progress_input_bytes": 14605,
        "evidence": {"schema_version": 1, "plan": null,
            "requests": [{"request_id": "r5", "purpose": "compaction", "observation": null}]}
    });
    (observation, value)
}

/// A live run's first calibrated `compaction_applied` record was once rejected
/// here as malformed, which stopped its archive writer.
#[test]
fn compaction_records_accept_every_estimate_basis_the_engine_reports() {
    for basis in [
        "input_bytes",
        "calibrated_bytes",
        "server_usage_plus_new_bytes",
    ] {
        let (observation, value) = applied_record(basis);
        assert!(
            playtime_protocol::compaction::validate(&observation, &value)
                .unwrap()
                .is_some(),
            "{basis}"
        );
    }
    let (observation, value) = applied_record("tokens");
    assert!(playtime_protocol::compaction::validate(&observation, &value).is_err());
}
