//! Integration test for `rustango_admin::router`.
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
#[rustango(table = "admin_user")]
pub struct AdminUser {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 32)]
    name: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
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
