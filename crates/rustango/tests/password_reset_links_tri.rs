//! A password change ends every older reset link, on every backend (#2248).

#![cfg(all(
    feature = "passwords",
    feature = "auth_flows",
    feature = "tenancy",
    feature = "testkit"
))]

use std::time::Duration;

use rustango::auth_flows::{
    confirm_password_reset_pool, AuthFlowError, LinkScope, LinkTarget, PasswordReset,
};
use rustango::sql::{FetcherPool as _, Pool};
use rustango::tenancy::User;
use rustango::tri_dialect_test;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";
const STRONG: &str = "brand-new-strong-password-7!";

async fn setup(pool: &Pool) {
    // Shared with other suites: create what is missing, never drop.
    rustango::testkit::migrate_framework(pool)
        .await
        .expect("framework tables");
}

/// A fresh user, `password_changed_at` as given.
async fn user(pool: &Pool, changed: Option<chrono::DateTime<chrono::Utc>>) -> i64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut u = User {
        username: format!("reset{n}"),
        password_hash: "OLD-HASH".into(),
        password_changed_at: changed,
        ..rustango::testkit::user()
    };
    u.insert_pool(pool).await.expect("insert user");
    *u.id.get().unwrap()
}

fn link(id: i64) -> String {
    PasswordReset::issue(
        &LinkScope::audience("app"),
        "https://x/reset",
        id,
        SECRET,
        Duration::from_secs(600),
    )
}

async fn hash(pool: &Pool, id: i64) -> String {
    User::objects()
        .filter("id", id)
        .fetch(pool)
        .await
        .unwrap()
        .remove(0)
        .password_hash
}

async fn using_a_newer_link_ends_an_older_one(pool: &Pool) {
    let id = user(pool, None).await;
    let (older, newer) = (link(id), link(id));

    confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &newer, STRONG, SECRET)
        .await
        .expect("newer link resets");
    let after = hash(pool, id).await;

    let res = confirm_password_reset_pool(
        LinkTarget::audience(pool, "app"),
        &older,
        "another-strong-pass-8?",
        SECRET,
    )
    .await;
    assert_eq!(res, Err(AuthFlowError::Expired));
    assert_eq!(hash(pool, id).await, after, "older link must not write");
}

async fn a_used_link_cannot_be_replayed(pool: &Pool) {
    let id = user(pool, None).await;
    let url = link(id);
    confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &url, STRONG, SECRET)
        .await
        .expect("first use");
    let res =
        confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &url, STRONG, SECRET).await;
    assert_eq!(res, Err(AuthFlowError::Expired));
}

async fn a_link_issued_after_the_last_change_works(pool: &Pool) {
    let earlier = chrono::Utc::now() - chrono::Duration::seconds(60);
    let id = user(pool, Some(earlier)).await;
    confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &link(id), STRONG, SECRET)
        .await
        .expect("link newer than the change");
    assert_ne!(hash(pool, id).await, "OLD-HASH");
}

async fn a_link_asked_for_just_after_a_change_works(pool: &Pool) {
    let id = user(pool, Some(chrono::Utc::now())).await;
    confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &link(id), STRONG, SECRET)
        .await
        .expect("a link from the same second, after the change");
}

async fn a_link_without_an_issue_time_is_refused(pool: &Pool) {
    let id = user(pool, None).await;
    let url = format!("https://x/reset?user_id={id}&purpose=pwreset&scope=aud%3Aapp");
    let url = rustango::signed_url::sign(&url, SECRET, Some(Duration::from_secs(600)));
    let res =
        confirm_password_reset_pool(LinkTarget::audience(pool, "app"), &url, STRONG, SECRET).await;
    assert_eq!(res, Err(AuthFlowError::Expired));
    assert_eq!(hash(pool, id).await, "OLD-HASH");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        using_a_newer_link_ends_an_older_one,
        a_used_link_cannot_be_replayed,
        a_link_issued_after_the_last_change_works,
        a_link_asked_for_just_after_a_change_works,
        a_link_without_an_issue_time_is_refused,
    ],
}
