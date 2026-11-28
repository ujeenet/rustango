//! Engine v2 under tenancy, live: schema mode on PG, DB mode on all three.

use std::path::Path;
#[cfg(any(feature = "postgres", feature = "sqlite"))]
use std::path::PathBuf;

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
        &t1.pool().await,
    )
    .await;
    assert!(stray.is_err(), "{stray:?}");
    let back = super::tenant_opts(&t1);
    assert_eq!(
        run::downgrade(&t1.pool().await, &f.dir(), 1, &back)
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
    // Refused before any write: the registry chain did not run either.
    assert!(!exists_in(&f.registry(), "public", "reg_one").await);
    put(
        &f.dir(),
        "0002_reg",
        &file("0002_reg", &["0001_reg"], "registry", flag("reg_two")),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.registry, ["0001_reg", "0002_reg"]);
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
    let t2 = cache_all(&reg).await.unwrap();
    let t2 = t2.iter().find(|r| r.0 == "t2").unwrap();
    assert_eq!(
        t2.2.as_deref(),
        Some("0001_items"),
        "a failed tenant keeps its head"
    );
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
    // Role set-up outside a fleet run (`ensure-tenant-roles`) keeps it locked.
    pools.ensure_tenant_role("s_t1").await.unwrap();
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
    let back = super::tenant_opts(&t0);
    assert_eq!(
        run::downgrade(&t0.pool().await, &dir, 1, &back)
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
    let resp = app.clone().oneshot(get("bad.localhost")).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    // Fixed, and migrated by another process: it serves again, no restart.
    let bad = Pool::connect(&url("bad.db")).await.unwrap();
    raw_execute_pool(&bad, "DROP TABLE items", Vec::new())
        .await
        .unwrap();
    bad.close().await;
    let other = TenantPools::new(registry.clone());
    let rep = run_fleet(&other, &root, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let resp = app.oneshot(get("bad.localhost")).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
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
    assert_eq!(rep.tenants.len(), 2, "one line per tenant: {rep:?}");
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
    let plan = run::plan_hash(&crate::migrate::v2::load::load_project(&f.dir()).unwrap());
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
    let server = b.serve_after_fleet(&addr, root);
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
        msg.contains("behind the engine v1 head") && msg.contains("to reach `0003_total`"),
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
    // The 0.69.0 upgrade (13.1 row 7): every framework table taken over
    // with no DDL, in the registry and in each tenant schema.
    no_system_ddl(&reg).await;
    for slug in ["t1", "t2", "t3"] {
        let t = target(&pools, &f.org(slug).await, &f.db.url).await.unwrap();
        no_system_ddl(&t.pool().await).await;
    }
}

/// The system ledger here took over its framework tables: rows, and no
/// unit in any of them.
async fn no_system_ddl(pool: &Pool) {
    let rows = Ledger::system().rows(pool).await.unwrap();
    assert!(!rows.is_empty(), "no system rows");
    let ran: Vec<String> = rows
        .iter()
        .filter(|r| !r.unit_plan.is_empty())
        .map(|r| r.app.clone())
        .collect();
    assert!(ran.is_empty(), "system apps ran DDL: {ran:?}");
}

/// The 0.69.0 upgrade under tenancy in DB mode on `DB`: v1 registry and
/// tenant chains, adopt, then the fleet takes over every framework table
/// in the registry and in each tenant database with no DDL.
async fn upgrade_from_0_69_db_mode<DB: sqlx::Database>(
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
    crate::migrate::v2::run::live_tests::v1_tenancy_project(&dir, false);
    crate::tenancy::migrate::migrate_registry(&pools, &dir)
        .await
        .unwrap();
    for (i, url) in tenants.iter().enumerate() {
        let slug = format!("u{i}");
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
    let v1 = crate::tenancy::migrate::migrate_tenants_dyn(&pools, &dir, &registry_url)
        .await
        .unwrap();
    assert!(v1.tenants.iter().all(|t| t.error.is_none()), "{v1:?}");
    crate::migrate::v2::adopt::adopt(&dir, crate::migrate::v2::run::backend_of(&registry)).unwrap();
    let rep = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    no_system_ddl(&registry).await;
    for url in &tenants {
        let c = Pool::connect(url).await.unwrap();
        no_system_ddl(&c).await;
        c.close().await;
    }
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn upgrade_from_0_69_db_mode_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "upr").await,
        Scratch::create(&admin, "upa").await,
        Scratch::create(&admin, "upb").await,
    );
    upgrade_from_0_69_db_mode::<sqlx::Postgres>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::Postgres,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn upgrade_from_0_69_db_mode_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "upr").await,
        Scratch::create(&admin, "upa").await,
        Scratch::create(&admin, "upb").await,
    );
    upgrade_from_0_69_db_mode::<sqlx::MySql>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::MySql,
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn upgrade_from_0_69_db_mode_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", dir.path().join(n).display());
    upgrade_from_0_69_db_mode::<sqlx::Sqlite>(
        url("registry.db"),
        [url("a.db"), url("b.db")],
        crate::tenancy::BackendKind::Sqlite,
    )
    .await;
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
            let opts = super::tenant_opts(&t);
            run::migrate(&t.pool().await, &d, &opts).await.unwrap();
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
    // An org row this build cannot read counts as behind.
    let reg = pools.registry_pool();
    let was: Vec<(String,)> = raw_query_pool(
        &format!(
            "SELECT storage_mode FROM rustango_orgs WHERE slug = '{}'",
            slugs[1]
        ),
        Vec::new(),
        &reg,
    )
    .await
    .unwrap();
    let set = |v: &str| {
        format!(
            "UPDATE rustango_orgs SET storage_mode = '{v}' WHERE slug = '{}'",
            slugs[1]
        )
    };
    raw_execute_pool(&reg, &set("bogus"), Vec::new())
        .await
        .unwrap();
    let gate = prune_gate(pools, dir, registry_url, "0003_squash")
        .await
        .unwrap();
    let crate::migrate::v2::squash::PruneGate::Fleet { behind } = &gate else {
        panic!("{gate:?}");
    };
    assert!(
        behind.iter().any(|b| b.ends_with("(unreadable)")),
        "{gate:?}"
    );
    raw_execute_pool(&reg, &set(&was[0].0), Vec::new())
        .await
        .unwrap();
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

/// An `every_database` file: a table and, on PG, an extension.
fn marks() -> Value {
    json!([
        {"op": "create_extension", "v": 1, "name": "citext", "skipped_on": ["mysql", "sqlite"]},
        {"op": "create_table", "v": 1, "table": {"name": "marks", "scope": "every_database",
            "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}])
}

async fn ledger_ids(pool: &Pool) -> Vec<String> {
    Ledger::default_names()
        .rows(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

/// `every_database` (design item 20) in DB mode on `DB`: once on the
/// registry and once on each tenant database, before the tenant chain,
/// with each database's ledger recording it; PG tenants get the extension.
async fn every_database_runs_on_each_database<DB: sqlx::Database>(
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
        let slug = format!("e{i}");
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
        "0001_marks",
        &file("0001_marks", &[], "every_database", marks()),
    );
    put(
        &dir,
        "0002_items",
        &file("0002_items", &[], "tenant", items()),
    );
    let rep = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert_eq!(rep.registry, ["0001_marks"], "{rep:?}");
    assert!(
        rep.tenants
            .iter()
            .all(|t| t.applied == ["0001_marks", "0002_items"]),
        "{rep:?}"
    );
    assert_eq!(ledger_ids(&registry).await, ["0001_marks"]);
    let pg = kind == crate::tenancy::BackendKind::Postgres;
    for url in &tenants {
        let c = Pool::connect(url).await.unwrap();
        assert!(crate::migrate::v2::catalog::read_table(&c, "marks")
            .await
            .unwrap()
            .is_some());
        assert_eq!(ledger_ids(&c).await, ["0001_marks", "0002_items"]);
        if pg {
            let n = count(
                &c,
                "SELECT COUNT(*) FROM pg_extension WHERE extname = 'citext'",
            )
            .await;
            assert_eq!(n, 1, "the tenant database got the extension");
        }
        c.close().await;
    }
    // A second fleet run changes nothing.
    let again = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    assert!(again.registry.is_empty() && again.tenants.iter().all(|t| t.applied.is_empty()));
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn every_database_runs_on_each_database_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "edr").await,
        Scratch::create(&admin, "eda").await,
        Scratch::create(&admin, "edb").await,
    );
    every_database_runs_on_each_database::<sqlx::Postgres>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::Postgres,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn every_database_runs_on_each_database_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "edr").await,
        Scratch::create(&admin, "eda").await,
        Scratch::create(&admin, "edb").await,
    );
    every_database_runs_on_each_database::<sqlx::MySql>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::MySql,
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn every_database_runs_on_each_database_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", dir.path().join(n).display());
    every_database_runs_on_each_database::<sqlx::Sqlite>(
        url("registry.db"),
        [url("a.db"), url("b.db")],
        crate::tenancy::BackendKind::Sqlite,
    )
    .await;
}

/// Schema mode: tenants share the registry database, so an `every_database`
/// file runs once there, its extension in `extensions`, and never in a
/// tenant schema or a tenant ledger.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn every_database_runs_once_for_schema_tenants() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("edschema").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    put(
        &f.dir(),
        "0001_marks",
        &file("0001_marks", &[], "every_database", marks()),
    );
    put(
        &f.dir(),
        "0002_items",
        &file("0002_items", &[], "tenant", items()),
    );
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert_eq!(rep.registry, ["0001_marks"], "{rep:?}");
    assert!(
        rep.tenants.iter().all(|t| t.applied == ["0002_items"]),
        "{rep:?}"
    );
    let reg = f.registry();
    assert!(exists_in(&reg, "public", "marks").await);
    for s in ["s_t1", "s_t2"] {
        assert!(!exists_in(&reg, s, "marks").await, "{s}");
    }
    let home = "SELECT COUNT(*) FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
                WHERE e.extname = 'citext' AND n.nspname = 'extensions'";
    assert_eq!(count(&reg, home).await, 1);
}

/// Counts `sqlx::query` events process-wide (each test runs in its own process).
#[cfg(feature = "postgres")]
static QUERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "postgres")]
struct QueryCounter;

#[cfg(feature = "postgres")]
impl tracing::Subscriber for QueryCounter {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target().starts_with("sqlx::query")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, e: &tracing::Event<'_>) {
        if e.metadata().target().starts_with("sqlx::query") {
            QUERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The highest `count_sql` (a connection count) on `admin` while `work` ran.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn peak_connections<F: std::future::Future>(
    admin: &str,
    count_sql: &str,
    work: F,
) -> (F::Output, i64) {
    let watch = Pool::connect(admin).await.unwrap();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll = {
        let (watch, done, sql) = (watch.clone(), done.clone(), count_sql.to_owned());
        tokio::spawn(async move {
            let mut peak = 0;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                peak = peak.max(count(&watch, &sql).await);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            peak
        })
    };
    let out = work.await;
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let peak = poll.await.unwrap();
    watch.close().await;
    (out, peak)
}

/// Open connections follow `--parallel`, not the number of tenants.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn fleet_connections_follow_parallel_not_tenants() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let db = Scratch::create(&admin, "conns").await;
    let pg = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&db.url)
        .await
        .unwrap();
    let f = PgFleet {
        db,
        pg,
        root: tempfile::tempdir().unwrap(),
    };
    let slugs: Vec<String> = (0..16).map(|i| format!("t{i:02}")).collect();
    let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();
    f.with_orgs(&refs).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let sql = format!(
        "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = '{}'",
        f.db.name
    );
    let (rep, peak) = peak_connections(&admin, &sql, f.fleet(&FleetOptions::default())).await;
    let rep = rep.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert!(
        peak <= 8,
        "{peak} connections open for 16 tenants one by one"
    );
}

/// A fleet run with nothing to do costs a handful of statements per tenant.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_no_op_fleet_is_cheap_per_tenant() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("noop").await else {
        return;
    };
    let _ = tracing::subscriber::set_global_default(QueryCounter);
    let slugs: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
    let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();
    f.with_orgs(&refs).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    // The registry's own cost, with no tenant, is not the tenants'.
    let run = |n: usize| {
        let f = &f;
        async move {
            let before = QUERIES.load(std::sync::atomic::Ordering::SeqCst);
            let rep = f.fleet(&FleetOptions::default()).await.unwrap();
            assert_eq!(rep.tenants.len(), n);
            assert!(rep.tenants.iter().all(|t| t.applied.is_empty()), "{rep:?}");
            QUERIES.load(std::sync::atomic::Ordering::SeqCst) - before
        }
    };
    let all = run(8).await;
    assert!(all > 0, "the counter saw no statement");
    raw_execute_pool(
        &f.registry(),
        "UPDATE rustango_orgs SET active = FALSE WHERE slug <> 't0'",
        Vec::new(),
    )
    .await
    .unwrap();
    let fo = FleetOptions {
        active_only: true,
        ..FleetOptions::default()
    };
    let before = QUERIES.load(std::sync::atomic::Ordering::SeqCst);
    f.fleet(&fo).await.unwrap();
    let one = QUERIES.load(std::sync::atomic::Ordering::SeqCst) - before;
    let per_tenant = (all - one) / 7;
    eprintln!("no-op fleet: {all} statements for 8 tenants, {one} for 1, {per_tenant} per tenant");
    assert!(
        per_tenant <= 12,
        "{per_tenant} statements per up-to-date tenant"
    );
}

/// PG database mode with a separate migrator follows the schema-mode model:
/// objects the tenant login made move to the migrator, the login keeps rows
/// (also in tables the migrator makes later), the ledger REVOKE holds, and
/// the login can create nothing the migrator would run.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn pg_database_tenant_under_a_migrator_keeps_rows_only() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, mut a) = (
        Scratch::create(&admin, "pmr").await,
        Scratch::create(&admin, "pma").await,
    );
    let login = format!("rv5t_{}", std::process::id());
    a.roles.push(login.clone());
    let root = Pool::connect(&a.url).await.unwrap();
    for sql in [
        format!("CREATE ROLE {login} LOGIN PASSWORD 'pw'"),
        format!("REVOKE ALL ON DATABASE {} FROM PUBLIC", a.name),
        format!("GRANT ALL PRIVILEGES ON DATABASE {} TO {login}", a.name),
        format!("GRANT ALL ON SCHEMA public TO {login}"),
    ] {
        raw_execute_pool(&root, &sql, Vec::new()).await.unwrap();
    }
    let (scheme_user, rest) = admin.split_once('@').unwrap();
    let user = scheme_user
        .strip_prefix("postgres://")
        .or_else(|| scheme_user.strip_prefix("postgresql://"))
        .unwrap();
    let (mig_user, mig_pw) = user.split_once(':').unwrap();
    let host = rest.split_once('/').unwrap().0;
    let tenant_url = format!("postgres://{login}:pw@{host}/{}", a.name);
    let raw = sqlx::PgPool::connect(&r.url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &r.url, &FleetOptions::default())
        .await
        .unwrap();
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "pm".into();
        o.display_name = "pm".into();
        o.storage_mode = StorageMode::Database;
        o.backend_kind = crate::tenancy::BackendKind::Postgres;
        o.database_url = Some(tenant_url.clone());
        o.schema_name = None;
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&registry).await.unwrap();
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    // Before the migrator is configured: the tenant login owns what it made.
    let rep = run_fleet(&pools, &dir, &r.url, &FleetOptions::default())
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let owner = |t: &str| {
        format!("SELECT COUNT(*) FROM pg_tables WHERE tablename = '{t}' AND tableowner = '{login}'")
    };
    assert_eq!(count(&root, &owner("items")).await, 1);
    let fo = FleetOptions {
        migrator: Some(MigratorLogin {
            user: mig_user.to_owned(),
            password: mig_pw.to_owned(),
        }),
        ..FleetOptions::default()
    };
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
    let ledger = Ledger::default_names();
    for t in ["items", "more", ledger.table.as_str()] {
        assert_eq!(count(&root, &owner(t)).await, 0, "{t} still tenant-owned");
    }
    let as_tenant = Pool::connect(&tenant_url).await.unwrap();
    for sql in [
        "INSERT INTO items (id, code) VALUES (1, 'a')",
        "INSERT INTO more (id) VALUES (1)",
    ] {
        raw_execute_pool(&as_tenant, sql, Vec::new()).await.unwrap();
    }
    assert!(
        count(
            &as_tenant,
            &format!("SELECT COUNT(*) FROM {}", ledger.table)
        )
        .await
            >= 2
    );
    for sql in [
        format!("UPDATE {} SET pos = pos", ledger.table),
        "CREATE TABLE planted (id int)".to_owned(),
        "CREATE SCHEMA planted".to_owned(),
        "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 1'".to_owned(),
    ] {
        let e = raw_execute_pool(&as_tenant, &sql, Vec::new())
            .await
            .expect_err("refused");
        assert!(e.to_string().contains("permission denied"), "{sql}: {e}");
    }
    as_tenant.close().await;
    root.close().await;
    registry.close().await;
}

/// Provisioning on an engine v2 project runs the new tenant's v2 chain,
/// in schema mode and in database mode.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn provisioning_runs_the_v2_chain_in_schema_mode() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("prov").await else {
        return;
    };
    f.with_orgs(&[]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.add_org("p1").await;
    crate::tenancy::migrate_one_tenant(&f.pools(), &f.org("p1").await, &f.dir(), &f.db.url, None)
        .await
        .unwrap();
    assert!(exists_in(&f.registry(), "s_p1", "items").await);
    // A second call finds it current.
    crate::tenancy::migrate_one_tenant(&f.pools(), &f.org("p1").await, &f.dir(), &f.db.url, None)
        .await
        .unwrap();
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn provisioning_runs_the_v2_chain_in_database_mode() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let raw = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "p1".into();
        o.display_name = "p1".into();
        o.storage_mode = StorageMode::Database;
        o.backend_kind = crate::tenancy::BackendKind::Sqlite;
        o.database_url = Some(url("p1.db"));
        o.schema_name = None;
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&registry).await.unwrap();
    crate::tenancy::migrate_one_tenant(&pools, &org, &dir, &registry_url, None)
        .await
        .unwrap();
    let t = Pool::connect(&url("p1.db")).await.unwrap();
    assert!(crate::migrate::v2::catalog::read_table(&t, "items")
        .await
        .unwrap()
        .is_some());
    t.close().await;
    registry.close().await;
}

/// A schema tenant named as the engine's `extensions` schema is refused
/// before the extension move, and provisioning refuses the name.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenant_schema_named_extensions_is_refused() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("extname").await else {
        return;
    };
    f.with_orgs(&[]).await;
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "x1".into();
        o.display_name = "x1".into();
        o.storage_mode = StorageMode::Schema;
        o.backend_kind = crate::tenancy::BackendKind::Postgres;
        o.database_url = None;
        o.schema_name = Some("extensions".into());
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&f.registry()).await.unwrap();
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("migration engine owns"), "{e}");
    assert!(crate::tenancy::provision::validate_schema_name("extensions").is_err());
}

/// `requires: tenants:<id>` cannot hold while a tenant is unreachable: the
/// registry file waits and the refusal names the tenant.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn requires_tenants_waits_for_an_unreachable_tenant() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let raw = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    for (slug, db) in [
        ("t1", url("t1.db")),
        ("t2", "sqlite:/nonexistent/dir/t2.db?mode=ro".to_owned()),
    ] {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = crate::tenancy::BackendKind::Sqlite;
            o.database_url = Some(db.clone());
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
    assert!(
        rep.tenants
            .iter()
            .any(|t| t.slug == "t2" && t.error_class.as_deref() == Some("unreachable")),
        "{rep:?}"
    );
    let marks = json!([{"op": "create_table", "v": 1, "table": {"name": "reg_marks",
        "scope": "registry",
        "fields": [{"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]);
    let mut reg_file = file("0002_reg", &[], "registry", marks);
    reg_file["requires"] = json!(["tenants:0001_items"]);
    put(&dir, "0002_reg", &reg_file);
    let e = run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .expect_err("t2 was never read");
    assert!(e.to_string().contains("t2 (unreachable)"), "{e}");
    registry.close().await;
}

/// `manage <args>` from the tenancy CLI; its output.
async fn tenancy_cli<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: &[&str],
) -> Result<String, TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut out = Vec::new();
    let args = args.iter().map(|a| (*a).to_owned());
    crate::tenancy::manage::run_with_writer(pools, registry_url, dir, args, &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

async fn has_table(pool: &Pool, table: &str) -> bool {
    crate::migrate::v2::catalog::read_table(pool, table)
        .await
        .unwrap()
        .is_some()
}

/// `dbmigrate` from the tenancy CLI works per database (QA B2/B3): `apply`
/// puts tenant tables in each tenant and never in the registry; `status`
/// and `check --drift` see every database; `down` needs a target and rolls
/// tenants back before the registry; `--plan` writes nothing. `tenants`
/// are `(slug, pool on that tenant)`, the registry chain already applied.
async fn dbmigrate_routes_through_the_fleet<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    tenants: &[(&str, Pool)],
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let cli = |args: &'static [&'static str]| tenancy_cli(pools, registry_url, dir, args);
    let registry = pools.registry_pool();
    put(
        dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let flags = json!([{"op": "create_table", "v": 1, "table": {"name": "flags",
        "scope": "registry", "fields": [
            {"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]);
    put(
        dir,
        "0002_flags",
        &file("0002_flags", &[], "registry", flags),
    );
    let e = cli(&["dbmigrate", "down"]).await.expect_err("no target");
    assert!(
        e.to_string()
            .contains("--registry, --tenant <slug> or --all-tenants"),
        "{e}"
    );
    let e = cli(&["dbmigrate", "apply", "--bogus"])
        .await
        .expect_err("typo");
    assert!(e.to_string().contains("--bogus"), "{e}");
    // A plan of every database, nothing written.
    let plan = cli(&["dbmigrate", "apply", "--plan"]).await.unwrap();
    for (slug, _) in tenants {
        assert!(plan.contains(&format!("-- tenant {slug}")), "{plan}");
    }
    assert!(plan.contains("-- registry"), "{plan}");
    assert!(!has_table(&registry, "flags").await);
    for (_, t) in tenants {
        assert!(!has_table(t, "items").await);
    }
    let out = cli(&["dbmigrate", "apply"]).await.unwrap();
    assert!(out.contains("registry: applied 1 migration(s)"), "{out}");
    assert!(has_table(&registry, "flags").await);
    assert!(
        !has_table(&registry, "items").await,
        "a tenant table in the registry"
    );
    for (slug, t) in tenants {
        assert!(has_table(t, "items").await, "{slug}");
        assert!(!has_table(t, "flags").await, "{slug}: a registry table");
    }
    let status = cli(&["dbmigrate", "status"]).await.unwrap();
    assert!(
        status.contains("registry:\n  [X] project:0002_flags"),
        "{status}"
    );
    for (slug, _) in tenants {
        assert!(
            status.contains(&format!("tenant {slug}:\n  [X] project:0001_items")),
            "{status}"
        );
    }
    let drift = cli(&["dbmigrate", "check", "--drift"]).await.unwrap();
    assert!(drift.contains("registry: No drift."), "{drift}");
    let (first, rest) = tenants.split_first().unwrap();
    // `--plan` lists the rollback and runs none of it.
    let plan = tenancy_cli(
        pools,
        registry_url,
        dir,
        &["dbmigrate", "down", "--tenant", first.0, "--plan"],
    )
    .await
    .unwrap();
    assert!(plan.contains("DROP TABLE"), "{plan}");
    assert!(has_table(&first.1, "items").await);
    let out = tenancy_cli(
        pools,
        registry_url,
        dir,
        &["dbmigrate", "down", "--tenant", first.0],
    )
    .await
    .unwrap();
    assert!(out.contains("Rolled back 0001_items"), "{out}");
    assert!(!has_table(&first.1, "items").await);
    for (slug, t) in rest {
        assert!(has_table(t, "items").await, "{slug} kept its table");
    }
    assert!(has_table(&registry, "flags").await);
    let out = cli(&["dbmigrate", "down", "--all-tenants", "--registry"])
        .await
        .unwrap();
    let reg_at = out.find("registry: Rolled back 0002_flags").expect(&out);
    for (slug, t) in rest {
        let at = out
            .find(&format!("tenant {slug}: Rolled back 0001_items"))
            .expect(&out);
        assert!(at < reg_at, "tenants roll back before the registry: {out}");
        assert!(!has_table(t, "items").await);
    }
    assert!(!has_table(&registry, "flags").await);
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn dbmigrate_routes_through_the_fleet_in_schema_mode() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("cli").await else {
        return;
    };
    f.with_orgs(&["t1", "t2"]).await;
    let pools = f.pools();
    let mut tenants = Vec::new();
    for slug in ["t1", "t2"] {
        let t = target(&pools, &f.org(slug).await, &f.db.url).await.unwrap();
        tenants.push((slug, t.pool().await));
    }
    dbmigrate_routes_through_the_fleet(&pools, &f.db.url, &f.dir(), &tenants).await;
    let reg = f.registry();
    assert!(!exists_in(&reg, "public", "items").await);
}

/// The DB-mode set-up of [`dbmigrate_routes_through_the_fleet`].
async fn dbmigrate_routes_in_db_mode<DB: sqlx::Database>(
    registry_url: String,
    urls: [String; 2],
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
    let mut tenants = Vec::new();
    for (slug, url) in ["d0", "d1"].into_iter().zip(&urls) {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = kind;
            o.database_url = Some(url.clone());
            o.schema_name = None;
            o.host_pattern = None;
            o.port = None;
            o.path_prefix = None;
        });
        org.insert(&registry).await.unwrap();
        tenants.push((slug, Pool::connect(url).await.unwrap()));
    }
    dbmigrate_routes_through_the_fleet(&pools, &registry_url, &dir, &tenants).await;
    for (_, c) in tenants {
        c.close().await;
    }
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn dbmigrate_routes_through_the_fleet_in_db_mode_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "clr").await,
        Scratch::create(&admin, "cla").await,
        Scratch::create(&admin, "clb").await,
    );
    dbmigrate_routes_in_db_mode::<sqlx::Postgres>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::Postgres,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn dbmigrate_routes_through_the_fleet_in_db_mode_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let (r, a, b) = (
        Scratch::create(&admin, "clr").await,
        Scratch::create(&admin, "cla").await,
        Scratch::create(&admin, "clb").await,
    );
    dbmigrate_routes_in_db_mode::<sqlx::MySql>(
        r.url.clone(),
        [a.url.clone(), b.url.clone()],
        crate::tenancy::BackendKind::MySql,
    )
    .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn dbmigrate_routes_through_the_fleet_in_db_mode_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", dir.path().join(n).display());
    dbmigrate_routes_in_db_mode::<sqlx::Sqlite>(
        url("registry.db"),
        [url("a.db"), url("b.db")],
        crate::tenancy::BackendKind::Sqlite,
    )
    .await;
}

/// `dbmigrate --help` from the tenancy CLI dials no registry.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn dbmigrate_help_needs_no_registry() {
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://u:p@127.0.0.1:1/none")
        .unwrap();
    let pools = TenantPools::new(unreachable);
    let dir = tempfile::tempdir().unwrap();
    let out = tenancy_cli(
        &pools,
        "postgres://u:p@127.0.0.1:1/none",
        dir.path(),
        &["dbmigrate", "--help"],
    )
    .await
    .unwrap();
    assert!(out.contains("down [<n> | --to <id>]"), "{out}");
}

/// `dbmigrate apply --plan` under tenancy writes nothing: a tenant whose
/// schema does not exist yet is reported, not created; `status` neither.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_tenancy_plan_writes_nothing() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("plan").await else {
        return;
    };
    // The fleet ran before the org: its schema is not made yet.
    f.with_orgs(&["t1"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let pools = f.pools();
    let schema_made = || async {
        count(
            &f.registry(),
            "SELECT COUNT(*) FROM pg_namespace WHERE nspname = 's_t1'",
        )
        .await
            > 0
    };
    assert!(!schema_made().await);
    let plan = tenancy_cli(
        &pools,
        &f.db.url,
        &f.dir(),
        &["dbmigrate", "apply", "--plan"],
    )
    .await
    .unwrap();
    assert!(
        plan.contains("-- tenant t1\nschema missing, would create"),
        "{plan}"
    );
    assert!(!schema_made().await, "the plan created the schema");
    let status = tenancy_cli(&pools, &f.db.url, &f.dir(), &["dbmigrate", "status"])
        .await
        .unwrap();
    assert!(status.contains("tenant t1:\n  schema missing"), "{status}");
    assert!(!schema_made().await, "status created the schema");
    tenancy_cli(&pools, &f.db.url, &f.dir(), &["dbmigrate", "apply"])
        .await
        .unwrap();
    assert!(exists_in(&f.registry(), "s_t1", "items").await);
}

/// A tenant file whose one data unit sleeps `secs` on PG.
#[cfg(feature = "postgres")]
fn slow_file(id: &str, parent: &str, secs: f64) -> Value {
    let up = format!(
        "INSERT INTO {{{{t:items}}}} ({{{{c:id}}}}, {{{{c:code}}}}) \
         SELECT 77, 'slow' FROM (SELECT pg_sleep({secs})) s"
    );
    let ops = json!([{"op": "data_sql", "v": 1, "id": format!("{id}_v1"),
        "tables": {"write": ["items"]},
        "up": {"any": up},
        "down": {"any": "DELETE FROM {{t:items}} WHERE {{c:id}} = 77"}}]);
    file(id, &[parent], "tenant", ops)
}

/// Polls until `slug`'s registry cache row says `running`; `false` after 10 s.
#[cfg(feature = "postgres")]
async fn wait_running(reg: &Pool, slug: &str) -> bool {
    for _ in 0..500 {
        let rows = cache_all(reg).await.unwrap_or_default();
        if rows.iter().any(|r| r.0 == slug && r.1 == "running") {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// A shutdown during a fleet run: the tenant in flight finishes its unit and
/// runs no other, later tenants do not start, the record says `stopped`,
/// and the next run resumes (6.11).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_shutdown_stops_the_fleet_after_the_unit_in_flight() {
    use crate::migrate::v2::ledger::fleet_runs;
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("sigterm").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    put(
        &f.dir(),
        "0002_slow",
        &slow_file("0002_slow", "0001_items", 1.5),
    );
    put(
        &f.dir(),
        "0003_more",
        &file("0003_more", &["0002_slow"], "tenant", table_op("more")),
    );
    let reg = f.registry();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = {
        let (reg, stop) = (reg.clone(), stop.clone());
        tokio::spawn(async move {
            assert!(wait_running(&reg, "t1").await, "t1 never ran");
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let rep = f
        .fleet(&FleetOptions {
            stop: Some(stop),
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    signal.await.unwrap();
    assert!(rep.shutdown && rep.stop_error().is_some(), "{rep:?}");
    let slow = |s: &str| format!("SELECT COUNT(*) FROM {s}.items WHERE id = 77");
    assert_eq!(
        count(&reg, &slow("s_t1")).await,
        1,
        "the unit in flight ends"
    );
    assert!(
        !exists_in(&reg, "s_t1", "more").await,
        "a unit after the stop"
    );
    for s in ["s_t2", "s_t3"] {
        assert_eq!(count(&reg, &slow(s)).await, 0, "{s} started");
    }
    assert_eq!(rep.skipped, ["t2", "t3"]);
    let runs = fleet_runs(&reg).await.unwrap();
    assert_eq!(runs.last().unwrap().2, "stopped", "{runs:?}");
    let rep = f.fleet(&FleetOptions::default()).await.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    for s in ["s_t1", "s_t2", "s_t3"] {
        assert!(exists_in(&reg, s, "more").await, "{s} did not resume");
    }
}

/// Boot with the shutdown flag up: the fleet stops and boot fails, so
/// `/ready` never opens (6.11).
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn boot_fails_on_a_shutdown_during_the_fleet() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite://{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let registry = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("migrations")).unwrap();
    let builder =
        || crate::server::Builder::from_pool(registry.clone(), registry_url.clone(), "localhost");
    let first = builder().migrate(&root).await.unwrap();
    let mut o = Org::new("t1".to_owned());
    o.storage_mode = StorageMode::Database;
    o.backend_kind = crate::tenancy::BackendKind::Sqlite;
    o.database_url = Some(url("t1.db"));
    o.insert(&first.pools().registry_pool()).await.unwrap();
    put(
        &root.join("migrations"),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let mut b = builder();
    b.fleet.stop = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
        true,
    )));
    let e = b.migrate(&root).await.err().expect("a stopped boot fails");
    assert!(e.to_string().contains(run::STOPPED), "{e}");
}

/// The fleet record's heartbeat is refreshed on a timer, so it stays fresh
/// while one tenant takes longer than the staleness limit (6.11).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn the_fleet_heartbeat_stays_fresh_through_a_long_tenant() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("beat").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    put(
        &f.dir(),
        "0002_slow",
        &slow_file("0002_slow", "0001_items", 3.0),
    );
    let reg = f.registry();
    let watch = {
        let reg = reg.clone();
        tokio::spawn(async move {
            assert!(wait_running(&reg, "t1").await, "t1 never ran");
            for _ in 0..25 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let stale = crate::migrate::v2::ledger::fleet_stale(
                    &reg,
                    std::time::Duration::from_secs(1),
                )
                .await
                .unwrap();
                if stale.is_some() {
                    return stale;
                }
            }
            None
        })
    };
    let rep = f
        .fleet(&FleetOptions {
            heartbeat_every: Some(std::time::Duration::from_millis(200)),
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    assert_eq!(watch.await.unwrap(), None, "the heartbeat went stale");
}

/// A booting replica waits on a live fleet holder past its staleness limit,
/// then finds nothing to do (6.11).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_booting_waiter_waits_on_a_live_fleet_holder() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("waitlive").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    put(
        &f.dir(),
        "0002_slow",
        &slow_file("0002_slow", "0001_items", 2.5),
    );
    // The fleet future is not `Send`: both run on this task.
    let beat = FleetOptions {
        heartbeat_every: Some(std::time::Duration::from_millis(200)),
        ..FleetOptions::default()
    };
    let holder = f.fleet(&beat);
    let waiter = async {
        assert!(wait_running(&f.registry(), "t1").await, "t1 never ran");
        f.fleet(&FleetOptions {
            stale_after: Some(std::time::Duration::from_secs(1)),
            ..FleetOptions::default()
        })
        .await
    };
    let (held, waited) = tokio::join!(holder, waiter);
    assert_eq!(held.unwrap().failed().count(), 0);
    let rep = waited.expect("waits on a live holder");
    assert!(rep.tenants.iter().all(|t| t.applied.is_empty()), "{rep:?}");
}

/// A booting replica gives up on a fleet holder whose record's heartbeat is
/// older than its staleness limit, on each backend's clock (6.11).
async fn a_waiter_leaves_a_hung_fleet_holder<DB: sqlx::Database>(registry_url: String)
where
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
    let t = registry
        .dialect()
        .quote_ident(crate::migrate::v2::ledger::FLEET_RUNS);
    raw_execute_pool(
        &registry,
        &format!(
            "INSERT INTO {t} (run_id, direction, plan_hash, state, heartbeat_at, started_at) \
             VALUES ('hung', 'up', 'x', 'running', '2000-01-01 00:00:00', '2000-01-01 00:00:00')"
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    let holder_pool = Pool::connect(&registry_url).await.unwrap();
    let (held_tx, held) = tokio::sync::oneshot::channel();
    let (go_tx, go) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        run::fleet_locked(&holder_pool, || async {
            held_tx.send(()).unwrap();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), go).await;
            Ok(())
        })
        .await
        .unwrap();
        holder_pool.close().await;
    });
    held.await.unwrap();
    let fo = FleetOptions {
        stale_after: Some(std::time::Duration::from_secs(1)),
        ..FleetOptions::default()
    };
    let waited = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        run_fleet(&pools, &dir, &registry_url, &fo),
    )
    .await;
    let _ = go_tx.send(());
    holder.await.unwrap();
    let e = waited.expect("gave up in time").expect_err("a hung holder");
    assert!(e.to_string().contains("looks hung"), "{e}");
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_waiter_leaves_a_hung_fleet_holder_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let r = Scratch::create(&admin, "hung").await;
    a_waiter_leaves_a_hung_fleet_holder::<sqlx::Postgres>(r.url.clone()).await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn a_waiter_leaves_a_hung_fleet_holder_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    let r = Scratch::create(&admin, "hung").await;
    a_waiter_leaves_a_hung_fleet_holder::<sqlx::MySql>(r.url.clone()).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn a_waiter_leaves_a_hung_fleet_holder_sqlite() {
    let _g = serial().lock().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite:{}?mode=rwc",
        dir.path().join("registry.db").display()
    );
    a_waiter_leaves_a_hung_fleet_holder::<sqlx::Sqlite>(url).await;
}

/// A refused fleet run writes nothing: no registry chain, no tenant schema.
/// Here another plan's unfinished run refuses it, then a tenant blocker.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_refused_fleet_run_writes_nothing() {
    use crate::migrate::v2::ledger::FLEET_RUNS;
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("refwrite").await else {
        return;
    };
    f.with_orgs(&["t1"]).await;
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
            "INSERT INTO {FLEET_RUNS} (run_id, direction, plan_hash, state, started_at) \
             VALUES ('dead', 'up', 'another-plan', 'running', now())"
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    let flags = json!([{"op": "create_table", "v": 1, "table": {"name": "flags",
        "scope": "registry", "fields": [
            {"column": "id", "ty": "i64", "primary_key": true, "nullable": false}]}}]);
    put(
        &f.dir(),
        "0002_flags",
        &file("0002_flags", &[], "registry", flags),
    );
    f.add_org("t2").await;
    let schema_made = |s: &'static str| {
        let reg = reg.clone();
        async move {
            count(
                &reg,
                &format!("SELECT COUNT(*) FROM pg_namespace WHERE nspname = '{s}'"),
            )
            .await
                > 0
        }
    };
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("refused");
    assert!(e.to_string().contains("--abandon-fleet-run"), "{e}");
    assert!(!has_table(&reg, "flags").await, "the registry chain ran");
    assert!(!schema_made("s_t2").await, "a tenant schema was made");
    raw_execute_pool(
        &reg,
        &format!("DELETE FROM {FLEET_RUNS} WHERE run_id = 'dead'"),
        Vec::new(),
    )
    .await
    .unwrap();
    // A blocker in t1 (an edited applied file) refuses before the registry chain.
    raw_execute_pool(
        &reg,
        &format!(
            "UPDATE s_t1.{} SET checksum = 'edited'",
            Ledger::default_names().table
        ),
        Vec::new(),
    )
    .await
    .unwrap();
    put(
        &f.dir(),
        "0003_other",
        &file("0003_other", &["0001_items"], "tenant", table_op("other")),
    );
    let e = f
        .fleet(&FleetOptions::default())
        .await
        .expect_err("blocked");
    assert!(e.to_string().contains("blocked"), "{e}");
    assert!(!has_table(&reg, "flags").await, "the registry chain ran");
    assert!(!schema_made("s_t2").await, "a tenant schema was made");
}

/// Database-mode tenants open short-lived connections: the peak follows
/// `--parallel`, not the tenant count, and none stays open after.
#[cfg(any(feature = "postgres", feature = "mysql"))]
async fn db_mode_connections_follow_parallel<DB: sqlx::Database>(
    admin: &str,
    kind: crate::tenancy::BackendKind,
    tenants: usize,
) where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let r = Scratch::create(admin, "dbcr").await;
    let mut dbs = Vec::new();
    for _ in 0..tenants {
        dbs.push(Scratch::create(admin, "dbct").await);
    }
    let raw = sqlx::Pool::<DB>::connect(&r.url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &r.url, &FleetOptions::default())
        .await
        .unwrap();
    for (i, db) in dbs.iter().enumerate() {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = format!("c{i:02}");
            o.display_name = o.slug.clone();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = kind;
            o.database_url = Some(db.url.clone());
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
    let prefix = format!("rv5\\_dbct\\_{}\\_%", std::process::id());
    let sql = match kind {
        crate::tenancy::BackendKind::MySql => {
            format!("SELECT COUNT(*) FROM information_schema.processlist WHERE db LIKE '{prefix}'")
        }
        _ => format!("SELECT COUNT(*) FROM pg_stat_activity WHERE datname LIKE '{prefix}'"),
    };
    let k = 2;
    let fo = FleetOptions {
        parallel: k,
        ..FleetOptions::default()
    };
    let (rep, peak) = peak_connections(admin, &sql, run_fleet(&pools, &dir, &r.url, &fo)).await;
    let rep = rep.unwrap();
    assert_eq!(rep.failed().count(), 0, "{rep:?}");
    let watch = Pool::connect(admin).await.unwrap();
    let after = count(&watch, &sql).await;
    watch.close().await;
    eprintln!("{kind:?}: {tenants} tenants, --parallel {k}: peak {peak}, open after {after}");
    assert!(
        peak <= 3 * k as i64 + 1,
        "{peak} connections for {tenants} tenants"
    );
    assert_eq!(after, 0, "tenant connections left open");
    registry.close().await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn db_mode_connections_follow_parallel_postgres() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    db_mode_connections_follow_parallel::<sqlx::Postgres>(
        &admin,
        crate::tenancy::BackendKind::Postgres,
        12,
    )
    .await;
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn db_mode_connections_follow_parallel_mysql() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("MYSQL_TEST_URL", "mysql") else {
        return;
    };
    db_mode_connections_follow_parallel::<sqlx::MySql>(
        &admin,
        crate::tenancy::BackendKind::MySql,
        12,
    )
    .await;
}

/// The tenancy CLI's output, also when the verb fails.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn tenancy_cli_out<DB: sqlx::Database>(
    pools: &TenantPools<DB>,
    registry_url: &str,
    dir: &Path,
    args: &[&str],
) -> (Result<(), TenancyError>, String)
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let mut out = Vec::new();
    let args = args.iter().map(|a| (*a).to_owned());
    let r = crate::tenancy::manage::run_with_writer(pools, registry_url, dir, args, &mut out).await;
    (r, String::from_utf8(out).unwrap())
}

/// `dbmigrate apply` prints why a tenant failed, and counts and lists the
/// tenants a stop left out.
#[cfg(feature = "sqlite")]
#[tokio::test]
async fn apply_prints_why_a_tenant_failed() {
    let _g = serial().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let url = |n: &str| format!("sqlite:{}?mode=rwc", tmp.path().join(n).display());
    let registry_url = url("registry.db");
    let raw = sqlx::SqlitePool::connect(&registry_url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &registry_url, &FleetOptions::default())
        .await
        .unwrap();
    for (slug, db) in [
        ("t1", url("t1.db")),
        ("t2", "sqlite:/nonexistent/dir/t2.db?mode=ro".to_owned()),
        ("t3", url("t3.db")),
    ] {
        let mut org = crate::testkit::org_with(|o| {
            o.id = crate::sql::Auto::default();
            o.slug = slug.into();
            o.display_name = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = crate::tenancy::BackendKind::Sqlite;
            o.database_url = Some(db.clone());
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
    let (r, out) = tenancy_cli_out(
        &pools,
        &registry_url,
        &dir,
        &["dbmigrate", "apply", "--fail-fast"],
    )
    .await;
    let line = out
        .lines()
        .find(|l| l.starts_with("tenant t2: failed (unreachable): "))
        .unwrap_or_else(|| panic!("no reason for t2: {out}"));
    assert!(
        line.len() > "tenant t2: failed (unreachable): ".len() + 5,
        "{out}"
    );
    assert!(out.contains("tenant t1: skipped, not run"), "{out}");
    let e = r.expect_err("t2 failed");
    assert!(
        e.to_string().contains("1 of 3 tenant(s) failed, 2 skipped"),
        "{e}"
    );
    registry.close().await;
}

/// A migrator cannot take over a database its tenant login owns: refused up
/// front, naming the fix, even with the login's rights granted (8.1).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_migrator_refuses_a_database_its_tenant_login_owns() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let (r, mut a) = (
        Scratch::create(&admin, "omr").await,
        Scratch::create(&admin, "oma").await,
    );
    let (login, mig) = (
        format!("rv5ot_{}", std::process::id()),
        format!("rv5om_{}", std::process::id()),
    );
    a.roles.extend([mig.clone(), login.clone()]);
    let root = Pool::connect(&a.url).await.unwrap();
    for sql in [
        format!("CREATE ROLE {login} LOGIN PASSWORD 'pw'"),
        format!("CREATE ROLE {mig} LOGIN PASSWORD 'pw' NOSUPERUSER NOCREATEDB"),
        format!("ALTER DATABASE {} OWNER TO {login}", a.name),
        format!("GRANT {login} TO {mig}"),
    ] {
        raw_execute_pool(&root, &sql, Vec::new()).await.unwrap();
    }
    root.close().await;
    let host = admin.split_once('@').unwrap().1.split_once('/').unwrap().0;
    let raw = sqlx::PgPool::connect(&r.url).await.unwrap();
    let registry: Pool = raw.clone().into();
    let pools = TenantPools::new(raw);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("migrations");
    std::fs::create_dir_all(&dir).unwrap();
    run_fleet(&pools, &dir, &r.url, &FleetOptions::default())
        .await
        .unwrap();
    let mut org = crate::testkit::org_with(|o| {
        o.id = crate::sql::Auto::default();
        o.slug = "own".into();
        o.display_name = "own".into();
        o.storage_mode = StorageMode::Database;
        o.backend_kind = crate::tenancy::BackendKind::Postgres;
        o.database_url = Some(format!("postgres://{login}:pw@{host}/{}", a.name));
        o.schema_name = None;
        o.host_pattern = None;
        o.port = None;
        o.path_prefix = None;
    });
    org.insert(&registry).await.unwrap();
    put(
        &dir,
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let fo = FleetOptions {
        migrator: Some(MigratorLogin {
            user: mig.clone(),
            password: "pw".into(),
        }),
        ..FleetOptions::default()
    };
    let rep = run_fleet(&pools, &dir, &r.url, &fo).await.unwrap();
    let e = rep
        .failed()
        .next()
        .and_then(|t| t.error.clone())
        .expect("refused");
    assert!(
        e.contains("owns its database") && e.contains("OWNER TO <migrator>"),
        "{e}"
    );
    registry.close().await;
}

/// A fleet `down 1` that failed part way, rerun: the tenants already rolled
/// back stay at the same target; the rest catch up (8.4). The failure names
/// `--to`.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_rerun_fleet_down_stops_at_the_same_targets() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("downrerun").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let five = json!([{"op": "data_sql", "v": 1, "id": "five_v1",
        "tables": {"write": ["items"]},
        "up": {"any": "INSERT INTO {{t:items}} ({{c:id}}, {{c:code}}) VALUES (5, '5')"},
        "down": {"any": "DELETE FROM {{t:items}} WHERE CAST({{c:code}} AS BIGINT) = 5"}}]);
    put(
        &f.dir(),
        "0002_five",
        &file("0002_five", &["0001_items"], "tenant", five),
    );
    f.fleet(&FleetOptions::default()).await.unwrap();
    let reg = f.registry();
    raw_execute_pool(
        &reg,
        "INSERT INTO s_t2.items (id, code) VALUES (9, 'xx')",
        Vec::new(),
    )
    .await
    .unwrap();
    let pools = f.pools();
    let down = ["dbmigrate", "down", "1", "--all-tenants"];
    let (r, _) = tenancy_cli_out(&pools, &f.db.url, &f.dir(), &down).await;
    let e = r.expect_err("t2 fails");
    assert!(
        e.to_string().contains("t2") && e.to_string().contains("--to"),
        "{e}"
    );
    let fives = |s: &str| format!("SELECT COUNT(*) FROM {s}.items WHERE id = 5");
    assert_eq!(count(&reg, &fives("s_t1")).await, 0);
    assert_eq!(count(&reg, &fives("s_t3")).await, 1);
    raw_execute_pool(&reg, "DELETE FROM s_t2.items WHERE id = 9", Vec::new())
        .await
        .unwrap();
    let (r, out) = tenancy_cli_out(&pools, &f.db.url, &f.dir(), &down).await;
    r.unwrap();
    assert!(out.contains("tenant t1: nothing to roll back"), "{out}");
    for s in ["s_t1", "s_t2", "s_t3"] {
        assert!(exists_in(&reg, s, "items").await, "{s} lost 0001_items");
        assert_eq!(count(&reg, &fives(s)).await, 0, "{s}");
    }
    // Finished: the same command again is a new run, one step further.
    let (r, _) = tenancy_cli_out(&pools, &f.db.url, &f.dir(), &down).await;
    r.unwrap();
    for s in ["s_t1", "s_t2", "s_t3"] {
        assert!(!exists_in(&reg, s, "items").await, "{s}");
    }
}

/// `--canary` keeps the order given, and a slug no tenant has is refused
/// before anything runs (8.3).
#[cfg(feature = "postgres")]
#[tokio::test]
async fn canaries_keep_their_order_and_an_unknown_one_refuses() {
    let _g = serial().lock().await;
    let Some(f) = PgFleet::new("canord").await else {
        return;
    };
    f.with_orgs(&["t1", "t2", "t3"]).await;
    put(
        &f.dir(),
        "0001_items",
        &file("0001_items", &[], "tenant", items()),
    );
    let e = f
        .fleet(&FleetOptions {
            canaries: vec!["t9".into()],
            ..FleetOptions::default()
        })
        .await
        .expect_err("an unknown canary");
    assert!(e.to_string().contains("t9"), "{e}");
    assert!(!exists_in(&f.registry(), "s_t1", "items").await);
    let rep = f
        .fleet(&FleetOptions {
            canaries: vec!["t3".into(), "t1".into()],
            ..FleetOptions::default()
        })
        .await
        .unwrap();
    let ran: Vec<&str> = rep.tenants.iter().map(|t| t.slug.as_str()).collect();
    assert_eq!(ran, ["t3", "t1", "t2"], "{rep:?}");
}

/// Two replicas booting on a fresh database revoke PUBLIC's TEMP at once:
/// the one that loses the race reads the grants again instead of failing.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn a_concurrent_public_revoke_is_not_an_error() {
    let _g = serial().lock().await;
    let Some(admin) = crate::testkit::matrix::live_url("DATABASE_URL", "postgres") else {
        return;
    };
    let db = Scratch::create(&admin, "race").await;
    let (a, b) = (
        sqlx::PgPool::connect(&db.url).await.unwrap(),
        sqlx::PgPool::connect(&db.url).await.unwrap(),
    );
    let mut tx = a.begin().await.unwrap();
    sqlx::query(&format!(
        "REVOKE TEMPORARY ON DATABASE {} FROM PUBLIC",
        db.name
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    let other = tokio::spawn(async move {
        let mut c = b.acquire().await.unwrap();
        crate::tenancy::schema_role::revoke_public_writes_beside_others(&mut c).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    tx.commit().await.unwrap();
    other.await.unwrap().expect("the second revoke reads again");
    a.close().await;
}
