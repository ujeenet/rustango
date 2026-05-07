//! axum router for serving the OpenAPI spec + Swagger UI / Redoc viewers.
//!
//! - `GET /openapi.json` — the spec serialized as JSON.
//! - `GET /docs` — Swagger UI (loaded from a CDN).
//! - `GET /redoc` — Redoc (also CDN-loaded).
//!
//! No JS files are bundled into rustango — the viewer pages are tiny
//! HTML shells that pull the viewer from `unpkg.com`. If you need an
//! offline build, write the spec out at startup and self-host the
//! viewer assets in your own static dir.

use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::OpenApiSpec;

/// Mount the spec + viewer routes under the given router.
#[must_use]
pub fn openapi_router(spec: OpenApiSpec) -> Router {
    Router::new()
        .route("/openapi.json", get(serve_spec))
        .route("/docs", get(swagger_ui))
        .route("/redoc", get(redoc))
        .with_state(Arc::new(spec))
}

async fn serve_spec(State(spec): State<Arc<OpenApiSpec>>) -> Response {
    let body = spec.to_json();
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

async fn swagger_ui() -> Html<&'static str> {
    Html(SWAGGER_HTML)
}

async fn redoc() -> Html<&'static str> {
    Html(REDOC_HTML)
}

const SWAGGER_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>API Docs</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
</head>
<body>
<div id="swagger-ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
<script>
  window.onload = () => {
    window.ui = SwaggerUIBundle({
      url: "/openapi.json",
      dom_id: "#swagger-ui",
      deepLinking: true,
    });
  };
</script>
</body>
</html>
"##;

const REDOC_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>API Docs</title>
<style>body { margin: 0; }</style>
</head>
<body>
<redoc spec-url="/openapi.json"></redoc>
<script src="https://cdn.jsdelivr.net/npm/redoc@2/bundles/redoc.standalone.js"></script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::{Operation, PathItem, Response as OpenApiResponse, Schema};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn sample_spec() -> OpenApiSpec {
        OpenApiSpec::new("Demo", "1.2.3").add_path(
            "/ping",
            PathItem::new().get(Operation::new().summary("Ping").response(
                "200",
                OpenApiResponse::new("pong").json_content(Schema::string()),
            )),
        )
    }

    #[tokio::test]
    async fn openapi_json_returns_serialized_spec() {
        let app = openapi_router(sample_spec());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json; charset=utf-8"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["info"]["title"], "Demo");
        assert_eq!(v["info"]["version"], "1.2.3");
        assert_eq!(v["paths"]["/ping"]["get"]["summary"], "Ping");
    }

    #[tokio::test]
    async fn docs_serves_swagger_ui_html() {
        let app = openapi_router(sample_spec());
        let resp = app
            .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("swagger-ui"));
        assert!(body.contains("/openapi.json"));
    }

    #[tokio::test]
    async fn redoc_serves_redoc_html() {
        let app = openapi_router(sample_spec());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/redoc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("redoc"));
        assert!(body.contains("/openapi.json"));
    }
}
