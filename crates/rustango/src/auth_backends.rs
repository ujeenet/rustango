//! A chain of authentication backends, like Django's
//! `AUTHENTICATION_BACKENDS`.
//!
//! ## The chain
//!
//! [`AuthBackendChain`] walks an ordered list of [`AuthBackend`]s and
//! returns the first `Some` result. `None` means "this backend knows
//! nothing about these credentials", so the chain moves on. An `Err`
//! stops the walk and goes back to the caller, so a DB outage in one
//! backend is never hidden by a fallthrough to the next.
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
//! ## Scope
//!
//! This module is only the registry. The concrete backends, such as
//! the tenant `User` table, the operator console and OAuth, keep
//! their own APIs; you wrap one in an `AuthBackend` impl and
//! register it.
//!
//! [`Principal`] is the small shared shape the chain returns,
//! because each backend has its own native record. It carries an
//! id, a username, two flags and an attribute bag. Map it to your
//! own user type when you need the full record.
//!
//! [`Principal`]: crate::auth_backends::Principal
//! [`AuthBackend`]: crate::auth_backends::AuthBackend
//! [`AuthBackendChain`]: crate::auth_backends::AuthBackendChain

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ Credentials

/// The inputs handed to every backend in the chain. Most backends
/// read one field: a password backend reads `username` and
/// `password`, [`RemoteUserBackend`] reads `remote_user`, an OAuth
/// callback puts its token in `extras`.
#[derive(Debug, Default, Clone)]
pub struct Credentials {
    /// Login handle, set by any form-based flow.
    pub username: Option<String>,
    /// The plain-text password, for the backend to compare against
    /// a hash. **Never store it and never log it.**
    pub password: Option<String>,
    /// A user id from a trusted upstream, such as an SSO proxy's
    /// `X-Remote-User` header. It is only as trustworthy as the
    /// thing that filled it in; see [`RemoteUserBackend`].
    pub remote_user: Option<String>,
    /// Anything else a backend needs, such as a token, a provider
    /// name or an MFA code. Strings only; parse them yourself.
    pub extras: HashMap<String, String>,
}

impl Credentials {
    /// Credentials from a login form.
    #[must_use]
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Self::default()
        }
    }

    /// Credentials from a trusted upstream header. Only build these
    /// where you know the header came from your proxy.
    #[must_use]
    pub fn remote(remote_user: impl Into<String>) -> Self {
        Self {
            remote_user: Some(remote_user.into()),
            ..Self::default()
        }
    }

    /// Add one entry to `extras`.
    #[must_use]
    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extras.insert(key.into(), value.into());
        self
    }
}

// ------------------------------------------------------------------ Principal

/// What every backend returns. Map it to your own user type when
/// you need more.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// Opaque id chosen by the backend: often the numeric user id as
    /// a string, a UUID, or the username when there is nothing else.
    pub id: String,
    /// Login handle.
    pub username: String,
    /// `true` when the account is not disabled.
    pub is_active: bool,
    /// `true` when the account is an admin on that backend.
    pub is_superuser: bool,
    /// [`AuthBackend::name`] of the backend that authenticated this
    /// principal. A handler can branch on it, for example to skip
    /// MFA for a federated login.
    pub backend: String,
    /// Extra claims that do not fit the fields above, such as
    /// groups or OAuth claims.
    pub attributes: HashMap<String, serde_json::Value>,
}

// ------------------------------------------------------------------ AuthError

#[derive(Debug)]
pub enum AuthError {
    /// The backend ran and the credentials did not match. This
    /// stops the chain. Prefer `Ok(None)`, which lets an unknown
    /// username fall through to the next backend.
    InvalidCredentials,
    /// The backend itself failed: a DB outage, an LDAP timeout. It
    /// stops the chain so the caller sees the real problem instead
    /// of "no backend recognised you".
    Backend(String),
    /// Any other backend error. It also stops the chain.
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

/// One authentication source. `authenticate` returns:
///
/// - `Ok(Some(principal))`: accepted, and the chain stops.
/// - `Ok(None)`: nothing to say, so the chain tries the next one.
/// - `Err(_)`: the backend failed, and the chain stops.
///
/// Return `Ok(None)` for a wrong password too, not an error, so the
/// response cannot tell a caller which store holds the account.
#[async_trait::async_trait]
pub trait AuthBackend: Send + Sync {
    /// Short name for this backend, copied into
    /// [`Principal::backend`].
    fn name(&self) -> &'static str;

    /// Try to authenticate these credentials.
    async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError>;

    /// Look a principal up by its [`Principal::id`], for reloading a
    /// user from a session. Override it if your backend can; the
    /// default returns `Ok(None)`.
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
    /// An empty chain. Every call returns `Ok(None)` until you add
    /// a backend, so nobody can log in.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a backend to the end of the chain. Order matters: the
    /// first backend that accepts wins.
    #[must_use]
    pub fn with(mut self, backend: Arc<dyn AuthBackend>) -> Self {
        self.backends.push(backend);
        self
    }

    /// How many backends are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// `true` when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// Walk the chain and return the first backend that accepts.
    /// `Ok(None)` means no backend did; an `Err` stops the walk.
    pub async fn authenticate(&self, creds: &Credentials) -> Result<Option<Principal>, AuthError> {
        for backend in &self.backends {
            match backend.authenticate(creds).await? {
                Some(principal) => return Ok(Some(principal)),
                None => continue,
            }
        }
        Ok(None)
    }

    /// Walk the chain to reload a principal by id, usually one held
    /// in a session. The first hit wins; an `Err` stops the walk.
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

/// Trust the user-identity header of an upstream proxy, such as
/// Cloudflare Access or Tailscale. The proxy does the
/// authentication; this backend takes the name on faith.
///
/// **Security: this backend performs no check of its own.** Anyone
/// who can set `remote_user` becomes that user. Register it only
/// when the deployment guarantees the header comes from your proxy:
/// strip the header from every un-proxied request in middleware, or
/// listen on an address only the proxy can reach.
pub struct RemoteUserBackend {
    /// Decides which remote users to admit, for example by group.
    /// The default admits any non-empty name.
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
    /// Admit any non-empty name. Use it when the SSO proxy already
    /// enforces who may reach the app.
    #[must_use]
    pub fn trust_username() -> Self {
        Self {
            admit: Arc::new(|u| !u.is_empty()),
        }
    }

    /// Admit only the names the predicate accepts, which adds a
    /// local allow-list on top of the upstream's check.
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

    /// Accepts one fixed `(username, password)` pair.
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

    /// Always errors, to pin the stop-on-error rule.
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
        // Both accept the same credentials, so order decides.
        let chain = AuthBackendChain::new()
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }))
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: true, // must not reach the result
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
        // An unknown id falls through to `None`.
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
        // Empty name: the admit predicate says no.
        let r = chain.authenticate(&Credentials::remote("")).await.unwrap();
        assert!(r.is_none());
        // No remote_user at all: also none.
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
        // Bob passed upstream, but our allow-list rejects him.
        assert!(chain
            .authenticate(&Credentials::remote("bob"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn remote_user_falls_through_to_password_backend() {
        // A common setup: SSO header for proxied requests, password
        // form for local admin. Remote first, so SSO wins.
        let chain = AuthBackendChain::new()
            .with(Arc::new(RemoteUserBackend::trust_username()))
            .with(Arc::new(FixedPasswordBackend {
                username: "alice",
                password: "s3cret",
                is_superuser: false,
            }));

        // No header, so this falls through to the password backend.
        let p = chain
            .authenticate(&Credentials::password("alice", "s3cret"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.backend, "fixed_password");

        // With the header, the remote backend wins.
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
