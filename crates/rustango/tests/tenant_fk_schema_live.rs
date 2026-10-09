#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! #1645 — an FK an `ensure_*` path creates in a schema-mode tenant must
//! point inside that tenant's schema, never at a same-named table in
//! `public`. Runs in its own database so the looping `seed-permissions`
//! sees only this test's tenant. Reads `DATABASE_URL`; skips when unset.

use rustango::core::Column as _;
use rustango::sql::sqlx;
use rustango::sql::{CounterPool as _, FetcherPool as _};
use rustango::tenancy::{manage, Org, TenantPools};

#[path = "support/scratch_db.rs"]
mod scratch_db;
use scratch_db::ScratchDb;

/// FKs declared on `schema`'s tables whose target lives in another schema.
async fn cross_schema_fks(pool: &sqlx::PgPool, schema: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT c.conname || ' -> ' || tn.nspname || '.' || t.relname \
         FROM pg_constraint c \
         JOIN pg_class s ON s.oid = c.conrelid \
         JOIN pg_namespace sn ON sn.oid = s.relnamespace \
         JOIN pg_class t ON t.oid = c.confrelid \
         JOIN pg_namespace tn ON tn.oid = t.relnamespace \
         WHERE c.contype = 'f' AND sn.nspname = $1 AND tn.nspname <> $1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn run(pools: &TenantPools, url: &str, parts: &[&str]) -> Result<(), String> {
    let mut buf = Vec::new();
    let dir = std::env::temp_dir();
    let args: Vec<String> = parts.iter().map(|s| (*s).to_owned()).collect();
    manage::run_with_writer(pools, url, &dir, args, &mut buf)
        .await
        .map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&buf)))
}

#[tokio::test]
async fn seed_permissions_never_binds_a_tenant_fk_to_public() {
    let Ok(admin_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let db = ScratchDb::create(&admin_url, "rustango_t1645").await;
    let url = db.url().to_owned();

    // The registry's `public` holds every framework table, `rustango_users` included.
    let registry = sqlx::PgPool::connect(&url).await.unwrap();
    rustango::testkit::migrate_framework(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(registry.clone());

    // A schema-mode tenant whose schema is still empty.
    let slug = "t1645-acme";
    run(
        &pools,
        &url,
        &["create-tenant", slug, "--mode", "schema", "--no-migrate"],
    )
    .await
    .unwrap();
    let org = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap()
        .remove(0);
    let schema = org
        .schema_name
        .clone()
        .expect("schema-mode org has a schema");

    let res = run(&pools, &url, &["seed-permissions"]).await;
    assert_eq!(
        cross_schema_fks(&registry, &schema).await,
        Vec::<String>::new()
    );
    let err = res.expect_err("no rustango_users in the tenant: must refuse");
    let missing = format!(r#"relation "{schema}.rustango_users" does not exist"#);
    assert!(err.contains(&missing), "{err}");

    // Once the tenant has its own users table, the same path succeeds
    // and every FK stays inside the tenant.
    let scoped = pools.scoped_pool_dyn(&org).await.unwrap();
    rustango::testkit::migrate_framework(&scoped).await.unwrap();
    run(&pools, &url, &["seed-permissions"]).await.unwrap();
    assert_eq!(
        cross_schema_fks(&registry, &schema).await,
        Vec::<String>::new()
    );

    // UPGRADING's repair: drop the constraint, re-run `seed-permissions --slug`.
    let fks = [
        ("rustango_api_keys", "user_id"),
        ("rustango_role_permissions", "role_id"),
        ("rustango_user_roles", "user_id"),
        ("rustango_user_roles", "role_id"),
        ("rustango_user_permissions", "user_id"),
    ];
    for (table, column) in fks {
        sqlx::query(&format!(
            r#"ALTER TABLE "{schema}"."{table}" DROP CONSTRAINT "{table}_{column}_fkey""#
        ))
        .execute(&registry)
        .await
        .unwrap();
    }
    run(&pools, &url, &["seed-permissions", "--slug", slug])
        .await
        .unwrap();
    for (table, column) in fks {
        let target: String = sqlx::query_scalar(
            "SELECT tn.nspname FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.confrelid \
             JOIN pg_namespace tn ON tn.oid = t.relnamespace \
             JOIN pg_class s ON s.oid = c.conrelid \
             JOIN pg_namespace sn ON sn.oid = s.relnamespace \
             WHERE c.conname = $1 AND sn.nspname = $2",
        )
        .bind(format!("{table}_{column}_fkey"))
        .bind(&schema)
        .fetch_one(&registry)
        .await
        .unwrap_or_else(|e| panic!("{table}.{column} FK not re-created: {e}"));
        assert_eq!(target, schema, "{table}.{column}");
    }

    drop(scoped);
    drop(pools);
    registry.close().await;
}

/// A tenant whose seeding fails must not stop the next one (#2156).
#[tokio::test]
async fn seed_permissions_continues_past_a_failing_tenant() {
    let Ok(admin_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let db = ScratchDb::create(&admin_url, "rustango_t2156").await;
    let url = db.url().to_owned();
    let registry = sqlx::PgPool::connect(&url).await.unwrap();
    rustango::testkit::migrate_framework(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(registry.clone());

    // `t2156-a-broken` has no users table, so its FKs cannot be added.
    // Created last, it still runs first: tenants go in slug order.
    for slug in ["t2156-b-ok", "t2156-a-broken"] {
        run(
            &pools,
            &url,
            &["create-tenant", slug, "--mode", "schema", "--no-migrate"],
        )
        .await
        .unwrap();
    }
    let ok = Org::objects()
        .where_(Org::slug.eq("t2156-b-ok".to_owned()))
        .fetch(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap()
        .remove(0);
    let scoped = pools.scoped_pool_dyn(&ok).await.unwrap();
    rustango::testkit::migrate_framework(&scoped).await.unwrap();

    let perms = || async {
        rustango::tenancy::permissions::Permission::objects()
            .count(&scoped)
            .await
            .unwrap_or(0)
    };
    assert_eq!(perms().await, 0);
    let err = run(&pools, &url, &["seed-permissions"])
        .await
        .expect_err("the broken tenant must fail the run");
    assert!(
        perms().await > 0,
        "the tenant after the broken one was not seeded"
    );
    assert!(err.contains("1 of 2 tenant(s) failed"), "{err}");
    let broken = err.find("failed `t2156-a-broken`").expect(&err);
    let seeded = err.find("seeded `t2156-b-ok`").expect(&err);
    assert!(broken < seeded, "tenants must run in slug order: {err}");

    drop(scoped);
    drop(pools);
    registry.close().await;
}

/// Logs of `migrate-tenants` on a schema tenant while `public` holds a
/// passkey. `existing`: the tenant had users before, as before 0.60.5.
#[cfg(all(feature = "passkey", feature = "runtime", feature = "testkit"))]
async fn migrate_logs_with_public_passkeys(
    db_name: &str,
    slug: &str,
    existing: bool,
) -> Option<String> {
    use rustango::sql::Auto;
    use tracing::instrument::WithSubscriber as _;
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let db = ScratchDb::create(&admin_url, db_name).await;
    let url = db.url().to_owned();
    let registry = sqlx::PgPool::connect(&url).await.unwrap();
    let reg = rustango::sql::Pool::from(registry.clone());
    rustango::testkit::migrate_framework(&reg).await.unwrap();
    // A pre-0.60.5 app kept passkeys in `public`.
    rustango::passkey::ensure_table(&reg).await.unwrap();
    let mut cred = rustango::passkey::WebauthnCredential {
        id: Auto::default(),
        user_id: 1,
        credential_id: "cred-1".into(),
        public_key: vec![1, 2, 3],
        sign_count: 0,
        label: String::new(),
        created_at: chrono::Utc::now(),
    };
    cred.insert_pool(&reg).await.unwrap();
    let pools = TenantPools::new(registry.clone());
    run(
        &pools,
        &url,
        &["create-tenant", slug, "--mode", "schema", "--no-migrate"],
    )
    .await
    .unwrap();
    if existing {
        // Migrated before 0.60.5: users, but no tenant passkey table.
        run(&pools, &url, &["migrate-tenants"]).await.unwrap();
        let org = Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&reg)
            .await
            .unwrap()
            .remove(0);
        let schema = org.schema_name.clone().unwrap_or_else(|| slug.to_owned());
        sqlx::query(&format!(
            r#"DROP TABLE "{schema}".rustango_webauthn_credentials"#
        ))
        .execute(&registry)
        .await
        .unwrap();
    }

    let buf = rustango::testkit::CaptureWriter::default();
    let sink = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    run(&pools, &url, &["migrate-tenants"])
        .with_subscriber(subscriber)
        .await
        .unwrap();
    drop(pools);
    registry.close().await;
    Some(buf.contents())
}

/// #2518 — an existing schema tenant's new passkey table hides the rows in
/// `public`; `migrate-tenants` must say so.
#[cfg(all(feature = "passkey", feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn migrate_warns_when_public_passkeys_get_hidden() {
    let Some(logs) = migrate_logs_with_public_passkeys("rustango_t2518", "t2518-acme", true).await
    else {
        return;
    };
    assert!(
        logs.contains("public.rustango_webauthn_credentials") && logs.contains("t2518-acme"),
        "no warning about hidden passkeys:\n{logs}"
    );
}

/// #2518 — a brand-new tenant never read `public`, so it stays quiet.
#[cfg(all(feature = "passkey", feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn migrate_is_quiet_about_passkeys_for_a_new_tenant() {
    let Some(logs) = migrate_logs_with_public_passkeys("rustango_t2518b", "t2518-new", false).await
    else {
        return;
    };
    assert!(
        !logs.contains("rustango_webauthn_credentials"),
        "a new tenant was warned:\n{logs}"
    );
}
