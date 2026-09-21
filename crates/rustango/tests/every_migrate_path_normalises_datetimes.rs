//! Every path that applies migrations must also run the #1464 SQLite
//! datetime sweep.
//!
//! The sweep converts values written by the pre-0.57.11 default into
//! the one shape the ORM now binds. It is not optional maintenance: the
//! same release changed every write path, so a database that is
//! migrated but not swept holds a shape nothing compares against, and
//! `migrate` reports success.
//!
//! It shipped wired into exactly one function —
//! `migrate::manage::migrate`, the single-database path — and
//! `tenancy::manage` intercepts the `migrate` verb *before* the
//! fall-through to that function, deliberately, so migrations stay
//! scope-aware. The result was that no tenancy deployment ran the sweep
//! at all: not one tenant database, not even the registry.
//!
//! ## Why a source scan
//!
//! The defect is an **absent call site**. There is no value to assert
//! and no behaviour to observe — the function simply is not reached, and
//! a behavioural test would need a registry, orgs and per-tenant
//! databases to demonstrate an omission that is plain in the source.
//! A scan also covers the seam added next year by someone who has not
//! read #1464, which is the case that matters.

use std::path::{Path, PathBuf};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn read(rel: &str) -> String {
    let p = src().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Count the lines that **call** `name`, excluding its declaration.
///
/// Two mistakes were made here in sequence and both mattered:
///
/// 1. `text.contains(name)` — the helper's own `async fn` line contains
///    its name, so the check passed with every call site deleted.
/// 2. Excluding only declarations — but
///    `normalise_datetimes_quietly`'s *body* calls
///    `normalise_sqlite_datetimes`, so searching for "any sweep name"
///    still matched after the four seams were removed.
///
/// So the question has to be asked about one specific function, and
/// answered by counting call sites rather than mentions. That is what
/// the sibling count below was already doing correctly; this is the
/// same logic, applied where it should have been from the start
/// (#1616 rework review, tenancy-004).
fn call_sites(text: &str, name: &str) -> usize {
    text.lines()
        .filter(|l| {
            let calls = l.contains(&format!("{name}("));
            let declares = l.contains("fn ") || l.trim_start().starts_with("///");
            calls && !declares
        })
        .count()
}

#[test]
fn the_single_database_migrate_runs_the_sweep() {
    assert!(
        call_sites(&read("migrate/manage.rs"), "normalise_sqlite_datetimes") > 0,
        "migrate::manage no longer runs the #1464 datetime sweep. A database \
         migrated without it holds timestamps in a shape the ORM does not \
         compare against, and `migrate` reports success."
    );
}

#[test]
fn the_tenancy_migrate_paths_run_the_sweep() {
    assert!(
        call_sites(&read("tenancy/migrate.rs"), "normalise_datetimes_quietly") > 0,
        "tenancy::migrate no longer runs the #1464 datetime sweep. This is the \
         state the fix shipped in: `tenancy::manage` intercepts `migrate` \
         before `migrate::manage`, so tenancy deployments never reach the \
         single-database call site and every tenant database is left \
         un-normalised."
    );
}

/// The sweep belongs beside the per-database seeding, and this pins that
/// pairing rather than the presence of a call somewhere in the file.
///
/// `contenttypes::ensure_seeded` runs once per database the tenancy
/// migrator touches — the registry, then each tenant. The sweep has the
/// same shape: same scope, same "runs after the schema is in place".
/// So a new seam that seeds a database and does not sweep it is the
/// exact omission this file exists to catch, and comparing the two
/// counts catches it without naming any particular seam.
#[test]
fn every_seeded_database_is_also_swept() {
    let text = read("tenancy/migrate.rs");
    let seeded = text.matches("contenttypes::ensure_seeded(").count();
    // Call sites only — the helper's own `async fn` line contains the
    // name too, and counting it hid one missing seam when this guard
    // was first written.
    let swept = call_sites(&text, "normalise_datetimes_quietly");

    assert!(
        seeded > 0,
        "expected `contenttypes::ensure_seeded` to anchor this check; it is \
         gone, so the pairing this test measures no longer exists and the \
         test needs rewriting rather than deleting"
    );
    assert_eq!(
        swept, seeded,
        "tenancy::migrate seeds {seeded} database(s) but sweeps {swept}. \
         Every database the migrator touches needs the #1464 sweep, or that \
         one keeps timestamps in a shape the ORM cannot compare against. If \
         a new seam genuinely should not be swept, say why here rather than \
         letting the counts drift."
    );
}
