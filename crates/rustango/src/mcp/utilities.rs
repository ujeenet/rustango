//! Optional MCP utilities (epic #1013, follow-up #1091): `logging/setLevel`
//! and `completion/complete`.
//!
//! * **logging** — `logging/setLevel` validates + acknowledges a client's
//!   minimum log level. (Per-session level state is a no-op on stateless
//!   Streamable HTTP; the server-push side rides the SSE bus from #1087.)
//! * **completion** — `completion/complete` returns prefix suggestions for
//!   the agent's granted prompts (skill codenames) and resource URIs,
//!   fail-closed and result-capped.

use serde_json::{json, Value};

use super::tools::McpContext;
use super::types::{codes, JsonRpcError};

/// MCP syslog-style levels, lowest→highest severity.
const LOG_LEVELS: &[&str] = &[
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

/// Max suggestions returned by `completion/complete` (MCP caps at 100).
const COMPLETION_CAP: usize = 100;

/// `logging/setLevel` — validate the requested level and acknowledge.
///
/// # Errors
/// `INVALID_PARAMS` for a missing or unrecognized `level`.
pub fn set_log_level(params: Value) -> Result<Value, JsonRpcError> {
    let level = params.get("level").and_then(Value::as_str).ok_or_else(|| {
        JsonRpcError::invalid_params("logging/setLevel requires a string `level`")
    })?;
    if !LOG_LEVELS.contains(&level) {
        return Err(JsonRpcError::invalid_params(format!(
            "unknown log level `{level}` (expected one of {})",
            LOG_LEVELS.join(", ")
        )));
    }
    Ok(json!({}))
}

/// `completion/complete` — suggest values for an argument, derived from the
/// agent's granted prompts + resources (fail-closed). `argument.value` is
/// the prefix to match.
///
/// # Errors
/// `INVALID_PARAMS` for a malformed request; DB errors propagate as
/// `INTERNAL_ERROR`.
pub async fn complete(ctx: &McpContext, params: Value) -> Result<Value, JsonRpcError> {
    let prefix = params
        .get("argument")
        .and_then(|a| a.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Candidate pool = granted prompt codenames + granted/static resource URIs.
    let mut candidates: Vec<String> = ctx.agent.skills.clone();

    let resources = crate::tenancy::resources_for_skills_pool(&ctx.pool, &ctx.agent.skills)
        .await
        .map_err(|e| JsonRpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    candidates.extend(resources.into_iter().map(|r| r.resource_uri));

    let mut values: Vec<String> = candidates
        .into_iter()
        .filter(|c| c.starts_with(prefix))
        .collect();
    values.sort();
    values.dedup();
    let total = values.len();
    let has_more = total > COMPLETION_CAP;
    values.truncate(COMPLETION_CAP);

    Ok(json!({
        "completion": { "values": values, "total": total, "hasMore": has_more }
    }))
}
