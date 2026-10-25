//! Create and drop a database-mode tenant's database and login from a
//! server admin connection, and check that the admin may.
//!
//! Postgres and MySQL get a database named as the slug plus a login
//! granted only that database. SQLite gets `<dir>/<slug>.db` and no login.
//! All SQL comes from the [`crate::sql::Dialect`] emitters.
//!
//! Tenant databases need PostgreSQL 15 or later: before 15 every login may
//! create tables in `public` of every shared database (registry, template1).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sql::connect_diagnosis::{ConnectDiagnosis, ConnectFault};
use crate::sql::{DatabaseName, ExecError, Login, LoginName, LoginPassword, Pool};

use super::TenancyError;
use crate::sql::sqlx;

/// Why a tenant database operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TenantDbError {
    #[error("database `{0}` already exists — choose another slug, or drop it first")]
    DatabaseExists(DatabaseName),
    #[error("login `{0}` already exists — choose another, or drop it first")]
    LoginExists(LoginName),
    #[error("database `{0}` has open sessions — close them and drop again")]
    InUse(DatabaseName),
    #[error("the admin login may not {step}: {detail}")]
    Denied { step: &'static str, detail: String },
    /// The admin's own database or login, or a name passed to
    /// [`ServerAdmin::with_protected`].
    #[error("refusing to drop `{0}` — it is protected")]
    Protected(String),
    #[error("{0}")]
    Connect(ConnectDiagnosis),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The operation failed and undoing it failed too.
    #[error("{cause}; undoing it also failed ({undo}), so {leftover} may remain")]
    RollbackFailed {
        cause: Box<TenantDbError>,
        undo: Box<TenantDbError>,
        leftover: String,
    },
}

impl From<TenantDbError> for TenancyError {
    fn from(e: TenantDbError) -> Self {
        match e {
            TenantDbError::Exec(e) => Self::Exec(e),
            TenantDbError::Io(e) => Self::Io(e),
            TenantDbError::Connect(d) => Self::Validation(d.summary()),
            other => Self::Validation(other.to_string()),
        }
    }
}

/// What the admin login may do on its server.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // one flag per right probed
pub struct PrivilegeReport {
    /// `"postgres"`, `"mysql"` or `"sqlite"`.
    pub backend: &'static str,
    pub server_version: Option<String>,
    /// PostgreSQL's major version; `None` on other backends.
    pub pg_major: Option<u32>,
    pub can_login: bool,
    /// Why the login was refused, when it was.
    pub login_error: Option<ConnectDiagnosis>,
    /// Create and drop databases. SQLite: the directory is writable.
    pub can_create_database: bool,
    /// Create and drop logins. Always `true` on SQLite.
    pub can_create_user: bool,
    /// Grant a login all rights on one database (MySQL: GRANT OPTION on
    /// every database-level privilege). Always `true` on SQLite.
    pub can_grant: bool,
}

impl PrivilegeReport {
    /// Everything [`ServerAdmin::create_tenant_database`] needs.
    #[must_use]
    pub fn can_provision(&self) -> bool {
        self.blocker().is_none()
    }

    /// The first reason tenant databases cannot be made here.
    #[must_use]
    pub fn blocker(&self) -> Option<&'static str> {
        if !self.can_login {
            Some("the admin login is refused")
        } else if self.pg_major.is_some_and(|m| m < 15) {
            Some(
                "PostgreSQL 15 or later is needed: before 15 every login may create \
                 tables in the shared databases",
            )
        } else if !self.can_create_database {
            Some("the admin login may not create databases")
        } else if !self.can_create_user {
            Some("the admin login may not create logins")
        } else if !self.can_grant {
            Some("the admin login may not grant rights on a database")
        } else {
            None
        }
    }

    fn refused(backend: &'static str, why: ConnectDiagnosis) -> Self {
        Self {
            backend,
            server_version: None,
            pg_major: None,
            can_login: false,
            login_error: Some(why),
            can_create_database: false,
            can_create_user: false,
            can_grant: false,
        }
    }
}

/// A tenant database just made.
#[non_exhaustive]
pub struct Created {
    pub database: DatabaseName,
    /// `None` on SQLite.
    pub login: Option<Login>,
    url: String,
}

impl Created {
    /// The tenant's connection URL, as its own login. Holds the password.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl std::fmt::Debug for Created {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Created")
            .field("database", &self.database)
            .field("login", &self.login)
            .field("url", &crate::sql::connect_diagnosis::redact(&self.url))
            .finish()
    }
}

/// An admin connection to a database server, or a SQLite directory.
pub struct ServerAdmin {
    target: Target,
    /// Database and login names `drop_tenant_database` refuses.
    protected: Vec<String>,
}

enum Target {
    Server {
        pool: Pool,
        url: String,
        timeout: Duration,
    },
    Sqlite {
        dir: PathBuf,
    },
}

/// The MySQL database-level privileges `GRANT ALL ON db.*` hands out.
const MYSQL_DB_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "REFERENCES",
    "INDEX",
    "ALTER",
    "CREATE TEMPORARY TABLES",
    "LOCK TABLES",
    "EXECUTE",
    "CREATE VIEW",
    "SHOW VIEW",
    "CREATE ROUTINE",
    "ALTER ROUTINE",
    "EVENT",
    "TRIGGER",
];

/// MariaDB's `ALL` on a database also holds these.
const MARIADB_DB_PRIVILEGES: &[&str] = &["DELETE HISTORY"];

/// Whether `rows` of (privilege, grantable) cover `GRANT ALL ON db.*`.
fn mysql_can_grant(rows: &[(String, String)], mariadb: bool) -> bool {
    let grantable = |p: &str| {
        rows.iter()
            .any(|(r, g)| r.eq_ignore_ascii_case(p) && g.eq_ignore_ascii_case("YES"))
    };
    let extra: &[&str] = if mariadb { MARIADB_DB_PRIVILEGES } else { &[] };
    MYSQL_DB_PRIVILEGES.iter().chain(extra).all(|p| grantable(p))
}

/// `16` from `16.15` or `14.9 (Debian 14.9-1)`.
fn pg_major(version: &str) -> Option<u32> {
    let digits: String = version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The user and database an admin URL names, URL-decoded.
fn url_user_and_database(url: &str) -> (Option<String>, Option<String>) {
    let (user, rest) = match crate::sql::connect_diagnosis::split_userinfo(url) {
        Some((_, userinfo, rest)) => {
            let user = userinfo.split(':').next().unwrap_or(userinfo);
            (Some(crate::url_codec::percent_decode_path(user)), rest)
        }
        None => (None, url.split_once("://").map_or(url, |(_, r)| r)),
    };
    let path = rest.split('?').next().unwrap_or(rest);
    let db = path
        .split_once('/')
        .map(|(_, d)| crate::url_codec::percent_decode_path(d))
        .filter(|d| !d.is_empty());
    (user.filter(|u| !u.is_empty()), db)
}

impl ServerAdmin {
    /// Connect to the server behind `url` (`postgres://` or `mysql://`)
    /// as its admin login. DDL runs on the maintenance target; the URL's
    /// database and login are protected from [`Self::drop_tenant_database`].
    ///
    /// # Errors
    /// [`TenantDbError::Connect`] naming the fault.
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self, TenantDbError> {
        let pool = Pool::connect_admin(url, None, timeout)
            .await
            .map_err(TenantDbError::Connect)?;
        let (user, db) = url_user_and_database(url);
        Ok(Self {
            target: Target::Server {
                pool,
                url: url.to_owned(),
                timeout,
            },
            protected: user.into_iter().chain(db).collect(),
        })
    }

    /// SQLite tenants live as `<dir>/<slug>.db`.
    #[must_use]
    pub fn sqlite_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            target: Target::Sqlite { dir: dir.into() },
            protected: Vec::new(),
        }
    }

    /// Database and login names `drop_tenant_database` must refuse, such
    /// as the registry's. Callers should still drop only names they
    /// recorded when creating the tenant.
    #[must_use]
    pub fn with_protected<S: Into<String>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.protected.extend(names.into_iter().map(Into::into));
        self
    }

    fn is_protected(&self, name: &str) -> bool {
        self.protected.iter().any(|p| p.eq_ignore_ascii_case(name))
    }

    /// The file a SQLite tenant lives in; `None` for a server.
    #[must_use]
    pub fn sqlite_path(&self, db: &DatabaseName) -> Option<PathBuf> {
        match &self.target {
            Target::Sqlite { dir } => Some(sqlite_file(dir, db)),
            Target::Server { .. } => None,
        }
    }

    /// Connect as `url`'s login and report its rights. A refused login is
    /// a report with `can_login: false`, not an error.
    ///
    /// # Errors
    /// [`TenantDbError::Connect`] when the server cannot be reached.
    pub async fn probe_url(url: &str, timeout: Duration) -> Result<PrivilegeReport, TenantDbError> {
        match Self::connect(url, timeout).await {
            Ok(admin) => {
                let report = admin.probe().await;
                admin.close().await;
                report
            }
            Err(TenantDbError::Connect(d)) if d.fault == ConnectFault::AuthFailed => {
                let backend = if url.starts_with("mysql") {
                    "mysql"
                } else {
                    "postgres"
                };
                Ok(PrivilegeReport::refused(backend, d))
            }
            Err(e) => Err(e),
        }
    }

    /// What this admin login may do.
    ///
    /// MySQL reads global privileges only: rights from roles or
    /// database-pattern grants are not counted.
    ///
    /// # Errors
    /// The driver's error if a catalog query fails.
    pub async fn probe(&self) -> Result<PrivilegeReport, TenantDbError> {
        let pool = match &self.target {
            Target::Sqlite { dir } => return Ok(probe_dir(dir)),
            Target::Server { pool, .. } => pool,
        };
        let d = pool.dialect();
        let mut report = PrivilegeReport {
            backend: d.name(),
            server_version: None,
            pg_major: None,
            can_login: true,
            login_error: None,
            can_create_database: false,
            can_create_user: false,
            can_grant: false,
        };
        if let Some(sql) = d.server_version_sql() {
            let rows: Vec<(String,)> = crate::sql::raw_query_pool(sql, Vec::new(), pool).await?;
            report.server_version = rows.into_iter().next().map(|(v,)| v);
        }
        if d.name() == "postgres" {
            report.pg_major = report.server_version.as_deref().and_then(pg_major);
        }
        let Some(sql) = d.login_privileges_sql() else {
            return Ok(report);
        };
        if d.name() == "postgres" {
            let rows: Vec<(bool, bool, bool)> =
                crate::sql::raw_query_pool(sql, Vec::new(), pool).await?;
            if let Some((sup, createdb, createrole)) = rows.into_iter().next() {
                report.can_create_database = sup || createdb;
                report.can_create_user = sup || createrole;
                // The creator owns the database, so may grant on it.
                report.can_grant = report.can_create_database;
            }
        } else {
            let rows: Vec<(String, String)> =
                crate::sql::raw_query_pool(sql, Vec::new(), pool).await?;
            let has = |p: &str| rows.iter().any(|(r, _)| r.eq_ignore_ascii_case(p));
            // DROP too: a failed create is undone by dropping.
            report.can_create_database = has("CREATE") && has("DROP");
            report.can_create_user = has("CREATE USER");
            let mariadb = report
                .server_version
                .as_deref()
                .is_some_and(|v| v.contains("MariaDB"));
            report.can_grant = mysql_can_grant(&rows, mariadb);
        }
        Ok(report)
    }

    /// Create `name`, then `login` with `password`, then grant `login`
    /// all rights on `name` only. A failure after the first step drops
    /// what this call made. An existing database or login is refused,
    /// never adopted. SQLite ignores `login` and `password`.
    ///
    /// # Errors
    /// [`TenantDbError::DatabaseExists`] / [`TenantDbError::LoginExists`],
    /// [`TenantDbError::Denied`] for a missing right, or the step's error.
    pub async fn create_tenant_database(
        &self,
        name: &DatabaseName,
        login: &Login,
        password: &LoginPassword,
    ) -> Result<Created, TenantDbError> {
        let (pool, url, timeout) = match &self.target {
            Target::Sqlite { dir } => return create_sqlite(dir, name),
            Target::Server { pool, url, timeout } => (pool, url, *timeout),
        };
        // Built first: nothing to undo when the admin URL cannot yield one.
        let tenant = tenant_url(url, login, password, name)?;
        let d = pool.dialect();
        let sql = d
            .create_database_sql(name)
            .ok_or(TenantDbError::Unsupported("this backend has no databases to create"))?;
        step(pool, &sql, "create a database")
            .await
            .map_err(|e| exists_as(e, || TenantDbError::DatabaseExists(name.clone())))?;

        let mut login_made = false;
        let rest = async {
            let taken = || TenantDbError::LoginExists(login.name().clone());
            // `None` when the admin may not read the user catalog.
            let existed = login_exists(pool, login).await;
            if existed == Some(true) {
                return Err(taken());
            }
            let sql = d
                .create_login_sql(login, password)
                .ok_or(TenantDbError::Unsupported("this backend has no logins"))?;
            if let Err(e) = step(pool, &sql, "create a login").await {
                let f = match &e {
                    TenantDbError::Exec(x) => fault(x),
                    TenantDbError::Denied { .. } => Fault::Denied,
                    _ => Fault::Other,
                };
                // The server may have made it before the error reached us.
                login_made = undo_login_after_failure(f, existed);
                return Err(exists_as(e, taken));
            }
            login_made = true;
            for sql in d.grant_database_sql(name, login) {
                step(pool, &sql, "grant a login its database").await?;
            }
            let inside = d.grant_in_database_sql(login);
            if !inside.is_empty() {
                let db = Pool::connect_admin(url, Some(name.as_str()), timeout)
                    .await
                    .map_err(TenantDbError::Connect)?;
                let mut res = Ok(());
                for sql in &inside {
                    res = step(&db, sql, "grant a login its database").await;
                    if res.is_err() {
                        break;
                    }
                }
                // Closed before any rollback: a session inside blocks the drop.
                db.close().await;
                res?;
            }
            Ok::<_, TenantDbError>(())
        }
        .await;

        if let Err(cause) = rest {
            let undo = undo_create(pool, name, login_made.then_some(login)).await;
            return Err(match undo {
                Ok(()) => cause,
                Err(undo) => TenantDbError::RollbackFailed {
                    cause: Box::new(cause),
                    undo: Box::new(undo),
                    leftover: if login_made {
                        format!("database `{name}` and login `{}`", login.name())
                    } else {
                        format!("database `{name}`")
                    },
                },
            });
        }
        Ok(Created {
            database: name.clone(),
            login: Some(login.clone()),
            url: tenant,
        })
    }

    /// Drop `name` and then `login`; each is skipped when absent.
    /// SQLite deletes the file and its `-wal` / `-shm` / `-journal`.
    ///
    /// Drops whatever valid names it is given, short of the protected
    /// ones: pass the names recorded when the tenant was created.
    ///
    /// # Errors
    /// [`TenantDbError::Protected`], [`TenantDbError::InUse`] while
    /// sessions are open, or the step's error.
    pub async fn drop_tenant_database(
        &self,
        name: &DatabaseName,
        login: &Login,
    ) -> Result<(), TenantDbError> {
        if self.is_protected(name.as_str()) {
            return Err(TenantDbError::Protected(name.to_string()));
        }
        match &self.target {
            Target::Sqlite { dir } => drop_sqlite(dir, name),
            Target::Server { pool, .. } => {
                if self.is_protected(login.name().as_str()) {
                    return Err(TenantDbError::Protected(login.name().to_string()));
                }
                drop_server(pool, name, Some(login), false).await
            }
        }
    }

    /// Close the admin connection.
    pub async fn close(self) {
        if let Target::Server { pool, .. } = self.target {
            pool.close().await;
        }
    }
}

/// Undo a failed create. `force` ends sessions inside the new database
/// (Postgres 13+): the admin's own just-closed one can still be exiting.
async fn undo_create(
    pool: &Pool,
    name: &DatabaseName,
    login: Option<&Login>,
) -> Result<(), TenantDbError> {
    drop_server(pool, name, login, true).await
}

/// Whether a failed CREATE login may have left one behind that is ours
/// to drop: an unclear failure, and the login was absent beforehand.
fn undo_login_after_failure(fault: Fault, existed_before: Option<bool>) -> bool {
    !matches!(fault, Fault::Exists | Fault::Denied) && existed_before == Some(false)
}

/// `Some(true)` when `login` exists; `None` when it cannot be checked.
async fn login_exists(pool: &Pool, login: &Login) -> Option<bool> {
    let stmt = pool.dialect().login_exists_sql(login)?;
    let rows: Vec<(i64,)> = crate::sql::raw_query_pool(&stmt.sql, stmt.params, pool)
        .await
        .ok()?;
    rows.first().map(|(n,)| *n > 0)
}

/// Database first: Postgres refuses to drop a role still granted one.
async fn drop_server(
    pool: &Pool,
    name: &DatabaseName,
    login: Option<&Login>,
    force: bool,
) -> Result<(), TenantDbError> {
    let d = pool.dialect();
    let forced = force.then(|| d.force_drop_database_sql(name)).flatten();
    if let Some(sql) = forced.or_else(|| d.drop_database_sql(name)) {
        step(pool, &sql, "drop a database")
            .await
            .map_err(|e| match e {
                TenantDbError::Exec(ref x) if fault(x) == Fault::InUse => {
                    TenantDbError::InUse(name.clone())
                }
                other => other,
            })?;
    }
    if let Some(sql) = login.and_then(|l| d.drop_login_sql(l)) {
        step(pool, &sql, "drop a login").await?;
    }
    Ok(())
}

/// A deterministic login name for `db`: the slug itself when it is a
/// valid login (MySQL's 32 characters, not `root`), else a prefix plus
/// a hash of the whole slug.
///
/// # Panics
/// Never: a slug prefix plus `-<hex>` always passes [`LoginName::new`].
#[must_use]
pub fn tenant_login_name(db: &DatabaseName) -> LoginName {
    use sha2::{Digest, Sha256};
    let slug = db.as_str();
    LoginName::new(slug).unwrap_or_else(|_| {
        let h = Sha256::digest(slug.as_bytes());
        // 15 + `-` + 16 hex digits = 32 characters.
        let head = slug[..slug.len().min(15)].trim_end_matches('-');
        let mut name = format!("{head}-");
        for b in &h[..8] {
            let _ = std::fmt::Write::write_fmt(&mut name, format_args!("{b:02x}"));
        }
        LoginName::new(name).expect("a slug prefix plus a hash is a valid login name")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Exists,
    Denied,
    InUse,
    Other,
}

/// Classified on the SQLSTATE (Postgres) or error number (MySQL).
fn fault(e: &ExecError) -> Fault {
    let ExecError::Driver(sqlx::Error::Database(db)) = e else {
        return Fault::Other;
    };
    #[cfg(feature = "mysql")]
    if let Some(my) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
        return match my.number() {
            // 1007 database exists, 1396 CREATE USER failed (it exists).
            1007 | 1396 => Fault::Exists,
            // access denied: to database, command, specific privilege,
            // creating a user with GRANT.
            1044 | 1142 | 1227 | 1410 => Fault::Denied,
            _ => Fault::Other,
        };
    }
    pg_fault(db.code().as_deref(), db.constraint())
}

/// The Postgres decision, apart from the driver type so it can be tested.
fn pg_fault(code: Option<&str>, constraint: Option<&str>) -> Fault {
    match (code, constraint) {
        // duplicate_database, duplicate_object
        (Some("42P04" | "42710"), _) => Fault::Exists,
        // A concurrent create of the same name loses on the catalog index.
        (Some("23505"), Some("pg_database_datname_index" | "pg_authid_rolname_index")) => {
            Fault::Exists
        }
        (Some("42501"), _) => Fault::Denied,
        // object_in_use
        (Some("55006"), _) => Fault::InUse,
        _ => Fault::Other,
    }
}

/// An "already exists" failure as the caller's typed error.
fn exists_as(e: TenantDbError, typed: impl FnOnce() -> TenantDbError) -> TenantDbError {
    match e {
        TenantDbError::Exec(ref x) if fault(x) == Fault::Exists => typed(),
        other => other,
    }
}

/// Run one DDL statement; a missing right becomes [`TenantDbError::Denied`].
async fn step(pool: &Pool, sql: &str, what: &'static str) -> Result<(), TenantDbError> {
    match crate::sql::raw_execute_pool(pool, sql, Vec::new()).await {
        Ok(_) => Ok(()),
        Err(e) => Err(match fault(&e) {
            Fault::Denied => TenantDbError::Denied {
                step: what,
                detail: e.to_string(),
            },
            _ => TenantDbError::Exec(e),
        }),
    }
}

/// The admin URL's server and TLS keys, with `login` and `db`.
fn tenant_url(
    admin_url: &str,
    login: &Login,
    password: &LoginPassword,
    db: &DatabaseName,
) -> Result<String, TenantDbError> {
    let bad = || TenantDbError::Unsupported("the admin URL has no host to reuse");
    let (scheme, rest) = match crate::sql::connect_diagnosis::split_userinfo(admin_url) {
        Some((scheme, _, rest)) => (scheme, rest),
        None => admin_url.split_once("://").ok_or_else(bad)?,
    };
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let host = path.split('/').next().filter(|h| !h.is_empty()).ok_or_else(bad)?;
    let enc = crate::url_codec::url_encode;
    Ok(format!(
        "{scheme}://{}:{}@{host}/{db}{}",
        enc(login.name().as_str()),
        enc(password.expose()),
        super::provision::tls_query_suffix(query)
    ))
}

fn sqlite_file(dir: &Path, db: &DatabaseName) -> PathBuf {
    dir.join(format!("{db}.db"))
}

/// sqlx percent-decodes the path, so `%`, `?` and `#` are encoded.
fn sqlite_url(path: &Path) -> Result<String, TenantDbError> {
    let path = path
        .to_str()
        .ok_or(TenantDbError::Unsupported("the SQLite directory is not UTF-8"))?;
    let mut out = String::from("sqlite://");
    for c in path.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            c => out.push(c),
        }
    }
    out.push_str("?mode=rwc");
    Ok(out)
}

/// `create_new`, so an existing file is refused without a race.
fn create_sqlite(dir: &Path, name: &DatabaseName) -> Result<Created, TenantDbError> {
    let path = sqlite_file(dir, name);
    let url = sqlite_url(&path)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(TenantDbError::DatabaseExists(name.clone()));
        }
        Err(e) => return Err(e.into()),
    }
    Ok(Created {
        database: name.clone(),
        login: None,
        url,
    })
}

fn drop_sqlite(dir: &Path, name: &DatabaseName) -> Result<(), TenantDbError> {
    let path = sqlite_file(dir, name);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut file = path.clone().into_os_string();
        file.push(suffix);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The directory exists and a file can be made in it.
fn probe_dir(dir: &Path) -> PrivilegeReport {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(".rustango-probe-{nanos}"));
    let writable = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
    if writable {
        let _ = std::fs::remove_file(&probe);
    }
    PrivilegeReport {
        backend: "sqlite",
        server_version: None,
        pg_major: None,
        can_login: dir.is_dir(),
        login_error: None,
        can_create_database: writable,
        can_create_user: true,
        can_grant: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(s: &str) -> DatabaseName {
        DatabaseName::new(s).unwrap()
    }

    #[test]
    fn login_name_fits_mysql() {
        assert_eq!(tenant_login_name(&db("acme-shop")).as_str(), "acme-shop");
        let long = db(&format!("{}-x", "a".repeat(40)));
        let other = db(&format!("{}-y", "a".repeat(40)));
        let (l1, l2) = (tenant_login_name(&long), tenant_login_name(&other));
        assert!(l1.as_str().len() <= 32);
        assert_ne!(l1, l2);
        assert_eq!(l1, tenant_login_name(&long));
        // A valid slug that is a reserved login gets the hashed form.
        assert!(tenant_login_name(&db("root")).as_str().starts_with("root-"));
        // 64 bits of hash, still within MySQL's 32 characters.
        let (_, hash) = l1.as_str().rsplit_once('-').unwrap();
        assert_eq!(hash.len(), 16, "{l1}");
        assert_eq!(l1.as_str().len(), 32, "{l1}");
    }

    #[test]
    fn concurrent_create_races_count_as_exists() {
        assert!(pg_fault(Some("42P04"), None) == Fault::Exists);
        assert!(pg_fault(Some("23505"), Some("pg_database_datname_index")) == Fault::Exists);
        assert!(pg_fault(Some("23505"), Some("pg_authid_rolname_index")) == Fault::Exists);
        assert!(pg_fault(Some("23505"), Some("users_email_key")) == Fault::Other);
        assert!(pg_fault(Some("55006"), None) == Fault::InUse);
    }

    /// Only a login this call may have made is dropped after a failure.
    #[test]
    fn an_unclear_create_login_failure_drops_only_a_new_login() {
        assert!(undo_login_after_failure(Fault::Other, Some(false)));
        assert!(!undo_login_after_failure(Fault::Other, Some(true)));
        assert!(!undo_login_after_failure(Fault::Other, None));
        assert!(!undo_login_after_failure(Fault::Exists, Some(false)));
        assert!(!undo_login_after_failure(Fault::Denied, Some(false)));
    }

    #[test]
    fn mariadb_grant_needs_delete_history() {
        let mut rows: Vec<(String, String)> = MYSQL_DB_PRIVILEGES
            .iter()
            .map(|p| ((*p).to_owned(), "YES".to_owned()))
            .collect();
        assert!(mysql_can_grant(&rows, false));
        assert!(!mysql_can_grant(&rows, true));
        rows.push(("DELETE HISTORY".into(), "YES".into()));
        assert!(mysql_can_grant(&rows, true));
        rows[0].1 = "NO".into();
        assert!(!mysql_can_grant(&rows, false));
    }

    #[test]
    fn postgres_before_15_cannot_provision() {
        let mut r = PrivilegeReport {
            backend: "postgres",
            server_version: Some("14.9".into()),
            pg_major: pg_major("14.9 (Debian 14.9-1)"),
            can_login: true,
            login_error: None,
            can_create_database: true,
            can_create_user: true,
            can_grant: true,
        };
        assert_eq!(r.pg_major, Some(14));
        assert!(!r.can_provision());
        assert!(r.blocker().is_some_and(|b| b.contains("15")));
        r.pg_major = pg_major("16.15");
        assert!(r.can_provision(), "{:?}", r.blocker());
    }

    /// The rollback drop ends sessions inside the new database.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn rollback_drops_a_database_with_a_session_open() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let admin = ServerAdmin::connect(&url, Duration::from_secs(10)).await.unwrap();
        let Target::Server { pool, .. } = &admin.target else {
            unreachable!()
        };
        let name = db(&format!("rt{}-force", std::process::id()));
        let login = Login::new(tenant_login_name(&name));
        admin.drop_tenant_database(&name, &login).await.unwrap();
        let created = admin
            .create_tenant_database(&name, &login, &LoginPassword::generate())
            .await
            .unwrap();
        let session = Pool::connect(created.url()).await.unwrap();
        crate::sql::raw_execute_pool(&session, "SELECT 1", Vec::new())
            .await
            .unwrap();
        undo_create(pool, &name, Some(&login)).await.expect("rollback drop");
        session.close().await;
        let gone = Pool::connect_admin(&url, Some(name.as_str()), Duration::from_secs(10)).await;
        assert!(gone.is_err(), "the database survived");
    }

    #[test]
    fn tenant_url_swaps_login_and_database_and_keeps_tls() {
        let login = Login::new(LoginName::new("acme-shop").unwrap());
        let pw = LoginPassword::new("p@ss/w'rd").unwrap();
        let url = tenant_url(
            "postgres://admin:se/cret@db.internal:5432/postgres?sslmode=require&user=x",
            &login,
            &pw,
            &db("acme-shop"),
        )
        .unwrap();
        assert_eq!(
            url,
            "postgres://acme-shop:p%40ss%2Fw%27rd@db.internal:5432/acme-shop?sslmode=require"
        );
        let url = tenant_url("mysql://root@h:3306", &login, &pw, &db("a")).unwrap();
        assert_eq!(url, "mysql://acme-shop:p%40ss%2Fw%27rd@h:3306/a");
    }

    #[test]
    fn created_debug_hides_the_password() {
        let c = Created {
            database: db("a"),
            login: None,
            url: "postgres://a:topsecret@h/a".into(),
        };
        assert!(!format!("{c:?}").contains("topsecret"));
    }
}
