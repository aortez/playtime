use crate::{Error, Failure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REQUEST_BYTES: usize = 262_144;
pub const RESPONSE_BYTES: usize = 524_288;
pub const DIAGNOSTIC_BYTES: usize = 16_384;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "name", content = "arguments", rename_all = "snake_case")]
pub enum Tool {
    Exec {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<f64>,
    },
    ReadFile {
        path: String,
        offset_bytes: u64,
    },
    WriteFile {
        path: String,
        content: String,
        append: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> ToolLimits {
        ToolLimits {
            command_seconds: 1.0,
            output_bytes: 4096,
        }
    }
    #[test]
    fn encoded_bytes_not_characters_control_admission() {
        let request = |content| Tool::WriteFile {
            path: "f".into(),
            content,
            append: false,
        };
        let overhead = request(String::new()).encode(limits()).unwrap().len();
        let remaining = REQUEST_BYTES - overhead;
        let content = "\u{0001}".repeat(remaining / 6) + &"x".repeat(remaining % 6);
        assert_eq!(
            request(content.clone()).encode(limits()).unwrap().len(),
            REQUEST_BYTES
        );
        assert_eq!(
            request(content + "x").encode(limits()).unwrap_err().failure,
            Failure::InvalidInput
        );
        let command = Tool::Exec {
            command: "\u{0001}".repeat(65536),
            cwd: None,
            timeout_seconds: None,
        };
        assert!(command.encode(limits()).is_err());
    }
    #[test]
    fn typed_results_preserve_zero_false_binary_error_and_reject_conflicting_shapes() {
        let tool = Tool::Exec {
            command: String::new(),
            cwd: None,
            timeout_seconds: None,
        };
        assert!(matches!(tool.decode(br#"{"stdout":"","stderr":"","exit_code":0,"timed_out":false,"output_truncated":false,"duration_seconds":0}"#).unwrap(),ToolReply::Exec{exit_code:0,timed_out:false,output_truncated:false,..}));
        assert!(matches!(tool.decode(br#"{"stdout":"","stderr":"","exit_code":-9,"timed_out":true,"output_truncated":true,"duration_seconds":0.1}"#).unwrap(),ToolReply::Exec{timed_out:true,..}));
        assert!(tool.decode(br#"{"stdout":"","stderr":"","exit_code":7,"exit_code":0,"timed_out":false,"output_truncated":false,"duration_seconds":0}"#).is_err());
        for invalid in [b"not JSON".as_slice(),b"\xff",br#"{"error":"x","stdout":"y"}"#,br#"{"stdout":"","stderr":"","exit_code":0,"timed_out":0,"output_truncated":false,"duration_seconds":0}"#,br#"{"stdout":"","stderr":"","exit_code":0,"timed_out":false,"output_truncated":false,"duration_seconds":-1}"#] {assert!(tool.decode(invalid).is_err());}
        let tool = Tool::ReadFile {
            path: "binary".into(),
            offset_bytes: 0,
        };
        assert_eq!(
            tool.decode(br#"{"error":"binary","size_bytes":0}"#)
                .unwrap(),
            ToolReply::Error {
                error: "binary".into(),
                size_bytes: Some(0)
            }
        );
        assert_eq!(
            tool.decode(br#"{"content":"","offset_bytes":0,"truncated":false}"#)
                .unwrap(),
            ToolReply::Read {
                content: String::new(),
                offset_bytes: 0,
                truncated: false
            }
        );
        assert!(
            tool.decode(br#"{"path":"/work/f","bytes_written":0}"#)
                .is_err()
        );
        assert!(tool.decode(&vec![b' '; RESPONSE_BYTES + 1]).is_err());
    }
}
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ToolLimits {
    pub command_seconds: f64,
    pub output_bytes: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolReply {
    Error {
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
    },
    Exec {
        stdout: String,
        stderr: String,
        exit_code: i64,
        timed_out: bool,
        output_truncated: bool,
        duration_seconds: f64,
    },
    Read {
        content: String,
        offset_bytes: u64,
        truncated: bool,
    },
    Write {
        path: String,
        bytes_written: u64,
    },
}
impl Tool {
    pub(crate) fn encode(&self, limits: ToolLimits) -> Result<Vec<u8>> {
        if !limits.command_seconds.is_finite()
            || limits.command_seconds <= 0.0
            || limits.command_seconds > 3600.0
            || limits.output_bytes > 65536
        {
            return Err(Error::new(
                Failure::InvalidInput,
                "tool",
                "invalid bridge limits",
            ));
        }
        let valid = match self {
            Self::Exec {
                command,
                cwd,
                timeout_seconds,
            } => {
                command.len() <= 65536
                    && cwd.as_ref().is_none_or(|s| s.len() <= REQUEST_BYTES)
                    && timeout_seconds.is_none_or(|t| t.is_finite() && t > 0.0)
            }
            Self::WriteFile { path, content, .. } => {
                path.len() <= REQUEST_BYTES && content.len() <= 131072
            }
            Self::ReadFile { path, .. } => path.len() <= REQUEST_BYTES,
        };
        if !valid {
            return Err(Error::new(
                Failure::InvalidInput,
                "tool",
                "invalid bridge argument",
            ));
        }
        let mut value = serde_json::to_value(self)
            .map_err(|_| Error::new(Failure::InvalidInput, "tool", "bridge encoding failed"))?;
        value["limits"] = serde_json::to_value(limits).expect("finite limits");
        let encoded = serde_json::to_vec(&value)
            .map_err(|_| Error::new(Failure::InvalidInput, "tool", "bridge encoding failed"))?;
        if encoded.len() > REQUEST_BYTES {
            return Err(Error::new(
                Failure::InvalidInput,
                "tool",
                "encoded JSON exceeds 262144 bytes",
            ));
        }
        Ok(encoded)
    }
    pub(crate) fn decode(&self, bytes: &[u8]) -> Result<ToolReply> {
        let malformed = || Error::new(Failure::Malformed, "tool", "invalid bridge result");
        if bytes.len() > RESPONSE_BYTES {
            return Err(malformed());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| malformed())?;
        let value: Value = serde_json::from_str(text).map_err(|_| malformed())?;
        let result: ToolReply = serde_json::from_str(text).map_err(|_| malformed())?;
        // Untagged decoding alone would accept conflicting shapes. Require the
        // exact known fields for this operation (including legitimate 0/false).
        let keys: &[&str] = match (&result, self) {
            (ToolReply::Error { size_bytes, .. }, _) => {
                if size_bytes.is_some() {
                    &["error", "size_bytes"]
                } else {
                    &["error"]
                }
            }
            (
                ToolReply::Exec {
                    duration_seconds, ..
                },
                Self::Exec { .. },
            ) if duration_seconds.is_finite() && *duration_seconds >= 0.0 => &[
                "stdout",
                "stderr",
                "exit_code",
                "timed_out",
                "output_truncated",
                "duration_seconds",
            ],
            (ToolReply::Read { .. }, Self::ReadFile { .. }) => {
                &["content", "offset_bytes", "truncated"]
            }
            (ToolReply::Write { .. }, Self::WriteFile { .. }) => &["path", "bytes_written"],
            _ => return Err(malformed()),
        };
        let object = value.as_object().ok_or_else(malformed)?;
        if object.len() != keys.len() || keys.iter().any(|k| !object.contains_key(*k)) {
            return Err(malformed());
        }
        Ok(result)
    }
}
