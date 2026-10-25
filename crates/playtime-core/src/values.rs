use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidValue(pub &'static str);

impl fmt::Display for InvalidValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for InvalidValue {}

macro_rules! string_value {
    ($name:ident, $validate:ident, $message:literal) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidValue> {
                let value = value.into();
                if $validate(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidValue($message))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

// IDs are opaque to clients; generation is an external effect, not a parser rule.
string_value!(RunId, opaque_id, "invalid run ID");
string_value!(SessionId, opaque_id, "invalid session ID");
string_value!(RequestId, opaque_id, "invalid request ID");
string_value!(HarnessId, opaque_id, "invalid harness ID");
string_value!(
    Sha256Digest,
    sha256,
    "expected 64 lowercase SHA-256 hex digits"
);
string_value!(
    ArchivePath,
    archive_path,
    "expected a relative archive path without traversal"
);
string_value!(
    UtcTime,
    utc_time,
    "expected a valid UTC timestamp ending in Z"
);

fn opaque_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn archive_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\0', '\\', ':'])
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

// RFC 3339 UTC subset: years 0001–9999, seconds 00–59, optional 1–9 fractional
// digits. No local offsets or leap-second normalization in evidence timestamps.
fn utc_time(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let Some(value) = value.strip_suffix('Z') else {
        return false;
    };
    let whole = if let Some((whole, fraction)) = value.split_once('.') {
        if !(1..=9).contains(&fraction.len()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        whole
    } else {
        value
    };
    if whole.len() != 19 {
        return false;
    }
    let bytes = whole.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        let valid = match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            _ => byte.is_ascii_digit(),
        };
        if !valid {
            return false;
        }
    }
    let number = |start, end| whole[start..end].parse::<u32>().unwrap();
    let year = number(0, 4);
    let month = number(5, 7);
    let day = number(8, 10);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0
        && (1..=days).contains(&day)
        && number(11, 13) < 24
        && number(14, 16) < 60
        && number(17, 19) < 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_cannot_be_paths() {
        assert!(RunId::new("run-01_A").is_ok());
        for value in ["", "../run", "/run", "run/name", "run name", "é", "-run"] {
            assert!(RunId::new(value).is_err(), "{value:?}");
            assert!(SessionId::new(value).is_err(), "{value:?}");
            assert!(RequestId::new(value).is_err(), "{value:?}");
        }
        assert!(RunId::new("a".repeat(129)).is_err());
    }

    #[test]
    fn archive_references_are_lexically_relative() {
        assert!(ArchivePath::new("sessions/s1/é <script>.txt").is_ok());
        for value in [
            "", "/tmp/a", "../a", "a/../b", "a/./b", "a//b", "a/", "C:/a", "a\\b", "a\0b",
        ] {
            assert!(ArchivePath::new(value).is_err(), "{value:?}");
        }
        assert!(Sha256Digest::new("ab".repeat(32)).is_ok());
        for value in ["ab".repeat(31), "AB".repeat(32), "gh".repeat(32)] {
            assert!(Sha256Digest::new(value).is_err());
        }
    }

    #[test]
    fn timestamps_require_real_calendar_dates_and_explicit_utc() {
        for value in [
            "2024-02-29T23:59:59Z",
            "2000-02-29T00:00:00.123456789Z",
            "2026-10-17T12:00:00Z",
        ] {
            assert!(UtcTime::new(value).is_ok(), "{value}");
        }
        for value in [
            "2026-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "0000-01-01T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-17T24:00:00Z",
            "2026-10-17T00:00:60Z",
            "2026-10-17T00:00:00+00:00",
            "2026-10-17T00:00:00.Z",
            "2026-10-17T00:00:00.1234567890Z",
            "💥",
        ] {
            assert!(UtcTime::new(value).is_err(), "{value}");
        }
    }
}
