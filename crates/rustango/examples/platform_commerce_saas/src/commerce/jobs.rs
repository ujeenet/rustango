//! Background jobs.
//!
//! **Twin file.** Identical in `platform_commerce_saas`. The single-
//! tenant app registers one pool under the slug `__single__`; the SaaS
//! app registers one per tenant. Nothing else differs, which is the
//! point: the payload carries the routing key either way.
//!
//! ## Why every job takes a `tenant` field
//!
//! `Job::run(&self)` receives **only the deserialized payload** — no
//! pool, no tenant, no request context. Workers are `tokio::spawn`ed
//! and inherit no task-local state. `docs/jobs.md` gets isolation from
//! "the pool the queue was built on", but a handler cannot see that
//! pool, and `register::<T>()` installs one handler shared by every
//! queue in the process. So the payload carries a routing key and
//! `run` resolves its pool first — the fallback the same doc names.
//!
//! Each job stamps the tenant it *believed* it was running for into
//! `ShipmentEvent.tenant_slug`. After a soak, sweeping every tenant and
//! asserting that no row names a different tenant is a direct
//! cross-tenant-leak check on a pattern the framework forces on every
//! multi-tenant app.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock, RwLock};

use rustango::core::{Column as _, F};
use rustango::jobs::{Job, JobError};
use rustango::sql::{Auto, ForeignKey, Pool, UpdaterPool as _};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use super::models::{InventoryItem, ShipmentEvent};

/// The slug the single-tenant app files everything under.
///
/// Unused in the SaaS twin, where every pool is keyed by a real tenant
/// slug — but this file is byte-identical to `platform_commerce`'s on
/// purpose, so the constant stays rather than the two drifting apart.
#[allow(dead_code)]
pub const SINGLE: &str = "__single__";

/// Run a job body with its tenant published to the logging layer.
///
/// `rustango::tenant_log` gives a request an ambient tenant: the
/// resolver calls `record`, and everything logged under that request —
/// the access log, the ORM, anything — carries `tenant=`. Its scope is
/// per-task and `tokio::spawn` does not inherit it, which is exactly
/// what a queue worker is. So a job got `tenant=-` on every line the
/// framework emitted, and only the ones this file hand-annotated named
/// a tenant at all. The framework's own docs mark this as out of scope
/// (issues #1229 / #1223); the payload is the tenant context a job has,
/// so this is where it becomes ambient.
///
/// The span declares `tenant` as an empty field because `Span::record`
/// only writes fields the metadata already declares — omit it and
/// `record` silently does nothing to the span half.
async fn with_tenant<F, T>(slug: &str, job: &'static str, fut: F) -> T
where
    F: Future<Output = T>,
{
    let span = tracing::info_span!("job", job, tenant = tracing::field::Empty);
    rustango::tenant_log::scope(
        async {
            rustango::tenant_log::record(slug, None);
            fut.await
        }
        .instrument(span),
    )
    .await
}

/// Pools reachable from a job handler, by tenant slug.
static POOLS: OnceLock<RwLock<HashMap<String, Pool>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Pool>> {
    POOLS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Make a pool reachable from `run()`. Call once per tenant at boot,
/// and again whenever a tenant is provisioned.
pub fn register_pool(slug: &str, pool: Pool) {
    tracing::info!(tenant = %slug, dialect = pool.dialect().name(), "job pool registered");
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(slug.to_owned(), pool);
}

/// Drop a tenant's pool when its queue is retired, returning it so the
/// caller can close it.
///
/// Without this the map grows for the life of the process: a tenant
/// deactivated at 09:00 still has a live pool holding connections at
/// midnight, and if its database was dropped every one of them is
/// broken. Unused in the single-tenant twin, which keeps this file
/// byte-identical to the SaaS one.
#[allow(dead_code)]
pub fn unregister_pool(slug: &str) -> Option<Pool> {
    let removed = registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(slug);
    if removed.is_some() {
        tracing::info!(tenant = %slug, "job pool unregistered");
    }
    removed
}

/// Slugs currently registered — the worker's stranded-row detector
/// reads this.
pub fn registered_slugs() -> Vec<String> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect()
}

fn pool_for(slug: &str) -> Result<Pool, JobError> {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(slug)
        .cloned()
        // Fatal, not Retryable: an unregistered slug does not become
        // registered by waiting, so retrying would burn all attempts
        // and then dead-letter anyway, four backoffs later.
        .ok_or_else(|| JobError::Fatal(format!("no pool registered for tenant `{slug}`")))
}

async fn record_event(
    pool: &Pool,
    order_id: i64,
    kind: &str,
    slug: &str,
    detail: Option<String>,
) -> Result<(), JobError> {
    // `save` on the model, not hand-written SQL. The ORM emits the
    // dialect's own placeholders and binds through the same path the
    // ViewSets use, so this exercises the code a reader would actually
    // write — and `at` is `#[rustango(auto_now_add)]`, so the framework
    // stamps it rather than the job choosing between `NOW()`,
    // `CURRENT_TIMESTAMP` and `datetime('now')`.
    //
    // `detail` is a nullable TEXT column, so its NULL was never the
    // #1450 case — text is what the old binder assumed.
    // `Order::assigned_picker_id` is the one that mattered.
    let mut event = ShipmentEvent {
        id: Auto::Unset,
        order_id: ForeignKey::unloaded(order_id),
        kind: kind.to_owned(),
        tenant_slug: slug.to_owned(),
        detail,
        at: Auto::Unset,
    };
    // `save_pool`, not `save`: the bare name is Postgres-typed, and this
    // app runs on all three dialects.
    event
        .save_pool(pool)
        .await
        .map_err(|e| JobError::Retryable(e.to_string()))?;
    Ok(())
}

/// The happy path, and the bulk of the soak's job volume.
#[derive(Serialize, Deserialize)]
pub struct OrderConfirmation {
    pub tenant: String,
    pub order_id: i64,
}

#[async_trait::async_trait]
impl Job for OrderConfirmation {
    const NAME: &'static str = "commerce:order_confirmation";

    async fn run(&self) -> Result<(), JobError> {
        // `tenant` is on the span now, not repeated per line — which is
        // the point: everything logged inside, including the ORM's own
        // events, carries it without this file annotating anything.
        with_tenant(&self.tenant, Self::NAME, async {
            // DEBUG for the per-job trace, INFO for the outcome. At soak
            // volumes DEBUG is thousands of lines a minute, which is why
            // it is off unless `RUST_LOG` asks for it.
            tracing::debug!(order = self.order_id, "confirming order");
            let pool = pool_for(&self.tenant)?;
            record_event(&pool, self.order_id, "confirmed", &self.tenant, None).await?;
            tracing::info!(order = self.order_id, "order confirmed");
            Ok(())
        })
        .await
    }
}

/// Contends with `PATCH /api/v1/inventory` on purpose — and on SQLite,
/// with every other writer in the process, since SQLite serialises
/// writes globally.
#[derive(Serialize, Deserialize)]
pub struct InventoryReconciliation {
    pub tenant: String,
    pub order_id: i64,
    pub product_id: i64,
}

#[async_trait::async_trait]
impl Job for InventoryReconciliation {
    const NAME: &'static str = "commerce:inventory_reconcile";
    const MAX_ATTEMPTS: u32 = 3;

    async fn run(&self) -> Result<(), JobError> {
        with_tenant(&self.tenant, Self::NAME, async {
        tracing::debug!(product = self.product_id, "reconciling inventory");
        let pool = pool_for(&self.tenant)?;
        // `F("on_hand") - 1` in the UPDATE, not a read-modify-write: the
        // decrement happens inside the statement, so two workers
        // reconciling the same product cannot both read 5 and both
        // write 4. `gt("on_hand", 0)` keeps it from going negative,
        // which is also what the table's CHECK constraint enforces —
        // the guard is here so a contended row is a no-op rather than a
        // constraint violation.
        //
        // The ORM emits each dialect's own placeholder and arithmetic;
        // this file previously built the statement by hand, which meant
        // the example demonstrated string formatting rather than the
        // framework.
        InventoryItem::objects()
            .filter("product_id", self.product_id)
            .where_(InventoryItem::on_hand.gt(0_i64))
            .update()
            .set_expr("on_hand", F("on_hand") - 1_i64)
            .execute_pool(&pool)
            .await
            // Retryable: a lock timeout or serialization failure is exactly
            // what a backoff is for.
            .map_err(|e| JobError::Retryable(e.to_string()))?;
        record_event(&pool, self.order_id, "reconciled", &self.tenant, None).await?;
        tracing::info!(
            order = self.order_id, product = self.product_id,
            "inventory reconciled"
        );
        Ok(())
        })
        .await
    }
}

/// Fails on purpose, **deterministically**.
///
/// `MAX_ATTEMPTS = 4` is one run plus three retries — a *total*
/// ceiling, not a retry ceiling (#1410). Backoff is 1s, 2s, 4s.
///
/// The failure is a hash of the order id, not a random draw, so the
/// harness can compute the expected dead-letter count exactly. A range
/// is where an off-by-one in `MAX_ATTEMPTS` hides: "between 90 and 110
/// dead letters" passes whether the ceiling is 4 or 5.
#[derive(Serialize, Deserialize)]
pub struct FlakyPaymentCapture {
    pub tenant: String,
    pub order_id: i64,
    /// Percentage of orders that fail every attempt, 0-100.
    ///
    /// Kept low deliberately. At 10% a 30-minute run produced 6308
    /// ERROR lines in five minutes and buried everything real; 2% still
    /// yields hundreds of samples, which is far more than enough to
    /// pin an attempt count.
    pub fail_ratio_pct: u8,
}

impl FlakyPaymentCapture {
    /// Deliberately not a hasher from `std::collections` — `DefaultHasher`
    /// is explicitly not guaranteed stable across releases, and this
    /// number has to be reproducible for the harness to predict it.
    #[must_use]
    pub fn always_fails(order_id: i64, fail_ratio_pct: u8) -> bool {
        let mut h = order_id as u64;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        (h % 100) < u64::from(fail_ratio_pct)
    }
}

#[async_trait::async_trait]
impl Job for FlakyPaymentCapture {
    const NAME: &'static str = "commerce:payment_capture";
    const MAX_ATTEMPTS: u32 = 4;

    async fn run(&self) -> Result<(), JobError> {
        with_tenant(&self.tenant, Self::NAME, async {
            let pool = pool_for(&self.tenant)?;
            if Self::always_fails(self.order_id, self.fail_ratio_pct) {
                // One line per *attempt*, so the 1s/2s/4s backoff
                // sequence is visible in the log rather than only its
                // dead-letter.
                tracing::debug!(
                    order = self.order_id,
                    "payment declined (injected failure) — will retry"
                );
                return Err(JobError::Retryable(format!(
                    "payment gateway declined order {}",
                    self.order_id
                )));
            }
            record_event(&pool, self.order_id, "captured", &self.tenant, None).await?;
            tracing::info!(order = self.order_id, "payment captured");
            Ok(())
        })
        .await
    }
}

/// `JobError::Fatal` must bypass retry entirely — one attempt, then
/// dead-letter, regardless of `MAX_ATTEMPTS`.
#[derive(Serialize, Deserialize)]
pub struct FatalProbe {
    pub tenant: String,
    pub order_id: i64,
}

#[async_trait::async_trait]
impl Job for FatalProbe {
    const NAME: &'static str = "commerce:fatal_probe";

    async fn run(&self) -> Result<(), JobError> {
        // Wrapped like the rest even though it only errors: the queue
        // logs the failure, and that line should name the tenant too.
        with_tenant(&self.tenant, Self::NAME, async {
            Err(JobError::Fatal(format!(
                "unrecoverable by construction (order {})",
                self.order_id
            )))
        })
        .await
    }
}

/// Is this dead letter one the soak *caused on purpose*?
///
/// Two of the four job types fail by construction — that is how the
/// run proves `MAX_ATTEMPTS` is a total-attempt ceiling and that
/// `JobError::Fatal` bypasses retry. Their dead letters are assertions
/// passing, not faults.
///
/// Logging them at ERROR was a mistake worth not repeating. At a 10%
/// failure ratio the fleet produced **6308 ERROR lines in five
/// minutes**, and a real failure in that stream would have been
/// invisible — which is the precise failure mode this whole release is
/// about. Expected failures log at WARN; ERROR is reserved for a dead
/// letter nobody asked for, so one of those still stands out.
#[must_use]
pub fn is_expected_failure(job_name: &str) -> bool {
    job_name == FlakyPaymentCapture::NAME || job_name == FatalProbe::NAME
}

/// The dead-letter handler every queue in both apps installs.
///
/// Shared so the single-tenant app, the SaaS supervisor and the worker
/// binary cannot disagree about severity — they did, and the
/// single-tenant in-process queue had no handler at all, so its dead
/// letters fell through to the framework's generic
/// "no callback configured" log.
pub fn log_dead_letter(tenant: Option<&str>, dl: &rustango::jobs::JobDeadLetter) {
    let tenant = tenant.unwrap_or(SINGLE);
    if is_expected_failure(dl.name) {
        tracing::warn!(
            tenant = %tenant, job = dl.name, attempts = dl.attempts,
            expected = true,
            "job dead-lettered (deliberate: this is the soak's own failure injection)"
        );
    } else {
        tracing::error!(
            tenant = %tenant, job = dl.name, attempts = dl.attempts,
            error = %dl.error,
            "job dead-lettered"
        );
    }
}

/// Register every job type on a queue, and install the dead-letter
/// handler.
///
/// **All four, in every process that calls `start()`.** A database-queue
/// worker that picks up a row whose `NAME` is not registered here logs
/// and returns *without unlocking the row*. The row is then stranded
/// until a `reclaim_stuck_jobs_pool` sweep frees it — whereupon it is
/// picked up and stranded again — and it never appears in
/// `pending_count()`. A web tier that only dispatches and a worker tier
/// that only registers some types produces a queue that looks empty and
/// drains nothing.
pub async fn register_all<Q: rustango::jobs::JobQueue>(queue: &Arc<Q>) {
    queue.register::<OrderConfirmation>().await;
    queue.register::<InventoryReconciliation>().await;
    queue.register::<FlakyPaymentCapture>().await;
    queue.register::<FatalProbe>().await;
}

/// The names `register_all` installs — for the worker's stranded-row
/// detector, which compares them against `SELECT DISTINCT name FROM
/// rustango_jobs`.
#[must_use]
pub fn registered_job_names() -> [&'static str; 4] {
    [
        OrderConfirmation::NAME,
        InventoryReconciliation::NAME,
        FlakyPaymentCapture::NAME,
        FatalProbe::NAME,
    ]
}

#[cfg(test)]
mod tests {
    use super::FlakyPaymentCapture;

    /// The harness predicts the dead-letter count from this function,
    /// so it has to be a pure function of the id — not of the clock,
    /// the thread, or a `DefaultHasher` whose output may change between
    /// Rust releases.
    #[test]
    fn failure_is_deterministic_and_roughly_the_requested_ratio() {
        for id in [1_i64, 2, 3, 99, 1000] {
            let first = FlakyPaymentCapture::always_fails(id, 10);
            for _ in 0..100 {
                assert_eq!(
                    FlakyPaymentCapture::always_fails(id, 10),
                    first,
                    "order {id} must fail or succeed consistently"
                );
            }
        }
        let failures = (0..10_000_i64)
            .filter(|&id| FlakyPaymentCapture::always_fails(id, 10))
            .count();
        assert!(
            (800..1200).contains(&failures),
            "10% of 10000 should be ~1000, got {failures}"
        );
        // 0 and 100 have to be exact, because the harness uses them to
        // assert "no dead letters" and "all dead letters".
        assert_eq!(
            (0..1000_i64)
                .filter(|&id| FlakyPaymentCapture::always_fails(id, 0))
                .count(),
            0
        );
        assert_eq!(
            (0..1000_i64)
                .filter(|&id| FlakyPaymentCapture::always_fails(id, 100))
                .count(),
            1000
        );
    }
}
