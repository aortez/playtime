//! Session-neutral archive presentation. Explicit identities group calls and
//! tools; compaction evidence remains separate from the summary model response.
use super::{Projection, Role, Section, excerpt, json, progress, thousands, words};
use playtime_core::RequestPurpose;
use playtime_protocol::{execution::Fact, observations::ObservationData};
use serde_json::Value;

fn section(p: &mut Projection, label: &str, text: impl Into<String>) {
    p.readable.push(Section {
        label: label.into(),
        text: text.into(),
    });
}
fn text(p: &mut Projection, value: &Value, field: &str, label: &str) {
    if let Some(text) = value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        section(p, label, text);
    }
}
/// A recorded token estimate and the basis the policy used for it.
fn estimate(value: &Value, tokens: &str, basis: &str, what: &str) -> String {
    match value[tokens].as_u64() {
        Some(n) => format!("{} {what}, from {}", thousands(n), words(&value[basis])),
        None => "unavailable".into(),
    }
}
pub(super) fn project(fact: &Fact, p: &mut Projection) {
    match fact {
        Fact::Observation(o) => {
            p.title = words(&o.observation_kind);
            p.progress = progress::Update::execution(o);
            let grouped = o.observation_kind.starts_with("model_")
                || o.observation_kind.starts_with("request_")
                || o.observation_kind.starts_with("tool_");
            if grouped && let Some(request) = &o.request {
                let family = format!("run:{}", request.request_id);
                p.display.family = Some(family.clone());
                p.display.call = Some(request.call.get());
                p.display.purpose = Some(match request.purpose {
                    RequestPurpose::Compaction => "Compaction summary".into(),
                    other => progress::capitalized(&words(&other)),
                });
                p.display.tool = o.identity.tool_request_sequence.is_some();
                p.key = o
                    .identity
                    .tool_request_sequence
                    .map_or_else(|| format!("model:{family}"), |id| format!("tool:run:{id}"));
            }
            let ObservationData::Inline { value: v } = &o.data else {
                p.status = "Retained payload · inspect verified source".into();
                section(
                    p,
                    "Recorded data",
                    "Payload retained separately; open source to load verified bytes.",
                );
                return;
            };
            p.parts = vec![Section {
                label: o.observation_kind.clone(),
                text: json(v),
            }];
            match o.observation_kind.as_str() {
                "model_request" => {
                    p.display.state = Some("Awaiting model response".into());
                }
                "model_chunk" | "model_response" => {
                    let final_response = o.observation_kind == "model_response";
                    let response = &v[if final_response { "response" } else { "chunk" }];
                    p.final_response = final_response;
                    p.display.state = Some(
                        if !final_response {
                            "Streaming · incomplete"
                        } else if response["done"] == true {
                            "Response recorded"
                        } else {
                            "Partial response recorded"
                        }
                        .into(),
                    );
                    text(p, &response["message"], "thinking", "Recorded thinking");
                    text(p, &response["message"], "content", "Model response");
                }
                // A normal stop, or no recorded reason, adds nothing. Any other
                // reason replaces the response state; summaries and finish_run
                // require a normal stop.
                "model_completion" => match v["finish_reason"].as_str() {
                    None | Some("stop") => (),
                    Some("length") => {
                        p.display.state = Some("Stopped at its token limit".into());
                        section(
                            p,
                            "Completion",
                            "The response reached its token limit before it finished (finish reason: length).",
                        );
                    }
                    Some(other) => {
                        p.display.state = Some(excerpt(&format!("Finish reason: {other}"), 120));
                        section(
                            p,
                            "Completion",
                            excerpt(&format!("Finish reason: {other}"), 512),
                        );
                    }
                },
                "model_error" => {
                    p.display.state = Some("Model error · partial inference may be present".into());
                    text(p, &v["error"], "message", "Error");
                }
                "tool_request" => {
                    let name = v["name"].as_str().unwrap_or("unknown");
                    let args = &v["arguments"];
                    p.display.title = Some(excerpt(&format!("Tool · {name}"), 240));
                    p.display.state = Some("Awaiting recorded result".into());
                    p.display.pending = true;
                    for (field, label) in [
                        ("command", "Command"),
                        ("path", "Path"),
                        ("content", "Content requested for writing"),
                        ("summary", "Declared completion"),
                    ] {
                        text(p, args, field, label);
                    }
                }
                "tool_result" => {
                    p.display.state = Some(super::presentation::result_state(
                        v["name"].as_str().unwrap_or("unknown"),
                        &v["result"],
                    ));
                    for (field, label) in [
                        ("stdout", "Standard output"),
                        ("stderr", "Standard error"),
                        ("content", "Content read"),
                        ("error", "Error"),
                    ] {
                        text(p, &v["result"], field, label);
                    }
                    if ["truncated", "output_truncated"]
                        .iter()
                        .any(|k| v["result"][k] == true)
                    {
                        section(p, "Output limit", "Recorded output was truncated");
                    }
                }
                "tool_not_dispatched" | "request_not_dispatched" => {
                    p.display.state = Some("Not dispatched".into())
                }
                "compaction_planned" => {
                    p.title = format!("Compaction {} · planned", v["ordinal"]);
                    // Either request can start a compaction, so show both.
                    section(
                        p,
                        "Context estimate",
                        format!(
                            "Work request: {}\nSummary request: {}",
                            estimate(v, "projected_tokens", "estimate_basis", "tokens projected"),
                            estimate(
                                v,
                                "summary_input_bound",
                                "summary_estimate_basis",
                                "input tokens"
                            )
                        ),
                    );
                    let exchanges = |field: &str| v[field].as_array().map_or(0, Vec::len) as u64;
                    section(
                        p,
                        "Conversation selection",
                        format!(
                            "{} · {} retained. Inspect source for exact before-conversation and request links.",
                            progress::count(
                                exchanges("source_requests"),
                                "older exchange",
                                "older exchanges"
                            ),
                            progress::count(
                                exchanges("retained_requests"),
                                "recent exchange",
                                "recent exchanges"
                            )
                        ),
                    );
                }
                "compaction_deferred" => {
                    p.title = format!("Compaction {} · deferred", v["ordinal"]);
                    let seconds = |field: &str| match v[field].as_u64() {
                        Some(ms) => format!("{} s", thousands(ms.div_ceil(1000))),
                        None => "unavailable".into(),
                    };
                    section(
                        p,
                        "Time",
                        format!(
                            "{} left; at the run's measured pace a summary was expected to take {}, so work continues past the trigger.",
                            seconds("remaining_ms"),
                            seconds("expected_summary_ms")
                        ),
                    );
                    section(
                        p,
                        "Context estimate",
                        format!(
                            "Work request: {}\nSummary request: {}",
                            estimate(v, "projected_tokens", "estimate_basis", "tokens projected"),
                            estimate(
                                v,
                                "summary_input_bound",
                                "summary_estimate_basis",
                                "input tokens"
                            )
                        ),
                    );
                }
                "compaction_applied" => {
                    p.title = format!("Compaction {} · applied", v["ordinal"]);
                    text(p, v, "summary", "Applied summary");
                    section(
                        p,
                        "Provenance",
                        "Derived summary; exact resulting conversation and source request links are retained in source.",
                    );
                }
                // Both rejections stop the run. A compaction rejected before its
                // plan records the previous ordinal, so the title leaves it out.
                "compaction_rejected" => {
                    p.title = "Compaction rejected".into();
                    p.status = "Rejected · the run stops".into();
                    section(p, "Reason", progress::capitalized(&words(&v["reason"])));
                    text(p, v, "detail", "Detail");
                }
                "tool_batch_rejected" => {
                    p.display.state = Some("Tool calls rejected · the run stops".into());
                    section(p, "Rejected", progress::capitalized(&words(&v["reason"])));
                    text(p, v, "error", "Error");
                }
                "controller_stop" => {
                    p.role = Role::Marker {
                        text: format!("Controller stopped: {}", words(&v["reason"])),
                        checkpoint: None,
                    };
                }
                "completion_declared" => {
                    p.title = "Model declared completion".into();
                    text(p, v, "summary", "Completion claim");
                    section(
                        p,
                        "Outcome",
                        "A model claim, not verified task success. The terminal event records why the controller stopped.",
                    );
                }
                "controller_finished" => {
                    p.finished = true;
                    let diagnostics: Vec<&str> = v["diagnostics"]
                        .as_array()
                        .map(|d| d.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if diagnostics.is_empty() {
                        p.role = Role::Bookkeeping;
                    } else {
                        p.title = "Controller diagnostics".into();
                        section(p, "Diagnostics", diagnostics.join("\n"));
                    }
                }
                "controller_started"
                | "compaction_configured"
                | "phase_changed"
                | "workspace_started"
                | "workspace_captured" => p.role = Role::Bookkeeping,
                _ => (),
            }
            if p.readable.is_empty() && !grouped && p.role == Role::Activity {
                section(p, "Recorded data", "Inspect source for recorded details.");
            }
        }
        Fact::RunFinished {
            reason, cleanup, ..
        } => {
            p.finished = true;
            p.role = Role::Marker {
                text: format!(
                    "Run finished: {} · cleanup {}",
                    words(reason),
                    match cleanup {
                        playtime_protocol::events::CleanupOutcome::Complete => "complete".into(),
                        playtime_protocol::events::CleanupOutcome::Failed { errors } => format!(
                            "failed ({})",
                            progress::count(errors.len() as u64, "error", "errors")
                        ),
                        playtime_protocol::events::CleanupOutcome::Unknown => "unknown".into(),
                    }
                ),
                checkpoint: None,
            };
        }
        Fact::CapturePublished { .. } => {
            p.role = Role::Marker {
                text: "Final stopped workspace captured · open Files to inspect".into(),
                checkpoint: None,
            }
        }
        Fact::WorkspaceStarted { .. } => {
            p.role = Role::Marker {
                text: "Continuous workshop started".into(),
                checkpoint: None,
            }
        }
        Fact::ObservationOmitted {
            observation_kind,
            reason,
            ..
        } => {
            p.title = format!("{} · payload omitted", words(observation_kind));
            section(p, "Unavailable evidence", excerpt(reason, 512));
        }
        _ => p.role = Role::Bookkeeping,
    }
}
