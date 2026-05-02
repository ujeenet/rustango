//! Secrets manager — pluggable backend for retrieving secret values.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::secrets::{Secrets, EnvSecrets};
//! use std::sync::Arc;
//!
//! let secrets: Arc<dyn Secrets> = Arc::new(EnvSecrets::with_prefix("MYAPP_"));
//!
//! let db_password = secrets.get("DB_PASSWORD").await?;  // looks up MYAPP_DB_PASSWORD
//! ```
//!
//! ## Backends
//!
//! | Backend | Description |
//! |---------|-------------|
//! | [`EnvSecrets`] | Reads from environment variables (with optional prefix) |
//! | [`InMemorySecrets`] | Tests — secrets in a HashMap |
//! | Vault / AWS / GCP | Plug your own — implement `Secrets` for the SDK of your choice |
//!
//! ## Example: AWS Secrets Manager
//!
//! ```ignore
//! use rustango::secrets::{Secrets, SecretsError};
//! use async_trait::async_trait;
//!
//! pub struct AwsSecrets { client: aws_sdk_secretsmanager::Client }
//!
//! #[async_trait]
//! impl Secrets for AwsSecrets {
//!     async fn get(&self, key: &str) -> Result<Option<String>, SecretsError> {
//!         let result = self.client.get_secret_value().secret_id(key).send().await
//!             .map_err(|e| SecretsError::Backend(e.to_string()))?;
//!         Ok(result.secret_string)
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Pluggable secrets backend.
#[async_trait]
pub trait Secrets: Send + Sync + 'static {
    /// Retrieve the secret value for `key`. Returns `None` when the
    /// secret is not present in the backend.
    async fn get(&self, key: &str) -> Result<Option<String>, SecretsError>;

    /// Convenience: get a required secret. Returns `Err(Backend)` if missing.
    async fn require(&self, key: &str) -> Result<String, SecretsError> {
        self.get(key)
            .await?
            .ok_or_else(|| SecretsError::Backend(format!("secret `{key}` not set")))
    }
}

/// `Arc<dyn Secrets>` alias.
pub type BoxedSecrets = Arc<dyn Secrets>;

// ------------------------------------------------------------------ EnvSecrets

/// Environment-variable backed secrets store.
///
/// With a prefix (e.g. `"MYAPP_"`), `get("DB_PASSWORD")` reads `MYAPP_DB_PASSWORD`.
/// Without a prefix, reads `DB_PASSWORD` directly.
pub struct EnvSecrets {
    prefix: String,
}

impl EnvSecrets {
    /// New env-secrets reader with no prefix — reads env vars verbatim.
    #[must_use]
    pub fn new() -> Self {
        Self { prefix: String::new() }
    }

    /// New env-secrets reader that prepends `prefix` to every key lookup.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into() }
    }
}

impl Default for EnvSecrets {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Secrets for EnvSecrets {
    async fn get(&self, key: &str) -> Result<Option<String>, SecretsError> {
        let full = format!("{}{key}", self.prefix);
        match std::env::var(&full) {
            Ok(v) => Ok(Some(v)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretsError::Backend(format!(
                "env var `{full}` is not valid UTF-8"
            ))),
        }
    }
}

// ------------------------------------------------------------------ InMemorySecrets

/// In-memory secrets store — for tests. `Arc<Mutex<HashMap>>` inside so
/// you can mutate after construction.
pub struct InMemorySecrets {
    inner: Mutex<HashMap<String, String>>,
}

impl InMemorySecrets {
    #[must_use]
    pub fn new() -> Self {
        Self { inner: Mutex::new(HashMap::new()) }
    }

    /// Pre-populate with key→value pairs. Builder-style.
    #[must_use]
    pub fn with(mut self, pairs: &[(&str, &str)]) -> Self {
        for (k, v) in pairs {
            self.inner
                .get_mut()
                .expect("secrets poisoned")
                .insert((*k).to_owned(), (*v).to_owned());
        }
        self
    }

    /// Set a secret value.
    pub fn set(&self, key: &str, value: &str) {
        self.inner
            .lock()
            .expect("secrets poisoned")
            .insert(key.to_owned(), value.to_owned());
    }

    /// Remove a secret.
    pub fn remove(&self, key: &str) {
        self.inner.lock().expect("secrets poisoned").remove(key);
    }
}

impl Default for InMemorySecrets {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Secrets for InMemorySecrets {
    async fn get(&self, key: &str) -> Result<Option<String>, SecretsError> {
        Ok(self.inner.lock().expect("secrets poisoned").get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn env_lock() -> &'static Mutex<()> {
        static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn env_secrets_no_prefix_reads_var() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("RUSTANGO_TEST_SECRET_PLAIN", "foo");
        let s = EnvSecrets::new();
        let v = s.get("RUSTANGO_TEST_SECRET_PLAIN").await.unwrap();
        assert_eq!(v.as_deref(), Some("foo"));
        std::env::remove_var("RUSTANGO_TEST_SECRET_PLAIN");
    }

    #[tokio::test]
    async fn env_secrets_with_prefix_prepends() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("MYAPP_DB_PASSWORD", "hunter2");
        let s = EnvSecrets::with_prefix("MYAPP_");
        let v = s.get("DB_PASSWORD").await.unwrap();
        assert_eq!(v.as_deref(), Some("hunter2"));
        std::env::remove_var("MYAPP_DB_PASSWORD");
    }

    #[tokio::test]
    async fn env_secrets_missing_returns_none() {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var("RUSTANGO_TEST_MISSING_SECRET");
        let s = EnvSecrets::new();
        let v = s.get("RUSTANGO_TEST_MISSING_SECRET").await.unwrap();
        assert_eq!(v, None);
    }

    #[tokio::test]
    async fn require_errors_when_missing() {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var("RUSTANGO_TEST_REQUIRED_MISSING");
        let s = EnvSecrets::new();
        let r = s.require("RUSTANGO_TEST_REQUIRED_MISSING").await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn require_returns_value_when_present() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("RUSTANGO_TEST_REQUIRED_OK", "value");
        let s = EnvSecrets::new();
        let r = s.require("RUSTANGO_TEST_REQUIRED_OK").await.unwrap();
        assert_eq!(r, "value");
        std::env::remove_var("RUSTANGO_TEST_REQUIRED_OK");
    }

    #[tokio::test]
    async fn in_memory_set_and_get() {
        let s = InMemorySecrets::new();
        s.set("api_key", "abc123");
        assert_eq!(s.get("api_key").await.unwrap().as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn in_memory_with_builder() {
        let s = InMemorySecrets::new().with(&[("k1", "v1"), ("k2", "v2")]);
        assert_eq!(s.get("k1").await.unwrap().as_deref(), Some("v1"));
        assert_eq!(s.get("k2").await.unwrap().as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn in_memory_missing_returns_none() {
        let s = InMemorySecrets::new();
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_remove() {
        let s = InMemorySecrets::new().with(&[("k", "v")]);
        s.remove("k");
        assert!(s.get("k").await.unwrap().is_none());
    }
}
