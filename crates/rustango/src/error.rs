//! Top-level `RustangoError` — single error type for app-level handlers.
//!
//! Each module ships its own granular error type (`ExecError`, `MigrateError`,
//! `CacheError`, `JwtIssueError`, etc.). Those are the right shape inside the
//! framework where you want to know *which* layer failed.
//!
//! At the **handler boundary**, you usually don't care — you want one
//! `?`-friendly type to bubble all of them up to `IntoResponse`. That's
//! what `RustangoError` is for.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::{RustangoError, RustangoResult};
//! use axum::Json;
//!
//! async fn handler() -> RustangoResult<Json<Post>> {
//!     let post = Post::objects().get(&pool, 1).await?;     // ExecError → RustangoError::Sql
//!     let cached = cache.get("k").await?;                  // CacheError → RustangoError::Cache
//!     let pair = jwt.issue_pair_with(uid, claims)?;        // JwtIssueError → RustangoError::JwtIssue
//!     Ok(Json(post))
//! }
//! ```
//!
//! No manual `From` impls in your code — every framework error type already
//! has the conversion.
//!
//! ## When to use it vs. granular errors
//!
//! - **Library / module code:** use the granular per-module error
//!   (`Result<T, CacheError>`). Exposes the right surface for callers.
//! - **HTTP handlers / request lifecycle:** use `RustangoError`. The
//!   `IntoResponse` impl maps each variant to a sensible HTTP status code,
//!   and the `?` operator does the conversion automatically.
//! - **Mixing:** `RustangoError::other(msg)` and `RustangoError::other_from(e)`
//!   wrap arbitrary `std::error::Error + Send + Sync + 'static` types when
//!   you've got a third-party crate's error to bubble up.

use std::fmt;

/// Fixed body text for a 5xx whose real cause must not be published.
///
/// The 5xx family's only callers are the ViewSet (`admin` /
/// `tenancy`) and `into_response` (`admin`), so a build without
/// either has none. Compiled anyway — rather than `#[cfg]`-ed away —
/// so widening a caller's gate cannot turn this into a missing-item
/// error; same shape as `extractors::tenant::TenantConnCell::Deferred`.
/// Ungated, these tripped `-D warnings` on the bare `sqlite`,
/// `postgres` and `mysql` rows of `feature_combos`, a gated job that
/// does not run on an unlabelled PR (#1604 review, dialects-004).
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) const OPAQUE_SERVER_ERROR: &str = "internal server error";

/// Env var that opts a deployment **in** to publishing error detail.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) const DISCLOSE_ENV: &str = "RUSTANGO_DISCLOSE_ERRORS";

/// `true` when a 5xx response body may carry the underlying error text.
///
/// **Defaults to `false`.** Only an explicit truthy
/// `RUSTANGO_DISCLOSE_ERRORS` turns disclosure on.
///
/// This deliberately does *not* reuse
/// [`crate::template_debug::enabled`], which answers a different
/// question — "should I render the dev error overlay" — and defaults
/// to *on* outside prod, which is right for a local overlay and wrong
/// for what an unauthenticated client is handed. Routing the 5xx body
/// through that tier made the sanitiser inert on every deployment that
/// had not set `RUSTANGO_ENV=prod`, which is the default, and the
/// original tests hid it by pinning the variable (#1525).
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) fn disclose_server_errors() -> bool {
    matches!(
        std::env::var(DISCLOSE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `true` when the current process should serve the dev template-error
/// overlay: `RUSTANGO_TEMPLATE_DEBUG` if set, else `RUSTANGO_ENV` is
/// not prod.
///
/// Held here rather than in `template_debug` because that module is
/// gated on `_tera` while the decision is about env vars alone.
/// **Not** the 5xx-body decision — see [`disclose_server_errors`].
#[cfg_attr(
    not(any(feature = "admin", feature = "tenancy", feature = "_tera")),
    allow(dead_code)
)]
pub(crate) fn debug_details_enabled() -> bool {
    if let Ok(raw) = std::env::var("RUSTANGO_TEMPLATE_DEBUG") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            // Any other value — ignore and fall through to env-tier.
            _ => {}
        }
    }
    let env = std::env::var("RUSTANGO_ENV").unwrap_or_default();
    !matches!(
        env.trim().to_ascii_lowercase().as_str(),
        "prod" | "production"
    )
}

/// Shared harness for tests that mutate the tier env vars. Lives
/// here, ungated, because `error` and `template_debug` both read
/// them and cargo builds the lib tests as one binary — a second
/// lock in the other module would serialize nothing.
#[cfg(test)]
pub(crate) mod test_env {
    /// Suite-wide lock. Env is process-global, so every test that
    /// sets `RUSTANGO_ENV` / `RUSTANGO_TEMPLATE_DEBUG` takes it.
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Set `key` for the duration of `f`, then restore it. Edition
    /// 2021 still permits bare `set_var`/`remove_var`; the workspace
    /// `unsafe_code = "forbid"` lint blocks the edition-2024 unsafe
    /// form, so these stay bare.
    pub(crate) fn with<F: FnOnce()>(key: &str, val: Option<&str>, f: F) {
        let prev = std::env::var(key).ok();
        match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

/// Body text for a 5xx: always log `e`, publish it only on a debug
/// tier.
///
/// Driver errors carry table names, constraint names, column lists
/// and often the database host. Returning `e.to_string()` to an
/// unauthenticated client hands all of that over on any 500 (#1525).
/// The operator still gets the full text — from the log, which is
/// where it was always meant to be read.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) fn server_error_body(context: &str, e: &dyn fmt::Display) -> String {
    tracing::error!(target: "rustango::error", context, error = %e, "server error");
    if disclose_server_errors() {
        e.to_string()
    } else {
        OPAQUE_SERVER_ERROR.to_owned()
    }
}

/// Body text for a client-caused 4xx.
///
/// Same withholding rule, but logged at `warn` and attributed to the
/// caller: a client can drive these at will, and an `ERROR` line per
/// bad request is a log-volume lever pointed at the operator (#1604
/// review, performance-003).
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) fn client_error_body(context: &str, e: &dyn fmt::Display) -> String {
    tracing::warn!(target: "rustango::error", context, error = %e, "rejected request");
    if disclose_server_errors() {
        e.to_string()
    } else {
        OPAQUE_CLIENT_ERROR.to_owned()
    }
}

/// Fixed body text for a 4xx. Distinct from [`OPAQUE_SERVER_ERROR`]
/// because "internal server error" on a 400 is simply false.
#[cfg_attr(not(any(feature = "admin", feature = "tenancy")), allow(dead_code))]
pub(crate) const OPAQUE_CLIENT_ERROR: &str = "request rejected";

// ------------------------------------------------------------------ enum

/// Unified error type for app-level code. `From` impls cover every module
/// in the framework so `?` Just Works in handlers.
#[derive(Debug)]
#[non_exhaustive]
pub enum RustangoError {
    /// SQL execution / driver error.
    Sql(crate::sql::ExecError),

    /// Migration runner / file / diff error.
    Migrate(crate::migrate::MigrateError),

    /// Form parsing / validation error.
    #[cfg(feature = "forms")]
    Forms(crate::forms::FormErrors),

    /// Cache backend error.
    #[cfg(feature = "cache")]
    Cache(crate::cache::CacheError),

    /// Email send failure.
    #[cfg(feature = "email")]
    Mail(crate::email::MailError),

    /// File storage error.
    #[cfg(feature = "storage")]
    Storage(crate::storage::StorageError),

    /// Auth backend / login failure.
    #[cfg(feature = "tenancy")]
    Auth(crate::tenancy::auth_backends::AuthError),

    /// JWT issuance error (reserved-claim conflict, malformed payload).
    #[cfg(feature = "tenancy")]
    JwtIssue(crate::tenancy::jwt_lifecycle::JwtIssueError),

    /// Generic password helper error.
    #[cfg(feature = "passwords")]
    Password(crate::passwords::PasswordError),

    /// API-key generation / verification error.
    #[cfg(feature = "api_keys")]
    ApiKey(crate::api_keys::ApiKeyError),

    /// Signed-URL parse / verify error.
    #[cfg(feature = "signed_url")]
    SignedUrl(crate::signed_url::SignedUrlError),

    /// Pre-built auth-flow error (password reset, email verify, magic link).
    #[cfg(feature = "auth_flows")]
    AuthFlow(crate::auth_flows::AuthFlowError),

    /// Bulk-action runner error.
    #[cfg(feature = "tenancy")]
    BulkAction(crate::bulk_actions::BulkActionError),

    /// IP filter parse error (invalid CIDR).
    #[cfg(feature = "admin")]
    IpFilter(crate::ip_filter::IpFilterError),

    /// Job queue error.
    #[cfg(feature = "jobs")]
    Job(crate::jobs::JobError),

    /// Test fixture loader error.
    Fixture(crate::fixtures::FixtureError),

    /// i18n translation loader error.
    I18n(crate::i18n::I18nError),

    /// Env-variable read / parse error.
    Env(crate::env::EnvError),

    /// Secrets backend error.
    #[cfg(feature = "secrets")]
    Secrets(crate::secrets::SecretsError),

    /// I/O (file / network / etc.).
    Io(std::io::Error),

    /// JSON encode / decode.
    Serde(serde_json::Error),

    /// Catch-all for third-party errors and ad-hoc bail-outs.
    Other(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl RustangoError {
    /// Construct from any `std::error::Error + Send + Sync + 'static`.
    pub fn other_from<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Self::Other(Box::new(e))
    }

    /// Construct from a static string — for ad-hoc validation messages.
    pub fn other(msg: impl Into<String>) -> Self {
        let s: Box<dyn std::error::Error + Send + Sync + 'static> =
            Box::<dyn std::error::Error + Send + Sync + 'static>::from(msg.into());
        Self::Other(s)
    }
}

// ------------------------------------------------------------------ Display

impl fmt::Display for RustangoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "sql: {e}"),
            Self::Migrate(e) => write!(f, "migrate: {e}"),
            #[cfg(feature = "forms")]
            Self::Forms(e) => write!(f, "form: {e}"),
            #[cfg(feature = "cache")]
            Self::Cache(e) => write!(f, "cache: {e}"),
            #[cfg(feature = "email")]
            Self::Mail(e) => write!(f, "mail: {e}"),
            #[cfg(feature = "storage")]
            Self::Storage(e) => write!(f, "storage: {e}"),
            #[cfg(feature = "tenancy")]
            Self::Auth(e) => write!(f, "auth: {e}"),
            #[cfg(feature = "tenancy")]
            Self::JwtIssue(e) => write!(f, "jwt: {e}"),
            #[cfg(feature = "passwords")]
            Self::Password(e) => write!(f, "password: {e}"),
            #[cfg(feature = "api_keys")]
            Self::ApiKey(e) => write!(f, "api_key: {e}"),
            #[cfg(feature = "signed_url")]
            Self::SignedUrl(e) => write!(f, "signed_url: {e}"),
            #[cfg(feature = "auth_flows")]
            Self::AuthFlow(e) => write!(f, "auth_flow: {e}"),
            #[cfg(feature = "tenancy")]
            Self::BulkAction(e) => write!(f, "bulk_action: {e}"),
            #[cfg(feature = "admin")]
            Self::IpFilter(e) => write!(f, "ip_filter: {e}"),
            #[cfg(feature = "jobs")]
            Self::Job(e) => write!(f, "job: {e}"),
            Self::Fixture(e) => write!(f, "fixture: {e}"),
            Self::I18n(e) => write!(f, "i18n: {e}"),
            Self::Env(e) => write!(f, "env: {e}"),
            #[cfg(feature = "secrets")]
            Self::Secrets(e) => write!(f, "secrets: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Serde(e) => write!(f, "serde: {e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RustangoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(e) => Some(e),
            Self::Migrate(e) => Some(e),
            #[cfg(feature = "forms")]
            Self::Forms(e) => Some(e),
            #[cfg(feature = "cache")]
            Self::Cache(e) => Some(e),
            #[cfg(feature = "email")]
            Self::Mail(e) => Some(e),
            #[cfg(feature = "storage")]
            Self::Storage(e) => Some(e),
            #[cfg(feature = "tenancy")]
            Self::Auth(e) => Some(e),
            #[cfg(feature = "tenancy")]
            Self::JwtIssue(e) => Some(e),
            #[cfg(feature = "passwords")]
            Self::Password(e) => Some(e),
            #[cfg(feature = "api_keys")]
            Self::ApiKey(e) => Some(e),
            #[cfg(feature = "signed_url")]
            Self::SignedUrl(e) => Some(e),
            #[cfg(feature = "auth_flows")]
            Self::AuthFlow(e) => Some(e),
            #[cfg(feature = "tenancy")]
            Self::BulkAction(e) => Some(e),
            #[cfg(feature = "admin")]
            Self::IpFilter(e) => Some(e),
            #[cfg(feature = "jobs")]
            Self::Job(e) => Some(e),
            Self::Fixture(e) => Some(e),
            Self::I18n(e) => Some(e),
            Self::Env(e) => Some(e),
            #[cfg(feature = "secrets")]
            Self::Secrets(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Serde(e) => Some(e),
            Self::Other(e) => Some(&**e),
        }
    }
}

/// Standard alias.
pub type RustangoResult<T> = Result<T, RustangoError>;

// ------------------------------------------------------------------ From impls

impl From<crate::sql::ExecError> for RustangoError {
    fn from(e: crate::sql::ExecError) -> Self {
        Self::Sql(e)
    }
}

impl From<crate::migrate::MigrateError> for RustangoError {
    fn from(e: crate::migrate::MigrateError) -> Self {
        Self::Migrate(e)
    }
}

#[cfg(feature = "forms")]
impl From<crate::forms::FormErrors> for RustangoError {
    fn from(e: crate::forms::FormErrors) -> Self {
        Self::Forms(e)
    }
}

#[cfg(feature = "cache")]
impl From<crate::cache::CacheError> for RustangoError {
    fn from(e: crate::cache::CacheError) -> Self {
        Self::Cache(e)
    }
}

#[cfg(feature = "email")]
impl From<crate::email::MailError> for RustangoError {
    fn from(e: crate::email::MailError) -> Self {
        Self::Mail(e)
    }
}

#[cfg(feature = "storage")]
impl From<crate::storage::StorageError> for RustangoError {
    fn from(e: crate::storage::StorageError) -> Self {
        Self::Storage(e)
    }
}

#[cfg(feature = "tenancy")]
impl From<crate::tenancy::auth_backends::AuthError> for RustangoError {
    fn from(e: crate::tenancy::auth_backends::AuthError) -> Self {
        Self::Auth(e)
    }
}

#[cfg(feature = "tenancy")]
impl From<crate::tenancy::jwt_lifecycle::JwtIssueError> for RustangoError {
    fn from(e: crate::tenancy::jwt_lifecycle::JwtIssueError) -> Self {
        Self::JwtIssue(e)
    }
}

#[cfg(feature = "passwords")]
impl From<crate::passwords::PasswordError> for RustangoError {
    fn from(e: crate::passwords::PasswordError) -> Self {
        Self::Password(e)
    }
}

#[cfg(feature = "api_keys")]
impl From<crate::api_keys::ApiKeyError> for RustangoError {
    fn from(e: crate::api_keys::ApiKeyError) -> Self {
        Self::ApiKey(e)
    }
}

#[cfg(feature = "signed_url")]
impl From<crate::signed_url::SignedUrlError> for RustangoError {
    fn from(e: crate::signed_url::SignedUrlError) -> Self {
        Self::SignedUrl(e)
    }
}

#[cfg(feature = "auth_flows")]
impl From<crate::auth_flows::AuthFlowError> for RustangoError {
    fn from(e: crate::auth_flows::AuthFlowError) -> Self {
        Self::AuthFlow(e)
    }
}

#[cfg(feature = "tenancy")]
impl From<crate::bulk_actions::BulkActionError> for RustangoError {
    fn from(e: crate::bulk_actions::BulkActionError) -> Self {
        Self::BulkAction(e)
    }
}

#[cfg(feature = "admin")]
impl From<crate::ip_filter::IpFilterError> for RustangoError {
    fn from(e: crate::ip_filter::IpFilterError) -> Self {
        Self::IpFilter(e)
    }
}

#[cfg(feature = "jobs")]
impl From<crate::jobs::JobError> for RustangoError {
    fn from(e: crate::jobs::JobError) -> Self {
        Self::Job(e)
    }
}

impl From<crate::fixtures::FixtureError> for RustangoError {
    fn from(e: crate::fixtures::FixtureError) -> Self {
        Self::Fixture(e)
    }
}

impl From<crate::i18n::I18nError> for RustangoError {
    fn from(e: crate::i18n::I18nError) -> Self {
        Self::I18n(e)
    }
}

impl From<crate::env::EnvError> for RustangoError {
    fn from(e: crate::env::EnvError) -> Self {
        Self::Env(e)
    }
}

#[cfg(feature = "secrets")]
impl From<crate::secrets::SecretsError> for RustangoError {
    fn from(e: crate::secrets::SecretsError) -> Self {
        Self::Secrets(e)
    }
}

impl From<std::io::Error> for RustangoError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RustangoError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

// ------------------------------------------------------------------ IntoResponse

#[cfg(feature = "admin")]
mod into_response {
    use super::RustangoError;
    use crate::api_errors::ApiError;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};

    impl IntoResponse for RustangoError {
        fn into_response(self) -> Response {
            map_to_api_error(self).into_response()
        }
    }

    /// Map a `RustangoError` to a sensible HTTP status + JSON shape.
    /// Validation-style errors → 422, auth → 401, missing resource → 404,
    /// permission → 403, rate limit → 429, everything else → 500.
    fn map_to_api_error(err: RustangoError) -> ApiError {
        let msg = err.to_string();
        match err {
            // Validation: 422
            #[cfg(feature = "forms")]
            RustangoError::Forms(_) => ApiError::validation(msg),
            #[cfg(feature = "auth_flows")]
            RustangoError::AuthFlow(_) => ApiError::bad_request(msg),
            #[cfg(feature = "signed_url")]
            RustangoError::SignedUrl(_) => ApiError::bad_request(msg),

            // Auth: 401
            #[cfg(feature = "tenancy")]
            RustangoError::Auth(_) => ApiError::unauthorized(msg),
            #[cfg(feature = "tenancy")]
            RustangoError::JwtIssue(_) => ApiError::unauthorized(msg),
            #[cfg(feature = "passwords")]
            RustangoError::Password(_) => ApiError::unauthorized(msg),
            #[cfg(feature = "api_keys")]
            RustangoError::ApiKey(_) => ApiError::unauthorized(msg),

            // Bad input: 400
            RustangoError::Env(_) => ApiError::bad_request(msg),
            #[cfg(feature = "tenancy")]
            RustangoError::BulkAction(_) => ApiError::bad_request(msg),
            #[cfg(feature = "admin")]
            RustangoError::IpFilter(_) => ApiError::bad_request(msg),

            // Server-side: 500
            other => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                super::server_error_body("RustangoError", &other),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn from_io_error_works() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e: RustangoError = io.into();
        assert!(matches!(e, RustangoError::Io(_)));
        assert!(e.to_string().contains("io: "));
    }

    #[test]
    fn from_serde_error_works() {
        let res: Result<i64, _> = serde_json::from_str("not-json");
        let serde_err = res.unwrap_err();
        let e: RustangoError = serde_err.into();
        assert!(matches!(e, RustangoError::Serde(_)));
    }

    #[cfg(feature = "cache")]
    #[test]
    fn from_cache_error_works() {
        let e: RustangoError = crate::cache::CacheError::Connection("nope".into()).into();
        assert!(matches!(e, RustangoError::Cache(_)));
    }

    #[test]
    fn other_from_wraps_external() {
        #[derive(Debug)]
        struct Custom;
        impl std::fmt::Display for Custom {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "custom")
            }
        }
        impl std::error::Error for Custom {}

        let e = RustangoError::other_from(Custom);
        assert!(matches!(e, RustangoError::Other(_)));
        assert!(e.to_string().contains("custom"));
    }

    #[test]
    fn other_with_string_works() {
        let e = RustangoError::other("ad-hoc message");
        assert!(matches!(e, RustangoError::Other(_)));
        assert!(e.to_string().contains("ad-hoc message"));
    }

    #[test]
    fn source_chain_for_serde() {
        let res: Result<i64, _> = serde_json::from_str("not-json");
        let e: RustangoError = res.unwrap_err().into();
        assert!(
            e.source().is_some(),
            "RustangoError should expose source for chained errors"
        );
    }

    #[test]
    fn rustango_result_alias_works() {
        fn returns() -> RustangoResult<i32> {
            Ok(42)
        }
        assert_eq!(returns().unwrap(), 42);
    }

    #[test]
    fn question_mark_propagates_io_error() {
        fn inner() -> RustangoResult<()> {
            std::fs::read_to_string("/no/such/file/exists/at/all/promise/12345").map(|_| ())?;
            Ok(())
        }
        let r = inner();
        assert!(matches!(r, Err(RustangoError::Io(_))));
    }

    #[cfg(feature = "admin")]
    #[test]
    fn into_response_maps_io_to_500() {
        use axum::response::IntoResponse;
        let io = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        let e: RustangoError = io.into();
        let r = e.into_response();
        assert_eq!(r.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[cfg(all(feature = "admin", feature = "forms"))]
    #[test]
    fn into_response_maps_form_errors_to_422() {
        use axum::response::IntoResponse;
        let mut errs = crate::forms::FormErrors::default();
        errs.add("title", "required");
        let e: RustangoError = errs.into();
        let r = e.into_response();
        assert_eq!(r.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[cfg(all(feature = "admin", feature = "tenancy"))]
    #[test]
    fn into_response_maps_jwt_to_401() {
        use axum::response::IntoResponse;
        let jwt_err = crate::tenancy::jwt_lifecycle::JwtIssueError::ReservedClaim("sub".into());
        let e: RustangoError = jwt_err.into();
        let r = e.into_response();
        assert_eq!(r.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    /// A driver error of the shape sqlx actually produces.
    const DRIVER_ERROR: &str = "error returned from database: relation \
         \"tenant_billing_accounts\" violates unique constraint \
         \"uq_billing_stripe_customer\" (db=pg-prod-01.internal:5432)";

    /// Assert `body` carries none of the four separate disclosures in
    /// `DRIVER_ERROR`. Each is checked on its own: a single
    /// `!= DRIVER_ERROR` passes on a body that leaked only the host.
    fn assert_withholds(body: &str, what: &str) {
        for secret in [
            "tenant_billing_accounts",
            "uq_billing_stripe_customer",
            "pg-prod-01.internal",
            "5432",
        ] {
            assert!(!body.contains(secret), "{what} leaked `{secret}`: {body}");
        }
    }

    /// The test that matters: **nothing set**. The first version of
    /// this pinned `RUSTANGO_ENV=prod`, so it asserted the one
    /// configuration in which the fix was active and never exercised
    /// the default every deployment actually runs in — where the old
    /// tier answered "disclose" (#1525, found in review of #1604).
    #[test]
    fn server_error_body_withholds_by_default_with_no_env_set() {
        let _g = test_env::lock();
        test_env::with(DISCLOSE_ENV, None, || {
            test_env::with("RUSTANGO_TEMPLATE_DEBUG", None, || {
                test_env::with("RUSTANGO_ENV", None, || {
                    let body = server_error_body("test", &DRIVER_ERROR);
                    assert_withholds(&body, "default 500 body");
                    assert_eq!(body, OPAQUE_SERVER_ERROR);
                });
            });
        });
    }

    /// The dev-overlay tier must not decide this one. `RUSTANGO_ENV`
    /// unset *and* `RUSTANGO_TEMPLATE_DEBUG=1` is exactly the state
    /// that used to disclose.
    #[test]
    fn the_template_debug_tier_does_not_open_the_5xx_body() {
        let _g = test_env::lock();
        test_env::with(DISCLOSE_ENV, None, || {
            test_env::with("RUSTANGO_TEMPLATE_DEBUG", Some("1"), || {
                test_env::with("RUSTANGO_ENV", None, || {
                    assert!(
                        debug_details_enabled(),
                        "precondition: the overlay tier is on in this state",
                    );
                    let body = server_error_body("test", &DRIVER_ERROR);
                    assert_withholds(&body, "body with the overlay tier on");
                });
            });
        });
    }

    #[test]
    fn server_error_body_withholds_driver_text_in_prod() {
        let _g = test_env::lock();
        test_env::with(DISCLOSE_ENV, None, || {
            test_env::with("RUSTANGO_TEMPLATE_DEBUG", None, || {
                test_env::with("RUSTANGO_ENV", Some("prod"), || {
                    let body = server_error_body("test", &DRIVER_ERROR);
                    assert_withholds(&body, "prod 500 body");
                    assert_eq!(body, OPAQUE_SERVER_ERROR);
                });
            });
        });
    }

    #[test]
    fn a_client_caused_4xx_does_not_claim_a_server_fault() {
        let _g = test_env::lock();
        test_env::with(DISCLOSE_ENV, None, || {
            let body = client_error_body("test", &DRIVER_ERROR);
            assert_withholds(&body, "default 400 body");
            assert_eq!(body, OPAQUE_CLIENT_ERROR);
            assert!(
                !body.contains("internal server error"),
                "a 400 must not report a server fault: {body}",
            );
        });
    }

    #[test]
    fn an_explicit_opt_in_still_shows_the_cause() {
        // The control. Without it, a `server_error_body` that always
        // returned the fixed string would pass every test above while
        // making a 500 undebuggable for an operator who asked to see
        // it. Opting in is now its own variable, not a side effect of
        // the dev-overlay tier.
        let _g = test_env::lock();
        test_env::with(DISCLOSE_ENV, Some("1"), || {
            test_env::with("RUSTANGO_TEMPLATE_DEBUG", None, || {
                test_env::with("RUSTANGO_ENV", Some("prod"), || {
                    let body = server_error_body("test", &DRIVER_ERROR);
                    assert!(
                        body.contains("uq_billing_stripe_customer"),
                        "an explicit opt-in must still show the cause: {body}",
                    );
                });
            });
        });
    }
}
