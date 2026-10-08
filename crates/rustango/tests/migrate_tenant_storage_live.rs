#![cfg(feature = "postgres")]
//! Integration test for the `migrate-tenant-storage` verb (item #58
//! in the future-feature backlog). Most tests use `--dry-run`; the
//! restore test needs `pg_dump` / `psql` on PATH.

#![cfg(all(feature = "tenancy", feature = "postgres"))]

use std::sync::Arc;

use rustango::core::Column as _;
use rustango::sql::sqlx::PgPool;
use rustango::sql::Auto;
use rustango::tenancy::{manage::run_with_writer, Org, StorageMode, TenantPools};

use tokio::sync::Mutex;

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
    let Some(pool) = pool().await else {
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
    fresh(&pool).await;
    let registry_url = std::env::var("DATABASE_URL").unwrap();
    let drop_src = "DROP DATABASE IF EXISTS rustango_t1864_src WITH (FORCE)";
    sqlx_exec(&pool, drop_src).await;
    sqlx_exec(&pool, "CREATE DATABASE rustango_t1864_src").await;
    sqlx_exec(&pool, "DROP SCHEMA IF EXISTS t1864_moved CASCADE").await;
    // So the move has to create them (#2210).
    for ext in ["citext", "pg_trgm", "dblink", "earthdistance", "cube"] {
        sqlx_exec(&pool, &format!("DROP EXTENSION IF EXISTS {ext} CASCADE")).await;
    }
    let (base, _) = registry_url.rsplit_once('/').unwrap();
    let src_url = format!("{base}/rustango_t1864_src");
    let mut org = Org {
        id: Auto::default(),
        slug: "t1864".into(),
        display_name: "Moved".into(),
        storage_mode: StorageMode::Database.as_str().into(),
        backend_kind: "postgres".to_owned(),
        database_url: Some(src_url.clone()),
        schema_name: None,
        host_pattern: None,
        port: None,
        path_prefix: None,
        ..rustango::testkit::org()
    };
    org.insert(&pool).await.unwrap();

    let pools = TenantPools::new(pool.clone());
    let migrate = || async {
        run_with_writer(
            &pools,
            &registry_url,
            std::path::Path::new("."),
            args(&[
                "migrate-tenant-storage",
                "t1864",
                "--to",
                "schema",
                "--schema-name",
                "t1864_moved",
            ]),
            &mut Vec::<u8>::new(),
        )
        .await
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
    // An untrusted extension the tenant uses is refused up front (#2210).
    let src = PgPool::connect(&src_url).await.unwrap();
    for stmt in [
        "CREATE EXTENSION dblink",
        "CREATE EXTENSION earthdistance CASCADE",
        "CREATE TABLE places (id INT, at earth)",
    ] {
        sqlx_exec(&src, stmt).await;
    }
    let err = migrate().await.unwrap_err().to_string();
    assert!(
        err.contains("earthdistance") && err.contains("--allow-extension"),
        "{err}"
    );
    assert!(!extension("earthdistance").await && !extension("cube").await);
    sqlx_exec(&src, "DROP TABLE places").await;
    src.close().await;

    // No `rustango_users`: the smoke check fails and drops what it restored.
    let err = migrate().await.unwrap_err();
    assert!(err.to_string().contains("dropped"), "{err}");
    let left: i64 = rustango::sql::sqlx::query_scalar(
        "SELECT count(*) FROM pg_namespace WHERE nspname = 't1864_moved'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0, "the failed restore left its schema");

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
    migrate().await.unwrap_or_else(|e| panic!("{e}"));
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

    sqlx_exec(&pool, "DROP SCHEMA t1864_moved CASCADE").await;
    drop_extensions(&pool, &["citext", "pg_trgm"]).await;
    sqlx_exec(&pool, drop_src).await;
    rustango::migrate::drop_all(&pool).await.unwrap();
}

/// #2189 — schema → database lands the rows in the new database's `public`.
#[tokio::test]
async fn migrate_tenant_storage_restores_rows_into_a_database() {
    let _g = live_lock().lock().await;
    let Some(pool) = pool().await else {
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
    fresh(&pool).await;
    let registry_url = std::env::var("DATABASE_URL").unwrap();
    let drop_dst = "DROP DATABASE IF EXISTS rustango_t2189_dst WITH (FORCE)";
    sqlx_exec(&pool, drop_dst).await;
    sqlx_exec(&pool, "CREATE DATABASE rustango_t2189_dst").await;
    sqlx_exec(&pool, "DROP SCHEMA IF EXISTS t2189_src CASCADE").await;
    // Extension types and opclasses move too (#2210).
    for stmt in [
        "CREATE EXTENSION IF NOT EXISTS citext",
        "CREATE EXTENSION IF NOT EXISTS pg_trgm",
        "CREATE EXTENSION IF NOT EXISTS hstore",
        "CREATE SCHEMA t2189_src",
        // Only an array of it: the extension is found through the element.
        "CREATE TABLE t2189_src.notes (id INT, kv hstore[])",
        "CREATE TABLE t2189_src.rustango_users (id BIGSERIAL PRIMARY KEY, username CITEXT NOT NULL)",
        "CREATE INDEX users_trgm ON t2189_src.rustango_users USING gin (username gin_trgm_ops)",
        "INSERT INTO t2189_src.rustango_users (username) VALUES ('ann'), ('bob')",
    ] {
        sqlx_exec(&pool, stmt).await;
    }
    let (base, _) = registry_url.rsplit_once('/').unwrap();
    let dst_url = format!("{base}/rustango_t2189_dst");
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

    let pools = TenantPools::new(pool.clone());
    let migrate = || async {
        run_with_writer(
            &pools,
            &registry_url,
            std::path::Path::new("."),
            args(&[
                "migrate-tenant-storage",
                "t2189",
                "--to",
                "database",
                "--database-url",
                &dst_url,
            ]),
            &mut Vec::<u8>::new(),
        )
        .await
    };
    // A non-empty `public` is refused before anything moves.
    let dst = PgPool::connect(&dst_url).await.unwrap();
    sqlx_exec(&dst, "CREATE TABLE public.junk (id INT)").await;
    let err = migrate().await.unwrap_err().to_string();
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
    dst.close().await;
    migrate().await.unwrap_or_else(|e| panic!("{e}"));

    let dst = PgPool::connect(&dst_url).await.unwrap();
    let granted: bool = rustango::sql::sqlx::query_scalar(
        "SELECT has_schema_privilege('public', 'public', 'USAGE')",
    )
    .fetch_one(&dst)
    .await
    .unwrap();
    assert!(granted, "other roles lost USAGE on public");
    let names: Vec<(String,)> = rustango::sql::sqlx::query_as(
        "SELECT username::text FROM rustango_users WHERE username = 'ANN' \
             OR username = 'Bob' ORDER BY id",
    )
    .fetch_all(&dst)
    .await
    .unwrap();
    assert_eq!(names, [("ann".to_owned(),), ("bob".to_owned(),)]);
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
    assert_eq!(moved[0].database_url.as_deref(), Some(dst_url.as_str()));

    sqlx_exec(&pool, "DROP SCHEMA t2189_src CASCADE").await;
    drop_extensions(&pool, &["citext", "pg_trgm", "hstore"]).await;
    sqlx_exec(&pool, drop_dst).await;
    rustango::migrate::drop_all(&pool).await.unwrap();
}

async fn sqlx_exec(pool: &PgPool, sql: &str) {
    rustango::sql::sqlx::query(sql).execute(pool).await.unwrap();
}

/// Drop extensions this test installed, unless another suite on the
/// shared test DB still uses them (e.g. `hstore_field_pg_live`).
async fn drop_extensions(pool: &PgPool, exts: &[&str]) {
    for ext in exts {
        let sql = format!("DROP EXTENSION IF EXISTS {ext}");
        let _ = rustango::sql::sqlx::query(&sql).execute(pool).await;
    }
}

// Suppress unused-import warning when the file's only consumer
// is the test runner.
#[allow(dead_code)]
fn _force_use_arc() {
    let _: Option<Arc<TenantPools>> = None;
}
