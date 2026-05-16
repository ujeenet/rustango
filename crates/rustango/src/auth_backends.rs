//! Pluggable authentication-backend chain — Django's
//! `AUTHENTICATION_BACKENDS = [...]` setting.
//!
//! ## The chain
//!
//! [`AuthBackendChain`] walks an ordered list of [`AuthBackend`]s,
//! returning the first non-`None` `authenticate(...)` result. Misses
//! cascade — `None` means "this backend has nothing to say about
//! these credentials"; the chain moves on. An `Err` short-circuits
//! the walk (and is returned to the caller as-is) so a transient
//! DB outage in backend N doesn't get masked by a fallthrough to
//! backend N+1.
//!
//! ```ignore
//! use std::sync::Arc;
//! use rustango::auth_backends::{
//!     AuthBackendChain, Credentials, RemoteUserBackend,
//! };
//!
//! let chain = AuthBackendChain::new()
//!     .with(Arc::new(my_password_backend))
//!     .with(Arc::new(RemoteUserBackend::trust_username));
//!
//! let creds = Credentials::password("alice", "s3cret");
//! let principal = chain.authenticate(&creds).await?;
//! ```
//!
//! ## What this module owns vs. what it doesn't
//!
//! Concrete backends in rustango (the tenant `User` table, the
//! operator console, OAuth/OIDC) keep their existing surfaces —
//! this module is a portable *registry* on top. The first wave of
//! adopters wraps an existing `authenticate_user_pool(...)` call in
//! an `AuthBackend` impl and registers it.
//!
//! ## What [`Principal`] is
//!
//! Backends authenticate users from very different stores (tenant
//! DB, header, LDAP, OAuth provider, …) returning very different
//! native records. [`Principal`] is the lowest-common-denominator
//! shape the chain hands back — `id` + `username` + boolean flags
//! + `attributes` bag. Callers map it to their own `User` type at
//! the call site if they need full record access.
//!
//! Issue #54.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ Credentials

/// One bundle of inputs handed to every backend in the chain. Most
/// backends use only one field — `RemoteUserBackend` reads
/// `remote_user`, a password backend reads `username` + `password`,
/// an OAuth callback path stashes its token in `extras`.
#[derive(Debug, Default, Clone)]
pub struct Credentials {
    /// Conventional login handle. Set by every form-based auth flow.
    pub username: Option<String>,
    /// Plain-text password to be hash-compared by the backend.
    /// Never stored, never logged.
    pub password: Option<String>,
    /// Trusted upstream user identifier (e.g. an SSO proxy's
    /// `X-Remote-User` header). Backends that consume it MUST
    /// document that they trust the framework to have validated
    /// the upstream — see [`RemoteUserBackend`].
    pub remote_user: Option<String>,
    /// Open-ended bag for backend-specific inputs (`token`,
    /// `provider`, MFA codes, etc). String-valued only — backends
    /// that need typed inputs deserialize from these strings.
    pub extras: HashMap<String, String>,
}

impl Credentials {
    /// Build a password-form credential bundle.
    #[must_use]
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Self::default()
        }
    }

    /// Build a remote-user credential bundle (typically populated by
    /// a `RemoteUserMiddleware` from a trusted upstream header).
    #[must_use]
    pub fn remote(remote_user: impl Into<String>) -> Self {
        Self {
            remote_user: Some(remote_user.into()),
            ..Self::default()
        }
    }

    /// Add an extra key-value pair (returns `self` for builder use).
    #[must_use]
    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extras.insert(key.into(), value.into());
        self
    }
}

// ------------------------------------------------------------------ Principal

/// Common return shape for every backend. Callers project this into
/// their own user type as needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// Backend-defined opaque identifier. Typically the user's
    /// numeric ID stringified, or a UUID, or the username itself
    /// when the backend has no other ID.
    pub id: String,
    /// Login handle.
    pub username: String,
    /// `true` if the user is currently active (not soft-disabled).
    pub is_active: bool,
    /// `true` if the user is a superuser (admin) on the backend.
    pub is_superuser: bool,
    /// Backend-defined identifier of the backend that authenticated
    /// this principal. Useful when the chain has multiple backends
    /// and a downstream handler wants to skip MFA for federated
    /// users, etc.
    pub backend: String,
    /// Free-form attribute bag for backend-supplied claims that
    /// don't fit the canonical fields (groups, OAuth claims, custom
    /// flags).
    pub attributes: HashMap<String, serde_json::Value>,
}

// ------------------------------------------------------------------ AuthError

#[derive(Debug)]
pub enum AuthError {
    /// Backend ran but the credentials don't match. The chain
    /// translates `Ok(None)` and this variant differently: `Ok(None)`
    /// keeps walking the chain, `Err(InvalidCredentials)` aborts.
    /// Most backends prefer `Ok(None)` so a username-not-in-this-store
    /// falls through to the next backend cleanly.
    InvalidCredentials,
    /// Backend infrastructure failure — DB outage, LDAP timeout, etc.
    /// Aborts the chain so the caller sees the underlying problem
    /// rather than a misleading "no backend recognized you".
    Backend(String),
    /// Catch-all for backend-defined errors not covered above. The
    /// chain still aborts; the message bubbles up.
    Other(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredentials => f.write_str("invalid credentials"),
            Self::Backend(msg) => write!(f, "auth backend failure: {msg}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for AuthError {}

// ------------------------------------------------------------------ AuthBackend

/// Trait every backend implements. `authenticate` returns:
///
/// - `Ok(Some(principal))` — backend accepted these credentials,
///   chain stops, principal returned to caller.
/// - `Ok(None)` — backend has nothing to say (e.g. username
///   doesn't exist in this store). Chain moves to the next backend.
/// - `Err(_)` — backend failure, chain aborts.
///
/// The async trait uses [`async_trait::async_trait`] for object
/// safety. `Box<dyn AuthBackend>` is the chain element shape.
#[async_trait::async_trait]
pub trait AuthBackend: Send + Sync {
    /// Backend-defined identifier, recorded in [`Principal::backend`].
    fn name(&self) -> &'static str;

    /// Try to authenticate the credentials.
    async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError>;

    /// Look up a principal by ID (the value [`Principal::id`] held).
    /// Defaults to `Ok(None)`; backends that support session-reload
    /// (e.g. via a session cookie's stashed `user_id`) override.
    async fn get_user(&self, _id: &str) -> Result<Option<Principal>, AuthError> {
        Ok(None)
    }
}

// ------------------------------------------------------------------ AuthBackendChain

/// Ordered list of backends. Walk via [`Self::authenticate`].
#[derive(Default, Clone)]
pub struct AuthBackendChain {
    backends: Vec<Arc<dyn AuthBackend>>,
}

impl AuthBackendChain {
    /// Empty chain — every call returns `Ok(None)` until backends are
    /// registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend at the end of the chain. Order matters —
    /// the first backend that returns `Ok(Some(_))` wins.
    #[must_use]
    pub fn with(mut self, backend: Arc<dyn AuthBackend>) -> Self {
        self.backends.push(backend);
        self
    }

    /// Number of registered backends.
    #[must_use]
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// `true` when no backends are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Walk the chain. Returns the first `Ok(Some(_))`; bails on any
    /// `Err`; returns `Ok(None)` if every backend returned
    /// `Ok(None)`.
    pub async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError> {
        for backend in &self.backends {
            match backend.authenticate(creds).await? {
                Some(principal) => return Ok(Some(principal)),
                None => continue,
            }
        }
        Ok(None)
    }

    /// Walk the chain looking up a previously authenticated principal
    /// by ID (typically from a session cookie). First non-`None`
    /// result wins; `Err` aborts.
    pub async fn get_user(&self, id: &str) -> Result<Option<Principal>, AuthError> {
        for backend in &self.backends {
            match backend.get_user(id).await? {
                Some(principal) => return Ok(Some(principal)),
                None => continue,
            }
        }
        Ok(None)
    }
}

// ------------------------------------------------------------------ RemoteUserBackend

/// Trust an upstream proxy's user-identity header (Cloudflare Access,
/// Tailscale, mod_auth_kerb, etc). The proxy is responsible for
/// authentication — this backend takes the username on faith and
/// returns a `Principal`.
///
/// **Security**: only register this backend when the deployment
/// guarantees that the `remote_user` field can only be populated by
/// the trusted upstream (typically via a middleware that strips the
/// header from un-proxied requests, or a separate listener that
/// only the proxy can reach).
pub struct RemoteUserBackend {
    /// Optional callback that decides whether to admit a remote user
    /// (e.g. to gate by group membership). Default: admit any
    /// non-empty `remote_user`.
    pub admit: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl fmt::Debug for RemoteUserBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteUserBackend").finish()
    }
}

impl Default for RemoteUserBackend {
    fn default() -> Self {
        Self::trust_username()
    }
}

impl RemoteUserBackend {
    /// Admit every non-empty `remote_user`. Sensible default for an
    /// SSO proxy that already enforces group membership upstream.
    #[must_use]
    pub fn trust_username() -> Self {
        Self {
            admit: Arc::new(|u| !u.is_empty()),
        }
    }

    /// Admit only usernames the predicate accepts. Use this to layer
    /// a local allow-list on top of the upstream's auth.
    #[must_use]
    pub fn with_predicate<F>(predicate: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        Self {
            admit: Arc::new(predicate),
        }
    }
}

#[async_trait::async_trait]
impl AuthBackend for RemoteUserBackend {
    fn name(&self) -> &'static str {
        "remote_user"
    }

    async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError> {
        let Some(username) = creds.remote_user.as_deref() else {
            return Ok(None);
        };
        if !(self.admit)(username) {
            return Ok(None);
        }
        Ok(Some(Principal {
            id: username.to_owned(),
            username: username.to_owned(),
            is_active: true,
            is_superuser: false,
            backend: "remote_user".to_owned(),
            attributes: HashMap::new(),
        }))
    }
}

// ------------------------------------------------------------------ Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper backend that returns the same Principal for every
    /// password-form input matching `(username, password)`.
    struct FixedPasswordBackend {
        username: &'static str,
        password: &'static str,
        is_superuser: bool,
    }

    #[async_trait::async_trait]
    impl AuthBackend for FixedPasswordBackend {
        fn name(&self) -> &'static str {
            "fixed_password"
        }
        async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError> {
            let (Some(u), Some(p)) = (creds.username.as_deref(), creds.password.as_deref()) else {
                return Ok(None);
            };
            if u == self.username && p == self.password {
                return Ok(Some(Principal {
                    id: "1".into(),
                    username: u.into(),
                    is_active: true,
                    is_superuser: self.is_superuser,
                    backend: "fixed_password".into(),
                    attributes: HashMap::new(),
                }));
            }
            Ok(None)
        }
        async fn get_user(&self, id: &str) -> Result<Option<Principal>, AuthError> {
            if id == "1" {
                Ok(Some(Principal {
                    id: "1".into(),
                    username: self.username.into(),
                    is_active: true,
                    is_superuser: self.is_superuser,
                    backend: "fixed_password".into(),
                    attributes: HashMap::new(),
                }))
            } else {
                Ok(None)
            }
        }
    }

    /// Backend that always errors. Used to pin the abort-on-error
    /// chain semantics.
    struct ExplodingBackend;
    #[async_trait::async_trait]
    impl AuthBackend for ExplodingBackend {
        fn name(&self) -> &'static str {
            "exploding"
        }
        async fn authenticate(&self, _: &Credentials) -> Result<Option<Principal>, AuthError> {
            Err(AuthError::Backend("simulated DB outage".into()))
        }
    }

    #[tokio::test]
    async fn empty_chain_returns_none() {
        let chain = AuthBackendChain::new();
        assert!(chain.is_empty());
        let r = chain
            .authenticate(&Credentials::password("alice", "x"))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn single_backend_match() {
        let chain = AuthBackendChain::new().with(Arc::new(FixedPasswordBackend {
            username: "alice",
            password: "s3cret",
            is_superuser: false,
        }));
        let p = chain
            .authenticate(&Credentials::password("alice", "s3cret"))
            .await
            .unwrap()
            .expect("backend should authenticate");
        assert_eq!(p.username, "alice");
        assert_eq!(p.backend, "fixed_password");
        assert!(p.is_active);
        assert!(!p.is_superuser);
    }

    #[tokio::test]
    async fn single_backend_miss_returns_none() {
        let chain = AuthBackendChain::new().with(Arc::new(FixedPasswordBackend {
            username: "alice",
            password: "s3cret",
            is_superuser: false,
        }));
        let r = chain
            .authenticate(&Credentials::password("alice", "wrong"))
            .await
            .unwrap();
        assert!(r.is_none(), "wrong password should miss");
    }

    #[tokio::test]
    async fn chain_falls_through_to_next_backend_on_none() {
        let chain = AuthBackendChain::new()
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }))
            .with(Arc::new(FixedPasswordBackend {
                username: "bob",
                password: "hunter2",
                is_superuser: true,
            }));
        let p = chain
            .authenticate(&Credentials::password("bob", "hunter2"))
            .await
            .unwrap()
            .expect("second backend should authenticate bob");
        assert_eq!(p.username, "bob");
        assert!(p.is_superuser);
    }

    #[tokio::test]
    async fn first_backend_wins_when_both_match() {
        // Both backends accept the same creds; the FIRST one in the
        // chain returns its principal — order matters.
        let chain = AuthBackendChain::new()
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }))
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: true, // would have been superuser via 2nd
            }));
        let p = chain
            .authenticate(&Credentials::password("alice", "s3cret"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !p.is_superuser,
            "first backend wins; second backend's superuser flag must NOT leak"
        );
    }

    #[tokio::test]
    async fn err_short_circuits_chain() {
        let chain = AuthBackendChain::new()
            .with(Arc::new(ExplodingBackend))
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }));
        let err = chain
            .authenticate(&Credentials::password("alice", "s3cret"))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Backend(_)));
    }

    #[tokio::test]
    async fn get_user_walks_chain() {
        let chain = AuthBackendChain::new().with(Arc::new(FixedPasswordBackend {
            username: "alice",
            password: "s3cret",
            is_superuser: false,
        }));
        let p = chain.get_user("1").await.unwrap().unwrap();
        assert_eq!(p.username, "alice");
        // Default impl returns None — so an ID the backend doesn't
        // recognize falls through cleanly.
        let r = chain.get_user("999").await.unwrap();
        assert!(r.is_none());
    }

    // ---------- RemoteUserBackend ----------

    #[tokio::test]
    async fn remote_user_backend_authenticates_header() {
        let chain = AuthBackendChain::new().with(Arc::new(RemoteUserBackend::trust_username()));
        let p = chain
            .authenticate(&Credentials::remote("alice"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.username, "alice");
        assert_eq!(p.backend, "remote_user");
        assert_eq!(p.id, "alice");
    }

    #[tokio::test]
    async fn remote_user_backend_ignores_empty_header() {
        let chain = AuthBackendChain::new().with(Arc::new(RemoteUserBackend::trust_username()));
        // Empty remote_user — admit predicate returns false → None.
        let r = chain.authenticate(&Credentials::remote("")).await.unwrap();
        assert!(r.is_none());
        // No remote_user at all (password-form creds) — still None.
        let r = chain
            .authenticate(&Credentials::password("alice", "x"))
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn remote_user_backend_with_allowlist_predicate() {
        let chain =
            AuthBackendChain::new().with(Arc::new(RemoteUserBackend::with_predicate(|u| {
                u == "alice"
            })));
        assert!(chain
            .authenticate(&Credentials::remote("alice"))
            .await
            .unwrap()
            .is_some());
        // Bob is upstream-authenticated but our allow-list rejects.
        assert!(chain
            .authenticate(&Credentials::remote("bob"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn remote_user_falls_through_to_password_backend() {
        // Common deployment: SSO header for proxied requests, password
        // form for local admin access. Order them remote-first so SSO
        // wins when present.
        let chain = AuthBackendChain::new()
            .with(Arc::new(RemoteUserBackend::trust_username()))
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }));

        // Direct request with password — falls through remote backend
        // (no header) into the password one.
        let p = chain
            .authenticate(&Credentials::password("alice", "s3cret"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.backend, "fixed_password");

        // Proxied request with header — remote backend wins, password
        // backend never consulted.
        let p = chain
            .authenticate(&Credentials::remote("alice"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.backend, "remote_user");
    }

    // ---------- Credentials ergonomics ----------

    #[test]
    fn credentials_with_extra_chains() {
        let c = Credentials::password("alice", "x")
            .with_extra("mfa_code", "123456")
            .with_extra("device_id", "iphone");
        assert_eq!(c.username.as_deref(), Some("alice"));
        assert_eq!(c.extras.get("mfa_code").map(String::as_str), Some("123456"));
        assert_eq!(
            c.extras.get("device_id").map(String::as_str),
            Some("iphone")
        );
    }

    #[test]
    fn auth_error_displays_each_variant() {
        assert_eq!(
            format!("{}", AuthError::InvalidCredentials),
            "invalid credentials"
        );
        assert_eq!(
            format!("{}", AuthError::Backend("oops".into())),
            "auth backend failure: oops"
        );
        assert_eq!(format!("{}", AuthError::Other("custom".into())), "custom");
    }
}
