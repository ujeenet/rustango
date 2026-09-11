#![cfg(feature = "tenancy")]
//! Tenant-database pre-flight (#1319) and the connect-failure taxonomy
//! (#1306), on all three dialects.
//!
//! The point is not "did it return an error" — the old code managed
//! that. It is that **connection refused on :5432**, **database `acme`
//! does not exist**, **role `app` cannot create tables** and **something
//! else is on this port** are four different operator actions, and the
//! message has to name which one.
//!
//! PG reads `DATABASE_URL`, MySQL reads `MYSQL_TEST_URL`; both skip when
//! unset. SQLite always runs.

use rustango::sql::ConnectFault;
use rustango::tenancy::preflight::{self, Preflight};

/// A port nothing is on. High and odd enough to be safe in CI.
const DEAD_PORT: u16 = 59_417;

/// Point a URL at [`DEAD_PORT`], whatever port it currently names.
///
/// The obvious version — `url.replace("5457", …)` — hardcodes whatever
/// port the developer's local container happened to use, so in CI
/// (where the URL says `:5432`) the replace silently does nothing and
/// the "unreachable" test connects to a perfectly healthy database.
/// That is not a test failing loudly; it is a test asserting the
/// opposite of what it claims, and only the `expect_err` caught it.
fn at_a_dead_port(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").expect("url has a scheme");
    // Userinfo may contain `@` and `:`; the authority is what follows
    // the LAST `@`, and the path starts at the first `/` after that.
    let (userinfo, authority) = match rest.rsplit_once('@') {
        Some((u, a)) => (format!("{u}@"), a),
        None => (String::new(), rest),
    };
    let (hostport, tail) = match authority.find('/') {
        Some(i) => (&authority[..i], &authority[i..]),
        None => (authority, ""),
    };
    let host = hostport.rsplit_once(':').map_or(hostport, |(h, _)| h);
    format!("{scheme}://{userinfo}{host}:{DEAD_PORT}{tail}")
}

#[test]
fn the_dead_port_helper_rewrites_whatever_port_is_there() {
    assert_eq!(
        at_a_dead_port("postgres://u:p@localhost:5432/db"),
        format!("postgres://u:p@localhost:{DEAD_PORT}/db")
    );
    // No port at all — one still has to be added, or the test would
    // hit the backend's default and reach a real server.
    assert_eq!(
        at_a_dead_port("mysql://root@127.0.0.1/app"),
        format!("mysql://root@127.0.0.1:{DEAD_PORT}/app")
    );
    // A password containing `@` must not be mistaken for the
    // authority separator.
    assert_eq!(
        at_a_dead_port("postgres://u:p@ss@db.internal:5432/x"),
        format!("postgres://u:p@ss@db.internal:{DEAD_PORT}/x")
    );
}

// ---------------------------------------------------------------- SQLite

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::{preflight, ConnectFault, Preflight};

    #[tokio::test]
    async fn a_writable_file_passes_including_the_write_probe() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("ok.db").display());

        let ok = preflight::check(&url, &Preflight::default())
            .await
            .expect("a fresh sqlite file should be usable");
        assert!(
            ok.writes_verified,
            "the write probe should have run and passed"
        );
    }

    /// SQLite's "nothing there" needs an explicit `mode=ro`.
    ///
    /// rustango appends `?mode=rwc` to any sqlite URL that does not
    /// name a mode (`ensure_sqlite_rwc_default`), so a missing file is
    /// *created*, deliberately — which means "the file does not exist"
    /// is simply not a failure on this dialect. Pinning that here so
    /// the asymmetry with PG/MySQL is recorded rather than rediscovered.
    #[tokio::test]
    async fn a_missing_file_is_created_unless_the_url_says_read_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let absent = tmp.path().join("absent.db");

        // Default mode: created, and usable.
        let created = preflight::check(
            &format!("sqlite://{}", absent.display()),
            &Preflight::default(),
        )
        .await
        .expect("rustango defaults sqlite to mode=rwc, so this is created");
        assert!(created.writes_verified);

        // Explicit read-only against a path that really is not there.
        let url = format!("sqlite://{}?mode=ro", tmp.path().join("nope.db").display());
        let err = preflight::check(&url, &Preflight::read_only())
            .await
            .expect_err("a read-only open of a missing file must fail");
        assert!(
            matches!(
                err.fault,
                ConnectFault::Unreachable | ConnectFault::PermissionDenied | ConnectFault::Other
            ),
            "got {:?}: {err}",
            err.fault
        );
    }

    /// `read_only()` must genuinely not write. Proven by the absence of
    /// any `rustango_preflight_*` table afterwards.
    #[tokio::test]
    async fn the_read_only_probe_leaves_no_table_behind() {
        use rustango::sql::sqlx;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ro.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());

        let ok = preflight::check(&url, &Preflight::read_only())
            .await
            .expect("reachable");
        assert!(!ok.writes_verified);

        let pool = sqlx::SqlitePool::connect(&url).await.expect("reconnect");
        let names: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .expect("list tables");
        assert!(
            names
                .iter()
                .all(|(n,)| !n.starts_with("rustango_preflight")),
            "read-only probe wrote a table: {names:?}"
        );
    }

    /// And the full probe cleans up after itself.
    #[tokio::test]
    async fn the_write_probe_drops_its_own_table() {
        use rustango::sql::sqlx;

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cleanup.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());

        preflight::check(&url, &Preflight::default())
            .await
            .expect("reachable");

        let pool = sqlx::SqlitePool::connect(&url).await.expect("reconnect");
        let names: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .expect("list tables");
        assert!(
            names
                .iter()
                .all(|(n,)| !n.starts_with("rustango_preflight")),
            "probe table was left behind: {names:?}"
        );
    }
}

// ------------------------------------------------------------ PostgreSQL

#[cfg(feature = "postgres")]
mod pg {
    use super::{preflight, ConnectFault, Preflight};

    fn base() -> Option<String> {
        std::env::var("DATABASE_URL").ok()
    }

    /// Swap the database name in a `postgres://…/name` URL.
    fn with_db(url: &str, db: &str) -> String {
        let (head, _) = url.rsplit_once('/').expect("url has a database segment");
        format!("{head}/{db}")
    }

    /// Swap the password.
    fn with_password(url: &str, pw: &str) -> String {
        let (scheme, rest) = url.split_once("://").expect("scheme");
        let (userinfo, hostpart) = rest.split_once('@').expect("userinfo");
        let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
        format!("{scheme}://{user}:{pw}@{hostpart}")
    }

    #[tokio::test]
    async fn a_healthy_database_passes_the_write_probe() {
        let Some(url) = base() else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let ok = preflight::check(&url, &Preflight::default())
            .await
            .expect("healthy postgres");
        assert!(ok.writes_verified);
        // The endpoint is echoed back with no password in it.
        assert!(
            !ok.endpoint.contains("postgres:postgres"),
            "{}",
            ok.endpoint
        );
    }

    #[tokio::test]
    async fn a_dead_port_reads_as_unreachable() {
        let Some(url) = base() else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let dead = super::at_a_dead_port(&url);
        let err = preflight::check(&dead, &Preflight::default())
            .await
            .expect_err("nothing is listening there");
        assert_eq!(err.fault, ConnectFault::Unreachable, "{err}");
        assert!(err.to_string().contains("nothing is listening"), "{err}");
    }

    #[tokio::test]
    async fn a_wrong_password_reads_as_auth_failed_not_as_unreachable() {
        let Some(url) = base() else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let err = preflight::check(
            &with_password(&url, "definitely-not-it"),
            &Preflight::default(),
        )
        .await
        .expect_err("bad credentials");
        assert_eq!(err.fault, ConnectFault::AuthFailed, "{err}");
        assert!(
            err.to_string().contains("refused these credentials"),
            "{err}"
        );
        // The bad password must not be echoed back.
        assert!(
            !err.to_string().contains("definitely-not-it"),
            "password leaked: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_database_says_so_rather_than_failing_generically() {
        let Some(url) = base() else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let err = preflight::check(&with_db(&url, "no_such_db_here"), &Preflight::default())
            .await
            .expect_err("database does not exist");
        assert_eq!(err.fault, ConnectFault::NoSuchDatabase, "{err}");
        assert!(err.to_string().contains("no such database"), "{err}");
    }

    /// The case a naive `SELECT 1` check sails past and a migration
    /// then dies on: a role that can connect and read but cannot
    /// create tables.
    #[tokio::test]
    async fn a_role_that_cannot_create_tables_is_caught_by_the_write_probe() {
        use rustango::sql::sqlx;

        let Some(url) = base() else {
            eprintln!("skipping: DATABASE_URL unset");
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.expect("admin connect");
        // A role with LOGIN and nothing else. `public` in PG 15+ no
        // longer grants CREATE to everyone, which is what makes this
        // reproducible.
        let _ = sqlx::query("DROP ROLE IF EXISTS rgo_nocreate")
            .execute(&admin)
            .await;
        sqlx::query("CREATE ROLE rgo_nocreate LOGIN PASSWORD 'pw'")
            .execute(&admin)
            .await
            .expect("create role");
        sqlx::query("REVOKE CREATE ON SCHEMA public FROM PUBLIC")
            .execute(&admin)
            .await
            .expect("revoke");

        let (_, hostpart) = url.split_once('@').expect("userinfo");
        let limited = format!("postgres://rgo_nocreate:pw@{hostpart}");

        // Read-only: passes, because reading is all it checks.
        preflight::check(&limited, &Preflight::read_only())
            .await
            .expect("a read-only check should pass — which is the problem");

        // Full: caught.
        let err = preflight::check(&limited, &Preflight::default())
            .await
            .expect_err("the write probe must catch this");
        assert_eq!(err.fault, ConnectFault::PermissionDenied, "{err}");
        assert!(err.to_string().contains("cannot create tables"), "{err}");

        let _ = sqlx::query("GRANT CREATE ON SCHEMA public TO PUBLIC")
            .execute(&admin)
            .await;
        let _ = sqlx::query("DROP ROLE IF EXISTS rgo_nocreate")
            .execute(&admin)
            .await;
    }

    /// #1306: pointing a `postgres://` URL at a server that does not
    /// speak the Postgres protocol must read as "wrong server on this
    /// port", not as a locale problem. MySQL stands in for "a different
    /// server", which is exactly the shape of the original bug.
    #[tokio::test]
    async fn a_mysql_server_on_a_postgres_url_reads_as_the_wrong_server() {
        let Ok(my) = std::env::var("MYSQL_TEST_URL") else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let (_, hostpart) = my.split_once('@').expect("userinfo");
        let confused = format!("postgres://root:my@{hostpart}");

        let err = preflight::check(&confused, &Preflight::default())
            .await
            .expect_err("a MySQL server does not speak the Postgres protocol");
        assert!(
            matches!(
                err.fault,
                ConnectFault::UnexpectedServer | ConnectFault::Unreachable
            ),
            "got {:?}: {err}",
            err.fault
        );
    }
}

// ------------------------------------------------------------------ MySQL

#[cfg(feature = "mysql")]
mod mysql {
    use super::{preflight, ConnectFault, Preflight};

    fn base() -> Option<String> {
        std::env::var("MYSQL_TEST_URL").ok()
    }

    #[tokio::test]
    async fn a_healthy_database_passes_the_write_probe() {
        let Some(url) = base() else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let ok = preflight::check(&url, &Preflight::default())
            .await
            .expect("healthy mysql");
        assert!(ok.writes_verified);
        assert!(!ok.endpoint.contains("root:my"), "{}", ok.endpoint);
    }

    #[tokio::test]
    async fn a_dead_port_reads_as_unreachable() {
        let Some(url) = base() else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let dead = super::at_a_dead_port(&url);
        let err = preflight::check(&dead, &Preflight::default())
            .await
            .expect_err("nothing is listening there");
        assert_eq!(err.fault, ConnectFault::Unreachable, "{err}");
    }

    #[tokio::test]
    async fn a_wrong_password_reads_as_auth_failed() {
        let Some(url) = base() else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let (scheme, rest) = url.split_once("://").expect("scheme");
        let (userinfo, hostpart) = rest.split_once('@').expect("userinfo");
        let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
        let bad = format!("{scheme}://{user}:nope@{hostpart}");

        let err = preflight::check(&bad, &Preflight::default())
            .await
            .expect_err("bad credentials");
        assert_eq!(err.fault, ConnectFault::AuthFailed, "{err}");
        assert!(
            !err.to_string().contains(":nope@"),
            "password leaked: {err}"
        );
    }

    #[tokio::test]
    async fn a_missing_database_says_so() {
        let Some(url) = base() else {
            eprintln!("skipping: MYSQL_TEST_URL unset");
            return;
        };
        let (head, _) = url.rsplit_once('/').expect("database segment");
        let err = preflight::check(&format!("{head}/no_such_db_here"), &Preflight::default())
            .await
            .expect_err("database does not exist");
        assert_eq!(err.fault, ConnectFault::NoSuchDatabase, "{err}");
    }
}
