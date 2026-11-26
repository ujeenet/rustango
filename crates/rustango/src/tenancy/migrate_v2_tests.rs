//! Engine v2 under tenancy, live: schema mode on PG, DB mode on all three.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::migrate::v2::ledger::{cache_all, Ledger};
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
struct Scratch {
    admin: String,
    name: String,
    url: String,
    roles: Vec<String>,
}

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
            let mut org = crate::testkit::org_with(|o| {
                o.id = crate::sql::Auto::default();
                o.slug = (*slug).into();
                o.display_name = (*slug).into();
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
