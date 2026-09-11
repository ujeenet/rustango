//! Managing the extra hostnames a tenant answers on, from the console.
//!
//! [`super::super::org_host`] shipped the model, the resolver and the
//! whole `add` / `remove` / `enable` API — reachable only from Rust.
//! An operator adding a customer's vanity domain had to write a program
//! or hand-edit `rustango_org_hosts`, which is the kind of task a
//! console exists for. `TenantHost::is_base` was even documented as
//! being "for the UI to explain why the row has no delete button", for
//! a UI that did not exist.
//!
//! ## Why its own page rather than a section of the edit form
//!
//! The edit form is one `POST` that writes one `Org` row. Hostnames are
//! a collection with three verbs (add, remove, enable/disable) acting on
//! individual members, and folding them into that form would mean either
//! a submit that does several unrelated things at once, or per-row
//! forms nested inside the outer one — which HTML does not allow.
//!
//! ## What this page may not do
//!
//! Remove the base host. It has no row in `rustango_org_hosts` to
//! remove — that is [`org_host`](super::super::org_host)'s deliberate
//! design, not an oversight — so the page renders it without controls
//! and the engine refuses it anyway. Both, because an operator who
//! guesses the `POST` should get the same answer as one who reads the
//! page.

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::{Response, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::Extension;
use serde::Deserialize;
use tera::Context;

use super::super::auth;
use super::super::org_host::{self, HostError};
use super::{inject_op_brand, render, urlencoding_lite, ConsoleState};
use crate::core::Column as _;
use crate::sql::FetcherPool as _;

#[derive(Deserialize)]
pub(super) struct HostsQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notice: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AddForm {
    #[serde(default)]
    hostname: String,
}

#[derive(Deserialize)]
pub(super) struct HostActionForm {
    #[serde(default)]
    hostname: String,
    /// Present and `"on"` for the enable action; absent means disable.
    /// A checkbox posts nothing when unchecked, which is exactly the
    /// shape this needs.
    #[serde(default)]
    enabled: Option<String>,
}

/// The page: base host first, then every extra, with its controls.
pub(super) async fn org_hosts_view(
    State(state): State<ConsoleState>,
    Path(slug): Path<String>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<HostsQuery>,
) -> Response<Body> {
    // 404 before listing, so an unknown slug reads as "no such tenant"
    // rather than "a tenant with no hosts".
    if !org_exists(&state, &slug).await {
        return (StatusCode::NOT_FOUND, format!("org `{slug}` not found")).into_response();
    }

    let hosts = match org_host::list_for_org(&state.registry, &slug).await {
        Ok(h) => h,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("slug", &slug);
    ctx.insert("hosts", &hosts);
    ctx.insert("error", &q.error);
    ctx.insert("notice", &q.notice);

    render(&state, "op_org_hosts.html", &ctx)
}

/// Bind one more hostname to this tenant.
pub(super) async fn org_hosts_add(
    State(state): State<ConsoleState>,
    Path(slug): Path<String>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<AddForm>,
) -> Response<Body> {
    match org_host::add_host(&state.registry, &slug, &form.hostname).await {
        Ok(row) => {
            audit(&state, &slug, &op, "host_add", &row.hostname).await;
            back(&slug, None, Some(&format!("added `{}`", row.hostname)))
        }
        Err(e) => back(&slug, Some(&explain(&e, &slug)), None),
    }
}

/// Unbind a hostname. Refused for the base host.
pub(super) async fn org_hosts_remove(
    State(state): State<ConsoleState>,
    Path(slug): Path<String>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<HostActionForm>,
) -> Response<Body> {
    match org_host::remove_host(&state.registry, &slug, &form.hostname).await {
        Ok(()) => {
            audit(&state, &slug, &op, "host_remove", &form.hostname).await;
            back(&slug, None, Some(&format!("removed `{}`", form.hostname)))
        }
        Err(e) => back(&slug, Some(&explain(&e, &slug)), None),
    }
}

/// Park or un-park a hostname without losing the record.
pub(super) async fn org_hosts_toggle(
    State(state): State<ConsoleState>,
    Path(slug): Path<String>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<HostActionForm>,
) -> Response<Body> {
    let enable = form.enabled.is_some();
    match org_host::set_host_enabled(&state.registry, &slug, &form.hostname, enable).await {
        Ok(()) => {
            let verb = if enable {
                "host_enable"
            } else {
                "host_disable"
            };
            audit(&state, &slug, &op, verb, &form.hostname).await;
            let word = if enable { "enabled" } else { "disabled" };
            back(&slug, None, Some(&format!("{word} `{}`", form.hostname)))
        }
        Err(e) => back(&slug, Some(&explain(&e, &slug)), None),
    }
}

// ------------------------------------------------------------- helpers

async fn org_exists(state: &ConsoleState, slug: &str) -> bool {
    super::super::Org::objects()
        .where_(super::super::Org::slug.eq(slug.to_owned()))
        .fetch(&state.registry)
        .await
        .is_ok_and(|rows: Vec<super::super::Org>| !rows.is_empty())
}

/// Post/Redirect/Get back to the page, carrying the outcome.
///
/// A redirect rather than a render so a reload does not repost the
/// action — these are writes that change how traffic routes.
fn back(slug: &str, error: Option<&str>, notice: Option<&str>) -> Response<Body> {
    use std::fmt::Write as _;

    let mut url = format!("/orgs/{}/hosts", urlencoding_lite(slug));
    if let Some(e) = error {
        let _ = write!(url, "?error={}", urlencoding_lite(e));
    } else if let Some(n) = notice {
        let _ = write!(url, "?notice={}", urlencoding_lite(n));
    }
    Redirect::to(&url).into_response()
}

/// The engine's message, plus what the operator should do about it.
///
/// `HostError`'s own `Display` is written for a log line. On a page
/// where someone is about to retype the value, "is already registered"
/// without "to which tenant, and what to do" just invites a second
/// attempt at the same thing.
fn explain(e: &HostError, slug: &str) -> String {
    match e {
        HostError::Invalid(h) => format!(
            "`{h}` is not a bare hostname — no scheme, no port, no path, no trailing dot. \
             Use `shop.example.com`"
        ),
        HostError::Taken(h) => format!(
            "`{h}` is already registered. A hostname resolves to exactly one tenant, so \
             remove it from the tenant that holds it first"
        ),
        HostError::IsBaseHost(h) => format!(
            "`{h}` is `{slug}`'s base host and has no row to remove. Change it on the edit \
             page instead"
        ),
        HostError::NotFound => "no such host on this tenant".to_owned(),
        HostError::Driver(d) => format!("the registry rejected the change: {d}"),
    }
}

async fn audit(state: &ConsoleState, slug: &str, op: &auth::Operator, verb: &str, host: &str) {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "hostname".into(),
        serde_json::Value::String(host.to_owned()),
    );
    let operator_id = op.id.get().copied().unwrap_or_default();
    super::emit_op_audit(&state.registry, slug, operator_id, verb, extra).await;
}
