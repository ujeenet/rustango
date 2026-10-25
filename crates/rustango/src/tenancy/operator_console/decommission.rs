//! Taking a tenant out of service from the console.
//!
//! Two actions with very different consequences, so the page keeps them
//! visibly apart: deactivating is one click and reversible, purging
//! requires typing the slug and destroys the storage.
//!
//! The typed slug is the same guard `purge-tenant --confirm` uses on the
//! command line. It is not security — an operator who can reach this page
//! can already do the damage — it is a pause between intent and an
//! unrecoverable act.

use axum::body::Body;
use axum::extract::{Form, Path, State};
use axum::http::{Response, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::Extension;
use serde::Deserialize;

use super::super::auth;
use super::super::decommission::Action;
use super::super::provision_store::{self as store, RunKind};
use super::flash::{self, Flash};
use super::{urlencoding_lite, withheld, ConsoleState};

#[derive(Deserialize)]
pub(super) struct PurgeForm {
    /// Must repeat the slug verbatim.
    #[serde(default)]
    confirm: String,
    /// Present for a database-mode tenant, whose purge is a
    /// `DROP DATABASE` rather than a `DROP SCHEMA`.
    #[serde(default)]
    purge_database: Option<String>,
}

/// Soft-delete: `active = false`, nothing destroyed.
pub(super) async fn deactivate(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(slug): Path<String>,
) -> Response<Body> {
    let Some(pools) = state.pools.clone() else {
        return not_available();
    };
    match pools.decommission(&slug, Action::Deactivate).await {
        Ok(report) => {
            audit(&state, &op, &slug, "tenant_deactivate").await;
            let msg = if report.no_change {
                format!("`{slug}` was already inactive")
            } else {
                format!("deactivated `{slug}` — its data is untouched")
            };
            back_to_orgs(Some(&msg), None)
        }
        Err(e) => {
            let msg = e.user_facing().unwrap_or_else(|| {
                withheld(
                    "operator_console::deactivate",
                    &slug,
                    "Could not deactivate",
                    &e,
                )
            });
            back_to_orgs(None, Some(&msg))
        }
    }
}

/// Hard-delete: drop the storage, remove the row.
pub(super) async fn purge(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(slug): Path<String>,
    Form(form): Form<PurgeForm>,
) -> Response<Body> {
    use std::fmt::Write as _;

    let Some(pools) = state.pools.clone() else {
        return not_available();
    };
    if form.confirm != slug {
        return back_to_tenant(
            &slug,
            &format!("Type `{slug}` exactly to confirm. Nothing was destroyed."),
        );
    }

    // With a provisioner the purge is a recorded run, followed live.
    if let Some(provisioner) = state.provisioner.clone() {
        return purge_in_run(&state, &op, provisioner, slug, form.purge_database.is_some()).await;
    }
    let action = Action::Purge {
        purge_database: form.purge_database.is_some(),
    };
    match pools.decommission(&slug, action).await {
        Ok(report) => {
            audit(&state, &op, &slug, "tenant_purge").await;
            let mut msg = format!("purged `{slug}`");
            if let Err(e) =
                super::super::branding::delete_brand_assets(&slug, &state.brand_storage).await
            {
                let kept = withheld(
                    "operator_console::purge",
                    &slug,
                    "brand files left in place",
                    &e,
                );
                let _ = write!(msg, " — {kept}");
            }
            if let Some(schema) = &report.schema_dropped {
                let _ = write!(msg, " — dropped schema `{schema}`");
            }
            if report.database_dropped.is_some() {
                msg.push_str(" — dropped its database");
            }
            if report.media_deleted > 0 {
                let _ = write!(msg, " — deleted {} media objects", report.media_deleted);
            }
            // Too many for a flash message; the log keeps every key.
            for (disk, key) in &report.media_left {
                tracing::warn!(target: "rustango::tenancy::decommission", tenant = %slug, disk, key, "media object left in storage");
            }
            for note in &report.notes {
                let _ = write!(msg, " — {note}");
            }
            back_to_orgs(Some(&msg), None)
        }
        // Back to the tenant, not the list: it still exists, and the
        // reason usually names something to change.
        Err(e) => {
            let msg = e.user_facing().unwrap_or_else(|| {
                withheld("operator_console::purge", &slug, "Could not purge", &e)
            });
            back_to_tenant(&slug, &msg)
        }
    }
}

/// Open a purge run, start it detached, and send the operator to its
/// page. A refusal shows there, on the `check` step.
async fn purge_in_run(
    state: &ConsoleState,
    op: &auth::Operator,
    provisioner: std::sync::Arc<dyn super::super::provision::TenantProvisioner>,
    slug: String,
    purge_database: bool,
) -> Response<Body> {
    let run = match open_run(state, op, &slug, RunKind::Purge).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let brand_storage = state.brand_storage.clone();
    tokio::spawn(async move {
        if provisioner
            .purge_in_run(run, &slug, purge_database)
            .await
            .is_ok()
        {
            if let Err(e) = super::super::branding::delete_brand_assets(&slug, &brand_storage).await
            {
                tracing::warn!(target: "rustango::tenancy::operator_console", tenant = %slug, error = %e, "brand files left in place");
            }
        }
    });
    Redirect::to(&format!("/orgs/provision/{run}")).into_response()
}

/// Give a server-made tenant's login a new password, as a recorded run.
pub(super) async fn rotate_password(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(slug): Path<String>,
) -> Response<Body> {
    let Some(provisioner) = state.provisioner.clone() else {
        return not_available();
    };
    let run = match open_run(&state, &op, &slug, RunKind::RotatePassword).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    tokio::spawn(async move {
        let _ = provisioner.rotate_password_in_run(run, &slug).await;
    });
    Redirect::to(&format!("/orgs/provision/{run}")).into_response()
}

/// Open a run of `kind` for `slug` and audit it with the run id.
async fn open_run(
    state: &ConsoleState,
    op: &auth::Operator,
    slug: &str,
    kind: RunKind,
) -> Result<i64, Response<Body>> {
    use crate::core::Column as _;
    let org = match super::super::Org::objects()
        .where_(super::super::Org::slug.eq(slug.to_owned()))
        .first(&state.registry)
        .await
    {
        Ok(Some(org)) => org,
        Ok(None) => return Err(back_to_orgs(None, Some(&format!("no tenant `{slug}`")))),
        Err(e) => return Err(super::server_error("could not read the tenant", &e)),
    };
    let run = store::open_operation_run(&state.registry, kind, &org, Some(&op.username))
        .await
        .map_err(|e| super::server_error("could not open a run", &e))?;
    let run_id = run.id.get().copied().unwrap_or_default();
    let mut extra = serde_json::Map::new();
    extra.insert("run_id".into(), serde_json::json!(run_id));
    let verb = match kind {
        RunKind::RotatePassword => "tenant_rotate_password",
        _ => "tenant_purge",
    };
    let operator_id = op.id.get().copied().unwrap_or_default();
    super::emit_op_audit(&state.registry, slug, operator_id, verb, extra).await;
    Ok(run_id)
}

fn not_available() -> Response<Body> {
    (
        StatusCode::NOT_FOUND,
        "this console is read-only".to_owned(),
    )
        .into_response()
}

fn back_to_orgs(notice: Option<&str>, error: Option<&str>) -> Response<Body> {
    match (notice, error) {
        (Some(n), _) => flash::redirect("/orgs", Flash::Notice(n)),
        (_, Some(e)) => flash::redirect("/orgs", Flash::Error(e)),
        _ => Redirect::to("/orgs").into_response(),
    }
}

fn back_to_tenant(slug: &str, error: &str) -> Response<Body> {
    flash::redirect(
        &format!("/orgs/{}/edit", urlencoding_lite(slug)),
        Flash::Error(error),
    )
}

async fn audit(state: &ConsoleState, op: &auth::Operator, slug: &str, verb: &str) {
    let operator_id = op.id.get().copied().unwrap_or_default();
    super::emit_op_audit(
        &state.registry,
        slug,
        operator_id,
        verb,
        serde_json::Map::new(),
    )
    .await;
}
