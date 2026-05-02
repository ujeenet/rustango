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
}
