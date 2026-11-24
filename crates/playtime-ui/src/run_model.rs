//! Display identity from a verified archived configuration, never a recipe title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedModel {
    pub name: String,
    /// What `digest` identifies: an Ollama model's digest, or the artifacts
    /// manifest of a Strata lock's host.
    pub digest_label: &'static str,
    pub digest: String,
    pub endpoint: Option<String>,
    pub details: Vec<String>,
}

pub fn parse(bytes: &[u8]) -> Result<Option<ArchivedModel>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "Invalid archived configuration")?;
    let Some(model) = value.get("model") else {
        return Ok(None);
    };
    if model.is_null() {
        return Ok(None);
    }
    let text = |pointer: &str, limit| {
        model
            .pointer(pointer)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty() && s.len() <= limit && !s.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(|| "Archived model identity unavailable".to_owned())
    };
    let strata = model.get("provider").and_then(|p| p.as_str()) == Some("strata");
    let (name, digest) = if strata {
        (
            text("/lock/identity/model", 512)?,
            text("/lock/host/artifacts_manifest_sha256", 64)?,
        )
    } else {
        (text("/lock/name", 512)?, text("/lock/digest", 64)?)
    };
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid archived model digest".into());
    }
    let endpoint = if model.pointer("/host/endpoint").is_some() {
        Some(text("/host/endpoint", 2048)?)
    } else {
        None
    };
    let field = |pointer: &str| match model.pointer(pointer) {
        Some(serde_json::Value::String(s))
            if s.len() <= 128 && !s.chars().any(char::is_control) =>
        {
            s.clone()
        }
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => "not recorded".into(),
    };
    let reasoning = match model.pointer("/settings/thinking") {
        Some(serde_json::Value::Bool(false)) => "off".into(),
        Some(serde_json::Value::Bool(true)) => "on".into(),
        // The runner archives the model's own default as an explicit null.
        Some(serde_json::Value::Null) => "model default".into(),
        _ => field("/settings/thinking"),
    };
    if strata {
        // The lock's reviewed settings, which every request sent.
        return Ok(Some(ArchivedModel {
            name,
            digest_label: "Artifacts",
            digest,
            endpoint,
            details: vec![
                format!("Strata lock: {}", field("/lock_name")),
                format!(
                    "Engine: {} · Backend: {} · Quantization: {}",
                    field("/lock/identity/engine"),
                    field("/lock/host/backend"),
                    field("/lock/host/quantization")
                ),
                format!(
                    "Server context: {} tokens",
                    field("/lock/identity/context_tokens")
                ),
                format!(
                    "Reasoning: {reasoning} · Summary reasoning: {}",
                    field("/settings/summary_thinking")
                ),
                format!(
                    "Temperature: {} · Top-p: {}",
                    field("/settings/temperature"),
                    field("/settings/top_p")
                ),
                format!(
                    "Top-k: {} · Min-p: {} · Repetition penalty: {}",
                    field("/settings/top_k"),
                    field("/settings/min_p"),
                    field("/settings/repetition_penalty")
                ),
                format!(
                    "Presence penalty: {} · Frequency penalty: {} · Penalty window: {}",
                    field("/settings/presence_penalty"),
                    field("/settings/frequency_penalty"),
                    field("/settings/penalty_last_n")
                ),
                format!(
                    "Launcher profile: {} · Server start: {}",
                    field("/host/profile"),
                    field("/host/started")
                ),
            ],
        }));
    }
    let details = vec![
        format!(
            "Size: {} · Quantization: {}",
            field("/lock/definition/details/parameter_size"),
            field("/lock/definition/details/quantization_level")
        ),
        format!("Context: {} tokens", field("/settings/context_tokens")),
        format!("Reasoning: {reasoning}"),
        format!(
            "Temperature: {} · Top-p: {}",
            field("/settings/temperature"),
            field("/settings/top_p")
        ),
        format!(
            "Top-k: {} · Repeat penalty: {}",
            field("/settings/top_k"),
            field("/settings/repeat_penalty")
        ),
        format!("Ollama: {}", field("/lock/ollama_version")),
    ];
    Ok(Some(ArchivedModel {
        name,
        digest_label: "Digest",
        digest,
        endpoint,
        details,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_comes_from_lock_not_inherited_title() {
        let config = serde_json::json!({
            "recipe": {"name": "Curiosity on gpt-oss:20b"},
            "model": {
                "lock": {"name": "qwen3-coder-next:q4_K_M", "digest": "c".repeat(64),
                    "definition": {"details": {"parameter_size": "79.7B", "quantization_level": "Q4_K_M"}}},
                "settings": {"context_tokens": 65536, "thinking": false, "temperature": 1.0, "top_p": 0.95},
                "host": {"endpoint": "http://framework.local:11434/"}
            }
        });
        let model = parse(&serde_json::to_vec(&config).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(model.name, "qwen3-coder-next:q4_K_M");
        assert!(model.details.iter().any(|s| s == "Context: 65536 tokens"));
        assert!(model.details.iter().any(|s| s == "Reasoning: off"));
        assert!(
            model
                .details
                .iter()
                .any(|s| s == "Size: 79.7B · Quantization: Q4_K_M")
        );
        assert_eq!(
            model.endpoint.as_deref(),
            Some("http://framework.local:11434/")
        );
        assert_eq!(parse(br#"{"scenario":{}}"#).unwrap(), None);
        assert!(parse(br#"{"model":{"lock":{"name":"gpt-oss:20b"}}}"#).is_err());
        // An omitted choice is recorded as null; only a missing key is unrecorded.
        let reasoning = |settings: serde_json::Value| {
            let mut config = config.clone();
            config["model"]["settings"] = settings;
            let model = parse(&serde_json::to_vec(&config).unwrap())
                .unwrap()
                .unwrap();
            model.details[2].clone()
        };
        assert_eq!(
            reasoning(serde_json::json!({"thinking": null})),
            "Reasoning: model default"
        );
        assert_eq!(
            reasoning(serde_json::json!({"thinking": "high"})),
            "Reasoning: high"
        );
        assert_eq!(reasoning(serde_json::json!({})), "Reasoning: not recorded");
    }

    #[test]
    fn a_strata_model_comes_from_its_lock_and_held_host() {
        let config = serde_json::json!({
            "recipe": {"name": "Live smoke"},
            "model": {
                "provider": "strata", "lock_name": "example-strata-lock-v2",
                "lock": {"identity": {"model": "example-model", "engine": "0.1.58",
                        "context_tokens": 163840},
                    "host": {"backend": "hip", "quantization": "TQ2_0",
                        "artifacts_manifest_sha256": "a".repeat(64)}},
                "settings": {"thinking": "medium", "summary_thinking": "none",
                    "temperature": 0.6, "top_p": 0.95, "top_k": 20, "min_p": 0.0,
                    "repetition_penalty": 1.0, "presence_penalty": 0.0,
                    "frequency_penalty": 0.0, "penalty_last_n": 64},
                "host": {"profile": "example-profile", "endpoint": "http://127.0.0.1:41000/v1",
                    "started": 123}
            }
        });
        let model = parse(&serde_json::to_vec(&config).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(model.name, "example-model");
        assert_eq!(
            (model.digest_label, model.digest.as_str()),
            ("Artifacts", "a".repeat(64).as_str())
        );
        assert_eq!(model.endpoint.as_deref(), Some("http://127.0.0.1:41000/v1"));
        assert_eq!(
            model.details,
            [
                "Strata lock: example-strata-lock-v2",
                "Engine: 0.1.58 · Backend: hip · Quantization: TQ2_0",
                "Server context: 163840 tokens",
                "Reasoning: medium · Summary reasoning: none",
                "Temperature: 0.6 · Top-p: 0.95",
                "Top-k: 20 · Min-p: 0.0 · Repetition penalty: 1.0",
                "Presence penalty: 0.0 · Frequency penalty: 0.0 · Penalty window: 64",
                "Launcher profile: example-profile · Server start: 123",
            ]
        );
        // A Strata record never falls back to Ollama's fields.
        let mut missing = config;
        missing["model"]["lock"]["name"] = "ollama-name".into();
        missing["model"]["lock"]["identity"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        assert!(parse(&serde_json::to_vec(&missing).unwrap()).is_err());
    }
}
