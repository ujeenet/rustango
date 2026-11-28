//! The inverse of each op (3.8), from the state around it. Rollback
//! inverts the stored `applied_body`, never the file on disk.

use super::format::{Change, ColumnChanges, Field, InlineFk, Op, TableDef};
use super::format::{DataDown, RustDown};
use super::state::{State, TableState, Target};
use super::{Code, LoadError};

/// What a rollback may undo.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Allow {
    /// Re-create dropped tables and columns empty: their data is gone.
    pub(crate) data_loss: bool,
}

/// `create_table` for `t` as state holds it: single-column FKs inline,
/// indexes and checks folded in.
///
/// # Errors
/// [`Code::NotEnabled`] for a composite FK.
pub(crate) fn create_op(t: &TableState, at: &str) -> Result<Op, LoadError> {
    let mut fields: Vec<Field> = t
        .columns
        .iter()
        .map(|c| Field {
            column: c.clone(),
            fk: None,
        })
        .collect();
    for fk in &t.fks {
        let ([col], [on]) = (fk.columns.as_slice(), fk.on.as_slice()) else {
            return Err(LoadError::new(
                Code::NotEnabled,
                at,
                format!(
                    "FK `{}` is composite; composite FKs are not enabled yet",
                    fk.name
                ),
            ));
        };
        let field = fields
            .iter_mut()
            .find(|f| &f.column.column == col)
            .ok_or_else(|| {
                LoadError::new(
                    Code::State,
                    at,
                    format!("FK `{}`: no column `{col}`", fk.name),
                )
            })?;
        field.fk = Some(InlineFk {
            name: fk.name.clone(),
            to: fk.to.clone(),
            on: on.clone(),
            on_delete: fk.on_delete,
            to_ty: fk.to_ty.clone(),
        });
    }
    Ok(Op::CreateTable {
        table: TableDef {
            name: t.name.clone(),
            scope: t.scope,
            fields,
            comment: t.comment.clone(),
        },
        indexes: t.indexes.clone(),
        checks: t.checks.clone(),
    })
}

fn swap<T: Clone>(c: &Option<Change<T>>) -> Option<Change<T>> {
    c.as_ref().map(|c| Change {
        from: c.to.clone(),
        to: c.from.clone(),
    })
}

fn blocked(at: &str, msg: impl Into<String>) -> LoadError {
    LoadError::new(Code::Blocked, at, msg)
}

/// Undo `nested` state ops on `s`, the state after them, newest first:
/// the inverse ops in run order. Data ops change no state.
///
/// # Errors
/// A nested op with no state inverse, or one that does not replay.
pub(crate) fn undo_nested(
    s: &mut State,
    nested: &[Op],
    at: &str,
    target: Target,
) -> Result<Vec<Op>, LoadError> {
    let ctx = super::replay::Ctx {
        target,
        registry: None,
    };
    let mut out = Vec::new();
    for (i, n) in nested.iter().enumerate().rev() {
        if matches!(n, Op::DataSql { .. } | Op::DataRust { .. }) {
            continue;
        }
        let at = format!("{at}/state/{i}");
        for inv in invert(n, s, Allow { data_loss: true }, &at)? {
            super::replay::apply(s, &inv, &at, &ctx, &[])?;
            out.push(inv);
        }
    }
    Ok(out)
}

/// The ops that undo `op`, given the state just after it.
///
/// # Errors
/// [`Code::Blocked`] for what cannot be undone: dropped data (unless
/// allowed), a raw_sql with no `down` for a backend it ran on, an extension.
pub(crate) fn invert(op: &Op, after: &State, allow: Allow, at: &str) -> Result<Vec<Op>, LoadError> {
    let table_after =
        |t: &str| {
            after.tables.get(t).map(|a| (**a).clone()).ok_or_else(|| {
                LoadError::new(Code::State, at, format!("table `{t}` is not in state"))
            })
        };
    Ok(match op {
        Op::CreateTable { table, .. } => vec![Op::DropTable {
            table: table.name.clone(),
            was: table_after(&table.name)?,
            restore: super::format::Restore::ShapeOnly,
        }],
        Op::DropTable { was, .. } => {
            if !allow.data_loss {
                return Err(blocked(
                    at,
                    format!("drop_table `{}`: its rows are gone; re-creating it empty needs --accept-data-loss", was.name),
                ));
            }
            vec![create_op(was, at)?]
        }
        Op::RenameTable { from, to } => vec![Op::RenameTable {
            from: to.clone(),
            to: from.clone(),
        }],
        Op::AlterTableComment { table, from, to } => vec![Op::AlterTableComment {
            table: table.clone(),
            from: to.clone(),
            to: from.clone(),
        }],
        Op::AddColumn { table, column, .. } => {
            let was = table_after(table)?
                .column(&column.column.column)
                .cloned()
                .ok_or_else(|| LoadError::new(Code::State, at, "added column not in state"))?;
            vec![Op::DropColumn {
                table: table.clone(),
                column: was.column.clone(),
                was: Field {
                    column: was,
                    fk: None,
                },
                restore: super::format::Restore::ShapeOnly,
            }]
        }
        Op::DropColumn {
            table,
            was,
            restore: super::format::Restore::Archive,
            ..
        } => {
            // Re-add nullable, refill from the archive, then NOT NULL again.
            let c = &was.column;
            let key = table_after(table)?
                .columns
                .iter()
                .find(|k| k.primary_key)
                .map(|k| k.column.clone())
                .ok_or_else(|| LoadError::new(Code::State, at, "no primary key to restore by"))?;
            let archive = super::format::archive_name(table, &c.column);
            let mut open = c.clone();
            open.nullable = true;
            let mut ops = vec![
                Op::AddColumn {
                    table: table.clone(),
                    column: Field {
                        column: open,
                        fk: None,
                    },
                    fill: None,
                },
                Op::ArchiveRestore {
                    table: table.clone(),
                    column: c.column.clone(),
                    key,
                    archive: archive.clone(),
                    not_null: !c.nullable,
                },
            ];
            if !c.nullable {
                ops.push(Op::AlterColumn {
                    table: table.clone(),
                    column: c.column.clone(),
                    changes: super::format::ColumnChanges {
                        nullable: Some(super::format::Change {
                            from: true,
                            to: false,
                        }),
                        ..super::format::ColumnChanges::default()
                    },
                    using: None,
                    using_down: None,
                });
            }
            ops.push(Op::ArchiveDrop { archive });
            ops
        }
        Op::DropColumn { table, was, .. } => {
            let c = &was.column;
            if !allow.data_loss {
                return Err(blocked(
                    at,
                    format!("drop_column `{table}.{}`: its values are gone; re-adding it empty needs --accept-data-loss", c.column),
                ));
            }
            if !c.nullable && !c.has_default() {
                return Err(blocked(
                    at,
                    format!(
                        "drop_column `{table}.{}`: NOT NULL with no default cannot come back empty",
                        c.column
                    ),
                ));
            }
            vec![Op::AddColumn {
                table: table.clone(),
                column: Field {
                    column: c.clone(),
                    fk: None,
                },
                fill: None,
            }]
        }
        Op::RenameColumn { table, from, to } => vec![Op::RenameColumn {
            table: table.clone(),
            from: to.clone(),
            to: from.clone(),
        }],
        Op::AlterColumn {
            table,
            column,
            changes,
            using,
            using_down,
        } => {
            let c = changes;
            vec![Op::AlterColumn {
                table: table.clone(),
                column: column.clone(),
                changes: ColumnChanges {
                    ty: swap(&c.ty),
                    nullable: swap(&c.nullable),
                    default: swap(&c.default),
                    portable_default: swap(&c.portable_default),
                    max_length: swap(&c.max_length),
                    comment: swap(&c.comment),
                    case_insensitive: swap(&c.case_insensitive),
                    primary_key: swap(&c.primary_key),
                    auto: swap(&c.auto),
                    generated_as: swap(&c.generated_as),
                    sealed: swap(&c.sealed),
                },
                using: using_down.clone(),
                using_down: using.clone(),
            }]
        }
        Op::AddFk { table, column, fk } => vec![Op::DropFk {
            table: table.clone(),
            column: column.clone(),
            was: fk.clone(),
        }],
        Op::DropFk { table, column, was } => vec![Op::AddFk {
            table: table.clone(),
            column: column.clone(),
            fk: was.clone(),
        }],
        Op::AddCompositeFk { table, fk } => vec![Op::DropCompositeFk {
            table: table.clone(),
            was: fk.clone(),
        }],
        Op::DropCompositeFk { table, was } => vec![Op::AddCompositeFk {
            table: table.clone(),
            fk: was.clone(),
        }],
        Op::AddIndex {
            table,
            index,
            concurrently,
        } => vec![Op::DropIndex {
            table: table.clone(),
            name: index.name.clone(),
            was: index.clone(),
            concurrently: *concurrently,
        }],
        Op::DropIndex {
            table,
            was,
            concurrently,
            ..
        } => vec![Op::AddIndex {
            table: table.clone(),
            index: was.clone(),
            concurrently: *concurrently,
        }],
        Op::RenameIndex { table, from, to } => vec![Op::RenameIndex {
            table: table.clone(),
            from: to.clone(),
            to: from.clone(),
        }],
        Op::AddCheck { table, check } => vec![Op::DropCheck {
            table: table.clone(),
            name: check.name.clone(),
            was: check.body.clone(),
        }],
        Op::DropCheck { table, name, was } => vec![Op::AddCheck {
            table: table.clone(),
            check: super::state::CheckDef {
                name: name.clone(),
                body: was.clone(),
            },
        }],
        Op::DataSql {
            id,
            tables,
            down,
            down_binds,
            on_fresh,
            batch,
            ..
        } => match down {
            DataDown::Noop(_) => Vec::new(),
            DataDown::Irreversible(why) => {
                return Err(blocked(
                    at,
                    format!("data_sql `{id}` is irreversible: {why}"),
                ))
            }
            DataDown::Sql(sql) => vec![Op::DataSql {
                id: id.clone(),
                tables: tables.clone(),
                up: sql.clone(),
                binds: down_binds.clone(),
                down: DataDown::Noop("the reverse of a reverse".into()),
                down_binds: Vec::new(),
                on_fresh: *on_fresh,
                restores: Vec::new(),
                // A down that names `{{batch_range}}` walks back by batch.
                batch: batch
                    .clone()
                    .filter(|_| sql.texts().any(|t| t.contains("{{batch_range}}"))),
            }],
        },
        // Captured undo rows restore it, whatever `down` says (7.4).
        Op::DataRust {
            id,
            tables,
            on_fresh,
            undo: true,
            reverse: false,
            ..
        } => vec![Op::DataRust {
            id: id.clone(),
            tables: tables.clone(),
            down: RustDown::Noop("the reverse of a reverse".into()),
            on_fresh: *on_fresh,
            restores: Vec::new(),
            batch: None,
            undo: true,
            reverse: true,
        }],
        Op::DataRust {
            id,
            tables,
            down,
            on_fresh,
            batch,
            reverse: false,
            ..
        } => match down {
            RustDown::Noop(_) => Vec::new(),
            RustDown::Irreversible(why) => {
                return Err(blocked(
                    at,
                    format!("data_rust `{id}` is irreversible: {why}"),
                ))
            }
            RustDown::Rust => vec![Op::DataRust {
                id: id.clone(),
                tables: tables.clone(),
                down: RustDown::Noop("the reverse of a reverse".into()),
                on_fresh: *on_fresh,
                restores: Vec::new(),
                batch: batch.clone(),
                undo: false,
                reverse: true,
            }],
        },
        Op::DataRust { .. } | Op::ArchiveRestore { .. } | Op::ArchiveDrop { .. } => Vec::new(),
        Op::RawSql {
            id,
            up,
            down,
            probe,
            ..
        } => {
            let Some(down) = down else {
                return Err(blocked(at, format!("raw_sql `{id}` has no `down`")));
            };
            for b in super::state::Backend::ALL {
                if up.get(b).is_some() && down.get(b).is_none() {
                    return Err(blocked(
                        at,
                        format!(
                            "raw_sql `{id}` ran on {} but has no `down` there",
                            b.as_str()
                        ),
                    ));
                }
            }
            // Its declared state reverses too, so a `state` probe has what
            // the reverse leaves to check.
            let mut s = after.clone();
            let back = undo_nested(&mut s, &op.nested(at)?, at, Target::Tenant)?;
            vec![Op::RawSql {
                id: id.clone(),
                up: down.clone(),
                down: Some(up.clone()),
                state: super::format::Declared::Inverse(back),
                probe: probe.clone(),
            }]
        }
        // State-only ops and forgets: nothing runs, and the rollback recomputes state.
        Op::StateOnly { .. } => Vec::new(),
        Op::ExternTable { .. }
        | Op::AdoptTable { .. }
        | Op::AdoptColumn { .. }
        | Op::CreateAdopted { .. } => Vec::new(),
        Op::CreateExtension { name, .. } => {
            return Err(blocked(
                at,
                format!("create_extension `{name}` is not undone by a rollback"),
            ))
        }
    })
}
