//! Checksum algorithm 1: SHA-256 of our own canonical form of the raw file.
//!
//! The form is defined here, not by `serde_json`'s writer, so it cannot move
//! with a dependency: object keys sorted by bytes, no whitespace, integers
//! only, strings escaped as below. Golden fixtures freeze the output.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The algorithm the ledger records next to each checksum.
pub(crate) const CHECKSUM_ALG: u32 = 1;

/// Top-level keys for humans only.
const TOP_LEVEL_DENY: [&str; 2] = ["created_at", "description"];

/// A key left out at any depth.
const DENY_ANYWHERE: &str = "label";

/// Tuning knobs inside a `batch` object: changing them changes no outcome.
const BATCH_KNOBS: [&str; 3] = ["size", "sleep_ms", "max_batches_per_run"];

/// A number that is not an integer: floats and exponents are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotInteger {
    /// JSON pointer of the value.
    pub(crate) pointer: String,
}

/// The hex SHA-256 of `file`'s canonical form, deny-listed keys left out.
///
/// # Errors
/// [`NotInteger`] for a float or exponent anywhere in `file`.
pub(crate) fn checksum(file: &Value) -> Result<String, NotInteger> {
    let mut out = String::new();
    write(file, Ctx::Top, "", &mut out)?;
    Ok(hex(&Sha256::digest(out.as_bytes())))
}

/// The canonical form itself; also what state hashes are taken over.
///
/// # Errors
/// As [`checksum`].
pub(crate) fn canonical(value: &Value) -> Result<String, NotInteger> {
    let mut out = String::new();
    write(value, Ctx::Plain, "", &mut out)?;
    Ok(out)
}

/// Where a value sits, for the deny-list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// The file object itself.
    Top,
    /// The value of a `batch` key.
    Batch,
    /// Anywhere else in a file.
    Inner,
    /// Not a file: nothing is left out.
    Plain,
}

fn skipped(ctx: Ctx, key: &str) -> bool {
    match ctx {
        Ctx::Plain => false,
        Ctx::Top => key == DENY_ANYWHERE || TOP_LEVEL_DENY.contains(&key),
        Ctx::Batch => {
            key == DENY_ANYWHERE || BATCH_KNOBS.contains(&key) || key.ends_with("timeout_ms")
        }
        Ctx::Inner => key == DENY_ANYWHERE,
    }
}

fn write(v: &Value, ctx: Ctx, pointer: &str, out: &mut String) -> Result<(), NotInteger> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if !(n.is_i64() || n.is_u64()) {
                return Err(NotInteger {
                    pointer: pointer.to_owned(),
                });
            }
            out.push_str(&n.to_string());
        }
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, child(ctx, ""), &format!("{pointer}/{i}"), out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().filter(|k| !skipped(ctx, k)).collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(k, out);
                out.push(':');
                let at = format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1"));
                write(&map[k], child(ctx, k), &at, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn child(ctx: Ctx, key: &str) -> Ctx {
    match ctx {
        Ctx::Plain => Ctx::Plain,
        _ if key == "batch" => Ctx::Batch,
        _ => Ctx::Inner,
    }
}

/// `"`, `\` and control characters escaped (`\u00xx`, lowercase); all else raw UTF-8.
fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Frozen across releases: a change here breaks every applied checksum.
    /// Values from an independent implementation of the form above.
    #[test]
    fn golden_checksums() {
        assert_eq!(
            CHECKSUM_ALG, 1,
            "a new algorithm gets a new number and new goldens"
        );
        for (name, text, want) in super::super::fixtures::CHECKSUM_GOLDENS {
            let v: Value = serde_json::from_str(text).unwrap();
            assert_eq!(checksum(&v).unwrap(), *want, "{name}");
        }
    }

    #[test]
    fn formatting_and_key_order_do_not_count() {
        let a: Value = serde_json::from_str(r#"{"b": 1, "a": [1, 2]}"#).unwrap();
        let b: Value = serde_json::from_str("{\n  \"a\":[1,2],\"b\":1}").unwrap();
        assert_eq!(checksum(&a).unwrap(), checksum(&b).unwrap());
    }

    #[test]
    fn the_deny_list_does_not_count_and_the_rest_does() {
        let base = json!({"id": "0001_a", "operations": [
            {"op": "data_sql", "label": "x", "batch": {"table": "t", "size": 10}}]});
        let mut human = base.clone();
        human["created_at"] = json!("2026-01-01");
        human["description"] = json!("hello");
        human["operations"][0]["label"] = json!("y");
        human["operations"][0]["batch"]["size"] = json!(99);
        human["operations"][0]["batch"]["lock_timeout_ms"] = json!(5);
        assert_eq!(checksum(&base).unwrap(), checksum(&human).unwrap());
        let mut real = base.clone();
        real["operations"][0]["batch"]["table"] = json!("u");
        assert_ne!(checksum(&base).unwrap(), checksum(&real).unwrap());
        // `size` outside a batch, and `description` below the top, count.
        let mut nested = base.clone();
        nested["operations"][0]["description"] = json!("d");
        assert_ne!(checksum(&base).unwrap(), checksum(&nested).unwrap());
    }

    #[test]
    fn floats_and_exponents_are_refused() {
        for text in [r#"{"a": 1.5}"#, r#"{"a": [1e3]}"#, r#"{"a": {"b": 2.0}}"#] {
            let v: Value = serde_json::from_str(text).unwrap();
            assert!(checksum(&v).is_err(), "{text}");
        }
        let v: Value = serde_json::from_str(r#"{"a": {"b/c": [0, 2.0]}}"#).unwrap();
        assert_eq!(checksum(&v).unwrap_err().pointer, "/a/b~1c/1");
    }

    #[test]
    fn escapes_are_fixed() {
        let v = json!({"s": "a\"b\\c\nd\u{1}é"});
        assert_eq!(
            canonical(&v).unwrap(),
            "{\"s\":\"a\\\"b\\\\c\\u000ad\\u0001é\"}"
        );
    }
}
