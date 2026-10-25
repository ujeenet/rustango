//! Creating a tenant from the operator console (#1322).
//!
//! Until now the console could list, edit, re-brand and impersonate —
//! but not create. An operator who needed a new tenant needed shell
//! access to a box holding registry credentials.
//!
//! ## Why the stream reads the table instead of a channel
//!
//! The obvious design is an in-process broadcast: provisioning pushes
//! events, the SSE handler subscribes. It does not survive contact
//! with two pods — the operator's stream can land on a different pod
//! from the one doing the work, and they watch an empty page while
//! provisioning succeeds somewhere else. It does not survive a reload
//! either, because a broadcast has no history.
//!
//! So the run is persisted (#1321) and this polls it. Unglamorous, and
//! it is what makes a reconnect resume exactly where it left off and a
//! second pod see everything. `Last-Event-ID` is the `seq` column.
//!
//! ## Who may do this
//!
//! Provisioning routes are mounted **only** when the deployment builds
//! the console through [`super::router_with_provisioning`]. That is a
//! deployment-level gate, and it is the only one: operators are
//! uniformly fully capable by design (#1342), so there is no
//! per-operator permission to check here and none is coming. What a
//! console can do is decided when it is assembled, not per request.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use tera::Context;

use super::{inject_op_brand, withheld, withheld_in, ConsoleState};
use crate::sql::connect_diagnosis::redact;
use crate::tenancy::auth;
use crate::tenancy::first_admin::{FirstAdmin, Handover};
use crate::tenancy::org::{BackendKind, StorageMode};
use crate::tenancy::preflight::{self, Preflight};
use crate::tenancy::provision::{self, DatabaseSource, ProvisionRequest, TenantStorage};
use crate::tenancy::provision_store::{self as store, RunState};

/// How often the stream looks for new events.
///
/// Short enough that a checklist feels live, long enough that an idle
/// stream is not a busy loop against the registry. Each poll is one
/// indexed `SELECT … WHERE run_id = ? AND seq > ?`.
const POLL_EVERY: Duration = Duration::from_millis(500);

/// Stop streaming a run that never terminates, so an abandoned browser
/// tab cannot hold a connection (and a poll loop) open indefinitely.
/// A provisioning run that has not finished in this long is stuck, and
/// the non-streaming view still shows whatever it did manage.
const STREAM_MAX: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------- new

/// The create form.
pub(super) async fn org_new_form(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    render_form(&state, &op, &HashMap::new(), None).await
}

async fn render_form(
    state: &ConsoleState,
    op: &auth::Operator,
    form: &HashMap<String, String>,
    error: Option<&str>,
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    // A URL may carry a password, so it is never sent back.
    let prefill: HashMap<&String, &String> =
        form.iter().filter(|(k, _)| *k != "database_url").collect();
    ctx.insert("prefill", &prefill);
    ctx.insert("error", &error);
    // Schema-mode is Postgres-only by language. Offering it on a
    // SQLite or MySQL registry would be offering a choice that can
    // only fail, so the form does not.
    ctx.insert(
        "schema_mode_available",
        &(state.registry.dialect().name() == "postgres"),
    );
    // A tenant always uses the registry's backend.
    ctx.insert("backend", BackendKind::of_pool(&state.registry).as_str());
    let servers: Vec<String> = crate::tenancy::db_server::list(&state.registry)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.active)
        .map(|s| s.name)
        .collect();
    ctx.insert(
        "default_source",
        // Isolated by default: the registry path shares its login.
        if servers.is_empty() { "url" } else { "server" },
    );
    ctx.insert("servers", &servers);
    // The first-admin fields appear only where a handover is set up.
    let handover = state
        .provisioner
        .as_ref()
        .and_then(|p| p.first_admin_handover());
    ctx.insert(
        "first_admin",
        &handover.map(|h| if h.mails() { "mail" } else { "link" }),
    );
    // What a derived URL will look like, with the password removed.
    // The real one is built server-side at submit time; the browser
    // never sees the registry's credentials.
    ctx.insert(
        "derived_url_example",
        &state
            .provisioner
            .as_ref()
            .and_then(|p| {
                provision::tenant_url_on_registry_server(&p.registry_url(), "tenant_<slug>")
            })
            .map(|u| redact(&u)),
    );
    render(state, "op_orgs_new.html", &ctx)
}

use super::render;

/// Turn the submitted form into a request, or say what is wrong with it.
///
/// `registry_url` is used to derive the tenant's URL when the operator
/// supplied a database *name* rather than a full URL — see
/// [`provision::tenant_url_on_registry_server`]. It is never rendered.
fn request_from_form(
    form: &HashMap<String, String>,
    registry_url: Option<&str>,
) -> Result<ProvisionRequest, String> {
    let field = |k: &str| form.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());

    let slug = field("slug")
        .ok_or("A slug is required — it is the tenant's globally unique name.")?
        .to_owned();

    let mode = match field("storage_mode").unwrap_or("database") {
        "schema" => StorageMode::Schema,
        _ => StorageMode::Database,
    };
    // The form shows the registry's backend; a posted one is checked
    // against it by the provisioner.
    let backend = field("backend_kind")
        .map(|b| BackendKind::parse(b).map_err(|got| format!("Unknown backend `{got}`.")))
        .transpose()?;
    if let Some(backend) = backend {
        backend
            .validate_storage_mode(mode)
            .map_err(ToOwned::to_owned)?;
    }

    let port = match field("port") {
        Some(raw) => Some(
            raw.parse::<i32>()
                .map_err(|_| format!("Port must be a whole number, got `{raw}`."))?,
        ),
        None => None,
    };

    // Where the tenant's database URL comes from, in order of
    // deference to what the operator actually said:
    //
    // 1. an explicit URL (the advanced escape hatch — another server);
    // 2. a database *name* on the registry's server, derived here so
    //    the registry password never reaches the browser and never
    //    travels back in a form post;
    // 3. nothing, for schema-mode, which has no separate database.
    let storage = match (mode, field("database_source")) {
        (StorageMode::Schema, _) => {
            TenantStorage::Schema(field("schema_name").map(ToOwned::to_owned))
        }
        (_, Some("server")) => TenantStorage::Database(DatabaseSource::Server(
            field("server")
                .ok_or("Choose the saved database server to create the database on.")?
                .to_owned(),
        )),
        (_, Some("url")) => TenantStorage::Database(DatabaseSource::Url(
            field("database_url")
                .ok_or("Enter the existing database's URL.")?
                .to_owned(),
        )),
        (_, Some("registry")) => TenantStorage::Database(DatabaseSource::Url(
            existing_database_url(&|k| if k == "database_url" { None } else { field(k) }, &slug, registry_url)?,
        )),
        // A post from before the choice existed.
        (_, _) => TenantStorage::Database(DatabaseSource::Url(existing_database_url(
            &field, &slug, registry_url,
        )?)),
    };

    let first_admin = match (field("admin_email"), field("admin_username")) {
        (Some(email), username) => Some(FirstAdmin::new(email, username)?),
        (None, Some(_)) => return Err("Enter the first admin's email, or clear the username.".into()),
        (None, None) => None,
    };

    Ok(ProvisionRequest {
        backend,
        display_name: field("display_name").map(ToOwned::to_owned),
        host_pattern: field("host_pattern").map(ToOwned::to_owned),
        port,
        path_prefix: field("path_prefix").map(ToOwned::to_owned),
        run_migrations: !form.contains_key("no_migrate"),
        first_admin,
        ..ProvisionRequest::new(slug, storage)
    })
}

/// An existing database: the URL given, or a database name derived on
/// the registry's server.
fn existing_database_url<'a>(
    field: &impl Fn(&str) -> Option<&'a str>,
    slug: &str,
    registry_url: Option<&str>,
) -> Result<String, String> {
    match field("database_url") {
        Some(explicit) => Ok(explicit.to_owned()),
        None => {
            // A blank database name means the default the form shows:
            // `tenant_<slug>`. Applying it here and not only in the
            // helper text matters — the page promises it, and a
            // promise the submit does not honour is worse than having
            // no default at all. (It was: the form said "blank uses
            // tenant_globex" and the submit answered "database mode
            // needs a database URL".)
            let name =
                field("database_name").map_or_else(|| format!("tenant_{slug}"), ToOwned::to_owned);
            let registry = registry_url
                .ok_or("This console cannot derive a database URL — supply a full URL instead.")?;
            provision::tenant_url_on_registry_server(registry, &name).ok_or_else(|| {
                format!(
                    "Could not derive a URL for database `{name}` from the registry's own \
                     connection. Supply a full URL instead."
                )
            })
        }
    }
}

/// Create the tenant, then redirect to its run.
pub(super) async fn org_new_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let provisioner = state
        .provisioner
        .as_ref()
        .expect("provisioning routes are only mounted when a provisioner is supplied");

    let registry_url = provisioner.registry_url();
    let request = match request_from_form(&form, Some(&registry_url)) {
        Ok(r) => r,
        Err(msg) => return render_form(&state, &op, &form, Some(&msg)).await,
    };
    let handover = provisioner.first_admin_handover();
    if request.first_admin.is_some() && handover.is_none() {
        let msg = "This console cannot hand over a first admin; leave the admin email empty.";
        return render_form(&state, &op, &form, Some(msg)).await;
    }

    // Provisioning runs inline. It is slower than a request should be
    // — the migrate step is seconds — but the alternative is spawning
    // and losing the failure, and the operator is watching a form they
    // just submitted. The stream view is for *re*-connecting, not for
    // escaping this wait.
    let outcome = provisioner
        .provision(&request, Some(&op.username), None)
        .await;
    let operator_id = op.id.get().copied().unwrap_or_default();
    let mut extra = serde_json::Map::new();
    // A failure can come after the Org row lands, so it is audited too.
    let verb = match &outcome {
        Ok((run, _)) => {
            let run_id = run.id.get().copied().unwrap_or_default();
            extra.insert("run_id".into(), serde_json::json!(run_id));
            "create"
        }
        Err(_) => "create_failed",
    };
    super::emit_op_audit(&state.registry, &request.slug, operator_id, verb, extra).await;
    match outcome {
        Ok((run, mut outcome)) => {
            let run_id = run.id.get().copied().unwrap_or_default();
            let (Some(handover), Some(_)) = (handover, &outcome.first_admin) else {
                return Redirect::to(&format!("provision/{run_id}")).into_response();
            };
            let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok());
            if let Err(e) = handover
                .hand_over_outcome(&state.registry, &mut outcome, host)
                .await
            {
                tracing::warn!(target: "rustango::tenancy::operator_console", run_id, error = %e, "first admin handover failed");
            }
            // Rendered here, not redirected: the link is shown this once.
            let shown = outcome.first_admin.as_ref().map(|admin| HandoverView {
                username: admin.username.clone(),
                mailed_to: match &admin.handover {
                    Some(Handover::Mailed { to }) => Some(to.clone()),
                    _ => None,
                },
                mail_failed: matches!(admin.handover, Some(Handover::MailFailed(_))),
                link: admin
                    .handover
                    .as_ref()
                    .and_then(Handover::link)
                    .map(|l| l.expose().to_owned()),
                hours: handover.ttl_hours(),
            });
            let mut resp = render_run(&state, &op, run_id, shown.as_ref()).await;
            let h = resp.headers_mut();
            h.insert(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-store"),
            );
            h.insert(
                axum::http::header::REFERRER_POLICY,
                axum::http::HeaderValue::from_static("no-referrer"),
            );
            resp
        }
        // A failure before the row lands — a taken slug, an unreachable
        // database — comes back here rather than as a run to watch,
        // because there is nothing to watch. Re-render with the
        // operator's input intact so they can fix one field.
        Err(e) => {
            let msg = e.user_facing().unwrap_or_else(|| {
                withheld(
                    "operator_console::provision",
                    &request.slug,
                    "Could not create the tenant",
                    &e,
                )
            });
            render_form(&state, &op, &form, Some(&msg)).await
        }
    }
}

// ---------------------------------------------------- test connection

/// Reach the database the form names, and report the taxonomy.
///
/// Writes nothing, creates nothing. Returns a fragment the form drops
/// in place, so this is usable from a button without a page reload.
pub(super) async fn test_connection(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let field = |k: &str| form.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());

    // A new database on a saved server: report its admin's rights.
    if field("storage_mode") != Some("schema") && field("database_source") == Some("server") {
        let Some(name) = field("server") else {
            return probe_bad("Choose a saved database server first.");
        };
        // With a slug, everything a submit checks before DDL, dry.
        if field("slug").is_some() {
            let Some(p) = state.provisioner.as_ref() else {
                return probe_bad("This console cannot create tenants.");
            };
            let request = match request_from_form(&form, None) {
                Ok(r) => r,
                Err(msg) => return probe_bad(&msg),
            };
            let secrets = state.pools.as_ref().map_or_else(
                crate::tenancy::TenantSecrets::standard,
                |p| p.tenant_secrets(),
            );
            let dry =
                provision::dry_run_on_server(&state.registry, &secrets, &p.registry_url(), &request)
                    .await;
            if let Err(e) = dry {
                let msg = e.user_facing().unwrap_or_else(|| {
                    withheld("operator_console::test_connection", name, "Could not check", &e)
                });
                return probe_bad(&msg);
            }
        }
        return super::db_servers::probe_response(&state, &op, name).await;
    }

    // Probe exactly what a submit would use — an explicit URL, or the
    // one derived from the database name. Probing a different target
    // from the one about to be provisioned is worse than not probing
    // at all, because it reports confidence in the wrong thing.
    let target = if let Some(explicit) = field("database_url") {
        Some(explicit.to_owned())
    } else {
        {
            let db = field("database_name")
                .map(ToOwned::to_owned)
                .or_else(|| field("slug").map(|s| format!("tenant_{s}")));
            match (state.provisioner.as_ref(), db) {
                (Some(p), Some(db)) => {
                    provision::tenant_url_on_registry_server(&p.registry_url(), &db)
                }
                _ => None,
            }
        }
    };
    let Some(url) = target else {
        return probe_bad("Enter a slug, a database name, or a full URL first.");
    };
    // Answer the same question the submit will: resolve the reference,
    // then the refusals that have nothing to do with reachability. The
    // registry's own database is reachable and this role *can* create
    // tables in it, so the probe used to bless the one target
    // provisioning refuses outright.
    // Checks only: a probe stores nothing, so it neither seals nor warns.
    let secrets = state.pools.as_ref().map_or_else(
        crate::tenancy::TenantSecrets::standard,
        |p| p.tenant_secrets(),
    );
    let registry_url = state.provisioner.as_ref().map(|p| p.registry_url());
    let resolved =
        provision::check_database_url(&url, &secrets, &state.registry, registry_url.as_deref())
            .await;
    let url = match resolved {
        Ok(u) => u,
        Err(crate::tenancy::TenancyError::Validation(msg)) => return probe_bad(&msg),
        Err(_) => return probe_bad("Could not check this database URL."),
    };

    match preflight::check(&url, &Preflight::default()).await {
        Ok(ok) => Probe::Ok {
            endpoint: Some(ok.endpoint),
            message: "This role can create tables, so migrations will run.",
        }
        .into_response(),
        // The diagnosis already leads with what to change — see
        // `sql::connect_diagnosis`. Rendering it verbatim is the point.
        Err(d) => probe_bad(&d.to_string()),
    }
}

// ------------------------------------------------------------ run view

/// Probe an *existing* tenant's database.
///
/// The create form's probe takes a URL from the form, which an existing
/// tenant cannot use: its URL carries a password and may be a secret
/// reference. This resolves server-side from the slug, so the URL never
/// reaches the browser.
///
/// Without it, testing a tenant whose database moved meant starting to
/// create a different tenant and borrowing that button.
pub(super) async fn test_tenant_connection(
    State(state): State<ConsoleState>,
    Extension(_op): Extension<auth::Operator>,
    Path(slug): Path<String>,
) -> Response<Body> {
    use crate::core::Column as _;
    use crate::sql::FetcherPool as _;

    let Some(pools) = state.pools.clone() else {
        return probe_bad("This console is read-only.");
    };
    let rows: Vec<super::super::Org> = match super::super::Org::objects()
        .where_(super::super::Org::slug.eq(slug.clone()))
        .fetch(&state.registry)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return probe_bad(&withheld(
                "operator_console::test_tenant_connection",
                &slug,
                "Could not read the tenant",
                &e,
            ))
        }
    };
    let Some(org) = rows.into_iter().next() else {
        return probe_bad(&format!("No tenant `{slug}`."));
    };

    if org.database_url.is_none() {
        return Probe::Ok {
            endpoint: None,
            message: "Schema-mode: this tenant lives in the registry's own database, which is \
                      already connected. There is no separate connection to test.",
        }
        .into_response();
    }

    let url = match pools.resolved_database_url(&org).await {
        Ok(u) => u,
        Err(e) => {
            return probe_bad(&withheld(
                "operator_console::test_tenant_connection",
                &slug,
                "Could not resolve the tenant's database URL",
                &e,
            ))
        }
    };
    match preflight::check(&url, &Preflight::default()).await {
        Ok(ok) => Probe::Ok {
            endpoint: Some(ok.endpoint),
            message: "This role can create tables.",
        }
        .into_response(),
        Err(d) => probe_bad(&d.to_string()),
    }
}

/// A connection probe's result, as JSON: the console builds the DOM
/// from it, so no server HTML reaches `innerHTML` (#2144).
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Probe {
    Ok {
        endpoint: Option<String>,
        message: &'static str,
    },
    Bad {
        message: String,
    },
}

impl IntoResponse for Probe {
    fn into_response(self) -> Response<Body> {
        axum::Json(self).into_response()
    }
}

fn probe_bad(message: &str) -> Response<Body> {
    Probe::Bad {
        message: message.to_owned(),
    }
    .into_response()
}

/// Every provisioning run, newest first.
///
/// A run was reachable only by its id — in practice only from the
/// redirect that created it, so navigating away stranded the record.
pub(super) async fn provision_runs_index(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<super::PageQuery>,
) -> Response<Body> {
    use crate::core::Model as _;

    let paged =
        match super::Paged::of_model(&state.registry, store::ProvisioningRun::SCHEMA, q.page).await
        {
            Ok(p) => p,
            Err(e) => {
                return super::server_error("could not count the runs", &e);
            }
        };
    let runs = match store::recent_runs(&state.registry, paged.limit, paged.offset).await {
        Ok(r) => r,
        Err(e) => {
            return super::server_error("could not read the run list", &e);
        }
    };

    let view: Vec<_> = runs
        .iter()
        .map(|r| {
            let state_str = r.state.clone();
            let kind = store::RunKind::parse(&r.kind);
            serde_json::json!({
                "id": r.id.get().copied().unwrap_or_default(),
                "kind": kind.as_str(),
                "is_migrate": kind == store::RunKind::Migrate,
                // A batch migrate spans every tenant, so it stores no
                // slug; the events name them as they go.
                "slug": if r.slug.is_empty() { "all tenants".to_owned() } else { r.slug.clone() },
                "state": state_str,
                "failed": RunState::parse(&r.state) == RunState::Failed,
                "running": !RunState::parse(&r.state).is_terminal(),
                "storage_mode": r.storage_mode,
                "backend_kind": r.backend_kind,
                "requested_by": r.requested_by,
                "error": r.error,
                "started_at": r.started_at.get()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string()),
                // How long it took, rather than a second wide timestamp
                // column: the finish time on its own says little that
                // the start time and a duration do not.
                "took": took(r),
            })
        })
        .collect();

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("runs", &view);
    paged.inject(&mut ctx, "/orgs/provision", "");
    render(&state, "op_provision_runs.html", &ctx)
}

/// How long a finished run took, or `None` while it is still going.
fn took(run: &store::ProvisioningRun) -> Option<String> {
    let started = run.started_at.get()?;
    let finished = run.finished_at?;
    let ms = (finished - *started).num_milliseconds();
    if ms < 0 {
        // Clocks on two pods can disagree; a negative duration is not
        // information worth rendering.
        return None;
    }
    // Integer arithmetic rather than a float divide: one decimal place
    // of a millisecond count needs no `f64`, and `i64 as f64` loses
    // precision for large values.
    Some(if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    })
}

/// The non-streaming view of a run: what it did, whether it finished.
///
/// Also the page a finished run settles into, and the audit trail
/// someone comes back to later.
pub(super) async fn provision_run_view(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(run_id): Path<i64>,
) -> Response<Body> {
    render_run(&state, &op, run_id, None).await
}

/// A first admin's handover, for the one render that shows it.
#[derive(serde::Serialize)]
struct HandoverView {
    username: String,
    mailed_to: Option<String>,
    mail_failed: bool,
    link: Option<String>,
    hours: u64,
}

async fn render_run(
    state: &ConsoleState,
    op: &auth::Operator,
    run_id: i64,
    handover: Option<&HandoverView>,
) -> Response<Body> {
    let Some(run) = (match store::run_by_id(&state.registry, run_id).await {
        Ok(r) => r,
        Err(e) => return super::server_error("operator_console", &e),
    }) else {
        return (StatusCode::NOT_FOUND, format!("no run {run_id}")).into_response();
    };
    let events = store::events_since(&state.registry, run_id, 0)
        .await
        .unwrap_or_default();

    let state_str = run.state.clone();
    let parsed = RunState::parse(&state_str);
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("run_id", &run_id);
    let kind = store::RunKind::parse(&run.kind);
    let is_migrate = kind == store::RunKind::Migrate;
    ctx.insert("is_migrate", &is_migrate);
    // Chosen here rather than in the template: Tera has no ternary, and
    // the alternative is the same word in four `{% if %}` blocks.
    ctx.insert(
        "verb",
        if is_migrate {
            "Migrating"
        } else {
            "Provisioning"
        },
    );
    ctx.insert(
        "slug",
        &if run.slug.is_empty() {
            "all tenants".to_owned()
        } else {
            run.slug.clone()
        },
    );
    ctx.insert("state", &state_str);
    ctx.insert("terminal", &parsed.is_terminal());
    ctx.insert("error", &run.error);
    ctx.insert(
        "events",
        &events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "seq": e.seq,
                    "step": e.step,
                    "status": e.status,
                    "message": e.message,
                })
            })
            .collect::<Vec<_>>(),
    );
    ctx.insert("handover", &handover);
    render(state, "op_provision_run.html", &ctx)
}

#[derive(serde::Deserialize, Default)]
pub(super) struct StreamQuery {
    /// Resume point, for clients that would rather pass it explicitly
    /// than through the `Last-Event-ID` header.
    after: Option<i64>,
}

/// Stream a run's events as they land.
///
/// Replays everything from `after` (or `Last-Event-ID`, or the
/// beginning) and then follows, **terminating** when the run reaches a
/// terminal state. A stream that never ends is a connection leak and
/// gives a browser no way to know it is watching a finished run.
pub(super) async fn provision_run_stream(
    State(state): State<ConsoleState>,
    Extension(_op): Extension<auth::Operator>,
    Path(run_id): Path<i64>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
    // `impl IntoResponse` rather than naming `Sse<impl Stream<…>>`:
    // the `Stream` trait lives in `futures-core`, which is in the tree
    // transitively but not a declared dependency. Not worth adding one
    // to spell a return type.
) -> impl IntoResponse {
    // The error type is pinned once: nothing in this stream can fail
    // in a way axum has to render — a store read that errors is
    // reported *as an event* and closes the stream, which is what a
    // watcher can actually act on.
    type Item = Result<Event, std::convert::Infallible>;

    // `Last-Event-ID` is how the browser's own EventSource resumes
    // after a dropped connection — it replays the header without being
    // asked. Honouring it is what makes a reconnect seamless rather
    // than a duplicate log.
    let resume = q.after.or_else(|| {
        headers
            .get(axum::http::header::HeaderName::from_static("last-event-id"))
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
    });

    let registry = state.registry.clone();
    let stream = async_stream::stream! {
        let mut last = resume.unwrap_or(0);
        let started = std::time::Instant::now();

        loop {
            if started.elapsed() > STREAM_MAX {
                yield Item::Ok(Event::default()
                    .event("error")
                    .data("stream timed out; the run is still unfinished"));
                return;
            }
            // A logout, deactivation or idle expiry ends the stream too.
            match super::active_session(&state, &headers).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    yield Item::Ok(Event::default().event("error").data("session ended; sign in again"));
                    return;
                }
                // A failed lookup is no logout: try again next poll.
                Err(e) => {
                    tracing::warn!(target: "rustango::tenancy::operator_console", run_id, error = %e, "stream session check failed");
                    tokio::time::sleep(POLL_EVERY).await;
                    continue;
                }
            }
            match store::events_since(&registry, run_id, last).await {
                Ok(events) => {
                    for event in events {
                        last = event.seq;
                        let data = serde_json::json!({
                            "seq": event.seq,
                            "step": event.step,
                            "status": event.status,
                            "message": event.message,
                        });
                        yield Item::Ok(Event::default()
                            // The id IS the resume point. Without it a
                            // reconnect starts over.
                            .id(event.seq.to_string())
                            .event("step")
                            .data(data.to_string()));
                    }
                }
                Err(e) => {
                    // Driver text is logged, not streamed (#2193).
                    let msg = withheld_in(
                        &tracing::error_span!("operator_console", run_id),
                        "operator_console::provision_stream",
                        "Could not read the run",
                        &e,
                    );
                    yield Item::Ok(Event::default().event("error").data(msg));
                    return;
                }
            }

            // Terminal check *after* draining, so the last events are
            // always delivered before the stream closes.
            let finished = matches!(
                store::run_by_id(&registry, run_id).await,
                Ok(Some(run)) if RunState::parse(&run.state).is_terminal()
            );
            if finished {
                yield Item::Ok(Event::default().event("done").data(last.to_string()));
                return;
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a deployment's registry connection.
    const TEST_REGISTRY: &str = "postgres://app:pw@db.internal:5432/registry_db";

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_minimal_form_becomes_a_database_mode_request() {
        let r = request_from_form(
            &form(&[("slug", "acme"), ("database_url", "postgres://u:p@h/acme")]),
            Some(TEST_REGISTRY),
        )
        .expect("valid");
        assert_eq!(r.slug, "acme");
        assert_eq!(r.mode(), StorageMode::Database);
        assert!(r.run_migrations, "migrations run unless opted out");
    }

    /// Each choice reads only its own fields.
    #[test]
    fn the_registry_and_url_choices_read_their_own_fields() {
        let both = [
            ("slug", "acme"),
            ("database_name", "acme_db"),
            ("database_url", "postgres://u:p@elsewhere/own"),
        ];
        let mut f = form(&both);
        f.insert("database_source".into(), "registry".into());
        let r = request_from_form(&f, Some(TEST_REGISTRY)).unwrap();
        assert!(r.database_url().unwrap().ends_with("db.internal:5432/acme_db"), "{r:?}");
        f.insert("database_source".into(), "url".into());
        let r = request_from_form(&f, Some(TEST_REGISTRY)).unwrap();
        assert_eq!(r.database_url(), Some("postgres://u:p@elsewhere/own"));
        let err = request_from_form(
            &form(&[("slug", "acme"), ("database_source", "url")]),
            Some(TEST_REGISTRY),
        )
        .expect_err("no URL");
        assert!(err.contains("URL"), "{err}");
    }

    /// The saved-server choice names the server, and only that.
    #[test]
    fn the_server_choice_becomes_a_server_source() {
        let r = request_from_form(
            &form(&[
                ("slug", "acme"),
                ("database_source", "server"),
                ("server", "main"),
                ("database_url", "postgres://u:p@h/ignored"),
            ]),
            Some(TEST_REGISTRY),
        )
        .expect("valid");
        assert_eq!(r.server(), Some("main"));
        assert_eq!(r.database_url(), None);
        let err = request_from_form(
            &form(&[("slug", "acme"), ("database_source", "server")]),
            Some(TEST_REGISTRY),
        )
        .expect_err("no server chosen");
        assert!(err.contains("server"), "{err}");
    }

    /// Blank fields are absent fields. A form posts every input it
    /// has, so treating "" as `Some("")` would write empty strings
    /// into columns that mean something by being NULL.
    #[test]
    fn blank_fields_are_treated_as_unset() {
        let r = request_from_form(
            &form(&[
                ("slug", "acme"),
                ("database_url", "postgres://u:p@h/acme"),
                ("display_name", "   "),
                ("host_pattern", ""),
            ]),
            Some(TEST_REGISTRY),
        )
        .expect("valid");
        assert_eq!(r.display_name, None);
        assert_eq!(r.host_pattern, None);
    }

    #[test]
    fn a_missing_slug_is_refused_with_a_useful_message() {
        let err = request_from_form(&form(&[("database_url", "x")]), Some(TEST_REGISTRY))
            .expect_err("no slug");
        assert!(err.contains("slug"), "{err}");
    }

    #[test]
    fn a_non_numeric_port_is_refused() {
        let err = request_from_form(
            &form(&[("slug", "a"), ("port", "eighty")]),
            Some(TEST_REGISTRY),
        )
        .expect_err("bad port");
        assert!(err.contains("whole number"), "{err}");
    }

    /// The checkbox is absent when unticked, which is how HTML forms
    /// work — so its *presence* is what turns migrations off.
    #[test]
    fn the_no_migrate_checkbox_inverts_correctly() {
        let on = request_from_form(
            &form(&[("slug", "a"), ("no_migrate", "on")]),
            Some(TEST_REGISTRY),
        )
        .unwrap();
        assert!(!on.run_migrations);
        let off = request_from_form(&form(&[("slug", "a")]), Some(TEST_REGISTRY)).unwrap();
        assert!(off.run_migrations);
    }

    /// Schema-mode on a non-PG backend is refused at the form, not
    /// deep in the pool layer.
    #[test]
    fn schema_mode_on_a_non_pg_backend_is_refused_early() {
        let err = request_from_form(
            &form(&[
                ("slug", "a"),
                ("storage_mode", "schema"),
                ("backend_kind", "sqlite"),
            ]),
            Some(TEST_REGISTRY),
        )
        .expect_err("sqlite cannot do schema mode");
        assert!(!err.is_empty());
    }
}

#[cfg(test)]
mod derivation_tests {
    use super::*;

    const REGISTRY: &str = "postgres://app:pw@db.internal:5432/registry_db";

    fn form(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A slug alone is enough. This is the whole point: an operator
    /// should not retype a host, a port and a password they already
    /// configured once.
    #[test]
    fn a_slug_alone_derives_a_url_on_the_registry_server() {
        let r = request_from_form(&form(&[("slug", "globex")]), Some(REGISTRY)).expect("valid");
        assert_eq!(
            r.database_url(),
            Some("postgres://app:pw@db.internal:5432/tenant_globex")
        );
    }

    /// The default the form *advertises* must be the default the
    /// submit *applies*. It was not: the page said "blank uses
    /// `tenant_globex`" and the submit answered "database mode needs a
    /// database URL".
    #[test]
    fn a_blank_database_name_uses_the_advertised_default() {
        let r = request_from_form(
            &form(&[("slug", "globex"), ("database_name", "   ")]),
            Some(REGISTRY),
        )
        .expect("valid");
        assert!(r.database_url().unwrap().ends_with("/tenant_globex"));
    }

    #[test]
    fn an_explicit_database_name_wins_over_the_default() {
        let r = request_from_form(
            &form(&[("slug", "globex"), ("database_name", "acme_prod")]),
            Some(REGISTRY),
        )
        .unwrap();
        assert!(r.database_url().unwrap().ends_with("/acme_prod"));
    }

    /// The escape hatch: a tenant on another server entirely.
    #[test]
    fn an_explicit_url_overrides_the_derivation() {
        let r = request_from_form(
            &form(&[
                ("slug", "globex"),
                ("database_name", "ignored"),
                ("database_url", "postgres://u:p@elsewhere:5432/own"),
            ]),
            Some(REGISTRY),
        )
        .unwrap();
        assert_eq!(r.database_url(), Some("postgres://u:p@elsewhere:5432/own"));
    }

    /// Schema mode has no separate database, so nothing is derived —
    /// and a URL is not silently carried along either.
    #[test]
    fn schema_mode_derives_nothing() {
        let r = request_from_form(
            &form(&[
                ("slug", "globex"),
                ("storage_mode", "schema"),
                ("database_name", "would_be_ignored"),
            ]),
            Some(REGISTRY),
        )
        .unwrap();
        assert_eq!(r.database_url(), None);
    }
}
