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
