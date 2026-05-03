#![cfg(feature = "tenancy")]
//! Live tests for the 2-domain auth model.
//!
//! Reads `DATABASE_URL`. Skips silently when unset.

use std::sync::atomic::{AtomicU64, Ordering};

use rustango::core::Column as _;
use rustango::sql::{sqlx, Fetcher};
use rustango::migrate as rmig;
use rustango::tenancy::{
    authenticate_operator, authenticate_user, manage, password, Org, TenantPools,
};

async fn lookup_org(pool: &sqlx::PgPool, slug: &str) -> Org {
    let mut rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(pool)
        .await
        .unwrap();
    rows.pop().expect("org should exist")
}

static UNIQ: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let n = UNIQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{prefix}_{pid}_{n}")
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.unwrap())
}

async fn run(
    pools: &TenantPools,
    url: &str,
    parts: &[&str],
) -> Result<(), rustango::tenancy::TenancyError> {
    let mut buf: Vec<u8> = Vec::new();
    let dir = std::env::temp_dir().join("rustango_auth_dir");
    let _ = std::fs::create_dir_all(&dir);
    manage::run_with_writer(
        pools,
        url,
        &dir,
        parts.iter().map(|s| (*s).to_string()),
        &mut buf,
    )
    .await
}

async fn drop_schema(pool: &sqlx::PgPool, name: &str) {
    let sql = format!(r#"DROP SCHEMA IF EXISTS "{name}" CASCADE"#);
    sqlx::query(&sql).execute(pool).await.unwrap();
}

#[tokio::test]
async fn create_operator_and_authenticate_round_trip() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let username = unique("admin");
    run(
        &pools,
        &url,
        &["create-operator", &username, "--password", "hunter2"],
    )
    .await
    .unwrap();

    // Right password authenticates.
    let op = authenticate_operator(&pool, &username, "hunter2").await.unwrap();
    assert!(op.is_some(), "right password should authenticate");
    assert_eq!(op.unwrap().username, username);

    // Wrong password rejects.
    let op = authenticate_operator(&pool, &username, "wrong").await.unwrap();
    assert!(op.is_none(), "wrong password should reject");

    // Unknown username rejects.
    let op = authenticate_operator(&pool, "nobody", "hunter2").await.unwrap();
    assert!(op.is_none(), "unknown user should reject");

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_operator_rejects_duplicate_username() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let username = unique("dup");
    run(
        &pools,
        &url,
        &["create-operator", &username, "--password", "x"],
    )
    .await
    .unwrap();

    let err = run(
        &pools,
        &url,
        &["create-operator", &username, "--password", "x"],
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("already exists"));

    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn create_user_in_schema_mode_tenant_authenticates_against_that_schema() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    // Provision a schema-mode tenant. We need the rustango_users
    // table in that schema — create it manually since slice 6 doesn't
    // ship a bootstrap migration. (v0.6 will package one with the
    // crate.)
    let slug = unique("acme");
    drop_schema(&pool, &slug).await;
    sqlx::query(&format!(r#"CREATE SCHEMA "{slug}""#))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        r#"CREATE TABLE "{slug}"."rustango_users" (
            "id" BIGSERIAL NOT NULL PRIMARY KEY,
            "username" VARCHAR(64) NOT NULL,
            "password_hash" VARCHAR(255) NOT NULL,
            "is_superuser" BOOLEAN NOT NULL,
            "active" BOOLEAN NOT NULL,
            "created_at" TIMESTAMPTZ NOT NULL
        )"#
    ))
    .execute(&pool)
    .await
    .unwrap();

    let pools = TenantPools::new(pool.clone());
    run(
        &pools,
        &url,
        &[
            "create-tenant",
            &slug,
            "--mode",
            "schema",
            "--schema-name",
            &slug,
            "--no-migrate",
        ],
    )
    .await
    .unwrap();

    // Create a per-tenant user.
    let user = unique("alice");
    run(
        &pools,
        &url,
        &[
            "create-user",
            &slug,
            &user,
            "--password",
            "hunter2",
            "--superuser",
        ],
    )
    .await
    .unwrap();

    // Authenticate via a schema-scoped connection from TenantPools.
    let org = lookup_org(pools.registry(), &slug).await;
    let mut conn = pools.acquire(&org).await.unwrap();
    let auth = authenticate_user(&mut conn, &user, "hunter2").await.unwrap();
    assert!(auth.is_some(), "right password should authenticate");
    let u = auth.unwrap();
    assert_eq!(u.username, user);
    assert!(u.is_superuser);

    // Wrong password rejects.
    let auth = authenticate_user(&mut conn, &user, "wrong").await.unwrap();
    assert!(auth.is_none());

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn hard_wall_operator_credential_does_not_authenticate_against_tenant() {
    // Create an operator in the registry. Try to authenticate them
    // as a tenant user — must fail because the username doesn't
    // exist in the tenant's rustango_users.
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("acme_hw");
    drop_schema(&pool, &slug).await;
    sqlx::query(&format!(r#"CREATE SCHEMA "{slug}""#))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        r#"CREATE TABLE "{slug}"."rustango_users" (
            "id" BIGSERIAL NOT NULL PRIMARY KEY,
            "username" VARCHAR(64) NOT NULL,
            "password_hash" VARCHAR(255) NOT NULL,
            "is_superuser" BOOLEAN NOT NULL,
            "active" BOOLEAN NOT NULL,
            "created_at" TIMESTAMPTZ NOT NULL
        )"#
    ))
    .execute(&pool)
    .await
    .unwrap();

    let pools = TenantPools::new(pool.clone());
    let op_user = unique("operator_only");
    run(
        &pools,
        &url,
        &["create-operator", &op_user, "--password", "secret"],
    )
    .await
    .unwrap();

    run(
        &pools,
        &url,
        &[
            "create-tenant",
            &slug,
            "--mode",
            "schema",
            "--schema-name",
            &slug,
            "--no-migrate",
        ],
    )
    .await
    .unwrap();

    let org = lookup_org(pools.registry(), &slug).await;
    let mut conn = pools.acquire(&org).await.unwrap();
    // Operator's exact username + password — should NOT authenticate
    // as a tenant user (the hard wall).
    let auth = authenticate_user(&mut conn, &op_user, "secret").await.unwrap();
    assert!(
        auth.is_none(),
        "operator credentials must not authenticate against a tenant"
    );

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn hard_wall_tenant_user_credential_does_not_authenticate_as_operator() {
    let Some(pool) = pool().await else {
        return;
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    rmig::drop_all(&pool).await.unwrap();
    rmig::apply_all(&pool).await.unwrap();

    let slug = unique("acme_hw2");
    drop_schema(&pool, &slug).await;
    sqlx::query(&format!(r#"CREATE SCHEMA "{slug}""#))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        r#"CREATE TABLE "{slug}"."rustango_users" (
            "id" BIGSERIAL NOT NULL PRIMARY KEY,
            "username" VARCHAR(64) NOT NULL,
            "password_hash" VARCHAR(255) NOT NULL,
            "is_superuser" BOOLEAN NOT NULL,
            "active" BOOLEAN NOT NULL,
            "created_at" TIMESTAMPTZ NOT NULL
        )"#
    ))
    .execute(&pool)
    .await
    .unwrap();

    let pools = TenantPools::new(pool.clone());
    run(
        &pools,
        &url,
        &[
            "create-tenant",
            &slug,
            "--mode",
            "schema",
            "--schema-name",
            &slug,
            "--no-migrate",
        ],
    )
    .await
    .unwrap();
    let user = unique("tenant_super");
    run(
        &pools,
        &url,
        &[
            "create-user",
            &slug,
            &user,
            "--password",
            "topsecret",
            "--superuser",
        ],
    )
    .await
    .unwrap();

    // The tenant user (even superuser) must NOT authenticate as an
    // operator — the username doesn't exist in rustango_operators.
    let op = authenticate_operator(&pool, &user, "topsecret")
        .await
        .unwrap();
    assert!(
        op.is_none(),
        "tenant user (even superuser) must not authenticate as operator"
    );

    drop_schema(&pool, &slug).await;
    rmig::drop_all(&pool).await.unwrap();
}

#[test]
fn password_helpers_match_in_test_only() {
    let h = password::hash("hunter2").unwrap();
    assert!(password::verify("hunter2", &h).unwrap());
    assert!(!password::verify("wrong", &h).unwrap());
}
