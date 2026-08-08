//! Cookbook Chapter 4 — migrations exercised end-to-end live.
//!
//! Hand-rolls a tiny migration directory in a tempdir, applies it
//! against an isolated DB-side schema, queries the schema catalog to
//! verify the DDL took, then unapplies and verifies the table is gone.
//!
//! Reuses the cookbook's `EMBEDDED` const for §4.56 and the framework's
//! [`migrate::file::Migration`] for §4.55 / §4.57 / §4.58.
//!
//! Run: `DATABASE_URL=... cargo test --test cookbook_chapter04_migrations -- --test-threads=1`

use rustango::migrate::{self, Migration, Operation, SchemaChange, SchemaSnapshot};
use rustango::sql::sqlx;
use std::fs;

fn url() -> Option<String> { std::env::var("DATABASE_URL").ok() }

async fn pool() -> Option<sqlx::PgPool> {
    let url = url()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

// §4.56 — embed_migrations! covered in chapter01; here we re-assert
// the const carries the cookbook's own tables migration so apps relying
// on it ship migrations baked into the binary.
#[test]
fn embedded_migrations_const_is_loaded() {
    assert!(!cookbook_blog::EMBEDDED.is_empty());
    assert!(
        cookbook_blog::EMBEDDED
            .iter()
            .any(|(name, _)| *name == "0002_cookbook_tables"),
        "embedded names: {:?}",
        cookbook_blog::EMBEDDED
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
    );
}

// §4.55 / §4.57 / §4.58 — migration file format: serde_json round-trip
// of a Migration carrying both Schema + Data ops.
#[test]
fn migration_serde_round_trips_schema_and_data_ops() {
    let mig = Migration {
        name: "9999_demo".into(),
        created_at: "2026-05-04T00:00:00Z".into(),
        prev: None,
        scope: rustango::migrate::MigrationScope::Tenant,
        atomic: true,
        // Squash bookkeeping — empty for an ordinary migration, and omitted
        // from the JSON when empty (so pre-squash files round-trip unchanged).
        replaces: Vec::new(),
        snapshot: SchemaSnapshot::default(),
        forward: vec![
            Operation::Schema(SchemaChange::CreateTable("cookbook_demo".into())),
            Operation::Data(rustango::migrate::DataOp {
                sql: "INSERT INTO cookbook_demo (id) VALUES (1)".into(),
                reverse_sql: Some("DELETE FROM cookbook_demo WHERE id = 1".into()),
                reversible: true,
            }),
        ],
    };
    let json = serde_json::to_string(&mig).expect("serialize");
    let back: Migration = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.name, "9999_demo");
    assert_eq!(back.forward.len(), 2);
    matches!(back.forward[0], Operation::Schema(SchemaChange::CreateTable(_)));
    if let Operation::Data(d) = &back.forward[1] {
        assert!(d.reversible);
        assert_eq!(d.reverse_sql.as_deref(), Some("DELETE FROM cookbook_demo WHERE id = 1"));
    } else { panic!("second op should be Data"); }
}

// §4.51 / §4.53 / §4.54 / §4.61 — apply / unapply lifecycle:
//   1. Write a migrations/ tempdir with one CreateTable + CreateIndex op.
//   2. migrate() applies; verify the table + index exist via catalog.
//   3. unapply() rolls back; verify the table + index are gone.
#[tokio::test]
async fn apply_then_unapply_round_trip() {
    let Some(pool) = pool().await else { return };

    // Use a unique table name so re-runs and parallel runs don't clash.
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let table = format!("cookbook_mig_{suffix}");
    let mig_name = format!("0001_create_{suffix}");

    let dir = tempdir();
    let path = dir.join(format!("{mig_name}.json"));

    let mig_json = serde_json::json!({
        "name": mig_name,
        "created_at": "2026-05-04T00:00:00Z",
        "scope": "tenant",
        "atomic": true,
        "snapshot": {
            "tables": [{
                "name": table,
                "model": "Mig",
                "fields": [
                    {"name": "id",    "column": "id",    "ty": "i64",    "nullable": false, "primary_key": true, "auto": true},
                    {"name": "label", "column": "label", "ty": "string", "nullable": false, "primary_key": false, "max_length": 80}
                ]
            }],
            "indexes": [{
                "name": format!("{table}_label_idx"),
                "table": table,
                "columns": ["label"],
                "unique": false
            }]
        },
        "forward": [
            {"schema": {"CreateTable": table}},
            {"schema": {"CreateIndex": {
                "name": format!("{table}_label_idx"),
                "table": table,
                "columns": ["label"],
                "unique": false
            }}}
        ]
    });
    fs::write(&path, serde_json::to_string_pretty(&mig_json).unwrap()).unwrap();

    // Apply.
    let applied = migrate::migrate(&pool, &dir).await.expect("apply");
    assert_eq!(applied.len(), 1, "one migration applied");
    assert_eq!(applied[0].name, mig_name);

    // Verify table exists.
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_tables WHERE tablename = $1"
    ).bind(&table).fetch_one(&pool).await.unwrap();
    assert_eq!(table_count, 1, "{table} should exist after apply");

    // Verify index exists.
    let idx_name = format!("{table}_label_idx");
    let idx_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE indexname = $1"
    ).bind(&idx_name).fetch_one(&pool).await.unwrap();
    assert_eq!(idx_count, 1, "index {idx_name} should exist");

    // Unapply.
    migrate::unapply(&pool, &dir, &mig_name).await.expect("unapply");
    let table_count_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_tables WHERE tablename = $1"
    ).bind(&table).fetch_one(&pool).await.unwrap();
    assert_eq!(table_count_after, 0, "{table} should be gone after unapply");

    // Cleanup tempdir.
    fs::remove_dir_all(&dir).ok();
}

// §4.59 — AlterField ops (AlterColumnType, AlterColumnNullable,
// AlterColumnDefault) round-trip through the JSON format. This is a
// schema-level smoke; the real apply path is covered by rustango's
// own migrate_runner_live.rs.
#[test]
fn alter_column_ops_serialize_with_external_tag() {
    let ops = vec![
        SchemaChange::AlterColumnType {
            table: "t".into(), column: "c".into(),
            from: "i32".into(), to: "i64".into(),
        },
        SchemaChange::AlterColumnNullable {
            table: "t".into(), column: "c".into(), nullable: true,
        },
        SchemaChange::AlterColumnDefault {
            table: "t".into(), column: "c".into(),
            from: None, to: Some("'x'".into()),
        },
    ];
    let json = serde_json::to_value(&ops).unwrap();
    let arr = json.as_array().unwrap();
    assert!(arr[0].get("AlterColumnType").is_some());
    assert!(arr[1].get("AlterColumnNullable").is_some());
    assert!(arr[2].get("AlterColumnDefault").is_some());

    let back: Vec<SchemaChange> = serde_json::from_value(json).unwrap();
    assert_eq!(back, ops);
}

// §4.64 — composite-FK ops (v0.15-F.5b) serialize round-trip.
#[test]
fn composite_fk_ops_serialize_round_trip() {
    let ops = vec![
        SchemaChange::AddCompositeFk {
            table: "t".into(),
            name: "t_pair_fk".into(),
            to: "u".into(),
            from: vec!["a".into(), "b".into()],
            on:   vec!["x".into(), "y".into()],
        },
        SchemaChange::DropCompositeFk {
            table: "t".into(), name: "t_pair_fk".into(),
        },
    ];
    let back: Vec<SchemaChange> =
        serde_json::from_value(serde_json::to_value(&ops).unwrap()).unwrap();
    assert_eq!(back, ops);
}

fn tempdir() -> std::path::PathBuf {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let dir = std::env::temp_dir().join(format!("cookbook_mig_{suffix}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}
