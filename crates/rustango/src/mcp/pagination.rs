//! Cursor pagination for the MCP `*/list` methods (epic #1013,
//! follow-up #1089).
//!
//! MCP list results carry an opaque `nextCursor`; the client passes it
//! back as the `cursor` param to fetch the next page. Page size is the
//! server's `[mcp].max_tools_listed` knob (`None`/0 ⇒ pagination off, a
//! single page with no `nextCursor`). The cursor encodes an offset over
//! the agent's *stable-ordered* list, base64url-wrapped for opacity; a
//! malformed cursor is rejected (`invalid_params`).

use serde_json::Value;

use super::types::JsonRpcError;

/// Apply pagination to a `{ "<key>": [...] }` list result. Returns the
/// (possibly sliced) value with a `nextCursor` when more items remain.
///
/// # Errors
/// `invalid_params` if `cursor` is present but malformed.
pub fn paginate(
    mut full: Value,
    key: &str,
    cursor: Option<&str>,
    page_size: Option<usize>,
) -> Result<Value, JsonRpcError> {
    // Pagination disabled (no/zero page size): single page, no cursor.
    let Some(page_size) = page_size.filter(|n| *n > 0) else {
        return Ok(full);
    };
    let offset = match cursor {
        Some(c) => decode_cursor(c)?,
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
        full["nextCursor"] = Value::String(encode_cursor(next));
    }
    Ok(full)
}

fn encode_cursor(offset: usize) -> String {
    crate::url_codec::urlsafe_base64_encode(format!("o{offset}").as_bytes())
}

fn decode_cursor(c: &str) -> Result<usize, JsonRpcError> {
    crate::url_codec::urlsafe_base64_decode(c)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.strip_prefix('o').and_then(|n| n.parse::<usize>().ok()))
        .ok_or_else(|| JsonRpcError::invalid_params("invalid pagination cursor"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list(n: usize) -> Value {
        json!({ "tools": (0..n).map(|i| json!({ "name": i })).collect::<Vec<_>>() })
    }

    #[test]
    fn disabled_returns_all() {
        let out = paginate(list(5), "tools", None, None).unwrap();
        assert_eq!(out["tools"].as_array().unwrap().len(), 5);
        assert!(out.get("nextCursor").is_none());
    }

    #[test]
    fn pages_and_cursor_roundtrip() {
        let p1 = paginate(list(5), "tools", None, Some(2)).unwrap();
        assert_eq!(p1["tools"].as_array().unwrap().len(), 2);
        let cur = p1["nextCursor"].as_str().expect("nextCursor");

        let p2 = paginate(list(5), "tools", Some(cur), Some(2)).unwrap();
        assert_eq!(p2["tools"].as_array().unwrap().len(), 2);
        assert_eq!(p2["tools"][0]["name"], 2); // continues after page 1

        // Last page has no nextCursor.
        let cur2 = p2["nextCursor"].as_str().unwrap();
        let p3 = paginate(list(5), "tools", Some(cur2), Some(2)).unwrap();
        assert_eq!(p3["tools"].as_array().unwrap().len(), 1);
        assert!(p3.get("nextCursor").is_none());
    }

    #[test]
    fn bad_cursor_is_rejected() {
        let err = paginate(list(5), "tools", Some("!!!notb64"), Some(2)).unwrap_err();
        assert_eq!(err.code, super::super::types::codes::INVALID_PARAMS);
    }
}
