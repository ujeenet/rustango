//! Login limits on the bare admin `POST /login` (#1609): per-IP limit,
//! a per-username lock that treats unknown names like real ones, and
//! a real user still getting in from another address.

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

/// Small per-IP limit so the test stays fast; same for every test here.
fn limits() {
    let _ = configure_shared(LoginThrottle::new(LoginLimits {
        ip_limit: 3,
        ..LoginLimits::default()
    }));
}

async fn app() -> axum::Router {
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
    for name in ["thr_alice", "thr_bob"] {
        let mut u = AdminUser::new_with_password(name, "right-pass", false).unwrap();
        u.insert_pool(&pool).await.unwrap();
    }
    Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build()
}

struct Answer {
    status: StatusCode,
    retry_after: Option<String>,
    session: bool,
    body: Vec<u8>,
}

async fn login(app: &axum::Router, ip: &str, user: &str, pass: &str) -> Answer {
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!(
            "_csrf={CSRF}&username={user}&password={pass}"
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
    let session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| v.to_str().unwrap_or("").contains("rustango_admin_session="));
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        retry_after,
        session,
        body,
    }
}

/// One IP spraying usernames is cut off with 429 + Retry-After, and a
/// real user on another IP still logs in.
#[tokio::test]
async fn per_ip_limit_trips_but_another_ip_logs_in() {
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

/// Failed logins lock a username whether or not it exists, and the
/// locked answer is byte-for-byte the same for both.
#[tokio::test]
async fn unknown_username_locks_like_a_real_one() {
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
