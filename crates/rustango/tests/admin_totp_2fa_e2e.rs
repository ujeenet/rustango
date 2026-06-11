#![cfg(all(feature = "sqlite", feature = "admin", feature = "totp"))]
//! End-to-end HTTP test for admin TOTP two-factor login — issue #367.
//!
//! Builds the real admin router via the public `admin::Builder` API
//! (session auth on) against a seeded SQLite database, then drives
//! `POST /login` through the full axum stack and asserts the challenge
//! gates correctly:
//! - enrolled user, **no code** → rejected (200 re-render, no session);
//! - enrolled user, **wrong code** → rejected;
//! - enrolled user, **correct code** → 303 + session cookie;
//! - **non-enrolled** user → logs in normally (no code required).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rustango::admin::{totp_store, AdminUser, Builder};
use rustango::session::SessionSecret;
use rustango::sql::{sqlx, FetcherPool as _, Pool};
use rustango::totp::TotpSecret;
use tower::ServiceExt as _;

// `Builder::build()` merges the login/protected routes at the router
// root (the admin_prefix only rewrites internal links; the caller nests
// the whole router). So the login route is `/login`, not prefixed.
const PREFIX: &str = "";

async fn seed() -> (Pool, TotpSecret) {
    let p = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("sqlite");
    // AdminUser table (matches `rustango_admin_users` / AdminUser::SCHEMA).
    sqlx::query(
        r#"CREATE TABLE rustango_admin_users (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            username      TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            is_superuser  INTEGER NOT NULL DEFAULT 0,
            active        INTEGER NOT NULL DEFAULT 1,
            created_at    TEXT NOT NULL
        )"#,
    )
    .execute(&p)
    .await
    .unwrap();
    let pool: Pool = p.into();
    totp_store::ensure_table(&pool).await.unwrap();

    // Two users: "alice" (2FA-enrolled, confirmed) and "bob" (no 2FA).
    for (name, su) in [("alice", true), ("bob", false)] {
        let mut u = AdminUser::new_with_password(name, "correct horse", su).unwrap();
        u.insert_pool(&pool).await.unwrap();
    }
    let alice_id = AdminUser::objects()
        .filter("username", "alice")
        .fetch_pool(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;
    let alice_id = *alice_id.get().unwrap();

    let secret = TotpSecret::generate();
    totp_store::start_enrollment(&pool, alice_id, &secret)
        .await
        .unwrap();
    totp_store::confirm(&pool, alice_id).await.unwrap();

    (pool, secret)
}

fn router(pool: Pool) -> axum::Router {
    Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build()
}

/// GET the login page, returning the `rustango_csrf` cookie value (the
/// double-submit token = the cookie value).
async fn fetch_csrf(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{PREFIX}/login"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let set_cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            s.strip_prefix("rustango_csrf=")
                .map(|rest| rest.split(';').next().unwrap_or("").to_owned())
        })
        .expect("csrf cookie issued on GET /login");
    assert!(!set_cookie.is_empty());
    set_cookie
}

/// POST /login with the given credentials + code. Returns
/// `(status, issued_session_cookie)`.
async fn login(
    app: &axum::Router,
    csrf: &str,
    username: &str,
    password: &str,
    totp_code: &str,
) -> (StatusCode, bool) {
    let body = format!(
        "_csrf={csrf}&username={username}&password={password}&totp_code={totp_code}",
        password = urlencoding(password),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/login"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={csrf}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let issued_session = resp.headers().get_all(header::SET_COOKIE).iter().any(|v| {
        v.to_str()
            .map(|s| s.contains("rustango_admin_session="))
            .unwrap_or(false)
    });
    (status, issued_session)
}

fn urlencoding(s: &str) -> String {
    s.replace(' ', "%20")
}

#[tokio::test]
async fn enrolled_user_is_gated_by_the_totp_code() {
    let (pool, secret) = seed().await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;

    // Correct password, NO code → rejected (re-render, no session).
    let (status, sess) = login(&app, &csrf, "alice", "correct horse", "").await;
    assert!(!sess, "no session without a TOTP code: {status}");

    // Correct password, WRONG code → rejected.
    let (_s, sess) = login(&app, &csrf, "alice", "correct horse", "000000").await;
    assert!(!sess, "no session with a wrong TOTP code");

    // Correct password + correct code → session granted (303 redirect).
    let code = rustango::totp::generate(&secret, 30, 6);
    let (status, sess) = login(&app, &csrf, "alice", "correct horse", &code).await;
    assert!(sess, "valid code grants a session (status {status})");
    assert_eq!(status, StatusCode::SEE_OTHER, "success redirects");
}

#[tokio::test]
async fn non_enrolled_user_logs_in_without_a_code() {
    let (pool, _secret) = seed().await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;

    // Bob has no 2FA device — a blank code is fine.
    let (status, sess) = login(&app, &csrf, "bob", "correct horse", "").await;
    assert!(
        sess,
        "non-enrolled user logs in with no code (status {status})"
    );
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn wrong_password_never_reaches_the_totp_step() {
    let (pool, secret) = seed().await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;

    // Even with a valid code, a wrong password fails (no session).
    let code = rustango::totp::generate(&secret, 30, 6);
    let (_s, sess) = login(&app, &csrf, "alice", "wrong", &code).await;
    assert!(!sess, "wrong password is rejected regardless of the code");
}
