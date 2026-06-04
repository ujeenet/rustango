//! Index access-method DSL (issue #34). Adds `method = "..."` to the
//! `#[rustango(index(...))]` attribute and threads it through the
//! migration snapshot + DDL writer. PG emits `USING <method>`; MySQL
//! only honours `hash`; SQLite always emits btree (silent fallback).
//!
//! ORM-extractability note: the `IndexMethod` enum + `index_method_clause`
//! Dialect method live in `core/` + `sql/`, with no admin/tenancy
//! coupling.

use rustango::core::{IndexMethod, IndexSchema};
#[cfg(feature = "mysql")]
use rustango::sql::MySql;
#[cfg(feature = "sqlite")]
use rustango::sql::Sqlite;
use rustango::sql::{Dialect, Postgres};

// ---------- IndexMethod surface ----------

#[test]
fn index_method_as_str_round_trips() {
    for method in [
        IndexMethod::BTree,
        IndexMethod::Gin,
        IndexMethod::Gist,
        IndexMethod::Brin,
        IndexMethod::SpGist,
        IndexMethod::Hash,
        IndexMethod::Bloom,
    ] {
        let token = method.as_str();
        let parsed = IndexMethod::from_token(token);
        assert_eq!(parsed, method, "round-trip for {token}");
    }
}

#[test]
fn index_method_from_token_falls_back_to_btree() {
    // Older snapshot (pre-#34) carries no method field → "" default.
    // Stray / unknown tokens also fall back to btree so we never
    // emit invalid SQL.
    assert_eq!(IndexMethod::from_token(""), IndexMethod::BTree);
    assert_eq!(IndexMethod::from_token("unknown"), IndexMethod::BTree);
    assert_eq!(IndexMethod::from_token("BTREE"), IndexMethod::BTree); // case-sensitive
}

#[test]
fn index_method_postgres_only_classification() {
    assert!(!IndexMethod::BTree.is_postgres_only());
    assert!(!IndexMethod::Hash.is_postgres_only()); // MySQL MEMORY engine
    assert!(IndexMethod::Gin.is_postgres_only());
    assert!(IndexMethod::Gist.is_postgres_only());
    assert!(IndexMethod::Brin.is_postgres_only());
    assert!(IndexMethod::SpGist.is_postgres_only());
    assert!(IndexMethod::Bloom.is_postgres_only());
}

#[test]
fn index_schema_default_method_is_btree() {
    // Compile-time guarantee: omitting `method` yields BTree.
    let idx = IndexSchema {
        name: "idx_foo",
        columns: &["foo"],
        unique: false,
        method: IndexMethod::default(),
        where_clause: None,
        include: &[],
    };
    assert_eq!(idx.method, IndexMethod::BTree);
}

// ---------- Dialect emission ----------

#[test]
fn pg_emits_using_clause_for_non_btree() {
    let d = Postgres;
    assert_eq!(d.index_method_clause("btree"), "");
    assert_eq!(d.index_method_clause(""), "");
    assert_eq!(d.index_method_clause("gin"), " USING gin");
    assert_eq!(d.index_method_clause("gist"), " USING gist");
    assert_eq!(d.index_method_clause("brin"), " USING brin");
    assert_eq!(d.index_method_clause("spgist"), " USING spgist");
    assert_eq!(d.index_method_clause("hash"), " USING hash");
    assert_eq!(d.index_method_clause("bloom"), " USING bloom");
}

#[cfg(feature = "mysql")]
#[test]
fn mysql_honours_hash_drops_others() {
    let d = MySql;
    assert_eq!(d.index_method_clause("btree"), "");
    assert_eq!(d.index_method_clause("hash"), " USING HASH");
    // Every other method silently degrades to MySQL's default btree
    // (no clause emitted). Issue #34 docs cover the rationale.
    assert_eq!(d.index_method_clause("gin"), "");
    assert_eq!(d.index_method_clause("gist"), "");
    assert_eq!(d.index_method_clause("brin"), "");
    assert_eq!(d.index_method_clause("spgist"), "");
    assert_eq!(d.index_method_clause("bloom"), "");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_drops_every_method_clause() {
    let d = Sqlite;
    // SQLite has no `USING` clause; every token degrades to btree.
    for token in [
        "btree", "gin", "gist", "brin", "spgist", "hash", "bloom", "",
    ] {
        assert_eq!(
            d.index_method_clause(token),
            "",
            "sqlite should drop `{token}`"
        );
    }
}

// ---------- Macro integration: `#[rustango(index(method = "gin"))]` ----------

#[derive(rustango::Model)]
#[rustango(table = "idx_doc")]
#[allow(dead_code)]
pub struct Doc {
    #[rustango(primary_key)]
    id: i64,
    #[rustango(index(method = "gin"))]
    tags: serde_json::Value,
    #[rustango(index(method = "brin"))]
    created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(index)]
    title: String,
}

#[test]
fn macro_threads_method_through_index_schema() {
    use rustango::core::Model as _;
    let schema = Doc::SCHEMA;
    let by_name: std::collections::HashMap<&str, &IndexSchema> =
        schema.indexes.iter().map(|idx| (idx.name, idx)).collect();

    // The field-level index attributes auto-derive index names as
    // `<table>_<col>_idx`. Confirm each carries the declared method.
    let tags_idx = by_name
        .get("idx_doc_tags_idx")
        .expect("tags index registered");
    assert_eq!(tags_idx.method, IndexMethod::Gin);

    let created_idx = by_name
        .get("idx_doc_created_at_idx")
        .expect("created_at index registered");
    assert_eq!(created_idx.method, IndexMethod::Brin);

    // Bare `#[rustango(index)]` with no method → defaults to btree.
    let title_idx = by_name
        .get("idx_doc_title_idx")
        .expect("title index registered");
    assert_eq!(title_idx.method, IndexMethod::BTree);
}
