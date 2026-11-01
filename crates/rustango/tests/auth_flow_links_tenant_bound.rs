//! A reset link minted for one tenant must not reset the same user id in
//! another tenant's database.

#![cfg(all(
    feature = "sqlite",
    feature = "passwords",
    feature = "auth_flows",
    feature = "tenancy"
))]

use std::time::Duration;

use rustango::auth_flows::{confirm_password_reset_pool, AuthFlowError, LinkScope, PasswordReset};
use rustango::extractors::Tenant;
use rustango::sql::{Auto, Pool};
use rustango::tenancy::Org;

const SECRET: &[u8] = b"a-strong-32-byte-secret-key-here";
const STRONG: &str = "brand-new-strong-password-7!";

/// One tenant database holding user 1.
async fn tenant_db() -> Pool {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    rustango::testkit::create_tables_for::<rustango::tenancy::User>(&pool)
        .await
        .expect("create rustango_users");
    let sq = match &pool {
        Pool::Sqlite(sq) => sq,
        #[allow(unreachable_patterns)]
        _ => unreachable!("connected to sqlite"),
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
    let sq = match pool {
        Pool::Sqlite(sq) => sq,
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    };
    sqlx::query("SELECT password_hash FROM rustango_users WHERE id = 1")
        .fetch_one(sq)
        .await
        .expect("fetch")
        .get("password_hash")
}

/// The org at `slug`, id `id`, over `pool`.
fn tenant(slug: &str, id: i64, pool: &Pool) -> Tenant<sqlx::Sqlite> {
    let sq = match pool {
        Pool::Sqlite(sq) => sq,
        #[allow(unreachable_patterns)]
        _ => unreachable!("connected to sqlite"),
    };
    let org = Org {
        id: Auto::Set(id),
        slug: slug.into(),
        ..rustango::testkit::org()
    };
    Tenant::for_test_sqlite(org, sq.clone())
}

#[tokio::test]
async fn a_link_for_another_tenant_does_not_reset_this_one() {
    let db = tenant_db().await;
    let tenant_b = tenant("b", 2, &db);
    // Minted by tenant a for its own user 1.
    let url = PasswordReset::issue(
        &LinkScope::from(&tenant("a", 1, &db)),
        "https://a.example.com/reset",
        1,
        SECRET,
        Duration::from_secs(600),
    );
    let res = confirm_password_reset_pool(&tenant_b, &url, STRONG, SECRET).await;
    assert_eq!(res, Err(AuthFlowError::WrongScope), "a's link reset b");
    assert_eq!(hash(&db).await, "OLD-HASH");
}

#[tokio::test]
async fn a_link_resets_in_its_own_tenant() {
    let db = tenant_db().await;
    let tenant_b = tenant("b", 2, &db);
    let url = PasswordReset::issue(
        &LinkScope::from(&tenant_b),
        "https://b/reset",
        1,
        SECRET,
        Duration::from_secs(600),
    );
    confirm_password_reset_pool(&tenant_b, &url, STRONG, SECRET)
        .await
        .expect("own tenant");
    assert_ne!(hash(&db).await, "OLD-HASH");
}
