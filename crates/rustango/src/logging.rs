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
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "runtime")]
use tracing_subscriber::util::SubscriberInitExt;
#[cfg(feature = "runtime")]
use tracing_subscriber::EnvFilter;

/// Default env-filter when `RUST_LOG` is unset:
/// info for app code + warn for sqlx (sqlx is verbose at info).
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Install the canonical dev logger: pretty format, env-filter from
/// `RUST_LOG` (falling back to `"info,sqlx=warn"`).
///
/// Idempotent — safe to call from `main`, tests, anywhere. Stdout-only;
/// for file output use [`Setup::with_file`].
#[cfg(feature = "runtime")]
pub fn setup() {
    let _ = Setup::new().install();
}

/// File-rotation cadence for [`Setup::with_file`]. Mirrors
/// `tracing_appender::rolling::Rotation` — re-exported here so
/// callers don't need a direct dep on `tracing-appender`.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy)]
pub enum Rotation {
    /// Roll the file at midnight UTC.
    Daily,
    /// Roll the file every hour on the hour.
    Hourly,
    /// Roll the file every minute (mostly useful for tests).
    Minutely,
    /// One file forever — no rotation.
    Never,
}

#[cfg(feature = "runtime")]
impl Rotation {
    fn to_appender(self) -> tracing_appender::rolling::Rotation {
        use tracing_appender::rolling::Rotation as R;
        match self {
            Self::Daily => R::DAILY,
            Self::Hourly => R::HOURLY,
            Self::Minutely => R::MINUTELY,
            Self::Never => R::NEVER,
        }
    }
}

/// One configured file output for [`Setup`]. Internal — users
/// construct this implicitly via [`Setup::with_file`].
#[cfg(feature = "runtime")]
struct FileSink {
    dir: std::path::PathBuf,
    filename_prefix: String,
    rotation: Rotation,
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
    /// Tee logs to a rolling file in addition to stdout. `None` =
    /// stdout-only (the default, matches Setup::new).
    file_sink: Option<FileSink>,
    /// `true` keeps the stdout layer alongside the file output. Set
    /// to `false` via [`Setup::file_only`] when you want logs to land
    /// in the file ONLY (e.g. headless workers, daemonized
    /// processes).
    keep_stdout: bool,
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
            file_sink: None,
            keep_stdout: true,
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

    /// Tee logs to a rolling file in `dir`/`filename_prefix.YYYY-MM-DD`
    /// in addition to stdout. By default rotates daily; pass a
    /// different [`Rotation`] to override. The directory is created on
    /// first write if it doesn't exist.
    ///
    /// File output uses `tracing-appender`'s non-blocking writer so a
    /// stalled disk doesn't pause request handling — events queue
    /// in-memory and drop only under sustained extreme pressure.
    /// Closes future-backlog item #1 ("advanced logging config").
    ///
    /// ```ignore
    /// use rustango::logging::{Setup, Rotation};
    /// Setup::new()
    ///     .json()
    ///     .with_file("/var/log/myapp", "app", Rotation::Daily)
    ///     .install();
    /// ```
    #[must_use]
    pub fn with_file(
        mut self,
        dir: impl Into<std::path::PathBuf>,
        filename_prefix: impl Into<String>,
        rotation: Rotation,
    ) -> Self {
        self.file_sink = Some(FileSink {
            dir: dir.into(),
            filename_prefix: filename_prefix.into(),
            rotation,
        });
        self
    }

    /// When [`Self::with_file`] is configured, drop the stdout layer
    /// so logs land in the rolling file ONLY. No-op when no file
    /// sink is configured.
    #[must_use]
    pub fn file_only(mut self) -> Self {
        self.keep_stdout = false;
        self
    }

    /// Apply the config. Uses `try_init` under the hood — duplicate calls
    /// are silently ignored. When [`Self::with_file`] is configured,
    /// returns the `tracing_appender::WorkerGuard` that flushes
    /// pending writes on drop — keep it alive for the lifetime of the
    /// process (typically by stashing in a `static` or `OnceLock`).
    /// `None` is returned when no file sink is configured.
    #[must_use = "the returned WorkerGuard must outlive the process so file writes flush"]
    pub fn install(self) -> Option<tracing_appender::non_blocking::WorkerGuard> {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&self.default_filter));

        let Some(file_sink) = self.file_sink else {
            // No file sink — keep the prior fmt::init path so the
            // single-output story is unchanged for existing callers.
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
            return None;
        };

        // File sink + optional stdout: compose two `fmt::Layer`s
        // through `tracing_subscriber::registry()`. Each layer gets
        // its own writer (stdout vs the rolling file), but they
        // share the env filter.
        let appender = tracing_appender::rolling::RollingFileAppender::new(
            file_sink.rotation.to_appender(),
            file_sink.dir,
            file_sink.filename_prefix,
        );
        let (file_writer, guard) = tracing_appender::non_blocking(appender);

        // Build the layers and `try_init` the registry. Two arms:
        // one for json, one for pretty — couldn't share a generic
        // because `Layer` types differ when format toggles.
        if self.json {
            let file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers)
                .with_writer(file_writer);
            let registry = tracing_subscriber::registry()
                .with(env_filter)
                .with(file_layer);
            if self.keep_stdout {
                let stdout_layer = tracing_subscriber::fmt::layer()
                    .json()
                    .with_target(self.with_targets)
                    .with_thread_ids(self.with_thread_ids)
                    .with_line_number(self.with_line_numbers);
                let _ = registry.with(stdout_layer).try_init();
            } else {
                let _ = registry.try_init();
            }
        } else {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers)
                .with_writer(file_writer)
                .with_ansi(false);
            let registry = tracing_subscriber::registry()
                .with(env_filter)
                .with(file_layer);
            if self.keep_stdout {
                let stdout_layer = tracing_subscriber::fmt::layer()
                    .with_target(self.with_targets)
                    .with_thread_ids(self.with_thread_ids)
                    .with_line_number(self.with_line_numbers);
                let _ = registry.with(stdout_layer).try_init();
            } else {
                let _ = registry.try_init();
            }
        }
        Some(guard)
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
/// JSON in prod, pretty in dev. Stdout-only; for file output use
/// [`Setup::with_file`].
#[cfg(feature = "runtime")]
pub fn setup_for_env() {
    let mut s = Setup::new();
    if should_use_json_for_env() {
        s = s.json();
    }
    let _ = s.install();
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
        let _ = Setup::new().install();
        let _ = Setup::new().install();
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn with_file_sets_file_sink() {
        let s = Setup::new().with_file("/tmp/_logging_test", "app", Rotation::Daily);
        assert!(s.file_sink.is_some());
        let sink = s.file_sink.as_ref().unwrap();
        assert_eq!(sink.filename_prefix, "app");
        assert!(matches!(sink.rotation, Rotation::Daily));
        assert!(s.keep_stdout, "default keeps stdout alongside file");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn file_only_drops_stdout() {
        let s = Setup::new()
            .with_file("/tmp/_logging_test", "app", Rotation::Hourly)
            .file_only();
        assert!(!s.keep_stdout);
    }
}
