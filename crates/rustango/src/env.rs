//! Typed environment variable readers — pydantic-settings / django-environ shape.
//!
//! Reads `std::env::var` and parses the value into the target type. Returns
//! `Result` so missing or malformed values surface explicitly at startup
//! rather than silently defaulting in handlers.
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
    /// Required env var missing during structured assembly (e.g.
    /// [`database_url_from_env`]). Carries a `hint` so the operator
    /// sees what to set, not just what's missing.
    #[error("environment variable `{name}` is required: {hint}")]
    MissingRequired { name: String, hint: String },
    /// `DB_DRIVER` (or sniffed `DATABASE_URL` scheme) wasn't a
    /// recognized backend.
    #[error("unsupported DB driver `{0}` — expected one of: postgres, postgresql, mysql")]
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
/// [`EnvError::Parse`] if the variable IS set but can't be parsed into `T`.
/// (A typo'd value is more dangerous than a missing one — surface it.)
pub fn with_default<T>(name: &str, default: T) -> Result<T, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(name) else { return Ok(default) };
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
    let Some(raw) = lookup(name) else { return Ok(None) };
    raw.parse::<T>().map(Some).map_err(|e| EnvError::Parse {
        name: name.to_owned(),
        ty: std::any::type_name::<T>(),
        detail: e.to_string(),
    })
}

/// Read a comma-separated env var into a `Vec<T>`.
///
/// Empty entries (e.g. trailing comma) are dropped. Returns `None` when
/// the variable is unset.
///
/// # Errors
/// [`EnvError::Parse`] if any entry can't be parsed into `T`.
pub fn list<T>(name: &str) -> Result<Option<Vec<T>>, EnvError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = lookup(name) else { return Ok(None) };
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

/// Read an env var as a `Duration` interpreting the value as seconds.
///
/// # Errors
/// [`EnvError::Missing`] when unset.
/// [`EnvError::Parse`] when the value isn't a non-negative integer.
pub fn duration_secs(name: &str) -> Result<Duration, EnvError> {
    let secs: u64 = required(name)?;
    Ok(Duration::from_secs(secs))
}

/// Read an env var as a `Duration` interpreting the value as milliseconds.
///
/// # Errors
/// As [`duration_secs`].
pub fn duration_millis(name: &str) -> Result<Duration, EnvError> {
    let ms: u64 = required(name)?;
    Ok(Duration::from_millis(ms))
}

// ------------------------------------------------------------------ Startup validation

/// One declared environment-variable requirement.
#[derive(Debug, Clone)]
pub struct EnvRequirement {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Validator that collects required env vars and reports ALL missing ones at once.
///
/// Use during `Builder::serve(...)` startup so misconfigurations surface as
/// a single clear error message instead of crashing on the first DB call.
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

    /// Add an optional env var (logged as a hint when absent, not a failure).
    #[must_use]
    pub fn optional(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.reqs.push(EnvRequirement {
            name: name.into(),
            description: description.into(),
            required: false,
        });
        self
    }

    /// Run validation. Returns names of missing REQUIRED vars (empty when all set).
    /// Optional vars are not in the failure list.
    #[must_use]
    pub fn check(&self) -> Vec<&EnvRequirement> {
        self.reqs
            .iter()
            .filter(|r| r.required && lookup(&r.name).is_none())
            .collect()
    }

    /// Run validation and return `Err(message)` listing every missing required var.
    /// The message is formatted as a multi-line shell-runnable hint block.
    ///
    /// # Errors
    /// Returns the formatted error message when any required vars are missing.
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

    /// Run validation and panic with a multi-line message if any required
    /// vars are missing. Suitable for `Builder::serve(...)` startup.
    ///
    /// # Panics
    /// Panics with a formatted message when any required vars are missing.
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
    /// Canonical scheme as it appears in the URL (`"postgres"` /
    /// `"mysql"`). `postgresql://` is accepted on parse but normalized
    /// to `postgres://` here.
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

/// Builder for a database URL assembled from individual fields.
///
/// Used by [`database_url_from_env`] when `DATABASE_URL` is unset and
/// the connection details are split across `DB_HOST` / `DB_USER` etc.
/// Passwords are percent-encoded automatically so `@`, `:`, `/`, `#`,
/// `?`, `%`, and other URL-special characters in passwords don't
/// corrupt the URL.
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
#[derive(Debug, Clone)]
pub struct DatabaseUrlBuilder {
    driver: DbDriver,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
    params: Option<String>,
}

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
    /// Raw query string appended after `?` (e.g.
    /// `"sslmode=require&application_name=app"`). Caller is
    /// responsible for percent-encoding inside `params`.
    #[must_use]
    pub fn params(mut self, params: impl Into<String>) -> Self {
        self.params = Some(params.into());
        self
    }

    /// Assemble the URL. Always emits `scheme://[user[:pw]@]host:port[/db][?params]`.
    /// Host defaults to `localhost`; port defaults to the driver's default
    /// when the field is unset.
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
        out.push_str(host);
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

/// Resolve a database URL from environment variables.
///
/// Resolution order:
/// 1. `DATABASE_URL` if set — returned verbatim (driver sniffed from
///    scheme; `postgresql://` → `postgres://`).
/// 2. Otherwise assembled from:
///    - `DB_DRIVER` (default `postgres`) — one of `postgres` /
///      `postgresql` / `mysql` / `mariadb`.
///    - `DB_HOST` (default `localhost`)
///    - `DB_PORT` (default `5432` for postgres, `3306` for mysql)
///    - `DB_USER` (**required** when assembling)
///    - `DB_PASSWORD` (optional; auto percent-encoded)
///    - `DB_NAME` (**required** when assembling)
///    - `DB_PARAMS` (optional raw query string)
///
/// # Errors
/// - [`EnvError::MissingRequired`] when assembling and `DB_USER` /
///   `DB_NAME` are unset.
/// - [`EnvError::UnsupportedDriver`] when `DB_DRIVER` isn't recognized.
/// - [`EnvError::Parse`] if `DB_PORT` is set to something non-numeric.
pub fn database_url_from_env() -> Result<String, EnvError> {
    if let Some(url) = lookup("DATABASE_URL") {
        // Normalize `postgresql://` → `postgres://` so callers can
        // match on a single scheme downstream.
        if let Some(rest) = url.strip_prefix("postgresql://") {
            return Ok(format!("postgres://{rest}"));
        }
        return Ok(url);
    }
    let driver = match lookup("DB_DRIVER") {
        Some(s) => DbDriver::parse(&s)?,
        None => DbDriver::Postgres,
    };
    let user = lookup("DB_USER").ok_or_else(|| EnvError::MissingRequired {
        name: "DB_USER".into(),
        hint: "set DATABASE_URL or DB_USER (e.g. `export DB_USER=app`)".into(),
    })?;
    let name = lookup("DB_NAME").ok_or_else(|| EnvError::MissingRequired {
        name: "DB_NAME".into(),
        hint: "set DATABASE_URL or DB_NAME (e.g. `export DB_NAME=app`)".into(),
    })?;
    let mut b = DatabaseUrlBuilder::new(driver).user(user).database(name);
    if let Some(host) = lookup("DB_HOST") {
        b = b.host(host);
    }
    if let Some(port) = optional::<u16>("DB_PORT")? {
        b = b.port(port);
    }
    if let Some(pw) = lookup("DB_PASSWORD") {
        b = b.password(pw);
    }
    if let Some(params) = lookup("DB_PARAMS") {
        b = b.params(params);
    }
    Ok(b.build())
}

/// RFC 3986 percent-encode for the *userinfo* component (user / password).
/// Encodes everything outside the unreserved set + `-._~`. Conservative —
/// we'd rather over-encode than risk a `@` or `:` in a password breaking
/// the URL parser on the other end.
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

/// Percent-encode for the *path* component. More permissive than
/// userinfo — `/` is the segment separator so we leave it alone in
/// the rare case someone names a DB with a `/` (they shouldn't).
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

    // Serialize env-var tests so they don't trample each other
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
        with_env("RUSTANGO_TEST_HOSTS", "a.example.com, b.example.com,c.example.com", || {
            let v: Vec<String> = list("RUSTANGO_TEST_HOSTS").unwrap().unwrap();
            assert_eq!(v, vec!["a.example.com", "b.example.com", "c.example.com"]);
        });
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
        // Set two vars without nesting with_env — nested locks would deadlock
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
            let v = Validator::new()
                .require("RUSTANGO_TEST_MISSING_REQ", "needed for X");
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
            let v = Validator::new()
                .require("RUSTANGO_TEST_FORMAT_REQ", "Postgres URL");
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
        assert_eq!(url, "postgres://svc:secret@db.internal:6543/app?sslmode=require");
    }

    #[test]
    fn builder_percent_encodes_password_specials() {
        // `@`, `:`, `/`, `#`, `?`, `%` must all be encoded inside the password.
        let url = DatabaseUrlBuilder::new(DbDriver::Postgres)
            .user("u")
            .password("p@ss:/word#?%")
            .database("db")
            .build();
        assert_eq!(url, "postgres://u:p%40ss%3A%2Fword%23%3F%25@localhost:5432/db");
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

    /// Drop-guard that restores or unsets a list of env vars on exit.
    /// Lets a test capture and restore the *original* state — important
    /// because the test process might already have e.g. `DATABASE_URL`
    /// set by the developer's shell.
    struct EnvSnapshot {
        saved: Vec<(&'static str, Option<String>)>,
    }
    impl EnvSnapshot {
        fn capture(names: &[&'static str]) -> Self {
            let saved = names
                .iter()
                .map(|n| (*n, std::env::var(n).ok()))
                .collect();
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
        "DB_NAME",
        "DB_PARAMS",
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
