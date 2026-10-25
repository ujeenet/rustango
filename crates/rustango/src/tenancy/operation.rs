//! Work on an existing tenant, a purge or a password rotation, reported
//! step by step and recorded like a provisioning run, so the console's
//! run page and stream show it and `manage` prints it live.

use super::error::TenancyError;
use super::provision::StepStatus;
use super::provision_store::{self as store, RunLog, RunState, RunText};

/// One step of a purge or a password rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperationStep {
    /// Every refusal, before anything changes.
    Check,
    /// `active = false`, so the tenant stops serving first.
    Deactivate,
    /// Delete the tenant's media objects from storage.
    PurgeMedia,
    DropSchema,
    DropDatabase,
    DropLogin,
    /// Delete the `rustango_tenant_databases` record.
    DeleteRecord,
    /// Delete the `rustango_orgs` row.
    RemoveOrg,
    /// Give the login a new password on the server.
    AlterPassword,
    /// Seal the new URL and store it over the old one.
    StorePassword,
    /// Drop cached pools so new connections use the new password.
    InvalidatePools,
    /// Forget the retained old password (MySQL 8.0.14+).
    DiscardOldPassword,
    /// Put the old password back after a failed store.
    Revert,
}

impl OperationStep {
    /// Stable wire name, stored in `rustango_provisioning_events.step`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Deactivate => "deactivate",
            Self::PurgeMedia => "purge_media",
            Self::DropSchema => "drop_schema",
            Self::DropDatabase => "drop_database",
            Self::DropLogin => "drop_login",
            Self::DeleteRecord => "delete_record",
            Self::RemoveOrg => "remove_org",
            Self::AlterPassword => "alter_password",
            Self::StorePassword => "store_password",
            Self::InvalidatePools => "invalidate_pools",
            Self::DiscardOldPassword => "discard_old_password",
            Self::Revert => "revert",
        }
    }
}

/// A step transition, for a watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OperationEvent {
    pub step: OperationStep,
    pub status: StepStatus,
}

/// Receives [`OperationEvent`]s as they happen. Must not block.
pub trait OperationObserver: Send + Sync {
    fn on_event(&self, event: OperationEvent);
}

impl<F> OperationObserver for F
where
    F: Fn(OperationEvent) + Send + Sync,
{
    fn on_event(&self, event: OperationEvent) {
        self(event);
    }
}

/// Reports steps to an observer and, when persisting, to a run. Stored
/// text is fixed, validated names, or an operator-facing error: failure
/// causes are logged under a reference, never stored.
pub(crate) struct OpReporter<'a> {
    observer: Option<&'a dyn OperationObserver>,
    store: Option<Store<'a>>,
}

struct Store<'a> {
    registry: &'a crate::sql::Pool,
    run_id: i64,
    slug: String,
    log: RunLog,
    seq: std::sync::atomic::AtomicI64,
    failure: std::sync::Mutex<Option<RunText>>,
}

impl<'a> OpReporter<'a> {
    pub(crate) fn new(observer: Option<&'a dyn OperationObserver>) -> Self {
        Self {
            observer,
            store: None,
        }
    }

    /// Also record into run `run_id`.
    pub(crate) fn persisting(
        mut self,
        registry: &'a crate::sql::Pool,
        run_id: i64,
        slug: &str,
    ) -> Self {
        self.store = Some(Store {
            registry,
            run_id,
            slug: slug.to_owned(),
            log: RunLog::new(run_id),
            seq: std::sync::atomic::AtomicI64::new(1),
            failure: std::sync::Mutex::new(None),
        });
        self
    }

    async fn emit(&self, step: OperationStep, status: StepStatus, text: &RunText) {
        if let Some(s) = &self.store {
            let seq = s.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Err(e) =
                store::append_event_text(s.registry, s.run_id, seq, step.as_str(), status.as_str(), text)
                    .await
            {
                tracing::warn!(target: "rustango::tenancy::operation", run_id = s.run_id, error = %e, "could not record a step; the run continues");
            }
        }
        if let Some(o) = self.observer {
            o.on_event(OperationEvent { step, status });
        }
    }

    pub(crate) async fn started(&self, step: OperationStep) {
        self.emit(step, StepStatus::Started, &RunText::default()).await;
    }

    pub(crate) async fn ok(&self, step: OperationStep) {
        self.emit(step, StepStatus::Ok, &RunText::default()).await;
    }

    /// `Ok`, with what was done.
    pub(crate) async fn done(&self, step: OperationStep, text: RunText) {
        self.emit(step, StepStatus::Ok, &text).await;
    }

    pub(crate) async fn skipped(&self, step: OperationStep, why: &'static str) {
        self.emit(step, StepStatus::Skipped(why.to_owned()), &RunText::fixed(why))
            .await;
    }

    /// Report `e` as `step`'s failure and hand it back. The observer gets
    /// the full text; the run gets the operator-safe one.
    pub(crate) async fn fail<T>(&self, step: OperationStep, e: TenancyError) -> Result<T, TenancyError> {
        let text = match &self.store {
            Some(s) => {
                let text = s.log.failure(&s.slug, "Step failed", &e);
                if let Ok(mut last) = s.failure.lock() {
                    *last = Some(text.clone());
                }
                text
            }
            None => RunText::default(),
        };
        self.emit(step, StepStatus::Failed(e.to_string()), &text).await;
        Err(e)
    }

    /// Close the run, if recording, as succeeded or failed with `e`.
    pub(crate) async fn finish(&self, e: Option<&TenancyError>) {
        let Some(s) = &self.store else {
            return;
        };
        let (state, error) = match e {
            None => (RunState::Succeeded, None),
            Some(e) => {
                let last = s.failure.lock().ok().and_then(|f| f.clone());
                let text = RunText::user_facing(e)
                    .or(last)
                    .unwrap_or_else(|| s.log.failure(&s.slug, "Failed", e));
                (RunState::Failed, Some(text))
            }
        };
        if let Err(e) = store::finish_run_text(s.registry, s.run_id, state, error.as_ref()).await {
            tracing::warn!(target: "rustango::tenancy::operation", run_id = s.run_id, error = %e, "could not close the run");
        }
    }
}
