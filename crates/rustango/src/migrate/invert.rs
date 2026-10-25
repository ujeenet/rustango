//! Invert a forward operation list into its rollback form.
//!
//! No I/O. The runner's `unapply` calls [`invert`], then executes the
//! result the same way it executes a forward list.
//!
//! Each schema op flips to its opposite: create becomes drop, add
//! becomes remove, a rename swaps its two names. The drop-shaped ones
//! need the dropped thing in the `prev` snapshot, because that is where
//! the metadata to recreate it comes from.
//!
//! A data op inverts to its `reverse_sql`, and the result is itself
//! one-way. A data op with `reversible: false` is an error, never a
//! silent no-op: a `downgrade` must stop so the operator can step in.

use super::diff::SchemaChange;
use super::error::MigrateError;
use super::file::{DataOp, Operation};
use super::snapshot::SchemaSnapshot;

/// Compute the rollback form of a forward operation list.
///
/// Walks `forward` **in reverse**: the last op applied is the first op
/// rolled back. `prev` is the schema **before** the migration ran, so
/// the predecessor's snapshot, or empty for the first migration.
///
/// # Errors
/// Returns [`MigrateError::Validation`] if:
/// * A data op has `reversible: false`, so it cannot be rolled back.
/// * A data op claims to be reversible but has no `reverse_sql`.
/// * A drop op names something missing from `prev`, so the metadata
///   needed to recreate it is gone.
pub fn invert(
    forward: &[Operation],
    prev: &SchemaSnapshot,
) -> Result<Vec<Operation>, MigrateError> {
    let mut out = Vec::with_capacity(forward.len());
    let mut last = Vec::new();
    for (i, op) in forward.iter().enumerate().rev() {
        let names = Names::new(&forward[..i]);
        match recreate_as_prev(op, prev, &names) {
            Some(op) => last.push(op),
            None => out.push(invert_one(op, prev, &names)?),
        }
    }
    out.extend(last);
    Ok(out)
}

/// A dropped CHECK, partial index or exclusion constraint on a table an
/// earlier op renamed, or renamed a column of: its SQL text holds `prev`'s
/// names, which cannot be remapped, so it is recreated as `prev` has it,
/// at the end, once every rename is undone.
fn recreate_as_prev(op: &Operation, prev: &SchemaSnapshot, names: &Names<'_>) -> Option<Operation> {
    let change = match op {
        Operation::Schema(SchemaChange::DropCheckConstraint { name, .. }) => {
            let c = prev.check(name)?;
            SchemaChange::AddCheckConstraint {
                name: name.clone(),
                table: c.table.clone(),
                expr: c.expr.clone(),
            }
        }
        Operation::Schema(SchemaChange::DropIndex { name, .. }) => {
            let idx = prev.index(name).filter(|i| i.where_clause.is_some())?;
            SchemaChange::CreateIndex {
                name: name.clone(),
                table: idx.table.clone(),
                columns: idx.columns.clone(),
                unique: idx.unique,
                method: idx.method.clone(),
                where_clause: idx.where_clause.clone(),
                include: idx.include.clone(),
            }
        }
        Operation::Schema(SchemaChange::DropExclusionConstraint { name, .. }) => {
            let x = prev
                .excludes
                .iter()
                .find(|x| x.name == *name && x.where_clause.is_some())?;
            super::diff::add_exclude(x)
        }
        _ => return None,
    };
    names
        .renames(change.table())
        .then_some(Operation::Schema(change))
}

/// Maps names between `prev` and one forward op, through the renames
/// before it: a drop looks its object up under the old name and
/// recreates it under the name at its op.
struct Names<'a> {
    earlier: &'a [Operation],
    /// `earlier`'s renames undone, last first.
    undo: Vec<Operation>,
}

impl<'a> Names<'a> {
    fn new(earlier: &'a [Operation]) -> Self {
        let undo = earlier
            .iter()
            .rev()
            .filter_map(|op| match op {
                Operation::Schema(SchemaChange::RenameTable { old_name, new_name }) => {
                    Some(SchemaChange::RenameTable {
                        old_name: new_name.clone(),
                        new_name: old_name.clone(),
                    })
                }
                Operation::Schema(SchemaChange::RenameColumn {
                    table,
                    old_column,
                    new_column,
                }) => Some(SchemaChange::RenameColumn {
                    table: table.clone(),
                    old_column: new_column.clone(),
                    new_column: old_column.clone(),
                }),
                _ => None,
            })
            .map(Operation::Schema)
            .collect();
        Self { earlier, undo }
    }

    /// `table` at the op, as `prev` names it.
    fn before(&self, table: &str) -> String {
        super::rebuild::name_at_end(table, &self.undo)
    }

    fn column_before(&self, table: &str, column: &str) -> String {
        super::rebuild::columns_at_end(table, &[column.to_owned()], &self.undo).remove(0)
    }

    /// Whether an earlier op renames `prev`'s `table` or one of its columns.
    fn renames(&self, table: &str) -> bool {
        let mut name = table.to_owned();
        self.earlier.iter().any(|op| match op {
            Operation::Schema(SchemaChange::RenameTable { old_name, new_name })
                if *old_name == name =>
            {
                name.clone_from(new_name);
                true
            }
            Operation::Schema(SchemaChange::RenameColumn { table, .. }) => *table == name,
            _ => false,
        })
    }

    /// `prev`'s `table` as it is named at the op.
    fn now(&self, table: &str) -> String {
        super::rebuild::name_at_end(table, self.earlier)
    }

    fn columns_now(&self, table: &str, columns: &[String]) -> Vec<String> {
        super::rebuild::columns_at_end(table, columns, self.earlier)
    }
}

fn invert_one(
    op: &Operation,
    prev: &SchemaSnapshot,
    names: &Names<'_>,
) -> Result<Operation, MigrateError> {
    match op {
        Operation::Schema(SchemaChange::CreateTable(t)) => {
            Ok(Operation::Schema(SchemaChange::DropTable(t.clone())))
        }
        Operation::Schema(SchemaChange::DropTable(t)) => {
            if prev.table(&names.before(t)).is_none() {
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
            let t = prev.table(&names.before(table)).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropColumn(`{table}`.`{column}`): table missing in predecessor snapshot",
                ))
            })?;
            if t.field(&names.column_before(table, column)).is_none() {
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
        Operation::Schema(SchemaChange::AlterColumnComment {
            table,
            column,
            from,
            to,
        }) => Ok(Operation::Schema(SchemaChange::AlterColumnComment {
            table: table.clone(),
            column: column.clone(),
            from: to.clone(),
            to: from.clone(),
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
        Operation::Schema(SchemaChange::AlterFkOnDelete {
            table,
            column,
            from,
            to,
        }) => Ok(Operation::Schema(SchemaChange::AlterFkOnDelete {
            table: table.clone(),
            column: column.clone(),
            from: to.clone(),
            to: from.clone(),
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
                table: names.now(&c.table),
                expr: c.expr.clone(),
            }))
        }
        Operation::Schema(SchemaChange::AddExclusionConstraint { name, table, .. }) => {
            // PG needs only the name to drop it, so the full
            // definition is not carried over.
            Ok(Operation::Schema(SchemaChange::DropExclusionConstraint {
                name: name.clone(),
                table: table.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropExclusionConstraint { name, .. }) => {
            let x = prev.excludes.iter().find(|x| x.name == *name).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropExclusionConstraint(`{name}`): constraint not in predecessor snapshot",
                ))
            })?;
            let mut x = x.clone();
            let columns: Vec<String> = x.elements.iter().map(|(c, _)| c.clone()).collect();
            for ((c, _), now) in x
                .elements
                .iter_mut()
                .zip(names.columns_now(&x.table, &columns))
            {
                *c = now;
            }
            x.table = names.now(&x.table);
            Ok(Operation::Schema(super::diff::add_exclude(&x)))
        }
        Operation::Schema(SchemaChange::CreateIndex { name, table, .. }) => {
            Ok(Operation::Schema(SchemaChange::DropIndex {
                name: name.clone(),
                // MySQL needs `DROP INDEX <name> ON <table>`, so the
                // table must travel with the inverse op.
                table: table.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropIndex { name, .. }) => {
            let idx = prev.index(name).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropIndex(`{name}`): index not in predecessor snapshot",
                ))
            })?;
            Ok(Operation::Schema(SchemaChange::CreateIndex {
                name: name.clone(),
                table: names.now(&idx.table),
                columns: names.columns_now(&idx.table, &idx.columns),
                unique: idx.unique,
                method: idx.method.clone(),
                where_clause: idx.where_clause.clone(),
                include: names.columns_now(&idx.table, &idx.include),
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
            let mt = prev.m2m_table(&names.before(through)).ok_or_else(|| {
                MigrateError::Validation(format!(
                    "cannot invert DropM2MTable(`{through}`): junction table not in predecessor snapshot",
                ))
            })?;
            let cols = names.columns_now(&mt.through, &[mt.src_col.clone(), mt.dst_col.clone()]);
            Ok(Operation::Schema(SchemaChange::CreateM2MTable {
                through: through.clone(),
                src_table: names.now(&mt.src_table),
                src_col: cols[0].clone(),
                dst_table: names.now(&mt.dst_table),
                dst_col: cols[1].clone(),
            }))
        }
        Operation::Schema(SchemaChange::AddCompositeFk { table, name, .. }) => {
            Ok(Operation::Schema(SchemaChange::DropCompositeFk {
                table: table.clone(),
                name: name.clone(),
            }))
        }
        Operation::Schema(SchemaChange::DropCompositeFk { table, name }) => {
            let t = prev.table(&names.before(table)).ok_or_else(|| {
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
                to: names.now(&cf.to),
                from: names.columns_now(&t.name, &cf.from),
                on: names.columns_now(&cf.to, &cf.on),
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
        Operation::Callback(c) => {
            let Some(reverse_name) = &c.reverse_name else {
                return Err(MigrateError::Validation(format!(
                    "callback `{}` cannot be rolled back: no `reverse_name` set",
                    c.name,
                )));
            };
            Ok(Operation::Callback(crate::migrate::file::CallbackOp {
                name: reverse_name.clone(),
                reverse_name: None,
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
