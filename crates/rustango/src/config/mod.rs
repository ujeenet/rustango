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
    /// Load + merge `config/default.toml`, `config/{env}.toml`, and
    /// `RUSTANGO__*` env-var overrides. Returns the typed settings
    /// struct.
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
}
