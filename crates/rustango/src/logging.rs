//! Tracing-subscriber setup in one call.
//!
//! ## Quick start
//!
//! ```ignore
//! fn main() {
//!     rustango::logging::setup();        // env-filter, full, "info,sqlx=warn"
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
//! All functions use `try_init`, so calling them twice never panics.

#[cfg(feature = "runtime")]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "runtime")]
use tracing_subscriber::util::SubscriberInitExt;
#[cfg(feature = "runtime")]
use tracing_subscriber::EnvFilter;

/// Default env-filter when `RUST_LOG` is unset. `sqlx` is very noisy at
/// info, so it is pinned to warn.
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Install the dev logger: `full` format, filter from `RUST_LOG` or
/// `"info,sqlx=warn"`.
///
/// Safe to call more than once. Stdout only; for a file use
/// [`Setup::with_file`].
#[cfg(feature = "runtime")]
pub fn setup() {
    let _ = Setup::new().install();
}

/// How often [`Setup::with_file`] rolls the log file. Mirrors
/// `tracing_appender::rolling::Rotation`, so callers do not need a
/// direct dependency on `tracing-appender`.
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

/// One configured file output, built by [`Setup::with_file`].
#[cfg(feature = "runtime")]
struct FileSink {
    dir: std::path::PathBuf,
    filename_prefix: String,
    rotation: Rotation,
}

/// How the terminal output is shaped.
///
/// `#[non_exhaustive]` so a new variant does not break a downstream
/// `match`.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Format {
    /// Single-line, the default. `tracing_subscriber`'s `Full`.
    #[default]
    Full,
    /// Multi-line, one field per line. Verbose; good for a dev terminal.
    Pretty,
    /// Terser single line: drops the target, shortens the level.
    Compact,
    /// One JSON object per event, for log aggregators.
    Json,
}

/// When to emit ANSI colour.
///
/// # Not honoured under `#[rustango::main]`
///
/// That macro installs a subscriber before your `main` body runs, so a
/// later `Setup::install()` does nothing and this setting is ignored.
/// The macro always uses the [`Color::Auto`] rule. To pick `Always` or
/// `Never`, install the subscriber yourself from a plain `main`.
///
/// `#[non_exhaustive]` — see [`Format`].
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Color {
    /// Colour only when stdout is a terminal. The default.
    #[default]
    Auto,
    /// Always colour, even when piped. For a pager that understands it.
    Always,
    /// Never colour.
    Never,
}

#[cfg(feature = "runtime")]
impl Color {
    /// Should the stdout layer emit escape codes?
    ///
    /// `Auto` checks [`NO_COLOR`](https://no-color.org) first, then asks
    /// whether stdout is a terminal. Any non-empty `NO_COLOR` value
    /// means "no colour"; an empty one does not.
    ///
    /// The `NO_COLOR` check is explicit because every layer below calls
    /// `.with_ansi(..)`, which overrides the `tracing-subscriber`
    /// default that would otherwise handle it.
    #[must_use]
    pub fn should_colour(self) -> bool {
        use std::io::IsTerminal as _;
        match self {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => {
                let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
                !no_color && std::io::stdout().is_terminal()
            }
        }
    }
}

/// A `fmt` layer with its formatter type erased.
///
/// `Vec<Box<dyn Layer<S>>>` is itself a `Layer<S>`, so sinks can be
/// collected before the registry is built. Chaining `.with()` instead
/// would change the subscriber's type at every step.
#[cfg(feature = "runtime")]
type Erased = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// The one place a [`Format`] becomes a formatter. Separate from
/// `install` so tests can render through it directly.
///
/// The caller resolves `ansi`, and passes false for a file sink: escape
/// codes in a log file break every later grep.
#[cfg(feature = "runtime")]
fn fmt_layer<W>(
    format: Format,
    ansi: bool,
    targets: bool,
    thread_ids: bool,
    line_numbers: bool,
    writer: Option<W>,
) -> Erased
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Clone + Send + Sync + 'static,
{
    use tracing_subscriber::Layer as _;
    macro_rules! sink {
        ($l:expr) => {
            match writer.clone() {
                Some(w) => $l.with_writer(w).boxed(),
                None => $l.boxed(),
            }
        };
    }
    let base = tracing_subscriber::fmt::layer()
        .with_target(targets)
        .with_thread_ids(thread_ids)
        .with_line_number(line_numbers);
    match format {
        Format::Json => sink!(base.json()),
        Format::Pretty => sink!(base.pretty().with_ansi(ansi)),
        Format::Compact => sink!(base.compact().with_ansi(ansi)),
        Format::Full => sink!(base.with_ansi(ansi)),
    }
}

/// Builder for the tracing-subscriber config.
///
/// Every option has a sensible default. Set what you need, then call
/// [`install`](Self::install).
#[cfg(feature = "runtime")]
pub struct Setup {
    format: Format,
    color: Color,
    default_filter: String,
    with_targets: bool,
    with_thread_ids: bool,
    with_line_numbers: bool,
    /// Also write logs to a rolling file. `None` means stdout only.
    file_sink: Option<FileSink>,
    /// Keep the stdout layer next to the file output. [`Setup::file_only`]
    /// clears it.
    keep_stdout: bool,
}

#[cfg(feature = "runtime")]
impl Setup {
    /// New builder with defaults: `full` format, `"info,sqlx=warn"`
    /// filter, no thread IDs, no line numbers, targets shown.
    #[must_use]
    pub fn new() -> Self {
        Self {
            format: Format::Full,
            color: Color::Auto,
            default_filter: DEFAULT_FILTER.to_owned(),
            with_targets: true,
            with_thread_ids: false,
            with_line_numbers: false,
            file_sink: None,
            keep_stdout: true,
        }
    }

    /// Output JSON instead of the terminal format. Best for production,
    /// where log aggregators parse JSON.
    #[must_use]
    pub fn json(mut self) -> Self {
        self.format = Format::Json;
        self
    }

    /// Pick the output format explicitly.
    #[must_use]
    pub fn with_format(mut self, format: Format) -> Self {
        self.format = format;
        self
    }

    /// When to colour terminal output. Defaults to [`Color::Auto`].
    #[must_use]
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// Default env-filter when `RUST_LOG` is unset. Defaults to
    /// `"info,sqlx=warn"`.
    #[must_use]
    pub fn with_default_env_filter(mut self, filter: impl Into<String>) -> Self {
        self.default_filter = filter.into();
        self
    }

    /// Hide event targets (the module path) from the output. Applies to
    /// every format.
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

    /// Also write logs to a rolling file at
    /// `dir`/`filename_prefix.YYYY-MM-DD`. The directory is created on
    /// first write.
    ///
    /// Writes go through `tracing-appender`'s non-blocking writer, so a
    /// slow disk does not stall request handling. Events queue in
    /// memory and drop only under heavy sustained pressure.
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

    /// Drop the stdout layer so logs go only to the rolling file.
    /// Does nothing unless [`Self::with_file`] was called.
    #[must_use]
    pub fn file_only(mut self) -> Self {
        self.keep_stdout = false;
        self
    }

    /// Build a `Setup` from a [`crate::config::LoggingSettings`]
    /// section. Every TOML field maps to a builder method. An
    /// unknown `format` or `file_rotation` value logs a warning and
    /// falls back to the default, so a typo does not fail boot.
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
        setup = match s.format.as_deref() {
            Some("json") => setup.with_format(Format::Json),
            Some("pretty") => setup.with_format(Format::Pretty),
            Some("compact") => setup.with_format(Format::Compact),
            Some("full") | None => setup.with_format(Format::Full),
            Some(other) => {
                tracing::warn!(
                    target: "rustango::logging",
                    format = other,
                    "unknown logging format; falling back to full"
                );
                setup.with_format(Format::Full)
            }
        };
        setup = match s.color.as_deref() {
            Some("always") => setup.with_color(Color::Always),
            Some("never") => setup.with_color(Color::Never),
            Some("auto") | None => setup.with_color(Color::Auto),
            Some(other) => {
                tracing::warn!(
                    target: "rustango::logging",
                    color = other,
                    "unknown logging color mode; falling back to auto"
                );
                setup.with_color(Color::Auto)
            }
        };
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

    /// Apply the config. Uses `try_init`, so a duplicate call is
    /// ignored.
    ///
    /// With [`Self::with_file`], returns a `WorkerGuard` that flushes
    /// pending writes when dropped. Keep it alive for the whole
    /// process. Returns `None` when there is no file sink.
    #[must_use = "the returned WorkerGuard must outlive the process so file writes flush"]
    pub fn install(self) -> Option<tracing_appender::non_blocking::WorkerGuard> {
        let env_filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(&self.default_filter));

        // Colour is a stdout-only decision, resolved once so every
        // branch agrees and `Auto` asks about the terminal once.
        let ansi = self.format != Format::Json && self.color.should_colour();

        let Some(file_sink) = self.file_sink else {
            // Stdout only: the simple `fmt::init` path.
            let b = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers);
            let outcome = match self.format {
                Format::Json => b.json().try_init(),
                Format::Pretty => b.pretty().with_ansi(ansi).try_init(),
                Format::Compact => b.compact().with_ansi(ansi).try_init(),
                Format::Full => b.with_ansi(ansi).try_init(),
            };
            warn_if_already_installed(&outcome);
            return None;
        };

        // File sink plus optional stdout: two `fmt::Layer`s on one
        // registry. Each gets its own writer; they share the filter.
        let appender = tracing_appender::rolling::RollingFileAppender::new(
            file_sink.rotation.to_appender(),
            file_sink.dir,
            file_sink.filename_prefix,
        );
        let (file_writer, guard) = tracing_appender::non_blocking(appender);

        // Each format is a different `Layer` type, so the layers are
        // boxed and collected: `Vec<Box<dyn Layer<S>>>` is itself a
        // `Layer<S>`. The file layer always gets `ansi = false`;
        // escape codes in a log file break every later grep.
        let build = |writer: Option<tracing_appender::non_blocking::NonBlocking>| -> Erased {
            let to_file = writer.is_some();
            fmt_layer(
                self.format,
                ansi && !to_file,
                self.with_targets,
                self.with_thread_ids,
                self.with_line_numbers,
                writer,
            )
        };

        let mut layers: Vec<Erased> = vec![build(Some(file_writer))];
        if self.keep_stdout {
            layers.push(build(None));
        }
        let outcome = tracing_subscriber::registry()
            .with(layers)
            .with(env_filter)
            .try_init();
        if warn_if_already_installed(&outcome) {
            // Returning the guard would keep a writer alive for a file
            // nothing is routed to, which looks like a working sink.
            return None;
        }
        Some(guard)
    }
}

/// Warn when a subscriber was already installed, and return `true`
/// in that case.
///
/// An `Err` from `try_init` means every setting just assembled was
/// dropped and the first subscriber stays. The usual cause is
/// `#[rustango::main]`, so the message names its opt-out. Staying
/// quiet here is how `format = "json"` silently stays plain text.
#[cfg(feature = "runtime")]
fn warn_if_already_installed<E: std::fmt::Display>(outcome: &Result<(), E>) -> bool {
    let Err(e) = outcome else {
        return false;
    };
    // The installed subscriber may filter this out, so also print to
    // stderr. The point is that it is not silent.
    let msg = format!(
        "[logging] settings ignored: a tracing subscriber is already installed ({e}). \
         If this is `#[rustango::main]`, use `#[rustango::main(logging = false)]` \
         so your own setup installs first."
    );
    tracing::warn!(target: "rustango::logging", "{msg}");
    eprintln!("warning: {msg}");
    true
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

/// One-call setup that picks the format from `RUSTANGO_ENV`: JSON in
/// prod, `full` in dev. Stdout only; for a file use
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

    /// The suite-wide env lock, shared with `error` and
    /// `template_debug`. Lib tests share one process, so a private
    /// mutex here would not serialize against those modules.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::error::test_env::lock()
    }

    #[test]
    fn should_use_json_for_prod_env() {
        let _g = env_lock();
        std::env::set_var("RUSTANGO_ENV", "prod");
        assert!(should_use_json_for_env());
        std::env::set_var("RUSTANGO_ENV", "production");
        assert!(should_use_json_for_env());
        std::env::remove_var("RUSTANGO_ENV");
    }

    #[test]
    fn should_not_use_json_for_other_envs() {
        let _g = env_lock();
        std::env::set_var("RUSTANGO_ENV", "local");
        assert!(!should_use_json_for_env());
        std::env::set_var("RUSTANGO_ENV", "staging");
        assert!(!should_use_json_for_env());
        std::env::remove_var("RUSTANGO_ENV");
    }

    #[test]
    fn should_not_use_json_when_unset() {
        let _g = env_lock();
        std::env::remove_var("RUSTANGO_ENV");
        assert!(!should_use_json_for_env());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn builder_sets_json_flag() {
        let s = Setup::new().json();
        assert_eq!(s.format, Format::Json);
    }

    /// Every documented format value maps to its own `Format`. That the
    /// output really differs is checked by the `rendered` module below.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn every_format_value_maps_to_a_distinct_formatter() {
        let of = |v: Option<&str>| {
            Setup::from_settings(&crate::config::LoggingSettings {
                format: v.map(str::to_owned),
                ..Default::default()
            })
            .format
        };
        assert_eq!(of(Some("json")), Format::Json);
        assert_eq!(of(Some("pretty")), Format::Pretty);
        assert_eq!(of(Some("compact")), Format::Compact);
        assert_eq!(of(Some("full")), Format::Full);
        assert_eq!(of(None), Format::Full, "absent means full");
        assert_eq!(of(Some("nonsense")), Format::Full, "unknown falls back");

        // No `assert_ne!` loop over the variants: on distinct fieldless
        // variants that is a tautology. The mapping that matters is
        // format -> formatter, which the `rendered` module checks by
        // capturing real output.
    }

    /// Colour: `never` off, `always` on, `auto` decided by the terminal.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn color_setting_maps_and_no_color_suppresses() {
        let of = |v: Option<&str>| {
            Setup::from_settings(&crate::config::LoggingSettings {
                color: v.map(str::to_owned),
                ..Default::default()
            })
            .color
        };
        assert_eq!(of(Some("always")), Color::Always);
        assert_eq!(of(Some("never")), Color::Never);
        assert_eq!(of(Some("auto")), Color::Auto);
        assert_eq!(of(None), Color::Auto);
        assert_eq!(of(Some("nonsense")), Color::Auto);

        assert!(Color::Always.should_colour());
        assert!(!Color::Never.should_colour());

        // `Auto` is not asserted against a fixed value: it asks whether
        // fd 1 is a terminal, which differs between a local run and CI.
        // Only the `NO_COLOR` contract is deterministic.
        let _guard = env_lock();
        let previous = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        assert!(
            !Color::Auto.should_colour(),
            "NO_COLOR=1 must suppress colour regardless of tty state"
        );
        std::env::set_var("NO_COLOR", "");
        // An empty NO_COLOR does not mean "no colour", so Auto falls
        // through to the tty question. Not asserted, see above.
        let _ = Color::Auto.should_colour();
        match previous {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
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

    // ---- from_settings ----

    /// Empty `LoggingSettings` builds the same Setup as `Setup::new()`,
    /// so adding an empty `[logging]` section changes nothing.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_empty_matches_new_defaults() {
        let s = Setup::from_settings(&crate::config::LoggingSettings::default());
        assert_eq!(s.format, Format::Full);
        assert_eq!(s.color, Color::Auto);
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
            color: None,
            with_thread_ids: Some(true),
            with_line_numbers: Some(true),
            without_targets: Some(true),
            file_dir: None,
            file_prefix: None,
            file_rotation: None,
            file_only: None,
            access_log: None,
        };
        let s = Setup::from_settings(&cfg);
        assert_eq!(s.format, Format::Json);
        assert_eq!(s.default_filter, "debug,sqlx=info");
        assert!(s.with_thread_ids);
        assert!(s.with_line_numbers);
        assert!(!s.with_targets);
    }

    /// Every rotation value maps to the right `Rotation`; unknown ones
    /// fall back to `Daily`.
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

    /// `file_only = true` drops stdout only when `file_dir` is also
    /// set. Without a sink it does nothing.
    #[cfg(all(feature = "runtime", feature = "config"))]
    #[test]
    fn from_settings_file_only_requires_file_dir() {
        // file_only=true but no file_dir → no sink, stdout kept.
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

/// Rendered-output tests for the format mapping.
///
/// The enum-level tests cannot see two variants reaching the same
/// formatter. These render a real event through [`fmt_layer`], the
/// function `install` uses, and compare the bytes.
#[cfg(all(test, feature = "runtime"))]
mod rendered {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Emit one event through `fmt_layer` and return what was written.
    fn render(format: Format, ansi: bool) -> String {
        let buf = Buf::default();
        let layer = fmt_layer(format, ansi, true, false, false, Some(buf.clone()));
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            // Inside a span and with a target: `Compact` differs from
            // `Full` only in how it renders those, so a bare event
            // cannot tell them apart.
            let span = tracing::info_span!("req", route = "/x");
            let _g = span.enter();
            tracing::info!(target: "demo::target", answer = 42, "hello");
        });
        let bytes = buf.0.lock().expect("buffer lock").clone();
        String::from_utf8(bytes).expect("utf8")
    }

    /// Strip the RFC3339 timestamp, which differs on every render.
    /// Without this, `assert_ne!` between two renders just compares
    /// two clocks and passes whatever the formatters did.
    fn shape(s: &str) -> String {
        // Strip the timestamp wherever it appears, not only as a whole
        // whitespace-separated token: JSON renders as one blob with no
        // spaces, so a token filter would miss its clock reading.
        let mut out = String::with_capacity(s.len());
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            // `2026-09-16T19:03:15.520524Z` — date, `T`, time, `Z`.
            let looks_like_ts = b[i] == b'2'
                && i + 20 <= b.len()
                && b[i + 1].is_ascii_digit()
                && b[i + 4] == b'-'
                && b[i + 7] == b'-'
                && b[i + 10] == b'T';
            if looks_like_ts {
                let mut j = i + 10;
                while j < b.len() && b[j] != b'Z' {
                    j += 1;
                }
                if j < b.len() {
                    out.push_str("<ts>");
                    i = j + 1;
                    continue;
                }
            }
            out.push(b[i] as char);
            i += 1;
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// `shape()` must really remove the clock.
    ///
    /// The control for `format_variants_render_differently`: two
    /// renders of the *same* format differ only by timestamp, so they
    /// must compare equal after `shape()`. Otherwise that test's
    /// `assert_ne!` pairs pass because of the clock.
    #[test]
    fn shape_removes_the_timestamp_for_every_format() {
        for f in [Format::Full, Format::Pretty, Format::Compact, Format::Json] {
            let a = render(f, false);
            let b = render(f, false);
            assert_ne!(a, b, "{f:?}: two renders should differ by timestamp");
            assert_eq!(
                shape(&a),
                shape(&b),
                "{f:?}: shape() left a clock reading in, so every comparison \
                 using it can pass for the wrong reason:\n{a}\n{b}"
            );
        }
    }

    /// No two formats render the same shape. Point `Format::Compact`
    /// at the `Full` layer and only this test fails.
    #[test]
    fn format_variants_render_differently() {
        let all = [
            (Format::Full, render(Format::Full, false)),
            (Format::Pretty, render(Format::Pretty, false)),
            (Format::Compact, render(Format::Compact, false)),
            (Format::Json, render(Format::Json, false)),
        ];
        for (f, out) in &all {
            assert!(!out.is_empty(), "{f:?} rendered nothing");
            assert!(out.contains("hello"), "{f:?} lost the message: {out:?}");
        }
        for (i, (fa, a)) in all.iter().enumerate() {
            for (fb, b) in &all[i + 1..] {
                assert_ne!(
                    shape(a),
                    shape(b),
                    "{fa:?} and {fb:?} render identically once the timestamp is \
                     removed:\n{a}"
                );
            }
        }
    }

    /// The shapes are what their names claim.
    #[test]
    fn each_format_has_its_documented_shape() {
        let json = render(Format::Json, false);
        assert!(
            json.trim_start().starts_with('{') && json.contains("\"answer\":42"),
            "json should be one object per event: {json}"
        );
        // Pretty puts fields on their own lines; Full and Compact do not.
        assert!(
            render(Format::Pretty, false).lines().count()
                > render(Format::Full, false).lines().count(),
            "pretty should be multi-line where full is not"
        );
    }

    /// Colour is observed, not just configured. Without this, the
    /// `ansi` feature could be left off and every test still pass.
    #[test]
    fn ansi_produces_escape_codes_and_its_absence_does_not() {
        const ESC: char = '\u{1b}';
        for f in [Format::Full, Format::Pretty, Format::Compact] {
            assert!(
                render(f, true).contains(ESC),
                "{f:?} with ansi=true emitted no escape codes — the `ansi` feature \
                 is not compiled in"
            );
            assert!(
                !render(f, false).contains(ESC),
                "{f:?} with ansi=false still emitted escape codes"
            );
        }
        assert!(
            !render(Format::Json, true).contains(ESC),
            "JSON must never be coloured — escape codes inside a JSON string break \
             every consumer"
        );
    }
}
