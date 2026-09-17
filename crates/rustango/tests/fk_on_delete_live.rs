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
