use crate::args::Format;
use playtime_client::{Error, state::StateError};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Debug, Clone)]
pub struct Failure {
    pub exit: i32,
    pub data: Value,
}
impl Failure {
    pub fn new(exit: i32, code: &str, message: impl AsRef<str>) -> Self {
        Self {
            exit,
            data: json!({"exit_code":exit,"code":code,"message":message.as_ref()}),
        }
    }
    pub fn context(mut self, recovery: Value) -> Self {
        self.data["recovery"] = recovery;
        self
    }
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        let exit = match &error {
            Error::Api { status, response } => match response.code.as_str() {
                "unauthenticated" | "forbidden_origin" | "forbidden_host" => 4,
                "unsupported_version"
                | "unsupported_capability"
                | "archive_corrupt"
                | "content_mismatch"
                | "evidence_limit_exceeded" => 6,
                "backend_unavailable"
                | "ownership_unresolved"
                | "request_timeout"
                | "too_many_observers" => 3,
                _ if matches!(status, 401 | 403) => 4,
                _ if *status >= 500 => 3,
                _ => 5,
            },
            Error::Transport | Error::Timeout | Error::RetryBudgetExhausted => 3,
            Error::IdentityMismatch => 5,
            Error::UnsupportedEndpoint => 2,
            Error::InvalidResponse | Error::LimitExceeded | Error::ContentMismatch => 6,
            Error::Io => 7,
            Error::AmbiguousMutation(_) => 3,
            Error::Cancelled => 130,
            Error::UnacknowledgedRecord => 1,
        };
        let mut failure = Self::new(
            exit,
            match &error {
                Error::AmbiguousMutation(_) => "unresolved_mutation",
                Error::IdentityMismatch => "service_identity_mismatch",
                _ => "client_error",
            },
            error.to_string(),
        );
        if let Error::Api { status, response } = &error {
            failure.data = serde_json::to_value(response).unwrap();
            failure.data["exit_code"] = json!(exit);
            failure.data["http_status"] = json!(status);
        }
        if let Error::AmbiguousMutation(cause) = error {
            failure.data["cause"] = Failure::from(*cause).data;
        }
        failure
    }
}
impl From<StateError> for Failure {
    fn from(e: StateError) -> Self {
        Self::new(
            match e {
                StateError::Io => 7,
                StateError::Invalid => 6,
                _ => 5,
            },
            "client_state",
            e.to_string(),
        )
    }
}
impl From<playtime_client::config::ConfigError> for Failure {
    fn from(e: playtime_client::config::ConfigError) -> Self {
        Self::new(2, "configuration", e.to_string())
    }
}

pub fn envelope(kind: &str, data: impl Serialize) -> Value {
    json!({"schema_version":1,"kind":kind,"data":data})
}
pub fn escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn word(value: &Value) -> String {
    escape(
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if value.is_null() {
                    "unavailable".into()
                } else {
                    value.to_string()
                }
            })
            .as_str(),
    )
}
fn run_summary(view: &Value) -> String {
    let usage = if view["usage"].is_null() {
        "Usage: unavailable".into()
    } else {
        format!(
            "Usage: {} known + {} unknown reserved = {} charged; complete: {}",
            word(&view["usage"]["known_generated_tokens"]),
            word(&view["usage"]["unknown_reserved_tokens"]),
            word(&view["usage"]["budget_charged_tokens"]),
            word(&view["usage"]["usage_complete"])
        )
    };
    format!(
        "Run {}: {} | backend {} | mode {}\nRecipe {} @ {} | phase {} | outcome {}\nCleanup: {} | evidence: {} | checkpoint: {}\n{}\nCancellation: {}",
        word(&view["run_id"]),
        word(&view["orchestration"]["state"]),
        word(&view["backend"]["kind"]),
        word(&view["mode"]),
        word(&view["recipe_id"]),
        word(&view["preview_revision"]),
        word(&view["controller_phase"]),
        word(&view["outcome"]["reason"]),
        word(&view["cleanup"]["status"]),
        word(&view["evidence"]["status"]),
        word(&view["checkpoint"]["session_id"]),
        usage,
        if view["cancellation"].is_null() {
            "none".into()
        } else {
            word(&view["cancellation"])
        }
    )
}
fn launch_summary(data: &Value) -> String {
    format!(
        "Accepted {} | key {} | replayed {}\nReceipt: {}",
        word(&data["acceptance"]["run_id"]),
        word(&data["receipt"]["key"]),
        word(&data["acceptance"]["replayed"]),
        word(&data["receipt_path"])
    )
}
fn text(value: &Value) -> String {
    let kind = value["kind"].as_str().unwrap_or("result");
    let data = &value["data"];
    match kind {
        "event" => format!(
            "{} {} {} {}",
            word(&data["event"]["run_id"]),
            word(&data["event"]["sequence"]),
            word(&data["event"]["kind"]),
            word(&data["event"]["data"])
        ),
        "error" => format!(
            "{}: {}{}",
            word(&data["code"]),
            word(&data["message"]),
            data.get("recovery")
                .map(|r| format!(" (recovery: {})", word(r)))
                .unwrap_or_default()
        ),
        "notice" if data.get("backend").is_some() => format!(
            "Backend: {} | service {} | {}",
            word(&data["backend"]["kind"]),
            word(&data["service_id"]),
            word(&data["backend"]["description"])
        ),
        "notice" if data.get("saved_launch").is_some() => format!(
            "Saved launch {}: {}",
            word(&data["saved_launch"]["key"]),
            word(&data["saved_launch"]["receipt_path"])
        ),
        "notice" if data.get("stream_end").is_some() => format!(
            "Stream reconciled: {} (through {})",
            word(&data["stream_end"]["status"]),
            word(&data["stream_end"]["frontier"]["prefix"]["sequence"])
        ),
        "launch" if data.get("acceptance").is_some() => launch_summary(data),
        "page" if data["items"].is_array() => {
            let mut rows: Vec<String> = data["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    if item.get("run_id").is_some() && item.get("orchestration").is_some() {
                        format!(
                            "{}  {}  {}  {}",
                            word(&item["run_id"]),
                            word(&item["orchestration"]["state"]),
                            word(&item["recipe_id"]),
                            word(&item["backend"]["kind"])
                        )
                    } else if item.get("key").is_some() {
                        format!(
                            "{}  service={}  run={}",
                            word(&item["key"]),
                            word(&item["service_id"]),
                            if item["run_id"].is_null() {
                                "unresolved".into()
                            } else {
                                word(&item["run_id"])
                            }
                        )
                    } else if item.get("artifact_id").is_some() {
                        format!(
                            "{}  {}  {} bytes  {}",
                            word(&item["artifact_id"]),
                            word(&item["kind"]),
                            word(&item["content"]["size_bytes"]),
                            word(&item["name"])
                        )
                    } else if item.get("recipe_id").is_some() {
                        format!(
                            "{}  {}  modes={}",
                            word(&item["recipe_id"]),
                            word(&item["name"]),
                            word(&item["modes"])
                        )
                    } else if item.get("checkpoint").is_some() {
                        format!(
                            "{}  published at {}",
                            word(&item["checkpoint"]["session_id"]),
                            word(&item["checkpoint"]["published_sequence"])
                        )
                    } else {
                        word(item)
                    }
                })
                .collect();
            if rows.is_empty() {
                rows.push("No items".into());
            }
            if !data["next_cursor"].is_null() {
                rows.push(format!("Next cursor: {}", word(&data["next_cursor"])));
            }
            rows.join("\n")
        }
        "page" if data.get("frontier").is_some() => format!(
            "History through {} | more: {}",
            word(&data["frontier"]["prefix"]["sequence"]),
            word(&data["has_more"])
        ),
        "result" if data.get("capabilities").is_some() => {
            let c = &data["capabilities"];
            format!(
                "Service {} | build {} | backend {}\nModes: {}\nFeatures: {}\nUnsupported adapters: {}\nVersions: API {} / archive {} / event {}\nLimits: {}\nConnection: {}",
                word(&c["service_id"]),
                word(&c["service_build"]),
                word(&c["backend"]["kind"]),
                word(&c["modes"]),
                word(&c["features"]),
                word(&c["unsupported_adapters"]),
                word(&c["api_versions"]),
                word(&c["archive_versions"]),
                word(&c["event_versions"]),
                word(&c["limits"]),
                word(&data["connection"])
            )
        }
        "result" if data.get("verified_bytes").is_some() => format!(
            "Verified {} bytes to {} (artifact {})",
            word(&data["verified_bytes"]),
            word(&data["output"]),
            word(&data["artifact"]["artifact_id"])
        ),
        _ if data.get("run").is_some() => {
            let mut lines = Vec::new();
            if let Some(launch) = data.get("launch") {
                lines.push(launch_summary(launch));
            }
            lines.push(run_summary(&data["run"]));
            if let Some(code) = data.get("exit_code") {
                lines.push(format!("Observer result: {}", word(code)));
            }
            lines.join("\n")
        }
        _ if data.get("orchestration").is_some() => run_summary(data),
        _ => serde_json::to_string_pretty(data).unwrap(),
    }
}
pub fn bytes(format: Format, value: &Value) -> Result<Vec<u8>, Failure> {
    let rendered = match format {
        Format::Text => text(value)
            .lines()
            .map(escape)
            .collect::<Vec<_>>()
            .join("\n"),
        Format::Json => serde_json::to_string_pretty(value)
            .map_err(|_| Failure::new(1, "serialization", "cannot serialize output"))?,
        Format::Jsonl => serde_json::to_string(value)
            .map_err(|_| Failure::new(1, "serialization", "cannot serialize output"))?,
    };
    Ok(format!("{rendered}\n").into_bytes())
}
#[derive(Clone, Copy)]
pub struct Output(pub Format);
impl Output {
    pub async fn emit(&self, kind: &str, data: impl Serialize) -> Result<(), Failure> {
        self.write(false, envelope(kind, data)).await
    }
    pub async fn diagnostic(&self, kind: &str, data: impl Serialize) -> Result<(), Failure> {
        self.write(true, envelope(kind, data)).await
    }
    async fn write(&self, stderr: bool, value: Value) -> Result<(), Failure> {
        let format = if stderr && self.0 != Format::Text {
            Format::Jsonl
        } else {
            self.0
        };
        let bytes = bytes(format, &value)?;
        tokio::task::spawn_blocking(move || {
            let mut stream: Box<dyn Write> = if stderr {
                Box::new(std::io::stderr().lock())
            } else {
                Box::new(std::io::stdout().lock())
            };
            stream
                .write_all(&bytes)
                .and_then(|_| stream.flush())
                .map_err(|_| {
                    Failure::new(
                        7,
                        "output_io",
                        "cannot write or flush output (possibly a closed pipe)",
                    )
                })
        })
        .await
        .map_err(|_| Failure::new(1, "output_task", "output task failed"))?
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_contract_and_controls() {
        let value = envelope(
            "event",
            json!({"event":{"run_id":"r","sequence":1,"kind":"future","data":{"text":"\u{1b}[31m\n\u{202e}x","unknown":7}},"cursor":"opaque"}),
        );
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/output.json")).unwrap();
        assert_eq!(value, fixture);
        assert!(!bytes(Format::Text, &value).unwrap().contains(&27));
        let bytes = bytes(Format::Jsonl, &value).unwrap();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
    }
}
