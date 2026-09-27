//! A full password-hashing queue answers 503 after a bounded wait, the
//! same for a known and an unknown user (#1732). Own binary: it fills
//! the process-wide queue.

#![cfg(all(feature = "sqlite", feature = "admin"))]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use rustango::admin::{AdminUser, Builder};
use rustango::session::SessionSecret;
use rustango::sql::{sqlx, Pool};
use tower::ServiceExt as _;

const CSRF: &str = "cccccccccccccccccccccccccccccccc";

async fn login(app: &axum::Router, ip: &str, user: &str) -> (StatusCode, bool, Vec<u8>) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rustango_csrf={CSRF}"))
        .body(Body::from(format!(
            "_csrf={CSRF}&username={user}&password=right-pass"
        )))
        .unwrap();
    let addr: SocketAddr = format!("{ip}:4000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let resp = tokio::time::timeout(Duration::from_secs(20), app.clone().oneshot(req))
        .await
        .expect("login must not wait forever for a hashing slot")
        .unwrap();
    let status = resp.status();
    let retry = resp.headers().contains_key(header::RETRY_AFTER);
    let body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, retry, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_queue_answers_busy_for_known_and_unknown_users() {
    assert!(rustango::passwords::configure_hash_wait(
        Duration::from_millis(50)
    ));
    let p = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let pool: Pool = p.into();
    rustango::testkit::create_tables_for::<AdminUser>(&pool)
        .await
        .unwrap();
    #[cfg(feature = "totp")]
    rustango::admin::totp_store::ensure_table(&pool)
        .await
        .unwrap();
    let mut u = AdminUser::new_with_password("hq_alice", "right-pass", false).unwrap();
    u.insert_pool(&pool).await.unwrap();
    let app = Builder::new(pool)
        .admin_prefix("")
        .with_session_auth(SessionSecret::from_bytes(vec![7u8; 32]))
        .build();

    // More hashers than slots keep a queue ahead of every login.
    let slots = std::thread::available_parallelism().map_or(4, |n| n.get());
    let run = Arc::new(AtomicBool::new(true));
    let hogs: Vec<_> = (0..slots + 3)
        .map(|_| {
            let run = Arc::clone(&run);
            tokio::spawn(async move {
                while run.load(Ordering::Relaxed) {
                    let _ = rustango::passwords::hash_async("hog").await;
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let known = login(&app, "10.63.0.1", "hq_alice").await;
    let unknown = login(&app, "10.63.0.2", "hq_ghost").await;
    run.store(false, Ordering::Relaxed);
    for h in hogs {
        let _ = h.await;
    }

    assert_eq!(known.0, StatusCode::SERVICE_UNAVAILABLE, "known user");
    assert_eq!(unknown.0, StatusCode::SERVICE_UNAVAILABLE, "unknown user");
    assert!(known.1 && unknown.1, "503 must carry Retry-After");
    assert_eq!(known.2, unknown.2, "busy must not reveal which name exists");

    // Once the queue drains, the real user logs in again.
    let after = login(&app, "10.63.0.3", "hq_alice").await;
    assert_eq!(after.0, StatusCode::SEE_OTHER);
}
