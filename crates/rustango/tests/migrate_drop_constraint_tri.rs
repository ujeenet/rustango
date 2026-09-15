//! Dropping a CHECK constraint and a composite FK, executed against
//! every backend — one body, three dialects (#559, #1461).
//!
//! ## Why this file exists
//!
//! `migrate/diff.rs` emitted the PostgreSQL shape to MySQL:
//!
//! ```text
//! ALTER TABLE `t` DROP CONSTRAINT IF EXISTS `ck_x`
//!   ERROR 1064 (42000): ... right syntax to use near 'IF EXISTS `ck_x`'
//! ```
//!
//! MySQL accepts `DROP CONSTRAINT` from 8.0.19 but takes no `IF EXISTS`
//! on any drop-constraint form. So every MySQL migration that dropped a
//! check constraint or a composite foreign key died with a syntax error.
//!
//! It survived because **the only tests covering it were emission
//! tests**. `migrate_diff.rs`, `migrate_invert.rs` and the unit tests in
//! `diff.rs` all compare the rendered string, and two of them asserted
//! the unparseable one — under names ending `_uses_backticks`, which was
//! accurate as far as it went. An emission test proves the writer
//! emitted what its author intended. It cannot prove the server accepts
//! it.
//!
//! That is the distinction worth holding on to when reading the rest of
//! the divergence surface: a dialect branch that *rejects* an operation
//! is fully covered by an emission test, because the rejection is the
//! whole contract. A branch that *rewrites* into different SQL is not
//! covered by one at all. This was a rewrite.
//!
//! The two MySQL migration suites that do run live
//! (`migrate_fake_initial_mysql_live`, `migrate_reconcile_mysql_live`)
//! could not have caught it either: every migration they build carries
//! only `SchemaChange::CreateTable`, so they never reach this branch.
//!
//! ## What is genuinely per-dialect here
//!
//! | | PostgreSQL | MySQL | SQLite |
//! |---|---|---|---|
//! | drop a CHECK | `DROP CONSTRAINT IF EXISTS` | `DROP CHECK` | **rejected at render** |
//! | drop a composite FK | `DROP CONSTRAINT IF EXISTS` | `DROP FOREIGN KEY` | **rejected at render** |
//! | idempotent? | yes | **no** — error 3821 | n/a |
//!
//! SQLite has no `ALTER TABLE DROP CONSTRAINT` at all, so the framework
//! refuses at render time with a message pointing at the rebuild-the-
//! table workaround (#559). That is an engine incapability, so it is
//! pinned with `by_dialect!` rather than papered over — and the SQLite
//! arm asserts the *refusal*, which is the behaviour users depend on.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::migrate::{render_changes_split_with_dialect, SchemaChange, SchemaSnapshot};
use rustango::sql::{raw_execute_pool, Auto, Pool};
use rustango::testkit::matrix::fresh_table;
use rustango::{by_dialect, tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_widget")]
#[rustango(app = "migrate_drop_constraint_tri")]
#[allow(dead_code)]
pub struct Widget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub n: i64,
}

/// The FK target. `unique_together` is what makes `(region, code)` a
/// legal thing for a composite foreign key to reference.
#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_parent")]
#[rustango(app = "migrate_drop_constraint_tri")]
#[rustango(unique_together = "region, code")]
#[allow(dead_code)]
pub struct Parent {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub region: String,
    pub code: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_child")]
#[rustango(app = "migrate_drop_constraint_tri")]
#[allow(dead_code)]
pub struct Child {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 16)]
    pub region: String,
    pub code: i64,
}

/// Rebuild all three tables, then give the parent the unique index its
/// `unique_together` implies.
///
/// Child before parent is load-bearing: a scenario may have left an FK
/// on the child, and MySQL refuses to drop a table another table still
/// references. Recreating the child first clears the constraint, after
/// which the parent drops cleanly.
///
/// The explicit `CreateIndex` is load-bearing too, and worth reading.
/// `fresh_table` goes through `testkit::create_tables_for`, whose
/// `emit_tables` creates the table, its FK constraints and its comments
/// — but **not** composite-unique indexes; `testkit/mod.rs:140` says so
/// outright. So a `unique_together` model built by the harness has no
/// composite uniqueness at all, and a foreign key cannot reference it:
/// PostgreSQL answers "no unique constraint matching given keys", MySQL
/// answers 1822. Rendering the index through `CreateIndex` here keeps
/// the DDL coming from the framework's own emitter rather than a
/// hand-written guess.
async fn setup(pool: &Pool) {
    fresh_table::<Widget>(pool).await;
    fresh_table::<Child>(pool).await;
    fresh_table::<Parent>(pool).await;

    run(
        pool,
        &render(
            pool,
            SchemaChange::CreateIndex {
                name: "uq_dc_tri_parent_region_code".into(),
                table: "dc_tri_parent".into(),
                columns: vec!["region".into(), "code".into()],
                unique: true,
                method: "btree".into(),
                where_clause: None,
                include: Vec::new(),
            },
        )
        .expect("render the composite unique index"),
    )
    .await;
}

/// Render one schema change through the pool's own dialect.
fn render(pool: &Pool, change: SchemaChange) -> Result<Vec<String>, String> {
    let snap = SchemaSnapshot::from_models(&[]);
    render_changes_split_with_dialect(&[change], &snap, pool.dialect()).map(|b| {
        let mut out = b.immediate;
        out.extend(b.deferred_fks);
        out
    })
}

/// Run every statement, failing with the statement that broke.
///
/// This is the whole point of the file: the emitted SQL reaches a real
/// parser. `ERROR 1064` shows up here and nowhere else.
async fn run(pool: &Pool, stmts: &[String]) {
    for sql in stmts {
        raw_execute_pool(pool, sql, Vec::new())
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{} rejected a statement the framework emitted for it:\n  {sql}\n  {e}",
                    pool.dialect().name()
                )
            });
    }
}

async fn insert_widget(pool: &Pool, n: i64) -> Result<u64, rustango::sql::ExecError> {
    let d = pool.dialect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        d.quote_ident("dc_tri_widget"),
        d.quote_ident("n"),
        if d.name() == "postgres" { "$1" } else { "?" },
    );
    raw_execute_pool(pool, &sql, vec![rustango::core::SqlValue::I64(n)]).await
}

/// A CHECK constraint: add it, prove it bites, drop it, prove it stopped.
///
/// Asserting on the *enforcement* rather than on the rendered string is
/// deliberate. A string assertion is what let the broken SQL live here
/// for as long as it did.
async fn check_constraint_can_be_added_and_dropped(pool: &Pool) {
    let add = SchemaChange::AddCheckConstraint {
        name: "ck_dc_tri_n_positive".into(),
        table: "dc_tri_widget".into(),
        expr: "n > 0".into(),
    };
    let drop = SchemaChange::DropCheckConstraint {
        name: "ck_dc_tri_n_positive".into(),
        table: "dc_tri_widget".into(),
    };

    let supported = by_dialect! { pool,
        postgres => true,
            because "ALTER TABLE ... ADD/DROP CONSTRAINT is native, and the drop takes \
                     IF EXISTS so it is idempotent",
        mysql => true,
            because "MySQL 8.0.16+ enforces CHECK constraints; the drop is spelled \
                     DROP CHECK and takes no IF EXISTS — emitting the Postgres form \
                     here was error 1064 (#1461)",
        sqlite => false,
            because "SQLite has no ALTER TABLE DROP CONSTRAINT at all, so the framework \
                     refuses at render time and points at the rebuild-the-table \
                     workaround (#559)",
    };

    if !supported.value {
        let err = render(pool, add).expect_err("SQLite must refuse to add a CHECK");
        assert!(
            err.contains("AddCheckConstraint") && err.contains("sqlite"),
            "the refusal should name the operation and the dialect — {}\ngot: {err}",
            supported.why
        );
        let err = render(pool, drop).expect_err("SQLite must refuse to drop a CHECK");
        assert!(
            err.contains("DropCheckConstraint") && err.contains("sqlite"),
            "got: {err}"
        );
        return;
    }

    run(pool, &render(pool, add).expect("render ADD CHECK")).await;
    assert!(
        insert_widget(pool, -1).await.is_err(),
        "the CHECK is added, so a negative row must be rejected on {}",
        pool.dialect().name()
    );

    run(pool, &render(pool, drop).expect("render DROP CHECK")).await;
    insert_widget(pool, -1)
        .await
        .expect("the CHECK was dropped, so a negative row must now be accepted");
}

/// A composite FK: add it, prove it bites, drop it, prove it stopped.
async fn composite_fk_can_be_added_and_dropped(pool: &Pool) {
    let add = SchemaChange::AddCompositeFk {
        table: "dc_tri_child".into(),
        name: "fk_dc_tri_child_parent".into(),
        to: "dc_tri_parent".into(),
        from: vec!["region".into(), "code".into()],
        on: vec!["region".into(), "code".into()],
    };
    let drop = SchemaChange::DropCompositeFk {
        table: "dc_tri_child".into(),
        name: "fk_dc_tri_child_parent".into(),
    };

    let supported = by_dialect! { pool,
        postgres => true,
            because "composite FKs are native; the ADD is deferred so the target exists first",
        mysql => true,
            because "native too, but the drop is DROP FOREIGN KEY — the Postgres form was \
                     error 1064, and ddl::drop_constraints_sql_with_dialect already knew \
                     this for per-field FKs while diff.rs did not (#1461)",
        sqlite => false,
            because "no ALTER TABLE DROP CONSTRAINT, same as the CHECK case (#559)",
    };

    if !supported.value {
        let err = render(pool, drop).expect_err("SQLite must refuse to drop a composite FK");
        assert!(
            err.contains("DropCompositeFk") && err.contains("sqlite"),
            "the refusal should name the operation and the dialect — {}\ngot: {err}",
            supported.why
        );
        return;
    }

    run(pool, &render(pool, add).expect("render ADD FK")).await;

    let d = pool.dialect();
    let orphan = format!(
        "INSERT INTO {} ({}, {}) VALUES ({}, {})",
        d.quote_ident("dc_tri_child"),
        d.quote_ident("region"),
        d.quote_ident("code"),
        if d.name() == "postgres" { "$1" } else { "?" },
        if d.name() == "postgres" { "$2" } else { "?" },
    );
    let binds = || {
        vec![
            rustango::core::SqlValue::String("nowhere".into()),
            rustango::core::SqlValue::I64(999),
        ]
    };
    assert!(
        raw_execute_pool(pool, &orphan, binds()).await.is_err(),
        "the FK is added, so a row with no parent must be rejected on {}",
        d.name()
    );

    run(pool, &render(pool, drop).expect("render DROP FK")).await;
    raw_execute_pool(pool, &orphan, binds())
        .await
        .expect("the FK was dropped, so the orphan row must now be accepted");
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        check_constraint_can_be_added_and_dropped,
        composite_fk_can_be_added_and_dropped,
    ],
}
