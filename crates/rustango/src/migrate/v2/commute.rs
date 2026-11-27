//! Read/write keys per op, and the merge rule (4.5): two branches merge
//! only when no op of one writes what an op of the other reads or writes.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::format::{op_name, Op, PerBackend};
use super::load::Project;
use super::state::CheckBody;
use super::state::Target;
use super::{Code, LoadError};

/// One key: a table, a column of it, a schema-wide name (index, check,
/// FK), an extension, or everything (literal SQL).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key {
    Table(String),
    Column(String, String),
    Name(String),
    Extension(String),
    All,
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Table(t) => write!(f, "table `{t}`"),
            Self::Column(t, c) => write!(f, "column `{t}.{c}`"),
            Self::Name(n) => write!(f, "name `{n}`"),
            Self::Extension(e) => write!(f, "extension `{e}`"),
            Self::All => write!(f, "literal SQL"),
        }
    }
}

/// What an op writes and reads.
#[derive(Debug, Default)]
pub(crate) struct Keys {
    pub(crate) writes: BTreeSet<Key>,
    pub(crate) reads: BTreeSet<Key>,
}

/// Whether a write of `w` touches `k`: the same key, a table and anything
/// in it, or literal SQL.
fn touches(w: &Key, k: &Key) -> bool {
    match (w, k) {
        (Key::All, _) | (_, Key::All) => true,
        (Key::Table(t), Key::Column(u, _)) | (Key::Column(u, _), Key::Table(t)) => t == u,
        _ => w == k,
    }
}

/// A column write and a read of its table's existence commute: only a
/// table write (create, drop, rename) reaches a table read.
fn write_reaches_read(w: &Key, r: &Key) -> bool {
    match (w, r) {
        (Key::Column(..), Key::Table(_)) => false,
        _ => touches(w, r),
    }
}

impl Keys {
    /// The first key on which `self` and `other` do not commute.
    pub(crate) fn conflict(&self, other: &Self) -> Option<(Key, Key)> {
        let pairs = |a: &Self, b: &Self| -> Option<(Key, Key)> {
            for w in &a.writes {
                if let Some(k) = b.writes.iter().find(|k| touches(w, k)) {
                    return Some((w.clone(), k.clone()));
                }
                if let Some(k) = b.reads.iter().find(|k| write_reaches_read(w, k)) {
                    return Some((w.clone(), k.clone()));
                }
            }
            None
        };
        pairs(self, other).or_else(|| pairs(other, self).map(|(a, b)| (b, a)))
    }
}

fn table(t: &str) -> Key {
    Key::Table(t.to_owned())
}

fn col(t: &str, c: &str) -> Key {
    Key::Column(t.to_owned(), c.to_owned())
}

fn name(n: &str) -> Key {
    Key::Name(n.to_owned())
}

/// A footprint entry, `t` or `t.c`.
fn entry(e: &str) -> Key {
    match e.split_once('.') {
        Some((t, c)) => col(t, c),
        None => table(e),
    }
}

/// Text with no `{{t:}}`/`{{c:}}` placeholders names tables nobody declared.
fn literal(up: &PerBackend) -> bool {
    up.texts().any(|t| !t.contains("{{"))
}

/// The columns a CHECK expression names: its identifiers, read on `t`.
fn expr_columns(t: &str, expr: &str, k: &mut Keys) {
    match super::norm::tokenize(crate::sql::DialectKind::Postgres, expr) {
        Ok(toks) => {
            for tok in toks {
                if let super::norm::Tok::Ident(w) = tok.tok {
                    k.reads.insert(col(t, &w));
                }
            }
        }
        Err(_) => {
            k.reads.insert(table(t));
        }
    }
}

/// `op`'s keys.
///
/// # Errors
/// A `raw_sql`/`state_only` whose nested state does not decode.
pub(crate) fn keys(op: &Op, at: &str) -> Result<Keys, LoadError> {
    let mut k = Keys::default();
    match op {
        Op::CreateTable {
            table: t,
            indexes,
            checks,
        }
        | Op::AdoptTable {
            table: t,
            indexes,
            checks,
        }
        | Op::CreateAdopted {
            table: t,
            indexes,
            checks,
        } => {
            k.writes.insert(table(&t.name));
            for f in &t.fields {
                if let Some(fk) = &f.fk {
                    k.writes.insert(name(&fk.name));
                    k.reads.insert(table(&fk.to));
                }
            }
            k.writes.extend(indexes.iter().map(|i| name(&i.name)));
            k.writes.extend(checks.iter().map(|c| name(&c.name)));
        }
        Op::DropTable { table: t, was, .. } => {
            k.writes.insert(table(t));
            k.writes.extend(was.indexes.iter().map(|i| name(&i.name)));
            k.writes.extend(was.checks.iter().map(|c| name(&c.name)));
            k.writes.extend(was.fks.iter().map(|f| name(&f.name)));
        }
        Op::RenameTable { from, to } => {
            k.writes.insert(table(from));
            k.writes.insert(table(to));
        }
        Op::AlterTableComment { table: t, .. } | Op::ExternTable { table: t, .. } => {
            k.writes.insert(table(t));
        }
        Op::AddColumn {
            table: t, column, ..
        }
        | Op::AdoptColumn {
            table: t, column, ..
        } => {
            k.writes.insert(col(t, &column.column.column));
            k.reads.insert(table(t));
            if let Op::AdoptColumn { indexes, .. } = op {
                k.writes.extend(indexes.iter().map(|i| name(&i.name)));
            }
            if let Some(fk) = &column.fk {
                k.writes.insert(name(&fk.name));
                k.reads.insert(table(&fk.to));
            }
        }
        Op::DropColumn {
            table: t, column, ..
        }
        | Op::AlterColumn {
            table: t, column, ..
        } => {
            k.writes.insert(col(t, column));
            k.reads.insert(table(t));
        }
        Op::RenameColumn { table: t, from, to } => {
            k.writes.insert(col(t, from));
            k.writes.insert(col(t, to));
            k.reads.insert(table(t));
        }
        Op::AddFk {
            table: t,
            column,
            fk,
        } => {
            k.writes.insert(name(&fk.name));
            k.reads.insert(col(t, column));
            k.reads.insert(table(&fk.to));
        }
        Op::DropFk {
            table: t,
            column,
            was,
        } => {
            k.writes.insert(name(&was.name));
            k.reads.insert(col(t, column));
        }
        Op::AddIndex {
            table: t, index, ..
        } => {
            k.writes.insert(name(&index.name));
            k.reads.extend(index.columns.iter().map(|c| col(t, c)));
            k.reads.insert(table(t));
        }
        Op::DropIndex {
            table: t, name: n, ..
        } => {
            k.writes.insert(name(n));
            k.reads.insert(table(t));
        }
        Op::RenameIndex { table: t, from, to } => {
            k.writes.insert(name(from));
            k.writes.insert(name(to));
            k.reads.insert(table(t));
        }
        Op::AddCheck { table: t, check } => {
            k.writes.insert(name(&check.name));
            k.reads.insert(table(t));
            match &check.body {
                CheckBody::Sql(e) => expr_columns(t, e, &mut k),
                CheckBody::Range { column, .. } => {
                    k.reads.insert(col(t, column));
                }
            }
        }
        Op::DropCheck {
            table: t, name: n, ..
        } => {
            k.writes.insert(name(n));
            k.reads.insert(table(t));
        }
        Op::RawSql { .. } => {
            k.writes.insert(Key::All);
        }
        Op::StateOnly { .. } => {
            for (i, nested) in op.nested(at)?.iter().enumerate() {
                let n = keys(nested, &format!("{at}/ops/{i}"))?;
                k.writes.extend(n.writes);
                k.reads.extend(n.reads);
            }
        }
        Op::CreateExtension { name: e, .. } => {
            k.writes.insert(Key::Extension(e.clone()));
        }
        Op::DataSql { tables, up, .. } => {
            if literal(up) {
                k.writes.insert(Key::All);
            }
            k.writes.extend(tables.write.iter().map(|e| entry(e)));
            k.reads.extend(tables.read.iter().map(|e| entry(e)));
        }
        Op::DataRust { tables, .. } => {
            k.writes.extend(tables.write.iter().map(|e| entry(e)));
            k.reads.extend(tables.read.iter().map(|e| entry(e)));
        }
        Op::ArchiveRestore { .. } | Op::ArchiveDrop { .. } => {}
    }
    Ok(k)
}

/// Every merge node's branches commute (4.5); a conflict is refused with
/// both ops named.
///
/// # Errors
/// [`Code::Conflict`] on the first conflict.
pub(crate) fn refuse_conflicts(project: &Project) -> Result<(), LoadError> {
    for target in [
        Target::EveryDatabase,
        Target::Registry,
        Target::Tenant,
        Target::Database,
    ] {
        let Some(order) = project.order.get(&target) else {
            continue;
        };
        let by_key: HashMap<(&str, &str), usize> = order
            .iter()
            .map(|&i| {
                let f = &project.files[i].file;
                ((f.app.as_str(), f.id.as_str()), i)
            })
            .collect();
        let up = |i: usize| -> HashSet<usize> {
            let mut seen = HashSet::new();
            let mut stack = vec![i];
            while let Some(n) = stack.pop() {
                if !seen.insert(n) {
                    continue;
                }
                let f = &project.files[n].file;
                let parents = f.parents.iter().map(|p| (f.app.as_str(), p.as_str()));
                let deps = f.depends_on.iter().map(|(a, d)| (a.as_str(), d.as_str()));
                stack.extend(parents.chain(deps).filter_map(|k| by_key.get(&k).copied()));
            }
            seen
        };
        for &m in order {
            let f = &project.files[m].file;
            if f.parents.len() < 2 {
                continue;
            }
            let lines: Vec<HashSet<usize>> = f
                .parents
                .iter()
                .filter_map(|p| by_key.get(&(f.app.as_str(), p.as_str())))
                .map(|&p| up(p))
                .collect();
            let common: HashSet<usize> = lines.iter().skip(1).fold(lines[0].clone(), |acc, l| {
                acc.intersection(l).copied().collect()
            });
            let branches: Vec<Vec<usize>> = lines
                .iter()
                .map(|l| {
                    let mut b: Vec<usize> = l.difference(&common).copied().collect();
                    b.sort_unstable();
                    b
                })
                .collect();
            for (x, bx) in branches.iter().enumerate() {
                for by in &branches[x + 1..] {
                    refuse_pair(project, bx, by, &project.files[m].at)?;
                }
            }
        }
    }
    Ok(())
}

fn refuse_pair(project: &Project, a: &[usize], b: &[usize], at: &str) -> Result<(), LoadError> {
    let ops = |side: &[usize]| -> Result<Vec<(String, Keys)>, LoadError> {
        let mut out = Vec::new();
        for &i in side {
            let l = &project.files[i];
            for (n, env) in l.file.operations.iter().enumerate() {
                let op_at = format!("{}#/operations/{n}", l.at);
                out.push((
                    format!(
                        "`{}:{}` op {n} ({})",
                        l.file.app,
                        l.file.id,
                        op_name(&env.op)
                    ),
                    keys(&env.op, &op_at)?,
                ));
            }
        }
        Ok(out)
    };
    let (left, right) = (ops(a)?, ops(b)?);
    for (ln, lk) in &left {
        for (rn, rk) in &right {
            if let Some((x, y)) = lk.conflict(rk) {
                return Err(LoadError::new(
                    Code::Conflict,
                    at,
                    format!(
                        "the merged branches do not commute: {ln} on {x} | {rn} on {y}; \
                         rebase one branch onto the other (`manage dbmigrate make --rebase`)"
                    ),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "commute_tests.rs"]
mod tests;
