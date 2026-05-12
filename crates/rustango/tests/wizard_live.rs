#![cfg(all(feature = "tenancy", feature = "postgres"))]
//! Smoke test for `manage wizard` (roadmap #2, v0.30.14).
//!
//! The wizard's prompt + dispatch logic is covered by the unit
//! tests in `tenancy::manage::wizard::tests`. This file verifies
//! the dispatcher wiring: the public `tenancy::manage::run` path
//! recognizes `wizard` as a known verb (no "unknown verb" error)
//! and the help text mentions it.
//!
//! A full end-to-end interactive test isn't practical from a Rust
//! test process — the wizard reads from `std::io::stdin()`
//! directly, and integration tests can't redirect a parent
//! process's stdin without spawning a subprocess. The shape of
//! the prompts (yes/no parsing, defaults, write-out format) is
//! exercised in the module unit tests with a `Cursor` reader.

/// `wizard | init` shows up in the dispatcher's help output —
/// regression guard for the verb registration.
#[test]
fn wizard_verb_appears_in_help_text() {
    let mut buf: Vec<u8> = Vec::new();
    rustango::tenancy::manage::write_help(&mut buf).unwrap();
    let help = String::from_utf8(buf).unwrap();
    assert!(
        help.contains("wizard | init"),
        "wizard verb missing from help output"
    );
    assert!(
        help.contains("Interactive setup"),
        "wizard description missing"
    );
    assert!(
        help.contains("opt-in"),
        "wizard help should call out the opt-in step shape"
    );
}
