//! Session-neutral archive presentation. Explicit identities group calls and
//! tools; compaction evidence remains separate from the summary model response.
use super::{Projection, Role, Section, excerpt, json, progress, words};
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
                    other => words(&other),
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
                    section(
                        p,
                        "Context estimate",
                        format!(
                            "{} projected tokens · basis {}",
                            v["projected_tokens"],
                            v["estimate_basis"].as_str().unwrap_or("unavailable")
                        ),
                    );
                    section(
                        p,
                        "Conversation selection",
                        format!(
                            "{} older exchanges · {} recent exchanges retained. Inspect source for exact before-conversation and request links.",
                            v["source_requests"].as_array().map_or(0, Vec::len),
                            v["retained_requests"].as_array().map_or(0, Vec::len)
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
                "compaction_rejected" | "tool_batch_rejected" => {
                    text(p, v, "reason", "Rejected");
                    p.status = "Rejected · inspect source for details".into();
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
                    p.role = Role::Bookkeeping;
                }
                "controller_started"
                | "compaction_configured"
                | "phase_changed"
                | "workspace_started"
                | "workspace_captured" => p.role = Role::Bookkeeping,
                _ => (),
            }
            if p.readable.is_empty() && !grouped {
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
                        playtime_protocol::events::CleanupOutcome::Failed { errors } =>
                            format!("failed ({} errors)", errors.len()),
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
