//! The `[auth]` login keys reach the gate, the lockout and the hash
//! queue, and a value set in code wins over them in either order
//! (#1609, #1732); `argon2_*` reach the hasher (#1728). Own binary: it sets process-global state, in order.

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

/// `[auth] argon2_*` set the cost of new hashes; code wins (#1728).
#[test]
fn argon2_keys_reach_the_hasher() {
    use rustango::passwords::{self, Argon2Params};
    // An invalid combination keeps the default.
    apply(|a| a.argon2_parallelism = Some(0));
    assert_eq!(passwords::argon2_params(), Argon2Params::DEFAULT);

    apply(|a| {
        a.argon2_memory_kib = Some(8_192);
        a.argon2_iterations = Some(3);
    });
    let h = passwords::hash("pw").unwrap();
    assert!(h.contains("$m=8192,t=3,p=1$"), "{h}");
    assert!(passwords::verify("pw", &h).unwrap());

    assert!(passwords::configure_argon2(
        Argon2Params::new(9_216, 2, 1).unwrap()
    ));
    apply(|a| a.argon2_memory_kib = Some(8_192));
    let h2 = passwords::hash("pw").unwrap();
    assert!(h2.contains("$m=9216,t=2,p=1$"), "{h2}");
    // Hashes made at the old cost still verify.
    assert!(passwords::verify("pw", &h).unwrap());
}

fn ip_ext(ip: &str) -> axum::http::Extensions {
    let mut ext = axum::http::Extensions::new();
    let addr: std::net::SocketAddr = format!("{ip}:1").parse().unwrap();
    ext.insert(axum::extract::ConnectInfo(addr));
    ext
}
