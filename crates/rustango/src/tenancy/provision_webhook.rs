//! Creating a tenant from an inbound webhook (#1323).
//!
//! A billing system fires `checkout.completed`; a tenant appears. The
//! alternative today is that somebody notices and runs a CLI verb.
//!
//! Note the direction. [`crate::webhook_delivery`] is **outbound** —
//! rustango POSTs signed payloads to subscribers. This is the opposite
//! way round, and it is the more dangerous one: it accepts input from
//! outside and creates infrastructure from it.
//!
//! ## May the caller name the database?
//!
//! **No, by default.** This is the decision worth arguing about, so
//! here is the argument.
//!
//! Accepting `database_url` from the payload means anyone holding the
//! signing secret can point a tenant at *any host the app can reach* —
//! including an attacker's, which turns a leaked secret into a
//! credential-exfiltration primitive (the app connects out and
//! authenticates), and including internal hosts a request from outside
//! should never touch. A billing system has no business knowing your
//! database topology anyway.
//!
//! So the default [`UrlPolicy::Template`] derives the URL from a
//! configured pattern, substituting only the slug, and the caller
//! supplies nothing but a name. [`UrlPolicy::CallerSupplied`] exists
//! for the deployment that genuinely orchestrates its own databases —
//! and a caller who sends a `database_url` under the default policy is
//! **rejected**, not silently ignored: quietly dropping a field the
//! caller believed in is how a tenant ends up on the wrong database.
//!
//! ## Accept and hand off
//!
//! Provisioning takes tens of seconds — well past any webhook
//! provider's timeout — so the endpoint verifies, opens a run, spawns
//! the work and returns `202` with the run id. The run is persisted
//! (#1321), so the outcome is durable and readable from the operator
//! console even though the task itself is in-process. A pod that dies
//! mid-run leaves the run in `running`, which is exactly the state
//! that table was designed to make visible.
//!
//! ## What this deliberately does not do
//!
//! **Rate limiting.** The crate already ships rate-limiting middleware;
//! layering it on this route is the caller's call, and a second
//! limiter baked in here would be one they cannot configure. A leaked
//! secret should not be an unbounded tenant-creation faucet — say so
//! in the deployment docs and layer the middleware.
//!
//! **The completion callback.** [`crate::webhook_delivery`] needs a job
//! queue handle, and reaching into the caller's would be guessing at
//! their setup. The run is pollable and streamable in the meantime.
//!
//! [`UrlPolicy::Template`]: crate::tenancy::provision_webhook::UrlPolicy::Template
//! [`UrlPolicy::CallerSupplied`]: crate::tenancy::provision_webhook::UrlPolicy::CallerSupplied

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::webhook::{verify_signature, SignatureFormat};

use super::org::{BackendKind, StorageMode};
use super::preflight::Preflight;
use super::provision::{DatabaseSource, ProvisionRequest, TenantProvisioner, TenantStorage};
use super::provision_store::{self as store, RunState};

/// Where a new tenant's database URL comes from.
#[derive(Debug, Clone)]
pub enum UrlPolicy {
    /// Derive it, substituting `{slug}` in the template.
    ///
    /// ```text
    /// postgres://app:secret@db.internal:5432/tenant_{slug}
    /// ```
    ///
    /// The default, and the safe one: the caller names a tenant, not a
    /// host. See the module docs.
    Template(String),

    /// Take `database_url` from the payload.
    ///
    /// Only for a deployment that really does orchestrate its own
    /// databases per tenant. Understand what it grants before using
    /// it: whoever holds the signing secret chooses what the app
    /// connects to.
    CallerSupplied,

    /// Schema-mode tenants: no separate database to name.
    SchemaMode,

    /// Create the database, named as the slug, with its own login on
    /// this saved [`DbServer`](crate::tenancy::DbServer).
    Server(String),
}

impl Default for UrlPolicy {
    fn default() -> Self {
        // No sensible template can be guessed, so the default refuses
        // rather than inventing one. A deployment must say where
        // tenant databases live.
        Self::Template(String::new())
    }
}

/// An HMAC key of at least [`WebhookSecret::MIN_LEN`] bytes, so an unset
/// env var read as `""` can't become a key anyone can sign with (#1850).
#[derive(Clone)]
pub struct WebhookSecret(Arc<Vec<u8>>);

impl WebhookSecret {
    /// Shortest key accepted: the HMAC-SHA256 output size.
    pub const MIN_LEN: usize = 32;

    /// `None` when `bytes` is shorter than [`Self::MIN_LEN`].
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Option<Self> {
        let bytes = bytes.into();
        (bytes.len() >= Self::MIN_LEN).then(|| Self(Arc::new(bytes)))
    }
}

impl AsRef<[u8]> for WebhookSecret {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// How the endpoint authenticates and what it is allowed to create.
#[derive(Clone)]
pub struct WebhookConfig {
    /// HMAC key. Shared with the caller out of band.
    pub secret: WebhookSecret,
    /// Which signature encoding the caller sends.
    pub format: SignatureFormat,
    /// Header carrying the signature. Providers disagree
    /// (`X-Hub-Signature-256`, `Stripe-Signature`, …), so it is
    /// configurable rather than assumed.
    pub signature_header: String,
    pub url_policy: UrlPolicy,
    /// How far out of date a payload's `timestamp` may be.
    ///
    /// A signature alone is replayable forever — capture one delivery
    /// and you can re-send it indefinitely. Five minutes is the
    /// industry-conventional window and accommodates ordinary clock
    /// skew.
    pub timestamp_tolerance: Duration,
    pub storage_mode: StorageMode,
    /// `None` takes the registry's; anything else must equal it.
    pub backend: Option<BackendKind>,
    /// A `running` run with no event for this long is taken as dead, so
    /// a retry of its event starts over (#1883).
    pub stale_run_after: Duration,
}

impl WebhookConfig {
    /// The safe shape: derive URLs from `template`, reject anything the
    /// caller tries to say about hosts.
    ///
    /// # Panics
    /// If `secret` is shorter than [`WebhookSecret::MIN_LEN`] bytes.
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>, database_url_template: impl Into<String>) -> Self {
        let secret = WebhookSecret::new(secret).unwrap_or_else(|| {
            panic!(
                "WebhookConfig::new: the secret must be at least {} bytes",
                WebhookSecret::MIN_LEN
            )
        });
        Self {
            secret,
            format: SignatureFormat::HexSha256WithPrefix,
            signature_header: "x-signature-256".to_owned(),
            url_policy: UrlPolicy::Template(database_url_template.into()),
            timestamp_tolerance: Duration::from_secs(300),
            storage_mode: StorageMode::Database,
            backend: None,
            stale_run_after: store::STALE_RUN_AFTER,
        }
    }
}

/// Everything the endpoint needs, as axum state.
#[derive(Clone)]
pub struct WebhookState {
    pub config: WebhookConfig,
    pub provisioner: Arc<dyn TenantProvisioner>,
}

/// The payload a caller signs and sends.
#[derive(Debug, serde::Deserialize)]
pub struct ProvisionPayload {
    /// Idempotency key. Every provider retries; this is what makes two
    /// deliveries of one event produce one tenant.
    pub event_id: String,
    /// Unix seconds. Checked against
    /// [`WebhookConfig::timestamp_tolerance`].
    pub timestamp: i64,
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub host_pattern: Option<String>,
    /// Only honoured under [`UrlPolicy::CallerSupplied`]; **rejected**
    /// under any other policy rather than ignored.
    #[serde(default)]
    pub database_url: Option<String>,
    /// A first admin, mailed a set-password link. Refused unless the
    /// provisioner mails links: nobody here could pass one on.
    #[serde(default)]
    pub admin_email: Option<String>,
    /// Defaults to `admin_email`.
    #[serde(default)]
    pub admin_username: Option<String>,
}

/// What the caller gets back.
#[derive(Debug, serde::Serialize)]
pub struct Accepted {
    pub run_id: i64,
    pub slug: String,
    pub state: String,
    /// True when this delivery matched an existing `event_id` and no
    /// new work was started.
    pub duplicate: bool,
}

/// Why a delivery was refused.
#[derive(Debug)]
enum Refusal {
    BadSignature,
    Stale,
    Malformed(String),
    Policy(String),
    Internal(String),
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            // Deliberately the same opaque answer for a bad signature
            // and a missing one: an attacker probing the endpoint
            // learns nothing about which part they got wrong.
            Self::BadSignature => (StatusCode::UNAUTHORIZED, "invalid signature".to_owned()),
            Self::Stale => (
                StatusCode::UNAUTHORIZED,
                "payload timestamp is outside the accepted window".to_owned(),
            ),
            Self::Malformed(m) => (StatusCode::BAD_REQUEST, m),
            Self::Policy(m) => (StatusCode::FORBIDDEN, m),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        crate::api_errors::ApiError::logged(code, "provision_webhook", message).into_response()
    }
}

/// `POST` handler: verify, de-duplicate, hand off, answer.
///
/// Takes `Bytes`, not `Json<ProvisionPayload>` — **the signature must
/// be checked over the exact bytes received**. A handler that
/// deserializes first and re-serializes to verify is checking a
/// different document from the one that was signed, which is a
/// forgery hole wide enough to drive through.
pub async fn provision_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match handle(&state, &headers, &body).await {
        Ok(accepted) => {
            let code = if accepted.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (code, Json(accepted)).into_response()
        }
        Err(refusal) => refusal.into_response(),
    }
}

async fn handle(
    state: &WebhookState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Accepted, Refusal> {
    // ---- 1. Signature, over the raw bytes, before anything else ----
    let Some(signature) = headers
        .get(state.config.signature_header.as_str())
        .and_then(|v| v.to_str().ok())
    else {
        return Err(Refusal::BadSignature);
    };
    if !verify_signature(
        state.config.format,
        state.config.secret.as_ref(),
        body,
        signature,
    ) {
        return Err(Refusal::BadSignature);
    }

    // ---- 2. Only now is the body worth parsing ----
    let payload: ProvisionPayload = serde_json::from_slice(body)
        .map_err(|e| Refusal::Malformed(format!("payload is not the expected JSON: {e}")))?;

    // ---- 3. Replay window ----
    let age = chrono::Utc::now().timestamp() - payload.timestamp;
    let tolerance = i64::try_from(state.config.timestamp_tolerance.as_secs()).unwrap_or(i64::MAX);
    // Both directions: a timestamp far in the *future* is as much a
    // sign of a forged or misconfigured sender as a stale one.
    if age.abs() > tolerance {
        return Err(Refusal::Stale);
    }

    // ---- 4. Idempotency ----
    //
    // Checked here for the common case (a retry arriving after the
    // first finished). The race — two deliveries at once, both
    // passing this check — is settled by the unique constraint on
    // `idempotency_key` when the second tries to open its run.
    let registry = state.provisioner.registry();
    if let Some(existing) = store::run_by_idempotency_key(&registry, &payload.event_id)
        .await
        .map_err(|e| Refusal::Internal(e.to_string()))?
    {
        // A failed or interrupted run starts over under the same key (#1883).
        let retry = store::release_for_retry(&registry, &existing, state.config.stale_run_after)
            .await
            .map_err(|e| Refusal::Internal(e.to_string()))?;
        if !retry {
            return Ok(Accepted {
                run_id: existing.id.get().copied().unwrap_or_default(),
                slug: existing.slug,
                state: existing.state,
                duplicate: true,
            });
        }
    }

    // ---- 5. Build the request under the URL policy ----
    let request = build_request(&state.config, &payload)?;
    if request.first_admin.is_some()
        && !state
            .provisioner
            .first_admin_handover()
            .is_some_and(super::first_admin::FirstAdminHandover::mails)
    {
        return Err(Refusal::Policy(
            "this endpoint cannot mail a first admin's link; remove `admin_email`".to_owned(),
        ));
    }

    // ---- 6. Open the run, then hand off ----
    //
    // The run is opened *synchronously* so the caller gets an id it
    // can poll, and so the idempotency constraint fires now rather
    // than inside a spawned task where nobody would see it.
    let run = match store::open_run(
        &registry,
        &request.slug,
        request.mode().as_str(),
        request.backend_on(&registry).as_str(),
        request.database_url(),
        Some("webhook"),
        Some(&payload.event_id),
    )
    .await
    {
        Ok(run) => run,
        Err(e) => {
            // Almost certainly the unique key: a twin delivery won the race.
            // Re-read rather than guess, so a real failure stays a 500.
            let existing = store::run_by_idempotency_key(&registry, &payload.event_id)
                .await
                .ok()
                .flatten();
            return match existing {
                Some(existing) => Ok(Accepted {
                    run_id: existing.id.get().copied().unwrap_or_default(),
                    slug: existing.slug,
                    state: existing.state,
                    duplicate: true,
                }),
                None => Err(Refusal::Internal(format!(
                    "could not open a provisioning run: {e}"
                ))),
            };
        }
    };
    let run_id = run.id.get().copied().unwrap_or_default();

    let provisioner = Arc::clone(&state.provisioner);
    let slug = request.slug.clone();
    tokio::spawn(async move {
        // Into the run opened above — `provision_in_run`, not
        // `provision`, or the task would open a second run for the
        // same tenant and the caller's id would point at a run that
        // never progresses.
        match provisioner.provision_in_run(run_id, &request).await {
            Ok(mut outcome) => {
                if let Some(handover) = provisioner.first_admin_handover() {
                    let handed = handover
                        .hand_over_outcome(&provisioner.registry(), &mut outcome, None)
                        .await;
                    let mailed = outcome
                        .first_admin
                        .as_ref()
                        .is_none_or(|a| matches!(a.handover, Some(super::first_admin::Handover::Mailed { .. })));
                    // Never the link: name the verb that issues a new one.
                    if handed.is_err() || !mailed {
                        tracing::warn!(
                            target: "rustango::tenancy::provision_webhook",
                            slug = %request.slug,
                            run_id,
                            "the first admin's link was not mailed; issue one with `set-password-link`"
                        );
                        if let Some(admin) = &outcome.first_admin {
                            let note = store::RunText::handover_lost(&outcome.slug, &admin.username);
                            let noted = store::append_note(
                                &provisioner.registry(),
                                run_id,
                                super::provision::ProvisionStep::CreateAdmin.as_str(),
                                "failed",
                                &note,
                            )
                            .await;
                            if let Err(e) = noted {
                                tracing::warn!(target: "rustango::tenancy::provision_webhook", run_id, error = %e, "could not record the lost handover");
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // The run itself already records this; the log line is for
                // an operator tailing the app, who has no reason to be
                // watching the console.
                tracing::warn!(
                    target: "rustango::tenancy::provision_webhook",
                    slug = %request.slug,
                    run_id,
                    error = %e,
                    "webhook-initiated provisioning failed"
                );
            }
        }
    });

    Ok(Accepted {
        run_id,
        slug,
        state: RunState::Running.as_str().to_owned(),
        duplicate: false,
    })
}

/// Apply the URL policy, and refuse anything it does not allow.
fn build_request(
    config: &WebhookConfig,
    payload: &ProvisionPayload,
) -> Result<ProvisionRequest, Refusal> {
    if payload.slug.trim().is_empty() {
        return Err(Refusal::Malformed("`slug` must not be empty".to_owned()));
    }
    // A slug becomes a database name, a schema name and a subdomain
    // label. Anything outside this set is either a quoting problem
    // waiting to happen or a hostname that cannot exist.
    if !payload
        .slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Refusal::Malformed(
            "`slug` may contain only lowercase letters, digits and hyphens".to_owned(),
        ));
    }

    let database_url = match (&config.url_policy, &payload.database_url) {
        // Refused, not ignored: a caller who sent a URL believes it
        // was used.
        (UrlPolicy::Template(_) | UrlPolicy::SchemaMode | UrlPolicy::Server(_), Some(_)) => {
            return Err(Refusal::Policy(
                "this endpoint derives tenant database URLs itself; \
                 remove `database_url` from the payload"
                    .to_owned(),
            ));
        }
        (UrlPolicy::Template(template), None) => {
            if template.is_empty() {
                return Err(Refusal::Policy(
                    "no tenant database URL template is configured on this endpoint".to_owned(),
                ));
            }
            Some(template.replace("{slug}", &payload.slug))
        }
        (UrlPolicy::SchemaMode | UrlPolicy::Server(_), None) => None,
        (UrlPolicy::CallerSupplied, url) => {
            let Some(url) = url.as_ref().filter(|u| !u.trim().is_empty()) else {
                return Err(Refusal::Malformed(
                    "`database_url` is required by this endpoint's policy".to_owned(),
                ));
            };
            Some(url.clone())
        }
    };

    let schema = matches!(config.url_policy, UrlPolicy::SchemaMode)
        || config.storage_mode == StorageMode::Schema;
    let storage = match (&config.url_policy, database_url) {
        (UrlPolicy::Server(_), _) if schema => {
            return Err(Refusal::Policy(
                "this endpoint creates schema-mode tenants, which have no database to create"
                    .to_owned(),
            ))
        }
        (UrlPolicy::Server(name), _) => TenantStorage::Database(DatabaseSource::Server(name.clone())),
        _ if schema => TenantStorage::Schema(None),
        (_, Some(url)) => TenantStorage::Database(DatabaseSource::Url(url)),
        (_, None) => {
            return Err(Refusal::Policy(
                "this endpoint creates database-mode tenants but derives no database URL"
                    .to_owned(),
            ))
        }
    };
    let first_admin = match (&payload.admin_email, &payload.admin_username) {
        (Some(email), username) => Some(
            super::first_admin::FirstAdmin::new(email, username.as_deref())
                .map_err(Refusal::Malformed)?,
        ),
        (None, Some(_)) => {
            return Err(Refusal::Malformed(
                "`admin_username` needs `admin_email`".to_owned(),
            ))
        }
        (None, None) => None,
    };
    Ok(ProvisionRequest {
        backend: config.backend,
        display_name: payload.display_name.clone(),
        host_pattern: payload.host_pattern.clone(),
        preflight: Preflight::default(),
        first_admin,
        ..ProvisionRequest::new(payload.slug.clone(), storage)
    })
}

/// Mount the endpoint at `path`.
///
/// ```ignore
/// let app = app.merge(tenancy::provision_webhook::router(
///     "/hooks/provision",
///     WebhookState { config, provisioner },
/// ));
/// ```
///
/// Layer your rate limiter on the returned router — see the module
/// docs for why one is not baked in.
pub fn router(path: &str, state: WebhookState) -> axum::Router {
    axum::Router::new()
        .route(path, axum::routing::post(provision_webhook))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(slug: &str, url: Option<&str>) -> ProvisionPayload {
        ProvisionPayload {
            event_id: "evt_1".to_owned(),
            timestamp: chrono::Utc::now().timestamp(),
            slug: slug.to_owned(),
            display_name: None,
            host_pattern: None,
            database_url: url.map(ToOwned::to_owned),
            admin_email: None,
            admin_username: None,
        }
    }

    fn config(policy: UrlPolicy) -> WebhookConfig {
        WebhookConfig {
            url_policy: policy,
            ..WebhookConfig::new([7u8; WebhookSecret::MIN_LEN].to_vec(), "")
        }
    }

    /// An unset env var read as `""` must not become a usable key (#1850).
    #[test]
    #[should_panic(expected = "at least 32 bytes")]
    fn an_empty_secret_is_refused() {
        let _ = WebhookConfig::new(Vec::new(), "");
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(WebhookSecret::new(vec![1; WebhookSecret::MIN_LEN - 1]).is_none());
        assert!(WebhookSecret::new(vec![1; WebhookSecret::MIN_LEN]).is_some());
    }

    #[test]
    fn the_template_policy_substitutes_the_slug_and_nothing_else() {
        let c = config(UrlPolicy::Template(
            "postgres://app:pw@db.internal:5432/tenant_{slug}".to_owned(),
        ));
        let r = build_request(&c, &payload("acme", None)).expect("valid");
        assert_eq!(
            r.database_url(),
            Some("postgres://app:pw@db.internal:5432/tenant_acme")
        );
    }

    /// The security decision, pinned. A caller-supplied URL under the
    /// default policy is refused — never silently dropped, because a
    /// caller who sent one believes it was used.
    #[test]
    fn a_caller_supplied_url_is_refused_under_the_template_policy() {
        let c = config(UrlPolicy::Template("postgres://h/{slug}".to_owned()));
        let err = build_request(&c, &payload("acme", Some("postgres://attacker/x")))
            .expect_err("must be refused");
        assert!(
            matches!(err, Refusal::Policy(_)),
            "should be a policy refusal, got {err:?}"
        );
    }

    #[test]
    fn the_caller_supplied_policy_accepts_one_when_deliberately_enabled() {
        let c = config(UrlPolicy::CallerSupplied);
        let r = build_request(&c, &payload("acme", Some("postgres://mine/x"))).expect("allowed");
        assert_eq!(r.database_url(), Some("postgres://mine/x"));
    }

    #[test]
    fn the_caller_supplied_policy_still_needs_a_url() {
        let c = config(UrlPolicy::CallerSupplied);
        assert!(matches!(
            build_request(&c, &payload("acme", None)),
            Err(Refusal::Malformed(_))
        ));
    }

    /// An unconfigured template refuses rather than inventing a host.
    #[test]
    fn an_empty_template_is_a_configuration_refusal() {
        let c = config(UrlPolicy::Template(String::new()));
        assert!(matches!(
            build_request(&c, &payload("acme", None)),
            Err(Refusal::Policy(_))
        ));
    }

    /// A slug becomes a database name, a schema name and a subdomain
    /// label. Everything outside the safe set is refused at the door.
    #[test]
    fn a_slug_is_restricted_to_what_is_safe_everywhere_it_is_used() {
        let c = config(UrlPolicy::Template("postgres://h/{slug}".to_owned()));
        for bad in [
            "Acme",        // uppercase — not a valid hostname label
            "ac me",       // space
            "acme;DROP",   // punctuation
            "acme/../etc", // path traversal
            "acme'",       // quote
            "",            // empty
        ] {
            assert!(
                build_request(&c, &payload(bad, None)).is_err(),
                "slug `{bad}` should have been refused"
            );
        }
        assert!(build_request(&c, &payload("acme-2", None)).is_ok());
    }

    /// Signup creates the database itself on a saved server; the caller
    /// still names only the tenant.
    #[test]
    fn the_server_policy_creates_the_database_on_a_saved_server() {
        let c = config(UrlPolicy::Server("main".to_owned()));
        let r = build_request(&c, &payload("acme", None)).expect("valid");
        assert_eq!(r.server(), Some("main"));
        assert_eq!(r.database_url(), None);
        assert!(matches!(
            build_request(&c, &payload("acme", Some("postgres://attacker/x"))),
            Err(Refusal::Policy(_))
        ));
        let schema = WebhookConfig {
            storage_mode: StorageMode::Schema,
            ..config(UrlPolicy::Server("main".to_owned()))
        };
        assert!(matches!(build_request(&schema, &payload("acme", None)), Err(Refusal::Policy(_))));
    }

    #[test]
    fn schema_mode_takes_no_url_at_all() {
        let c = config(UrlPolicy::SchemaMode);
        let r = build_request(&c, &payload("acme", None)).expect("valid");
        assert_eq!(r.database_url(), None);
    }
}
