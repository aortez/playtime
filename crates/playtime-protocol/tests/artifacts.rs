use playtime_protocol::artifacts::{ArtifactCatalog, ArtifactKind};
use serde_json::{Value, json};

#[test]
fn artifact_v1_contract_preserves_identity_and_paired_source_without_checkpoint_fields() {
    // Synthetic metadata, not a claim about bytes in a native session.
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/artifact-catalog-v1.json")).unwrap();
    let catalog: ArtifactCatalog = serde_json::from_value(golden.clone()).unwrap();
    assert_eq!(catalog.artifacts[0].kind, ArtifactKind::Diagram);
    assert_eq!(
        catalog.artifacts[0].source.as_ref().unwrap().media_type,
        "text/plain"
    );
    assert_eq!(serde_json::to_value(&catalog).unwrap(), golden);
    let mut incompatible = golden;
    incompatible["artifacts"][0]["model_input"] = json!(true);
    assert!(serde_json::from_value::<ArtifactCatalog>(incompatible).is_err());
}
