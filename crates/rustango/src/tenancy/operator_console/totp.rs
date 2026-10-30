//! Optional TOTP second factor for operators.
//!
//! After the password, an enrolled operator gets only a short-lived pending
//! cookie, signed under its own tag, and `/login/totp` asks for the code.
//! `require_session` never reads that cookie, so it opens no console page.
//! `RUSTANGO_OPERATOR_REQUIRE_TOTP=1` makes every operator enroll there
//! before the first session.

use axum::body::Body;
use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::{IntoResponse as _, Redirect};
use axum::Extension;
use cookie::time::Duration as CookieDuration;
use cookie::{Cookie, SameSite};
use serde::Deserialize;
use tera::Context;

use super::{
    emit_registry_audit, flash, inject_op_brand, render, sanitize_next, start_session,
    urlencoding_lite, ConsoleState,
};
use crate::admin::totp_store::Stored;
use crate::session::PendingSecondFactor as Pending;
use crate::tenancy::{auth, operator_totp};
use crate::totp::TotpSecret;

const PENDING_COOKIE: &str = "rustango_op_totp";
/// Domain tag: a pending cookie is never a valid MAC of a session.
const TAG: &[u8] = b"rustango-op-totp-v1.";
/// `1`, `true`, `yes` or `on`: every operator needs a second factor.
const REQUIRE_VAR: &str = "RUSTANGO_OPERATOR_REQUIRE_TOTP";
const LOG: &str = "rustango::tenancy::operator_console";

/// Whether an operator without a device may sign in on the password alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Optional,
    Required,
}

/// The console's TOTP settings, and the once-per-process table check.
pub(super) struct Gate {
    policy: Policy,
    /// Set once `rustango_operator_totp` exists. Without it an optional
    /// policy signs in on the password (no device can exist); a read error
    /// on a table that exists fails closed.
    ensured: tokio::sync::OnceCell<()>,
}

impl Gate {
    pub(super) fn from_env() -> Self {
        let on = std::env::var(REQUIRE_VAR).is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
        Self {
            policy: if on {
                Policy::Required
            } else {
                Policy::Optional
            },
            ensured: tokio::sync::OnceCell::new(),
        }
    }

    async fn ensure(&self, registry: &crate::sql::Pool) {
        let made = self
            .ensured
            .get_or_try_init(|| operator_totp::ensure_table(registry))
            .await;
        if let Err(e) = made {
            tracing::warn!(target: LOG, error = %e, "cannot create rustango_operator_totp");
        }
    }
}

/// What a sign-in owes once the password matched.
pub(super) enum Owed {
    Nothing,
    /// A code for this confirmed device.
    Code(Stored),
    /// No device, under a required policy: enroll one first.
    Enrollment,
}

fn oid(op: &auth::Operator) -> i64 {
    op.id.get().copied().unwrap_or_default()
}

pub(super) async fn owed(
    state: &ConsoleState,
    op: &auth::Operator,
) -> Result<Owed, crate::sql::ExecError> {
    state.totp.owed(&state.registry, oid(op)).await
}

impl Gate {
    async fn owed(
        &self,
        registry: &crate::sql::Pool,
        operator_id: i64,
    ) -> Result<Owed, crate::sql::ExecError> {
        self.ensure(registry).await;
        let device = match operator_totp::confirmed(registry, operator_id).await {
            Ok(device) => device,
            // A table that does not exist holds no device, so an optional policy
            // signs in on the password; any other read error fails closed.
            Err(e) if self.policy == Policy::Optional && table_is_missing(registry).await => {
                tracing::warn!(target: LOG, error = %e, "rustango_operator_totp is missing; run `migrate`");
                None
            }
            Err(e) => return Err(e),
        };
        Ok(match device {
            Some(device) => Owed::Code(device),
            None if self.policy == Policy::Required => Owed::Enrollment,
            None => Owed::Nothing,
        })
    }
}

/// `rustango_operator_totp` provably absent; a failed probe is not "absent".
async fn table_is_missing(registry: &crate::sql::Pool) -> bool {
    use crate::core::Model as _;
    matches!(
        crate::migrate::try_table_exists_here(registry, operator_totp::OperatorTotp::SCHEMA.table)
            .await,
        Ok(false)
    )
}

fn pending_cookie(value: &str, max_age: i64) -> HeaderValue {
    let cookie = Cookie::build((PENDING_COOKIE, value.to_owned()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(crate::session::secure_cookies())
        .max_age(CookieDuration::seconds(max_age))
        .build();
    HeaderValue::from_str(&cookie.to_string()).expect("cookie is ascii")
}

fn code_url(next: &str) -> String {
    format!("/login/totp?next={}", urlencoding_lite(next))
}

/// Key for the rehash sealed in the pending cookie, apart from the session MAC.
fn upgrade_key(state: &ConsoleState) -> String {
    let key = crate::session::sign(&state.session_secret, b"rustango-op-totp-rehash-v1");
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key)
}

/// The password matched: set the pending cookie and ask for the code.
/// A rehash rides along encrypted; `code_submit` stores it.
pub(super) fn challenge(
    state: &ConsoleState,
    op: &auth::PasswordVerified<auth::Operator>,
    next: &str,
) -> Response<Body> {
    let mut pending = Pending::new(&state.session_secret, oid(op), &op.password_hash);
    pending.upgrade = op.upgrade().and_then(|new| {
        crate::aead::encrypt_with(Some(&upgrade_key(state)), new.as_bytes(), TAG).ok()
    });
    let mut resp = Redirect::to(&code_url(next)).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        pending_cookie(&pending.seal(&state.session_secret, TAG), Pending::TTL_SECS),
    );
    resp
}

/// The active operator behind a valid pending cookie, whose password has
/// not changed since, with the rehash the password step carried.
async fn pending_operator(
    state: &ConsoleState,
    headers: &HeaderMap,
) -> Option<auth::PasswordVerified<auth::Operator>> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;
    let pending = crate::cookies::cookie_from_headers(headers, PENDING_COOKIE)
        .and_then(|v| Pending::open(&state.session_secret, TAG, v))?;
    let op = auth::Operator::objects()
        .where_(auth::Operator::id.eq(pending.uid))
        .fetch(&state.registry)
        .await
        .ok()?
        .into_iter()
        .next()?;
    let upgrade = pending.upgrade.as_deref().and_then(|sealed| {
        let new = crate::aead::decrypt_with(Some(&upgrade_key(state)), sealed, TAG).ok()?;
        String::from_utf8(new).ok()
    });
    (op.active
        && pending
            .pwf
            .matches(&state.session_secret, &op.password_hash))
    .then(|| auth::PasswordVerified::resume(op, upgrade))
}

fn expired(next: &str) -> Response<Body> {
    flash::redirect(
        &format!("/login?next={}", urlencoding_lite(next)),
        flash::Flash::Error("Your sign-in expired. Please sign in again."),
    )
}

fn otpauth(state: &ConsoleState, op: &auth::Operator, secret: &TotpSecret) -> String {
    crate::totp::otpauth_url(&state.op_brand.name, &op.username, secret)
}

/// The secret an enrollment shows: the one in progress, else a new one.
async fn enrollment_secret(
    state: &ConsoleState,
    op: &auth::Operator,
) -> Result<TotpSecret, &'static str> {
    if crate::aead::key_status() == crate::aead::KeyStatus::Missing {
        return Err("RUSTANGO_SECRET_KEY is not set, so a second factor cannot be stored.");
    }
    state.totp.ensure(&state.registry).await;
    match operator_totp::pending(&state.registry, oid(op)).await {
        Ok(Some(s)) => return Ok(s.secret),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(target: LOG, error = %e, "TOTP enrollment lookup");
            return Err("Could not start the enrollment. Please try again.");
        }
    }
    let secret = TotpSecret::generate();
    match operator_totp::start(&state.registry, oid(op), &secret).await {
        Ok(()) => Ok(secret),
        Err(e) => {
            tracing::warn!(target: LOG, error = %e, "TOTP enrollment start");
            Err("Could not start the enrollment. Please try again.")
        }
    }
}

fn no_store(mut resp: Response<Body>) -> Response<Body> {
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    resp
}

/// Every code entered here is a failed one or not; never the code itself.
async fn audit(state: &ConsoleState, op: &auth::Operator, verb: &str, step: &str) {
    let mut detail = serde_json::Map::new();
    detail.insert("step".into(), serde_json::json!(step));
    emit_registry_audit(
        &state.registry,
        "rustango_operators",
        &oid(op).to_string(),
        oid(op),
        verb,
        detail,
    )
    .await;
}

/// Settle the login gate after a code check: only a wrong, non-empty code counts.
async fn settle(attempt: crate::login_throttle::LoginAttempt, ok: bool, code: &str) {
    match (ok, code.is_empty()) {
        (true, _) => attempt.succeeded().await,
        (false, false) => attempt.failed().await,
        (false, true) => attempt.prompted().await,
    }
}

// ----------------------------- /login/totp

#[derive(Deserialize)]
pub(super) struct NextQuery {
    #[serde(default)]
    next: Option<String>,
}

pub(super) async fn code_form(
    State(state): State<ConsoleState>,
    headers: HeaderMap,
    Query(q): Query<NextQuery>,
    messages: Option<Extension<flash::Messages>>,
) -> Response<Body> {
    let next = sanitize_next(q.next.as_deref());
    let Some(op) = pending_operator(&state, &headers).await else {
        return expired(&next);
    };
    let messages = messages.map(|Extension(m)| m).unwrap_or_default();
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("next", &next);
    ctx.insert("error", &messages.error);
    match owed(&state, &op).await {
        Ok(Owed::Code(_)) => ctx.insert("enroll", &false),
        Ok(Owed::Enrollment) => {
            ctx.insert("enroll", &true);
            match enrollment_secret(&state, &op).await {
                Ok(secret) => {
                    ctx.insert("secret_base32", &secret.to_base32());
                    ctx.insert("otpauth_url", &otpauth(&state, &op, &secret));
                }
                Err(msg) => ctx.insert("error", &Some(msg)),
            }
        }
        // Unenrolled meanwhile, under an optional policy: start over.
        Ok(Owed::Nothing) => return expired(&next),
        Err(e) => {
            tracing::error!(target: LOG, error = %e, "cannot read the operator's TOTP device");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "login temporarily unavailable",
            )
                .into_response();
        }
    }
    no_store(render(&state, "op_login_totp.html", &ctx))
}

#[derive(Deserialize)]
pub(super) struct CodeInput {
    #[serde(default)]
    totp_code: Option<String>,
    #[serde(default)]
    next: Option<String>,
}

pub(super) async fn code_submit(
    State(state): State<ConsoleState>,
    ip: crate::login_throttle::ClientIp,
    extensions: axum::http::Extensions,
    headers: HeaderMap,
    Form(form): Form<CodeInput>,
) -> Response<Body> {
    use crate::login_throttle::LoginScope;
    use crate::signals::auth::{
        meta_from_parts, send_user_login_failed, AuthFailureReason, UserLoginFailedContext,
    };
    let next = sanitize_next(form.next.as_deref());
    let Some(op) = pending_operator(&state, &headers).await else {
        return expired(&next);
    };
    // The same lock and limits as the password step.
    let attempt = match crate::login_throttle::shared()
        .begin(&LoginScope::Operator, &ip, &op.username)
        .await
    {
        Ok(a) => a,
        Err(refused) => return refused.into_response(),
    };
    let code = form.totp_code.as_deref().unwrap_or("").trim();
    let (accepted, enrolled) = match owed(&state, &op).await {
        Ok(Owed::Code(device)) => {
            let ok = !code.is_empty()
                && operator_totp::redeem(&state.registry, oid(&op), &device, code)
                    .await
                    .unwrap_or(false);
            (ok, false)
        }
        Ok(Owed::Enrollment) => {
            let pending = operator_totp::pending(&state.registry, oid(&op))
                .await
                .ok()
                .flatten();
            let ok = match (&pending, code.is_empty()) {
                (Some(s), false) => operator_totp::confirm(&state.registry, oid(&op), s, code)
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            (ok, true)
        }
        Ok(Owed::Nothing) => {
            drop(attempt);
            return expired(&next);
        }
        Err(e) => {
            tracing::error!(target: LOG, error = %e, "cannot read the operator's TOTP device");
            drop(attempt);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "login temporarily unavailable",
            )
                .into_response();
        }
    };
    let meta = meta_from_parts(&extensions, &headers, Some("/login/totp"));
    settle(attempt, accepted, code).await;
    if !accepted {
        if !code.is_empty() {
            audit(&state, &op, "totp_failed", "login").await;
            send_user_login_failed(UserLoginFailedContext {
                source: "operator",
                attempted_username: Some(op.username.clone()),
                reason: AuthFailureReason::InvalidCredentials,
                request: meta,
                // Operators live in the registry, outside any tenant.
                tenant: None,
            })
            .await;
        }
        let message = if code.is_empty() {
            "Enter the 6-digit code from your authenticator app."
        } else {
            "That code was not accepted: it is wrong or was already used. Enter the current code from your authenticator app."
        };
        return flash::redirect(&code_url(&next), flash::Flash::Error(message));
    }
    if enrolled {
        audit(&state, &op, "totp_enroll", "login").await;
    }
    let op = op.complete(&state.registry).await;
    let mut resp = start_session(&state, &op, &next, meta).await;
    resp.headers_mut()
        .append(header::SET_COOKIE, pending_cookie("", 0));
    resp
}

// ----------------------------- /account/totp

fn account_page(
    state: &ConsoleState,
    op: &auth::Operator,
    enabled: bool,
    secret: Option<&TotpSecret>,
    error: Option<&str>,
    success: Option<&str>,
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "totp");
    ctx.insert("operator_username", &op.username);
    ctx.insert("enabled", &enabled);
    ctx.insert("required", &(state.totp.policy == Policy::Required));
    ctx.insert("secret_base32", &secret.map(TotpSecret::to_base32));
    ctx.insert("otpauth_url", &secret.map(|s| otpauth(state, op, s)));
    ctx.insert("error", &error);
    ctx.insert("success", &success);
    // Without a key the "Set it up" link leads back here; say what to do instead.
    ctx.insert(
        "key_missing",
        &(crate::aead::key_status() == crate::aead::KeyStatus::Missing),
    );
    no_store(render(state, "op_totp.html", &ctx))
}

/// `GET /account/totp`: the device's state, or a new enrollment.
pub(super) async fn account_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    state.totp.ensure(&state.registry).await;
    match operator_totp::confirmed(&state.registry, oid(&op)).await {
        Ok(Some(_)) => account_page(&state, &op, true, None, None, None),
        Ok(None) => match enrollment_secret(&state, &op).await {
            Ok(secret) => account_page(&state, &op, false, Some(&secret), None, None),
            Err(msg) => account_page(&state, &op, false, None, Some(msg), None),
        },
        Err(e) => super::server_error("operator_console::totp", &e),
    }
}

#[derive(Deserialize)]
pub(super) struct AccountInput {
    #[serde(default)]
    totp_code: Option<String>,
}

async fn begin(
    op: &auth::Operator,
    ip: &crate::login_throttle::ClientIp,
) -> Result<crate::login_throttle::LoginAttempt, Response<Body>> {
    crate::login_throttle::shared()
        .begin(
            &crate::login_throttle::LoginScope::Operator,
            ip,
            &op.username,
        )
        .await
        .map_err(crate::login_throttle::LoginRefused::into_response)
}

/// `POST /account/totp`: confirm the enrollment with a code.
pub(super) async fn account_confirm(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    ip: crate::login_throttle::ClientIp,
    Form(form): Form<AccountInput>,
) -> Response<Body> {
    let attempt = match begin(&op, &ip).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let code = form.totp_code.as_deref().unwrap_or("").trim();
    let Ok(Some(pending)) = operator_totp::pending(&state.registry, oid(&op)).await else {
        attempt.prompted().await;
        return account_form(State(state), Extension(op)).await;
    };
    let ok = !code.is_empty()
        && operator_totp::confirm(&state.registry, oid(&op), &pending, code)
            .await
            .unwrap_or(false);
    settle(attempt, ok, code).await;
    if !ok {
        if !code.is_empty() {
            audit(&state, &op, "totp_failed", "enroll").await;
        }
        return account_page(
            &state,
            &op,
            false,
            Some(&pending.secret),
            Some("That code didn't match. Try again."),
            None,
        );
    }
    audit(&state, &op, "totp_enroll", "account").await;
    account_page(
        &state,
        &op,
        true,
        None,
        None,
        Some("Two-factor sign-in is now on."),
    )
}

/// `POST /account/totp/disable`: a current code turns the device off.
pub(super) async fn account_disable(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    ip: crate::login_throttle::ClientIp,
    Form(form): Form<AccountInput>,
) -> Response<Body> {
    let attempt = match begin(&op, &ip).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let code = form.totp_code.as_deref().unwrap_or("").trim();
    let device = match operator_totp::confirmed(&state.registry, oid(&op)).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            attempt.prompted().await;
            return account_form(State(state), Extension(op)).await;
        }
        Err(e) => {
            drop(attempt);
            return super::server_error("operator_console::totp", &e);
        }
    };
    let ok = !code.is_empty()
        && operator_totp::redeem(&state.registry, oid(&op), &device, code)
            .await
            .unwrap_or(false);
    settle(attempt, ok, code).await;
    if !ok {
        if !code.is_empty() {
            audit(&state, &op, "totp_failed", "disable").await;
        }
        return account_page(
            &state,
            &op,
            true,
            None,
            Some("Enter a current code from your authenticator app to turn it off."),
            None,
        );
    }
    match operator_totp::remove(&state.registry, oid(&op), &device).await {
        Ok(true) => {}
        Ok(false) => {
            return account_page(
                &state,
                &op,
                true,
                None,
                Some("Your device changed meanwhile. Please try again."),
                None,
            )
        }
        Err(e) => return super::server_error("operator_console::totp", &e),
    }
    audit(&state, &op, "totp_disable", "account").await;
    account_page(
        &state,
        &op,
        false,
        None,
        None,
        Some("Two-factor sign-in is off."),
    )
}

#[cfg(all(test, feature = "testkit"))]
mod missing_table_tests {
    use super::*;
    use crate::sql::Pool;
    use crate::testkit::matrix::OwnDatabase;

    /// A gate whose table step already ran, so the scenario's table state stands.
    fn gate(policy: Policy) -> Gate {
        Gate {
            policy,
            ensured: tokio::sync::OnceCell::new_with(Some(())),
        }
    }

    /// Each scenario opens a database of its own, which has no table yet.
    async fn no_table(_: &Pool) {}

    async fn optional_policy_without_the_table_signs_in_on_the_password(pool: &Pool) {
        let db = OwnDatabase::on(pool).await;
        let owed = gate(Policy::Optional).owed(&db.pool, 1).await;
        assert!(matches!(owed, Ok(Owed::Nothing)), "{:?}", owed.err());
    }

    async fn required_policy_without_the_table_is_refused(pool: &Pool) {
        let db = OwnDatabase::on(pool).await;
        assert!(gate(Policy::Required).owed(&db.pool, 1).await.is_err());
    }

    /// The table exists but the device row does not decode: refused.
    async fn an_unreadable_table_is_refused(pool: &Pool) {
        use crate::sql::UpdaterPool as _;
        crate::aead::set_test_key();
        let db = OwnDatabase::on(pool).await;
        let pool = &db.pool;
        operator_totp::ensure_table(pool).await.unwrap();
        operator_totp::start(pool, 1, &TotpSecret::generate())
            .await
            .unwrap();
        operator_totp::OperatorTotp::objects()
            .filter("operator_id", 1_i64)
            .update()
            .set("secret", "not-sealed".to_owned())
            .set("confirmed", true)
            .execute_pool(pool)
            .await
            .unwrap();
        let owed = gate(Policy::Optional).owed(pool, 1).await;
        assert!(owed.is_err(), "an unreadable device read as no device");
    }

    crate::tri_dialect_test!(
        setup: no_table,
        scenarios: [
            optional_policy_without_the_table_signs_in_on_the_password,
            required_policy_without_the_table_is_refused,
            an_unreadable_table_is_refused,
        ],
    );
}
