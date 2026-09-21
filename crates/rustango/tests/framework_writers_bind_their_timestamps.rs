#![cfg(feature = "sqlite")]
//! Every framework writer stamps its own timestamps (#1464).
//!
//! The ORM's `auto_now_add` / `auto_now` are covered by
//! `auto_now_add_sqlite_format`. This file covers the writers that do
//! not go through a model — `audit`, `jobs`, `i18n`, and the migration
//! ledger — each of which used to omit its timestamp column from the
//! INSERT and let the column default fire.
//!
//! ## Why every fixture creates the table the *old* way
//!
//! The current DDL emits the canonical `strftime` default, so a table
//! built by `ensure_table_pool` produces canonical rows whether or not
//! the writer binds anything. A test on that table passes on broken
//! code — which is the only thing worth guarding against here.
//!
//! So each fixture creates the table with `DEFAULT CURRENT_TIMESTAMP`,
//! which is exactly what a database created before #1464 still holds
//! and will hold forever: SQLite's `ALTER TABLE` grammar is
//! RENAME / ADD / DROP, and none of those replaces a column default.
//! On that table the default is observable — it writes
//! `YYYY-MM-DD HH:MM:SS`, whose separator at index 10 is `' '` (0x20)
//! rather than `'T'` (0x54). Since SQLite stores datetimes as TEXT and
//! compares them lexicographically, such a row sorts below every
//! canonical one no matter what instant it holds.
//!
//! Revert any one of these binds and its test fails with the stored
//! value in the message.

use rustango::core::SqlValue;
use rustango::sql::Pool;

/// The canonical shape, checked structurally rather than by matching a
/// format string: separator at index 10, six fractional digits, and an
/// explicit `+00:00` offset.
///
/// Spelled as a property because the failure is a *second* spelling of
/// the same instant, and a test that compared against one literal would
/// pass on any other wrong one.
fn assert_canonical(what: &str, stored: &str) {
    let b = stored.as_bytes();
    assert!(
        b.len() == 32 && b[10] == b'T' && stored.ends_with("+00:00") && b[19] == b'.',
        "{what} must be stored in the canonical shape \
         `YYYY-MM-DDTHH:MM:SS.ffffff+00:00` — the column default on this \
         table is the pre-#1464 `CURRENT_TIMESTAMP`, so a value in any \
         other shape means the writer did not bind and the default \
         fired. Stored: {stored:?}"
    );
}

async fn column(pool: &Pool, sql: &str) -> Vec<String> {
    let rows: Vec<(String,)> = rustango::sql::raw_query_pool(sql, Vec::new(), pool)
        .await
        .expect("read back");
    rows.into_iter().map(|r| r.0).collect()
}

async fn exec(pool: &Pool, sql: &str) {
    rustango::sql::raw_execute_pool(pool, sql, Vec::new())
        .await
        .expect("fixture DDL");
}

// --------------------------------------------------------------------
// audit
// --------------------------------------------------------------------

/// The pre-#1464 `rustango_audit_log`, as an upgraded database holds
/// it. Only `occurred_at`'s default differs from what `ensure_table_pool`
/// writes today.
async fn legacy_audit_table(pool: &Pool) {
    exec(
        pool,
        r#"CREATE TABLE "rustango_audit_log" (
            "id"           INTEGER PRIMARY KEY AUTOINCREMENT,
            "entity_table" TEXT NOT NULL,
            "entity_pk"    TEXT NOT NULL,
            "operation"    TEXT NOT NULL,
            "source"       TEXT NOT NULL,
            "changes"      TEXT NOT NULL,
            "occurred_at"  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"#,
    )
    .await;
}

fn entry(pk: &str) -> rustango::audit::PendingEntry {
    rustango::audit::PendingEntry {
        entity_table: "post",
        entity_pk: pk.to_owned(),
        operation: rustango::audit::AuditOp::Update,
        source: rustango::audit::AuditSource::Custom("guard".into()),
        changes: serde_json::json!({"title": {"after": "x"}}),
    }
}

#[tokio::test]
async fn audit_emit_one_binds_occurred_at() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_audit_table(&pool).await;

    rustango::audit::emit_one_pool(&pool, &entry("1"))
        .await
        .expect("emit");

    let stored = column(&pool, r#"SELECT "occurred_at" FROM "rustango_audit_log""#).await;
    assert_eq!(stored.len(), 1);
    assert_canonical("audit occurred_at", &stored[0]);
}

/// The batch path is a separate statement on every dialect — a
/// per-row loop inside a transaction here, one multi-row `VALUES` on
/// Postgres — so it can regress on its own.
#[tokio::test]
async fn audit_emit_many_binds_occurred_at() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_audit_table(&pool).await;

    let entries: Vec<_> = (1..=3).map(|i| entry(&i.to_string())).collect();
    rustango::audit::emit_many_pool(&pool, &entries)
        .await
        .expect("emit batch");

    let stored = column(&pool, r#"SELECT "occurred_at" FROM "rustango_audit_log""#).await;
    assert_eq!(stored.len(), 3);
    for s in &stored {
        assert_canonical("audit occurred_at (batch)", s);
    }
}

/// The consequence, not the spelling — the upgraded database as it
/// actually is, holding rows of both origins.
///
/// `cleanup_keep_last_n` ranks with
/// `ROW_NUMBER() OVER (ORDER BY occurred_at DESC)`, which on SQLite is
/// a plain text sort with nothing normalising it. So the fixture seeds
/// one **canonical** row with a deliberately old instant — what the
/// migrate sweep leaves behind for every row written before the
/// upgrade — and then emits a new one through the framework. Under the
/// stale default the new row arrives as `' '`-separated text, sorts
/// below the year-old canonical row, and "keep the last 1" keeps the
/// wrong one.
///
/// This is the shape of security-002: retention deleting the newest
/// audit entries and keeping the oldest.
#[tokio::test]
async fn keeping_the_last_entry_keeps_the_newest_one() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_audit_table(&pool).await;

    // A row from before the upgrade, normalised by the sweep. A year
    // back so no clock skew can reorder it against the fresh one.
    // Bound as a `DateTime`, so the executor's own binder encodes it —
    // the canonical spelling comes from the code under test rather than
    // from a literal copied into the fixture.
    // Same `(entity_table, entity_pk)` as the emitted row: retention
    // partitions by that pair, so two different pks would each rank
    // first in their own partition and nothing would be deleted.
    let old = chrono::Utc::now() - chrono::Duration::days(365);
    rustango::sql::raw_execute_pool(
        &pool,
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes", "occurred_at")
           VALUES ('post', '1', 'update', 'swept', '{}', ?)"#,
        vec![SqlValue::DateTime(old)],
    )
    .await
    .expect("seed the swept row");

    rustango::audit::emit_one_pool(&pool, &entry("1"))
        .await
        .expect("emit");

    rustango::audit::cleanup_keep_last_n_pool(&pool, 1)
        .await
        .expect("retention");

    let survivors = column(&pool, r#"SELECT "source" FROM "rustango_audit_log""#).await;
    assert_eq!(
        survivors,
        vec!["guard".to_owned()],
        "`keep_last_n(1)` must keep the entry written a moment ago, not \
         the one from a year back. Keeping `swept` means the fresh row \
         sorted below it — the writer let the stale `CURRENT_TIMESTAMP` \
         default fire, and retention is now deleting newest-first."
    );
}

// --------------------------------------------------------------------
// jobs
// --------------------------------------------------------------------

#[cfg(feature = "jobs-postgres")]
mod jobs {
    use super::{assert_canonical, column, exec};
    use rustango::jobs::pg::PgJobQueue;
    use rustango::jobs::{Job, JobError, JobQueue};
    use rustango::sql::Pool;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Noop;

    #[async_trait::async_trait]
    impl Job for Noop {
        const NAME: &'static str = "guard:noop";
        async fn run(&self) -> Result<(), JobError> {
            Ok(())
        }
    }

    /// `run_at` is the pickup queue's sort key
    /// (`ORDER BY run_at, id`), so a legacy-shaped row does not merely
    /// look wrong — it jumps ahead of every correctly-stamped job, and
    /// keeps doing so as long as the stale default keeps writing them.
    #[tokio::test]
    async fn dispatch_binds_run_at_and_created_at() {
        let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
        exec(
            &pool,
            "CREATE TABLE rustango_jobs (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                name         TEXT     NOT NULL,
                payload      TEXT     NOT NULL,
                attempt      INTEGER  NOT NULL DEFAULT 0,
                max_attempts INTEGER  NOT NULL,
                run_at       TEXT     NOT NULL DEFAULT CURRENT_TIMESTAMP,
                locked_at    TEXT,
                locked_by    TEXT,
                last_error   TEXT,
                created_at   TEXT     NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .await;

        let queue = PgJobQueue::with_workers_pool(pool.clone(), 0);
        queue.dispatch(&Noop).await.expect("dispatch");

        let run_at = column(&pool, "SELECT run_at FROM rustango_jobs").await;
        let created_at = column(&pool, "SELECT created_at FROM rustango_jobs").await;
        assert_eq!(run_at.len(), 1);
        assert_canonical("jobs run_at", &run_at[0]);
        assert_canonical("jobs created_at", &created_at[0]);
    }
}

// --------------------------------------------------------------------
// i18n
// --------------------------------------------------------------------

/// The pre-#1464 `rustango_translations` — and also pre-mapping, since
/// `Translation` did not declare these two columns at all until the
/// same change.
async fn legacy_translations_table(pool: &Pool) {
    exec(
        pool,
        r#"CREATE TABLE "rustango_translations" (
            "id"         INTEGER PRIMARY KEY AUTOINCREMENT,
            "locale"     TEXT NOT NULL,
            "key"        TEXT NOT NULL,
            "value"      TEXT NOT NULL,
            "updated_by" TEXT NOT NULL DEFAULT '',
            "created_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            "updated_at" TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            CONSTRAINT "rustango_translations_locale_key_uq" UNIQUE ("locale", "key")
        )"#,
    )
    .await;
}

#[tokio::test]
async fn translation_upsert_binds_both_timestamps() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_translations_table(&pool).await;

    rustango::i18n::db::upsert_pool(&pool, "fr", "greeting", "Bonjour", "guard")
        .await
        .expect("upsert");

    let created = column(&pool, r#"SELECT "created_at" FROM "rustango_translations""#).await;
    let updated = column(&pool, r#"SELECT "updated_at" FROM "rustango_translations""#).await;
    assert_eq!(created.len(), 1);
    assert_canonical("translation created_at", &created[0]);
    assert_canonical("translation updated_at", &updated[0]);
}

/// The conflict branch writes a different statement from the insert
/// branch — `DO UPDATE SET` rather than `VALUES` — so it needs its own
/// assertion, and `updated_at` has to actually move.
#[tokio::test]
async fn re_upserting_a_translation_advances_updated_at() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_translations_table(&pool).await;

    rustango::i18n::db::upsert_pool(&pool, "fr", "greeting", "Bonjour", "guard")
        .await
        .expect("insert branch");
    let first = column(&pool, r#"SELECT "updated_at" FROM "rustango_translations""#).await;

    rustango::i18n::db::upsert_pool(&pool, "fr", "greeting", "Salut", "guard")
        .await
        .expect("conflict branch");
    let second = column(&pool, r#"SELECT "updated_at" FROM "rustango_translations""#).await;

    assert_eq!(second.len(), 1, "the upsert must not have inserted a row");
    assert_canonical("translation updated_at after edit", &second[0]);
    assert!(
        second[0] >= first[0],
        "`auto_now` must restamp `updated_at` on the conflict branch — \
         it is the whole point of the column. Before: {:?}, after: {:?}",
        first[0],
        second[0]
    );
}

// --------------------------------------------------------------------
// the migration ledger
// --------------------------------------------------------------------

/// `applied_at` is nothing's sort key today, so this guards the thing
/// that would make it one: `migrate`'s own SQLite datetime sweep
/// normalises this column on every run, and a defaulted write puts the
/// legacy spelling straight back — the sweep and the runner undoing
/// each other, once per migration.
#[tokio::test]
async fn recording_a_migration_binds_applied_at() {
    use rustango::migrate::{self, file, Migration, Operation, SchemaSnapshot};

    const LEDGER: &str = "guard_ledger";

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let url = format!("sqlite://{}?mode=rwc", tmp.path().display());
    let pool = Pool::connect(&url).await.expect("sqlite file pool");

    // The pre-#1464 ledger. `ensure_ledger_pool` would build the
    // canonical one, which is why this does not call it.
    exec(
        &pool,
        &format!(
            "CREATE TABLE {LEDGER} (\
             name VARCHAR(255) PRIMARY KEY, \
             applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)"
        ),
    )
    .await;

    let mig = Migration {
        name: "0001_guard".to_owned(),
        created_at: "2026-09-20T00:00:00Z".into(),
        prev: None,
        atomic: true,
        scope: migrate::MigrationScope::default(),
        replaces: Vec::new(),
        snapshot: SchemaSnapshot::default(),
        forward: vec![Operation::Data(file::DataOp {
            sql: "SELECT 1".to_owned(),
            reverse_sql: None,
            reversible: false,
        })],
    };
    let dir = tempfile::tempdir().expect("tempdir");
    file::write(&dir.path().join("0001_guard.json"), &mig).expect("write migration");

    migrate::migrate_pool_with_ledger(&pool, dir.path(), LEDGER)
        .await
        .expect("migrate");

    let stored = column(&pool, &format!("SELECT applied_at FROM {LEDGER}")).await;
    assert_eq!(stored.len(), 1, "the migration should be recorded once");
    assert_canonical("ledger applied_at", &stored[0]);
}

// --------------------------------------------------------------------
// the fixtures themselves
// --------------------------------------------------------------------

/// Every test above is an assertion that the default did **not** fire.
/// That is a negative, so it passes on a fixture whose default was
/// never the legacy one — a typo in the DDL, or a SQLite that stopped
/// honouring `CURRENT_TIMESTAMP`. This proves the fixtures still carry
/// the defect they are meant to expose.
#[tokio::test]
async fn the_legacy_default_still_writes_the_legacy_shape() {
    let pool = Pool::connect("sqlite::memory:").await.expect("sqlite");
    legacy_audit_table(&pool).await;

    // Straight past the framework, the way a hand-written INSERT does.
    exec(
        &pool,
        r#"INSERT INTO "rustango_audit_log"
              ("entity_table", "entity_pk", "operation", "source", "changes")
           VALUES ('post', '1', 'update', 'custom:hand', '{}')"#,
    )
    .await;

    let stored = column(&pool, r#"SELECT "occurred_at" FROM "rustango_audit_log""#).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].as_bytes()[10],
        b' ',
        "the fixture's DEFAULT must still produce the legacy \
         `YYYY-MM-DD HH:MM:SS` shape, or the tests above assert nothing. \
         Stored: {:?}",
        stored[0]
    );

    // And it is genuinely unorderable against a canonical value — the
    // property the whole change exists to remove.
    //
    // The cutoff is **midnight of the row's own day**, not some
    // interval away from now. A cutoff a day or a year apart differs
    // from the row inside the date, so the comparison is decided at
    // index 3 or 8 and never reaches the separator — it would pass on
    // a correctly-stored row too, and prove nothing. Sharing the date
    // forces index 10 to decide it, which is the defect: `' '` (0x20)
    // sorts below `'T'` (0x54), so a row written at 17:50 reads as
    // older than midnight of the same morning.
    let midnight = format!("{}T00:00:00.000000+00:00", &stored[0][..10]);
    let hits: Vec<(i64,)> = rustango::sql::raw_query_pool(
        r#"SELECT COUNT(*) FROM "rustango_audit_log" WHERE "occurred_at" < ?"#,
        vec![SqlValue::String(midnight.clone())],
        &pool,
    )
    .await
    .expect("compare");
    assert_eq!(
        hits[0].0, 1,
        "a legacy-shaped row must compare as older than midnight of its \
         own day ({midnight}) — that is the defect, and if it no longer \
         reproduces here then the guards above prove nothing. \
         Stored: {:?}",
        stored[0]
    );
}
