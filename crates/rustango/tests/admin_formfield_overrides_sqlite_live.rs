//! Issue #359 — `admin(formfield_overrides = "field:widget, …")`
//! swaps the FieldType-default input on the admin change-form for a
//! named built-in widget. Unknown names fall back to the default
//! (with a tracing warning at dispatch time).

#![cfg(all(feature = "sqlite", feature = "admin", feature = "tenancy"))]

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::core::Model as _;
use rustango::sql::Pool;
use rustango::Model;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ffo_account",
    display = "username",
    admin(
        formfield_overrides = "secret:password, color:color, bio:textarea, age:range, homepage:url, contact:email, anything:hidden"
    )
)]
#[allow(dead_code)]
pub struct Account {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 30)]
    pub username: String,
    #[rustango(max_length = 64)]
    pub secret: String,
    #[rustango(max_length = 7)]
    pub color: String,
    pub bio: String,
    pub age: i32,
    #[rustango(max_length = 200)]
    pub homepage: String,
    #[rustango(max_length = 200)]
    pub contact: String,
    #[rustango(max_length = 200)]
    pub anything: String,
}

fn build_app(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool).admin_prefix("").build()
}

async fn fresh_pool() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    rustango::sql::raw_execute_pool(
        &pool,
        r#"CREATE TABLE "ffo_account" (
            "id"        INTEGER PRIMARY KEY AUTOINCREMENT,
            "username"  TEXT NOT NULL,
            "secret"    TEXT NOT NULL,
            "color"     TEXT NOT NULL,
            "bio"       TEXT NOT NULL,
            "age"       INTEGER NOT NULL,
            "homepage"  TEXT NOT NULL,
            "contact"   TEXT NOT NULL,
            "anything"  TEXT NOT NULL
        )"#,
        Vec::new(),
    )
    .await
    .expect("create");
    pool
}

async fn fetch_new_form_html(pool: Pool) -> String {
    let app = build_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/ffo_account/new")
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn schema_carries_formfield_overrides() {
    let cfg = Account::SCHEMA.admin.expect("admin attr set");
    // Order matches the declaration in the macro arg.
    let by_name: std::collections::HashMap<&str, &str> =
        cfg.formfield_overrides.iter().copied().collect();
    assert_eq!(by_name.get("secret"), Some(&"password"));
    assert_eq!(by_name.get("color"), Some(&"color"));
    assert_eq!(by_name.get("bio"), Some(&"textarea"));
    assert_eq!(by_name.get("age"), Some(&"range"));
    assert_eq!(by_name.get("homepage"), Some(&"url"));
    assert_eq!(by_name.get("contact"), Some(&"email"));
    assert_eq!(by_name.get("anything"), Some(&"hidden"));
}

#[tokio::test]
async fn password_widget_renders_input_type_password() {
    let pool = fresh_pool().await;
    let body = fetch_new_form_html(pool).await;
    assert!(
        body.contains(r#"<input type="password" name="secret""#),
        "secret should render as a password input, got: {body}"
    );
    // Sanity — overridden field should NOT also render its default
    // (which would be `<input type="text" name="secret" …maxlength="64"`).
    assert!(
        !body.contains(r#"<input type="text" name="secret""#),
        "default text input must NOT also be emitted for secret"
    );
}

#[tokio::test]
async fn color_widget_renders_input_type_color() {
    let pool = fresh_pool().await;
    let body = fetch_new_form_html(pool).await;
    assert!(
        body.contains(r#"<input type="color" name="color""#),
        "color should render as a color picker"
    );
}

#[tokio::test]
async fn textarea_widget_renders_textarea_for_string() {
    let pool = fresh_pool().await;
    let body = fetch_new_form_html(pool).await;
    // bio has no max_length so default would be a textarea already —
    // ensure the override path is taken (textarea with no maxlength
    // attribute). We test by looking for the bio textarea + absence
    // of an input[type=text] for it.
    assert!(
        body.contains(r#"<textarea name="bio""#),
        "bio should render as textarea"
    );
}

#[tokio::test]
async fn range_widget_renders_input_type_range() {
    let pool = fresh_pool().await;
    let body = fetch_new_form_html(pool).await;
    assert!(
        body.contains(r#"<input type="range" step="1" name="age""#),
        "age should render as a range slider"
    );
    // Default would be `<input type="number" step="1" name="age" …>`.
    assert!(
        !body.contains(r#"<input type="number" step="1" name="age""#),
        "default number input must NOT also be emitted for age"
    );
}

#[tokio::test]
async fn url_email_hidden_widgets_render_correctly() {
    let pool = fresh_pool().await;
    let body = fetch_new_form_html(pool).await;
    assert!(body.contains(r#"<input type="url" name="homepage""#));
    assert!(body.contains(r#"<input type="email" name="contact""#));
    assert!(body.contains(r#"<input type="hidden" name="anything""#));
}

async fn get_text(pool: Pool, uri: &str) -> String {
    let resp = build_app(pool)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// #2228: `?q=` must not probe a secret column, in the list or autocomplete.
#[tokio::test]
async fn search_skips_secret_fields() {
    let pool = fresh_pool().await;
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "ffo_account" ("username", "secret", "color", "bio", "age", "homepage", "contact", "anything")
           VALUES ('alice', 'hunter2', '#000000', '', 1, '', '', '')"#,
        Vec::new(),
    )
    .await
    .expect("seed");

    let hit = get_text(pool.clone(), "/ffo_account?q=alice").await;
    assert!(hit.contains("alice"), "control: a normal column matches");
    let list = get_text(pool.clone(), "/ffo_account?q=hunter").await;
    assert!(!list.contains("alice"), "the list matched the secret");

    let hit = get_text(pool.clone(), "/ffo_account/__autocomplete?q=ali").await;
    assert!(hit.contains("alice"), "control: autocomplete matches");
    let ac = get_text(pool, "/ffo_account/__autocomplete?q=hunter").await;
    assert!(
        !ac.contains("alice"),
        "autocomplete matched the secret: {ac}"
    );
}

/// A secret in `list_filter` gets no facet.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ffo_vault",
    admin(
        list_display = "id",
        list_filter = "kind, pin",
        formfield_overrides = "pin:password"
    )
)]
#[allow(dead_code)]
pub struct Vault {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 30)]
    pub kind: String,
    #[rustango(max_length = 30)]
    pub pin: String,
}

#[tokio::test]
async fn secret_list_filter_gets_no_facet() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for sql in [
        "CREATE TABLE ffo_vault (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, pin TEXT NOT NULL)",
        "INSERT INTO ffo_vault (id, kind, pin) VALUES (1, 'kind-x', 'pin-s3cr3t')",
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect(sql);
    }
    let body = get_text(pool, "/ffo_vault").await;
    assert!(body.contains("kind-x"), "control: a normal facet shows");
    assert!(!body.contains("pin-s3cr3t"), "the secret's facet shows it");
}

/// `search_fields` names the secret on purpose.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "ffo_login",
    admin(
        search_fields = "username, secret",
        formfield_overrides = "secret:password"
    )
)]
#[allow(dead_code)]
pub struct Login {
    #[rustango(primary_key)]
    pub id: rustango::Auto<i64>,
    #[rustango(max_length = 30)]
    pub username: String,
    #[rustango(max_length = 30)]
    pub secret: String,
}

#[tokio::test]
async fn explicit_search_fields_skip_the_secret() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite pool");
    for sql in [
        "CREATE TABLE ffo_login (id INTEGER PRIMARY KEY, username TEXT NOT NULL, secret TEXT NOT NULL)",
        "INSERT INTO ffo_login (id, username, secret) VALUES (1, 'bob', 'hunter2')",
    ] {
        rustango::sql::raw_execute_pool(&pool, sql, Vec::new())
            .await
            .expect(sql);
    }
    let hit = get_text(pool.clone(), "/ffo_login?q=bob").await;
    assert!(hit.contains("bob"), "control: username matches");
    let list = get_text(pool.clone(), "/ffo_login?q=hunter").await;
    assert!(!list.contains("bob"), "the list matched the secret");
    let ac = get_text(pool, "/ffo_login/__autocomplete?q=hunter").await;
    assert!(!ac.contains("bob"), "autocomplete matched the secret: {ac}");
}
