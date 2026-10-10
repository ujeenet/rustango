//! Bare-admin SSO login wiring, behind the `admin-sso` feature.
//!
//! The reusable SSO core (types, `build_provider`, `verified_email`,
//! `SSO_FLOW_COOKIE`) lives in [`crate::sso`]. This module only wires
//! it to the bare admin: it builds a
//! [`ResolvedSso`](crate::sso::ResolvedSso) from an
//! [`SsoProvider`](crate::sso::SsoProvider) row, runs the handshake,
//! signs in the [`AdminUser`] linked to the IdP subject, and mints the
//! admin session cookie.
//!
//! SSO never creates an admin account or links one by email: an admin
//! adds the [`SsoLink`](crate::sso::SsoLink) row. The refusal log names the subject.
//!
//! The re-export below keeps the older `crate::admin::sso::…` paths
//! working for callers such as [`crate::tenancy::sso`].

pub use crate::sso::*;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use super::session::{self, AdminSession, SESSION_COOKIE};
use super::urls::AppState;
use super::user::AdminUser;
use crate::signals::auth::{
    meta_from_parts, send_user_logged_in, AuthFailureReason, AuthRequestMeta, UserLoggedInContext,
};
use crate::sso::link::{signal_refused, Account, AccountLookup, EmailLookup};

/// The auth-signal `source` for a bare-admin SSO sign-in.
const SIGNAL_SOURCE: &str = "admin_sso";

/// Query params on the IdP callback (`?code=…&state=…` or `?error=…`).
#[derive(serde::Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Routes for the bare-admin SSO flow, mounted next to `/login`.
/// `GET /login/sso/{slug}` starts the handshake for one configured
/// [`SsoProvider`](super::sso_provider::SsoProvider), and
/// `.../callback` completes it.
pub(crate) fn sso_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/login/sso/{slug}", get(sso_begin))
        .route("/login/sso/{slug}/callback", get(sso_callback));
    // Outside `/login/sso/`, so no provider slug is shadowed.
    #[cfg(feature = "totp")]
    let router = router.route(
        "/login/sso-totp",
        axum::routing::post(second_factor::submit),
    );
    router.with_state(state)
}

fn login_path(state: &AppState) -> String {
    let p = &state.config.admin_prefix;
    if p.is_empty() {
        "/login".to_owned()
    } else {
        format!("{p}/login")
    }
}

/// Per-provider callback URL built from the request host:
/// `{scheme}://{host}{login_path}/sso/{slug}/callback`. The scheme
/// comes from `X-Forwarded-Proto` sent by a trusted proxy, else `https`.
fn derive_bare_redirect(
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
    state: &AppState,
    slug: &str,
) -> Option<String> {
    let host = crate::urls::HostAuthority::parse(headers.get(header::HOST)?.to_str().ok()?)?;
    let scheme = crate::real_ip::trusted_forwarded(headers, extensions, "x-forwarded-proto")
        .unwrap_or("https");
    Some(format!(
        "{scheme}://{host}{}/sso/{slug}/callback",
        login_path(state)
    ))
}

/// Redirect back to the login page with a generic `?sso_error=` marker.
/// Details are logged, never shown to the user.
fn login_error(state: &AppState, code: &str) -> Response {
    Redirect::to(&format!("{}?sso_error={code}", login_path(state))).into_response()
}

fn cookie_attrs(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

// GET /login/sso/{slug}: start the handshake for one provider.
async fn sso_begin(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
) -> Response {
    let Some(secret) = state.config.session_secret.as_ref() else {
        return login_error(&state, "disabled");
    };
    let Some(redirect_uri) = derive_bare_redirect(&headers, &extensions, &state, &slug) else {
        return login_error(&state, "config");
    };
    let cfg = match super::sso_provider::resolve_by_slug(&state.pool, &slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(&state, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "begin resolve: {e}");
            return login_error(&state, "config");
        }
    };
    let provider = match build_provider(&cfg.sso).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "begin: {e}");
            return login_error(&state, "config");
        }
    };
    let (url, flow) = provider.begin();
    let sealed = seal_flow(
        &flow,
        FlowScope::new(FlowPurpose::Admin, "", &slug),
        secret.key(),
    );
    let cookie = format!(
        "{SSO_FLOW_COOKIE}={sealed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600{s}",
        s = cookie_attrs(state.config.secure_cookies),
    );
    let mut resp = Redirect::to(&url).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

// GET /login/sso/{slug}/callback: finish the handshake, link the
// account, mint the session.
async fn sso_callback(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(secret) = state.config.session_secret.as_ref() else {
        return login_error(&state, "disabled");
    };
    if params.error.is_some() {
        return login_error(&state, "denied");
    }
    let (Some(code), Some(cb_state)) = (params.code, params.state) else {
        return login_error(&state, "callback");
    };
    // Recover + verify the sealed flow from its cookie.
    let Some(sealed) = crate::cookies::cookie_from_headers(&headers, SSO_FLOW_COOKIE) else {
        return login_error(&state, "expired");
    };
    let flow = match open_flow(
        sealed,
        FlowScope::new(FlowPurpose::Admin, "", &slug),
        secret.key(),
    ) {
        Ok(f) => f,
        Err(_) => return login_error(&state, "expired"),
    };
    let Some(redirect_uri) = derive_bare_redirect(&headers, &extensions, &state, &slug) else {
        return login_error(&state, "config");
    };
    let cfg = match super::sso_provider::resolve_by_slug(&state.pool, &slug, redirect_uri).await {
        Ok(Some(c)) => c,
        Ok(None) => return login_error(&state, "disabled"),
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "callback resolve: {e}");
            return login_error(&state, "config");
        }
    };
    let provider = match build_provider(&cfg.sso).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "rustango::admin::sso", "callback build: {e}");
            return login_error(&state, "config");
        }
    };
    let normalized = match provider.complete(&flow, &code, &cb_state).await {
        Ok((u, _tokens)) => u,
        Err(e) => {
            tracing::warn!(target: "rustango::admin::sso", "handshake: {e}");
            return login_error(&state, "handshake");
        }
    };
    // Sign in by (provider, sub) link only: every admin account is staff,
    // so a verified email never creates a link here.
    let key = cfg.key(LinkSource::Admin);
    let accounts = AdminAccounts(&state.pool);
    let meta = || meta_from_parts(&extensions, &headers, Some(&format!("/login/sso/{slug}")));
    let uid =
        match crate::sso::link::sign_in(&state.pool, &key, false, &normalized, &accounts).await {
            Ok(uid) => uid,
            Err(e) => {
                tracing::warn!(
                    target: "rustango::admin::sso",
                    slug,
                    provider_id = cfg.id,
                    issuer = key.issuer(),
                    subject = %normalized.provider_user_id,
                    "sso refused: {e}"
                );
                if let Some(reason) = e.failure_reason() {
                    signal_refused(SIGNAL_SOURCE, &normalized, reason, meta()).await;
                }
                return login_error(&state, "nouser");
            }
        };
    let Ok(Some(user)) = accounts.user(uid).await else {
        return login_error(&state, "nouser");
    };
    if !user.active {
        signal_refused(
            SIGNAL_SOURCE,
            &normalized,
            AuthFailureReason::Inactive,
            meta(),
        )
        .await;
        return login_error(&state, "inactive");
    }
    // A confirmed device owes its code first, as on the password login (#2249).
    #[cfg(feature = "totp")]
    match super::totp_store::confirmed_secret_checked(&state.pool, uid).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return second_factor::prompt(&state, &extensions, &headers, secret, &user).await
        }
        Err(e) => {
            // Fail closed: an unreadable device is not "no second factor" (#1644).
            tracing::error!(target: "rustango::admin::sso", user_id = uid, error = %e, "cannot read the TOTP device");
            return login_error(&state, "config");
        }
    }
    mint_session(&state, secret, &user, meta()).await
}

/// Mint the normal admin session, bound to the user's stored password
/// hash, exactly as a successful password login does.
async fn mint_session(
    state: &AppState,
    secret: &session::AdminSessionSecret,
    user: &AdminUser,
    request: AuthRequestMeta,
) -> Response {
    let uid = user.id.get().copied().unwrap_or_default();
    let auth_hash = crate::session::PasswordFingerprint::of(secret, &user.password_hash);
    let cookie_value = session::encode(
        secret,
        AdminSession::new(uid, user.username.clone(), user.is_superuser),
        &auth_hash,
        user.sessions_revoked_at,
    );
    let session_cookie = format!(
        "{SESSION_COOKIE}={cookie_value}; Path=/; HttpOnly; SameSite=Lax{s}",
        s = cookie_attrs(state.config.secure_cookies),
    );
    let redirect_to = if state.config.admin_prefix.is_empty() {
        "/".to_owned()
    } else {
        state.config.admin_prefix.clone()
    };
    let mut resp = Redirect::to(&redirect_to).into_response();
    if let Ok(v) = HeaderValue::from_str(&session_cookie) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    clear_cookie(&mut resp, SSO_FLOW_COOKIE);
    send_user_logged_in(UserLoggedInContext {
        source: SIGNAL_SOURCE,
        user_id: uid,
        username: user.username.clone(),
        is_superuser: user.is_superuser,
        request,
    })
    .await;
    resp
}

/// Expire the transient cookie `name`.
fn clear_cookie(resp: &mut Response, name: &str) {
    if let Ok(v) = HeaderValue::from_str(&format!("{name}=; Path=/; HttpOnly; Max-Age=0")) {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

/// The TOTP step after an SSO sign-in (#2249). The IdP proved who the
/// user is; a signed, short-lived cookie carries that to the code form.
#[cfg(feature = "totp")]
mod second_factor {
    use super::*;
    use crate::session::PasswordFingerprint;
    use base64::Engine as _;

    const PENDING_COOKIE: &str = "rustango_admin_sso_totp";
    /// Domain tag: a pending cookie is never a valid MAC of a session.
    const TAG: &[u8] = b"rustango-admin-sso-totp-v1.";
    const TTL_SECS: i64 = 300;

    /// Who finished SSO and still owes a code. Bound to the password hash,
    /// so a password change in between ends it.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Pending {
        uid: i64,
        pwf: PasswordFingerprint,
        exp: i64,
    }

    fn mac(secret: &session::AdminSessionSecret, body: &str) -> [u8; 32] {
        crate::session::sign(secret, &[TAG, body.as_bytes()].concat())
    }

    fn seal(secret: &session::AdminSessionSecret, p: &Pending) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let body = b64.encode(serde_json::to_vec(p).unwrap_or_default());
        let sig = b64.encode(mac(secret, &body));
        format!("{body}.{sig}")
    }

    fn open(secret: &session::AdminSessionSecret, value: &str) -> Option<Pending> {
        use subtle::ConstantTimeEq as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let (body, sig) = value.split_once('.')?;
        let sig = b64.decode(sig).ok()?;
        if mac(secret, body).ct_eq(&sig[..]).unwrap_u8() == 0 {
            return None;
        }
        let p: Pending = serde_json::from_slice(&b64.decode(body).ok()?).ok()?;
        (chrono::Utc::now().timestamp() < p.exp).then_some(p)
    }

    /// Ask for the code: the TOTP-only login form plus the pending cookie.
    pub(super) async fn prompt(
        state: &AppState,
        extensions: &axum::http::Extensions,
        headers: &HeaderMap,
        secret: &session::AdminSessionSecret,
        user: &AdminUser,
    ) -> Response {
        let pending = Pending {
            uid: user.id.get().copied().unwrap_or_default(),
            pwf: PasswordFingerprint::of(secret, &user.password_hash),
            exp: chrono::Utc::now().timestamp() + TTL_SECS,
        };
        let cookie = format!(
            "{PENDING_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={TTL_SECS}{s}",
            seal(secret, &pending),
            s = cookie_attrs(state.config.secure_cookies),
        );
        let mut resp =
            crate::admin::login_view::sso_totp_response(state, extensions, headers, None).await;
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        clear_cookie(&mut resp, SSO_FLOW_COOKIE);
        resp
    }

    #[derive(serde::Deserialize)]
    pub(super) struct CodeInput {
        #[serde(rename = "_csrf", default)]
        csrf_token: Option<String>,
        #[serde(default)]
        totp_code: Option<String>,
    }

    // POST /login/sso-totp: redeem the code, then mint the session.
    pub(super) async fn submit(
        State(state): State<AppState>,
        ip: crate::login_throttle::ClientIp,
        extensions: axum::http::Extensions,
        headers: HeaderMap,
        axum::Form(form): axum::Form<CodeInput>,
    ) -> Response {
        use crate::login_throttle::LoginScope;
        let Some(secret) = state.config.session_secret.as_ref() else {
            return login_error(&state, "disabled");
        };
        if !crate::forms::csrf::verify_form_token(&headers, form.csrf_token.as_deref()) {
            return crate::admin::login_view::sso_totp_response(
                &state,
                &extensions,
                &headers,
                Some("Your session expired or the form was invalid. Please try again."),
            )
            .await;
        }
        let Some(pending) = crate::cookies::cookie_from_headers(&headers, PENDING_COOKIE)
            .and_then(|v| open(secret, v))
        else {
            return login_error(&state, "expired");
        };
        let Ok(Some(user)) = AdminAccounts(&state.pool).user(pending.uid).await else {
            return login_error(&state, "nouser");
        };
        if !user.active || !pending.pwf.matches(secret, &user.password_hash) {
            return login_error(&state, "expired");
        }
        // Same lock and limits as the password login's code check.
        let attempt = match crate::login_throttle::shared()
            .begin(&LoginScope::Admin, &ip, &user.username)
            .await
        {
            Ok(a) => a,
            Err(refused) => return refused.into_response(),
        };
        let device = match super::super::totp_store::confirmed_secret_checked(
            &state.pool,
            pending.uid,
        )
        .await
        {
            Ok(Some(device)) => device,
            Ok(None) | Err(_) => {
                attempt.prompted().await;
                return login_error(&state, "expired");
            }
        };
        let code = form.totp_code.as_deref().unwrap_or("").trim();
        let accepted = !code.is_empty()
            && super::super::totp_store::redeem_code(&state.pool, pending.uid, &device, code)
                .await
                .unwrap_or(false);
        if !accepted {
            if code.is_empty() {
                attempt.prompted().await;
            } else {
                attempt.failed().await;
            }
            use crate::signals::auth::{send_user_login_failed, AuthFailureReason};
            send_user_login_failed(crate::signals::auth::UserLoginFailedContext {
                source: SIGNAL_SOURCE,
                attempted_username: Some(user.username.clone()),
                reason: AuthFailureReason::InvalidCredentials,
                request: crate::signals::auth::meta_from_parts(
                    &extensions,
                    &headers,
                    Some("/login/sso-totp"),
                ),
            })
            .await;
            return crate::admin::login_view::sso_totp_response(
                &state,
                &extensions,
                &headers,
                Some("Enter the 6-digit code from your authenticator app."),
            )
            .await;
        }
        attempt.succeeded().await;
        let meta = meta_from_parts(&extensions, &headers, Some("/login/sso-totp"));
        let mut resp = mint_session(&state, secret, &user, meta).await;
        clear_cookie(&mut resp, PENDING_COOKIE);
        resp
    }
}

/// The bare admin's accounts. Every one is staff, so none links by email.
struct AdminAccounts<'a>(&'a crate::sql::Pool);

impl AdminAccounts<'_> {
    /// The admin user with this id; a driver error is an error, not "missing".
    async fn user(&self, id: i64) -> Result<Option<AdminUser>, String> {
        use crate::sql::FetcherPool as _;
        Ok(AdminUser::objects()
            .filter("id", id)
            .fetch(self.0)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .next())
    }
}

fn admin_account(u: &AdminUser) -> Option<Account> {
    Some(Account::new(u.id.get().copied()?, true, u.active))
}

impl AccountLookup for AdminAccounts<'_> {
    async fn by_id(&self, id: i64) -> Result<Option<Account>, String> {
        Ok(self.user(id).await?.as_ref().and_then(admin_account))
    }

    async fn by_email(&self, email: &str) -> Result<EmailLookup, String> {
        use crate::sql::FetcherPool as _;
        let rows = AdminUser::objects()
            .filter("email__iexact", email.to_owned())
            .fetch(self.0)
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            match EmailLookup::pick(&rows, email, |u| u.email.as_deref()) {
                Ok(Some(u)) => admin_account(u).map_or(EmailLookup::Missing, EmailLookup::Found),
                Ok(None) => EmailLookup::Missing,
                Err(()) => EmailLookup::Collides,
            },
        )
    }
}
