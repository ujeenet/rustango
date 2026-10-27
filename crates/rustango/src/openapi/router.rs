//! An axum router that serves the OpenAPI spec and two viewers.
//!
//! - `GET /openapi.json` returns the spec as JSON.
//! - `GET /docs` serves Swagger UI.
//! - `GET /redoc` serves Redoc.
//!
//! rustango bundles no JavaScript. The two viewer pages are small
//! HTML shells that load an exact viewer version from a CDN, checked
//! by a Subresource Integrity hash. They fetch the spec next to
//! themselves, so the router can be nested under any prefix. To work
//! offline, write the spec to a file at startup and host the viewer
//! from your own static directory.

use std::sync::Arc;

use axum::extract::{OriginalUri, State};
use axum::http::{header, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::OpenApiSpec;

/// A router carrying the spec and viewer routes.
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

/// `/openapi.json` under the prefix this router is nested at.
fn spec_url(original: &Uri, inner: &Uri) -> String {
    let prefix = original.path().strip_suffix(inner.path()).unwrap_or("");
    format!("{prefix}/openapi.json")
}

async fn swagger_ui(OriginalUri(original): OriginalUri, inner: Uri) -> Html<String> {
    // A JS string; `<` escaped so the path cannot close the script.
    let url = serde_json::to_string(&spec_url(&original, &inner))
        .unwrap_or_default()
        .replace('<', "\\u003c");
    Html(SWAGGER_HTML.replace("{{SPEC_URL}}", &url))
}

async fn redoc(OriginalUri(original): OriginalUri, inner: Uri) -> Html<String> {
    let url = crate::text::html_escape(&spec_url(&original, &inner));
    Html(REDOC_HTML.replace("{{SPEC_URL}}", &url))
}

const SWAGGER_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>API Docs</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5.34.0/swagger-ui.css" integrity="sha384-4c3vsE5f+trhFyHrFrLM1fFGUn531NIaC7dcqEWIJdoRQJ5CkJbSuSCwXLpaS1KF" crossorigin="anonymous">
</head>
<body>
<div id="swagger-ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5.34.0/swagger-ui-bundle.js" integrity="sha384-9Td7MKiIr2S83E1kZyyVi8KdJw/S8CpAvu0llLyMeg0xq/hDGiZJfVn0fV4llCmK" crossorigin="anonymous"></script>
<script>
  window.onload = () => {
    window.ui = SwaggerUIBundle({
      url: {{SPEC_URL}},
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
<redoc spec-url="{{SPEC_URL}}"></redoc>
<script src="https://cdn.jsdelivr.net/npm/redoc@2.5.4/bundles/redoc.standalone.js" integrity="sha384-w447zOpYfw/1Tv/5AK9NfHTlQIqE3RVR6KY62jCyy9zNDgO64cMwGGP1Fj0zJVf5" crossorigin="anonymous"></script>
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

    async fn page(app: Router, uri: &str) -> String {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{uri}");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Nested under `/api`, the viewers fetch `/api/openapi.json`, and
    /// every CDN asset is an exact version with an integrity hash.
    #[tokio::test]
    async fn nested_viewers_find_the_spec_and_pin_their_assets() {
        let app = Router::new().nest("/api", openapi_router(sample_spec()));
        for path in ["/api/docs", "/api/redoc"] {
            let body = page(app.clone(), path).await;
            assert!(body.contains("\"/api/openapi.json\""), "{path}: {body}");
            let tags: Vec<&str> = body.split('<').filter(|t| t.contains("https://")).collect();
            assert!(!tags.is_empty(), "{path}");
            for tag in tags {
                assert!(tag.contains(" integrity=\"sha384-"), "{path}: {tag}");
                assert!(tag.contains("crossorigin=\"anonymous\""), "{path}: {tag}");
                assert!(
                    !tag.contains("@5/") && !tag.contains("@2/"),
                    "{path}: {tag}"
                );
            }
        }
        let spec = page(app, "/api/openapi.json").await;
        assert!(spec.contains("\"Demo\""));
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
