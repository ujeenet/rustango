//! Layered TOML configuration (slice 8.3).
//!
//! `Settings::load("local")` reads, in order:
//!
//! 1. `config/default.toml` — committed defaults shared by every env.
//! 2. `config/{env}.toml` — env-specific overrides (`local`, `staging`,
//!    `prod`, …). Missing file is fine — the load skips it.
//! 3. Environment variables — anything matching `RUSTANGO__SECTION__KEY`
//!    overrides the corresponding nested TOML key. Double-underscore
//!    is the path separator. `RUSTANGO__DATABASE__URL=postgres://…`
//!    overrides `[database] url = "…"`.
//!
//! The pipeline returns a typed [`Settings`] struct with sections
//! for `database`, `secret_key`, `admin`, `tenancy`, `cache`, `jobs`,
//! `mail`. Unknown keys in the TOML are ignored (forward-compat).
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
//! Gated by the `config` feature (in `default`). Drop with
//! `default-features = false` if you want a bare ORM dep without
//! `toml` pulled in.

mod loader;
mod sections;

pub use loader::ConfigError;
pub use sections::{
    AdminSettings, CacheSettings, DatabaseSettings, JobsSettings, MailSettings, Settings,
    TenancySettings,
};

impl Settings {
    /// Load + merge `config/default.toml`, `config/{env}_settings.toml`
    /// (or the legacy `config/{env}.toml`), and `RUSTANGO__*` env-var
    /// overrides. Returns the typed settings struct.
    ///
    /// Since v0.29 (#87) the tier convention `<env>_settings.toml` is
    /// preferred — the loader checks both filenames and prefers the
    /// `_settings` variant.
    ///
    /// # Errors
    /// * [`ConfigError::Io`] — `config/default.toml` is missing or
    ///   unreadable. The env-specific overlay is *optional* (skipped
    ///   silently if missing) — `default.toml` is the contract.
    /// * [`ConfigError::Parse`] — TOML syntax error.
    /// * [`ConfigError::EnvOverride`] — a `RUSTANGO__*` env var
    ///   couldn't be parsed into the target field's type.
    pub fn load(env: &str) -> Result<Self, ConfigError> {
        loader::load_with_root(std::path::Path::new("config"), env)
    }

    /// Load with an explicit `config/` directory — used in tests so
    /// fixtures don't have to sit at the project root.
    ///
    /// # Errors
    /// As [`Settings::load`].
    pub fn load_from(root: &std::path::Path, env: &str) -> Result<Self, ConfigError> {
        loader::load_with_root(root, env)
    }

    /// Convenience entry point that picks the env tier from the
    /// `RUSTANGO_ENV` environment variable (#87, v0.29). Falls back
    /// to `"dev"` when the var is unset / empty so a fresh
    /// `cargo run` Just Works without explicit config.
    ///
    /// Loads `config/default.toml` + `config/<RUSTANGO_ENV>_settings.toml`
    /// (or the legacy `config/<RUSTANGO_ENV>.toml`) + `RUSTANGO__*`
    /// env-var overrides — the same pipeline as [`Self::load`], just
    /// with the env tier chosen for you.
    ///
    /// Production deployments set `RUSTANGO_ENV=prod`; staging sets
    /// `RUSTANGO_ENV=staging`; local dev leaves it unset (or sets it
    /// to `dev` explicitly to make the choice visible).
    ///
    /// # Errors
    /// As [`Self::load`].
    pub fn load_from_env() -> Result<Self, ConfigError> {
        let env = current_env_tier();
        loader::load_with_root(std::path::Path::new("config"), &env)
    }

    /// The tier this process should load — reads `RUSTANGO_ENV`,
    /// defaults to `"dev"`. Public so deployment-audit tooling
    /// (`manage check --deploy`) can compare the resolved tier
    /// against the loaded settings without re-reading the env var.
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

    /// Default tier is `dev` when `RUSTANGO_ENV` is unset (or empty).
    /// Test deliberately uses no env-mutation — the workspace bans
    /// `unsafe std::env::set_var`, so we only cover the unset path
    /// here. The set-path is exercised end-to-end by the integration
    /// suite that spawns subprocesses with overridden envs.
    #[test]
    fn current_env_tier_defaults_to_dev_when_unset() {
        // Best effort — only meaningful when the test runner didn't
        // set RUSTANGO_ENV. Most CI runs leave it unset.
        if std::env::var("RUSTANGO_ENV").is_err() {
            assert_eq!(Settings::current_env_tier(), "dev");
        }
    }
}
