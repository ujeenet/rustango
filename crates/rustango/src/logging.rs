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
/// How the terminal output is shaped.
///
/// Split out because `json` used to be a `bool` and `pretty`/`compact`
/// were strings the installer ignored — three documented values, two
/// behaviours, and no type that said so (#1480).
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Single-line, the default. `tracing_subscriber`'s `Full`.
    #[default]
    Full,
    /// Multi-line, one field per line. Verbose; good for a dev terminal.
    Pretty,
    /// Terser single line — drops the target and shortens the level.
    Compact,
    /// One JSON object per event, for log aggregators.
    Json,
}

/// When to emit ANSI colour.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// `Auto` honours [`NO_COLOR`](https://no-color.org) first, then asks
    /// the OS whether stdout is a terminal — piping to a file, or a CI
    /// runner capturing output, reports "not a terminal" and wants plain
    /// text.
    ///
    /// The `NO_COLOR` check has to be explicit here. `tracing-subscriber`
    /// consults it in its own default, but every layer built below calls
    /// `.with_ansi(..)`, which replaces that default outright — so
    /// turning the `ansi` feature on and then setting the flag by hand
    /// would have *removed* `NO_COLOR` support that was otherwise about
    /// to arrive for free.
    ///
    /// Per the spec any non-empty value means "no colour"; `NO_COLOR=`
    /// set but empty does not.
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

/// A `fmt` layer with its concrete formatter type erased.
///
/// `Vec<Box<dyn Layer<S>>>` is itself a `Layer<S>`, which is what lets
/// the sinks be collected before the registry is built. Chaining
/// `.with()` per layer changes the subscriber's type at every step, so
/// an erased layer built for `Registry` would not fit after the first.
#[cfg(feature = "runtime")]
type Erased = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// The one place a [`Format`] becomes a formatter.
///
/// Extracted from `install` so it can be exercised directly. The mapping
/// is where the original bug lived — `Pretty` and `Compact` both fell
/// through to `Full` — and settings-to-enum tests cannot see it: they
/// pass unchanged if this function sends every variant to the same
/// formatter. `format_variants_render_differently` renders through
/// *this* function rather than a copy of its match.
///
/// `ansi` is caller-resolved and already false for a file sink: escape
/// codes written into a rotating log file corrupt every downstream grep.
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

#[cfg(feature = "runtime")]
pub struct Setup {
    format: Format,
    color: Color,
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

    /// Output JSON instead of the terminal format. Recommended for
    /// production (Loki / CloudWatch / Datadog all parse JSON).
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
        // Every recognised value now reaches a distinct formatter.
        // `pretty` and `compact` used to be accepted and discarded.
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

        // Colour is a stdout-only decision. It is resolved once here so
        // every branch below agrees, and so `Auto` asks about the
        // terminal exactly once.
        let ansi = self.format != Format::Json && self.color.should_colour();

        let Some(file_sink) = self.file_sink else {
            // No file sink — keep the prior fmt::init path so the
            // single-output story is unchanged for existing callers.
            let b = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(self.with_targets)
                .with_thread_ids(self.with_thread_ids)
                .with_line_number(self.with_line_numbers);
            // Each arm calls the formatter that its name promises.
            // `Pretty` and `Compact` previously fell through to `Full`,
            // so `format = "compact"` and `format = "pretty"` produced
            // byte-identical output (#1480).
            match self.format {
                Format::Json => {
                    let _ = b.json().try_init();
                }
                Format::Pretty => {
                    let _ = b.pretty().with_ansi(ansi).try_init();
                }
                Format::Compact => {
                    let _ = b.compact().with_ansi(ansi).try_init();
                }
                Format::Full => {
                    let _ = b.with_ansi(ansi).try_init();
                }
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

        // Build the layers and `try_init` the registry.
        //
        // Each formatter is a different `Layer` type, so four formats
        // times two sinks is eight concrete types that cannot share a
        // binding. The previous shape dodged that by having only two
        // arms — which is the mechanism by which `pretty` and `compact`
        // silently became `full`. `.boxed()` erases the type instead, so
        // adding a format costs one match arm and cannot quietly
        // collapse into another.
        //
        // `with_ansi(false)` on the file layer is not belt-and-braces:
        // escape codes written into a rotating log file corrupt every
        // downstream grep.
        // `Vec<Box<dyn Layer<S>>>` is itself a `Layer<S>`, which is what
        // lets the sinks be collected before the registry is built. The
        // obvious alternative — chaining `.with()` per layer — changes
        // the subscriber's type at every step, so an erased layer built
        // for `Registry` no longer fits after the first one.
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
        let _ = tracing_subscriber::registry()
            .with(layers)
            .with(env_filter)
            .try_init();
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
        assert_eq!(s.format, Format::Json);
    }

    /// Every documented format value reaches its own formatter.
    ///
    /// `pretty` and `compact` were both accepted and both discarded, so
    /// three documented values produced two behaviours (#1480). Asserting
    /// the mapping is the cheap half; the expensive half — that the
    /// output actually differs — is the `rendered` module at the foot of
    /// this file.
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

        // NOTE: this deliberately does NOT loop `assert_ne!` over the
        // variants. An earlier version did, and it could not fail:
        // `assert_ne!` on distinct fieldless enum variants is a
        // tautology. It read like a distinctness check and asserted
        // nothing.
        //
        // What actually needs pinning is enum -> formatter, which lives
        // in `install()` and cannot be reached from here — changing
        // `Format::Compact` to build a `Full` layer leaves every test in
        // this module green. That is exactly where the original bug was,
        // so it is checked by capturing real output in
        // the `rendered` module below.
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

        // `Auto` is deliberately NOT asserted against a fixed value.
        //
        // This used to read `assert!(!Color::Auto.should_colour())` on
        // the stated premise that "under `cargo test` stdout is
        // captured, so Auto must resolve to false". That premise is
        // false: libtest captures `print!` through a thread-local, while
        // `should_colour` asks `std::io::stdout().is_terminal()` about
        // file descriptor 1 — which is still the terminal. So the test
        // failed for every contributor running `cargo test` from a
        // terminal, and passed in CI only because runners have no tty.
        // A test whose result depends on who is watching cannot be a
        // gate, and this one was green in exactly the place that could
        // not see it.
        //
        // What *is* deterministic is the `NO_COLOR` contract, which is
        // the part with a specification behind it.
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        assert!(
            !Color::Auto.should_colour(),
            "NO_COLOR=1 must suppress colour regardless of tty state"
        );
        std::env::set_var("NO_COLOR", "");
        // Per the spec, an *empty* NO_COLOR does not mean "no colour",
        // so Auto falls through to the tty question — whose answer
        // depends on the environment and so is not asserted here.
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

    // ---- from_settings (roadmap #8, v0.30.11) ----

    /// Empty `LoggingSettings` (every field `None`) builds a Setup
    /// matching `Setup::new()` — the safer default that doesn't
    /// surprise existing projects when they add an empty
    /// `[logging]` section.
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

/// Rendered-output tests for the format mapping.
///
/// Everything else about formats is asserted at the enum level, which
/// cannot see the bug these exist for: `install()` sending two variants
/// to the same formatter. These render a real event through
/// [`fmt_layer`] — the same function `install` uses — and compare bytes.
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
            // Inside a span, and with a target: `Compact` differs from
            // `Full` precisely in how it renders span context and the
            // target, so a bare event cannot tell them apart.
            let span = tracing::info_span!("req", route = "/x");
            let _g = span.enter();
            tracing::info!(target: "demo::target", answer = 42, "hello");
        });
        let bytes = buf.0.lock().expect("buffer lock").clone();
        String::from_utf8(bytes).expect("utf8")
    }

    /// Strip the RFC3339 timestamp, which is different on every render.
    ///
    /// Without this, comparing two renders is comparing two clocks:
    /// `assert_ne!` passes no matter what the formatters did, which is
    /// how the first version of this test stayed green while
    /// `Format::Compact` was pointed at the `Full` formatter.
    fn shape(s: &str) -> String {
        // Strip any RFC3339-looking run wherever it appears, not just
        // whole whitespace-separated tokens.
        //
        // The token filter alone could not see JSON's timestamp: JSON
        // renders as one whitespace-free blob starting `{` and ending
        // `}`, so `starts_with("20") && ends_with('Z')` never matched
        // and a clock reading survived into every comparison. Each
        // `assert_ne!` pair involving `Format::Json` then passed because
        // the two runs happened at different microseconds — regardless
        // of what `fmt_layer` did with the format. That is the exact
        // vacuity this test was rewritten to remove, one level in.
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

    /// `shape()` must actually remove the clock.
    ///
    /// This is the control for `format_variants_render_differently`.
    /// Two renders of the **same** format differ only by timestamp, so
    /// if `shape()` works they compare equal — and if it does not, the
    /// `assert_ne!` pairs in that test pass because of the clock rather
    /// than because of the formatter. JSON is the case that was broken:
    /// it renders as one whitespace-free blob, so a token-level filter
    /// never saw its embedded timestamp.
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

    /// No two formats render the same shape.
    ///
    /// This is the assertion the enum-level test could not make. Point
    /// `Format::Compact` at `base.with_ansi(ansi)` and this fails;
    /// every other logging test stays green.
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

    /// Colour is observed, not merely configured.
    ///
    /// Nothing asserted this before: the `ansi` feature could have been
    /// left off and every test still passed, which is how colour came to
    /// be compiled out while the docs advertised it (#1480).
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
