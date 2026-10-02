//! `make:worker` exists to encode one thing correctly, so that one
//! thing is what this guards.
//!
//! A worker that awaits `tokio::signal::ctrl_c()` handles SIGINT and
//! **not** SIGTERM. SIGTERM is what `docker stop`, Kubernetes and
//! systemd actually send, so such a worker never drains: the container
//! is SIGKILLed when its grace period expires, in-flight jobs are lost,
//! and nothing is logged — the process exits 0. That is #1409 in a
//! worker rather than a server, and it is invisible until production.
//!
//! The assertion is therefore not "the template mentions shutdown" but
//! "the template does not await ctrl_c directly", which is the mistake
//! being prevented. Revert `make_worker_cmd` to `ctrl_c()` and this
//! fails.

#![cfg(feature = "manage")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `make:worker` writes to `src/bin/` relative to the *process* working
/// directory, so these tests have to chdir — which is global state. One
/// lock for the file; each integration test file is its own binary, so
/// nothing outside it can race us.
static CWD: Mutex<()> = Mutex::new(());

/// Run `make:worker` in a scratch directory and return what it wrote.
fn generate(tag: &str) -> String {
    let _guard = CWD.lock().unwrap_or_else(|e| e.into_inner());

    let tmp: PathBuf =
        std::env::temp_dir().join(format!("rustango-mkworker-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).expect("mkdir src");

    let original = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&tmp).expect("chdir to scratch");

    let args = vec!["make:worker".to_owned(), "JobsWorker".to_owned()];
    let mut out = Vec::new();
    let dispatched = rustango::migrate::manage::run_pool_free(&args, &mut out);

    // Restore before asserting, so a failure does not strand the whole
    // test binary in a deleted directory.
    std::env::set_current_dir(&original).expect("chdir back");

    dispatched
        .expect("`make:worker` must be dispatchable without a pool")
        .expect("`make:worker` must succeed");

    let written = std::fs::read_to_string(tmp.join("src/bin/jobs_worker.rs"))
        .expect("make:worker must write src/bin/jobs_worker.rs");
    let stdout = String::from_utf8_lossy(&out).into_owned();
    let _ = std::fs::remove_dir_all(&tmp);

    // It is a binary, not a module: cargo auto-discovers `src/bin/*.rs`,
    // so telling the reader to add `mod ...` would be wrong advice.
    assert!(
        stdout.contains("cargo run --bin jobs_worker"),
        "make:worker must tell the reader how to run the binary, not how to declare a \
         module. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains("add `mod"),
        "a `src/bin/` target needs no `mod` declaration — that advice belongs to \
         `write_generated`, not to a binary. Got:\n{stdout}"
    );

    written
}

/// Drop comment lines and trailing comments.
///
/// The template *explains* why `ctrl_c` is wrong, so a bare
/// `contains("ctrl_c")` matches the warning as readily as the mistake —
/// it fired on the correct template the first time this ran. What
/// matters is whether the generated **code** awaits it.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_generated_worker_takes_sigterm_not_just_ctrl_c() {
    let body = generate("sigterm");
    let code = code_only(&body);

    assert!(
        code.contains("shutdown::shutdown_signal()"),
        "the generated worker must await `shutdown::shutdown_signal()`, which takes \
         SIGINT and SIGTERM. Got:\n{body}"
    );
    // The actual defect being prevented: awaiting ctrl_c is SIGINT-only.
    assert!(
        !code.contains("ctrl_c"),
        "the generated worker's code must not await `ctrl_c` — it is SIGINT-only, so \
         `docker stop` (SIGTERM) would skip the drain entirely. (A comment explaining \
         that is fine; this checks the code.) Got:\n{body}"
    );
    // Draining has to happen after the signal, or the await is decoration.
    let sig = body
        .find("shutdown_signal()")
        .expect("signal await present");
    let drain = body.find(".shutdown().await").expect("queue drain present");
    assert!(
        drain > sig,
        "the queue drain must come *after* the signal await, otherwise the worker \
         drains an idle queue at boot and abandons in-flight work on the way out. \
         Got:\n{body}"
    );
}

#[test]
fn the_generated_worker_warns_about_unregistered_job_names() {
    let body = generate("register");

    // A DB-queue worker that picks up a row whose name is unregistered
    // in *this* process logs and returns without unlocking it. The row
    // is then stranded until a reclaim sweep, and `pending_count()`
    // never shows it. The template has to say so where someone editing
    // it will read it.
    assert!(
        body.contains("register") && body.contains("stranded"),
        "the template must tell the reader to register every job type and say what \
         happens if they don't — a stranded row is invisible in `pending_count()`. \
         Got:\n{body}"
    );
}

#[test]
fn the_generated_worker_is_not_postgres_only() {
    let body = generate("dialect");

    // `make:job` hardcodes `PgPool` and so only compiles on Postgres.
    // The queue itself is tri-dialect (its DDL and row-pickup strategy
    // are chosen from the pool's dialect), so the worker template must
    // go through `sql::Pool` and the `_pool` API family — the bare
    // `with_workers`/`new`/`ensure_table` are `#[cfg(feature =
    // "postgres")]` and would not compile under `--features sqlite`.
    assert!(
        !body.contains("PgPool"),
        "the worker template must not name PgPool — it would pin the generated \
         project to Postgres. Got:\n{body}"
    );
    for needed in [
        "sql::Pool",
        "ensure_table_pool",
        "with_workers_pool",
        "reclaim_stuck_jobs_pool",
    ] {
        assert!(
            body.contains(needed),
            "the worker template must use the tri-dialect `_pool` API (`{needed}` \
             missing) — the bare constructors are postgres-gated. Got:\n{body}"
        );
    }
}

/// A generated binary is the first thing an operator runs, usually with
/// something misconfigured. `std::env::var(..)?` surfaces as the bare
/// word `NotPresent`, which names neither the variable nor the fix —
/// the template said exactly that until this test was written.
#[test]
fn the_generated_worker_names_the_env_var_it_is_missing() {
    let body = generate("envvar");
    assert!(
        body.contains("missing env var 'DATABASE_URL'"),
        "the generated worker must say which variable is missing and how to set it, \
         not propagate `VarError` as `NotPresent`. Got:\n{body}"
    );
}

/// The path is part of the contract: cargo only auto-discovers binaries
/// under `src/bin/`.
#[test]
fn it_lands_in_src_bin() {
    let _guard = CWD.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = std::env::temp_dir().join(format!("rustango-mkworker-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).expect("mkdir src");

    let original = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&tmp).expect("chdir");
    let args = vec!["make:worker".to_owned(), "JobsWorker".to_owned()];
    let _ = rustango::migrate::manage::run_pool_free(&args, &mut Vec::new());
    std::env::set_current_dir(&original).expect("chdir back");

    assert!(
        Path::new(&tmp).join("src/bin/jobs_worker.rs").is_file(),
        "make:worker must write src/bin/<snake>.rs — cargo auto-discovers binaries \
         only there"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
