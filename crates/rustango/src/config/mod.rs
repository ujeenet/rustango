//! Layered TOML configuration.
//!
//! `Settings::load("local")` reads three layers, in order:
//!
//! 1. `config/default.toml`: committed defaults for every env.
//! 2. `config/{env}.toml`: per-env overrides (`local`, `staging`,
//!    `prod`, …). A missing file is fine and is skipped.
//! 3. Environment variables named `RUSTANGO__SECTION__KEY`. The double
//!    underscore separates path parts, so
//!    `RUSTANGO__DATABASE__URL=postgres://…` overrides
//!    `[database] url`.
//!
//! The result is a typed [`Settings`] struct. Unknown keys (from files
//! or env vars) are ignored with a warning on stderr, so an older
//! binary still reads a newer config file. Set `RUSTANGO_CONFIG_STRICT=1`
//! to refuse them instead.
//!
//! # Example
//!
//! ```ignore
//! // config/default.toml
//! //   [database]
//! //   url = "postgres://localhost/myapp_dev"
//! //
//! //   [admin]
//! //   read_only_tables = ["audit_log"]
//!
//! // config/prod.toml
//! //   [database]
//! //   pool_max_size = 50
//!
//! // RUSTANGO__DATABASE__URL=postgres://…/myapp_prod cargo run
//!
//! let cfg = rustango::config::Settings::load("prod")?;
//! assert_eq!(cfg.database.pool_max_size, 50);
//! ```
//!
//! Behind the `config` feature, which is on by default. Turn it off
//! with `default-features = false` to use the ORM without `toml`.

mod loader;
mod sections;

pub use loader::{ConfigError, TomlSyntaxError};
pub use sections::{
    AdminSettings, AuditSettings, AuthSettings, BrandSettings, CacheSettings, DatabaseSettings,
    DiskSettings, ErrorsSettings, I18nSettings, JobsSettings, JwtSettings, LoggingSettings,
    MailSettings, McpSettings, RoutesSettings, SecuritySettings, ServerSettings, Settings,
    SsoSettings, StorageSettings, TenancySettings,
};

impl Settings {
    /// Merge `config/default.toml`, `config/{env}_settings.toml` (or
    /// the older `config/{env}.toml`), and `RUSTANGO__*` env vars.
    /// The loader checks both filenames and prefers `_settings`.
    /// Inside `test_settings::with_overridden` it returns the overlay.
    ///
    /// # Errors
    /// * [`ConfigError::Io`] when `config/default.toml` is missing or
    ///   unreadable. The per-env file is optional.
    /// * [`ConfigError::Parse`] on a TOML syntax error.
    /// * [`ConfigError::EnvOverride`] when a `RUSTANGO__*` var does
    ///   not parse into the field's type.
    pub fn load(env: &str) -> Result<Self, ConfigError> {
        if let Some(overlay) = crate::test_settings::overlay() {
            return Ok(overlay);
        }
        loader::load_with_root(std::path::Path::new("config"), env)
    }

    /// Load from a given `config/` directory. Tests use this so
    /// fixtures need not sit at the project root.
    ///
    /// # Errors
    /// As [`Settings::load`].
    pub fn load_from(root: &std::path::Path, env: &str) -> Result<Self, ConfigError> {
        loader::load_with_root(root, env)
    }

    /// Like [`Self::load`], but takes the tier from the `RUSTANGO_ENV`
    /// variable. Falls back to `"dev"` when it is unset or empty, so a
    /// fresh `cargo run` works with no config.
    ///
    /// Set `RUSTANGO_ENV=prod` in production and `staging` in
    /// staging. Local dev can leave it unset.
    ///
    /// # Errors
    /// As [`Self::load`].
    pub fn load_from_env() -> Result<Self, ConfigError> {
        Self::load(&current_env_tier())
    }

    /// The tier this process loads: `RUSTANGO_ENV` trimmed and lowercased
    /// (`production` reads `prod`), or `"dev"`.
    /// Public so `manage check --deploy` can compare the tier with
    /// the loaded settings without reading the env var again.
    #[must_use]
    pub fn current_env_tier() -> String {
        current_env_tier()
    }
}

fn current_env_tier() -> String {
    crate::env::tier_from_env()
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    /// The tier is `dev` when `RUSTANGO_ENV` is unset or empty. This
    /// test sets no env var, because the workspace bans
    /// `std::env::set_var`. The set path is covered by the
    /// integration suite, which spawns subprocesses.
    #[test]
    fn current_env_tier_defaults_to_dev_when_unset() {
        // Only meaningful when the runner left RUSTANGO_ENV unset,
        // which most CI runs do.
        if std::env::var("RUSTANGO_ENV").is_err() {
            assert_eq!(Settings::current_env_tier(), "dev");
        }
    }
}
