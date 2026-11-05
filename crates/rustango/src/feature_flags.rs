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
//! flags.enable("new_checkout").await?;
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
//! User ids are per tenant, so under tenancy scope the overrides with
//! `FeatureFlags::for_tenant`: they move to
//! `tenant:#org:<id>:<slug>:flag:<name>:user:<user_id>`. The global state and
//! the percentage stay platform-wide.
//!
//! Entries never expire by default: flag state is durable. Opt in to
//! expiry with [`FeatureFlags::ttl`]. Without a TTL flags are written with
//! `set_forever`, which `InMemoryCache` never evicts; with one they can be.
//!
//! [`FeatureFlags::ttl`]: crate::feature_flags::FeatureFlags::ttl

use std::time::Duration;

use crate::cache::{BoxedCache, CacheError};

const KEY_PREFIX: &str = "flag";

#[derive(Clone)]
pub struct FeatureFlags {
    cache: BoxedCache,
    /// Holds the per-user overrides: `cache`, or its tenant view.
    users: BoxedCache,
    /// `None` = never expire; a kill switch must not lapse on its own.
    ttl: Option<Duration>,
}

impl FeatureFlags {
    #[must_use]
    pub fn new(cache: BoxedCache) -> Self {
        Self {
            users: cache.clone(),
            cache,
            ttl: None,
        }
    }

    /// Keep per-user overrides to one tenant, since user ids repeat across
    /// tenants. The global state and the percentage stay platform-wide.
    #[cfg(feature = "tenancy")]
    #[must_use]
    pub fn for_tenant(mut self, tenant: &crate::tenancy::TenantSlug) -> Self {
        self.users = crate::cache::ScopedCache::for_tenant(self.cache.clone(), tenant).boxed();
        self
    }

    /// Make every write expire after `ttl`; the flag then reads as never
    /// set. Default: no expiry.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
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

    /// One write; `set_forever` so a backend default TTL cannot expire it.
    async fn put(&self, cache: &BoxedCache, key: &str, value: &str) -> Result<(), CacheError> {
        match self.ttl {
            Some(ttl) => cache.set(key, value, Some(ttl)).await,
            None => cache.set_forever(key, value).await,
        }
    }

    /// Globally enable the flag for everyone.
    ///
    /// # Errors
    /// The cache write's error: the flag did not change.
    pub async fn enable(&self, name: &str) -> Result<(), CacheError> {
        self.put(&self.cache, &self.global_key(name), "on").await
    }

    /// Globally disable the flag. This beats any rollout percentage,
    /// but not a per-user override: to kill a flag for a user who has
    /// one, also call [`Self::disable_for_user`].
    ///
    /// # Errors
    /// The cache write's error: the flag did not change.
    pub async fn disable(&self, name: &str) -> Result<(), CacheError> {
        self.put(&self.cache, &self.global_key(name), "off").await
    }

    /// Drop the global state, the percentage and the overrides of the
    /// listed users. The `Cache` trait cannot list keys, so the caller
    /// names the users to clear.
    ///
    /// # Errors
    /// The first failed cache delete; later keys are not tried.
    pub async fn clear(&self, name: &str, known_user_ids: &[&str]) -> Result<(), CacheError> {
        self.cache.delete(&self.global_key(name)).await?;
        self.cache.delete(&self.pct_key(name)).await?;
        for u in known_user_ids {
            self.users.delete(&self.user_key(name, u)).await?;
        }
        Ok(())
    }

    /// Set a rollout percentage, 0..=100. Values above 100 are clamped.
    /// The user id is hashed, so the same user gets the same answer on
    /// every check.
    ///
    /// # Errors
    /// The cache write's error.
    pub async fn set_percentage(&self, name: &str, percent: u8) -> Result<(), CacheError> {
        let p = percent.min(100);
        self.put(&self.cache, &self.pct_key(name), &p.to_string())
            .await
    }

    /// Turn the flag on for one user, whatever the global state says.
    /// Handy for QA and staff testing.
    ///
    /// # Errors
    /// The cache write's error.
    pub async fn enable_for_user(&self, name: &str, user_id: &str) -> Result<(), CacheError> {
        self.put(&self.users, &self.user_key(name, user_id), "on")
            .await
    }

    /// Turn the flag off for one user.
    ///
    /// # Errors
    /// The cache write's error.
    pub async fn disable_for_user(&self, name: &str, user_id: &str) -> Result<(), CacheError> {
        self.put(&self.users, &self.user_key(name, user_id), "off")
            .await
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
            .users
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

    /// Unscoped keys are unchanged; a tenant's overrides carry its org id and slug.
    #[tokio::test]
    async fn override_keys_are_bare_or_bound_to_the_tenant() {
        let root: BoxedCache = StdArc::new(InMemoryCache::new());
        FeatureFlags::new(root.clone())
            .enable_for_user("beta", "5")
            .await
            .unwrap();
        assert!(root.exists("flag:beta:user:5").await.unwrap());
        #[cfg(feature = "tenancy")]
        {
            use crate::tenancy::TenantSlug;
            root.clear().await.unwrap();
            let acme = FeatureFlags::new(root.clone()).for_tenant(&TenantSlug::new(1, "acme"));
            let heir = FeatureFlags::new(root.clone()).for_tenant(&TenantSlug::new(2, "acme"));
            acme.enable_for_user("beta", "5").await.unwrap();
            assert!(root
                .exists("tenant:#org:1:acme:flag:beta:user:5")
                .await
                .unwrap());
            assert!(
                !heir.is_enabled_for("beta", "5").await,
                "slug reuse sees it"
            );
            assert!(
                !FeatureFlags::new(root.clone())
                    .is_enabled_for("beta", "5")
                    .await
            );
            // The kill switch stays platform-wide.
            FeatureFlags::new(root).enable("beta").await.unwrap();
            assert!(heir.is_enabled_for("beta", "6").await);
        }
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
        f.enable("new").await.unwrap();
        assert!(f.is_enabled("new").await);
        assert!(f.is_enabled_for("new", "alice").await);
        assert!(f.is_enabled_for("new", "bob").await);
    }

    #[tokio::test]
    async fn global_disable_is_a_killswitch_over_per_user_enable() {
        let f = flags();
        f.enable_for_user("new", "alice").await.unwrap();
        assert!(f.is_enabled_for("new", "alice").await);
        // The per-user override is checked first, so it still wins.
        f.disable("new").await.unwrap();
        assert!(
            f.is_enabled_for("new", "alice").await,
            "per-user override beats global"
        );
        // To kill it fully, clear the per-user override as well.
        f.disable_for_user("new", "alice").await.unwrap();
        assert!(!f.is_enabled_for("new", "alice").await);
    }

    #[tokio::test]
    async fn per_user_override_wins_over_global_off() {
        let f = flags();
        f.disable("new").await.unwrap();
        f.enable_for_user("new", "qa-bot").await.unwrap();
        assert!(f.is_enabled_for("new", "qa-bot").await);
        // Other users still see the global off.
        assert!(!f.is_enabled_for("new", "alice").await);
    }

    #[tokio::test]
    async fn percentage_rollout_at_zero_means_off() {
        let f = flags();
        f.set_percentage("new", 0).await.unwrap();
        for u in &["a", "b", "c", "d"] {
            assert!(!f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_at_100_means_on() {
        let f = flags();
        f.set_percentage("new", 100).await.unwrap();
        for u in &["a", "b", "c", "d"] {
            assert!(f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_is_stable_per_user() {
        let f = flags();
        f.set_percentage("new", 50).await.unwrap();
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
        f.set_percentage("new", 250).await.unwrap(); // clamped to 100
        for u in &["a", "b", "c"] {
            assert!(f.is_enabled_for("new", u).await);
        }
    }

    #[tokio::test]
    async fn percentage_rollout_distributes_roughly_evenly() {
        let f = flags();
        f.set_percentage("new", 30).await.unwrap();
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
        f.set_percentage("alpha", 50).await.unwrap();
        f.set_percentage("beta", 50).await.unwrap();
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
        f.enable("new").await.unwrap();
        f.set_percentage("new", 75).await.unwrap();
        f.enable_for_user("new", "alice").await.unwrap();

        f.clear("new", &["alice"]).await.unwrap();
        assert!(!f.is_enabled("new").await);
        assert!(!f.is_enabled_for("new", "alice").await);
        // Bob had no state, so clear had nothing to do for him.
        assert!(!f.is_enabled_for("new", "bob").await);
    }

    /// Records the TTL of every write, so a test sees what reaches the backend.
    struct TtlSpy {
        inner: InMemoryCache,
        ttls: std::sync::Mutex<Vec<Option<Duration>>>,
    }

    #[async_trait::async_trait]
    impl crate::cache::Cache for TtlSpy {
        async fn touch(
            &self,
            k: &str,
            ttl: Option<std::time::Duration>,
        ) -> Result<bool, crate::cache::CacheError> {
            self.inner.touch(k, ttl).await
        }
        async fn get(&self, k: &str) -> Result<Option<String>, crate::cache::CacheError> {
            self.inner.get(k).await
        }
        async fn set(
            &self,
            k: &str,
            v: &str,
            ttl: Option<Duration>,
        ) -> Result<(), crate::cache::CacheError> {
            self.ttls.lock().unwrap().push(ttl);
            self.inner.set(k, v, ttl).await
        }
        async fn delete(&self, k: &str) -> Result<(), crate::cache::CacheError> {
            self.inner.delete(k).await
        }
        async fn exists(&self, k: &str) -> Result<bool, crate::cache::CacheError> {
            self.inner.exists(k).await
        }
        async fn clear(&self) -> Result<(), crate::cache::CacheError> {
            self.inner.clear().await
        }
    }

    async fn write_all(f: &FeatureFlags) {
        f.enable("on").await.unwrap();
        f.disable("off").await.unwrap();
        f.set_percentage("pct", 100).await.unwrap();
        f.enable_for_user("u", "a").await.unwrap();
        f.disable_for_user("u", "b").await.unwrap();
    }

    async fn all_set(f: &FeatureFlags) -> bool {
        f.is_enabled("on").await
            && f.cache.exists(&f.global_key("off")).await.unwrap()
            && f.is_enabled_for("pct", "x").await
            && f.is_enabled_for("u", "a").await
            && f.cache.exists(&f.user_key("u", "b")).await.unwrap()
    }

    #[tokio::test]
    async fn writes_outlive_a_backend_default_ttl() {
        // #1956: a `None` write took the cache's default TTL, so a kill switch lapsed.
        let cache: BoxedCache =
            StdArc::new(InMemoryCache::with_default_ttl(Duration::from_millis(1)));
        let f = FeatureFlags::new(cache);
        write_all(&f).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(all_set(&f).await);
    }

    #[tokio::test]
    async fn opt_in_ttl_reaches_every_write_and_expires() {
        let spy = StdArc::new(TtlSpy {
            inner: InMemoryCache::new(),
            ttls: std::sync::Mutex::new(Vec::new()),
        });
        let ttl = Duration::from_millis(1);
        let f = FeatureFlags::new(spy.clone()).ttl(ttl);
        write_all(&f).await;
        assert_eq!(spy.ttls.lock().unwrap().clone(), vec![Some(ttl); 5]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!f.is_enabled("on").await);
        assert!(!f.is_enabled_for("u", "a").await);
    }

    /// Every write and delete fails, like a Redis that is down.
    struct DownCache;

    #[async_trait::async_trait]
    impl crate::cache::Cache for DownCache {
        async fn touch(&self, _: &str, _: Option<Duration>) -> Result<bool, CacheError> {
            Err(CacheError::Connection("down".into()))
        }
        async fn get(&self, _: &str) -> Result<Option<String>, CacheError> {
            Err(CacheError::Connection("down".into()))
        }
        async fn set(&self, _: &str, _: &str, _: Option<Duration>) -> Result<(), CacheError> {
            Err(CacheError::Connection("down".into()))
        }
        async fn delete(&self, _: &str) -> Result<(), CacheError> {
            Err(CacheError::Connection("down".into()))
        }
        async fn exists(&self, _: &str) -> Result<bool, CacheError> {
            Err(CacheError::Connection("down".into()))
        }
        async fn clear(&self) -> Result<(), CacheError> {
            Err(CacheError::Connection("down".into()))
        }
    }

    #[tokio::test]
    async fn a_failed_write_reaches_the_caller() {
        // A kill switch that did not land must not look like success.
        let f = FeatureFlags::new(StdArc::new(DownCache));
        assert!(f.disable("new").await.is_err());
        assert!(f.enable("new").await.is_err());
        assert!(f.set_percentage("new", 5).await.is_err());
        assert!(f.enable_for_user("new", "a").await.is_err());
        assert!(f.disable_for_user("new", "a").await.is_err());
        assert!(f.clear("new", &["a"]).await.is_err());
        let ttl = FeatureFlags::new(StdArc::new(DownCache)).ttl(Duration::from_secs(1));
        assert!(ttl.disable("new").await.is_err());
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
