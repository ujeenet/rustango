//! Cookbook Chapter 9b — `ViewSet::serializer::<S>()` wiring.
//!
//! When set, list / retrieve / create / update responses run every
//! row through `S::from_model` + `to_value` instead of the default
//! field-level projection. SerializerMethodField / read_only /
//! source / nested / many overrides all apply uniformly across the
//! API.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter09b_viewset_serializer -- --test-threads=1`

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use cookbook_blog::apps::blog::models::Author;
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::serializer::ModelSerializer;
use rustango::sql::{sqlx, Auto};
use rustango::viewset::ViewSet;
use rustango::Serializer;
use tower::ServiceExt;

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn pool() -> Option<sqlx::PgPool> {
    Some(sqlx::PgPool::connect(&url()?).await.expect("connect"))
}

async fn fresh_author_table(pool: &sqlx::PgPool) {
    sqlx::query("DROP TABLE IF EXISTS cookbook_author CASCADE")
        .execute(pool).await.unwrap();
    sqlx::query(
        r#"CREATE TABLE cookbook_author (
            id BIGSERIAL PRIMARY KEY,
            name VARCHAR(80) NOT NULL,
            email VARCHAR(200) NOT NULL UNIQUE,
            bio VARCHAR(500) NULL,
            joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"#,
    ).execute(pool).await.unwrap();
}

/// Custom serializer with shape-shifting overrides — DRF parity hit list.
#[derive(Serializer, serde::Deserialize, Default, Debug)]
#[serializer(model = Author)]
pub struct AuthorPublic {
    pub id: Auto<i64>,
    pub name: String,
    /// Renamed in JSON output.
    #[serializer(source = "email")]
    pub contact_email: String,
    /// Computed via SerializerMethodField.
    #[serializer(method = "first_letter")]
    pub initial: String,
    /// Hidden from output but writable.
    #[serializer(write_only)]
    pub admin_secret: String,
}

impl AuthorPublic {
    fn first_letter(model: &Author) -> String {
        model.name.chars().next().map(|c| c.to_string()).unwrap_or_default()
    }
}

async fn json_request(
    router: axum::Router, method: Method, uri: &str, body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(s) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(s.to_owned())
        }
        None => Body::empty(),
    };
    let resp = router.oneshot(req.body(body).unwrap()).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn router_with_serializer(pool: sqlx::PgPool) -> axum::Router {
    ViewSet::for_model(Author::SCHEMA)
        .serializer::<AuthorPublic>()
        .ordering(&[("id", false)])
        .router("/api", pool)
}

fn router_default(pool: sqlx::PgPool) -> axum::Router {
    ViewSet::for_model(Author::SCHEMA)
        .ordering(&[("id", false)])
        .router("/api", pool)
}

// §9b.1 — list responses route through the serializer.
//   - `email` becomes `contact_email` (source rename)
//   - `initial` appears (SerializerMethodField)
//   - `admin_secret` is absent (write_only)
#[tokio::test]
async fn list_response_uses_serializer_when_set() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "ada".into(),
        email: "ada@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();

    // With serializer wired.
    let (status, body) = json_request(router_with_serializer(pool.clone()), Method::GET, "/api", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = &body["results"][0];
    assert_eq!(row["name"], "ada");
    assert_eq!(row["contact_email"], "ada@example.com",
        "source = \"email\" rename — must show as contact_email");
    assert!(row.get("email").is_none(),
        "raw model field name must NOT leak when serializer renames");
    assert_eq!(row["initial"], "a", "method = \"first_letter\" computed value");
    assert!(row.get("admin_secret").is_none(),
        "write_only field must not appear in JSON output");
    assert!(row.get("bio").is_none(),
        "fields not in serializer must not appear");
}

// §9b.2 — without serializer, default field-level projection still works.
#[tokio::test]
async fn list_response_uses_field_projection_when_no_serializer() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "bob".into(),
        email: "bob@example.com".into(),
        bio: Some("hi".into()),
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();

    let (status, body) = json_request(router_default(pool.clone()), Method::GET, "/api", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = &body["results"][0];
    assert_eq!(row["name"], "bob");
    // Default shape carries the model's field names directly.
    assert_eq!(row["email"], "bob@example.com");
    assert_eq!(row["bio"], "hi");
    // No serializer-only computed fields.
    assert!(row.get("initial").is_none());
    assert!(row.get("contact_email").is_none());
}

// §9b.3 — retrieve (`GET /api/{id}`) routes through the same renderer.
#[tokio::test]
async fn retrieve_response_uses_serializer_when_set() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let mut a = Author {
        id: Auto::Unset,
        name: "carol".into(),
        email: "carol@example.com".into(),
        bio: None,
        joined_at: Auto::Unset,
    };
    a.save(&pool).await.unwrap();
    let id = match a.id { Auto::Set(v) => v, _ => unreachable!() };

    let (status, body) = json_request(
        router_with_serializer(pool), Method::GET, &format!("/api/{id}"), None,
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["contact_email"], "carol@example.com");
    assert_eq!(body["initial"], "c");
    assert!(body.get("admin_secret").is_none());
}

// §9b.4 — create (POST /api) returns the post-INSERT row through the
// serializer too. This catches the `fetch_by_pk` plumbing that runs
// after a successful create.
#[tokio::test]
async fn create_response_uses_serializer_when_set() {
    let Some(pool) = pool().await else { return };
    fresh_author_table(&pool).await;

    let payload = r#"{"name": "dave", "email": "dave@example.com"}"#;
    let (status, body) = json_request(
        router_with_serializer(pool), Method::POST, "/api", Some(payload),
    ).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "create returned {status}; body: {body}"
    );
    assert_eq!(body["contact_email"], "dave@example.com");
    assert_eq!(body["initial"], "d");
    assert!(body.get("admin_secret").is_none());
    // Even when the user posts admin_secret as input, the response (which
    // routes through the serializer's to_value) excludes it.
}

#[allow(dead_code)]
fn _smoke_serializer_writable_fields() {
    // Compile-only check that AuthorPublic exposes name + contact_email +
    // admin_secret as writable (DRF parity smoke).
    let writable = AuthorPublic::writable_fields();
    assert!(writable.contains(&"name"));
    assert!(writable.contains(&"contact_email"));
    assert!(writable.contains(&"admin_secret"));
}
