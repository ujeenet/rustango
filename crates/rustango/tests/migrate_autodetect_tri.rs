//! `makemigrations` → `migrate` on every backend, for the autodetector
//! fixes in #1877–#1881.
//!
//! Each scenario writes real migration files from two snapshots and
//! applies them with the runner, so the ops, their order and their DDL
//! all reach a real server. The tables exist only as snapshots, so the
//! probes below are literal-only statements, quoted by the dialect.

#![cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]

use rustango::migrate::{
    make_migrations_from, migrate_pool_with_ledger, unapply_pool_with_ledger, Migration, Operation,
    SchemaChange, SchemaSnapshot,
};
use rustango::sql::{raw_execute_pool, Pool};
use rustango::testkit::matrix::drop_table;
use rustango::{by_dialect, tri_dialect_test};
use serde_json::{json, Value};

async fn no_setup(_pool: &Pool) {}

#[derive(rustango::Model)]
#[rustango(table = "mad_sdm_parent")]
#[allow(dead_code)]
pub struct SdParent {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(rustango::Model)]
#[rustango(table = "mad_sdm_child")]
#[allow(dead_code)]
pub struct SdChild {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(
        fk = "mad_sdm_parent",
        on = "id",
        on_delete = "set_default",
        default = "0"
    )]
    pub parent_id: i64,
}

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

    /// `makemigrations` against `current`, then `migrate`; the new name.
    async fn step(&self, pool: &Pool, current: Value) -> Result<String, String> {
        self.step_with(pool, current, true).await
    }

    /// As [`Self::step`], with the new migration's `atomic` set.
    async fn step_with(&self, pool: &Pool, current: Value, atomic: bool) -> Result<String, String> {
        let mut mig = make_migrations_from(self.dir.path(), &snap(current), None)
            .map_err(|e| e.to_string())?
            .expect("the snapshot changed, so a migration is written");
        if !atomic {
            mig.atomic = false;
            self.write(&mig);
        }
        self.migrate(pool).await.map(|()| mig.name)
    }

    /// Write `mig` into the chain, as a hand edit would.
    fn write(&self, mig: &Migration) {
        let path = self.dir.path().join(format!("{}.json", mig.name));
        rustango::migrate::file::write(&path, mig).expect("write migration");
    }

    /// The chain's newest migration.
    fn head(&self) -> Migration {
        rustango::migrate::file::list_dir(self.dir.path())
            .expect("list")
            .pop()
            .expect("a migration")
    }

    async fn migrate(&self, pool: &Pool) -> Result<(), String> {
        migrate_pool_with_ledger(pool, self.dir.path(), &self.ledger)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// `migrate` back past the head migration `name`.
    async fn undo(&self, pool: &Pool, name: &str) -> Result<(), String> {
        unapply_pool_with_ledger(pool, self.dir.path(), name, &self.ledger)
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

    // PG drops it by name, MySQL by its catalog name, SQLite rebuilds (#1676).
    chain
        .step(pool, with(false))
        .await
        .expect("dropping UNIQUE finds the constraint");
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
    let added = chain
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

    // MySQL refused to drop the FK column (1828) (#1981).
    chain
        .undo(pool, &added)
        .await
        .expect("unapply drops the FK and unique columns");
}

/// Dropping an FK column drops its constraint first; MySQL refused (1828).
async fn fk_column_drops(pool: &Pool) {
    let (a, b) = ("mad_fd_author", "mad_fd_book");
    let chain = Chain::new(pool, "fd", &[b, a]).await;
    let with = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain
        .step(pool, with(vec![id(), col("author_id", "i64", fk(a))]))
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[b, "id", "author_id"],
    )
    .await
    .unwrap();
    chain
        .step(pool, with(vec![id()]))
        .await
        .expect("DropColumn of an FK column applies");
}

/// A `now()` column added to a table with rows, forward and by unapply.
/// SQLite refuses a non-constant DEFAULT there (#2017).
async fn now_column_adds_to_a_filled_table(pool: &Pool) {
    now_column_adds(pool, "mad_nw_item", true).await;
}

/// As above through the non-atomic runners.
async fn now_column_adds_without_a_transaction(pool: &Pool) {
    now_column_adds(pool, "mad_nwn_item", false).await;
}

async fn now_column_adds(pool: &Pool, t: &str, atomic: bool) {
    let chain = Chain::new(pool, t, &[t]).await;
    let stamp = col(
        "created_at",
        "datetime",
        json!({"nullable": false, "default": "now()"}),
    );
    chain
        .step(pool, json!({"tables": [table(t, vec![id()])]}))
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[t, "id"])
        .await
        .unwrap();
    chain
        .step_with(
            pool,
            json!({"tables": [table(t, vec![id(), stamp])]}),
            atomic,
        )
        .await
        .expect("AddColumn with now() on a table with rows");
    let dropped = chain
        .step_with(pool, json!({"tables": [table(t, vec![id()])]}), atomic)
        .await
        .expect("DropColumn");
    chain
        .undo(pool, &dropped)
        .await
        .expect("unapply re-adds the now() column");
    exec(pool, "INSERT INTO {} ({}) VALUES (2)", &[t, "id"])
        .await
        .expect("the column still has a default");
    let sql = q(
        pool,
        "SELECT COUNT(*) FROM {} WHERE {} IS NULL",
        &[t, "created_at"],
    );
    let nulls: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(nulls, vec![(0,)], "every row has a timestamp");
}

/// A UUID column with a DB DEFAULT, added to a table with rows and to an
/// empty one. MySQL refused it on both (1674), SQLite on the first.
async fn uuid_column_adds_to_a_filled_table(pool: &Pool) {
    let (full, empty) = ("mad_uu_full", "mad_uu_empty");
    let chain = Chain::new(pool, "uu", &[full, empty]).await;
    let token = col(
        "token",
        "uuid",
        json!({"nullable": false, "default": "gen_random_uuid()"}),
    );
    let tables = |f: Vec<Value>| json!({"tables": [table(full, f.clone()), table(empty, f)]});
    chain.step(pool, tables(vec![id()])).await.expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1), (2)", &[full, "id"])
        .await
        .unwrap();
    chain
        .step(pool, tables(vec![id(), token]))
        .await
        .expect("AddColumn with a UUID DEFAULT");
    for t in [full, empty] {
        exec(pool, "INSERT INTO {} ({}) VALUES (3)", &[t, "id"])
            .await
            .expect("the column has a default, or is nullable");
    }
    let count = |t: &str| {
        q(
            pool,
            "SELECT COUNT(DISTINCT {}), COUNT(*) FROM {}",
            &["token", t],
        )
    };
    let full_ids: Vec<(i64, i64)> = rustango::sql::raw_query_pool(&count(full), Vec::new(), pool)
        .await
        .unwrap();
    let filled = by_dialect! { pool,
        postgres => 3, because "ADD COLUMN fills each row; the DEFAULT stays",
        mysql => 3, because "rows backfilled, then the DEFAULT set by MODIFY",
        sqlite => 2, because "rows backfilled; no DEFAULT can be added to a filled table",
    };
    assert_eq!(full_ids, vec![(filled.value, 3)], "{}", filled.why);
    let empty_ids: Vec<(i64, i64)> = rustango::sql::raw_query_pool(&count(empty), Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(empty_ids, vec![(1, 1)], "the DEFAULT fills an insert");
}

/// The FK of a column dropped after a rename: MySQL looked for the
/// constraint under the new column's name (1091).
async fn renamed_fk_column_drops(pool: &Pool) {
    let (a, b) = ("mad_rn_author", "mad_rn_book");
    let chain = Chain::new(pool, "rn", &[b, a]).await;
    let with = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain
        .step(pool, with(vec![id(), col("author_id", "i64", fk(a))]))
        .await
        .expect("initial");
    let head = chain.head();
    chain.write(&Migration {
        name: "0002_rename".into(),
        prev: Some(head.name),
        forward: vec![Operation::Schema(SchemaChange::RenameColumn {
            table: b.into(),
            old_column: "author_id".into(),
            new_column: "writer_id".into(),
        })],
        snapshot: snap(with(vec![id(), col("writer_id", "i64", fk(a))])),
        ..head
    });
    chain.migrate(pool).await.expect("RenameColumn");
    chain
        .step(pool, with(vec![id()]))
        .await
        .expect("the renamed FK column drops");
}

/// An FK named with 64 bytes, as releases before 0.59.12 wrote it on
/// MySQL; the drop looked for the 63-byte name (1091).
async fn long_named_fk_column_drops(pool: &Pool) {
    let (a, b) = ("mad_ln_author", "mad_ln_book");
    let long = "w".repeat(64 - b.len() - "__fkey".len());
    let chain = Chain::new(pool, "ln", &[b, a]).await;
    let with = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain
        .step(pool, with(vec![id(), col(&long, "i64", fk(a))]))
        .await
        .expect("initial");
    if pool.dialect().name() == "mysql" {
        let short = rustango::migrate::ddl::fk_constraint_name(b, &long);
        let full = format!("{b}_{long}_fkey");
        assert_eq!(full.len(), 64);
        for sql in [
            format!("ALTER TABLE {b} DROP FOREIGN KEY {short}"),
            format!(
                "ALTER TABLE {b} ADD CONSTRAINT {full} FOREIGN KEY ({long}) REFERENCES {a} (id)"
            ),
        ] {
            raw_execute_pool(pool, &sql, Vec::new()).await.unwrap();
        }
    }
    chain
        .step(pool, with(vec![id()]))
        .await
        .expect("the long-named FK column drops");
}

/// A NOT NULL FK column with a default, added to a table with rows.
/// SQLite refuses inline `REFERENCES` beside a non-NULL default there.
async fn add_fk_column_with_default_to_filled_table(pool: &Pool) {
    let (a, b) = ("mad_ad_author", "mad_ad_book");
    let chain = Chain::new(pool, "ad", &[b, a]).await;
    let with = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain.step(pool, with(vec![id()])).await.expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[b, "id"])
        .await
        .unwrap();
    let mut author = fk(a);
    author["nullable"] = json!(false);
    author["default"] = json!("1");
    chain
        .step(pool, with(vec![id(), col("author_id", "i64", author)]))
        .await
        .expect("AddColumn applies on a table with rows");
    let enforced = by_dialect! { pool,
        postgres => true, because "the FK is added by ALTER TABLE",
        mysql => true, because "the FK is added by ALTER TABLE",
        sqlite => false, because "the FK is left out and a warning says so (#559)",
    };
    let bad = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (2, 99)",
        &[b, "id", "author_id"],
    )
    .await;
    assert_eq!(bad.is_err(), enforced.value, "{}", enforced.why);
}

/// FK names over 64 bytes, from CREATE TABLE and from ADD COLUMN. MySQL
/// refused them (1059) after the column had committed.
async fn long_fk_names_apply(pool: &Pool) {
    let (a, b) = (
        "mad_lf_author",
        "mad_lf_subscription_notification_preferences",
    );
    let (c1, c2) = ("primary_contact_author_id", "secondary_contact_author_id");
    let chain = Chain::new(pool, "lf", &[b, a]).await;
    let with = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain
        .step(pool, with(vec![id(), col(c1, "i64", fk(a))]))
        .await
        .expect("CREATE TABLE with a long FK name");
    chain
        .step(
            pool,
            with(vec![id(), col(c1, "i64", fk(a)), col(c2, "i64", fk(a))]),
        )
        .await
        .expect("ADD COLUMN with a long FK name");
    for c in [c1, c2] {
        assert!(
            exec(
                pool,
                "INSERT INTO {} ({}, {}) VALUES (1, 99)",
                &[b, "id", c]
            )
            .await
            .is_err(),
            "{c} REFERENCES its target on {}",
            pool.dialect().name()
        );
    }
}

// ---------------------------------------------------------------- #1879

/// Dropping a column drops its index and CHECK first. SQLite refused
/// the column; MySQL took the index with it and then failed the drop.
async fn column_drops_after_its_index_and_check(pool: &Pool) {
    let t = "mad_dc_item";
    let chain = Chain::new(pool, "dc", &[t]).await;
    // SQLite adds the CHECK by a rebuild (#2127).
    let checks_json = json!([{"name": "mad_dc_ck", "table": t, "expr": "p >= 0"}]);
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
    // A new table's composite FK comes once, with its CREATE (#1983).
    chain
        .step(
            pool,
            json!({"tables": [table(parent, ab()), kid(true)], "indexes": uq}),
        )
        .await
        .expect("a new table with a composite FK");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}, {}) VALUES (1, 9, 9)",
            &[child, "id", "a", "b"]
        )
        .await
        .is_err(),
        "the composite FK holds on {}",
        pool.dialect().name()
    );
    // SQLite drops it by a rebuild (#2127).
    chain
        .step(pool, json!({"tables": [kid(false)]}))
        .await
        .expect("the FK drops before its index and table");
    // With the FK left, SQLite would refuse this: its parent table is gone.
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (2, 9, 9)",
        &[child, "id", "a", "b"],
    )
    .await
    .expect("the composite FK is gone");
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
    chain
        .step(pool, c("i32", json!({})))
        .await
        .expect("the type change applies");
    let refused = by_dialect! { pool,
        postgres => true, because "an integer column refuses text",
        mysql => true, because "strict mode refuses text in an integer column",
        sqlite => false, because "INTEGER affinity keeps text it cannot convert",
    };
    let text = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'abc')",
        &[t, "id", "c"],
    )
    .await;
    assert_eq!(text.is_err(), refused.value, "{}", refused.why);
    let ints = by_dialect! { pool,
        postgres => true, because "the column is an integer",
        mysql => true, because "the column is an integer",
        sqlite => true, because "the rebuilt column has INTEGER affinity",
    };
    exec(pool, "DELETE FROM {}", &[t]).await.unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, '42')",
        &[t, "id", "c"],
    )
    .await
    .unwrap();
    let sql = q(pool, "SELECT {} FROM {}", &["c", t]);
    let got: Vec<(i32,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got == [(42,)], ints.value, "{}", ints.why);
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
        postgres => Some("too long"),
            because "without USING, PG refuses a value too long for the new type",
        mysql => Some("truncated"),
            because "strict mode refuses to truncate in MODIFY COLUMN",
        sqlite => None, because "SQLite never enforces VARCHAR length, so the change is a no-op",
    };
    let Some(expected) = refuses.value else {
        shrink.expect(refuses.why);
        return;
    };
    let err = shrink.expect_err(refuses.why);
    assert!(err.contains(expected), "{}: {err}", refuses.why);
}

/// `i32` → `String(max_length = 5)` ends as VARCHAR(5), not TEXT.
async fn type_change_into_a_string_keeps_its_length(pool: &Pool) {
    let t = "mad_ts_item";
    let chain = Chain::new(pool, "ts", &[t]).await;
    let c = |ty: &str, extra: Value| json!({"tables": [table(t, vec![id(), col("c", ty, extra)])]});
    chain
        .step(pool, c("i32", json!({})))
        .await
        .expect("initial");
    chain
        .step(pool, c("string", json!({"max_length": 5})))
        .await
        .expect("the type change applies");
    let enforced = by_dialect! { pool,
        postgres => true, because "the column is VARCHAR(5)",
        mysql => true, because "the column is VARCHAR(5)",
        sqlite => false, because "SQLite never enforces VARCHAR length",
    };
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'abcde')",
        &[t, "id", "c"],
    )
    .await
    .expect("five characters fit");
    assert_eq!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'abcdef')",
            &[t, "id", "c"]
        )
        .await
        .is_err(),
        enforced.value,
        "{}",
        enforced.why
    );
}

// ---------------------------------------------------------------- #1881

/// A CHECK whose expression changes under the same name is replaced.
async fn edited_check_is_replaced(pool: &Pool) {
    let t = "mad_ck_item";
    let chain = Chain::new(pool, "ck", &[t]).await;
    let with_default = |expr: &str, default: Value| {
        json!({"tables": [table(t, vec![id(), col("price", "i64", default)])],
               "checks": [{"name": "mad_ck_price", "table": t, "expr": expr}]})
    };
    let with = |expr: &str| with_default(expr, json!({}));
    chain.step(pool, with("price >= 0")).await.expect("initial");
    chain
        .step(pool, with("price > 0"))
        .await
        .expect("the edit applies");
    // SQLite rebuilds the table for this; the CHECK must survive it.
    chain
        .step(pool, with_default("price > 0", json!({"default": "1"})))
        .await
        .expect("a later column change");
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
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 5, 5)",
        &[p1, "id", "a", "b"],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}, {}) VALUES (2, 5, 5)",
            &[child, "id", "a", "b"],
        )
        .await
        .is_err(),
        "the new FK exists and refuses a row only parent1 has"
    );
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
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (101, '[2026-01-01,2026-01-02)')",
        &[t, "id", "during"],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (102, '[2026-01-01,2026-01-02)')",
            &[t, "id", "during"],
        )
        .await
        .is_err(),
        "the new EXCLUDE exists and refuses an overlap among high ids"
    );
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
    alter.expect("the NULL row is backfilled first");
    let sql = q(pool, "SELECT {} FROM {}", &["n", t]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(0,)], "the NULL became the default");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, NULL)",
            &[t, "id", "n"]
        )
        .await
        .is_err(),
        "the column is NOT NULL on {}",
        pool.dialect().name()
    );
}

// ---------------------------------------------------------------- #1557

/// `SELECT COUNT(*)` of `t`.
async fn rows(pool: &Pool, t: &str) -> i64 {
    let sql = q(pool, "SELECT COUNT(*) FROM {}", &[t]);
    let n: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    n[0].0
}

/// A changed `on_delete` reaches a table that already exists, which
/// keeps its rows, its index and the cascading FK into it.
async fn on_delete_reaches_an_existing_table(pool: &Pool) {
    on_delete_reaches(pool, "mad_od", true).await;
}

/// No `on_delete` and an explicit `NO ACTION` are one schema: neither way
/// writes a migration, so SQLite never rebuilds for nothing (#1573).
async fn no_action_is_not_a_change(pool: &Pool) {
    let (a, b) = ("mad_noact_author", "mad_noact_book");
    for (first, second) in [(None, Some("NO ACTION")), (Some("NO ACTION"), None)] {
        let chain = Chain::new(pool, "mad_noact", &[b, a]).await;
        let with = |on_delete: Option<&str>| {
            let mut rel = json!({"kind": "fk", "to": a, "on": "id"});
            if let Some(action) = on_delete {
                rel["on_delete"] = json!(action);
            }
            json!({"tables": [
                table(a, vec![id()]),
                table(b, vec![id(), col("author_id", "i64", json!({"fk": rel}))]),
            ]})
        };
        chain.step(pool, with(first)).await.expect("initial");
        for _ in 0..2 {
            let mig = make_migrations_from(chain.dir.path(), &snap(with(second)), None)
                .expect("makemigrations");
            assert!(mig.is_none(), "{first:?} -> {second:?} wrote {mig:?}");
        }
    }
}

/// `SET DEFAULT` resets the child where the server enforces it, and is
/// refused where InnoDB would accept it and then block the delete (#1573).
async fn set_default_is_enforced_or_refused(pool: &Pool) {
    let (a, b) = ("mad_setdef_author", "mad_setdef_book");
    let chain = Chain::new(pool, "mad_setdef", &[b, a]).await;
    let rel = json!({"kind": "fk", "to": a, "on": "id", "on_delete": "SET DEFAULT"});
    let step = chain
        .step(
            pool,
            json!({"tables": [
                table(a, vec![id()]),
                table(b, vec![id(), col("author_id", "i64",
                    json!({"fk": rel, "default": "0"}))]),
            ]}),
        )
        .await;
    let refused = by_dialect! { pool,
        postgres => false, because "PG enforces SET DEFAULT",
        mysql => true, because "InnoDB records SET DEFAULT but refuses the parent delete",
        sqlite => false, because "SQLite enforces SET DEFAULT",
    };
    if refused.value {
        let err = step.expect_err(refused.why);
        assert!(err.contains("set_default"), "{err}");
        return;
    }
    step.expect(refused.why);
    exec(pool, "INSERT INTO {} ({}) VALUES (0), (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[b, "id", "author_id"],
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM {} WHERE {} = 1", &[a, "id"])
        .await
        .expect(refused.why);
    let sql = q(pool, "SELECT {} FROM {}", &["author_id", b]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(0,)], "{}", refused.why);
}

/// The model-based DDL path refuses it on MySQL too (#2180).
async fn set_default_is_refused_by_create_tables(pool: &Pool) {
    drop_table(pool, "mad_sdm_child").await;
    drop_table(pool, "mad_sdm_parent").await;
    rustango::testkit::create_tables_for::<SdParent>(pool)
        .await
        .expect("parent");
    let r = rustango::testkit::create_tables_for::<SdChild>(pool).await;
    let refused = by_dialect! { pool,
        postgres => false, because "PG enforces SET DEFAULT",
        mysql => true, because "InnoDB records SET DEFAULT but refuses the parent delete",
        sqlite => false, because "SQLite enforces SET DEFAULT",
    };
    match r {
        Err(e) if refused.value => assert!(e.to_string().contains("set_default"), "{e}"),
        r => assert_eq!(r.is_err(), refused.value, "{}: {r:?}", refused.why),
    }
}

/// As above through the non-atomic runners.
async fn on_delete_reaches_without_a_transaction(pool: &Pool) {
    on_delete_reaches(pool, "mad_odn", false).await;
}

async fn on_delete_reaches(pool: &Pool, tag: &str, atomic: bool) {
    let (a, b, c) = (
        format!("{tag}_author"),
        format!("{tag}_book"),
        format!("{tag}_page"),
    );
    let (a, b, c) = (a.as_str(), b.as_str(), c.as_str());
    let idx = format!("{tag}_title_idx");
    let chain = Chain::new(pool, tag, &[c, b, a]).await;
    // `None` is how a snapshot from before #1549 reads.
    let with = |on_delete: Option<&str>| {
        let mut rel = json!({"kind": "fk", "to": a, "on": "id"});
        if let Some(action) = on_delete {
            rel["on_delete"] = json!(action);
        }
        json!({
            "tables": [
                table(a, vec![id()]),
                table(b, vec![id(), col("author_id", "i64", json!({"fk": rel})),
                    col("title", "string", json!({"max_length": 32}))]),
                table(c, vec![id(), col("book_id", "i64", fk(b))]),
            ],
            "indexes": [{"name": idx, "table": b, "columns": ["title"], "unique": false}],
        })
    };
    chain.step(pool, with(None)).await.expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 't')",
        &[b, "id", "author_id", "title"],
    )
    .await
    .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[c, "id", "book_id"],
    )
    .await
    .unwrap();
    assert!(
        exec(pool, "DELETE FROM {}", &[a]).await.is_err(),
        "NO ACTION before"
    );
    // SQLite re-creates the table's triggers from the catalog.
    let trigger = by_dialect! { pool,
        postgres => false, because "no table rebuild",
        mysql => false, because "no table rebuild",
        sqlite => true, because "the rebuild drops and re-creates triggers",
    };
    let trg = format!("{tag}_trg");
    let create_trigger = q(
        pool,
        "CREATE TRIGGER {} AFTER UPDATE ON {} BEGIN SELECT 1; END",
        &[&trg, b],
    );
    if trigger.value {
        raw_execute_pool(pool, &create_trigger, Vec::new())
            .await
            .unwrap();
    }

    let altered = chain
        .step_with(pool, with(Some("CASCADE")), atomic)
        .await
        .expect("AlterFkOnDelete applies to the existing table");
    assert_eq!(
        (rows(pool, b).await, rows(pool, c).await),
        (1, 1),
        "rows kept"
    );
    if trigger.value {
        assert!(
            raw_execute_pool(pool, &create_trigger, Vec::new())
                .await
                .is_err(),
            "{}",
            trigger.why
        );
    }
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 99)",
            &[c, "id", "book_id"]
        )
        .await
        .is_err(),
        "the FK into the table still holds on {}",
        pool.dialect().name()
    );
    assert!(
        exec(pool, "CREATE INDEX {} ON {} ({})", &[&idx, b, "title"])
            .await
            .is_err(),
        "the index is still there on {}",
        pool.dialect().name()
    );
    exec(pool, "DELETE FROM {}", &[a])
        .await
        .expect("CASCADE after");
    assert_eq!(
        (rows(pool, b).await, rows(pool, c).await),
        (0, 0),
        "cascaded"
    );

    chain.undo(pool, &altered).await.expect("unapply");
    exec(pool, "INSERT INTO {} ({}) VALUES (2)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (2, 2, 't')",
        &[b, "id", "author_id", "title"],
    )
    .await
    .unwrap();
    assert!(
        exec(pool, "DELETE FROM {}", &[a]).await.is_err(),
        "NO ACTION again after unapply"
    );
}

/// An action change and a dropped column on one table, in one migration:
/// the first rebuild already drops the column.
async fn on_delete_and_drop_in_one_migration(pool: &Pool) {
    let (a, b) = ("mad_om_author", "mad_om_book");
    let chain = Chain::new(pool, "om", &[b, a]).await;
    let with = |rel: Value, extra: Vec<Value>| {
        let mut fields = vec![id(), col("author_id", "i64", json!({"fk": rel}))];
        fields.extend(extra);
        json!({"tables": [table(a, vec![id()]), table(b, fields)]})
    };
    chain
        .step(
            pool,
            with(
                json!({"kind": "fk", "to": a, "on": "id"}),
                vec![col("old", "i64", json!({}))],
            ),
        )
        .await
        .expect("initial");
    chain
        .step(
            pool,
            with(
                json!({"kind": "fk", "to": a, "on": "id", "on_delete": "CASCADE"}),
                vec![],
            ),
        )
        .await
        .expect("AlterFkOnDelete then DropColumn");
}

/// A rebuild refuses to lose a column the snapshot does not know.
async fn rebuild_keeps_unknown_columns(pool: &Pool) {
    let (a, b) = ("mad_ou_author", "mad_ou_book");
    let chain = Chain::new(pool, "ou", &[b, a]).await;
    let with = |rel: Value| {
        json!({"tables": [table(a, vec![id()]),
            table(b, vec![id(), col("author_id", "i64", json!({"fk": rel}))])]})
    };
    chain
        .step(pool, with(json!({"kind": "fk", "to": a, "on": "id"})))
        .await
        .expect("initial");
    exec(
        pool,
        "ALTER TABLE {} ADD COLUMN {} INTEGER",
        &[b, "by_hand"],
    )
    .await
    .unwrap();
    let altered = chain
        .step(
            pool,
            with(json!({"kind": "fk", "to": a, "on": "id", "on_delete": "CASCADE"})),
        )
        .await;
    let rebuilds = by_dialect! { pool,
        postgres => false, because "the FK is replaced in place",
        mysql => false, because "the FK is replaced in place",
        sqlite => true, because "the table is rebuilt from the snapshot",
    };
    if rebuilds.value {
        assert!(altered.expect_err(rebuilds.why).contains("by_hand"));
    } else {
        altered.expect(rebuilds.why);
    }
}

// ---------------------------------------------------------------- #1982

/// A column in a table-level UNIQUE drops; SQLite refused it. Rows and
/// the AUTOINCREMENT high-water mark survive the rebuild.
async fn unique_column_drops(pool: &Pool) {
    let t = "mad_ud_item";
    let chain = Chain::new(pool, "ud", &[t]).await;
    let n = col("n", "i64", json!({}));
    let code = col("code", "string", json!({"max_length": 16, "unique": true}));
    chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(), code, n.clone()])]}),
        )
        .await
        .expect("initial");
    for (code, n) in [("a", "1"), ("b", "2"), ("c", "3")] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ('{code}', {n})"),
            &[t, "code", "n"],
        )
        .await
        .unwrap();
    }
    exec(pool, "DELETE FROM {} WHERE {} = 3", &[t, "n"])
        .await
        .unwrap();
    chain
        .step(pool, json!({"tables": [table(t, vec![id(), n])]}))
        .await
        .expect("DropColumn of a UNIQUE column applies");
    exec(pool, "INSERT INTO {} ({}) VALUES (4)", &[t, "n"])
        .await
        .unwrap();
    let sql = q(
        pool,
        "SELECT {}, {} FROM {} ORDER BY {}",
        &["id", "n", t, "id"],
    );
    let got: Vec<(i64, i64)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(1, 1), (2, 2), (4, 4)], "rows kept, id 3 not reused");
}

// ---------------------------------------------------------------- #2121

impl Chain {
    /// A hand-edited migration to `after` whose ops are `forward`.
    async fn hand(
        &self,
        pool: &Pool,
        after: Value,
        forward: Vec<SchemaChange>,
        data: Option<&str>,
    ) -> Result<String, String> {
        let mut mig = make_migrations_from(self.dir.path(), &snap(after), None)
            .map_err(|e| e.to_string())?
            .expect("the snapshot changed");
        mig.forward = forward.into_iter().map(Operation::Schema).collect();
        if let Some(sql) = data {
            mig.forward.push(Operation::Data(rustango::migrate::DataOp {
                sql: sql.to_owned(),
                reverse_sql: Some(sql.to_owned()),
                reversible: true,
            }));
        }
        self.write(&mig);
        self.migrate(pool).await.map(|()| mig.name)
    }

    /// Delete the newest migration file, one that failed to apply.
    /// Write a hand-built migration of `forward` ops ending at `current`.
    fn write_hand(&self, current: Value, forward: Vec<Operation>) {
        let prev = self.head();
        let n: u32 = prev.name[..4].parse().expect("a numbered migration");
        self.write(&Migration {
            name: format!("{:04}_hand", n + 1),
            created_at: prev.created_at.clone(),
            prev: Some(prev.name.clone()),
            atomic: true,
            scope: prev.scope,
            replaces: Vec::new(),
            snapshot: snap(current),
            forward,
        });
    }

    fn discard_head(&self) {
        let path = self.dir.path().join(format!("{}.json", self.head().name));
        std::fs::remove_file(path).unwrap();
    }
}

/// The FK is found by its live name, not the rendered one, and a composite
/// FK on the same column stays.
async fn hand_named_and_composite_fks_survive(pool: &Pool) {
    let (a, b) = ("mad_hc_author", "mad_hc_book");
    let chain = Chain::new(pool, "hc", &[b, a]).await;
    let with = |on_delete: Option<&str>| {
        let mut rel = json!({"kind": "fk", "to": a, "on": "id"});
        if let Some(action) = on_delete {
            rel["on_delete"] = json!(action);
        }
        let composite = json!([{"name": "author_code", "to": a,
                                "from": ["author_id", "code"], "on": ["id", "code"]}]);
        json!({
            "tables": [
                table(a, vec![id(), col("code", "i64", json!({}))]),
                {"name": b, "model": b, "fields": [id(),
                    col("author_id", "i64", json!({"fk": rel})), col("code", "i64", json!({}))],
                 "composite_fks": composite},
            ],
            "indexes": [{"name": "mad_hc_author_id_code", "table": a,
                         "columns": ["id", "code"], "unique": true}],
        })
    };
    chain.step(pool, with(None)).await.expect("initial");
    // As a hand edit or an old release would have named it.
    let renamed = by_dialect! { pool,
        postgres => true, because "PG renames a constraint in place",
        mysql => true, because "MySQL re-adds it under another name",
        sqlite => false, because "SQLite FKs have no name to look up",
    };
    if renamed.value {
        let fk = format!("{b}_author_id_fkey");
        let rename = match pool.dialect().name() {
            "postgres" => vec![q(
                pool,
                "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                &[b, &fk, "hand_fk"],
            )],
            _ => vec![
                q(pool, "ALTER TABLE {} DROP FOREIGN KEY {}", &[b, &fk]),
                q(
                    pool,
                    "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
                    &[b, "hand_fk", "author_id", a, "id"],
                ),
            ],
        };
        for sql in rename {
            raw_execute_pool(pool, &sql, Vec::new()).await.unwrap();
        }
    }
    chain
        .step(pool, with(Some("CASCADE")))
        .await
        .expect("AlterFkOnDelete replaces the hand-named FK");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[a, "id", "code"],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}, {}) VALUES (1, 1, 2)",
            &[b, "id", "author_id", "code"],
        )
        .await
        .is_err(),
        "the composite FK still holds on {}",
        pool.dialect().name()
    );
    exec(
        pool,
        // A NULL `code` leaves the composite FK out of the delete.
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[b, "id", "author_id"],
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM {}", &[a])
        .await
        .expect("no NO ACTION FK is left behind; the cascade fires");
    assert_eq!(rows(pool, b).await, 0, "cascaded");
}

/// Unapplying [RenameColumn, AddColumn]: the DropColumn rebuild takes the
/// table as it is then, with the new column name.
async fn rebuild_uses_the_shape_at_its_op(pool: &Pool) {
    let t = "mad_sa_item";
    let chain = Chain::new(pool, "sa", &[t]).await;
    chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(), col("a", "i64", json!({}))])]}),
        )
        .await
        .expect("initial");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 7)",
        &[t, "id", "a"],
    )
    .await
    .unwrap();
    let name = chain
        .hand(
            pool,
            json!({"tables": [table(t, vec![id(), col("b", "i64", json!({})),
                col("c", "i64", json!({}))])]}),
            vec![
                SchemaChange::RenameColumn {
                    table: t.into(),
                    old_column: "a".into(),
                    new_column: "b".into(),
                },
                SchemaChange::AddColumn {
                    table: t.into(),
                    column: "c".into(),
                },
            ],
            None,
        )
        .await
        .expect("rename then add");
    chain
        .undo(pool, &name)
        .await
        .expect("unapply drops c, then renames b back");
    let sql = q(pool, "SELECT {} FROM {}", &["a", t]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(7,)]);
}

/// #2140 — a rebuild before a RenameTable of the same table keeps its CHECK,
/// which the final snapshot keys by the new name.
async fn rebuild_before_rename_keeps_checks(pool: &Pool) {
    let (t, u) = ("mad_rk_old", "mad_rk_new");
    let chain = Chain::new(pool, "rk", &[u, t]).await;
    let with = |name: &str, default: Value| {
        json!({"tables": [table(name, vec![id(), col("price", "i64", default)])],
               "checks": [{"name": "mad_rk_price", "table": name, "expr": "price >= 0"}]})
    };
    chain.step(pool, with(t, json!({}))).await.expect("initial");
    chain
        .hand(
            pool,
            with(u, json!({"default": "1"})),
            vec![
                SchemaChange::AlterColumnDefault {
                    table: t.into(),
                    column: "price".into(),
                    from: None,
                    to: Some("1".into()),
                },
                SchemaChange::RenameTable {
                    old_name: t.into(),
                    new_name: u.into(),
                },
            ],
            None,
        )
        .await
        .expect("default then rename");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (1, -5)",
            &[u, "id", "price"]
        )
        .await
        .is_err(),
        "the CHECK survives on {}",
        pool.dialect().name()
    );
}

/// #2149 — an FK column's UNIQUE dropped before its table is renamed: MySQL
/// still finds the FK that the index backs.
async fn fk_unique_drop_before_rename(pool: &Pool) {
    let (a, t, u) = ("mad_fr_author", "mad_fr_old", "mad_fr_new");
    let chain = Chain::new(pool, "fr", &[u, t, a]).await;
    let with = |name: &str, unique: bool| {
        let mut owner = fk(a);
        owner["unique"] = json!(unique);
        json!({"tables": [table(a, vec![id()]),
                          table(name, vec![id(), col("author_id", "i64", owner)])]})
    };
    chain.step(pool, with(t, true)).await.expect("initial");
    chain
        .hand(
            pool,
            with(u, false),
            vec![
                SchemaChange::AlterColumnUnique {
                    table: t.into(),
                    column: "author_id".into(),
                    unique: false,
                },
                SchemaChange::RenameTable {
                    old_name: t.into(),
                    new_name: u.into(),
                },
            ],
            None,
        )
        .await
        .expect("drop unique then rename");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, 1)"),
            &[u, "id", "author_id"],
        )
        .await
        .expect("no longer unique");
    }
}

/// #2188 review — the FK that comes back before a rename names the tables
/// as they are then: a self-FK, and a target renamed later.
async fn fk_comes_back_under_names_at_its_op(pool: &Pool) {
    let (t, u) = ("mad_sf_old", "mad_sf_new");
    let chain = Chain::new(pool, "sf", &[u, t]).await;
    let with = |name: &str, unique: bool| {
        let mut parent = fk(name);
        parent["unique"] = json!(unique);
        json!({"tables": [table(name, vec![id(), col("parent_id", "i64", parent)])]})
    };
    chain.step(pool, with(t, true)).await.expect("initial");
    let ops = vec![
        SchemaChange::AlterColumnUnique {
            table: t.into(),
            column: "parent_id".into(),
            unique: false,
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, with(u, false), ops, None)
        .await
        .expect("self-FK: drop unique then rename");
    for (id, parent) in [(1, "NULL"), (2, "1"), (3, "1")] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, {parent})"),
            &[u, "id", "parent_id"],
        )
        .await
        .expect("no longer unique");
    }

    let (a, a2, t, u) = ("mad_rt_a", "mad_rt_a2", "mad_rt_old", "mad_rt_new");
    let chain = Chain::new(pool, "rt", &[u, t, a2, a]).await;
    let with = |a: &str, name: &str, unique: bool| {
        let mut owner = fk(a);
        owner["unique"] = json!(unique);
        json!({"tables": [table(a, vec![id()]),
                          table(name, vec![id(), col("author_id", "i64", owner)])]})
    };
    chain.step(pool, with(a, t, true)).await.expect("initial");
    let ops = vec![
        SchemaChange::AlterColumnUnique {
            table: t.into(),
            column: "author_id".into(),
            unique: false,
        },
        SchemaChange::RenameTable {
            old_name: a.into(),
            new_name: a2.into(),
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, with(a2, u, false), ops, None)
        .await
        .expect("renamed target: drop unique then renames");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a2, "id"])
        .await
        .unwrap();
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, 1)"),
            &[u, "id", "author_id"],
        )
        .await
        .expect("no longer unique");
    }
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (3, 99)",
            &[u, "id", "author_id"]
        )
        .await
        .is_err(),
        "the FK into {a2} is back on {}",
        pool.dialect().name()
    );
}

/// #2188 review — an ON DELETE change before a rename of its table.
async fn on_delete_change_before_rename(pool: &Pool) {
    let (a, t, u) = ("mad_odr_author", "mad_odr_old", "mad_odr_new");
    let chain = Chain::new(pool, "odr", &[u, t, a]).await;
    let with = |name: &str, action: &str| {
        let owner = json!({"fk": {"kind": "fk", "to": a, "on": "id", "on_delete": action}});
        json!({"tables": [table(a, vec![id()]),
                          table(name, vec![id(), col("author_id", "i64", owner)])]})
    };
    chain.step(pool, with(t, "CASCADE")).await.expect("initial");
    let ops = vec![
        SchemaChange::AlterFkOnDelete {
            table: t.into(),
            column: "author_id".into(),
            from: Some("CASCADE".into()),
            to: Some("SET NULL".into()),
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, with(u, "SET NULL"), ops, None)
        .await
        .expect("on delete then rename");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[u, "id", "author_id"],
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM {} WHERE {} = 1", &[a, "id"])
        .await
        .unwrap();
    let sql = q(pool, "SELECT {} FROM {}", &["author_id", u]);
    let got: Vec<(Option<i64>,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(None,)], "SET NULL on {}", pool.dialect().name());
}

/// #2188 review — a column's UNIQUE dropped before a rename keeps the
/// declared unique index on it.
async fn declared_index_survives_unique_drop_before_rename(pool: &Pool) {
    let (t, u) = ("mad_di_old", "mad_di_new");
    let chain = Chain::new(pool, "di", &[u, t]).await;
    let with = |name: &str, unique: bool| {
        json!({"tables": [table(name, vec![id(), col("code", "i64", json!({"unique": unique}))])],
               "indexes": [{"name": "mad_di_code_uq", "table": name, "columns": ["code"], "unique": true}]})
    };
    chain.step(pool, with(t, true)).await.expect("initial");
    let ops = vec![
        SchemaChange::AlterColumnUnique {
            table: t.into(),
            column: "code".into(),
            unique: false,
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, with(u, false), ops, None)
        .await
        .expect("drop unique then rename");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 5)",
        &[u, "id", "code"],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 5)",
            &[u, "id", "code"]
        )
        .await
        .is_err(),
        "the declared index still makes code unique on {}",
        pool.dialect().name()
    );
}

/// #2190 — a composite FK added before its table is renamed.
async fn composite_fk_add_before_rename(pool: &Pool) {
    let (parent, t, u) = ("mad_cr_parent", "mad_cr_old", "mad_cr_new");
    let chain = Chain::new(pool, "cr", &[u, t, parent]).await;
    let ab = || vec![id(), col("a", "i64", json!({})), col("b", "i64", json!({}))];
    let with = |name: &str, with_fk: bool| {
        let mut kid = table(name, ab());
        if with_fk {
            kid["composite_fks"] =
                json!([{"name": "mad_cr_fk", "to": parent, "from": ["a", "b"], "on": ["a", "b"]}]);
        }
        json!({"tables": [table(parent, ab()), kid],
               "indexes": [{"name": "mad_cr_ab_uq", "table": parent, "columns": ["a", "b"],
                            "unique": true}]})
    };
    chain.step(pool, with(t, false)).await.expect("initial");
    let ops = vec![
        SchemaChange::AddCompositeFk {
            table: t.into(),
            name: "mad_cr_fk".into(),
            to: parent.into(),
            from: vec!["a".into(), "b".into()],
            on: vec!["a".into(), "b".into()],
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, with(u, true), ops, None)
        .await
        .expect("add composite FK then rename");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}, {}) VALUES (1, 9, 9)",
            &[u, "id", "a", "b"]
        )
        .await
        .is_err(),
        "the composite FK holds on {}",
        pool.dialect().name()
    );
}

/// #2195 review — as above, with its columns and its target renamed later.
async fn composite_fk_add_before_target_and_column_renames(pool: &Pool) {
    let (p, p2, t, u) = (
        "mad_cc_parent",
        "mad_cc_parent2",
        "mad_cc_old",
        "mad_cc_new",
    );
    let chain = Chain::new(pool, "cc", &[u, t, p2, p]).await;
    let cols = |a: &str, b: &str| vec![id(), col(a, "i64", json!({})), col(b, "i64", json!({}))];
    let snap = |p: &str, pb: &str, kid: Value| {
        json!({"tables": [table(p, cols("a", pb)), kid],
               "indexes": [{"name": "mad_cc_ab_uq", "table": p, "columns": ["a", pb],
                            "unique": true}]})
    };
    chain
        .step(pool, snap(p, "b", table(t, cols("a", "b"))))
        .await
        .expect("initial");
    let mut kid = table(u, cols("a2", "b"));
    kid["composite_fks"] =
        json!([{"name": "mad_cc_fk", "to": p2, "from": ["a2", "b"], "on": ["a", "b2"]}]);
    let ops = vec![
        SchemaChange::AddCompositeFk {
            table: t.into(),
            name: "mad_cc_fk".into(),
            to: p.into(),
            from: vec!["a".into(), "b".into()],
            on: vec!["a".into(), "b".into()],
        },
        SchemaChange::RenameColumn {
            table: t.into(),
            old_column: "a".into(),
            new_column: "a2".into(),
        },
        SchemaChange::RenameColumn {
            table: p.into(),
            old_column: "b".into(),
            new_column: "b2".into(),
        },
        SchemaChange::RenameTable {
            old_name: p.into(),
            new_name: p2.into(),
        },
        SchemaChange::RenameTable {
            old_name: t.into(),
            new_name: u.into(),
        },
    ];
    chain
        .hand(pool, snap(p2, "b2", kid), ops, None)
        .await
        .expect("add composite FK, then column and table renames");
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}, {}) VALUES (1, 9, 9)",
            &[u, "id", "a2", "b"]
        )
        .await
        .is_err(),
        "the composite FK holds on {}",
        pool.dialect().name()
    );
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 9, 9)",
        &[p2, "id", "a", "b2"],
    )
    .await
    .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (2, 9, 9)",
        &[u, "id", "a2", "b"],
    )
    .await
    .expect("a row that matches the parent");
}

/// SQLite: a rebuild that orphans a row rolls back; an orphan that was
/// already there elsewhere does not block it; a RunSQL beside it is refused.
async fn rebuild_checks_only_its_own_orphans(pool: &Pool) {
    let (a, b) = ("mad_ro_author", "mad_ro_book");
    let chain = Chain::new(pool, "ro", &[b, a]).await;
    let books = |fields: Vec<Value>| json!({"tables": [table(a, vec![id()]), table(b, fields)]});
    chain
        .step(
            pool,
            books(vec![
                id(),
                col("x", "i64", json!({})),
                col("y", "i64", json!({})),
            ]),
        )
        .await
        .expect("initial");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[b, "id", "x"],
    )
    .await
    .unwrap();
    // A NOT NULL FK column with a default: SQLite adds it without the FK.
    let author = col(
        "author_id",
        "i64",
        json!({"nullable": false, "default": "5",
        "fk": {"kind": "fk", "to": a, "on": "id"}}),
    );
    let added = chain
        .step(
            pool,
            books(vec![
                id(),
                col("x", "i64", json!({})),
                col("y", "i64", json!({})),
                author.clone(),
            ]),
        )
        .await;
    let sqlite = by_dialect! { pool,
        postgres => false, because "the FK is added and refuses author 5",
        mysql => false, because "the FK is added and refuses author 5",
        sqlite => true, because "SQLite adds the column without its FK",
    };
    if !sqlite.value {
        added.expect_err(sqlite.why);
        return;
    }
    added.expect(sqlite.why);
    // The rebuild adds the FK, which row 1 (author 5) breaks.
    let err = chain
        .step(
            pool,
            books(vec![id(), col("y", "i64", json!({})), author.clone()]),
        )
        .await
        .expect_err("the rebuild orphans row 1");
    assert!(err.contains("FOREIGN KEY"), "{err}");
    chain.discard_head();
    exec(pool, "SELECT {} FROM {}", &["x", b])
        .await
        .expect("rolled back: x is still there");
    // Author 5 exists now; an old orphan in another table does not block.
    exec(pool, "INSERT INTO {} ({}) VALUES (5)", &[a, "id"])
        .await
        .unwrap();
    // Only SQLite gets here; the gate keeps mysql- and postgres-only builds compiling.
    #[cfg(feature = "sqlite")]
    {
        let sq = pool.as_sqlite().expect("sqlite");
        let mut conn = sq.acquire().await.unwrap();
        for sql in [
            "PRAGMA foreign_keys = OFF",
            "CREATE TABLE IF NOT EXISTS mad_ro_other (id INTEGER PRIMARY KEY, \
             a_id INTEGER REFERENCES mad_ro_author (id))",
            "INSERT INTO mad_ro_other (id, a_id) VALUES (1, 99)",
            "PRAGMA foreign_keys = ON",
        ] {
            rustango::sql::sqlx::query(sql)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }
    let err = chain
        .hand(
            pool,
            books(vec![id(), col("y", "i64", json!({})), author.clone()]),
            vec![SchemaChange::DropColumn {
                table: b.into(),
                column: "x".into(),
            }],
            Some("SELECT 1"),
        )
        .await
        .expect_err("RunSQL beside a rebuild");
    assert!(err.contains("RunSQL"), "{err}");
    chain.discard_head();
    chain
        .step(pool, books(vec![id(), col("y", "i64", json!({})), author]))
        .await
        .expect("an old orphan in mad_ro_other does not block");
    drop_table(pool, "mad_ro_other").await;
}

/// The `&PgPool` runners drop the live FK too.
async fn legacy_pg_runner_replaces_the_fk(pool: &Pool) {
    let legacy = by_dialect! { pool,
        postgres => true, because "the `&PgPool` runners are Postgres-only",
        mysql => false, because "no `&PgPool` runner",
        sqlite => false, because "no `&PgPool` runner",
    };
    #[cfg(feature = "postgres")]
    if let Some(pg) = pool.as_postgres().filter(|_| legacy.value) {
        legacy_pg_runner_body(pool, pg, legacy.why).await;
    }
    #[cfg(not(feature = "postgres"))]
    let _ = legacy;
}

/// Gated so sqlite- and mysql-only builds compile without the `&PgPool` runners.
#[cfg(feature = "postgres")]
async fn legacy_pg_runner_body(pool: &Pool, pg: &rustango::sql::sqlx::PgPool, why: &str) {
    let (a, b) = ("mad_lg_author", "mad_lg_book");
    let chain = Chain::new(pool, "lg", &[b, a]).await;
    drop_table(pool, "mad_ledger_legacy").await;
    let runner = rustango::migrate::Builder::new().ledger("mad_ledger_legacy");
    let with = |rel: Value| {
        json!({"tables": [table(a, vec![id()]),
            table(b, vec![id(), col("author_id", "i64", json!({"fk": rel}))])]})
    };
    for (rel, atomic) in [
        (json!({"kind": "fk", "to": a, "on": "id"}), true),
        (
            json!({"kind": "fk", "to": a, "on": "id", "on_delete": "CASCADE"}),
            false,
        ),
    ] {
        let mut mig = make_migrations_from(chain.dir.path(), &snap(with(rel)), None)
            .unwrap()
            .unwrap();
        mig.atomic = atomic;
        chain.write(&mig);
        runner.migrate(pg, chain.dir.path()).await.expect(why);
    }
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 1)",
        &[b, "id", "author_id"],
    )
    .await
    .unwrap();
    exec(pool, "DELETE FROM {}", &[a]).await.expect("CASCADE");
    runner
        .unapply(pg, chain.dir.path(), &chain.head().name)
        .await
        .expect("legacy unapply");
    exec(pool, "INSERT INTO {} ({}) VALUES (2)", &[a, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (2, 2)",
        &[b, "id", "author_id"],
    )
    .await
    .unwrap();
    assert!(
        exec(pool, "DELETE FROM {}", &[a]).await.is_err(),
        "NO ACTION again"
    );
}

// ---------------------------------------------------------------- #1676

/// A squash whose tables exist under another ledger still runs its change
/// to a table it does not create; it was recorded with the change skipped.
async fn cross_ledger_squash_runs_its_other_changes(pool: &Pool) {
    let (a, other) = ("mad_sq_a", "mad_sq_other");
    let first = Chain::new(pool, "sq1", &[a, other]).await;
    first
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), table(other, vec![id()])]}),
        )
        .await
        .expect("history under the first ledger");
    let second = Chain::new(pool, "sq2", &[]).await;
    let after = json!({"tables": [table(a, vec![id()]),
        table(other, vec![id(), col("c", "i64", json!({}))])]});
    let mut squash = make_migrations_from(second.dir.path(), &snap(after), None)
        .unwrap()
        .unwrap();
    squash.replaces = vec!["0001_gone".into()];
    squash.forward = vec![
        Operation::Schema(SchemaChange::CreateTable(a.into())),
        Operation::Schema(SchemaChange::AddColumn {
            table: other.into(),
            column: "c".into(),
        }),
    ];
    second.write(&squash);
    second.migrate(pool).await.expect("the squash reconciles");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 2)",
        &[other, "id", "c"],
    )
    .await
    .expect("its AddColumn ran");
}

/// A UNIQUE under a name the migrations did not give it still drops, and
/// an AlterColumn before a later change on the table still rebuilds right.
async fn unique_drop_finds_the_live_name(pool: &Pool) {
    let t = "mad_ul_item";
    let chain = Chain::new(pool, "ul", &[t]).await;
    let with = |unique: bool| {
        json!({"tables": [table(t, vec![id(),
            col("c", "string", json!({"max_length": 20, "unique": unique}))])]})
    };
    chain.step(pool, with(true)).await.expect("initial");
    let renamed = by_dialect! { pool,
        postgres => false, because "PG drops the constraint by its rendered name",
        mysql => true, because "MySQL looks the index up in the catalog",
        sqlite => false, because "SQLite rebuilds the table",
    };
    if renamed.value {
        let name = rustango::migrate::ddl::unique_constraint_name(t, "c");
        exec(
            pool,
            "ALTER TABLE {} RENAME INDEX {} TO {}",
            &[t, &name, "hand_uq"],
        )
        .await
        .unwrap();
    }
    chain.step(pool, with(false)).await.expect("UNIQUE drops");
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, 'same')"),
            &[t, "id", "c"],
        )
        .await
        .expect("no longer unique");
    }
}

/// A column added and another made NOT NULL in one migration: the
/// rebuild keeps the new column and fills the NULL.
async fn alter_then_add_on_one_table(pool: &Pool) {
    let t = "mad_aa_item";
    let chain = Chain::new(pool, "aa", &[t]).await;
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
    chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(),
                col("n", "i64", json!({"nullable": false, "default": "3"})),
                col("m", "i64", json!({}))])]}),
        )
        .await
        .expect("alter and add apply");
    let sql = q(pool, "SELECT {}, {} FROM {}", &["n", "m", t]);
    let got: Vec<(i64, Option<i64>)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(3, None)]);
}

// ---------------------------------------------------------------- #2126

/// A cross-ledger squash whose other changes the first ledger already
/// applied fakes, data op and M2M junction included, as it did before.
async fn cross_ledger_squash_already_applied_fakes(pool: &Pool) {
    let (a, other, through) = ("mad_sf_a", "mad_sf_other", "mad_sf_a_other");
    let first = Chain::new(pool, "sf1", &[through, a, other]).await;
    let m2m = json!([{"through": through, "src_table": a, "src_col": "a_id",
                      "dst_table": other, "dst_col": "other_id"}]);
    let after = json!({"tables": [table(a, vec![id()]),
        table(other, vec![id(), col("c", "i64", json!({}))])], "m2m_tables": m2m});
    first
        .step(pool, after.clone())
        .await
        .expect("history under the first ledger");
    let second = Chain::new(pool, "sf2", &[]).await;
    let mut squash = make_migrations_from(second.dir.path(), &snap(after), None)
        .unwrap()
        .unwrap();
    squash.replaces = vec!["0001_gone".into()];
    squash.forward = vec![
        Operation::Schema(SchemaChange::CreateTable(a.into())),
        Operation::Schema(SchemaChange::AddColumn {
            table: other.into(),
            column: "c".into(),
        }),
        Operation::Schema(SchemaChange::CreateM2MTable {
            through: through.into(),
            src_table: a.into(),
            src_col: "a_id".into(),
            dst_table: other.into(),
            dst_col: "other_id".into(),
        }),
        Operation::Data(rustango::migrate::DataOp {
            sql: q(pool, "DELETE FROM {}", &[a]),
            reverse_sql: Some("SELECT 1".into()),
            reversible: true,
        }),
    ];
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    second.write(&squash);
    second
        .migrate(pool)
        .await
        .expect("everything is applied, so the squash fakes");
    assert_eq!(rows(pool, a).await, 1, "the data op did not run again");
}

/// A file from before the default-first order: NOT NULL, then the default.
async fn not_null_before_default_still_fills(pool: &Pool) {
    let t = "mad_od2_item";
    let chain = Chain::new(pool, "od2", &[t]).await;
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
    chain
        .hand(
            pool,
            json!({"tables": [table(t, vec![id(),
                col("n", "i64", json!({"nullable": false, "default": "4"}))])]}),
            vec![
                SchemaChange::AlterColumnNullable {
                    table: t.into(),
                    column: "n".into(),
                    nullable: false,
                },
                SchemaChange::AlterColumnDefault {
                    table: t.into(),
                    column: "n".into(),
                    from: None,
                    to: Some("4".into()),
                },
            ],
            None,
        )
        .await
        .expect("the NULL is filled with the later default");
    let sql = q(pool, "SELECT {} FROM {}", &["n", t]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(4,)]);
}

/// Dropping a field's UNIQUE keeps a declared unique index on the column,
/// and works on an FK column (MySQL 1553).
async fn unique_drop_keeps_a_declared_index(pool: &Pool) {
    let (a, b) = ("mad_dk_author", "mad_dk_book");
    let chain = Chain::new(pool, "dk", &[b, a]).await;
    let with = |unique: bool| {
        json!({"tables": [table(a, vec![id()]), table(b, vec![id(),
            col("author_id", "i64", json!({"unique": unique,
                "fk": {"kind": "fk", "to": a, "on": "id"}})),
            col("code", "i64", json!({"unique": unique}))])],
            "indexes": [{"name": "mad_dk_code_uq", "table": b, "columns": ["code"], "unique": true}]})
    };
    chain.step(pool, with(true)).await.expect("initial");
    chain
        .step(pool, with(false))
        .await
        .expect("both UNIQUEs drop");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}, {{}}) VALUES ({id}, 1, {id})"),
            &[b, "id", "author_id", "code"],
        )
        .await
        .expect("author_id is no longer unique");
    }
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (3, 1)",
            &[b, "id", "code"]
        )
        .await
        .is_err(),
        "the declared index still makes code unique on {}",
        pool.dialect().name()
    );
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (4, 99)",
            &[b, "id", "author_id"]
        )
        .await
        .is_err(),
        "the FK is back on {}",
        pool.dialect().name()
    );
}

/// An AddColumn UNIQUE dropped after its column was renamed: the index
/// keeps its old name.
async fn unique_drop_after_a_rename(pool: &Pool) {
    let t = "mad_ur_item";
    let chain = Chain::new(pool, "ur", &[t]).await;
    chain
        .step(pool, json!({"tables": [table(t, vec![id()])]}))
        .await
        .expect("initial");
    let unique = |name: &str, unique: bool| json!({"tables": [table(t, vec![id(), col(name, "i64", json!({"unique": unique}))])]});
    chain
        .step(pool, unique("c", true))
        .await
        .expect("AddColumn UNIQUE");
    chain
        .hand(
            pool,
            unique("d", true),
            vec![SchemaChange::RenameColumn {
                table: t.into(),
                old_column: "c".into(),
                new_column: "d".into(),
            }],
            None,
        )
        .await
        .expect("rename");
    // Every backend finds the old name in the catalog (#2133).
    chain
        .step(pool, unique("d", false))
        .await
        .expect("drop the renamed column's UNIQUE");
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, 5)"),
            &[t, "id", "d"],
        )
        .await
        .expect("no longer unique");
    }
}

/// AlterColumn* unapplies on every backend.
async fn alter_column_unapplies(pool: &Pool) {
    let t = "mad_ua_item";
    let chain = Chain::new(pool, "ua", &[t]).await;
    let with = |strict: bool| {
        let n = if strict {
            json!({"nullable": false, "default": "5"})
        } else {
            json!({})
        };
        json!({"tables": [table(t, vec![id(), col("n", "i64", n),
            col("s", "string", json!({"max_length": 10, "unique": strict}))])]})
    };
    chain.step(pool, with(false)).await.expect("initial");
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'x')",
        &[t, "id", "s"],
    )
    .await
    .unwrap();
    let name = chain.step(pool, with(true)).await.expect("alter");
    chain.undo(pool, &name).await.expect("unapply");
    exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (2, NULL, 'x')",
        &[t, "id", "n", "s"],
    )
    .await
    .expect("nullable and not unique again");
}

// ---------------------------------------------------------------- #2240

/// A new PG database, which has no `citext` yet; `None` elsewhere.
async fn fresh_pg(pool: &Pool, tag: &str) -> Option<(Pool, String)> {
    if pool.dialect().name() != "postgres" {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = format!("rustango_mad_{tag}_{}_{nanos}", std::process::id());
    raw_execute_pool(pool, &format!("CREATE DATABASE {db}"), Vec::new())
        .await
        .expect("create database");
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (base, _) = url.rsplit_once('/').unwrap();
    let fresh = Pool::connect(&format!("{base}/{db}"))
        .await
        .expect("connect");
    Some((fresh, db))
}

async fn drop_fresh_pg(pool: &Pool, fresh: Option<(Pool, String)>) {
    if let Some((fresh, db)) = fresh {
        fresh.close().await;
        let _ = raw_execute_pool(pool, &format!("DROP DATABASE {db}"), Vec::new()).await;
    }
}

/// A unique case-insensitive `email` refuses `a@X.COM` next to `A@x.com`.
async fn assert_ci_unique(pool: &Pool, t: &str, column: &str) {
    exec(pool, "DELETE FROM {}", &[t]).await.unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'A@x.com')",
        &[t, "id", column],
    )
    .await
    .unwrap();
    assert!(
        exec(
            pool,
            "INSERT INTO {} ({}, {}) VALUES (2, 'a@X.COM')",
            &[t, "id", column]
        )
        .await
        .is_err(),
        "`{t}.{column}` is unique ignoring case on {}",
        pool.dialect().name()
    );
}

fn ci_email(name: &str) -> Value {
    col(
        name,
        "string",
        json!({"max_length": 100, "case_insensitive": true, "unique": true}),
    )
}

/// CreateTable and AddColumn of a CITEXT column on a database without
/// the extension: PG said `type "citext" does not exist`.
async fn citext_column_on_a_fresh_database(shared: &Pool) {
    let t = "mad_ci_user";
    let steps: [&[Value]; 2] = [
        &[json!({"tables": [table(t, vec![id(), ci_email("email")])]})],
        &[
            json!({"tables": [table(t, vec![id()])]}),
            json!({"tables": [table(t, vec![id(), ci_email("email")])]}),
        ],
    ];
    for (i, steps) in steps.into_iter().enumerate() {
        let fresh = fresh_pg(shared, &format!("ci{i}")).await;
        let pool = fresh.as_ref().map_or(shared, |(p, _)| p);
        let chain = Chain::new(pool, &format!("ci{i}"), &[t]).await;
        for s in steps {
            chain
                .step(pool, s.clone())
                .await
                .expect("the CITEXT column applies");
        }
        assert_ci_unique(pool, t, "email").await;
        drop_fresh_pg(shared, fresh).await;
    }
}

// ---------------------------------------------------------------- #2238

/// A max_length or type change keeps a CITEXT column case-insensitive:
/// PG turned it into VARCHAR or TEXT.
async fn citext_survives_length_and_type_changes(pool: &Pool) {
    let t = "mad_cl_user";
    let chain = Chain::new(pool, "cl", &[t]).await;
    let email =
        |extra: Value| json!({"tables": [table(t, vec![id(), col("email", "string", extra)])]});
    let ci = |n: u32| json!({"max_length": n, "case_insensitive": true, "unique": true});
    chain.step(pool, email(ci(100))).await.expect("initial");
    chain
        .step(pool, email(ci(200)))
        .await
        .expect("the length change applies");
    assert_ci_unique(pool, t, "email").await;

    // No length, so the type change is the only op.
    let t = "mad_ct_user";
    let chain = Chain::new(pool, "ct", &[t]).await;
    let c = |ty: &str, extra: Value| json!({"tables": [table(t, vec![id(), col("c", ty, extra)])]});
    chain
        .step(pool, c("i32", json!({})))
        .await
        .expect("initial");
    chain
        .step(pool, c("string", json!({"case_insensitive": true})))
        .await
        .expect("the type change applies");
    assert_ci_equal(pool, t, "c").await;
}

/// `c = 'abc'` finds the row holding `ABC`.
async fn assert_ci_equal(pool: &Pool, t: &str, column: &str) {
    exec(pool, "DELETE FROM {}", &[t]).await.unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'ABC')",
        &[t, "id", column],
    )
    .await
    .unwrap();
    let sql = q(
        pool,
        "SELECT {} FROM {} WHERE {} = 'abc'",
        &["id", t, column],
    );
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(
        got,
        [(1,)],
        "`{t}.{column}` compares ignoring case on {}",
        pool.dialect().name()
    );
}

// ---------------------------------------------------------------- #2239

/// Turning `case_insensitive` on and off reaches the column; makemigrations
/// wrote nothing for it.
async fn case_insensitive_change_applies(pool: &Pool) {
    let t = "mad_cf_user";
    let chain = Chain::new(pool, "cif", &[t]).await;
    let email = |ci: bool| {
        json!({"tables": [table(t, vec![id(), col("email", "string",
            json!({"max_length": 10, "case_insensitive": ci, "unique": true}))])]})
    };
    chain.step(pool, email(false)).await.expect("initial");
    chain.step(pool, email(true)).await.expect("turned on");
    assert_ci_unique(pool, t, "email").await;
    chain.step(pool, email(false)).await.expect("turned off");
    exec(pool, "DELETE FROM {}", &[t]).await.unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 'A@x.com')",
        &[t, "id", "email"],
    )
    .await
    .unwrap();
    let other_case = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (2, 'a@X.COM')",
        &[t, "id", "email"],
    )
    .await;
    let distinct = by_dialect! { pool,
        postgres => true, because "VARCHAR compares case-sensitively",
        mysql => false, because "the database's default collation ignores case",
        sqlite => true, because "the rebuilt column has no NOCASE",
    };
    assert_eq!(other_case.is_ok(), distinct.value, "{}", distinct.why);
    let long = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (3, 'abcdefghijk')",
        &[t, "id", "email"],
    )
    .await;
    let enforced = by_dialect! { pool,
        postgres => true, because "the column is VARCHAR(10) again, not TEXT",
        mysql => true, because "the column is VARCHAR(10)",
        sqlite => false, because "SQLite never enforces VARCHAR length",
    };
    assert_eq!(long.is_err(), enforced.value, "{}", enforced.why);
}

/// The column comment the catalog holds; `None` on SQLite.
async fn column_comment(pool: &Pool, t: &str, column: &str) -> Option<String> {
    let sql = match pool.dialect().name() {
        "postgres" => format!(
            "SELECT COALESCE(col_description('{t}'::regclass, ordinal_position::int), '') \
             FROM information_schema.columns WHERE table_name = '{t}' AND column_name = '{column}' \
             AND table_schema = current_schema()"
        ),
        "mysql" => format!(
            "SELECT CAST(COLUMN_COMMENT AS CHAR) FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = '{t}' AND COLUMN_NAME = '{column}'"
        ),
        _ => return None,
    };
    let got: Vec<(String,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    Some(got[0].0.clone())
}

/// A `db_comment` change is applied, replaced, dropped and unapplied.
async fn db_comment_change_applies(pool: &Pool) {
    let t = "mad_cm_item";
    let chain = Chain::new(pool, "cm", &[t]).await;
    let with = |comment: Option<&str>| json!({"tables": [table(t, vec![id(), col("c", "i64", json!({"db_comment": comment}))])]});
    let expect = |s: &str| (pool.dialect().name() != "sqlite").then(|| s.to_owned());
    chain.step(pool, with(None)).await.expect("initial");
    chain.step(pool, with(Some("first"))).await.expect("added");
    assert_eq!(column_comment(pool, t, "c").await, expect("first"));
    let name = chain
        .step(pool, with(Some("it's second")))
        .await
        .expect("changed");
    assert_eq!(column_comment(pool, t, "c").await, expect("it's second"));
    chain.undo(pool, &name).await.expect("unapply");
    assert_eq!(column_comment(pool, t, "c").await, expect("first"));
    chain.discard_head();
    chain.step(pool, with(None)).await.expect("dropped");
    assert_eq!(column_comment(pool, t, "c").await, expect(""));
}

/// Dropping the index an FK uses, then its whole table; MySQL refused
/// both with 1553 (#2244).
async fn fk_index_drops(pool: &Pool) {
    let (a, b) = ("mad_fi_author", "mad_fi_book");
    let chain = fk_index_chain(pool, "fi", a, b, false).await;
    let dropped = chain
        .step(pool, json!({"tables": [table(a, vec![id()]), book(a, b)]}))
        .await
        .expect("the FK's index drops");
    orphan_refused(pool, b, "author_id").await;
    chain.undo(pool, &dropped).await.expect("unapply");

    // A failed run left the FK dropped; the step still brings it back.
    let (a, b) = ("mad_fk_author", "mad_fk_book");
    let chain = fk_index_chain(pool, "fk", a, b, false).await;
    if pool.dialect().name() == "mysql" {
        let fk_name = rustango::migrate::ddl::fk_constraint_name(b, "author_id");
        exec(pool, "ALTER TABLE {} DROP FOREIGN KEY {}", &[b, &fk_name])
            .await
            .unwrap();
    }
    chain
        .step(pool, json!({"tables": [table(a, vec![id()]), book(a, b)]}))
        .await
        .expect("the index drops");
    orphan_refused(pool, b, "author_id").await;

    let (a, b) = ("mad_fj_author", "mad_fj_book");
    let chain = fk_index_chain(pool, "fj", a, b, false).await;
    chain
        .step(pool, json!({"tables": [table(a, vec![id()])]}))
        .await
        .expect("the table drops with its FK's index");
}

/// Another index serves the FK, so a plain DROP INDEX is enough: no FK
/// drop and table-copy re-add, which an orphan row would now fail (#2244).
async fn fk_index_drop_keeps_a_served_fk(pool: &Pool) {
    let (a, b) = ("mad_fo_author", "mad_fo_book");
    let chain = fk_index_chain(pool, "fo", a, b, true).await;
    #[cfg(feature = "mysql")]
    if let Some(my) = pool.as_mysql() {
        let mut conn = my.acquire().await.unwrap();
        for sql in [
            "SET foreign_key_checks = 0".to_owned(),
            q(
                pool,
                "INSERT INTO {} ({}, {}) VALUES (7, 99)",
                &[b, "id", "author_id"],
            ),
            "SET foreign_key_checks = 1".to_owned(),
        ] {
            rustango::sql::sqlx::query(&sql)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }
    let n_idx = json!([{"name": format!("{b}_author_n_idx"), "table": b,
                        "columns": ["author_id", "n"], "unique": false}]);
    chain
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), book(a, b)], "indexes": n_idx}),
        )
        .await
        .expect("a plain DROP INDEX, the FK untouched");
}

/// DropIndex, then an alter of the FK column in the same migration: the FK
/// came back twice on MySQL (1826) (#2244).
async fn fk_index_drop_then_alter(pool: &Pool) {
    let (a, b) = ("mad_fa_author", "mad_fa_book");
    let chain = fk_index_chain(pool, "fa", a, b, false).await;
    let alter = SchemaChange::AlterColumnType {
        table: b.into(),
        column: "author_id".into(),
        from: "i64".into(),
        to: "i64".into(),
    };
    chain.write_hand(
        json!({"tables": [table(a, vec![id()]), book(a, b)]}),
        vec![drop_index(b), Operation::Schema(alter)],
    );
    chain.migrate(pool).await.expect("the FK comes back once");
    orphan_refused(pool, b, "author_id").await;
}

/// An alter of the FK column, then DropIndex, in one migration: both
/// re-added the FK on MySQL (1826) (#2244).
async fn fk_alter_then_index_drop(pool: &Pool) {
    let (a, b) = ("mad_fb_author", "mad_fb_book");
    let chain = fk_index_chain(pool, "fb", a, b, false).await;
    let alter = SchemaChange::AlterColumnType {
        table: b.into(),
        column: "author_id".into(),
        from: "i64".into(),
        to: "i64".into(),
    };
    chain.write_hand(
        json!({"tables": [table(a, vec![id()]), book(a, b)]}),
        vec![Operation::Schema(alter), drop_index(b)],
    );
    chain.migrate(pool).await.expect("the FK comes back once");
    orphan_refused(pool, b, "author_id").await;
}

/// DropIndex, then a rename of the FK column in the same migration: the
/// re-add named the old column on MySQL (1072) (#2244).
async fn fk_index_drop_then_rename(pool: &Pool) {
    let (a, b) = ("mad_fz_author", "mad_fz_book");
    let chain = fk_index_chain(pool, "fz", a, b, false).await;
    let renamed = table(
        b,
        vec![
            id(),
            col("writer_id", "i64", fk(a)),
            col("n", "i32", json!({})),
        ],
    );
    let rename = SchemaChange::RenameColumn {
        table: b.into(),
        old_column: "author_id".into(),
        new_column: "writer_id".into(),
    };
    chain.write_hand(
        json!({"tables": [table(a, vec![id()]), renamed]}),
        vec![drop_index(b), Operation::Schema(rename)],
    );
    chain
        .migrate(pool)
        .await
        .expect("the FK comes back on writer_id");
    orphan_refused(pool, b, "writer_id").await;
}

/// A shape the runner cannot rebuild at the DropIndex is an error, not a
/// reason to drop the FK for good (#2244).
async fn fk_index_drop_refuses_an_unknown_shape(pool: &Pool) {
    if pool.dialect().name() != "mysql" {
        return; // Only MySQL takes FKs off for an index drop.
    }
    let (a, b) = ("mad_fu_author", "mad_fu_book");
    let chain = fk_index_chain(pool, "fu", a, b, false).await;
    // `n` is altered, then dropped: its shape at the DropIndex is unknown.
    let ops = vec![
        drop_index(b),
        Operation::Schema(SchemaChange::AlterColumnNullable {
            table: b.into(),
            column: "n".into(),
            nullable: false,
        }),
        Operation::Schema(SchemaChange::DropColumn {
            table: b.into(),
            column: "n".into(),
        }),
    ];
    let without_n = table(b, vec![id(), col("author_id", "i64", fk(a))]);
    chain.write_hand(json!({"tables": [table(a, vec![id()]), without_n]}), ops);
    assert!(chain.migrate(pool).await.is_err(), "refused");
    orphan_refused(pool, b, "author_id").await;
}

/// The composite FKs MySQL's catalog says only `index` on `t` serves;
/// `None` elsewhere.
async fn composite_fks_needing(pool: &Pool, t: &str, index: &str) -> Option<Vec<(String,)>> {
    use rustango::sql::Dialect as _;
    if pool.dialect().name() != "mysql" {
        return None;
    }
    let sql = rustango::sql::MySql.composite_fks_needing_index_sql()?;
    let s = |v: &str| rustango::core::SqlValue::String(v.to_owned());
    Some(
        rustango::sql::raw_query_pool(sql, vec![s(t), s(index), s(index)], pool)
            .await
            .unwrap(),
    )
}

/// Dropping the index a composite FK uses, though another index starts with
/// its first column; MySQL refused it with 1553 (#2326). An index that also
/// serves it, in the same column order, leaves the FK alone.
async fn composite_fk_index_drops(pool: &Pool) {
    let (a, b) = ("mad_ci_author", "mad_ci_book");
    let chain = Chain::new(pool, "ci", &[b, a]).await;
    let with = |indexed: bool, served: bool| {
        let mut idx = vec![
            json!({"name": "mad_ci_author_id_code", "table": a,
                   "columns": ["id", "code"], "unique": true}),
            json!({"name": "mad_ci_book_author_idx", "table": b,
                   "columns": ["author_id"], "unique": false}),
            json!({"name": "mad_ci_book_ca_idx", "table": b,
                   "columns": ["code", "author_id"], "unique": false}),
        ];
        if indexed {
            idx.push(json!({"name": "mad_ci_book_acn_idx", "table": b,
                            "columns": ["author_id", "code", "n"], "unique": false}));
        }
        if served {
            idx.push(json!({"name": "mad_ci_book_ac_idx", "table": b,
                            "columns": ["author_id", "code"], "unique": false}));
        }
        let composite = json!([{"name": "mad_ci_author_code", "to": a,
                                "from": ["author_id", "code"], "on": ["id", "code"]}]);
        json!({
            "tables": [
                table(a, vec![id(), col("code", "i64", json!({}))]),
                {"name": b, "model": b, "fields": [id(), col("author_id", "i64", json!({})),
                    col("code", "i64", json!({})), col("n", "i32", json!({}))],
                 "composite_fks": composite},
            ],
            "indexes": idx,
        })
    };
    let needing = |index: &'static str| composite_fks_needing(pool, b, index);
    let mysql = |names: &[&str]| {
        (pool.dialect().name() == "mysql")
            .then(|| names.iter().map(|n| ((*n).to_owned(),)).collect::<Vec<_>>())
    };
    chain.step(pool, with(true, true)).await.expect("initial");
    assert_eq!(needing("mad_ci_book_acn_idx").await, mysql(&[]));
    assert_eq!(needing("mad_ci_book_ac_idx").await, mysql(&[]));
    chain
        .step(pool, with(true, false))
        .await
        .expect("an index the other one covers drops");
    assert_eq!(
        needing("mad_ci_book_acn_idx").await,
        mysql(&["mad_ci_author_code"]),
        "`(code, author_id)` does not serve it"
    );
    chain
        .step(pool, with(false, false))
        .await
        .expect("the composite FK's index drops");
    let orphan = exec(
        pool,
        "INSERT INTO {} ({}, {}, {}) VALUES (1, 99, 1)",
        &[b, "id", "author_id", "code"],
    )
    .await;
    let err = orphan.expect_err("the composite FK is back");
    assert!(err.to_lowercase().contains("foreign key"), "{err}");
}

/// `b`: an FK to `a`, and `n`.
fn book(a: &str, b: &str) -> Value {
    table(
        b,
        vec![
            id(),
            col("author_id", "i64", fk(a)),
            col("n", "i32", json!({})),
        ],
    )
}

fn drop_index(b: &str) -> Operation {
    Operation::Schema(SchemaChange::DropIndex {
        name: format!("{b}_author_idx"),
        table: b.into(),
    })
}

/// An orphan in `t.column` fails on its FK, not on anything else.
async fn orphan_refused(pool: &Pool, t: &str, column: &str) {
    let got = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 99)",
        &[t, "id", column],
    )
    .await;
    let err = got.expect_err("an orphan row");
    assert!(
        err.to_lowercase().contains("foreign key"),
        "{}: {err}",
        pool.dialect().name()
    );
}

/// [`book`] with an index on `author_id`, and one on `(author_id, n)` if
/// `served`, applied.
async fn fk_index_chain(pool: &Pool, tag: &str, a: &str, b: &str, served: bool) -> Chain {
    let chain = Chain::new(pool, tag, &[b, a]).await;
    let mut idx = vec![json!({"name": format!("{b}_author_idx"), "table": b,
                              "columns": ["author_id"], "unique": false})];
    if served {
        idx.push(json!({"name": format!("{b}_author_n_idx"), "table": b,
                        "columns": ["author_id", "n"], "unique": false}));
    }
    chain
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), book(a, b)], "indexes": idx}),
        )
        .await
        .expect("initial");
    chain
}

/// An `Auto` PK widened to i64 hands out ids past 2^31; PG's sequence
/// stayed `AS integer` (#2245).
async fn auto_pk_widens_its_sequence(pool: &Pool) {
    let t = "mad_aw_item";
    let chain = Chain::new(pool, "aw", &[t]).await;
    let with = |ty: &str| {
        json!({"tables": [table(t, vec![
            json!({"name": "id", "column": "id", "ty": ty, "nullable": false,
                   "primary_key": true, "auto": true}),
            col("n", "i32", json!({}))])]})
    };
    chain.step(pool, with("i32")).await.expect("initial");
    chain.step(pool, with("i64")).await.expect("i32 → i64");
    let jump = by_dialect! { pool,
        postgres => "SELECT setval(pg_get_serial_sequence('{}', 'id'), 3000000000)",
            because "PG's sequence must be bigint to take it",
        mysql => "ALTER TABLE {} AUTO_INCREMENT = 3000000001",
            because "MySQL's counter follows the column type",
        sqlite => "INSERT INTO {} (id) VALUES (3000000000)",
            because "SQLite's rowid is always 64-bit",
    };
    exec(pool, jump.value, &[t])
        .await
        .expect("the counter takes 3e9");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[t, "n"])
        .await
        .expect("an id past 2^31");
    let sql = q(pool, "SELECT MAX({}) FROM {}", &["id", t]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(3_000_000_001,)]);
}

/// Two FKs whose names cut to the same 63 bytes are refused before any
/// DDL; PG and MySQL failed mid-migration (#2245).
async fn fk_name_collision_is_refused(pool: &Pool) {
    let (a, b) = (
        "mad_fn_author",
        "mad_fn_book_with_a_rather_long_table_name_xxxx",
    );
    let chain = Chain::new(pool, "fn", &[b, a]).await;
    let got = chain
        .step(
            pool,
            json!({"tables": [table(a, vec![id()]), table(b, vec![id(),
                col("author_reference_first", "i64", fk(a)),
                col("author_reference_second", "i64", fk(a))])]}),
        )
        .await;
    let refused = by_dialect! { pool,
        postgres => true, because "PG wants FK names unique per table",
        mysql => true, because "MySQL wants them unique per database",
        sqlite => false, because "SQLite does not care",
    };
    match got {
        Err(e) if refused.value => assert!(e.contains("rename a table or column"), "{e}"),
        other => assert_eq!(other.is_ok(), !refused.value, "{}: {other:?}", refused.why),
    }
}

/// A junction column renamed keeps its rows (#2245).
async fn m2m_column_rename_keeps_rows(pool: &Pool) {
    let (post, tag, through) = ("mad_mr_post", "mad_mr_tag", "mad_mr_post_tags");
    let chain = Chain::new(pool, "mr", &[through, post, tag]).await;
    let with = |dst_col: &str| {
        json!({
            "tables": [table(post, vec![id()]), table(tag, vec![id()])],
            "m2m_tables": [{"through": through, "src_table": post, "src_col": "post_id",
                            "dst_table": tag, "dst_col": dst_col}],
        })
    };
    chain.step(pool, with("tag_id")).await.expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[post, "id"])
        .await
        .unwrap();
    exec(pool, "INSERT INTO {} ({}) VALUES (7)", &[tag, "id"])
        .await
        .unwrap();
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 7)",
        &[through, "post_id", "tag_id"],
    )
    .await
    .unwrap();
    let name = chain
        .step(pool, with("label_id"))
        .await
        .expect("the rename applies");
    let sql = q(pool, "SELECT {} FROM {}", &["label_id", through]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(7,)], "the row survived the rename");
    chain.undo(pool, &name).await.expect("unapply");
    let sql = q(pool, "SELECT {} FROM {}", &["tag_id", through]);
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(7,)], "and the unapply");
}

/// Each FK column of `t` with its live FK's name, sorted; `None` on SQLite,
/// whose names are never looked up.
async fn fk_names(pool: &Pool, t: &str) -> Option<Vec<(String, String)>> {
    let sql = match pool.dialect().name() {
        "postgres" => format!(
            "SELECT a.attname::text, c.conname::text FROM pg_constraint c \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
             WHERE c.conrelid = '{t}'::regclass AND c.contype = 'f' ORDER BY 1"
        ),
        "mysql" => format!(
            "SELECT CAST(COLUMN_NAME AS CHAR), CAST(CONSTRAINT_NAME AS CHAR) \
             FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = DATABASE() \
             AND TABLE_NAME = '{t}' AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY 1"
        ),
        _ => return None,
    };
    Some(
        rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
            .await
            .unwrap(),
    )
}

/// What [`fk_names`] reads when each of `cols` has the FK migrate names.
fn fks_named(pool: &Pool, t: &str, cols: &[&str]) -> Option<Vec<(String, String)>> {
    let mut v: Vec<(String, String)> = cols
        .iter()
        .map(|c| {
            let name = rustango::migrate::ddl::fk_constraint_name(t, c);
            ((*c).to_owned(), name)
        })
        .collect();
    v.sort();
    (pool.dialect().name() != "sqlite").then_some(v)
}

/// A renamed junction column's FK takes the new column's name, through a
/// swap and an unapply; it kept the old one (#2307).
async fn m2m_column_rename_renames_its_fk(pool: &Pool) {
    let (post, tag, through) = ("mad_mf_post", "mad_mf_tag", "mad_mf_post_tags");
    let chain = Chain::new(pool, "mf", &[through, post, tag]).await;
    let with = |src_col: &str, dst_col: &str| {
        json!({
            "tables": [table(post, vec![id()]), table(tag, vec![id()])],
            "m2m_tables": [{"through": through, "src_table": post, "src_col": src_col,
                            "dst_table": tag, "dst_col": dst_col}],
        })
    };
    let names = |cols: &[&str]| fks_named(pool, through, cols);
    chain
        .step(pool, with("post_id", "tag_id"))
        .await
        .expect("initial");
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[post, "id"])
        .await
        .unwrap();
    exec(pool, "INSERT INTO {} ({}) VALUES (7)", &[tag, "id"])
        .await
        .unwrap();
    let renamed = chain
        .step(pool, with("post_id", "label_id"))
        .await
        .expect("the rename applies");
    assert_eq!(
        fk_names(pool, through).await,
        names(&["label_id", "post_id"])
    );
    chain.undo(pool, &renamed).await.expect("unapply");
    assert_eq!(fk_names(pool, through).await, names(&["post_id", "tag_id"]));
    chain.discard_head();
    // A swap goes through a spare name; each FK follows its column.
    chain
        .step(pool, with("tag_id", "post_id"))
        .await
        .expect("the swap applies");
    assert_eq!(fk_names(pool, through).await, names(&["post_id", "tag_id"]));
    exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (1, 7)",
        &[through, "tag_id", "post_id"],
    )
    .await
    .expect("tag_id now points at the post, post_id at the tag");
    let orphan = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (7, 1)",
        &[through, "tag_id", "post_id"],
    )
    .await;
    let err = orphan.expect_err("each FK is enforced");
    assert!(err.to_lowercase().contains("foreign key"), "{err}");
}

/// A renamed FK column's FK takes its name, and a later op on that FK in the
/// same migration adds it once, both ways; it was added twice (#2307 review).
async fn fk_column_rename_then_fk_op(pool: &Pool) {
    let (a, b) = ("mad_rfo_author", "mad_rfo_book");
    let chain = Chain::new(pool, "rfo", &[b, a]).await;
    let with = |column: &str, on_delete: &str, unique: bool| {
        let rel = json!({"fk": {"kind": "fk", "to": a, "on": "id", "on_delete": on_delete},
                         "unique": unique});
        json!({"tables": [table(a, vec![id()]),
                          table(b, vec![id(), col(column, "i64", rel)])]})
    };
    let rename = |from: &str, to: &str| SchemaChange::RenameColumn {
        table: b.into(),
        old_column: from.into(),
        new_column: to.into(),
    };
    chain
        .step(pool, with("author_id", "CASCADE", true))
        .await
        .expect("initial");
    let plain = chain
        .hand(
            pool,
            with("writer_id", "CASCADE", true),
            vec![rename("author_id", "writer_id")],
            None,
        )
        .await
        .expect("a plain rename");
    assert_eq!(fk_names(pool, b).await, fks_named(pool, b, &["writer_id"]));
    chain.undo(pool, &plain).await.expect("unapply the rename");
    assert_eq!(fk_names(pool, b).await, fks_named(pool, b, &["author_id"]));
    chain.discard_head();
    // Then ON DELETE, which re-adds the FK on every backend.
    let ops = vec![
        rename("author_id", "writer_id"),
        SchemaChange::AlterFkOnDelete {
            table: b.into(),
            column: "writer_id".into(),
            from: Some("CASCADE".into()),
            to: Some("SET NULL".into()),
        },
    ];
    let on_delete = chain
        .hand(pool, with("writer_id", "SET NULL", true), ops, None)
        .await
        .expect("rename then ON DELETE");
    assert_eq!(fk_names(pool, b).await, fks_named(pool, b, &["writer_id"]));
    chain.undo(pool, &on_delete).await.expect("its unapply");
    assert_eq!(fk_names(pool, b).await, fks_named(pool, b, &["author_id"]));
    chain.discard_head();
    // Then a UNIQUE drop, which MySQL's MODIFY needs the FK off for.
    let ops = vec![
        rename("author_id", "writer_id"),
        SchemaChange::AlterColumnUnique {
            table: b.into(),
            column: "writer_id".into(),
            unique: false,
        },
    ];
    chain
        .hand(pool, with("writer_id", "CASCADE", false), ops, None)
        .await
        .expect("rename then UNIQUE drop");
    assert_eq!(fk_names(pool, b).await, fks_named(pool, b, &["writer_id"]));
    exec(pool, "INSERT INTO {} ({}) VALUES (1)", &[a, "id"])
        .await
        .unwrap();
    for id in [1, 2] {
        exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, 1)"),
            &[b, "id", "writer_id"],
        )
        .await
        .expect("no longer unique");
    }
    let orphan = exec(
        pool,
        "INSERT INTO {} ({}, {}) VALUES (3, 99)",
        &[b, "id", "writer_id"],
    )
    .await;
    assert!(orphan.is_err(), "the FK is back");
}

/// A `db_comment` lands with CreateTable and AddColumn too (#2270).
async fn db_comment_on_create_and_add_column(pool: &Pool) {
    let t = "mad_cc_item";
    let chain = Chain::new(pool, "cc", &[t]).await;
    let a = col("a", "i64", json!({"db_comment": "on create"}));
    let b = col("b", "i64", json!({"db_comment": "on add"}));
    let expect = |s: &str| (pool.dialect().name() != "sqlite").then(|| s.to_owned());
    chain
        .step(pool, json!({"tables": [table(t, vec![id(), a.clone()])]}))
        .await
        .expect("CreateTable");
    assert_eq!(column_comment(pool, t, "a").await, expect("on create"));
    chain
        .step(
            pool,
            json!({"tables": [table(t, vec![id(), a.clone(), b.clone()])]}),
        )
        .await
        .expect("AddColumn");
    assert_eq!(column_comment(pool, t, "b").await, expect("on add"));
    // MySQL adds it nullable, then MODIFYs it, which kept no comment.
    let d = col(
        "d",
        "uuid",
        json!({"nullable": false, "default": "gen_random_uuid()", "db_comment": "uuid"}),
    );
    chain
        .step(pool, json!({"tables": [table(t, vec![id(), a, b, d])]}))
        .await
        .expect("NOT NULL UUID AddColumn");
    assert_eq!(column_comment(pool, t, "d").await, expect("uuid"));
}

// ---------------------------------------------------------------- #2241

/// Unapplying a dropped EXCLUDE puts it back; it always errored.
async fn dropped_exclude_unapplies(pool: &Pool) {
    let t = "mad_xu_booking";
    let chain = Chain::new(pool, "xu", &[t]).await;
    let with = |exclude: bool| {
        let excludes = if exclude {
            json!([{"name": "mad_xu_no_overlap", "table": t, "using": "gist",
                    "elements": [["during", "&&"]]}])
        } else {
            json!([])
        };
        json!({"tables": [table(t, vec![id(), col("during", "range_datetime", json!({}))])],
               "excludes": excludes})
    };
    chain.step(pool, with(true)).await.expect("initial");
    let name = chain.step(pool, with(false)).await.expect("dropped");
    chain.undo(pool, &name).await.expect("unapply");
    if pool.dialect().name() != "postgres" {
        return;
    }
    for id in [1, 2] {
        let overlap = exec(
            pool,
            &format!("INSERT INTO {{}} ({{}}, {{}}) VALUES ({id}, '[2026-01-01,2026-01-02)')"),
            &[t, "id", "during"],
        )
        .await;
        assert_eq!(overlap.is_err(), id == 2, "the EXCLUDE is back");
    }
}

// ---------------------------------------------------------------- #2242

/// A type change on a column with a DEFAULT: PG said the default
/// "cannot be cast automatically". The new default applies after.
async fn type_change_with_a_default(pool: &Pool) {
    let t = "mad_td_item";
    let chain = Chain::new(pool, "td", &[t]).await;
    let uuid0 = "'00000000-0000-0000-0000-000000000000'";
    let with = |flag: (&str, &str), code: &str| {
        json!({"tables": [table(t, vec![id(),
            col("flag", flag.0, json!({"default": flag.1})),
            col("code", code, json!({"default": uuid0}))])]})
    };
    let bools = by_dialect! { pool,
        postgres => ("false", "true"), because "PG has a boolean type",
        mysql => ("0", "1"), because "MySQL's BOOLEAN is TINYINT(1)",
        sqlite => ("0", "1"), because "SQLite stores booleans as integers",
    };
    let (f, tr) = bools.value;
    chain
        .step(pool, with(("bool", f), "string"))
        .await
        .expect("initial");
    exec(
        pool,
        &format!("INSERT INTO {{}} ({{}}, {{}}, {{}}) VALUES (1, {tr}, {uuid0})"),
        &[t, "id", "flag", "code"],
    )
    .await
    .unwrap();
    let name = chain
        .step(pool, with(("i32", "7"), "uuid"))
        .await
        .expect("bool → i32 and string → uuid apply with their defaults");
    exec(pool, "INSERT INTO {} ({}) VALUES (2)", &[t, "id"])
        .await
        .unwrap();
    let sql = q(pool, "SELECT {} FROM {} ORDER BY {}", &["flag", t, "id"]);
    let got: Vec<(i32,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(1,), (7,)], "the old value cast, the new default set");

    // Undo: the old type comes back with its old default.
    exec(pool, "DELETE FROM {} WHERE {} = 2", &[t, "id"])
        .await
        .unwrap();
    chain.undo(pool, &name).await.expect("unapply");
    exec(pool, "INSERT INTO {} ({}) VALUES (3)", &[t, "id"])
        .await
        .unwrap();
    let sql = q(
        pool,
        &format!("SELECT {{}} FROM {{}} WHERE {{}} = {f} ORDER BY {{}}"),
        &["id", t, "flag", "id"],
    );
    let got: Vec<(i64,)> = rustango::sql::raw_query_pool(&sql, Vec::new(), pool)
        .await
        .unwrap();
    assert_eq!(got, [(3,)], "the old default is back");
}

tri_dialect_test!(
    setup: no_setup,
    scenarios: [
        type_change_with_a_default,
        dropped_exclude_unapplies,
        case_insensitive_change_applies,
        db_comment_change_applies,
        db_comment_on_create_and_add_column,
        fk_index_drops,
        fk_index_drop_keeps_a_served_fk,
        fk_index_drop_then_alter,
        fk_alter_then_index_drop,
        fk_index_drop_then_rename,
        fk_index_drop_refuses_an_unknown_shape,
        composite_fk_index_drops,
        auto_pk_widens_its_sequence,
        fk_name_collision_is_refused,
        m2m_column_rename_keeps_rows,
        m2m_column_rename_renames_its_fk,
        fk_column_rename_then_fk_op,
        citext_survives_length_and_type_changes,
        citext_column_on_a_fresh_database,
        on_delete_reaches_an_existing_table,
        on_delete_reaches_without_a_transaction,
        no_action_is_not_a_change,
        set_default_is_enforced_or_refused,
        set_default_is_refused_by_create_tables,
        on_delete_and_drop_in_one_migration,
        hand_named_and_composite_fks_survive,
        rebuild_uses_the_shape_at_its_op,
        rebuild_before_rename_keeps_checks,
        fk_unique_drop_before_rename,
        fk_comes_back_under_names_at_its_op,
        on_delete_change_before_rename,
        composite_fk_add_before_rename,
        composite_fk_add_before_target_and_column_renames,
        declared_index_survives_unique_drop_before_rename,
        rebuild_checks_only_its_own_orphans,
        legacy_pg_runner_replaces_the_fk,
        cross_ledger_squash_runs_its_other_changes,
        unique_drop_finds_the_live_name,
        alter_then_add_on_one_table,
        cross_ledger_squash_already_applied_fakes,
        not_null_before_default_still_fills,
        unique_drop_keeps_a_declared_index,
        unique_drop_after_a_rename,
        alter_column_unapplies,
        rebuild_keeps_unknown_columns,
        unique_column_drops,
        unique_drops_on_long_names,
        add_column_keeps_fk_and_unique,
        fk_column_drops,
        now_column_adds_to_a_filled_table,
        now_column_adds_without_a_transaction,
        uuid_column_adds_to_a_filled_table,
        renamed_fk_column_drops,
        long_named_fk_column_drops,
        add_fk_column_with_default_to_filled_table,
        long_fk_names_apply,
        column_drops_after_its_index_and_check,
        tables_drop_child_first,
        m2m_drops_before_its_tables,
        composite_fk_drops_before_its_parent,
        type_change_is_not_undone_by_max_length,
        shrinking_max_length_refuses_to_truncate,
        type_change_into_a_string_keeps_its_length,
        edited_check_is_replaced,
        edited_composite_fk_is_replaced,
        edited_m2m_is_replaced,
        edited_exclude_is_replaced,
        not_null_with_default_backfills,
    ]
);
