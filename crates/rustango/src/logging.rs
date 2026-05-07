//! Tracing-subscriber setup helpers — the boilerplate every rustango app
//! writes by hand becomes one call.
//!
//! ## Quick start
//!
//! ```ignore
//! fn main() {
//!     rustango::logging::setup();        // env-filter, pretty, "info,sqlx=warn"
//!     // ... rest of your main
//! }
//! ```
//!
//! Production:
//!
//! ```ignore
//! rustango::logging::Setup::new()
//!     .json()                            // structured output for log aggregators
//!     .with_default_env_filter("info")
//!     .install();
//! ```
//!
//! All functions are idempotent — `try_init` underneath, so calling twice
//! (e.g. from a test + from main) won't panic.

#[cfg(feature = "runtime")]
use tracing_subscriber::EnvFilter;

/// Default env-filter when `RUST_LOG` is unset:
/// info for app code + warn for sqlx (sqlx is verbose at info).
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Install the canonical dev logger: pretty format, env-filter from
/// `RUST_LOG` (falling back to `"info,sqlx=warn"`).
///
/// Idempotent — safe to call from `main`, tests, anywhere.
#[cfg(feature = "runtime")]
pub fn setup() {
    Setup::new().install();
}

/// Builder for the tracing-subscriber config.
///
/// All knobs are optional with sensible defaults. Build up the config and
/// call [`install`](Self::install) when done.
#[cfg(feature = "runtime")]
pub struct Setup {
    json: bool,
    default_filter: String,
    with_targets: bool,
    with_thread_ids: bool,
    with_line_numbers: bool,
}

#[cfg(feature = "runtime")]
impl Setup {
    /// New builder with defaults: pretty format, `"info,sqlx=warn"` filter,
    /// no thread IDs, no line numbers, targets shown.
    #[must_use]
    pub fn new() -> Self {
        Self {
            json: false,
            default_filter: DEFAULT_FILTER.to_owned(),
            with_targets: true,
            with_thread_ids: false,
            with_line_numbers: false,
        }
    }

    /// Output JSON instead of pretty colored format. Recommended for
    /// production (Loki / CloudWatch / Datadog all parse JSON).
    #[must_use]
    pub fn json(mut self) -> Self {
        self.json = true;
        self
    }

    /// Default env-filter when `RUST_LOG` is unset. Defaults to
    /// `"info,sqlx=warn"`.
    #[must_use]
    pub fn with_default_env_filter(mut self, filter: impl Into<String>) -> Self {
        self.default_filter = filter.into();
        self
    }

    /// Hide event targets (the module path) in pretty output.
    #[must_use]
    pub fn without_targets(mut self) -> Self {
        self.with_targets = false;
        self
    }

    /// Include thread IDs in events.
    #[must_use]
    pub fn with_thread_ids(mut self) -> Self {
        self.with_thread_ids = true;
        self
    }

    /// Include source-file line numbers in events. Useful in dev,
    /// noisy in prod.
    #[must_use]
    pub fn with_line_numbers(mut self) -> Self {
        self.with_line_numbers = true;
        self
    }

    /// Apply the config. Uses `try_init` under the hood — duplicate calls
    /// are silently ignored.
    pub fn install(self) {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&self.default_filter));

        if self.json {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .json()
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers)
                .try_init();
        } else {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers)
                .try_init();
        }
    }
}

#[cfg(feature = "runtime")]
impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

/// Decide whether to use JSON output based on `RUSTANGO_ENV`.
/// Returns `true` when env is `prod` or `production`.
#[must_use]
pub fn should_use_json_for_env() -> bool {
    matches!(
        std::env::var("RUSTANGO_ENV").as_deref(),
        Ok("prod") | Ok("production")
    )
}

/// One-call setup that picks the right format based on `RUSTANGO_ENV`:
/// JSON in prod, pretty in dev.
#[cfg(feature = "runtime")]
pub fn setup_for_env() {
    let mut s = Setup::new();
    if should_use_json_for_env() {
        s = s.json();
    }
    s.install();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn env_lock() -> &'static Mutex<()> {
        static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn should_use_json_for_prod_env() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("RUSTANGO_ENV", "prod");
        assert!(should_use_json_for_env());
        std::env::set_var("RUSTANGO_ENV", "production");
        assert!(should_use_json_for_env());
        std::env::remove_var("RUSTANGO_ENV");
    }

    #[test]
    fn should_use_pretty_for_other_envs() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("RUSTANGO_ENV", "local");
        assert!(!should_use_json_for_env());
        std::env::set_var("RUSTANGO_ENV", "staging");
        assert!(!should_use_json_for_env());
        std::env::remove_var("RUSTANGO_ENV");
    }

    #[test]
    fn should_use_pretty_when_unset() {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var("RUSTANGO_ENV");
        assert!(!should_use_json_for_env());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn builder_sets_json_flag() {
        let s = Setup::new().json();
        assert!(s.json);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn builder_with_default_env_filter_overrides() {
        let s = Setup::new().with_default_env_filter("debug");
        assert_eq!(s.default_filter, "debug");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn default_filter_constant() {
        assert_eq!(DEFAULT_FILTER, "info,sqlx=warn");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn install_is_idempotent() {
        // Calling twice should not panic
        Setup::new().install();
        Setup::new().install();
    }
}
