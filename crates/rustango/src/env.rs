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
}
