//! Class-level test fixtures — Django's `setUpTestData(cls)`.
//!
//! Django runs `setUpTestData` once per test class and shares the
//! resulting objects across every test method. The `tests` framework
//! wraps each test in a transaction that rolls back, so mutations
//! during one test don't leak. Rust has no test classes; the
//! idiomatic equivalent is a per-module `OnceCell` whose async
//! initializer runs lazily on the first access.
//!
//! This module ships the [`setup_test_data!`] / [`setup_test_data_async!`]
//! macros that wrap that pattern in a Django-shape API:
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
//! ## Scope and caveats vs Django
//!
//! - **Lifetime**: the fixture lives for the entire test-binary
//!   process, not "per test class." `cargo test` runs each
//!   integration-test file as a separate process, which is the
//!   coarsest analog of Django's class boundary. Test methods in the
//!   same file share the cell.
//! - **Transaction rollback**: Django's `TestCase` wraps each test
//!   in a savepoint that rolls back, so per-test mutations don't
//!   contaminate the shared fixture. Rustango doesn't ship that
//!   wrapper yet (issue #39 four-tier TestCase). Treat
//!   `setup_test_data!` fixtures as **read-only** for now —
//!   if your tests need to mutate shared state, fall back to
//!   per-test setup or wrap each test in `atomic!` and explicitly
//!   roll back.
//! - **Init failures**: the macro `.unwrap()`s the closure result.
//!   For richer error handling, write the OnceCell out longhand —
//!   the macro is sugar for the common case.
//!
//! Issue #42.

/// Class-level **synchronous** test fixture — Django's
/// `setUpTestData` for fixtures that don't need an async runtime.
/// Wraps a [`std::sync::OnceLock`] in a function-call surface.
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
/// The fixture initializer runs at most once per test-binary
/// process — every subsequent call returns a `&'static Type`
/// reference to the same value.
#[macro_export]
macro_rules! setup_test_data {
    ($vis:vis fn $name:ident () -> $ty:ty $body:block) => {
        $vis fn $name() -> &'static $ty {
            static CELL: ::std::sync::OnceLock<$ty> = ::std::sync::OnceLock::new();
            CELL.get_or_init(|| $body)
        }
    };
}

/// Class-level **async** test fixture — Django's `setUpTestData`
/// for fixtures that need to hit the database (the common case for
/// model-row fixtures). Wraps a [`tokio::sync::OnceCell`] so
/// concurrent first-access calls all wait on the same future, with
/// only one actually running the init body.
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
        // Pointer identity: the second call must return the same
        // `&'static` — that's the whole point of `OnceLock`.
        let n2 = shared_numbers();
        assert!(
            std::ptr::eq(n, n2),
            "two calls should return the same &'static Vec",
        );
    }

    // Pin "init runs at most once": share a counter across two
    // accessor calls.
    static INIT_RUNS: AtomicUsize = AtomicUsize::new(0);
    setup_test_data!(
        fn counted_fixture() -> i32 {
            INIT_RUNS.fetch_add(1, Ordering::SeqCst);
            42
        }
    );

    #[test]
    fn sync_fixture_init_runs_at_most_once() {
        // Force-evaluate first.
        let _ = counted_fixture();
        let after_first = INIT_RUNS.load(Ordering::SeqCst);
        // Second call must not re-run init.
        let _ = counted_fixture();
        let after_second = INIT_RUNS.load(Ordering::SeqCst);
        assert_eq!(after_first, after_second, "init re-ran on second call");
        // And the count is exactly 1 (other tests don't share this fixture).
        assert!(after_first <= 1, "init counter: {after_first}");
    }

    setup_test_data_async!(
        async fn shared_async_fixture() -> Vec<String> {
            // Touching `tokio::time::sleep` proves the body is actually
            // an async block. Don't sleep long enough to slow down `cargo
            // test`.
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
        // Fire ten concurrent callers and verify only one ran init.
        let calls = (0..10)
            .map(|_| tokio::spawn(async { counted_async_fixture().await }))
            .collect::<Vec<_>>();
        for c in calls {
            let v = c.await.unwrap();
            assert_eq!(v, &100);
        }
        // tokio's OnceCell guarantees single init even under
        // contention.
        assert_eq!(ASYNC_INIT_RUNS.load(Ordering::SeqCst), 1);
    }

    // Pub-visibility variant — pin that the macro accepts a `vis`
    // modifier (some test modules `pub use` their fixtures).
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
