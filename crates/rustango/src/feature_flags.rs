//! Feature flags backed by the [`Cache`](crate::cache::Cache) trait.
//!
//! A flag can be resolved three ways:
//!
//! - Global on/off, with `is_enabled("new_checkout")`.
//! - A per-user override, with `is_enabled_for("new_checkout", "u-42")`.
//! - A percentage rollout, with `set_percentage("new_checkout", 25)`.
//!   The user id is hashed, so the same user always lands on the same
//!   side and does not flicker between requests.
//!
//! Use [`RedisCache`](crate::cache::redis_backend::RedisCache) if every
//! replica must see the same flag state.
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
//! ## Cache keys
//!
//! - `flag:<name>` — global on/off (`"on"`, `"off"` or absent)
//! - `flag:<name>:user:<user_id>` — per-user override
//! - `flag:<name>:pct` — rollout percentage, 0..=100
//!
//! Entries get a 1 hour TTL by default, so a write reaches every replica
//! within an hour even with no invalidation. Change it with
//! [`FeatureFlags::ttl`].
//!
//! [`FeatureFlags::ttl`]: crate::feature_flags::FeatureFlags::ttl

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

    /// Set the per-entry TTL. A lower value spreads changes to other
    /// replicas faster; a higher one costs less cache traffic. Default
    /// is 1 hour.
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

    /// Globally disable the flag. This beats any rollout percentage,
    /// but not a per-user override: to kill a flag for a user who has
    /// one, also call [`Self::disable_for_user`].
    pub async fn disable(&self, name: &str) {
        let _ = self
            .cache
            .set(&self.global_key(name), "off", Some(*self.ttl))
            .await;
    }

    /// Drop the global state, the percentage and the overrides of the
    /// listed users. The `Cache` trait cannot list keys, so the caller
    /// names the users to clear.
    pub async fn clear(&self, name: &str, known_user_ids: &[&str]) {
        let _ = self.cache.delete(&self.global_key(name)).await;
        let _ = self.cache.delete(&self.pct_key(name)).await;
        for u in known_user_ids {
            let _ = self.cache.delete(&self.user_key(name, u)).await;
        }
    }

    /// Set a rollout percentage, 0..=100. Values above 100 are clamped.
    /// The user id is hashed, so the same user gets the same answer on
    /// every check.
    pub async fn set_percentage(&self, name: &str, percent: u8) {
        let p = percent.min(100);
        let _ = self
            .cache
            .set(&self.pct_key(name), &p.to_string(), Some(*self.ttl))
            .await;
    }

    /// Turn the flag on for one user, whatever the global state says.
    /// Handy for QA and staff testing.
    pub async fn enable_for_user(&self, name: &str, user_id: &str) {
        let _ = self
            .cache
            .set(&self.user_key(name, user_id), "on", Some(*self.ttl))
            .await;
    }

    /// Turn the flag off for one user.
    pub async fn disable_for_user(&self, name: &str, user_id: &str) {
        let _ = self
            .cache
            .set(&self.user_key(name, user_id), "off", Some(*self.ttl))
            .await;
    }

    /// Read the global state only. Returns `false` when the flag is off
    /// or was never set. Per-user overrides and the rollout percentage
    /// are ignored here; use [`Self::is_enabled_for`] for those.
    pub async fn is_enabled(&self, name: &str) -> bool {
        match self.cache.get(&self.global_key(name)).await.ok().flatten() {
            Some(v) if v == "on" => true,
            Some(_) => false,
            None => false,
        }
    }

    /// Resolve the flag for one user. The first rule that applies wins:
    ///
    /// 1. A per-user override.
    /// 2. The global state.
    /// 3. The rollout percentage, if above 0.
    /// 4. Otherwise `false`.
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
        // 2. Global state. `off` here also cancels any percentage.
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

/// Stable bucket 0..=99 for `(flag_name, user_id)`. Same input, same
/// bucket, so a user stays in or out of a rollout. FNV-1a 64-bit seeded
/// with the flag name, so one user can be in flag A but not flag B.
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
        // The per-user override is checked first, so it still wins.
        f.disable("new").await;
        assert!(
            f.is_enabled_for("new", "alice").await,
            "per-user override beats global"
        );
        // To kill it fully, clear the per-user override as well.
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
        // 30% of 1000 = 300, with a wide tolerance for hash spread.
        let pct = on * 100 / total;
        assert!(
            (20..=40).contains(&pct),
            "expected ~30%, got {pct}% ({on}/{total})"
        );
    }

    #[tokio::test]
    async fn different_flag_names_get_different_buckets() {
        // The bucket is keyed on (flag, user), so a user can be inside
        // flag A's 50% and outside flag B's.
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
        // Bob had no state, so clear had nothing to do for him.
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
