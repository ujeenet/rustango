//! Engine v2 under tenancy, live: schema mode on PG, DB mode on all three.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
#[cfg(feature = "postgres")]
use crate::migrate::v2::ledger::cache_all;
use crate::migrate::v2::ledger::Ledger;
use crate::sql::{raw_execute_pool, raw_query_pool, Pool};

/// Every test here shares servers: one at a time.
fn serial() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn file(id: &str, parents: &[&str], target: &str, ops: Value) -> Value {
    json!({"engine": 2, "format": 1, "id": id, "app": "project", "parents": parents,
           "target": target, "operations": ops})
}

fn put(dir: &Path, id: &str, v: &Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

fn items() -> Value {
    json!([{"op": "create_table", "v": 1, "table": {"name": "items", "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
                   {"column": "code", "ty": "string", "max_length": 40, "nullable": true}]},
        "indexes": [{"name": "items_code_key", "columns": ["code"], "unique": true}]}])
}

/// A database of the test's own on a server; dropped (and the roles the
/// test made) on drop.
#[cfg(any(feature = "postgres", feature = "mysql"))]
struct Scratch {
    admin: String,
    name: String,
    url: String,
    roles: Vec<String>,
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl Scratch {
    async fn create(admin: &str, tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("rv5_{tag}_{}_{n}", std::process::id());
        let a = Pool::connect(admin).await.unwrap();
        let sql = format!("CREATE DATABASE {}", a.dialect().quote_ident(&name));
        raw_execute_pool(&a, &sql, Vec::new()).await.unwrap();
        a.close().await;
        let (base, _) = admin.rsplit_once('/').unwrap();
        Self {
            admin: admin.to_owned(),
            url: format!("{base}/{name}"),
            name,
            roles: Vec::new(),
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
impl Drop for Scratch {
    fn drop(&mut self) {
        let (admin, name, roles, url) = (
            self.admin.clone(),
            self.name.clone(),
            self.roles.clone(),
            self.url.clone(),
        );
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut roles = roles;
                roles.extend(tenant_roles(&url).await);
                let Ok(a) = Pool::connect(&admin).await else {
                    return;
                };
                let d = a.dialect();
                let force = if d.kind() == crate::sql::DialectKind::Postgres {
                    " WITH (FORCE)"
                } else {
                    ""
                };
                let sql = format!("DROP DATABASE IF EXISTS {}{force}", d.quote_ident(&name));
                let _ = raw_execute_pool(&a, &sql, Vec::new()).await;
                for r in roles {
                    let sql = format!("DROP ROLE IF EXISTS {}", d.quote_ident(&r));
                    let _ = raw_execute_pool(&a, &sql, Vec::new()).await;
                }
                a.close().await;
            });
        })
        .join();
    }
}

/// The schema tenants' login roles with rights in the database at `url`.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn tenant_roles(url: &str) -> Vec<String> {
    if !url.starts_with("postgres") {
        return Vec::new();
    }
    let Ok(pool) = Pool::connect(url).await else {
        return Vec::new();
    };
    let roles: Vec<(String,)> = raw_query_pool(
        "SELECT r.rolname::text FROM pg_roles r WHERE r.rolname LIKE 'rgo\\_t\\_%' \
         AND EXISTS (SELECT 1 FROM pg_namespace n CROSS JOIN LATERAL aclexplode(n.nspacl) a \
                     WHERE a.grantee = r.oid)",
        Vec::new(),
        &pool,
    )
    .await
    .unwrap_or_default();
    pool.close().await;
    roles.into_iter().map(|r| r.0).collect()
}

async fn count(pool: &Pool, sql: &str) -> i64 {
    let r: Vec<(i64,)> = raw_query_pool(sql, Vec::new(), pool).await.unwrap();
    r[0].0
}

#[cfg(feature = "postgres")]
async fn exists_in(pool: &Pool, schema: &str, table: &str) -> bool {
    count(
        pool,
        &format!(
            "SELECT COUNT(*) FROM information_schema.tables \
             WHERE table_schema = '{schema}' AND table_name = '{table}'"
        ),
    )
    .await
        > 0
}

/// A PG registry in its own database, `migrations/` beside it.
#[cfg(feature = "postgres")]
struct PgFleet {
    db: Scratch,
    pg: sqlx::PgPool,
    root: tempfile::TempDir,
}

#[cfg(feature = "postgres")]
impl PgFleet {
    async fn new(tag: &str) -> Option<Self> {
        let admin = crate::testkit::matrix::live_url("DATABASE_URL", "postgres")?;
        let db = Scratch::create(&admin, tag).await;
        let pg = sqlx::PgPool::connect(&db.url).await.unwrap();
        Some(Self {
            db,
            pg,
            root: tempfile::tempdir().unwrap(),
        })
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("migrations")
    }

    fn registry(&self) -> Pool {
        self.pg.clone().into()
    }

    fn pools(&self) -> TenantPools<sqlx::Postgres> {
        TenantPools::new(self.pg.clone())
    }

    async fn fleet(&self, fo: &FleetOptions) -> Result<FleetReport, TenancyError> {
        run_fleet(&self.pools(), &self.dir(), &self.db.url, fo).await
    }

    /// The registry chain once (it makes the orgs table), then schema orgs.
    async fn with_orgs(&self, slugs: &[&str]) {
        std::fs::create_dir_all(self.dir()).unwrap();
        self.fleet(&FleetOptions::default()).await.unwrap();
        for slug in slugs {
            self.add_org(slug).await;
        }
    }

    /// A schema org `slug` in schema `s_<slug>`.
    async fn add_org(&self, slug: &str) {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Schema;
            o.backend_kind = crate::tenancy::BackendKind::Postgres;
            o.database_url = None;
            o.schema_name = Some(format!("s_{slug}"));
            o.host_pattern = None;
            o.port = None;
            o.path_prefix = None;
        });
        org.insert(&self.registry()).await.unwrap();
    }

    async fn org(&self, slug: &str) -> Org {
        use crate::core::Column as _;
        use crate::sql::FetcherPool as _;
        Org::objects()
            .where_(Org::slug.eq(slug.to_owned()))
            .fetch(&self.registry())
            .await
            .unwrap()
            .remove(0)
    }
}

/// Schema mode: tenants with the same row ids stay apart; rolling one back
/// and re-applying it leaves the other as it was (8.6).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn schema_tenants_stay_apart() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("apart").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let reg = f.registry();
    for s in ["s_t1", "s_t2"] {
        raw_execute_pool(
            &reg,
            &format!("INSERT INTO {s}.items (id, code) VALUES (1, 'a')"),
            Vec::new(),
        )
        .await
        .unwrap();
    }
    // The registry's public schema got neither the table nor a tenant ledger row.
    assert!(!exists_in(&reg, "public", "items").await);
    let t1 = target(&f.pools(), &f.org("t1").await, &f.db.url)
        .await
        .unwrap();
    // The tenant's path is its schema alone: a registry table, named bare,
    // fails loudly instead of falling through to `public` (decision 1).
    let orgs = {
        use crate::core::Model as _;
        Org::SCHEMA.table
    };
    let stray: Result<Vec<(i64,)>, _> = raw_query_pool(
        &format!("SELECT COUNT(*) FROM {orgs}"),
        Vec::new(),
        &t1.pool,
    )
    .await;
    assert!(stray.is_err(), "{stray:?}");
    let back = Options {
        run_as: RunAs::Tenant,
        lock_scope: t1.scope.clone(),
        ..Options::default()
    };
    assert_eq!(
        run::downgrade(&t1.pool, &f.dir(), 1, &back)
            .await
            .unwrap()
            .rolled_back,
        ["0001_items"]
    );
    assert!(!exists_in(&reg, "s_t1", "items").await);
    assert_eq!(
        count(&reg, "SELECT COUNT(*) FROM s_t2.items WHERE id = 1").await,
        1
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t1.items").await, 0);
    assert_eq!(
        count(&reg, "SELECT COUNT(*) FROM s_t2.items WHERE id = 1").await,
        1
    );
}

/// `requires: registry:<id>` refuses the tenant file, and so the fleet,
/// until that registry migration is applied; then it runs (8.4).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn requires_waits_for_the_registry() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("requires").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    let flag = |name: &str| {
        json!([{"op": "create_table", "v": 1, "table": {"name": name, "scope": "registry",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}])
    };
    put(
        &f.dir(),
        "0001_reg",
        &file("0001_reg", &[], "registry", flag("reg_one")),
    );
    let mut t = file("0001_items", &[], "tenant", items());
    t["requires"] = json!(["registry:0002_reg"]);
    put(&f.dir(), "0001_items", &t);
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("refused");
    assert!(
        e.to_string().contains("apply registry `0002_reg` first"),
        "{e}"
    );
    assert!(!exists_in(&f.registry(), "s_t1", "items").await);
    put(
        &f.dir(),
        "0002_reg",
        &file("0002_reg", &["0001_reg"], "registry", flag("reg_two")),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.registry, ["0002_reg"]);
    assert!(exists_in(&f.registry(), "s_t1", "items").await);
}

/// The registry cache holds no row value: every text in it.
#[cfg(feature = "postgres")]
async fn cache_text(registry: &Pool) -> String {
    format!("{:?}", cache_all(registry).await.unwrap())
}

/// A failure in tenant 2 of 5: within `--max-failures 1` the fleet goes on;
/// the failed tenant's cache row holds the error class only, never the row
/// value in the driver's message (8.3).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_failed_tenant_does_not_stop_the_fleet() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("maxfail").await else {
        return;
    };
    let slugs = ["t1", "t2", "t3", "t4", "t5"];
    f.with_orgs(&slugs).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t2.items (id, code) VALUES (99, 'secret-xyz')",
        Vec::new(),
    )
    .await
    .unwrap();
    // Every tenant gets row 1; tenant 2's row 99 makes the cast fail with
    // the row's value in the driver's message.
    let insert = json!([{"op": "data_sql", "v": 1, "id": "seed_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}, {{c:code}}) SELECT 1, 'one' WHERE NOT EXISTS (SELECT 1 FROM {{t:items}} WHERE CAST({{c:code}} AS BIGINT) > 0)"},
        "down": {"any": "DELETE FROM {{t:items}} WHERE {{c:id}} = 1"}}]);
    put(
        &f.dir(),
        "0002_seed",
        &file("0002_seed", &["0001_items"], "tenant", insert),
    );
    let rep = f
        .fleet(&FleetOptions {
            max_failures: Some(1),
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    let failed: Vec<&str> = rep.failed().map(|t| t.slug.as_str()).collect();
    assert_eq!(failed, ["t2"], "{rep:?}");
    assert!(!rep.stopped);
    assert!(
        rep.failed()
            .all(|t| t.error.as_deref().is_some_and(|e| e.contains("secret-xyz"))),
        "the failure carries the value in the log only: {rep:?}"
    );
    for s in ["s_t1", "s_t3", "s_t4", "s_t5"] {
        assert_eq!(
            count(
                &reg,
                &format!("SELECT COUNT(*) FROM {s}.items WHERE id = 1")
            )
            .await,
            1
        );
    }
    let cache = cache_text(&reg).await;
    assert!(!cache.contains("secret-xyz"), "{cache}");
    assert!(cache.contains("\"failed\""), "{cache}");
}

/// `--max-failures 0`: the first failure stops the fleet; later tenants
/// do not run.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn max_failures_zero_stops_at_the_first() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("maxzero").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t1.items (id, code) VALUES (99, 'dup')",
        Vec::new(),
    )
    .await
    .unwrap();
    let insert = json!([{"op": "data_sql", "v": 1, "id": "seed_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}, {{c:code}}) VALUES (1, 'dup')"},
        "down": {"any": "DELETE FROM {{t:items}} WHERE {{c:id}} = 1"}}]);
    put(
        &f.dir(),
        "0002_seed",
        &file("0002_seed", &["0001_items"], "tenant", insert),
    );
    let rep = f
        .fleet(&FleetOptions {
            max_failures: Some(0),
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    assert!(rep.stopped, "{rep:?}");
    let ran: Vec<&str> = rep.tenants.iter().map(|t| t.slug.as_str()).collect();
    assert_eq!(ran, ["t1"], "{rep:?}");
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t2.items").await, 0);
}

/// A blocker in one tenant (here an edited applied file, as its ledger
/// sees it) refuses the whole fleet before any tenant changes (8.3).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_blocker_in_one_tenant_refuses_all() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("blocker").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        &format!(
            "UPDATE s_t3.{} SET checksum = 'edited'",
            Ledger::default_names().table
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    let other = json!([{"op": "create_table", "v": 1, "table": {"name": "other", "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]);
    put(
        &f.dir(),
        "0002_other",
        &file("0002_other", &["0001_items"], "tenant", other),
    );
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("blocked");
    assert!(
        e.to_string().contains("t3") && e.to_string().contains("blocked"),
        "{e}"
    );
    for s in ["s_t1", "s_t2", "s_t3"] {
        assert!(!exists_in(&reg, s, "other").await, "{s}");
    }
}

/// Two orchestrators: the second waits on the fleet lock, then finds
/// nothing to do (8.3).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_second_orchestrator_waits() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("orch").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let holder_pool: Pool = sqlx::PgPool::connect(&f.db.url).await.unwrap().into();
    let (held_tx, held) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        run::fleet_locked(&holder_pool, || async {
            held_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            Ok(())
        })
        .await
        .unwrap();
        holder_pool.close().await;
    });
    held.await.unwrap();
    let started = std::time::Instant::now();
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1300),
        "did not wait"
    );
    assert_eq!(rep.tenants[0].applied, ["0001_items"]);
    holder.await.unwrap();
}

/// With per-tenant login roles, the tenant login reads the ledger but
/// cannot write it (8.1).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn the_tenant_login_cannot_write_the_ledger() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("revoke").await else {
        return;
    };
    let secret = crate::session::SessionSecret::from_bytes(vec![42; 32]);
    let pools = TenantPools::new(f.pg.clone())
        .config(crate::tenancy::TenantPoolsConfig {
            schema_tenant_roles: true,
            ..crate::tenancy::TenantPoolsConfig::default()
        })
        .tenant_role_secret(secret);
    f.with_orgs(&["t1"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let rep = run_fleet(&pools, &f.dir(), &f.db.url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let role = pools.tenant_role("s_t1").await.unwrap().expect("roles on");
    let mut db = f.db;
    db.roles.push(role.name.clone());
    let opts: sqlx::postgres::PgConnectOptions = db.url.parse().unwrap();
    let mut conn = <sqlx::PgConnection as sqlx::Connection>::connect_with(&role.login(opts))
        .await
        .unwrap();
    let ledger = format!("s_t1.{}", Ledger::default_names().table);
    let read: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {ledger}"))
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(read.0 >= 1);
    let write = sqlx::query(&format!("UPDATE {ledger} SET pos = pos"))
        .execute(&mut conn)
        .await
        .expect_err("refused");
    assert!(write.to_string().contains("permission denied"), "{write}");
}

/// A separate, non-superuser migrator role runs the registry chain and
/// the fleet with only the grants the docs name (8.1).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_separate_migrator_role_runs_the_fleet() {
    let _g = serial().lock().await;
    let Some(mut f) = PgFleet::new("migrator").await else {
        return;
    };
    let role = format!("rv5_mig_{}", std::process::id());
    f.db.roles.push(role.clone());
    let reg = f.registry();
    for sql in [
        // CREATEROLE: schema tenants get login roles of their own.
        format!("CREATE ROLE {role} LOGIN CREATEROLE PASSWORD 'pw'"),
        format!("GRANT CREATE ON DATABASE {} TO {role}", f.db.name),
        format!("GRANT CREATE ON SCHEMA public TO {role}"),
        // As the owner, so tenant roles cannot create objects (the role check).
        format!("REVOKE TEMPORARY ON DATABASE {} FROM PUBLIC", f.db.name),
    ] {
        raw_execute_pool(&reg, &sql, Vec::new()).await.unwrap();
    }
    let (scheme, rest) = f.db.url.split_once("://").unwrap();
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let mig_url = format!("{scheme}://{role}:pw@{host}");
    let mig = sqlx::PgPool::connect(&mig_url).await.unwrap();
    let pools = TenantPools::new(mig.clone());
    std::fs::create_dir_all(f.dir()).unwrap();
    run_fleet(&pools, &f.dir(), &mig_url, &FleetOptions::default())
        .await
        .unwrap();
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "m1".into();
        o.display_name = "m1".into();
        o.storage_mode = StorageMode::Schema;
        o.backend_kind = crate::tenancy::BackendKind::Postgres;
        o.database_url = None;
        o.schema_name = Some("s_m1".into());
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&mig.clone().into()).await.unwrap();
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let rep = run_fleet(&pools, &f.dir(), &mig_url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert!(exists_in(&reg, "s_m1", "items").await);
    mig.close().await;
}

/// DB mode on `DB`: a registry and two tenant databases; the same row ids
/// in both; rolling one back leaves the other as it was (8.6).
async fn db_mode_tenants_stay_apart<DB: sqlx::Database>(
    registry_url: String,
    tenants: [String; 2],
    kind: crate::tenancy::BackendKind,
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let raw = sqlx::Pool::<DB>::connect(&registry_url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    for (i, url) in tenants.iter().enumerate() {
        let slug = format!("d{i}");
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.clone();
            o.display_name = slug.clone();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = kind;
            o.database_url = Some(url.clone());
            o.schema_name = None;
            o.host_pattern = None;
            o.port = None;
            o.path_prefix = None;
        });
        org.insert(&registry).await.unwrap();
    }
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let rep = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let conns: Vec<Pool> = {
        let mut v = Vec::new();
        for url in &tenants {
            v.push(Pool::connect(url).await.unwrap());
        }
        v
    };
    for c in &conns {
        raw_execute_pool(
            c,
            "INSERT INTO items (id, code) VALUES (1, 'a')",
            Vec::new(),
        )
        .await
        .unwrap();
    }
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let d0 = Org::objects()
        .where_(Org::slug.eq("d0".to_owned()))
        .fetch(&registry)
        .await
        .unwrap()
        .remove(0);
    let t0 = target(&pools, &d0, &registry_url).await.unwrap();
    let back = Options {
        run_as: RunAs::Tenant,
        ..Options::default()
    };
    assert_eq!(
        run::downgrade(&t0.pool, &dir, 1, &back)
            .await
            .unwrap()
            .rolled_back,
        ["0001_items"]
    );
    assert!(crate::migrate::v2::catalog::read_table(&conns[0], "items")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        count(&conns[1], "SELECT COUNT(*) FROM items WHERE id = 1").await,
        1
    );
    // Nothing of the tenants' reached the registry.
    assert!(crate::migrate::v2::catalog::read_table(&registry, "items")
        .await
        .unwrap()
        .is_none());
    for c in conns {
        c.close().await;
    }
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn db_mode_tenants_stay_apart_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "dbr").await,
        Scratch::create(&admin, "dba").await,
        Scratch::create(&admin, "dbb").await,
    );
    db_mode_tenants_stay_apart::<sqlx::Postgres>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::Postgres,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn db_mode_tenants_stay_apart_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "dbr").await,
        Scratch::create(&admin, "dba").await,
        Scratch::create(&admin, "dbb").await,
    );
    db_mode_tenants_stay_apart::<sqlx::MySql>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::MySql,
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn db_mode_tenants_stay_apart_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", dir.path().join(n).display());
    db_mode_tenants_stay_apart::<sqlx::Sqlite>(
        url("registry.db"),
        [url("a.db"), url("b.db")],
        crate::tenancy::BackendKind::Sqlite,
    )
    .await;
}

/// Boot (8.3): two replicas boot together on one registry; the fleet runs
/// once. A tenant whose migration fails does not stop the boot: it is in
/// maintenance and answers 503, the other tenant serves.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn boot_runs_the_fleet_and_parks_a_failed_tenant() {
    use tower::ServiceExt as _;
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite://{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let registry = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let root = tmp.path().join("project");
    let dir = root.join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let builder =
        || crate::server::Builder::from_pool(registry.clone(), registry_url.clone(), "localhost");
    let first = builder().migrate(&root).await.unwrap();
    let pools = first.pools().clone();
    for slug in ["ok", "bad"] {
        let mut o = Org::new(slug.to_owned());
        o.storage_mode = StorageMode::Database;
        o.backend_kind = crate::tenancy::BackendKind::Sqlite;
        o.database_url = Some(url(&format!("{slug}.db")));
        o.host_pattern = Some(format!("{slug}.localhost"));
        o.insert(&pools.registry_pool()).await.unwrap();
    }
    // `bad` already holds a table named like the one the file creates.
    let bad = Pool::connect(&url("bad.db")).await.unwrap();
    raw_execute_pool(&bad, "CREATE TABLE items (x INTEGER)", Vec::new())
        .await
        .unwrap();
    bad.close().await;
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let (a, b) = tokio::join!(builder().migrate(&root), builder().migrate(&root));
    let (a, _b) = (a.unwrap(), b.unwrap());
    let ok = Pool::connect(&url("ok.db")).await.unwrap();
    let ledger = Ledger::default_names().table;
    assert_eq!(
        count(
            &ok,
            &format!("SELECT COUNT(*) FROM {ledger} WHERE id = '0001_items'")
        )
        .await,
        1
    );
    ok.close().await;
    assert_eq!(a.pools().tenants_in_maintenance(), ["bad"]);
    let app = a
        .api(axum::Router::new().route(
            "/who",
            axum::routing::get(|t: crate::extractors::Tenant<sqlx::Sqlite>| async move {
                t.org.slug.clone()
            }),
        ))
        .into_router()
        .await
        .unwrap();
    let get = |host: &str| {
        axum::http::Request::builder()
            .uri("/who")
            .header("host", host)
            .body(axum::body::Body::empty())
            .unwrap()
    };
    let resp = app.clone().oneshot(get("ok.localhost")).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let resp = app.oneshot(get("bad.localhost")).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

/// The same tenant file, with `phase`.
#[cfg(feature = "postgres")]
fn phased(id: &str, parents: &[&str], phase: &str, ops: Value) -> Value {
    let mut f = file(id, parents, "tenant", ops);
    f["phase"] = json!(phase);
    f
}

#[cfg(feature = "postgres")]
fn table_op(name: &str) -> Value {
    json!([{"op": "create_table", "v": 1, "table": {"name": name, "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}])
}

/// A seed that fails on a tenant holding row 99 (a duplicate key).
#[cfg(feature = "postgres")]
fn seed_failing_on_99() -> Value {
    json!([{"op": "data_sql", "v": 1, "id": "seed_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}, {{c:code}}) VALUES (1, 'dup')"},
        "down": {"any": "DELETE FROM {{t:items}} WHERE {{c:id}} = 1"}}])
}

/// Decision 10: an inactive tenant is migrated by default; `--active-only`
/// leaves it out.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn an_inactive_tenant_is_migrated() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("inactive").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "UPDATE rustango_orgs SET active = FALSE WHERE slug = 't2'",
        Vec::new(),
    )
    .await
    .unwrap();
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    assert!(exists_in(&reg, "s_t2", "items").await);
    put(
        &f.dir(),
        "0002_other",
        &file("0002_other", &["0001_items"], "tenant", table_op("other")),
    );
    f.fleet(&FleetOptions {
        active_only: true,
        ..FleetOptions::default()
    })
    .await
    .unwrap();
    assert!(exists_in(&reg, "s_t1", "other").await);
    assert!(!exists_in(&reg, "s_t2", "other").await);
}

/// A tenant provisioned while the fleet runs is migrated before it ends:
/// the fleet re-reads the orgs until none is new (8.3).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenant_provisioned_mid_rollout_is_migrated() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("midroll").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    // Slow enough to provision a tenant while t1 migrates.
    let slow = json!([{"op": "create_table", "v": 1, "table": {"name": "items", "scope": "tenant",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}},
        {"op": "data_sql", "v": 1, "id": "slow_v1", "tables": {"write": ["items"]},
         "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}) SELECT 1 FROM pg_sleep(1.5)"},
         "down": {"any": "DELETE FROM {{t:items}}"}}]);
    put(
        &f.dir(),
        "0001_slow",
        &file("0001_slow", &[], "tenant", slow),
    );
    let reg = f.registry();
    let poller = {
        let reg = reg.clone();
        tokio::spawn(async move {
            for _ in 0..200 {
                let running = cache_all(&reg)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .any(|c| c.0 == "t1" && c.1 == "running");
                if running {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            false
        })
    };
    let fo = FleetOptions::default();
    let fleet = f.fleet(&fo);
    let provision = async {
        assert!(poller.await.unwrap(), "t1 never ran");
        f.add_org("t9").await;
    };
    let (rep, ()) = tokio::join!(fleet, provision);
    let rep = rep.unwrap();
    let ran: Vec<&str> = rep.tenants.iter().map(|t| t.slug.as_str()).collect();
    assert_eq!(ran, ["t1", "t9"], "{rep:?}");
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t9.items").await, 1);
}

/// Canaries go first; a canary failure stops the fleet and parks the rest.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_canary_failure_stops_the_fleet() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("canary").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t3.items (id, code) VALUES (1, 'x')",
        Vec::new(),
    )
    .await
    .unwrap();
    put(
        &f.dir(),
        "0002_seed",
        &file("0002_seed", &["0001_items"], "tenant", seed_failing_on_99()),
    );
    let rep = f
        .fleet(&FleetOptions {
            canaries: vec!["t3".into()],
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    assert!(rep.stopped, "{rep:?}");
    let ran: Vec<&str> = rep.tenants.iter().map(|t| t.slug.as_str()).collect();
    assert_eq!(ran, ["t3"], "{rep:?}");
    assert_eq!(rep.skipped, ["t1", "t2"]);
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t1.items").await, 0);
}

/// `--parallel 3` migrates every tenant, each once.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_parallel_fleet_migrates_every_tenant() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("parallel").await else {
        return;
    };
    let slugs = ["t1", "t2", "t3", "t4", "t5"];
    f.with_orgs(&slugs).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let rep = f
        .fleet(&FleetOptions {
            parallel: 3,
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let mut ran: Vec<&str> = rep.tenants.iter().map(|t| t.slug.as_str()).collect();
    ran.sort_unstable();
    assert_eq!(ran, slugs);
    for s in slugs {
        assert!(exists_in(&f.registry(), &format!("s_{s}"), "items").await);
    }
}

/// Contract files wait until every tenant reached its expand head (8.3):
/// one tenant failing an expand file keeps contract files off all of them;
/// `--phase expand` stops there; `--allow-mixed-heads` lifts the gate.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn contract_files_wait_for_every_expand_head() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("contract").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t2.items (id, code) VALUES (1, 'x')",
        Vec::new(),
    )
    .await
    .unwrap();
    put(
        &f.dir(),
        "0002_seed",
        &phased("0002_seed", &["0001_items"], "expand", seed_failing_on_99()),
    );
    put(
        &f.dir(),
        "0003_gone",
        &phased("0003_gone", &["0002_seed"], "contract", table_op("gone")),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(
        rep.failed().map(|t| t.slug.as_str()).collect::<Vec<_>>(),
        ["t2"]
    );
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t1.items").await, 1);
    assert!(!exists_in(&reg, "s_t1", "gone").await, "contract ran early");
    // `--phase expand` with every tenant able: still no contract file.
    raw_execute_pool(&reg, "DELETE FROM s_t2.items", Vec::new())
        .await
        .unwrap();
    f.fleet(&FleetOptions {
        expand_only: true,
        ..FleetOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t2.items").await, 1);
    assert!(!exists_in(&reg, "s_t1", "gone").await);
    // A plain run: every expand head is reached, contract files run.
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    for s in ["s_t1", "s_t2"] {
        assert!(exists_in(&reg, s, "gone").await, "{s}");
    }
    // `--allow-mixed-heads`: t1 takes a contract file while t2 fails its expand.
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t2.items (id, code) VALUES (2, 'y')",
        Vec::new(),
    )
    .await
    .unwrap();
    let seed2 = json!([{"op": "data_sql", "v": 1, "id": "seed2_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}, {{c:code}}) VALUES (2, 'y')"},
        "down": {"any": "DELETE FROM {{t:items}} WHERE {{c:id}} = 2"}}]);
    put(
        &f.dir(),
        "0004_seed",
        &phased("0004_seed", &["0003_gone"], "expand", seed2),
    );
    put(
        &f.dir(),
        "0005_gone",
        &phased("0005_gone", &["0004_seed"], "contract", table_op("gone2")),
    );
    f.fleet(&FleetOptions {
        allow_mixed_heads: true,
        ..FleetOptions::default()
    })
    .await
    .unwrap();
    assert!(exists_in(&reg, "s_t1", "gone2").await);
    assert!(!exists_in(&reg, "s_t2", "gone2").await);
}

/// The fleet run record (8.3): a run that died with another plan refuses
/// the next one until `--abandon-fleet-run`; the same plan resumes it.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn an_unfinished_fleet_run_with_another_plan_refuses() {
    use crate::migrate::v2::ledger::{fleet_runs, FLEET_RUNS};
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("record").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    let reg = f.registry();
    let runs = fleet_runs(&reg).await.unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        (runs[0].1.as_str(), runs[0].2.as_str()),
        ("up", "succeeded")
    );
    let dead = |id: &str, plan: &str| {
        format!(
            "INSERT INTO {FLEET_RUNS} (run_id, direction, plan_hash, state, started_at) \
             VALUES ('{id}', 'up', '{plan}', 'running', now())"
        )
    };
    raw_execute_pool(&reg, &dead("dead1", "an-older-plan"), Vec::new())
        .await
        .unwrap();
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("--abandon-fleet-run"), "{e}");
    assert!(!exists_in(&reg, "s_t1", "items").await);
    f.fleet(&FleetOptions {
        abandon: true,
        ..FleetOptions::default()
    })
    .await
    .unwrap();
    assert!(exists_in(&reg, "s_t1", "items").await);
    let plan = run::plan_hash(&f.dir()).unwrap();
    raw_execute_pool(&reg, &dead("dead2", &plan), Vec::new())
        .await
        .unwrap();
    f.fleet(&FleetOptions::default()).await.unwrap();
    let states: std::collections::BTreeMap<String, String> = fleet_runs(&reg)
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _, s)| (id, s))
        .collect();
    assert_eq!(states["dead1"], "abandoned");
    assert_eq!(states["dead2"], "resumed");
    assert!(!states.values().any(|s| s == "running"), "{states:?}");
}

/// A raw HTTP/1.1 GET: the status code.
#[cfg(all(feature = "sqlite", feature = "manage"))]
async fn http_status(port: u16, path: &str) -> Option<u16> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.ok()?;
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Boot binds before the fleet runs (6.11, 8.3): `/health` answers 200 and
/// `/ready` 503 while it waits for the fleet lock; the app opens after.
#[cfg(all(feature = "sqlite", feature = "manage"))]
#[tokio::test]
async fn boot_binds_before_the_fleet_runs() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let registry_url = format!(
        "sqlite://{}?mode=rwc",
        tmp.path().join("registry.db").display()
    );
    let registry = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let root = tmp.path().join("project");
    // A v2 project: the boot runs the fleet.
    put(
        &root.join("migrations"),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (held_tx, held) = tokio::sync::oneshot::channel();
    let (go_tx, go) = tokio::sync::oneshot::channel::<()>();
    let holder_pool: Pool = registry.clone().into();
    let holder = tokio::spawn(async move {
        run::fleet_locked(&holder_pool, || async {
            held_tx.send(()).unwrap();
            let _ = go.await;
            Ok(())
        })
        .await
        .unwrap();
    });
    held.await.unwrap();
    // The boot future is not `Send`, as `run-server`'s: drive it beside the client.
    let b = crate::server::Builder::from_pool(registry.clone(), registry_url, "localhost")
        .with_health();
    let addr = format!("127.0.0.1:{port}");
    let server = b.serve_after_fleet(&addr, root, std::time::Duration::from_secs(30));
    let client = async {
        let mut health = None;
        for _ in 0..100 {
            health = http_status(port, "/health").await;
            if health.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(health, Some(200));
        assert_eq!(http_status(port, "/ready").await, Some(503));
        go_tx.send(()).unwrap();
        holder.await.unwrap();
        let mut ready = None;
        for _ in 0..200 {
            ready = http_status(port, "/ready").await;
            if ready == Some(200) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(ready, Some(200));
    };
    tokio::select! {
        r = server => panic!("the server ended: {:?}", r.err().map(|e| e.to_string())),
        () = client => {}
    }
}

/// A registry table, and a tenant file with an FK to it.
fn put_fk_to_registry(dir: &Path) {
    let reg = json!([{"op": "create_table", "v": 1, "table": {"name": "plan", "scope": "registry",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]);
    put(dir, "0001_plan", &file("0001_plan", &[], "registry", reg));
    let t = json!([{"op": "create_table", "v": 1, "table": {"name": "invoice", "scope": "tenant",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false},
            {"column": "plan_id", "ty": "i64", "nullable": false,
             "fk": {"to": "plan", "on": "id", "name": "invoice_plan_fk"}}]},
        "indexes": [{"name": "invoice_plan_idx", "columns": ["plan_id"]}]}]);
    put(dir, "0001_invoice", &file("0001_invoice", &[], "tenant", t));
}

fn assert_fk_refused(e: &TenancyError) {
    let msg = e.to_string();
    assert!(
        msg.contains("MV2-E030") && msg.contains("references registry table `plan`"),
        "{msg}"
    );
}

/// The tenant→registry FK ban in schema mode: the fleet refuses, no
/// tenant table is made (8.5).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenant_fk_to_the_registry_is_refused_in_schema_mode() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("fkschema").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    put_fk_to_registry(&f.dir());
    assert_fk_refused(
        &f.fleet(&FleetOptions::default())
            .await
            .expect_err("refused"),
    );
    assert!(!exists_in(&f.registry(), "s_t1", "invoice").await);
}

/// The FK ban in DB mode on `DB`.
async fn fk_to_registry_refused_db_mode<DB: sqlx::Database>(
    registry_url: String,
    tenant_url: String,
    kind: crate::tenancy::BackendKind,
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let raw = sqlx::Pool::<DB>::connect(&registry_url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "d0".into();
        o.display_name = "d0".into();
        o.storage_mode = StorageMode::Database;
        o.backend_kind = kind;
        o.database_url = Some(tenant_url.clone());
        o.schema_name = None;
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&registry).await.unwrap();
    put_fk_to_registry(&dir);
    let e = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .expect_err("refused");
    assert_fk_refused(&e);
    let t = Pool::connect(&tenant_url).await.unwrap();
    assert!(crate::migrate::v2::catalog::read_table(&t, "invoice")
        .await
        .unwrap()
        .is_none());
    t.close().await;
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenant_fk_to_the_registry_is_refused_in_db_mode_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, a) = (
        Scratch::create(&admin, "fkr").await,
        Scratch::create(&admin, "fka").await,
    );
    fk_to_registry_refused_db_mode::<sqlx::Postgres>(
        r.url.clone(),
        a.url.clone(),
        crate::tenancy::BackendKind::Postgres,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn a_tenant_fk_to_the_registry_is_refused_in_db_mode_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, a) = (
        Scratch::create(&admin, "fkr").await,
        Scratch::create(&admin, "fka").await,
    );
    fk_to_registry_refused_db_mode::<sqlx::MySql>(
        r.url.clone(),
        a.url.clone(),
        crate::tenancy::BackendKind::MySql,
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_tenant_fk_to_the_registry_is_refused_in_db_mode_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", dir.path().join(n).display());
    fk_to_registry_refused_db_mode::<sqlx::Sqlite>(
        url("registry.db"),
        url("a.db"),
        crate::tenancy::BackendKind::Sqlite,
    )
    .await;
}

/// Adopt for a tenancy folder (9.2): every tenant behind the v1 head is
/// listed and the fleet blocks with the pre-switch message; caught up,
/// each target records its own checkpoint.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenancy_project_switches_and_behind_tenants_block() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("adopt").await else {
        return;
    };
    let dir = f.dir();
    crate::migrate::v2::run::live_tests::v1_tenancy_project(&dir, false);
    let pools = f.pools();
    crate::tenancy::migrate::migrate_registry(&pools, &dir)
        .await
        .unwrap();
    for slug in ["t1", "t2", "t3"] {
        f.add_org(slug).await;
    }
    crate::tenancy::migrate::migrate_tenants(&pools, &dir, &f.db.url)
        .await
        .unwrap();
    let reg = f.registry();
    let v1 = crate::migrate::LEDGER_TABLE;
    let behind = |s: &str, from: &str, to: &str| {
        format!("UPDATE {s}.{v1} SET name = '{to}' WHERE name = '{from}'")
    };
    for s in ["s_t2", "s_t3"] {
        raw_execute_pool(&reg, &behind(s, "0003_total", "0003_x"), Vec::new())
            .await
            .unwrap();
    }
    crate::migrate::v2::adopt::adopt(&dir, crate::migrate::v2::state::Backend::Postgres).unwrap();
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("blocked");
    let msg = e.to_string();
    assert!(
        msg.contains("t2:") && msg.contains("t3:") && !msg.contains("t1:"),
        "{msg}"
    );
    assert!(
        msg.contains("pre-switch release (engine v1) to `0003_total`"),
        "{msg}"
    );
    for s in ["s_t2", "s_t3"] {
        raw_execute_pool(&reg, &behind(s, "0003_x", "0003_total"), Vec::new())
            .await
            .unwrap();
    }
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert!(
        rep.tenants
            .iter()
            .all(|t| t.applied == ["0004_adopt_tenant"]),
        "{rep:?}"
    );
    let v2 = Ledger::default_names().table;
    assert_eq!(
        count(
            &reg,
            &format!("SELECT COUNT(*) FROM public.{v2} WHERE id = '0004_adopt_registry'")
        )
        .await,
        1
    );
}

/// The extension move (8.2): citext installed in `public` moves to
/// `extensions` and tenant compares stay case-insensitive; a needed
/// extension in a schema tenant paths leave out, or a missing one, refuses
/// before any tenant runs, naming the fix.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn extensions_move_to_their_own_schema() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("ext").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    let reg = f.registry();
    raw_execute_pool(&reg, "CREATE EXTENSION citext", Vec::new())
        .await
        .unwrap();
    let ext = |name: &str| json!([{"op": "create_extension", "v": 1, "name": name}]);
    put(
        &f.dir(),
        "0001_ext",
        &file("0001_ext", &[], "registry", ext("citext")),
    );
    let mut ci = items();
    ci.as_array_mut().unwrap().push(json!({"op": "data_sql", "v": 1, "id": "ci_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}) SELECT 1 WHERE CAST('ABC' AS citext) = CAST('abc' AS citext)"},
        "down": {"any": "DELETE FROM {{t:items}}"}}));
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", ci),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let home = |name: &str| {
        format!(
            "SELECT COUNT(*) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
             WHERE e.extname = '{name}' AND n.nspname = 'extensions'"
        )
    };
    assert_eq!(count(&reg, &home("citext")).await, 1);
    assert_eq!(count(&reg, "SELECT COUNT(*) FROM s_t1.items").await, 1);
    // Skipped: pg_trgm sits in a schema with a table of its own.
    for sql in [
        "CREATE SCHEMA junk",
        "CREATE TABLE junk.t (id INTEGER)",
        "CREATE EXTENSION pg_trgm SCHEMA junk",
    ] {
        raw_execute_pool(&reg, sql, Vec::new()).await.unwrap();
    }
    put(
        &f.dir(),
        "0002_trgm",
        &file("0002_trgm", &["0001_ext"], "registry", ext("pg_trgm")),
    );
    put(
        &f.dir(),
        "0002_other",
        &file("0002_other", &["0001_items"], "tenant", table_op("other")),
    );
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("skipped");
    assert!(
        e.to_string()
            .contains("ALTER EXTENSION pg_trgm SET SCHEMA extensions"),
        "{e}"
    );
    assert!(!exists_in(&reg, "s_t1", "other").await);
    // Missing.
    raw_execute_pool(&reg, "DROP EXTENSION pg_trgm", Vec::new())
        .await
        .unwrap();
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("missing");
    assert!(
        e.to_string()
            .contains("CREATE EXTENSION pg_trgm SCHEMA extensions"),
        "{e}"
    );
    raw_execute_pool(
        &reg,
        "CREATE EXTENSION pg_trgm SCHEMA extensions",
        Vec::new(),
    )
    .await
    .unwrap();
    f.fleet(&FleetOptions::default()).await.unwrap();
    assert!(exists_in(&reg, "s_t1", "other").await);
}

/// MySQL DB mode with a separate migrator login (design item 18): the
/// tenant login's table-level ledger grants are revoked, SELECT on the
/// ledger kept.
#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_tenant_login_loses_table_level_ledger_writes() {
    mysql_revoke_case(false).await;
}

/// A tenant login with database-level writes refuses the fleet before any
/// change, naming the fix (design item 18).
#[cfg(feature = "mysql")]
#[tokio::test]
async fn mysql_tenant_login_with_database_writes_refuses() {
    mysql_revoke_case(true).await;
}

#[cfg(feature = "mysql")]
async fn mysql_revoke_case(wide_case: bool) {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, mut a, mut b) = (
        Scratch::create(&admin, "rvr").await,
        Scratch::create(&admin, "rva").await,
        Scratch::create(&admin, "rvb").await,
    );
    let (narrow, wide) = (
        format!("rv5n_{}", std::process::id()),
        format!("rv5w_{}", std::process::id()),
    );
    let root = Pool::connect(&admin).await.unwrap();
    for sql in [
        format!("CREATE USER '{narrow}'@'%' IDENTIFIED BY 'pw'"),
        format!("CREATE USER '{wide}'@'%' IDENTIFIED BY 'pw'"),
        format!("GRANT ALL ON `{}`.* TO '{wide}'@'%'", b.name),
    ] {
        raw_execute_pool(&root, &sql, Vec::new()).await.unwrap();
    }
    a.roles.push(narrow.clone());
    b.roles.push(wide.clone());
    let (user, rest) = admin
        .strip_prefix("mysql://")
        .unwrap()
        .split_once('@')
        .unwrap();
    let (mig_user, mig_pw) = user.split_once(':').unwrap();
    let host = rest.split_once('/').unwrap().0;
    let tenant_url = |login: &str, db: &str| format!("mysql://{login}:pw@{host}/{db}");
    let raw = sqlx::MySqlPool::connect(&r.url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let fo = FleetOptions {
        migrator: Some(MigratorLogin {
            user: mig_user.to_owned(),
            password: mig_pw.to_owned(),
        }),
        ..FleetOptions::default()
    };
    run_fleet(&pools, &dir, &r.url, &fo).await.unwrap();
    let add = |slug: &str, url: String| {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = crate::tenancy::BackendKind::MySql;
            o.database_url = Some(url);
            o.schema_name = None;
            o.host_pattern = None;
            o.port = None;
            o.path_prefix = None;
        });
        let registry = registry.clone();
        async move { org.insert(&registry).await.unwrap() }
    };
    if wide_case {
        add("wide", tenant_url(&wide, &b.name)).await;
        put(
            &dir,
            "0001_items",
            &file("0001_items", &[], "tenant", items()),
        );
        let e = run_fleet(&pools, &dir, &r.url, &fo)
            .await
            .expect_err("refused");
        let msg = e.to_string();
        assert!(
            msg.contains("wide") && msg.contains("database-level") && msg.contains("table-level"),
            "{msg}"
        );
        assert!(crate::migrate::v2::catalog::read_table(
            &Pool::connect(&b.url).await.unwrap(),
            "items"
        )
        .await
        .unwrap()
        .is_none());
        registry.close().await;
        root.close().await;
        return;
    }
    add("narrow", tenant_url(&narrow, &a.name)).await;
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    // The narrow login alone: it migrates, then gets table-level ledger grants.
    let rep = run_fleet(&pools, &dir, &r.url, &fo).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let ledger = Ledger::default_names();
    for t in [ledger.table.as_str(), ledger.events.as_str()] {
        let sql = format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON `{}`.`{t}` TO '{narrow}'@'%'",
            a.name
        );
        raw_execute_pool(&root, &sql, Vec::new()).await.unwrap();
    }
    put(
        &dir,
        "0002_more",
        &file(
            "0002_more",
            &["0001_items"],
            "tenant",
            json!([{"op": "create_table", "v": 1,
            "table": {"name": "more", "scope": "tenant",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]),
        ),
    );
    let rep = run_fleet(&pools, &dir, &r.url, &fo).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let as_tenant = Pool::connect(&tenant_url(&narrow, &a.name)).await.unwrap();
    let read: Vec<(i64,)> = raw_query_pool(
        &format!("SELECT COUNT(*) FROM {}", ledger.table),
        Vec::new(),
        &as_tenant,
    )
    .await
    .unwrap();
    assert!(read[0].0 >= 2);
    for sql in [
        format!("UPDATE {} SET pos = pos", ledger.table),
        format!("DELETE FROM {}", ledger.events),
    ] {
        let e = raw_execute_pool(&as_tenant, &sql, Vec::new())
            .await
            .expect_err("revoked");
        assert!(e.to_string().contains("denied"), "{sql}: {e}");
    }
    as_tenant.close().await;
    registry.close().await;
    root.close().await;
}

/// Prune under tenancy (9.6): refused while a tenant ran part of what the
/// checkpoint replaces; allowed once it caught up with the old files.
/// `slugs` are orgs already on `pools`, the registry chain run.
#[cfg(any(feature = "postgres", feature = "sqlite"))]
async fn prune_waits_for_tenants<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    slugs: [&str; 2],
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let registry = pools.registry_pool();
    let org = |slug: &str| {
        let registry = registry.clone();
        let slug = slug.to_owned();
        async move {
            Org::objects()
                .where_(Org::slug.eq(slug))
                .fetch(&registry)
                .await
                .unwrap()
                .remove(0)
        }
    };
    let table = |name: &str| {
        json!([{"op": "create_table", "v": 1, "table": {"name": name, "scope": "tenant",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}])
    };
    put(dir, "0001_a", &file("0001_a", &[], "tenant", table("pr_a")));
    run_fleet(pools, dir, registry_url, &FleetOptions::default())
        .await
        .unwrap();
    put(
        dir,
        "0002_b",
        &file("0002_b", &["0001_a"], "tenant", table("pr_b")),
    );
    put(
        dir,
        "0003_c",
        &file("0003_c", &["0002_b"], "tenant", table("pr_c")),
    );
    // The pre-squash files, for the tenant that catches up later.
    let old = tempfile::tempdir().unwrap();
    let old_dir = old.path().join("migrations");
    std::fs::create_dir_all(&old_dir).unwrap();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        std::fs::copy(&p, old_dir.join(p.file_name().unwrap())).unwrap();
    }
    let run_one = |slug: &str, d: PathBuf| {
        let o = org(slug);
        async move {
            let t = target(pools, &o.await, registry_url).await.unwrap();
            let opts = Options {
                run_as: RunAs::Tenant,
                lock_scope: t.scope.clone(),
                ..Options::default()
            };
            run::migrate(&t.pool, &d, &opts).await.unwrap();
        }
    };
    run_one(slugs[0], dir.to_path_buf()).await;
    crate::migrate::v2::squash::squash(dir, dir, "project", "0001_a", "0003_c").unwrap();
    let gate = prune_gate(pools, dir, registry_url, "0003_squash")
        .await
        .unwrap();
    assert_eq!(
        gate,
        crate::migrate::v2::squash::PruneGate::Fleet {
            behind: vec![slugs[1].to_owned()]
        }
    );
    let e = crate::migrate::v2::squash::prune(dir, dir, "0003_squash", &gate).unwrap_err();
    assert!(e.msg.contains(slugs[1]), "{e}");
    run_one(slugs[1], old_dir).await;
    let gate = prune_gate(pools, dir, registry_url, "0003_squash")
        .await
        .unwrap();
    let removed = crate::migrate::v2::squash::prune(dir, dir, "0003_squash", &gate).unwrap();
    assert_eq!(removed.len(), 3);
    run_fleet(pools, dir, registry_url, &FleetOptions::default())
        .await
        .unwrap();
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn prune_waits_for_a_behind_schema_tenant() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("prune").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    prune_waits_for_tenants(&f.pools(), &f.db.url, &f.dir(), ["t1", "t2"]).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn prune_waits_for_a_behind_database_tenant() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let raw = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let pools = TenantPools::new(raw.clone());
    let dir = tmp.path().join("project").join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    for slug in ["d0", "d1"] {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = crate::tenancy::BackendKind::Sqlite;
            o.database_url = Some(url(&format!("{slug}.db")));
            o.schema_name = None;
            o.host_pattern = None;
            o.port = None;
            o.path_prefix = None;
        });
        org.insert(&Pool::from(raw.clone())).await.unwrap();
    }
    prune_waits_for_tenants(&pools, &registry_url, &dir, ["d0", "d1"]).await;
}
