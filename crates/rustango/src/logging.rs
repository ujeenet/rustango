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

    /// Build a `Setup` from a [`crate::config::LoggingSettings`]
    /// section, mapping every TOML field to the matching builder
    /// method. Unknown enum-shaped values (`format`, `file_rotation`)
    /// fall back to the default + a `tracing::warn!` so a typo in
    /// the TOML doesn't fail boot. Roadmap #8, v0.30.11.
    ///
    /// ```ignore
    /// let settings = rustango::config::Settings::load_from_env()?;
    /// let _guard = rustango::logging::Setup::from_settings(&settings.logging).install();
    /// ```
    ///
    /// Or via the one-liner [`crate::manage::Cli::with_logging`].
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(s: &crate::config::LoggingSettings) -> Self {
        let mut setup = Self::new();
        if let Some(filter) = s.level.as_deref() {
            setup = setup.with_default_env_filter(filter);
        }
        match s.format.as_deref() {
            Some("json") => setup = setup.json(),
            Some("pretty") | None => {} // default
            Some("compact") => {}       // currently same as pretty; reserved
            Some(other) => {
                tracing::warn!(
                    target: "rustango::logging",
                    format = other,
                    "unknown logging format; falling back to pretty"
                );
            }
        }
        if matches!(s.with_thread_ids, Some(true)) {
            setup = setup.with_thread_ids();
        }
        if matches!(s.with_line_numbers, Some(true)) {
            setup = setup.with_line_numbers();
        }
        if matches!(s.without_targets, Some(true)) {
            setup = setup.without_targets();
        }
        if let Some(dir) = s.file_dir.as_deref() {
            let prefix = s.file_prefix.as_deref().unwrap_or("app");
            let rotation = match s.file_rotation.as_deref() {
                Some("hourly") => Rotation::Hourly,
                Some("minutely") => Rotation::Minutely,
                Some("never") => Rotation::Never,
                Some("daily") | None => Rotation::Daily,
                Some(other) => {
                    tracing::warn!(
                        target: "rustango::logging",
                        rotation = other,
                        "unknown logging rotation; falling back to daily"
                    );
                    Rotation::Daily
                }
            };
            setup = setup.with_file(dir, prefix, rotation);
            if matches!(s.file_only, Some(true)) {
                setup = setup.file_only();
            }
        }
        setup
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

    // ---- from_settings (roadmap #8, v0.30.11) ----

    /// Empty `LoggingSettings` (every field `None`) builds a Setup
    /// matching `Setup::new()` — the safer default that doesn't
    /// surprise existing projects when they add an empty
    /// `[logging]` section.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_empty_matches_new_defaults() {
        let s = Setup::from_settings(&crate::config::LoggingSettings::default());
        assert!(!s.json);
        assert_eq!(s.default_filter, DEFAULT_FILTER);
        assert!(!s.with_thread_ids);
        assert!(!s.with_line_numbers);
        assert!(s.with_targets);
        assert!(s.file_sink.is_none());
        assert!(s.keep_stdout);
    }

    /// Every populated field maps to the corresponding builder
    /// method. `format = "json"` flips to JSON output;
    /// `with_thread_ids` / `with_line_numbers` flip the format
    /// flags; `without_targets` hides target paths.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_populated_fields_drive_builder() {
        let cfg = crate::config::LoggingSettings {
            level: Some("debug,sqlx=info".into()),
            format: Some("json".into()),
            with_thread_ids: Some(true),
            with_line_numbers: Some(true),
            without_targets: Some(true),
            file_dir: None,
            file_prefix: None,
            file_rotation: None,
            file_only: None,
        };
        let s = Setup::from_settings(&cfg);
        assert!(s.json);
        assert_eq!(s.default_filter, "debug,sqlx=info");
        assert!(s.with_thread_ids);
        assert!(s.with_line_numbers);
        assert!(!s.with_targets);
    }

    /// `file_dir` set + every rotation variant maps to the right
    /// `Rotation`. Unknown values fall back to `Daily` (with a
    /// `tracing::warn!` we don't easily intercept here, but the
    /// effective behavior is right).
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_file_sink_resolves_rotation() {
        let mk = |rot: Option<&str>| {
            let mut cfg = crate::config::LoggingSettings::default();
            cfg.file_dir = Some("/tmp/_logging_settings_test".into());
            cfg.file_prefix = Some("app".into());
            cfg.file_rotation = rot.map(str::to_owned);
            Setup::from_settings(&cfg)
        };
        for (input, want) in [
            (Some("daily"), Rotation::Daily),
            (Some("hourly"), Rotation::Hourly),
            (Some("minutely"), Rotation::Minutely),
            (Some("never"), Rotation::Never),
            (None, Rotation::Daily),             // missing → daily
            (Some("nonsense"), Rotation::Daily), // unknown → daily fallback
        ] {
            let s = mk(input);
            let sink = s
                .file_sink
                .as_ref()
                .expect("file sink set when file_dir is");
            assert!(
                std::mem::discriminant(&sink.rotation) == std::mem::discriminant(&want),
                "rotation `{input:?}` resolved wrong"
            );
        }
    }

    /// `file_only = true` only drops stdout when `file_dir` is also
    /// set — `file_only` without a sink is a no-op (the boolean is
    /// ignored, no panic).
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_file_only_requires_file_dir() {
        // file_only=true but no file_dir → no sink, stdout kept
        // (file_only is a no-op without a sink to opt out of).
        let mut cfg = crate::config::LoggingSettings::default();
        cfg.file_only = Some(true);
        let s = Setup::from_settings(&cfg);
        assert!(s.file_sink.is_none());
        assert!(s.keep_stdout, "no sink → stdout stays");

        // file_dir set + file_only=true → sink set, stdout dropped.
        let mut cfg = crate::config::LoggingSettings::default();
        cfg.file_dir = Some("/tmp/_logging_settings_test".into());
        cfg.file_only = Some(true);
        let s = Setup::from_settings(&cfg);
        assert!(s.file_sink.is_some());
        assert!(!s.keep_stdout);
    }
}
