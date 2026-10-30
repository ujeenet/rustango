//! Typed readers for environment variables.
//!
//! Each reader parses `std::env::var` into the type you ask for and
//! returns a `Result`, so a missing or bad value fails at startup
//! instead of quietly defaulting inside a handler.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::env;
//! use std::time::Duration;
//!
//! struct AppConfig {
//!     database_url: String,
//!     port: u16,
//!     debug: bool,
//!     allowed_hosts: Vec<String>,
//!     session_ttl: Duration,
//! }
//!
//! fn load_config() -> Result<AppConfig, env::EnvError> {
//!     Ok(AppConfig {
//!         database_url:  env::required::<String>("DATABASE_URL")?,
//!         port:          env::with_default("PORT", 8080)?,
//!         debug:         env::with_default("DEBUG", false)?,
//!         allowed_hosts: env::list("ALLOWED_HOSTS").unwrap_or_default(),
//!         session_ttl:   env::duration_secs("SESSION_TTL_SECS").unwrap_or(Duration::from_secs(3600)),
//!     })
//! }
//! ```

use std::env::VarError;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("environment variable `{0}` is not set")]
    Missing(String),
    #[error("environment variable `{name}` is not valid {ty}: {detail}")]
    Parse {
        name: String,
        ty: &'static str,
        detail: String,
    },
    /// A variable needed to assemble a value (see
    /// [`database_url_from_env`]) is unset. The `hint` tells the
    /// operator what to set.
    #[error("environment variable `{name}` is required: {hint}")]
    MissingRequired { name: String, hint: String },
    /// `DB_DRIVER`, or the scheme read from `DATABASE_URL`, is not a
    /// backend we support.
    #[error("unsupported DB driver `{0}` — expected one of: postgres, postgresql, mysql, mariadb, sqlite")]
    UnsupportedDriver(String),
}

fn lookup(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => Some(v),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => None,
    }
}

/// Read a required env var and parse it into `T`.
///
/// # Errors
/// [`EnvError::Missing`] if the variable is unset.
/// [`EnvError::Parse`] if the value can't be parsed into `T`.
pub fn required<T>(name: &str) -> Result<T, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let raw = lookup(name).ok_or_else(|| EnvError::Missing(name.to_owned()))?;
    raw.parse::<T>().map_err(|e| EnvError::Parse {
        name: name.to_owned(),
        ty: std::any::type_name::<T>(),
        detail: e.to_string(),
    })
}

/// Read an env var, returning `default` when unset.
///
/// # Errors
/// [`EnvError::Parse`] if the variable is set but does not parse into
/// `T`. A typo is reported rather than replaced by the default,
/// because a wrong value is more dangerous than a missing one.
pub fn with_default<T>(name: &str, default: T) -> Result<T, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(name) else {
        return Ok(default);
    };
    raw.parse::<T>().map_err(|e| EnvError::Parse {
        name: name.to_owned(),
        ty: std::any::type_name::<T>(),
        detail: e.to_string(),
    })
}

/// Read an optional env var. Returns `None` if unset, `Some(parsed)` if set.
///
/// # Errors
/// [`EnvError::Parse`] if the variable IS set but can't be parsed.
pub fn optional<T>(name: &str) -> Result<Option<T>, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(name) else {
        return Ok(None);
    };
    raw.parse::<T>().map(Some).map_err(|e| EnvError::Parse {
        name: name.to_owned(),
        ty: std::any::type_name::<T>(),
        detail: e.to_string(),
    })
}

/// Read a comma-separated env var into a `Vec<T>`. Empty entries,
/// such as the one a trailing comma makes, are dropped. Gives `None`
/// when the variable is unset.
///
/// # Errors
/// [`EnvError::Parse`] if any entry can't be parsed into `T`.
pub fn list<T>(name: &str) -> Result<Option<Vec<T>>, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(name) else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let v = part.parse::<T>().map_err(|e| EnvError::Parse {
            name: name.to_owned(),
            ty: std::any::type_name::<T>(),
            detail: format!("entry `{part}`: {e}"),
        })?;
        out.push(v);
    }
    Ok(Some(out))
}

/// Read an env var as a `Duration`, reading the value as seconds.
///
/// # Errors
/// [`EnvError::Missing`] when unset.
/// [`EnvError::Parse`] when the value isn't a non-negative integer.
pub fn duration_secs(name: &str) -> Result<Duration, EnvError> {
    let secs: u64 = required(name)?;
    Ok(Duration::from_secs(secs))
}

/// Read an env var as a `Duration`, reading the value as milliseconds.
///
/// # Errors
/// As [`duration_secs`].
pub fn duration_millis(name: &str) -> Result<Duration, EnvError> {
    let ms: u64 = required(name)?;
    Ok(Duration::from_millis(ms))
}

/// The one yes/no set rustango reads flags with, any case: `1`/`true`/
/// `yes`/`on` and `0`/`false`/`no`/`off`. `None` for anything else.
pub(crate) fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The one tier parser: trimmed, lowercase, `production` as `prod`,
/// `dev` when blank. Config files, cookies, logging and the audit all use it.
pub(crate) fn canonical_tier(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => "dev".to_owned(),
        "production" => "prod".to_owned(),
        other => other.to_owned(),
    }
}

/// `RUSTANGO_ENV` through [`canonical_tier`].
pub(crate) fn tier_from_env() -> String {
    canonical_tier(&std::env::var("RUSTANGO_ENV").unwrap_or_default())
}

/// `true` when `raw` names the production tier.
pub(crate) fn is_prod_tier(raw: &str) -> bool {
    canonical_tier(raw) == "prod"
}

/// Listener address when neither `RUSTANGO_BIND` nor `[server] bind` is set:
/// all interfaces on prod and staging, like their scaffolded tiers; loopback elsewhere.
#[cfg_attr(not(any(feature = "manage", feature = "tenancy")), allow(dead_code))]
pub(crate) fn default_bind(tier: &str) -> &'static str {
    match canonical_tier(tier).as_str() {
        "prod" | "staging" => "0.0.0.0:8080",
        _ => "127.0.0.1:8080",
    }
}

// ------------------------------------------------------------------ Startup validation

/// One declared environment-variable requirement.
#[derive(Debug, Clone)]
pub struct EnvRequirement {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Collects the env vars an app needs and reports every missing one
/// at once.
///
/// Run it at startup so a bad config gives one clear message instead
/// of a crash on the first DB call.
///
/// ## Quick start
///
/// ```ignore
/// use rustango::env::Validator;
///
/// Validator::new()
///     .require("DATABASE_URL", "Postgres connection string (postgres://...)")
///     .require("SECRET_KEY", "32+ byte secret for cookie signing")
///     .optional("REDIS_URL", "Redis connection string (defaults to in-memory cache)")
///     .check_or_panic();
/// ```
#[derive(Default)]
pub struct Validator {
    reqs: Vec<EnvRequirement>,
}

impl Validator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a required env var.
    #[must_use]
    pub fn require(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.reqs.push(EnvRequirement {
            name: name.into(),
            description: description.into(),
            required: true,
        });
        self
    }

    /// Add an optional env var. Missing ones are listed as a hint,
    /// not a failure.
    #[must_use]
    pub fn optional(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.reqs.push(EnvRequirement {
            name: name.into(),
            description: description.into(),
            required: false,
        });
        self
    }

    /// Return the required vars that are unset. Optional ones are
    /// never included.
    #[must_use]
    pub fn check(&self) -> Vec<&EnvRequirement> {
        self.reqs
            .iter()
            .filter(|r| r.required && lookup(&r.name).is_none())
            .collect()
    }

    /// Like [`Self::check`], but returns a ready-to-print message
    /// listing each missing var and an `export` line for it.
    ///
    /// # Errors
    /// The formatted message, when a required var is missing.
    pub fn check_or_error(&self) -> Result<(), String> {
        let missing = self.check();
        if missing.is_empty() {
            return Ok(());
        }
        let mut out = String::from("Missing required environment variables:\n");
        for req in &missing {
            out.push_str(&format!("  - {} — {}\n", req.name, req.description));
        }
        out.push_str("\nSet them and re-run, e.g.:\n");
        for req in &missing {
            out.push_str(&format!("  export {}=...\n", req.name));
        }
        Err(out)
    }

    /// Like [`Self::check_or_error`], but panics with the message.
    /// Meant for startup.
    ///
    /// # Panics
    /// When a required var is missing.
    pub fn check_or_panic(&self) {
        if let Err(msg) = self.check_or_error() {
            panic!("{msg}");
        }
    }

    /// Number of declared requirements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.reqs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reqs.is_empty()
    }
}

// ------------------------------------------------------------------ Database URL assembly

/// Backends recognized by [`DatabaseUrlBuilder`] and
/// [`database_url_from_env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbDriver {
    Postgres,
    Mysql,
}

impl DbDriver {
    /// The scheme as it appears in the URL: `"postgres"` or
    /// `"mysql"`. `postgresql` parses, but comes back as `postgres`.
    #[must_use]
    pub fn scheme(self) -> &'static str {
        match self {
            DbDriver::Postgres => "postgres",
            DbDriver::Mysql => "mysql",
        }
    }

    /// Default TCP port for the backend.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            DbDriver::Postgres => 5432,
            DbDriver::Mysql => 3306,
        }
    }

    fn parse(s: &str) -> Result<Self, EnvError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Ok(DbDriver::Postgres),
            "mysql" | "mariadb" => Ok(DbDriver::Mysql),
            other => Err(EnvError::UnsupportedDriver(other.to_owned())),
        }
    }
}

/// Builds a database URL from separate fields.
///
/// [`database_url_from_env`] uses it when `DATABASE_URL` is unset and
/// the details come from `DB_HOST`, `DB_USER` and friends. The user
/// and password are percent-encoded, so `@`, `:`, `/`, `#`, `?`, `%`
/// and other URL-special characters in a password cannot break the
/// URL apart.
///
/// ```ignore
/// use rustango::env::{DatabaseUrlBuilder, DbDriver};
///
/// let url = DatabaseUrlBuilder::new(DbDriver::Postgres)
///     .host("db.internal")
///     .user("svc")
///     .password("p@ss/word")  // auto-encoded
///     .database("app")
///     .build();
/// assert_eq!(url, "postgres://svc:p%40ss%2Fword@db.internal:5432/app");
/// ```
#[derive(Clone)]
pub struct DatabaseUrlBuilder {
    driver: DbDriver,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
    params: Option<String>,
}

redacted_debug!(DatabaseUrlBuilder { driver, host, port, user, database, params; password });

impl DatabaseUrlBuilder {
    #[must_use]
    pub fn new(driver: DbDriver) -> Self {
        Self {
            driver,
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            params: None,
        }
    }

    #[must_use]
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }
    #[must_use]
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }
    #[must_use]
    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = Some(database.into());
        self
    }
    /// Raw query string put after `?`, such as
    /// `"sslmode=require&application_name=app"`. You must
    /// percent-encode it yourself; this builder does not.
    #[must_use]
    pub fn params(mut self, params: impl Into<String>) -> Self {
        self.params = Some(params.into());
        self
    }

    /// Build `scheme://[user[:pw]@]host:port[/db][?params]`. Host
    /// falls back to `localhost` and port to the driver's default; an
    /// IPv6 host such as `::1` is bracketed.
    #[must_use]
    pub fn build(&self) -> String {
        let scheme = self.driver.scheme();
        let host = self.host.as_deref().unwrap_or("localhost");
        let port = self.port.unwrap_or(self.driver.default_port());
        let mut out = format!("{scheme}://");
        if let Some(user) = &self.user {
            out.push_str(&percent_encode_userinfo(user));
            if let Some(pw) = &self.password {
                out.push(':');
                out.push_str(&percent_encode_userinfo(pw));
            }
            out.push('@');
        }
        // An IPv6 literal needs brackets; a zone id's `%` is `%25` in a URL.
        if host.contains(':') && !host.starts_with('[') {
            out.push('[');
            out.push_str(&host.replace('%', "%25"));
            out.push(']');
        } else {
            out.push_str(host);
        }
        out.push(':');
        out.push_str(&port.to_string());
        if let Some(db) = &self.database {
            out.push('/');
            out.push_str(&percent_encode_path(db));
        }
        if let Some(params) = &self.params {
            if !params.is_empty() {
                out.push('?');
                out.push_str(params);
            }
        }
        out
    }
}

/// The separate connection vars [`database_url_from_env`] assembles from.
const DB_PART_VARS: &[&str] = &[
    "DB_DRIVER",
    "DB_HOST",
    "DB_PORT",
    "DB_USER",
    "DB_PASSWORD",
    "DB_PASSWORD_FILE",
    "DB_NAME",
    "DB_PARAMS",
];

/// What to tell an operator who set no connection style.
const DB_UNSET_HINT: &str = "set DATABASE_URL, or DB_USER + DB_NAME \
     (with DB_HOST, DB_PORT, DB_PASSWORD or DB_PASSWORD_FILE, DB_DRIVER); \
     for SQLite, DB_DRIVER=sqlite + DB_NAME=<file path>; \
     or RUSTANGO__DATABASE__URL, or [database] url in the settings file";

/// The settings-file `[database] url` override var.
const DATABASE_URL_OVERRIDE: &str = "RUSTANGO__DATABASE__URL";

/// Where a setting's value came from, highest precedence first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingSource {
    /// A bare environment variable, such as `DATABASE_URL`.
    Env(&'static str),
    /// A `RUSTANGO__SECTION__KEY` variable.
    Override(String),
    /// A settings file.
    File(String),
    /// Set nowhere.
    Default,
}

impl std::fmt::Display for SettingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(var) => write!(f, "env {var}"),
            Self::Override(var) => write!(f, "env {var}"),
            Self::File(file) => f.write_str(file),
            Self::Default => f.write_str("default"),
        }
    }
}

/// `[database] url` as `Cli::with_settings` loaded it; first call wins.
static SETTINGS_DATABASE_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Hand the settings' `[database] url` to the resolver, below every env var.
pub(crate) fn set_settings_database_url(url: &str) {
    if !url.trim().is_empty() {
        let _ = SETTINGS_DATABASE_URL.set(url.to_owned());
    }
}

/// Resolve a database URL from environment variables.
///
/// Resolution order:
/// 1. `DATABASE_URL` if set. It is returned as given, except that
///    `postgresql://` becomes `postgres://`.
/// 2. Otherwise assembled from:
///    - `DB_DRIVER` (default `postgres`): one of `postgres`,
///      `postgresql`, `mysql`, `mariadb`, `sqlite`.
///    - `DB_HOST` (default `localhost`)
///    - `DB_PORT` (default `5432` for postgres, `3306` for mysql)
///    - `DB_USER` (**required** when assembling, except for SQLite)
///    - `DB_PASSWORD`, or `DB_PASSWORD_FILE` naming a file that holds
///      it (Docker / Kubernetes secrets); optional, auto percent-encoded
///    - `DB_NAME` (**required** when assembling; the file path for SQLite)
///    - `DB_PARAMS` (optional raw query string)
///
/// 3. `RUSTANGO__DATABASE__URL`.
/// 4. `[database] url` from the settings file, once `Cli::with_settings`
///    has loaded it. The environment always wins over the file.
///
/// A blank value counts as unset, so `DATABASE_URL=` falls through.
///
/// When both env styles are set, `DATABASE_URL` wins and one warning per
/// process says so. 0.62.0 flips this: the separate vars will win.
///
/// # Errors
/// - [`EnvError::MissingRequired`] when neither style is set, or when
///   assembling without `DB_USER` or `DB_NAME`.
/// - [`EnvError::UnsupportedDriver`] for an unknown `DB_DRIVER`.
/// - [`EnvError::Parse`] when `DB_PORT` is not a number, or
///   `DB_PASSWORD_FILE` can't be read or is set with `DB_PASSWORD`.
pub fn database_url_from_env() -> Result<String, EnvError> {
    let (url, _) = database_url_with_source()?;
    if let Some(msg) = database_style_conflict_with(&lookup) {
        warn_style_conflict_once(&msg);
    }
    Ok(url)
}

/// [`database_url_from_env`] plus where the URL came from.
pub(crate) fn database_url_with_source() -> Result<(String, SettingSource), EnvError> {
    resolve_database_url_from(&lookup, SETTINGS_DATABASE_URL.get().map(String::as_str))
}

/// [`database_url_from_env`] with the CLI's advice on where to set it.
#[cfg(feature = "manage")]
pub(crate) fn database_url_or_advice() -> Result<String, String> {
    database_url_from_env()
        .map_err(|e| format!("{e}. Set it in your shell, or copy `.env.example` to `.env`."))
}

/// The warning [`database_url_from_env`] logs when `DATABASE_URL` and
/// `DB_*` vars are both set; `None` when only one style is.
pub(crate) fn database_style_conflict() -> Option<String> {
    database_style_conflict_with(&lookup)
}

fn database_style_conflict_with(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let url = non_blank(get, "DATABASE_URL")?;
    let parts: Vec<&str> = DB_PART_VARS
        .iter()
        .copied()
        .filter(|n| non_blank(get, n).is_some())
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "both DATABASE_URL and {} are set; using DATABASE_URL ({}). \
         From 0.62.0 the DB_* vars win; unset one of the two styles",
        parts.join(", "),
        crate::sql::connect_diagnosis::redact(&url),
    ))
}

/// One warning per process: every pool open resolves the URL again.
fn warn_style_conflict_once(msg: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // CLI verbs often run with no subscriber; stderr then.
    if tracing::dispatcher::has_been_set() {
        tracing::warn!(target: "rustango::sql", "{msg}");
    } else {
        eprintln!("warning: {msg}");
    }
}

/// `get`, with a blank value read as unset: `.env` templates ship
/// `DATABASE_URL=` and `DB_HOST=`, which mean "not set", not "empty".
fn non_blank(get: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name).filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
fn resolve_database_url(get: &dyn Fn(&str) -> Option<String>) -> Result<String, EnvError> {
    resolve_database_url_from(get, None).map(|(url, _)| url)
}

/// The one database URL resolver: bare env vars, then
/// `RUSTANGO__DATABASE__URL`, then the settings file's `url`.
pub(crate) fn resolve_database_url_from(
    get: &dyn Fn(&str) -> Option<String>,
    settings_url: Option<&str>,
) -> Result<(String, SettingSource), EnvError> {
    let get = &|name: &str| non_blank(get, name);
    // One scheme downstream, so callers match on `postgres://`.
    let normal = |url: String| match url.strip_prefix("postgresql://") {
        Some(rest) => format!("postgres://{rest}"),
        None => url,
    };
    if let Some(url) = get("DATABASE_URL") {
        return Ok((normal(url), SettingSource::Env("DATABASE_URL")));
    }
    if let Some(var) = DB_PART_VARS.iter().copied().find(|n| get(n).is_some()) {
        return assemble_database_url(get).map(|url| (url, SettingSource::Env(var)));
    }
    if let Some(url) = get(DATABASE_URL_OVERRIDE) {
        return Ok((
            normal(url),
            SettingSource::Override(DATABASE_URL_OVERRIDE.into()),
        ));
    }
    if let Some(url) = settings_url.filter(|u| !u.trim().is_empty()) {
        return Ok((
            normal(url.to_owned()),
            SettingSource::File("[database] url".into()),
        ));
    }
    Err(EnvError::MissingRequired {
        name: "DATABASE_URL".into(),
        hint: DB_UNSET_HINT.into(),
    })
}

/// The URL from the separate `DB_*` vars; at least one is set.
fn assemble_database_url(get: &dyn Fn(&str) -> Option<String>) -> Result<String, EnvError> {
    let name = |var: &str| {
        get(var).ok_or_else(|| EnvError::MissingRequired {
            name: var.into(),
            hint: format!("set DATABASE_URL or {var} (e.g. `export {var}=app`)"),
        })
    };
    // `DbDriver` has no SQLite variant until 0.62.0: adding one breaks matches.
    let driver = match get("DB_DRIVER") {
        Some(s) if s.trim().eq_ignore_ascii_case("sqlite") => {
            return Ok(sqlite_url(&name("DB_NAME")?, get("DB_PARAMS")));
        }
        Some(s) => DbDriver::parse(&s)?,
        None => DbDriver::Postgres,
    };
    let user = name("DB_USER")?;
    let mut b = DatabaseUrlBuilder::new(driver)
        .user(user)
        .database(name("DB_NAME")?);
    if let Some(host) = get("DB_HOST") {
        b = b.host(host);
    }
    if let Some(port) = get("DB_PORT") {
        b = b.port(
            port.parse()
                .map_err(|e: std::num::ParseIntError| EnvError::Parse {
                    name: "DB_PORT".into(),
                    ty: "u16",
                    detail: e.to_string(),
                })?,
        );
    }
    if let Some(pw) = db_password(get)? {
        b = b.password(pw);
    }
    if let Some(params) = get("DB_PARAMS") {
        b = b.params(params);
    }
    Ok(b.build())
}

/// `DB_PASSWORD`, or the contents of `DB_PASSWORD_FILE` less its
/// trailing newline. Both set is an error: neither silently wins.
fn db_password(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, EnvError> {
    let file_err = |detail: String| EnvError::Parse {
        name: "DB_PASSWORD_FILE".into(),
        ty: "readable file path",
        detail,
    };
    match (get("DB_PASSWORD"), get("DB_PASSWORD_FILE")) {
        (Some(_), Some(_)) => Err(file_err(
            "set DB_PASSWORD or DB_PASSWORD_FILE, not both".into(),
        )),
        (Some(pw), None) => Ok(Some(pw)),
        (None, Some(path)) => {
            let raw =
                std::fs::read_to_string(&path).map_err(|e| file_err(format!("{path}: {e}")))?;
            Ok(Some(raw.trim_end_matches(['\n', '\r']).to_owned()))
        }
        (None, None) => Ok(None),
    }
}

/// `sqlite:<path>`; `:memory:` stays the in-memory database.
fn sqlite_url(path: &str, params: Option<String>) -> String {
    let mut url = format!("sqlite:{path}");
    if let Some(p) = params.filter(|p| !p.is_empty()) {
        url.push('?');
        url.push_str(&p);
    }
    url
}

/// RFC 3986 percent-encoding for the userinfo part (user and
/// password). Everything outside the unreserved set is encoded. This
/// over-encodes on purpose: a `@` or `:` left raw in a password would
/// change how the other side parses the URL.
fn percent_encode_userinfo(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_userinfo_unreserved(b) {
            out.push(b as char);
        } else {
            // Encoding to a String can't fail.
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Percent-encoding for the path part, used for the database name.
fn percent_encode_path(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_path_safe(b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn is_userinfo_unreserved(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
    )
}

fn is_path_safe(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global, so these tests run one at a time.
    fn env_lock() -> &'static Mutex<()> {
        static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    fn with_env<F: FnOnce()>(name: &str, value: &str, f: F) {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(name, value);
        f();
        std::env::remove_var(name);
    }

    fn without_env<F: FnOnce()>(name: &str, f: F) {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var(name);
        f();
    }

    /// With no bind configured, only the deployed tiers listen on every interface.
    #[test]
    fn fallback_bind_is_loopback_off_the_deployed_tiers() {
        for tier in ["", "dev", "test", "qa"] {
            assert_eq!(default_bind(tier), "127.0.0.1:8080", "tier {tier:?}");
        }
        for tier in ["prod", "PRODUCTION", "staging"] {
            assert_eq!(default_bind(tier), "0.0.0.0:8080", "tier {tier:?}");
        }
    }

    #[test]
    fn required_returns_parsed_value() {
        with_env("RUSTANGO_TEST_PORT", "8080", || {
            let v: u16 = required("RUSTANGO_TEST_PORT").unwrap();
            assert_eq!(v, 8080);
        });
    }

    #[test]
    fn required_errors_when_missing() {
        without_env("RUSTANGO_TEST_MISSING_PORT", || {
            let r = required::<u16>("RUSTANGO_TEST_MISSING_PORT");
            assert!(matches!(r, Err(EnvError::Missing(_))));
        });
    }

    #[test]
    fn required_errors_on_parse_failure() {
        with_env("RUSTANGO_TEST_BAD_PORT", "not-a-number", || {
            let r = required::<u16>("RUSTANGO_TEST_BAD_PORT");
            assert!(matches!(r, Err(EnvError::Parse { .. })));
        });
    }

    #[test]
    fn with_default_returns_default_when_missing() {
        without_env("RUSTANGO_TEST_DEFAULT_PORT", || {
            let v = with_default("RUSTANGO_TEST_DEFAULT_PORT", 9090u16).unwrap();
            assert_eq!(v, 9090);
        });
    }

    #[test]
    fn with_default_returns_set_value() {
        with_env("RUSTANGO_TEST_SET_PORT", "1234", || {
            let v = with_default("RUSTANGO_TEST_SET_PORT", 9090u16).unwrap();
            assert_eq!(v, 1234);
        });
    }

    #[test]
    fn with_default_errors_on_bad_value() {
        with_env("RUSTANGO_TEST_BAD_DEFAULT", "garbage", || {
            let r = with_default("RUSTANGO_TEST_BAD_DEFAULT", 1u16);
            assert!(matches!(r, Err(EnvError::Parse { .. })));
        });
    }

    #[test]
    fn optional_returns_none_when_missing() {
        without_env("RUSTANGO_TEST_OPT_MISSING", || {
            let v: Option<i32> = optional("RUSTANGO_TEST_OPT_MISSING").unwrap();
            assert_eq!(v, None);
        });
    }

    #[test]
    fn optional_returns_some_when_set() {
        with_env("RUSTANGO_TEST_OPT_SET", "42", || {
            let v: Option<i32> = optional("RUSTANGO_TEST_OPT_SET").unwrap();
            assert_eq!(v, Some(42));
        });
    }

    #[test]
    fn list_parses_comma_separated() {
        with_env(
            "RUSTANGO_TEST_HOSTS",
            "a.example.com, b.example.com,c.example.com",
            || {
                let v: Vec<String> = list("RUSTANGO_TEST_HOSTS").unwrap().unwrap();
                assert_eq!(v, vec!["a.example.com", "b.example.com", "c.example.com"]);
            },
        );
    }

    #[test]
    fn list_drops_empty_entries() {
        with_env("RUSTANGO_TEST_LIST_TRAILING", "a,b,,", || {
            let v: Vec<String> = list("RUSTANGO_TEST_LIST_TRAILING").unwrap().unwrap();
            assert_eq!(v, vec!["a", "b"]);
        });
    }

    #[test]
    fn list_returns_none_when_missing() {
        without_env("RUSTANGO_TEST_LIST_MISSING", || {
            let v: Option<Vec<String>> = list("RUSTANGO_TEST_LIST_MISSING").unwrap();
            assert_eq!(v, None);
        });
    }

    #[test]
    fn list_parses_typed_values() {
        with_env("RUSTANGO_TEST_PORTS", "8080, 8081, 8082", || {
            let v: Vec<u16> = list("RUSTANGO_TEST_PORTS").unwrap().unwrap();
            assert_eq!(v, vec![8080, 8081, 8082]);
        });
    }

    #[test]
    fn duration_secs_parses() {
        with_env("RUSTANGO_TEST_TTL", "60", || {
            let d = duration_secs("RUSTANGO_TEST_TTL").unwrap();
            assert_eq!(d, Duration::from_secs(60));
        });
    }

    // -------------------------------------------------------------- Validator tests

    #[test]
    fn validator_check_passes_when_all_required_set() {
        // Set both vars inline: nesting `with_env` would deadlock.
        let _g = env_lock().lock().unwrap();
        std::env::set_var("RUSTANGO_TEST_VALID_DB", "x");
        std::env::set_var("RUSTANGO_TEST_VALID_KEY", "y");
        let v = Validator::new()
            .require("RUSTANGO_TEST_VALID_DB", "db url")
            .require("RUSTANGO_TEST_VALID_KEY", "key");
        assert!(v.check().is_empty());
        assert!(v.check_or_error().is_ok());
        std::env::remove_var("RUSTANGO_TEST_VALID_DB");
        std::env::remove_var("RUSTANGO_TEST_VALID_KEY");
    }

    #[test]
    fn validator_check_lists_missing_required() {
        without_env("RUSTANGO_TEST_MISSING_REQ", || {
            let v = Validator::new().require("RUSTANGO_TEST_MISSING_REQ", "needed for X");
            let missing = v.check();
            assert_eq!(missing.len(), 1);
            assert_eq!(missing[0].name, "RUSTANGO_TEST_MISSING_REQ");
        });
    }

    #[test]
    fn validator_optional_not_in_missing_list() {
        without_env("RUSTANGO_TEST_OPT", || {
            let v = Validator::new().optional("RUSTANGO_TEST_OPT", "fallback OK");
            assert!(v.check().is_empty());
        });
    }

    #[test]
    fn validator_check_or_error_returns_formatted_message() {
        without_env("RUSTANGO_TEST_FORMAT_REQ", || {
            let v = Validator::new().require("RUSTANGO_TEST_FORMAT_REQ", "Postgres URL");
            let err = v.check_or_error().unwrap_err();
            assert!(err.contains("RUSTANGO_TEST_FORMAT_REQ"));
            assert!(err.contains("Postgres URL"));
            assert!(err.contains("export RUSTANGO_TEST_FORMAT_REQ=..."));
        });
    }

    #[test]
    fn validator_len_and_is_empty() {
        let v = Validator::new();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
        let v = v.require("X", "x").optional("Y", "y");
        assert_eq!(v.len(), 2);
        assert!(!v.is_empty());
    }

    // -------------------------------------------------------------- DatabaseUrlBuilder tests

    #[test]
    fn builder_assembles_minimal_postgres_url() {
        let url = DatabaseUrlBuilder::new(DbDriver::Postgres)
            .user("app")
            .database("appdb")
            .build();
        assert_eq!(url, "postgres://app@localhost:5432/appdb");
    }

    #[test]
    fn builder_assembles_full_postgres_url() {
        let url = DatabaseUrlBuilder::new(DbDriver::Postgres)
            .host("db.internal")
            .port(6543)
            .user("svc")
            .password("secret")
            .database("app")
            .params("sslmode=require")
            .build();
        assert_eq!(
            url,
            "postgres://svc:secret@db.internal:6543/app?sslmode=require"
        );
    }

    #[test]
    fn builder_percent_encodes_password_specials() {
        // `@`, `:`, `/`, `#`, `?`, `%` must all be encoded inside the password.
        let url = DatabaseUrlBuilder::new(DbDriver::Postgres)
            .user("u")
            .password("p@ss:/word#?%")
            .database("db")
            .build();
        assert_eq!(
            url,
            "postgres://u:p%40ss%3A%2Fword%23%3F%25@localhost:5432/db"
        );
    }

    #[test]
    fn builder_uses_mysql_default_port() {
        let url = DatabaseUrlBuilder::new(DbDriver::Mysql)
            .user("u")
            .database("d")
            .build();
        assert_eq!(url, "mysql://u@localhost:3306/d");
    }

    #[test]
    fn db_driver_parses_aliases() {
        assert_eq!(DbDriver::parse("postgres").unwrap(), DbDriver::Postgres);
        assert_eq!(DbDriver::parse("PostgreSQL").unwrap(), DbDriver::Postgres);
        assert_eq!(DbDriver::parse("pg").unwrap(), DbDriver::Postgres);
        assert_eq!(DbDriver::parse("mysql").unwrap(), DbDriver::Mysql);
        assert_eq!(DbDriver::parse("MariaDB").unwrap(), DbDriver::Mysql);
        assert!(matches!(
            DbDriver::parse("oracle"),
            Err(EnvError::UnsupportedDriver(_))
        ));
    }

    /// Drop guard that puts a set of env vars back as it found them.
    /// The developer's shell may already export `DATABASE_URL`.
    struct EnvSnapshot {
        saved: Vec<(&'static str, Option<String>)>,
    }
    impl EnvSnapshot {
        fn capture(names: &[&'static str]) -> Self {
            let saved = names.iter().map(|n| (*n, std::env::var(n).ok())).collect();
            for n in names {
                std::env::remove_var(n);
            }
            Self { saved }
        }
    }
    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (name, prev) in &self.saved {
                match prev {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    const DB_VARS: &[&str] = &[
        "DATABASE_URL",
        "DB_DRIVER",
        "DB_HOST",
        "DB_PORT",
        "DB_USER",
        "DB_PASSWORD",
        "DB_PASSWORD_FILE",
        "DB_NAME",
        "DB_PARAMS",
        "RUSTANGO__DATABASE__URL",
    ];

    #[test]
    fn database_url_from_env_prefers_database_url() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DATABASE_URL", "postgres://x@y:1/z");
        let url = database_url_from_env().unwrap();
        assert_eq!(url, "postgres://x@y:1/z");
    }

    #[test]
    fn database_url_normalizes_postgresql_scheme() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DATABASE_URL", "postgresql://x@y:1/z");
        let url = database_url_from_env().unwrap();
        assert_eq!(url, "postgres://x@y:1/z");
    }

    #[test]
    fn database_url_assembles_from_split_vars() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DB_HOST", "db.example.com");
        std::env::set_var("DB_PORT", "6543");
        std::env::set_var("DB_USER", "app");
        std::env::set_var("DB_PASSWORD", "p@ss");
        std::env::set_var("DB_NAME", "appdb");
        let url = database_url_from_env().unwrap();
        assert_eq!(url, "postgres://app:p%40ss@db.example.com:6543/appdb");
    }

    #[test]
    fn database_url_assembles_mysql_when_driver_set() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DB_DRIVER", "mysql");
        std::env::set_var("DB_USER", "u");
        std::env::set_var("DB_NAME", "d");
        let url = database_url_from_env().unwrap();
        assert_eq!(url, "mysql://u@localhost:3306/d");
    }

    #[test]
    fn database_url_errors_when_required_missing() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DB_NAME", "x");
        // DB_USER missing
        let err = database_url_from_env().unwrap_err();
        match err {
            EnvError::MissingRequired { name, .. } => assert_eq!(name, "DB_USER"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A lookup over fixed pairs, so these need no process env.
    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn database_url_assembles_sqlite_from_db_name() {
        let get = vars(&[("DB_DRIVER", "SQLite"), ("DB_NAME", "./var/app.db")]);
        assert_eq!(resolve_database_url(&get).unwrap(), "sqlite:./var/app.db");
        let get = vars(&[("DB_DRIVER", "sqlite"), ("DB_NAME", ":memory:")]);
        assert_eq!(resolve_database_url(&get).unwrap(), "sqlite::memory:");
        let get = vars(&[
            ("DB_DRIVER", "sqlite"),
            ("DB_NAME", "/abs/x.db"),
            ("DB_PARAMS", "mode=rwc"),
        ]);
        assert_eq!(
            resolve_database_url(&get).unwrap(),
            "sqlite:/abs/x.db?mode=rwc"
        );
        let get = vars(&[("DB_DRIVER", "sqlite")]);
        match resolve_database_url(&get).unwrap_err() {
            EnvError::MissingRequired { name, .. } => assert_eq!(name, "DB_NAME"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn database_url_reads_the_password_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        std::fs::write(&path, "p@ss word\n").unwrap();
        let path = path.to_string_lossy().into_owned();
        let get = vars(&[
            ("DB_USER", "u"),
            ("DB_NAME", "d"),
            ("DB_PASSWORD_FILE", path.as_str()),
        ]);
        assert_eq!(
            resolve_database_url(&get).unwrap(),
            "postgres://u:p%40ss%20word@localhost:5432/d"
        );
        let both = vars(&[
            ("DB_USER", "u"),
            ("DB_NAME", "d"),
            ("DB_PASSWORD", "x"),
            ("DB_PASSWORD_FILE", path.as_str()),
        ]);
        assert!(matches!(
            resolve_database_url(&both),
            Err(EnvError::Parse { ref name, .. }) if name == "DB_PASSWORD_FILE"
        ));
        let missing = vars(&[
            ("DB_USER", "u"),
            ("DB_NAME", "d"),
            ("DB_PASSWORD_FILE", "/nonexistent/rustango-pw"),
        ]);
        assert!(resolve_database_url(&missing).is_err());
    }

    /// A blank var (a `.env` template's `DATABASE_URL=`) counts as unset.
    #[test]
    fn database_url_treats_blank_vars_as_unset() {
        let get = vars(&[
            ("DATABASE_URL", " "),
            ("DB_DRIVER", ""),
            ("DB_HOST", ""),
            ("DB_PORT", ""),
            ("DB_PASSWORD", ""),
            ("DB_PARAMS", ""),
            ("DB_USER", "u"),
            ("DB_NAME", "d"),
        ]);
        assert_eq!(
            resolve_database_url(&get).unwrap(),
            "postgres://u@localhost:5432/d"
        );
        assert!(database_style_conflict_with(&get).is_none());
        match resolve_database_url(&vars(&[("DB_USER", ""), ("DB_NAME", "d")])).unwrap_err() {
            EnvError::MissingRequired { name, .. } => assert_eq!(name, "DB_USER"),
            other => panic!("wrong variant: {other:?}"),
        }
        assert!(matches!(
            resolve_database_url(&vars(&[("DATABASE_URL", ""), ("DB_HOST", "")])),
            Err(EnvError::MissingRequired { ref name, .. }) if name == "DATABASE_URL"
        ));
    }

    #[test]
    fn database_url_brackets_an_ipv6_host() {
        let get = vars(&[("DB_HOST", "::1"), ("DB_USER", "u"), ("DB_NAME", "d")]);
        assert_eq!(
            resolve_database_url(&get).unwrap(),
            "postgres://u@[::1]:5432/d"
        );
        let b = DatabaseUrlBuilder::new(DbDriver::Mysql).host("[fe80::1]");
        assert_eq!(b.build(), "mysql://[fe80::1]:3306");
        let b = DatabaseUrlBuilder::new(DbDriver::Mysql).host("fe80::1%eth0");
        assert_eq!(b.build(), "mysql://[fe80::1%25eth0]:3306");
    }

    #[test]
    fn database_url_unset_error_names_both_styles() {
        let err = resolve_database_url(&vars(&[])).unwrap_err().to_string();
        for needle in [
            "DATABASE_URL",
            "DB_USER",
            "DB_NAME",
            "DB_PASSWORD_FILE",
            "RUSTANGO__DATABASE__URL",
            "[database] url",
        ] {
            assert!(err.contains(needle), "{needle} missing from: {err}");
        }
    }

    #[test]
    fn database_url_still_wins_and_the_conflict_is_redacted() {
        let get = vars(&[
            ("DATABASE_URL", "postgres://app:s3cret@db/app"),
            ("DB_USER", "other"),
            ("DB_NAME", "otherdb"),
            ("DB_HOST", ""),
        ]);
        assert_eq!(
            resolve_database_url(&get).unwrap(),
            "postgres://app:s3cret@db/app"
        );
        let msg = database_style_conflict_with(&get).expect("conflict");
        assert!(!msg.contains("s3cret"), "{msg}");
        assert!(
            msg.contains("DB_USER, DB_NAME") && !msg.contains("DB_HOST"),
            "{msg}"
        );
        assert!(msg.contains("0.62.0"), "{msg}");
        assert!(
            database_style_conflict_with(&vars(&[("DATABASE_URL", "sqlite::memory:")])).is_none()
        );
        assert!(database_style_conflict_with(&vars(&[("DB_USER", "u")])).is_none());
    }

    /// Bare env, then `RUSTANGO__DATABASE__URL`, then the settings file.
    #[test]
    fn database_url_precedence_puts_the_environment_over_the_file() {
        let file = Some("postgres://from-file/dev");
        let resolve = |pairs: &[(&str, &str)]| resolve_database_url_from(&vars(pairs), file);
        assert_eq!(
            resolve(&[]).unwrap(),
            (
                "postgres://from-file/dev".to_owned(),
                SettingSource::File("[database] url".into())
            )
        );
        let over = [("RUSTANGO__DATABASE__URL", "postgresql://over/x")];
        assert_eq!(
            resolve(&over).unwrap(),
            (
                "postgres://over/x".to_owned(),
                SettingSource::Override("RUSTANGO__DATABASE__URL".into())
            )
        );
        let parts = [
            ("RUSTANGO__DATABASE__URL", "postgres://over/x"),
            ("DB_USER", "u"),
            ("DB_NAME", "d"),
        ];
        assert_eq!(resolve(&parts).unwrap().1, SettingSource::Env("DB_USER"));
        let bare = [
            ("RUSTANGO__DATABASE__URL", "postgres://over/x"),
            ("DATABASE_URL", "postgres://env/prod"),
        ];
        assert_eq!(
            resolve(&bare).unwrap(),
            (
                "postgres://env/prod".to_owned(),
                SettingSource::Env("DATABASE_URL")
            )
        );
        // A blank override and a blank file value count as unset.
        let blank = resolve_database_url_from(&vars(&[("RUSTANGO__DATABASE__URL", " ")]), Some(""));
        assert!(matches!(blank, Err(EnvError::MissingRequired { .. })));
    }

    #[test]
    fn database_url_errors_on_bad_driver() {
        let _g = env_lock().lock().unwrap();
        let _snap = EnvSnapshot::capture(DB_VARS);
        std::env::set_var("DB_DRIVER", "oracle");
        std::env::set_var("DB_USER", "u");
        std::env::set_var("DB_NAME", "d");
        let err = database_url_from_env().unwrap_err();
        assert!(matches!(err, EnvError::UnsupportedDriver(_)));
    }
}
