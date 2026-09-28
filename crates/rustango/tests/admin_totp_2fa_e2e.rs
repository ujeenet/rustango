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
    // `max_connections(1)`: a `sqlite::memory:` database is per-connection,
    // so a multi-connection pool would hand the router a *different*
    // (empty) DB than the one we seed on. Pinning to one connection keeps
    // the seed + every request on the same in-memory database. (Passes
    // locally either way, but CI's timing can open a second connection.)
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let pool: Pool = p.into();
    // rustango_admin_users from `AdminUser::SCHEMA` (no hand-written DDL).
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    totp_store::ensure_table(&pool).await.unwrap();

    // Two users: "alice" (2FA-enrolled, confirmed) and "bob" (no 2FA).
    for (name, su) in [("alice", true), ("bob", false)] {
        let mut u = AdminUser::new_with_password(name, "correct horse", su).unwrap();
        u.insert_pool(&pool).await.unwrap();
    }
    let alice_id = AdminUser::objects()
        .filter("username", "alice")
        .fetch(&pool)
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

/// #1672 — a code that signed in once cannot sign in again.
#[tokio::test]
async fn a_used_code_cannot_sign_in_again() {
    let (pool, secret) = seed().await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;
    let code = rustango::totp::generate(&secret, 30, 6);
    let (_s, sess) = login(&app, &csrf, "alice", "correct horse", &code).await;
    assert!(sess, "first use signs in");
    let (_s, sess) = login(&app, &csrf, "alice", "correct horse", &code).await;
    assert!(!sess, "a replayed TOTP code granted a second session");
}

/// The session cookie (`name=value`) a successful password login sets.
async fn session_cookie(app: &axum::Router, csrf: &str, username: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/login"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={csrf}"))
                .body(Body::from(format!(
                    "_csrf={csrf}&username={username}&password=correct%20horse&totp_code="
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|v| {
            let s = v.to_str().ok()?;
            s.starts_with("rustango_admin_session=")
                .then(|| s.split(';').next().unwrap_or("").to_owned())
        })
        .expect("session cookie")
}

/// #1672 — the code that confirms enrollment cannot then sign in.
#[tokio::test]
async fn the_enrollment_code_cannot_sign_in() {
    let (pool, _secret) = seed().await;
    // A superuser without 2FA: only superusers reach the admin.
    AdminUser::new_with_password("carol", "correct horse", true)
        .unwrap()
        .insert_pool(&pool)
        .await
        .unwrap();
    let app = router(pool.clone());
    let csrf = fetch_csrf(&app).await;
    let session = session_cookie(&app, &csrf, "carol").await;
    let cookies = format!("rustango_csrf={csrf}; {session}");

    // GET starts enrollment and stores a pending secret.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{PREFIX}/account/totp"))
                .header(header::COOKIE, &cookies)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let carol_id = *AdminUser::objects()
        .filter("username", "carol")
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id
        .get()
        .unwrap();
    let device = totp_store::device(&pool, carol_id)
        .await
        .expect("pending device");
    let secret = TotpSecret::from_base32(&device.secret_base32).unwrap();
    let code = rustango::totp::generate(&secret, 30, 6);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/account/totp"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, &cookies)
                .body(Body::from(format!("_csrf={csrf}&totp_code={code}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        totp_store::confirmed_secret(&pool, carol_id)
            .await
            .is_some(),
        "enrollment confirmed"
    );

    let (_s, sess) = login(&app, &csrf, "carol", "correct horse", &code).await;
    assert!(!sess, "the enrollment code signed in a second time");
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

/// #1695 — a valid pair and valid credentials from a foreign Origin
/// get no session.
#[tokio::test]
async fn login_from_a_foreign_origin_is_refused() {
    let (pool, _secret) = seed().await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/login"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::HOST, "admin.example.com")
                .header(header::ORIGIN, "http://evil.example")
                .header(header::COOKIE, format!("rustango_csrf={csrf}"))
                .body(Body::from(format!(
                    "_csrf={csrf}&username=bob&password=correct%20horse&totp_code="
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let sess = resp.headers().get_all(header::SET_COOKIE).iter().any(|v| {
        v.to_str()
            .is_ok_and(|s| s.contains("rustango_admin_session="))
    });
    assert!(
        !sess,
        "foreign Origin must not get a session ({})",
        resp.status()
    );
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

/// Fresh install: `migrate` creates the TOTP table, so a password login
/// works before anyone has opened the enroll page.
#[tokio::test]
async fn fresh_install_login_works_without_the_enroll_page() {
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite");
    let pool: Pool = p.into();
    let dir = tempfile::tempdir().unwrap();
    rustango::migrate::manage::run_with_writer(
        &pool,
        dir.path(),
        ["migrate".to_owned()],
        &mut Vec::new(),
    )
    .await
    .expect("migrate");
    let mut u = AdminUser::new_with_password("dora", "correct horse", true).unwrap();
    u.insert_pool(&pool).await.unwrap();
    let id = *u.id.get().unwrap();

    let app = router(pool.clone());
    let csrf = fetch_csrf(&app).await;
    let (status, sess) = login(&app, &csrf, "dora", "correct horse", "").await;
    assert!(
        sess,
        "fresh install refused a valid login (status {status})"
    );
    assert!(totp_store::confirmed_secret_checked(&pool, id)
        .await
        .is_ok());
}
