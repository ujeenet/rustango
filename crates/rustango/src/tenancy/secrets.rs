//! Pluggable secret resolution — `Org.database_url` is a *reference*,
//! not necessarily a literal connection string.
//!
//! Default impl is [`LiteralSecretsResolver`] (pass-through), which
//! makes the abstraction zero-cost for users who don't need vault
//! integration. Users add HashiCorp Vault / AWS Secrets Manager /
//! Azure Key Vault / GCP Secret Manager / 1Password by implementing
//! the trait themselves or pulling in a future
//! `rustango-tenancy-vault` / `-aws-sm` / etc. crate.
//!
//! The contract: given a reference string from an Org row's
//! `database_url`, return the actual Postgres connection URL.
//! Implementations decide what reference format they accept —
//! `vault://path/to/secret`, `env://VAR_NAME`, `aws-sm://arn:...`,
//! or just a literal `postgres://...` URL.
//!
//! ## Caching
//!
//! Resolution can be expensive (network round-trip to the vault).
//! [`TenantPools`](super::TenantPools) caches the *resolved pool*,
//! not the resolved string — so the resolver runs once per tenant
//! per pool-build. Vault rotation works as long as the operator
//! eventually drops & rebuilds the pool (or a TTL-aware resolver
//! re-resolves on its own schedule). A standalone TTL-cached
//! resolver wrapper can land as a follow-up; not in slice 3.

use async_trait::async_trait;

/// Errors raised while resolving a secret reference.
#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    /// The reference points at something that doesn't exist (env var
    /// unset, vault path missing, etc.).
    #[error("secret not found: {0}")]
    NotFound(String),

    /// The reference itself is malformed for this resolver's scheme.
    #[error("invalid secret reference: {0}")]
    Invalid(String),

    /// Backend (vault, AWS, etc.) errored out — network, auth, etc.
    /// Boxed so any backend can wrap its own error type.
    #[error("secrets backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Resolve a secret reference (e.g. `vault://secret/data/tenants/acme`)
/// to its actual value (e.g. a Postgres connection URL).
///
/// `reference` shape is implementation-defined. The default
/// [`LiteralSecretsResolver`] passes through verbatim.
#[async_trait]
pub trait SecretsResolver: Send + Sync + 'static {
    /// Resolve `reference` to its plaintext secret.
    ///
    /// # Errors
    /// As [`SecretsError`].
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError>;
}

// ---------------- LiteralSecretsResolver ----------------

/// Pass-through resolver. The reference IS the value. Default for
/// [`super::TenantPools`] when the operator doesn't supply one.
///
/// Use this when `Org.database_url` carries the actual connection URL
/// (typical hobby / pre-production setups). Move to
/// [`EnvSecretsResolver`] or a vault-backed impl when secrets need to
/// stop living in the registry plaintext.
pub struct LiteralSecretsResolver;

#[async_trait]
impl SecretsResolver for LiteralSecretsResolver {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        Ok(reference.to_owned())
    }
}

// ---------------- EnvSecretsResolver ----------------

/// Resolve `env://VAR_NAME` references via [`std::env::var`]. Bare
/// references (no scheme) error — mix with [`ChainSecretsResolver`]
/// + [`LiteralSecretsResolver`] if you want fallback to literal.
///
/// Useful as a stepping stone toward vault-backed deployments: keep
/// secrets out of the registry DB by storing only the env-var name
/// and letting the operator's env (Kubernetes secrets, .env, systemd
/// EnvironmentFile, etc.) carry the actual value.
pub struct EnvSecretsResolver;

#[async_trait]
impl SecretsResolver for EnvSecretsResolver {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        let var = reference.strip_prefix("env://").ok_or_else(|| {
            SecretsError::Invalid(format!("expected env:// prefix, got `{reference}`"))
        })?;
        if var.is_empty() {
            return Err(SecretsError::Invalid(format!(
                "env:// reference has empty variable name: `{reference}`"
            )));
        }
        std::env::var(var).map_err(|_| SecretsError::NotFound(var.to_owned()))
    }
}

// ---------------- ChainSecretsResolver ----------------

/// Try each child resolver in order, dispatching by URL scheme prefix.
/// Falls through to the final default (typically
/// [`LiteralSecretsResolver`]) when no scheme matches.
///
/// Standard composition: `[("env://", EnvSecretsResolver)]` plus
/// `LiteralSecretsResolver` as the catch-all default.
pub struct ChainSecretsResolver {
    matchers: Vec<(String, Box<dyn SecretsResolver>)>,
    default: Box<dyn SecretsResolver>,
}

impl ChainSecretsResolver {
    /// Construct an empty chain. Use [`Self::push`] / [`Self::default_to`]
    /// to populate.
    #[must_use]
    pub fn new(default: impl SecretsResolver) -> Self {
        Self {
            matchers: Vec::new(),
            default: Box::new(default),
        }
    }

    /// Append a scheme-keyed resolver. The first scheme prefix that
    /// matches the reference wins.
    #[must_use]
    pub fn push(mut self, scheme: impl Into<String>, resolver: impl SecretsResolver) -> Self {
        self.matchers.push((scheme.into(), Box::new(resolver)));
        self
    }

    /// Standard chain: `env://` → [`EnvSecretsResolver`], everything
    /// else → [`LiteralSecretsResolver`]. Recommended starting point.
    #[must_use]
    pub fn standard() -> Self {
        Self::new(LiteralSecretsResolver).push("env://", EnvSecretsResolver)
    }
}

#[async_trait]
impl SecretsResolver for ChainSecretsResolver {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        for (scheme, resolver) in &self.matchers {
            if reference.starts_with(scheme.as_str()) {
                return resolver.resolve(reference).await;
            }
        }
        self.default.resolve(reference).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_resolver_passes_through() {
        let r = LiteralSecretsResolver;
        let v = r.resolve("postgres://u:p@h/db").await.unwrap();
        assert_eq!(v, "postgres://u:p@h/db");
    }

    #[tokio::test]
    async fn env_resolver_reads_named_env_var() {
        // Workspace lints `unsafe_code = forbid`, so we can't mutate
        // env vars in tests. Read one that's guaranteed to be set on
        // every platform we run CI on instead.
        let key = if std::env::var("PATH").is_ok() {
            "PATH"
        } else {
            "USER"
        };
        let r = EnvSecretsResolver;
        let v = r.resolve(&format!("env://{key}")).await.unwrap();
        assert!(!v.is_empty(), "{key} should be a non-empty env var");
    }

    #[tokio::test]
    async fn env_resolver_rejects_missing_prefix() {
        let r = EnvSecretsResolver;
        let err = r.resolve("FOO").await.unwrap_err();
        assert!(matches!(err, SecretsError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn env_resolver_rejects_empty_var_name() {
        let r = EnvSecretsResolver;
        let err = r.resolve("env://").await.unwrap_err();
        assert!(matches!(err, SecretsError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn env_resolver_returns_not_found_for_unset_var() {
        let r = EnvSecretsResolver;
        let err = r
            .resolve("env://RUSTANGO_TENANCY_DEFINITELY_NOT_SET_xyzzy")
            .await
            .unwrap_err();
        assert!(matches!(err, SecretsError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn chain_dispatches_by_scheme_prefix() {
        let chain = ChainSecretsResolver::standard();

        // env:// path goes to EnvSecretsResolver — read PATH which
        // is set on every platform.
        let v = chain.resolve("env://PATH").await.unwrap();
        assert!(!v.is_empty());

        // Non-matching scheme falls through to literal.
        let v = chain.resolve("postgres://u:p@h/db").await.unwrap();
        assert_eq!(v, "postgres://u:p@h/db");
    }
}
