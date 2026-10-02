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

/// The session cookie (`name=value`) a successful login sets.
async fn session_cookie(app: &axum::Router, csrf: &str, username: &str, code: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/login"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={csrf}"))
                .body(Body::from(format!(
                    "_csrf={csrf}&username={username}&password=correct%20horse&totp_code={code}"
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
    let session = session_cookie(&app, &csrf, "carol", "").await;
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
        totp_store::confirmed_secret_checked(&pool, carol_id)
            .await
            .unwrap()
            .is_some(),
        "enrollment confirmed"
    );

    let (_s, sess) = login(&app, &csrf, "carol", "correct horse", &code).await;
    assert!(!sess, "the enrollment code signed in a second time");
}

/// A superuser with a confirmed device. Own user per test: the login
/// lock is process-global.
async fn enrolled_user(pool: &Pool, name: &str) -> (i64, TotpSecret) {
    let mut u = AdminUser::new_with_password(name, "correct horse", true).unwrap();
    u.insert_pool(pool).await.unwrap();
    let id = *u.id.get().unwrap();
    let secret = TotpSecret::generate();
    totp_store::start_enrollment(pool, id, &secret)
        .await
        .unwrap();
    totp_store::confirm(pool, id).await.unwrap();
    (id, secret)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// GET (no body) or POST `form` to the enroll page; returns the HTML.
async fn totp_page(app: &axum::Router, cookies: &str, form: Option<String>) -> String {
    let req = Request::builder()
        .uri(format!("{PREFIX}/account/totp"))
        .header(header::COOKIE, cookies);
    let req = match form {
        Some(f) => req
            .method("POST")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(f)),
        None => req.body(Body::empty()),
    };
    let resp = app.clone().oneshot(req.unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// #1776 — a session alone cannot start a re-enroll; a fresh code can.
#[tokio::test]
async fn reenroll_needs_a_current_code() {
    let (pool, _alice) = seed().await;
    let (id, secret) = enrolled_user(&pool, "erin1776").await;
    let app = router(pool.clone());
    let csrf = fetch_csrf(&app).await;
    let now = unix_now();
    let code = rustango::totp::generate_at(&secret, now, 30, 6);
    let session = session_cookie(&app, &csrf, "erin1776", &code).await;
    let cookies = format!("rustango_csrf={csrf}; {session}");

    for body in [
        format!("_csrf={csrf}&reset=1"),
        format!("_csrf={csrf}&reset=1&totp_code=000000"),
        // Already spent on the login above.
        format!("_csrf={csrf}&reset=1&totp_code={code}"),
    ] {
        let html = totp_page(&app, &cookies, Some(body.clone())).await;
        let device = totp_store::device(&pool, id).await.expect("device");
        assert!(
            device.pending_secret_base32.is_none(),
            "{body} staged a secret"
        );
        assert!(
            html.contains("enabled</strong>"),
            "not the 2FA page: {html}"
        );
        assert!(!html.contains("Setup key"), "re-enroll started by {body}");
    }

    let next = rustango::totp::generate_at(&secret, now + 30, 30, 6);
    let html = totp_page(
        &app,
        &cookies,
        Some(format!("_csrf={csrf}&reset=1&totp_code={next}")),
    )
    .await;
    assert!(html.contains("Setup key"), "a fresh code must start it");
}

/// #1776 — wrong step-up codes lock the account like wrong login codes.
#[tokio::test]
async fn wrong_reenroll_codes_lock_the_account() {
    let (pool, _alice) = seed().await;
    let (_id, secret) = enrolled_user(&pool, "dave1776").await;
    let app = router(pool);
    let csrf = fetch_csrf(&app).await;
    let now = unix_now();
    let code = rustango::totp::generate_at(&secret, now, 30, 6);
    let session = session_cookie(&app, &csrf, "dave1776", &code).await;
    let reenroll = |code: String| {
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{PREFIX}/account/totp"))
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("rustango_csrf={csrf}; {session}"))
                .body(Body::from(format!("_csrf={csrf}&reset=1&totp_code={code}")))
                .unwrap(),
        )
    };
    let next = rustango::totp::generate_at(&secret, now + 30, 30, 6);
    let wrong = if next == "000000" { "111111" } else { "000000" };
    for n in 0..5 {
        let r = reenroll(wrong.to_owned()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "attempt {n}");
    }
    let r = reenroll(next).await.unwrap();
    assert_eq!(
        r.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a locked account must refuse even the right code"
    );
}

/// #1756 — an unfinished re-enroll keeps the old factor in force.
#[tokio::test]
async fn an_unfinished_reenroll_keeps_the_old_code() {
    let (pool, _alice) = seed().await;
    let (id, secret) = enrolled_user(&pool, "fay1756").await;
    let app = router(pool.clone());
    let csrf = fetch_csrf(&app).await;
    let now = unix_now();
    let code = rustango::totp::generate_at(&secret, now, 30, 6);
    let session = session_cookie(&app, &csrf, "fay1756", &code).await;
    let cookies = format!("rustango_csrf={csrf}; {session}");
    let next = rustango::totp::generate_at(&secret, now + 30, 30, 6);
    let html = totp_page(
        &app,
        &cookies,
        Some(format!("_csrf={csrf}&reset=1&totp_code={next}")),
    )
    .await;
    assert!(html.contains("Setup key"), "the re-enroll did not start");

    // Both codes in the window are spent, so check the store, not a login.
    let device = totp_store::device(&pool, id).await.expect("device");
    assert!(device.pending_secret_base32.is_some(), "nothing staged");
    assert_eq!(
        totp_store::confirmed_secret_checked(&pool, id)
            .await
            .unwrap()
            .map(|s| s.to_base32()),
        Some(secret.to_base32()),
        "the unfinished re-enroll replaced the factor"
    );
    let (_s, sess) = login(&app, &csrf, "fay1756", "correct horse", "").await;
    assert!(!sess, "a pending re-enroll let the password alone sign in");
}

/// #1756 — only the reset response shows a re-enroll's key; a code for
/// it then swaps the factor.
#[tokio::test]
async fn a_pending_reenroll_key_is_shown_once_and_promotes() {
    let (pool, _alice) = seed().await;
    let (id, old) = enrolled_user(&pool, "gus1756").await;
    let app = router(pool.clone());
    let csrf = fetch_csrf(&app).await;
    let now = unix_now();
    let session = session_cookie(
        &app,
        &csrf,
        "gus1756",
        &rustango::totp::generate_at(&old, now, 30, 6),
    )
    .await;
    let cookies = format!("rustango_csrf={csrf}; {session}");

    let current = rustango::totp::generate_at(&old, now + 30, 30, 6);
    let reset = totp_page(
        &app,
        &cookies,
        Some(format!("_csrf={csrf}&reset=1&totp_code={current}")),
    )
    .await;
    let pending = totp_store::device(&pool, id)
        .await
        .unwrap()
        .pending_secret_base32
        .expect("reset stages a pending secret");
    assert!(reset.contains(&pending), "the reset response shows the key");

    let get = totp_page(&app, &cookies, None).await;
    assert!(!get.contains(&pending), "GET leaked the pending key");
    let failed = totp_page(
        &app,
        &cookies,
        Some(format!("_csrf={csrf}&totp_code=000000")),
    )
    .await;
    assert!(
        !failed.contains(&pending),
        "a failed confirm leaked the key"
    );

    let fresh = TotpSecret::from_base32(&pending).unwrap();
    let code = rustango::totp::generate_at(&fresh, now, 30, 6);
    let done = totp_page(
        &app,
        &cookies,
        Some(format!("_csrf={csrf}&totp_code={code}")),
    )
    .await;
    assert!(done.contains("now enabled"), "promote failed: {done}");

    // The old codes in the window are spent, so check the store.
    assert_eq!(
        totp_store::confirmed_secret_checked(&pool, id)
            .await
            .unwrap()
            .map(|s| s.to_base32()),
        Some(pending.clone()),
        "the old factor is still the confirmed one"
    );
    let new_next = rustango::totp::generate_at(&fresh, now + 30, 30, 6);
    let (_s, sess) = login(&app, &csrf, "gus1756", "correct horse", &new_next).await;
    assert!(sess, "the new factor must sign in");
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
