//! Cursor pagination for the MCP `*/list` methods.
//!
//! A list result carries a `nextCursor`, which the client sends back
//! as the `cursor` param to get the next page. Page size comes from
//! `[mcp].max_tools_listed`; `None` or 0 means one page and no
//! cursor. The cursor holds an offset into the agent's list, whose
//! order is stable.
//!
//! **The cursor is signed and tied to one agent.** The payload
//! `o{offset}:a{agent_id}` is signed with a key derived from the
//! session secret, then base64url-wrapped so it looks opaque. A
//! cursor that was altered, forged, or minted for another agent is
//! rejected with `invalid_params`.

use serde_json::Value;

use super::types::JsonRpcError;
use crate::signing::Signer;

/// Paginate a `{ "<key>": [...] }` result. Returns one page, with a
/// `nextCursor` when items remain. The cursor is signed and tied to
/// `agent_id`.
///
/// # Errors
/// `invalid_params` when `cursor` is present but malformed, altered,
/// or minted for another agent.
pub fn paginate(
    mut full: Value,
    key: &str,
    cursor: Option<&str>,
    page_size: Option<usize>,
    agent_id: i64,
) -> Result<Value, JsonRpcError> {
    // No page size means one page and no cursor.
    let Some(page_size) = page_size.filter(|n| *n > 0) else {
        return Ok(full);
    };
    let offset = match cursor {
        Some(c) => decode_cursor(c, agent_id)?,
        None => 0,
    };
    let items = full
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    let total = items.len();
    let page: Vec<Value> = items.into_iter().skip(offset).take(page_size).collect();
    let next = offset.saturating_add(page_size);
    full[key] = Value::Array(page);
    if next < total {
        full["nextCursor"] = Value::String(encode_cursor(next, agent_id));
    }
    Ok(full)
}

/// The signer for pagination cursors. Its key comes from
/// `RUSTANGO_SESSION_SECRET`, or a random one per process when that
/// is unset. So a cursor lasts as long as the process, and verifies
/// on any instance that shares the secret. A salt keeps this key
/// separate from the ones other tokens use.
fn cursor_signer() -> &'static Signer {
    use std::sync::OnceLock;
    static SIGNER: OnceLock<Signer> = OnceLock::new();
    SIGNER.get_or_init(|| Signer::new(super::auth::jwt_secret()).with_salt("rustango.mcp.cursor"))
}

fn encode_cursor(offset: usize, agent_id: i64) -> String {
    let signed = cursor_signer().sign(&format!("o{offset}:a{agent_id}"));
    crate::url_codec::urlsafe_base64_encode(signed.as_bytes())
}

fn decode_cursor(c: &str, agent_id: i64) -> Result<usize, JsonRpcError> {
    let bad = || JsonRpcError::invalid_params("invalid pagination cursor");
    let signed = crate::url_codec::urlsafe_base64_decode(c)
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(bad)?;
    // Check the tag, in constant time, before trusting any byte.
    let value = cursor_signer().unsign(&signed).map_err(|_| bad())?;
    // value == "o{offset}:a{agent_id}"
    let (off_part, agent_part) = value.split_once(':').ok_or_else(bad)?;
    let offset = off_part
        .strip_prefix('o')
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or_else(bad)?;
    let bound = agent_part
        .strip_prefix('a')
        .and_then(|n| n.parse::<i64>().ok())
        .ok_or_else(bad)?;
    // A cursor minted for another agent cannot be replayed here.
    if bound != agent_id {
        return Err(bad());
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AGENT: i64 = 7;

    fn list(n: usize) -> Value {
        json!({ "tools": (0..n).map(|i| json!({ "name": i })).collect::<Vec<_>>() })
    }

    #[test]
    fn disabled_returns_all() {
        let out = paginate(list(5), "tools", None, None, AGENT).unwrap();
        assert_eq!(out["tools"].as_array().unwrap().len(), 5);
        assert!(out.get("nextCursor").is_none());
    }

    #[test]
    fn pages_and_cursor_roundtrip() {
        let p1 = paginate(list(5), "tools", None, Some(2), AGENT).unwrap();
        assert_eq!(p1["tools"].as_array().unwrap().len(), 2);
        let cur = p1["nextCursor"].as_str().expect("nextCursor");

        let p2 = paginate(list(5), "tools", Some(cur), Some(2), AGENT).unwrap();
        assert_eq!(p2["tools"].as_array().unwrap().len(), 2);
        assert_eq!(p2["tools"][0]["name"], 2); // continues after page 1

        // The last page carries no cursor.
        let cur2 = p2["nextCursor"].as_str().unwrap();
        let p3 = paginate(list(5), "tools", Some(cur2), Some(2), AGENT).unwrap();
        assert_eq!(p3["tools"].as_array().unwrap().len(), 1);
        assert!(p3.get("nextCursor").is_none());
    }

    #[test]
    fn bad_cursor_is_rejected() {
        let err = paginate(list(5), "tools", Some("!!!notb64"), Some(2), AGENT).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }

    #[test]
    fn unsigned_or_tampered_cursor_is_rejected() {
        // A cursor that looks right but is unsigned fails the check.
        let forged = crate::url_codec::urlsafe_base64_encode(b"o2:a7");
        let err = paginate(list(5), "tools", Some(&forged), Some(2), AGENT).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }

    #[test]
    fn cursor_is_bound_to_the_minting_agent() {
        // A cursor valid for one agent must not decode for another.
        let p1 = paginate(list(5), "tools", None, Some(2), AGENT).unwrap();
        let cur = p1["nextCursor"].as_str().unwrap();
        let err = paginate(list(5), "tools", Some(cur), Some(2), AGENT + 1).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }
}
