//! Integration test for `rustango::admin::router`.
//!
//! Reads `DATABASE_URL`. If unset, every test returns silently. We boot
//! the router via `tower::ServiceExt::oneshot` (no socket required) and
//! make HTTP requests against it.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rustango::migrate;
use rustango::sql::sqlx;
use rustango::Model;
use sqlx::Row;
use tokio::sync::Mutex;
use tower::ServiceExt;

#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_user", display = "name")]
pub struct AdminUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_post", display = "title")]
pub struct AdminPost {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 200)]
    title: String,
    #[rustango(fk = "admin_user", on = "id")]
    author_id: i64,
}

/// Auto-PK twin of `AdminUser` — exists only to assert that the create
/// form omits the server-assigned `id` column (S7 regression — HTML5
/// `required` on a blank Auto-PK column was silently blocking submit).
#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_widget", display = "label")]
pub struct AdminWidget {
    #[rustango(primary_key)]
    id: rustango::Auto<i64>,
    #[rustango(max_length = 32)]
    label: String,
}

/// Slice 10.2/10.3 fixture: model with `admin(...)` attribute set so we
/// can assert `list_display`, `search_fields`, and `ordering` flow into
/// the rendered list view + executed SQL.
#[derive(Model, Debug, Clone)]
#[rustango(table = "admin_django", display = "name")]
#[rustango(admin(
    list_display = "name, color",
    search_fields = "name",
    list_per_page = 10,
    ordering = "-name",
))]
pub struct AdminDjango {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(max_length = 16)]
    color: String,
    #[rustango(max_length = 200)]
    notes: String,
}

fn live_lock() -> &'static Mutex<()> {
    static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn seed(pool: &sqlx::PgPool) {
    migrate::drop_all(pool).await.unwrap();
    migrate::apply_all(pool).await.unwrap();
    for user in [
        AdminUser {
            id: 1,
            name: "alice".into(),
            age: 30,
            is_active: true,
        },
        AdminUser {
            id: 2,
            name: "bob".into(),
            age: 45,
            is_active: false,
        },
    ] {
        user.insert(pool).await.unwrap();
    }
}

#[tokio::test]
async fn index_lists_registered_models() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("rustango admin"), "missing title: {body}");
    assert!(body.contains("AdminUser"), "missing model name: {body}");
    assert!(
        body.contains("href=\"/admin_user\""),
        "missing link to table: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_renders_seeded_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("AdminUser"), "missing heading: {body}");
    assert!(body.contains("2 rows"), "missing row count: {body}");
    assert!(body.contains("alice"), "missing alice: {body}");
    assert!(body.contains("bob"), "missing bob: {body}");
    // Numeric & boolean rendering
    assert!(body.contains(">30<"), "missing alice age 30: {body}");
    assert!(body.contains(">45<"), "missing bob age 45: {body}");
    assert!(body.contains(">true<"), "missing true: {body}");
    assert!(body.contains(">false<"), "missing false: {body}");
    // PK marker
    assert!(body.contains("(pk)"), "missing pk marker: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_empty_table_says_no_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("No rows"), "missing empty marker: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn table_view_unknown_table_returns_404() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    let app = rustango::admin::router(pool);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/no_such_table")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_string(response).await;
    assert!(
        body.contains("table not found"),
        "missing error message: {body}",
    );
}

#[tokio::test]
async fn html_escapes_user_provided_strings() {
    // A row whose name contains HTML special chars should not be rendered raw.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    AdminUser {
        id: 1,
        name: "<script>".into(),
        age: 30,
        is_active: true,
    }
    .insert(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = body_string(response).await;
    assert!(
        body.contains("&lt;script&gt;"),
        "raw script tag leaked: {body}",
    );
    assert!(
        !body.contains(">script<"),
        "unescaped script element: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================ CRUD forms

fn form_request(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn detail_view_renders_full_row() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("AdminUser #1"), "missing heading: {body}");
    assert!(body.contains("alice"), "missing alice: {body}");
    assert!(body.contains(">30<"), "missing age 30: {body}");
    assert!(body.contains(">true<"), "missing active true: {body}");
    assert!(
        body.contains(r#"href="/admin_user/1/edit""#),
        "missing edit link: {body}",
    );
    assert!(
        body.contains(r#"action="/admin_user/1/delete""#),
        "missing delete form: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn detail_view_unknown_pk_returns_404() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_string(response).await;
    assert!(body.contains("row not found"), "missing 404 body: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn detail_view_unparseable_pk_returns_400() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/not-a-number")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_form_shows_one_input_per_field() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("New AdminUser"), "missing title: {body}");
    assert!(body.contains(r#"name="id""#), "missing id input: {body}");
    assert!(
        body.contains(r#"name="name""#),
        "missing name input: {body}"
    );
    assert!(body.contains(r#"name="age""#), "missing age input: {body}");
    assert!(
        body.contains(r#"name="is_active""#),
        "missing is_active input: {body}",
    );
    // max_length=32 surfaces on the input
    assert!(
        body.contains(r#"maxlength="32""#),
        "missing maxlength: {body}"
    );
    // min/max from age surface on the number input
    assert!(body.contains(r#"min="0""#), "missing min: {body}");
    assert!(body.contains(r#"max="150""#), "missing max: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_form_for_auto_pk_omits_id_input() {
    // S7 regression: the create form for an `Auto<i64>` PK model used
    // to render `<input type="number" name="id" required>`. Empty
    // `id` + `required` made HTML5 silently block form submit. The
    // column is now omitted entirely on create — Postgres' BIGSERIAL
    // DEFAULT fills it via `insert_returning`.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_widget/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        !body.contains(r#"name="id""#),
        "Auto-PK `id` must not render on create form: {body}"
    );
    assert!(
        body.contains(r#"name="label""#),
        "label input still rendered: {body}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_submit_for_auto_pk_assigns_pk_and_redirects() {
    // S7 round-trip: POST to an Auto-PK model without an `id` field —
    // server-assigned PK from `insert_returning`, redirect lands on
    // the new row's detail URL.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(Method::POST, "/admin_widget", "label=auto-pk-test"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    let prefix = "/admin_widget/";
    assert!(
        location.starts_with(prefix),
        "expected /admin_widget/<id>, got `{location}`"
    );
    let pk: i64 = location[prefix.len()..]
        .parse()
        .expect("redirect should include numeric PK");
    assert!(pk > 0, "expected server-assigned positive PK, got `{pk}`");

    let row = sqlx::query("SELECT label FROM admin_widget WHERE id = $1")
        .bind(pk)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<String, _>("label").unwrap(), "auto-pk-test");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_submit_inserts_row_and_redirects() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user",
            "id=42&name=zelda&age=27&is_active=true",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(location, "/admin_user/42");

    // Verify the row landed via direct sqlx.
    let row = sqlx::query("SELECT name, age, is_active FROM admin_user WHERE id = 42")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<String, _>("name").unwrap(), "zelda");
    assert_eq!(row.try_get::<i32, _>("age").unwrap(), 27);
    assert!(row.try_get::<bool, _>("is_active").unwrap());

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_submit_unchecked_checkbox_is_false() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    // Browser omits unchecked checkbox keys entirely.
    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user",
            "id=43&name=quiet&age=20",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let row = sqlx::query("SELECT is_active FROM admin_user WHERE id = 43")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!row.try_get::<bool, _>("is_active").unwrap());

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_submit_validation_error_re_renders_form() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    // age=200 violates the max=150 bound.
    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user",
            "id=44&name=oldie&age=200&is_active=true",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("out of range"),
        "missing validation error: {body}",
    );
    // The submitted values should be preserved in the re-rendered form.
    assert!(body.contains(r#"value="oldie""#), "lost name: {body}");
    assert!(body.contains(r#"value="200""#), "lost age: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn edit_form_pre_populates_with_pk_readonly() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/1/edit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("Edit AdminUser"), "missing title: {body}");
    assert!(body.contains(r#"value="1""#), "missing pk value: {body}");
    assert!(
        body.contains(r#"value="alice""#),
        "missing prefilled name: {body}"
    );
    assert!(
        body.contains(r#"value="30""#),
        "missing prefilled age: {body}"
    );
    // PK input should be readonly in edit mode.
    assert!(
        body.contains("name=\"id\"") && body.contains("readonly"),
        "id should be readonly: {body}",
    );
    // is_active was true → checkbox checked.
    assert!(body.contains("checked"), "missing checkbox check: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn edit_submit_updates_row_and_redirects() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user/1",
            "id=1&name=ALICE&age=31&is_active=true",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/admin_user/1",
    );

    let row = sqlx::query("SELECT name, age FROM admin_user WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.try_get::<String, _>("name").unwrap(), "ALICE");
    assert_eq!(row.try_get::<i32, _>("age").unwrap(), 31);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn delete_submit_removes_row_and_redirects() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(form_request(Method::POST, "/admin_user/1/delete", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/admin_user",
    );

    let count: i64 = sqlx::query("SELECT COUNT(*) FROM admin_user")
        .fetch_one(&pool)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    assert_eq!(count, 1); // bob remains

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn list_view_has_new_link_and_per_row_view_link() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        body.contains(r#"href="/admin_user/new""#),
        "missing + new: {body}"
    );
    assert!(
        body.contains(r#"href="/admin_user/1""#),
        "missing alice link: {body}"
    );
    assert!(
        body.contains(r#"href="/admin_user/2""#),
        "missing bob link: {body}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================ pagination

async fn seed_n(pool: &sqlx::PgPool, n: i64) {
    migrate::drop_all(pool).await.unwrap();
    migrate::apply_all(pool).await.unwrap();
    for i in 1..=n {
        AdminUser {
            id: i,
            name: format!("u{i}"),
            age: 30,
            is_active: true,
        }
        .insert(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn list_view_pages_at_50_rows_per_page() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_n(&pool, 60).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(body.contains("60 rows"), "missing total: {body}");
    assert!(body.contains("page 1 of 2"), "missing pager: {body}");
    assert!(
        body.contains(r#"href="/admin_user?page=2""#),
        "missing next link: {body}",
    );
    assert!(
        body.contains(r#"href="/admin_user/50""#),
        "missing row 50: {body}",
    );
    assert!(
        !body.contains(r#"href="/admin_user/51""#),
        "row 51 leaked onto page 1: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn list_view_page_2_shows_remaining_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_n(&pool, 60).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?page=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(body.contains("page 2 of 2"), "missing pager: {body}");
    assert!(
        body.contains(r#"href="/admin_user/51""#),
        "missing row 51: {body}",
    );
    assert!(
        body.contains(r#"href="/admin_user/60""#),
        "missing row 60: {body}",
    );
    assert!(
        body.contains(r#"href="/admin_user?page=1""#),
        "missing prev link: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn list_view_no_pager_when_under_one_page() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_n(&pool, 5).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(body.contains("5 rows"), "missing total: {body}");
    assert!(!body.contains("page 1 of"), "should not show pager: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================ basic auth

#[tokio::test]
async fn unprotected_router_lets_requests_through() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn protected_router_returns_401_without_credentials() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::protect_with_basic_auth(
        rustango::admin::router(pool.clone()),
        "admin",
        "secret",
    );
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let www_authenticate = response
        .headers()
        .get(header::WWW_AUTHENTICATE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        www_authenticate.contains("Basic"),
        "missing Basic challenge: {www_authenticate}",
    );
    assert!(
        www_authenticate.contains("rustango admin"),
        "missing realm: {www_authenticate}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn protected_router_rejects_wrong_credentials() {
    use base64::Engine;
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::protect_with_basic_auth(
        rustango::admin::router(pool.clone()),
        "admin",
        "secret",
    );
    let creds = base64::engine::general_purpose::STANDARD.encode(b"admin:wrong");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::AUTHORIZATION, format!("Basic {creds}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn protected_router_accepts_correct_credentials() {
    use base64::Engine;
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::protect_with_basic_auth(
        rustango::admin::router(pool.clone()),
        "admin",
        "secret",
    );
    let creds = base64::engine::general_purpose::STANDARD.encode(b"admin:secret");
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::AUTHORIZATION, format!("Basic {creds}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("rustango admin"), "missing title: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn protected_router_rejects_malformed_authorization() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };

    let app = rustango::admin::protect_with_basic_auth(
        rustango::admin::router(pool.clone()),
        "admin",
        "secret",
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::AUTHORIZATION, "Basic !!!not-base64!!!")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ============================================================ permissions

#[tokio::test]
async fn show_only_filters_index_to_listed_tables() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    // Allowlist a non-existent table → admin_user is hidden from the index.
    let app = rustango::admin::Builder::new(pool.clone())
        .show_only(["nope"])
        .build();
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        !body.contains("AdminUser"),
        "AdminUser leaked through allowlist: {body}",
    );
    assert!(
        body.contains("No models registered"),
        "expected empty index: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn show_only_returns_404_for_filtered_table() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .show_only(["nope"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn show_only_admits_listed_tables() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .show_only(["admin_user"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("AdminUser"), "missing model name: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_hides_new_button_in_list() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        !body.contains(r#"href="/admin_user/new""#),
        "new link leaked on read-only table: {body}",
    );
    assert!(
        body.contains("read-only"),
        "missing read-only marker: {body}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_hides_edit_and_delete_on_detail() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        !body.contains(r#"href="/admin_user/1/edit""#),
        "edit link leaked: {body}",
    );
    assert!(
        !body.contains(r#"action="/admin_user/1/delete""#),
        "delete form leaked: {body}",
    );
    assert!(body.contains("read-only"), "missing read-only note: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_blocks_create_with_403() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user",
            "id=99&name=z&age=20&is_active=true",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_string(response).await;
    assert!(
        body.contains("read-only"),
        "missing read-only error: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_blocks_update_with_403() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(form_request(
            Method::POST,
            "/admin_user/1",
            "id=1&name=ALICE&age=30&is_active=true",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_blocks_delete_with_403() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(form_request(Method::POST, "/admin_user/1/delete", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn read_only_blocks_new_form_with_403() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .read_only(["admin_user"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn show_only_and_read_only_compose() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    // Both: visible AND read-only.
    let app = rustango::admin::Builder::new(pool.clone())
        .show_only(["admin_user"])
        .read_only(["admin_user"])
        .build();

    // Visible (200) but new is forbidden (403).
    let view = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view.status(), StatusCode::OK);

    let new_form = app
        .oneshot(
            Request::builder()
                .uri("/admin_user/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(new_form.status(), StatusCode::FORBIDDEN);

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================ FK display

async fn seed_blog(pool: &sqlx::PgPool) {
    migrate::drop_all(pool).await.unwrap();
    migrate::apply_all(pool).await.unwrap();
    AdminUser {
        id: 1,
        name: "alice".into(),
        age: 30,
        is_active: true,
    }
    .insert(pool)
    .await
    .unwrap();
    AdminUser {
        id: 2,
        name: "bob".into(),
        age: 45,
        is_active: false,
    }
    .insert(pool)
    .await
    .unwrap();
    AdminPost {
        id: 10,
        title: "hello".into(),
        author_id: 1,
    }
    .insert(pool)
    .await
    .unwrap();
    AdminPost {
        id: 11,
        title: "second".into(),
        author_id: 2,
    }
    .insert(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn list_renders_fk_as_link_to_display_value() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_blog(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;

    // Each post row should link author_id to /admin_user/<id> with the
    // displayed alice/bob — not the raw integer.
    assert!(
        body.contains(r#"<a href="/admin_user/1">alice</a>"#),
        "post 10 should link to alice: {body}",
    );
    assert!(
        body.contains(r#"<a href="/admin_user/2">bob</a>"#),
        "post 11 should link to bob: {body}",
    );
    // Raw integer should NOT appear in the FK cell.
    assert!(
        !body.contains("<td>1</td>") || body.contains("<td>1</td><td><a"),
        "raw author_id 1 leaked: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn detail_renders_fk_as_link_to_display_value() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_blog(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_post/10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains(r#"<dd><a href="/admin_user/1">alice</a></dd>"#),
        "detail should show alice link: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn fk_falls_back_to_raw_when_target_hidden() {
    // If admin_user is filtered out via show_only, post.author_id has
    // no resolvable display — should render the raw value, not crash.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_blog(&pool).await;

    let app = rustango::admin::Builder::new(pool.clone())
        .show_only(["admin_post"])
        .build();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    // No link to /admin_user
    assert!(
        !body.contains(r#"href="/admin_user/"#),
        "FK link leaked despite hidden target: {body}",
    );
    // Raw author_id renders.
    assert!(
        body.contains(">1<") || body.contains(">2<"),
        "raw author_id missing: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn fk_falls_back_to_raw_when_target_row_missing() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();

    // Insert a post with no matching author. We bypass FK enforcement
    // by inserting via raw SQL after dropping the constraint.
    sqlx::query("ALTER TABLE admin_post DROP CONSTRAINT admin_post_author_id_fkey")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO admin_post (id, title, author_id) VALUES (99, 'orphan', 999)")
        .execute(&pool)
        .await
        .unwrap();

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_post")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    // Should render the raw 999 (no link), not crash and not show alice.
    assert!(
        !body.contains(r#"<a href="/admin_user/999""#),
        "should not link to missing target: {body}",
    );
    assert!(body.contains(">999<"), "raw 999 should render: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================ search + filters

#[tokio::test]
async fn list_view_renders_search_box_when_searchable_field_exists() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        body.contains(r#"<input type="search" name="q""#),
        "missing search box: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn search_filters_to_matching_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?q=ali")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("alice"), "alice should match: {body}");
    assert!(!body.contains("bob"), "bob should not match: {body}");
    // Active filter badge surfaces the query
    assert!(
        body.contains("filtered by:"),
        "missing active filters note: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn search_is_case_insensitive() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?q=ALICE")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(
        body.contains("alice"),
        "case-insensitive match failed: {body}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn field_filter_keeps_only_matching_rows() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?is_active=false")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(body.contains("bob"), "bob should match: {body}");
    assert!(!body.contains("alice"), "alice should not match: {body}");
    assert!(
        body.contains("<code>is_active=false</code>"),
        "missing filter badge: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn unknown_filter_field_is_silently_ignored() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?nope=42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Bad URL params should not 500 — we just render the unfiltered view.
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("alice"), "missing alice: {body}");
    assert!(body.contains("bob"), "missing bob: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn search_and_filter_compose() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    // Add a third row so search + filter can each independently narrow.
    seed(&pool).await;
    AdminUser {
        id: 3,
        name: "alfred".into(),
        age: 50,
        is_active: false,
    }
    .insert(&pool)
    .await
    .unwrap();

    let app = rustango::admin::router(pool.clone());
    // ?q=al matches alice + alfred; ?is_active=true narrows to alice.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?q=al&is_active=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    assert!(body.contains("alice"), "alice should match: {body}");
    assert!(
        !body.contains("alfred"),
        "alfred should be filtered out: {body}"
    );
    assert!(!body.contains("bob"), "bob should not match: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn pager_links_preserve_search_and_filters() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    migrate::drop_all(&pool).await.unwrap();
    migrate::apply_all(&pool).await.unwrap();
    // 60 active alphas + 60 inactive betas → exactly 60 active rows = 2 pages
    for i in 1..=60 {
        AdminUser {
            id: i,
            name: format!("alpha{i}"),
            age: 25,
            is_active: true,
        }
        .insert(&pool)
        .await
        .unwrap();
    }
    for i in 100..160 {
        AdminUser {
            id: i,
            name: format!("beta{i}"),
            age: 25,
            is_active: false,
        }
        .insert(&pool)
        .await
        .unwrap();
    }

    let app = rustango::admin::router(pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin_user?is_active=true&q=alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(response).await;
    // Next link must carry both q and is_active forward
    assert!(
        body.contains("page=2") && body.contains("q=alpha") && body.contains("is_active=true"),
        "pager dropped filters: {body}",
    );

    migrate::drop_all(&pool).await.unwrap();
}

// ============================================================== SLICE 10.2 + 10.3
//
// Per-model `#[rustango(admin(...))]` attribute drives `list_display`,
// `search_fields`, `list_per_page`, and `ordering` on the list view.

async fn seed_admin_django(pool: &sqlx::PgPool) {
    migrate::drop_all(pool).await.unwrap();
    migrate::apply_all(pool).await.unwrap();
    for (id, name, color, notes) in [
        (1_i64, "alpha", "red", "first row"),
        (2, "bravo", "green", "second"),
        (3, "charlie", "blue", "third"),
    ] {
        AdminDjango {
            id,
            name: name.into(),
            color: color.into(),
            notes: notes.into(),
        }
        .insert(pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn list_display_attr_renders_only_named_columns() {
    // `admin(list_display = "name, color")` — `id` and `notes` must NOT
    // appear in the list view's <thead>. `name` and `color` must.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_admin_django(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_django")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    let head = body
        .split("<tbody")
        .next()
        .expect("tbody marker present");
    assert!(head.contains(">name<"), "name column missing: {head}");
    assert!(head.contains(">color<"), "color column missing: {head}");
    assert!(
        !head.contains(">id<"),
        "id should be hidden by list_display: {head}"
    );
    assert!(
        !head.contains(">notes<"),
        "notes should be hidden by list_display: {head}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn search_fields_attr_filters_by_named_columns() {
    // `admin(search_fields = "name")` — `?q=alpha` must match the
    // `name=alpha` row and only that row, not `notes` or `color`.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_admin_django(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_django?q=alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(body.contains(">alpha<"), "alpha row missing: {body}");
    assert!(!body.contains(">bravo<"), "bravo leaked: {body}");
    assert!(!body.contains(">charlie<"), "charlie leaked: {body}");

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn ordering_attr_drives_sort_order() {
    // `admin(ordering = "-name")` — rows should come back in reverse
    // alphabetic order (charlie, bravo, alpha).
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed_admin_django(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_django")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    let charlie = body.find(">charlie<").expect("charlie row present");
    let bravo = body.find(">bravo<").expect("bravo row present");
    let alpha = body.find(">alpha<").expect("alpha row present");
    assert!(
        charlie < bravo && bravo < alpha,
        "expected reverse-name order; got positions charlie={charlie} bravo={bravo} alpha={alpha}"
    );

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn no_admin_attr_falls_back_to_all_scalar_fields() {
    // Sanity: `AdminUser` has no `admin(...)` attribute — every scalar
    // column (id, name, age, is_active) must render as a column.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    let head = body
        .split("<tbody")
        .next()
        .expect("tbody marker present");
    for col in ["id", "name", "age", "is_active"] {
        assert!(
            head.contains(&format!(">{col}")),
            "expected default-list_display column `{col}`: {head}"
        );
    }

    migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn sidebar_renders_on_admin_pages() {
    // Slice 10.1 regression: every admin page now embeds the sidebar
    // partial. Assert the sidebar's `<aside class="sidebar">` shell
    // appears on the index, list, and detail routes, and that the
    // active table gets `class="active"` highlighted.
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        return;
    };
    seed(&pool).await;

    let app = rustango::admin::router(pool.clone());
    for path in ["/", "/admin_user", "/admin_user/1"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_string(resp).await;
        assert!(
            body.contains(r#"<aside class="sidebar">"#),
            "sidebar missing on `{path}`: {body}"
        );
    }
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin_user")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(
        body.contains(r#"href="/admin_user" class="active""#),
        "active sidebar link not highlighted: {body}"
    );

    migrate::drop_all(&pool).await.unwrap();
}
