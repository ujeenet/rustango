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

/// An op's own key for humans only.
const OP_DENY: &str = "label";

/// Tuning knobs inside an op's `batch` object: changing them changes no outcome.
const BATCH_KNOBS: [&str; 3] = ["size", "sleep_ms", "max_batches_per_run"];

/// Top-level keys at their default: hashed as absent, so writing a default
/// out (as `fmt` does) never changes an applied file's checksum.
fn top_default(key: &str, v: &Value) -> bool {
    match key {
        "kind" => v == "migration",
        "phase" => v == "expand",
        "tx" => v == "auto",
        "offline" => v == false,
        "parents" | "depends_on" | "requires" | "replaces" | "allow" => is_empty_array(v),
        "base" | "state" | "irreversible_below" => v.is_null(),
        _ => false,
    }
}

/// Keys of op payloads at their default (op envelope, columns, indexes,
/// FKs, probes), hashed as absent. Per-backend SQL keys are never listed:
/// there `null` means "nothing runs".
fn field_default(key: &str, v: &Value) -> bool {
    match key {
        "restore" => v == "shape_only",
        "on_fresh" => v == "run",
        "method" => v == "btree",
        "on_delete" => v == "no_action",
        "unique" | "primary_key" | "auto" | "case_insensitive" | "sealed" | "idempotent"
        | "constraint" => v == false,
        "allow" | "include" | "skipped_on" | "indexes" | "checks" | "unique_sets" | "binds"
        | "down_binds" | "restores" => is_empty_array(v),
        "tx" | "concurrently" | "where" | "to_ty" | "max_length" | "default" | "comment"
        | "generated_as" | "min" | "max" | "fill" | "schema" | "probe" | "down" | "using"
        | "using_down" | "fk" | "portable_default" => v.is_null(),
        _ => false,
    }
}

fn is_empty_array(v: &Value) -> bool {
    v.as_array().is_some_and(Vec::is_empty)
}

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

/// Where a value sits, for the deny-list and the defaults.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// The file object itself.
    Top,
    /// An array of ops: `operations`, or a raw_sql `state` / state_only `ops`.
    OpList,
    /// One op object.
    Op,
    /// The value of an op's `batch` key.
    Batch,
    /// Inside an op payload: columns, indexes, FKs, probes, SQL maps.
    OpInner,
    /// Elsewhere in a file (`base`, a checkpoint `state`): nothing left out.
    Inner,
    /// Not a file: nothing is left out.
    Plain,
}

fn skipped(ctx: Ctx, key: &str, v: &Value) -> bool {
    match ctx {
        Ctx::Plain | Ctx::Inner | Ctx::OpList => false,
        Ctx::Top => TOP_LEVEL_DENY.contains(&key) || top_default(key, v),
        Ctx::Op => key == OP_DENY || field_default(key, v),
        Ctx::Batch => BATCH_KNOBS.contains(&key) || key.ends_with("timeout_ms"),
        Ctx::OpInner => field_default(key, v),
    }
}

/// The context of the value under `key` of an object in `ctx`.
fn child(ctx: Ctx, key: &str, v: &Value) -> Ctx {
    match (ctx, key) {
        (Ctx::Plain, _) => Ctx::Plain,
        (Ctx::Top, "operations") => Ctx::OpList,
        (Ctx::Top | Ctx::Inner, _) => Ctx::Inner,
        (Ctx::Op, "batch") => Ctx::Batch,
        (Ctx::Op, "state" | "ops") if v.is_array() => Ctx::OpList,
        _ => Ctx::OpInner,
    }
}

/// The context of an array element in `ctx`.
fn element(ctx: Ctx) -> Ctx {
    match ctx {
        Ctx::OpList => Ctx::Op,
        other => other,
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
                write(item, element(ctx), &format!("{pointer}/{i}"), out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .iter()
                .filter(|(k, v)| !skipped(ctx, k, v))
                .map(|(k, _)| k)
                .collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(k, out);
                out.push(':');
                let at = format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1"));
                write(&map[k], child(ctx, k, &map[k]), &at, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
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

    /// A key at its default hashes as absent: `fmt` writing defaults out
    /// keeps the checksum. A different value still counts.
    #[test]
    fn defaults_hash_as_absent() {
        let bare = json!({"engine": 2, "format": 1, "id": "0001_a", "operations": [
            {"op": "add_index", "v": 1, "table": "t", "name": "i", "columns": ["a"]}]});
        let mut full = bare.clone();
        full["kind"] = json!("migration");
        full["parents"] = json!([]);
        full["tx"] = json!("auto");
        full["operations"][0]["unique"] = json!(false);
        full["operations"][0]["method"] = json!("btree");
        full["operations"][0]["skipped_on"] = json!([]);
        assert_eq!(checksum(&bare).unwrap(), checksum(&full).unwrap());
        let mut other = bare.clone();
        other["operations"][0]["unique"] = json!(true);
        assert_ne!(checksum(&bare).unwrap(), checksum(&other).unwrap());
        // `null` in a per-backend SQL map means "nothing runs": it counts.
        let raw = |up: Value| json!({"operations": [{"op": "raw_sql", "up": up}]});
        assert_ne!(
            checksum(&raw(json!({"any": "X", "mysql": null}))).unwrap(),
            checksum(&raw(json!({"any": "X"}))).unwrap()
        );
    }

    /// Only an op's own `label` is left out, not a table or key named `label`.
    #[test]
    fn label_is_denied_on_ops_only() {
        // A table named `label` still counts: dropping it would equal no table.
        let with = json!({"kind": "checkpoint", "state": {"tables": {"label": {"name": "label"}}}});
        let without = json!({"kind": "checkpoint", "state": {"tables": {}}});
        assert_ne!(checksum(&with).unwrap(), checksum(&without).unwrap());
        let a = json!({"operations": [{"op": "x", "label": "a"}]});
        let b = json!({"operations": [{"op": "x", "label": "b"}]});
        assert_eq!(checksum(&a).unwrap(), checksum(&b).unwrap());
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
