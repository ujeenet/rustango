//! Slice 17.1 invariant: `#[derive(Model)]` (and friends) must not
//! emit backend `#[cfg(feature = ...)]` arms into consumer-crate code,
//! where they test the *consumer's* features, not rustango's.
//!
//! The PG-typed `LoadRelated`, `save` / `save_on`, reverse-FK and
//! aliased-row helpers still carry `cfg(feature = "postgres")`; moving
//! them behind rustango-side aliases changes generated code, so it waits
//! for a minor release. Until then this is a ratchet: no MySQL or SQLite
//! arm at all, and no new Postgres one.
//!
//! Grep, not expansion: a consumer-side check needs a fixture crate
//! built without rustango's default features.

use std::path::PathBuf;

/// `cfg(feature = "postgres")` occurrences left in the macro source.
/// Lower it when one goes; never raise it.
const POSTGRES_ARMS_LEFT: usize = 24;

fn macro_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("rustango-macros/src/lib.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn macro_emits_no_mysql_or_sqlite_cfg_arms() {
    let body = macro_source();
    for needle in [r#"cfg(feature = "mysql")"#, r#"cfg(feature = "sqlite")"#] {
        assert!(
            !body.contains(needle),
            "rustango-macros/src/lib.rs contains `{needle}`: backend cfgs must not be \
             emitted into consumer code. Gate inside rustango and call it unconditionally."
        );
    }
}

#[test]
fn macro_emits_no_new_postgres_cfg_arms() {
    let n = macro_source()
        .matches(r#"cfg(feature = "postgres")"#)
        .count();
    assert_eq!(
        n, POSTGRES_ARMS_LEFT,
        "`cfg(feature = \"postgres\")` count in rustango-macros/src/lib.rs changed: gate new \
         backend code inside rustango, and lower POSTGRES_ARMS_LEFT when one goes."
    );
}
