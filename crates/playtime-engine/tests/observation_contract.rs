#[path = "common/bridge_scenarios.rs"]
mod bridge_scenarios;

use playtime_protocol::{
    events::{EventPayload, decode_event},
    observations::ObservationLocation,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn mapping_goldens_preserve_the_actual_controller_records_and_effect_identities() {
    let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../playtime-protocol/tests/fixtures/controller-observations");
    let mapping: Value =
        serde_json::from_slice(&std::fs::read(fixture_root.join("mapping.json")).unwrap()).unwrap();
    for (name, case) in bridge_scenarios::cases() {
        let bytes = std::fs::read(fixture_root.join(format!("{name}.jsonl"))).unwrap();
        let expected = &mapping["cases"][name];
        assert_eq!(bytes.len() as u64, expected["size_bytes"]);
        assert_eq!(digest(&bytes), expected["sha256"]);
        let locations: Vec<ObservationLocation> =
            serde_json::from_value(expected["locations"].clone()).unwrap();
        let mut actual = Vec::new();
        let mut count = 0;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            count += 1;
            let event = decode_event(std::str::from_utf8(line).unwrap()).unwrap();
            assert_eq!(event.envelope().sequence.get(), count);
            let EventPayload::Observation(observation) = event.payload() else {
                continue;
            };
            let index = actual.len();
            let original = &case.records[index];
            assert_eq!(
                serde_json::to_value(observation.record().unwrap()).unwrap(),
                serde_json::to_value(original).unwrap(),
                "{name} source {}",
                original.sequence
            );
            let location = &locations[index];
            assert_eq!(location.controller_sequence.get(), original.sequence);
            assert_eq!(location.archive_sequence, event.envelope().sequence);
            assert_eq!(location.record_sha256.as_str(), digest(line));
            assert_ne!(
                digest(line),
                digest(&line[..line.len() - 1]),
                "LF belongs to the record identity"
            );
            let identity = &observation.observation().identity;
            if original.kind.starts_with("model_") || original.kind.starts_with("request_") {
                assert_eq!(
                    identity.effect_id.unwrap().get(),
                    case.model_effects[identity.request_id.as_ref().unwrap().as_str()]
                );
            }
            if let Some(sequence) = identity.tool_request_sequence {
                assert_eq!(
                    identity.effect_id.map(|id| id.get()),
                    case.tool_effects.get(&sequence.get()).copied()
                );
            }
            actual.push(original.sequence);
        }
        assert_eq!(actual.len(), case.records.len());
        assert_eq!(locations.len(), actual.len());
        assert_eq!(count, expected["record_count"]);
    }
}
