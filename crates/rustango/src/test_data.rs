//! Shared test fixtures, built once per test binary.
//!
//! Build a fixture once and reuse it across the tests in a file. The
//! [`setup_test_data!`] and [`setup_test_data_async!`] macros wrap a
//! `OnceLock` or `OnceCell` in a plain function call:
//!
//! ```ignore
//! use rustango::setup_test_data_async;
//!
//! setup_test_data_async!(pub async fn shared_articles() -> Vec<Article> {
//!     let pool = crate::test_pool().await;
//!     vec![
//!         Article::objects()
//!             .insert(Article { title: "First".into(), ..Default::default() })
//!             .fetch_one(&pool)
//!             .await
//!             .unwrap(),
//!     ]
//! });
//!
//! #[tokio::test]
//! async fn test_uses_shared_fixture() {
//!     let articles = shared_articles().await;
//!     assert_eq!(articles.len(), 1);
//! }
//! ```
//!
//! ## What to watch for
//!
//! - The fixture lives as long as the test binary. `cargo test` runs
//!   each integration-test file in its own process, so tests in one
//!   file share a fixture and tests in another do not.
//! - Treat a fixture as read only. Nothing rolls back a change one
//!   test makes to it, so the next test would see the change. Wrap
//!   tests that write in [`crate::test_db::with_rollback`].
//! - The macros panic if the body panics. For real error handling,
//!   write the `OnceLock` out by hand.

/// A shared fixture built by sync code. Wraps a
/// [`std::sync::OnceLock`].
///
/// ```ignore
/// rustango::setup_test_data!(pub fn shared_locales() -> Vec<&'static str> {
///     vec!["en", "fr", "de"]
/// });
///
/// #[test]
/// fn locales_loaded_once() {
///     let l = shared_locales();
///     assert_eq!(l.len(), 3);
/// }
/// ```
///
/// The body runs at most once per test binary. Later calls return a
/// `&'static` reference to the same value.
#[macro_export]
macro_rules! setup_test_data {
    ($vis:vis fn $name:ident () -> $ty:ty $body:block) => {
        $vis fn $name() -> &'static $ty {
            static CELL: ::std::sync::OnceLock<$ty> = ::std::sync::OnceLock::new();
            CELL.get_or_init(|| $body)
        }
    };
}

/// A shared fixture built by async code, such as one that inserts
/// rows. Wraps a [`tokio::sync::OnceCell`], so concurrent first
/// callers all wait on one run of the body.
///
/// ```ignore
/// rustango::setup_test_data_async!(pub async fn shared_articles() -> Vec<Article> {
///     let pool = crate::test_pool().await;
///     vec![
///         Article::objects().insert(...).fetch_one(&pool).await.unwrap(),
///     ]
/// });
///
/// #[tokio::test]
/// async fn t() {
///     let a = shared_articles().await;
///     assert!(!a.is_empty());
/// }
/// ```
#[macro_export]
macro_rules! setup_test_data_async {
    ($vis:vis async fn $name:ident () -> $ty:ty $body:block) => {
        $vis async fn $name() -> &'static $ty {
            static CELL: ::tokio::sync::OnceCell<$ty> = ::tokio::sync::OnceCell::const_new();
            CELL.get_or_init(|| async move { $body }).await
        }
    };
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    setup_test_data!(
        fn shared_numbers() -> Vec<i32> {
            vec![1, 2, 3, 4, 5]
        }
    );

    #[test]
    fn sync_fixture_returns_static_ref() {
        let n = shared_numbers();
        assert_eq!(n, &vec![1, 2, 3, 4, 5]);
        // The second call returns the same reference.
        let n2 = shared_numbers();
        assert!(
            std::ptr::eq(n, n2),
            "two calls should return the same &'static Vec",
        );
    }

    // A counter, to prove the body runs at most once.
    static INIT_RUNS: AtomicUsize = AtomicUsize::new(0);
    setup_test_data!(
        fn counted_fixture() -> i32 {
            INIT_RUNS.fetch_add(1, Ordering::SeqCst);
            42
        }
    );

    #[test]
    fn sync_fixture_init_runs_at_most_once() {
        let _ = counted_fixture();
        let after_first = INIT_RUNS.load(Ordering::SeqCst);
        let _ = counted_fixture();
        let after_second = INIT_RUNS.load(Ordering::SeqCst);
        assert_eq!(after_first, after_second, "init re-ran on second call");
        // No other test uses this fixture, so the count is 1.
        assert!(after_first <= 1, "init counter: {after_first}");
    }

    setup_test_data_async!(
        async fn shared_async_fixture() -> Vec<String> {
            // An await proves the body really is async.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            vec!["alpha".to_owned(), "beta".to_owned()]
        }
    );

    #[tokio::test]
    async fn async_fixture_returns_static_ref() {
        let v = shared_async_fixture().await;
        assert_eq!(v, &vec!["alpha".to_owned(), "beta".to_owned()]);
        let v2 = shared_async_fixture().await;
        assert!(std::ptr::eq(v, v2));
    }

    static ASYNC_INIT_RUNS: AtomicUsize = AtomicUsize::new(0);
    setup_test_data_async!(
        async fn counted_async_fixture() -> i32 {
            ASYNC_INIT_RUNS.fetch_add(1, Ordering::SeqCst);
            100
        }
    );

    #[tokio::test]
    async fn async_fixture_init_runs_at_most_once_across_concurrent_callers() {
        // Ten concurrent callers, one run of the body.
        let calls = (0..10)
            .map(|_| tokio::spawn(async { counted_async_fixture().await }))
            .collect::<Vec<_>>();
        for c in calls {
            let v = c.await.unwrap();
            assert_eq!(v, &100);
        }
        assert_eq!(ASYNC_INIT_RUNS.load(Ordering::SeqCst), 1);
    }

    // The macro accepts a visibility modifier.
    setup_test_data!(
        pub fn pub_shared_pi() -> f64 {
            3.14159
        }
    );

    #[test]
    fn pub_vis_variant_compiles_and_returns_value() {
        assert!((pub_shared_pi() - 3.14159).abs() < 1e-9);
    }
}
