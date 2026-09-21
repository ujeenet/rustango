//! Override [`Settings`] for the span of one test.
//!
//! [`with_overridden`](crate::test_settings::with_overridden) runs a
//! future with a task-local overlay. Code
//! that reads through [`current`] sees it; code holding its own
//! `&Settings` does not.
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
//! ## Scope
//!
//! The overlay is task-local. A task started with `tokio::spawn`
//! inside the scope does not inherit it.
//!
//! ## When to use it
//!
//! Most handlers take a `&Settings` argument, so a test should build
//! its own and pass it in. Reach for the overlay only when the code
//! under test reads settings through [`current`] and gives you no
//! place to hand one in.
//!
//! [`Settings`]: crate::config::Settings
//! [`current`]: crate::test_settings::current

use std::sync::OnceLock;

use crate::config::Settings;

tokio::task_local! {
    static OVERLAY: Settings;
}

/// What [`current`] returns when no overlay is active. Set it with
/// [`install_fallback`], or get `Settings::default()`.
static FALLBACK: OnceLock<Settings> = OnceLock::new();

/// Run `future` with `overlay` as the active Settings. Inside the
/// scope [`current`] returns `overlay`; outside it, the fallback.
///
/// A `setting_changed` signal fires on entry and on exit, so code
/// that caches config can refresh.
pub async fn with_overridden<F>(overlay: Settings, future: F) -> F::Output
where
    F: std::future::Future,
{
    use crate::signals::setting::{send_setting_changed, SettingChangedContext};
    // Let receivers flush cached config before the overlay applies.
    send_setting_changed(SettingChangedContext { enter: true }).await;
    let result = OVERLAY.scope(overlay, future).await;
    // And again, so they refresh against the restored settings.
    send_setting_changed(SettingChangedContext { enter: false }).await;
    result
}

/// The active Settings: the task's overlay, else the fallback, else
/// `Settings::default()`. Never panics.
#[must_use]
pub fn current() -> Settings {
    if let Ok(overlay) = OVERLAY.try_with(Clone::clone) {
        return overlay;
    }
    FALLBACK.get().cloned().unwrap_or_default()
}

/// Set the Settings [`current`] returns outside any overlay. Call it
/// once at startup. Only the first call takes effect.
///
/// Returns `true` if this call installed the fallback, `false` if one
/// was already there.
pub fn install_fallback(fallback: Settings) -> bool {
    FALLBACK.set(fallback).is_ok()
}

/// `true` when this task has an overlay. For assertions only; do not
/// branch on it in production code.
#[must_use]
pub fn has_overlay() -> bool {
    OVERLAY.try_with(|_| ()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_outside_overlay_returns_default() {
        // A sibling test may already have installed FALLBACK for the
        // whole binary, so only check that current() does not panic
        // and that no overlay is active.
        let _ = current();
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
        // Two tasks, two overlays; neither sees the other's.
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
            // The outer overlay is back.
            assert_eq!(current().secret_key.as_deref(), Some("outer"));
        })
        .await;
    }

    #[tokio::test]
    async fn install_fallback_first_caller_wins() {
        // FALLBACK is process-global and cannot be reset, and test
        // order is not fixed, so check only that at most one call
        // returns true.
        let mut a = Settings::default();
        a.secret_key = Some("fallback-a".into());
        let mut b = Settings::default();
        b.secret_key = Some("fallback-b".into());
        let result_a = install_fallback(a);
        let result_b = install_fallback(b);
        assert!(
            !(result_a && result_b),
            "install_fallback returned true twice; OnceLock contract broken"
        );
    }
}
