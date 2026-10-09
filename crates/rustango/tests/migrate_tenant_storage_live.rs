#![cfg(feature = "postgres")]
//! Integration test for the `migrate-tenant-storage` verb (item #58
//! in the future-feature backlog). Most tests use `--dry-run`; the
//! restore test needs `pg_dump` / `psql` on PATH.

#![cfg(all(feature = "tenancy", feature = "postgres"))]

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::sqlx::PgPool;
use rustango::sql::Auto;
use rustango::tenancy::{
    manage::run_with_writer, ChainSecretsResolver, Org, StorageMode, TenantPools,
};

use tokio::sync::Mutex;

#[path = "support/scratch_db.rs"]
mod scratch_db;
use scratch_db::ScratchDb;

/// Suite-wide lock. Every test in this file resets the shared PG
/// schema; under cargo's default parallel harness two tests would race
/// on PG's `pg_type_typname_nsp_index` / `pg_class_relname_nsp_index`
/// system-catalog uniques when both try to CREATE/DROP the same table
/// at once.
fn live_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        PgPool::connect(&url)
            .await
            .unwrap_or_else(|e| panic!("DATABASE_URL is set but unreachable ({url}): {e}")),
    )
}

async fn fresh(pool: &PgPool) {
    rustango::migrate::drop_all(pool).await.unwrap();
    rustango::migrate::apply_all(pool).await.unwrap();
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[tokio::test]
async fn migrate_tenant_storage_dry_run_prints_plan() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // Seed a schema-mode tenant.
    let mut org = Org {
        id: Auto::default(),
        slug: "acme_dryrun".into(),
        display_name: "ACME Dry Run".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("acme_dryrun".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();
    let dir = std::env::temp_dir(); // not used by this verb

    let mut buf = Vec::<u8>::new();
    let res = run_with_writer(
        &pools,
        &registry_url,
        &dir,
        args(&[
            "migrate-tenant-storage",
            "acme_dryrun",
            "--to",
            "database",
            "--database-url",
            "postgres://example:password@db.example.com/acme_dryrun",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await;
    res.unwrap();

    let output = String::from_utf8(buf).unwrap();
    assert!(
        output.contains("schema → database"),
        "expected mode-flip line: {output}"
    );
    assert!(
        output.contains("[dry-run] no changes"),
        "expected dry-run terminator: {output}"
    );
    assert!(
        output.contains("***@db.example.com"),
        "password should be redacted in printed plan: {output}"
    );

    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_same_mode_no_op() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut org = Org {
        id: Auto::default(),
        slug: "noop_tenant".into(),
        display_name: "No-op".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("noop_tenant".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "noop_tenant",
            "--to",
            "schema",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("already in `schema`"),
        "expected same-mode error: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_database_target_without_url() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let mut org = Org {
        id: Auto::default(),
        slug: "needs_url".into(),
        display_name: "Needs URL".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("needs_url".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "needs_url",
            "--to",
            "database",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("--database-url"),
        "expected validation error mentioning --database-url: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

#[tokio::test]
async fn migrate_tenant_storage_rejects_unknown_slug() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();

    let mut buf = Vec::<u8>::new();
    let err = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "ghost_tenant",
            "--to",
            "database",
            "--database-url",
            "postgres://x:y@h/d",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await
    .unwrap_err();

    assert!(
        format!("{err}").contains("not found"),
        "expected `not found` for unknown slug: {err}"
    );
    rustango::migrate::drop_all(&pool).await.unwrap();
}

/// #1864 — database → schema restores the rows into the target schema.
/// `psql -c` ignored the piped dump, so the schema stayed empty.
#[tokio::test]
async fn migrate_tenant_storage_restores_rows_into_a_schema() {
    let _g = live_lock().lock().await;
    let Some(shared) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if std::process::Command::new("pg_dump")
        .arg("--version")
        .output()
        .is_err()
    {
        assert!(std::env::var("CI").is_err(), "CI needs pg_dump and psql");
        eprintln!("skipping: pg_dump not on PATH");
        return;
    }
    let shared_before = extensions(&shared).await;
    let (registry, pool) = private_registry("rustango_t1864_reg").await;
    let registry_url = registry.url().to_owned();
    let src_db = ScratchDb::create(
        &std::env::var("DATABASE_URL").unwrap(),
        "rustango_t1864_src",
    )
    .await;
    let src_url = src_db.url().to_owned();
    // Stored as a secret reference: the move resolves it (#2384).
    std::env::set_var("RUSTANGO_T2384_SRC", &src_url);
    let mut org = Org {
        id: Auto::default(),
        slug: "t1864".into(),
        display_name: "Moved".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some("env://RUSTANGO_T2384_SRC".into()),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::with_secrets(pool.clone(), ChainSecretsResolver::standard());
    let migrate = |drain: &'static str| {
        let (pools, registry_url) = (&pools, &registry_url);
        async move {
            let mut out = Vec::<u8>::new();
            run_with_writer(
                pools,
                registry_url,
                std::path::Path::new("."),
                args(&[
                    "migrate-tenant-storage",
                    "t1864",
                    "--to",
                    "schema",
                    "--schema-name",
                    "t1864_moved",
                    "--drain-secs",
                    drain,
                ]),
                &mut out,
            )
            .await
            .map(|()| String::from_utf8(out).unwrap())
        }
    };
    let extension = |name: &'static str| {
        let pool = pool.clone();
        async move {
            rustango::sql::sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_extension WHERE extname = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap()
                == 1
        }
    };
    // A new registry lacks them, so the move has to create them (#2210).
    for ext in ["citext", "pg_trgm", "dblink", "earthdistance", "cube"] {
        assert!(!extension(ext).await, "the new registry has {ext}");
    }
    // An untrusted extension the tenant uses is refused up front (#2210).
    let src = PgPool::connect(&src_url).await.unwrap();
    for stmt in [
        "CREATE EXTENSION dblink",
        "CREATE EXTENSION earthdistance CASCADE",
        "CREATE TABLE places (id INT, at earth)",
    ] {
        sqlx_exec(&src, stmt).await;
    }
    let err = migrate("0").await.unwrap_err().to_string();
    assert!(
        err.contains("earthdistance") && err.contains("--allow-extension"),
        "{err}"
    );
    assert!(!extension("earthdistance").await && !extension("cube").await);
    sqlx_exec(&src, "DROP TABLE places").await;
    src.close().await;

    // No `rustango_users`: the smoke check fails and drops what it restored.
    let err = migrate("0").await.unwrap_err();
    assert!(err.to_string().contains("dropped"), "{err}");
    let left: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname = 't1864_moved'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0, "the failed restore left its schema");
    // Offline for the move, active again after a failed one (#2383).
    assert!(org_row(&pool, "t1864").await.active, "left inactive");
    // An inactive tenant is not activated by a failed move either.
    set_active(&pool, "t1864", false).await;
    assert!(migrate("0").await.is_err());
    assert!(
        !org_row(&pool, "t1864").await.active,
        "a failed move activated it"
    );
    set_active(&pool, "t1864", true).await;

    let src = PgPool::connect(&src_url).await.unwrap();
    // Extension types and opclasses move too (#2210).
    for stmt in [
        "CREATE EXTENSION IF NOT EXISTS citext",
        "CREATE EXTENSION IF NOT EXISTS pg_trgm",
        "CREATE TABLE rustango_users (id BIGSERIAL PRIMARY KEY, username CITEXT NOT NULL, \
         tags CITEXT[])",
        "CREATE INDEX users_trgm ON rustango_users USING gin (username gin_trgm_ops)",
        "INSERT INTO rustango_users (username) VALUES ('ann'), ('bob')",
    ] {
        sqlx_exec(&src, stmt).await;
    }
    src.close().await;
    // Known limit (#2383): a suspension made during the move is undone by it.
    let out = migrate("0").await.unwrap_or_else(|e| panic!("{e}"));
    assert!(org_row(&pool, "t1864").await.active, "left inactive");
    // Names the old database as stored, never resolved (#2384) or `purge-tenant` (#2382).
    let src_name = src_url.rsplit('/').next().unwrap();
    assert!(
        out.contains("the database at env://RUSTANGO_T2384_SRC; once")
            && out.contains("drop that database by hand"),
        "{out}"
    );
    assert!(!out.contains(src_name), "printed the resolved URL: {out}");
    assert_eq!(out.matches("purge-tenant").count(), 1, "{out}");
    assert!(out.contains("Not with `purge-tenant`"), "{out}");
    assert!(extension("citext").await && extension("pg_trgm").await);
    assert!(
        !extension("dblink").await && !extension("cube").await,
        "an unused extension was installed"
    );

    let names: Vec<(String,)> = rustango::sql::sqlx::query_as(
        "SELECT username::text FROM t1864_moved.rustango_users WHERE username = 'ANN' \
         OR username = 'Bob' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(names, [("ann".to_owned(),), ("bob".to_owned(),)]);
    let staging: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_database WHERE datname LIKE 'rustango_stage_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(staging, 0, "staging database left behind");
    let moved: Vec<Org> = Org::objects()
        .where_(Org::slug.eq("t1864".to_owned()))
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(moved[0].storage_mode, StorageMode::Schema.as_str());
    assert_eq!(moved[0].schema_name.as_deref(), Some("t1864_moved"));
    pool.close().await;
    assert_eq!(
        extensions(&shared).await,
        shared_before,
        "shared DB touched"
    );
}

/// #2189 — schema → database lands the rows in the new database's `public`.
#[tokio::test]
async fn migrate_tenant_storage_restores_rows_into_a_database() {
    let _g = live_lock().lock().await;
    let Some(shared) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    if std::process::Command::new("pg_dump")
        .arg("--version")
        .output()
        .is_err()
    {
        assert!(std::env::var("CI").is_err(), "CI needs pg_dump and psql");
        eprintln!("skipping: pg_dump not on PATH");
        return;
    }
    let shared_before = extensions(&shared).await;
    let (registry, pool) = private_registry("rustango_t2189_reg").await;
    let registry_url = registry.url().to_owned();
    let dst_db = ScratchDb::create(
        &std::env::var("DATABASE_URL").unwrap(),
        "rustango_t2189_dst",
    )
    .await;
    // Extension types and opclasses move too (#2210).
    for stmt in [
        "CREATE EXTENSION IF NOT EXISTS citext",
        "CREATE EXTENSION IF NOT EXISTS pg_trgm",
        "CREATE SCHEMA t2189_ext",
        "CREATE EXTENSION earthdistance WITH SCHEMA t2189_ext CASCADE",
        "CREATE SCHEMA t2189_src",
        // In the tenant's own schema: the dump creates it (#2386).
        "CREATE EXTENSION hstore WITH SCHEMA t2189_src",
        "CREATE TABLE t2189_src.places (id INT, at t2189_ext.earth)",
        // Only an array of it: the extension is found through the element.
        "CREATE TABLE t2189_src.notes (id INT, kv t2189_src.hstore[])",
        "CREATE TABLE t2189_src.rustango_users (id BIGSERIAL PRIMARY KEY, username CITEXT NOT NULL)",
        "CREATE INDEX users_trgm ON t2189_src.rustango_users USING gin (username gin_trgm_ops)",
        "INSERT INTO t2189_src.rustango_users (username) VALUES ('ann'), ('bob')",
    ] {
        sqlx_exec(&pool, stmt).await;
    }
    let dst_url = dst_db.url().to_owned();
    // Resolved to connect, stored as given (#2384).
    std::env::set_var("RUSTANGO_T2384_DST", &dst_url);
    let mut org = Org {
        id: Auto::default(),
        slug: "t2189".into(),
        display_name: "Moved".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: None,
        schema_name: Some("t2189_src".into()),
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::with_secrets(pool.clone(), ChainSecretsResolver::standard());
    let migrate = |drain: &'static str| {
        let (pools, registry_url) = (&pools, &registry_url);
        async move {
            let mut out = Vec::<u8>::new();
            run_with_writer(
                pools,
                registry_url,
                std::path::Path::new("."),
                args(&[
                    "migrate-tenant-storage",
                    "t2189",
                    "--to",
                    "database",
                    "--database-url",
                    "env://RUSTANGO_T2384_DST",
                    "--drain-secs",
                    drain,
                ]),
                &mut out,
            )
            .await
            .map(|()| String::from_utf8(out).unwrap())
        }
    };
    // A non-empty `public` is refused before anything moves.
    let dst = PgPool::connect(&dst_url).await.unwrap();
    sqlx_exec(&dst, "CREATE TABLE public.junk (id INT)").await;
    let err = migrate("0").await.unwrap_err().to_string();
    assert!(
        err.contains("must be empty") && err.contains("junk"),
        "{err}"
    );
    let left: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname = 't2189_src'",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert_eq!(left, 0, "a refused move restored something");
    sqlx_exec(&dst, "DROP TABLE public.junk").await;
    // An untrusted extension is refused only while the target lacks it (#2385).
    let err = migrate("0").await.unwrap_err().to_string();
    assert!(
        err.contains("earthdistance") && err.contains("--allow-extension"),
        "{err}"
    );
    // In another schema than the registry's: refused before the drain.
    for stmt in [
        "CREATE SCHEMA t2189_wrong",
        "CREATE EXTENSION earthdistance WITH SCHEMA t2189_wrong CASCADE",
    ] {
        sqlx_exec(&dst, stmt).await;
    }
    let started = std::time::Instant::now();
    let err = migrate("30").await.unwrap_err().to_string();
    assert!(
        err.contains("t2189_wrong") && err.contains("t2189_ext"),
        "{err}"
    );
    assert!(started.elapsed().as_secs() < 30, "refused after the drain");
    assert!(org_row(&pool, "t2189").await.active);
    for stmt in [
        "DROP EXTENSION earthdistance",
        "DROP EXTENSION cube",
        "DROP SCHEMA t2189_wrong",
        "CREATE SCHEMA t2189_ext",
        "CREATE EXTENSION earthdistance WITH SCHEMA t2189_ext CASCADE",
    ] {
        sqlx_exec(&dst, stmt).await;
    }

    // Interrupted while offline: back to active, nothing restored (#2383).
    let interrupt = async {
        wait_inactive(&pool, "t2189", 3).await;
        let pid = std::process::id().to_string();
        let sent = std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap();
        assert!(sent.success());
    };
    let (res, ()) = tokio::join!(migrate("5"), interrupt);
    let err = res.unwrap_err().to_string();
    assert!(err.contains("interrupted"), "{err}");
    assert!(org_row(&pool, "t2189").await.active, "left inactive");
    let left: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname = 't2189_src'",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert_eq!(left, 0, "an interrupted move restored something");
    dst.close().await;

    // Offline before the dump: a write made during the drain moves, and
    // an edit made meanwhile survives (#2383).
    let edit = async {
        wait_inactive(&pool, "t2189", 3).await;
        sqlx_exec(
            &pool,
            "INSERT INTO t2189_src.rustango_users (username) VALUES ('carol')",
        )
        .await;
        Org::objects()
            .where_(Org::slug.eq("t2189".to_owned()))
            .update()
            .set_typed(Org::display_name.set("Renamed".to_owned()))
            .execute_on(&pool)
            .await
            .unwrap();
    };
    let started = std::time::Instant::now();
    let (out, ()) = tokio::join!(migrate("3"), edit);
    let out = out.unwrap_or_else(|e| panic!("{e}"));
    assert!(started.elapsed().as_secs() >= 3, "no drain wait");
    assert!(out.contains("edit-tenant t2189 --activate"), "{out}");
    // The reference as given, never the resolved URL (#2384).
    let dst_name = dst_url.rsplit('/').next().unwrap();
    assert!(out.contains("target: env://RUSTANGO_T2384_DST"), "{out}");
    assert!(!out.contains(dst_name), "printed the resolved URL: {out}");
    // Names the old schema, never `purge-tenant` (#2382).
    assert!(
        out.contains("schema `t2189_src` on the registry")
            && out.contains("`DROP SCHEMA \"t2189_src\" CASCADE` there by hand"),
        "{out}"
    );
    assert_eq!(out.matches("purge-tenant").count(), 1, "{out}");
    assert!(out.contains("Not with `purge-tenant`"), "{out}");

    let dst = PgPool::connect(&dst_url).await.unwrap();
    let granted: bool = rustango::sql::sqlx::query_scalar(
        "SELECT has_schema_privilege('public', 'public', 'USAGE')",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert!(granted, "other roles lost USAGE on public");
    let hstore_at: String = rustango::sql::sqlx::query_scalar(
        "SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname = 'hstore'",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert_eq!(hstore_at, "public");
    let names: Vec<(String,)> = rustango::sql::sqlx::query_as(
        "SELECT username::text FROM rustango_users WHERE username = 'ANN' \
             OR username = 'Bob' OR username = 'carol' ORDER BY id",
    )
    .fetch_all(&dst)
    .await
    .unwrap();
    assert_eq!(
        names,
        [
            ("ann".to_owned(),),
            ("bob".to_owned(),),
            ("carol".to_owned(),)
        ]
    );
    let schemas: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname = 't2189_src'",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert_eq!(
        schemas, 0,
        "the old schema name is left in the new database"
    );
    dst.close().await;
    let moved: Vec<Org> = Org::objects()
        .where_(Org::slug.eq("t2189".to_owned()))
        .fetch_on(&pool)
        .await
        .unwrap();
    assert_eq!(moved[0].storage_mode, StorageMode::Database.as_str());
    assert!(moved[0].active, "left inactive");
    assert_eq!(moved[0].display_name, "Renamed", "the edit was overwritten");
    assert_eq!(
        moved[0].database_url.as_deref(),
        Some("env://RUSTANGO_T2384_DST")
    );
    pool.close().await;
    assert_eq!(
        extensions(&shared).await,
        shared_before,
        "shared DB touched"
    );
}

async fn set_active(pool: &PgPool, slug: &str, active: bool) {
    Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .update()
        .set_typed(Org::active.set(active))
        .execute_on(pool)
        .await
        .unwrap();
}

/// Poll until the move has taken `slug` offline.
async fn wait_inactive(pool: &PgPool, slug: &str, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while org_row(pool, slug).await.active {
        assert!(
            std::time::Instant::now() < deadline,
            "the tenant stayed active for the move"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn org_row(pool: &PgPool, slug: &str) -> Org {
    Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch_on(pool)
        .await
        .unwrap()
        .remove(0)
}

async fn sqlx_exec(pool: &PgPool, sql: &str) {
    rustango::sql::sqlx::query(sql).execute(pool).await.unwrap();
}

/// A migrated registry of the test's own: other suites' extensions on the
/// shared DB stay out of reach (#2223).
async fn private_registry(prefix: &str) -> (ScratchDb, PgPool) {
    let db = ScratchDb::create(&std::env::var("DATABASE_URL").unwrap(), prefix).await;
    let pool = PgPool::connect(db.url()).await.unwrap();
    rustango::migrate::apply_all(&pool).await.unwrap();
    (db, pool)
}

async fn extensions(pool: &PgPool) -> Vec<String> {
    rustango::sql::sqlx::query_scalar("SELECT extname::text FROM pg_extension ORDER BY 1")
        .fetch_all(pool)
        .await
        .unwrap()
}

// Suppress unused-import warning when the file's only consumer
// is the test runner.
#[allow(dead_code)]
fn _force_use_arc() {
    let _: Option<Arc<TenantPools>> = None;
}

/// Moving into a schema another tenant uses is refused before any copy (#2290).
#[tokio::test]
async fn migrate_tenant_storage_refuses_a_schema_another_tenant_uses() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    fresh(&pool).await;

    // `NULL` schema_name: the slug is the schema.
    let mut owner = Org {
        slug: "taken_schema".into(),
        display_name: "Owner".into(),
        storage_mode: StorageMode::Schema.as_str().into(),
        schema_name: None,
        ..rustango::testkit::org()
    };
    owner.insert(&pool).await.unwrap();
    let mut mover = Org {
        slug: "mover".into(),
        display_name: "Mover".into(),
        database_url: Some("postgres://example@db.example.com/mover".into()),
        ..rustango::testkit::org()
    };
    mover.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let registry_url = std::env::var("DATABASE_URL").unwrap();
    let mut buf = Vec::<u8>::new();
    let res = run_with_writer(
        &pools,
        &registry_url,
        &std::env::temp_dir(),
        args(&[
            "migrate-tenant-storage",
            "mover",
            "--to",
            "schema",
            "--schema-name",
            "taken_schema",
            "--dry-run",
        ]),
        &mut buf,
    )
    .await;
    let err = res.expect_err("a used schema must be refused");
    assert!(err.to_string().contains("already used"), "{err}");

    rustango::migrate::drop_all(&pool).await.unwrap();
}
