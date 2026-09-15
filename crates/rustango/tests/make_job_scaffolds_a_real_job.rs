//! `make:job` must scaffold a `jobs::Job` (#1455).
//!
//! It used to emit a struct holding a `PgPool` with an inherent
//! `run(self: Arc<Self>)` and a comment wiring it to
//! `scheduler::every(..)`. That is a *scheduler task* — a real thing the
//! framework has, but not the one the verb is named after. Nothing it
//! produced could be dispatched, registered, retried, backed off or
//! dead-lettered, so a reader who ran `make:job` and then opened
//! `docs/jobs.md` was looking at two unrelated things with one name.
//!
//! It also hardcoded `PgPool`, so the generated file did not compile at
//! all in a project built `--no-default-features --features sqlite`.
//! Both the job queue and the scheduler are tri-dialect; only this
//! template was not.
//!
//! The scheduler shape now lives under `make:scheduled`.

#![cfg(feature = "manage")]

use std::path::PathBuf;
use std::sync::Mutex;

/// Both verbs write relative to the process working directory, so these
/// tests chdir — global state, hence one lock. Each integration test
/// file is its own binary, so nothing outside can race us.
static CWD: Mutex<()> = Mutex::new(());

fn generate(verb: &str, tag: &str) -> String {
    let _guard = CWD.lock().unwrap_or_else(|e| e.into_inner());

    let tmp: PathBuf =
        std::env::temp_dir().join(format!("rustango-mkjob-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).expect("mkdir src");

    let original = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&tmp).expect("chdir");
    let args = vec![verb.to_owned(), "SendReceipt".to_owned()];
    let dispatched = rustango::migrate::manage::run_pool_free(&args, &mut Vec::new());
    std::env::set_current_dir(&original).expect("chdir back");

    dispatched
        .unwrap_or_else(|| panic!("`{verb}` must be dispatchable without a pool"))
        .unwrap_or_else(|e| panic!("`{verb}` failed: {e}"));

    let written = std::fs::read_to_string(tmp.join("src/send_receipt.rs")).expect("generated file");
    let _ = std::fs::remove_dir_all(&tmp);
    written
}

#[test]
fn make_job_emits_a_job_impl() {
    let body = generate("make:job", "job");

    assert!(
        body.contains("impl Job for SendReceipt"),
        "make:job must implement `jobs::Job` — the verb is named after it. Got:\n{body}"
    );
    for needed in [
        "const NAME: &'static str",
        "const MAX_ATTEMPTS: u32",
        "async fn run(&self) -> Result<(), JobError>",
    ] {
        assert!(
            body.contains(needed),
            "the generated job is missing `{needed}`, so it is not a `Job`. Got:\n{body}"
        );
    }
    // A Job payload is serialised into the queue row; without these
    // derives the impl does not satisfy the trait bounds.
    assert!(
        body.contains("Serialize") && body.contains("Deserialize"),
        "a Job payload must derive Serialize + Deserialize. Got:\n{body}"
    );
}

/// The template must compile in a project that has no Postgres.
#[test]
fn make_job_is_not_postgres_only() {
    let body = generate("make:job", "dialect");
    assert!(
        !body.contains("PgPool"),
        "the job template must not name PgPool — it pinned every generated job to \
         Postgres and would not compile under `--features sqlite`. Got:\n{body}"
    );
}

/// `run(&self)` gets the payload and nothing else. A template that
/// implies otherwise sends people looking for a pool argument that does
/// not exist.
#[test]
fn make_job_says_what_run_receives() {
    let body = generate("make:job", "context");
    assert!(
        body.contains("no pool") && body.contains("no tenant"),
        "the template must say that `run` receives only the payload — no pool, no \
         tenant, no request context. Got:\n{body}"
    );
}

/// `MAX_ATTEMPTS` is a total-attempt ceiling, and the docs called it a
/// retry ceiling for long enough (#1410) that the template should not
/// repeat the mistake.
#[test]
fn make_job_describes_max_attempts_correctly() {
    let body = generate("make:job", "attempts");
    assert!(
        body.contains("total attempts") || body.contains("**total attempts**"),
        "the template must describe MAX_ATTEMPTS as a total-attempt ceiling, not a \
         retry ceiling (#1410). Got:\n{body}"
    );
}

/// The scheduler shape did not disappear — it moved to the verb that
/// describes it, and lost its hardcoded `PgPool` on the way.
#[test]
fn make_scheduled_keeps_the_timer_shape_tri_dialect() {
    let body = generate("make:scheduled", "sched");

    assert!(
        body.contains("scheduler::Scheduler") && body.contains("every("),
        "make:scheduled must scaffold a fixed-interval task. Got:\n{body}"
    );
    assert!(
        !body.contains("PgPool") && body.contains("sql::Pool"),
        "make:scheduled must route through `sql::Pool` so it compiles on every \
         backend. Got:\n{body}"
    );
    assert!(
        !body.contains("impl Job for"),
        "make:scheduled is the timer shape, not a queue job — that is make:job. \
         Got:\n{body}"
    );
}
