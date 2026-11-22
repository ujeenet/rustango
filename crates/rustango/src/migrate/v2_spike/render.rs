//! SPIKE S3: per-op rendering for migrations engine v2.
//!
//! Pure: `(state, op, dialect) -> units`. Nothing here is async or takes a
//! pool, so rendering cannot read the live catalog. Each op renders only
//! its own statements, in place; defaults go out exactly as written.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::ops::{CheckDecl, ColumnDecl, FkDecl, IndexDecl, TableDecl};
use crate::migrate::diff::{self, DefaultText, SchemaChange};
use crate::migrate::rebuild::TableRebuild;
use crate::migrate::snapshot::{IndexSnapshot, SchemaSnapshot, TableSnapshot};
use crate::sql::{Dialect, DialectKind, TableTarget};

// ------------------------------------------------------------------ state

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexState {
    pub decl: IndexDecl,
    pub skipped_on: Vec<DialectKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableState {
    pub decl: TableDecl,
    pub indexes: Vec<IndexState>,
}

/// A table this chain does not own; FKs may point at its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternTable {
    pub name: String,
    pub columns: Vec<ColumnDecl>,
    /// Column sets with a unique key on them.
    pub unique: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    /// `Arc` per table: a state clone is pointer copies, an op copies one table.
    pub tables: BTreeMap<String, Arc<TableState>>,
    pub externs: BTreeMap<String, ExternTable>,
    pub extensions: BTreeSet<String>,
}

// -------------------------------------------------------------------- ops

/// One SQL list per dialect; `None` = nothing runs there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PerDialect {
    pub postgres: Option<Vec<String>>,
    pub mysql: Option<Vec<String>>,
    pub sqlite: Option<Vec<String>>,
}

impl PerDialect {
    #[must_use]
    pub fn get(&self, d: DialectKind) -> Option<&Vec<String>> {
        match d {
            DialectKind::Postgres => self.postgres.as_ref(),
            DialectKind::Mysql => self.mysql.as_ref(),
            _ => self.sqlite.as_ref(),
        }
    }
}

/// The section 3 ops S3 renders. `alter_column` carries the whole column on
/// both sides; the file's `changes` map is the diff of the two.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    CreateTable {
        table: TableDecl,
        indexes: Vec<IndexState>,
    },
    DropTable {
        was: TableDecl,
    },
    RenameTable {
        from: String,
        to: String,
    },
    AlterTableComment {
        table: String,
        from: Option<String>,
        to: Option<String>,
    },
    AddColumn {
        table: String,
        column: ColumnDecl,
        /// SQL text for existing rows of a NOT NULL column with no default.
        fill: Option<String>,
    },
    DropColumn {
        table: String,
        was: ColumnDecl,
    },
    RenameColumn {
        table: String,
        from: String,
        to: String,
    },
    AlterColumn {
        table: String,
        from: ColumnDecl,
        to: ColumnDecl,
    },
    AddIndex {
        index: IndexState,
        /// `None`: CONCURRENTLY on PG when the table existed before this file.
        concurrently: Option<bool>,
    },
    DropIndex {
        was: IndexState,
        concurrently: Option<bool>,
    },
    RenameIndex {
        table: String,
        from: String,
        to: String,
    },
    AddCheck {
        table: String,
        check: CheckDecl,
    },
    DropCheck {
        table: String,
        was: CheckDecl,
    },
    AddFk {
        table: String,
        fk: FkDecl,
    },
    DropFk {
        table: String,
        was: FkDecl,
    },
    AlterFk {
        table: String,
        from: FkDecl,
        to: FkDecl,
    },
    RawSql {
        up: PerDialect,
        down: PerDialect,
        state: Vec<Op>,
        autocommit: bool,
    },
    StateOnly {
        ops: Vec<Op>,
        reason: String,
    },
    CreateExtension {
        name: String,
        schema: Option<String>,
        skipped_on: Vec<DialectKind>,
    },
    ExternTable(ExternTable),
    AdoptTable {
        table: TableDecl,
        indexes: Vec<IndexState>,
    },
}

// ------------------------------------------------------------------ units

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// One transaction.
    Tx,
    /// Each statement commits on its own (MySQL DDL, PG CONCURRENTLY).
    Autocommit,
    /// SQLite rebuild: FK enforcement off outside, `foreign_key_check` before commit.
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub op: usize,
    pub kind: UnitKind,
    pub sql: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {msg}")]
pub struct RenderError {
    pub code: &'static str,
    pub msg: String,
}

fn err<T>(code: &'static str, msg: impl Into<String>) -> Result<T, RenderError> {
    Err(RenderError {
        code,
        msg: msg.into(),
    })
}

/// State mismatch or a broken precondition.
pub const E_STATE: &str = "MV2-E001";
/// Type change on a column in an FK: MySQL 3780 unless the FK is dropped first.
pub const E_FK_TYPE: &str = "MV2-E3780";
/// Rename/drop of a column a CHECK, predicate or generated column names: MySQL 3959.
pub const E_CHECK_COL: &str = "MV2-E3959";
/// Not expressible on this dialect and not `skipped_on` it.
pub const E_DIALECT: &str = "MV2-E010";
/// NOT NULL with no default on an existing table needs `fill`.
pub const E_FILL: &str = "MV2-E011";
/// MySQL needs an index of its own under each FK.
pub const E_FK_INDEX: &str = "MV2-E012";
/// A session or transaction statement in a transactional raw_sql unit.
pub const E_RAW: &str = "MV2-E013";
/// `primary_key`, `auto` and `generated_as` changes stay a hard error.
pub const E_UNSUPPORTED: &str = "MV2-E014";

// ------------------------------------------------------------------ apply

fn table_mut<'s>(s: &'s mut State, t: &str) -> Result<&'s mut TableState, RenderError> {
    s.tables
        .get_mut(t)
        .map(Arc::make_mut)
        .ok_or_else(|| RenderError {
            code: E_STATE,
            msg: format!("table `{t}` is not in state"),
        })
}

fn table<'s>(s: &'s State, t: &str) -> Result<&'s TableState, RenderError> {
    s.tables.get(t).map(|a| &**a).ok_or_else(|| RenderError {
        code: E_STATE,
        msg: format!("table `{t}` is not in state"),
    })
}

/// Index, check and FK names: unique per target (MySQL wants FK and check
/// names unique per database, PG index names per schema).
fn name_taken(s: &State, name: &str) -> bool {
    // A scan with no allocation: the old `BTreeSet<String>` of every name
    // cost one clone per name per claim and dominated replay.
    s.tables.values().any(|t| {
        t.indexes.iter().any(|i| i.decl.name == name)
            || t.decl.checks.iter().any(|c| c.name == name)
            || t.decl.fks.iter().any(|f| f.name == name)
    })
}

fn claim(s: &State, name: &str) -> Result<(), RenderError> {
    if name.len() > 63 {
        return err(E_STATE, format!("`{name}` is longer than 63 bytes"));
    }
    if name_taken(s, name) {
        return err(E_STATE, format!("the name `{name}` is taken in state"));
    }
    Ok(())
}

/// `col` appears as an identifier token in `expr`.
#[must_use]
pub fn mentions(expr: &str, col: &str) -> bool {
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok.eq_ignore_ascii_case(col))
}

/// What in `t` (or pointing at it) names `col`, as `(code, what)`.
fn users_of(s: &State, t: &TableState, col: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for k in &t.decl.checks {
        if mentions(&k.expr, col) {
            out.push((E_CHECK_COL, format!("check `{}`", k.name)));
        }
    }
    for c in &t.decl.columns {
        if c.generated.as_deref().is_some_and(|g| mentions(g, col)) {
            out.push((E_CHECK_COL, format!("generated column `{}`", c.name)));
        }
    }
    for i in &t.indexes {
        if i.decl
            .predicate
            .as_deref()
            .is_some_and(|p| mentions(p, col))
        {
            out.push((E_CHECK_COL, format!("predicate of index `{}`", i.decl.name)));
        } else if i
            .decl
            .columns
            .iter()
            .chain(&i.decl.include)
            .any(|c| c == col)
        {
            out.push((E_STATE, format!("index `{}`", i.decl.name)));
        }
    }
    for f in &t.decl.fks {
        if f.columns.iter().any(|c| c == col) {
            out.push((E_STATE, format!("FK `{}`", f.name)));
        }
    }
    for (name, other) in &s.tables {
        for f in &other.decl.fks {
            if f.ref_table == t.decl.name && f.ref_columns.iter().any(|c| c == col) {
                out.push((E_STATE, format!("FK `{}` on `{name}`", f.name)));
            }
        }
    }
    out
}

/// FKs on `t` or into `t` that use `col`.
fn fks_using<'s>(s: &'s State, t: &str, col: &str) -> Vec<&'s FkDecl> {
    let mut out = Vec::new();
    for (name, ts) in &s.tables {
        for f in &ts.decl.fks {
            let from = name == t && f.columns.iter().any(|c| c == col);
            let to = f.ref_table == t && f.ref_columns.iter().any(|c| c == col);
            if from || to {
                out.push(f);
            }
        }
    }
    out
}

/// `(ty, max_length)` of a column in a state or extern table.
fn col_type(s: &State, t: &str, col: &str) -> Option<(String, Option<u32>)> {
    let cols = s
        .tables
        .get(t)
        .map(|t| &t.decl.columns)
        .or_else(|| s.externs.get(t).map(|e| &e.columns))?;
    cols.iter()
        .find(|c| c.name == col)
        .map(|c| (c.ty.clone(), c.max_length))
}

fn is_unique_key(s: &State, t: &str, cols: &[String]) -> bool {
    let same = |a: &[String]| {
        let mut a = a.to_vec();
        let mut b = cols.to_vec();
        a.sort();
        b.sort();
        a == b
    };
    if let Some(ts) = s.tables.get(t) {
        let pk: Vec<String> = ts
            .decl
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        return same(&pk)
            || ts
                .indexes
                .iter()
                .any(|i| i.decl.unique && i.decl.predicate.is_none() && same(&i.decl.columns));
    }
    s.externs
        .get(t)
        .is_some_and(|e| e.unique.iter().any(|u| same(u)))
}

/// The FK target exists in this target's state, is a unique key, and each
/// column pair has one type (MySQL 3780 on ADD otherwise).
fn check_fk(s: &State, t: &str, fk: &FkDecl) -> Result<(), RenderError> {
    if !s.tables.contains_key(&fk.ref_table) && !s.externs.contains_key(&fk.ref_table) {
        return err(
            E_STATE,
            format!(
                "FK `{}` points at `{}`, which is not in this target's state",
                fk.name, fk.ref_table
            ),
        );
    }
    if fk.columns.len() != fk.ref_columns.len() || fk.columns.is_empty() {
        return err(E_STATE, format!("FK `{}`: column counts differ", fk.name));
    }
    if !is_unique_key(s, &fk.ref_table, &fk.ref_columns) {
        return err(
            E_STATE,
            format!(
                "FK `{}`: `{}`({}) is not a primary or unique key",
                fk.name,
                fk.ref_table,
                fk.ref_columns.join(", ")
            ),
        );
    }
    for (c, r) in fk.columns.iter().zip(&fk.ref_columns) {
        let a = col_type(s, t, c);
        let b = col_type(s, &fk.ref_table, r);
        if a.is_none() || b.is_none() {
            return err(E_STATE, format!("FK `{}`: missing column", fk.name));
        }
        if a != b {
            return err(
                E_FK_TYPE,
                format!(
                    "FK `{}`: `{t}.{c}` is {a:?} but `{}.{r}` is {b:?}; one type on both ends",
                    fk.name, fk.ref_table
                ),
            );
        }
    }
    Ok(())
}

fn add_table(s: &mut State, table: &TableDecl, indexes: &[IndexState]) -> Result<(), RenderError> {
    if s.tables.contains_key(&table.name) || s.externs.contains_key(&table.name) {
        return err(E_STATE, format!("table `{}` already exists", table.name));
    }
    let mut seen = BTreeSet::new();
    for k in &table.checks {
        claim(s, &k.name)?;
        if !seen.insert(&k.name) {
            return err(E_STATE, format!("the name `{}` is taken in state", k.name));
        }
    }
    let mut fresh = TableState {
        decl: table.clone(),
        indexes: Vec::new(),
    };
    fresh.decl.fks.clear();
    s.tables.insert(table.name.clone(), Arc::new(fresh));
    for ix in indexes {
        add_index(s, ix)?;
    }
    // After the table and its keys exist, so a self-reference resolves.
    for fk in &table.fks {
        claim(s, &fk.name)?;
        check_fk(s, &table.name, fk)?;
        table_mut(s, &table.name)?.decl.fks.push(fk.clone());
    }
    Ok(())
}

fn add_index(s: &mut State, ix: &IndexState) -> Result<(), RenderError> {
    claim(s, &ix.decl.name)?;
    let t = table_mut(s, &ix.decl.table)?;
    for c in ix.decl.columns.iter().chain(&ix.decl.include) {
        if !t.decl.columns.iter().any(|x| &x.name == c) {
            return err(
                E_STATE,
                format!("index `{}`: no column `{c}`", ix.decl.name),
            );
        }
    }
    t.indexes.push(ix.clone());
    Ok(())
}

fn column_mut<'t>(t: &'t mut TableState, col: &str) -> Result<&'t mut ColumnDecl, RenderError> {
    let name = t.decl.name.clone();
    t.decl
        .columns
        .iter_mut()
        .find(|c| c.name == col)
        .ok_or_else(|| RenderError {
            code: E_STATE,
            msg: format!("column `{name}.{col}` is not in state"),
        })
}

/// Replay one op: preconditions, `was`/`from` equal state, then the change.
///
/// # Errors
/// Any broken precondition.
pub fn apply(s: &mut State, op: &Op) -> Result<(), RenderError> {
    match op {
        Op::CreateTable { table, indexes } | Op::AdoptTable { table, indexes } => {
            add_table(s, table, indexes)?;
        }
        Op::DropTable { was } => {
            let t = table(s, &was.name)?;
            if &t.decl != was {
                return err(
                    E_STATE,
                    format!("drop_table `{}`: `was` differs from state", was.name),
                );
            }
            for (name, other) in &s.tables {
                if name != &was.name {
                    if let Some(f) = other.decl.fks.iter().find(|f| f.ref_table == was.name) {
                        return err(
                            E_STATE,
                            format!("drop_table `{}`: FK `{}` on `{name}` still points at it; drop_fk first", was.name, f.name),
                        );
                    }
                }
            }
            s.tables.remove(&was.name);
        }
        Op::RenameTable { from, to } => {
            if s.tables.contains_key(to) || s.externs.contains_key(to) {
                return err(E_STATE, format!("rename_table: `{to}` exists"));
            }
            let mut t = s.tables.remove(from).ok_or_else(|| RenderError {
                code: E_STATE,
                msg: format!("table `{from}` is not in state"),
            })?;
            let tm = Arc::make_mut(&mut t);
            tm.decl.name.clone_from(to);
            for i in &mut tm.indexes {
                i.decl.table.clone_from(to);
            }
            s.tables.insert(to.clone(), t);
            // Copy only the tables that point at the renamed one.
            for ts in s.tables.values_mut() {
                if ts.decl.fks.iter().any(|f| &f.ref_table == from) {
                    for f in &mut Arc::make_mut(ts).decl.fks {
                        if &f.ref_table == from {
                            f.ref_table.clone_from(to);
                        }
                    }
                }
            }
        }
        Op::AlterTableComment { table: t, from, to } => {
            let ts = table_mut(s, t)?;
            if &ts.decl.comment != from {
                return err(
                    E_STATE,
                    format!("alter_table_comment `{t}`: `from` differs"),
                );
            }
            ts.decl.comment.clone_from(to);
        }
        Op::AddColumn {
            table: t, column, ..
        } => {
            let ts = table_mut(s, t)?;
            if ts.decl.columns.iter().any(|c| c.name == column.name) {
                return err(E_STATE, format!("column `{t}.{}` exists", column.name));
            }
            if column.primary_key || column.auto {
                return err(E_UNSUPPORTED, "add_column cannot add a primary key");
            }
            ts.decl.columns.push(column.clone());
        }
        Op::DropColumn { table: t, was } => {
            let ts = table(s, t)?;
            let live = ts.decl.columns.iter().find(|c| c.name == was.name);
            if live != Some(was) {
                return err(
                    E_STATE,
                    format!("drop_column `{t}.{}`: `was` differs", was.name),
                );
            }
            if was.primary_key {
                return err(
                    E_UNSUPPORTED,
                    "drop_column cannot drop a primary key column",
                );
            }
            if let Some((code, what)) = users_of(s, ts, &was.name).into_iter().next() {
                return err(
                    code,
                    format!(
                        "drop_column `{t}.{}`: {what} uses it; drop that first",
                        was.name
                    ),
                );
            }
            table_mut(s, t)?.decl.columns.retain(|c| c.name != was.name);
        }
        Op::RenameColumn { table: t, from, to } => {
            let ts = table(s, t)?;
            if ts.decl.columns.iter().any(|c| &c.name == to) {
                return err(E_STATE, format!("column `{t}.{to}` exists"));
            }
            if let Some((_, what)) = users_of(s, ts, from)
                .into_iter()
                .find(|(code, _)| *code == E_CHECK_COL)
            {
                return err(
                    E_CHECK_COL,
                    format!(
                        "rename_column `{t}.{from}`: {what} names it; write drop_check before \
                         and add_check (new text) after"
                    ),
                );
            }
            let ts = table_mut(s, t)?;
            column_mut(ts, from)?.name.clone_from(to);
            for i in &mut ts.indexes {
                for c in &mut i.decl.columns {
                    if c == from {
                        c.clone_from(to);
                    }
                }
            }
            for f in &mut ts.decl.fks {
                for c in &mut f.columns {
                    if c == from {
                        c.clone_from(to);
                    }
                }
            }
            for other in s.tables.values_mut() {
                let hit = |f: &FkDecl| &f.ref_table == t && f.ref_columns.iter().any(|c| c == from);
                if !other.decl.fks.iter().any(hit) {
                    continue;
                }
                for f in &mut Arc::make_mut(other).decl.fks {
                    if &f.ref_table == t {
                        for c in &mut f.ref_columns {
                            if c == from {
                                c.clone_from(to);
                            }
                        }
                    }
                }
            }
        }
        Op::AlterColumn { table: t, from, to } => {
            let ts = table(s, t)?;
            if ts.decl.columns.iter().find(|c| c.name == from.name) != Some(from) {
                return err(
                    E_STATE,
                    format!("alter_column `{t}.{}`: `from` differs", from.name),
                );
            }
            if from.name != to.name {
                return err(E_STATE, "alter_column cannot rename; use rename_column");
            }
            if from.primary_key != to.primary_key
                || from.auto != to.auto
                || from.generated != to.generated
            {
                return err(
                    E_UNSUPPORTED,
                    format!("alter_column `{t}.{}`: primary_key, auto and generated_as changes are not supported", to.name),
                );
            }
            let retype = from.ty != to.ty || from.max_length != to.max_length;
            if retype {
                if let Some(f) = fks_using(s, t, &to.name).first() {
                    return err(
                        E_FK_TYPE,
                        format!(
                            "alter_column `{t}.{}` changes its type under FK `{}`; write drop_fk \
                             before and add_fk after (MySQL error 3780 otherwise)",
                            to.name, f.name
                        ),
                    );
                }
            }
            *column_mut(table_mut(s, t)?, &to.name)? = to.clone();
        }
        Op::AddIndex { index, .. } => add_index(s, index)?,
        Op::DropIndex { was, .. } => {
            let ts = table_mut(s, &was.decl.table)?;
            let before = ts.indexes.len();
            ts.indexes.retain(|i| i != was);
            if ts.indexes.len() == before {
                return err(
                    E_STATE,
                    format!("drop_index `{}`: `was` differs", was.decl.name),
                );
            }
        }
        Op::RenameIndex { table: t, from, to } => {
            claim(s, to)?;
            let ts = table_mut(s, t)?;
            let i = ts
                .indexes
                .iter_mut()
                .find(|i| &i.decl.name == from)
                .ok_or_else(|| RenderError {
                    code: E_STATE,
                    msg: format!("index `{from}` is not on `{t}`"),
                })?;
            i.decl.name.clone_from(to);
        }
        Op::AddCheck { table: t, check } => {
            claim(s, &check.name)?;
            table_mut(s, t)?.decl.checks.push(check.clone());
        }
        Op::DropCheck { table: t, was } => {
            let ts = table_mut(s, t)?;
            let n = ts.decl.checks.len();
            ts.decl.checks.retain(|c| c != was);
            if ts.decl.checks.len() == n {
                return err(E_STATE, format!("drop_check `{}`: `was` differs", was.name));
            }
        }
        Op::AddFk { table: t, fk } => {
            table(s, t)?;
            claim(s, &fk.name)?;
            check_fk(s, t, fk)?;
            table_mut(s, t)?.decl.fks.push(fk.clone());
        }
        Op::DropFk { table: t, was } => {
            let ts = table_mut(s, t)?;
            let n = ts.decl.fks.len();
            ts.decl.fks.retain(|f| f != was);
            if ts.decl.fks.len() == n {
                return err(E_STATE, format!("drop_fk `{}`: `was` differs", was.name));
            }
        }
        Op::AlterFk { table: t, from, to } => {
            let same_shape = from.name == to.name
                && from.columns == to.columns
                && from.ref_table == to.ref_table
                && from.ref_columns == to.ref_columns;
            if !same_shape {
                return err(E_STATE, "alter_fk changes only on_delete");
            }
            let ts = table_mut(s, t)?;
            let f = ts
                .decl
                .fks
                .iter_mut()
                .find(|f| *f == from)
                .ok_or_else(|| RenderError {
                    code: E_STATE,
                    msg: format!("alter_fk `{}`: `from` differs", from.name),
                })?;
            *f = to.clone();
        }
        Op::RawSql { state, .. } => {
            for o in state {
                if matches!(o, Op::RawSql { .. } | Op::StateOnly { .. }) {
                    return err(E_STATE, "raw_sql state ops are schema ops only");
                }
                apply(s, o)?;
            }
        }
        Op::StateOnly { ops, .. } => {
            for o in ops {
                if matches!(o, Op::RawSql { .. } | Op::StateOnly { .. }) {
                    return err(E_STATE, "state_only nests schema ops only");
                }
                apply(s, o)?;
            }
        }
        Op::CreateExtension { name, .. } => {
            s.extensions.insert(name.clone());
        }
        Op::ExternTable(e) => {
            if s.tables.contains_key(&e.name) || s.externs.contains_key(&e.name) {
                return err(E_STATE, format!("table `{}` already exists", e.name));
            }
            s.externs.insert(e.name.clone(), e.clone());
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- render

fn q(d: &dyn Dialect, s: &str) -> String {
    d.quote_ident(s)
}

fn list(d: &dyn Dialect, cols: &[String]) -> String {
    cols.iter().map(|c| q(d, c)).collect::<Vec<_>>().join(", ")
}

fn tgt(t: &str) -> TableTarget<'_> {
    TableTarget::in_schema(None, t)
}

fn unit(kind: UnitKind, sql: Vec<String>) -> Vec<Unit> {
    if sql.is_empty() {
        return Vec::new();
    }
    vec![Unit { op: 0, kind, sql }]
}

/// One unit per statement on MySQL (each DDL commits), one tx elsewhere.
fn ddl(d: &dyn Dialect, sql: Vec<String>) -> Vec<Unit> {
    if d.kind() == DialectKind::Mysql {
        sql.into_iter()
            .map(|s| Unit {
                op: 0,
                kind: UnitKind::Autocommit,
                sql: vec![s],
            })
            .collect()
    } else {
        unit(UnitKind::Tx, sql)
    }
}

fn to_snapshot(t: &TableDecl) -> TableSnapshot {
    let mut snap: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": t.name, "model": "", "fields": [],
    }))
    .expect("a minimal TableSnapshot");
    snap.fields = t.columns.iter().map(ColumnDecl::snapshot).collect();
    snap.db_table_comment.clone_from(&t.comment);
    snap
}

/// The column as `CREATE TABLE`/`ADD COLUMN` writes it: the v1 definition
/// with the default as written.
fn col_def(d: &dyn Dialect, c: &ColumnDecl) -> String {
    diff::column_definition(&c.snapshot(), d, DefaultText::AsWritten)
}

fn fk_clause(d: &dyn Dialect, fk: &FkDecl) -> String {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        q(d, &fk.name),
        list(d, &fk.columns),
        q(d, &fk.ref_table),
        list(d, &fk.ref_columns)
    );
    if let Some(a) = &fk.on_delete {
        s.push_str(" ON DELETE ");
        s.push_str(a);
    }
    s
}

fn skipped(ix: &IndexState, d: DialectKind) -> bool {
    ix.skipped_on.contains(&d)
}

/// What `d` cannot build of `ix`, as a load error.
fn index_supported(d: DialectKind, ix: &IndexState) -> Result<(), RenderError> {
    if skipped(ix, d) {
        return Ok(());
    }
    let i = &ix.decl;
    let bad = match d {
        DialectKind::Postgres => None,
        DialectKind::Mysql => {
            if i.predicate.is_some() {
                Some("a partial index")
            } else if !i.include.is_empty() {
                Some("INCLUDE")
            } else if !matches!(i.method.as_str(), "" | "btree") {
                Some("a non-btree method")
            } else {
                None
            }
        }
        _ => {
            if !i.include.is_empty() {
                Some("INCLUDE")
            } else if !matches!(i.method.as_str(), "" | "btree") {
                Some("a non-btree method")
            } else {
                None
            }
        }
    };
    match bad {
        Some(what) => err(
            E_DIALECT,
            format!(
                "index `{}` uses {what}; add `skipped_on` for this dialect",
                i.name
            ),
        ),
        None => Ok(()),
    }
}

/// `CREATE [UNIQUE] INDEX` on PG/SQLite. No `IF NOT EXISTS`: it would keep
/// an invalid or different index of the same name.
fn create_index(d: &dyn Dialect, ix: &IndexDecl, concurrently: bool) -> String {
    let mut s = format!(
        "CREATE {}INDEX {}{} ON {}{} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        if concurrently { "CONCURRENTLY " } else { "" },
        q(d, &ix.name),
        q(d, &ix.table),
        d.index_method_clause(&ix.method),
        list(d, &ix.columns)
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", list(d, &ix.include)));
    }
    if let Some(p) = &ix.predicate {
        s.push_str(&format!(" WHERE {p}"));
    }
    s
}

fn mysql_index_clause(d: &dyn Dialect, ix: &IndexDecl) -> String {
    format!(
        "{}INDEX {} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        q(d, &ix.name),
        list(d, &ix.columns)
    )
}

fn is_serial_pk(d: &dyn Dialect, c: &ColumnDecl) -> bool {
    c.auto
        && c.primary_key
        && crate::migrate::snapshot::is_integer_ty(&c.ty)
        && d.serial_type_includes_primary_key()
}

/// `CREATE TABLE <name>` from `t`: columns, PK, checks and named FKs; on
/// MySQL also the indexes, so the op stays one statement.
fn create_table_sql(d: &dyn Dialect, t: &TableDecl, name: &str, inline: &[&IndexState]) -> String {
    let mut parts: Vec<String> = t.columns.iter().map(|c| col_def(d, c)).collect();
    let pk: Vec<String> = t
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect();
    if !pk.is_empty() && !t.columns.iter().any(|c| is_serial_pk(d, c)) {
        parts.push(format!("PRIMARY KEY ({})", list(d, &pk)));
    }
    for k in &t.checks {
        parts.push(format!("CONSTRAINT {} CHECK ({})", q(d, &k.name), k.expr));
    }
    for f in &t.fks {
        parts.push(fk_clause(d, f));
    }
    for ix in inline {
        parts.push(mysql_index_clause(d, &ix.decl));
    }
    let mut s = format!("CREATE TABLE {} ({})", q(d, name), parts.join(", "));
    if let Some(inline) = t
        .comment
        .as_deref()
        .and_then(|c| d.write_inline_table_comment(c))
    {
        s.push_str(&inline);
    }
    s
}

/// MySQL builds a hidden index named after an FK that no index covers, and
/// `DROP FOREIGN KEY` leaves it behind; state would not know it.
fn mysql_fk_covered(t: &TableState, fk: &FkDecl) -> Result<(), RenderError> {
    let prefix = |cols: &[String]| {
        cols.len() >= fk.columns.len() && cols[..fk.columns.len()] == fk.columns[..]
    };
    let pk: Vec<String> = t
        .decl
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.clone())
        .collect();
    let covered = prefix(&pk)
        || t.indexes
            .iter()
            .any(|i| !skipped(i, DialectKind::Mysql) && prefix(&i.decl.columns));
    if covered {
        Ok(())
    } else {
        err(
            E_FK_INDEX,
            format!(
                "FK `{}` on `{}`({}) has no index on MySQL; add_index on those columns first",
                fk.name,
                t.decl.name,
                fk.columns.join(", ")
            ),
        )
    }
}

/// The SQLite rebuild of `after`'s table: new table, copy, swap (v1's
/// `TableRebuild` tail), then its indexes from state, not the catalog.
fn rebuild(d: &dyn Dialect, after: &TableState, copy: &[(String, String)]) -> Vec<Unit> {
    let name = &after.decl.name;
    let new = format!("_rustango_rebuild_{name}");
    let mut rb = TableRebuild::new(&to_snapshot(&after.decl), Vec::new());
    for (c, e) in copy {
        rb = rb.copy_as(c, e.clone());
    }
    let mut sql = vec![create_table_sql(d, &after.decl, &new, &[])];
    sql.extend(rb.copy_and_swap(d, &new));
    for ix in after.indexes.iter().filter(|i| !skipped(i, d.kind())) {
        sql.push(create_index(d, &ix.decl, false));
    }
    unit(UnitKind::Rebuild, sql)
}

/// A temporary CHECK name for PG's online NOT NULL.
fn not_null_check(s: &State, t: &str, c: &str) -> Result<String, RenderError> {
    let name = format!("{t}_{c}_v2nn");
    claim(s, &name)?;
    Ok(name)
}

/// `{{t:..}}`, `{{c:..}}`, and `{{batch_range}}` when `batch` is given (data ops).
pub fn expand_templates(
    s: &State,
    after: &State,
    d: &dyn Dialect,
    sql: &str,
    batch: Option<&str>,
) -> Result<String, RenderError> {
    let mut out = String::new();
    let mut rest = sql;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let end = rest[at..].find("}}").ok_or_else(|| RenderError {
            code: E_STATE,
            msg: "unclosed `{{`".into(),
        })? + at;
        let tag = &rest[at + 2..end];
        match tag.split_once(':') {
            Some(("t", name)) => {
                if !s.tables.contains_key(name) && !after.tables.contains_key(name) {
                    return err(E_STATE, format!("`{{{{t:{name}}}}}` is not in state"));
                }
                out.push_str(&q(d, name));
            }
            Some(("c", name)) => out.push_str(&q(d, name)),
            None if tag == "batch_range" && batch.is_some() => {
                out.push_str(batch.unwrap_or_default());
            }
            _ => return err(E_STATE, format!("unknown template `{tag}`")),
        }
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

const SESSION_STMTS: &[&str] = &[
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "START TRANSACTION",
    "SET ROLE",
    "SET SEARCH_PATH",
    "SET SESSION",
    "USE ",
    "PRAGMA",
];

/// Render one op. `before`/`after` are the states around it; `created`
/// holds the tables this file created before it (empty, so no online recipe).
///
/// # Errors
/// What the dialect cannot express, or a rule the op breaks.
#[allow(clippy::too_many_lines)]
pub fn render_op(
    before: &State,
    after: &State,
    op: &Op,
    d: &dyn Dialect,
    created: &BTreeSet<String>,
) -> Result<Vec<Unit>, RenderError> {
    let k = d.kind();
    let (pg, my, lite) = (
        k == DialectKind::Postgres,
        k == DialectKind::Mysql,
        k == DialectKind::Sqlite,
    );
    let existed = |t: &str| !created.contains(t);
    Ok(match op {
        Op::CreateTable { table, indexes } => {
            for ix in indexes {
                index_supported(k, ix)?;
            }
            let ts = table_of(after, &table.name)?;
            if my {
                for f in &table.fks {
                    mysql_fk_covered(ts, f)?;
                }
                let inline: Vec<&IndexState> = indexes.iter().filter(|i| !skipped(i, k)).collect();
                unit(
                    UnitKind::Autocommit,
                    vec![create_table_sql(d, table, &table.name, &inline)],
                )
            } else {
                let mut sql = vec![create_table_sql(d, table, &table.name, &[])];
                if let Some(c) = &table.comment {
                    sql.extend(d.table_comment_statement(tgt(&table.name), c));
                }
                for c in &table.columns {
                    if let Some(cm) = &c.comment {
                        sql.extend(d.column_comment_statement(tgt(&table.name), &c.name, cm));
                    }
                }
                for ix in indexes.iter().filter(|i| !skipped(i, k)) {
                    sql.push(create_index(d, &ix.decl, false));
                }
                unit(UnitKind::Tx, sql)
            }
        }
        // PG without CASCADE: a missed FK fails loudly.
        Op::DropTable { was } => ddl(d, vec![format!("DROP TABLE {}", q(d, &was.name))]),
        Op::RenameTable { from, to } => {
            let mut sql = vec![format!("ALTER TABLE {} RENAME TO {}", q(d, from), q(d, to))];
            if pg {
                let t = table_of(before, from)?;
                // Names PG derived from the table; a later table of the old name collides.
                if t.decl.columns.iter().any(|c| c.primary_key) {
                    sql.push(format!(
                        "ALTER INDEX {} RENAME TO {}",
                        q(d, &format!("{from}_pkey")),
                        q(d, &format!("{to}_pkey"))
                    ));
                }
                for c in t.decl.columns.iter().filter(|c| c.auto) {
                    let old = format!("{from}_{}_seq", c.name);
                    let new = format!("{to}_{}_seq", c.name);
                    if old.len() > 63 || new.len() > 63 {
                        return err(
                            E_STATE,
                            format!("sequence name `{new}` is longer than 63 bytes"),
                        );
                    }
                    sql.push(format!(
                        "ALTER SEQUENCE {} RENAME TO {}",
                        q(d, &old),
                        q(d, &new)
                    ));
                }
            }
            ddl(d, sql)
        }
        Op::AlterTableComment { table, to, .. } => ddl(
            d,
            d.alter_table_comment_statement(tgt(table), to.as_deref().unwrap_or(""))
                .into_iter()
                .collect(),
        ),
        Op::AddColumn {
            table,
            column,
            fill,
        } => {
            let not_null_no_default =
                !column.nullable && column.default.is_none() && column.generated.is_none();
            if fill.is_some() && !not_null_no_default {
                return err(
                    E_FILL,
                    "`fill` is only for a NOT NULL column with no default",
                );
            }
            if not_null_no_default && fill.is_none() && existed(table) {
                return err(
                    E_FILL,
                    format!(
                        "add_column `{table}.{}` is NOT NULL with no default on an existing table; give it `fill`",
                        column.name
                    ),
                );
            }
            let t = q(d, table);
            let c = q(d, &column.name);
            if lite {
                let paren_default = column.default.as_deref().is_some_and(|x| {
                    let x = x.trim();
                    x.starts_with('(') || x.to_ascii_uppercase().starts_with("CURRENT_")
                });
                let needs_rebuild =
                    column.generated.is_some() || paren_default || not_null_no_default;
                if needs_rebuild {
                    let value = fill
                        .clone()
                        .or_else(|| column.default.clone())
                        .unwrap_or_else(|| "NULL".into());
                    let copy = if column.generated.is_some() {
                        vec![]
                    } else {
                        vec![(column.name.clone(), value)]
                    };
                    return Ok(rebuild(d, table_of(after, table)?, &copy));
                }
                return Ok(unit(
                    UnitKind::Tx,
                    vec![format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, column))],
                ));
            }
            let mut sql = Vec::new();
            match fill {
                Some(f) => {
                    let mut with = column.clone();
                    with.default = Some(f.clone());
                    sql.push(format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, &with)));
                    sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT"));
                }
                None => sql.push(format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, column))),
            }
            if pg {
                if let Some(cm) = &column.comment {
                    sql.extend(d.column_comment_statement(tgt(table), &column.name, cm));
                }
            }
            ddl(d, sql)
        }
        Op::DropColumn { table, was } => ddl(
            d,
            vec![format!(
                "ALTER TABLE {} DROP COLUMN {}",
                q(d, table),
                q(d, &was.name)
            )],
        ),
        Op::RenameColumn { table, from, to } => ddl(
            d,
            vec![format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                q(d, table),
                q(d, from),
                q(d, to)
            )],
        ),
        Op::AlterColumn { table, from, to } => {
            alter_column(before, after, d, table, from, to, existed(table))?
        }
        Op::AddIndex {
            index,
            concurrently,
        } => {
            index_supported(k, index)?;
            if skipped(index, k) {
                return Ok(Vec::new());
            }
            let ix = &index.decl;
            if my {
                let mut s = format!(
                    "ALTER TABLE {} ADD {}",
                    q(d, &ix.table),
                    mysql_index_clause(d, ix)
                );
                if existed(&ix.table) {
                    // Fail fast rather than fall back to a copying, locking ALTER.
                    s.push_str(", ALGORITHM=INPLACE, LOCK=NONE");
                }
                return Ok(unit(UnitKind::Autocommit, vec![s]));
            }
            let conc = pg && concurrently.unwrap_or_else(|| existed(&ix.table));
            let kind = if conc {
                UnitKind::Autocommit
            } else {
                UnitKind::Tx
            };
            unit(kind, vec![create_index(d, ix, conc)])
        }
        Op::DropIndex { was, concurrently } => {
            if skipped(was, k) {
                return Ok(Vec::new());
            }
            let ix = &was.decl;
            if my {
                let t = table_of(after, &ix.table)?;
                for f in &t.decl.fks {
                    mysql_fk_covered(t, f)?;
                }
                return Ok(unit(
                    UnitKind::Autocommit,
                    vec![format!(
                        "ALTER TABLE {} DROP INDEX {}",
                        q(d, &ix.table),
                        q(d, &ix.name)
                    )],
                ));
            }
            let conc = pg && concurrently.unwrap_or_else(|| existed(&ix.table));
            let kind = if conc {
                UnitKind::Autocommit
            } else {
                UnitKind::Tx
            };
            let c = if conc { "CONCURRENTLY " } else { "" };
            unit(kind, vec![format!("DROP INDEX {c}{}", q(d, &ix.name))])
        }
        Op::RenameIndex { table, from, to } => {
            let ix = table_of(after, table)?
                .indexes
                .iter()
                .find(|i| &i.decl.name == to)
                .expect("apply renamed it");
            if skipped(ix, k) {
                return Ok(Vec::new());
            }
            match k {
                DialectKind::Postgres => unit(
                    UnitKind::Tx,
                    vec![format!("ALTER INDEX {} RENAME TO {}", q(d, from), q(d, to))],
                ),
                DialectKind::Mysql => unit(
                    UnitKind::Autocommit,
                    vec![format!(
                        "ALTER TABLE {} RENAME INDEX {} TO {}",
                        q(d, table),
                        q(d, from),
                        q(d, to)
                    )],
                ),
                // No RENAME INDEX: drop and re-create from state.
                _ => unit(
                    UnitKind::Tx,
                    vec![
                        format!("DROP INDEX {}", q(d, from)),
                        create_index(d, &ix.decl, false),
                    ],
                ),
            }
        }
        Op::AddCheck { table, check } => {
            if lite {
                return Ok(rebuild(d, table_of(after, table)?, &[]));
            }
            let add = format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({})",
                q(d, table),
                q(d, &check.name),
                check.expr
            );
            if pg && existed(table) {
                validate_later(d, table, &check.name, add)
            } else {
                ddl(d, vec![add])
            }
        }
        Op::DropCheck { table, was } => {
            if lite {
                return Ok(rebuild(d, table_of(after, table)?, &[]));
            }
            ddl(
                d,
                d.drop_check_constraint_sql(tgt(table), &was.name)
                    .into_iter()
                    .collect(),
            )
        }
        Op::AddFk { table, fk } => {
            if lite {
                return Ok(rebuild(d, table_of(after, table)?, &[]));
            }
            if my {
                mysql_fk_covered(table_of(after, table)?, fk)?;
            }
            let add = format!("ALTER TABLE {} ADD {}", q(d, table), fk_clause(d, fk));
            if pg && existed(table) {
                validate_later(d, table, &fk.name, add)
            } else {
                ddl(d, vec![add])
            }
        }
        Op::DropFk { table, was } => {
            if lite {
                return Ok(rebuild(d, table_of(after, table)?, &[]));
            }
            ddl(
                d,
                d.drop_foreign_key_sql(tgt(table), &was.name)
                    .into_iter()
                    .collect(),
            )
        }
        Op::AlterFk { table, to, .. } => {
            if lite {
                return Ok(rebuild(d, table_of(after, table)?, &[]));
            }
            let t = q(d, table);
            if my {
                // One ALTER cannot drop and re-add an FK name (1826).
                ddl(
                    d,
                    vec![
                        d.drop_foreign_key_sql(tgt(table), &to.name)
                            .expect("MySQL drops FKs"),
                        format!("ALTER TABLE {t} ADD {}", fk_clause(d, to)),
                    ],
                )
            } else {
                let both = format!(
                    "ALTER TABLE {t} DROP CONSTRAINT {}, ADD {}",
                    q(d, &to.name),
                    fk_clause(d, to)
                );
                if existed(table) {
                    validate_later(d, table, &to.name, both)
                } else {
                    unit(UnitKind::Tx, vec![both])
                }
            }
        }
        Op::RawSql { up, autocommit, .. } => {
            let Some(stmts) = up.get(k) else {
                return Ok(Vec::new());
            };
            let mut sql = Vec::new();
            for s in stmts {
                let s = expand_templates(before, after, d, s, None)?;
                let head = s.trim_start().to_ascii_uppercase();
                if !autocommit && SESSION_STMTS.iter().any(|p| head.starts_with(p)) {
                    return err(
                        E_RAW,
                        format!("`{s}` is not allowed in a transactional unit"),
                    );
                }
                sql.push(s);
            }
            if *autocommit || my {
                sql.into_iter()
                    .map(|s| Unit {
                        op: 0,
                        kind: UnitKind::Autocommit,
                        sql: vec![s],
                    })
                    .collect()
            } else {
                unit(UnitKind::Tx, sql)
            }
        }
        Op::StateOnly { .. } | Op::ExternTable(_) | Op::AdoptTable { .. } => Vec::new(),
        Op::CreateExtension {
            name,
            schema,
            skipped_on,
        } => {
            if skipped_on.contains(&k) {
                return Ok(Vec::new());
            }
            if !pg {
                return err(
                    E_DIALECT,
                    format!(
                        "create_extension `{name}` needs `skipped_on` for {}",
                        d.name()
                    ),
                );
            }
            let mut s = format!("CREATE EXTENSION IF NOT EXISTS {}", q(d, name));
            if let Some(sc) = schema {
                s.push_str(&format!(" SCHEMA {}", q(d, sc)));
            }
            unit(UnitKind::Tx, vec![s])
        }
    })
}

fn table_of<'s>(s: &'s State, t: &str) -> Result<&'s TableState, RenderError> {
    table(s, t)
}

/// PG on an existing table: add `NOT VALID`, then `VALIDATE` in its own
/// unit, so the scan holds only a SHARE UPDATE EXCLUSIVE lock.
fn validate_later(d: &dyn Dialect, table: &str, name: &str, add: String) -> Vec<Unit> {
    vec![
        Unit {
            op: 0,
            kind: UnitKind::Tx,
            sql: vec![format!("{add} NOT VALID")],
        },
        Unit {
            op: 0,
            kind: UnitKind::Tx,
            sql: vec![format!(
                "ALTER TABLE {} VALIDATE CONSTRAINT {}",
                q(d, table),
                q(d, name)
            )],
        },
    ]
}

/// `alter_column`: v1's `push_alter_changes` splits it; PG renders each
/// change through the v1 arms, MySQL restates the column once, SQLite rebuilds.
fn alter_column(
    before: &State,
    after: &State,
    d: &dyn Dialect,
    table: &str,
    from: &ColumnDecl,
    to: &ColumnDecl,
    existed: bool,
) -> Result<Vec<Unit>, RenderError> {
    let mut changes = Vec::new();
    diff::push_alter_changes(table, &from.snapshot(), &to.snapshot(), &mut changes);
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    match d.kind() {
        DialectKind::Mysql => Ok(unit(
            UnitKind::Autocommit,
            vec![format!(
                "ALTER TABLE {} MODIFY COLUMN {}",
                q(d, table),
                col_def(d, to)
            )],
        )),
        DialectKind::Postgres => {
            // NULL-ness is v2's: v1 fills NULLs with an UPDATE and never goes online.
            let (null, rest): (Vec<_>, Vec<_>) = changes
                .into_iter()
                .partition(|c| matches!(c, SchemaChange::AlterColumnNullable { .. }));
            let after_t = table_of(after, table)?;
            let lowered = SchemaSnapshot {
                tables: vec![to_snapshot(&after_t.decl)],
                indexes: after_t
                    .indexes
                    .iter()
                    .map(|i| index_snapshot(&i.decl))
                    .collect(),
                ..SchemaSnapshot::default()
            };
            let batch =
                diff::render_changes_split_with_dialect(&rest, &lowered, d).map_err(|m| {
                    RenderError {
                        code: E_STATE,
                        msg: m,
                    }
                })?;
            assert!(batch.deferred_fks.is_empty() && batch.rebuild.is_none());
            let mut sql = batch.immediate;
            let mut out = Vec::new();
            let t = q(d, table);
            let c = q(d, &to.name);
            match null.first() {
                Some(SchemaChange::AlterColumnNullable { nullable: true, .. }) => {
                    sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP NOT NULL"));
                }
                Some(_) if !existed => {
                    sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} SET NOT NULL"));
                }
                Some(_) => {
                    let name = not_null_check(before, table, &to.name)?;
                    let nn = q(d, &name);
                    out.extend(unit(UnitKind::Tx, std::mem::take(&mut sql)));
                    out.extend(validate_later(
                        d,
                        table,
                        &name,
                        format!("ALTER TABLE {t} ADD CONSTRAINT {nn} CHECK ({c} IS NOT NULL)"),
                    ));
                    // PG 12+ skips the scan: a valid CHECK proves the column has no NULL.
                    sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} SET NOT NULL"));
                    sql.push(format!("ALTER TABLE {t} DROP CONSTRAINT {nn}"));
                }
                None => {}
            }
            out.extend(unit(UnitKind::Tx, sql));
            Ok(out)
        }
        _ => {
            // VARCHAR(n) is TEXT there and has no comments: v1 writes nothing.
            let only_cosmetic = changes.iter().all(|c| {
                matches!(
                    c,
                    SchemaChange::AlterColumnMaxLength { .. }
                        | SchemaChange::AlterColumnComment { .. }
                )
            });
            if only_cosmetic {
                return Ok(Vec::new());
            }
            Ok(rebuild(d, table_of(after, table)?, &[]))
        }
    }
}

fn index_snapshot(i: &IndexDecl) -> IndexSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": i.name, "table": i.table, "columns": i.columns, "unique": i.unique,
        "method": i.method, "where_clause": i.predicate, "include": i.include,
    }))
    .expect("an IndexSnapshot")
}

/// Render a whole file from `state`: one op at a time, each against the
/// state just before it. Returns the units and the state after.
///
/// # Errors
/// The first op that fails to replay or render, with its index.
pub fn render_file(
    state: &State,
    ops: &[Op],
    d: &dyn Dialect,
) -> Result<(Vec<Unit>, State), (usize, RenderError)> {
    let mut st = state.clone();
    let mut created = BTreeSet::new();
    let mut units = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let mut after = st.clone();
        apply(&mut after, op).map_err(|e| (i, e))?;
        for mut u in render_op(&st, &after, op, d, &created).map_err(|e| (i, e))? {
            u.op = i;
            units.push(u);
        }
        match op {
            Op::CreateTable { table, .. } => {
                created.insert(table.name.clone());
            }
            Op::RenameTable { from, to } if created.remove(from) => {
                created.insert(to.clone());
            }
            Op::DropTable { was } => {
                created.remove(&was.name);
            }
            _ => {}
        }
        st = after;
    }
    Ok((units, st))
}

/// Units as golden text.
#[must_use]
pub fn golden(units: &[Unit]) -> String {
    let mut s = String::new();
    for u in units {
        let kind = match u.kind {
            UnitKind::Tx => "tx",
            UnitKind::Autocommit => "autocommit",
            UnitKind::Rebuild => "rebuild",
        };
        s.push_str(&format!("[op {}] {kind}\n", u.op));
        for stmt in &u.sql {
            s.push_str(&format!("  {stmt};\n"));
        }
    }
    s
}
