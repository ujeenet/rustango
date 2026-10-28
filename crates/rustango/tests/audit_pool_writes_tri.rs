//! Audited `insert_pool`, `soft_delete`, `restore` and `delete_pool` on
//! `&Pool` each write one audit row, keyed by the row's real PK (#1675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{DateTime, Utc};
use rustango::audit::{self, AuditEntry, AuditLog};
use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, CounterPool as _, FetcherPool as _, Pool};
use rustango::{tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1675_note",
    app = "audit1675",
    audit(track = "title, deleted_at")
)]
#[allow(dead_code)]
pub struct Note {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Tracks a column the UPDATE never writes, so dropping it fails only
/// the pre-read (#1907).
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit1907_stamp",
    app = "audit1675",
    audit(track = "title, created_at")
)]
#[allow(dead_code)]
pub struct Stamp {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 64)]
    pub title: String,
    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}

/// An audited parent whose delete cascades to an audited child, so the
/// delete reads both before it writes.
#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit_cascade_shelf",
    app = "audit1675",
    audit(track = "label")
)]
#[allow(dead_code)]
pub struct Shelf {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 32)]
    pub label: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "audit_cascade_book",
    app = "audit1675",
    audit(track = "title")
)]
#[allow(dead_code)]
pub struct Book {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(on_delete = "cascade")]
    pub shelf: rustango::sql::ForeignKey<Shelf>,
    #[rustango(max_length = 32)]
    pub title: String,
}

const TABLE: &str = "audit1675_note";

/// Fresh table, and no audit rows left for it by an earlier run.
async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::testkit::matrix::fresh_table::<Stamp>(pool).await;
    rustango::testkit::matrix::drop_table(pool, Book::SCHEMA.table).await;
    rustango::testkit::matrix::fresh_table::<Shelf>(pool).await;
    rustango::testkit::matrix::fresh_table::<Book>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    AuditLog::delete_where("entity_table", TABLE, pool)
        .await
        .expect("clear audit rows");
}

async fn audit_rows(pool: &Pool) -> i64 {
    AuditLog::objects()
        .filter("entity_table", TABLE)
        .count(pool)
        .await
        .expect("count audit rows")
}

async fn entries(pool: &Pool, pk: &str) -> Vec<AuditEntry> {
    audit::fetch_for_entity_pool(pool, TABLE, pk)
        .await
        .expect("fetch audit rows")
}

async fn insert_note(pool: &Pool) -> Note {
    let mut note = Note {
        id: Auto::default(),
        title: "hello".into(),
        deleted_at: None,
    };
    note.insert_pool(pool).await.expect("insert");
    note
}

async fn insert_records_the_assigned_pk(pool: &Pool) {
    let note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();
    assert_eq!(audit_rows(pool).await, 1);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows.len(), 1, "the audit row must carry PK {pk}");
    assert_eq!(rows[0].operation, "create");
    assert_eq!(rows[0].changes["title"], "hello");
}

async fn soft_delete_and_restore_each_write_one_row(pool: &Pool) {
    let mut note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();

    assert_eq!(note.soft_delete(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "soft_delete");
    assert!(
        !rows[0].changes["deleted_at"].is_null(),
        "records the written time"
    );

    // Stale in-memory value: the audit row must still record NULL.
    note.deleted_at = Some(Utc::now());
    assert_eq!(note.restore(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 3);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "restore");
    assert!(rows[0].changes["deleted_at"].is_null());
}

async fn delete_writes_one_row(pool: &Pool) {
    let note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();
    assert_eq!(note.delete_pool(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    let rows = entries(pool, &pk).await;
    assert_eq!(rows[0].operation, "delete");
    assert_eq!(rows[0].changes["title"], "hello");
}

async fn no_row_changed_writes_no_audit_row(pool: &Pool) {
    let note = insert_note(pool).await;
    assert_eq!(note.delete_pool(pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    assert_eq!(note.delete_pool(pool).await.unwrap(), 0);
    assert_eq!(note.soft_delete(pool).await.unwrap(), 0);
    assert_eq!(note.restore(pool).await.unwrap(), 0);
    assert_eq!(audit_rows(pool).await, 2);
}

async fn noop_save_writes_no_audit_row(pool: &Pool) {
    let mut note = insert_note(pool).await;
    note.save_pool(pool).await.expect("no-op save");
    assert_eq!(
        audit_rows(pool).await,
        1,
        "a no-op save wrote an update row"
    );
    note.title = "changed".into();
    note.save_pool(pool).await.expect("save");
    assert_eq!(audit_rows(pool).await, 2);
}

/// An audited `save_partial` diffs only the fields it writes (#1744).
async fn save_partial_audits_only_its_fields(pool: &Pool) {
    let mut note = insert_note(pool).await;
    let pk = note.id.get().expect("pk assigned").to_string();
    note.title = "partial".into();
    note.deleted_at = Some(Utc::now());
    assert_eq!(note.save_partial(&["title"], pool).await.unwrap(), 1);
    assert_eq!(audit_rows(pool).await, 2);
    let changes = &entries(pool, &pk).await[0].changes;
    assert_eq!(changes["title"]["before"], "hello");
    assert_eq!(changes["title"]["after"], "partial");
    assert!(changes.get("deleted_at").is_none(), "{changes}");
}

async fn failed_pre_read_fails_the_save(pool: &Pool) {
    // SQLite reads a missing double-quoted column as a string literal,
    // so its pre-read cannot be made to fail this way.
    if pool.dialect().name() == "sqlite" {
        return;
    }
    let mut stamp = Stamp {
        id: Auto::default(),
        title: "old".into(),
        created_at: Auto::default(),
    };
    stamp.insert_pool(pool).await.expect("insert");
    let d = pool.dialect();
    let sql = format!(
        "ALTER TABLE {} DROP COLUMN {}",
        d.quote_ident("audit1907_stamp"),
        d.quote_ident("created_at")
    );
    rustango::sql::raw_execute_pool(pool, &sql, Vec::new())
        .await
        .expect("drop column");
    stamp.title = "new".into();
    assert!(stamp.save_pool(pool).await.is_err(), "save ran unaudited");
    let updated = Stamp::objects()
        .filter("title", "new")
        .count(pool)
        .await
        .expect("count");
    assert_eq!(updated, 0, "the UPDATE committed without its audit row");
}

async fn second_soft_delete_keeps_the_first_stamp(pool: &Pool) {
    let note = insert_note(pool).await;
    assert_eq!(note.soft_delete(pool).await.unwrap(), 1);
    let first = Note::objects().fetch(pool).await.unwrap()[0].deleted_at;
    assert_eq!(note.soft_delete(pool).await.unwrap(), 0, "re-stamped");
    assert_eq!(
        Note::objects().fetch(pool).await.unwrap()[0].deleted_at,
        first
    );
    assert_eq!(
        audit_rows(pool).await,
        2,
        "a second soft delete wrote an audit row"
    );
}

/// Waits until `n` before-image reads of `audit1675_note` wait on a row lock.
/// SQLite has no lock view: it waits until the savers hold a connection.
async fn wait_until_blocked(pool: &Pool, n: i64) {
    wait_until_blocked_on(pool, TABLE, n).await;
}

/// [`wait_until_blocked`] for the locked reads of `table`.
async fn wait_until_blocked_on(pool: &Pool, table: &str, n: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let blocked: i64 = match pool.dialect().name() {
            #[cfg(feature = "postgres")]
            "postgres" => rustango::sql::sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
                 AND query LIKE $1 AND query LIKE '%FOR UPDATE%'",
            )
            .bind(format!("%{table}%"))
            .fetch_one(pool.as_postgres().unwrap())
            .await
            .unwrap(),
            #[cfg(feature = "mysql")]
            "mysql" => rustango::sql::sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.PROCESSLIST \
                 WHERE ID <> CONNECTION_ID() AND INFO LIKE ?",
            )
            .bind(format!("%{table}%FOR UPDATE%"))
            .fetch_one(pool.as_mysql().unwrap())
            .await
            .unwrap(),
            #[cfg(feature = "sqlite")]
            "sqlite" => {
                let p = pool.as_sqlite().unwrap();
                // The writer holds one connection.
                i64::from(p.size()) - p.num_idle() as i64 - 1
            }
            other => panic!("no lock probe for {other}"),
        };
        if blocked >= n {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {blocked} of {n} saves reached the locked row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if pool.dialect().name() == "sqlite" {
        // Covers the step from acquire to `BEGIN IMMEDIATE`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// A save behind an open writer waits for it, then diffs against its row.
/// SQLite on a file: the in-memory pool shares one cache, not WAL.
async fn save_waits_for_a_concurrent_writer(pool: &Pool) {
    #[cfg(feature = "sqlite")]
    let file;
    #[cfg(feature = "sqlite")]
    let pool = if pool.dialect().name() == "sqlite" {
        file = rustango::testkit::matrix::sqlite_file_pool().await;
        setup(&file).await;
        &file
    } else {
        pool
    };
    let mut note = insert_note(pool).await;
    let id = *note.id.get().expect("pk assigned");
    let update = Note::objects()
        .where_(Note::id.eq(id))
        .update()
        .set("title", "held")
        .compile()
        .unwrap();
    let mut writer = rustango::sql::transaction_pool(pool).await.unwrap();
    rustango::sql::update_tx(&mut writer, &update)
        .await
        .unwrap();
    let saver = {
        let pool = pool.clone();
        tokio::spawn(async move {
            note.title = "saved".into();
            note.save_pool(&pool).await
        })
    };
    wait_until_blocked(pool, 1).await;
    assert!(!saver.is_finished(), "the save did not wait for the writer");
    writer.commit().await.unwrap();
    saver.await.unwrap().expect("save once the writer commits");
    let changes = &entries(pool, &id.to_string()).await[0].changes;
    assert_eq!(changes["title"]["before"], "held", "{changes}");
}

/// Two macro `save(&PgPool)` calls queued behind one writer: each must hold
/// its row lock through its UPDATE, so the second diffs against the first.
async fn pg_macro_saves_queue_behind_each_other(pool: &Pool) {
    #[cfg(not(feature = "postgres"))]
    let _ = pool;
    #[cfg(feature = "postgres")]
    {
        let Some(pg) = pool.as_postgres() else {
            return;
        };
        let note = insert_note(pool).await;
        let id = *note.id.get().expect("pk assigned");
        let update = Note::objects()
            .where_(Note::id.eq(id))
            .update()
            .set("title", "held")
            .compile()
            .unwrap();
        let mut writer = rustango::sql::transaction_pool(pool).await.unwrap();
        rustango::sql::update_tx(&mut writer, &update)
            .await
            .unwrap();
        let savers: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|title| {
                let (pg, mut note) = (pg.clone(), note.clone());
                tokio::spawn(async move {
                    note.title = title.into();
                    note.save(&pg).await
                })
            })
            .collect();
        wait_until_blocked(pool, 2).await;
        writer.commit().await.unwrap();
        for s in savers {
            s.await.unwrap().expect("save once the writer commits");
        }
        let rows = entries(pool, &id.to_string()).await;
        let befores: Vec<&str> = rows
            .iter()
            .filter(|e| e.operation == "update")
            .map(|e| e.changes["title"]["before"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(befores.len(), 2, "{rows:?}");
        assert_ne!(befores[0], befores[1], "both saves diffed one stale row");
    }
}

/// The macro `save(&PgPool)` pre-read guard is the only thing between a
/// failure and an unaudited UPDATE.
async fn pg_macro_save_skips_noop_and_fails_on_pre_read(pool: &Pool) {
    #[cfg(not(feature = "postgres"))]
    let _ = pool;
    #[cfg(feature = "postgres")]
    pg_macro_save(pool).await;
}

#[cfg(feature = "postgres")]
async fn pg_macro_save(pool: &Pool) {
    let Some(pg) = pool.as_postgres() else {
        return;
    };
    let mut note = insert_note(pool).await;
    note.save(pg).await.expect("no-op save");
    assert_eq!(
        audit_rows(pool).await,
        1,
        "a no-op save wrote an update row"
    );
    note.title = "changed".into();
    note.save(pg).await.expect("save");
    assert_eq!(audit_rows(pool).await, 2);

    let mut stamp = Stamp {
        id: Auto::default(),
        title: "old".into(),
        created_at: Auto::default(),
    };
    stamp.insert_pool(pool).await.expect("insert");
    rustango::sql::raw_execute_pool(
        pool,
        r#"ALTER TABLE "audit1907_stamp" DROP COLUMN "created_at""#,
        Vec::new(),
    )
    .await
    .expect("drop column");
    stamp.title = "new".into();
    assert!(stamp.save(pg).await.is_err(), "save ran unaudited");
    let updated = Stamp::objects()
        .filter("title", "new")
        .count(pool)
        .await
        .expect("count");
    assert_eq!(updated, 0, "the UPDATE ran without its audit row");
}

/// A macro `save(&PgPool)` or `insert(&PgPool)` whose COMMIT fails leaves the
/// pk unset, so a retry inserts instead of updating a row that was rolled back.
async fn pg_macro_save_retries_after_failed_commit(pool: &Pool) {
    #[cfg(not(feature = "postgres"))]
    let _ = pool;
    #[cfg(feature = "postgres")]
    {
        let Some(pg) = pool.as_postgres() else {
            return;
        };
        rustango::sql::raw_execute_pool(
            pool,
            r#"ALTER TABLE "audit1675_note" ADD CONSTRAINT "audit_note_title_deferred"
               UNIQUE ("title") DEFERRABLE INITIALLY DEFERRED"#,
            Vec::new(),
        )
        .await
        .expect("deferred unique");
        insert_note(pool).await;
        let mut note = Note {
            id: Auto::default(),
            title: "hello".into(),
            deleted_at: None,
        };
        assert!(note.save(pg).await.is_err(), "the COMMIT must fail");
        assert!(
            note.id.get().is_none(),
            "pk kept after a rolled-back insert"
        );
        assert!(note.insert(pg).await.is_err(), "the COMMIT must fail");
        assert!(
            note.id.get().is_none(),
            "insert kept the pk of a rolled-back row"
        );
        note.title = "retried".into();
        assert_eq!(note.save(pg).await.expect("retry"), 1);
        let written = Note::objects()
            .filter("title", "retried")
            .count(pool)
            .await
            .expect("count");
        assert_eq!(written, 1, "the retry wrote no row");
    }
}

/// The macro `insert(&PgPool)` / `delete(&PgPool)` roll the write back when
/// its audit row fails.
async fn pg_macro_insert_and_delete_are_atomic(pool: &Pool) {
    #[cfg(not(feature = "postgres"))]
    let _ = pool;
    #[cfg(feature = "postgres")]
    pg_macro_atomic(pool).await;
}

#[cfg(feature = "postgres")]
async fn pg_macro_atomic(pool: &Pool) {
    let Some(pg) = pool.as_postgres() else {
        return;
    };
    let kept = insert_note(pool).await;
    // Refuse audit rows for this table only; other suites share the log.
    let ddl = |sql: &'static str| rustango::sql::raw_execute_pool(pool, sql, Vec::new());
    ddl(r#"ALTER TABLE "rustango_audit_log" DROP CONSTRAINT IF EXISTS "audit_atomic_refuse""#)
        .await
        .unwrap();
    ddl(r#"ALTER TABLE "rustango_audit_log" ADD CONSTRAINT "audit_atomic_refuse" CHECK (entity_table <> 'audit1675_note') NOT VALID"#)
        .await
        .unwrap();
    let mut note = Note {
        id: Auto::default(),
        title: "unaudited".into(),
        deleted_at: None,
    };
    let inserted = note.insert(pg).await;
    let deleted = kept.delete(pg).await;
    ddl(r#"ALTER TABLE "rustango_audit_log" DROP CONSTRAINT "audit_atomic_refuse""#)
        .await
        .unwrap();
    assert!(inserted.is_err(), "insert ran unaudited");
    assert!(deleted.is_err(), "delete ran unaudited");
    let titles: Vec<String> = Note::objects()
        .fetch(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.title)
        .collect();
    assert_eq!(titles, ["hello"], "a write committed without its audit row");
}

/// Polls for the note titled `title`; returns its newest audit source.
#[cfg(any(feature = "jobs", feature = "scheduler"))]
async fn source_of_title(pool: &Pool, title: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let notes = Note::objects()
            .filter("title", title)
            .fetch(pool)
            .await
            .expect("fetch");
        if let Some(note) = notes.first() {
            let pk = note.id.get().expect("pk").to_string();
            if let Some(e) = entries(pool, &pk).await.first() {
                return Some(e.source.clone());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    None
}

#[cfg(feature = "jobs")]
mod ctx_jobs {
    use super::*;
    use rustango::jobs::{Job, JobError};
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// Pools by key: a job payload cannot carry one, and backends share
    /// the process under `cargo test`.
    pub fn pools() -> &'static Mutex<HashMap<String, Pool>> {
        static P: OnceLock<Mutex<HashMap<String, Pool>>> = OnceLock::new();
        P.get_or_init(Default::default)
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct WriteNote {
        pub key: String,
        pub title: String,
    }

    #[async_trait::async_trait]
    impl Job for WriteNote {
        const NAME: &'static str = "audit1229_write_note";
        async fn run(&self) -> Result<(), JobError> {
            let pool = pools().lock().unwrap()[&self.key].clone();
            let mut note = Note {
                id: Auto::default(),
                title: self.title.clone(),
                deleted_at: None,
            };
            note.insert_pool(&pool)
                .await
                .map_err(|e| JobError::Fatal(e.to_string()))
        }
    }

    /// What each probe saw, by its key: the backends share the process
    /// under `cargo test`.
    pub fn probe_seen() -> &'static Mutex<HashMap<String, String>> {
        static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        S.get_or_init(Default::default)
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct ProbeAtomic {
        pub key: String,
    }

    /// Opens its own `atomic` block: one carried over from the enqueuer
    /// (already committed) would make it a nested block that cannot run.
    #[async_trait::async_trait]
    impl Job for ProbeAtomic {
        const NAME: &'static str = "audit1229_probe_atomic";
        async fn run(&self) -> Result<(), JobError> {
            let seen = |v: &str| {
                probe_seen()
                    .lock()
                    .unwrap()
                    .insert(self.key.clone(), v.to_owned());
            };
            seen("started");
            let pool = pools().lock().unwrap()[&self.key].clone();
            let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = fired.clone();
            let res = rustango::sql::atomic(&pool, move |_tx| {
                Box::pin(async move {
                    let outer = rustango::sql::on_commit_pending();
                    rustango::sql::on_commit(move || {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    });
                    assert_eq!(rustango::sql::on_commit_pending(), outer + 1);
                    Ok(outer)
                })
            })
            .await;
            let fired = fired.load(std::sync::atomic::Ordering::SeqCst);
            seen(&format!("{res:?} fired={fired}"));
            Ok(())
        }
    }
}

/// #1229 — a job's audit row names its enqueuer; one enqueued outside a
/// scope stays `system`.
async fn job_audits_as_its_enqueuer(pool: &Pool) {
    #[cfg(not(feature = "jobs"))]
    let _ = pool;
    #[cfg(feature = "jobs")]
    {
        use ctx_jobs::{pools, WriteNote};
        use rustango::audit::{with_source, AuditSource};
        use rustango::jobs::{InMemoryJobQueue, JobQueue as _};

        let key = format!("{:p}", pool);
        pools().lock().unwrap().insert(key.clone(), pool.clone());
        let q = InMemoryJobQueue::with_workers(1);
        q.register::<WriteNote>().await;
        q.start().await;
        with_source(AuditSource::User { id: "42".into() }, async {
            let job = WriteNote {
                key: key.clone(),
                title: "by-user".into(),
            };
            q.dispatch(&job).await.unwrap();
        })
        .await;
        let job = WriteNote {
            key,
            title: "by-nobody".into(),
        };
        q.dispatch(&job).await.unwrap();

        let by_user = source_of_title(pool, "by-user").await;
        let by_nobody = source_of_title(pool, "by-nobody").await;
        q.shutdown().await;
        assert_eq!(by_user.as_deref(), Some("user:42"));
        assert_eq!(by_nobody.as_deref(), Some("system"));
    }
}

/// #1229 — the context crosses, the transaction does not: a job
/// dispatched inside `atomic` opens and commits a block of its own.
async fn job_dispatched_in_atomic_does_not_join_it(pool: &Pool) {
    #[cfg(not(feature = "jobs"))]
    let _ = pool;
    #[cfg(feature = "jobs")]
    {
        use ctx_jobs::{pools, probe_seen, ProbeAtomic};
        use rustango::jobs::{InMemoryJobQueue, JobQueue as _};

        let key = format!("{:p}", pool);
        pools().lock().unwrap().insert(key.clone(), pool.clone());
        probe_seen().lock().unwrap().remove(&key);
        let q = std::sync::Arc::new(InMemoryJobQueue::with_workers(1));
        q.register::<ProbeAtomic>().await;
        q.start().await;
        let (inner, k) = (q.clone(), key.clone());
        rustango::sql::atomic(pool, move |_tx| {
            Box::pin(async move {
                rustango::sql::on_commit(|| {});
                assert_eq!(rustango::sql::on_commit_pending(), 1);
                inner
                    .dispatch(&ProbeAtomic { key: k.clone() })
                    .await
                    .unwrap();
                Ok(())
            })
        })
        .await
        .expect("atomic");
        // The job runs after the commit, still outside the block.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let done = || {
            probe_seen()
                .lock()
                .unwrap()
                .get(&key)
                .is_some_and(|v| v != "started")
        };
        while !done() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        q.shutdown().await;
        assert_eq!(
            probe_seen().lock().unwrap().get(&key).map(String::as_str),
            Some("Ok(0) fired=true"),
            "the job must run in its own block, outside the enqueuer's"
        );
    }
}

/// #1229 — a scheduled tick audits as the scope `every()` ran in.
async fn scheduled_task_audits_as_its_registration(pool: &Pool) {
    #[cfg(not(feature = "scheduler"))]
    let _ = pool;
    #[cfg(feature = "scheduler")]
    {
        use rustango::audit::{with_source, AuditSource};
        use rustango::scheduler::Scheduler;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let s = Scheduler::new();
        let (p, done) = (pool.clone(), Arc::new(AtomicBool::new(false)));
        let d = done.clone();
        with_source(AuditSource::Custom("cron:sweep".into()), async {
            s.every(
                "audit1229",
                std::time::Duration::from_millis(20),
                move || {
                    let (p, d) = (p.clone(), d.clone());
                    async move {
                        if d.swap(true, Ordering::SeqCst) {
                            return;
                        }
                        let mut note = Note {
                            id: Auto::default(),
                            title: "by-cron".into(),
                            deleted_at: None,
                        };
                        note.insert_pool(&p).await.expect("insert");
                    }
                },
            );
        })
        .await;
        let handle = s.start();
        let source = source_of_title(pool, "by-cron").await;
        handle.shutdown().await;
        assert_eq!(source.as_deref(), Some("cron:sweep"));
    }
}

/// An audited delete that cascades reads before it writes. Behind an open
/// writer it waits, and on SQLite takes the lock at BEGIN: a deferred read
/// snapshot fails its write at once ("database is locked").
async fn cascading_delete_waits_for_a_concurrent_writer(pool: &Pool) {
    #[cfg(feature = "sqlite")]
    let file;
    #[cfg(feature = "sqlite")]
    let pool = if pool.dialect().name() == "sqlite" {
        file = rustango::testkit::matrix::sqlite_file_pool().await;
        setup(&file).await;
        &file
    } else {
        pool
    };
    let mut shelf = Shelf {
        id: 1,
        label: "a".into(),
    };
    shelf.insert_pool(pool).await.expect("shelf");
    Book {
        id: 1,
        shelf: rustango::sql::ForeignKey::unloaded(1),
        title: "b".into(),
    }
    .insert_pool(pool)
    .await
    .expect("book");
    let update = Shelf::objects()
        .where_(Shelf::id.eq(1))
        .update()
        .set("label", "held")
        .compile()
        .unwrap();
    let mut writer = rustango::sql::transaction_pool(pool).await.unwrap();
    rustango::sql::update_tx(&mut writer, &update)
        .await
        .unwrap();
    let deleter = {
        let pool = pool.clone();
        tokio::spawn(async move { shelf.delete_pool(&pool).await })
    };
    wait_until_blocked_on(pool, Shelf::SCHEMA.table, 1).await;
    writer.commit().await.unwrap();
    let gone = deleter
        .await
        .unwrap()
        .expect("delete once the writer commits");
    assert_eq!(gone, 1);
    assert_eq!(Book::objects().count(pool).await.unwrap(), 0, "cascaded");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        insert_records_the_assigned_pk,
        soft_delete_and_restore_each_write_one_row,
        delete_writes_one_row,
        no_row_changed_writes_no_audit_row,
        noop_save_writes_no_audit_row,
        save_partial_audits_only_its_fields,
        failed_pre_read_fails_the_save,
        second_soft_delete_keeps_the_first_stamp,
        save_waits_for_a_concurrent_writer,
        cascading_delete_waits_for_a_concurrent_writer,
        pg_macro_saves_queue_behind_each_other,
        pg_macro_save_skips_noop_and_fails_on_pre_read,
        pg_macro_save_retries_after_failed_commit,
        pg_macro_insert_and_delete_are_atomic,
        job_audits_as_its_enqueuer,
        job_dispatched_in_atomic_does_not_join_it,
        scheduled_task_audits_as_its_registration,
    ],
}
