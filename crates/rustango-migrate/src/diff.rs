//! Diff two `SchemaSnapshot`s into a list of DDL statements.
//!
//! v0.2 scope: detect new tables, dropped tables, new columns, dropped
//! columns. Type / constraint changes and renames are explicitly
//! deferred — they can't be inferred from a snapshot diff (rename vs
//! drop+add are indistinguishable) and need a more explicit migration
//! authoring story (Django's `RenameField` operation).
//!
//! Output is `Vec<String>` of fully-formed Postgres DDL the runner can
//! execute one statement at a time. New-table CREATE TABLEs come before
//! ADD COLUMNs (so a new table referenced by a new column already
//! exists), and DROP COLUMNs come before DROP TABLEs for the same
//! reason. FK constraints for new tables are emitted last.
//!
//! `ADD COLUMN ... NOT NULL` is supported only when the field carries
//! a `default` (rendered as `DEFAULT <expr>` so Postgres can backfill
//! existing rows). Without a default, `AddColumn` of a non-null field
//! is rejected with an explanatory error pointing at the two fixes:
//! make the field `Option<T>`, or set `#[rustango(default = "…")]`.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::snapshot::{FieldSnapshot, SchemaSnapshot, TableSnapshot};

/// One thing that should change to move from `prev` to `current`.
///
/// Serializes externally-tagged: `{"CreateTable": "foo"}`,
/// `{"AddColumn": {"table": "foo", "column": "bar"}}`. That's what
/// migration files store under `Operation::Schema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaChange {
    CreateTable(String /* table name */),
    DropTable(String /* table name */),
    AddColumn { table: String, column: String },
    DropColumn { table: String, column: String },
}

/// Compute the ordered list of changes from `prev` → `current`.
#[must_use]
pub fn detect_changes(prev: &SchemaSnapshot, current: &SchemaSnapshot) -> Vec<SchemaChange> {
    let mut changes = Vec::new();

    // New tables.
    for t in &current.tables {
        if prev.table(&t.name).is_none() {
            changes.push(SchemaChange::CreateTable(t.name.clone()));
        }
    }
    // New columns on existing tables.
    for t in &current.tables {
        let Some(pt) = prev.table(&t.name) else {
            continue;
        };
        for f in &t.fields {
            if pt.field(&f.column).is_none() {
                changes.push(SchemaChange::AddColumn {
                    table: t.name.clone(),
                    column: f.column.clone(),
                });
            }
        }
    }
    // Dropped columns on remaining tables.
    for pt in &prev.tables {
        let Some(t) = current.table(&pt.name) else {
            continue;
        };
        for f in &pt.fields {
            if t.field(&f.column).is_none() {
                changes.push(SchemaChange::DropColumn {
                    table: pt.name.clone(),
                    column: f.column.clone(),
                });
            }
        }
    }
    // Dropped tables.
    for pt in &prev.tables {
        if current.table(&pt.name).is_none() {
            changes.push(SchemaChange::DropTable(pt.name.clone()));
        }
    }
    changes
}

/// Render a list of [`SchemaChange`]s as Postgres DDL strings ready to
/// execute. The `current` snapshot is consulted to read field metadata
/// for each `AddColumn` and `CreateTable` (so we know type, nullability,
/// bounds, defaults, etc.).
///
/// **Order is preserved** — this function is order-preserving: changes
/// come out in the same order they came in, with the single exception
/// that FK constraint ALTERs for new tables are appended at the end (so
/// they run after every CREATE TABLE in the batch). Callers that care
/// about dependency-safe ordering (CREATE before ADD COLUMN, DROP COLUMN
/// before DROP TABLE) should hand the changes in already in that order.
/// [`detect_changes`] does that by construction.
///
/// # Errors
/// Returns an error string describing any unsupported change shape (e.g.
/// `AddColumn` referring to a missing field — shouldn't happen if the
/// snapshot was produced by `from_registry`, but worth surfacing).
pub fn render_changes(
    changes: &[SchemaChange],
    current: &SchemaSnapshot,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut new_table_constraints: Vec<String> = Vec::new();

    for change in changes {
        match change {
            SchemaChange::CreateTable(name) => {
                let table = current.table(name).ok_or_else(|| {
                    format!("CreateTable for `{name}` but no snapshot entry for it")
                })?;
                out.push(create_table_sql_from_snapshot(table));
                new_table_constraints.extend(constraints_sql_from_snapshot(table));
            }
            SchemaChange::DropColumn { table, column } => {
                out.push(format!(r#"ALTER TABLE "{table}" DROP COLUMN "{column}""#,));
            }
            SchemaChange::AddColumn { table, column } => {
                let t = current.table(table).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but table missing in snapshot")
                })?;
                let f = t.field(column).ok_or_else(|| {
                    format!("AddColumn for `{table}.{column}` but field missing in snapshot")
                })?;
                if !f.nullable && f.default.is_none() {
                    return Err(format!(
                        "AddColumn `{table}.{column}` is NOT NULL with no `default` — Postgres can't backfill existing rows. Make the field `Option<…>` or set `#[rustango(default = \"…\")]`.",
                    ));
                }
                out.push(add_column_sql(table, f));
            }
            SchemaChange::DropTable(name) => {
                out.push(format!(r#"DROP TABLE "{name}" CASCADE"#));
            }
        }
    }
    out.extend(new_table_constraints);
    Ok(out)
}

fn create_table_sql_from_snapshot(t: &TableSnapshot) -> String {
    let mut sql = format!(r#"CREATE TABLE "{}" ("#, t.name);
    let mut first = true;
    for f in &t.fields {
        if !first {
            sql.push_str(", ");
        }
        first = false;
        let _ = write!(sql, r#""{}" {}"#, f.column, sql_type(f));
        if let Some(expr) = &f.default {
            let _ = write!(sql, " DEFAULT {expr}");
        }
        if !f.nullable {
            sql.push_str(" NOT NULL");
        }
        if f.primary_key {
            sql.push_str(" PRIMARY KEY");
        }
        if f.min.is_some() || f.max.is_some() {
            sql.push_str(" CHECK (");
            let mut wrote = false;
            if let Some(min) = f.min {
                let _ = write!(sql, r#""{}" >= {}"#, f.column, min);
                wrote = true;
            }
            if let Some(max) = f.max {
                if wrote {
                    sql.push_str(" AND ");
                }
                let _ = write!(sql, r#""{}" <= {}"#, f.column, max);
            }
            sql.push(')');
        }
    }
    sql.push(')');
    sql
}

fn constraints_sql_from_snapshot(t: &TableSnapshot) -> Vec<String> {
    t.fields
        .iter()
        .filter_map(|f| {
            f.fk.as_ref().map(|rel| {
                format!(
                    r#"ALTER TABLE "{}" ADD CONSTRAINT "{}_{}_fkey" FOREIGN KEY ("{}") REFERENCES "{}" ("{}")"#,
                    t.name, t.name, f.column, f.column, rel.to, rel.on,
                )
            })
        })
        .collect()
}

fn add_column_sql(table: &str, f: &FieldSnapshot) -> String {
    let mut sql = format!(
        r#"ALTER TABLE "{}" ADD COLUMN "{}" {}"#,
        table,
        f.column,
        sql_type(f)
    );
    if let Some(expr) = &f.default {
        let _ = write!(sql, " DEFAULT {expr}");
    }
    if !f.nullable {
        sql.push_str(" NOT NULL");
    }
    if f.min.is_some() || f.max.is_some() {
        sql.push_str(" CHECK (");
        let mut wrote = false;
        if let Some(min) = f.min {
            let _ = write!(sql, r#""{}" >= {}"#, f.column, min);
            wrote = true;
        }
        if let Some(max) = f.max {
            if wrote {
                sql.push_str(" AND ");
            }
            let _ = write!(sql, r#""{}" <= {}"#, f.column, max);
        }
        sql.push(')');
    }
    sql
}

fn sql_type(f: &FieldSnapshot) -> String {
    match f.ty.as_str() {
        "i32" => "INTEGER".into(),
        "i64" => "BIGINT".into(),
        "f32" => "REAL".into(),
        "f64" => "DOUBLE PRECISION".into(),
        "bool" => "BOOLEAN".into(),
        "string" => match f.max_length {
            Some(n) => format!("VARCHAR({n})"),
            None => "TEXT".into(),
        },
        "datetime" => "TIMESTAMPTZ".into(),
        "date" => "DATE".into(),
        "uuid" => "UUID".into(),
        "json" => "JSONB".into(),
        other => other.to_uppercase(),
    }
}
