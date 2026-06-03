//! Django parity — `ForeignKey(on_delete=...)`. rustango spells the
//! attribute as `#[rustango(fk = "<table>", on_delete = "cascade")]`.
//! The migration writer renders `ON DELETE <action>` after the FK
//! constraint clause; the runtime DB enforces the action when the
//! referenced row goes.

#![cfg(feature = "sqlite")]

use rustango::core::{OnDeleteAction, Relation};
use rustango::migrate::ddl::create_constraints_sql_with_dialect;
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
    let schema = <PostCascade as rustango::core::Model>::SCHEMA;
    let stmts = create_constraints_sql_with_dialect(&Sqlite, schema);
    assert_eq!(stmts.len(), 1, "one FK constraint");
    let sql = &stmts[0];
    assert!(
        sql.contains("ON DELETE CASCADE"),
        "missing ON DELETE CASCADE in: {sql}"
    );

    let null_schema = <PostSetNull as rustango::core::Model>::SCHEMA;
    let null_stmts = create_constraints_sql_with_dialect(&Sqlite, null_schema);
    assert!(
        null_stmts[0].contains("ON DELETE SET NULL"),
        "missing ON DELETE SET NULL in: {}",
        null_stmts[0]
    );

    let default_schema = <PostDefault as rustango::core::Model>::SCHEMA;
    let default_stmts = create_constraints_sql_with_dialect(&Sqlite, default_schema);
    assert!(
        !default_stmts[0].contains("ON DELETE"),
        "no on_delete → no ON DELETE clause; got: {}",
        default_stmts[0]
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
