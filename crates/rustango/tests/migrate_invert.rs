//! Pure unit tests for `migrate::invert`.

use rustango::migrate::{invert, DataOp, Operation, SchemaChange, SchemaSnapshot, TableSnapshot};

fn empty() -> SchemaSnapshot {
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
        "name": "user",
        "model": "U",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "name", "column": "name", "ty": "string", "nullable": false, "primary_key": false, "max_length": 32}
        ]
    })).unwrap()
}

// ---------------- empty input ----------------

#[test]
fn invert_empty_is_empty() {
    let out = invert(&[], &empty()).unwrap();
    assert!(out.is_empty());
}

// ---------------- create ↔ drop table ----------------

#[test]
fn invert_create_table_yields_drop_table() {
    let forward = vec![Operation::Schema(SchemaChange::CreateTable("foo".into()))];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::DropTable("foo".into()))]
    );
}

#[test]
fn invert_drop_table_yields_create_table_when_in_prev() {
    // Inverting a DropTable requires the table to be in `prev`, since
    // the runner needs to render CREATE TABLE for the inverted op.
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let forward = vec![Operation::Schema(SchemaChange::DropTable("user".into()))];
    let out = invert(&forward, &prev).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::CreateTable("user".into()))]
    );
}

#[test]
fn invert_drop_table_missing_in_prev_is_validation_error() {
    let forward = vec![Operation::Schema(SchemaChange::DropTable("ghost".into()))];
    let err = invert(&forward, &empty()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("DropTable"), "{msg}");
    assert!(msg.contains("ghost"), "{msg}");
}

// ---------------- add ↔ drop column ----------------

#[test]
fn invert_add_column_yields_drop_column() {
    let forward = vec![Operation::Schema(SchemaChange::AddColumn {
        table: "user".into(),
        column: "bio".into(),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::DropColumn {
            table: "user".into(),
            column: "bio".into(),
        })]
    );
}

#[test]
fn invert_drop_column_yields_add_column_using_prev_metadata() {
    // To recreate a dropped column, the column must exist in `prev`
    // so the runner can pull its type/nullability when rendering.
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let forward = vec![Operation::Schema(SchemaChange::DropColumn {
        table: "user".into(),
        column: "name".into(),
    })];
    let out = invert(&forward, &prev).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AddColumn {
            table: "user".into(),
            column: "name".into(),
        })]
    );
}

#[test]
fn invert_drop_column_missing_table_in_prev_is_error() {
    let forward = vec![Operation::Schema(SchemaChange::DropColumn {
        table: "ghost".into(),
        column: "x".into(),
    })];
    let err = invert(&forward, &empty()).unwrap_err();
    assert!(format!("{err}").contains("ghost"));
}

#[test]
fn invert_drop_column_missing_column_in_prev_is_error() {
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    // user has `id` and `name`, NOT `phantom`.
    let forward = vec![Operation::Schema(SchemaChange::DropColumn {
        table: "user".into(),
        column: "phantom".into(),
    })];
    let err = invert(&forward, &prev).unwrap_err();
    assert!(format!("{err}").contains("phantom"));
}

// ---------------- data ops ----------------

#[test]
fn invert_data_uses_reverse_sql_as_new_sql() {
    let forward = vec![Operation::Data(DataOp {
        sql: "UPDATE x SET y = 1".into(),
        reverse_sql: Some("UPDATE x SET y = 0".into()),
        reversible: true,
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Data(DataOp {
            sql: "UPDATE x SET y = 0".into(),
            reverse_sql: None,
            reversible: false,
        })]
    );
}

#[test]
fn invert_irreversible_data_op_is_validation_error() {
    let forward = vec![Operation::Data(DataOp {
        sql: "DELETE FROM events WHERE older_than 90 days".into(),
        reverse_sql: None,
        reversible: false,
    })];
    let err = invert(&forward, &empty()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("reversible"), "{msg}");
    assert!(
        msg.contains("DELETE"),
        "should include offending sql: {msg}"
    );
}

#[test]
fn invert_data_with_reversible_true_but_no_reverse_sql_is_error() {
    // file::load already rejects this, but invert defends in depth.
    let forward = vec![Operation::Data(DataOp {
        sql: "UPDATE x SET y = 1".into(),
        reverse_sql: None,
        reversible: true,
    })];
    let err = invert(&forward, &empty()).unwrap_err();
    assert!(format!("{err}").contains("reverse_sql"));
}

// ---------------- ordering ----------------

#[test]
fn invert_walks_forward_in_reverse_order() {
    let prev = SchemaSnapshot {
        tables: vec![user_table()],
        ..Default::default()
    };
    let forward = vec![
        Operation::Schema(SchemaChange::AddColumn {
            table: "user".into(),
            column: "bio".into(),
        }),
        Operation::Data(DataOp {
            sql: "UPDATE user SET bio = ''".into(),
            reverse_sql: Some("UPDATE user SET bio = NULL".into()),
            reversible: true,
        }),
        Operation::Schema(SchemaChange::DropColumn {
            table: "user".into(),
            column: "name".into(),
        }),
    ];

    let out = invert(&forward, &prev).unwrap();
    assert_eq!(out.len(), 3);

    // Reverse order: last-applied is first-rolled-back.
    // Forward[2] = DropColumn(name) → out[0] = AddColumn(name)
    assert_eq!(
        out[0],
        Operation::Schema(SchemaChange::AddColumn {
            table: "user".into(),
            column: "name".into(),
        })
    );
    // Forward[1] = Data(UPDATE bio = '') → out[1] = Data(UPDATE bio = NULL)
    if let Operation::Data(d) = &out[1] {
        assert!(d.sql.contains("bio = NULL"));
    } else {
        panic!("expected Data at out[1], got {:?}", out[1]);
    }
    // Forward[0] = AddColumn(bio) → out[2] = DropColumn(bio)
    assert_eq!(
        out[2],
        Operation::Schema(SchemaChange::DropColumn {
            table: "user".into(),
            column: "bio".into(),
        })
    );
}

// ---------------- short-circuit on irreversible ----------------

#[test]
fn invert_fails_fast_on_first_irreversible_op() {
    // Even though the second op is fine, the irreversible second op
    // makes the whole list non-rollbackable. The error names the
    // offending op.
    let forward = vec![
        Operation::Schema(SchemaChange::CreateTable("foo".into())),
        Operation::Data(DataOp {
            sql: "ALTER USER admin RENAME TO root".into(),
            reverse_sql: None,
            reversible: false,
        }),
    ];
    let err = invert(&forward, &empty()).unwrap_err();
    assert!(format!("{err}").contains("ALTER USER"));
}

// ---------------- AlterField + Rename inverts (v0.4 Slice 3) ----------------

#[test]
fn invert_alter_column_type_swaps_from_and_to() {
    let forward = vec![Operation::Schema(SchemaChange::AlterColumnType {
        table: "user".into(),
        column: "age".into(),
        from: "i32".into(),
        to: "i64".into(),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AlterColumnType {
            table: "user".into(),
            column: "age".into(),
            from: "i64".into(),
            to: "i32".into(),
        })]
    );
}

#[test]
fn invert_alter_column_nullable_flips_the_flag() {
    let forward = vec![Operation::Schema(SchemaChange::AlterColumnNullable {
        table: "user".into(),
        column: "name".into(),
        nullable: true,
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AlterColumnNullable {
            table: "user".into(),
            column: "name".into(),
            nullable: false,
        })]
    );
}

#[test]
fn invert_alter_column_default_swaps_from_and_to() {
    let forward = vec![Operation::Schema(SchemaChange::AlterColumnDefault {
        table: "user".into(),
        column: "is_active".into(),
        from: None,
        to: Some("true".into()),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AlterColumnDefault {
            table: "user".into(),
            column: "is_active".into(),
            from: Some("true".into()),
            to: None,
        })]
    );
}

#[test]
fn invert_alter_column_max_length_swaps_from_and_to() {
    let forward = vec![Operation::Schema(SchemaChange::AlterColumnMaxLength {
        table: "user".into(),
        column: "name".into(),
        from: Some(32),
        to: Some(64),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AlterColumnMaxLength {
            table: "user".into(),
            column: "name".into(),
            from: Some(64),
            to: Some(32),
        })]
    );
}

#[test]
fn invert_rename_table_swaps_old_and_new() {
    let forward = vec![Operation::Schema(SchemaChange::RenameTable {
        old_name: "user".into(),
        new_name: "account".into(),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::RenameTable {
            old_name: "account".into(),
            new_name: "user".into(),
        })]
    );
}

#[test]
fn invert_rename_column_swaps_old_and_new() {
    let forward = vec![Operation::Schema(SchemaChange::RenameColumn {
        table: "user".into(),
        old_column: "name".into(),
        new_column: "username".into(),
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::RenameColumn {
            table: "user".into(),
            old_column: "username".into(),
            new_column: "name".into(),
        })]
    );
}

// ---------------- composite-FK invert (F.5b) ----------------

fn pair_table_with_composite_fk() -> TableSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": "pair",
        "model": "Pair",
        "fields": [
            {"name": "id", "column": "id", "ty": "i64", "nullable": false, "primary_key": true},
            {"name": "left_id", "column": "left_id", "ty": "i64", "nullable": false, "primary_key": false},
            {"name": "right_id", "column": "right_id", "ty": "i64", "nullable": false, "primary_key": false}
        ],
        "composite_fks": [
            {
                "name": "pair_left_right_fkey",
                "to": "target",
                "from": ["left_id", "right_id"],
                "on": ["a_id", "b_id"]
            }
        ]
    })).unwrap()
}

#[test]
fn invert_add_composite_fk_yields_drop_composite_fk() {
    let forward = vec![Operation::Schema(SchemaChange::AddCompositeFk {
        table: "pair".into(),
        name: "pair_left_right_fkey".into(),
        to: "target".into(),
        from: vec!["left_id".into(), "right_id".into()],
        on: vec!["a_id".into(), "b_id".into()],
    })];
    let out = invert(&forward, &empty()).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::DropCompositeFk {
            table: "pair".into(),
            name: "pair_left_right_fkey".into(),
        })]
    );
}

#[test]
fn invert_drop_composite_fk_recovers_metadata_from_prev() {
    let prev = SchemaSnapshot {
        tables: vec![pair_table_with_composite_fk()],
        ..Default::default()
    };
    let forward = vec![Operation::Schema(SchemaChange::DropCompositeFk {
        table: "pair".into(),
        name: "pair_left_right_fkey".into(),
    })];
    let out = invert(&forward, &prev).unwrap();
    assert_eq!(
        out,
        vec![Operation::Schema(SchemaChange::AddCompositeFk {
            table: "pair".into(),
            name: "pair_left_right_fkey".into(),
            to: "target".into(),
            from: vec!["left_id".into(), "right_id".into()],
            on: vec!["a_id".into(), "b_id".into()],
        })]
    );
}

#[test]
fn invert_drop_composite_fk_missing_in_prev_is_validation_error() {
    let forward = vec![Operation::Schema(SchemaChange::DropCompositeFk {
        table: "pair".into(),
        name: "pair_left_right_fkey".into(),
    })];
    let err = invert(&forward, &empty()).unwrap_err();
    assert!(format!("{err:?}").contains("pair_left_right_fkey"));
}
