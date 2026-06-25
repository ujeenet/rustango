//! Cursor pagination for the MCP `*/list` methods (epic #1013,
//! follow-up #1089).
//!
//! MCP list results carry an opaque `nextCursor`; the client passes it
//! back as the `cursor` param to fetch the next page. Page size is the
//! server's `[mcp].max_tools_listed` knob (`None`/0 ⇒ pagination off, a
//! single page with no `nextCursor`). The cursor encodes an offset over
//! the agent's *stable-ordered* list.
//!
//! The cursor is **HMAC-signed and bound to the requesting agent** (#1099):
//! the payload `o{offset}:a{agent_id}` is signed with a process-stable key
//! derived from the framework session secret, then base64url-wrapped for
//! opacity. A tampered, forged, or another agent's cursor is rejected
//! (`invalid_params`).

use serde_json::Value;

use super::types::JsonRpcError;
use crate::signing::Signer;

/// Apply pagination to a `{ "<key>": [...] }` list result. Returns the
/// (possibly sliced) value with a `nextCursor` when more items remain. The
/// cursor is signed + bound to `agent_id`.
///
/// # Errors
/// `invalid_params` if `cursor` is present but malformed, tampered, or minted
/// for a different agent.
pub fn paginate(
    mut full: Value,
    key: &str,
    cursor: Option<&str>,
    page_size: Option<usize>,
    agent_id: i64,
) -> Result<Value, JsonRpcError> {
    // Pagination disabled (no/zero page size): single page, no cursor.
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

/// Process-stable signer for pagination cursors. Keyed off the framework
/// session secret (`RUSTANGO_SESSION_SECRET`, or a per-process random fallback)
/// so cursors stay valid for the process lifetime and verify across instances
/// that share the secret. Salt isolates this from other token purposes.
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
    // Verify the HMAC tag (constant-time) before trusting any bytes.
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
    // Agent-bound: a cursor minted for another agent can't be replayed.
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

        // Last page has no nextCursor.
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
        // A plausible-looking but unsigned cursor (the pre-#1099 format) fails
        // the HMAC check.
        let forged = crate::url_codec::urlsafe_base64_encode(b"o2:a7");
        let err = paginate(list(5), "tools", Some(&forged), Some(2), AGENT).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }

    #[test]
    fn cursor_is_bound_to_the_minting_agent() {
        // A valid cursor for AGENT must NOT decode for a different agent.
        let p1 = paginate(list(5), "tools", None, Some(2), AGENT).unwrap();
        let cur = p1["nextCursor"].as_str().unwrap();
        let err = paginate(list(5), "tools", Some(cur), Some(2), AGENT + 1).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }
}
