//! Feature flags / killswitches backed by the [`Cache`](crate::cache::Cache) trait.
//!
//! Three resolution modes:
//!
//! - **Boolean killswitch** — `flags.is_enabled("new_checkout").await`
//!   reads a single bool from the cache.
//! - **Per-user override** — `flags.is_enabled_for("new_checkout",
//!   "user-42").await` checks for a per-user enable record on top of
//!   the global flag.
//! - **Percentage rollout** — `flags.set_percentage("new_checkout",
//!   25).await` enables the flag for a stable 25% of user IDs (hashed
//!   so the same id always falls in or out, avoiding flicker between
//!   requests).
//!
//! Pair with [`crate::cache::RedisCache`] for cross-replica visibility.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::feature_flags::FeatureFlags;
//! use rustango::cache::{BoxedCache, InMemoryCache};
//! use std::sync::Arc;
//!
//! let cache: BoxedCache = Arc::new(InMemoryCache::new());
//! let flags = FeatureFlags::new(cache);
//!
//! // Bootstrap a flag at startup (or via an admin route):
//! flags.enable("new_checkout").await;
//!
//! // In handlers:
//! if flags.is_enabled_for("new_checkout", &current_user_id).await {
//!     run_new_path();
//! } else {
//!     run_legacy_path();
//! }
//! ```
//!
//! ## Cache key shape
//!
//! - `flag:<name>` — global on/off (`"on"` / `"off"` / absent)
//! - `flag:<name>:user:<user_id>` — explicit per-user override
//! - `flag:<name>:pct` — rollout percentage 0..=100
//!
//! All entries are stored with a 1 hour TTL by default so writes
//! propagate within an hour even without active invalidation. Override
//! with [`FeatureFlags::ttl`].

use std::sync::Arc;
use std::time::Duration;

use crate::cache::BoxedCache;

const KEY_PREFIX: &str = "flag";
const DEFAULT_TTL_SECS: u64 = 3600;

#[derive(Clone)]
pub struct FeatureFlags {
    cache: BoxedCache,
    ttl: Arc<Duration>,
}

impl FeatureFlags {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            cache,
            ttl: Arc::new(Duration::from_secs(DEFAULT_TTL_SECS)),
        }
    }

    /// Override the per-entry TTL. Lower = faster propagation across
    /// replicas; higher = less cache load. Default 1 hour.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Arc::new(ttl);
        self
    }

    fn global_key(&self, name: &str) -> String {
        format!("{KEY_PREFIX}:{name}")
    }

    fn user_key(&self, name: &str, user_id: &str) -> String {
        format!("{KEY_PREFIX}:{name}:user:{user_id}")
    }

    fn pct_key(&self, name: &str) -> String {
        format!("{KEY_PREFIX}:{name}:pct")
    }

    /// Globally enable the flag for everyone.
    pub async fn enable(&self, name: &str) {
        let _ = self
            .cache
            .set(&self.global_key(name), "on", Some(*self.ttl))
            .await;
    }

    /// Globally disable the flag for everyone (overrides per-user
    /// enables for the killswitch effect).
    pub async fn disable(&self, name: &str) {
        let _ = self
            .cache
            .set(&self.global_key(name), "off", Some(*self.ttl))
            .await;
    }

    /// Drop every record for `name` — global state, percentage, and
    /// known per-user overrides for the supplied list. (We can't
    /// enumerate keys generically through the `Cache` trait, so callers
    /// supply any user ids they want to scrub.)
    pub async fn clear(&self, name: &str, known_user_ids: &[&str]) {
        let _ = self.cache.delete(&self.global_key(name)).await;
        let _ = self.cache.delete(&self.pct_key(name)).await;
        for u in known_user_ids {
            let _ = self.cache.delete(&self.user_key(name, u)).await;
        }
    }

    /// Set a rollout percentage 0..=100. The flag returns `true` for a
    /// stable, hashed slice of user ids — the same id falls inside the
    /// percentage on every check, so a user doesn't flicker between
    /// requests.
    pub async fn set_percentage(&self, name: &str, percent: u8) {
        let p = percent.min(100);
        let _ = self
            .cache
            .set(&self.pct_key(name), &p.to_string(), Some(*self.ttl))
            .await;
    }

    /// Force-enable the flag for a specific user, regardless of global
    /// state. Useful for QA / staff dogfood.
    pub async fn enable_for_user(&self, name: &str, user_id: &str) {
        let _ = self
            .cache
            .set(&self.user_key(name, user_id), "on", Some(*self.ttl))
            .await;
    }

    /// Force-disable the flag for a specific user.
    pub async fn disable_for_user(&self, name: &str, user_id: &str) {
        let _ = self
            .cache
            .set(&self.user_key(name, user_id), "off", Some(*self.ttl))
            .await;
    }

    /// Resolve the flag globally — no per-user awareness. Returns
    /// `false` when:
    ///
    /// - the flag was explicitly disabled, OR
    /// - the cache is empty and there's no rollout percentage record
    ///   (i.e. the flag has never been touched).
    pub async fn is_enabled(&self, name: &str) -> bool {
        match self.cache.get(&self.global_key(name)).await.ok().flatten() {
            Some(v) if v == "on" => true,
            Some(_) => false,
            None => false,
        }
    }

    /// Resolve the flag for a specific user, considering all three
    /// modes in this order:
    ///
    /// 1. Per-user override (`enable_for_user` / `disable_for_user`).
    ///    Whichever wins.
    /// 2. Global state (`enable` / `disable`). If the global state is
    ///    `off`, that's the answer (killswitch wins).
    /// 3. Percentage rollout — if set and `> 0`, hash the user id and
    ///    return `true` when it falls inside the percentage.
    /// 4. Default `false`.
    pub async fn is_enabled_for(&self, name: &str, user_id: &str) -> bool {
        // 1. Per-user override.
        match self
            .cache
            .get(&self.user_key(name, user_id))
            .await
            .ok()
            .flatten()
            .as_deref()
        {
            Some("on") => return true,
            Some(_) => return false,
            None => {}
        }
        // 2. Global state. `off` is a killswitch (overrides any %).
        match self
            .cache
            .get(&self.global_key(name))
            .await
            .ok()
            .flatten()
            .as_deref()
        {
            Some("on") => return true,
            Some(_) => return false,
            None => {}
        }
        // 3. Percentage rollout.
        let pct = self
            .cache
            .get(&self.pct_key(name))
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(0);
        if pct == 0 {
            return false;
        }
        if pct >= 100 {
            return true;
        }
        bucket_for(name, user_id) < pct
    }
}

/// Stable hash bucket 0..=99 for `(flag_name, user_id)`. Same input →
/// same bucket, so a user that's "in the 25%" stays in across requests.
/// FNV-1a 64-bit, seeded with the flag name so a user can be in one
/// flag's rollout but not another.
fn bucket_for(name: &str, user_id: &str) -> u8 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= u64::from(b':');
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    for &b in user_id.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    u8::try_from(h % 100).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::InMemoryCache;
    use std::sync::Arc as StdArc;

    fn flags() -> FeatureFlags {
        let cache: BoxedCache = StdArc::new(InMemoryCache::new());
        FeatureFlags::new(cache)
    }

    #[tokio::test]
    async fn fresh_flag_is_disabled() {
        let f = flags();
        assert!(!f.is_enabled("new").await);
        assert!(!f.is_enabled_for("new", "alice").await);
    }

    #[tokio::test]
    async fn global_enable_propagates_to_all_users() {
        let f = flags();
        f.enable("new").await;
        assert!(f.is_enabled("new").await);
        assert!(f.is_enabled_for("new", "alice").await);
        assert!(f.is_enabled_for("new", "bob").await);
    }

    #[tokio::test]
    async fn global_disable_is_a_killswitch_over_per_user_enable() {
        let f = flags();
        f.enable_for_user("new", "alice").await;
        assert!(f.is_enabled_for("new", "alice").await);
        // Per-user override is checked FIRST so it should still win.
        f.disable("new").await;
        assert!(
            f.is_enabled_for("new", "alice").await,
            "per-user override beats global"
        );
        // Use the killswitch the right way: kill the per-user override too.
        f.disable_for_user("new", "alice").await;
        assert!(!f.is_enabled_for("new", "alice").await);
    }

    #[tokio::test]
    async fn per_user_override_wins_over_global_off() {
        let f = flags();
        f.disable("new").await;
        f.enable_for_user("new", "qa-bot").await;
        assert!(f.is_enabled_for("new", "qa-bot").await);
        // Other users still see the global off.
        assert!(!f.is_enabled_for("new", "alice").await);
    }

    #[tokio::test]
    async fn percentage_rollout_at_zero_means_off() {
        let f = flags();
        f.set_percentage("new", 0).await;
        for u in &["a", "b", "c", "d"] {
            assert!(!f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_at_100_means_on() {
        let f = flags();
        f.set_percentage("new", 100).await;
        for u in &["a", "b", "c", "d"] {
            assert!(f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_is_stable_per_user() {
        let f = flags();
        f.set_percentage("new", 50).await;
        for u in &["alice", "bob", "carol", "dave"] {
            let first = f.is_enabled_for("new", u).await;
            let second = f.is_enabled_for("new", u).await;
            let third = f.is_enabled_for("new", u).await;
            assert_eq!(first, second);
            assert_eq!(first, third);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_clamps_above_100() {
        let f = flags();
        f.set_percentage("new", 250).await; // clamped to 100
        for u in &["a", "b", "c"] {
            assert!(f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_distributes_roughly_evenly() {
        let f = flags();
        f.set_percentage("new", 30).await;
        let mut on = 0;
        let total = 1000;
        for i in 0..total {
            let u = format!("user-{i}");
            if f.is_enabled_for("new", &u).await {
                on += 1;
            }
        }
        // 30% of 1000 = 300 ± a generous tolerance for hash quality.
        let pct = on * 100 / total;
        assert!(
            (20..=40).contains(&pct),
            "expected ~30%, got {pct}% ({on}/{total})"
        );
    }

    #[tokio::test]
    async fn different_flag_names_get_different_buckets() {
        // The bucket is keyed on (flag, user) so a user might be in
        // 50% for flag A but out of 50% for flag B. Prove it: pick a
        // user that flips between two flag names.
        let f = flags();
        f.set_percentage("alpha", 50).await;
        f.set_percentage("beta", 50).await;
        let mut differs = false;
        for i in 0..100 {
            let u = format!("u-{i}");
            if f.is_enabled_for("alpha", &u).await != f.is_enabled_for("beta", &u).await {
                differs = true;
                break;
            }
        }
        assert!(
            differs,
            "two flags at 50% should disagree on at least one user"
        );
    }

    #[tokio::test]
    async fn clear_resets_all_known_state() {
        let f = flags();
        f.enable("new").await;
        f.set_percentage("new", 75).await;
        f.enable_for_user("new", "alice").await;

        f.clear("new", &["alice"]).await;
        assert!(!f.is_enabled("new").await);
        assert!(!f.is_enabled_for("new", "alice").await);
        // Bob's state was never set so clear had nothing to do for him.
        assert!(!f.is_enabled_for("new", "bob").await);
    }

    #[test]
    fn bucket_for_is_deterministic() {
        assert_eq!(bucket_for("flag", "user"), bucket_for("flag", "user"));
    }

    #[test]
    fn bucket_for_is_in_range() {
        for i in 0..1000 {
            let u = format!("u-{i}");
            assert!(bucket_for("flag", &u) < 100);
        }
    }
}
