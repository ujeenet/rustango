//! Saved database servers: an operator stores a server's admin login
//! once, tests it, and provisions tenant databases on it later.
//!
//! The admin password is stored only sealed (`enc:v1:`), never in plain
//! text, even without `require_sealed_secrets()`: the table is new, so no
//! legacy rows need the fallback, and this login can drop every database.
//! The admin URL is built in memory when a connection is made.

use std::time::Duration;

use crate::core::Column as _;
use crate::sql::connect_diagnosis::ConnectDiagnosis;
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
    #[rustango(secret)]
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

/// A tenant database rustango created on a saved server, so a purge
/// drops only what rustango made. Written by provisioning only.
#[derive(crate::Model, Debug, Clone)]
#[rustango(
    table = "rustango_tenant_databases",
    scope = "registry",
    display = "database_name",
    unique_together = "server_name, database_name"
)]
pub struct TenantDatabase {
    #[rustango(primary_key)]
    pub id: crate::sql::Auto<i64>,
    #[rustango(max_length = 63)]
    pub org_slug: String,
    /// Set once the tenant row exists.
    pub org_id: Option<i64>,
    #[rustango(max_length = 64)]
    pub server_name: String,
    #[rustango(max_length = 63)]
    pub database_name: String,
    /// `None` on SQLite, which has no logins.
    #[rustango(max_length = 32)]
    pub login_name: Option<String>,
    /// Written before the `CREATE`, set once it succeeded. A row still
    /// `false` may name a database that exists but is not ours.
    #[rustango(default = "false")]
    pub created: bool,
    #[rustango(auto_now_add)]
    pub created_at: crate::sql::Auto<chrono::DateTime<chrono::Utc>>,
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
    /// Someone else changed the server after it was read.
    #[error("database server `{0}` was changed meanwhile; reload it and edit again")]
    Stale(String),
    #[error("database server `{name}` may be used by tenant(s) {}", tenants.join(", "))]
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
        write!(
            f,
            "AdminUrl({})",
            crate::sql::connect_diagnosis::redact(&self.0)
        )
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

    /// Set for a server row saved without a TLS mode.
    #[must_use]
    pub fn tls_warning(&self) -> Option<&'static str> {
        (self.backend != BackendKind::Sqlite.as_str() && self.tls_mode.is_none()).then_some(
            "no TLS mode is saved, so the driver default applies and the password may travel \
             in clear; set one (require is the default for new servers)",
        )
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
        let port =
            self.port
                .map(|p| {
                    u16::try_from(p).ok().filter(|p| *p != 0).ok_or_else(|| {
                        DbServerError::Invalid(format!("stored port {p} is not a port"))
                    })
                })
                .transpose()?;
        Ok(DbServerSpec {
            name: self.name.clone(),
            backend: self.backend_kind()?,
            host: self.host.clone(),
            port,
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
            Some(url) => {
                ServerAdmin::connect_with(url.expose(), self.maintenance_db.as_deref(), timeout)
                    .await
                    .map_err(|e| self.scrubbed_error(e))
            }
            None => Ok(ServerAdmin::sqlite_dir(
                self.sqlite_dir.clone().unwrap_or_default(),
            )),
        }
    }

    /// Log in and report what the admin login may do. A refused login is
    /// a report with `can_login: false`. Diagnoses name the server, not
    /// the login.
    ///
    /// # Errors
    /// [`DbServerError::Unseal`], or an unreachable server.
    pub async fn probe(&self, timeout: Duration) -> Result<PrivilegeReport, DbServerError> {
        match self.admin_url().await? {
            Some(url) => {
                let mut report = ServerAdmin::probe_url_with(
                    url.expose(),
                    self.maintenance_db.as_deref(),
                    timeout,
                )
                .await
                .map_err(|e| self.scrubbed_error(e))?;
                report.login_error = report.login_error.map(|d| self.scrubbed(d));
                Ok(report)
            }
            None => Ok(self.connect(timeout).await?.probe().await?),
        }
    }

    /// `d` with the login and password removed: the endpoint becomes the
    /// server, and the driver's text loses the user name.
    fn scrubbed(&self, d: ConnectDiagnosis) -> ConnectDiagnosis {
        let mut detail = d.detail;
        if let Some(user) = self.admin_user.as_deref().filter(|u| !u.is_empty()) {
            detail = detail.replace(user, "<admin user>");
        }
        ConnectDiagnosis {
            fault: d.fault,
            endpoint: format!(
                "{}:{}",
                self.host.as_deref().unwrap_or_default(),
                self.port
                    .map_or_else(|| "default port".to_owned(), |p| p.to_string())
            ),
            detail,
        }
    }

    fn scrubbed_error(&self, e: TenantDbError) -> DbServerError {
        match e {
            TenantDbError::Connect(d) => {
                DbServerError::Admin(TenantDbError::Connect(self.scrubbed(d)))
            }
            other => DbServerError::Admin(other),
        }
    }

    /// Tenants this server must not be removed under: database-mode
    /// tenants whose URL points at its host and port (SQLite: a file in
    /// its directory), those whose URL is missing, does not resolve or
    /// does not parse, since they may be on it too, and each
    /// [`TenantDatabase`] recorded on it whose tenant still exists or whose
    /// run started within the last hour.
    ///
    /// Hosts match ignoring case, IPv6 brackets and one trailing dot, after
    /// sqlx reads `?host=` / `hostaddr=`. Other spellings of one machine
    /// (`localhost` and `127.0.0.1`, a name and its IP) do not match.
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
            let verdict = match org.database_url.as_deref() {
                None => Uses::Unknown("it has no database URL"),
                Some(stored) => match secrets.resolve(stored).await {
                    Ok(url) => self.holds(backend, &url),
                    Err(_) => Uses::Unknown("its database URL does not resolve here"),
                },
            };
            match verdict {
                Uses::Yes => out.push(org.slug),
                Uses::No => {}
                Uses::Unknown(why) => out.push(format!("{} ({why})", org.slug)),
            }
        }
        // A database rustango created here counts while its tenant exists,
        // whatever the tenant's URL says now, and so does a run in progress.
        // Records of purged tenants do not block removal.
        let created: Vec<TenantDatabase> = TenantDatabase::objects()
            .where_(TenantDatabase::server_name.eq(self.name.clone()))
            .fetch(registry)
            .await?;
        let in_progress = chrono::Utc::now()
            - chrono::Duration::from_std(super::provision_store::STALE_RUN_AFTER)
                .unwrap_or_default();
        for record in created {
            let live = match record.org_id {
                Some(id) => Org::objects()
                    .where_(Org::id.eq(id))
                    .first(registry)
                    .await?
                    .is_some(),
                None => record.created_at.get().is_some_and(|t| *t > in_progress),
            };
            if live && !out.iter().any(|t| t == &record.org_slug) {
                out.push(record.org_slug);
            }
        }
        out.sort();
        Ok(out)
    }

    /// `Ok` when tenant `url` names exactly the database and login in
    /// `record` on this server; else what differs. Never echoes the URL.
    pub(crate) fn names_record(&self, url: &str, record: &TenantDatabase) -> Result<(), String> {
        let backend = self.backend_kind().map_err(|e| e.to_string())?;
        self.names_database(backend, url, &record.database_name)?;
        let (_, user) = url_database_and_user(backend, url);
        if backend != BackendKind::Sqlite && user.as_deref() != record.login_name.as_deref() {
            return Err(format!(
                "it signs in as another login than `{}`",
                record.login_name.as_deref().unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// `Ok` when `url` names database `database` on this server.
    pub(crate) fn names_database(
        &self,
        backend: BackendKind,
        url: &str,
        database: &str,
    ) -> Result<(), String> {
        match self.holds(backend, url) {
            Uses::Yes => {}
            Uses::No => return Err(format!("it points at another server than `{}`", self.name)),
            Uses::Unknown(why) => return Err(why.to_owned()),
        }
        let (named, _) = url_database_and_user(backend, url);
        if named.as_deref() != Some(database) {
            return Err(format!("it names another database than `{database}`"));
        }
        Ok(())
    }

    /// Whether `url` names `database` here; `Err` when it cannot tell.
    pub(crate) fn may_name_database(
        &self,
        backend: BackendKind,
        url: &str,
        database: &str,
    ) -> Result<bool, &'static str> {
        match self.holds(backend, url) {
            Uses::Yes => Ok(url_database_and_user(backend, url).0.as_deref() == Some(database)),
            Uses::No => Ok(false),
            Uses::Unknown(why) => Err(why),
        }
    }

    /// Whether tenant `url` lives on this server.
    fn holds(&self, backend: BackendKind, url: &str) -> Uses {
        let same_host = |host: &str, port: u16| {
            let ours = self.host.as_deref().map(normalized_host);
            let ours_port = self
                .port
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or_else(|| default_port(backend));
            if ours.as_deref() == Some(normalized_host(host).as_str()) && ours_port == port {
                Uses::Yes
            } else {
                Uses::No
            }
        };
        let unparsable = Uses::Unknown("its database URL does not parse");
        #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
        use std::str::FromStr as _;
        match backend {
            #[cfg(feature = "postgres")]
            BackendKind::Postgres => crate::sql::sqlx::postgres::PgConnectOptions::from_str(url)
                .map_or(unparsable, |o| same_host(o.get_host(), o.get_port())),
            #[cfg(feature = "mysql")]
            BackendKind::MySql => crate::sql::sqlx::mysql::MySqlConnectOptions::from_str(url)
                .map_or(unparsable, |o| same_host(o.get_host(), o.get_port())),
            #[cfg(feature = "sqlite")]
            BackendKind::Sqlite => {
                let Some(dir) = self.sqlite_dir.as_deref() else {
                    return Uses::No;
                };
                let Ok(o) = crate::sql::sqlx::sqlite::SqliteConnectOptions::from_str(url) else {
                    return unparsable;
                };
                let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
                if o.get_filename()
                    .parent()
                    .is_some_and(|p| canon(p) == canon(std::path::Path::new(dir)))
                {
                    Uses::Yes
                } else {
                    Uses::No
                }
            }
            #[allow(unreachable_patterns)]
            _ => {
                let _ = (url, same_host);
                unparsable
            }
        }
    }
}

/// Whether a tenant's database is on a server.
#[derive(Debug, PartialEq, Eq)]
enum Uses {
    Yes,
    No,
    /// Cannot tell, so assume it is.
    Unknown(&'static str),
}

/// The database and user a tenant URL names, as the backend's driver
/// reads it. A SQLite database is the file name without `.db`.
fn url_database_and_user(backend: BackendKind, url: &str) -> (Option<String>, Option<String>) {
    #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
    use std::str::FromStr as _;
    match backend {
        #[cfg(feature = "postgres")]
        BackendKind::Postgres => crate::sql::sqlx::postgres::PgConnectOptions::from_str(url)
            .map_or((None, None), |o| {
                (
                    o.get_database().map(str::to_owned),
                    Some(o.get_username().to_owned()),
                )
            }),
        #[cfg(feature = "mysql")]
        BackendKind::MySql => {
            crate::sql::sqlx::mysql::MySqlConnectOptions::from_str(url).map_or((None, None), |o| {
                (
                    o.get_database().map(str::to_owned),
                    Some(o.get_username().to_owned()),
                )
            })
        }
        #[cfg(feature = "sqlite")]
        BackendKind::Sqlite => {
            let db = crate::sql::sqlx::sqlite::SqliteConnectOptions::from_str(url)
                .ok()
                .and_then(|o| {
                    o.get_filename()
                        .file_name()
                        .and_then(|f| f.to_str())
                        .and_then(|f| f.strip_suffix(".db"))
                        .map(str::to_owned)
                });
            (db, None)
        }
        #[allow(unreachable_patterns)]
        _ => {
            let _ = url;
            (None, None)
        }
    }
}

impl TenantDatabase {
    /// The record of the database rustango made for `org`: by tenant id,
    /// else by slug while the record was never linked.
    ///
    /// # Errors
    /// A registry read failure.
    pub async fn of_org(registry: &Pool, org: &Org) -> Result<Option<Self>, crate::sql::ExecError> {
        if let Some(id) = org.id.get().copied() {
            let linked = Self::objects()
                .where_(Self::org_id.eq(Some(id)))
                .first(registry)
                .await?;
            if linked.is_some() {
                return Ok(linked);
            }
        }
        Self::objects()
            .where_(Self::org_slug.eq(org.slug.clone()))
            .where_(Self::org_id.is_null())
            .first(registry)
            .await
    }
}

/// `[::1]` → `::1`, `DB.Example.` → `db.example`.
fn normalized_host(h: &str) -> String {
    let h = h.trim();
    let h = h
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(h);
    h.strip_suffix('.').unwrap_or(h).to_ascii_lowercase()
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
    let mut spec = validate(spec, registry.dialect().name())?;
    // TLS unless the operator turned it off: the login can drop every database.
    if spec.backend != BackendKind::Sqlite && spec.tls.is_none() {
        spec.tls = Some(TlsMode::Require);
    }
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
/// `seen` is the `updated_at` the caller's form was built from; the write
/// also happens only while the row is as read here, so a concurrent edit
/// is never overwritten.
///
/// # Errors
/// As [`add`], plus [`DbServerError::NotFound`] and [`DbServerError::Stale`].
pub async fn update(
    registry: &Pool,
    name: &str,
    spec: &DbServerSpec,
    password: PasswordChange,
    seen: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(DbServer, Vec<&'static str>), DbServerError> {
    let mut row = get(registry, name).await?;
    let read_at = row.updated_at.get().copied();
    if seen.is_some() && seen != read_at {
        return Err(DbServerError::Stale(row.name));
    }
    let spec = validate(spec, registry.dialect().name())?;
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
        (
            "maintenance_db",
            before.maintenance_db != spec.maintenance_db,
        ),
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
    write_if_unchanged(registry, &mut row, read_at).await?;
    Ok((row, changed))
}

/// `UPDATE … WHERE id = ? AND updated_at = <read_at>`; no row means
/// another write landed in between.
async fn write_if_unchanged(
    registry: &Pool,
    row: &mut DbServer,
    read_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), DbServerError> {
    use crate::core::{Assignment, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr};
    use chrono::SubsecRound as _;
    let now = chrono::Utc::now().trunc_subsecs(6);
    let set = |column: &'static str, value: SqlValue| Assignment {
        column,
        value: value.into(),
    };
    let id = row.id.get().copied().unwrap_or_default();
    let mut filters = vec![Filter {
        column: "id",
        op: Op::Eq,
        value: SqlValue::I64(id),
    }];
    filters.push(match read_at {
        Some(t) => Filter {
            column: "updated_at",
            op: Op::Eq,
            value: SqlValue::DateTime(t),
        },
        None => Filter {
            column: "updated_at",
            op: Op::IsNull,
            value: SqlValue::Bool(true),
        },
    });
    let query = UpdateQuery {
        model: DbServer::SCHEMA,
        set: vec![
            set("host", row.host.clone().into()),
            set("port", row.port.into()),
            set("admin_user", row.admin_user.clone().into()),
            set("admin_password", row.admin_password.clone().into()),
            set("tls_mode", row.tls_mode.clone().into()),
            set("maintenance_db", row.maintenance_db.clone().into()),
            set("sqlite_dir", row.sqlite_dir.clone().into()),
            set("active", SqlValue::Bool(row.active)),
            set("updated_at", SqlValue::DateTime(now)),
        ],
        where_clause: WhereExpr::and_predicates(filters),
    };
    if crate::sql::update_pool(registry, &query).await? == 0 {
        return Err(DbServerError::Stale(row.name.clone()));
    }
    row.updated_at = crate::sql::Auto::Set(now);
    Ok(())
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

/// What `reseal-tenant-secrets` did with one saved password.
pub(crate) enum Resealed {
    Would,
    Done,
    /// Changed since it was read; left as is.
    Skipped,
    Failed(String),
}

/// Re-seal `row`'s password with the current key (opening it with the
/// current or previous one), only over the value read.
pub(crate) async fn reseal_password(registry: &Pool, row: &DbServer, apply: bool) -> Resealed {
    use crate::core::{Assignment, Filter, Model as _, Op, SqlValue, UpdateQuery, WhereExpr};
    use chrono::SubsecRound as _;
    let Some(old) = row.admin_password.as_deref() else {
        return Resealed::Skipped;
    };
    let plain = match super::SealedSecretsResolver.resolve(old).await {
        Ok(p) => p,
        Err(e) => return Resealed::Failed(format!("cannot open: {e}")),
    };
    if !apply {
        return Resealed::Would;
    }
    let new = match LoginPassword::new(plain)
        .map_err(|e| DbServerError::Seal(e.to_string()))
        .and_then(|pw| sealed(&pw))
    {
        Ok(s) => s,
        Err(e) => return Resealed::Failed(e.to_string()),
    };
    let query = UpdateQuery {
        model: DbServer::SCHEMA,
        set: vec![
            Assignment {
                column: "admin_password",
                value: SqlValue::String(new).into(),
            },
            Assignment {
                column: "updated_at",
                value: SqlValue::DateTime(chrono::Utc::now().trunc_subsecs(6)).into(),
            },
        ],
        where_clause: WhereExpr::and_predicates(vec![
            Filter {
                column: "id",
                op: Op::Eq,
                value: SqlValue::I64(row.id.get().copied().unwrap_or_default()),
            },
            Filter {
                column: "admin_password",
                op: Op::Eq,
                value: SqlValue::String(old.to_owned()),
            },
        ]),
    };
    match crate::sql::update_pool(registry, &query).await {
        Ok(0) => Resealed::Skipped,
        Ok(_) => Resealed::Done,
        Err(e) => Resealed::Failed(e.to_string()),
    }
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
    let current = crate::aead::env_key();
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
fn validate(
    spec: &DbServerSpec,
    registry_backend: &'static str,
) -> Result<DbServerSpec, DbServerError> {
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
        let dir_ok = sqlite_dir.as_deref().is_some_and(|d| {
            let p = std::path::Path::new(d);
            !d.contains('\0')
                && p.is_absolute()
                && !p.components().any(|c| c == std::path::Component::ParentDir)
        });
        if !dir_ok {
            return bad("a SQLite server needs an absolute directory without `..`");
        }
    } else {
        if sqlite_dir.is_some() {
            return bad("only a SQLite server has a directory");
        }
        let host_ok = host.as_deref().is_some_and(|h| {
            h.len() <= 255
                && !h
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "/?#@[]%".contains(c))
        });
        if !host_ok {
            return bad(
                "a host is required: a name or an IP address, without scheme, port or path",
            );
        }
        // One colon is `host:port`; an IPv6 address has two or more.
        if host.as_deref().is_some_and(|h| h.matches(':').count() == 1) {
            return bad("put the port in the port field, not after the host");
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
            assert!(
                !shown.contains("p%40ss") && !shown.contains("p@ss"),
                "{shown}"
            );
        }
        assert!(!format!("{s:?}").contains("enc:v1"), "{s:?}");
    }

    /// A reseal writes only over the value it read: a newer password wins.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_reseal_never_overwrites_a_newer_password() {
        use crate::sql::UpdaterPool as _;
        crate::aead::set_test_key();
        let dir = tempfile::tempdir().unwrap();
        let pool = Pool::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("r.db").display()
        ))
        .await
        .unwrap();
        crate::testkit::migrate_framework(&pool).await.unwrap();
        let mut row = pg("db.internal");
        row.admin_password = Some(sealed(&LoginPassword::new("first-pass").unwrap()).unwrap());
        row.insert_pool(&pool).await.unwrap();
        let stale = row.clone();
        let id = row.id.get().copied().unwrap();
        let newer = sealed(&LoginPassword::new("second-pass").unwrap()).unwrap();
        DbServer::objects()
            .where_(DbServer::id.eq(id))
            .update()
            .set("admin_password", Some(newer.clone()))
            .execute_pool(&pool)
            .await
            .unwrap();
        assert!(matches!(
            reseal_password(&pool, &stale, true).await,
            Resealed::Skipped
        ));
        let now = DbServer::objects()
            .where_(DbServer::id.eq(id))
            .first(&pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(now.admin_password.as_deref(), Some(newer.as_str()));
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

    /// Brackets, case and a trailing dot do not hide a tenant; another
    /// spelling of the same machine is not matched.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_tenant_url_on_the_server_is_found_in_any_spelling() {
        let mut s = pg("DB.Internal");
        s.port = None;
        let on = |url: &str| s.holds(BackendKind::Postgres, url);
        assert_eq!(on("postgres://u:p@db.internal./t"), Uses::Yes);
        assert_eq!(on("postgres://u:p@db.internal:5432/t"), Uses::Yes);
        assert_eq!(on("postgres://u:p@other/t?host=db.internal"), Uses::Yes);
        assert_eq!(on("postgres://u:p@db.internal:6432/t"), Uses::No);
        assert_eq!(
            on("postgres://u:p@10.0.0.5/t"),
            Uses::No,
            "an IP is another spelling"
        );
        let mut v6 = pg("::1");
        v6.port = Some(5432);
        assert_eq!(
            v6.holds(BackendKind::Postgres, "postgres://u@[::1]/t"),
            Uses::Yes
        );
        assert_eq!(
            v6.holds(BackendKind::Postgres, "postgres://u@x/t?hostaddr=::1"),
            Uses::Yes
        );
        assert!(matches!(
            on("postgres://u:p@db.internal:notaport/t"),
            Uses::Unknown(_)
        ));
    }

    /// A record counts only while the URL names its server, database and login.
    #[cfg(feature = "postgres")]
    #[test]
    fn a_record_matches_only_its_own_database_and_login() {
        let mut s = pg("db.internal");
        s.port = None;
        let record = TenantDatabase {
            id: crate::sql::Auto::Unset,
            org_slug: "acme".into(),
            org_id: Some(1),
            server_name: "main".into(),
            database_name: "acme".into(),
            login_name: Some("acme".into()),
            created: true,
            created_at: crate::sql::Auto::Unset,
        };
        assert_eq!(
            s.names_record("postgres://acme:pw@db.internal/acme", &record),
            Ok(())
        );
        for (url, why) in [
            ("postgres://acme:pw@other/acme", "another server"),
            ("postgres://acme:pw@db.internal/acme2", "another database"),
            ("postgres://admin:pw@db.internal/acme", "another login"),
        ] {
            let err = s.names_record(url, &record).unwrap_err();
            assert!(err.contains(why), "{url}: {err}");
            assert!(!err.contains("pw@"), "{err}");
        }
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_default_port_counts() {
        let mut s = pg("db");
        s.backend = "mysql".into();
        s.port = Some(3306);
        assert_eq!(s.holds(BackendKind::MySql, "mysql://u@DB/t"), Uses::Yes);
        assert_eq!(s.holds(BackendKind::MySql, "mysql://u@db:3307/t"), Uses::No);
    }

    #[test]
    fn hosts_and_directories_are_checked() {
        let server = |host: &str| DbServerSpec {
            name: "a".into(),
            backend: BackendKind::Postgres,
            host: Some(host.into()),
            admin_user: Some("u".into()),
            ..DbServerSpec::default()
        };
        for bad in ["db%2Einternal", "db:5432", "db/x", "[::1]"] {
            assert!(validate(&server(bad), "postgres").is_err(), "{bad}");
        }
        for good in ["db.internal", "::1", "fe80::1", "10.0.0.5"] {
            assert!(validate(&server(good), "postgres").is_ok(), "{good}");
        }
        let dir = |d: &str| DbServerSpec {
            name: "a".into(),
            backend: BackendKind::Sqlite,
            sqlite_dir: Some(d.into()),
            ..DbServerSpec::default()
        };
        // `/srv` has no drive, so it is not absolute on Windows.
        let (root, up) = if cfg!(windows) {
            (r"C:\srv\tenants", r"C:\srv\..\etc")
        } else {
            ("/srv/tenants", "/srv/../etc")
        };
        for bad in ["tenants", "./tenants", up] {
            assert!(validate(&dir(bad), "sqlite").is_err(), "{bad}");
        }
        assert!(validate(&dir(root), "sqlite").is_ok());
    }

    #[test]
    fn a_stored_port_out_of_range_is_an_error() {
        let mut s = pg("db");
        s.port = Some(70_000);
        assert!(s.spec().is_err());
        s.port = Some(0);
        assert!(s.spec().is_err());
    }

    #[test]
    fn a_row_without_tls_warns() {
        let mut s = pg("db");
        assert!(s.tls_warning().is_none());
        s.tls_mode = None;
        assert!(s.tls_warning().is_some());
    }

    #[test]
    fn tls_modes_round_trip() {
        for m in TlsMode::ALL {
            assert_eq!(TlsMode::parse(m.as_str()).unwrap(), m);
        }
        assert!(TlsMode::parse("on").is_err());
    }
}
