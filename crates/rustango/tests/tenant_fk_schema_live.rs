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
    let args = rustango::testkit::routed_args(parts);
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
    migrate_logs_with_public_passkeys_opts(db_name, slug, existing, false).await
}

/// As above; `fail_index` fails the run after the tenant's passkey table is made.
#[cfg(all(feature = "passkey", feature = "runtime", feature = "testkit"))]
async fn migrate_logs_with_public_passkeys_opts(
    db_name: &str,
    slug: &str,
    existing: bool,
    fail_index: bool,
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
    if fail_index {
        // An event trigger fails the passkey table's index, after the table exists.
        for sql in [
            "CREATE FUNCTION t_fail_passkey_index() RETURNS event_trigger LANGUAGE plpgsql AS $$ \
             BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() \
             WHERE object_identity LIKE '%webauthn%') THEN RAISE EXCEPTION 'injected'; END IF; END $$",
            "CREATE EVENT TRIGGER t_fail_passkey_index ON ddl_command_end \
             WHEN TAG IN ('CREATE INDEX') EXECUTE FUNCTION t_fail_passkey_index()",
        ] {
            sqlx::query(sql).execute(&registry).await.unwrap();
        }
        run(&pools, &url, &["migrate-tenants"])
            .with_subscriber(subscriber)
            .await
            .expect_err("the injected index failure fails the run");
        sqlx::query("DROP EVENT TRIGGER t_fail_passkey_index")
            .execute(&registry)
            .await
            .unwrap();
        // The next run finds the table and must not be the first to stay quiet.
        let sink = buf.clone();
        let again = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .with_max_level(tracing::Level::WARN)
            .finish();
        run(&pools, &url, &["migrate-tenants"])
            .with_subscriber(again)
            .await
            .unwrap();
    } else {
        run(&pools, &url, &["migrate-tenants"])
            .with_subscriber(subscriber)
            .await
            .unwrap();
    }
    drop(pools);
    registry.close().await;
    Some(buf.contents())
}

/// A run that fails after making the tenant's passkey table still warns, as
/// no later run can tell the table is new.
#[cfg(all(feature = "passkey", feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn a_failed_migrate_still_warns_about_hidden_passkeys() {
    let Some(logs) =
        migrate_logs_with_public_passkeys_opts("rustango_t2518c", "t2518-fail", true, true).await
    else {
        return;
    };
    assert!(
        logs.contains("public.rustango_webauthn_credentials") && logs.contains("t2518-fail"),
        "no warning about hidden passkeys:\n{logs}"
    );
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

/// A scratch registry with the framework tables in `public` and an
/// unmigrated schema tenant `slug`, plus `extensions` installed in `public`.
#[cfg(all(feature = "runtime", feature = "testkit"))]
async fn unmigrated_schema_tenant(
    prefix: &str,
    slug: &str,
    extensions: &[&str],
) -> Option<(ScratchDb, sqlx::PgPool, TenantPools, Org)> {
    let admin_url = std::env::var("DATABASE_URL").ok()?;
    let db = ScratchDb::create(&admin_url, prefix).await;
    let url = db.url().to_owned();
    let registry = sqlx::PgPool::connect(&url).await.unwrap();
    for ext in extensions {
        sqlx::query(&format!(
            "CREATE EXTENSION IF NOT EXISTS {ext} SCHEMA public"
        ))
        .execute(&registry)
        .await
        .unwrap();
    }
    rustango::testkit::migrate_framework(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap();
    let pools = TenantPools::new(registry.clone());
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
    Some((db, registry, pools, org))
}

#[cfg(all(feature = "runtime", feature = "testkit"))]
async fn show_search_path(pool: &rustango::sql::Pool) -> String {
    sqlx::query_scalar("SHOW search_path")
        .fetch_one(pool.as_postgres().unwrap())
        .await
        .unwrap()
}

/// With no extension in `public`, a tenant's path leaves `public` out, so a
/// table only `public` has is unreachable through the tenant's pools.
#[cfg(all(feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn a_table_only_in_public_is_unreachable_through_a_tenant_pool() {
    let Some((db, registry, pools, org)) =
        unmigrated_schema_tenant("rustango_tstrict", "tstrict-acme", &[]).await
    else {
        return;
    };
    let scoped = pools.scoped_pool_dyn(&org).await.unwrap();
    let path = show_search_path(&scoped).await;
    assert!(
        !path.contains("public"),
        "public on the tenant's path: {path}"
    );
    let read = rustango::sql::raw_execute_pool(
        &scoped,
        "SELECT 1 FROM rustango_users LIMIT 1",
        Vec::new(),
    )
    .await;
    assert!(read.is_err(), "the tenant read public.rustango_users");
    let mut held = pools.acquire(&org).await.unwrap();
    let read = sqlx::query("SELECT 1 FROM rustango_users LIMIT 1")
        .execute(&mut **held)
        .await;
    assert!(
        read.is_err(),
        "a held connection read public.rustango_users"
    );
    drop(held);
    scoped.close().await;
    drop(pools);
    registry.close().await;
    drop(db);
}

/// An extension in `public` keeps it on the path: a tenant missing tables
/// `public` has is refused until `migrate-tenants` creates them.
#[cfg(all(feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn an_extension_in_public_refuses_a_tenant_missing_tables() {
    let Some((db, registry, pools, org)) =
        unmigrated_schema_tenant("rustango_trefuse", "trefuse-acme", &["citext"]).await
    else {
        return;
    };
    let err = pools.scoped_pool_dyn(&org).await.unwrap_err().to_string();
    assert!(err.contains("rustango_users"), "unexpected error: {err}");
    let err = pools.acquire(&org).await.err().expect("held pool refused");
    assert!(err.to_string().contains("rustango_users"), "{err}");

    // Migrate itself must not be refused: it creates those tables.
    run(&pools, db.url(), &["migrate-tenants"]).await.unwrap();
    let scoped = TenantPools::new(registry.clone())
        .scoped_pool_dyn(&org)
        .await
        .unwrap();
    let path = show_search_path(&scoped).await;
    assert!(path.contains("public"), "extension schema missing: {path}");
    scoped.close().await;
    drop(pools);
    registry.close().await;
    drop(db);
}

/// citext in `public` still resolves through a tenant pool, and compares
/// case-insensitively rather than falling back to text equality.
#[cfg(all(feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn citext_in_public_compares_case_insensitively_for_a_tenant() {
    let Some((db, registry, pools, org)) =
        unmigrated_schema_tenant("rustango_tcitext", "tcitext-acme", &["citext"]).await
    else {
        return;
    };
    run(&pools, db.url(), &["migrate-tenants"]).await.unwrap();
    let scoped = pools.scoped_pool_dyn(&org).await.unwrap();
    for sql in [
        "CREATE TABLE ci_probe (email citext)",
        "INSERT INTO ci_probe VALUES ('A@x.io')",
    ] {
        rustango::sql::raw_execute_pool(&scoped, sql, Vec::new())
            .await
            .unwrap();
    }
    let hits: i64 = sqlx::query_scalar("SELECT count(*) FROM ci_probe WHERE email = 'a@X.io'")
        .fetch_one(scoped.as_postgres().unwrap())
        .await
        .unwrap();
    assert_eq!(hits, 1, "citext compared case-sensitively");
    scoped.close().await;
    drop(pools);
    registry.close().await;
    drop(db);
}

/// An extension in tenant A's schema must not put A's schema on tenant B's
/// path: B would read A's tables.
#[cfg(all(feature = "runtime", feature = "testkit"))]
#[tokio::test]
async fn an_extension_in_another_tenants_schema_stays_off_the_path() {
    let Some((db, registry, pools, a)) =
        unmigrated_schema_tenant("rustango_textleak", "textleak-a", &[]).await
    else {
        return;
    };
    let url = db.url().to_owned();
    run(
        &pools,
        &url,
        &[
            "create-tenant",
            "textleak-b",
            "--mode",
            "schema",
            "--no-migrate",
        ],
    )
    .await
    .unwrap();
    run(&pools, &url, &["migrate-tenants"]).await.unwrap();
    let a_pool = pools.scoped_pool_dyn(&a).await.unwrap();
    let a_name: String = sqlx::query_scalar("SELECT current_schema()::text")
        .fetch_one(a_pool.as_postgres().unwrap())
        .await
        .unwrap();
    a_pool.close().await;
    let a_schema = format!("\"{a_name}\"");
    for sql in [
        format!("CREATE EXTENSION pg_trgm SCHEMA {a_schema}"),
        format!("CREATE TABLE {a_schema}.a_secret (id int)"),
    ] {
        sqlx::query(&sql).execute(&registry).await.unwrap();
    }
    let b = Org::objects()
        .where_(Org::slug.eq("textleak-b".to_owned()))
        .fetch(&rustango::sql::Pool::from(registry.clone()))
        .await
        .unwrap()
        .remove(0);
    let scoped = TenantPools::new(registry.clone())
        .scoped_pool_dyn(&b)
        .await
        .unwrap();
    let path = show_search_path(&scoped).await;
    assert!(!path.contains(&a_name), "A's schema on B's path: {path}");
    let read = rustango::sql::raw_execute_pool(&scoped, "SELECT 1 FROM a_secret", Vec::new()).await;
    assert!(read.is_err(), "tenant B read tenant A's table");
    scoped.close().await;
    drop(pools);
    registry.close().await;
    drop(db);
}
