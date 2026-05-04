//! Settings layering — Chapter 1 §1.8.
//!
//! `config/default.toml` → `config/{RUSTANGO_ENV}.toml` → env-var
//! overrides (`RUSTANGO__SECTION__KEY`). Backed by
//! [`rustango::config::Settings::load_from`].

use rustango::config::{ConfigError, Settings};

pub fn load() -> Result<Settings, ConfigError> {
    let env = std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "default".into());
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
    Settings::load_from(&root, &env)
}
