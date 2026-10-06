#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! #1645 — an FK an `ensure_*` path creates in a schema-mode tenant must
//! point inside that tenant's schema, never at a same-named table in
//! `public`. Runs in its own database so the looping `seed-permissions`
//! sees only this test's tenant. Reads `DATABASE_URL`; skips when unset.

use rustango::core::Column as _;
use rustango::sql::sqlx;
use rustango::sql::{CounterPool as _, FetcherPool as _};
use rustango::tenancy::{manage, Org, TenantPools};

fn sibling_database_url(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("DATABASE_URL names a database");
    format!("{base}/{database}")
}

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
    let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
    let db = "rustango_t1645";
    let drop_db = format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)");
    sqlx::query(&drop_db).execute(&admin).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap();
    let url = sibling_database_url(&admin_url, &db);

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
    sqlx::query(&drop_db).execute(&admin).await.unwrap();
}

/// A tenant whose seeding fails must not stop the next one (#2156).
#[tokio::test]
async fn seed_permissions_continues_past_a_failing_tenant() {
    let Ok(admin_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
    let db = format!("rustango_t2156_{}", std::process::id());
    let drop_db = format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)");
    sqlx::query(&drop_db).execute(&admin).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE {db}"))
        .execute(&admin)
        .await
        .unwrap();
    let url = sibling_database_url(&admin_url, &db);
    let registry = sqlx::PgPool::connect(&url).await.unwrap();
    rustango::testkit::migrate_framework(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(registry.clone());

    // `t2156-broken` has no users table, so its FKs cannot be added.
    for slug in ["t2156-broken", "t2156-ok"] {
        run(
            &pools,
            &url,
            &["create-tenant", slug, "--mode", "schema", "--no-migrate"],
        )
        .await
        .unwrap();
    }
    let ok = Org::objects()
        .where_(Org::slug.eq("t2156-ok".to_owned()))
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
    assert!(err.contains("failed `t2156-broken`"), "{err}");

    drop(scoped);
    drop(pools);
    registry.close().await;
    sqlx::query(&drop_db).execute(&admin).await.unwrap();
}
