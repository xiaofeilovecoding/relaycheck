//! Canonical JSON serialization — the tested RFC 8785 (JCS) subset used for
//! manifests, receipts and profile hashes.
//!
//! Frozen rules (spec §4):
//! - UTF-8 output, no BOM
//! - object keys sorted by UTF-16 code units (JCS)
//! - strings escaped minimally per JCS: `"` `\` and C0 controls only,
//!   using `\b \t \n \f \r` or `\u00xx` (lowercase hex); everything else
//!   literal (including `/`)
//! - numbers: integers within the IEEE-754 double safe range
//!   (±(2^53−1)) serialize as integers; any other number is rejected
//!   (manifests/receipts carry counts and sizes, not floats)
//! - no Unicode normalization is performed
//!
//! A fixed golden vector with its SHA-256 is asserted in tests so the
//! canonical form cannot drift silently.

use crate::errors::{RelayError, RelayResult};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn canonicalize(value: &Value) -> RelayResult<Vec<u8>> {
    let mut out = Vec::new();
    write_value(value, &mut out)?;
    Ok(out)
}

pub fn canonical_sha256(value: &Value) -> RelayResult<String> {
    let bytes = canonicalize(value)?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> RelayResult<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                if (-9007199254740991i64..=9007199254740991i64).contains(&i) {
                    out.extend_from_slice(i.to_string().as_bytes());
                    return Ok(());
                }
            } else if let Some(u) = n.as_u64() {
                if u <= 9007199254740991u64 {
                    out.extend_from_slice(u.to_string().as_bytes());
                    return Ok(());
                }
            }
            // Floats and out-of-safe-range integers are rejected: canonical
            // float formatting (ES6 Number::toString) is deliberately out of
            // scope because our frozen schemas never carry such values.
            return Err(RelayError::validation(
                "canonical JSON only supports integers within the ±(2^53-1) safe range",
            ));
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // JCS sorts keys by their UTF-16 code units, which differs from
            // UTF-8 byte order for non-BMP characters. Sort explicitly.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_key(|k| k.encode_utf16().collect::<Vec<u16>>());
            out.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(k, out);
                out.push(b':');
                write_value(&map[*k], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{9}' => out.extend_from_slice(b"\\t"),
            '\u{A}' => out.extend_from_slice(b"\\n"),
            '\u{C}' => out.extend_from_slice(b"\\f"),
            '\u{D}' => out.extend_from_slice(b"\\r"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// The golden canonicalization vector. The expected bytes below are the
/// literal canonical output; the SHA-256 is asserted to match so any change
/// to the serializer is caught by tests on every platform.
pub mod golden {
    /// Input as parsed JSON text.
    pub const INPUT_JSON: &str = r##"{"b":1,"a":2,"nested":{"z":[1,2,{"k":"v"}],"ä":true,"€":null},"s":"line\n\t\"q\"\\back €漢","big":9007199254740991,"arr":[]}"##;
    /// Expected canonical output (keys sorted by UTF-16 code units, minimal escapes).
    pub const EXPECTED: &str = "{\"a\":2,\"arr\":[],\"b\":1,\"big\":9007199254740991,\"nested\":{\"z\":[1,2,{\"k\":\"v\"}],\"ä\":true,\"€\":null},\"s\":\"line\\n\\t\\\"q\\\"\\\\back €漢\"}";
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn golden_vector_matches() {
        let value: Value = serde_json::from_str(golden::INPUT_JSON).unwrap();
        let bytes = canonicalize(&value).unwrap();
        let got = String::from_utf8(bytes).unwrap();
        assert_eq!(got, golden::EXPECTED);
        let sha = hex::encode(Sha256::digest(golden::EXPECTED.as_bytes()));
        // Recorded cross-platform acceptance value for the golden vector.
        assert_eq!(sha, "68366a42efe4152aa866cc6fe4e36bfe98c4cd1b96120cd2f74f520e387511ab");
    }

    #[test]
    fn rejects_floats_and_unsafe_integers() {
        let v: Value = serde_json::from_str("{\"x\":1.5}").unwrap();
        assert!(canonicalize(&v).is_err());
        let v: Value = serde_json::from_str("{\"x\":12345678901234567890}").unwrap();
        assert!(canonicalize(&v).is_err());
    }

    #[test]
    fn sorts_keys_by_utf16_units() {
        // Non-BMP: "\u{10000}" encodes as the surrogate pair 0xD800 0xDC00,
        // which sorts BEFORE "\u{E9FF}" (0xE9FF) under UTF-16 ordering even
        // though its scalar value is larger. JSON input uses surrogate pairs.
        let v: Value = serde_json::from_str(
            "{\"\\uE9FF\":1,\"\\uD800\\uDC00\":2}",
        )
        .unwrap();
        let bytes = canonicalize(&v).unwrap();
        let got = String::from_utf8(bytes).unwrap();
        let idx_10000 = got.find('\u{10000}').unwrap();
        let idx_e9ff = got.find('\u{E9FF}').unwrap();
        assert!(idx_10000 < idx_e9ff, "got: {got}");
    }
}
