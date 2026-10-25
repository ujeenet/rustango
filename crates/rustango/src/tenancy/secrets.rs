//! Pluggable secret resolution — `Org.database_url` is a *reference*,
//! not necessarily a literal connection string.
//!
//! Default is [`ChainSecretsResolver::standard`]: `env://VAR`, sealed
//! `enc:v1:` values (see [`seal`]), and literal URLs as a fallback.
//! Users add HashiCorp Vault / AWS Secrets Manager /
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

use crate::sql::connect_diagnosis::SEALED_SCHEME;

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

/// Pass-through resolver. The reference IS the value. The fallback of
/// [`ChainSecretsResolver::standard`].
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
        // Never echo the reference: a literal URL here carries its password.
        let var = reference
            .strip_prefix("env://")
            .ok_or_else(|| SecretsError::Invalid("expected an env:// reference".into()))?;
        if var.is_empty() {
            return Err(SecretsError::Invalid(
                "env:// reference has an empty variable name".into(),
            ));
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
    /// Construct an empty chain. Use [`Self::push`] to populate.
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

    /// Standard chain: `env://` → [`EnvSecretsResolver`], `enc:` →
    /// [`SealedSecretsResolver`], everything else →
    /// [`LiteralSecretsResolver`]. The default for [`super::TenantPools`].
    #[must_use]
    pub fn standard() -> Self {
        Self::new(LiteralSecretsResolver)
            .push(ENV_SCHEME, EnvSecretsResolver)
            .push(SEALED_SCHEME, SealedSecretsResolver)
    }
}

/// A shared resolver is a resolver, so one can be handed to several pools.
#[async_trait]
impl<R: SecretsResolver + ?Sized> SecretsResolver for std::sync::Arc<R> {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        (**self).resolve(reference).await
    }
}

// ---------------- SealedSecretsResolver ----------------

const ENV_SCHEME: &str = "env://";
/// The only sealed format so far.
const SEALED_V1: &str = "enc:v1:";
/// Binds a sealed secret to this use, so an `EncryptedString` column
/// value cannot be pasted in as a tenant URL.
const SEALED_AAD: &[u8] = b"rustango:tenancy-secret:v1";

/// Seal `plaintext` as `enc:v1:<base64>` (XChaCha20-Poly1305, key from
/// `RUSTANGO_SECRET_KEY`). [`ChainSecretsResolver::standard`] opens it.
/// Use a key of 32 or more random bytes; a shorter one logs a warning.
///
/// # Errors
/// [`SecretsError::Backend`] when `RUSTANGO_SECRET_KEY` is not set.
pub fn seal(plaintext: &str) -> Result<String, SecretsError> {
    if crate::aead::key_is_short() {
        tracing::warn!(
            target: "rustango::tenancy::secrets",
            min = crate::aead::MIN_KEY_LEN,
            "RUSTANGO_SECRET_KEY is shorter than the minimum; use 32 or more random bytes"
        );
    }
    crate::aead::encrypt(plaintext.as_bytes(), SEALED_AAD)
        .map(|b64| format!("{SEALED_V1}{b64}"))
        .map_err(sealed_error)
}

/// Decrypt-only fallback key, so a rotation or rollback can still open
/// values sealed under the old key until `reseal-tenant-secrets` runs.
pub const PREVIOUS_KEY_VAR: &str = "RUSTANGO_SECRET_KEY_PREVIOUS";

/// Open an `enc:` reference made by [`seal`]. In the standard chain.
/// Falls back to `RUSTANGO_SECRET_KEY_PREVIOUS` when the current key fails.
pub struct SealedSecretsResolver;

#[async_trait]
impl SecretsResolver for SealedSecretsResolver {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        let current = std::env::var(crate::aead::KEY_VAR).ok();
        let previous = std::env::var(PREVIOUS_KEY_VAR).ok();
        open_sealed(reference, current.as_deref(), previous.as_deref())
    }
}

fn open_sealed(
    reference: &str,
    current: Option<&str>,
    previous: Option<&str>,
) -> Result<String, SecretsError> {
    use crate::aead::{decrypt_with, AeadError};
    let b64 = reference.strip_prefix(SEALED_V1).ok_or_else(|| {
        SecretsError::Invalid("unsupported sealed secret; expected `enc:v1:`".into())
    })?;
    let plain = match decrypt_with(current, b64, SEALED_AAD) {
        Err(AeadError::Decrypt | AeadError::MissingKey) if previous.is_some() => {
            decrypt_with(previous, b64, SEALED_AAD)
        }
        other => other,
    }
    .map_err(sealed_error)?;
    String::from_utf8(plain).map_err(|_| SecretsError::Invalid("sealed secret is not UTF-8".into()))
}

/// How tenant database URLs are opened, and whether a password URL may
/// be stored unsealed. Owned by [`super::TenantPools`].
#[derive(Clone)]
pub struct TenantSecrets {
    resolver: std::sync::Arc<dyn SecretsResolver>,
    require_sealed: bool,
}

/// Set to `1` to refuse storing a password URL unsealed.
pub const REQUIRE_SEALED_VAR: &str = "RUSTANGO_REQUIRE_SEALED_SECRETS";

impl TenantSecrets {
    /// `resolver`, with `require_sealed` from `RUSTANGO_REQUIRE_SEALED_SECRETS`.
    #[must_use]
    pub fn new(resolver: impl SecretsResolver) -> Self {
        Self {
            resolver: std::sync::Arc::new(resolver),
            require_sealed: std::env::var(REQUIRE_SEALED_VAR).is_ok_and(|v| v == "1"),
        }
    }

    /// [`ChainSecretsResolver::standard`].
    #[must_use]
    pub fn standard() -> Self {
        Self::new(ChainSecretsResolver::standard())
    }

    /// Refuse to store a password URL that cannot be sealed.
    #[must_use]
    pub fn require_sealed(mut self) -> Self {
        self.require_sealed = true;
        self
    }

    /// Whether [`Self::require_sealed`] is on.
    #[must_use]
    pub fn requires_sealed(&self) -> bool {
        self.require_sealed
    }
}

#[async_trait]
impl SecretsResolver for TenantSecrets {
    async fn resolve(&self, reference: &str) -> Result<String, SecretsError> {
        self.resolver.resolve(reference).await
    }
}

/// Never carries the plaintext or the ciphertext.
fn sealed_error(e: crate::aead::AeadError) -> SecretsError {
    use crate::aead::AeadError;
    match e {
        AeadError::MissingKey => SecretsError::Backend(
            format!(
                "{} is not set; it is needed to seal and open `enc:` secrets",
                crate::aead::KEY_VAR
            )
            .into(),
        ),
        AeadError::Encrypt(e) => SecretsError::Backend(format!("sealing failed: {e}").into()),
        AeadError::Decode(e) => SecretsError::Invalid(format!("sealed secret is not base64: {e}")),
        AeadError::Short => SecretsError::Invalid("sealed secret is too short".into()),
        AeadError::Decrypt => SecretsError::Invalid(format!(
            "sealed secret does not open: wrong {} or a tampered value",
            crate::aead::KEY_VAR
        )),
    }
}

/// What to store for an operator-entered `reference` that resolved to
/// `resolved` under `secrets`.
///
/// A literal URL with a password is sealed, if a key is set and `secrets`
/// opens the result back to the same URL. Otherwise it is kept as given
/// with a warning, or refused when `secrets` requires sealing.
pub(crate) async fn stored_form(
    reference: &str,
    resolved: &str,
    secrets: &TenantSecrets,
) -> Result<String, String> {
    // A reference the resolver passed through unchanged is the literal itself.
    let literal_with_password =
        reference == resolved && crate::sql::connect_diagnosis::redact(reference) != reference;
    if !literal_with_password {
        return Ok(reference.to_owned());
    }
    let why = match seal(reference) {
        Ok(sealed) => match secrets.resolve(&sealed).await {
            Ok(back) if back == resolved => return Ok(sealed),
            // A custom resolver that does not know `enc:` would break the tenant.
            _ => "the secrets resolver cannot open `enc:` values".to_owned(),
        },
        Err(e) => e.to_string(),
    };
    if secrets.require_sealed {
        return Err(format!(
            "this database URL has a password and sealed storage is required, but it cannot \
             be sealed: {why}"
        ));
    }
    tracing::warn!(target: "rustango::tenancy::secrets", reason = %why, "tenant database URL stored unsealed");
    Ok(reference.to_owned())
}

/// How a stored `database_url` may be shown: an `env://` name as is,
/// anything else never.
pub(crate) fn shown_reference(stored: &str) -> String {
    if stored.starts_with(ENV_SCHEME) {
        stored.to_owned()
    } else if stored.starts_with(SEALED_SCHEME) {
        crate::sql::connect_diagnosis::ENCRYPTED_LABEL.to_owned()
    } else {
        "<literal connection URL — masked>".to_owned()
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

    fn with_key() {
        crate::aead::set_test_key();
    }

    #[tokio::test]
    async fn the_standard_chain_opens_a_sealed_secret() {
        with_key();
        let sealed = seal("postgres://u:hunter2@h/db").unwrap();
        assert!(sealed.starts_with("enc:v1:"), "{sealed}");
        assert!(!sealed.contains("hunter2"));
        let back = ChainSecretsResolver::standard()
            .resolve(&sealed)
            .await
            .unwrap();
        assert_eq!(back, "postgres://u:hunter2@h/db");
    }

    #[tokio::test]
    async fn an_unknown_sealed_version_is_refused_not_used_literally() {
        let err = ChainSecretsResolver::standard()
            .resolve("enc:v9:AAAA")
            .await
            .unwrap_err();
        assert!(matches!(err, SecretsError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_tampered_sealed_secret_does_not_open() {
        with_key();
        let mut sealed = seal("postgres://u:p@h/db").unwrap();
        let i = sealed.len() - 3;
        let flip = if &sealed[i..=i] == "A" { "B" } else { "A" };
        sealed.replace_range(i..=i, flip);
        let err = SealedSecretsResolver.resolve(&sealed).await.unwrap_err();
        assert!(err.to_string().contains("does not open"), "{err}");
    }

    #[test]
    fn a_missing_key_names_the_variable() {
        let err = sealed_error(crate::aead::AeadError::MissingKey);
        assert!(err.to_string().contains("RUSTANGO_SECRET_KEY"), "{err}");
    }

    #[tokio::test]
    async fn a_literal_with_a_password_is_stored_sealed() {
        with_key();
        let url = "postgres://u:hunter2@h/db";
        let chain = TenantSecrets::standard();
        let stored = stored_form(url, url, &chain).await.unwrap();
        assert!(stored.starts_with("enc:v1:"), "{stored}");
        assert_eq!(chain.resolve(&stored).await.unwrap(), url);
    }

    #[tokio::test]
    async fn references_and_password_free_urls_are_stored_as_given() {
        with_key();
        let chain = TenantSecrets::standard();
        for (reference, resolved) in [
            ("env://TENANT_URL", "postgres://u:p@h/db"),
            ("sqlite:///tmp/t.db?mode=rwc", "sqlite:///tmp/t.db?mode=rwc"),
            ("postgres://u@h/db", "postgres://u@h/db"),
        ] {
            assert_eq!(stored_form(reference, resolved, &chain).await.unwrap(), reference);
        }
    }

    /// A resolver without `enc:` support must not get a value it cannot open.
    #[tokio::test]
    async fn a_resolver_that_cannot_open_enc_gets_the_literal() {
        with_key();
        let url = "postgres://u:p@h/db";
        let literal = TenantSecrets::new(LiteralSecretsResolver);
        assert_eq!(stored_form(url, url, &literal).await.unwrap(), url);
    }

    /// Strict mode refuses the unsealed fallback instead of storing it.
    #[tokio::test]
    async fn strict_mode_refuses_an_unsealed_password_url() {
        with_key();
        let url = "postgres://u:hunter2@h/db";
        let strict = TenantSecrets::new(LiteralSecretsResolver).require_sealed();
        let err = stored_form(url, url, &strict).await.unwrap_err();
        assert!(err.contains("sealed storage is required"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
        let ok = TenantSecrets::standard().require_sealed();
        assert!(stored_form(url, url, &ok).await.unwrap().starts_with("enc:v1:"));
    }

    /// A value sealed under the previous key still opens after a rotation.
    #[test]
    fn the_previous_key_opens_what_the_old_key_sealed() {
        let b64 = crate::aead::encrypt_with(Some("old-key"), b"postgres://u:p@h/db", SEALED_AAD)
            .unwrap();
        let sealed = format!("{SEALED_V1}{b64}");
        assert!(open_sealed(&sealed, Some("new-key"), None).is_err());
        assert_eq!(
            open_sealed(&sealed, Some("new-key"), Some("old-key")).unwrap(),
            "postgres://u:p@h/db"
        );
        assert_eq!(open_sealed(&sealed, None, Some("old-key")).unwrap(), "postgres://u:p@h/db");
    }

    #[test]
    fn only_an_env_name_is_shown() {
        assert_eq!(shown_reference("env://X"), "env://X");
        assert_eq!(shown_reference("enc:v1:QUJD"), "<encrypted>");
        assert!(!shown_reference("postgres://u:p@h/db").contains("u:p"));
        assert!(!shown_reference("vault://secret/x").contains("vault"));
    }

    #[tokio::test]
    async fn env_resolver_rejects_missing_prefix() {
        let r = EnvSecretsResolver;
        let err = r.resolve("FOO").await.unwrap_err();
        assert!(matches!(err, SecretsError::Invalid(_)), "got {err:?}");
    }

    /// The error is logged per request: it must not carry a literal URL.
    #[tokio::test]
    async fn env_resolver_error_does_not_echo_the_reference() {
        let err = EnvSecretsResolver
            .resolve("postgres://u:hunter2@h/db")
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
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
