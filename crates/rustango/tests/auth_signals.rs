//! Django-parity #414 — `user_logged_in` / `user_logged_out` /
//! `user_login_failed` lifecycle signals.
//!
//! Pure in-memory registry test — connects a receiver per signal, fires
//! the signal manually, asserts the receiver saw the context. No DB
//! roundtrip; the live wiring (admin / tenant / operator login handlers
//! calling `send_*`) is covered by separate live tests under each
//! console's existing test suite.

#![cfg(feature = "signals")]

use std::sync::Arc;

use tokio::sync::Mutex;

use rustango::signals::auth::{
    clear_all, connect_user_logged_in, connect_user_logged_out, connect_user_login_failed,
    disconnect_user_logged_in, receiver_count, send_user_logged_in, send_user_logged_out,
    send_user_login_failed, AuthFailureReason, AuthRequestMeta, UserLoggedInContext,
    UserLoggedOutContext, UserLoginFailedContext,
};

/// Cargo's default parallel harness runs these tests on shared global
/// signal state. Take a suite-wide mutex at entry so two tests don't
/// race on `clear_all` between each other's setup and assert.
fn suite_lock() -> &'static tokio::sync::Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn user_logged_in_fires_with_full_context() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Option<UserLoggedInContext>>> = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    let id = connect_user_logged_in(move |ctx| {
        let sink = sink.clone();
        async move {
            *sink.lock().await = Some(ctx);
        }
    });

    send_user_logged_in(UserLoggedInContext {
        source: "admin",
        user_id: 42,
        username: "alice".into(),
        is_superuser: true,
        request: AuthRequestMeta {
            ip_address: Some("1.2.3.4".into()),
            user_agent: Some("curl/8".into()),
            path: Some("/login".into()),
        },
    })
    .await;

    let got = captured.lock().await.clone().expect("receiver fired");
    assert_eq!(got.user_id, 42);
    assert_eq!(got.username, "alice");
    assert!(got.is_superuser);
    assert_eq!(got.source, "admin");
    assert_eq!(got.request.ip_address.as_deref(), Some("1.2.3.4"));
    assert_eq!(got.request.path.as_deref(), Some("/login"));

    assert!(rustango::signals::auth::disconnect_user_logged_in(id));
}

#[tokio::test]
async fn user_login_failed_carries_failure_reason() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<UserLoginFailedContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_user_login_failed(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    for reason in [
        AuthFailureReason::InvalidCredentials,
        AuthFailureReason::Inactive,
        AuthFailureReason::Locked,
    ] {
        send_user_login_failed(UserLoginFailedContext {
            source: "tenant_admin",
            attempted_username: Some("eve".into()),
            reason,
            request: AuthRequestMeta::default(),
        })
        .await;
    }

    let got = captured.lock().await;
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].reason, AuthFailureReason::InvalidCredentials);
    assert_eq!(got[1].reason, AuthFailureReason::Inactive);
    assert_eq!(got[2].reason, AuthFailureReason::Locked);
    assert!(got
        .iter()
        .all(|c| c.attempted_username.as_deref() == Some("eve")));
}

#[tokio::test]
async fn user_logged_out_supports_optional_user_id() {
    let _g = suite_lock().lock().await;
    clear_all();

    let captured: Arc<Mutex<Vec<UserLoggedOutContext>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    connect_user_logged_out(move |ctx| {
        let sink = sink.clone();
        async move {
            sink.lock().await.push(ctx);
        }
    });

    // Authenticated logout
    send_user_logged_out(UserLoggedOutContext {
        source: "operator",
        user_id: Some(7),
        username: Some("op".into()),
        request: AuthRequestMeta::default(),
    })
    .await;
    // Stale-cookie logout — receivers tolerate both Nones
    send_user_logged_out(UserLoggedOutContext {
        source: "operator",
        user_id: None,
        username: None,
        request: AuthRequestMeta::default(),
    })
    .await;

    let got = captured.lock().await;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].user_id, Some(7));
    assert_eq!(got[1].user_id, None);
}

#[tokio::test]
async fn receivers_run_in_registration_order() {
    let _g = suite_lock().lock().await;
    clear_all();

    let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    for tag in 1u8..=3 {
        let log = log.clone();
        connect_user_logged_in(move |_| {
            let log = log.clone();
            async move {
                log.lock().await.push(tag);
            }
        });
    }
    send_user_logged_in(UserLoggedInContext {
        source: "admin",
        user_id: 1,
        username: "u".into(),
        is_superuser: false,
        request: AuthRequestMeta::default(),
    })
    .await;
    assert_eq!(&*log.lock().await, &[1, 2, 3]);
}

#[tokio::test]
async fn disconnect_removes_only_the_named_receiver() {
    let _g = suite_lock().lock().await;
    clear_all();

    let counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let mut ids = Vec::new();
    for _ in 0..3 {
        let counter = counter.clone();
        ids.push(connect_user_logged_in(move |_| {
            let counter = counter.clone();
            async move {
                *counter.lock().await += 1;
            }
        }));
    }
    assert_eq!(receiver_count(), 3);

    assert!(disconnect_user_logged_in(ids[1]));
    assert_eq!(receiver_count(), 2);

    send_user_logged_in(UserLoggedInContext {
        source: "admin",
        user_id: 1,
        username: "u".into(),
        is_superuser: false,
        request: AuthRequestMeta::default(),
    })
    .await;

    assert_eq!(*counter.lock().await, 2);
}

#[test]
fn meta_from_headers_extracts_real_ip_and_ua() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("user-agent", "test-agent/1.0".parse().unwrap());
    headers.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());

    let meta = rustango::signals::auth::meta_from_headers(&headers, Some("/login"));
    assert_eq!(meta.user_agent.as_deref(), Some("test-agent/1.0"));
    // First IP in X-Forwarded-For wins.
    assert_eq!(meta.ip_address.as_deref(), Some("203.0.113.7"));
    assert_eq!(meta.path.as_deref(), Some("/login"));
}

#[test]
fn meta_from_headers_prefers_x_real_ip_over_xff() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-real-ip", "10.0.0.99".parse().unwrap());
    headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
    let meta = rustango::signals::auth::meta_from_headers(&headers, None);
    assert_eq!(meta.ip_address.as_deref(), Some("10.0.0.99"));
}
