//! Saved database servers from the console: list, add, edit, delete and
//! test. Mounted with provisioning, the capability they serve.
//!
//! The admin password is a write-only field: never rendered, never
//! pre-filled, blank on edit keeps it. Audit rows and logs name the
//! fields that changed, never their values.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::Response;
use axum::response::IntoResponse;
use axum::Extension;
use serde::Deserialize;
use tera::Context;

use super::flash::{redirect, Flash};
use super::{inject_op_brand, render, urlencoding_lite, ConsoleState};
use crate::sql::LoginPassword;
use crate::tenancy::auth;
use crate::tenancy::db_server::{self, DbServer, DbServerError, DbServerSpec, PasswordChange, TlsMode};
use crate::tenancy::BackendKind;

const TABLE: &str = "rustango_db_servers";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why an edit was not saved.
enum EditFail {
    /// Changed meanwhile: reload, so the other change shows first.
    Stale(String),
    /// Re-render the form with this message.
    Form(String),
}

impl From<String> for EditFail {
    fn from(m: String) -> Self {
        Self::Form(m)
    }
}

#[derive(Deserialize)]
pub(super) struct MessagesQuery {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notice: Option<String>,
}

fn registry_backend(state: &ConsoleState) -> BackendKind {
    BackendKind::parse(state.registry.dialect().name()).unwrap_or_default()
}

/// A row for the templates: no password, only whether one is saved.
fn view(s: &DbServer) -> serde_json::Value {
    serde_json::json!({
        "name": s.name,
        "backend": s.backend,
        "host": s.host,
        "port": s.port,
        "admin_user": s.admin_user,
        "tls_mode": s.tls_mode,
        "maintenance_db": s.maintenance_db,
        "sqlite_dir": s.sqlite_dir,
        "active": s.active,
        "has_password": s.has_password(),
        "version": s.updated_at.get().map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
    })
}

fn base_ctx(state: &ConsoleState, op: &auth::Operator) -> Context {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("backend", registry_backend(state).as_str());
    ctx.insert("is_sqlite", &(registry_backend(state) == BackendKind::Sqlite));
    ctx.insert(
        "tls_modes",
        &TlsMode::ALL.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
    );
    ctx.insert("key_warning", &key_warning());
    ctx
}

/// What a missing or short `RUSTANGO_SECRET_KEY` disables. Never the key.
pub(super) fn key_warning() -> Option<&'static str> {
    use crate::aead::{key_status, KeyStatus};
    match key_status() {
        KeyStatus::Missing => Some(
            "RUSTANGO_SECRET_KEY is not set, so server passwords cannot be saved and new \
             tenant URLs are stored unsealed. Set it to 32 or more random bytes \
             (openssl rand -base64 48) and restart.",
        ),
        KeyStatus::Short => Some(
            "RUSTANGO_SECRET_KEY is shorter than 32 bytes, so passwords are sealed with a weak \
             key. Move it to RUSTANGO_SECRET_KEY_PREVIOUS, set 32 or more random bytes, restart \
             and run reseal-tenant-secrets --apply.",
        ),
        KeyStatus::Ok => None,
    }
}

/// `form` without the password, for re-rendering after an error.
fn prefill(form: &HashMap<String, String>) -> HashMap<String, String> {
    form.iter()
        .filter(|(k, _)| k.as_str() != "admin_password")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn render_list(
    state: &ConsoleState,
    op: &auth::Operator,
    rows: &[DbServer],
    form: &HashMap<String, String>,
    error: Option<&str>,
    notice: Option<&str>,
) -> Response<Body> {
    let mut ctx = base_ctx(state, op);
    ctx.insert("servers", &rows.iter().map(view).collect::<Vec<_>>());
    ctx.insert("prefill", &prefill(form));
    ctx.insert("error", &error);
    ctx.insert("notice", &notice);
    render(state, "op_db_servers.html", &ctx)
}

pub(super) async fn list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<MessagesQuery>,
) -> Response<Body> {
    match db_server::list(&state.registry).await {
        Ok(rows) => render_list(
            &state,
            &op,
            &rows,
            &HashMap::new(),
            q.error.as_deref(),
            q.notice.as_deref(),
        ),
        Err(e) => super::server_error("could not read the database servers", &e),
    }
}

/// The spec a form describes; the backend is always the registry's.
fn spec_from_form(
    state: &ConsoleState,
    form: &HashMap<String, String>,
    name: String,
) -> Result<DbServerSpec, String> {
    let field = |k: &str| form.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let port = field("port")
        .map(|p| {
            p.parse::<u16>()
                .map_err(|_| format!("Port must be a number from 1 to 65535, got `{p}`."))
        })
        .transpose()?;
    let tls = field("tls_mode")
        .map(TlsMode::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(DbServerSpec {
        name,
        backend: registry_backend(state),
        host: field("host").map(ToOwned::to_owned),
        port,
        admin_user: field("admin_user").map(ToOwned::to_owned),
        tls,
        maintenance_db: field("maintenance_db").map(ToOwned::to_owned),
        sqlite_dir: field("sqlite_dir").map(ToOwned::to_owned),
        active: form.contains_key("active"),
    })
}

/// The typed password, or `None` when the field is blank.
fn password_from_form(form: &HashMap<String, String>) -> Result<Option<LoginPassword>, String> {
    match form.get("admin_password").filter(|p| !p.is_empty()) {
        None => Ok(None),
        Some(p) => LoginPassword::new(p.clone())
            .map(Some)
            .map_err(|_| "The password must be 1 to 255 bytes.".to_owned()),
    }
}

/// The error as a page may show it; a driver error stays in the log.
fn explain(e: &DbServerError, name: &str) -> String {
    match e {
        DbServerError::Exec(d)
        | DbServerError::Admin(crate::tenancy::server_admin::TenantDbError::Exec(d)) => {
            super::withheld(
            "operator_console::db_servers",
            name,
            "the database refused the request",
            d,
        )
        }
        other => other.to_string(),
    }
}

async fn audit(state: &ConsoleState, op: &auth::Operator, name: &str, verb: &str, fields: &[&str]) {
    let extra = fields
        .iter()
        .map(|f| ((*f).to_owned(), serde_json::Value::Bool(true)))
        .collect();
    let operator_id = op.id.get().copied().unwrap_or_default();
    super::emit_registry_audit(&state.registry, TABLE, name, operator_id, verb, extra).await;
}

pub(super) async fn create(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let name = form.get("name").map(|n| n.trim().to_owned()).unwrap_or_default();
    let outcome = async {
        let spec = spec_from_form(&state, &form, name.clone())?;
        let password = password_from_form(&form)?;
        db_server::add(&state.registry, &spec, password.as_ref())
            .await
            .map_err(|e| explain(&e, &name))
    }
    .await;
    match outcome {
        Ok(row) => {
            let mut fields = vec!["name", "backend"];
            for (f, set) in [
                ("host", row.host.is_some()),
                ("port", row.port.is_some()),
                ("admin_user", row.admin_user.is_some()),
                ("admin_password", row.has_password()),
                ("tls_mode", row.tls_mode.is_some()),
                ("maintenance_db", row.maintenance_db.is_some()),
                ("sqlite_dir", row.sqlite_dir.is_some()),
                ("active", true),
            ] {
                if set {
                    fields.push(f);
                }
            }
            audit(&state, &op, &row.name, "db_server_add", &fields).await;
            redirect(
                "/db-servers",
                Flash::Notice(&format!("saved `{}` — test it before you use it", row.name)),
            )
        }
        Err(msg) => {
            let rows = db_server::list(&state.registry).await.unwrap_or_default();
            render_list(&state, &op, &rows, &form, Some(&msg), None)
        }
    }
}

pub(super) async fn edit_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(name): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Response<Body> {
    match db_server::get(&state.registry, &name).await {
        Ok(row) => render_edit(&state, &op, &row, None, q.error.as_deref(), q.notice.as_deref()),
        Err(DbServerError::NotFound(_)) => redirect(
            "/db-servers",
            Flash::Error(&format!("no database server `{name}`")),
        ),
        Err(e) => super::server_error("could not read the database server", &e),
    }
}

fn render_edit(
    state: &ConsoleState,
    op: &auth::Operator,
    row: &DbServer,
    form: Option<&HashMap<String, String>>,
    error: Option<&str>,
    notice: Option<&str>,
) -> Response<Body> {
    let mut ctx = base_ctx(state, op);
    ctx.insert("server", &view(row));
    ctx.insert("prefill", &form.map(prefill));
    ctx.insert("error", &error);
    ctx.insert("notice", &notice);
    render(state, "op_db_server_edit.html", &ctx)
}

pub(super) async fn edit_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let row = match db_server::get(&state.registry, &name).await {
        Ok(r) => r,
        Err(e) => return redirect("/db-servers", Flash::Error(&explain(&e, &name))),
    };
    let outcome = async {
        let spec = spec_from_form(&state, &form, row.name.clone())?;
        let password = if form.contains_key("clear_password") {
            PasswordChange::Clear
        } else {
            password_from_form(&form)?.map_or(PasswordChange::Keep, PasswordChange::Set)
        };
        // The row the form was built from; a later write makes this stale.
        let seen = match form.get("version").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            None => None,
            Some(v) => Some(
                chrono::DateTime::parse_from_rfc3339(v)
                    .map_err(|_| "The form's version is not valid; reload the page.".to_owned())?
                    .with_timezone(&chrono::Utc),
            ),
        };
        db_server::update(&state.registry, &name, &spec, password, seen)
            .await
            .map_err(|e| match e {
                DbServerError::Stale(_) => EditFail::Stale(e.to_string()),
                e => EditFail::Form(explain(&e, &name)),
            })
    }
    .await;
    match outcome {
        Err(EditFail::Stale(msg)) => redirect(
            &format!("/db-servers/{}/edit", urlencoding_lite(&row.name)),
            Flash::Error(&msg),
        ),
        Ok((row, changed)) => {
            if !changed.is_empty() {
                audit(&state, &op, &row.name, "db_server_edit", &changed).await;
            }
            redirect(
                &format!("/db-servers/{}/edit", urlencoding_lite(&row.name)),
                Flash::Notice(&format!("updated `{}`", row.name)),
            )
        }
        Err(EditFail::Form(msg)) => render_edit(&state, &op, &row, Some(&form), Some(&msg), None),
    }
}

pub(super) async fn delete(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(name): Path<String>,
) -> Response<Body> {
    // The deployment's resolver decides which tenants use the server; a
    // stand-in could miss one.
    let Some(pools) = state.pools.as_ref() else {
        return redirect(
            "/db-servers",
            Flash::Error(
                "this console has no tenant pools, so it cannot tell which tenants use the \
                 server; remove it with `remove-db-server`",
            ),
        );
    };
    match db_server::remove(&state.registry, &name, &pools.tenant_secrets()).await {
        Ok(()) => {
            audit(&state, &op, &name, "db_server_delete", &[]).await;
            redirect(
                "/db-servers",
                Flash::Notice(&format!("removed database server `{name}`")),
            )
        }
        Err(e) => redirect("/db-servers", Flash::Error(&explain(&e, &name))),
    }
}

/// A probe's result as JSON. `status`/`message` feed the console's
/// shared `showProbe`; the rest is the report itself.
#[derive(serde::Serialize)]
struct ServerProbe {
    status: &'static str,
    message: String,
    login: bool,
    server_version: Option<String>,
    can_create_database: bool,
    can_create_user: bool,
    can_grant: bool,
    blocker: Option<&'static str>,
}

pub(super) async fn test(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(name): Path<String>,
) -> Response<Body> {
    probe_response(&state, &op, &name).await
}

/// Log in to server `name` and report its admin's rights as JSON. Also
/// the create form's test for a new database on a saved server.
pub(super) async fn probe_response(
    state: &ConsoleState,
    op: &auth::Operator,
    name: &str,
) -> Response<Body> {
    // A probe logs in with the saved password, so it is on the trail too.
    audit(state, op, name, "db_server_test", &[]).await;
    let bad = |message: String| ServerProbe {
        status: "bad",
        message,
        login: false,
        server_version: None,
        can_create_database: false,
        can_create_user: false,
        can_grant: false,
        blocker: None,
    };
    let probe = match db_server::get(&state.registry, name).await {
        Ok(row) => match row.probe(PROBE_TIMEOUT).await {
            Ok(r) => {
                let blocker = r.blocker();
                let yes = |b: bool| if b { "yes" } else { "no" };
                let mut message = match &r.login_error {
                    Some(e) => format!("Login refused: {e}"),
                    None => format!(
                        "Logged in{}. Create databases: {}, create logins: {}, grant: {}.",
                        r.server_version
                            .as_deref()
                            .map(|v| format!(" to {v}"))
                            .unwrap_or_default(),
                        yes(r.can_create_database),
                        yes(r.can_create_user),
                        yes(r.can_grant)
                    ),
                };
                if let Some(b) = blocker {
                    message.push_str(&format!(" Not ready: {b}."));
                }
                if let Some(w) = row.tls_warning() {
                    message.push_str(&format!(" Warning: {w}."));
                }
                ServerProbe {
                    status: if blocker.is_none() { "ok" } else { "bad" },
                    message,
                    login: r.can_login,
                    server_version: r.server_version.clone(),
                    can_create_database: r.can_create_database,
                    can_create_user: r.can_create_user,
                    can_grant: r.can_grant,
                    blocker,
                }
            }
            Err(e) => bad(explain(&e, name)),
        },
        Err(e) => bad(explain(&e, name)),
    };
    axum::Json(probe).into_response()
}
