//! Tenant database and login DDL on every backend: create, isolate,
//! refuse duplicates, roll back, probe rights, drop.
//!
//! The server arms need an admin login: `DATABASE_URL` (Postgres) and
//! `MYSQL_TEST_URL` (MySQL) must be able to create databases and users.

#![cfg(feature = "tenancy")]

use std::time::Duration;

use rustango::sql::connect_diagnosis::ConnectFault;
use rustango::sql::{DatabaseName, Login, LoginName, LoginPassword, Pool};
use rustango::tenancy::server_admin::{ServerAdmin, TenantDbError};

const T: Duration = Duration::from_secs(10);

fn name(tag: &str) -> DatabaseName {
    DatabaseName::new(format!("rt{}-{tag}", std::process::id())).expect("test name")
}

fn login_for(db: &DatabaseName) -> Login {
    Login::new(LoginName::new(db.as_str()).expect("test login"))
}

/// `url` with its userinfo replaced.
fn as_user(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap();
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let enc = rustango::url_codec::url_encode;
    format!("{scheme}://{}:{}@{host}", enc(user), enc(password))
}

/// `url` pointed at database `db`.
fn on_db(url: &str, db: &str) -> String {
    let (base, rest) = url.rsplit_once('/').unwrap();
    let query = rest.split_once('?').map(|(_, q)| format!("?{q}"));
    format!("{base}/{db}{}", query.unwrap_or_default())
}

async fn connects(url: &str) -> bool {
    match Pool::connect_diagnosed(url, T).await {
        Ok(p) => {
            p.close().await;
            true
        }
        Err(_) => false,
    }
}

/// Whether the database exists, seen by the admin.
async fn database_exists(admin: &ServerAdmin, admin_url: Option<&str>, db: &DatabaseName) -> bool {
    match admin_url {
        Some(url) => match Pool::connect_admin(url, Some(db.as_str()), T).await {
            Ok(p) => {
                p.close().await;
                true
            }
            Err(d) => {
                assert_eq!(d.fault, ConnectFault::NoSuchDatabase, "{d}");
                false
            }
        },
        None => admin
            .sqlite_path(db)
            .expect("sqlite admin")
            .exists(),
    }
}

async fn clean(admin: &ServerAdmin, dbs: &[&DatabaseName]) {
    for db in dbs {
        admin
            .drop_tenant_database(db, &login_for(db))
            .await
            .expect("pre-clean");
    }
}

async fn lifecycle(admin: &ServerAdmin, admin_url: Option<&str>) {
    let (a, b) = (name("life-a"), name("life-b"));
    clean(admin, &[&a, &b]).await;

    let ca = admin
        .create_tenant_database(&a, &login_for(&a), &LoginPassword::generate())
        .await
        .expect("create a");
    let cb = admin
        .create_tenant_database(&b, &login_for(&b), &LoginPassword::generate())
        .await
        .expect("create b");
    assert!(database_exists(admin, admin_url, &a).await);

    // The tenant login can build its schema.
    let tenant = Pool::connect_diagnosed(ca.url(), T)
        .await
        .expect("tenant connects");
    rustango::sql::raw_execute_pool(&tenant, "CREATE TABLE notes (id INTEGER)", vec![])
        .await
        .expect("tenant creates a table");
    rustango::sql::raw_execute_pool(&tenant, "INSERT INTO notes (id) VALUES (1)", vec![])
        .await
        .expect("tenant writes");
    tenant.close().await;

    if admin_url.is_some() {
        // A's login cannot reach B's database.
        let cross = on_db(ca.url(), b.as_str());
        assert!(!connects(&cross).await, "a's login reached b's database");
        assert!(connects(cb.url()).await);
    }

    admin
        .drop_tenant_database(&a, &login_for(&a))
        .await
        .expect("drop a");
    admin
        .drop_tenant_database(&b, &login_for(&b))
        .await
        .expect("drop b");
    assert!(!database_exists(admin, admin_url, &a).await);
    assert!(!database_exists(admin, admin_url, &b).await);
    if admin_url.is_some() {
        // The login went with it: not even the maintenance target admits it.
        assert!(ServerAdmin::connect(ca.url(), T).await.is_err());
    }
    // Dropping again is a no-op.
    admin
        .drop_tenant_database(&a, &login_for(&a))
        .await
        .expect("second drop");
}

async fn existing_database_or_login_is_refused(admin: &ServerAdmin, admin_url: Option<&str>) {
    let (a, b, c) = (name("ex-a"), name("ex-b"), name("ex-c"));
    clean(admin, &[&a, &b, &c]).await;
    let ca = admin
        .create_tenant_database(&a, &login_for(&a), &LoginPassword::generate())
        .await
        .expect("create a");

    let again = admin
        .create_tenant_database(&a, &login_for(&b), &LoginPassword::generate())
        .await;
    assert!(
        matches!(again, Err(TenantDbError::DatabaseExists(ref d)) if *d == a),
        "{again:?}"
    );

    if admin_url.is_some() {
        // A taken login: the new database is rolled back, the login kept.
        let taken = admin
            .create_tenant_database(&c, &login_for(&a), &LoginPassword::generate())
            .await;
        assert!(
            matches!(taken, Err(TenantDbError::LoginExists(ref l)) if l.as_str() == a.as_str()),
            "{taken:?}"
        );
        assert!(!database_exists(admin, admin_url, &c).await, "c survived");
        assert!(connects(ca.url()).await, "the existing login was dropped");
    }
    clean(admin, &[&a, &b, &c]).await;
}

async fn password_with_quotes_and_backslashes(admin: &ServerAdmin) {
    let a = name("pw");
    clean(admin, &[&a]).await;
    let pw = LoginPassword::new(r#"x'y\z"w`$1; DROP DATABASE rustango_test; --\"#).unwrap();
    let ca = admin
        .create_tenant_database(&a, &login_for(&a), &pw)
        .await
        .expect("create with an odd password");
    assert!(connects(ca.url()).await, "the stored password differs");
    assert!(!format!("{ca:?}").contains("DROP DATABASE"), "{ca:?}");
    clean(admin, &[&a]).await;
}

async fn probe_reports_rights(admin: &ServerAdmin, admin_url: Option<&str>) {
    let report = admin.probe().await.expect("probe");
    assert!(report.can_login);
    assert!(report.can_create_database, "{report:?}");
    assert!(report.can_create_user, "{report:?}");
    assert!(report.can_grant, "{report:?}");
    assert!(report.can_provision());
    let Some(url) = admin_url else {
        let missing = ServerAdmin::sqlite_dir("/nonexistent/rustango-probe");
        let report = missing.probe().await.expect("probe missing dir");
        assert!(!report.can_create_database);
        return;
    };
    assert!(report.server_version.as_deref().is_some_and(|v| !v.is_empty()));

    // A tenant login is a plain one.
    let a = name("probe");
    clean(admin, &[&a]).await;
    let ca = admin
        .create_tenant_database(&a, &login_for(&a), &LoginPassword::generate())
        .await
        .expect("create");
    let plain = ServerAdmin::probe_url(ca.url(), T).await.expect("probe plain");
    assert!(plain.can_login, "{plain:?}");
    assert!(!plain.can_create_database, "{plain:?}");
    assert!(!plain.can_create_user, "{plain:?}");
    assert!(!plain.can_provision());

    let wrong = ServerAdmin::probe_url(&as_user(url, a.as_str(), "wrong"), T)
        .await
        .expect("a refused login is a report");
    assert!(!wrong.can_login, "{wrong:?}");
    clean(admin, &[&a]).await;
}

/// A step after the database and login exist fails: both are removed.
async fn failed_grant_rolls_back(admin: &ServerAdmin, admin_url: &str, limit: &[&str]) {
    let (lim, a) = (name("rb-adm"), name("rb-t"));
    clean(admin, &[&lim, &a]).await;
    let pool = Pool::connect_admin(admin_url, None, T).await.expect("admin pool");
    let pw = LoginPassword::new("rb-pass").unwrap();
    let sql = pool
        .dialect()
        .create_login_sql(&login_for(&lim), &pw)
        .expect("server backend");
    rustango::sql::raw_execute_pool(&pool, &sql, vec![]).await.expect("limited admin");
    for stmt in limit {
        let stmt = stmt.replace("{u}", lim.as_str());
        rustango::sql::raw_execute_pool(&pool, &stmt, vec![])
            .await
            .expect("limit the admin");
    }
    pool.close().await;

    // Short: on Postgres the refused second connection waits this out.
    let short = Duration::from_secs(2);
    let limited = ServerAdmin::connect(&as_user(admin_url, lim.as_str(), "rb-pass"), short)
        .await
        .expect("limited admin connects");
    let failed = limited
        .create_tenant_database(&a, &login_for(&a), &LoginPassword::generate())
        .await;
    assert!(failed.is_err(), "the grant step was expected to fail");
    eprintln!("grant failure: {failed:?}");
    limited.close().await;

    assert!(!database_exists(admin, Some(admin_url), &a).await, "database survived");
    // The login is gone too: creating it again succeeds.
    admin
        .create_tenant_database(&a, &login_for(&a), &LoginPassword::generate())
        .await
        .expect("login and database were rolled back");
    clean(admin, &[&a, &lim]).await;
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    async fn admin() -> Option<(ServerAdmin, String)> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let admin = ServerAdmin::connect(&url, T).await.expect("admin connects");
        Some((admin, url))
    }

    #[tokio::test]
    async fn lifecycle() {
        let Some((a, url)) = admin().await else { return };
        super::lifecycle(&a, Some(&url)).await;
    }

    #[tokio::test]
    async fn existing_database_or_login_is_refused() {
        let Some((a, url)) = admin().await else { return };
        super::existing_database_or_login_is_refused(&a, Some(&url)).await;
    }

    #[tokio::test]
    async fn password_with_quotes_and_backslashes() {
        let Some((a, _)) = admin().await else { return };
        super::password_with_quotes_and_backslashes(&a).await;
    }

    #[tokio::test]
    async fn probe_reports_rights() {
        let Some((a, url)) = admin().await else { return };
        super::probe_reports_rights(&a, Some(&url)).await;
    }

    /// One connection allowed: the in-database grant cannot open its own.
    #[tokio::test]
    async fn failed_grant_rolls_back() {
        let Some((a, url)) = admin().await else { return };
        super::failed_grant_rolls_back(
            &a,
            &url,
            &[r#"ALTER ROLE "{u}" CREATEDB CREATEROLE CONNECTION LIMIT 1"#],
        )
        .await;
    }
}

#[cfg(feature = "mysql")]
mod mysql {
    use super::*;

    async fn admin() -> Option<(ServerAdmin, String)> {
        let url = std::env::var("MYSQL_TEST_URL").ok()?;
        let admin = ServerAdmin::connect(&url, T).await.expect("admin connects");
        Some((admin, url))
    }

    #[tokio::test]
    async fn lifecycle() {
        let Some((a, url)) = admin().await else { return };
        super::lifecycle(&a, Some(&url)).await;
    }

    #[tokio::test]
    async fn existing_database_or_login_is_refused() {
        let Some((a, url)) = admin().await else { return };
        super::existing_database_or_login_is_refused(&a, Some(&url)).await;
    }

    #[tokio::test]
    async fn password_with_quotes_and_backslashes() {
        let Some((a, _)) = admin().await else { return };
        super::password_with_quotes_and_backslashes(&a).await;
    }

    #[tokio::test]
    async fn probe_reports_rights() {
        let Some((a, url)) = admin().await else { return };
        super::probe_reports_rights(&a, Some(&url)).await;
    }

    /// No GRANT OPTION: the database and login are made, the grant is denied.
    #[tokio::test]
    async fn failed_grant_rolls_back() {
        let Some((a, url)) = admin().await else { return };
        super::failed_grant_rolls_back(
            &a,
            &url,
            &["GRANT CREATE, DROP, CREATE USER ON *.* TO `{u}`@`%`"],
        )
        .await;
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::*;

    fn admin() -> (ServerAdmin, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        (ServerAdmin::sqlite_dir(dir.path()), dir)
    }

    #[tokio::test]
    async fn lifecycle() {
        let (a, _dir) = admin();
        super::lifecycle(&a, None).await;
    }

    #[tokio::test]
    async fn existing_database_or_login_is_refused() {
        let (a, _dir) = admin();
        super::existing_database_or_login_is_refused(&a, None).await;
    }

    #[tokio::test]
    async fn probe_reports_rights() {
        let (a, _dir) = admin();
        super::probe_reports_rights(&a, None).await;
    }

    /// Drop removes the WAL side files too.
    #[tokio::test]
    async fn drop_removes_side_files() {
        let (a, dir) = admin();
        let db = name("wal");
        let created = a
            .create_tenant_database(&db, &login_for(&db), &LoginPassword::generate())
            .await
            .expect("create");
        // Held open, so the `-wal` and `-shm` files exist at drop time.
        let pool = Pool::connect(created.url()).await.expect("open");
        rustango::sql::raw_execute_pool(&pool, "CREATE TABLE t (id INTEGER)", vec![])
            .await
            .unwrap();
        a.drop_tenant_database(&db, &login_for(&db)).await.expect("drop");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        pool.close().await;
        assert!(left.is_empty(), "{left:?}");
    }
}
