use playtime_core::{CommandOutcome, GenerationUsage, RunMode, StopReason, UsageTotals};
use playtime_protocol::{
    ReadError,
    archive::decode_input_manifest,
    events::{
        CleanupOutcome, ContextStart, EventOrder, EventPayload, KnownEvent, MemoryRef, decode_event,
    },
    v1::{UsageSummary, decode_run_parameters},
};
use serde_json::{Value, json};

const TRACE: &str = include_str!("fixtures/two-session/events.jsonl");
const MANIFEST: &str = include_str!("fixtures/two-session/inputs/manifest.json");
const PARAMETERS: &str = include_str!("fixtures/two-session/inputs/configuration.json");

fn event(kind: &str) -> Value {
    TRACE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|value| value["kind"] == kind)
        .unwrap()
}

#[test]
fn two_session_trace_preserves_memory_and_unknown_usage_through_completion() {
    let mut order = EventOrder::default();
    let mut totals = UsageTotals::default();
    let mut checkpoint = None;
    let mut sessions = 0;
    let mut endings = Vec::new();
    let mut terminal = false;
    for line in TRACE.lines() {
        let decoded = decode_event(line).unwrap();
        order.observe(&decoded).unwrap();
        assert_eq!(
            decoded.envelope().run_id.as_str(),
            "run-two-session-fixture"
        );
        assert!(!terminal, "fixture has events after its terminal result");
        assert_eq!(
            serde_json::to_value(decoded.envelope()).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
        let Some(payload) = decoded.payload().known() else {
            panic!("fixture kind must be known")
        };
        let serialized = serde_json::to_value(payload).unwrap();
        assert_eq!(serialized["kind"], decoded.envelope().kind);
        assert_eq!(serialized["data"], decoded.envelope().data);
        match payload {
            KnownEvent::RunStarted {
                mode,
                input_manifest,
            } => {
                assert_eq!(*mode, RunMode::Simulation);
                assert_eq!(input_manifest.path.as_str(), "inputs/manifest.json");
            }
            KnownEvent::SessionStarted {
                ordinal,
                context,
                workspace,
                memento,
                evidence,
                ..
            } => {
                sessions += 1;
                assert_eq!(ordinal.get(), sessions);
                assert_eq!(*context, ContextStart::Fresh);
                if let Some(previous) = &checkpoint {
                    let previous: &playtime_protocol::events::CheckpointRef = previous;
                    assert_eq!(workspace, &previous.workspace);
                    assert_eq!(memento, &previous.memento);
                    assert_eq!(evidence, &previous.evidence);
                } else {
                    assert_eq!(*memento, MemoryRef::Missing);
                    assert!(evidence.is_none());
                }
            }
            KnownEvent::RequestFinished { usage, .. } => totals.record(*usage).unwrap(),
            KnownEvent::CommandFinished { outcome, .. } => {
                assert_eq!(*outcome, CommandOutcome::Exited { exit_code: 0 })
            }
            KnownEvent::SessionEnded {
                finish_run,
                handoff_saved,
                ..
            } => {
                assert!(*handoff_saved);
                endings.push(*finish_run);
            }
            KnownEvent::CheckpointPublished {
                checkpoint: published,
            } => checkpoint = Some(published.clone()),
            KnownEvent::RunFinished {
                reason,
                usage,
                cleanup,
                last_checkpoint,
                recovery_origin,
            } => {
                assert!(recovery_origin.is_none());
                assert_eq!(*reason, StopReason::AgentFinished);
                assert_eq!(*usage, UsageSummary::from(totals));
                assert!(!usage.usage_complete);
                assert_eq!(usage.budget_charged_tokens, 110);
                assert_eq!(*cleanup, CleanupOutcome::Complete);
                assert_eq!(last_checkpoint, &checkpoint);
                terminal = true;
            }
            KnownEvent::PhaseChanged { .. } => {}
        }
    }
    assert_eq!(sessions, 2);
    assert_eq!(endings, [false, true]);
    assert!(terminal);
}

#[test]
fn unsupported_versions_are_distinct_even_when_the_new_body_is_unrecognizable() {
    for version in [0, 2, u32::MAX] {
        let input = json!({"schema_version": version, "future_format": []}).to_string();
        for result in [
            decode_event(&input).map(|_| ()),
            decode_input_manifest(&input).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(
                matches!(error, ReadError::UnsupportedVersion { received, .. } if received == version)
            );
            let response = error.into_response(None);
            assert_eq!(response.code, "unsupported_version");
            assert_eq!(response.details["received"], version);
        }
    }
    for input in [
        "{",
        "{}",
        "[]",
        r#"{"schema_version":true}"#,
        r#"{"schema_version":"1"}"#,
        r#"{"schema_version":1.0}"#,
    ] {
        assert!(matches!(
            decode_event(input),
            Err(ReadError::MalformedData(_))
        ));
        assert!(matches!(
            decode_input_manifest(input),
            Err(ReadError::MalformedData(_))
        ));
    }
}

#[test]
fn unknown_events_remain_visible_and_retain_their_payload() {
    let mut value = event("run_started");
    value["kind"] = json!("future_observation");
    for data in [
        json!({"html": "<script>λ</script>", "zero": 0, "flag": false, "nested": [null]}),
        json!([1, false]),
        Value::Null,
    ] {
        value["data"] = data.clone();
        let decoded = decode_event(&value.to_string()).unwrap();
        assert_eq!(
            decoded.payload(),
            &EventPayload::Unknown {
                kind: "future_observation".into(),
                data
            }
        );
        assert_eq!(serde_json::to_value(decoded.envelope()).unwrap(), value);
    }
}

#[test]
fn known_events_allow_optional_extensions_without_hiding_malformed_known_data() {
    let mut value = event("session_ended");
    value["future_envelope_field"] = json!("compatible");
    value["data"]["future_payload_field"] = json!({"flag": false});
    let decoded = decode_event(&value.to_string()).unwrap();
    assert!(matches!(
        decoded.payload().known(),
        Some(KnownEvent::SessionEnded {
            finish_run: false,
            ..
        })
    ));
    assert_eq!(decoded.envelope().data, value["data"]);
    value["data"]["finish_run"] = json!(0);
    assert!(matches!(
        decode_event(&value.to_string()),
        Err(ReadError::MalformedData(_))
    ));
}

#[test]
fn malformed_ids_sequences_times_and_missing_usage_are_rejected() {
    for (field, invalid) in [
        ("run_id", json!("../outside")),
        ("run_id", json!("")),
        ("sequence", json!(0)),
        ("sequence", json!(-1)),
        ("sequence", json!(true)),
        ("time", json!("2026-02-30T00:00:00Z")),
        ("time", json!("2026-10-17T12:00:00")),
        ("kind", json!("")),
    ] {
        let mut value = event("request_finished");
        value[field] = invalid;
        assert!(decode_event(&value.to_string()).is_err(), "{value}");
    }
    for usage in [
        Value::Null,
        json!({}),
        json!({"status":"known"}),
        json!({"status":"unknown", "reserved_tokens":0}),
        json!({"status":"known", "generated_tokens":false}),
    ] {
        let mut value = event("request_finished");
        value["data"]["usage"] = usage;
        assert!(decode_event(&value.to_string()).is_err(), "{value}");
    }
    let mut value = event("request_finished");
    value["data"]["usage"] = json!({"status": "known", "generated_tokens": 0});
    assert!(matches!(
        decode_event(&value.to_string()).unwrap().payload().known(),
        Some(KnownEvent::RequestFinished {
            usage: GenerationUsage::Known {
                generated_tokens: 0
            },
            ..
        })
    ));
}

#[test]
fn sequence_order_is_per_run_and_does_not_advance_after_rejection() {
    let mut order = EventOrder::default();
    let mut value = event("run_started");
    let first = decode_event(&value.to_string()).unwrap();
    order.observe(&first).unwrap();
    assert!(order.observe(&first).is_err());
    value["sequence"] = json!(3);
    order
        .observe(&decode_event(&value.to_string()).unwrap())
        .unwrap();
    value["sequence"] = json!(2);
    assert!(
        order
            .observe(&decode_event(&value.to_string()).unwrap())
            .is_err()
    );
    value["sequence"] = json!(3);
    assert!(
        order
            .observe(&decode_event(&value.to_string()).unwrap())
            .is_err()
    );
    value["sequence"] = json!(u64::MAX);
    order
        .observe(&decode_event(&value.to_string()).unwrap())
        .unwrap();
    value["run_id"] = json!("another-run");
    value["sequence"] = json!(1);
    order
        .observe(&decode_event(&value.to_string()).unwrap())
        .unwrap();
}

#[test]
fn inconsistent_terminal_accounting_cannot_be_read_as_a_valid_result() {
    for (field, invalid) in [
        ("budget_charged_tokens", json!(10)),
        ("unknown_requests", json!(0)),
        ("unknown_reserved_tokens", json!(0)),
        ("usage_complete", json!(true)),
        ("known_generated_tokens", json!(u64::MAX)),
    ] {
        let mut value = event("run_finished");
        value["data"]["usage"][field] = invalid;
        assert!(decode_event(&value.to_string()).is_err(), "{value}");
    }
}

#[test]
fn missing_memory_unknown_commands_and_unverified_cleanup_are_distinct() {
    let unavailable: MemoryRef =
        serde_json::from_value(json!({"status":"unavailable", "reason":"oversized"})).unwrap();
    assert_ne!(unavailable, MemoryRef::Missing);
    let unknown: CommandOutcome =
        serde_json::from_value(json!({"status":"unknown", "reason":"result lost"})).unwrap();
    assert_ne!(unknown, CommandOutcome::Exited { exit_code: 0 });
    let timeout: CommandOutcome =
        serde_json::from_value(json!({"status":"timed_out", "exit_code":null})).unwrap();
    assert_eq!(timeout, CommandOutcome::TimedOut { exit_code: None });
    let mut value = event("run_finished");
    value["data"]["cleanup"] = json!({"status":"unknown"});
    assert!(matches!(
        decode_event(&value.to_string()).unwrap().payload().known(),
        Some(KnownEvent::RunFinished {
            cleanup: CleanupOutcome::Unknown,
            ..
        })
    ));
    value["data"]["cleanup"] = json!({"status":"failed", "errors":[]});
    assert!(decode_event(&value.to_string()).is_err());
}

#[test]
fn manifest_round_trip_keeps_worker_identity_separate_from_other_builds() {
    let manifest = decode_input_manifest(MANIFEST).unwrap();
    assert!(!manifest.worker.build.dirty);
    assert_eq!(manifest.worker.platform.os, "linux");
    assert_eq!(manifest.references().count(), 10);
    assert_eq!(
        serde_json::to_value(&manifest).unwrap(),
        serde_json::from_str::<Value>(MANIFEST).unwrap()
    );
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["viewer_build"] = json!("a-newer-viewer");
    assert_eq!(
        decode_input_manifest(&value.to_string()).unwrap().worker,
        manifest.worker
    );
}

#[test]
fn manifest_rejects_ambiguous_or_escaping_input_references() {
    for path in [
        "inputs/manifest.json",
        "../external",
        "/etc/passwd",
        "inputs/../outside",
        "sessions/s1/memento.txt",
        "inputs//duplicate",
    ] {
        let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
        value["configuration"]["path"] = json!(path);
        assert!(decode_input_manifest(&value.to_string()).is_err(), "{path}");
    }
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["files"][0] = value["configuration"].clone();
    assert!(decode_input_manifest(&value.to_string()).is_err());
    value = serde_json::from_str(MANIFEST).unwrap();
    value["worker"]["executable"]["sha256"] = json!("not-a-digest");
    assert!(decode_input_manifest(&value.to_string()).is_err());
}

#[test]
fn requested_settings_reject_typos_wrong_types_and_conflicting_reserves() {
    assert_eq!(decode_run_parameters(PARAMETERS).unwrap().sampling_seed, 0);
    let mut fractional: Value = serde_json::from_str(PARAMETERS).unwrap();
    fractional["workshop"]["cpu_cores"] = json!(0.123456);
    assert_eq!(
        decode_run_parameters(&fractional.to_string())
            .unwrap()
            .workshop
            .cpu_cores,
        0.123456
    );
    for invalid in [json!(0), json!(65), json!(false), json!("NaN")] {
        fractional["workshop"]["cpu_cores"] = invalid;
        assert!(decode_run_parameters(&fractional.to_string()).is_err());
    }
    for (field, invalid) in [
        ("generated_tokens", json!(50)),
        ("context_tokens", json!(100)),
        ("sessions", json!(0)),
        ("wall_seconds", json!(false)),
        ("tokens_per_call", json!(u64::MAX)),
        ("tool_parse_retries", json!(11)),
        ("tool_parse_error_limit", json!(0)),
        ("tool_parse_error_limit", Value::Null),
        ("wall_seconds", json!("")),
        ("wall_second", json!(100)),
    ] {
        let mut value: Value = serde_json::from_str(PARAMETERS).unwrap();
        value["limits"][field] = invalid;
        assert!(
            decode_run_parameters(&value.to_string()).is_err(),
            "{value}"
        );
    }
    for (field, invalid) in [
        ("sampling_seed", json!(2147483648_u64)),
        ("sampling_seed", json!(false)),
        ("memory_condition", json!("future")),
        ("future_requested_setting", json!(true)),
    ] {
        let mut value: Value = serde_json::from_str(PARAMETERS).unwrap();
        value[field] = invalid;
        assert!(decode_run_parameters(&value.to_string()).is_err());
    }
    let mut value: Value = serde_json::from_str(PARAMETERS).unwrap();
    value["workshop"]["memory_bytes"] = value["workshop"]["workspace_bytes"].clone();
    assert!(decode_run_parameters(&value.to_string()).is_err());
    value = serde_json::from_str(PARAMETERS).unwrap();
    value["workshop"]["memory_mb"] = json!(512);
    assert!(decode_run_parameters(&value.to_string()).is_err());
}

#[test]
fn documented_legacy_defaults_are_explicit_without_truthiness() {
    let mut value: Value = serde_json::from_str(PARAMETERS).unwrap();
    value.as_object_mut().unwrap().remove("memory_condition");
    value["limits"]
        .as_object_mut()
        .unwrap()
        .remove("tool_parse_retries");
    value["limits"]
        .as_object_mut()
        .unwrap()
        .remove("tool_parse_error_limit");
    let parameters = decode_run_parameters(&value.to_string()).unwrap();
    assert_eq!(
        parameters.memory_condition,
        playtime_core::MemoryCondition::SelfWritten
    );
    assert_eq!(parameters.limits.tool_parse_retries, 0);
    assert_eq!(parameters.limits.tool_parse_error_limit, None);
}

#[test]
fn event_error_and_archive_schema_matches_the_reviewed_golden() {
    let expected: Value = serde_json::from_str(include_str!("fixtures/schema.json")).unwrap();
    let schema = playtime_protocol::schema_document();
    assert_eq!(serde_json::to_value(&schema).unwrap(), expected);
    for (name, value) in expected["components"]["schemas"].as_object().unwrap() {
        serde_json::from_value::<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>(
            value.clone(),
        )
        .unwrap_or_else(|error| panic!("schema {name}: {error}"));
    }
    let restored: utoipa::openapi::OpenApi = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), expected);
}

#[test]
fn docker_requires_the_exact_advertised_profile_and_older_bundles_stay_readable() {
    use playtime_core::Sha256Digest;
    use playtime_protocol::worker_bundle::Capabilities;
    let profile = Sha256Digest::new("a".repeat(64)).unwrap();
    let older: Capabilities = serde_json::from_value(json!({
        "private_protocol": 1, "manifest_schema": 1, "event_schema": 1,
        "controller_simulation": true, "real_model": false, "real_workshop": true
    }))
    .unwrap();
    assert!(older.supported());
    assert!(!older.implements_docker(&profile));
    let current = Capabilities {
        docker_profile: Some(profile.clone()),
        ..Capabilities::current()
    };
    assert!(current.supported());
    assert!(current.implements_docker(&profile));
    assert!(!current.implements_docker(&Sha256Digest::new("b".repeat(64)).unwrap()));
    for changed in [
        Capabilities {
            real_model: true,
            ..current.clone()
        },
        Capabilities {
            real_workshop: false,
            ..current
        },
    ] {
        assert!(!changed.implements_docker(&profile));
    }
}
