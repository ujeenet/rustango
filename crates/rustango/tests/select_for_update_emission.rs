//! Tri-dialect SQL-emission tests for `QuerySet::select_for_update`
//! (issue #21). Django's `SELECT … FOR UPDATE [NO KEY] [OF …]
//! [SKIP LOCKED | NOWAIT]` shapes. PG supports the full set; MySQL
//! 8.0.1+ supports most (no `NO KEY`); SQLite has no row-lock
//! syntax and emits no clause at all.

#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};
use rustango::Model;

#[derive(Model)]
#[rustango(table = "sfu_job")]
#[allow(dead_code)]
pub struct Job {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(max_length = 20)]
    status: String,
    priority: i32,
}

// ---------- PG: full lock-clause matrix ----------

#[test]
fn select_for_update_plain_emits_for_update_on_pg() {
    let q = Job::objects().select_for_update().compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(stmt.sql.ends_with(" FOR UPDATE"), "PG plain: {}", stmt.sql);
}

#[test]
fn skip_locked_emits_for_update_skip_locked_on_pg() {
    let q = Job::objects()
        .select_for_update()
        .skip_locked()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE SKIP LOCKED"),
        "PG skip_locked: {}",
        stmt.sql
    );
}

#[test]
fn nowait_emits_for_update_nowait_on_pg() {
    let q = Job::objects()
        .select_for_update()
        .nowait()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE NOWAIT"),
        "PG nowait: {}",
        stmt.sql
    );
}

#[test]
fn no_key_emits_for_no_key_update_on_pg() {
    let q = Job::objects()
        .select_for_update()
        .no_key()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR NO KEY UPDATE"),
        "PG no_key: {}",
        stmt.sql
    );
}

#[test]
fn of_emits_for_update_of_table_list_on_pg() {
    let q = Job::objects()
        .select_for_update()
        .of(&["sfu_job"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(r#" FOR UPDATE OF "sfu_job""#),
        "PG of: {}",
        stmt.sql
    );
}

#[test]
fn full_lock_clause_combines_no_key_of_and_skip_locked() {
    let q = Job::objects()
        .select_for_update()
        .no_key()
        .of(&["sfu_job"])
        .skip_locked()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .ends_with(r#" FOR NO KEY UPDATE OF "sfu_job" SKIP LOCKED"#),
        "PG full combo: {}",
        stmt.sql
    );
}

/// `skip_locked` and `nowait` cannot both appear in the SQL — the
/// database rejects the combination. The writer picks `SKIP LOCKED`
/// (more permissive) when both flags are set.
#[test]
fn skip_locked_wins_over_nowait_when_both_set() {
    let q = Job::objects()
        .select_for_update()
        .skip_locked()
        .nowait()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.contains(" SKIP LOCKED"),
        "should pick SKIP LOCKED: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains(" NOWAIT"),
        "should NOT also emit NOWAIT: {}",
        stmt.sql
    );
}

// ---------- Default queryset (no lock) emits no clause ----------

#[test]
fn without_select_for_update_no_lock_clause_emitted() {
    let q = Job::objects().compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        !stmt.sql.contains(" FOR UPDATE"),
        "no lock when select_for_update not called: {}",
        stmt.sql
    );
}

// ---------- LIMIT/OFFSET come BEFORE the lock clause ----------

#[test]
fn lock_clause_emits_after_limit_offset() {
    let q = Job::objects()
        .limit(10)
        .offset(5)
        .select_for_update()
        .skip_locked()
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    let limit_pos = stmt.sql.find(" LIMIT ").expect("has LIMIT");
    let for_pos = stmt.sql.find(" FOR UPDATE").expect("has FOR UPDATE");
    assert!(limit_pos < for_pos, "LIMIT before FOR UPDATE: {}", stmt.sql);
}

// ---------- MySQL: most options work, NO KEY falls back to UPDATE ----------

#[cfg(feature = "mysql")]
#[test]
fn select_for_update_skip_locked_on_mysql() {
    let q = Job::objects()
        .select_for_update()
        .skip_locked()
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE SKIP LOCKED"),
        "MySQL skip_locked: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn no_key_falls_back_to_plain_for_update_on_mysql() {
    // MySQL has no `FOR NO KEY UPDATE` — falls back to `FOR UPDATE`.
    let q = Job::objects()
        .select_for_update()
        .no_key()
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE"),
        "MySQL no_key falls back: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("NO KEY"),
        "no NO KEY on MySQL: {}",
        stmt.sql
    );
}

#[cfg(feature = "mysql")]
#[test]
fn of_table_uses_backticks_on_mysql() {
    let q = Job::objects()
        .select_for_update()
        .of(&["sfu_job"])
        .compile()
        .unwrap();
    let stmt = MySql.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE OF `sfu_job`"),
        "MySQL `of` with backticks: {}",
        stmt.sql
    );
}

// ---------- SQLite: lock clause is a no-op ----------

#[cfg(feature = "sqlite")]
#[test]
fn select_for_update_emits_no_lock_clause_on_sqlite() {
    let q = Job::objects()
        .select_for_update()
        .skip_locked()
        .nowait()
        .no_key()
        .of(&["sfu_job"])
        .compile()
        .unwrap();
    let stmt = Sqlite.compile_select(&q).unwrap();
    assert!(
        !stmt.sql.contains("FOR UPDATE"),
        "SQLite: no row-lock syntax, lock clause skipped: {}",
        stmt.sql
    );
    assert!(
        !stmt.sql.contains("SKIP LOCKED"),
        "SQLite: no SKIP LOCKED: {}",
        stmt.sql
    );
}

// ---------- Chained flags compose without losing prior state ----------

#[test]
fn chained_flag_calls_imply_select_for_update() {
    // Calling `.skip_locked()` without prior `.select_for_update()`
    // implicitly sets the lock — Django-style ergonomics.
    let q = Job::objects().skip_locked().compile().unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql.ends_with(" FOR UPDATE SKIP LOCKED"),
        "skip_locked alone implies FOR UPDATE: {}",
        stmt.sql
    );
}

#[test]
fn of_calls_accumulate() {
    let q = Job::objects()
        .select_for_update()
        .of(&["sfu_job"])
        .of(&["other_table"])
        .compile()
        .unwrap();
    let stmt = Postgres.compile_select(&q).unwrap();
    assert!(
        stmt.sql
            .ends_with(r#" FOR UPDATE OF "sfu_job", "other_table""#),
        "two .of() calls accumulate: {}",
        stmt.sql
    );
}
