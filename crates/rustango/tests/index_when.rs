//! Django parity — `Index(fields=[...], condition=Q(...))` partial index.
//! rustango spells the attribute as `#[rustango(index_when(...))]` —
//! sibling of `unique_when` which emits the UNIQUE variant.
//!
//! Both forms drop a `WHERE <expr>` tail on the `CREATE INDEX` so PG +
//! SQLite skip rows that don't match the predicate. MySQL has no native
//! partial-index support; the writer emits a plain CREATE INDEX with a
//! tracing warning so the migration still applies.
//!
//! Issue #319 follow-up.

use rustango::Model;

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "idxwhen_post",
    // Non-unique partial index — useful when 90% of rows have the same
    // `status = "draft"` and you only want fast lookups on the 10%
    // published. Django shape: `Index(fields=["status"], condition=Q(deleted_at__isnull=True))`.
    index_when(
        columns = "status",
        condition = "deleted_at IS NULL",
        name = "active_status_idx",
    ),
    // Composite partial — multi-column + non-default method.
    index_when(
        columns = "tenant_id, created_at",
        condition = "deleted_at IS NULL",
        name = "active_tenant_created_idx",
        method = "btree",
    )
)]
#[allow(dead_code)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: i64,
    #[rustango(max_length = 16)]
    pub status: String,
    pub tenant_id: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Model, Debug, Clone)]
#[rustango(table = "idxwhen_plain")]
#[allow(dead_code)]
pub struct Plain {
    #[rustango(primary_key)]
    pub id: i64,
    pub status: String,
}

#[test]
fn schema_carries_partial_index_with_where_clause() {
    let schema = <Post as rustango::core::Model>::SCHEMA;
    let active = schema
        .indexes
        .iter()
        .find(|i| i.name == "active_status_idx")
        .expect("missing active_status_idx");
    assert!(!active.unique, "index_when should emit a non-unique index");
    assert_eq!(active.columns, &["status"]);
    assert_eq!(active.where_clause, Some("deleted_at IS NULL"));

    let composite = schema
        .indexes
        .iter()
        .find(|i| i.name == "active_tenant_created_idx")
        .expect("missing active_tenant_created_idx");
    assert!(!composite.unique);
    assert_eq!(composite.columns, &["tenant_id", "created_at"]);
    assert_eq!(composite.where_clause, Some("deleted_at IS NULL"));
}

#[test]
fn plain_model_has_no_partial_indexes() {
    let plain = <Plain as rustango::core::Model>::SCHEMA;
    assert!(plain.indexes.iter().all(|i| i.where_clause.is_none()));
}

#[test]
fn diff_emits_create_index_with_where_clause() {
    // End-to-end: `makemigrations`-shape diff. A fresh-table snapshot
    // should emit `CreateIndex` ops carrying the partial WHERE clause
    // so the migration-runner's DDL render emits the right SQL.
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

    let active = changes
        .iter()
        .find(
            |c| matches!(c, SchemaChange::CreateIndex { name, .. } if name == "active_status_idx"),
        )
        .expect("missing CreateIndex for active_status_idx");
    match active {
        SchemaChange::CreateIndex {
            unique,
            where_clause,
            columns,
            ..
        } => {
            assert!(!unique, "index_when is non-unique");
            assert_eq!(where_clause.as_deref(), Some("deleted_at IS NULL"));
            assert_eq!(columns, &vec!["status".to_owned()]);
        }
        _ => unreachable!(),
    }
}
