//! A [`JtiStore`] in the registry database, so a spent handoff token or an
//! ended impersonation stays spent across restarts and replicas.
//!
//! [`JtiStore`]: crate::jti_store::JtiStore

use crate::core::{Column, DeleteQuery, InsertQuery, Model as _};
use crate::jti_store::{JtiFuture, JtiStore};
use crate::sql::{ExistsPool as _, Pool};

/// One spent id; the row can go once `exp` has passed.
#[derive(crate::Model, Debug, Clone)]
#[rustango(table = "rustango_used_jti", access = "never", scope = "registry")]
pub struct UsedJti {
    #[rustango(primary_key, max_length = 128)]
    #[rustango(help_text = "The spent token id.")]
    #[rustango(verbose_name = "Token id")]
    pub jti: String,
    #[rustango(index)]
    #[rustango(help_text = "When the token expires, as unix seconds; the row is pruned after.")]
    #[rustango(verbose_name = "Expires")]
    pub exp: i64,
}

/// Longest id the column holds; a longer one is refused, not truncated.
const MAX_JTI_LEN: usize = 128;

fn column<C: Column>(_: C) -> &'static str {
    C::COLUMN
}

/// [`JtiStore`] over [`UsedJti`] rows. A read or write error counts as
/// "spent", so a database outage refuses tokens instead of replaying them.
#[derive(Clone)]
pub struct DbJtiStore {
    pool: Pool,
    /// Unix second of this store's last prune.
    last_prune: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

/// A row stays this long past `exp`, so a replica whose clock runs fast
/// never prunes one a slower replica still checks.
const PRUNE_MARGIN_SECS: i64 = 300;

/// Seconds between prunes: one range `DELETE` per write would take gap
/// locks that deadlock concurrent writers on MySQL.
const PRUNE_EVERY_SECS: i64 = 60;

impl DbJtiStore {
    /// A store on `pool`, the registry (or single) database.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            last_prune: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(i64::MIN)),
        }
    }

    /// Drop rows expired more than [`PRUNE_MARGIN_SECS`] ago, at most once
    /// per [`PRUNE_EVERY_SECS`]. Pruning never decides whether an id is spent.
    async fn prune(&self) {
        use std::sync::atomic::Ordering;
        let Ok(now) = crate::clock::unix_now_i64() else {
            return;
        };
        let last = self.last_prune.load(Ordering::Relaxed);
        if now.saturating_sub(last) < PRUNE_EVERY_SECS
            || self
                .last_prune
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let query = DeleteQuery {
            model: UsedJti::SCHEMA,
            where_clause: UsedJti::exp.lt(now - PRUNE_MARGIN_SECS).into(),
        };
        if let Err(e) = crate::sql::delete_pool(&self.pool, &query).await {
            tracing::warn!(target: "rustango::tenancy", error = %e, "could not prune spent token ids");
        }
    }
}

impl JtiStore for DbJtiStore {
    fn is_used<'a>(&'a self, jti: &'a str) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            UsedJti::objects()
                .where_(UsedJti::jti.eq(jti.to_owned()))
                .exists(&self.pool)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(target: "rustango::tenancy", error = %e, "spent token lookup failed; refusing the token");
                    true
                })
        })
    }

    fn mark_used<'a>(&'a self, jti: &'a str, exp_unix: i64) -> JtiFuture<'a, bool> {
        Box::pin(async move {
            if jti.len() > MAX_JTI_LEN {
                return false;
            }
            self.prune().await;
            let insert = InsertQuery::new(
                UsedJti::SCHEMA,
                vec![column(UsedJti::jti), column(UsedJti::exp)],
                vec![jti.to_owned().into(), exp_unix.into()],
            );
            crate::sql::insert_or_ignore(&self.pool, &insert)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(target: "rustango::tenancy", error = %e, "could not record a spent token id");
                    false
                })
        })
    }
}
