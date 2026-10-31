//! Display identity from a verified archived configuration, never a recipe title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedModel {
    pub name: String,
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
    let name = text("/lock/name", 512)?;
    let digest = text("/lock/digest", 64)?;
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
}
