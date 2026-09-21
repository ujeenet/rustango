//! Run tests by tag, like Django's `@tag('slow', 'core')`.
//!
//! `cargo test` can only filter on the test name. This module adds
//! tags that work with plain `#[test]` and `#[tokio::test]`:
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
//! The [`tags!`] macro reads two comma-separated env vars,
//! `RUSTANGO_TEST_TAGS` to include and `RUSTANGO_TEST_EXCLUDE_TAGS`
//! to exclude, and returns early when the test is filtered out. A
//! skipped test still prints as `ok`.
//!
//! ## Rules
//!
//! - No include list: run everything.
//! - With an include list: run a test only if one of its tags is on
//!   that list.
//! - The exclude list always wins.
//! - Tags are case-sensitive. Spaces around them are trimmed and
//!   empty entries are ignored.
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

/// Env var holding the include list.
pub const ENV_INCLUDE: &str = "RUSTANGO_TEST_TAGS";

/// Env var holding the exclude list.
pub const ENV_EXCLUDE: &str = "RUSTANGO_TEST_EXCLUDE_TAGS";

/// `true` if a test with these tags should run. Reads both env vars
/// each time it is called.
#[must_use]
pub fn should_run(tags: &[&str]) -> bool {
    should_run_with(tags, |name| std::env::var(name).ok())
}

/// Like [`should_run`], but you supply the env lookup. Tests use
/// this to fake the vars instead of changing the process
/// environment, which is unsafe in Rust 2024.
#[must_use]
pub fn should_run_with(tags: &[&str], env_get: impl Fn(&str) -> Option<String>) -> bool {
    let include = env_get(ENV_INCLUDE).unwrap_or_default();
    let exclude = env_get(ENV_EXCLUDE).unwrap_or_default();
    decide(tags, &include, &exclude)
}

/// The filter rule itself, with the two lists passed in directly.
#[must_use]
pub fn decide(tags: &[&str], include_csv: &str, exclude_csv: &str) -> bool {
    let include: Vec<&str> = csv(include_csv);
    let exclude: Vec<&str> = csv(exclude_csv);

    // Exclude wins over everything else.
    if tags.iter().any(|t| exclude.contains(t)) {
        return false;
    }
    if include.is_empty() {
        return true;
    }
    tags.iter().any(|t| include.contains(t))
}

fn csv(s: &str) -> Vec<&str> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Declare this test's tags and return early if the env vars filter
/// it out. Put it at the top of the test body, before any setup.
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
        // Include says yes, exclude says no, so the test is skipped.
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
        // `Slow` and `slow` are different tags.
        assert!(!decide(&["Slow"], "slow", ""));
        assert!(decide(&["slow"], "slow", ""));
    }

    #[test]
    fn macro_compiles_with_one_tag() {
        // Checks that the macro expands and type-checks. With no
        // include list set, it never filters this test out.
        crate::tags!("compile-check");
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
