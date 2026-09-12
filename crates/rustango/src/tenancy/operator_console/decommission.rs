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
use super::{urlencoding_lite, ConsoleState};

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
        Err(e) => back_to_orgs(None, Some(&e.to_string())),
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

    let action = Action::Purge {
        purge_database: form.purge_database.is_some(),
    };
    match pools.decommission(&slug, action).await {
        Ok(report) => {
            audit(&state, &op, &slug, "tenant_purge").await;
            let mut msg = format!("purged `{slug}`");
            if let Some(schema) = &report.schema_dropped {
                let _ = write!(msg, " — dropped schema `{schema}`");
            }
            if report.database_dropped.is_some() {
                msg.push_str(" — dropped its database");
            }
            for note in &report.notes {
                let _ = write!(msg, " — {note}");
            }
            back_to_orgs(Some(&msg), None)
        }
        // Back to the tenant, not the list: it still exists, and the
        // reason usually names something to change.
        Err(e) => back_to_tenant(&slug, &e.to_string()),
    }
}

fn not_available() -> Response<Body> {
    (
        StatusCode::NOT_FOUND,
        "this console is read-only".to_owned(),
    )
        .into_response()
}

fn back_to_orgs(notice: Option<&str>, error: Option<&str>) -> Response<Body> {
    let query = match (notice, error) {
        (Some(n), _) => format!("?notice={}", urlencoding_lite(n)),
        (_, Some(e)) => format!("?error={}", urlencoding_lite(e)),
        _ => String::new(),
    };
    Redirect::to(&format!("/orgs{query}")).into_response()
}

fn back_to_tenant(slug: &str, error: &str) -> Response<Body> {
    Redirect::to(&format!(
        "/orgs/{}/edit?error={}",
        urlencoding_lite(slug),
        urlencoding_lite(error)
    ))
    .into_response()
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
