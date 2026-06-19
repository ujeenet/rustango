//! Backing test for `docs/auth-jwt-api.md` — the access+refresh token engine
//! (`JwtLifecycle`) behind the built-in `/api/auth/{login,refresh,logout,me}`
//! router. Pure (in-memory JTI store), no DB. The HTTP endpoints themselves are
//! tenant-scoped and exercised end-to-end by the framework's
//! `crates/rustango/tests/tenant_auth_live.rs`.
//!
//! Run: `cargo test -p auth_demo --test auth_jwt_api`

use std::sync::Arc;

use rustango::jti_store::{InMemoryJtiStore, JtiStore};
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;

fn jwt() -> JwtLifecycle {
    JwtLifecycle::new(b"a-signing-secret-at-least-32-bytes-long!!".to_vec())
}

#[test]
fn login_issues_a_pair_and_the_token_types_are_distinct() {
    let j = jwt();
    let pair = j.issue_pair(42); // what POST /api/auth/login returns

    let claims = j.verify_access(&pair.access).expect("access verifies");
    assert_eq!(claims.sub, 42);
    assert_eq!(claims.typ, "access");

    // An access token is rejected where a refresh is required, and vice versa —
    // so a stolen short-lived access token can't be used to mint new ones.
    assert!(j.verify_refresh(&pair.access).is_none());
    assert!(j.verify_access(&pair.refresh).is_none());
}

#[test]
fn refresh_rotates_and_blacklists_the_old_refresh_token() {
    let j = jwt();
    let pair = j.issue_pair(7);

    let rotated = j.refresh(&pair.refresh).expect("POST /api/auth/refresh");
    assert_ne!(pair.access, rotated.access);
    assert_eq!(j.verify_access(&rotated.access).unwrap().sub, 7);

    // Sliding refresh: the old refresh token is single-use — replay is rejected.
    assert!(j.refresh(&pair.refresh).is_none());
}

#[test]
fn logout_revokes_the_token() {
    let j = jwt();
    let pair = j.issue_pair(1);
    assert!(j.verify_access(&pair.access).is_some());

    assert!(j.revoke(&pair.access)); // what POST /api/auth/logout does
    assert!(j.verify_access(&pair.access).is_none());
}

#[test]
fn custom_claims_ride_in_the_token_and_survive_refresh() {
    let j = jwt();
    let custom = serde_json::json!({ "roles": ["admin"], "tenant": "acme" })
        .as_object()
        .unwrap()
        .clone();

    let pair = j.issue_pair_with(99, custom).unwrap();
    let claims = j.verify_access(&pair.access).unwrap();
    assert_eq!(
        claims.get_custom::<Vec<String>>("roles").unwrap(),
        vec!["admin".to_string()]
    );

    // refresh() carries the same custom payload onto the new pair (use
    // refresh_with() to re-evaluate permissions instead).
    let rotated = j.refresh(&pair.refresh).unwrap();
    let rc = j.verify_access(&rotated.access).unwrap();
    assert_eq!(rc.get_custom::<String>("tenant").as_deref(), Some("acme"));
}

#[test]
fn revocation_is_visible_across_handles_via_a_shared_store() {
    // The default in-memory store is single-process. In production pass a
    // Redis/DB-backed `JtiStore` so a logout on one replica is seen by all —
    // here two handles share one in-memory store to prove the wiring.
    let secret = b"shared-signing-secret-32-bytes-long-xx!!".to_vec();
    let shared: Arc<dyn JtiStore> = Arc::new(InMemoryJtiStore::new());
    let a = JwtLifecycle::new(secret.clone()).with_jti_store(Arc::clone(&shared));
    let b = JwtLifecycle::new(secret).with_jti_store(Arc::clone(&shared));

    let pair = a.issue_pair(5);
    assert!(b.verify_access(&pair.access).is_some());
    a.revoke(&pair.access);
    assert!(
        b.verify_access(&pair.access).is_none(),
        "instance B must see the revocation made on instance A"
    );
}
