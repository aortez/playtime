use std::num::NonZeroU64;

use playtime_core::{GenerationUsage, UsageTotals};
use playtime_protocol::v1::{ErrorResponse, MALFORMED_DATA, UNSUPPORTED_VERSION, UsageSummary};
use serde_json::{Value, json};
use utoipa::OpenApi;

#[test]
fn unknown_usage_matches_the_synthetic_legacy_expectation() {
    let mut totals = UsageTotals::default();
    totals
        .record(GenerationUsage::Unknown {
            reserved_tokens: NonZeroU64::new(100).unwrap(),
        })
        .unwrap();
    totals
        .record(GenerationUsage::Known {
            generated_tokens: 10,
        })
        .unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/usage-unknown.json")).unwrap();
    let summary = UsageSummary::from(totals);
    assert_eq!(serde_json::to_value(&summary).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<UsageSummary>(expected).unwrap(),
        summary
    );
}

#[test]
fn zero_and_false_survive_serialization() {
    let mut totals = UsageTotals::default();
    totals
        .record(GenerationUsage::Known {
            generated_tokens: 0,
        })
        .unwrap();
    totals
        .record(GenerationUsage::Unknown {
            reserved_tokens: NonZeroU64::new(50).unwrap(),
        })
        .unwrap();
    let value = serde_json::to_value(UsageSummary::from(totals)).unwrap();
    assert_eq!(value["known_generated_tokens"], 0);
    assert_eq!(value["usage_complete"], false);
}

#[test]
fn error_fixture_preserves_structured_details_and_opaque_request_id() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/error-unsupported-version.json")).unwrap();
    let error: ErrorResponse = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(error.code, UNSUPPORTED_VERSION);
    assert_ne!(error.code, MALFORMED_DATA);
    assert_eq!(
        error.request_id.as_ref().map(|id| id.as_str()),
        Some("request-fixture-1")
    );
    assert_eq!(serde_json::to_value(error).unwrap(), fixture);
}

#[test]
fn errors_accept_new_codes_and_optional_fields_without_inventing_a_request_id() {
    let error: ErrorResponse = serde_json::from_value(json!({
        "code": "future_error", "message": "New failure", "details": {},
        "future_optional_field": {"value": true}
    }))
    .unwrap();
    assert_eq!(error.code, "future_error");
    assert_eq!(error.request_id, None);
    assert!(
        serde_json::to_value(error)
            .unwrap()
            .get("request_id")
            .is_none()
    );
}

#[test]
fn errors_reject_missing_required_fields_and_malformed_details() {
    for value in [
        json!({"code": MALFORMED_DATA, "details": {}}),
        json!({"code": 0, "message": "Bad data", "details": {}}),
        json!({"code": MALFORMED_DATA, "message": "Bad data", "details": null}),
    ] {
        assert!(serde_json::from_value::<ErrorResponse>(value).is_err());
    }
}

#[test]
fn openapi_includes_the_read_models_without_a_server() {
    #[derive(OpenApi)]
    #[openapi(components(schemas(ErrorResponse, UsageSummary)))]
    struct Contracts;

    let document = serde_json::to_value(Contracts::openapi()).unwrap();
    let schemas = &document["components"]["schemas"];
    assert_eq!(
        schemas["UsageSummary"]["properties"]["usage_complete"]["type"],
        "boolean"
    );
    assert_eq!(
        schemas["ErrorResponse"]["properties"]["details"]["type"],
        "object"
    );
    let required = schemas["ErrorResponse"]["required"].as_array().unwrap();
    assert!(required.contains(&json!("code")));
    assert!(!required.contains(&json!("request_id")));
}
