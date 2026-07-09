//! Method-adaptive params extractor — one handler serves `GET /x?a=1`
//! and `QUERY /x` with body `a=1` (issue #1109, epic #1107).
//!
//! [`Params<T>`] deserializes `T` from the request querystring on GET /
//! HEAD and from the request body on QUERY (RFC 10008), so a search or
//! filter endpoint accepts either transport with no branching in the
//! handler:
//!
//! ```ignore
//! use rustango::params::Params;
//! use rustango::http_query::QueryRouterExt;
//! use axum::routing::get;
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Search { q: String, page: Option<u32> }
//!
//! async fn search(Params(s): Params<Search>) -> String {
//!     format!("q={} page={:?}", s.q, s.page)
//! }
//!
//! // One handler, both transports.
//! let app = axum::Router::new().route("/search", get(search).query(search));
//! ```
//!
//! On QUERY the body is read by `Content-Type`:
//! - `application/x-www-form-urlencoded` (or no `Content-Type`) → parsed
//!   with the same `serde_urlencoded` codepath as the querystring, so `T`
//!   deserializes identically on both transports.
//! - `application/json` (or a `…+json` suffix) → parsed with `serde_json`.
//! - anything else → `415 Unsupported Media Type`.
//!
//! Any method other than GET / HEAD / QUERY is rejected with `405`.
//! Request-body size is enforced upstream by the body-limit layer, so an
//! oversized QUERY body is rejected before it reaches this extractor.
//!
//! ## Arrays and nested criteria
//!
//! The urlencoded path uses `serde_urlencoded` — exactly like axum's
//! [`Query`](axum::extract::Query) / [`Form`](axum::extract::Form) — so it
//! is **flat and single-value**: it can't express nested structures, and a
//! repeated key (`?tag=a&tag=b`) does not merge into a `Vec` — it's a
//! `duplicate field` error (rejected, not last-wins). When a search needs
//! arrays or nesting, that is exactly what QUERY unlocks: send a JSON body
//! (`application/json`), which deserializes with full `serde_json`
//! fidelity. The GET/urlencoded transport stays for the flat,
//! drop-in-compatible case.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use crate::api_errors::ApiError;

/// Extractor that reads `T` from the querystring on GET/HEAD and from the
/// request body on QUERY. See the [module docs](self).
pub struct Params<T>(pub T);

impl<T, S> FromRequest<S> for Params<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ParamsRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let method = req.method().clone();

        if method == Method::GET || method == Method::HEAD {
            let query = req.uri().query().unwrap_or_default();
            // Querystring parse failure → 400, matching axum's `Query`.
            let value = serde_urlencoded::from_str(query)
                .map_err(|e| ParamsRejection::DeserializeQuery(e.to_string()))?;
            return Ok(Params(value));
        }

        if method == *crate::http_query::QUERY {
            // Read Content-Type before the body — `Bytes::from_request`
            // consumes `req`.
            let kind = body_kind(&req)?;
            let bytes = Bytes::from_request(req, state)
                .await
                .map_err(|e| ParamsRejection::BodyRead(Box::new(e.into_response())))?;
            // Body parse failure → 422, matching axum's `Form` (a
            // well-formed request whose *content* is unprocessable).
            let value = match kind {
                BodyKind::UrlEncoded => serde_urlencoded::from_bytes(&bytes)
                    .map_err(|e| ParamsRejection::DeserializeBody(e.to_string()))?,
                BodyKind::Json => serde_json::from_slice(&bytes)
                    .map_err(|e| ParamsRejection::DeserializeBody(e.to_string()))?,
            };
            return Ok(Params(value));
        }

        Err(ParamsRejection::MethodNotAllowed)
    }
}

/// How a QUERY request body should be parsed, resolved from `Content-Type`.
enum BodyKind {
    UrlEncoded,
    Json,
}

/// Classify a QUERY body by its `Content-Type`, or reject unsupported
/// media types with `415`. A *missing* `Content-Type` is treated as
/// urlencoded, mirroring the leniency HTML form posts get; a *present but
/// unreadable* one is a media type we don't support, so it's a clean 415
/// rather than a silent urlencoded downgrade.
fn body_kind(req: &Request) -> Result<BodyKind, ParamsRejection> {
    let Some(ct) = req.headers().get(CONTENT_TYPE) else {
        return Ok(BodyKind::UrlEncoded);
    };
    let Ok(ct_str) = ct.to_str() else {
        return Err(ParamsRejection::UnsupportedMediaType(
            "<non-ascii Content-Type>".to_owned(),
        ));
    };
    let essence = ct_str
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "" | "application/x-www-form-urlencoded" => Ok(BodyKind::UrlEncoded),
        "application/json" => Ok(BodyKind::Json),
        other if other.ends_with("+json") => Ok(BodyKind::Json),
        other => Err(ParamsRejection::UnsupportedMediaType(other.to_owned())),
    }
}

/// Why a [`Params`] extraction failed. Renders to a JSON [`ApiError`].
pub enum ParamsRejection {
    /// The querystring didn't deserialize into `T` (`400`, matching axum's
    /// [`Query`](axum::extract::Query)).
    DeserializeQuery(String),
    /// The QUERY body didn't deserialize into `T` (`422`, matching axum's
    /// [`Form`](axum::extract::Form) — a well-formed request whose content
    /// is unprocessable).
    DeserializeBody(String),
    /// A QUERY body arrived with an unsupported `Content-Type` (`415`).
    UnsupportedMediaType(String),
    /// The request used a method other than GET / HEAD / QUERY (`405`).
    MethodNotAllowed,
    /// The body itself couldn't be read (size limit, IO) — the underlying
    /// `Bytes` rejection response, passed through verbatim. Boxed to keep
    /// the enum (and every `Result` over it) small.
    BodyRead(Box<Response>),
}

// Manual `Debug` (parity with the other extractor rejections) — the
// `BodyRead` payload is an axum `Response`, which isn't `Debug`, so it
// can't be derived.
impl std::fmt::Debug for ParamsRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamsRejection::DeserializeQuery(m) => {
                f.debug_tuple("DeserializeQuery").field(m).finish()
            }
            ParamsRejection::DeserializeBody(m) => {
                f.debug_tuple("DeserializeBody").field(m).finish()
            }
            ParamsRejection::UnsupportedMediaType(m) => {
                f.debug_tuple("UnsupportedMediaType").field(m).finish()
            }
            ParamsRejection::MethodNotAllowed => f.write_str("MethodNotAllowed"),
            ParamsRejection::BodyRead(r) => f.debug_tuple("BodyRead").field(&r.status()).finish(),
        }
    }
}

impl IntoResponse for ParamsRejection {
    fn into_response(self) -> Response {
        match self {
            ParamsRejection::DeserializeQuery(msg) => {
                ApiError::bad_request(format!("could not parse query params: {msg}"))
                    .into_response()
            }
            ParamsRejection::DeserializeBody(msg) => {
                ApiError::validation(format!("could not parse QUERY body: {msg}")).into_response()
            }
            ParamsRejection::UnsupportedMediaType(ct) => ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                format!(
                    "QUERY body Content-Type `{ct}` is not supported; \
                     use application/x-www-form-urlencoded or application/json"
                ),
            )
            .into_response(),
            ParamsRejection::MethodNotAllowed => {
                let mut resp = ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed",
                    "the Params extractor accepts GET, HEAD, and QUERY only",
                )
                .into_response();
                // RFC 9110 §15.5.6 — a 405 MUST carry `Allow`.
                resp.headers_mut().insert(
                    axum::http::header::ALLOW,
                    axum::http::HeaderValue::from_static("GET,HEAD,QUERY"),
                );
                resp
            }
            ParamsRejection::BodyRead(resp) => *resp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_query::QueryRouterExt;
    use axum::body::Body;
    use axum::routing::{get, post};
    use axum::Router;
    use serde::Deserialize;
    use tower::ServiceExt;

    // Flat struct — the shape the urlencoded/GET transport supports.
    #[derive(Deserialize)]
    struct Search {
        q: String,
        page: Option<u32>,
    }

    async fn show(Params(s): Params<Search>) -> String {
        format!("q={};page={:?}", s.q, s.page)
    }

    // Nested/array struct — only expressible via a JSON body on QUERY.
    #[derive(Deserialize)]
    struct RichSearch {
        q: String,
        #[serde(default)]
        tags: Vec<String>,
    }

    async fn show_rich(Params(s): Params<RichSearch>) -> String {
        format!("q={};tags={:?}", s.q, s.tags)
    }

    fn app() -> Router {
        Router::new().route("/s", get(show).query(show))
    }

    async fn send(app: Router, method: &str, uri: &str, ct: Option<&str>, body: &str) -> Response {
        let mut b = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(uri);
        if let Some(ct) = ct {
            b = b.header(CONTENT_TYPE, ct);
        }
        app.oneshot(b.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn text(resp: Response) -> (StatusCode, String) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn get_and_query_urlencoded_are_identical() {
        let qs = "q=hi&page=2";
        let (gs, gb) = text(send(app(), "GET", &format!("/s?{qs}"), None, "").await).await;
        let (qsx, qb) = text(
            send(
                app(),
                "QUERY",
                "/s",
                Some("application/x-www-form-urlencoded"),
                qs,
            )
            .await,
        )
        .await;
        assert_eq!(gs, StatusCode::OK);
        assert_eq!(qsx, StatusCode::OK);
        assert_eq!(
            gb, qb,
            "GET querystring and QUERY body must deserialize the same"
        );
        assert_eq!(gb, "q=hi;page=Some(2)");
    }

    #[tokio::test]
    async fn query_json_body_supports_arrays() {
        let app = Router::new().route("/r", get(show_rich).query(show_rich));
        let (s, b) = text(
            send(
                app,
                "QUERY",
                "/r",
                Some("application/json"),
                r#"{"q":"hi","tags":["x","y"]}"#,
            )
            .await,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, "q=hi;tags=[\"x\", \"y\"]");
    }

    #[tokio::test]
    async fn query_json_suffix_content_type() {
        let app = Router::new().route("/r", get(show_rich).query(show_rich));
        let (s, _) = text(
            send(
                app,
                "QUERY",
                "/r",
                Some("application/vnd.api+json"),
                r#"{"q":"hi","tags":[]}"#,
            )
            .await,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn query_missing_content_type_is_urlencoded() {
        let (s, b) = text(send(app(), "QUERY", "/s", None, "q=hi").await).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, "q=hi;page=None");
    }

    #[tokio::test]
    async fn query_unsupported_content_type_is_415() {
        let (s, _) = text(send(app(), "QUERY", "/s", Some("text/plain"), "q=hi").await).await;
        assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn missing_required_field_in_querystring_is_400() {
        let (s, _) = text(send(app(), "GET", "/s?page=1", None, "").await).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_query_body_is_422() {
        // Well-formed request, unprocessable content (page not a u32) —
        // 422, matching axum's `Form` body semantics (vs 400 for a
        // querystring).
        let (s, _) = text(
            send(
                app(),
                "QUERY",
                "/s",
                Some("application/x-www-form-urlencoded"),
                "q=hi&page=abc",
            )
            .await,
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn method_other_than_get_head_query_is_405_with_allow() {
        // Mount on POST so the request reaches the extractor (routing would
        // otherwise 405 first); the extractor's own method guard fires.
        let app = Router::new().route("/p", post(show));
        let resp = send(
            app,
            "POST",
            "/p",
            Some("application/x-www-form-urlencoded"),
            "q=hi",
        )
        .await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("GET,HEAD,QUERY"),
            "405 must advertise the allowed methods (RFC 9110)"
        );
    }
}
