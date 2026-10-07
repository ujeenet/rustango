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
pub(super) struct Row {
    pub(super) step: String,
    pub(super) status: String,
    pub(super) message: String,
}

/// A migration event as a run-log line. Error text sits in `cause`,
/// which [`Self::stored`] logs and never stores: operators read the log (#2209).
pub(super) struct LogLine {
    slug: Option<String>,
    step: &'static str,
    status: &'static str,
    text: String,
    cause: Option<String>,
}

impl LogLine {
    /// The row to store; a `cause` is logged with the org and run id.
    pub(super) fn stored(self, run_id: i64) -> Row {
        let message = match &self.cause {
            Some(cause) => super::operator_console::withheld_in(
                &tracing::error_span!(
                    "migrate_run",
                    org = self.slug.as_deref().unwrap_or(""),
                    run_id
                ),
                "tenancy::migrate_run",
                &self.text,
                cause,
            ),
            None => self.text,
        };
        Row {
            step: self.step.into(),
            status: self.status.into(),
            message,
        }
    }
}

/// The one renderer of migration events for a stored run log.
pub(super) fn log_line(event: &TenantMigrationEvent) -> LogLine {
    use crate::migrate::MigrationEvent as E;
    let line = |slug: Option<&str>, step, status, text: String, cause: Option<&String>| LogLine {
        slug: slug.map(ToOwned::to_owned),
        step,
        status,
        text,
        cause: cause.cloned(),
    };
    match event {
        TenantMigrationEvent::Planned { tenants } => line(
            None,
            "plan",
            "info",
            format!("{tenants} tenant(s) to migrate"),
            None,
        ),
        TenantMigrationEvent::TenantStarted { slug, index, total } => line(
            Some(slug),
            "tenant",
            "started",
            format!("{slug} ({index}/{total})"),
            None,
        ),
        TenantMigrationEvent::Migration { slug, chain, event } => {
            let chain = chain_name(*chain);
            match event {
                E::Planned { total } => line(
                    Some(slug),
                    "migration",
                    "info",
                    format!("{slug} {chain}: {total} pending"),
                    None,
                ),
                E::Started { name, .. } => line(
                    Some(slug),
                    "migration",
                    "info",
                    format!("{slug} {chain}: {name} started"),
                    None,
                ),
                E::Finished { name, outcome, .. } => line(
                    Some(slug),
                    "migration",
                    "info",
                    format!("{slug} {chain}: applied {name} ({outcome:?})"),
                    None,
                ),
                E::Failed { name, error, .. } => line(
                    Some(slug),
                    "migration",
                    "failed",
                    format!("{slug} {chain}: {name} failed"),
                    Some(error),
                ),
            }
        }
        TenantMigrationEvent::TenantFinished {
            slug,
            applied,
            error,
            ..
        } => match error {
            None => line(
                Some(slug),
                "tenant",
                "ok",
                format!("{slug}: {applied} applied"),
                None,
            ),
            Some(e) => line(
                Some(slug),
                "tenant",
                "failed",
                format!("{slug} failed"),
                Some(e),
            ),
        },
    }
}

const fn chain_name(chain: tenant_migrate::Chain) -> &'static str {
    match chain {
        tenant_migrate::Chain::System => "system",
        tenant_migrate::Chain::Project => "app",
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
        let _ = tx.send(log_line(&event).stored(run_id));
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
    // Operator-safe, like the event rows (#2209).
    let error = result.as_ref().err().map(|e| {
        e.user_facing().unwrap_or_else(|| {
            super::operator_console::withheld_in(
                &tracing::error_span!("migrate_run", org = slug.unwrap_or(""), run_id),
                "tenancy::migrate_run",
                "Migration failed",
                e,
            )
        })
    });
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
