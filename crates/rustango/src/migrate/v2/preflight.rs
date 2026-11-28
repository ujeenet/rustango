//! Rollback data probes (7.1 step 6): read-only checks, run under the lock
//! before any reverse statement, for reverse ops that put a constraint
//! back. A hit is a blocker printed with the query that finds the rows.

use super::format::{Op, OpEnvelope};
use super::state::{Backend, CheckBody, State};
use crate::sql::{DialectKind, Pool};

/// What a reverse op needs the current rows to satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Need {
    NotNull {
        table: String,
        column: String,
    },
    Unique {
        table: String,
        name: String,
        columns: Vec<String>,
        /// A partial index's predicate: only those rows must be unique.
        predicate: Option<String>,
        skipped_on: Vec<Backend>,
    },
    Parent {
        table: String,
        columns: Vec<String>,
        to: String,
        on: Vec<String>,
    },
    Check {
        table: String,
        name: String,
        body: CheckBody,
    },
    MaxLength {
        table: String,
        column: String,
        max: u32,
    },
    /// An integer type narrows back: every value must fit its range.
    Range {
        table: String,
        column: String,
        ty: String,
        min: i64,
        max: i64,
    },
    /// An archive refills a column that goes back to NOT NULL: every row
    /// needs an archived value (7.5).
    Archived {
        table: String,
        column: String,
        key: String,
        archive: String,
    },
}

impl Need {
    fn table(&self) -> &str {
        match self {
            Self::NotNull { table, .. }
            | Self::Unique { table, .. }
            | Self::Parent { table, .. }
            | Self::Check { table, .. }
            | Self::MaxLength { table, .. }
            | Self::Range { table, .. }
            | Self::Archived { table, .. } => table,
        }
    }

    fn columns(&self) -> Vec<&str> {
        match self {
            Self::NotNull { column, .. }
            | Self::MaxLength { column, .. }
            | Self::Range { column, .. } => vec![column],
            Self::Unique { columns, .. } | Self::Parent { columns, .. } => {
                columns.iter().map(String::as_str).collect()
            }
            Self::Archived { key, .. } => vec![key],
            Self::Check { .. } => Vec::new(),
        }
    }
}

/// The needs of one reverse op.
pub(crate) fn needs(op: &Op) -> Vec<Need> {
    match op {
        Op::AlterColumn {
            table,
            column,
            changes,
            ..
        } => {
            let mut out = Vec::new();
            if changes.nullable.as_ref().is_some_and(|c| c.from && !c.to) {
                out.push(Need::NotNull {
                    table: table.clone(),
                    column: column.clone(),
                });
            }
            if let Some(c) = &changes.ty {
                if let (Some(from), Some(to)) = (int_range(&c.from), int_range(&c.to)) {
                    if to.0 > from.0 || to.1 < from.1 {
                        out.push(Need::Range {
                            table: table.clone(),
                            column: column.clone(),
                            ty: c.to.clone(),
                            min: to.0,
                            max: to.1,
                        });
                    }
                }
            }
            if let Some(c) = &changes.max_length {
                if let Some(to) = c.to.filter(|to| c.from.is_none_or(|from| from > *to)) {
                    out.push(Need::MaxLength {
                        table: table.clone(),
                        column: column.clone(),
                        max: to,
                    });
                }
            }
            out
        }
        Op::AddIndex { table, index, .. } if index.unique => vec![Need::Unique {
            table: table.clone(),
            name: index.name.clone(),
            columns: index.columns.clone(),
            predicate: index.predicate.clone(),
            skipped_on: index.skipped_on.clone(),
        }],
        Op::AddFk { table, column, fk } => vec![Need::Parent {
            table: table.clone(),
            columns: vec![column.clone()],
            to: fk.to.clone(),
            on: vec![fk.on.clone()],
        }],
        Op::AddCompositeFk { table, fk } => vec![Need::Parent {
            table: table.clone(),
            columns: fk.columns.clone(),
            to: fk.to.clone(),
            on: fk.on.clone(),
        }],
        Op::AddCheck { table, check } => vec![Need::Check {
            table: table.clone(),
            name: check.name.clone(),
            body: check.body.clone(),
        }],
        Op::ArchiveRestore {
            table,
            column,
            key,
            archive,
            not_null: true,
        } => vec![Need::Archived {
            table: table.clone(),
            column: column.clone(),
            key: key.clone(),
            archive: archive.clone(),
        }],
        _ => Vec::new(),
    }
}

/// The values an integer type holds.
fn int_range(ty: &str) -> Option<(i64, i64)> {
    match ty {
        "i16" => Some((i16::MIN.into(), i16::MAX.into())),
        "i32" => Some((i32::MIN.into(), i32::MAX.into())),
        "i64" => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

/// `table` or `table.column` entries a reverse data op writes.
fn writes(op: &Op) -> Vec<String> {
    match op {
        Op::DataSql { tables, .. } | Op::DataRust { tables, .. } => tables.write.clone(),
        _ => Vec::new(),
    }
}

/// Every need of `down` (reverse ops in run order) that the live rows can be
/// checked against now: on tables and columns in `now`, and not written by
/// an earlier reverse data op, which may fix the rows first.
pub(crate) fn checkable(down: &[&OpEnvelope], now: &State) -> Vec<Need> {
    let mut written: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for env in down {
        for need in needs(&env.op) {
            let t = need.table();
            let Some(table) = now.tables.get(t) else {
                continue;
            };
            let cols = need.columns();
            if cols.iter().any(|c| table.column(c).is_none()) {
                continue;
            }
            let fixed = written
                .iter()
                .any(|w| w == t || cols.iter().any(|c| *w == format!("{t}.{c}")));
            if !fixed {
                out.push(need);
            }
        }
        written.extend(writes(&env.op));
    }
    out
}

/// One blocker: why, and the query that lists the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) why: String,
    pub(crate) find: String,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; find them with: {}", self.why, self.find)
    }
}

/// Run every probe; the hits, each with its finder query.
///
/// # Errors
/// Driver failures.
pub(crate) async fn probe(pool: &Pool, needs: &[Need]) -> Result<Vec<Hit>, crate::sql::ExecError> {
    let d = pool.dialect();
    let q = |n: &str| d.quote_ident(n);
    let mut hits = Vec::new();
    for need in needs {
        let t = q(need.table());
        let (filter, why) = match need {
            Need::NotNull { table, column } => (
                format!("{} IS NULL", q(column)),
                format!("`{table}.{column}` goes back to NOT NULL, but rows hold NULL"),
            ),
            Need::Unique {
                table,
                name,
                columns,
                predicate,
                skipped_on,
            } => {
                if skipped_on.contains(&Backend::of(d.kind())) {
                    continue;
                }
                let cols: Vec<&str> = columns.iter().map(String::as_str).collect();
                let dup = super::super::ensure::has_duplicate_keys(
                    pool,
                    table,
                    &cols,
                    predicate.as_deref(),
                )
                .await
                .map_err(crate::sql::ExecError::Driver)?;
                if dup {
                    let list: Vec<String> = columns.iter().map(|c| q(c)).collect();
                    let mut set: Vec<String> =
                        list.iter().map(|c| format!("{c} IS NOT NULL")).collect();
                    set.extend(predicate.as_ref().map(|p| format!("({p})")));
                    let list = list.join(", ");
                    hits.push(Hit {
                        why: format!(
                            "unique `{name}` on `{table}` comes back, but rows repeat its values"
                        ),
                        find: format!(
                            "SELECT {list}, COUNT(*) FROM {t} WHERE {} GROUP BY {list} \
                             HAVING COUNT(*) > 1",
                            set.join(" AND ")
                        ),
                    });
                }
                continue;
            }
            Need::Parent {
                table,
                columns,
                to,
                on,
            } => {
                let set: Vec<String> = columns
                    .iter()
                    .map(|c| format!("{t}.{} IS NOT NULL", q(c)))
                    .collect();
                let eq: Vec<String> = columns
                    .iter()
                    .zip(on)
                    .map(|(c, o)| format!("p.{} = {t}.{}", q(o), q(c)))
                    .collect();
                (
                    format!(
                        "{} AND NOT EXISTS (SELECT 1 FROM {} AS p WHERE {})",
                        set.join(" AND "),
                        q(to),
                        eq.join(" AND ")
                    ),
                    format!(
                        "the FK `{table}.{}` → `{to}` comes back, but rows point nowhere",
                        columns.join(", ")
                    ),
                )
            }
            Need::Check { table, name, body } => (
                format!("NOT ({})", body.sql(d)),
                format!("check `{name}` on `{table}` comes back, but rows break it"),
            ),
            Need::Archived {
                table,
                column,
                key,
                archive,
            } => (
                format!(
                    "NOT EXISTS (SELECT 1 FROM {a} WHERE {a}.{k} = {t}.{key} AND {a}.{v} IS NOT NULL)",
                    a = q(archive),
                    k = q("k"),
                    v = q("v"),
                    key = q(key)
                ),
                format!(
                    "`{table}.{column}` comes back NOT NULL from its archive, but rows have no \
                     archived value"
                ),
            ),
            Need::Range {
                table,
                column,
                ty,
                min,
                max,
            } => (
                format!("{c} < {min} OR {c} > {max}", c = q(column)),
                format!("`{table}.{column}` narrows back to {ty}, but values outside it exist"),
            ),
            Need::MaxLength { table, column, max } => {
                let len = match d.kind() {
                    DialectKind::Mysql => "CHAR_LENGTH",
                    _ => "LENGTH",
                };
                (
                    format!("{len}({}) > {max}", q(column)),
                    format!("`{table}.{column}` narrows back to {max}, but longer values exist"),
                )
            }
        };
        let rows: Vec<(i64,)> = crate::sql::raw_query_pool(
            &format!("SELECT COUNT(*) FROM {t} WHERE {filter}"),
            Vec::new(),
            pool,
        )
        .await?;
        if let Some(n) = rows.first().map(|r| r.0).filter(|n| *n > 0) {
            hits.push(Hit {
                why: format!("{why} ({n} row(s))"),
                find: format!("SELECT * FROM {t} WHERE {filter}"),
            });
        }
    }
    Ok(hits)
}
