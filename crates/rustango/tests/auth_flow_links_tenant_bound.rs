//! A reset link minted in one tenant must not reset the same user id in
//! another tenant's database (#2472).

#![cfg(all(
    feature = "sqlite",
    feature = "passwords",
    feature = "auth_flows",
    feature = "tenancy"
))]

use std::time::Duration;

use rustango::auth_flows::{confirm_password_reset_pool, AuthFlowError, LinkScope, PasswordReset};
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
async fn a_link_from_tenant_x_does_not_reset_tenant_b() {
    let tenant_b = tenant_db().await;
    // Minted by tenant X's "forgot password" for its own user 1.
    let url = PasswordReset::issue(
        "https://x.example.com/reset",
        1,
        &LinkScope::tenant("x"),
        SECRET,
        Duration::from_secs(600),
    );
    let res =
        confirm_password_reset_pool(&tenant_b, &LinkScope::tenant("b"), &url, STRONG, SECRET).await;
    assert_eq!(
        res,
        Err(AuthFlowError::WrongScope),
        "tenant X's link reset tenant B"
    );
    assert_eq!(hash(&tenant_b).await, "OLD-HASH");
}

#[tokio::test]
async fn a_link_resets_in_its_own_tenant() {
    let tenant_b = tenant_db().await;
    let scope = LinkScope::tenant("b");
    let url = PasswordReset::issue(
        "https://b/reset",
        1,
        &scope,
        SECRET,
        Duration::from_secs(600),
    );
    confirm_password_reset_pool(&tenant_b, &scope, &url, STRONG, SECRET)
        .await
        .expect("own tenant");
    assert_ne!(hash(&tenant_b).await, "OLD-HASH");
}
