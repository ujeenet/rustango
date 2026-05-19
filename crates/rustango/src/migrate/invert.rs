//! Invert a forward operation list into its rollback form.
//!
//! Pure (no I/O). The runner's `unapply` calls [`invert`] to compute
//! the inverse op list, then executes it the same way the forward
//! runner does.
//!
//! Schema inversions:
//! * `CreateTable(t) → DropTable(t)`
//! * `DropTable(t) → CreateTable(t)` — needs `t` in `prev` snapshot
//! * `AddColumn{table, column} → DropColumn{...}`
//! * `DropColumn{table, column} → AddColumn{...}` — needs `(table,
//!   column)` in `prev` snapshot to recover field metadata at render
//!   time.
//!
//! Data inversions:
//! * `Data { sql, reverse_sql: Some(rs), reversible: true }`
//!   becomes `Data { sql: rs, reverse_sql: None, reversible: false }`
//!   — the inverted op is itself one-way; rolling-back-the-rollback
//!   would need a separate forward-applied migration.
//! * `Data { reversible: false }` → fail-fast with a clear error
//!   naming the offending op. **Never silently no-op**, which is one
//!   of Django's classic footguns: irreversible migrations should
//!   stop a `downgrade` cold so the operator can intervene.

use super::diff::SchemaChange;
use super::error::MigrateError;
use super::file::{DataOp, Operation};
use super::snapshot::SchemaSnapshot;

/// Compute the rollback form of a forward operation list.
///
/// Walks `forward` **in reverse** — the last op applied is the first
/// op rolled back. `prev` is the schema state **before** the
/// migration was applied (i.e. the predecessor migration's snapshot,
/// or empty for the very first migration).
///
/// # Errors
/// Returns [`MigrateError::Validation`] if:
/// * Any data op has `reversible: false` (cannot be rolled back).
/// * A data op has `reversible: true` but no `reverse_sql`
///   (corrupt — `file::load` should already have rejected this).
/// * A `DropTable`/`DropColumn` references something missing from
///   `prev`, meaning the predecessor snapshot was tampered with.
pub fn invert(
    forward: &[Operation],
    prev: &SchemaSnapshot,
) -> Result<Vec<Operation>, MigrateError> {
    let mut out = Vec::with_capacity(forward.len());
    for op in forward.iter().rev() {
        out.push(invert_one(op, prev)?);
    }
    Ok(out)
}

fn invert_one(op: &Operation, prev: &SchemaSnapshot) -> Result<Operation, MigrateError> {
    match op {
        Operation::Schema(SchemaChange::CreateTable(t)) => {
            Ok(Operation::Schema(SchemaChange::DropTable(t.clone())))
        }
        Operation::Schema(SchemaChange::DropTable(t)) => {
            if prev.table(t).is_none() {
                return Err(MigrateError::Validation(format!(
                    "cannot invert DropTable(`{t}`): table not in predecessor snapshot",
                )));
            }
            Ok(Operation::Schema(SchemaChange::CreateTable(t.clone())))
        }
        Operation::Schema(SchemaChange::AddColumn { table, column }) => {
            Ok(Operation::Schema(SchemaChange::DropColumn {
                table: table.clone(),
                column: column.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropColumn { table, column }) => {
            let t = prev.table(table).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropColumn(`{table}`.`{column}`): table missing in predecessor snapshot",
                ))
            })?;
            if t.field(column).is_none() {
                return Err(MigrateError::Validation(format!(
                    "cannot invert DropColumn(`{table}`.`{column}`): column missing in predecessor snapshot",
                )));
            }
            Ok(Operation::Schema(SchemaChange::AddColumn {
                table: table.clone(),
                column: column.clone(),
            }))
        }
        Operation::Schema(SchemaChange::AlterColumnType {
            table,
            column,
            from,
            to,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnType {
            table: table.clone(),
            column: column.clone(),
            from: to.clone(),
            to: from.clone(),
        })),
        Operation::Schema(SchemaChange::AlterColumnNullable {
            table,
            column,
            nullable,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnNullable {
            table: table.clone(),
            column: column.clone(),
            nullable: !*nullable,
        })),
        Operation::Schema(SchemaChange::AlterColumnDefault {
            table,
            column,
            from,
            to,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnDefault {
            table: table.clone(),
            column: column.clone(),
            from: to.clone(),
            to: from.clone(),
        })),
        Operation::Schema(SchemaChange::AlterColumnMaxLength {
            table,
            column,
            from,
            to,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnMaxLength {
            table: table.clone(),
            column: column.clone(),
            from: *to,
            to: *from,
        })),
        Operation::Schema(SchemaChange::AlterColumnUnique {
            table,
            column,
            unique,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnUnique {
            table: table.clone(),
            column: column.clone(),
            unique: !unique,
        })),
        Operation::Schema(SchemaChange::RenameTable { old_name, new_name }) => {
            Ok(Operation::Schema(SchemaChange::RenameTable {
                old_name: new_name.clone(),
                new_name: old_name.clone(),
            }))
        }
        Operation::Schema(SchemaChange::RenameColumn {
            table,
            old_column,
            new_column,
        }) => Ok(Operation::Schema(SchemaChange::RenameColumn {
            table: table.clone(),
            old_column: new_column.clone(),
            new_column: old_column.clone(),
        })),
        Operation::Schema(SchemaChange::AddCheckConstraint { name, table, .. }) => {
            Ok(Operation::Schema(SchemaChange::DropCheckConstraint {
                name: name.clone(),
                table: table.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropCheckConstraint { name, .. }) => {
            let c = prev.check(name).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropCheckConstraint(`{name}`): constraint not in predecessor snapshot",
                ))
            })?;
            Ok(Operation::Schema(SchemaChange::AddCheckConstraint {
                name: name.clone(),
                table: c.table.clone(),
                expr: c.expr.clone(),
            }))
        }
        Operation::Schema(SchemaChange::AddExclusionConstraint { name, table, .. }) => {
            // Inverse of Add is Drop. We don't carry the full
            // definition into the down migration because PG
            // doesn't need it for a Drop.
            Ok(Operation::Schema(SchemaChange::DropExclusionConstraint {
                name: name.clone(),
                table: table.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropExclusionConstraint { name, table }) => {
            // We can't reconstruct the AddExclusionConstraint
            // payload from a snapshot today (exclusion constraints
            // aren't tracked in `SchemaSnapshot` yet). Surface a
            // clear error so the user can hand-write the inverse
            // — same shape as `DropCheckConstraint` errors when
            // the constraint isn't in the predecessor snapshot.
            Err(MigrateError::Validation(format!(
                "cannot invert DropExclusionConstraint(`{name}` on `{table}`): exclusion constraints aren't tracked in SchemaSnapshot; write the inverse `AddExclusionConstraint` by hand. Issue #32.",
            )))
        }
        Operation::Schema(SchemaChange::CreateIndex { name, .. }) => {
            Ok(Operation::Schema(SchemaChange::DropIndex {
                name: name.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropIndex { name }) => {
            let idx = prev.index(name).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropIndex(`{name}`): index not in predecessor snapshot",
                ))
            })?;
            Ok(Operation::Schema(SchemaChange::CreateIndex {
                name: name.clone(),
                table: idx.table.clone(),
                columns: idx.columns.clone(),
                unique: idx.unique,
                method: idx.method.clone(),
                where_clause: idx.where_clause.clone(),
            }))
        }
        Operation::Schema(SchemaChange::CreateM2MTable {
            through,
            src_table: _,
            src_col: _,
            dst_table: _,
            dst_col: _,
        }) => Ok(Operation::Schema(SchemaChange::DropM2MTable {
            through: through.clone(),
        })),
        Operation::Schema(SchemaChange::DropM2MTable { through }) => {
            if prev.m2m_table(through).is_none() {
                return Err(MigrateError::Validation(format!(
                    "cannot invert DropM2MTable(`{through}`): junction table not in predecessor snapshot",
                )));
            }
            let mt = prev.m2m_table(through).unwrap();
            Ok(Operation::Schema(SchemaChange::CreateM2MTable {
                through: through.clone(),
                src_table: mt.src_table.clone(),
                src_col: mt.src_col.clone(),
                dst_table: mt.dst_table.clone(),
                dst_col: mt.dst_col.clone(),
            }))
        }
        Operation::Schema(SchemaChange::AddCompositeFk { table, name, .. }) => {
            Ok(Operation::Schema(SchemaChange::DropCompositeFk {
                table: table.clone(),
                name: name.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropCompositeFk { table, name }) => {
            let t = prev.table(table).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropCompositeFk(`{table}`.`{name}`): table missing in predecessor snapshot",
                ))
            })?;
            let cf = t.composite_fk(name).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropCompositeFk(`{table}`.`{name}`): composite FK not in predecessor snapshot",
                ))
            })?;
            Ok(Operation::Schema(SchemaChange::AddCompositeFk {
                table: table.clone(),
                name: name.clone(),
                to: cf.to.clone(),
                from: cf.from.clone(),
                on: cf.on.clone(),
            }))
        }
        Operation::Data(d) => {
            if !d.reversible {
                return Err(MigrateError::Validation(format!(
                    "data op marked `reversible: false` cannot be rolled back: {}",
                    truncate(&d.sql, 80),
                )));
            }
            let Some(reverse_sql) = &d.reverse_sql else {
                return Err(MigrateError::Validation(format!(
                    "data op marked `reversible: true` but missing `reverse_sql`: {}",
                    truncate(&d.sql, 80),
                )));
            };
            Ok(Operation::Data(DataOp {
                sql: reverse_sql.clone(),
                reverse_sql: None,
                reversible: false,
            }))
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}
