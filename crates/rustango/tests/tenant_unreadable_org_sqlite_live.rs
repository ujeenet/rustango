//! One `rustango_orgs` row whose storage mode or backend this version
//! cannot read must fail that tenant only: resolution of the others, the
//! breaker, and every sweep keep working and report the bad row.
//! SQLite only: this Org path has no per-backend code, and decode errors
//! are covered on all three backends by `text_enum_tri`.

#![cfg(all(feature = "sqlite", feature = "tenancy", feature = "testkit"))]

use http::request::Parts;
use http::Request;
use rustango::sql::{sqlx, Auto, Pool};
use rustango::tenancy::{
    for_each_tenant, BackendKind, Org, OrgResolver, StorageMode, SubdomainResolver, TenantPools,
};

/// Resolver state is process-global.
fn lock() -> &'static tokio::sync::Mutex<()> {
    static M: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn host(h: &str) -> Parts {
    Request::builder()
        .uri("/")
        .header("host", h)
        .body(())
        .expect("request")
        .into_parts()
        .0
}

struct Reg {
    url: String,
    pool: Pool,
    pools: TenantPools<sqlx::Sqlite>,
    dir: tempfile::TempDir,
}

/// A file registry with `good` and `bad` tenants, each with its own file
/// database; `bad`'s `column` is then set to `value` behind the ORM.
async fn registry(column: &str, value: &str) -> Reg {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("reg.db").display());
    let sq = sqlx::SqlitePool::connect(&url).await.expect("sqlite");
    let pool = Pool::Sqlite(sq.clone());
    rustango::testkit::migrate_framework(&pool)
        .await
        .expect("framework tables");
    for slug in ["good", "bad"] {
        let db = dir.path().join(format!("{slug}.db"));
        let mut org = rustango::testkit::org_with(|o| {
            o.id = Auto::Unset;
            o.slug = slug.into();
            o.storage_mode = StorageMode::Database;
            o.backend_kind = BackendKind::Sqlite;
            o.database_url = Some(format!("sqlite://{}?mode=rwc", db.display()));
            o.host_pattern = Some(format!("{slug}.app.test"));
        });
        org.insert(&pool).await.expect("insert org");
    }
    rustango::sql::raw_execute_pool(
        &pool,
        &format!("UPDATE rustango_orgs SET {column} = '{value}' WHERE slug = 'bad'"),
        Vec::new(),
    )
    .await
    .expect("corrupt the row");
    rustango::testkit::reset_registry_breaker();
    rustango::testkit::reset_org_cache();
    Reg {
        url,
        pool,
        pools: TenantPools::new(sq),
        dir,
    }
}

async fn resolve(pool: &Pool, h: &str) -> Result<Option<String>, String> {
    SubdomainResolver::new("app.test")
        .resolve(&host(h), pool)
        .await
        .map(|o| o.map(|o| o.slug))
        .map_err(|e| e.to_string())
}

/// The bad tenant errors; the good one still resolves, so the decode
/// error did not open the registry breaker. Neither answer is cached.
#[tokio::test]
async fn a_bad_row_fails_its_tenant_only() {
    let _g = lock().lock().await;
    let reg = registry("storage_mode", "shard").await;

    let err = resolve(&reg.pool, "bad.app.test").await.unwrap_err();
    assert!(
        err.contains("unknown value `shard` for StorageMode"),
        "{err}"
    );
    rustango::testkit::reset_org_cache();
    assert_eq!(
        resolve(&reg.pool, "good.app.test").await,
        Ok(Some("good".to_owned())),
        "one bad row opened the breaker for every tenant"
    );
    // Not cached as a miss: still the decode error, then the fixed row.
    assert!(resolve(&reg.pool, "bad.app.test").await.is_err());
    rustango::sql::raw_execute_pool(
        &reg.pool,
        "UPDATE rustango_orgs SET storage_mode = 'database' WHERE slug = 'bad'",
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        resolve(&reg.pool, "bad.app.test").await,
        Ok(Some("bad".to_owned()))
    );
}

#[tokio::test]
async fn fetch_reporting_skips_and_reports_the_bad_row() {
    let _g = lock().lock().await;
    let reg = registry("backend_kind", "Oracle\u{7}").await;
    let (orgs, bad) = Org::fetch_reporting(Org::objects(), &reg.pool)
        .await
        .expect("sweep read");
    assert_eq!(
        orgs.iter().map(|o| o.slug.as_str()).collect::<Vec<_>>(),
        ["good"]
    );
    assert_eq!(bad.len(), 1);
    assert_eq!(
        (bad[0].slug.as_str(), bad[0].column, bad[0].value.as_str()),
        ("bad", "backend_kind", "Oracle\\u{7}")
    );
}

#[tokio::test]
async fn sweeps_visit_the_good_tenant_and_report_the_bad_one() {
    let _g = lock().lock().await;
    // `storage_mode` still says database, so the prewarm filter sees the row.
    let reg = registry("backend_kind", "oracle").await;

    let sweep = for_each_tenant(&reg.pools, |org, _pool| async move {
        Ok::<_, std::convert::Infallible>(org.slug)
    })
    .await
    .expect("sweep runs");
    let visited: Vec<&str> = sweep.values().map(|(s, _)| s).collect();
    assert_eq!(visited, ["good"]);
    let errors: Vec<_> = sweep.errors().map(|(s, e)| (s, e.to_string())).collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, "bad");
    assert!(
        errors[0].1.contains("backend_kind holds `oracle`"),
        "{errors:?}"
    );

    let report = reg.pools.prewarm_database_tenants().await.expect("prewarm");
    assert_eq!(
        (report.total_active, report.warmed, report.failed),
        (2, 1, 1)
    );

    let migrations = tempfile::tempdir().unwrap();
    let report = rustango::tenancy::migrate_tenants_db(&reg.pools, migrations.path(), &reg.url)
        .await
        .expect("migrate-all runs");
    assert_eq!(report.failure_count(), 1);
    let failed = report.tenants.iter().find(|t| t.error.is_some()).unwrap();
    assert_eq!(failed.slug, "bad");
    assert!(report
        .tenants
        .iter()
        .any(|t| t.slug == "good" && t.error.is_none()));
}

async fn manage(reg: &Reg, args: &[&str]) -> (Result<(), String>, String) {
    let mut buf: Vec<u8> = Vec::new();
    let r = rustango::tenancy::manage::run_with_writer(
        &reg.pools,
        &reg.url,
        reg.dir.path(),
        args.iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
        &mut buf,
    )
    .await
    .map_err(|e| e.to_string());
    (r, String::from_utf8(buf).unwrap())
}

/// `list-tenants` lists the readable rows and names the bad one;
/// `check-tenants` fails on it; `repair-tenant` fixes it from raw text.
#[tokio::test]
async fn cli_lists_checks_and_repairs_a_bad_row() {
    let _g = lock().lock().await;
    let reg = registry("storage_mode", "Database").await;
    rustango::sql::raw_execute_pool(
        &reg.pool,
        "UPDATE rustango_orgs SET backend_kind = 'sqlite3' WHERE slug = 'good'",
        Vec::new(),
    )
    .await
    .unwrap();

    let (r, out) = manage(&reg, &["list-tenants"]).await;
    r.expect("list-tenants");
    assert!(out.contains("unreadable: tenant `bad`"), "{out}");
    assert!(out.contains("good"), "{out}");

    let (r, out) = manage(&reg, &["check-tenants"]).await;
    assert!(r.unwrap_err().contains("1 tenant value(s) cannot be read"));
    assert!(out.contains("storage_mode holds `Database`"), "{out}");
    assert!(
        out.contains("alias: tenant `good` stores backend_kind as `sqlite3`"),
        "{out}"
    );

    let (r, _) = manage(&reg, &["repair-tenant", "bad"]).await;
    assert!(r.unwrap_err().contains("pass the value it should hold"));
    let (r, _) = manage(&reg, &["repair-tenant", "good", "--backend", "mysql"]).await;
    assert!(r.unwrap_err().contains("does not move its data"));

    let (r, out) = manage(
        &reg,
        &["repair-tenant", "bad", "--storage-mode", "database"],
    )
    .await;
    r.expect("repair bad");
    assert!(out.contains("repaired tenant `bad`"), "{out}");
    let (r, _) = manage(&reg, &["repair-tenant", "good"]).await;
    r.expect("rewrite alias");

    let (r, out) = manage(&reg, &["check-tenants"]).await;
    r.expect("clean");
    assert!(
        out.contains("every tenant row reads") && !out.contains("alias:"),
        "{out}"
    );
    let stored: Vec<String> = Org::objects()
        .order_by(&[("slug", false)])
        .pluck("backend_kind", &reg.pool)
        .await
        .unwrap();
    assert_eq!(stored, ["sqlite", "sqlite"]);
}

/// Repairing a row to schema mode runs provisioning's claim check: it
/// never puts a second tenant on a schema another one lives in.
#[tokio::test]
async fn repair_to_schema_mode_refuses_a_claimed_schema() {
    let _g = lock().lock().await;
    let reg = registry("storage_mode", "Schema ").await;
    rustango::sql::raw_execute_pool(
        &reg.pool,
        "UPDATE rustango_orgs SET backend_kind = 'postgres' WHERE slug = 'bad'",
        Vec::new(),
    )
    .await
    .unwrap();
    rustango::sql::raw_execute_pool(
        &reg.pool,
        "UPDATE rustango_orgs SET storage_mode = 'schema', backend_kind = 'postgres', \
         schema_name = 'bad' WHERE slug = 'good'",
        Vec::new(),
    )
    .await
    .unwrap();

    let (r, _) = manage(&reg, &["repair-tenant", "bad", "--storage-mode", "schema"]).await;
    assert!(
        r.as_ref()
            .is_err_and(|e| e.contains("another tenant uses schema `bad`")),
        "{r:?}"
    );
    let (r, out) = manage(
        &reg,
        &["repair-tenant", "bad", "--storage-mode", "database"],
    )
    .await;
    r.expect("repair to database mode");
    assert!(out.contains("repaired tenant `bad`"), "{out}");
}
