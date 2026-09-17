//! Django parity — `ForeignKey(on_delete=...)`. rustango spells the
//! attribute as `#[rustango(fk = "<table>", on_delete = "cascade")]`.
//! The migration writer renders `ON DELETE <action>` after the FK
//! constraint clause; the runtime DB enforces the action when the
//! referenced row goes.

#![cfg(feature = "sqlite")]

use rustango::core::{OnDeleteAction, Relation};
use rustango::migrate::ddl::{create_constraints_sql_with_dialect, create_table_sql_with_dialect};
use rustango::sql::{sqlx, Sqlite};
use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_author")]
#[allow(dead_code)]
pub struct Author {
    #[rustango(primary_key)]
    pub id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_post_cascade")]
#[allow(dead_code)]
pub struct PostCascade {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_author", on = "id", on_delete = "cascade")]
    pub author_id: i64,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_post_set_null")]
#[allow(dead_code)]
pub struct PostSetNull {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_author", on = "id", on_delete = "set_null")]
    pub author_id: Option<i64>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "fkod_post_default")]
#[allow(dead_code)]
pub struct PostDefault {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(fk = "fkod_author", on = "id")]
    pub author_id: i64,
}

#[test]
fn schema_carries_fk_on_delete_action() {
    let cascade = <PostCascade as rustango::core::Model>::SCHEMA;
    let author_id = cascade
        .scalar_fields()
        .find(|f| f.name == "author_id")
        .expect("author_id field");
    assert_eq!(author_id.fk_on_delete, Some(OnDeleteAction::Cascade));
    assert!(matches!(author_id.relation, Some(Relation::Fk { .. })));

    let set_null = <PostSetNull as rustango::core::Model>::SCHEMA;
    let sn_field = set_null
        .scalar_fields()
        .find(|f| f.name == "author_id")
        .unwrap();
    assert_eq!(sn_field.fk_on_delete, Some(OnDeleteAction::SetNull));

    let default = <PostDefault as rustango::core::Model>::SCHEMA;
    let d_field = default
        .scalar_fields()
        .find(|f| f.name == "author_id")
        .unwrap();
    assert_eq!(
        d_field.fk_on_delete, None,
        "no on_delete attr → None → no ON DELETE clause"
    );
}

#[test]
fn ddl_renders_on_delete_clause() {
    // PR #720 — on SQLite the FK constraint is emitted INLINE inside
    // CREATE TABLE (since SQLite has no `ALTER TABLE ADD CONSTRAINT
    // FOREIGN KEY`); `create_constraints_sql_with_dialect` returns
    // empty for SQLite. Inspect `create_table_sql_with_dialect`
    // instead so the test verifies the on-delete clause where it
    // actually lands.
    let schema = <PostCascade as rustango::core::Model>::SCHEMA;
    let post_hoc = create_constraints_sql_with_dialect(&Sqlite, schema);
    assert!(
        post_hoc.is_empty(),
        "SQLite emits FK constraints inline in CREATE TABLE, not as post-hoc ALTERs; \
         create_constraints_sql_with_dialect should be empty"
    );
    let create = create_table_sql_with_dialect(&Sqlite, schema);
    assert!(
        create.contains("ON DELETE CASCADE"),
        "missing ON DELETE CASCADE in CREATE TABLE: {create}"
    );

    let null_schema = <PostSetNull as rustango::core::Model>::SCHEMA;
    let null_create = create_table_sql_with_dialect(&Sqlite, null_schema);
    assert!(
        null_create.contains("ON DELETE SET NULL"),
        "missing ON DELETE SET NULL in CREATE TABLE: {null_create}"
    );

    let default_schema = <PostDefault as rustango::core::Model>::SCHEMA;
    let default_create = create_table_sql_with_dialect(&Sqlite, default_schema);
    assert!(
        !default_create.contains("ON DELETE"),
        "no on_delete → no ON DELETE clause; got: {default_create}"
    );
}

#[tokio::test]
async fn cascade_deletes_dependent_rows_at_runtime() {
    // SQLite needs `PRAGMA foreign_keys=ON` per-connection for FK
    // actions to fire. Verify the migration writer's `ON DELETE
    // CASCADE` clause actually causes child rows to disappear when
    // the parent is removed.
    let pool = sqlx::SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool");
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE fkod_author (id INTEGER PRIMARY KEY)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE fkod_post_cascade (\
            id INTEGER PRIMARY KEY, \
            author_id INTEGER NOT NULL REFERENCES fkod_author(id) ON DELETE CASCADE)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO fkod_author (id) VALUES (1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO fkod_post_cascade (id, author_id) VALUES (10, 1), (11, 1)")
        .execute(&pool)
        .await
        .unwrap();
    let (before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fkod_post_cascade")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 2, "two posts before delete");

    sqlx::query("DELETE FROM fkod_author WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();

    let (after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fkod_post_cascade")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 0, "CASCADE removed both child rows");
}

// =====================================================================
// The path deployments actually run — #1549.
//
// Everything above renders from a `ModelSchema`, which is the path
// `ddl.rs` owns and which always emitted `ON DELETE` correctly. System
// migrations and `testkit::migrate_framework` do **not** take that
// path: they go `SchemaSnapshot::from_models` -> `detect_changes` ->
// `render_changes_split_with_dialect`.
//
// `RelationSnapshot` had no field for the action, so it was dropped the
// moment a snapshot was built and the clause never reached any
// database. Every framework FK in a live PostgreSQL carried
// `confdeltype='a'` (NO ACTION), and on MySQL deleting a parent row
// raised 1451 — a declared cascade had become a hard refusal.
//
// These tests render from a snapshot. A guard that renders from a
// `ModelSchema` cannot fail on this bug, which is why the old one
// passed for months.
// =====================================================================

use rustango::migrate::{detect_changes, render_changes_split_with_dialect, SchemaSnapshot};

/// Every statement a snapshot render produces for these models.
fn snapshot_ddl(dialect: &dyn rustango::sql::Dialect) -> Vec<String> {
    let models = [
        <Author as rustango::core::Model>::SCHEMA,
        <PostCascade as rustango::core::Model>::SCHEMA,
        <PostSetNull as rustango::core::Model>::SCHEMA,
        <PostDefault as rustango::core::Model>::SCHEMA,
    ];
    let current = SchemaSnapshot::from_models(&models);
    let changes = detect_changes(&SchemaSnapshot::default(), &current);
    let batch = render_changes_split_with_dialect(&changes, &current, dialect).expect("render");
    let mut out = batch.immediate;
    out.extend(batch.deferred_fks);
    out
}

/// The declared action survives into the snapshot.
#[test]
fn the_snapshot_carries_the_on_delete_action() {
    let models = [<PostCascade as rustango::core::Model>::SCHEMA];
    let snap = SchemaSnapshot::from_models(&models);
    let table = snap.table("fkod_post_cascade").expect("table in snapshot");
    let field = table
        .fields
        .iter()
        .find(|f| f.column == "author_id")
        .expect("author_id column");
    let rel = field.fk.as_ref().expect("author_id carries an fk");
    assert_eq!(
        rel.on_delete.as_deref(),
        Some("CASCADE"),
        "the declared `on_delete` was dropped when the snapshot was built, so every \
         migration rendered from it emits a constraint with no ON DELETE clause"
    );
}

/// …and reaches the SQL, on every dialect.
#[test]
fn the_snapshot_render_emits_on_delete() {
    for (name, dialect) in dialects() {
        let sql = snapshot_ddl(dialect).join("\n");
        assert!(
            sql.contains("ON DELETE CASCADE"),
            "{name}: a snapshot render emitted no `ON DELETE CASCADE` for \
             fkod_post_cascade. This is the path system migrations take, so the \
             declared action never reaches the database.\n{sql}"
        );
        assert!(
            sql.contains("ON DELETE SET NULL"),
            "{name}: a snapshot render emitted no `ON DELETE SET NULL` for \
             fkod_post_set_null.\n{sql}"
        );
    }
}

/// A FK with no declared action must not grow one.
#[test]
fn an_undeclared_action_stays_absent() {
    for (name, dialect) in dialects() {
        let stmts = snapshot_ddl(dialect);
        let for_default: Vec<&String> = stmts
            .iter()
            .filter(|s| s.contains("fkod_post_default"))
            .collect();
        assert!(
            !for_default.is_empty(),
            "{name}: nothing rendered for fkod_post_default"
        );
        for s in for_default {
            assert!(
                !s.contains("ON DELETE"),
                "{name}: a FK that declares no action grew one: {s}"
            );
        }
    }
}

fn dialects() -> Vec<(&'static str, &'static dyn rustango::sql::Dialect)> {
    let mut v: Vec<(&'static str, &'static dyn rustango::sql::Dialect)> = Vec::new();
    #[cfg(feature = "sqlite")]
    v.push(("sqlite", &rustango::sql::Sqlite));
    #[cfg(feature = "postgres")]
    v.push(("postgres", &rustango::sql::Postgres));
    #[cfg(feature = "mysql")]
    v.push(("mysql", &rustango::sql::MySql));
    v
}

/// The database enforces it, not just the string.
///
/// Renders through the snapshot path, executes it against a real
/// SQLite, then deletes a parent and checks the child went with it.
/// An emission test proves only that the writer emitted what its author
/// intended; this proves the server agrees.
#[tokio::test]
async fn sqlite_actually_cascades_the_delete() {
    use rustango::sql::Pool;

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .expect("enable fk enforcement");

    for stmt in snapshot_ddl(&Sqlite) {
        sqlx::query(&stmt)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("apply `{stmt}`: {e}"));
    }

    sqlx::query("INSERT INTO fkod_author (id) VALUES (1)")
        .execute(&pool)
        .await
        .expect("author");
    sqlx::query("INSERT INTO fkod_post_cascade (id, author_id) VALUES (10, 1)")
        .execute(&pool)
        .await
        .expect("post");

    sqlx::query("DELETE FROM fkod_author WHERE id = 1")
        .execute(&pool)
        .await
        .expect(
            "the parent delete must be permitted — a dropped CASCADE arrives as \
                 NO ACTION, which refuses this with a constraint violation",
        );

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fkod_post_cascade")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        remaining.0, 0,
        "the parent was deleted but the child survived — the constraint reached the \
         database without its ON DELETE action"
    );

    let _ = Pool::Sqlite(pool);
}

// =====================================================================
// The upgrade path — #1549.
//
// Adding `on_delete` to `RelationSnapshot` changes what an *existing*
// snapshot compares equal to. Every snapshot written before this release
// has `on_delete: None`; a freshly built one has `Some("CASCADE")`. A
// plain `pf.fk != cf.fk` therefore reports "fk changed" for every FK
// that declares an action, and `make_migrations` rejects any non-empty
// unsupported-change list — so an upgrade with **zero model changes**
// fails outright.
//
// It reached every existing tenancy project, not just media ones:
// eleven framework FKs declare `cascade` (ten in `tenancy`), and
// `fold_in_framework_tables` puts them in every project's snapshot.
//
// Nothing in the existing migrate suite could see this, because nothing
// simulates an old snapshot meeting new models. That is what these do.
// =====================================================================

use rustango::migrate::detect_unsupported_field_changes;

/// A snapshot as the previous release wrote it: no `on_delete` key.
fn snapshot_without_on_delete() -> SchemaSnapshot {
    let models = [
        <Author as rustango::core::Model>::SCHEMA,
        <PostCascade as rustango::core::Model>::SCHEMA,
        <PostSetNull as rustango::core::Model>::SCHEMA,
        <PostDefault as rustango::core::Model>::SCHEMA,
    ];
    let current = SchemaSnapshot::from_models(&models);
    // Round-trip through JSON with the key stripped — exactly the bytes
    // a pre-#1549 release produced, rather than a hand-built struct that
    // could drift from the real serialized form.
    let mut v: serde_json::Value = serde_json::to_value(&current).expect("snapshot serializes");
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("on_delete");
                for (_, x) in m.iter_mut() {
                    strip(x);
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    serde_json::from_value(v).expect("a pre-#1549 snapshot still deserializes")
}

/// Upgrading with no model change must not look like a schema change.
#[test]
fn an_upgrade_is_not_a_schema_change() {
    let old = snapshot_without_on_delete();
    let new = SchemaSnapshot::from_models(&[
        <Author as rustango::core::Model>::SCHEMA,
        <PostCascade as rustango::core::Model>::SCHEMA,
        <PostSetNull as rustango::core::Model>::SCHEMA,
        <PostDefault as rustango::core::Model>::SCHEMA,
    ]);

    let problems = detect_unsupported_field_changes(&old, &new);
    assert!(
        problems.is_empty(),
        "an upgrade with zero model changes was reported as an unsupported schema \
         change, which makes `make_migrations` refuse to run at all — and the advice \
         it prints cannot be followed, because there is no AlterFk operation to \
         author:\n  {}",
        problems.join("\n  ")
    );
}

/// A *real* action change is still reported.
///
/// Without this the fix above could be "ignore `on_delete` entirely",
/// which would be the pre-#1549 behaviour wearing a new coat.
#[test]
fn a_changed_on_delete_action_is_still_detected() {
    let base = SchemaSnapshot::from_models(&[<PostCascade as rustango::core::Model>::SCHEMA]);
    let mut changed = base.clone();
    let field = changed.tables[0]
        .fields
        .iter_mut()
        .find(|f| f.column == "author_id")
        .expect("author_id");
    field.fk.as_mut().expect("fk").on_delete = Some("SET NULL".to_owned());

    let problems = detect_unsupported_field_changes(&base, &changed);
    assert!(
        problems.iter().any(|p| p.contains("on_delete")),
        "changing a declared action from CASCADE to SET NULL was not reported: \
         {problems:?}"
    );
}

/// A snapshot written by this release still loads on the previous one.
#[test]
fn a_new_snapshot_is_readable_by_the_old_shape() {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldRelation {
        kind: String,
        to: String,
        on: String,
    }
    let new = SchemaSnapshot::from_models(&[<PostCascade as rustango::core::Model>::SCHEMA]);
    let json = serde_json::to_value(&new).expect("serialize");
    let rel = &json["tables"][0]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .find(|f| f["column"] == "author_id")
        .expect("author_id")["fk"];
    assert_eq!(
        rel["on_delete"], "CASCADE",
        "the new snapshot should carry the action"
    );
    serde_json::from_value::<OldRelation>(rel.clone())
        .expect("the previous release's three-field shape must still parse this");
}

/// A snapshot with an action survives a full JSON round trip.
///
/// `a_new_snapshot_is_readable_by_the_old_shape` above checks the
/// serialize half — the key is present with the right value — and
/// `snapshot_without_on_delete` checks that a snapshot *missing* the
/// key still loads. Neither reads the value back into a
/// `SchemaSnapshot`, and that is the direction that matters: snapshots
/// are written to disk as JSON and loaded again on the next
/// `make_migrations`, and `#[serde(default)]` means a deserialize that
/// misses the field produces `None` **silently** — the exact shape of
/// #1549, one release later and harder to see, because the in-memory
/// `from_models` test would still pass.
#[test]
fn on_delete_survives_a_json_round_trip() {
    let before = SchemaSnapshot::from_models(&[
        <Author as rustango::core::Model>::SCHEMA,
        <PostCascade as rustango::core::Model>::SCHEMA,
        <PostSetNull as rustango::core::Model>::SCHEMA,
        <PostDefault as rustango::core::Model>::SCHEMA,
    ]);
    let json = serde_json::to_string(&before).expect("serialize");
    let after: SchemaSnapshot = serde_json::from_str(&json).expect("deserialize");

    let action_of = |snap: &SchemaSnapshot, table: &str| -> Option<String> {
        snap.table(table)?
            .fields
            .iter()
            .find(|f| f.column == "author_id")?
            .fk
            .as_ref()?
            .on_delete
            .clone()
    };

    for (table, expected) in [
        ("fkod_post_cascade", Some("CASCADE")),
        ("fkod_post_set_null", Some("SET NULL")),
        // No action declared — must stay absent, not become a default.
        ("fkod_post_default", None),
    ] {
        assert_eq!(
            action_of(&before, table).as_deref(),
            expected,
            "control: `{table}` should carry {expected:?} before serializing"
        );
        assert_eq!(
            action_of(&after, table).as_deref(),
            expected,
            "`{table}` lost its `on_delete` in a JSON round trip. Snapshots live on \
             disk between runs, so this is the path every existing project takes — \
             and `#[serde(default)]` turns the loss into `None` with no error"
        );
    }
}

/// An absent action must stay absent in the serialized bytes.
///
/// `skip_serializing_if` is what keeps existing snapshot JSON
/// byte-identical for a project that declares no action anywhere. Drop
/// it and every such project sees a diff on its next `make_migrations`
/// — noise that looks like a schema change and is not one.
#[test]
fn a_declared_nothing_writes_nothing() {
    let snap = SchemaSnapshot::from_models(&[<PostDefault as rustango::core::Model>::SCHEMA]);
    let json = serde_json::to_string(&snap).expect("serialize");
    assert!(
        !json.contains("on_delete"),
        "a model declaring no `on_delete` still wrote the key, so every existing \
         snapshot gains a diff on the next run that is not a schema change:\n{json}"
    );
}
