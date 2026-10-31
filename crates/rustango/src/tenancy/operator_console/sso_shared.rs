//! Shared SSO providers (registry-wide, `admin-sso`).
//!
//! A provider defined here (`SharedSsoProvider`, registry scope) is
//! offered on every tenant's login page. Each tenant manages its own
//! providers (`SsoProvider`) from its own admin. Both lists merge at
//! login; the tenant one wins on a slug clash.

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::response::IntoResponse as _;
use axum::Extension;
use serde::{Deserialize, Serialize};
use tera::Context;

use super::flash::{redirect, Flash};
use super::{auth, emit_registry_audit, inject_op_brand, render, server_error, ConsoleState};
use crate::core::Column as _;
use crate::sql::FetcherPool as _;
use crate::tenancy::server::{tenant_link, LinkApex};
use crate::tenancy::sso::{callback_path, SharedSsoProvider};
use crate::tenancy::Org;

/// Stands in for the slug in a callback URL template; the form script swaps it.
const SLUG_MARK: &str = "<slug>";

/// The add / edit form as posted. Also the re-render prefill, so the
/// secret is never serialized back into the page.
#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
pub(super) struct SharedSsoForm {
    slug: String,
    label: String,
    kind: String,
    issuer_url: String,
    client_id: String,
    #[serde(skip_serializing)]
    client_secret: String,
    scopes: String,
    sort_order: String,
    enabled: String,
    allow_email_link: String,
    allowed_email_domains: String,
    blocked_email_domains: String,
    allowed_idp_tenants: String,
    allowed_hosted_domains: String,
    required_claims: String,
    require_verified_email: String,
    sso_only: String,
    auto_deactivate_after_days: String,
    session_ttl_secs: String,
    superuser_session_ttl_secs: String,
}

fn on(b: bool) -> String {
    if b { "on" } else { "" }.to_owned()
}

fn num(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

/// A blank or positive whole number.
fn positive(name: &str, raw: &str) -> Result<Option<i64>, String> {
    match raw.trim() {
        "" => Ok(None),
        s => match s.parse::<i64>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(format!("{name} must be a positive whole number")),
        },
    }
}

fn opt(s: &str) -> Option<String> {
    Some(s.trim().to_owned()).filter(|s| !s.is_empty())
}

impl SharedSsoForm {
    /// The blank add form: enabled, generic OIDC.
    fn blank() -> Self {
        Self {
            kind: "oidc".into(),
            sort_order: "0".into(),
            enabled: on(true),
            require_verified_email: on(true),
            ..Self::default()
        }
    }

    /// The edit form for a saved row; the secret stays blank.
    fn of(row: &SharedSsoProvider) -> Self {
        Self {
            slug: row.slug.clone(),
            label: row.label.clone(),
            kind: row.kind.clone(),
            issuer_url: row.issuer_url.clone().unwrap_or_default(),
            client_id: row.client_id.clone(),
            client_secret: String::new(),
            scopes: row.scopes.clone().unwrap_or_default(),
            sort_order: row.sort_order.to_string(),
            enabled: on(row.enabled),
            allow_email_link: on(row.allow_email_link),
            allowed_email_domains: row.allowed_email_domains.clone().unwrap_or_default(),
            blocked_email_domains: row.blocked_email_domains.clone().unwrap_or_default(),
            allowed_idp_tenants: row.allowed_idp_tenants.clone().unwrap_or_default(),
            allowed_hosted_domains: row.allowed_hosted_domains.clone().unwrap_or_default(),
            required_claims: match &row.required_claims {
                serde_json::Value::Object(o) if o.is_empty() => String::new(),
                serde_json::Value::Null => String::new(),
                v => v.to_string(),
            },
            require_verified_email: on(row.require_verified_email),
            sso_only: on(row.sso_only),
            auto_deactivate_after_days: num(row.auto_deactivate_after_days.map(i64::from)),
            session_ttl_secs: num(row.session_ttl_secs),
            superuser_session_ttl_secs: num(row.superuser_session_ttl_secs),
        }
    }

    /// Check the form and write it onto `row`. A blank secret keeps the
    /// saved one (create requires one); the slug is the caller's.
    fn apply(&self, row: &mut SharedSsoProvider, creating: bool) -> Result<(), String> {
        let label = self.label.trim();
        let client_id = self.client_id.trim();
        if label.is_empty() {
            return Err("the button label is required".into());
        }
        if client_id.is_empty() {
            return Err("the client ID is required".into());
        }
        let sort_order = match self.sort_order.trim() {
            "" => 0,
            s => s
                .parse::<i32>()
                .map_err(|_| "sort order must be a whole number".to_owned())?,
        };
        let kind = self.kind.trim();
        let issuer_url = opt(&self.issuer_url);
        crate::sso::check_provider(kind, issuer_url.as_deref()).map_err(|e| e.to_string())?;
        for (name, value, max) in [
            ("button label", label, 150),
            ("issuer URL", issuer_url.as_deref().unwrap_or(""), 255),
            ("client ID", client_id, 255),
            ("scopes", self.scopes.trim(), 255),
        ] {
            if value.chars().count() > max {
                return Err(format!("{name} must be at most {max} characters"));
            }
        }
        if creating && self.client_secret.is_empty() {
            return Err("the client secret is required".into());
        }
        // The same parsers as the admin, so a saved row always parses.
        let list = |col: &str, raw: &str, max: usize| {
            let v = crate::sso::policy::normalise_list(col, raw)?;
            match &v {
                Some(s) if s.len() > max => Err(format!(
                    "{} must be at most {max} characters",
                    col.replace('_', " ")
                )),
                _ => Ok(v),
            }
        };
        let allowed = list("allowed_email_domains", &self.allowed_email_domains, 2000)?;
        let blocked = list("blocked_email_domains", &self.blocked_email_domains, 2000)?;
        let tenants = list("allowed_idp_tenants", &self.allowed_idp_tenants, 1000)?;
        let hosted = list("allowed_hosted_domains", &self.allowed_hosted_domains, 1000)?;
        let claims = match self.required_claims.trim() {
            "" => serde_json::json!({}),
            s => serde_json::from_str(s).map_err(|e| format!("required claims: {e}"))?,
        };
        crate::sso::policy::RequiredClaims::parse(&claims).map_err(|e| e.to_string())?;
        let days = positive("deactivate after days", &self.auto_deactivate_after_days)?
            .map(|d| i32::try_from(d).map_err(|_| "deactivate after days is too large".to_owned()))
            .transpose()?;
        let ttl = positive("session seconds", &self.session_ttl_secs)?;
        let su_ttl = positive(
            "superuser session seconds",
            &self.superuser_session_ttl_secs,
        )?;
        row.label = label.to_owned();
        row.kind = kind.to_owned();
        row.issuer_url = issuer_url;
        row.client_id = client_id.to_owned();
        if !self.client_secret.is_empty() {
            row.client_secret = crate::casts::Cast::new(self.client_secret.clone());
        }
        row.scopes = opt(&self.scopes);
        row.sort_order = sort_order;
        row.enabled = self.enabled == "on";
        row.allow_email_link = self.allow_email_link == "on";
        row.allowed_email_domains = allowed;
        row.blocked_email_domains = blocked;
        row.allowed_idp_tenants = tenants;
        row.allowed_hosted_domains = hosted;
        row.required_claims = claims;
        row.require_verified_email = self.require_verified_email == "on";
        row.sso_only = self.sso_only == "on";
        row.auto_deactivate_after_days = days;
        row.session_ttl_secs = ttl;
        row.superuser_session_ttl_secs = su_ttl;
        Ok(())
    }
}

/// Builds the redirect URIs an operator registers at the IdP: one per
/// tenant host, so a pattern plus an example on a real tenant.
struct Callbacks {
    login_url: String,
    example: Option<(Org, Option<String>, LinkApex)>,
}

impl Callbacks {
    async fn load(state: &ConsoleState, headers: &HeaderMap, apex: Option<LinkApex>) -> Self {
        let example = Org::objects()
            .where_(Org::active.eq(true))
            .order_by(&[("slug", false)])
            .limit(1)
            .fetch(&state.registry)
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|org| {
                let host = headers
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                (org, host, apex.unwrap_or_default())
            });
        Self {
            login_url: state.tenant_login_url.clone(),
            example,
        }
    }

    /// `{ pattern, example?, example_tenant? }` for `slug`.
    fn view(&self, slug: &str) -> serde_json::Value {
        let path = callback_path(&self.login_url, slug);
        let example = self.example.as_ref().map(|(org, host, apex)| {
            (
                org.slug.clone(),
                tenant_link(org, &path, host.as_deref(), apex),
            )
        });
        serde_json::json!({
            "pattern": format!("https://<tenant host>{path}"),
            "example": example.as_ref().map(|e| &e.1),
            "example_tenant": example.as_ref().map(|e| &e.0),
        })
    }
}

fn base_ctx(state: &ConsoleState, op: &auth::Operator) -> Context {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "sso");
    ctx.insert("operator_username", &op.username);
    ctx.insert("edit_enabled", &state.pools.is_some());
    ctx.insert(
        "kinds",
        &["oidc", "google", "microsoft", "github", "gitlab", "discord"],
    );
    ctx.insert("slug_mark", SLUG_MARK);
    ctx
}

/// The `?error=` / `?notice=` a redirect left (the flash layer checks them).
#[derive(Deserialize)]
pub(super) struct MessagesQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notice: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn render_list(
    state: &ConsoleState,
    op: &auth::Operator,
    callbacks: &Callbacks,
    form: &SharedSsoForm,
    status: StatusCode,
    error: Option<&str>,
    notice: Option<&str>,
) -> Response<Body> {
    let mut rows: Vec<SharedSsoProvider> =
        match SharedSsoProvider::objects().fetch(&state.registry).await {
            Ok(r) => r,
            Err(e) => return server_error("operator_console", &e),
        };
    rows.sort_by_key(|p| p.sort_order);
    let view: Vec<_> = rows
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.get().copied().unwrap_or_default(),
                "callback": callbacks.view(&p.slug),
                "slug": p.slug,
                "label": p.label,
                "kind": p.kind,
                "issuer_url": p.issuer_url,
                "client_id": p.client_id,
                "enabled": p.enabled,
                "allow_email_link": p.allow_email_link,
                "sort_order": p.sort_order,
            })
        })
        .collect();
    let mut ctx = base_ctx(state, op);
    ctx.insert("providers", &view);
    ctx.insert("creating", &true);
    ctx.insert("f", form);
    ctx.insert("callback", &callbacks.view(SLUG_MARK));
    ctx.insert("error", &error);
    ctx.insert("notice", &notice);
    with_status(render(state, "op_sso_shared.html", &ctx), status)
}

fn with_status(mut resp: Response<Body>, status: StatusCode) -> Response<Body> {
    if resp.status().is_success() {
        *resp.status_mut() = status;
    }
    resp
}

pub(super) async fn list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    apex: Option<Extension<LinkApex>>,
    headers: HeaderMap,
    Query(q): Query<MessagesQuery>,
) -> Response<Body> {
    let callbacks = Callbacks::load(&state, &headers, apex.map(|Extension(a)| a)).await;
    render_list(
        &state,
        &op,
        &callbacks,
        &SharedSsoForm::blank(),
        StatusCode::OK,
        q.error.as_deref(),
        q.notice.as_deref(),
    )
    .await
}

pub(super) async fn create(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    apex: Option<Extension<LinkApex>>,
    headers: HeaderMap,
    Form(form): Form<SharedSsoForm>,
) -> Response<Body> {
    let mut row = SharedSsoProvider {
        id: crate::sql::Auto::Unset,
        slug: form.slug.trim().to_owned(),
        label: String::new(),
        kind: String::new(),
        issuer_url: None,
        client_id: String::new(),
        client_secret: crate::casts::Cast::new(String::new()),
        enabled: true,
        sort_order: 0,
        scopes: None,
        allow_email_link: false,
        allowed_email_domains: None,
        blocked_email_domains: None,
        allowed_idp_tenants: None,
        allowed_hosted_domains: None,
        required_claims: serde_json::json!({}),
        require_verified_email: true,
        signup_mode: None,
        default_roles: None,
        role_map: serde_json::json!({}),
        role_claims: None,
        sync_roles_on_login: false,
        username_from: "email_local".into(),
        username_clash: "suffix".into(),
        sync_profile: false,
        allow_email_link_staff: false,
        sso_only: false,
        auto_deactivate_after_days: None,
        session_ttl_secs: None,
        superuser_session_ttl_secs: None,
        created_at: crate::sql::Auto::Unset,
        updated_at: crate::sql::Auto::Unset,
    };
    let outcome = async {
        if row.slug.len() > 64 || crate::validators::validate_slug(&row.slug).is_err() {
            return Err("slug must be 1-64 letters, digits, `_` or `-`".to_owned());
        }
        form.apply(&mut row, true)?;
        let taken = format!("a shared provider `{}` already exists", row.slug);
        use crate::sql::ExistsPool as _;
        let exists = SharedSsoProvider::objects()
            .where_(SharedSsoProvider::slug.eq(row.slug.clone()))
            .exists(&state.registry)
            .await;
        if matches!(exists, Ok(true)) {
            return Err(taken);
        }
        match row.insert_pool(&state.registry).await {
            Err(e) if e.is_unique_violation() => Err(taken),
            other => Ok(other),
        }
    }
    .await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return server_error("create failed", &e),
        Err(msg) => {
            let callbacks = Callbacks::load(&state, &headers, apex.map(|Extension(a)| a)).await;
            return render_list(
                &state,
                &op,
                &callbacks,
                &form,
                StatusCode::BAD_REQUEST,
                Some(&msg),
                None,
            )
            .await;
        }
    }
    let id = row.id.get().copied().unwrap_or_default();
    audit(&state, &op, id, "sso_shared_create", detail(&row, None)).await;
    redirect(
        "/sso-shared",
        Flash::Notice(&format!(
            "added `{}` — register its callback URL at the IdP",
            row.slug
        )),
    )
}

/// What an audit row records: never the secret, only whether it changed.
fn detail(
    row: &SharedSsoProvider,
    secret_replaced: Option<bool>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut d = serde_json::Map::new();
    d.insert("slug".into(), serde_json::json!(row.slug));
    d.insert("kind".into(), serde_json::json!(row.kind));
    d.insert("enabled".into(), serde_json::json!(row.enabled));
    d.insert(
        "allow_email_link".into(),
        serde_json::json!(row.allow_email_link),
    );
    for (k, v) in [
        ("allowed_email_domains", &row.allowed_email_domains),
        ("blocked_email_domains", &row.blocked_email_domains),
        ("allowed_idp_tenants", &row.allowed_idp_tenants),
        ("allowed_hosted_domains", &row.allowed_hosted_domains),
    ] {
        d.insert(k.into(), serde_json::json!(v));
    }
    d.insert("required_claims".into(), row.required_claims.clone());
    d.insert(
        "require_verified_email".into(),
        serde_json::json!(row.require_verified_email),
    );
    if let Some(replaced) = secret_replaced {
        d.insert("issuer_url".into(), serde_json::json!(row.issuer_url));
        d.insert("client_id".into(), serde_json::json!(row.client_id));
        d.insert("secret_replaced".into(), serde_json::json!(replaced));
    }
    d
}

async fn fetch(state: &ConsoleState, id: i64) -> Result<Option<SharedSsoProvider>, Response<Body>> {
    SharedSsoProvider::objects()
        .filter("id", id)
        .fetch(&state.registry)
        .await
        .map(|v| v.into_iter().next())
        .map_err(|e| server_error("operator_console", &e))
}

fn not_found() -> Response<Body> {
    redirect("/sso-shared", Flash::Error("no such shared provider"))
}

#[allow(clippy::too_many_arguments)]
fn render_edit(
    state: &ConsoleState,
    op: &auth::Operator,
    id: i64,
    callbacks: &Callbacks,
    form: &SharedSsoForm,
    status: StatusCode,
    error: Option<&str>,
) -> Response<Body> {
    let mut ctx = base_ctx(state, op);
    ctx.insert("id", &id);
    ctx.insert("creating", &false);
    ctx.insert("f", form);
    ctx.insert("callback", &callbacks.view(&form.slug));
    ctx.insert("error", &error);
    with_status(render(state, "op_sso_shared_edit.html", &ctx), status)
}

pub(super) async fn edit_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    apex: Option<Extension<LinkApex>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response<Body> {
    let row = match fetch(&state, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(),
        Err(resp) => return resp,
    };
    let callbacks = Callbacks::load(&state, &headers, apex.map(|Extension(a)| a)).await;
    render_edit(
        &state,
        &op,
        id,
        &callbacks,
        &SharedSsoForm::of(&row),
        StatusCode::OK,
        None,
    )
}

/// Update in place: the id stays, so tenant users' links to it survive.
/// The slug is the IdP callback key and stays too.
pub(super) async fn edit_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    apex: Option<Extension<LinkApex>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(mut form): Form<SharedSsoForm>,
) -> Response<Body> {
    let mut row = match fetch(&state, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(),
        Err(resp) => return resp,
    };
    form.slug = row.slug.clone();
    if let Err(msg) = form.apply(&mut row, false) {
        let callbacks = Callbacks::load(&state, &headers, apex.map(|Extension(a)| a)).await;
        return render_edit(
            &state,
            &op,
            id,
            &callbacks,
            &form,
            StatusCode::BAD_REQUEST,
            Some(&msg),
        );
    }
    if let Err(e) = row.save_pool(&state.registry).await {
        return server_error("update failed", &e);
    }
    let d = detail(&row, Some(!form.client_secret.is_empty()));
    audit(&state, &op, id, "sso_shared_update", d).await;
    redirect(
        "/sso-shared",
        Flash::Notice(&format!("updated `{}`", row.slug)),
    )
}

/// Audit a shared provider change; these rows decide sign-in for every tenant.
async fn audit(
    state: &ConsoleState,
    op: &auth::Operator,
    id: i64,
    verb: &str,
    detail: serde_json::Map<String, serde_json::Value>,
) {
    use crate::core::Model as _;
    emit_registry_audit(
        &state.registry,
        SharedSsoProvider::SCHEMA.table,
        &id.to_string(),
        op.id.get().copied().unwrap_or(0),
        verb,
        detail,
    )
    .await;
}

/// The wanted `allow_email_link` value, `on` or `off`.
#[derive(Deserialize)]
pub(super) struct EmailLinkForm {
    allow_email_link: String,
}

/// Set `allow_email_link` in place (only that column), so the id and its links survive.
pub(super) async fn set_email_link(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(id): Path<i64>,
    Form(form): Form<EmailLinkForm>,
) -> Response<Body> {
    use crate::sql::UpdaterPool as _;
    let allow = match form.allow_email_link.as_str() {
        "on" => true,
        "off" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "allow_email_link must be on or off",
            )
                .into_response()
        }
    };
    match SharedSsoProvider::objects()
        .filter("id", id)
        .update()
        .set("allow_email_link", allow)
        .execute_pool(&state.registry)
        .await
    {
        Ok(1) => {
            let mut detail = serde_json::Map::new();
            detail.insert("allow_email_link".into(), serde_json::json!(allow));
            audit(&state, &op, id, "sso_shared_email_link", detail).await;
            redirect("/sso-shared", Flash::Notice("email linking updated"))
        }
        Ok(_) => (StatusCode::NOT_FOUND, "no such shared provider").into_response(),
        Err(e) => server_error("update failed", &e),
    }
}

pub(super) async fn delete(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(id): Path<i64>,
) -> Response<Body> {
    let r = match fetch(&state, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such shared provider").into_response(),
        Err(resp) => return resp,
    };
    if let Err(e) = r.delete_pool(&state.registry).await {
        return server_error("delete failed", &e);
    }
    let mut detail = serde_json::Map::new();
    detail.insert("slug".into(), serde_json::json!(r.slug));
    audit(&state, &op, id, "sso_shared_delete", detail).await;
    redirect(
        "/sso-shared",
        Flash::Notice(&format!("deleted `{}`", r.slug)),
    )
}
