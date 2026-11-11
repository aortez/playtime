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
                "execution_unavailable" if target_choice(&response.details) => 5,
                _ if matches!(status, 401 | 403) => 4,
                _ if *status >= 500 => 3,
                _ => 5,
            },
            Error::Transport | Error::Timeout | Error::RetryBudgetExhausted => 3,
            Error::IdentityMismatch => 5,
            Error::UnsupportedEndpoint => 2,
            Error::InvalidResponse
            | Error::LimitExceeded
            | Error::ContentMismatch
            | Error::UnsupportedCapability => 6,
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
/// A missing, unknown or misapplied target is the caller's choice to correct,
/// not an outage that a retry could clear.
fn target_choice(details: &serde_json::Map<String, Value>) -> bool {
    details
        .get("reasons")
        .and_then(Value::as_array)
        .is_some_and(|reasons| {
            !reasons.is_empty()
                && reasons.iter().all(|reason| {
                    matches!(
                        reason.as_str(),
                        Some(
                            "target_required" | "target_unknown" | "target_requires_live_execution"
                        )
                    )
                })
        })
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
    let mut usage = if view["usage"].is_null() {
        "Usage: unavailable".into()
    } else {
        format!(
            "{}: {} known + {} unknown reserved = {} charged; complete: {}",
            if view["accounting_coverage"].is_null() {
                "Usage"
            } else {
                "Settled usage"
            },
            word(&view["usage"]["known_generated_tokens"]),
            word(&view["usage"]["unknown_reserved_tokens"]),
            word(&view["usage"]["budget_charged_tokens"]),
            word(&view["usage"]["usage_complete"])
        )
    };
    if !view["accounting_coverage"].is_null() {
        usage.push_str(&format!(
            "\nPending allowance: {} across {} unsettled requests; coverage: {}",
            word(&view["accounting_coverage"]["unsettled_allowance"]),
            word(&view["accounting_coverage"]["unsettled_requests"]),
            word(&view["accounting_coverage"]["availability"]["status"])
        ));
    }
    if !view["target"].is_null() {
        usage.push_str(&format!(
            "\nTarget: {} ({}) · {}",
            word(&view["target"]["name"]),
            word(&view["target"]["id"]),
            word(&view["target"]["endpoint"])
        ));
    }
    let mut summary = format!(
        "Run {}: {} | backend {} | mode {}\nRecipe {} @ {} | phase {} | outcome {}\nCleanup: {} | evidence: {} | checkpoint: {}\n{}\nCancellation: {}",
        word(&view["run_id"]),
        if view["orchestration"]["reason"].is_null() {
            word(&view["orchestration"]["state"])
        } else {
            format!(
                "{} ({})",
                word(&view["orchestration"]["state"]),
                word(&view["orchestration"]["reason"])
            )
        },
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
    );
    if !view["orchestration"]["failure"].is_null() {
        summary.push_str(&format!(
            "\nWorker failure: {}",
            word(&view["orchestration"]["failure"])
        ));
    }
    summary
}
/// Text with the Markdown link, image and HTML punctuation escaped, so a
/// name or message a model chose can't embed a link, an image or a tag in
/// a report. The archive's text reaches a report through tool names, error
/// kinds and failure messages.
fn markdown_safe(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '[' | ']' | '<' | '>') {
            safe.push('\\');
        }
        safe.push(c);
    }
    safe
}
/// A run's report as Markdown: a heading with the run, a line with its
/// state, outcome, recipe, mode and the records counted, then each section
/// as a heading with a line per total.
fn report_text(data: &Value) -> String {
    let run = &data["run"];
    let state = if run["orchestration"]["reason"].is_null() {
        word(&run["orchestration"]["state"])
    } else {
        format!(
            "{} ({})",
            word(&run["orchestration"]["state"]),
            word(&run["orchestration"]["reason"])
        )
    };
    let mut lines = vec![
        format!("# Run {}", word(&run["run_id"])),
        format!(
            "{state} | outcome {} | recipe {} | mode {} | {} records",
            word(&run["outcome"]["reason"]),
            word(&run["recipe_id"]),
            word(&run["mode"]),
            data["records"]
                .as_u64()
                .map_or_else(|| word(&data["records"]), playtime_client::words::thousands)
        ),
    ];
    let sections = data["summary"].as_array().map_or(&[][..], Vec::as_slice);
    if sections.is_empty() {
        lines.push("No model calls, tools or compactions recorded.".into());
    }
    for section in sections {
        lines.push(String::new());
        lines.push(format!("## {}", word(&section["title"])));
        for line in section["lines"].as_array().map_or(&[][..], Vec::as_slice) {
            let text = line["text"].as_str().unwrap_or_default();
            lines.push(format!("- {}", escape(&markdown_safe(text))));
        }
    }
    lines.join("\n")
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
        "error" => {
            let mut lines = vec![format!(
                "{}: {}{}",
                word(&data["code"]),
                word(&data["message"]),
                data.get("recovery")
                    .map(|r| format!(" (recovery: {})", word(r)))
                    .unwrap_or_default()
            )];
            // A refused release: each entry's code, and the Docker resource it
            // names.
            if let Some(remaining) = data["details"]["remaining"].as_array() {
                lines.extend(remaining.iter().map(|item| {
                    let mut line = format!("  - {}", word(item.get("code").unwrap_or(item)));
                    if let (Some(kind), Some(name)) = (item.get("kind"), item.get("name")) {
                        line.push_str(&format!(": {} {}", word(kind), word(name)));
                    }
                    line
                }));
                if let Some(omitted) = data["details"]["omitted"].as_u64() {
                    lines.push(format!("  - {omitted} more, listed in the daemon's log"));
                }
            }
            // Reasons name what to change, such as a missing inference target.
            if let Some(reasons) = data["details"]["reasons"].as_array() {
                lines.extend(reasons.iter().map(|reason| format!("  - {}", word(reason))));
            }
            lines.join("\n")
        }
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
        // A checkpoint has a workspace and files too; the totals are the
        // download's alone.
        "result" if data.get("workspace").is_some() && data.get("not_saved").is_some() => {
            download_text(data)
        }
        "result" if data.get("verified_bytes").is_some() => format!(
            "Verified {} bytes to {} (artifact {})",
            word(&data["verified_bytes"]),
            word(&data["output"]),
            word(&data["artifact"]["artifact_id"])
        ),
        "result" if data.get("summary").is_some() && data.get("records").is_some() => {
            report_text(data)
        }
        _ if data.get("run").is_some() => {
            let mut lines = Vec::new();
            if let Some(launch) = data.get("launch") {
                lines.push(launch_summary(launch));
            }
            lines.push(run_summary(&data["run"]));
            if let Some(code) = data.get("exit_code") {
                lines.push(format!("Observer result: {}", word(code)));
            }
            if let Some(next) = data.get("next") {
                lines.push(format!(
                    "Stopped; needs an operator release: {}",
                    word(next)
                ));
            }
            lines.join("\n")
        }
        _ if data.get("orchestration").is_some() => run_summary(data),
        _ => serde_json::to_string_pretty(data).unwrap(),
    }
}
/// A workspace download: a line with the totals, then one per file.
fn download_text(data: &Value) -> String {
    let files = data["files"].as_array().cloned().unwrap_or_default();
    let source = match data["workspace"]["source"].as_str() {
        Some("final_capture") => "the final capture".to_owned(),
        _ => format!("checkpoint {}", word(&data["workspace"]["session_id"])),
    };
    let mut lines = vec![format!(
        "Saved {} of {} files, {} bytes, to {} from {}",
        word(&data["saved"]),
        files.len(),
        word(&data["saved_bytes"]),
        word(&data["output"]),
        source
    )];
    for file in &files {
        let name = word(&file["name"]);
        lines.push(match file["status"].as_str() {
            Some("saved") => format!("saved  {} bytes  {name}", word(&file["verified_bytes"])),
            Some("exists") => format!("exists (use --force to replace)  {name}"),
            Some("failed") => format!("failed ({})  {name}", word(&file["error"]["message"])),
            Some(status) => format!("{status} ({})  {name}", word(&file["reason"])),
            None => word(file),
        });
    }
    lines.join("\n")
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
    #[test]
    fn error_text_names_the_reasons() {
        let value = envelope(
            "error",
            json!({"code":"execution_unavailable","message":"Execution is unavailable","details":{"reasons":["target_required"]}}),
        );
        assert_eq!(
            text(&value),
            "execution_unavailable: Execution is unavailable\n  - target_required"
        );
    }
    #[test]
    fn a_refused_release_names_what_remains() {
        let value = envelope(
            "error",
            json!({"code":"release_refused","message":"The release was refused; see details",
                "details":{"remaining":[{"code":"docker_unavailable"},
                    {"code":"resource_present","kind":"volume","name":"playtime-x-volume-0"}],"omitted":3}}),
        );
        assert_eq!(
            text(&value),
            "release_refused: The release was refused; see details\n  - docker_unavailable\n  - resource_present: volume playtime-x-volume-0\n  - 3 more, listed in the daemon's log"
        );
    }
    #[test]
    fn a_report_reads_as_markdown() {
        let value = envelope(
            "result",
            json!({"run":{"run_id":"r","orchestration":{"state":"terminal","reason":"wall_limit"},
                "outcome":{"reason":"wall_limit"},"recipe_id":"hour","mode":"experiment"},
                "records":1234,"report":{},"files":null,
                "summary":[{"title":"Time","lines":[{"text":"1:00:01 elapsed · 98% in model requests","first_sequence":null}]},
                    {"title":"Model errors","lines":[{"text":"12 model errors: 11 parse\u{1b}","first_sequence":40}]}]}),
        );
        assert_eq!(
            text(&value),
            "# Run r\nterminal (wall_limit) | outcome wall_limit | recipe hour | mode experiment | 1,234 records\n\n## Time\n- 1:00:01 elapsed · 98% in model requests\n\n## Model errors\n- 12 model errors: 11 parse\\u{1b}"
        );
        let mut empty = value;
        empty["data"]["summary"] = json!([]);
        assert!(
            text(&empty)
                .ends_with("| 1,234 records\nNo model calls, tools or compactions recorded.")
        );
        // A model-chosen path in a failure message can't become an image, a
        // link or a tag when the report is rendered.
        let mut hostile = empty;
        hostile["data"]["summary"] = json!([{"title":"Tools","lines":[
            {"text":"1 × read_file: FileNotFoundError: '![x](http://host/x)' <img src=\"http://host/y\"> \\[y](z)","first_sequence":9}]}]);
        assert!(text(&hostile).ends_with(
            "\n## Tools\n- 1 × read_file: FileNotFoundError: '!\\[x\\](http://host/x)' \\<img src=\"http://host/y\"\\> \\\\\\[y\\](z)"
        ));
    }
    #[test]
    fn a_failed_run_shows_the_workers_diagnostic() {
        let run = json!({"run_id":"r","orchestration":{"state":"reconciling",
            "reason":"execution_failed","failure":"retired failed publication 7..9\u{1b}[31m"}});
        let summary = text(&envelope("run", run.clone()));
        assert!(
            summary.starts_with("Run r: reconciling (execution_failed) | "),
            "{summary}"
        );
        assert!(
            summary.ends_with("\nWorker failure: retired failed publication 7..9\\u{1b}[31m"),
            "{summary}"
        );
        let mut normal = run;
        normal["orchestration"]["failure"] = Value::Null;
        assert!(!text(&envelope("run", normal)).contains("Worker failure"));
    }
    #[test]
    fn target_choices_are_rejections_and_outages_stay_transient() {
        let exit = |reasons: Value| {
            let response = serde_json::from_value(json!({"code":"execution_unavailable",
                "message":"Execution is unavailable","details":{"reasons":reasons}}))
            .unwrap();
            Failure::from(Error::Api {
                status: 503,
                response,
            })
            .exit
        };
        for reason in [
            "target_required",
            "target_unknown",
            "target_requires_live_execution",
        ] {
            assert_eq!(exit(json!([reason])), 5, "{reason}");
        }
        assert_eq!(exit(json!(["model_unavailable"])), 3);
        assert_eq!(exit(json!(["target_unknown", "workshop_unavailable"])), 3);
        assert_eq!(exit(json!([])), 3);
    }
}
