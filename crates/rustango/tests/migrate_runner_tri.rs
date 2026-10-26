//! The migrate runner's lock, its refusals, generated names and the FKs a
//! key change must move, on every backend.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::core::Model as _;
use rustango::migrate::callbacks::MigrationCallbackFut;
use rustango::migrate::{
    file, make_migrations_from, migrate_pool_with_ledger, unapply_pool_with_ledger, CallbackOp,
    DataOp, Migration, Operation, SchemaChange,
};
use rustango::sql::{raw_execute_pool, Pool};
use rustango::testkit::matrix::{drop_table, fresh_table};
use rustango::{tri_dialect_test, Model};
use serde_json::{json, Value};

async fn no_setup(_pool: &Pool) {}

/// Two composite indexes whose derived names share their first 63 bytes,
/// and a field index whose derived name is longer than MySQL's 64.
#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_subscription_notification_preference_entries")]
#[rustango(index("primary_contact_email_address, secondary_a"))]
#[rustango(index("primary_contact_email_address, secondary_b"))]
#[allow(dead_code)]
pub struct LongIdx {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(index, max_length = 40)]
    pub primary_contact_email_address: String,
    #[rustango(max_length = 10)]
    pub secondary_a: String,
    #[rustango(max_length = 10)]
    pub secondary_b: String,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_ck_a")]
#[rustango(check(name = "mr_ck_positive", expr = "n >= 0"))]
#[allow(dead_code)]
pub struct CkA {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "mr_ck_b")]
#[rustango(check(name = "mr_ck_positive", expr = "n >= 0"))]
#[allow(dead_code)]
pub struct CkB {
    #[rustango(primary_key)]
    pub id: i64,
    pub n: i64,
}

#[test]
fn derived_index_names_fit_and_differ() {
    let names: Vec<&str> = LongIdx::SCHEMA.indexes.iter().map(|i| i.name).collect();
    assert_eq!(names.len(), 3, "{names:?}");
    for n in &names {
        assert!(n.len() <= 63, "`{n}` is {} bytes", n.len());
    }
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{names:?}");
}

/// The snapshot keys CHECKs by name, so one of the two tables loses its:
/// makemigrations says so, and still runs.
#[cfg(feature = "runtime")]
#[test]
fn check_name_on_two_tables_warns() {
    let buf = rustango::testkit::CaptureWriter::default();
    let sink = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    let dir = tempfile::tempdir().expect("tempdir");
    tracing::subscriber::with_default(subscriber, || {
        rustango::migrate::make_migrations(dir.path(), None).expect("makemigrations still runs")
    });
    let logs = buf.contents();
    assert!(
        logs.contains("mr_ck_positive")
            && logs.contains("mr_ck_a")
            && logs.contains("mr_ck_b")
            && logs.contains("MySQL"),
        "{logs}"
    );
}

/// The model's indexes create on every backend.
async fn long_index_names_apply(pool: &Pool) {
    fresh_table::<LongIdx>(pool).await;
    drop_table(pool, LongIdx::SCHEMA.table).await;
}

/// Widening a PK that another table's FK references, with the FK column.
async fn referenced_key_widens(pool: &Pool) {
    let (a, b) = ("mr_wk_author", "mr_wk_book");
    let ledger = "mr_wk_ledger";
    let dir = fresh(pool, &[b, a], ledger).await;
    let with = |ty: &str| {
        let key = |name: &str, fk: Option<&str>| {
            let mut f = json!({"name": name, "column": name, "ty": ty,
                "nullable": false, "primary_key": fk.is_none()});
            if let Some(to) = fk {
                f["fk"] = json!({"kind": "fk", "to": to, "on": "id", "on_delete": "CASCADE"});
            }
            f
        };
        let book_id = json!({"name": "id", "column": "id", "ty": "i64",
            "nullable": false, "primary_key": true});
        snap(vec![
            table(a, vec![key("id", None)]),
            table(b, vec![book_id, key("author_id", Some(a))]),
        ])
    };
    let step = |ty: &str| {
        make_migrations_from(dir.path(), &with(ty), None)
            .expect("makemigrations")
            .expect("a migration")
    };
    step("i32");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect("initial");
    let widen = step("i64");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("widening on {}: {e}", pool.dialect().name()));
    let d = pool.dialect();
    let (qa, qb) = (d.quote_ident(a), d.quote_ident(b));
    exec(pool, &format!("INSERT INTO {qa} (id) VALUES (3000000000)"))
        .await
        .expect("the PK is BIGINT");
    exec(
        pool,
        &format!("INSERT INTO {qb} (id, author_id) VALUES (1, 3000000000)"),
    )
    .await
    .expect("the FK column is BIGINT");
    assert!(
        exec(
            pool,
            &format!("INSERT INTO {qb} (id, author_id) VALUES (2, 7)")
        )
        .await
        .is_err(),
        "the FK is back on {}",
        d.name()
    );
    exec(pool, &format!("DELETE FROM {qb}")).await.unwrap();
    exec(pool, &format!("DELETE FROM {qa}")).await.unwrap();
    unapply_pool_with_ledger(pool, dir.path(), &widen.name, ledger)
        .await
        .unwrap_or_else(|e| panic!("narrowing on {}: {e}", d.name()));
    for t in [b, a, ledger] {
        drop_table(pool, t).await;
    }
}

/// FKs on `t`, from the catalog.
async fn fk_count(pool: &Pool, t: &str) -> i64 {
    let sql = match pool.dialect().name() {
        "sqlite" => format!("SELECT COUNT(*) FROM pragma_foreign_key_list('{t}')"),
        name => format!(
            "SELECT COUNT(*) FROM information_schema.table_constraints \
             WHERE table_schema = {} AND table_name = '{t}' \
             AND constraint_type = 'FOREIGN KEY'",
            if name == "mysql" {
                "DATABASE()"
            } else {
                "current_schema()"
            }
        ),
    };
    let n: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .expect("count FKs");
    n[0].0
}

/// A widened key beside a table created, a column added and one dropped in
/// the same migration: each referencing FK ends up there once, or gone.
async fn referenced_key_widens_beside_new_fks(pool: &Pool) {
    let (a, kid, pal, old) = ("mr_wn_author", "mr_wn_kid", "mr_wn_pal", "mr_wn_old");
    let ledger = "mr_wn_ledger";
    let dir = fresh(pool, &[kid, pal, old, a], ledger).await;
    let key = |ty: &str| json!({"name": "id", "column": "id", "ty": ty, "nullable": false, "primary_key": true});
    let fk_col = |ty: &str| {
        json!({"name": "author_id", "column": "author_id", "ty": ty, "nullable": true,
            "primary_key": false,
            "fk": {"kind": "fk", "to": a, "on": "id", "on_delete": "CASCADE"}})
    };
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![
            table(a, vec![key("i32")]),
            table(pal, vec![key("i64")]),
            table(old, vec![key("i64"), fk_col("i32")]),
        ]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect("initial");
    let s = |t: &str| t.to_owned();
    let mig = Migration {
        name: "0002_widen".into(),
        prev: Some(initial.name.clone()),
        snapshot: snap(vec![
            table(a, vec![key("i64")]),
            table(pal, vec![key("i64"), fk_col("i64")]),
            table(old, vec![key("i64")]),
            table(kid, vec![key("i64"), fk_col("i64")]),
        ]),
        forward: vec![
            Operation::Schema(SchemaChange::AlterColumnType {
                table: s(a),
                column: s("id"),
                from: s("i32"),
                to: s("i64"),
            }),
            Operation::Schema(SchemaChange::CreateTable(s(kid))),
            Operation::Schema(SchemaChange::AddColumn {
                table: s(pal),
                column: s("author_id"),
            }),
            Operation::Schema(SchemaChange::DropColumn {
                table: s(old),
                column: s("author_id"),
            }),
        ],
        ..initial
    };
    write(dir.path(), &mig);
    let name = pool.dialect().name();
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("widening on {name}: {e}"));
    assert_eq!(
        (
            fk_count(pool, kid).await,
            fk_count(pool, pal).await,
            fk_count(pool, old).await
        ),
        (1, 1, 0),
        "FKs on {name}"
    );
    unapply_pool_with_ledger(pool, dir.path(), &mig.name, ledger)
        .await
        .unwrap_or_else(|e| panic!("narrowing on {name}: {e}"));
    assert_eq!(
        (fk_count(pool, pal).await, fk_count(pool, old).await),
        (0, 1),
        "FKs after unapply on {name}"
    );
    for t in [kid, pal, old, a, ledger] {
        drop_table(pool, t).await;
    }
}

/// Widening a key an M2M junction references: MySQL widens the junction
/// column with it, so the junction's FK comes back (MySQL error 3780 otherwise).
async fn referenced_key_widens_its_junction(pool: &Pool) {
    let (post, tag, through) = ("mr_wj_post", "mr_wj_tag", "mr_wj_post_tags");
    let ledger = "mr_wj_ledger";
    let dir = fresh(pool, &[through, post, tag], ledger).await;
    let with = |ty: &str| -> rustango::migrate::SchemaSnapshot {
        let key = json!({"name": "id", "column": "id", "ty": ty, "nullable": false,
            "primary_key": true});
        serde_json::from_value(json!({
            "tables": [table(post, vec![id()]), table(tag, vec![key])],
            "m2m_tables": [{"through": through, "src_table": post, "src_col": "post_id",
                "dst_table": tag, "dst_col": "tag_id"}],
        }))
        .expect("snapshot")
    };
    let step = |ty: &str| {
        make_migrations_from(dir.path(), &with(ty), None)
            .expect("makemigrations")
            .expect("a migration")
    };
    step("i32");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect("initial");
    let widen = step("i64");
    let name = pool.dialect().name();
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("widening on {name}: {e}"));
    assert_eq!(fk_count(pool, through).await, 2, "junction FKs on {name}");
    let d = pool.dialect();
    let (qp, qt, qj) = (
        d.quote_ident(post),
        d.quote_ident(tag),
        d.quote_ident(through),
    );
    exec(pool, &format!("INSERT INTO {qp} (id) VALUES (1)"))
        .await
        .unwrap();
    exec(pool, &format!("INSERT INTO {qt} (id) VALUES (3000000000)"))
        .await
        .expect("the PK is BIGINT");
    exec(
        pool,
        &format!("INSERT INTO {qj} (post_id, tag_id) VALUES (1, 3000000000)"),
    )
    .await
    .expect("the junction column is BIGINT");
    assert!(
        exec(
            pool,
            &format!("INSERT INTO {qj} (post_id, tag_id) VALUES (1, 7)")
        )
        .await
        .is_err(),
        "the junction FK is back on {name}"
    );
    for t in [through, post, tag] {
        exec(pool, &format!("DELETE FROM {}", d.quote_ident(t)))
            .await
            .unwrap();
    }
    unapply_pool_with_ledger(pool, dir.path(), &widen.name, ledger)
        .await
        .unwrap_or_else(|e| panic!("narrowing on {name}: {e}"));
    assert_eq!(
        fk_count(pool, through).await,
        2,
        "junction FKs after unapply on {name}"
    );
    for t in [through, post, tag, ledger] {
        drop_table(pool, t).await;
    }
}

/// Rolling back drops that follow a column rename: the CHECK and the
/// partial index come back once the rename is undone, not before it.
async fn rollback_after_rename_restores_check_and_partial_index(pool: &Pool) {
    let t = "mr_rn_item";
    let ledger = "mr_rn_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let name = pool.dialect().name();
    // MySQL refuses to rename a column a CHECK reads (3959): the drop goes first.
    let check = true;
    let mut before = json!({
        "tables": [table(t, vec![id(), col("n", true)])],
        "indexes": [{"name": "mr_rn_pos_idx", "table": t, "columns": ["n"],
            "unique": false, "where_clause": "n > 0"}],
    });
    if check {
        before["checks"] = json!([{"name": "mr_rn_ck", "table": t, "expr": "n >= 0"}]);
    }
    let exclude = name == "postgres";
    if exclude {
        before["excludes"] = json!([{"name": "mr_rn_ex", "table": t, "using": "btree",
            "elements": [["n", "="]], "where_clause": "n > 100"}]);
    }
    let initial = make_migrations_from(
        dir.path(),
        &serde_json::from_value(before).expect("snapshot"),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    let mut forward = vec![Operation::Schema(SchemaChange::RenameColumn {
        table: t.into(),
        old_column: "n".into(),
        new_column: "m".into(),
    })];
    if check {
        forward.push(Operation::Schema(SchemaChange::DropCheckConstraint {
            name: "mr_rn_ck".into(),
            table: t.into(),
        }));
    }
    forward.push(Operation::Schema(SchemaChange::DropIndex {
        name: "mr_rn_pos_idx".into(),
        table: t.into(),
    }));
    if exclude {
        forward.push(Operation::Schema(SchemaChange::DropExclusionConstraint {
            name: "mr_rn_ex".into(),
            table: t.into(),
        }));
    }
    let mig = Migration {
        name: "0002_rename".into(),
        prev: Some(initial.name.clone()),
        snapshot: snap(vec![table(t, vec![id(), col("m", true)])]),
        forward,
        ..initial
    };
    write(dir.path(), &mig);
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("rename on {name}: {e}"));
    unapply_pool_with_ledger(pool, dir.path(), &mig.name, ledger)
        .await
        .unwrap_or_else(|e| panic!("rollback on {name}: {e}"));
    let q = pool.dialect().quote_ident(t);
    exec(pool, &format!("INSERT INTO {q} (n) VALUES (1)"))
        .await
        .unwrap_or_else(|e| panic!("`n` is back on {name}: {e}"));
    if check {
        assert!(
            exec(pool, &format!("INSERT INTO {q} (n) VALUES (-1)"))
                .await
                .is_err(),
            "the CHECK is back on {name}"
        );
    }
    if exclude {
        exec(pool, &format!("INSERT INTO {q} (n) VALUES (101)"))
            .await
            .unwrap();
        assert!(
            exec(pool, &format!("INSERT INTO {q} (n) VALUES (101)"))
                .await
                .is_err(),
            "the exclusion constraint is back"
        );
    }
    for d in [t, ledger] {
        drop_table(pool, d).await;
    }
}

/// A CHECK and a partial index dropped with their renamed table, before or
/// after the rename: rolling back creates each once.
async fn rollback_of_a_renamed_dropped_table_restores_its_check_once(pool: &Pool) {
    let (t, u) = ("mr_rt_item", "mr_rt_moved");
    let ledger = "mr_rt_ledger";
    let name = pool.dialect().name();
    if name == "sqlite" {
        return; // Its CHECK drop rebuilds the table, refused before a later DropTable.
    }
    let rename = || {
        Operation::Schema(SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        })
    };
    let drops = |on: &str| {
        vec![
            Operation::Schema(SchemaChange::DropCheckConstraint {
                name: "mr_rt_ck".into(),
                table: on.into(),
            }),
            Operation::Schema(SchemaChange::DropIndex {
                name: "mr_rt_pos_idx".into(),
                table: on.into(),
            }),
        ]
    };
    let before_rename = [drops(t), vec![rename()]].concat();
    let after_rename = [vec![rename()], drops(u)].concat();
    for mut forward in [before_rename, after_rename] {
        forward.push(Operation::Schema(SchemaChange::DropTable(u.into())));
        let dir = fresh(pool, &[t, u], ledger).await;
        let before = json!({
            "tables": [table(t, vec![id(), col("n", true)])],
            "indexes": [{"name": "mr_rt_pos_idx", "table": t, "columns": ["n"],
                "unique": false, "where_clause": "n > 0"}],
            "checks": [{"name": "mr_rt_ck", "table": t, "expr": "n >= 0"}],
        });
        let initial = make_migrations_from(
            dir.path(),
            &serde_json::from_value(before).expect("snapshot"),
            None,
        )
        .expect("makemigrations")
        .expect("a migration");
        let mig = Migration {
            name: "0002_drop".into(),
            prev: Some(initial.name.clone()),
            snapshot: snap(vec![]),
            forward,
            ..initial
        };
        write(dir.path(), &mig);
        migrate_pool_with_ledger(pool, dir.path(), ledger)
            .await
            .unwrap_or_else(|e| panic!("drop on {name}: {e}"));
        unapply_pool_with_ledger(pool, dir.path(), &mig.name, ledger)
            .await
            .unwrap_or_else(|e| panic!("rollback on {name}: {e}"));
        let q = pool.dialect().quote_ident(t);
        exec(pool, &format!("INSERT INTO {q} (n) VALUES (1)"))
            .await
            .unwrap_or_else(|e| panic!("`{t}` is back on {name}: {e}"));
        assert!(
            exec(pool, &format!("INSERT INTO {q} (n) VALUES (-1)"))
                .await
                .is_err(),
            "the CHECK is back on {name}"
        );
    }
    for d in [t, u, ledger] {
        drop_table(pool, d).await;
    }
}

/// Two branches add a column each. Going back to one branch unapplies the
/// other; SQLite rebuilt the table from the shared parent's snapshot and
/// refused to lose the first branch's column.
async fn unapplying_one_branch_keeps_the_others_column(pool: &Pool) {
    let t = "mr_br_item";
    let ledger = "mr_br_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let initial = make_migrations_from(dir.path(), &snap(vec![table(t, vec![id()])]), None)
        .expect("makemigrations")
        .expect("a migration");
    let branch = |name: &str, column: &str| Migration {
        name: name.into(),
        prev: Some(initial.name.clone()),
        snapshot: snap(vec![table(t, vec![id(), col(column, true)])]),
        forward: vec![Operation::Schema(SchemaChange::AddColumn {
            table: t.into(),
            column: column.into(),
        })],
        ..initial.clone()
    };
    write(dir.path(), &branch("0002_a", "a"));
    write(dir.path(), &branch("0002_b", "b"));
    write(
        dir.path(),
        &Migration {
            name: "0003_merge".into(),
            prev: Some("0002_b".into()),
            merges: vec!["0002_a".into()],
            snapshot: snap(vec![table(t, vec![id(), col("a", true), col("b", true)])]),
            forward: Vec::new(),
            ..initial.clone()
        },
    );
    let name = pool.dialect().name();
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("both branches on {name}: {e}"));
    rustango::migrate::migrate_to_pool_with_ledger(pool, dir.path(), "0002_a", ledger)
        .await
        .unwrap_or_else(|e| panic!("back to one branch on {name}: {e}"));
    let q = pool.dialect().quote_ident(t);
    exec(pool, &format!("INSERT INTO {q} (a) VALUES (1)"))
        .await
        .unwrap_or_else(|e| panic!("`a` kept on {name}: {e}"));
    assert!(
        exec(pool, &format!("INSERT INTO {q} (b) VALUES (1)"))
            .await
            .is_err(),
        "`b` gone on {name}"
    );
    for d in [t, ledger] {
        drop_table(pool, d).await;
    }
}

/// One branch adds a column, the other drops one. SQLite rebuilt the table
/// for the drop from its own branch's snapshot, which lacks the first column.
async fn applying_a_branch_keeps_the_others_column(pool: &Pool) {
    let t = "mr_bra_item";
    let ledger = "mr_bra_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![table(t, vec![id(), col("x", true)])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    let step = |name: &str, fields: Vec<Value>, change: SchemaChange| Migration {
        name: name.into(),
        prev: Some(initial.name.clone()),
        snapshot: snap(vec![table(t, fields)]),
        forward: vec![Operation::Schema(change)],
        ..initial.clone()
    };
    write(
        dir.path(),
        &step(
            "0002_a",
            vec![id(), col("x", true), col("a", true)],
            SchemaChange::AddColumn {
                table: t.into(),
                column: "a".into(),
            },
        ),
    );
    write(
        dir.path(),
        &step(
            "0002_b",
            vec![id()],
            SchemaChange::DropColumn {
                table: t.into(),
                column: "x".into(),
            },
        ),
    );
    write(
        dir.path(),
        &Migration {
            name: "0003_merge".into(),
            prev: Some("0002_b".into()),
            merges: vec!["0002_a".into()],
            snapshot: snap(vec![table(t, vec![id(), col("a", true)])]),
            forward: Vec::new(),
            ..initial.clone()
        },
    );
    let name = pool.dialect().name();
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("both branches on {name}: {e}"));
    let q = pool.dialect().quote_ident(t);
    exec(pool, &format!("INSERT INTO {q} (a) VALUES (1)"))
        .await
        .unwrap_or_else(|e| panic!("`a` kept on {name}: {e}"));
    for d in [t, ledger] {
        drop_table(pool, d).await;
    }
}

fn slow(_pool: Pool) -> MigrationCallbackFut {
    Box::pin(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        Ok(())
    })
}
rustango::register_migration_callback!("mr_slow", slow);

fn id() -> Value {
    json!({"name": "id", "column": "id", "ty": "i64", "nullable": false,
           "primary_key": true, "auto": true})
}

fn col(name: &str, nullable: bool) -> Value {
    json!({"name": name, "column": name, "ty": "i64", "nullable": nullable,
           "primary_key": false})
}

fn snap(tables: Vec<Value>) -> rustango::migrate::SchemaSnapshot {
    serde_json::from_value(json!({"tables": tables})).expect("snapshot")
}

fn table(name: &str, fields: Vec<Value>) -> Value {
    json!({"name": name, "model": name, "fields": fields})
}

/// `tables` and `ledger` dropped, and an empty migrations dir.
async fn fresh(pool: &Pool, tables: &[&str], ledger: &str) -> tempfile::TempDir {
    for t in tables.iter().chain([&ledger]) {
        drop_table(pool, t).await;
    }
    tempfile::tempdir().expect("tempdir")
}

fn write(dir: &std::path::Path, mig: &Migration) {
    file::write(&dir.join(format!("{}.json", mig.name)), mig).expect("write migration");
}

async fn exec(pool: &Pool, sql: &str) -> Result<(), String> {
    raw_execute_pool(pool, sql, Vec::new())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A downgrade refused at its second op ran nothing: the first op's
/// column is still there and the migration still applied.
async fn refused_downgrade_runs_nothing(pool: &Pool) {
    refused_downgrade(pool, "mr_rd_item", true).await;
}

async fn refused_loose_downgrade_runs_nothing(pool: &Pool) {
    refused_downgrade(pool, "mr_rl_item", false).await;
}

async fn refused_downgrade(pool: &Pool, t: &str, atomic: bool) {
    let ledger = format!("{t}_ledger");
    let dir = fresh(pool, &[t], &ledger).await;
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![table(t, vec![id(), col("req", false)])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    // Inverted, the AddColumn of `req` comes second, after `z` is dropped.
    let mig = Migration {
        name: "0002_swap".into(),
        prev: Some(initial.name.clone()),
        atomic,
        snapshot: snap(vec![table(t, vec![id(), col("z", true)])]),
        forward: vec![
            Operation::Schema(SchemaChange::DropColumn {
                table: t.into(),
                column: "req".into(),
            }),
            Operation::Schema(SchemaChange::AddColumn {
                table: t.into(),
                column: "z".into(),
            }),
        ],
        ..initial
    };
    write(dir.path(), &mig);
    migrate_pool_with_ledger(pool, dir.path(), &ledger)
        .await
        .expect("migrate");
    let err = unapply_pool_with_ledger(pool, dir.path(), &mig.name, &ledger)
        .await
        .expect_err("NOT NULL with no default is refused");
    assert!(err.to_string().contains("NOT NULL"), "{err}");
    let q = pool.dialect().quote_ident(t);
    exec(pool, &format!("INSERT INTO {q} (z) VALUES (1)"))
        .await
        .unwrap_or_else(|e| panic!("`z` was dropped on {}: {e}", pool.dialect().name()));
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE name = '0002_swap'",
        pool.dialect().quote_ident(&ledger)
    );
    let n: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .expect("ledger");
    assert_eq!(n[0].0, 1, "still applied");
    for d in [t, &ledger] {
        drop_table(pool, d).await;
    }
}

/// A unique key over rows that repeat it is refused before any DDL, with
/// the fix in the message; rows with a NULL in the key do not count.
async fn unique_key_over_duplicates_is_refused(pool: &Pool) {
    let t = "mr_uq_grant";
    let ledger = "mr_uq_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let cols = || vec![id(), col("a", false), col("b", true)];
    let initial = make_migrations_from(dir.path(), &snap(vec![table(t, cols())]), None)
        .expect("makemigrations")
        .expect("a migration");
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect("migrate");
    let q = pool.dialect().quote_ident(t);
    for (a, b) in [("1", "1"), ("1", "1"), ("2", "NULL"), ("2", "NULL")] {
        exec(pool, &format!("INSERT INTO {q} (a, b) VALUES ({a}, {b})"))
            .await
            .unwrap();
    }
    let with_key = serde_json::from_value(json!({
        "tables": [table(t, cols())],
        "indexes": [{"name": "mr_uq_grant_ab", "table": t, "columns": ["a", "b"], "unique": true}],
    }))
    .expect("snapshot");
    let mig = Migration {
        name: "0002_unique".into(),
        prev: Some(initial.name.clone()),
        snapshot: with_key,
        forward: vec![Operation::Schema(SchemaChange::CreateIndex {
            name: "mr_uq_grant_ab".into(),
            table: t.into(),
            columns: vec!["a".into(), "b".into()],
            unique: true,
            method: "btree".into(),
            where_clause: None,
            include: Vec::new(),
        })],
        ..initial
    };
    write(dir.path(), &mig);
    let err = migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .expect_err("duplicates refuse the key");
    let msg = err.to_string();
    assert!(
        msg.contains("mr_uq_grant_ab") && msg.contains("Remove the duplicate rows"),
        "{} said: {msg}",
        pool.dialect().name()
    );
    exec(pool, &format!("DELETE FROM {q} WHERE id = 2"))
        .await
        .unwrap();
    migrate_pool_with_ledger(pool, dir.path(), ledger)
        .await
        .unwrap_or_else(|e| panic!("NULLs do not block the key on {}: {e}", pool.dialect().name()));
    assert!(exec(pool, &format!("INSERT INTO {q} (a, b) VALUES (1, 1)"))
        .await
        .is_err());
    for d in [t, ledger] {
        drop_table(pool, d).await;
    }
}

/// The URL of `pool`'s server, or a fresh SQLite file.
fn url_of(pool: &Pool, file: &std::path::Path) -> String {
    match pool.dialect().name() {
        "postgres" => std::env::var("DATABASE_URL").expect("DATABASE_URL"),
        "mysql" => std::env::var("MYSQL_TEST_URL").expect("MYSQL_TEST_URL"),
        _ => format!("sqlite://{}?mode=rwc", file.display()),
    }
}

async fn pool_of(url: &str, max: u32) -> Pool {
    let timeout = std::time::Duration::from_secs(5);
    match url.split(':').next() {
        #[cfg(feature = "postgres")]
        Some("postgres" | "postgresql") => sqlx_pg(url, max, timeout).await,
        #[cfg(feature = "mysql")]
        Some("mysql") => Pool::from(
            rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(timeout)
                .connect(url)
                .await
                .expect("connect"),
        ),
        #[cfg(feature = "sqlite")]
        Some("sqlite") => Pool::from(
            rustango::sql::sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(max)
                .acquire_timeout(timeout)
                .connect(url)
                .await
                .expect("connect"),
        ),
        _ => unreachable!("{url}"),
    }
}

#[cfg(feature = "postgres")]
async fn sqlx_pg(url: &str, max: u32, timeout: std::time::Duration) -> Pool {
    Pool::from(
        rustango::sql::sqlx::postgres::PgPoolOptions::new()
            .max_connections(max)
            .acquire_timeout(timeout)
            .connect(url)
            .await
            .expect("connect"),
    )
}

/// A one-connection pool migrates: the lock no longer takes its only connection.
async fn one_connection_pool_migrates(pool: &Pool) {
    let t = "mr_one_item";
    let ledger = "mr_one_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let db = tempfile::tempdir().expect("tempdir");
    let one = pool_of(&url_of(pool, &db.path().join("one.db")), 1).await;
    make_migrations_from(dir.path(), &snap(vec![table(t, vec![id()])]), None)
        .expect("makemigrations")
        .expect("a migration");
    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        migrate_pool_with_ledger(&one, dir.path(), ledger),
    )
    .await
    .expect("no hang");
    ran.unwrap_or_else(|e| panic!("one connection on {}: {e}", pool.dialect().name()));
    drop_table(&one, t).await;
    drop_table(&one, ledger).await;
}

/// Two runs at once apply a non-atomic migration once, SQLite included.
async fn concurrent_runs_apply_once(pool: &Pool) {
    let t = "mr_cc_counter";
    let ledger = "mr_cc_ledger";
    let dir = fresh(pool, &[t], ledger).await;
    let db = tempfile::tempdir().expect("tempdir");
    let url = url_of(pool, &db.path().join("cc.db"));
    let (p1, p2) = (pool_of(&url, 5).await, pool_of(&url, 5).await);
    let initial = make_migrations_from(
        dir.path(),
        &snap(vec![table(t, vec![id(), col("n", true)])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    migrate_pool_with_ledger(&p1, dir.path(), ledger)
        .await
        .expect("initial");
    let q = pool.dialect().quote_ident(t);
    write(
        dir.path(),
        &Migration {
            name: "0002_count".into(),
            prev: Some(initial.name.clone()),
            atomic: false,
            forward: vec![
                Operation::Callback(CallbackOp {
                    name: "mr_slow".into(),
                    reverse_name: None,
                }),
                Operation::Data(DataOp {
                    sql: format!("INSERT INTO {q} (n) VALUES (1)"),
                    reverse_sql: None,
                    reversible: false,
                }),
            ],
            ..initial
        },
    );
    let (r1, r2) = tokio::join!(
        migrate_pool_with_ledger(&p1, dir.path(), ledger),
        migrate_pool_with_ledger(&p2, dir.path(), ledger),
    );
    let name = pool.dialect().name();
    let n: Vec<(i64,)> =
        rustango::sql::raw_query_pool(&format!("SELECT COUNT(*) FROM {q}"), Vec::new(), &p1)
            .await
            .expect("count");
    assert_eq!(n[0].0, 1, "the RunSQL ran once on {name}");
    let applied = [r1, r2].map(|r| r.unwrap_or_else(|e| panic!("a run on {name}: {e}")));
    assert_eq!(
        applied.iter().map(Vec::len).sum::<usize>(),
        1,
        "{applied:?}"
    );
    drop_table(&p1, t).await;
    drop_table(&p1, ledger).await;
}

/// A SQLite database whose directory refuses the lock file still migrates.
#[cfg(all(feature = "sqlite", unix))]
#[tokio::test]
async fn sqlite_without_a_writable_dir_migrates_unlocked() {
    use std::os::unix::fs::PermissionsExt as _;
    let db = tempfile::tempdir().expect("tempdir");
    let path = db.path().join("ro.db");
    let dir = tempfile::tempdir().expect("tempdir");
    make_migrations_from(
        dir.path(),
        &snap(vec![table("mr_ro_item", vec![id()])]),
        None,
    )
    .expect("makemigrations")
    .expect("a migration");
    let rw = pool_of(&format!("sqlite://{}?mode=rwc", path.display()), 1).await;
    migrate_pool_with_ledger(&rw, dir.path(), "mr_ro_ledger")
        .await
        .expect("initial");
    drop(rw);
    let lock = format!("{}-migrate.lock", path.display());
    let _ = std::fs::remove_file(&lock);
    let mode = |m| std::fs::set_permissions(db.path(), std::fs::Permissions::from_mode(m));
    mode(0o555).expect("read-only dir");
    let ro = pool_of(&format!("sqlite://{}?mode=ro", path.display()), 1).await;
    let ran = migrate_pool_with_ledger(&ro, dir.path(), "mr_ro_ledger").await;
    mode(0o755).expect("writable again");
    assert!(ran
        .expect("nothing pending runs without the lock")
        .is_empty());
}

/// A lock connection to a server that never answers gives up like the pool would.
#[cfg(any(feature = "postgres", feature = "mysql"))]
#[tokio::test]
async fn silent_server_fails_the_lock_in_time() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut urls = Vec::new();
    #[cfg(feature = "postgres")]
    urls.push(format!("postgres://u:p@127.0.0.1:{port}/db"));
    #[cfg(feature = "mysql")]
    urls.push(format!("mysql://u:p@127.0.0.1:{port}/db"));
    for url in urls {
        let timeout = std::time::Duration::from_secs(1);
        let pool: Pool = match url.split(':').next() {
            #[cfg(feature = "postgres")]
            Some("postgres") => rustango::sql::sqlx::postgres::PgPoolOptions::new()
                .acquire_timeout(timeout)
                .connect_lazy(&url)
                .expect("lazy pool")
                .into(),
            #[cfg(feature = "mysql")]
            Some("mysql") => rustango::sql::sqlx::mysql::MySqlPoolOptions::new()
                .acquire_timeout(timeout)
                .connect_lazy(&url)
                .expect("lazy pool")
                .into(),
            _ => unreachable!(),
        };
        let ran = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            migrate_pool_with_ledger(&pool, dir.path(), "mr_bh_ledger"),
        )
        .await
        .unwrap_or_else(|_| panic!("the lock connect hung on {url}"));
        assert!(ran.is_err(), "{url}");
    }
    drop(listener);
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        long_index_names_apply,
        referenced_key_widens,
        referenced_key_widens_beside_new_fks,
        referenced_key_widens_its_junction,
        rollback_after_rename_restores_check_and_partial_index,
        rollback_of_a_renamed_dropped_table_restores_its_check_once,
        unapplying_one_branch_keeps_the_others_column,
        applying_a_branch_keeps_the_others_column,
        refused_downgrade_runs_nothing,
        refused_loose_downgrade_runs_nothing,
        unique_key_over_duplicates_is_refused,
        one_connection_pool_migrates,
        concurrent_runs_apply_once,
    ]
);
