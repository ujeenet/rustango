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
//! deliberate deployment-level gate rather than a per-operator
//! permission, because `Operator` has no permission model at all
//! today — every authenticated operator can already do everything the
//! console exposes. Adding one flag for one route would be half a
//! permission system; the gate that actually means something right now
//! is whether the console can create tenants *at all*. A real operator
//! permission model is worth its own issue.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use tera::Context;

use super::{inject_op_brand, ConsoleState};
use crate::sql::connect_diagnosis::redact;
use crate::tenancy::auth;
use crate::tenancy::org::{BackendKind, StorageMode};
use crate::tenancy::preflight::{self, Preflight};
use crate::tenancy::provision::{self, ProvisionRequest};
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
    render_form(&state, &op, &HashMap::new(), None)
}

fn render_form(
    state: &ConsoleState,
    op: &auth::Operator,
    prefill: &HashMap<String, String>,
    error: Option<&str>,
) -> Response<Body> {
    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "orgs");
    ctx.insert("operator_username", &op.username);
    ctx.insert("prefill", prefill);
    ctx.insert("error", &error);
    // Schema-mode is Postgres-only by language. Offering it on a
    // SQLite or MySQL registry would be offering a choice that can
    // only fail, so the form does not.
    ctx.insert(
        "schema_mode_available",
        &(state.registry.dialect().name() == "postgres"),
    );
    // Only backends this binary can actually speak. Listing all three
    // unconditionally meant an operator could pick `sqlite` on a
    // Postgres-only build and be told to edit `Cargo.toml` — advice
    // aimed at a developer, for a choice the form itself offered.
    ctx.insert("backends", &compiled_backends());
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

/// The backends this binary was built with.
///
/// Compile-time truth, not a hardcoded list: a form must not offer a
/// choice the binary cannot honour.
fn compiled_backends() -> Vec<&'static str> {
    // Written as pushes rather than one `vec![]` because each entry is
    // independently `#[cfg]`-gated; clippy's `vec![]` suggestion does
    // not apply to a conditionally-built list.
    #[allow(clippy::vec_init_then_push)]
    {
        let mut out = Vec::new();
        #[cfg(feature = "postgres")]
        out.push("postgres");
        #[cfg(feature = "mysql")]
        out.push("mysql");
        #[cfg(feature = "sqlite")]
        out.push("sqlite");
        out
    }
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
    let backend = BackendKind::parse(field("backend_kind").unwrap_or("postgres"))
        .map_err(|got| format!("Unknown backend `{got}`."))?;
    backend
        .validate_storage_mode(mode)
        .map_err(ToOwned::to_owned)?;

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
    let database_url = match (mode, field("database_url")) {
        (StorageMode::Schema, _) => None,
        (_, Some(explicit)) => Some(explicit.to_owned()),
        (_, None) => {
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
            Some(
                provision::tenant_url_on_registry_server(registry, &name).ok_or_else(|| {
                    format!(
                        "Could not derive a URL for database `{name}` from the registry's own \
                         connection. Supply a full URL instead."
                    )
                })?,
            )
        }
    };

    Ok(ProvisionRequest {
        slug,
        mode,
        backend,
        display_name: field("display_name").map(ToOwned::to_owned),
        database_url,
        schema_name: field("schema_name").map(ToOwned::to_owned),
        host_pattern: field("host_pattern").map(ToOwned::to_owned),
        port,
        path_prefix: field("path_prefix").map(ToOwned::to_owned),
        run_migrations: !form.contains_key("no_migrate"),
        preflight: Preflight::default(),
    })
}

/// Create the tenant, then redirect to its run.
pub(super) async fn org_new_submit(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let provisioner = state
        .provisioner
        .as_ref()
        .expect("provisioning routes are only mounted when a provisioner is supplied");

    let registry_url = provisioner.registry_url();
    let request = match request_from_form(&form, Some(&registry_url)) {
        Ok(r) => r,
        Err(msg) => return render_form(&state, &op, &form, Some(&msg)),
    };

    // Provisioning runs inline. It is slower than a request should be
    // — the migrate step is seconds — but the alternative is spawning
    // and losing the failure, and the operator is watching a form they
    // just submitted. The stream view is for *re*-connecting, not for
    // escaping this wait.
    match provisioner
        .provision(&request, Some(&op.username), None)
        .await
    {
        Ok((run, _)) => {
            let run_id = run.id.get().copied().unwrap_or_default();
            Redirect::to(&format!("provision/{run_id}")).into_response()
        }
        // A failure before the row lands — a taken slug, an unreachable
        // database — comes back here rather than as a run to watch,
        // because there is nothing to watch. Re-render with the
        // operator's input intact so they can fix one field.
        Err(e) => render_form(&state, &op, &form, Some(&e.to_string())),
    }
}

// ---------------------------------------------------- test connection

/// Reach the database the form names, and report the taxonomy.
///
/// Writes nothing, creates nothing. Returns a fragment the form drops
/// in place, so this is usable from a button without a page reload.
pub(super) async fn test_connection(
    State(state): State<ConsoleState>,
    Extension(_op): Extension<auth::Operator>,
    Form(form): Form<HashMap<String, String>>,
) -> Response<Body> {
    let field = |k: &str| form.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());

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
        return Html(
            "<p class=\"probe probe-bad\">Enter a slug, a database name, or a full URL first.</p>"
                .to_owned(),
        )
        .into_response();
    };
    let url = url.as_str();

    // Answer the same question the submit will, including the one
    // refusal that has nothing to do with reachability. The registry's
    // own database is reachable and this role *can* create tables in
    // it, so the probe used to report "migrations will run" for the
    // single target provisioning refuses outright — blessing, in its
    // most confident wording, the mistake that ran the tenant
    // migration chain over the registry.
    if let Some(p) = state.provisioner.as_ref() {
        if let Err(msg) = provision::refuse_registry_url(url, &p.registry_url()) {
            return Html(format!(
                "<p class=\"probe probe-bad\">{}</p>",
                html_escape(&msg)
            ))
            .into_response();
        }
    }

    match preflight::check(url, &Preflight::default()).await {
        Ok(ok) => Html(format!(
            "<p class=\"probe probe-ok\">Reached <code>{}</code>. \
             This role can create tables, so migrations will run.</p>",
            html_escape(&ok.endpoint)
        ))
        .into_response(),
        // The diagnosis already leads with what to change — see
        // `sql::connect_diagnosis`. Rendering it verbatim is the point.
        Err(d) => Html(format!(
            "<p class=\"probe probe-bad\">{}</p>",
            html_escape(&d.to_string())
        ))
        .into_response(),
    }
}

// ------------------------------------------------------------ run view

/// How many runs a page of the index shows.
const RUNS_PAGE_SIZE: i64 = 50;

#[derive(Deserialize)]
pub(super) struct RunsQuery {
    #[serde(default)]
    page: Option<i64>,
}

/// Every provisioning run, newest first.
///
/// A run was reachable only by its id, which meant only from the
/// redirect that created it: navigate away and the record survived in
/// the table but not in anybody's reach. The runs are persisted so they
/// outlive the request, and this is what makes that worth anything —
/// including for the runs that failed, which are the ones somebody
/// comes back to.
pub(super) async fn provision_runs_index(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<RunsQuery>,
) -> Response<Body> {
    // Saturating: a page number large enough to overflow the multiply
    // parses into an `i64` fine and then panicked the worker. Clamped,
    // an absurd page is just an empty one.
    let page = q.page.unwrap_or(1).max(1);
    let offset = page.saturating_sub(1).saturating_mul(RUNS_PAGE_SIZE);

    // One more than a page, so "is there an older page?" costs no
    // second query.
    let mut runs = match store::recent_runs(&state.registry, RUNS_PAGE_SIZE + 1, offset).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read the run list: {e}"),
            )
                .into_response();
        }
    };
    let page_len = usize::try_from(RUNS_PAGE_SIZE).unwrap_or(usize::MAX);
    let has_next = runs.len() > page_len;
    runs.truncate(page_len);

    let view: Vec<_> = runs
        .iter()
        .map(|r| {
            let state_str = r.state.clone();
            serde_json::json!({
                "id": r.id.get().copied().unwrap_or_default(),
                "slug": r.slug,
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
    ctx.insert("page", &page);
    ctx.insert("has_next", &has_next);
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
    let Some(run) = (match store::run_by_id(&state.registry, run_id).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    ctx.insert("slug", &run.slug);
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
    render(&state, "op_provision_run.html", &ctx)
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
                    yield Item::Ok(Event::default()
                        .event("error")
                        .data(e.to_string()));
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
            if started.elapsed() > STREAM_MAX {
                yield Item::Ok(Event::default()
                    .event("error")
                    .data("stream timed out; the run is still unfinished"));
                return;
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Minimal escaping for the two fragments this module builds by hand.
///
/// Both carry a connection diagnosis, which contains a hostname and a
/// driver message — neither of which is ours, so neither goes into a
/// page unescaped.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        assert_eq!(r.mode, StorageMode::Database);
        assert!(r.run_migrations, "migrations run unless opted out");
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

    #[test]
    fn escaping_covers_the_characters_that_break_out_of_a_fragment() {
        assert_eq!(
            html_escape(r#"<script>"&"</script>"#),
            "&lt;script&gt;&quot;&amp;&quot;&lt;/script&gt;"
        );
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
            r.database_url.as_deref(),
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
        assert!(r
            .database_url
            .as_deref()
            .unwrap()
            .ends_with("/tenant_globex"));
    }

    #[test]
    fn an_explicit_database_name_wins_over_the_default() {
        let r = request_from_form(
            &form(&[("slug", "globex"), ("database_name", "acme_prod")]),
            Some(REGISTRY),
        )
        .unwrap();
        assert!(r.database_url.as_deref().unwrap().ends_with("/acme_prod"));
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
        assert_eq!(
            r.database_url.as_deref(),
            Some("postgres://u:p@elsewhere:5432/own")
        );
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
        assert_eq!(r.database_url, None);
    }
}
