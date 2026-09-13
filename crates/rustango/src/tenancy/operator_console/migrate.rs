//! Running tenant migrations from the console.
//!
//! After a deploy every tenant needs migrating, and that meant a shell
//! on the production host. The run is recorded and streamed like a
//! provisioning run, so it survives a reload and a second pod.
//!
//! Unlike provisioning, the work is spawned rather than awaited: a
//! batch across hundreds of tenants is minutes, not the seconds an
//! operator can watch a form for. The run is opened first so the
//! redirect has somewhere to point.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Response, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::Extension;

use super::super::auth;
use super::super::provision_store as store;
use super::ConsoleState;

/// Migrate every active tenant.
pub(super) async fn migrate_all(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
) -> Response<Body> {
    start(&state, &op, None).await
}

/// Migrate one tenant.
pub(super) async fn migrate_one(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Path(slug): Path<String>,
) -> Response<Body> {
    start(&state, &op, Some(slug)).await
}

async fn start(state: &ConsoleState, op: &auth::Operator, slug: Option<String>) -> Response<Body> {
    let Some(provisioner) = state.provisioner.clone() else {
        return (
            StatusCode::NOT_FOUND,
            "this console cannot run migrations".to_owned(),
        )
            .into_response();
    };

    let run =
        match store::open_migrate_run(&state.registry, slug.as_deref(), Some(&op.username)).await {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not open a run: {e}"),
                )
                    .into_response();
            }
        };
    let run_id = run.id.get().copied().unwrap_or_default();

    // Detached: the response is a link to the run, not the result.
    // Failures are recorded on the run, which is the only place anyone
    // will look for them.
    tokio::spawn(async move {
        let _ = provisioner.migrate_in_run(run_id, slug.as_deref()).await;
    });

    Redirect::to(&format!("/orgs/provision/{run_id}")).into_response()
}
