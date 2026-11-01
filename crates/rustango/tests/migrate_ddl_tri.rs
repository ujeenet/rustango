//! Migration DDL executed against every backend — one body, three
//! dialects (#559, #1461).
//!
//! Covers dropping a CHECK constraint, dropping a composite FK, and
//! renaming a table and a column. Every one of those was emitted as
//! PostgreSQL-shaped SQL to all three engines at some point, and every
//! one was pinned by an emission test that could not notice.
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
//! | drop a CHECK | `DROP CONSTRAINT IF EXISTS` | `DROP CHECK` | **table rebuild** |
//! | drop a composite FK | `DROP CONSTRAINT IF EXISTS` | `DROP FOREIGN KEY` | **table rebuild** |
//! | idempotent? | yes | **no** — error 3821 | n/a |
//!
//! SQLite has no `ALTER TABLE DROP CONSTRAINT`, so the migrate runner
//! rebuilds the table (#2127). The SQLite arm here asserts the render
//! hands the change to that rebuild; `migrate_autodetect_tri` runs it.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::migrate::{render_changes_split_with_dialect, SchemaChange, SchemaSnapshot};
use rustango::sql::{raw_execute_pool, Auto, Pool};
use rustango::testkit::matrix::fresh_table;
use rustango::{by_dialect, tri_dialect_test, Model};

#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_widget")]
#[rustango(app = "migrate_ddl_tri")]
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
#[rustango(app = "migrate_ddl_tri")]
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
#[rustango(app = "migrate_ddl_tri")]
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
    // The rename scenario moves `dc_tri_widget` aside and back. A panic
    // between those two steps leaves the intermediate name behind on a
    // persistent server, and every later run then dies on its first
    // statement with an error pointing at a rename that is correct.
    // Nothing else drops this, because no model is named for it.
    rustango::testkit::matrix::drop_table(pool, "dc_tri_widget_renamed").await;

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

/// SQLite applies `change` by a table rebuild only the runner runs (#2127);
/// `migrate_autodetect_tri` proves the rebuilt table enforces it.
fn assert_rebuilds(pool: &Pool, change: SchemaChange, table: &str) {
    let snap = SchemaSnapshot::from_models(&[Widget::SCHEMA, Parent::SCHEMA, Child::SCHEMA]);
    let b = render_changes_split_with_dialect(&[change], &snap, pool.dialect())
        .expect("SQLite renders the change as a rebuild");
    assert!(
        b.immediate.is_empty(),
        "no in-place ALTER: {:?}",
        b.immediate
    );
    assert_eq!(b.rebuild.as_ref().map(|r| r.table()), Some(table));
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

/// Insert a widget through the ORM.
///
/// This hand-wrote its own `INSERT`, including a
/// `if d.name() == "postgres" { "$1" } else { "?" }` placeholder branch
/// — a fourth copy of a rule the dialect already owns, inside the file
/// whose subject is that exact class of mistake. `insert_pool` picks the
/// placeholder, quotes the identifiers and binds the value.
async fn insert_widget(pool: &Pool, n: i64) -> Result<(), rustango::sql::ExecError> {
    let mut w = Widget {
        id: Auto::default(),
        n,
    };
    w.insert_pool(pool).await.map(|()| ())
}

/// Insert a child row through the ORM.
async fn insert_child(
    pool: &Pool,
    region: &str,
    code: i64,
) -> Result<(), rustango::sql::ExecError> {
    let mut c = Child {
        id: Auto::default(),
        region: region.to_owned(),
        code,
    };
    c.insert_pool(pool).await.map(|()| ())
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
            because "SQLite has no ALTER TABLE ADD/DROP CONSTRAINT, so the runner \
                     rebuilds the table (#2127)",
    };

    if !supported.value {
        assert_rebuilds(pool, add, "dc_tri_widget");
        assert_rebuilds(pool, drop, "dc_tri_widget");
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
            because "no ALTER TABLE ADD/DROP CONSTRAINT, same as the CHECK case (#2127)",
    };

    if !supported.value {
        assert_rebuilds(pool, add, "dc_tri_child");
        assert_rebuilds(pool, drop, "dc_tri_child");
        return;
    }

    run(pool, &render(pool, add).expect("render ADD FK")).await;

    assert!(
        insert_child(pool, "nowhere", 999).await.is_err(),
        "the FK is added, so a row with no parent must be rejected on {}",
        pool.dialect().name()
    );

    run(pool, &render(pool, drop).expect("render DROP FK")).await;
    insert_child(pool, "nowhere", 999)
        .await
        .expect("the FK was dropped, so the orphan row must now be accepted");
}

/// Renaming a table, executed.
///
/// `RenameTable` emitted `ALTER TABLE "old" RENAME TO "new"` with the
/// double quotes written into the format string — no `quote_ident`, no
/// dialect guard. On MySQL `"` delimits a string, so this is
/// `ERROR 1064`, exactly the failure #1461 fixed two arms further down
/// the same `match`. Every neighbouring `AlterColumn*` arm calls
/// `guard_alter_column_dialect`; these two never did, and unlike those
/// arms they do not need a guard — both renames are portable, they just
/// needed the dialect's quoting.
///
/// Asserted by using the new name through the ORM: a rename that emitted
/// nothing would leave the old table in place and the query would fail.
async fn a_table_can_be_renamed(pool: &Pool) {
    run(
        pool,
        &render(
            pool,
            SchemaChange::RenameTable {
                old_name: "dc_tri_widget".into(),
                new_name: "dc_tri_widget_renamed".into(),
            },
        )
        .expect("render RENAME TABLE"),
    )
    .await;

    let d = pool.dialect();
    // Read through the renamed table. `Widget::objects()` still points at
    // the old name, so this is the one place a literal name is needed —
    // and it is a SELECT the test owns, not emitted DDL.
    let sql = format!(
        "SELECT COUNT(*) FROM {}",
        d.quote_ident("dc_tri_widget_renamed")
    );
    raw_execute_pool(pool, &sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("the renamed table should exist on {}: {e}", d.name()));

    // And put it back, so the scenario is order-independent.
    run(
        pool,
        &render(
            pool,
            SchemaChange::RenameTable {
                old_name: "dc_tri_widget_renamed".into(),
                new_name: "dc_tri_widget".into(),
            },
        )
        .expect("render RENAME TABLE back"),
    )
    .await;
    insert_widget(pool, 1)
        .await
        .expect("the table is back under its original name");
}

/// Renaming a column, executed. Same bug, same arm neighbourhood.
async fn a_column_can_be_renamed(pool: &Pool) {
    run(
        pool,
        &render(
            pool,
            SchemaChange::RenameColumn {
                table: "dc_tri_widget".into(),
                old_column: "n".into(),
                new_column: "renamed_n".into(),
            },
        )
        .expect("render RENAME COLUMN"),
    )
    .await;

    let d = pool.dialect();
    let sql = format!(
        "SELECT {} FROM {}",
        d.quote_ident("renamed_n"),
        d.quote_ident("dc_tri_widget"),
    );
    raw_execute_pool(pool, &sql, Vec::new())
        .await
        .unwrap_or_else(|e| panic!("the renamed column should exist on {}: {e}", d.name()));
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_i32_tag")]
#[rustango(app = "migrate_ddl_tri")]
#[allow(dead_code)]
pub struct I32Tag {
    #[rustango(primary_key)]
    pub id: Auto<i32>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "dc_tri_i32_post")]
#[rustango(app = "migrate_ddl_tri")]
#[rustango(m2m(
    name = "tags",
    to = "dc_tri_i32_tag",
    through = "dc_tri_i32_post_tags",
    src = "post_id",
    dst = "tag_id"
))]
#[allow(dead_code)]
pub struct I32Post {
    #[rustango(primary_key)]
    pub id: Auto<i32>,
}

/// An auto junction over `i32` pks gets its FKs (MySQL refused BIGINT to
/// INT) and the manager reads it back.
async fn an_i32_pk_junction_gets_its_foreign_keys(pool: &Pool) {
    let drop_all = || async {
        for t in ["dc_tri_i32_post_tags", "dc_tri_i32_post", "dc_tri_i32_tag"] {
            rustango::testkit::matrix::drop_table(pool, t).await;
        }
    };
    drop_all().await;
    let snap = SchemaSnapshot::from_models(&[I32Tag::SCHEMA, I32Post::SCHEMA]);
    let changes = rustango::migrate::detect_changes(&SchemaSnapshot::from_models(&[]), &snap);
    let b = render_changes_split_with_dialect(&changes, &snap, pool.dialect())
        .expect("render the junction");
    run(pool, &b.immediate).await;
    run(pool, &b.deferred_fks).await;
    let mut tag = I32Tag { id: Auto::Unset };
    tag.insert_pool(pool).await.expect("tag");
    let mut post = I32Post { id: Auto::Unset };
    post.insert_pool(pool).await.expect("post");
    let tag_id = tag.id.get().copied().expect("tag id");
    post.tags_m2m().add(tag_id, pool).await.expect("add");
    assert_eq!(
        post.tags_m2m().all::<i64>(pool).await.expect("all"),
        vec![i64::from(tag_id)]
    );
    drop_all().await;
}

/// A stored snapshot names a junction's columns, not their width, so a
/// junction made as `BIGINT` before diffs to nothing.
#[test]
fn an_existing_bigint_junction_does_not_churn() {
    let now = SchemaSnapshot::from_models(&[I32Tag::SCHEMA, I32Post::SCHEMA]);
    let stored = serde_json::to_value(&now).unwrap();
    let junction = &stored["m2m_tables"][0];
    let keys: Vec<&str> = junction
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["dst_col", "dst_table", "src_col", "src_table", "through"]
    );
    let prev: SchemaSnapshot = serde_json::from_value(stored).unwrap();
    assert!(rustango::migrate::detect_changes(&prev, &now).is_empty());
}

/// MySQL's 3072-byte key cap, checked against the server: the widest
/// junction the renderer allows is created, one char more gets 1071.
async fn mysql_junction_key_limit_matches_the_server(pool: &Pool) {
    if pool.dialect().name() != "mysql" {
        return;
    }
    let snap = |len: u32| -> SchemaSnapshot {
        let table = |name: &str| {
            serde_json::json!({ "name": name, "model": name, "fields": [
                { "name": "id", "column": "id", "ty": "string", "nullable": false,
                  "primary_key": true, "max_length": len }] })
        };
        serde_json::from_value(serde_json::json!({ "tables": [
            table("dc_tri_jpost"),
            table("dc_tri_jtag"),
        ] }))
        .unwrap()
    };
    let changes = [
        SchemaChange::CreateTable("dc_tri_jpost".into()),
        SchemaChange::CreateTable("dc_tri_jtag".into()),
        SchemaChange::CreateM2MTable {
            through: "dc_tri_jpost_tags".into(),
            src_table: "dc_tri_jpost".into(),
            src_col: "post_id".into(),
            dst_table: "dc_tri_jtag".into(),
            dst_col: "tag_id".into(),
        },
    ];
    let drop_all = || async {
        for t in ["dc_tri_jpost_tags", "dc_tri_jpost", "dc_tri_jtag"] {
            rustango::testkit::matrix::drop_table(pool, t).await;
        }
    };
    drop_all().await;
    let err = render_changes_split_with_dialect(&changes, &snap(385), pool.dialect())
        .expect_err("385 + 385 chars is over 3072 bytes");
    assert!(err.contains("3072"), "{err}");
    let b = render_changes_split_with_dialect(&changes, &snap(384), pool.dialect())
        .expect("384 + 384 chars is exactly 3072 bytes");
    let junction = b
        .immediate
        .iter()
        .find(|s| s.contains("dc_tri_jpost_tags"))
        .expect("junction DDL")
        .clone();
    run(pool, &b.immediate).await;
    run(pool, &b.deferred_fks).await;
    drop_all().await;
    // One char wider: the server itself refuses the key.
    let wider = junction.replacen("VARCHAR(384)", "VARCHAR(385)", 1);
    assert_ne!(wider, junction, "{junction}");
    let e = raw_execute_pool(pool, &wider, Vec::new())
        .await
        .expect_err("MySQL took a key over 3072 bytes");
    assert!(
        e.to_string().contains("1071") || e.to_string().contains("too long"),
        "{e}"
    );
    drop_all().await;
}

tri_dialect_test! {
    setup: setup,
    scenarios: [
        an_i32_pk_junction_gets_its_foreign_keys,
        mysql_junction_key_limit_matches_the_server,
        check_constraint_can_be_added_and_dropped,
        composite_fk_can_be_added_and_dropped,
        a_table_can_be_renamed,
        a_column_can_be_renamed,
    ],
}
