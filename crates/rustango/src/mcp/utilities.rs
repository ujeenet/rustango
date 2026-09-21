//! Two optional MCP methods.
//!
//! `logging/setLevel` checks the level a client asks for and
//! acknowledges it. There is no per-session state to keep, because
//! Streamable HTTP is stateless; server-side pushes go over the SSE
//! bus.
//!
//! `completion/complete` suggests values that start with a given
//! prefix, drawn from the prompts and resources this agent was
//! granted. It fails closed and caps how much it returns.

use serde_json::{json, Value};

use super::tools::McpContext;
use super::types::{codes, JsonRpcError};

/// The log levels MCP allows, least severe first.
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

/// How many suggestions `completion/complete` may return. MCP caps
/// this at 100.
const COMPLETION_CAP: usize = 100;

/// `logging/setLevel`: check the level and acknowledge it.
///
/// # Errors
/// `INVALID_PARAMS` when `level` is missing or unknown.
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

/// `completion/complete`: suggest values for an argument, taken from
/// the prompts and resources this agent was granted.
/// `argument.value` is the prefix to match.
///
/// # Errors
/// `INVALID_PARAMS` on a malformed request, `INTERNAL_ERROR` on a
/// database failure.
pub async fn complete(ctx: &McpContext, params: Value) -> Result<Value, JsonRpcError> {
    let prefix = params
        .get("argument")
        .and_then(|a| a.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // Candidates are the granted prompt codenames plus the resource
    // URIs, both granted and static.
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
