//! Audited `insert_pool`, `soft_delete`, `restore` and `delete_pool` on
//! `&Pool` each write one audit row, keyed by the row's real PK (#1675).

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use chrono::{DateTime, Utc};
use rustango::audit::{self, AuditEntry, AuditLog};
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

/// Audited, with a PK the caller sets: `insert_tx` takes `&self`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "audit1460_tag", app = "audit1675", audit(track = "title"))]
#[allow(dead_code)]
pub struct Tag {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 64)]
    pub title: String,
}

const TABLE: &str = "audit1675_note";

/// Fresh table, and no audit rows left for it by an earlier run.
async fn setup(pool: &Pool) {
    rustango::testkit::matrix::fresh_table::<Note>(pool).await;
    rustango::testkit::matrix::fresh_table::<Stamp>(pool).await;
    rustango::testkit::matrix::fresh_table::<Tag>(pool).await;
    audit::ensure_table_pool(pool).await.expect("audit table");
    for table in [TABLE, "audit1460_tag"] {
        AuditLog::delete_where("entity_table", table, pool)
            .await
            .expect("clear audit rows");
    }
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

/// The macro `save(&PgPool)` runs outside a transaction, so its
/// pre-read guard is the only thing between a failure and an unaudited UPDATE.
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
    pub fn pending_seen() -> &'static Mutex<HashMap<String, usize>> {
        static S: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
        S.get_or_init(Default::default)
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct ProbeAtomic {
        pub key: String,
    }

    #[async_trait::async_trait]
    impl Job for ProbeAtomic {
        const NAME: &'static str = "audit1229_probe_atomic";
        async fn run(&self) -> Result<(), JobError> {
            let pending = rustango::sql::on_commit_pending();
            pending_seen()
                .lock()
                .unwrap()
                .insert(self.key.clone(), pending);
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
/// dispatched inside `atomic` sees none of its on-commit queue.
async fn job_dispatched_in_atomic_does_not_join_it(pool: &Pool) {
    #[cfg(not(feature = "jobs"))]
    let _ = pool;
    #[cfg(feature = "jobs")]
    {
        use ctx_jobs::{pending_seen, ProbeAtomic};
        use rustango::jobs::{InMemoryJobQueue, JobQueue as _};

        let key = format!("{:p}", pool);
        pending_seen().lock().unwrap().remove(&key);
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
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !pending_seen().lock().unwrap().contains_key(&k)
                    && std::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Ok(())
            })
        })
        .await
        .expect("atomic");
        q.shutdown().await;
        assert_eq!(pending_seen().lock().unwrap().get(&key), Some(&0));
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

/// The derived `_tx` methods write the same audit rows as the pool ones (#1460).
async fn tx_methods_write_audit_rows(pool: &Pool) {
    let pk = rustango::atomic!(pool, |tx| {
        let mut note = Note {
            id: Auto::default(),
            title: "hello".into(),
            deleted_at: None,
        };
        note.insert_tx(&mut *tx.lock().await?).await?;
        note.title = "changed".into();
        assert_eq!(note.save_tx(&mut *tx.lock().await?).await?, 1);
        assert_eq!(note.delete_tx(&mut *tx.lock().await?).await?, 1);
        let tag = Tag {
            id: 7,
            title: "t".into(),
        };
        tag.insert_tx(&mut *tx.lock().await?).await?;
        Ok(note.id.get().expect("pk assigned").to_string())
    })
    .await
    .expect("atomic");
    let rows = entries(pool, &pk).await;
    let mut ops: Vec<&str> = rows.iter().map(|r| r.operation.as_str()).collect();
    ops.sort_unstable();
    assert_eq!(ops, ["create", "delete", "update"]);
    let update = rows.iter().find(|r| r.operation == "update").unwrap();
    assert_eq!(update.changes["title"]["before"], "hello");
    assert_eq!(update.changes["title"]["after"], "changed");
    let tags = audit::fetch_for_entity_pool(pool, "audit1460_tag", "7")
        .await
        .expect("fetch audit rows");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].operation, "create");
}

/// The `_tx` audit rows roll back with the block that wrote them.
async fn tx_audit_rows_roll_back_with_the_block(pool: &Pool) {
    let res: Result<(), rustango::sql::ExecError> = rustango::atomic!(pool, |tx| {
        let mut note = Note {
            id: Auto::default(),
            title: "hello".into(),
            deleted_at: None,
        };
        note.insert_tx(&mut *tx.lock().await?).await?;
        Err(rustango::sql::ExecError::Sql(
            rustango::sql::SqlError::EmptyInList,
        ))
    })
    .await;
    assert!(res.is_err());
    assert_eq!(audit_rows(pool).await, 0);
    assert_eq!(Note::objects().count(pool).await.unwrap(), 0);
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        tx_methods_write_audit_rows,
        tx_audit_rows_roll_back_with_the_block,
        insert_records_the_assigned_pk,
        soft_delete_and_restore_each_write_one_row,
        delete_writes_one_row,
        no_row_changed_writes_no_audit_row,
        noop_save_writes_no_audit_row,
        save_partial_audits_only_its_fields,
        failed_pre_read_fails_the_save,
        second_soft_delete_keeps_the_first_stamp,
        pg_macro_save_skips_noop_and_fails_on_pre_read,
        job_audits_as_its_enqueuer,
        job_dispatched_in_atomic_does_not_join_it,
        scheduled_task_audits_as_its_registration,
    ],
}
