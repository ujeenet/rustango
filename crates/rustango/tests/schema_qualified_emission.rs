//! `#[rustango(schema = "…")]` (#2820): a model on a table outside the
//! connection's database is named `<schema>.<table>` wherever the table is
//! the FROM, JOIN or DML target, and only there. Columns keep the bare
//! `<table>.<column>` form: a qualified table is still exposed under its
//! own name, so qualifying them too would be redundant on every backend.
//!
//! SQL text only; `schema_qualified_mysql_live` runs the same shapes
//! against a second database on a real MySQL server.

#![cfg(feature = "mysql")]

use rustango::core::{Column as _, Model as _};
use rustango::sql::{Auto, Dialect as _, ForeignKey, MySql};
use rustango::Model;

/// Lives in `config_db`, not in the connection's database.
#[derive(Model, Debug, Clone)]
#[rustango(table = "qe_kinds", schema = "config_db", managed = false)]
pub struct Kind {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub name: String,
}

/// In the connection's own database, pointing at a `Kind`.
#[derive(Model, Debug, Clone)]
#[rustango(table = "qe_items")]
pub struct Item {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 50)]
    pub title: String,
    pub kind: ForeignKey<Kind>,
}

#[test]
fn schema_reaches_the_model_schema() {
    assert_eq!(Kind::SCHEMA.table_schema, Some("config_db"));
    assert_eq!(Kind::SCHEMA.table, "qe_kinds");
    assert_eq!(Item::SCHEMA.table_schema, None);
}

#[test]
fn select_names_the_qualified_table() {
    let q = Kind::objects()
        .where_(Kind::name.eq("person"))
        .compile()
        .unwrap();
    let sql = MySql.compile_select(&q).unwrap().sql;
    assert!(sql.contains("FROM `config_db`.`qe_kinds`"), "{sql}");
    // The table itself is never quoted as one identifier.
    assert!(!sql.contains("`config_db.qe_kinds`"), "{sql}");
}

#[test]
fn join_to_a_qualified_table_qualifies_only_the_join_target() {
    let q = Item::objects().select_related("kind").compile().unwrap();
    let sql = MySql.compile_select(&q).unwrap().sql;
    assert!(sql.contains("FROM `qe_items`"), "{sql}");
    assert!(sql.contains("JOIN `config_db`.`qe_kinds`"), "{sql}");
    assert!(!sql.contains("`config_db`.`qe_items`"), "{sql}");
}

#[test]
fn unqualified_models_are_unchanged() {
    let q = Item::objects().compile().unwrap();
    let sql = MySql.compile_select(&q).unwrap().sql;
    assert!(sql.contains("FROM `qe_items`"), "{sql}");
    assert!(!sql.contains("config_db"), "{sql}");
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_qualifies_the_same_way() {
    use rustango::sql::Postgres;
    let q = Kind::objects().compile().unwrap();
    let sql = Postgres.compile_select(&q).unwrap().sql;
    assert!(sql.contains(r#"FROM "config_db"."qe_kinds""#), "{sql}");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_qualifies_the_same_way() {
    use rustango::sql::Sqlite;
    let q = Kind::objects().compile().unwrap();
    let sql = Sqlite.compile_select(&q).unwrap().sql;
    assert!(sql.contains(r#"FROM "config_db"."qe_kinds""#), "{sql}");
}
