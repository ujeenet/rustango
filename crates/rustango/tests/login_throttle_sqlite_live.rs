//! Login limits on the bare admin `POST /login` (#1609): per-IP limit,
//! a per-username lock that treats unknown names like real ones, and
//! a real user still getting in from another address.
//!
//! The gate and the lockout are process-global, so every test holds
//! [`SUITE`] and uses its own usernames and addresses.

#![cfg(all(feature = "sqlite", feature = "admin"))]

use std::net::SocketAddr;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use rustango::admin::{AdminUser, Builder};
use rustango::login_throttle::{configure_shared, LoginLimits, LoginThrottle};
use rustango::session::SessionSecret;
use rustango::sql::{sqlx, Pool};
use tower::ServiceExt as _;

const CSRF: &str = "cccccccccccccccccccccccccccccccc";

static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Small per-IP limit so the test stays fast; same for every test here.
fn limits() {
    let _ = configure_shared(LoginThrottle::new(LoginLimits {
        ip_limit: 3,
        ..LoginLimits::default()
    }));
}

async fn app_with(users: &[(&str, bool)]) -> (axum::Router, Pool) {
    limits();
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let pool: Pool = p.into();
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    // The 2FA check fails closed without its table (#1644).
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(&pool)
        .await
        .unwrap();
    for (name, active) in users {
        let mut u = AdminUser::new_with_password(*name, "right-pass", false).unwrap();
        u.active = *active;
        u.insert_pool(&pool).await.unwrap();
    }
    let app = Builder::new(pool.clone())
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();
    (app, pool)
}

async fn app() -> axum::Router {
    app_with(&[("thr_alice", true), ("thr_bob", true)]).await.0
}

struct Answer {
    status: StatusCode,
    retry_after: Option<String>,
    session: bool,
    cookie: Option<String>,
    body: Vec<u8>,
}

async fn login(app: &axum::Router, ip: &str, user: &str, pass: &str) -> Answer {
    login_code(app, ip, user, pass, "").await
}

async fn login_code(app: &axum::Router, ip: &str, user: &str, pass: &str, code: &str) -> Answer {
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!(
            "_csrf={CSRF}&username={user}&password={pass}&totp_code={code}"
        )))
        .unwrap();
    let addr: SocketAddr = format!("{ip}:4000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(header::RETRY_AFTER)
        .map(|v| v.to_str().unwrap().to_owned());
    let cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("rustango_admin_session="))
        .map(|s| s.split(';').next().unwrap_or("").to_owned());
    let session = cookie.is_some();
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        retry_after,
        session,
        cookie,
        body,
    }
}

/// One IP spraying usernames is cut off with 429 + Retry-After, and a
/// real user on another IP still logs in.
#[tokio::test]
async fn per_ip_limit_trips_but_another_ip_logs_in() {
    let _g = SUITE.lock().await;
    let app = app().await;
    for n in 0..3 {
        let a = login(&app, "10.61.0.1", &format!("spray{n}"), "x").await;
        assert_eq!(a.status, StatusCode::OK, "attempt {n} is only a bad login");
    }
    let a = login(&app, "10.61.0.1", "spray9", "x").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(a.retry_after.is_some(), "429 must carry Retry-After");

    let a = login(&app, "10.61.0.2", "thr_bob", "right-pass").await;
    assert_eq!(a.status, StatusCode::SEE_OTHER);
    assert!(a.session, "the real user must still get a session");
}

/// Successful logins do not use up the per-IP limit, so many users
/// behind one address (a proxy without `RealIpLayer`) can still log in.
#[tokio::test]
async fn successes_do_not_use_up_the_per_ip_limit() {
    let _g = SUITE.lock().await;
    let app = app().await;
    for n in 0..8 {
        let a = login(&app, "10.64.0.1", "thr_bob", "right-pass").await;
        assert_eq!(a.status, StatusCode::SEE_OTHER, "login {n}");
    }
}

/// Failed logins lock a username whether or not it exists, and the
/// locked answer is byte-for-byte the same for both.
#[tokio::test]
async fn unknown_username_locks_like_a_real_one() {
    let _g = SUITE.lock().await;
    let app = app().await;
    // A fresh IP per attempt, so only the account lock can trip.
    for n in 0..5 {
        let a = login(&app, &format!("10.62.1.{n}"), "thr_alice", "wrong").await;
        assert_eq!(a.status, StatusCode::OK);
        let a = login(&app, &format!("10.62.2.{n}"), "thr_ghost", "wrong").await;
        assert_eq!(a.status, StatusCode::OK);
    }
    let known = login(&app, "10.62.3.1", "thr_alice", "wrong").await;
    let unknown = login(&app, "10.62.3.2", "thr_ghost", "wrong").await;
    assert_eq!(known.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(unknown.status, known.status);
    assert_eq!(unknown.retry_after, known.retry_after);
    assert_eq!(
        unknown.body, known.body,
        "the lock must not reveal which name exists"
    );

    // A different, untouched account is unaffected.
    let a = login(&app, "10.62.3.3", "thr_bob", "right-pass").await;
    assert_eq!(a.status, StatusCode::SEE_OTHER);
}

/// A locked account is refused even with the correct password.
#[tokio::test]
async fn a_locked_account_refuses_the_correct_password() {
    let _g = SUITE.lock().await;
    let (app, _) = app_with(&[("thr_carol", true)]).await;
    for n in 0..5 {
        let a = login(&app, &format!("10.65.1.{n}"), "thr_carol", "wrong").await;
        assert_eq!(a.status, StatusCode::OK);
    }
    let a = login(&app, "10.65.2.1", "thr_carol", "right-pass").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!a.session, "a locked account must not get a session");
}

/// A successful login resets the failure count.
#[tokio::test]
async fn a_success_resets_the_count() {
    let _g = SUITE.lock().await;
    let (app, _) = app_with(&[("thr_dave", true)]).await;
    let mut ip = 0;
    let mut fail = |n: u32| {
        let app = app.clone();
        let start = ip;
        ip += n;
        async move {
            for i in start..start + n {
                let a = login(&app, &format!("10.66.1.{i}"), "thr_dave", "wrong").await;
                assert_eq!(a.status, StatusCode::OK);
            }
        }
    };
    fail(4).await;
    let a = login(&app, "10.66.2.1", "thr_dave", "right-pass").await;
    assert_eq!(a.status, StatusCode::SEE_OTHER);
    fail(4).await;
    let a = login(&app, "10.66.2.2", "thr_dave", "right-pass").await;
    assert_eq!(
        a.status,
        StatusCode::SEE_OTHER,
        "old failures still counted"
    );
}

/// Logging in to an inactive account counts as a failure, even with
/// the right password.
#[tokio::test]
async fn an_inactive_account_counts_as_a_failure() {
    let _g = SUITE.lock().await;
    let (app, _) = app_with(&[("thr_erin", false)]).await;
    for n in 0..5 {
        let a = login(&app, &format!("10.67.1.{n}"), "thr_erin", "right-pass").await;
        assert_eq!(a.status, StatusCode::OK);
    }
    let a = login(&app, "10.67.2.1", "thr_erin", "right-pass").await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

/// A wrong TOTP code counts as a failure; a missing one does not.
#[cfg(feature = "totp")]
#[tokio::test]
async fn a_wrong_totp_code_counts_as_a_failure() {
    use rustango::admin::totp_store;
    use rustango::sql::FetcherPool as _;
    let _g = SUITE.lock().await;
    let (app, pool) = app_with(&[("thr_fred", true)]).await;
    let id = AdminUser::objects()
        .filter("username", "thr_fred")
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;
    let id = *id.get().unwrap();
    let secret = rustango::totp::TotpSecret::generate();
    totp_store::start_enrollment(&pool, id, &secret)
        .await
        .unwrap();
    totp_store::confirm(&pool, id).await.unwrap();
    // Never a valid code: not digits.
    const WRONG: &str = "abcdef";

    for n in 0..6 {
        let a = login(&app, &format!("10.68.0.{n}"), "thr_fred", "right-pass").await;
        assert_eq!(a.status, StatusCode::OK, "the prompt alone must not lock");
    }
    for n in 0..5 {
        let ip = format!("10.68.1.{n}");
        let a = login_code(&app, &ip, "thr_fred", "right-pass", WRONG).await;
        assert_eq!(a.status, StatusCode::OK);
    }
    let a = login_code(&app, "10.68.2.1", "thr_fred", "right-pass", WRONG).await;
    assert_eq!(a.status, StatusCode::TOO_MANY_REQUESTS);
}

/// The 2FA prompt (right password, no code yet) spends no per-IP
/// tokens, so one address can log in more often than the limit (#1748).
#[cfg(feature = "totp")]
#[tokio::test]
async fn the_totp_prompt_spends_no_limit_tokens() {
    use rustango::admin::totp_store;
    use rustango::sql::FetcherPool as _;
    let _g = SUITE.lock().await;
    let (app, pool) = app_with(&[("thr_gina", true)]).await;
    let id = AdminUser::objects()
        .filter("username", "thr_gina")
        .fetch(&pool)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;
    let id = *id.get().unwrap();
    let secret = rustango::totp::TotpSecret::generate();
    totp_store::start_enrollment(&pool, id, &secret)
        .await
        .unwrap();
    totp_store::confirm(&pool, id).await.unwrap();

    // Twice the per-IP limit of 3, all from one address.
    for n in 0..6 {
        let a = login(&app, "10.69.0.1", "thr_gina", "right-pass").await;
        assert_eq!(a.status, StatusCode::OK, "prompt {n} must not be throttled");
        assert!(!a.session, "no session without the code");
    }
}

/// #1776 — a re-enroll without a code is no guess, so it spends no
/// per-IP tokens.
#[cfg(feature = "totp")]
#[tokio::test]
async fn a_codeless_reenroll_spends_no_limit_tokens() {
    use rustango::admin::totp_store;
    let _g = SUITE.lock().await;
    let (app, pool) = app_with(&[]).await;
    // Only a superuser reaches the account pages.
    let mut u = AdminUser::new_with_password("thr_hank", "right-pass", true).unwrap();
    u.insert_pool(&pool).await.unwrap();
    let id = *u.id.get().unwrap();
    let secret = rustango::totp::TotpSecret::generate();
    totp_store::start_enrollment(&pool, id, &secret)
        .await
        .unwrap();
    totp_store::confirm(&pool, id).await.unwrap();
    let code = rustango::totp::generate(&secret, 30, 6);
    let a = login_code(&app, "10.70.0.1", "thr_hank", "right-pass", &code).await;
    let session = a.cookie.expect("session");

    // Twice the per-IP limit of 3, all from one address.
    for n in 0..6 {
        let mut req = Request::builder()
            .method("POST")
            .uri("/account/totp")
            .header("content-type", "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("rustango_csrf={CSRF}; {session}"))
            .body(Body::from(format!("_csrf={CSRF}&reset=1")))
            .unwrap();
        let addr: SocketAddr = "10.70.0.2:4000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "reset {n} was throttled");
    }
}

/// #1791 — wrong enrollment codes count against the account like login ones.
#[cfg(feature = "totp")]
#[tokio::test]
async fn wrong_enrollment_codes_lock_the_account() {
    use rustango::admin::totp_store;
    let _g = SUITE.lock().await;
    let (app, pool) = app_with(&[]).await;
    let mut u = AdminUser::new_with_password("thr_ivy", "right-pass", true).unwrap();
    u.insert_pool(&pool).await.unwrap();
    let id = *u.id.get().unwrap();
    let a = login(&app, "10.71.0.1", "thr_ivy", "right-pass").await;
    let session = a.cookie.expect("session");
    totp_store::start_enrollment(&pool, id, &rustango::totp::TotpSecret::generate())
        .await
        .unwrap();

    let confirm = |n: usize| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/account/totp")
            .header("content-type", "application/x-www-form-urlencoded")
            .header(header::COOKIE, format!("rustango_csrf={CSRF}; {session}"))
            .body(Body::from(format!("_csrf={CSRF}&totp_code=abcdef")))
            .unwrap();
        let addr: SocketAddr = format!("10.71.1.{n}:4000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        app.clone().oneshot(req)
    };
    for n in 0..5 {
        assert_eq!(confirm(n).await.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(
        confirm(9).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
