//! Read-only presentation of frozen previews. JSON interpretation stays off the
//! render path; absent fields never become guessed settings or identities.
use crate::{
    model::{PreviewSection, PreviewView},
    progress,
    transcript::{self, excerpt, thousands},
};
use playtime_protocol::api::Preview;
use serde_json::Value;

// Reserve room for all three text excerpts and the bounded summary sections,
// so opening Details never hides bytes from a source page before advancing it.
pub(crate) const SOURCE_BYTES: usize = 24 << 10;
const EXCERPT_BYTES: usize = (4 << 10) - 4;
pub(crate) fn source_page(text: &str, offset: usize) -> transcript::SourcePage {
    let mut page = transcript::source_page(text, offset);
    let mut end = SOURCE_BYTES.min(page.text.len());
    while !page.text.is_char_boundary(end) {
        end -= 1;
    }
    page.text.truncate(end);
    page
}

fn field(value: &Value, pointer: &str) -> String {
    match value.pointer(pointer) {
        Some(Value::String(text)) => excerpt(text, 256),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(value)) => if *value { "yes" } else { "no" }.into(),
        _ => "unavailable".into(),
    }
}
fn count(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .map(thousands)
        .unwrap_or_else(|| "unavailable".into())
}
fn short(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(|s| excerpt(s, 19))
        .unwrap_or_else(|| "unavailable".into())
}
pub(crate) fn bytes(bytes: u64) -> String {
    for (unit, divisor) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if bytes >= divisor {
            return if bytes.is_multiple_of(divisor) {
                format!("{} {unit}", thousands(bytes / divisor))
            } else {
                format!("{:.1} {unit}", bytes as f64 / divisor as f64)
            };
        }
    }
    format!("{} {}", bytes, if bytes == 1 { "byte" } else { "bytes" })
}
fn duration(seconds: u64) -> String {
    if seconds.is_multiple_of(60) {
        format!("{} min", thousands(seconds / 60))
    } else {
        format!("{} s", thousands(seconds))
    }
}
fn section(title: &'static str, lines: Vec<String>) -> PreviewSection {
    PreviewSection { title, lines }
}

pub fn prepare(value: Preview) -> PreviewView {
    let l = &value.parameters.limits;
    let w = &value.parameters.workshop;
    let config = &value.effective_configuration;
    let model = &value.model_identity;
    let workshop = &value.workshop_identity;
    let retries = u64::from(l.tool_parse_retries);
    let parse_limit = l
        .tool_parse_error_limit
        .map_or(retries + 1, |n| u64::from(n.get()));
    let mut sections = vec![section(
        "Run limits",
        vec![
            format!(
                "{} sessions · {} wall time · {} generated tokens",
                thousands(l.sessions.get().into()),
                duration(l.wall_seconds.get()),
                thousands(l.generated_tokens.get())
            ),
            format!(
                "{} work turns per session · {}",
                thousands(l.work_turns_per_session.get().into()),
                progress::memory_words(value.parameters.memory_condition)
            ),
            format!(
                "Model timeout: {} · Command timeout: {}",
                duration(l.model_seconds.get()),
                duration(l.command_seconds.get())
            ),
            format!(
                "Tool parse retries: {} consecutive · Per-session error limit: {}",
                thousands(retries),
                thousands(parse_limit)
            ),
            format!(
                "Command output: {} · Memento: {} · Event log: {}",
                bytes(l.output_bytes.get()),
                bytes(l.memento_bytes.get()),
                bytes(l.log_bytes.get())
            ),
            format!(
                "Command return allowance: {}",
                config
                    .get("command_return_ms")
                    .and_then(Value::as_u64)
                    .map(|n| format!("{} ms", thousands(n)))
                    .unwrap_or_else(|| "unavailable".into())
            ),
        ],
    )];
    let adapter = model
        .get("adapter")
        .or_else(|| model.get("kind"))
        .and_then(Value::as_str);
    let mut model_lines = match adapter {
        Some("fixture") => vec!["Fixture model · no inference".into()],
        Some("scripted") => vec!["Scripted model responses · no inference".into()],
        _ => vec![
            format!(
                "Model: {} · Digest: {}",
                field(model, "/name"),
                short(model, "/digest")
            ),
            format!("Ollama version: {}", field(model, "/ollama_version")),
            format!("Host: {}", field(model, "/endpoint")),
            format!(
                "Resolved addresses: {}",
                model
                    .get("addresses")
                    .and_then(Value::as_array)
                    .filter(|a| !a.is_empty())
                    .map(|a| a
                        .iter()
                        .take(16)
                        .map(|v| field(v, ""))
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_else(|| "unavailable".into())
            ),
        ],
    };
    if model.get("registration").is_some() {
        model_lines.push(format!(
            "Registration: {}",
            field(model, "/registration/registration_id")
        ));
        model_lines.push(format!(
            "Capability evidence: {}",
            field(model, "/registration/identity/capability_provenance")
        ));
        model_lines.push(format!(
            "Definition fingerprint: {}",
            short(model, "/definition_sha256")
        ));
    }
    model_lines.push(format!(
        "Context capacity: {} tokens · Response cap: {} · Handoff cap: {}",
        thousands(l.context_tokens.get()),
        thousands(l.tokens_per_call.get()),
        thousands(l.handoff_tokens.get())
    ));
    if !matches!(adapter, Some("fixture" | "scripted")) {
        model_lines.push(format!(
            "Thinking: {}",
            match model.pointer("/settings/thinking") {
                Some(Value::Null) => "model default (setting omitted)".into(),
                Some(Value::Bool(true)) => "On".into(),
                Some(Value::Bool(false)) => "Off".into(),
                _ => field(model, "/settings/thinking"),
            }
        ));
        model_lines.push(format!(
            "Temperature: {} · Top-p: {} · Top-k: {} · Repeat penalty: {}",
            field(model, "/settings/temperature"),
            field(model, "/settings/top_p"),
            field(model, "/settings/top_k"),
            field(model, "/settings/repeat_penalty")
        ));
    }
    model_lines.push(format!(
        "Sampling seed: {}",
        thousands(value.parameters.sampling_seed.into())
    ));
    sections.push(section("Model", model_lines));
    let mut workshop_lines = match workshop
        .get("adapter")
        .or_else(|| workshop.get("kind"))
        .and_then(Value::as_str)
    {
        Some("fixture") => vec!["Fixture workshop · no tool execution".into()],
        Some("scripted") => vec!["Scripted workshop · no tool execution".into()],
        _ => vec![
            format!("Image: {}", short(workshop, "/image/image_id")),
            format!(
                "Docker host: {} · Server: {}",
                field(workshop, "/daemon/name"),
                field(workshop, "/daemon/server_version")
            ),
        ],
    };
    workshop_lines.extend([
        format!(
            "CPU: {} cores · RAM: {} · Processes: {}",
            w.cpu_cores,
            bytes(w.memory_bytes.get()),
            thousands(w.pids.get().into())
        ),
        format!(
            "Workspace: {} · /tmp: {}",
            bytes(w.workspace_bytes.get()),
            bytes(w.tmp_bytes.get())
        ),
        format!(
            "Workspace entries: {}",
            if config.pointer("/resources/workspace_inodes").is_some() {
                count(config, "/resources/workspace_inodes")
            } else {
                count(config, "/recipe/workspace_entries")
            }
        ),
    ]);
    sections.push(section("Workshop", workshop_lines));
    let mut seed_lines = vec![format!(
        "{} files · {} total",
        thousands(value.seeds.files),
        bytes(value.seeds.bytes)
    )];
    if let Some(seeds) = config.pointer("/recipe/seeds").and_then(Value::as_object) {
        for (name, content) in seeds.iter().take(64) {
            seed_lines.push(format!(
                "{} · {}",
                excerpt(name, 256),
                content
                    .as_str()
                    .map(|s| bytes(s.len() as u64))
                    .unwrap_or_else(|| "size unavailable".into())
            ));
        }
        if seeds.len() > 64 {
            seed_lines.push("More seed files in the full frozen preview.".into());
        }
        if seeds.len() as u64 != value.seeds.files {
            seed_lines.push("Named seed metadata differs from the aggregate summary; inspect the full frozen preview.".into());
        }
    } else if value.seeds.files > 0 {
        seed_lines.push("Seed names and individual sizes unavailable in this preview.".into());
    } else {
        seed_lines.push("No seed files.".into());
    }
    sections.push(section("Seed files", seed_lines));
    let inventory = config
        .pointer("/scenario/inventory")
        .or_else(|| config.get("inventory"))
        .and_then(Value::as_str)
        .map(|s| {
            if s.is_empty() {
                "Empty inventory text.".into()
            } else {
                excerpt(s, EXCERPT_BYTES)
            }
        })
        .unwrap_or_else(|| "Inventory text unavailable in this preview.".into());
    let revision = excerpt(value.preview_revision.as_str(), 12);
    let source = source_page(&serde_json::to_string_pretty(&value).unwrap(), 0);
    let model_name = value
        .model_identity
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let prompt = excerpt(&value.prompt, EXCERPT_BYTES);
    let protocol = excerpt(&value.protocol, EXCERPT_BYTES);
    PreviewView {
        value,
        sections,
        prompt,
        protocol,
        inventory,
        model_name,
        revision,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_pages_reassemble_long_unicode_without_skipping_bytes() {
        let source = "🦀 frozen source\n".repeat(10000);
        let mut offset = 0;
        let mut restored = String::new();
        while offset < source.len() {
            let page = source_page(&source, offset);
            assert_eq!(page.start, offset);
            assert_eq!(page.total, source.len());
            assert!(page.text.len() <= SOURCE_BYTES);
            assert!(!page.text.is_empty());
            offset += page.text.len();
            restored.push_str(&page.text);
        }
        assert_eq!(restored, source);
    }
}
