//! The generated `docker-compose.yml` must not throw away the developer's
//! database (#1309).
//!
//! Without a named volume, a database container's data lives in its
//! writable layer. `docker compose down` deletes it and `up -d` returns an
//! empty database, saying nothing either way. The generated file declared
//! three named volumes for cargo caches and none for the data anyone would
//! miss.
//!
//! That cost more than the surprise: it produced a false bug report. In
//! #1307 a reporter concluded migrations were re-running because their
//! tables kept vanishing; the tables were vanishing because `down` had
//! removed them.
//!
//! Asserted per backend, because the mount path differs and SQLite has no
//! service to mount anything into. A volume declared with nothing mounting
//! it is an error to compose, so the declaration and the mount have to
//! agree — which is the pairing this pins.
//!
//! Generates through the real binary rather than calling the template
//! function: `cargo-rustango` has no library target, and going through the
//! binary is what a user does anyway.

use std::path::PathBuf;

/// Scaffold a project for `backend` and return its `docker-compose.yml`.
fn compose_for(backend: &str) -> String {
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("compose-{backend}"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("create work dir");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-rustango"))
        .args(["new", "probe", "--backend", backend])
        .current_dir(&work)
        .output()
        .expect("run scaffolder");
    assert!(
        out.status.success(),
        "scaffolding --backend {backend} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::read_to_string(work.join("probe").join("docker-compose.yml"))
        .expect("read generated docker-compose.yml")
}

#[test]
fn postgres_keeps_its_data_across_compose_down() {
    let yml = compose_for("postgres");
    assert!(
        yml.contains("- postgres-data:/var/lib/postgresql/data"),
        "the postgres service must mount a named volume at its data \
         directory, or `docker compose down` wipes the database:\n{yml}"
    );
    assert!(
        yml.contains("\n  postgres-data:\n"),
        "a mounted named volume must also be declared under `volumes:`, or \
         compose refuses the file:\n{yml}"
    );
    assert_eq!(
        yml.matches("postgres-data").count(),
        2,
        "expected exactly one mount and one declaration:\n{yml}"
    );
}

#[test]
fn mysql_keeps_its_data_across_compose_down() {
    let yml = compose_for("mysql");
    assert!(
        yml.contains("- mysql-data:/var/lib/mysql"),
        "the mysql service must mount a named volume at its data \
         directory:\n{yml}"
    );
    assert!(
        yml.contains("\n  mysql-data:\n"),
        "a mounted named volume must also be declared under `volumes:`:\n{yml}"
    );
    assert_eq!(
        yml.matches("mysql-data").count(),
        2,
        "expected exactly one mount and one declaration:\n{yml}"
    );
}

/// SQLite is a file in the bind mount, so there is no service and nothing
/// to mount. Declaring a volume nothing uses would make compose reject the
/// file outright.
#[test]
fn sqlite_declares_no_database_volume() {
    let yml = compose_for("sqlite");
    assert!(
        !yml.contains("postgres-data") && !yml.contains("mysql-data"),
        "sqlite has no database service, so it must declare no database \
         volume:\n{yml}"
    );
    assert!(
        yml.contains("\n  cargo-target:\n"),
        "the cargo caches are still declared — this is not `no volumes`:\n{yml}"
    );
}
