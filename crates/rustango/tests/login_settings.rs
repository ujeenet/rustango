//! The `[auth]` login keys reach the gate, the lockout and the hash
//! queue, and a value set in code wins over them in either order
//! (#1609, #1732). Own binary: it sets process-global state, in order.

#![cfg(all(
    feature = "config",
    feature = "manage",
    feature = "admin",
    feature = "cache",
    feature = "passwords"
))]

use std::sync::Arc;
use std::time::Duration;

use rustango::account_lockout::{self, Lockout};
use rustango::config::Settings;
use rustango::login_throttle::{self, ClientIp, LoginLimits, LoginScope, LoginThrottle};
use rustango::manage::Cli;

fn apply(f: impl FnOnce(&mut rustango::config::AuthSettings)) {
    let mut s = Settings::default();
    f(&mut s.auth);
    let _ = Cli::new().with_settings(&s);
}

#[tokio::test]
async fn auth_keys_reach_the_gate_and_code_wins() {
    // Distinct values, so a swapped pair cannot pass.
    apply(|a| {
        a.login_ip_limit = Some(2);
        a.login_ip_window_secs = Some(11);
        a.login_global_limit = Some(13);
        a.login_global_window_secs = Some(17);
        a.hash_wait_ms = Some(19);
        a.lockout_duration_secs = Some(23);
    });
    assert_eq!(
        login_throttle::shared().limits(),
        LoginLimits {
            ip_limit: 2,
            ip_window: Duration::from_secs(11),
            global_limit: 13,
            global_window: Duration::from_secs(17),
        }
    );
    assert_eq!(rustango::passwords::hash_wait(), Duration::from_millis(19));
    assert_eq!(
        account_lockout::shared().lock_duration(),
        Duration::from_secs(23)
    );

    // The gate enforces the per-IP value, not the global one.
    let scope = LoginScope::Operator;
    let ip = ClientIp::from_parts(&ip_ext("10.70.0.1"), &Default::default());
    for n in 0..2 {
        let a = login_throttle::shared()
            .begin(&scope, &ip, &format!("s{n}"))
            .await;
        a.expect("under the per-IP limit").failed().await;
    }
    assert!(login_throttle::shared()
        .begin(&scope, &ip, "s9")
        .await
        .is_err());

    // Code set after the settings wins.
    assert!(login_throttle::configure_shared(LoginThrottle::new(
        LoginLimits {
            ip_limit: 29,
            ..LoginLimits::default()
        }
    )));
    assert!(account_lockout::configure_shared(
        Lockout::new(Arc::new(rustango::cache::InMemoryCache::new()))
            .lockout_duration(Duration::from_secs(31))
    ));
    assert!(rustango::passwords::configure_hash_wait(
        Duration::from_millis(37)
    ));
    assert_eq!(login_throttle::shared().limits().ip_limit, 29);
    assert_eq!(
        account_lockout::shared().lock_duration(),
        Duration::from_secs(31)
    );
    assert_eq!(rustango::passwords::hash_wait(), Duration::from_millis(37));

    // Settings applied after code do not replace it.
    apply(|a| {
        a.login_ip_limit = Some(41);
        a.hash_wait_ms = Some(43);
        a.lockout_duration_secs = Some(47);
    });
    assert_eq!(login_throttle::shared().limits().ip_limit, 29);
    assert_eq!(rustango::passwords::hash_wait(), Duration::from_millis(37));
    assert_eq!(
        account_lockout::shared().lock_duration(),
        Duration::from_secs(31)
    );
}

fn ip_ext(ip: &str) -> axum::http::Extensions {
    let mut ext = axum::http::Extensions::new();
    let addr: std::net::SocketAddr = format!("{ip}:1").parse().unwrap();
    ext.insert(axum::extract::ConnectInfo(addr));
    ext
}
