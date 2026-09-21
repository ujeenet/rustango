//! The Streamable HTTP transport. `POST {prefix}` carries one
//! JSON-RPC message from the client. `GET {prefix}` opens an SSE
//! stream that carries `progress` and `list_changed` notifications
//! back.

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

/// `POST {prefix}`: parse one JSON-RPC message, run it, reply.
///
/// - Malformed JSON gives a `parse error` with a null id.
/// - Valid JSON in the wrong shape gives an `invalid request`.
/// - A notification, which has no `id`, gets `202 Accepted` and an
///   empty body.
/// - A request gets a `200` with a success or error result.
pub(crate) async fn post_handler(State(state): State<McpState>, body: Bytes) -> Response {
    // This route does not authenticate, so there is no agent and the
    // `tools/*` methods are refused.
    handle_message(&state, &body, None).await
}

/// Parse one message, dispatch it, and build the HTTP response.
/// Shared with the authenticated handler in [`super::auth`], which
/// verifies the token first and passes the agent context in.
pub(crate) async fn handle_message(
    state: &McpState,
    body: &[u8],
    ctx: Option<super::tools::McpContext>,
) -> Response {
    // Parse in two steps, so a message with valid JSON but the wrong
    // shape still yields its `id` for the error response.
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
        // `notifications/cancelled { requestId }` trips the cancel
        // token of a call in flight. It is scoped to the agent that
        // sent it, so nobody can cancel another agent's call. With no
        // authenticated agent there is nothing to cancel. Every other
        // notification, such as `initialized`, does nothing.
        if request.method == "notifications/cancelled" {
            if let (Some(ctx), Some(rid)) = (
                ctx.as_ref(),
                request
                    .params
                    .as_ref()
                    .and_then(|p| p.get("requestId"))
                    .map(jsonrpc_id_string),
            ) {
                super::progress::cancel(&ctx.agent.tenant, ctx.agent.agent_id, &rid);
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

/// `GET {prefix}`: an SSE stream of notifications **for the
/// connected agent only**.
///
/// It needs the same agent bearer token as the JSON-RPC endpoint, and
/// then filters the shared bus, so an agent never sees another
/// agent's or another tenant's frames.
pub(crate) async fn sse_handler(
    t: crate::extractors::Tenant,
    axum::extract::State(state): axum::extract::State<McpState>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(jwt) = state.jwt.as_ref() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "mcp auth not configured").into_response();
    };
    let Some(token) = super::auth::bearer(&headers) else {
        return super::auth::unauthorized(&headers, &uri);
    };
    // Accept both bearer shapes the JSON-RPC POST accepts. See
    // `auth::authenticate_bearer` for why this stream must not be
    // stricter than the endpoint next to it.
    let agent = match super::auth::authenticate_bearer(jwt, t.pool(), &t.org.slug, token).await {
        Ok(agent) => agent,
        Err(e) => return e.into_response(&headers, &uri),
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
                    // Frames for anyone else are dropped.
                }
                // This client fell behind the buffer. Skip what it
                // missed rather than closing the connection.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // Every sender is gone, so end the stream.
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

/// Turn a JSON-RPC id, which may be a string or a number, into one
/// stable key, so a `cancelled` notification finds its call.
fn jsonrpc_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
