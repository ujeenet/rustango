//! Prompts + resources from skills (epic #1013, Slice 5 / #1018).
//!
//! The full MCP skill surface beyond tools:
//!
//! * **Prompts** — each granted skill *is* a prompt; its `instructions`
//!   field is the prompt body. `prompts/list` / `prompts/get` are derived
//!   from the agent's granted skills (fail-closed: only granted skills).
//! * **Resources** — `resources/list` / `resources/read` return the
//!   resources attached to granted skills, **plus** any static framework
//!   resources registered with [`register_mcp_resource!`] (the same
//!   `inventory` pattern as tools; static resources are app-declared and
//!   always available).

use serde_json::{json, Value};

use super::tools::McpContext;
use super::types::{codes, JsonRpcError};

// ---------------------------------------------------------------- prompts

/// `prompts/list` — one entry per granted skill.
///
/// # Errors
/// DB errors while loading the granted skills.
pub async fn list_prompts(ctx: &McpContext) -> Result<Value, JsonRpcError> {
    let skills = crate::tenancy::skills_by_codenames_pool(&ctx.pool, &ctx.agent.skills)
        .await
        .map_err(internal)?;
    let prompts: Vec<Value> = skills
        .iter()
        .map(|s| json!({ "name": s.codename, "description": s.description, "arguments": [] }))
        .collect();
    Ok(json!({ "prompts": prompts }))
}

/// `prompts/get` — return a granted skill's instructions as a prompt
/// message. Fail-closed: an ungranted (or unknown) prompt name errors.
///
/// # Errors
/// `INVALID_PARAMS` for a missing `name`; `TOOL_FORBIDDEN` for a
/// prompt the agent isn't granted; DB errors otherwise.
pub async fn get_prompt(ctx: &McpContext, params: Value) -> Result<Value, JsonRpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_params("prompts/get requires a string `name`"))?;
    if !ctx.agent.skills.iter().any(|s| s == name) {
        return Err(JsonRpcError::new(
            codes::TOOL_FORBIDDEN,
            format!("prompt `{name}` is not granted to this agent"),
        ));
    }
    let skill = crate::tenancy::skills_by_codenames_pool(&ctx.pool, &[name.to_owned()])
        .await
        .map_err(internal)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            JsonRpcError::new(codes::TOOL_FORBIDDEN, format!("unknown prompt: {name}"))
        })?;
    Ok(json!({
        "description": skill.description,
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": skill.instructions },
        }],
    }))
}

// -------------------------------------------------------------- resources

/// A static, compile-time-registered framework resource.
pub struct McpResource {
    /// Resource URI (the `resources/read` key).
    pub uri: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// MIME type.
    pub mime_type: &'static str,
    /// Produces the resource body.
    pub read: fn() -> String,
}

inventory::collect!(McpResource);

fn static_resource(uri: &str) -> Option<&'static McpResource> {
    inventory::iter::<McpResource>
        .into_iter()
        .find(|r| r.uri == uri)
}

/// `resources/list` — granted-skill resources + static framework resources.
///
/// # Errors
/// DB errors while loading skill resources.
pub async fn list_resources(ctx: &McpContext) -> Result<Value, JsonRpcError> {
    let mut out: Vec<Value> = inventory::iter::<McpResource>
        .into_iter()
        .map(|r| json!({ "uri": r.uri, "name": r.name, "mimeType": r.mime_type }))
        .collect();

    let skill_resources = crate::tenancy::resources_for_skills_pool(&ctx.pool, &ctx.agent.skills)
        .await
        .map_err(internal)?;
    for r in &skill_resources {
        out.push(json!({ "uri": r.resource_uri, "mimeType": r.mime }));
    }
    Ok(json!({ "resources": out }))
}

/// `resources/read` — read a static framework resource or a granted-skill
/// resource by URI. Fail-closed: a skill resource only reads if its skill
/// is granted.
///
/// # Errors
/// `INVALID_PARAMS` for a missing `uri`; `TOOL_FORBIDDEN` when the URI isn't
/// a static resource and isn't reachable through a granted skill.
pub async fn read_resource(ctx: &McpContext, params: Value) -> Result<Value, JsonRpcError> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_params("resources/read requires a string `uri`"))?;

    // Static framework resources are always readable.
    if let Some(r) = static_resource(uri) {
        return Ok(contents(uri, r.mime_type, (r.read)()));
    }

    // Otherwise it must belong to one of the agent's granted skills.
    let skill_resources = crate::tenancy::resources_for_skills_pool(&ctx.pool, &ctx.agent.skills)
        .await
        .map_err(internal)?;
    let res = skill_resources
        .into_iter()
        .find(|r| r.resource_uri == uri)
        .ok_or_else(|| {
            JsonRpcError::new(
                codes::TOOL_FORBIDDEN,
                format!("resource `{uri}` is not available to this agent"),
            )
        })?;
    let text = res
        .data
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| res.data.to_string());
    Ok(contents(uri, &res.mime, text))
}

fn contents(uri: &str, mime: &str, text: String) -> Value {
    json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] })
}

fn internal(e: crate::tenancy::AgentError) -> JsonRpcError {
    JsonRpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

/// Register a static framework resource available to every authenticated
/// agent (independent of skill grants).
///
/// ```ignore
/// rustango::register_mcp_resource!(
///     "rustango://about", "About", "text/plain",
///     || "rustango MCP server".to_string(),
/// );
/// ```
#[macro_export]
macro_rules! register_mcp_resource {
    ($uri:expr, $name:expr, $mime:expr, $read:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::mcp::McpResource {
                uri: $uri,
                name: $name,
                mime_type: $mime,
                read: {
                    fn __rustango_mcp_resource_read() -> ::std::string::String {
                        ($read)()
                    }
                    __rustango_mcp_resource_read
                },
            }
        }
    };
}
