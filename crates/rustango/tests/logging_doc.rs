//! Backing test for `docs/logging.md` — the settings section and the
//! defaults the page publishes.
//!
//! The page's risky claims are not its prose, they are its two tables: the
//! `[logging]` TOML keys and their defaults. A key that never deserialized
//! would read as configured and do nothing, which is the failure mode a
//! reader cannot detect — their filter simply doesn't apply. So the TOML in
//! the page is written to disk here and loaded through the real loader.

#![cfg(all(feature = "config", feature = "runtime"))]

use rustango::config::Settings;
use rustango::logging::{Rotation, Setup};

/// Suite-wide lock. The loader reads the process environment, so one test
/// setting `RUSTANGO__LOGGING__LEVEL` is visible to every other test's
/// `load()` — under the default parallel harness that is a real flake, not
/// a theoretical one: it turned two of these green-alone tests red.
fn env_lock() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}

fn guard() -> std::sync::MutexGuard<'static, ()> {
    env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Write a `config/` tree and load it the way a deployment would.
fn load(default_toml: &str) -> (Settings, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("config");
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(cfg.join("default.toml"), default_toml).expect("write default.toml");
    let settings = Settings::load_from(&cfg, "dev").expect("load settings");
    (settings, dir)
}

/// Every key in the page's `[logging]` table reaches the struct. A typo'd
/// key name here is silently ignored by serde, so the assertion has to be
/// on the parsed value, not on the load succeeding.
#[test]
fn the_documented_logging_keys_all_deserialize() {
    let _g = guard();
    // Verbatim from `docs/logging.md`, both TOML blocks merged.
    let (settings, _dir) = load(
        r#"
[logging]
level             = "info,sqlx=warn"
format            = "json"
with_thread_ids   = true
with_line_numbers = true
without_targets   = true
file_dir          = "/var/log/myapp"
file_prefix       = "app"
file_rotation     = "daily"
file_only         = true
"#,
    );

    let l = &settings.logging;
    assert_eq!(l.level.as_deref(), Some("info,sqlx=warn"));
    assert_eq!(l.format.as_deref(), Some("json"));
    assert_eq!(l.with_thread_ids, Some(true));
    assert_eq!(l.with_line_numbers, Some(true));
    assert_eq!(l.without_targets, Some(true));
    assert_eq!(l.file_dir.as_deref(), Some("/var/log/myapp"));
    assert_eq!(l.file_prefix.as_deref(), Some("app"));
    assert_eq!(l.file_rotation.as_deref(), Some("daily"));
    assert_eq!(l.file_only, Some(true));
}

/// The page says every key is optional and an absent `[logging]` section is
/// equivalent to the defaults.
#[test]
fn an_absent_logging_section_leaves_every_key_unset() {
    let _g = guard();
    let (settings, _dir) = load("[server]\nbind = \"127.0.0.1:8080\"\n");
    let l = &settings.logging;
    assert!(l.level.is_none());
    assert!(l.format.is_none());
    assert!(l.file_dir.is_none());
    assert!(l.file_only.is_none());
}

/// The page states this default twice — as the fallback filter and as the
/// `level` default. Both come from here.
#[test]
fn the_documented_default_filter_is_the_one_in_code() {
    assert_eq!(rustango::logging::DEFAULT_FILTER, "info,sqlx=warn");
}

/// `Setup::from_settings` accepts the documented enum-shaped values, and
/// unknown ones fall back rather than failing the boot. Asserting it does
/// not panic is the claim: the page tells operators a typo is survivable.
#[test]
fn unknown_enum_values_fall_back_instead_of_failing() {
    let _g = guard();
    let (settings, _dir) = load(
        r#"
[logging]
format        = "nonsense"
file_dir      = "/tmp/rustango-doc-test"
file_rotation = "fortnightly"
"#,
    );
    let _setup = Setup::from_settings(&settings.logging);
}

/// Every rotation name the page's table lists is a real variant.
#[test]
fn the_documented_rotations_exist() {
    let _ = [
        Rotation::Daily,
        Rotation::Hourly,
        Rotation::Minutely,
        Rotation::Never,
    ];
}

/// The page tells operators to override any key per-deployment with
/// `RUSTANGO__LOGGING__LEVEL`. That path is assembled by splitting on `__`,
/// so it is worth pinning the exact spelling the page publishes.
///
/// Serialised against every other env-mutating test in this binary, and it
/// restores the prior value — the loader reads the process environment.
#[test]
fn the_documented_env_override_reaches_the_logging_section() {
    let _g = guard();

    let prior = std::env::var("RUSTANGO__LOGGING__LEVEL").ok();
    std::env::set_var("RUSTANGO__LOGGING__LEVEL", "debug,sqlx=warn");

    let (settings, _dir) = load("[logging]\nlevel = \"info\"\n");

    match prior {
        Some(v) => std::env::set_var("RUSTANGO__LOGGING__LEVEL", v),
        None => std::env::remove_var("RUSTANGO__LOGGING__LEVEL"),
    }

    assert_eq!(
        settings.logging.level.as_deref(),
        Some("debug,sqlx=warn"),
        "the env override must beat the TOML value"
    );
}
