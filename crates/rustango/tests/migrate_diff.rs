//! Pure-diff invariants and render ordering tests.
//!
//! Focuses on the `SchemaChange` IR and `render_changes` ordering —
//! the pieces that `make_migrations` and the runner both depend on.
//! Live PG tests live in `migrate_runner.rs`.

use rustango::migrate::{
    detect_changes, render_changes, SchemaChange, SchemaSnapshot, TableSnapshot,
};

// ---------------- helpers ----------------

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
    }
}

fn user_table() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "diff_user",
        "model": "DiffUser",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "name", "column": "name", "ty": "string", "nullable": false, "primary_key": false, "max_length": 32}
        ]
    })).unwrap()
}

fn post_table() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "diff_post",
        "model": "DiffPost",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {
                "name": "author_id", "column": "author_id", "ty": "i64",
                "nullable": false, "primary_key": false,
                "fk": {"kind": "fk", "to": "diff_user", "on": "id"}
            }
        ]
    }))
    .unwrap()
}

// ---------------- SchemaChange serde ----------------

#[test]
fn schema_change_create_table_round_trips() {
    let c = SchemaChange::CreateTable("foo".into());
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json, serde_json::json!({"CreateTable": "foo"}));
    let back: SchemaChange = serde_json::from_value(json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn schema_change_drop_table_round_trips() {
    let c = SchemaChange::DropTable("foo".into());
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json, serde_json::json!({"DropTable": "foo"}));
    let back: SchemaChange = serde_json::from_value(json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn schema_change_add_column_round_trips() {
    let c = SchemaChange::AddColumn {
        table: "t".into(),
        column: "c".into(),
    };
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"AddColumn": {"table": "t", "column": "c"}})
    );
    let back: SchemaChange = serde_json::from_value(json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn schema_change_drop_column_round_trips() {
    let c = SchemaChange::DropColumn {
        table: "t".into(),
        column: "c".into(),
    };
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"DropColumn": {"table": "t", "column": "c"}})
    );
    let back: SchemaChange = serde_json::from_value(json).unwrap();
    assert_eq!(c, back);
}

// ---------------- detect_changes invariants ----------------

#[test]
fn detect_changes_identity_is_empty() {
    let snap = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };
    let changes = detect_changes(&snap, &snap);
    assert!(
        changes.is_empty(),
        "identity diff must be empty: {changes:?}"
    );
}

#[test]
fn detect_changes_empty_to_empty_is_empty() {
    assert!(detect_changes(&empty_snapshot(), &empty_snapshot()).is_empty());
}

#[test]
fn detect_changes_new_column_on_new_table_is_just_create_table() {
    // Going from empty → a snapshot with `diff_user` (which has columns
    // `id` and `name`). The diff should NOT emit AddColumn for those —
    // they're implicit in the CreateTable.
    let prev = empty_snapshot();
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let changes = detect_changes(&prev, &current);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0], SchemaChange::CreateTable("diff_user".into()));
}

#[test]
fn detect_changes_dropped_column_on_dropped_table_is_just_drop_table() {
    // Going from `[diff_user]` → empty. Should NOT emit DropColumn for
    // `id` and `name` — `DROP TABLE ... CASCADE` handles them.
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let current = empty_snapshot();
    let changes = detect_changes(&prev, &current);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0], SchemaChange::DropTable("diff_user".into()));
}

#[test]
fn detect_changes_complex_multi_table_diff() {
    // prev: [user, post]. current: [user (with bio), comment]. So:
    // - post is dropped
    // - comment is created
    // - user gains a `bio` column
    let prev = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };

    let mut user_with_bio = user_table();
    user_with_bio.fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "bio", "column": "bio", "ty": "string",
            "nullable": true, "primary_key": false
        }))
        .unwrap(),
    );
    user_with_bio.fields.sort_by(|a, b| a.column.cmp(&b.column));

    let comment: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "diff_comment",
        "model": "DiffComment",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();

    let current = SchemaSnapshot {
        tables: vec![user_with_bio, comment],
        ..Default::default()
    };

    let changes = detect_changes(&prev, &current);
    assert!(changes.contains(&SchemaChange::CreateTable("diff_comment".into())));
    assert!(changes.contains(&SchemaChange::AddColumn {
        table: "diff_user".into(),
        column: "bio".into()
    }));
    assert!(changes.contains(&SchemaChange::DropTable("diff_post".into())));
    assert_eq!(changes.len(), 3);
}

#[test]
fn detect_changes_table_appears_in_both_with_no_field_changes_emits_nothing() {
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    assert!(detect_changes(&prev, &current).is_empty());
}

// ---------------- render_changes ordering ----------------

#[test]
fn render_changes_empty_returns_empty() {
    let snap = empty_snapshot();
    let ddl = render_changes(&[], &snap).unwrap();
    assert!(ddl.is_empty());
}

#[test]
fn detect_then_render_orders_create_before_add_before_drop_col_before_drop_table() {
    // The end-to-end contract users care about: detect_changes →
    // render_changes produces DDL in dependency-safe order
    // (CREATE → ADD → DROP COLUMN → DROP TABLE → new-table FK ALTERs).
    let prev = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };

    // current: drop post, drop a column from user, add bio to user, create comment.
    let mut user = user_table();
    user.fields.retain(|f| f.column != "name"); // drop name
    user.fields.push(
        serde_json::from_value(serde_json::json!({
            "name": "bio", "column": "bio", "ty": "string", "nullable": true, "primary_key": false
        }))
        .unwrap(),
    );
    user.fields.sort_by(|a, b| a.column.cmp(&b.column));
    let comment: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": "diff_comment",
        "model": "Cm",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true}
        ]
    }))
    .unwrap();
    let current = SchemaSnapshot {
        tables: vec![comment, user],
        ..Default::default()
    };

    let changes = detect_changes(&prev, &current);
    let ddl = render_changes(&changes, &current).unwrap();

    let pos = |needle: &str| ddl.iter().position(|s| s.contains(needle)).unwrap();
    let create_pos = pos(r#"CREATE TABLE "diff_comment""#);
    let add_pos = pos(r#"ADD COLUMN "bio""#);
    let drop_col_pos = pos(r#"DROP COLUMN "name""#);
    let drop_table_pos = pos(r#"DROP TABLE "diff_post""#);

    assert!(create_pos < add_pos, "{ddl:#?}");
    assert!(add_pos < drop_col_pos, "{ddl:#?}");
    assert!(drop_col_pos < drop_table_pos, "{ddl:#?}");
}

#[test]
fn render_changes_preserves_caller_order_except_for_new_table_fks() {
    // Contract: render_changes is order-preserving. The only thing
    // it relocates is the FK ALTERs for new tables — those always go
    // to the end so they're emitted after every CREATE TABLE has run.
    let current = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };

    // Intentionally awkward order — render emits as supplied.
    let ddl = render_changes(
        &[
            SchemaChange::CreateTable("diff_post".into()),
            SchemaChange::CreateTable("diff_user".into()),
        ],
        &current,
    )
    .unwrap();

    // post comes before user (caller's order), then the FK ALTER.
    let post_pos = ddl
        .iter()
        .position(|s| s.starts_with(r#"CREATE TABLE "diff_post""#))
        .unwrap();
    let user_pos = ddl
        .iter()
        .position(|s| s.starts_with(r#"CREATE TABLE "diff_user""#))
        .unwrap();
    let fk_pos = ddl
        .iter()
        .position(|s| s.contains("ADD CONSTRAINT") && s.contains("FOREIGN KEY"))
        .unwrap();

    assert!(
        post_pos < user_pos,
        "render preserves caller order: {ddl:#?}"
    );
    assert!(user_pos < fk_pos, "FK ALTER goes last: {ddl:#?}");
}

#[test]
fn render_changes_emits_fk_alters_at_the_end_of_create_tables() {
    let current = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };
    let ddl = render_changes(
        &[
            SchemaChange::CreateTable("diff_user".into()),
            SchemaChange::CreateTable("diff_post".into()),
        ],
        &current,
    )
    .unwrap();

    // Both CREATE TABLEs come before any ADD CONSTRAINT FK.
    let last_create = ddl
        .iter()
        .rposition(|s| s.starts_with("CREATE TABLE"))
        .unwrap();
    let first_fk = ddl
        .iter()
        .position(|s| s.contains("ADD CONSTRAINT") && s.contains("FOREIGN KEY"))
        .unwrap();
    assert!(
        last_create < first_fk,
        "FK ALTER must come after all CREATE TABLEs:\n{ddl:#?}"
    );
}

#[test]
fn render_changes_create_table_missing_in_snapshot_is_an_error() {
    let err = render_changes(
        &[SchemaChange::CreateTable("ghost".into())],
        &empty_snapshot(),
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[test]
fn render_changes_add_column_missing_field_is_an_error() {
    let current = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let err = render_changes(
        &[SchemaChange::AddColumn {
            table: "diff_user".into(),
            column: "ghost_column".into(),
        }],
        &current,
    )
    .unwrap_err();
    assert!(err.contains("ghost_column"), "{err}");
}

#[test]
fn render_changes_add_column_for_table_not_in_snapshot_is_an_error() {
    let err = render_changes(
        &[SchemaChange::AddColumn {
            table: "ghost".into(),
            column: "x".into(),
        }],
        &empty_snapshot(),
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[test]
fn render_changes_drop_column_does_not_consult_snapshot() {
    // DropColumn render only needs the table+column names — so even an
    // empty snapshot should let us render `ALTER TABLE ... DROP COLUMN`.
    let ddl = render_changes(
        &[SchemaChange::DropColumn {
            table: "ghost_table".into(),
            column: "ghost_col".into(),
        }],
        &empty_snapshot(),
    )
    .unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "ghost_table" DROP COLUMN "ghost_col""#]
    );
}

// ---------------- snapshot.field_by_column / scalar_fields ----------------

#[test]
fn schema_snapshot_table_lookup_by_name() {
    let snap = SchemaSnapshot {
        tables: vec![user_table(), post_table()],
        ..Default::default()
    };
    assert!(snap.table("diff_user").is_some());
    assert!(snap.table("diff_post").is_some());
    assert!(snap.table("ghost").is_none());
}

#[test]
fn table_snapshot_field_lookup_by_column() {
    let t = user_table();
    assert_eq!(t.field("id").unwrap().column, "id");
    assert_eq!(t.field("name").unwrap().column, "name");
    assert!(t.field("ghost").is_none());
}

// ---------------- AlterField + Rename DDL (v0.4 Slice 3) ----------------

fn empty_snap() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
    }
}

#[test]
fn render_alter_column_type_emits_alter_with_using_cast() {
    let changes = vec![SchemaChange::AlterColumnType {
        table: "u".into(),
        column: "age".into(),
        from: "i32".into(),
        to: "i64".into(),
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "age" TYPE BIGINT USING "age"::BIGINT"#]
    );
}

#[test]
fn render_alter_column_nullable_set_not_null_when_false() {
    let changes = vec![SchemaChange::AlterColumnNullable {
        table: "u".into(),
        column: "name".into(),
        nullable: false,
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" SET NOT NULL"#]
    );
}

#[test]
fn render_alter_column_nullable_drop_not_null_when_true() {
    let changes = vec![SchemaChange::AlterColumnNullable {
        table: "u".into(),
        column: "name".into(),
        nullable: true,
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" DROP NOT NULL"#]
    );
}

#[test]
fn render_alter_column_default_set_emits_set_default() {
    let changes = vec![SchemaChange::AlterColumnDefault {
        table: "u".into(),
        column: "is_active".into(),
        from: None,
        to: Some("true".into()),
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "is_active" SET DEFAULT true"#]
    );
}

#[test]
fn render_alter_column_default_drop_emits_drop_default() {
    let changes = vec![SchemaChange::AlterColumnDefault {
        table: "u".into(),
        column: "is_active".into(),
        from: Some("true".into()),
        to: None,
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "is_active" DROP DEFAULT"#]
    );
}

#[test]
fn render_alter_column_max_length_emits_varchar_or_text() {
    let to_varchar = vec![SchemaChange::AlterColumnMaxLength {
        table: "u".into(),
        column: "name".into(),
        from: None,
        to: Some(64),
    }];
    let ddl = render_changes(&to_varchar, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" TYPE VARCHAR(64) USING "name"::VARCHAR(64)"#]
    );

    let to_text = vec![SchemaChange::AlterColumnMaxLength {
        table: "u".into(),
        column: "name".into(),
        from: Some(64),
        to: None,
    }];
    let ddl = render_changes(&to_text, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" TYPE TEXT USING "name"::TEXT"#]
    );
}

#[test]
fn render_rename_table_emits_rename_to() {
    let changes = vec![SchemaChange::RenameTable {
        old_name: "user".into(),
        new_name: "account".into(),
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(ddl, vec![r#"ALTER TABLE "user" RENAME TO "account""#]);
}

#[test]
fn render_rename_column_emits_rename_column() {
    let changes = vec![SchemaChange::RenameColumn {
        table: "user".into(),
        old_column: "name".into(),
        new_column: "username".into(),
    }];
    let ddl = render_changes(&changes, &empty_snap()).unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "user" RENAME COLUMN "name" TO "username""#]
    );
}

#[test]
fn detect_changes_emits_alter_column_type_for_metadata_diff() {
    let prev = SchemaSnapshot {
        tables: vec![serde_json::from_value(serde_json::json!({
            "name": "u",
            "model": "U",
            "fields": [
                {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
                {"name": "age", "column": "age", "ty": "i32", "nullable": false, "primary_key": false}
            ]
        })).unwrap()],
                    ..Default::default()
    };
    let current = SchemaSnapshot {
        tables: vec![serde_json::from_value(serde_json::json!({
            "name": "u",
            "model": "U",
            "fields": [
                {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
                {"name": "age", "column": "age", "ty": "i64", "nullable": true, "primary_key": false, "default": "0"}
            ]
        })).unwrap()],
                    ..Default::default()
    };
    let changes = detect_changes(&prev, &current);
    assert!(changes
        .iter()
        .any(|c| matches!(c, SchemaChange::AlterColumnType { .. })));
    assert!(changes
        .iter()
        .any(|c| matches!(c, SchemaChange::AlterColumnNullable { .. })));
    assert!(changes
        .iter()
        .any(|c| matches!(c, SchemaChange::AlterColumnDefault { .. })));
}

// ---------------- composite-FK diff (F.5b) ----------------

fn pair_table_with_composite_fk(name: &str) -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "model": "Pair",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "left_id", "column": "left_id", "ty": "i64", "nullable": false, "primary_key": false},
            {"name": "right_id", "column": "right_id", "ty": "i64", "nullable": false, "primary_key": false}
        ],
        "composite_fks": [
            {
                "name": "diff_pair_left_right_fkey",
                "to": "diff_target",
                "from": ["left_id", "right_id"],
                "on": ["a_id", "b_id"]
            }
        ]
    })).unwrap()
}

fn pair_table_no_composite_fk(name: &str) -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "model": "Pair",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "left_id", "column": "left_id", "ty": "i64", "nullable": false, "primary_key": false},
            {"name": "right_id", "column": "right_id", "ty": "i64", "nullable": false, "primary_key": false}
        ]
    })).unwrap()
}

#[test]
fn detect_add_composite_fk_on_existing_table() {
    let prev = SchemaSnapshot {
        tables: vec![pair_table_no_composite_fk("diff_pair")],
        ..Default::default()
    };
    let current = SchemaSnapshot {
        tables: vec![pair_table_with_composite_fk("diff_pair")],
        ..Default::default()
    };
    let changes = detect_changes(&prev, &current);
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, SchemaChange::AddCompositeFk { name, .. } if name == "diff_pair_left_right_fkey")),
        "expected AddCompositeFk in {changes:?}",
    );
}

#[test]
fn detect_drop_composite_fk_on_existing_table() {
    let prev = SchemaSnapshot {
        tables: vec![pair_table_with_composite_fk("diff_pair")],
        ..Default::default()
    };
    let current = SchemaSnapshot {
        tables: vec![pair_table_no_composite_fk("diff_pair")],
        ..Default::default()
    };
    let changes = detect_changes(&prev, &current);
    assert!(
        changes
            .iter()
            .any(|c| matches!(c, SchemaChange::DropCompositeFk { name, .. } if name == "diff_pair_left_right_fkey")),
        "expected DropCompositeFk in {changes:?}",
    );
}

#[test]
fn render_add_composite_fk_emits_alter_table() {
    let snap = SchemaSnapshot {
        tables: vec![pair_table_with_composite_fk("diff_pair")],
        ..Default::default()
    };
    let ddl = render_changes(
        &[SchemaChange::AddCompositeFk {
            table: "diff_pair".into(),
            name: "diff_pair_left_right_fkey".into(),
            to: "diff_target".into(),
            from: vec!["left_id".into(), "right_id".into()],
            on: vec!["a_id".into(), "b_id".into()],
        }],
        &snap,
    )
    .unwrap();
    assert_eq!(ddl.len(), 1);
    assert!(ddl[0].contains(r#"ALTER TABLE "diff_pair""#));
    assert!(ddl[0].contains(r#"ADD CONSTRAINT "diff_pair_left_right_fkey""#));
    assert!(ddl[0].contains(r#"FOREIGN KEY ("left_id", "right_id")"#));
    assert!(ddl[0].contains(r#"REFERENCES "diff_target" ("a_id", "b_id")"#));
}

#[test]
fn render_drop_composite_fk_emits_alter_table_drop_constraint() {
    let snap = SchemaSnapshot {
        tables: vec![pair_table_no_composite_fk("diff_pair")],
        ..Default::default()
    };
    let ddl = render_changes(
        &[SchemaChange::DropCompositeFk {
            table: "diff_pair".into(),
            name: "diff_pair_left_right_fkey".into(),
        }],
        &snap,
    )
    .unwrap();
    assert_eq!(
        ddl,
        vec![r#"ALTER TABLE "diff_pair" DROP CONSTRAINT IF EXISTS "diff_pair_left_right_fkey""#]
    );
}

#[test]
fn create_table_emits_composite_fks_in_deferred_bucket() {
    // CREATE TABLE for a model that owns a composite FK should emit
    // the table creation immediately and the ADD CONSTRAINT after
    // (so the referenced table exists by the time the FK runs).
    let snap = SchemaSnapshot {
        tables: vec![pair_table_with_composite_fk("diff_pair")],
        ..Default::default()
    };
    let ddl = render_changes(&[SchemaChange::CreateTable("diff_pair".into())], &snap).unwrap();
    assert!(
        ddl[0].starts_with(r#"CREATE TABLE "diff_pair""#),
        "first stmt should be CREATE TABLE; got: {}",
        ddl[0],
    );
    assert!(
        ddl.iter().any(|s| s.contains("diff_pair_left_right_fkey")
            && s.contains(r#"FOREIGN KEY ("left_id", "right_id")"#)),
        "expected composite FK ALTER TABLE in {ddl:?}",
    );
}

// ---------------- in-place index change (v0.19.2 regression) ------

fn snap_with_index(name: &str, columns: &[&str], unique: bool) -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![user_table()],
        indexes: vec![rustango::migrate::IndexSnapshot {
            name: name.into(),
            table: "diff_user".into(),
            columns: columns.iter().map(|s| (*s).into()).collect(),
            unique,
            method: "btree".into(),
        }],
        ..Default::default()
    }
}

#[test]
fn changing_index_columns_keeps_name_emits_drop_then_create() {
    let prev = snap_with_index("uq", &["a", "b"], true);
    let current = snap_with_index("uq", &["a", "c"], true);
    let changes = detect_changes(&prev, &current);
    let drop_idx = changes
        .iter()
        .position(|c| matches!(c, SchemaChange::DropIndex { name } if name == "uq"));
    let create_idx = changes.iter().position(|c| {
        matches!(c, SchemaChange::CreateIndex { name, columns, .. }
            if name == "uq" && columns == &vec!["a".to_string(), "c".into()])
    });
    assert!(
        drop_idx.is_some(),
        "expected DropIndex(uq) — diff missed in-place column change: {changes:?}"
    );
    assert!(
        create_idx.is_some(),
        "expected CreateIndex(uq) with new columns: {changes:?}"
    );
    assert!(
        drop_idx.unwrap() < create_idx.unwrap(),
        "drop must come before create"
    );
}

#[test]
fn flipping_unique_flag_emits_drop_then_create() {
    let prev = snap_with_index("idx", &["a"], false);
    let current = snap_with_index("idx", &["a"], true);
    let changes = detect_changes(&prev, &current);
    assert!(changes
        .iter()
        .any(|c| matches!(c, SchemaChange::DropIndex { name } if name == "idx")));
    assert!(changes.iter().any(
        |c| matches!(c, SchemaChange::CreateIndex { name, unique, .. } if name == "idx" && *unique)
    ));
}

#[test]
fn unchanged_index_emits_nothing() {
    let prev = snap_with_index("uq", &["a", "b"], true);
    let current = snap_with_index("uq", &["a", "b"], true);
    let changes = detect_changes(&prev, &current);
    assert!(
        !changes.iter().any(|c| matches!(
            c,
            SchemaChange::DropIndex { .. } | SchemaChange::CreateIndex { .. }
        )),
        "no index change → no Drop/CreateIndex; got {changes:?}",
    );
}
