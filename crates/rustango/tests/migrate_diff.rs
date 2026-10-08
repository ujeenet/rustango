//! Pure-diff invariants and render ordering tests.
//!
//! Focuses on the `SchemaChange` IR and `render_changes` ordering —
//! the pieces that `make_migrations` and the runner both depend on.
//! Live PG tests live in `migrate_runner.rs`.

use rustango::migrate::{
    detect_changes, invert, render_changes, Operation, SchemaChange, SchemaSnapshot, TableSnapshot,
};

// ---------------- helpers ----------------

fn empty_snapshot() -> SchemaSnapshot {
    SchemaSnapshot {
        tables: vec![],
        m2m_tables: vec![],
        indexes: vec![],
        checks: vec![],
        excludes: vec![],
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
        excludes: vec![],
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
/// No `USING`: a `::VARCHAR(n)` cast silently truncates (#1878).
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
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" TYPE VARCHAR(64)"#]
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
        vec![r#"ALTER TABLE "u" ALTER COLUMN "name" TYPE TEXT"#]
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
    // The type change writes the new default itself.
    assert!(!changes
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
            where_clause: None,
            include: vec![],
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
        .position(|c| matches!(c, SchemaChange::DropIndex { name, .. } if name == "uq"));
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
        .any(|c| matches!(c, SchemaChange::DropIndex { name, .. } if name == "idx")));
    assert!(changes.iter().any(
        |c| matches!(c, SchemaChange::CreateIndex { name, unique, .. } if name == "idx" && *unique)
    ));
}

/// Dropping a model drops its indexes **first** (#1588, #1598).
///
/// This is the shape that broke a live MySQL tenant. `makemigrations`
/// emitted `DropTable` and then `DropIndex`; MySQL applied the
/// `DropTable`, failed on the index with 1146, and recorded the
/// migration as failed — so the table was gone with no ledger row, and
/// re-running failed differently (1051). Nothing reconciled that
/// without `migrate --fake`.
///
/// The first fix *suppressed* the index drop. That stopped the failure
/// and broke rollback instead — see
/// `dropping_a_table_round_trips_through_invert_with_its_indexes`.
/// Ordering is the fix that does both: every dialect accepts dropping
/// an index while its table is still there.
#[test]
fn dropping_a_table_drops_its_indexes_first() {
    let prev = snap_with_index("uq", &["a", "b"], true);
    // The table goes; so does the index that lived on it.
    let current = SchemaSnapshot::default();

    let changes = detect_changes(&prev, &current);

    let drop_table = changes
        .iter()
        .position(|c| matches!(c, SchemaChange::DropTable(t) if t == "diff_user"))
        .expect("control: the table itself must still be dropped");
    let drop_index = changes
        .iter()
        .position(|c| matches!(c, SchemaChange::DropIndex { name, .. } if name == "uq"))
        .unwrap_or_else(|| {
            panic!(
                "the index was not dropped at all. Suppressing it leaves rollback \
                 restoring the table without it: {changes:?}"
            )
        });

    assert!(
        drop_index < drop_table,
        "DropIndex must be emitted before DropTable. After the table is gone MySQL \
         fails the index drop with 1146, having already auto-committed the DROP TABLE \
         — schema changed, ledger empty, re-run fails 1051: {changes:?}"
    );
}

/// The same ordering for a table-level CHECK (#1598).
///
/// #1588's fix reached one of three identical loops. This one and the
/// EXCLUDE loop below still emitted their drop *after* `DropTable`, so
/// a model carrying a CHECK reproduced the original failure exactly.
#[test]
fn dropping_a_table_drops_its_check_constraints_first() {
    let mut prev = snap_with_index("uq", &["a", "b"], true);
    prev.checks = vec![serde_json::from_value(serde_json::json!({
        "name": "ck_age", "table": "diff_user", "expr": "age > 0"
    }))
    .expect("check snapshot")];
    let current = SchemaSnapshot::default();

    let changes = detect_changes(&prev, &current);

    let drop_table = changes
        .iter()
        .position(|c| matches!(c, SchemaChange::DropTable(t) if t == "diff_user"))
        .expect("control: the table must still be dropped");
    let drop_check = changes
        .iter()
        .position(
            |c| matches!(c, SchemaChange::DropCheckConstraint { name, .. } if name == "ck_age"),
        )
        .expect("the CHECK must still be dropped");

    assert!(
        drop_check < drop_table,
        "DropCheckConstraint must precede DropTable — on MySQL the table drop \
         auto-commits and `ALTER TABLE t DROP CHECK` then fails 1146, which is #1588 \
         reproduced through the loop its fix missed: {changes:?}"
    );
}

/// Dropping a model is invertible, and the inverse restores the
/// indexes (#1598).
///
/// The suppression fix made this silently false: the forward list
/// became `[DropTable]` alone, `invert` gave `[CreateTable]`, and
/// `CreateTable` renders no index DDL — so rolling back *succeeded*
/// and left the table without its indexes, UNIQUE ones included, while
/// the predecessor snapshot still listed them. `makemigrations` then
/// saw no drift, so nothing ever recreated them.
///
/// Asserting the round trip rather than the op list: the op list is
/// what changed, the property is what matters.
#[test]
fn dropping_a_table_round_trips_through_invert_with_its_indexes() {
    let prev = snap_with_index("uq", &["a", "b"], true);
    let current = SchemaSnapshot::default();

    let forward: Vec<Operation> = detect_changes(&prev, &current)
        .into_iter()
        .map(Operation::Schema)
        .collect();

    // `invert` walks the forward list in reverse, so the ordering the
    // forward list established is what puts CreateTable ahead of
    // CreateIndex here — no second ordering rule to keep in step.
    let back = invert(&forward, &prev).expect("a drop-model migration must be invertible");

    let creates_table = back.iter().position(
        |op| matches!(op, Operation::Schema(SchemaChange::CreateTable(t)) if t == "diff_user"),
    );
    let creates_index = back
        .iter()
        .position(|op| matches!(op, Operation::Schema(SchemaChange::CreateIndex { name, .. }) if name == "uq"));

    let creates_table = creates_table.expect("inverse must recreate the table");
    let creates_index = creates_index.expect(
        "inverse must recreate the index. Without it the rollback succeeds and \
         silently loses every index the table had, while the snapshot still lists them",
    );
    assert!(
        creates_table < creates_index,
        "the table must be created before its index: {back:?}"
    );
}

/// …but an index dropped on a table that *survives* is still emitted,
/// and now carries the table MySQL needs.
///
/// Without this, "suppress DropIndex" could be implemented as "never
/// emit DropIndex" and the test above would still pass.
#[test]
fn dropping_only_the_index_still_emits_drop_index_with_its_table() {
    let prev = snap_with_index("uq", &["a", "b"], true);
    let current = SchemaSnapshot {
        tables: prev.tables.clone(),
        indexes: vec![],
        ..Default::default()
    };

    let changes = detect_changes(&prev, &current);

    let found = changes.iter().find_map(|c| match c {
        SchemaChange::DropIndex { name, table } if name == "uq" => Some(table.clone()),
        _ => None,
    });
    assert_eq!(
        found.as_deref(),
        Some("diff_user"),
        "an index dropped from a surviving table must still be dropped, and must \
         name its table so MySQL can render `DROP INDEX <name> ON <table>`: {changes:?}"
    );
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

// ---------------- #1877–#1881 review follow-ups ----------------

fn snap(v: serde_json::Value) -> SchemaSnapshot {
    serde_json::from_value(v).expect("snapshot JSON")
}

/// Author, book (FK + UNIQUE), a composite FK onto a unique index, a
/// CHECK, an EXCLUDE and an M2M junction.
fn full_schema(fk_to: &str, check: &str, exclude_where: &str) -> SchemaSnapshot {
    let id = serde_json::json!({"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true});
    let int = |c: &str| serde_json::json!({"name": c, "column": c, "ty": "i64", "nullable": true, "primary_key": false});
    snap(serde_json::json!({
        "tables": [
            {"name": "rv_author", "model": "A", "fields": [id, int("a"), int("b")]},
            {"name": "rv_other", "model": "O", "fields": [id, int("a"), int("b")]},
            {"name": "rv_book", "model": "B", "fields": [id, int("a"), int("b"),
                {"name": "author_id", "column": "author_id", "ty": "i64", "nullable": true,
                 "primary_key": false, "fk": {"kind": "fk", "to": "rv_author", "on": "id"}},
                {"name": "isbn", "column": "isbn", "ty": "string", "nullable": true,
                 "primary_key": false, "max_length": 20, "unique": true}],
             "composite_fks": [{"name": "rv_book_ab_fk", "to": fk_to, "from": ["a", "b"], "on": ["a", "b"]}]},
        ],
        "indexes": [
            {"name": "rv_author_ab_uq", "table": "rv_author", "columns": ["a", "b"], "unique": true},
            {"name": "rv_other_ab_uq", "table": "rv_other", "columns": ["a", "b"], "unique": true},
        ],
        "checks": [{"name": "rv_book_ck", "table": "rv_book", "expr": check}],
        "excludes": [{"name": "rv_book_ex", "table": "rv_book", "using": "gist",
                      "elements": [["a", "="]], "where_clause": exclude_where}],
        "m2m_tables": [{"through": "rv_book_tags", "src_table": "rv_book", "src_col": "book_id",
                        "dst_table": "rv_author", "dst_col": "author_id"}],
    }))
}

/// `makemigrations` on an unchanged schema writes nothing, for every
/// object the diff compares whole.
#[test]
fn unchanged_schema_with_every_constraint_kind_emits_nothing() {
    let s = full_schema("rv_author", "a >= 0", "a > 0");
    assert_eq!(detect_changes(&s, &s.clone()), vec![]);
}

/// Both sides declaring one junction give the same snapshot in either
/// `inventory` order, so the diff between them is empty.
#[test]
fn shared_through_pair_emits_nothing_in_either_order() {
    use rustango::core::{M2MRelation, ModelSchema};
    const TAGS: &[M2MRelation] = &[M2MRelation::new(
        "tags",
        "rv_tag",
        "rv_shared_tags",
        "item_id",
        "tag_id",
    )];
    const ITEMS: &[M2MRelation] = &[M2MRelation::new(
        "items",
        "rv_post",
        "rv_shared_tags",
        "tag_id",
        "item_id",
    )];
    const fn model(
        name: &'static str,
        table: &'static str,
        m2m: &'static [M2MRelation],
    ) -> ModelSchema {
        let mut s = ModelSchema::new(name, table);
        s.m2m = m2m;
        s
    }
    static POST: ModelSchema = model("Post", "rv_post", TAGS);
    static TAG: ModelSchema = model("Tag", "rv_tag", ITEMS);
    let a = SchemaSnapshot::from_models(&[&POST, &TAG]);
    let b = SchemaSnapshot::from_models(&[&TAG, &POST]);
    assert_eq!(detect_changes(&a, &b), vec![]);
}

/// Declaring the other side of a junction, from a table that sorts first,
/// keeps the junction and its rows (#2000).
#[test]
fn adding_the_mirror_side_keeps_the_junction() {
    use rustango::core::{M2MRelation, ModelSchema};
    const TAGS: &[M2MRelation] = &[M2MRelation::new(
        "tags",
        "rv_a_tag",
        "rv_post_tags",
        "post_id",
        "tag_id",
    )];
    const POSTS: &[M2MRelation] = &[M2MRelation::new(
        "posts",
        "rv_post",
        "rv_post_tags",
        "tag_id",
        "post_id",
    )];
    static POST: ModelSchema = {
        let mut s = ModelSchema::new("Post", "rv_post");
        s.m2m = TAGS;
        s
    };
    static TAG: ModelSchema = {
        let mut s = ModelSchema::new("Tag", "rv_a_tag");
        s.m2m = POSTS;
        s
    };
    let current = SchemaSnapshot::from_models(&[&POST, &TAG]);
    // As 0.60.0 wrote it with only `Post.tags` declared.
    let mut prev = current.clone();
    prev.m2m_tables = serde_json::from_value(serde_json::json!([
        {"through": "rv_post_tags", "src_table": "rv_post", "src_col": "post_id",
         "dst_table": "rv_a_tag", "dst_col": "tag_id"}
    ]))
    .unwrap();
    assert_eq!(detect_changes(&prev, &current), vec![]);
    let one_side = SchemaSnapshot::from_models(&[&POST]);
    assert_eq!(one_side.m2m_tables, current.m2m_tables);
}

/// Each edited object is dropped and added again with its new shape, and
/// the recreated composite FK comes after the unique index it needs.
#[test]
fn edited_constraints_are_recreated_with_the_new_shape() {
    let prev = full_schema("rv_author", "a >= 0", "a > 0");
    let mut cur = full_schema("rv_other", "a > 0", "a > 1");
    cur.indexes[1].columns = vec!["b".into(), "a".into()];
    let changes = detect_changes(&prev, &cur);
    let pos = |f: &dyn Fn(&SchemaChange) -> bool| {
        changes
            .iter()
            .position(f)
            .unwrap_or_else(|| panic!("missing in {changes:#?}"))
    };
    let add_fk = pos(
        &|c| matches!(c, SchemaChange::AddCompositeFk { to, on, .. } if to == "rv_other" && on == &["a", "b"]),
    );
    pos(&|c| matches!(c, SchemaChange::DropCompositeFk { name, .. } if name == "rv_book_ab_fk"));
    pos(&|c| matches!(c, SchemaChange::AddCheckConstraint { expr, .. } if expr == "a > 0"));
    pos(
        &|c| matches!(c, SchemaChange::AddExclusionConstraint { where_clause: Some(w), .. } if w == "a > 1"),
    );
    let create_ix =
        pos(&|c| matches!(c, SchemaChange::CreateIndex { name, .. } if name == "rv_other_ab_uq"));
    assert!(
        create_ix < add_fk,
        "the index must exist before the FK: {changes:#?}"
    );
}

/// Dropping everything: dependents before their tables, tables child
/// first. Checked on the op order, because PG and SQLite `CASCADE` or
/// skip FK checks and would hide a wrong order.
#[test]
fn drops_come_before_what_they_hang_off() {
    let prev = full_schema("rv_author", "a >= 0", "a > 0");
    let changes = detect_changes(&prev, &SchemaSnapshot::default());
    let pos = |f: &dyn Fn(&SchemaChange) -> bool| {
        changes
            .iter()
            .position(f)
            .unwrap_or_else(|| panic!("missing in {changes:#?}"))
    };
    let table = |t: &'static str| pos(&move |c| matches!(c, SchemaChange::DropTable(n) if n == t));
    let (author, book) = (table("rv_author"), table("rv_book"));
    assert!(book < author, "child first: {changes:#?}");
    for dep in [
        pos(&|c| matches!(c, SchemaChange::DropM2MTable { .. })),
        pos(&|c| matches!(c, SchemaChange::DropIndex { name, .. } if name == "rv_author_ab_uq")),
        pos(&|c| matches!(c, SchemaChange::DropCheckConstraint { .. })),
        pos(&|c| matches!(c, SchemaChange::DropExclusionConstraint { .. })),
    ] {
        assert!(dep < book && dep < author, "{changes:#?}");
    }
}

/// A dropped column goes after its index.
#[test]
fn column_drops_after_its_index() {
    let prev = full_schema("rv_author", "a >= 0", "a > 0");
    let mut cur = prev.clone();
    cur.tables[0]
        .fields
        .retain(|f| f.column != "a" && f.column != "b");
    cur.indexes.remove(0);
    let changes = detect_changes(&prev, &cur);
    let ix = changes.iter().position(
        |c| matches!(c, SchemaChange::DropIndex { name, .. } if name == "rv_author_ab_uq"),
    );
    let col = changes
        .iter()
        .position(|c| matches!(c, SchemaChange::DropColumn { table, .. } if table == "rv_author"));
    assert!(ix.is_some() && ix < col, "{changes:#?}");
}

/// `i32` → `String(5)`: the type change, then the length, or the TEXT
/// the type change renders would win (#1878).
#[test]
fn type_change_into_a_string_runs_before_its_length() {
    let t = |ty: &str, len: Option<u32>| {
        snap(
            serde_json::json!({"tables": [{"name": "rv_t", "model": "T", "fields": [
            {"name": "c", "column": "c", "ty": ty, "nullable": true, "primary_key": false,
             "max_length": len}]}]}),
        )
    };
    let changes = detect_changes(&t("i32", None), &t("string", Some(5)));
    assert!(
        matches!(
            changes.as_slice(),
            [
                SchemaChange::AlterColumnType { .. },
                SchemaChange::AlterColumnMaxLength { to: Some(5), .. }
            ]
        ),
        "{changes:#?}"
    );
}

// ---------------- #2239 ----------------

/// `diff_user.name` with `extra` merged in.
fn user_with(extra: serde_json::Value) -> SchemaSnapshot {
    let mut t = serde_json::to_value(user_table()).unwrap();
    let name = &mut t["fields"][1];
    for (k, v) in extra.as_object().unwrap() {
        name[k] = v.clone();
    }
    SchemaSnapshot {
        tables: vec![serde_json::from_value(t).unwrap()],
        ..empty_snapshot()
    }
}

#[test]
fn case_insensitive_change_is_a_type_change() {
    let plain = user_with(serde_json::json!({}));
    let ci = user_with(serde_json::json!({"case_insensitive": true}));
    let change = SchemaChange::AlterColumnType {
        table: "diff_user".into(),
        column: "name".into(),
        from: "string".into(),
        to: "string".into(),
    };
    assert_eq!(detect_changes(&plain, &ci), [change.clone()]);
    assert_eq!(detect_changes(&ci, &plain), [change]);
    // The whole string type, so turning it off keeps VARCHAR(32).
    let off = render_changes(&detect_changes(&ci, &plain), &plain).unwrap();
    assert_eq!(
        off.last().unwrap(),
        r#"ALTER TABLE "diff_user" ALTER COLUMN "name" TYPE VARCHAR(32)"#
    );
}

#[test]
fn db_comment_change_is_an_alter() {
    let none = user_with(serde_json::json!({}));
    let some = user_with(serde_json::json!({"db_comment": "Shown name"}));
    let changes = detect_changes(&none, &some);
    assert_eq!(
        changes,
        [SchemaChange::AlterColumnComment {
            table: "diff_user".into(),
            column: "name".into(),
            from: None,
            to: Some("Shown name".into()),
        }]
    );
    assert_eq!(
        render_changes(&changes, &some).unwrap(),
        [r#"COMMENT ON COLUMN "diff_user"."name" IS 'Shown name'"#]
    );
    let back = detect_changes(&some, &none);
    assert_eq!(
        render_changes(&back, &none).unwrap(),
        [r#"COMMENT ON COLUMN "diff_user"."name" IS ''"#]
    );
}

#[test]
fn generated_as_change_is_refused() {
    let a = user_with(serde_json::json!({"generated_as": "'a'"}));
    let b = user_with(serde_json::json!({"generated_as": "'b'"}));
    assert!(detect_changes(&a, &b).is_empty());
    let refused = rustango::migrate::detect_unsupported_field_changes(&a, &b);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("generated_as changed"), "{refused:?}");
}

/// A junction column renamed over the same tables is a RenameColumn,
/// not a Drop + Create that loses the rows (#2245).
#[test]
fn junction_column_change_renames_it() {
    let snap = |src: &str, dst: &str, src_table: &str| -> SchemaSnapshot {
        serde_json::from_value(serde_json::json!({ "tables": [], "m2m_tables": [
            {"through": "rj_post_tags", "src_table": src_table, "src_col": src,
             "dst_table": "rj_tag", "dst_col": dst}] }))
        .unwrap()
    };
    let rename = |old: &str, new: &str| SchemaChange::RenameColumn {
        table: "rj_post_tags".into(),
        old_column: old.into(),
        new_column: new.into(),
    };
    let prev = snap("post_id", "tag_id", "rj_post");
    assert_eq!(
        detect_changes(&prev, &snap("post_id", "label_id", "rj_post")),
        vec![rename("tag_id", "label_id")]
    );
    assert_eq!(
        detect_changes(&prev, &snap("tag_id", "post_id", "rj_post")),
        vec![
            rename("post_id", "post_id_swp0"),
            rename("tag_id", "post_id"),
            rename("post_id_swp0", "tag_id"),
        ]
    );
    assert_eq!(
        detect_changes(&prev, &snap("x_id", "post_id", "rj_post")),
        vec![rename("post_id", "x_id"), rename("tag_id", "post_id")]
    );
    assert_eq!(
        detect_changes(&prev, &snap("tag_id", "y_id", "rj_post")),
        vec![rename("tag_id", "y_id"), rename("post_id", "tag_id")]
    );
    // The swap's spare name fits 63 bytes and is not a junction column.
    let long = "p".repeat(60);
    for (src, dst) in [(long.as_str(), "tag_id"), ("x_id", "x_id_swp0")] {
        let changes = detect_changes(&snap(src, dst, "rj_post"), &snap(dst, src, "rj_post"));
        let SchemaChange::RenameColumn { new_column, .. } = &changes[0] else {
            panic!("{changes:?}");
        };
        assert!(new_column.len() <= 63, "{new_column}");
        assert!(new_column != src && new_column != dst, "{new_column}");
    }
    // Self-referencing, both renamed: which is which is unknown.
    let self_ref = |a: &str, b: &str| -> SchemaSnapshot {
        serde_json::from_value(serde_json::json!({ "tables": [], "m2m_tables": [
            {"through": "rj_follows", "src_table": "rj_user", "src_col": a,
             "dst_table": "rj_user", "dst_col": b}] }))
        .unwrap()
    };
    let (prev, both) = (self_ref("from_id", "to_id"), self_ref("a_id", "b_id"));
    let refused = rustango::migrate::detect_unsupported_field_changes(&prev, &both);
    assert!(refused[0].contains("renamed both columns"), "{refused:?}");
    // One renamed: `from_id` keeps its rows' role, whichever end sorts first.
    let to_b = |a: &str, b: &str| SchemaChange::RenameColumn {
        table: "rj_follows".into(),
        old_column: a.into(),
        new_column: b.into(),
    };
    for now in [self_ref("from_id", "b_id"), self_ref("b_id", "from_id")] {
        assert!(rustango::migrate::detect_unsupported_field_changes(&prev, &now).is_empty());
        assert_eq!(detect_changes(&prev, &now), vec![to_b("to_id", "b_id")]);
    }
    // Not self-referencing, the old file's ends in the other order.
    let flipped = snap_m2m("rj_tag", "tag_id", "rj_post", "post_id");
    assert_eq!(
        detect_changes(
            &flipped,
            &snap_m2m("rj_post", "post_id", "rj_tag", "label_id")
        ),
        vec![rename("tag_id", "label_id")]
    );
}

fn snap_m2m(src_table: &str, src_col: &str, dst_table: &str, dst_col: &str) -> SchemaSnapshot {
    serde_json::from_value(serde_json::json!({ "tables": [], "m2m_tables": [
        {"through": "rj_post_tags", "src_table": src_table, "src_col": src_col,
         "dst_table": dst_table, "dst_col": dst_col}] }))
    .unwrap()
}

/// FK names that cut to one 63-byte name are refused where the backend
/// wants them unique: per table on PG, per database on MySQL (#2245).
#[test]
fn colliding_fk_names_are_refused() {
    use rustango::migrate::diff::render_changes_split_with_dialect as render;
    use rustango::sql::{MySql, Postgres};
    let fk_col = |c: &str, to: &str| {
        serde_json::json!({"name": c, "column": c, "ty": "i64", "nullable": true,
            "primary_key": false, "fk": {"kind": "fk", "to": to, "on": "id"}})
    };
    let id = || {
        serde_json::json!({"name": "id", "column": "id", "ty": "i64",
        "nullable": false, "primary_key": true})
    };
    let snap = |v: serde_json::Value| -> SchemaSnapshot { serde_json::from_value(v).unwrap() };
    let refused = |r: Result<_, String>| r.is_err_and(|e| e.contains("rename a table or column"));
    // `a_b.c` and `a.b_c` both make `a_b_c_fkey`: only MySQL refuses.
    let cross = snap(serde_json::json!({"tables": [
        {"name": "p", "model": "P", "fields": [id()]},
        {"name": "a_b", "model": "AB", "fields": [id(), fk_col("c", "p")]},
        {"name": "a", "model": "A", "fields": [id(), fk_col("b_c", "p")]}]}));
    let create = [
        SchemaChange::CreateTable("a_b".into()),
        SchemaChange::CreateTable("a".into()),
    ];
    assert!(refused(render(&create, &cross, &MySql)));
    assert!(render(&create, &cross, &Postgres).is_ok());
    // AddColumn: two long columns on one table, cut to one name.
    let t = "t".repeat(46);
    let long = snap(serde_json::json!({"tables": [
        {"name": "p", "model": "P", "fields": [id()]},
        {"name": t, "model": "T", "fields": [id(),
            fk_col("author_reference_first", "p"), fk_col("author_reference_second", "p")]}]}));
    let add = [SchemaChange::AddColumn {
        table: t.clone(),
        column: "author_reference_second".into(),
    }];
    assert!(refused(render(&add, &long, &Postgres)));
    // CreateM2MTable: its two FK names collide.
    let through = "j".repeat(46);
    let m2m = snap(
        serde_json::json!({"tables": [{"name": "p", "model": "P", "fields": [id()]}],
        "m2m_tables": [{"through": through, "src_table": "p", "src_col": "author_reference_first",
                        "dst_table": "p", "dst_col": "author_reference_second"}]}),
    );
    let create_m2m = [SchemaChange::CreateM2MTable {
        through: through.clone(),
        src_table: "p".into(),
        src_col: "author_reference_first".into(),
        dst_table: "p".into(),
        dst_col: "author_reference_second".into(),
    }];
    assert!(refused(render(&create_m2m, &m2m, &MySql)));
}
