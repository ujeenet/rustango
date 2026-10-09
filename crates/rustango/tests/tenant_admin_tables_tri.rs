//! After `migrate` + `migrate-tenants`, every model an admin lists has its
//! table, on every backend and tenant storage mode (#2360).

#![cfg(all(feature = "admin", feature = "tenancy", feature = "testkit"))]

use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustango::sql::Pool;
use tower::ServiceExt as _;

#[derive(Clone, Copy)]
enum Backend {
    #[cfg(feature = "postgres")]
    Postgres,
    #[cfg(feature = "mysql")]
    Mysql,
    #[cfg(feature = "sqlite")]
    Sqlite,
}

impl Backend {
    fn kind(self) -> &'static str {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres => "postgres",
            #[cfg(feature = "mysql")]
            Self::Mysql => "mysql",
            #[cfg(feature = "sqlite")]
            Self::Sqlite => "sqlite",
        }
    }
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
#[path = "support/scratch_db.rs"]
mod scratch_db;

#[cfg(any(feature = "postgres", feature = "mysql"))]
type DbGuard = Option<scratch_db::ScratchDb>;
#[cfg(not(any(feature = "postgres", feature = "mysql")))]
type DbGuard = Option<()>;

/// A fresh database, its URL and its guard; `None` when the URL is unset.
async fn fresh(backend: Backend, tmp: &Path, tag: &str) -> Option<(Pool, String, DbGuard)> {
    let prefix = format!("rustango_t2360_{tag}");
    let (url, guard): (String, DbGuard) = match backend {
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => {
            let db = tmp.join(format!("{prefix}.db"));
            (format!("sqlite:{}?mode=rwc", db.display()), None)
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let db =
                scratch_db::ScratchDb::create(&std::env::var("DATABASE_URL").ok()?, &prefix).await;
            (db.url().to_owned(), Some(db))
        }
        #[cfg(feature = "mysql")]
        Backend::Mysql => {
            let db = scratch_db::ScratchDb::create(&std::env::var("MYSQL_TEST_URL").ok()?, &prefix)
                .await;
            (db.url().to_owned(), Some(db))
        }
    };
    let _ = tmp;
    Some((Pool::connect(&url).await.expect("connect"), url, guard))
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String) {
    send(
        router,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

/// A tenant admin over `pool`, mounted at `/a`.
fn tenant_admin(pool: Pool) -> axum::Router {
    rustango::admin::Builder::new(pool)
        .tenant_mode()
        .admin_prefix("/a")
        .build()
}

/// The tables the admin index links to.
fn listed_tables(index: &str) -> Vec<String> {
    // Tera escapes `/` in attribute values.
    let index = index.replace("&#x2F;", "/");
    let mut out: Vec<String> = index
        .split("href=\"/a/")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        // `__audit` and `__docs` are admin pages, not tables.
        .filter(|t| !t.is_empty() && !t.contains('/') && !t.starts_with("__"))
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every table the admin lists opens with a 200.
async fn assert_listed_tables_exist(router: &axum::Router, what: &str) -> Vec<String> {
    let (status, index) = get(router, "/").await;
    assert_eq!(status, StatusCode::OK, "{what}: index");
    let tables = listed_tables(&index);
    assert!(!tables.is_empty(), "{what}: the index lists no model");
    let mut missing = Vec::new();
    for t in &tables {
        let (status, _) = get(router, &format!("/{t}")).await;
        if status != StatusCode::OK {
            missing.push(format!("{t}: {status}"));
        }
    }
    assert!(
        missing.is_empty(),
        "{what}: listed tables that do not open: {missing:?}"
    );
    tables
}

/// Registry tables stay out of a tenant admin, also where a schema-mode
/// `search_path` would reach the registry's copy.
fn assert_no_registry_tables(listed: &[String]) {
    for t in ["rustango_translations", "rustango_admin_totp"] {
        assert!(!listed.iter().any(|l| l == t), "tenant admin lists {t}");
    }
}

/// The translations editor and export are registry-only: a 404 on a
/// tenant admin, and nothing reaches the registry's table.
async fn assert_no_translations_editor(router: &axum::Router, registry: &Pool) {
    let post = Request::post("/rustango_translations/editor")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("tr:en:greeting=pwned"))
        .unwrap();
    let (status, body) = send(router, post).await;
    let rows = rustango::i18n::db::all_pool(registry).await.unwrap();
    assert!(rows.is_empty(), "registry translations written: {rows:?}");
    assert_eq!(status, StatusCode::NOT_FOUND, "POST editor: {body}");
    for uri in [
        "/rustango_translations/editor",
        "/rustango_translations/export.json",
    ] {
        let (status, body) = get(router, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {uri}: {body}");
    }
}

/// The documented setup: registry `migrate`, an org, `migrate-tenants`.
async fn database_mode_tenant_admin(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, _, _reg)) = fresh(backend, tmp.path(), "reg").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let (_, tenant_url, _ten) = fresh(backend, tmp.path(), "ten").await.unwrap();
    let dir = tmp.path().join("app/migrations");
    std::fs::create_dir_all(&dir).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry migrate");
    let mut org = rustango::tenancy::Org {
        slug: "t1".into(),
        display_name: "t1".into(),
        backend_kind: backend.kind().into(),
        database_url: Some(tenant_url),
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();
    let tenant = match &registry {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let pools = rustango::tenancy::TenantPools::new(pg.clone());
            let r = rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await;
            assert!(r.expect("tenants").all_ok());
            pools.scoped_pool_dyn(&org).await.unwrap()
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let pools = rustango::tenancy::TenantPools::new(my.clone());
            let r = rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await;
            assert!(r.expect("tenants").all_ok());
            pools.scoped_pool_dyn(&org).await.unwrap()
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let pools = rustango::tenancy::TenantPools::new(sq.clone());
            let r = rustango::tenancy::migrate_tenants_db(&pools, &dir, "").await;
            assert!(r.expect("tenants").all_ok());
            pools.scoped_pool_dyn(&org).await.unwrap()
        }
    };
    assert_passkeys_on_tenant_only(&tenant, &registry).await;
    let router = tenant_admin(tenant);
    let listed = assert_listed_tables_exist(&router, "tenant admin").await;
    assert!(listed.iter().any(|t| t == "rustango_users"), "{listed:?}");
    assert_no_registry_tables(&listed);
    assert_no_passkey_admin(&router).await;
    assert_no_translations_editor(&router, &registry).await;

    // The registry holds the translations the tenant admin no longer lists.
    let registry_admin = rustango::admin::Builder::new(registry.clone())
        .registry_mode()
        .admin_prefix("/a")
        .build();
    let listed = assert_listed_tables_exist(&registry_admin, "registry admin").await;
    assert!(!listed.iter().any(|t| t == "rustango_users"), "{listed:?}");
    let (status, _) = get(&registry_admin, "/rustango_translations").await;
    assert_eq!(status, StatusCode::OK, "registry rustango_translations");
    let (status, _) = get(&registry_admin, "/rustango_orgs").await;
    assert_eq!(status, StatusCode::OK, "registry rustango_orgs");
    assert_no_passkey_admin(&registry_admin).await;
    // ...and serves the editor the tenant admin 404s.
    let (status, body) = get(&registry_admin, "/rustango_translations/editor").await;
    assert_eq!(status, StatusCode::OK, "registry editor: {body}");
}

/// A single-database `migrate` creates what its admin lists.
async fn single_database_admin(backend: Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let Some((pool, _, _db)) = fresh(backend, tmp.path(), "one").await else {
        eprintln!("skipping — backend URL unset");
        return;
    };
    let dir = tmp.path().join("app/migrations");
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = Vec::new();
    rustango::migrate::manage::run_with_writer(&pool, &dir, ["migrate".to_owned()], &mut out)
        .await
        .expect("migrate");
    let router = rustango::admin::Builder::new(pool.clone())
        .admin_prefix("/a")
        .build();
    // Registry-only tables are not created here, so not listed (#2365).
    let listed = assert_listed_tables_exist(&router, "single-database admin").await;
    // Shared system tables stay listed (#2365).
    for t in [
        "rustango_translations",
        "rustango_audit_log",
        "rustango_content_types",
    ] {
        assert!(listed.iter().any(|l| l == t), "{t} missing: {listed:?}");
    }
    assert!(!listed.iter().any(|t| t == "rustango_orgs"), "{listed:?}");
    let (status, _) = get(&router, "/rustango_orgs").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "rustango_orgs");
    assert_no_passkey_admin(&router).await;
    // `migrate` creates the passkey store sign-in reads (#2364).
    #[cfg(feature = "passkey")]
    rustango::passkey::for_user(&pool, 1)
        .await
        .expect("passkey table");
}

/// Tenancy `migrate` creates the passkey store on the tenant, never the registry (#2364).
#[allow(unused_variables)]
async fn assert_passkeys_on_tenant_only(tenant: &Pool, registry: &Pool) {
    #[cfg(feature = "passkey")]
    {
        rustango::passkey::for_user(tenant, 1)
            .await
            .expect("tenant passkey table");
        assert!(
            rustango::passkey::for_user(registry, 1).await.is_err(),
            "the registry holds a passkey table"
        );
    }
}

/// No admin serves passkeys: a row signs in as its `user_id` (#2364).
async fn assert_no_passkey_admin(router: &axum::Router) {
    let (status, _) = get(router, "/rustango_webauthn_credentials").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "rustango_webauthn_credentials"
    );
}

macro_rules! per_backend {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[cfg(feature = "postgres")]
                #[tokio::test]
                async fn postgres() {
                    super::$scenario(super::Backend::Postgres).await;
                }
                #[cfg(feature = "mysql")]
                #[tokio::test]
                async fn mysql() {
                    super::$scenario(super::Backend::Mysql).await;
                }
                #[cfg(feature = "sqlite")]
                #[tokio::test]
                async fn sqlite() {
                    super::$scenario(super::Backend::Sqlite).await;
                }
            }
        )*
    };
}

per_backend!(database_mode_tenant_admin, single_database_admin);

/// The same on a schema-mode tenant, whose `search_path` also reaches `public`.
#[cfg(feature = "postgres")]
#[tokio::test]
async fn schema_mode_tenant_admin() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, registry_url, _db)) = fresh(Backend::Postgres, tmp.path(), "schema").await
    else {
        eprintln!("skipping — DATABASE_URL unset");
        return;
    };
    let dir = tmp.path().join("app/migrations");
    std::fs::create_dir_all(&dir).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry migrate");
    let mut org = rustango::tenancy::Org {
        slug: "t1".into(),
        display_name: "t1".into(),
        storage_mode: rustango::tenancy::StorageMode::Schema.as_str().into(),
        backend_kind: "postgres".into(),
        schema_name: Some("t1".into()),
        database_url: None,
        ..rustango::testkit::org()
    };
    org.insert_pool(&registry).await.unwrap();
    let pg = registry.as_postgres().expect("a Postgres pool");
    let pools = rustango::tenancy::TenantPools::new(pg.clone());
    let report = rustango::tenancy::migrate_tenants(&pools, &dir, &registry_url)
        .await
        .expect("tenants");
    assert!(report.all_ok(), "{report:?}");
    let tenant = pools.scoped_pool_dyn(&org).await.unwrap();
    assert_passkeys_on_tenant_only(&tenant, &registry).await;
    let router = tenant_admin(tenant);
    let listed = assert_listed_tables_exist(&router, "schema-mode tenant admin").await;
    assert_no_registry_tables(&listed);
    assert_no_passkey_admin(&router).await;
    assert_no_translations_editor(&router, &registry).await;

    // `search_path` falls back to `public`, so a 200 alone can't show
    // the table is the tenant's own. No public introspection API exists.
    let in_t1: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables WHERE table_schema = 't1'",
    )
    .fetch_all(pg)
    .await
    .unwrap();
    let foreign: Vec<&String> = listed.iter().filter(|t| !in_t1.contains(t)).collect();
    assert!(
        foreign.is_empty(),
        "listed but not in schema t1: {foreign:?}"
    );
}

/// A passkey enrolled on one schema-mode tenant is not found on another (#2364).
#[cfg(all(feature = "postgres", feature = "passkey"))]
#[tokio::test]
async fn schema_mode_passkeys_stay_per_tenant() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((registry, registry_url, _db)) = fresh(Backend::Postgres, tmp.path(), "pk").await
    else {
        eprintln!("skipping — DATABASE_URL unset");
        return;
    };
    let dir = tmp.path().join("app/migrations");
    std::fs::create_dir_all(&dir).unwrap();
    rustango::tenancy::migrate_registry_pool(&registry, &dir)
        .await
        .expect("registry migrate");
    let mut orgs = Vec::new();
    for slug in ["pa", "pb"] {
        let mut org = rustango::tenancy::Org {
            slug: slug.into(),
            display_name: slug.into(),
            storage_mode: rustango::tenancy::StorageMode::Schema.as_str().into(),
            backend_kind: "postgres".into(),
            schema_name: Some(slug.into()),
            database_url: None,
            ..rustango::testkit::org()
        };
        org.insert_pool(&registry).await.unwrap();
        orgs.push(org);
    }
    let pools = rustango::tenancy::TenantPools::new(registry.as_postgres().unwrap().clone());
    let report = rustango::tenancy::migrate_tenants(&pools, &dir, &registry_url)
        .await
        .expect("tenants");
    assert!(report.all_ok(), "{report:?}");
    let a = pools.scoped_pool_dyn(&orgs[0]).await.unwrap();
    let b = pools.scoped_pool_dyn(&orgs[1]).await.unwrap();
    rustango::passkey::register(&a, 7, "cred-a", vec![1, 2, 3], 0, "laptop")
        .await
        .expect("enroll on tenant a");
    assert!(rustango::passkey::by_credential_id(&a, "cred-a")
        .await
        .unwrap()
        .is_some());
    let on_b = rustango::passkey::by_credential_id(&b, "cred-a").await;
    assert!(
        matches!(on_b, Ok(None)),
        "tenant b sees a's passkey: {on_b:?}"
    );
    assert!(
        rustango::passkey::for_user(&registry, 7).await.is_err(),
        "the registry holds a passkey table"
    );
}
