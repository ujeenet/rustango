//! Tag-based test filtering — Django's `@tag('slow', 'core')` +
//! `manage test --tag fast --exclude-tag slow`. Issue #45.
//!
//! Rust has no built-in per-test tag mechanism (`cargo test` only
//! filters on substring match against the test name). This module
//! provides a thin convention that works with stock `#[test]` /
//! `#[tokio::test]`:
//!
//! ```ignore
//! use rustango::test_filter::tags;
//!
//! #[tokio::test]
//! async fn expensive_integration() {
//!     tags!("slow", "integration");
//!     // ...heavy work...
//! }
//! ```
//!
//! The [`tags!`] macro reads the `RUSTANGO_TEST_TAGS` (CSV, include
//! list) and `RUSTANGO_TEST_EXCLUDE_TAGS` (CSV, exclude list) env
//! vars and early-returns from the test if filtered out. The test
//! still shows as `ok` in `cargo test` output — that's the same
//! shape `#[ignore]` would give, just gated on runtime env instead
//! of source-time attribute.
//!
//! ## Semantics
//!
//! - Include list **empty** (env unset) → include everything.
//! - Include list **non-empty** → run only tests whose tag set
//!   intersects the include list.
//! - Exclude list always wins: any tag in the exclude list filters
//!   the test out.
//! - Tags are case-sensitive and trimmed; empty CSV tokens are
//!   ignored.
//!
//! ## Examples
//!
//! ```text
//! # default — run every test
//! cargo test
//!
//! # only `slow`-tagged tests
//! RUSTANGO_TEST_TAGS=slow cargo test
//!
//! # skip `slow`-tagged tests
//! RUSTANGO_TEST_EXCLUDE_TAGS=slow cargo test
//!
//! # combine: only `core` *and* not `flaky`
//! RUSTANGO_TEST_TAGS=core RUSTANGO_TEST_EXCLUDE_TAGS=flaky cargo test
//! ```

/// Environment variable name for the CSV include list.
pub const ENV_INCLUDE: &str = "RUSTANGO_TEST_TAGS";

/// Environment variable name for the CSV exclude list.
pub const ENV_EXCLUDE: &str = "RUSTANGO_TEST_EXCLUDE_TAGS";

/// Decide whether a test marked with `tags` should execute.
///
/// Reads `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` at call
/// time. Returns `true` to run; `false` to skip.
#[must_use]
pub fn should_run(tags: &[&str]) -> bool {
    should_run_with(tags, |name| std::env::var(name).ok())
}

/// Same as [`should_run`] but reads env vars through an injectable
/// resolver. The standard `should_run` is a thin wrapper that passes
/// `std::env::var(...).ok()`; this variant lets tests inject a fake
/// resolver without touching process-global env (which is unsafe to
/// mutate under `forbid(unsafe_code)` on Rust 2024+).
#[must_use]
pub fn should_run_with(tags: &[&str], env_get: impl Fn(&str) -> Option<String>) -> bool {
    let include = env_get(ENV_INCLUDE).unwrap_or_default();
    let exclude = env_get(ENV_EXCLUDE).unwrap_or_default();
    decide(tags, &include, &exclude)
}

/// Pure decision function — same logic as [`should_run`] but with
/// explicit `include`/`exclude` CSV strings. Useful for unit-testing
/// the policy without env-var fiddling.
#[must_use]
pub fn decide(tags: &[&str], include_csv: &str, exclude_csv: &str) -> bool {
    let include: Vec<&str> = csv(include_csv);
    let exclude: Vec<&str> = csv(exclude_csv);

    // Exclude wins: any overlap with the exclude list filters out.
    if tags.iter().any(|t| exclude.contains(t)) {
        return false;
    }
    // Empty include list = run everything.
    if include.is_empty() {
        return true;
    }
    // Non-empty include: at least one of the test's tags must
    // appear in the include list.
    tags.iter().any(|t| include.contains(t))
}

fn csv(s: &str) -> Vec<&str> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Declare this test's tags and early-return if the current
/// `RUSTANGO_TEST_TAGS` / `RUSTANGO_TEST_EXCLUDE_TAGS` env vars
/// say it shouldn't run. Place at the top of the test body —
/// before any setup that you don't want to run for filtered-out
/// tests.
///
/// ```ignore
/// #[tokio::test]
/// async fn slow_integration() {
///     rustango::tags!("slow", "integration");
///     // ...
/// }
/// ```
#[macro_export]
macro_rules! tags {
    ($($tag:expr),+ $(,)?) => {
        if !$crate::test_filter::should_run(&[$($tag),+]) {
            return;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_include_and_exclude_runs_everything() {
        assert!(decide(&["slow"], "", ""));
        assert!(decide(&[], "", ""));
        assert!(decide(&["a", "b"], "", ""));
    }

    #[test]
    fn include_filters_in_matching_tags() {
        assert!(decide(&["slow"], "slow", ""));
        assert!(decide(&["slow", "core"], "core", ""));
        assert!(!decide(&["slow"], "fast", ""));
        assert!(
            !decide(&[], "fast", ""),
            "untagged test skipped when include set"
        );
    }

    #[test]
    fn exclude_filters_out_matching_tags() {
        assert!(!decide(&["slow"], "", "slow"));
        assert!(!decide(&["slow", "core"], "", "slow"));
        assert!(decide(&["core"], "", "slow"));
    }

    #[test]
    fn exclude_wins_over_include() {
        // tagged with both — include says yes, exclude says no →
        // exclude wins.
        assert!(!decide(&["core", "flaky"], "core", "flaky"));
    }

    #[test]
    fn csv_handles_whitespace_and_empties() {
        assert!(decide(&["slow"], " slow , fast ", ""));
        assert!(!decide(&["slow"], "fast,, ,", ""));
        assert!(!decide(&["slow"], "", " slow , "));
    }

    #[test]
    fn case_sensitive_tags() {
        // Tags are case-sensitive — `Slow` and `slow` are distinct.
        assert!(!decide(&["Slow"], "slow", ""));
        assert!(decide(&["slow"], "slow", ""));
    }

    #[test]
    fn macro_compiles_with_one_tag() {
        // We can't easily test the early-return without spawning a
        // child process, but we *can* verify the macro expands and
        // type-checks. This test always runs (the include list is
        // empty under normal cargo test invocation).
        crate::tags!("compile-check");
        // If we got here, the macro didn't filter us out.
        let _ = 1 + 1;
    }

    #[test]
    fn macro_compiles_with_multiple_tags_and_trailing_comma() {
        crate::tags!("a", "b", "c",);
        let _ = 1 + 1;
    }

    #[test]
    fn should_run_with_uses_injected_env_resolver() {
        let env = |name: &str| match name {
            ENV_INCLUDE => Some("fast".to_owned()),
            ENV_EXCLUDE => Some("slow".to_owned()),
            _ => None,
        };
        assert!(should_run_with(&["fast"], &env));
        assert!(!should_run_with(&["slow"], &env));
        assert!(!should_run_with(&["other"], &env));
    }

    #[test]
    fn should_run_with_empty_resolver_runs_everything() {
        let env = |_: &str| None;
        assert!(should_run_with(&["slow"], &env));
        assert!(should_run_with(&[], &env));
    }
}
