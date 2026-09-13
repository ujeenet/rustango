//! Running tenant migrations against a recorded, streamable run.
//!
//! The migrate seams report through a synchronous observer called
//! inside the migrate lock, so it cannot await a write. Provisioning
//! solved that by buffering and flushing at the end, which is fine for
//! a run measured in seconds and useless for a batch across hundreds of
//! tenants — nobody watching would see anything until it finished.
//!
//! Here the observer sends down a channel and a task drains it, so
//! events land while the migration is still going and the run's SSE
//! stream shows them.

use std::path::Path;

use sqlx::Database;

use super::error::TenancyError;
use super::migrate::{self as tenant_migrate, TenantMigrationEvent};
use super::org::Org;
use super::pools::TenantPools;
use super::provision_store::{self as store, RunState};
use crate::core::Column as _;
use crate::sql::FetcherPool as _;

/// One event, flattened for the run's event table.
struct Row {
    step: String,
    status: String,
    message: String,
}

fn flatten(event: &TenantMigrationEvent) -> Row {
    match event {
        TenantMigrationEvent::Planned { tenants } => Row {
            step: "plan".into(),
            status: "info".into(),
            message: format!("{tenants} tenant(s) to migrate"),
        },
        TenantMigrationEvent::TenantStarted { slug, index, total } => Row {
            step: "tenant".into(),
            status: "started".into(),
            message: format!("{slug} ({index}/{total})"),
        },
        TenantMigrationEvent::Migration { slug, chain, event } => Row {
            step: "migration".into(),
            status: "info".into(),
            message: format!("{slug} {}: {}", chain_name(*chain), describe(event)),
        },
        TenantMigrationEvent::TenantFinished {
            slug,
            applied,
            error,
            ..
        } => error.as_ref().map_or_else(
            || Row {
                step: "tenant".into(),
                status: "ok".into(),
                message: format!("{slug}: {applied} applied"),
            },
            |e| Row {
                step: "tenant".into(),
                status: "failed".into(),
                message: format!("{slug}: {e}"),
            },
        ),
    }
}

const fn chain_name(chain: tenant_migrate::Chain) -> &'static str {
    match chain {
        tenant_migrate::Chain::System => "system",
        tenant_migrate::Chain::Project => "app",
    }
}

fn describe(event: &crate::migrate::MigrationEvent) -> String {
    use crate::migrate::MigrationEvent as E;
    match event {
        E::Planned { total } => format!("{total} pending"),
        E::Started { name, .. } => format!("{name} started"),
        E::Finished { name, outcome, .. } => format!("applied {name} ({outcome:?})"),
        E::Failed { name, error, .. } => format!("{name} failed: {error}"),
    }
}

/// Migrate one tenant, or every active one, recording into `run_id`.
///
/// `slug` picks a single tenant; `None` migrates the active batch.
/// Errors are recorded on the run and returned — the caller is usually
/// a spawned task with nowhere to report them.
///
/// # Errors
/// An unreachable tenant database, a failing migration, or a storage
/// mode this build cannot serve.
pub async fn migrate_in_run<DB: Database>(
    pools: &TenantPools<DB>,
    dir: &Path,
    registry_url: &str,
    run_id: i64,
    slug: Option<&str>,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let registry = pools.registry_pool();

    // The observer is synchronous and runs inside the migrate lock, so
    // it hands events off rather than writing them.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Row>();
    let writer_registry = registry.clone();
    let writer = tokio::spawn(async move {
        let mut seq = 1i64;
        while let Some(row) = rx.recv().await {
            if let Err(e) = store::append_event(
                &writer_registry,
                run_id,
                seq,
                &row.step,
                &row.status,
                &row.message,
            )
            .await
            {
                tracing::warn!(
                    target: "rustango::tenancy::migrate_run",
                    run_id, error = %e,
                    "could not record a migration event; the run continues"
                );
            }
            seq += 1;
        }
    });

    let observer = move |event: TenantMigrationEvent| {
        // A closed receiver means the writer died; the migration is
        // still worth finishing.
        let _ = tx.send(flatten(&event));
    };

    let result = match slug {
        Some(slug) => migrate_one(pools, &registry, dir, registry_url, slug, &observer).await,
        None => tenant_migrate::migrate_tenants_dyn_with_progress(
            pools,
            dir,
            registry_url,
            Some(&observer),
        )
        .await
        .map(|_| ()),
    };

    // Dropping the observer closes the channel, which ends the writer.
    drop(observer);
    let _ = writer.await;

    let state = if result.is_ok() {
        RunState::Succeeded
    } else {
        RunState::Failed
    };
    let error = result.as_ref().err().map(ToString::to_string);
    if let Err(e) = store::finish_run(&registry, run_id, state, error.as_deref()).await {
        tracing::warn!(
            target: "rustango::tenancy::migrate_run",
            run_id, error = %e,
            "could not close the run"
        );
    }
    result
}

async fn migrate_one<DB: Database>(
    pools: &TenantPools<DB>,
    registry: &crate::sql::Pool,
    dir: &Path,
    registry_url: &str,
    slug: &str,
    observer: &dyn tenant_migrate::TenantMigrationObserver,
) -> Result<(), TenancyError>
where
    crate::sql::Pool: From<sqlx::Pool<DB>>,
{
    let rows: Vec<Org> = Org::objects()
        .where_(Org::slug.eq(slug.to_owned()))
        .fetch(registry)
        .await?;
    let org = rows
        .into_iter()
        .next()
        .ok_or_else(|| TenancyError::Validation(format!("no tenant `{slug}`")))?;

    // The batch seam frames each tenant; a single run should read the
    // same way rather than starting mid-narrative.
    observer.on_event(TenantMigrationEvent::Planned { tenants: 1 });
    observer.on_event(TenantMigrationEvent::TenantStarted {
        slug: org.slug.clone(),
        index: 1,
        total: 1,
    });
    let applied =
        tenant_migrate::migrate_one_tenant(pools, &org, dir, registry_url, Some(observer)).await;
    observer.on_event(TenantMigrationEvent::TenantFinished {
        slug: org.slug.clone(),
        index: 1,
        total: 1,
        applied: applied.as_ref().map(Vec::len).unwrap_or(0),
        error: applied.as_ref().err().map(ToString::to_string),
    });
    applied.map(|_| ())
}
