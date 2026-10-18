mod common;
use common::*;
use playtime_ollama::*;
use serde_json::Value;

#[test]
fn exact_admitted_payload_zero_false_and_omission() {
    let mut request = request();
    let settings = EffectiveSettings {
        temperature: Some(0.0),
        top_p: Some(0.0),
        top_k: Some(0),
        repeat_penalty: Some(0.0),
        use_mmap: Some(false),
        ..Default::default()
    };
    let encoded = encode_request(&request, &settings, 4 * 1024 * 1024).unwrap();
    assert_eq!(encoded, include_bytes!("fixtures/request.expected.json"));
    let settings = EffectiveSettings {
        context_tokens: None,
        thinking: None,
        ..Default::default()
    };
    request.sampling_seed = 0;
    request.maximum_tokens = 1;
    let encoded = encode_request(&request, &settings, 4096).unwrap();
    let value: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value["messages"], serde_json::json!(request.messages));
    assert_eq!(value["tools"], serde_json::json!(request.tools));
    assert!(value.get("think").is_none());
    assert_eq!(
        value["options"],
        serde_json::json!({"num_predict":1,"seed":0})
    );
    assert!(encode_request(&request, &settings, encoded.len() - 1).is_err());
    assert_eq!(
        encode_request(&request, &settings, encoded.len()).unwrap(),
        encoded
    );
}
#[test]
fn unsupported_settings_are_refused_not_clamped() {
    let mut req = request();
    for settings in [
        EffectiveSettings {
            thinking: Some(Thinking::Boolean(false)),
            ..Default::default()
        },
        EffectiveSettings {
            thinking: Some(Thinking::Level("off".into())),
            ..Default::default()
        },
        EffectiveSettings {
            context_tokens: Some(0),
            ..Default::default()
        },
        EffectiveSettings {
            temperature: Some(f64::NAN),
            ..Default::default()
        },
        EffectiveSettings {
            top_p: Some(1.1),
            ..Default::default()
        },
        EffectiveSettings {
            keep_alive: "0".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(
            encode_request(&req, &settings, 4096).unwrap_err().kind,
            FailureKind::Request
        );
    }
    req.sampling_seed = 1 << 31;
    assert!(encode_request(&req, &EffectiveSettings::default(), 4096).is_err());
    req.sampling_seed = 1;
    req.maximum_tokens = 0;
    assert!(encode_request(&req, &EffectiveSettings::default(), 4096).is_err());
}
#[test]
fn endpoints_keep_base_paths_and_reject_ambient_credentials() {
    for base in [
        "http://127.0.0.1:99/prefix/",
        "https://example.test/prefix",
        "https://example.test/prefix%20space/",
    ] {
        let endpoint = Endpoint::parse(base).unwrap();
        assert_eq!(
            endpoint.url(Operation::Chat).as_str(),
            format!("{}/api/chat", base.trim_end_matches('/'))
        );
    }
    for base in [
        "ftp://host",
        "http://user:pass@host",
        "http://@host",
        "http://host?x",
        "http://host/#x",
        "relative/path",
    ] {
        assert!(Endpoint::parse(base).is_err(), "{base}");
    }
}

#[test]
fn frozen_resolution_and_trust_inputs_have_explicit_bounds() {
    for (endpoint, addresses) in [
        ("http://127.0.0.1", vec!["127.0.0.1".parse().unwrap()]),
        ("http://fixture.invalid", vec![]),
        (
            "http://fixture.invalid",
            vec!["127.0.0.1".parse().unwrap(); 17],
        ),
    ] {
        assert!(
            NativeTransport::with_resolution(
                Endpoint::parse(endpoint).unwrap(),
                Trust::BundledWebPki,
                Resolution::Pinned { addresses },
                &Limits::default()
            )
            .is_err()
        );
    }
    for certificates in [
        vec![],
        vec!["invalid certificate".into()],
        vec!["x".repeat(65537)],
    ] {
        assert!(
            NativeTransport::new(
                Endpoint::parse("https://fixture.invalid").unwrap(),
                Trust::PemOnly { certificates },
                &Limits::default()
            )
            .is_err()
        );
    }
}
