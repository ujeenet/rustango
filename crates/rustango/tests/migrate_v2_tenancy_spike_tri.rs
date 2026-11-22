//! SPIKE S4 (migrations engine v2): tenancy path and locks. The v2 tenant
//! connection without `public`, extensions reached through an extension
//! schema, loud missing tables, the load scan, `LockScope` against v1's
//! lock on PG/MySQL/SQLite, and a fleet run that records a tenant failure
//! and goes on. Each test makes and drops its own databases. Throwaway.

#![cfg(all(
    feature = "migrate-v2-spike",
    feature = "tenancy",
    feature = "postgres"
))]

use rustango::migrate::v2_spike::lock::{try_lock, v1_hold, v1_try, LockScope};
use rustango::migrate::v2_spike::tenancy::{
    apply, load, run_fleet, FleetTenant, TenantFile, CACHE,
};
use rustango::migrate::v2_spike::tenant_sql::{Allowed, E031, E_UNKNOWN_TABLE};
use rustango::sql::{raw_execute_pool, raw_query_pool, Pool};
use std::time::Duration;

fn url_with_db(url: &str, db: &str) -> String {
    let (main, query) = url
        .split_once('?')
        .map_or((url, None), |(m, q)| (m, Some(q)));
    let base = &main[..main.rfind('/').expect("URL names a database")];
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

async fn exec(pool: &Pool, sql: &str) {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn count(pool: &Pool, sql: &str) -> i64 {
    let rows: Vec<(i64,)> = raw_query_pool(sql, Vec::new(), pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows[0].0
}

/// A fresh server database named `db`; returns its URL. `None` skips.
async fn fresh_db(var: &str, db: &str) -> Option<String> {
    let admin_url = std::env::var(var).ok()?;
    let admin = Pool::connect(&admin_url).await.expect("admin connect");
    let drop = if admin_url.starts_with("postgres") {
        format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)")
    } else {
        format!("DROP DATABASE IF EXISTS `{db}`")
    };
    exec(&admin, &drop).await;
    exec(&admin, &format!("CREATE DATABASE {db}")).await;
    admin.close().await;
    Some(url_with_db(&admin_url, db))
}

async fn drop_db(var: &str, db: &str) {
    let admin = Pool::connect(&std::env::var(var).unwrap()).await.unwrap();
    let sql = if matches!(admin, Pool::Postgres(_)) {
        format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)")
    } else {
        format!("DROP DATABASE IF EXISTS `{db}`")
    };
    exec(&admin, &sql).await;
    admin.close().await;
}

fn pg(pool: &Pool) -> &sqlx::PgPool {
    match pool {
        Pool::Postgres(p) => p,
        _ => unreachable!(),
    }
}

/// Registry tables and extensions in `public`, as a v1 install has them.
async fn registry_with_public_extensions(reg: &Pool) {
    exec(reg, "CREATE EXTENSION citext SCHEMA public").await;
    exec(reg, "CREATE EXTENSION pg_trgm SCHEMA public").await;
    exec(
        reg,
        "CREATE TABLE public.rustango_orgs (id SERIAL PRIMARY KEY, slug TEXT NOT NULL, schema_name TEXT)",
    )
    .await;
    exec(
        reg,
        "CREATE TABLE public.rustango_users (id SERIAL PRIMARY KEY, email citext)",
    )
    .await;
    exec(
        reg,
        "INSERT INTO public.rustango_users (email) VALUES ('Ann@X.com')",
    )
    .await;
}

async fn tenant_schemas(reg: &Pool, schemas: &[&str]) {
    for s in schemas {
        exec(reg, &format!("CREATE SCHEMA {s}")).await;
        exec(
            reg,
            &format!("INSERT INTO public.rustango_orgs (slug, schema_name) VALUES ('{s}', '{s}')"),
        )
        .await;
    }
}

async fn path_of(pool: &Pool) -> String {
    let rows: Vec<(String,)> = raw_query_pool("SHOW search_path", Vec::new(), pool)
        .await
        .unwrap();
    rows[0].0.clone()
}

// ------------------------------------------------- criterion 1: open(Refuse)

/// `public` holds registry tables and the extensions: the v2 path leaves it
/// off, lists both extensions as skipped, and refuses a file that needs one.
#[tokio::test]
async fn s4_pg_open_refuse_excludes_public_with_tables() {
    let db = "mv2s4_open";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["acme"]).await;

    let p = rustango::tenancy::spike_v2::open_refuse(pg(&reg), "acme")
        .await
        .unwrap();
    assert_eq!(
        p.schemas,
        vec!["acme".to_owned()],
        "public must stay off: {p:?}"
    );
    assert_eq!(
        p.skipped,
        vec![
            "citext (in public)".to_owned(),
            "pg_trgm (in public)".to_owned()
        ]
    );
    // v1's migrate path keeps `public` (registry FK targets, citext).
    let v1 = rustango::tenancy::spike_v2::open_v1_migrate(pg(&reg), "acme")
        .await
        .unwrap();
    assert!(v1.iter().any(|s| s == "public"), "{v1:?}");

    // A skipped extension a file needs: refused before any statement, with the fix.
    let needs = vec!["citext".to_owned()];
    let e = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), "acme", &needs)
        .await
        .err()
        .expect("citext in public must be refused");
    assert!(
        e.contains("ALTER EXTENSION citext SET SCHEMA extensions"),
        "{e}"
    );

    // Without the check, unqualified citext fails loudly...
    let (t, _) = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), "acme", &[])
        .await
        .unwrap();
    assert_eq!(path_of(&t).await, "acme");
    let e = raw_execute_pool(&t, "CREATE TABLE u (email citext)", Vec::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("type \"citext\" does not exist"), "{e}");
    // ...and the qualified spelling is refused at load (`public` is denied).
    let files = [TenantFile::new(
        "0001",
        &["CREATE TABLE u (email public.citext)"],
    )];
    let e = load(&files, &Allowed::new(&["extensions"], "public"), &["u"]).unwrap_err();
    assert!(e.contains(E031), "{e}");
    t.close().await;
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

/// Why "render extension types qualified" (design 8.2) is not enough: with
/// citext off the path, `=` on a `public.citext` column resolves to the
/// `text` operator through the implicit cast, so it turns case-sensitive.
#[tokio::test]
async fn s4_pg_qualified_citext_compares_as_text() {
    let db = "mv2s4_qualified";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["acme"]).await;
    let (t, _) = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), "acme", &[])
        .await
        .unwrap();
    exec(&t, "CREATE TABLE u (email public.citext UNIQUE)").await;
    exec(&t, "INSERT INTO u (email) VALUES ('Bob@X.com')").await;
    // The index keeps citext's opclass: uniqueness stays case-insensitive.
    assert!(
        raw_execute_pool(&t, "INSERT INTO u (email) VALUES ('bob@x.com')", Vec::new())
            .await
            .is_err()
    );
    // Comparisons do not.
    assert_eq!(
        count(&t, "SELECT count(*) FROM u WHERE email = 'BOB@x.com'").await,
        0
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM u WHERE email = 'BOB@x.com'::public.citext"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM u WHERE email OPERATOR(public.=) 'BOB@x.com'"
        )
        .await,
        1
    );
    t.close().await;
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

/// The fix: extensions in a schema of their own. A fresh install creates
/// them there; an existing one moves them with `ALTER EXTENSION .. SET
/// SCHEMA` (columns keep working: they reference types by oid). The tenant
/// path picks the schema up as extension-only, so nothing is qualified.
#[tokio::test]
async fn s4_pg_extensions_schema_reachable_without_public() {
    let db = "mv2s4_extschema";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["acme"]).await;
    // An existing tenant made by v1 with citext from `public`.
    exec(&reg, "CREATE TABLE acme.old (email public.citext UNIQUE)").await;
    exec(&reg, "INSERT INTO acme.old (email) VALUES ('Old@X.com')").await;

    // The move (an adopt/system step), plus the registry's own path.
    exec(&reg, "CREATE SCHEMA extensions").await;
    exec(&reg, "ALTER EXTENSION citext SET SCHEMA extensions").await;
    exec(&reg, "ALTER EXTENSION pg_trgm SET SCHEMA extensions").await;
    exec(
        &reg,
        &format!("ALTER DATABASE {db} SET search_path TO \"$user\", public, extensions"),
    )
    .await;

    let needs = vec!["citext".to_owned(), "pg_trgm".to_owned()];
    let (t, path) = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), "acme", &needs)
        .await
        .unwrap();
    assert_eq!(
        path.schemas,
        vec!["acme".to_owned(), "extensions".to_owned()]
    );
    assert!(path.skipped.is_empty(), "{path:?}");
    let file = TenantFile::new(
        "0001_users",
        &[
            "CREATE TABLE users (id SERIAL PRIMARY KEY, email citext UNIQUE, name TEXT)",
            "CREATE INDEX users_name_trgm ON users USING gin (name gin_trgm_ops)",
            "INSERT INTO users (email, name) VALUES ('Bob@X.com', 'bobby')",
            "UPDATE users SET name = 'hit' WHERE email = 'BOB@x.com'",
            "UPDATE old SET email = 'moved@x.com' WHERE email = 'OLD@x.com'",
        ],
    )
    .needs("citext")
    .needs("pg_trgm");
    load(
        std::slice::from_ref(&file),
        &Allowed::new(&["extensions"], "public"),
        &["users", "old"],
    )
    .unwrap();
    assert_eq!(
        apply(&t, &[file]).await.unwrap(),
        vec!["0001_users".to_owned()]
    );
    assert_eq!(
        count(&t, "SELECT count(*) FROM users WHERE name = 'hit'").await,
        1
    );
    assert_eq!(
        count(&t, "SELECT count(*) FROM old WHERE email = 'MOVED@X.COM'").await,
        1
    );
    assert_eq!(
        count(
            &t,
            "SELECT count(*) FROM users WHERE similarity(name, 'hit') > 0.5"
        )
        .await,
        1
    );
    assert!(raw_execute_pool(
        &t,
        "INSERT INTO users (email) VALUES ('bob@x.com')",
        Vec::new()
    )
    .await
    .is_err());
    t.close().await;
    reg.close().await;
    // A new registry session gets the database's path: citext `=` still folds case.
    let reg = Pool::connect(&url).await.unwrap();
    assert_eq!(
        count(
            &reg,
            "SELECT count(*) FROM rustango_users WHERE email = 'ann@x.com'"
        )
        .await,
        1
    );
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

// ------------------------------------------- criterion 2: missing table loud

/// `rustango_users` exists in `public` (registry), not in the tenant: v2
/// fails with 42P01 where v1's migrate pool silently reads `public`.
#[tokio::test]
async fn s4_pg_missing_tenant_table_fails_loudly() {
    let db = "mv2s4_missing";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["acme"]).await;

    let (t, _) = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), "acme", &[])
        .await
        .unwrap();
    let files = [TenantFile::new(
        "0001_touch_users",
        &["UPDATE rustango_users SET email = lower(email)"],
    )];
    let err = apply(&t, &files).await.unwrap_err();
    assert_eq!(err, ("0001_touch_users".to_owned(), "db:42P01".to_owned()));
    assert_eq!(
        count(
            &reg,
            "SELECT count(*) FROM public.rustango_users WHERE email = 'Ann@X.com'"
        )
        .await,
        1,
        "the registry row must be untouched"
    );
    t.close().await;

    // v1's migrate pool: the same statement lands on the registry table.
    let v1 = rustango::tenancy::spike_v2::v1_migrate_pool(&url, "acme")
        .await
        .unwrap();
    let v1: Pool = v1.into();
    assert_eq!(count(&v1, "SELECT count(*) FROM rustango_users").await, 1);
    v1.close().await;
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

// ------------------------------------------ criterion 3: load-time refusal

#[test]
fn s4_tenant_file_naming_registry_schema_refused_at_load() {
    let allowed = Allowed::new(&["extensions"], "registry");
    for sql in [
        "UPDATE public.rustango_orgs SET slug = 'x'",
        "INSERT INTO \"registry\".\"org\" (id) VALUES (1)",
        "DELETE FROM users WHERE org_id IN (SELECT id FROM registry.org)",
        "SELECT * FROM `registry_db`.`org`",
    ] {
        let files = [TenantFile::new("0002", &[sql])];
        let e = load(&files, &allowed, &["users"]).unwrap_err();
        assert!(e.starts_with("0002: ") && e.contains(E031), "{sql}: {e}");
    }
    let files = [TenantFile::new("0003", &["UPDATE {{t:org}} SET x = 1"])];
    assert!(load(&files, &allowed, &["users"])
        .unwrap_err()
        .contains(E_UNKNOWN_TABLE));
    let ok = [TenantFile::new(
        "0004",
        &["UPDATE users u SET email = lower(u.email)"],
    )];
    load(&ok, &allowed, &["users"]).unwrap();
}

// ------------------------------------------------ criteria 4 and 5: locks

async fn v1_holder(
    pool: Pool,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (taken_tx, taken_rx) = tokio::sync::oneshot::channel();
    let (rel_tx, rel_rx) = tokio::sync::oneshot::channel();
    let h = tokio::spawn(async move { v1_hold(pool, taken_tx, rel_rx).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(10), taken_rx)
        .await
        .unwrap()
        .unwrap();
    (rel_tx, h)
}

fn schema_scope(db: &str, s: &str) -> LockScope {
    LockScope::Schema {
        database: db.into(),
        schema: s.into(),
    }
}

/// v1 schema-tenant runs and v2 `LockScope::Schema` agree on the key and
/// exclude each other both ways; another tenant is not blocked.
#[tokio::test]
async fn s4_lock_scope_matches_v1_pg_schema() {
    let db = "mv2s4_lock_pg";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["t1", "t2"]).await;
    let v1_t1: Pool = rustango::tenancy::spike_v2::v1_migrate_pool(&url, "t1")
        .await
        .unwrap()
        .into();
    let v1_t2: Pool = rustango::tenancy::spike_v2::v1_migrate_pool(&url, "t2")
        .await
        .unwrap()
        .into();
    use rustango::migrate::v2_spike::lock::v1_pg_key;
    assert_eq!(
        v1_pg_key(pg(&v1_t1)).await.unwrap(),
        schema_scope(db, "t1").pg_key()
    );
    assert_eq!(
        v1_pg_key(pg(&v1_t2)).await.unwrap(),
        schema_scope(db, "t2").pg_key()
    );
    assert_eq!(
        v1_pg_key(pg(&reg)).await.unwrap(),
        LockScope::Registry(db.into()).pg_key()
    );

    // v1 holds t1: v2 cannot take t1, can take t2.
    let (release, h) = v1_holder(v1_t1.clone()).await;
    assert!(try_lock(&reg, &schema_scope(db, "t1"))
        .await
        .unwrap()
        .is_none());
    let t2 = try_lock(&reg, &schema_scope(db, "t2"))
        .await
        .unwrap()
        .expect("t2 free");
    t2.release().await;
    release.send(()).unwrap();
    h.await.unwrap();

    // v2 holds t1: v1 times out on t1, runs on t2.
    let held = try_lock(&reg, &schema_scope(db, "t1"))
        .await
        .unwrap()
        .expect("t1 free");
    assert!(!v1_try(&v1_t1, Duration::from_millis(300)).await.unwrap());
    assert!(v1_try(&v1_t2, Duration::from_millis(300)).await.unwrap());
    held.release().await;
    assert!(v1_try(&v1_t1, Duration::from_millis(300)).await.unwrap());
    for p in [v1_t1, v1_t2, reg] {
        p.close().await;
    }
    drop_db("DATABASE_URL", db).await;
}

/// One registry pool, K tenants: v1's session-derived key collapses them
/// (and onto the registry's key); `LockScope` gives K distinct keys, all
/// held at once, each still exclusive.
#[tokio::test]
async fn s4_lock_scope_one_pool_k_tenants_k_keys_pg() {
    let db = "mv2s4_lock_k";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    let tenants = ["t1", "t2", "t3", "t4", "t5"];
    for t in tenants {
        exec(&reg, &format!("CREATE SCHEMA {t}")).await;
    }
    use rustango::migrate::v2_spike::lock::v1_pg_key;
    let v1 = v1_pg_key(pg(&reg)).await.unwrap();
    let registry = LockScope::Registry(db.into()).pg_key();
    assert_eq!(
        v1, registry,
        "qualified SQL from the registry pool: one key for all"
    );
    let keys: std::collections::BTreeSet<i64> = tenants
        .iter()
        .map(|t| schema_scope(db, t).pg_key())
        .collect();
    assert_eq!(keys.len(), tenants.len());
    assert!(!keys.contains(&registry));

    let mut held = Vec::new();
    for t in tenants {
        held.push(
            try_lock(&reg, &schema_scope(db, t))
                .await
                .unwrap()
                .expect(t),
        );
    }
    let r = try_lock(&reg, &LockScope::Registry(db.into()))
        .await
        .unwrap()
        .expect("registry");
    assert!(try_lock(&reg, &schema_scope(db, "t3"))
        .await
        .unwrap()
        .is_none());
    r.release().await;
    for h in held {
        h.release().await;
    }
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

/// MySQL DB-mode tenants: v2 binds the target database name where v1 reads
/// `DATABASE()`, so the names match, and a lock connection on another
/// database (one pool, K tenants) still gets K names.
#[cfg(feature = "mysql")]
#[tokio::test]
async fn s4_lock_scope_matches_v1_mysql_db_mode() {
    let var = "MYSQL_TEST_URL";
    let dbs = ["mv2s4_my_reg", "mv2s4_my_t1", "mv2s4_my_t2", "mv2s4_my_t3"];
    let mut urls = Vec::new();
    for d in dbs {
        let Some(u) = fresh_db(var, d).await else {
            return;
        };
        urls.push(u);
    }
    let reg = Pool::connect(&urls[0]).await.unwrap();
    let t1 = Pool::connect(&urls[1]).await.unwrap();
    let t2 = Pool::connect(&urls[2]).await.unwrap();
    let db = |i: usize| LockScope::Database(dbs[i].into());

    let (release, h) = v1_holder(t1.clone()).await;
    // Lock connection made from the registry pool, name from the target.
    assert!(try_lock(&reg, &db(1)).await.unwrap().is_none());
    let mut held = Vec::new();
    for i in [2, 3] {
        held.push(try_lock(&reg, &db(i)).await.unwrap().expect("free"));
    }
    release.send(()).unwrap();
    h.await.unwrap();
    for x in held {
        x.release().await;
    }

    let held = try_lock(&reg, &db(2)).await.unwrap().expect("t2 free");
    assert!(!v1_try(&t2, Duration::from_millis(300)).await.unwrap());
    assert!(v1_try(&t1, Duration::from_millis(300)).await.unwrap());
    held.release().await;
    assert!(v1_try(&t2, Duration::from_millis(300)).await.unwrap());
    for p in [reg, t1, t2] {
        p.close().await;
    }
    for d in dbs {
        drop_db(var, d).await;
    }
}

/// SQLite: v1 locks `<main file>-migrate.lock` from `pragma_database_list`,
/// which resolves symlinks; v2 canonicalises the target path to match, so a
/// database file reached through a symlink still gets v1's lock file.
#[cfg(all(feature = "sqlite", unix))]
#[tokio::test]
async fn s4_lock_scope_matches_v1_sqlite() {
    let dir = std::env::temp_dir().join(format!("mv2s4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t1.db");
    std::fs::write(&file, b"").unwrap();
    let link = dir.join("link-t1.db");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let via_link = link.clone();
    let pool = Pool::connect(&format!("sqlite://{}?mode=rwc", via_link.display()))
        .await
        .unwrap();
    exec(&pool, "CREATE TABLE x (id INTEGER)").await;
    let Pool::Sqlite(sq) = &pool else {
        unreachable!()
    };
    let v1_file = rustango::migrate::v2_spike::lock::v1_sqlite_lock_file(sq)
        .await
        .unwrap()
        .unwrap();
    let canon = std::fs::canonicalize(&file).unwrap();
    assert_eq!(v1_file, format!("{}-migrate.lock", canon.display()));

    let scope = LockScope::Database(via_link.display().to_string());
    let (release, h) = v1_holder(pool.clone()).await;
    assert!(try_lock(&pool, &scope).await.unwrap().is_none());
    release.send(()).unwrap();
    h.await.unwrap();
    let held = try_lock(&pool, &scope).await.unwrap().expect("free");
    assert!(!v1_try(&pool, Duration::from_millis(300)).await.unwrap());
    held.release().await;
    assert!(v1_try(&pool, Duration::from_millis(300)).await.unwrap());
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------- 14.2 fleet runs

fn fleet_files() -> Vec<TenantFile> {
    vec![
        TenantFile::new(
            "0001_notes",
            &["CREATE TABLE notes (id INTEGER PRIMARY KEY)"],
        ),
        TenantFile::new("0002_profile", &["UPDATE profile SET n = n + 1"]),
    ]
}

async fn cache_rows(reg: &Pool) -> Vec<(String, String, Option<String>, Option<String>)> {
    raw_query_pool(
        &format!("SELECT slug, status, file_id, error_class FROM {CACHE} ORDER BY slug"),
        Vec::new(),
        reg,
    )
    .await
    .unwrap()
}

fn assert_fleet(
    report: &rustango::migrate::v2_spike::tenancy::FleetReport,
    rows: &[(String, String, Option<String>, Option<String>)],
    missing_class: &str,
) {
    let both = vec!["0001_notes".to_owned(), "0002_profile".to_owned()];
    assert_eq!(
        report.applied,
        vec![("t1".to_owned(), both.clone()), ("t3".to_owned(), both)]
    );
    assert_eq!(
        report.failed,
        vec![(
            "t2".to_owned(),
            "0002_profile".to_owned(),
            missing_class.to_owned()
        )]
    );
    let t2 = &rows[1];
    assert_eq!(
        (
            t2.0.as_str(),
            t2.1.as_str(),
            t2.2.as_deref(),
            t2.3.as_deref()
        ),
        ("t2", "failed", Some("0002_profile"), Some(missing_class))
    );
    assert!(rows[0].1 == "applied" && rows[2].1 == "applied", "{rows:?}");
}

/// Schema mode: t2 lacks `profile` (which `public` has): t2 fails loudly,
/// is recorded with its error class, t1 and t3 still migrate; a re-run after
/// the fix finishes t2 from its ledger.
#[tokio::test]
async fn s4_fleet_schema_failure_recorded_others_continue() {
    let db = "mv2s4_fleet";
    let Some(url) = fresh_db("DATABASE_URL", db).await else {
        return;
    };
    let reg = Pool::connect(&url).await.unwrap();
    registry_with_public_extensions(&reg).await;
    tenant_schemas(&reg, &["t1", "t2", "t3"]).await;
    exec(&reg, "CREATE TABLE public.profile (n INTEGER)").await;
    for s in ["t1", "t3"] {
        exec(&reg, &format!("CREATE TABLE {s}.profile (n INTEGER)")).await;
    }
    let mut tenants = Vec::new();
    for s in ["t1", "t2", "t3"] {
        let (pool, _) = rustango::migrate::v2_spike::tenancy::open_tenant(pg(&reg), s, &[])
            .await
            .unwrap();
        tenants.push(FleetTenant {
            slug: s.into(),
            scope: schema_scope(db, s),
            lock_pool: reg.clone(),
            pool,
        });
    }
    let report = run_fleet(&reg, &tenants, &fleet_files()).await.unwrap();
    assert_fleet(&report, &cache_rows(&reg).await, "db:42P01");
    assert_eq!(
        count(&reg, "SELECT count(*) FROM t2.mv2_s4_ledger").await,
        1
    );

    exec(&reg, "CREATE TABLE t2.profile (n INTEGER)").await;
    let again = run_fleet(&reg, &tenants, &fleet_files()).await.unwrap();
    assert!(again.failed.is_empty(), "{again:?}");
    assert_eq!(
        again.applied[1],
        ("t2".to_owned(), vec!["0002_profile".to_owned()])
    );
    for t in tenants {
        t.pool.close().await;
    }
    reg.close().await;
    drop_db("DATABASE_URL", db).await;
}

async fn db_mode_fleet(urls: &[String], scopes: &[LockScope], missing_class: &str) {
    let reg = Pool::connect(&urls[0]).await.unwrap();
    let mut tenants = Vec::new();
    for (i, slug) in ["t1", "t2", "t3"].iter().enumerate() {
        let pool = Pool::connect(&urls[i + 1]).await.unwrap();
        if *slug != "t2" {
            exec(&pool, "CREATE TABLE profile (n INTEGER)").await;
        }
        tenants.push(FleetTenant {
            slug: (*slug).into(),
            scope: scopes[i].clone(),
            lock_pool: pool.clone(),
            pool,
        });
    }
    let report = run_fleet(&reg, &tenants, &fleet_files()).await.unwrap();
    assert_fleet(&report, &cache_rows(&reg).await, missing_class);
    for t in tenants {
        t.pool.close().await;
    }
    reg.close().await;
}

/// DB mode on all three backends: the same per-tenant record and continue.
#[tokio::test]
async fn s4_fleet_db_mode_tri() {
    let names = [
        "mv2s4_dbm_reg",
        "mv2s4_dbm_t1",
        "mv2s4_dbm_t2",
        "mv2s4_dbm_t3",
    ];
    let scopes: Vec<LockScope> = names[1..]
        .iter()
        .map(|n| LockScope::Database((*n).into()))
        .collect();
    for (var, class) in [("DATABASE_URL", "db:42P01"), ("MYSQL_TEST_URL", "db:42S02")] {
        if std::env::var(var).is_err() {
            eprintln!("{var} not set — skipping that arm");
            continue;
        }
        let mut urls = Vec::new();
        for n in names {
            urls.push(fresh_db(var, n).await.unwrap());
        }
        db_mode_fleet(&urls, &scopes, class).await;
        for n in names {
            drop_db(var, n).await;
        }
    }
    #[cfg(feature = "sqlite")]
    {
        let dir = std::env::temp_dir().join(format!("mv2s4-dbm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<std::path::PathBuf> =
            names.iter().map(|n| dir.join(format!("{n}.db"))).collect();
        let urls: Vec<String> = files
            .iter()
            .map(|f| format!("sqlite://{}?mode=rwc", f.display()))
            .collect();
        for u in &urls {
            Pool::connect(u).await.unwrap().close().await;
        }
        let scopes: Vec<LockScope> = files[1..]
            .iter()
            .map(|f| LockScope::Database(f.display().to_string()))
            .collect();
        db_mode_fleet(&urls, &scopes, "db:1").await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
