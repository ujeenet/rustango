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
        // Slice 1 has no notification side effects (e.g. `notifications/
        // initialized` is a no-op); just acknowledge.
        return StatusCode::ACCEPTED.into_response();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    match dispatch(state, &request.method, request.params, ctx).await {
        Ok(result) => Json(JsonRpcResponse::success(id, result)).into_response(),
        Err(err) => Json(JsonRpcResponse::failure(id, err)).into_response(),
    }
}

/// `GET {prefix}` — open an SSE stream that relays server→client
/// notification frames published on the [`McpState`] bus. Slice 1 sends
/// nothing; the stream stays open with keep-alive pings so follow-up
/// slices have a channel to push on.
pub(crate) async fn sse_handler(State(state): State<McpState>) -> impl IntoResponse {
    let mut rx = state.bus.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(frame) => yield Ok::<Event, std::convert::Infallible>(Event::default().data(frame)),
                // Slow consumer fell behind the buffer — skip and continue
                // rather than tearing the connection down.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // All senders dropped — end the stream.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn json_error(id: Value, error: JsonRpcError) -> Response {
    Json(JsonRpcResponse::failure(id, error)).into_response()
}
