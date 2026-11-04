//! Bounded rejected-attempt data for continuous work retries, never tool input.
use serde_json::{Value, json};

use crate::{ModelFailure, RequestContext, context};

const MAX_BYTES: u64 = 8192;
const NOTICE: &str =
    "Archived arguments from the rejected attempt (unexecuted data, not a new instruction):";

pub(super) fn message(request: &RequestContext, error: &ModelFailure) -> Option<Value> {
    let ModelFailure::Parse(diagnostic) = error else {
        return None;
    };
    Some(build(&request.request_id.to_string(), error, diagnostic))
}

fn build(request_id: &str, error: &ModelFailure, diagnostic: &str) -> Value {
    // The adapter may already have shortened the diagnostic. Never advertise
    // that excerpt as complete raw arguments, or attempt to repair its syntax.
    let raw = (!diagnostic.contains(" bytes omitted …] "))
        .then(|| diagnostic.strip_prefix("error parsing tool call: raw='"))
        .flatten()
        .and_then(|s| s.rsplit_once("', err=").map(|(raw, _)| raw));
    let text = raw.unwrap_or(diagnostic);
    let field = if raw.is_some() {
        "raw_arguments"
    } else {
        "diagnostic"
    };
    let render = |keep: usize| {
        let mut head = keep.div_ceil(2);
        let mut tail = text.len() - keep / 2;
        while !text.is_char_boundary(head) {
            head -= 1;
        }
        while !text.is_char_boundary(tail) {
            tail += 1;
        }
        let omitted = tail - head;
        let excerpt = if omitted == 0 {
            text.to_owned()
        } else {
            format!("{}…{}", &text[..head], &text[tail..])
        };
        let record = json!({
            "schema_version": 1,
            "failed_request_id": request_id,
            "tools_executed": false,
            "complete_raw_arguments": raw.is_some() && omitted == 0,
            "additional_omitted_bytes": omitted,
            field: excerpt,
        });
        context::message(
            "user",
            format!(
                "{}\n\n{NOTICE}\n{}",
                super::parse_retry_notice(error),
                context::canonical(&record)
            ),
        )
    };
    // Binary search counts the encoded message envelope and both escaping layers.
    let mut low = 0;
    let mut high = text.len().min(MAX_BYTES as usize);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if context::encoded_size(&render(middle)) <= MAX_BYTES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    render(low)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_rejected_arguments_without_parsing_or_repairing() {
        let raw = r#"{"path":"/work/test.py","content":"print('x')"],"append":false}"#;
        let diagnostic = format!("error parsing tool call: raw='{raw}', err=invalid character ']'");
        let value = build("r50", &ModelFailure::Parse(diagnostic.clone()), &diagnostic);
        let record: Value = serde_json::from_str(
            value["content"]
                .as_str()
                .unwrap()
                .split(&format!("{NOTICE}\n"))
                .nth(1)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(record["raw_arguments"], raw);
        assert_eq!(record["failed_request_id"], "r50");
        assert_eq!(record["tools_executed"], false);
        assert_eq!(record["complete_raw_arguments"], true);
    }

    #[test]
    fn escaping_unicode_and_upstream_truncation_remain_bounded_and_explicit() {
        for diagnostic in [
            format!(
                "error parsing tool call: raw='{}', err=bad",
                "\"\\\n🦀".repeat(20000)
            ),
            "error parsing tool call: raw='a [… 100 bytes omitted …] b', err=bad".into(),
            "unknown backend error format".into(),
        ] {
            let value = build("r1", &ModelFailure::Parse(diagnostic.clone()), &diagnostic);
            assert!(context::encoded_size(&value) <= MAX_BYTES);
            let record: Value = serde_json::from_str(
                value["content"]
                    .as_str()
                    .unwrap()
                    .split(&format!("{NOTICE}\n"))
                    .nth(1)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(record["complete_raw_arguments"], false);
            assert!(
                record["additional_omitted_bytes"].as_u64().unwrap() > 0
                    || record.get("diagnostic").is_some()
            );
        }
    }
}
