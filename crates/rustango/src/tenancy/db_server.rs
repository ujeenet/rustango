//! Saved database servers: an operator stores a server's admin login
//! once, tests it, and provisions tenant databases on it later.
//!
//! The admin password is stored only sealed (`enc:v1:`), never in plain
//! text, even without `require_sealed_secrets()`: the table is new, so no
//! legacy rows need the fallback, and this login can drop every database.
//! The admin URL is built in memory when a connection is made.

use std::time::Duration;

use crate::core::Column as _;
use crate::sql::{FetcherPool as _, LoginPassword, Pool};

use super::server_admin::{PrivilegeReport, ServerAdmin, TenantDbError};
use super::{BackendKind, Org, SecretsResolver as _, StorageMode, TenantSecrets};

/// One saved database server (or SQLite directory) in the registry.
///
/// Write it through [`add`] and [`update`], which validate it and seal
/// the password; the admin and the console never write it directly.
#[derive(crate::Model, Clone)]
#[rustango(table = "rustango_db_servers", scope = "registry", display = "name")]
pub struct DbServer {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,
    /// `[a-z0-9-]`, unique.
    #[rustango(max_length = 64, unique)]
    pub name: String,
    /// `postgres`, `mysql` or `sqlite`; always the registry's backend.
    #[rustango(max_length = 16)]
    pub backend: String,
    #[rustango(max_length = 255)]
    pub host: Option<String>,
    pub port: Option<i32>,
    #[rustango(max_length = 128)]
    pub admin_user: Option<String>,
    /// `enc:v1:` only. Private, so no caller can store a plain one.
    admin_password: Option<String>,
    /// A [`TlsMode`] name.
    #[rustango(max_length = 16)]
    pub tls_mode: Option<String>,
    /// Postgres only: where DDL runs instead of `postgres`.
    #[rustango(max_length = 63)]
    pub maintenance_db: Option<String>,
    #[rustango(max_length = 1024)]
    pub sqlite_dir: Option<String>,
    #[rustango(default = "true")]
    pub active: bool,
    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
    #[rustango(auto_now)]
    pub updated_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
}

impl std::fmt::Debug for DbServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbServer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("admin_user", &self.admin_user)
            .field("has_password", &self.has_password())
            .field("tls_mode", &self.tls_mode)
            .field("maintenance_db", &self.maintenance_db)
            .field("sqlite_dir", &self.sqlite_dir)
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

/// TLS for the admin connection, mapped to each backend's own setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl TlsMode {
    pub const ALL: [Self; 5] = [
        Self::Disable,
        Self::Prefer,
        Self::Require,
        Self::VerifyCa,
        Self::VerifyFull,
    ];

    /// The stored name, as Postgres' `sslmode` spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// # Errors
    /// The unknown value, echoed.
    pub fn parse(s: &str) -> Result<Self, DbServerError> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| {
                DbServerError::Invalid(format!(
                    "unknown TLS mode `{s}`; use disable, prefer, require, verify-ca or verify-full"
                ))
            })
    }

    /// The URL query pair for `backend`.
    fn query(self, backend: BackendKind) -> String {
        match backend {
            BackendKind::MySql => {
                let v = match self {
                    Self::Disable => "disabled",
                    Self::Prefer => "preferred",
                    Self::Require => "required",
                    Self::VerifyCa => "verify_ca",
                    Self::VerifyFull => "verify_identity",
                };
                format!("ssl-mode={v}")
            }
            _ => format!("sslmode={}", self.as_str()),
        }
    }
}

/// What a server is, without its password. Input to [`add`] and [`update`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbServerSpec {
    pub name: String,
    pub backend: BackendKind,
    pub host: Option<String>,
    /// The backend's default when `None`.
    pub port: Option<u16>,
    pub admin_user: Option<String>,
    pub tls: Option<TlsMode>,
    pub maintenance_db: Option<String>,
    pub sqlite_dir: Option<String>,
    pub active: bool,
}

/// What [`update`] does with the saved password.
#[derive(Debug, Clone)]
pub enum PasswordChange {
    Keep,
    Set(LoginPassword),
    Clear,
}

/// Why a saved server could not be stored, removed or reached.
/// Never carries the password.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbServerError {
    #[error("{0}")]
    Invalid(String),
    #[error("no database server named `{0}`")]
    NotFound(String),
    #[error("a database server named `{0}` already exists")]
    Taken(String),
    #[error("the registry runs on {registry}, so a {server} server cannot be used")]
    BackendMismatch {
        server: &'static str,
        registry: &'static str,
    },
    /// The password could not be sealed, so it was not stored.
    #[error("a server password is stored only sealed: {0}")]
    Seal(String),
    #[error("the saved password does not open: {0}")]
    Unseal(String),
    #[error("database server `{name}` is used by tenant(s) {}", tenants.join(", "))]
    InUse { name: String, tenants: Vec<String> },
    #[error(transparent)]
    Exec(#[from] crate::sql::ExecError),
    #[error(transparent)]
    Admin(#[from] TenantDbError),
}

impl From<DbServerError> for super::TenancyError {
    fn from(e: DbServerError) -> Self {
        match e {
            DbServerError::Exec(e) => Self::Exec(e),
            DbServerError::Admin(e) => e.into(),
            other => Self::Validation(other.to_string()),
        }
    }
}

/// A server's admin connection URL. Holds the password: `Debug` and
/// `Display` mask it.
pub struct AdminUrl(String);

impl AdminUrl {
    /// The URL with its password, for the one place that connects.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AdminUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AdminUrl({})", crate::sql::connect_diagnosis::redact(&self.0))
    }
}

impl std::fmt::Display for AdminUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::sql::connect_diagnosis::redact(&self.0))
    }
}

fn default_port(backend: BackendKind) -> u16 {
    match backend {
        BackendKind::MySql => 3306,
        _ => 5432,
    }
}

impl DbServer {
    /// Whether an admin password is saved.
    #[must_use]
    pub fn has_password(&self) -> bool {
        self.admin_password.is_some()
    }

    /// # Errors
    /// [`DbServerError::Invalid`] for a row holding an unknown backend.
    pub fn backend_kind(&self) -> Result<BackendKind, DbServerError> {
        BackendKind::parse(&self.backend)
            .map_err(|b| DbServerError::Invalid(format!("unknown backend `{b}`")))
    }

    /// The row as a spec, for an edit that changes some fields.
    ///
    /// # Errors
    /// [`DbServerError::Invalid`] for an unknown backend or TLS mode.
    pub fn spec(&self) -> Result<DbServerSpec, DbServerError> {
        Ok(DbServerSpec {
            name: self.name.clone(),
            backend: self.backend_kind()?,
            host: self.host.clone(),
            port: self.port.and_then(|p| u16::try_from(p).ok()),
            admin_user: self.admin_user.clone(),
            tls: self.tls_mode.as_deref().map(TlsMode::parse).transpose()?,
            maintenance_db: self.maintenance_db.clone(),
            sqlite_dir: self.sqlite_dir.clone(),
            active: self.active,
        })
    }

    /// The admin URL, password opened. Never stored. `None` for SQLite.
    ///
    /// # Errors
    /// [`DbServerError::Unseal`] when the saved password does not open.
    pub async fn admin_url(&self) -> Result<Option<AdminUrl>, DbServerError> {
        let spec = self.spec()?;
        if spec.backend == BackendKind::Sqlite {
            return Ok(None);
        }
        let password = match &self.admin_password {
            Some(sealed) => Some(
                super::SealedSecretsResolver
                    .resolve(sealed)
                    .await
                    .map_err(|e| DbServerError::Unseal(e.to_string()))?,
            ),
            None => None,
        };
        let enc = crate::url_codec::url_encode;
        let host = spec.host.as_deref().unwrap_or_default();
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let mut url = format!(
            "{}://{}",
            spec.backend.as_str(),
            enc(spec.admin_user.as_deref().unwrap_or_default())
        );
        if let Some(pw) = password {
            url.push(':');
            url.push_str(&enc(&pw));
        }
        url.push_str(&format!(
            "@{host}:{}",
            spec.port.unwrap_or_else(|| default_port(spec.backend))
        ));
        if let Some(db) = &spec.maintenance_db {
            url.push('/');
            url.push_str(&enc(db));
        }
        if let Some(tls) = spec.tls {
            url.push('?');
            url.push_str(&tls.query(spec.backend));
        }
        Ok(Some(AdminUrl(url)))
    }

    /// An admin connection to this server, or its SQLite directory.
    ///
    /// # Errors
    /// [`DbServerError::Unseal`], or the connect failure.
    pub async fn connect(&self, timeout: Duration) -> Result<ServerAdmin, DbServerError> {
        match self.admin_url().await? {
            Some(url) => Ok(ServerAdmin::connect_with(
                url.expose(),
                self.maintenance_db.as_deref(),
                timeout,
            )
            .await?),
            None => Ok(ServerAdmin::sqlite_dir(
                self.sqlite_dir.clone().unwrap_or_default(),
            )),
        }
    }

    /// Log in and report what the admin login may do. A refused login is
    /// a report with `can_login: false`.
    ///
    /// # Errors
    /// [`DbServerError::Unseal`], or an unreachable server.
    pub async fn probe(&self, timeout: Duration) -> Result<PrivilegeReport, DbServerError> {
        match self.admin_url().await? {
            Some(url) => Ok(ServerAdmin::probe_url_with(
                url.expose(),
                self.maintenance_db.as_deref(),
                timeout,
            )
            .await?),
            None => Ok(self.connect(timeout).await?.probe().await?),
        }
    }

    /// Tenants this server must not be removed under: database-mode
    /// tenants whose URL points at its host and port (SQLite: a file in
    /// its directory). URLs that do not resolve here are not counted.
    ///
    /// # Errors
    /// A registry read failure.
    pub async fn used_by(
        &self,
        registry: &Pool,
        secrets: &TenantSecrets,
    ) -> Result<Vec<String>, DbServerError> {
        let backend = self.backend_kind()?;
        let orgs: Vec<Org> = Org::objects()
            .where_(Org::storage_mode.eq(StorageMode::Database.as_str().to_owned()))
            .where_(Org::backend_kind.eq(backend.as_str().to_owned()))
            .fetch(registry)
            .await?;
        let mut out = Vec::new();
        for org in orgs {
            let Some(stored) = org.database_url.as_deref() else {
                continue;
            };
            let Ok(url) = secrets.resolve(stored).await else {
                continue;
            };
            if self.holds(backend, &url) {
                out.push(org.slug);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Whether tenant `url` lives on this server.
    fn holds(&self, backend: BackendKind, url: &str) -> bool {
        let same_host = |host: &str, port: u16| {
            self.host
                .as_deref()
                .is_some_and(|h| h.eq_ignore_ascii_case(host))
                && self
                    .port
                    .and_then(|p| u16::try_from(p).ok())
                    .unwrap_or_else(|| default_port(backend))
                    == port
        };
        #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
        use std::str::FromStr as _;
        match backend {
            #[cfg(feature = "postgres")]
            BackendKind::Postgres => crate::sql::sqlx::postgres::PgConnectOptions::from_str(url)
                .is_ok_and(|o| same_host(o.get_host(), o.get_port())),
            #[cfg(feature = "mysql")]
            BackendKind::MySql => crate::sql::sqlx::mysql::MySqlConnectOptions::from_str(url)
                .is_ok_and(|o| same_host(o.get_host(), o.get_port())),
            #[cfg(feature = "sqlite")]
            BackendKind::Sqlite => {
                let Some(dir) = self.sqlite_dir.as_deref() else {
                    return false;
                };
                let Ok(o) = crate::sql::sqlx::sqlite::SqliteConnectOptions::from_str(url) else {
                    return false;
                };
                let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
                o.get_filename()
                    .parent()
                    .is_some_and(|p| canon(p) == canon(std::path::Path::new(dir)))
            }
            #[allow(unreachable_patterns)]
            _ => {
                let _ = (url, same_host);
                false
            }
        }
    }
}

/// Every saved server, by name.
///
/// # Errors
/// A registry read failure.
pub async fn list(registry: &Pool) -> Result<Vec<DbServer>, DbServerError> {
    let mut rows: Vec<DbServer> = DbServer::objects().fetch(registry).await?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// The server named `name`.
///
/// # Errors
/// [`DbServerError::NotFound`], or a registry read failure.
pub async fn get(registry: &Pool, name: &str) -> Result<DbServer, DbServerError> {
    DbServer::objects()
        .where_(DbServer::name.eq(name.trim().to_owned()))
        .first(registry)
        .await?
        .ok_or_else(|| DbServerError::NotFound(name.to_owned()))
}

/// Save a new server. `password` is sealed before it is stored.
///
/// # Errors
/// [`DbServerError::Invalid`] / [`DbServerError::BackendMismatch`] for a
/// bad spec, [`DbServerError::Taken`], or [`DbServerError::Seal`] when
/// no `RUSTANGO_SECRET_KEY` is set.
pub async fn add(
    registry: &Pool,
    spec: &DbServerSpec,
    password: Option<&LoginPassword>,
) -> Result<DbServer, DbServerError> {
    let spec = validate(spec, registry)?;
    check_password(&spec, password.is_some())?;
    let mut row = DbServer {
        id: crate::sql::Auto::Unset,
        name: spec.name.clone(),
        backend: spec.backend.as_str().to_owned(),
        host: None,
        port: None,
        admin_user: None,
        admin_password: password.map(sealed).transpose()?,
        tls_mode: None,
        maintenance_db: None,
        sqlite_dir: None,
        active: spec.active,
        created_at: crate::sql::Auto::Unset,
        updated_at: crate::sql::Auto::Unset,
    };
    row.apply(&spec);
    if DbServer::objects()
        .where_(DbServer::name.eq(spec.name.clone()))
        .first(registry)
        .await?
        .is_some()
    {
        return Err(DbServerError::Taken(spec.name));
    }
    row.insert_pool(registry).await.map_err(|e| {
        if e.is_unique_violation() {
            DbServerError::Taken(spec.name.clone())
        } else {
            e.into()
        }
    })?;
    Ok(row)
}

/// Change the server named `name`. A server keeps its name and backend.
/// Returns the row and the names of the fields that changed.
///
/// # Errors
/// As [`add`], plus [`DbServerError::NotFound`].
pub async fn update(
    registry: &Pool,
    name: &str,
    spec: &DbServerSpec,
    password: PasswordChange,
) -> Result<(DbServer, Vec<&'static str>), DbServerError> {
    let mut row = get(registry, name).await?;
    let spec = validate(spec, registry)?;
    if spec.name != row.name {
        return Err(DbServerError::Invalid(
            "a database server cannot be renamed; add a new one instead".into(),
        ));
    }
    if spec.backend.as_str() != row.backend {
        return Err(DbServerError::Invalid(
            "a database server cannot change backend".into(),
        ));
    }
    let before = row.spec()?;
    let had_password = row.has_password();
    match &password {
        PasswordChange::Keep => {}
        PasswordChange::Set(pw) => row.admin_password = Some(sealed(pw)?),
        PasswordChange::Clear => row.admin_password = None,
    }
    check_password(&spec, row.has_password())?;
    row.apply(&spec);
    let mut changed = Vec::new();
    for (field, differs) in [
        ("host", before.host != spec.host),
        ("port", before.port != spec.port),
        ("admin_user", before.admin_user != spec.admin_user),
        ("tls_mode", before.tls != spec.tls),
        ("maintenance_db", before.maintenance_db != spec.maintenance_db),
        ("sqlite_dir", before.sqlite_dir != spec.sqlite_dir),
        ("active", before.active != spec.active),
        (
            "admin_password",
            matches!(password, PasswordChange::Set(_))
                || (had_password && matches!(password, PasswordChange::Clear)),
        ),
    ] {
        if differs {
            changed.push(field);
        }
    }
    row.save_pool(registry).await?;
    Ok((row, changed))
}

/// Delete the server named `name`, unless a tenant uses it.
///
/// # Errors
/// [`DbServerError::NotFound`], [`DbServerError::InUse`], or a registry failure.
pub async fn remove(
    registry: &Pool,
    name: &str,
    secrets: &TenantSecrets,
) -> Result<(), DbServerError> {
    let row = get(registry, name).await?;
    let tenants = row.used_by(registry, secrets).await?;
    if !tenants.is_empty() {
        return Err(DbServerError::InUse {
            name: row.name,
            tenants,
        });
    }
    row.delete_pool(registry).await?;
    Ok(())
}

impl DbServer {
    /// Copy the validated, non-secret fields.
    fn apply(&mut self, spec: &DbServerSpec) {
        self.host.clone_from(&spec.host);
        self.port = spec.port.map(i32::from);
        self.admin_user.clone_from(&spec.admin_user);
        self.tls_mode = spec.tls.map(|t| t.as_str().to_owned());
        self.maintenance_db.clone_from(&spec.maintenance_db);
        self.sqlite_dir.clone_from(&spec.sqlite_dir);
        self.active = spec.active;
    }
}

/// Seal `pw`, and check it opens back to the same text.
fn sealed(pw: &LoginPassword) -> Result<String, DbServerError> {
    let sealed = super::seal(pw.expose()).map_err(|e| DbServerError::Seal(e.to_string()))?;
    let current = std::env::var(crate::aead::KEY_VAR).ok();
    match super::secrets::open_sealed(&sealed, current.as_deref(), None) {
        Ok(back) if back == pw.expose() => Ok(sealed),
        _ => Err(DbServerError::Seal("the sealed value does not open".into())),
    }
}

fn check_password(spec: &DbServerSpec, has: bool) -> Result<(), DbServerError> {
    if has && spec.backend == BackendKind::Sqlite {
        return Err(DbServerError::Invalid(
            "a SQLite directory has no login, so no password".into(),
        ));
    }
    Ok(())
}

fn trimmed(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The spec with blanks dropped and every field checked.
fn validate(spec: &DbServerSpec, registry: &Pool) -> Result<DbServerSpec, DbServerError> {
    let bad = |m: &str| Err(DbServerError::Invalid(m.to_owned()));
    let name = spec.name.trim().to_owned();
    let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    if name.is_empty()
        || name.len() > 64
        || !name.bytes().all(legal)
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return bad("a server name is 1 to 64 lowercase letters, digits and inner hyphens");
    }
    let registry_backend = registry.dialect().name();
    if spec.backend.as_str() != registry_backend {
        return Err(DbServerError::BackendMismatch {
            server: spec.backend.as_str(),
            registry: registry_backend,
        });
    }
    let host = trimmed(spec.host.as_ref());
    let admin_user = trimmed(spec.admin_user.as_ref());
    let maintenance_db = trimmed(spec.maintenance_db.as_ref());
    let sqlite_dir = trimmed(spec.sqlite_dir.as_ref());
    if spec.backend == BackendKind::Sqlite {
        if host.is_some() || spec.port.is_some() || admin_user.is_some() || spec.tls.is_some() {
            return bad("a SQLite server has only a directory: no host, port, user or TLS");
        }
        if maintenance_db.is_some() {
            return bad("a maintenance database is for Postgres only");
        }
        match &sqlite_dir {
            Some(d) if !d.contains('\0') => {}
            _ => return bad("a SQLite server needs a directory"),
        }
    } else {
        if sqlite_dir.is_some() {
            return bad("only a SQLite server has a directory");
        }
        let host_ok = host.as_deref().is_some_and(|h| {
            h.len() <= 255
                && !h
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "/?#@[]".contains(c))
        });
        if !host_ok {
            return bad("a host is required: a name or an IP address, without scheme, port or path");
        }
        if spec.port == Some(0) {
            return bad("port 0 is not a port");
        }
        if !admin_user
            .as_deref()
            .is_some_and(|u| u.len() <= 128 && !u.chars().any(char::is_control))
        {
            return bad("an admin user is required");
        }
        if let Some(db) = &maintenance_db {
            if spec.backend != BackendKind::Postgres {
                return bad("a maintenance database is for Postgres only");
            }
            if db.len() > 63 || db.chars().any(|c| c.is_control() || "/?#".contains(c)) {
                return bad("the maintenance database is not a valid database name");
            }
        }
    }
    Ok(DbServerSpec {
        name,
        backend: spec.backend,
        host,
        port: spec.port,
        admin_user,
        tls: spec.tls,
        maintenance_db,
        sqlite_dir,
        active: spec.active,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(host: &str) -> DbServer {
        DbServer {
            id: crate::sql::Auto::Unset,
            name: "main".into(),
            backend: "postgres".into(),
            host: Some(host.into()),
            port: Some(6543),
            admin_user: Some("ad min".into()),
            admin_password: None,
            tls_mode: Some("require".into()),
            maintenance_db: Some("defaultdb".into()),
            sqlite_dir: None,
            active: true,
            created_at: crate::sql::Auto::Unset,
            updated_at: crate::sql::Auto::Unset,
        }
    }

    #[tokio::test]
    async fn the_admin_url_opens_the_password_and_masks_it() {
        crate::aead::set_test_key();
        let mut s = pg("db.internal");
        s.admin_password = Some(sealed(&LoginPassword::new("p@ss/w0rd").unwrap()).unwrap());
        let url = s.admin_url().await.unwrap().unwrap();
        assert_eq!(
            url.expose(),
            "postgres://ad%20min:p%40ss%2Fw0rd@db.internal:6543/defaultdb?sslmode=require"
        );
        for shown in [format!("{url}"), format!("{url:?}"), format!("{s:?}")] {
            assert!(!shown.contains("p%40ss") && !shown.contains("p@ss"), "{shown}");
        }
        assert!(!format!("{s:?}").contains("enc:v1"), "{s:?}");
    }

    #[tokio::test]
    async fn mysql_spells_tls_its_own_way_and_brackets_ipv6() {
        let mut s = pg("::1");
        s.backend = "mysql".into();
        s.maintenance_db = None;
        s.tls_mode = Some("verify-full".into());
        let url = s.admin_url().await.unwrap().unwrap();
        assert_eq!(
            url.expose(),
            "mysql://ad%20min@[::1]:6543?ssl-mode=verify_identity"
        );
    }

    #[test]
    fn tls_modes_round_trip() {
        for m in TlsMode::ALL {
            assert_eq!(TlsMode::parse(m.as_str()).unwrap(), m);
        }
        assert!(TlsMode::parse("on").is_err());
    }
}
