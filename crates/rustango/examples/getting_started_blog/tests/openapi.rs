//! Backing test for `docs/openapi.md` — generate an OpenAPI 3.1 spec from this
//! example's serializer + viewset, then serve it. Pure (no DB).
//!
//! Run: `cargo test -p getting_started_blog --test openapi`

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use getting_started_blog::blog::models::Post;
use getting_started_blog::post_serializer::PostSerializer;
use rustango::core::Model; // brings `Post::SCHEMA` into scope
use rustango::openapi::router::openapi_router;
use rustango::openapi::{OpenApiSchema, OpenApiSpec, SecurityScheme};
use rustango::viewset::ViewSet;
use tower::ServiceExt;

/// Build a full spec: serializer-derived schema + viewset-derived CRUD paths.
fn build_spec() -> OpenApiSpec {
    // 1. The component schema is generated from the `#[derive(Serializer)]`
    //    type — no hand-written JSON Schema.
    let mut spec = OpenApiSpec::new("Blog API", "1.0.0")
        .description("Demo of rustango's OpenAPI generation")
        .server("https://api.example.com", "Production")
        .add_security_scheme("bearerAuth", SecurityScheme::bearer("JWT"))
        .require_security("bearerAuth", [])
        .add_schema("Post", PostSerializer::openapi_schema());

    // 2. The CRUD path items are generated from the ViewSet (list/create/
    //    retrieve/update/patch/delete), including pagination + filter params.
    let posts = ViewSet::for_model(Post::SCHEMA)
        .filter_fields(&["author_id", "status"])
        .search_fields(&["title", "body"]);
    for (path, item) in posts.openapi_paths("/api/posts", "Post") {
        spec = spec.add_path(path, item);
    }
    spec
}

#[test]
fn serializer_schema_reflects_the_api_shape() {
    // The schema uses API field names: `content` is the serializer's rename of
    // the model's `body` column (`#[serializer(source = "body")]`).
    let v = serde_json::to_value(PostSerializer::openapi_schema()).unwrap();
    assert_eq!(v["type"], "object");
    assert_eq!(v["properties"]["id"]["type"], "integer");
    assert_eq!(v["properties"]["title"]["type"], "string");
    assert_eq!(v["properties"]["content"]["type"], "string");
}

#[test]
fn viewset_generates_crud_paths_referencing_the_schema() {
    let v: serde_json::Value = serde_json::from_str(&build_spec().to_json()).unwrap();

    // Collection route: list (GET) + create (POST).
    assert!(v["paths"]["/api/posts"]["get"].is_object());
    assert!(v["paths"]["/api/posts"]["post"].is_object());
    // Item route: retrieve + delete (and put/patch).
    assert!(v["paths"]["/api/posts/{pk}"]["get"].is_object());
    assert!(v["paths"]["/api/posts/{pk}"]["delete"].is_object());

    // The create body references the registered component schema.
    assert_eq!(
        v["paths"]["/api/posts"]["post"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/Post"
    );

    // The configured filter + search surface as query parameters.
    let params: Vec<String> = v["paths"]["/api/posts"]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(params.contains(&"author_id".to_owned()), "filter param");
    assert!(params.contains(&"search".to_owned()), "search param");
}

#[tokio::test]
async fn router_serves_spec_and_swagger_ui() {
    let app = openapi_router(build_spec());

    // GET /openapi.json → the serialized 3.1 spec.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["openapi"], "3.1.0");
    assert_eq!(v["info"]["title"], "Blog API");
    assert_eq!(v["components"]["schemas"]["Post"]["type"], "object");
    assert_eq!(
        v["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );

    // GET /docs → the Swagger UI shell that loads /openapi.json.
    let resp = app
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
