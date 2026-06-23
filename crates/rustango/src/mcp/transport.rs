//! Streamable-HTTP transport. `POST {prefix}` carries a single JSON-RPC
//! message client→server; the optional `GET {prefix}` opens an SSE stream
//! for server→client notifications (Slice 1 wires the channel; the
//! `list_changed` / `progress` follow-ups, #1087 / #1090, fill it).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use super::handlers::dispatch;
use super::router::McpState;
use super::types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// `POST {prefix}` — parse one JSON-RPC message, dispatch it, and reply.
///
/// - Malformed JSON → JSON-RPC `parse error` (id `null`).
/// - Valid JSON that isn't a well-formed request → `invalid request`.
/// - A notification (no `id`) is acknowledged with `202 Accepted` and no
///   body, per JSON-RPC 2.0 §4.1.
/// - A request gets a `200` JSON-RPC success/error response.
pub(crate) async fn post_handler(State(state): State<McpState>, body: Bytes) -> Response {
    // Unauthed Slice-1 transport: no agent principal, so `tools/*` are
    // refused with "authentication required".
    handle_message(&state, &body, None).await
}

/// Parse + dispatch one JSON-RPC message and build the HTTP response.
/// Shared by the unauthed handler above and the authed handler in
/// [`super::auth`] (which runs agent-JWT verification first and passes the
/// resolved [`McpContext`]).
pub(crate) async fn handle_message(
    state: &McpState,
    body: &[u8],
    ctx: Option<super::tools::McpContext>,
) -> Response {
    // Two-step parse so a syntactically valid but structurally wrong
    // message still recovers its `id` for the error response.
    let value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return json_error(Value::Null, JsonRpcError::parse_error()),
    };
    let recovered_id = value.get("id").cloned().unwrap_or(Value::Null);
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => return json_error(recovered_id, JsonRpcError::invalid_request(e.to_string())),
    };

    if request.is_notification() {
        // `notifications/cancelled { requestId }` trips the in-flight call's
        // cancel token (#1090); other notifications (e.g. `initialized`) are
        // no-ops. Either way a notification gets no JSON-RPC response.
        if request.method == "notifications/cancelled" {
            if let Some(rid) = request
                .params
                .as_ref()
                .and_then(|p| p.get("requestId"))
                .map(jsonrpc_id_string)
            {
                super::progress::cancel(&rid);
            }
        }
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    let request_id = jsonrpc_id_string(&id);
    match dispatch(
        state,
        &request.method,
        request.params,
        ctx,
        Some(request_id.as_str()),
    )
    .await
    {
        Ok(result) => Json(JsonRpcResponse::success(id, result)).into_response(),
        Err(err) => Json(JsonRpcResponse::failure(id, err)).into_response(),
    }
}

/// `GET {prefix}` — authenticated SSE stream relaying server→client
/// notifications for the **connected agent only** (#1092). Requires the same
/// tenant-pinned agent Bearer as the JSON-RPC endpoint, then filters the
/// process-global bus so an agent never sees another agent's / tenant's
/// `progress` or `list_changed` frames.
pub(crate) async fn sse_handler(
    t: crate::extractors::Tenant,
    axum::extract::State(state): axum::extract::State<McpState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(jwt) = state.jwt.as_ref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "mcp auth not configured").into_response();
    };
    let Some(token) = super::auth::bearer(&headers) else {
        return super::auth::unauthorized(&headers);
    };
    let Some(agent) = super::auth::verify_agent_token(jwt, token, &t.org.slug) else {
        return super::auth::unauthorized(&headers);
    };
    let tenant = agent.tenant.clone();
    let agent_id = agent.agent_id;

    let mut rx = super::notifications::bus().subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if super::notifications::frame_visible(&frame, &tenant, agent_id) {
                        yield Ok::<Event, std::convert::Infallible>(Event::default().data(frame.body));
                    }
                    // else: not for this agent — drop silently.
                }
                // Slow consumer fell behind the buffer — skip and continue
                // rather than tearing the connection down.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // All senders dropped — end the stream.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn json_error(id: Value, error: JsonRpcError) -> Response {
    Json(JsonRpcResponse::failure(id, error)).into_response()
}

/// Normalize a JSON-RPC id (string or number) to a stable registry key so
/// a `notifications/cancelled { requestId }` matches the in-flight call.
fn jsonrpc_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
