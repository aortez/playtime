//! Strict framing JSON plus the legacy Python canonical identity encoding.
use crate::{Failure, FailureKind, Result};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn parse(bytes: &[u8], limit: usize, identity: bool) -> Result<Value> {
    if bytes.len() > limit {
        return Err(Failure::new(
            FailureKind::ByteLimit,
            "JSON byte bound exceeded",
        ));
    }
    let invalid = |e: serde_json::Error| Failure::new(FailureKind::Protocol, e.to_string());
    // Bound depth before serde allocates nested collections. Also preserve the
    // integer/float distinction: identity integers outside i64/u64 are refused.
    let (mut depth, mut quoted, mut escaped, mut i) = (0_u32, false, false, 0);
    let mut negative_integer_zeros = vec![];
    while i < bytes.len() {
        let b = bytes[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > 64 {
                    return Err(Failure::new(
                        FailureKind::Protocol,
                        "JSON nesting exceeds 64",
                    ));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                let token = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| Failure::new(FailureKind::Protocol, "invalid numeric UTF-8"))?;
                if token == "-0" {
                    negative_integer_zeros.push(start);
                }
                if identity && !token.contains(['.', 'e', 'E']) {
                    if token.parse::<i64>().is_err() && token.parse::<u64>().is_err() {
                        return Err(Failure::new(
                            FailureKind::Identity,
                            "identity integer outside supported i64/u64 range",
                        ));
                    }
                } else if identity {
                    let number = token.parse::<f64>().map_err(|_| {
                        Failure::new(FailureKind::Identity, "invalid identity float")
                    })?;
                    if !number.is_finite()
                        || (number == 0.0
                            && token
                                .split(['e', 'E'])
                                .next()
                                .unwrap()
                                .bytes()
                                .any(|b| matches!(b, b'1'..=b'9')))
                    {
                        return Err(Failure::new(
                            FailureKind::Identity,
                            "identity float overflow/underflow",
                        ));
                    }
                }
                continue;
            }
            _ => (),
        }
        i += 1;
    }
    // serde_json represents lexical -0 as a float; Python treats this JSON
    // integer as zero. Preserve raw observations, normalize only the parse copy.
    let mut normalized;
    let bytes = if negative_integer_zeros.is_empty() {
        bytes
    } else {
        normalized = bytes.to_vec();
        for index in negative_integer_zeros {
            normalized[index] = b' ';
        }
        &normalized
    };
    serde_json::from_slice::<Unique>(bytes)
        .map(|v| v.0)
        .map_err(invalid)
}
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("strict JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut result = vec![];
                while let Some(Unique(v)) = seq.next_element()? {
                    result.push(v);
                }
                Ok(Unique(Value::Array(result)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut result = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if result.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate object key"));
                    }
                    let Unique(v) = map.next_value()?;
                    result.insert(key, v);
                }
                Ok(Unique(Value::Object(result)))
            }
        }
        d.deserialize_any(V)
    }
}

pub fn canonical(value: &Value, limit: usize) -> Result<Vec<u8>> {
    let mut writer = Canonical {
        bytes: Vec::new(),
        limit,
    };
    writer.value(value, 0)?;
    Ok(writer.bytes)
}
struct Canonical {
    bytes: Vec<u8>,
    limit: usize,
}
impl Canonical {
    fn add(&mut self, s: &str) -> Result<()> {
        if self
            .bytes
            .len()
            .checked_add(s.len())
            .is_none_or(|n| n > self.limit)
        {
            return Err(Failure::new(
                FailureKind::ByteLimit,
                "canonical JSON bound exceeded",
            ));
        }
        self.bytes.extend_from_slice(s.as_bytes());
        Ok(())
    }
    fn string(&mut self, s: &str) -> Result<()> {
        self.add("\"")?;
        for c in s.chars() {
            match c {
                '"' => self.add("\\\"")?,
                '\\' => self.add("\\\\")?,
                '\n' => self.add("\\n")?,
                '\r' => self.add("\\r")?,
                '\t' => self.add("\\t")?,
                '\u{08}' => self.add("\\b")?,
                '\u{0c}' => self.add("\\f")?,
                c if (' '..'\u{7f}').contains(&c) => self.add(&c.to_string())?,
                c => {
                    for word in c.encode_utf16(&mut [0; 2]) {
                        self.add(&format!("\\u{word:04x}"))?;
                    }
                }
            }
        }
        self.add("\"")
    }
    fn value(&mut self, v: &Value, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(Failure::new(
                FailureKind::Protocol,
                "canonical JSON depth exceeded",
            ));
        }
        match v {
            Value::Null => self.add("null"),
            Value::Bool(b) => self.add(if *b { "true" } else { "false" }),
            Value::String(s) => self.string(s),
            Value::Number(n) => {
                if n.is_f64() {
                    self.add(&python_float(n.as_f64().unwrap())?)
                } else {
                    self.add(&n.to_string())
                }
            }
            Value::Array(a) => {
                self.add("[")?;
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        self.add(",")?;
                    }
                    self.value(v, depth + 1)?;
                }
                self.add("]")
            }
            Value::Object(m) => {
                self.add("{")?;
                let mut keys: Vec<_> = m.keys().collect();
                keys.sort();
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        self.add(",")?;
                    }
                    self.string(k)?;
                    self.add(":")?;
                    self.value(&m[*k], depth + 1)?;
                }
                self.add("}")
            }
        }
    }
}
fn python_float(f: f64) -> Result<String> {
    if !f.is_finite() {
        return Err(Failure::new(
            FailureKind::Identity,
            "non-finite identity float",
        ));
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    if f == 0.0 {
        return Ok(format!("{sign}0.0"));
    }
    // Rust's LowerExp can pick the other equally near shortest decimal at a
    // halfway value. Ryū selects the nearest shortest decimal with ties to
    // even, as Python repr does. Apply Python's notation rules separately;
    // Ryū's pretty formatter has different fixed/scientific cutoffs.
    let mut buffer = ryu::Buffer::new();
    let shortest = buffer.format_finite(f.abs());
    let (mantissa, scale) = match shortest.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().unwrap()),
        None => (shortest, 0),
    };
    let point = mantissa.find('.').unwrap_or(mantissa.len());
    let all_digits = mantissa.replace('.', "");
    let significant = all_digits.trim_start_matches('0');
    let leading_zeros = all_digits.len() - significant.len();
    let exponent = point as i32 - leading_zeros as i32 - 1 + scale;
    let digits = significant.trim_end_matches('0');
    if !(-4..16).contains(&exponent) {
        let mantissa = if digits.len() == 1 {
            digits.to_owned()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        return Ok(format!(
            "{sign}{mantissa}e{}{exponent:02}",
            if exponent < 0 { "-" } else { "+" },
            exponent = exponent.abs()
        ));
    }
    let point = exponent + 1;
    let result = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    Ok(format!("{sign}{result}"))
}
