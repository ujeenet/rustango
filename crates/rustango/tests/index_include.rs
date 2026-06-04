//! Django parity — `Index(fields=[...], include=[...])` covering
//! index. PG 11+ ships `CREATE INDEX … (key_cols) INCLUDE (non_key)`
//! so non-key columns travel with the index leaf for index-only
//! scans. MySQL/SQLite have no equivalent — the migration writer
//! drops the clause with a `tracing::warn!` so the rest of the
//! migration still applies.
//!
//! rustango spells the attribute as `include = "col1, col2"` on
//! `index_when(...)` / `unique_when(...)` (this PR wires both). Both
//! `IndexSchema::include` and `IndexSnapshot::include` carry the
//! columns across the inventory → diff → render pipeline.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "idxinc_post",
    // Covering index on `status` that also returns `title, created_at`
    // without a heap visit.
    index_when(
        columns = "status",
        condition = "deleted_at IS NULL",
        name = "active_post_cover_idx",
        include = "title, created_at",
    ),
    // UNIQUE variant — composite key plus a covering payload.
    unique_when(
        columns = "tenant_id, slug",
        condition = "deleted_at IS NULL",
        name = "active_post_slug_unique",
        include = "title",
    ),
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    pub tenant_id: i64,
    #[rustango(max_length = 16)]
    pub status: String,
    #[rustango(max_length = 200)]
    pub title: String,
    #[rustango(max_length = 200)]
    pub slug: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "idxinc_plain",
    index_when(columns = "status", condition = "x IS NULL", name = "plain_idx")
)]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16)]
    pub status: String,
    pub x: Option<i64>,
}

#[test]
fn schema_carries_include_columns() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    let cover = schema
        .indexes
        .iter()
        .find(|i| i.name == "active_post_cover_idx")
        .expect("missing active_post_cover_idx");
    assert_eq!(cover.include, &["title", "created_at"]);

    let unique_cover = schema
        .indexes
        .iter()
        .find(|i| i.name == "active_post_slug_unique")
        .expect("missing active_post_slug_unique");
    assert!(unique_cover.unique);
    assert_eq!(unique_cover.include, &["title"]);
}

#[test]
fn plain_index_has_no_include_columns() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    let only = plain
        .indexes
        .iter()
        .find(|i| i.name == "plain_idx")
        .expect("missing plain_idx");
    assert!(only.include.is_empty());
}

#[test]
fn diff_emits_create_index_with_include() {
    use rustango::migrate::diff::{detect_changes, SchemaChange};
    use rustango::migrate::snapshot::SchemaSnapshot;

    let schema = <Post as rustango::core::Model>::SCHEMA;
    let prev = SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
    };
    let current = SchemaSnapshot::from_models(&[schema]);
    let changes = detect_changes(&prev, &current);

    let cover = changes
        .iter()
        .find(|c| matches!(c, SchemaChange::CreateIndex { name, .. } if name == "active_post_cover_idx"))
        .expect("missing CreateIndex for cover idx");
    match cover {
        SchemaChange::CreateIndex { include, .. } => {
            assert_eq!(include, &vec!["title".to_owned(), "created_at".to_owned()]);
        }
        _ => unreachable!(),
    }
}
