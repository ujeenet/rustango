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
//! The result is a typed [`Settings`] struct. Unknown TOML keys are
//! ignored, so an older binary still reads a newer config file.
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

pub use loader::ConfigError;
pub use sections::{
    AdminSettings, AuditSettings, AuthSettings, BrandSettings, CacheSettings, DatabaseSettings,
    I18nSettings, JobsSettings, JwtSettings, LoggingSettings, MailSettings, McpSettings,
    RoutesSettings, SecuritySettings, ServerSettings, Settings, SsoSettings, TenancySettings,
};

impl Settings {
    /// Merge `config/default.toml`, `config/{env}_settings.toml` (or
    /// the older `config/{env}.toml`), and `RUSTANGO__*` env vars.
    /// The loader checks both filenames and prefers `_settings`.
    ///
    /// # Errors
    /// * [`ConfigError::Io`] when `config/default.toml` is missing or
    ///   unreadable. The per-env file is optional.
    /// * [`ConfigError::Parse`] on a TOML syntax error.
    /// * [`ConfigError::EnvOverride`] when a `RUSTANGO__*` var does
    ///   not parse into the field's type.
    pub fn load(env: &str) -> Result<Self, ConfigError> {
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
        let env = current_env_tier();
        loader::load_with_root(std::path::Path::new("config"), &env)
    }

    /// The tier this process loads: `RUSTANGO_ENV`, or `"dev"`.
    /// Public so `manage check --deploy` can compare the tier with
    /// the loaded settings without reading the env var again.
    #[must_use]
    pub fn current_env_tier() -> String {
        current_env_tier()
    }
}

fn current_env_tier() -> String {
    std::env::var("RUSTANGO_ENV")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_owned())
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
