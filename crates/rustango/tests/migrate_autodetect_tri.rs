//! `makemigrations` → `migrate` on every backend, for the autodetector
//! fixes in #1877–#1881.
//!
//! Each scenario writes real migration files from two snapshots and
//! applies them with the runner, so the ops, their order and their DDL
//! all reach a real server. The tables exist only as snapshots, so the
//! probes below are literal-only statements, quoted by the dialect.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::migrate::{
    make_migrations_from, migrate_pool_with_ledger, render_changes_split_with_dialect,
    SchemaChange, SchemaSnapshot,
};
use rustango::sql::{raw_execute_pool, Pool};
use rustango::testkit::matrix::drop_table;
use rustango::{by_dialect, tri_dialect_test};
use serde_json::{json, Value};

async fn no_setup(_pool: &Pool) {}

fn id() -> Value {
    json!({"name": "id", "column": "id", "ty": "i64", "nullable": false,
           "primary_key": true, "auto": true})
}

/// A nullable, non-PK column; `extra` overrides any key.
fn col(name: &str, ty: &str, extra: Value) -> Value {
    let mut f = json!({"name": name, "column": name, "ty": ty, "nullable": true,
                       "primary_key": false});
    if let Value::Object(m) = extra {
        f.as_object_mut().unwrap().extend(m);
    }
    f
}

fn fk(to: &str) -> Value {
    json!({"fk": {"kind": "fk", "to": to, "on": "id", "on_delete": "CASCADE"}})
}

fn table(name: &str, fields: Vec<Value>) -> Value {
    json!({"name": name, "model": name, "fields": fields})
}

fn snap(v: Value) -> SchemaSnapshot {
    serde_json::from_value(v).expect("snapshot JSON")
}

/// A migrations dir and a ledger of its own, over tables dropped first.
struct Chain {
    dir: tempfile::TempDir,
    ledger: String,
}

impl Chain {
    /// `tables` child-first, so a persistent server drops them cleanly.
    async fn new(pool: &Pool, tag: &str, tables: &[&str]) -> Self {
        for t in tables {
            drop_table(pool, t).await;
        }
        let ledger = format!("mad_ledger_{tag}");
        drop_table(pool, &ledger).await;
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            ledger,
        }
    }

    /// `makemigrations` against `current`, then `migrate`.
    async fn step(&self, pool: &Pool, current: Value) -> Result<(), String> {
        make_migrations_from(self.dir.path(), &snap(current), None)
            .map_err(|e| e.to_string())?
            .expect("the snapshot changed, so a migration is written");
        migrate_pool_with_ledger(pool, self.dir.path(), &self.ledger)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// `sql` with each `{}` replaced by the next name, quoted by the dialect.
fn q(pool: &Pool, sql: &str, names: &[&str]) -> String {
    let mut out = sql.to_owned();
    for n in names {
        out = out.replacen("{}", &pool.dialect().quote_ident(n), 1);
    }
    out
}

async fn exec(pool: &Pool, sql: &str, names: &[&str]) -> Result<(), String> {
    raw_execute_pool(pool, &q(pool, sql, names), Vec::new())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Render `changes` for the pool's dialect and run them, as `unapply` would.
async fn apply_ops(pool: &Pool, changes: &[SchemaChange]) -> Result<(), String> {
    let b = render_changes_split_with_dialect(changes, &SchemaSnapshot::default(), pool.dialect())?;
    for sql in b.immediate.iter().chain(&b.deferred_fks) {
        raw_execute_pool(pool, sql, Vec::new())
            .await
            .map_err(|e| format!("{sql}: {e}"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- #1880

/// Dropping `unique` on a table+column too long for a plain
/// `{table}_{column}_key`: PG shortened the inline name, so the DROP
/// used to miss it.
async fn unique_drops_on_long_names(pool: &Pool) {
    let t = "mad_un_subscription_notification_preferences";
    let c = "primary_contact_email_address";
    let chain = Chain::new(pool, "un", &[t]).await;
    let with = |unique: bool| {
        json!({"tables": [table(t, vec![id(),
            col(c, "string", json!({"max_length": 100, "unique": unique}))])]})
    };
    chain
        .step(pool, with(true))
        .await
        .expect("CREATE TABLE with a named UNIQUE");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'a')",
        &[t, "id", c],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'a')",
            &[t, "id", c]
        )
        .await
        .is_err(),
        "the column is UNIQUE on {}",
        pool.dialect().name()
    );

    let drop = chain.step(pool, with(false)).await;
    let runs = by_dialect! { pool,
        postgres => true, because "AlterColumnUnique renders on Postgres",
        mysql => false, because "AlterColumn* is refused at render until #559",
        sqlite => false, because "AlterColumn* is refused at render until #559",
    };
    if !runs.value {
        let err = drop.expect_err(runs.why);
        assert!(err.contains("AlterColumnUnique"), "{err}");
        return;
    }
    drop.expect("dropping UNIQUE finds the constraint by name");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (2, 'a')",
        &[t, "id", c],
    )
    .await
    .expect("no longer unique");
}

// ---------------------------------------------------------------- #1877

/// A new FK column references its target and a new unique column is
/// unique. Both used to land as a bare `ADD COLUMN`.
async fn add_column_keeps_fk_and_unique(pool: &Pool) {
    let (a, b) = ("mad_ac_author", "mad_ac_book");
    let chain = Chain::new(pool, "ac", &[b, a]).await;
    chain
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), table(b, vec![id()])]}),
        )
        .await
        .expect("initial");
    chain
        .step(
            pool,
            json!({"tables": [
                table(a, vec![id()]),
                table(b, vec![id(),
                    col("author_id", "i64", fk(a)),
                    col("email", "string", json!({"max_length": 64, "unique": true}))]),
            ]}),
        )
        .await
        .expect("AddColumn applies");

    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 'x')",
        &[b, "id", "author_id", "email"],
    )
    .await
    .expect("a valid row");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'x')",
            &[b, "id", "email"]
        )
        .await
        .is_err(),
        "the added column is UNIQUE on {}",
        pool.dialect().name()
    );
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (3, 99)",
            &[b, "id", "author_id"]
        )
        .await
        .is_err(),
        "the added column REFERENCES its target on {}",
        pool.dialect().name()
    );

    // The rollback: unapply inverts each AddColumn to a DropColumn.
    let drop = |c: &str| SchemaChange::DropColumn {
        table: b.into(),
        column: c.into(),
    };
    let undo = by_dialect! { pool,
        postgres => vec![drop("email"), drop("author_id")],
            because "DROP COLUMN takes its constraints with it",
        mysql => vec![drop("email")],
            because "MySQL refuses to drop a column an FK still uses (1828), a separate gap",
        sqlite => vec![drop("email"), drop("author_id")],
            because "the unique index is dropped first, or SQLite refuses the column",
    };
    apply_ops(pool, &undo.value).await.expect(undo.why);
}

// ---------------------------------------------------------------- #1879

/// Dropping a column drops its index and CHECK first. SQLite refused
/// the column; MySQL took the index with it and then failed the drop.
async fn column_drops_after_its_index_and_check(pool: &Pool) {
    let t = "mad_dc_item";
    let chain = Chain::new(pool, "dc", &[t]).await;
    let checks = by_dialect! { pool,
        postgres => true, because "ALTER TABLE ADD CONSTRAINT CHECK is native",
        mysql => true, because "MySQL 8.0.16+ has CHECK; DROP COLUMN takes it along",
        sqlite => false, because "SQLite cannot add a CHECK to a table (#559)",
    };
    let checks_json = if checks.value {
        json!([{"name": "mad_dc_ck", "table": t, "expr": "p >= 0"}])
    } else {
        json!([])
    };
    chain
        .step(
            pool,
            json!({
                "tables": [table(t, vec![id(), col("a", "i64", json!({})), col("p", "i64", json!({}))])],
                "indexes": [{"name": "mad_dc_a_idx", "table": t, "columns": ["a"], "unique": false}],
                "checks": checks_json,
            }),
        )
        .await
        .expect("initial");
    chain
        .step(pool, json!({"tables": [table(t, vec![id()])]}))
        .await
        .expect("the index and check drop before their columns");
}

/// Tables drop child first. Name order dropped `author` before `book`,
/// which MySQL refuses (3730).
async fn tables_drop_child_first(pool: &Pool) {
    let (author, book) = ("mad_dt_author", "mad_dt_book");
    let chain = Chain::new(pool, "dt", &[book, author]).await;
    chain
        .step(
            pool,
            json!({"tables": [
                table(author, vec![id()]),
                table(book, vec![id(), col("author_id", "i64", fk(author))]),
            ]}),
        )
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[author, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[book, "id", "author_id"],
    )
    .await
    .unwrap();
    chain
        .step(pool, json!({"tables": []}))
        .await
        .expect("book drops before author");
}

/// An M2M junction drops before the tables it joins.
async fn m2m_drops_before_its_tables(pool: &Pool) {
    let (post, tag, through) = ("mad_dm_post", "mad_dm_tag", "mad_dm_post_tags");
    let chain = Chain::new(pool, "dm", &[through, post, tag]).await;
    chain
        .step(
            pool,
            json!({
                "tables": [table(post, vec![id()]), table(tag, vec![id()])],
                "m2m_tables": [{"through": through, "src_table": post, "src_col": "post_id",
                                "dst_table": tag, "dst_col": "tag_id"}],
            }),
        )
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[post, "id"])
        .await
        .unwrap();
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[tag, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[through, "post_id", "tag_id"],
    )
    .await
    .unwrap();
    chain
        .step(pool, json!({"tables": []}))
        .await
        .expect("the junction drops first");
}

/// A composite FK drops before the table it references and before that
/// table's unique index.
async fn composite_fk_drops_before_its_parent(pool: &Pool) {
    let (parent, child) = ("mad_dx_parent", "mad_dx_child");
    let chain = Chain::new(pool, "dx", &[child, parent]).await;
    let runs = by_dialect! { pool,
        postgres => true, because "composite FKs are added by ALTER TABLE",
        mysql => true, because "composite FKs are added by ALTER TABLE; 1553/3730 if misordered",
        sqlite => false, because "SQLite cannot add a composite FK to a table (#559)",
    };
    if !runs.value {
        return;
    }
    let ab = || vec![id(), col("a", "i64", json!({})), col("b", "i64", json!({}))];
    let kid = |with_fk: bool| {
        let mut t = table(child, ab());
        if with_fk {
            t["composite_fks"] =
                json!([{"name": "mad_dx_fk", "to": parent, "from": ["a", "b"], "on": ["a", "b"]}]);
        }
        t
    };
    let uq = json!([{"name": "mad_dx_ab_uq", "table": parent, "columns": ["a", "b"],
                     "unique": true}]);
    chain
        .step(
            pool,
            json!({"tables": [table(parent, ab()), kid(false)], "indexes": uq}),
        )
        .await
        .expect("initial");
    // A composite FK on a new table is emitted twice (a separate bug), so
    // it is added to the existing one.
    chain
        .step(
            pool,
            json!({"tables": [table(parent, ab()), kid(true)], "indexes": uq}),
        )
        .await
        .expect("composite FK");
    chain
        .step(pool, json!({"tables": [kid(false)]}))
        .await
        .expect("the FK drops before its index and table");
}

// ---------------------------------------------------------------- #1878

/// `String(max_length = 50)` → `i32` ends as an integer column. The
/// length change used to run after the type change and put TEXT back.
async fn type_change_is_not_undone_by_max_length(pool: &Pool) {
    let t = "mad_tc_item";
    let chain = Chain::new(pool, "tc", &[t]).await;
    let c = |ty: &str, extra: Value| json!({"tables": [table(t, vec![id(), col("c", ty, extra)])]});
    chain
        .step(pool, c("string", json!({"max_length": 50})))
        .await
        .expect("initial");
    let alter = chain.step(pool, c("i32", json!({}))).await;
    let runs = by_dialect! { pool,
        postgres => true, because "AlterColumnType renders on Postgres",
        mysql => false, because "AlterColumn* is refused at render until #559",
        sqlite => false, because "AlterColumn* is refused at render until #559",
    };
    if !runs.value {
        let err = alter.expect_err(runs.why);
        assert!(err.contains("is not yet supported on dialect"), "{err}");
        return;
    }
    alter.expect("the type change applies");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (1, 'abc')",
            &[t, "id", "c"]
        )
        .await
        .is_err(),
        "the column is an integer, so text is refused"
    );
}

/// Shrinking `max_length` over longer values fails instead of
/// truncating them: `USING c::VARCHAR(n)` cut `abcdefghij` to `abc`.
async fn shrinking_max_length_refuses_to_truncate(pool: &Pool) {
    let t = "mad_sh_item";
    let chain = Chain::new(pool, "sh", &[t]).await;
    let c = |n: u32| json!({"tables": [table(t, vec![id(), col("c", "string", json!({"max_length": n}))])]});
    chain.step(pool, c(10)).await.expect("initial");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'abcdefghij')",
        &[t, "id", "c"],
    )
    .await
    .unwrap();
    let shrink = chain.step(pool, c(3)).await;
    let refuses = by_dialect! { pool,
        postgres => true, because "without USING, PG refuses a value too long for the new type",
        mysql => true, because "AlterColumnMaxLength is refused at render until #559",
        sqlite => false, because "SQLite never enforces VARCHAR length, so the change is a no-op",
    };
    if !refuses.value {
        shrink.expect(refuses.why);
        return;
    }
    let err = shrink.expect_err(refuses.why);
    assert!(
        err.contains("too long") || err.contains("AlterColumnMaxLength"),
        "{err}"
    );
}

// ---------------------------------------------------------------- #1881

/// A CHECK whose expression changes under the same name is replaced.
async fn edited_check_is_replaced(pool: &Pool) {
    let t = "mad_ck_item";
    let chain = Chain::new(pool, "ck", &[t]).await;
    let runs = by_dialect! { pool,
        postgres => true, because "ALTER TABLE ADD/DROP CONSTRAINT CHECK is native",
        mysql => true, because "MySQL 8.0.16+ enforces CHECK and drops it with DROP CHECK",
        sqlite => false, because "SQLite cannot add a CHECK to a table (#559)",
    };
    if !runs.value {
        return;
    }
    let with = |expr: &str| {
        json!({"tables": [table(t, vec![id(), col("price", "i64", json!({}))])],
               "checks": [{"name": "mad_ck_price", "table": t, "expr": expr}]})
    };
    chain.step(pool, with("price >= 0")).await.expect("initial");
    chain
        .step(pool, with("price > 0"))
        .await
        .expect("the edit applies");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (1, 0)",
            &[t, "id", "price"]
        )
        .await
        .is_err(),
        "the new CHECK refuses 0"
    );
}

/// A composite FK that points elsewhere under the same name is replaced.
async fn edited_composite_fk_is_replaced(pool: &Pool) {
    let (p1, p2, child) = ("mad_cf_parent1", "mad_cf_parent2", "mad_cf_child");
    let chain = Chain::new(pool, "cf", &[child, p1, p2]).await;
    let runs = by_dialect! { pool,
        postgres => true, because "composite FKs are added by ALTER TABLE",
        mysql => true, because "composite FKs are added by ALTER TABLE",
        sqlite => false, because "SQLite cannot add a composite FK to a table (#559)",
    };
    if !runs.value {
        return;
    }
    let ab = || vec![id(), col("a", "i64", json!({})), col("b", "i64", json!({}))];
    let with = |to: Option<&str>| {
        let mut c = table(child, ab());
        if let Some(to) = to {
            c["composite_fks"] =
                json!([{"name": "mad_cf_fk", "to": to, "from": ["a", "b"], "on": ["a", "b"]}]);
        }
        json!({
            "tables": [table(p1, ab()), table(p2, ab()), c],
            "indexes": [
                {"name": "mad_cf_p1_uq", "table": p1, "columns": ["a", "b"], "unique": true},
                {"name": "mad_cf_p2_uq", "table": p2, "columns": ["a", "b"], "unique": true},
            ],
        })
    };
    chain.step(pool, with(None)).await.expect("initial");
    chain
        .step(pool, with(Some(p1)))
        .await
        .expect("composite FK");
    chain
        .step(pool, with(Some(p2)))
        .await
        .expect("the edit applies");
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 2)",
        &[p2, "id", "a", "b"],
    )
    .await
    .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 2)",
        &[child, "id", "a", "b"],
    )
    .await
    .expect("the FK now points at parent2");
}

/// An M2M junction whose target changes under the same name is rebuilt.
async fn edited_m2m_is_replaced(pool: &Pool) {
    let (post, tag, tag2, through) = (
        "mad_mm_post",
        "mad_mm_tag",
        "mad_mm_tag2",
        "mad_mm_post_tags",
    );
    let chain = Chain::new(pool, "mm", &[through, post, tag, tag2]).await;
    let with = |dst: &str| {
        json!({
            "tables": [table(post, vec![id()]), table(tag, vec![id()]), table(tag2, vec![id()])],
            "m2m_tables": [{"through": through, "src_table": post, "src_col": "post_id",
                            "dst_table": dst, "dst_col": "tag_id"}],
        })
    };
    chain.step(pool, with(tag)).await.expect("initial");
    chain
        .step(pool, with(tag2))
        .await
        .expect("the edit applies");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[post, "id"])
        .await
        .unwrap();
    exec(pool, "INSERT INTO {} ({}) VALUES (7)", &[tag2, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 7)",
        &[through, "post_id", "tag_id"],
    )
    .await
    .expect("the junction now references tag2");
}

/// A PG EXCLUDE whose predicate changes under the same name is replaced.
async fn edited_exclude_is_replaced(pool: &Pool) {
    let t = "mad_ex_booking";
    let chain = Chain::new(pool, "ex", &[t]).await;
    let pg = by_dialect! { pool,
        postgres => true, because "EXCLUDE is Postgres-only",
        mysql => false, because "EXCLUDE renders nothing and warns",
        sqlite => false, because "EXCLUDE renders nothing and warns",
    };
    let with = |pred: Option<&str>| {
        json!({"tables": [table(t, vec![id(), col("during", "range_datetime", json!({}))])],
               "excludes": [{"name": "mad_ex_no_overlap", "table": t, "using": "gist",
                             "elements": [["during", "&&"]], "where_clause": pred}]})
    };
    chain.step(pool, with(None)).await.expect("initial");
    chain
        .step(pool, with(Some("id > 100")))
        .await
        .expect("the edit applies");
    if !pg.value {
        return;
    }
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, '[2026-01-01,2026-01-02)')"),
            &[t, "id", "during"],
        )
        .await
        .expect("the new predicate leaves low ids unconstrained");
    }
}

/// `Option<T>` → `T` with a default fills the NULLs before SET NOT NULL.
async fn not_null_with_default_backfills(pool: &Pool) {
    let t = "mad_nn_item";
    let chain = Chain::new(pool, "nn", &[t]).await;
    chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(), col("n", "i64", json!({}))])]}),
        )
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[t, "id"])
        .await
        .unwrap();
    let alter = chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(),
                col("n", "i64", json!({"nullable": false, "default": "0"}))])]}),
        )
        .await;
    let runs = by_dialect! { pool,
        postgres => true, because "AlterColumnNullable renders on Postgres",
        mysql => false, because "AlterColumn* is refused at render until #559",
        sqlite => false, because "AlterColumn* is refused at render until #559",
    };
    if !runs.value {
        assert!(alter
            .expect_err(runs.why)
            .contains("is not yet supported on dialect"));
        return;
    }
    alter.expect("the NULL row is backfilled first");
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        unique_drops_on_long_names,
        add_column_keeps_fk_and_unique,
        column_drops_after_its_index_and_check,
        tables_drop_child_first,
        m2m_drops_before_its_tables,
        composite_fk_drops_before_its_parent,
        type_change_is_not_undone_by_max_length,
        shrinking_max_length_refuses_to_truncate,
        edited_check_is_replaced,
        edited_composite_fk_is_replaced,
        edited_m2m_is_replaced,
        edited_exclude_is_replaced,
        not_null_with_default_backfills,
    ]
);
