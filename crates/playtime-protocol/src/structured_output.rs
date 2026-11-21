//! Bounded, offline JSON Schema profile for caller-selected structured results.
use serde_json::Value;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const MAX_CONTRACT: usize = 32 * 1024;
pub const MAX_RESPONSE: usize = 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn check_digest(bytes: &[u8], expected: &str) -> Result<()> {
    if digest(bytes) != expected {
        return Err("input SHA-256 differs from frozen invocation".into());
    }
    Ok(())
}

/// An intentionally closed, nonrecursive JSON Schema profile. Reject unsupported
/// keywords before compiling: a misspelled constraint must never be ignored, and
/// schema admission must never fetch a URL or read a referenced local file.
pub struct Contract {
    bytes: Vec<u8>,
    schema: Value,
    validator: jsonschema::Validator,
}
impl Contract {
    pub fn parse(bytes: Vec<u8>, expected: &str) -> Result<Self> {
        check_digest(&bytes, expected)?;
        if bytes.len() > MAX_CONTRACT {
            return Err("output contract exceeds 32 KiB".into());
        }
        let schema = crate::strict_json::unique_json(&bytes)?;
        if schema["type"] != "object" {
            return Err("output contract root must have type object".into());
        }
        check_schema(&schema, 0, &mut 0)?;
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .offline()
            .build(&schema)?;
        Ok(Self {
            bytes,
            schema,
            validator,
        })
    }
    pub fn schema(&self) -> &Value {
        &self.schema
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> String {
        digest(&self.bytes)
    }
    pub fn validate(&self, bytes: &[u8]) -> Result<Value> {
        if bytes.len() > MAX_RESPONSE {
            return Err("model response exceeds 1 MiB".into());
        }
        let value = crate::strict_json::unique_json(bytes)?;
        if !self.validator.is_valid(&value) {
            return Err("model response does not match trusted output contract".into());
        }
        Ok(value)
    }
}

fn check_schema(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > 16 || *nodes > 512 {
        return Err("output contract exceeds schema depth/node limit".into());
    }
    let object = value.as_object().ok_or("schema nodes must be objects")?;
    let types = match value.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(types)) => types
            .iter()
            .map(|t| t.as_str().ok_or("invalid schema type"))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        _ => return Err("every schema node needs an explicit type".into()),
    };
    if types.is_empty()
        || types.iter().any(|t| {
            ![
                "object", "array", "string", "integer", "number", "boolean", "null",
            ]
            .contains(t)
        })
    {
        return Err("unsupported schema type".into());
    }
    for (key, item) in object {
        match key.as_str() {
            "type" | "enum" | "description" | "title" | "required" | "minLength" | "maxLength"
            | "minItems" | "maxItems" | "minimum" | "maximum" | "exclusiveMinimum"
            | "exclusiveMaximum" => (),
            "additionalProperties" if item == false => (),
            "properties" => {
                for child in item
                    .as_object()
                    .ok_or("properties must be an object")?
                    .values()
                {
                    check_schema(child, depth + 1, nodes)?;
                }
            }
            "items" => check_schema(item, depth + 1, nodes)?,
            _ => return Err(format!("unsupported output contract keyword: {key}").into()),
        }
    }
    if types.contains(&"object")
        && (value.get("additionalProperties") != Some(&Value::Bool(false))
            || !value["properties"].is_object())
    {
        return Err("object schemas require properties and additionalProperties:false".into());
    }
    if types.contains(&"array") && !value["items"].is_object() {
        return Err("array schemas require items".into());
    }
    Ok(())
}
