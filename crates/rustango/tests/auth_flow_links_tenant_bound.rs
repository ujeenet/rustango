//! A reset link minted for one scope must not reset the same user id in
//! another scope's database (#2472). The `&Tenant` form is unit-tested in `auth_flows`.

#![cfg(all(
    feature = "sqlite",
    feature = "passwords",
    feature = "auth_flows",
    feature = "tenancy"
))]

use std::time::Duration;

use rustango::auth_flows::{
    confirm_password_reset_pool, AuthFlowError, LinkScope, LinkTarget, PasswordReset,
};
use rustango::sql::Pool;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";
const STRONG: &str = "brand-new-strong-password-7!";

/// One tenant database holding user 1.
async fn tenant_db() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create rustango_users");
    let Pool::Sqlite(sq) = &pool else {
        unreachable!("connected to sqlite")
    };
    sqlx::query(
        "INSERT INTO rustango_users \
         (id, username, password_hash, is_superuser, active, created_at, password_changed_at) \
         VALUES (1, 'ada', 'OLD-HASH', 0, 1, datetime('now'), NULL)",
    )
    .execute(sq)
    .await
    .expect("seed user");
    pool
}

async fn hash(pool: &Pool) -> String {
    use sqlx::Row;
    let Pool::Sqlite(sq) = pool else {
        unreachable!()
    };
    sqlx::query("SELECT password_hash FROM rustango_users WHERE id = 1")
        .fetch_one(sq)
        .await
        .expect("fetch")
        .get("password_hash")
}

#[tokio::test]
async fn a_link_for_another_scope_does_not_reset_this_one() {
    let tenant_b = tenant_db().await;
    // Minted for scope "x" for its own user 1.
    let url = PasswordReset::issue(
        &LinkScope::audience("x"),
        "https://x.example.com/reset",
        1,
        SECRET,
        Duration::from_secs(600),
    );
    let res =
        confirm_password_reset_pool(LinkTarget::audience(&tenant_b, "b"), &url, STRONG, SECRET)
            .await;
    assert_eq!(
        res,
        Err(AuthFlowError::WrongScope),
        "scope x's link reset b"
    );
    assert_eq!(hash(&tenant_b).await, "OLD-HASH");
}

#[tokio::test]
async fn a_link_resets_in_its_own_scope() {
    let tenant_b = tenant_db().await;
    let url = PasswordReset::issue(
        &LinkScope::audience("b"),
        "https://b/reset",
        1,
        SECRET,
        Duration::from_secs(600),
    );
    confirm_password_reset_pool(LinkTarget::audience(&tenant_b, "b"), &url, STRONG, SECRET)
        .await
        .expect("own scope");
    assert_ne!(hash(&tenant_b).await, "OLD-HASH");
}
