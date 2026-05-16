//! Test-only Settings overlay — Django's `@override_settings` /
//! `with self.settings(...)`. Issue #43.
//!
//! Runs an async closure with a task-local [`Settings`] overlay.
//! Code that reads via [`current`] sees the override for the
//! duration of the scope; everything outside (or code that has
//! its own `&Settings` already in hand) is unaffected.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::test_settings::{with_overridden, current};
//! use rustango::config::Settings;
//!
//! #[tokio::test]
//! async fn admin_redirects_when_disabled() {
//!     let mut s = Settings::default();
//!     s.admin.url_prefix = Some("/admin-test".into());
//!     with_overridden(s, async {
//!         let cfg = current();
//!         assert_eq!(cfg.admin.url_prefix.as_deref(), Some("/admin-test"));
//!         // ... run the code under test ...
//!     })
//!     .await;
//! }
//! ```
//!
//! ## Scope caveat
//!
//! The overlay is **task-local**. Code that spawns a fresh task via
//! `tokio::spawn` inside the scope WILL NOT see the override unless
//! the new task is spawned through [`tokio::task::LocalSet`] or
//! re-enters via `current()`. This matches Django's
//! `override_settings` which works per-thread; spawning a new thread
//! likewise drops the override.
//!
//! ## When to use vs. construct a Settings directly
//!
//! Most rustango handlers receive a `&Settings` argument explicitly
//! — tests should build their own Settings and pass it in. Use this
//! overlay only when:
//! - The code path you're testing reads via a `Settings::current()`
//!   style global (rare today; the framework prefers explicit
//!   passing).
//! - You're testing transitive code that doesn't accept Settings
//!   directly but does read via the overlay.

use std::sync::OnceLock;

use crate::config::Settings;

tokio::task_local! {
    static OVERLAY: Settings;
}

/// The fallback Settings used by [`current`] when no overlay is
/// active. Populated lazily on first read from `Settings::default()`,
/// or replaced explicitly via [`install_fallback`] (typically called
/// once during app startup with the loaded production Settings).
static FALLBACK: OnceLock<Settings> = OnceLock::new();

/// Run `future` with `overlay` as the active Settings. Inside the
/// scope, [`current`] returns a clone of `overlay`; outside, the
/// fallback applies.
///
/// The future is `Send` so callers can use it in `tokio::test`
/// runtimes (single-thread + multi-thread alike).
pub async fn with_overridden<F>(overlay: Settings, future: F) -> F::Output
where
    F: std::future::Future,
{
    OVERLAY.scope(overlay, future).await
}

/// Return the active Settings: the task-local overlay when one is
/// installed, else the registered fallback, else a fresh
/// `Settings::default()`. **Never panics** — the worst-case return
/// is an empty `Settings::default()` so tests that haven't set up
/// an overlay still get a usable value.
#[must_use]
pub fn current() -> Settings {
    if let Ok(overlay) = OVERLAY.try_with(Clone::clone) {
        return overlay;
    }
    FALLBACK.get().cloned().unwrap_or_default()
}

/// Install `fallback` as the Settings returned by [`current`]
/// outside any overlay scope. Idempotent — only the first call
/// wins. Typically invoked once during app startup so non-test
/// code can read via `current()` and get the production
/// configuration.
///
/// Returns `true` when this call installed the fallback; `false`
/// when a fallback was already registered (which is the no-op
/// behaviour).
pub fn install_fallback(fallback: Settings) -> bool {
    FALLBACK.set(fallback).is_ok()
}

/// `true` when an overlay is currently active on this task. Useful
/// for assertions / diagnostics; production code shouldn't branch
/// on it.
#[must_use]
pub fn has_overlay() -> bool {
    OVERLAY.try_with(|_| ()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_outside_overlay_returns_default() {
        // Without any overlay or install_fallback() call, current()
        // must return a usable Settings::default() (i.e. not panic).
        let s = current();
        assert_eq!(s.secret_key, Settings::default().secret_key);
        assert!(!has_overlay());
    }

    #[tokio::test]
    async fn with_overridden_swaps_in_overlay() {
        let mut overlay = Settings::default();
        overlay.secret_key = Some("test-override-secret".into());

        with_overridden(overlay.clone(), async move {
            assert!(has_overlay());
            let active = current();
            assert_eq!(active.secret_key.as_deref(), Some("test-override-secret"));
        })
        .await;

        // Outside the scope, no overlay.
        assert!(!has_overlay());
        let outside = current();
        assert_ne!(outside.secret_key.as_deref(), Some("test-override-secret"));
    }

    #[tokio::test]
    async fn overlay_is_scoped_per_task() {
        // Two concurrent tasks set different overlays; neither sees
        // the other's. Pin the per-task isolation guarantee.
        let mut a = Settings::default();
        a.secret_key = Some("alpha".into());
        let mut b = Settings::default();
        b.secret_key = Some("beta".into());

        let ta = tokio::spawn(with_overridden(a, async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            current().secret_key
        }));
        let tb = tokio::spawn(with_overridden(b, async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            current().secret_key
        }));
        let (got_a, got_b) = (ta.await.unwrap(), tb.await.unwrap());
        assert_eq!(got_a.as_deref(), Some("alpha"));
        assert_eq!(got_b.as_deref(), Some("beta"));
    }

    #[tokio::test]
    async fn nested_overlay_replaces_outer() {
        let mut outer = Settings::default();
        outer.secret_key = Some("outer".into());
        let mut inner = Settings::default();
        inner.secret_key = Some("inner".into());

        with_overridden(outer.clone(), async move {
            assert_eq!(current().secret_key.as_deref(), Some("outer"));
            with_overridden(inner, async {
                assert_eq!(current().secret_key.as_deref(), Some("inner"));
            })
            .await;
            // Outer scope restored after inner exits.
            assert_eq!(current().secret_key.as_deref(), Some("outer"));
        })
        .await;
    }

    #[tokio::test]
    async fn install_fallback_first_caller_wins() {
        // Use a fresh OnceLock by isolating in a separate static. The
        // module's FALLBACK is process-global, so we test via the
        // documented semantics — the FIRST call to install_fallback
        // wins. We can't reset FALLBACK between tests, but we CAN
        // verify the boolean return value follows the contract on
        // this process. Subsequent calls return false.
        let mut a = Settings::default();
        a.secret_key = Some("fallback-a".into());
        let mut b = Settings::default();
        b.secret_key = Some("fallback-b".into());
        // Whether the FIRST call here wins depends on test ordering
        // — assert only that AT MOST ONE call returns true across
        // this run + every previous test (in this test binary).
        let result_a = install_fallback(a);
        let result_b = install_fallback(b);
        assert!(
            !(result_a && result_b),
            "install_fallback returned true twice; OnceLock contract broken"
        );
    }
}
