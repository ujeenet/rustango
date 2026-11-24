//! State by replay: each op checks its preconditions and its `was`/`from`
//! against state, then changes only what it names. Pure: no models, clock,
//! env or catalog.

use std::sync::Arc;

use super::format::{ColumnChanges, Field, File, Kind, Op};
use super::state::{
    pg_object_name, CheckDef, ColumnDef, ExternTable, FkDef, IndexDef, State, TableState, Target,
};
use super::{Code, LoadError, MAX_IDENT};

/// What an op may look at besides the state it changes.
pub(crate) struct Ctx<'a> {
    /// The file's target.
    pub(crate) target: Target,
    /// The registry state, to name a tenant→registry FK as such.
    pub(crate) registry: Option<&'a State>,
}

/// Replay one file onto `state`. `at` names the file in errors.
///
/// # Errors
/// The first op that does not replay, with its JSON pointer.
pub(crate) fn apply_file(
    state: &mut State,
    file: &File,
    at: &str,
    ctx: &Ctx<'_>,
) -> Result<(), LoadError> {
    if file.kind == Kind::Checkpoint {
        return apply_checkpoint(state, file, at);
    }
    let mut created: Vec<&str> = Vec::new();
    for (i, env) in file.operations.iter().enumerate() {
        let op_at = format!("{at}#/operations/{i}");
        apply(state, &env.op, &op_at, ctx, &created)?;
        if let Op::CreateTable { table, .. } = &env.op {
            created.push(&table.name);
        }
    }
    Ok(())
}

/// An adopt checkpoint brings its tables in whole.
fn apply_checkpoint(state: &mut State, file: &File, at: &str) -> Result<(), LoadError> {
    let doc = file.state.clone().unwrap_or_default();
    for (name, t) in doc.tables {
        if name != t.name || t.scope != file.target {
            return Err(LoadError::new(
                Code::Checkpoint,
                format!("{at}#/state/tables/{name}"),
                "the key must be the table name and its scope the file's target",
            ));
        }
        if taken(state, &name) {
            return Err(fail(at, format!("table `{name}` already exists")));
        }
        state.tables.insert(Arc::from(name), Arc::new(t));
    }
    for (name, e) in doc.externs {
        state.externs.insert(Arc::from(name), Arc::new(e));
    }
    state.extensions.extend(doc.extensions);
    Ok(())
}

fn fail(at: &str, msg: impl Into<String>) -> LoadError {
    LoadError::new(Code::State, at, msg)
}

fn coded(code: Code, at: &str, msg: impl Into<String>) -> LoadError {
    LoadError::new(code, at, msg)
}

/// Replay one op.
///
/// # Errors
/// Any broken precondition, or a `was`/`from` that differs from state.
pub(crate) fn apply(
    s: &mut State,
    op: &Op,
    at: &str,
    ctx: &Ctx<'_>,
    created_here: &[&str],
) -> Result<(), LoadError> {
    match op {
        Op::CreateTable {
            table,
            indexes,
            checks,
        }
        | Op::AdoptTable {
            table,
            indexes,
            checks,
        } => {
            ident(at, &table.name)?;
            if taken(s, &table.name) {
                return Err(fail(at, format!("`{}` already exists", table.name)));
            }
            let mut columns = Vec::with_capacity(table.fields.len());
            for f in &table.fields {
                ident(at, &f.column.column)?;
                if columns
                    .iter()
                    .any(|c: &ColumnDef| c.column == f.column.column)
                {
                    return Err(fail(at, format!("column `{}` twice", f.column.column)));
                }
                columns.push(f.column.clone());
            }
            let fresh = TableState {
                name: table.name.clone(),
                scope: table.scope,
                columns,
                indexes: Vec::new(),
                checks: Vec::new(),
                fks: Vec::new(),
                comment: table.comment.clone(),
                pg_pkey: None,
                pg_sequence: None,
            };
            s.tables
                .insert(Arc::from(table.name.as_str()), Arc::new(fresh));
            name_pg_objects(s, &table.name);
            for c in checks {
                add_check(s, at, &table.name, c)?;
            }
            for ix in indexes {
                add_index(s, at, &table.name, ix)?;
            }
            // After the keys exist, so a self-reference and its index resolve.
            for f in &table.fields {
                if let Some(fk) = &f.fk {
                    add_fk(s, at, ctx, &table.name, fk.state(&f.column.column))?;
                }
            }
        }
        Op::DropTable { table, was, .. } => {
            let live = get(s, at, table)?;
            if was.name != *table || live.normalised() != was.normalised() {
                return Err(fail(
                    at,
                    format!("drop_table `{table}`: `was` differs from state"),
                ));
            }
            for (name, other) in &s.tables {
                if &**name == table {
                    continue;
                }
                if let Some(f) = other.fks.iter().find(|f| &f.to == table) {
                    return Err(fail(
                        at,
                        format!(
                            "drop_table `{table}`: FK `{}` on `{name}` still points at it; \
                             drop_fk first",
                            f.name
                        ),
                    ));
                }
            }
            s.tables.remove(table.as_str());
        }
        Op::RenameTable { from, to } => {
            ident(at, to)?;
            if taken(s, to) {
                return Err(fail(at, format!("rename_table: `{to}` is taken")));
            }
            let mut t = s
                .tables
                .remove(from.as_str())
                .ok_or_else(|| fail(at, format!("table `{from}` is not in state")))?;
            let tm = Arc::make_mut(&mut t);
            tm.name.clone_from(to);
            for f in &mut tm.fks {
                if &f.to == from {
                    f.to.clone_from(to);
                }
            }
            // PG renames the owned sequence too; the pkey keeps its name.
            let had_sequence = tm.pg_sequence.take().is_some();
            s.tables.insert(Arc::from(to.as_str()), t);
            if had_sequence {
                name_pg_sequence(s, to);
            }
            for ts in s.tables.values_mut() {
                if ts.fks.iter().any(|f| &f.to == from) {
                    for f in &mut Arc::make_mut(ts).fks {
                        if &f.to == from {
                            f.to.clone_from(to);
                        }
                    }
                }
            }
        }
        Op::AlterTableComment { table, from, to } => {
            let t = get_mut(s, at, table)?;
            if &t.comment != from {
                return Err(fail(
                    at,
                    format!("alter_table_comment `{table}`: `from` differs"),
                ));
            }
            t.comment.clone_from(to);
        }
        Op::AddColumn {
            table,
            column,
            fill,
        } => add_column(s, at, table, column, fill.as_deref(), created_here)?,
        Op::DropColumn {
            table, column, was, ..
        } => {
            let t = get(s, at, table)?;
            if was.column.column != *column || t.column(column) != Some(&was.column) {
                return Err(fail(
                    at,
                    format!("drop_column `{table}.{column}`: `was` differs"),
                ));
            }
            if was.fk.is_some() {
                return Err(fail(
                    at,
                    format!("drop_column `{table}.{column}`: drop_fk first"),
                ));
            }
            if was.column.primary_key {
                return Err(coded(
                    Code::Unsupported,
                    at,
                    "drop_column cannot drop a primary key column",
                ));
            }
            if let Some((code, what)) = first_user(s, t, column, false) {
                return Err(coded(
                    code,
                    at,
                    format!("drop_column `{table}.{column}`: {what} uses it; drop that first"),
                ));
            }
            get_mut(s, at, table)?
                .columns
                .retain(|c| c.column != *column);
        }
        Op::RenameColumn { table, from, to } => rename_column(s, at, table, from, to)?,
        Op::AlterColumn {
            table,
            column,
            changes,
            ..
        } => alter_column(s, at, table, column, changes)?,
        Op::AddFk { table, column, fk } => {
            get(s, at, table)?;
            add_fk(s, at, ctx, table, fk.state(column))?;
        }
        Op::DropFk { table, column, was } => {
            let want = was.state(column);
            let t = get_mut(s, at, table)?;
            let n = t.fks.len();
            t.fks.retain(|f| *f != want);
            if t.fks.len() == n {
                return Err(fail(at, format!("drop_fk `{}`: `was` differs", was.name)));
            }
        }
        Op::AddIndex { table, index, .. } => add_index(s, at, table, index)?,
        Op::DropIndex {
            table, name, was, ..
        } => {
            let t = get(s, at, table)?;
            let live = t.indexes.iter().find(|i| &i.name == name);
            if was.name != *name || live.map(sorted_skips) != Some(sorted_skips(was)) {
                return Err(fail(at, format!("drop_index `{name}`: `was` differs")));
            }
            if let Some(f) = t
                .fks
                .iter()
                .find(|f| covers(was, &f.columns) && !covered(t, &f.columns, Some(name)))
            {
                return Err(coded(
                    Code::FkIndex,
                    at,
                    format!(
                        "drop_index `{name}`: it is the only index covering FK `{}`; drop_fk first",
                        f.name
                    ),
                ));
            }
            get_mut(s, at, table)?.indexes.retain(|i| &i.name != name);
        }
        Op::RenameIndex { table, from, to } => {
            claim(s, at, to)?;
            let t = get_mut(s, at, table)?;
            let i = t
                .indexes
                .iter_mut()
                .find(|i| &i.name == from)
                .ok_or_else(|| fail(at, format!("index `{from}` is not on `{table}`")))?;
            i.name.clone_from(to);
        }
        Op::AddCheck { table, name, expr } => add_check(
            s,
            at,
            table,
            &CheckDef {
                name: name.clone(),
                expr: expr.clone(),
            },
        )?,
        Op::DropCheck { table, name, was } => {
            let t = get_mut(s, at, table)?;
            let n = t.checks.len();
            t.checks.retain(|c| !(&c.name == name && &c.expr == was));
            if t.checks.len() == n {
                return Err(fail(at, format!("drop_check `{name}`: `was` differs")));
            }
        }
        Op::RawSql { .. } | Op::StateOnly { .. } => {
            for (i, nested) in op.nested(at)?.iter().enumerate() {
                apply(s, nested, &format!("{at}/state/{i}"), ctx, created_here)?;
            }
        }
        Op::CreateExtension { name, .. } => {
            s.extensions.insert(name.clone());
        }
        Op::ExternTable {
            table,
            columns,
            unique_sets,
            owner,
        } => {
            ident(at, table)?;
            if taken(s, table) {
                return Err(fail(at, format!("`{table}` already exists")));
            }
            if ctx.target == Target::Tenant
                && ctx
                    .registry
                    .is_some_and(|r| r.tables.contains_key(table.as_str()))
            {
                return Err(coded(
                    Code::CrossTarget,
                    at,
                    format!("`{table}` is a registry table; a tenant chain cannot declare it"),
                ));
            }
            s.externs.insert(
                Arc::from(table.as_str()),
                Arc::new(ExternTable {
                    table: table.clone(),
                    columns: columns.clone(),
                    unique_sets: unique_sets.clone(),
                    owner: *owner,
                }),
            );
        }
    }
    Ok(())
}

fn get<'s>(s: &'s State, at: &str, t: &str) -> Result<&'s TableState, LoadError> {
    s.tables
        .get(t)
        .map(|a| &**a)
        .ok_or_else(|| fail(at, format!("table `{t}` is not in state")))
}

fn get_mut<'s>(s: &'s mut State, at: &str, t: &str) -> Result<&'s mut TableState, LoadError> {
    s.tables
        .get_mut(t)
        .map(Arc::make_mut)
        .ok_or_else(|| fail(at, format!("table `{t}` is not in state")))
}

fn ident(at: &str, name: &str) -> Result<(), LoadError> {
    if name.len() > MAX_IDENT || name.is_empty() {
        return Err(coded(
            Code::Identifier,
            at,
            format!("`{name}` must be 1 to {MAX_IDENT} bytes"),
        ));
    }
    Ok(())
}

/// A table, index, check, FK, pkey or sequence uses `name`. One namespace
/// per target; a scan, so a claim allocates nothing.
pub(crate) fn taken(s: &State, name: &str) -> bool {
    s.externs.contains_key(name)
        || s.tables.iter().any(|(k, t)| {
            &**k == name
                || t.pg_pkey.as_deref() == Some(name)
                || t.pg_sequence.as_deref() == Some(name)
                || t.indexes.iter().any(|i| i.name == name)
                || t.checks.iter().any(|c| c.name == name)
                || t.fks.iter().any(|f| f.name == name)
        })
}

fn claim(s: &State, at: &str, name: &str) -> Result<(), LoadError> {
    ident(at, name)?;
    if taken(s, name) {
        return Err(fail(at, format!("the name `{name}` is taken in state")));
    }
    Ok(())
}

/// PG's pkey and sequence names for a new table, as PG picks them.
fn name_pg_objects(s: &mut State, table: &str) {
    let has_pk = s.tables[table].columns.iter().any(|c| c.primary_key);
    if has_pk {
        let name = pg_object_name(table, None, "pkey", |n| taken(s, n));
        get_table(s, table).pg_pkey = Some(name);
    }
    name_pg_sequence(s, table);
}

fn name_pg_sequence(s: &mut State, table: &str) {
    let auto = s.tables[table]
        .columns
        .iter()
        .find(|c| c.auto)
        .map(|c| c.column.clone());
    if let Some(col) = auto {
        let name = pg_object_name(table, Some(&col), "seq", |n| taken(s, n));
        get_table(s, table).pg_sequence = Some(name);
    }
}

fn get_table<'s>(s: &'s mut State, table: &str) -> &'s mut TableState {
    Arc::make_mut(s.tables.get_mut(table).expect("caller checked"))
}

fn add_check(s: &mut State, at: &str, table: &str, c: &CheckDef) -> Result<(), LoadError> {
    claim(s, at, &c.name)?;
    get_mut(s, at, table)?.checks.push(c.clone());
    Ok(())
}

fn add_index(s: &mut State, at: &str, table: &str, ix: &IndexDef) -> Result<(), LoadError> {
    claim(s, at, &ix.name)?;
    let t = get_mut(s, at, table)?;
    if ix.columns.is_empty() {
        return Err(fail(at, format!("index `{}` has no columns", ix.name)));
    }
    if let Some(c) = ix
        .columns
        .iter()
        .chain(&ix.include)
        .find(|c| t.column(c).is_none())
    {
        return Err(fail(at, format!("index `{}`: no column `{c}`", ix.name)));
    }
    t.indexes.push(ix.clone());
    Ok(())
}

fn add_column(
    s: &mut State,
    at: &str,
    table: &str,
    field: &Field,
    fill: Option<&str>,
    created_here: &[&str],
) -> Result<(), LoadError> {
    let c = &field.column;
    ident(at, &c.column)?;
    let t = get(s, at, table)?;
    if t.column(&c.column).is_some() {
        return Err(fail(at, format!("column `{table}.{}` exists", c.column)));
    }
    if c.primary_key || c.auto {
        return Err(coded(
            Code::Unsupported,
            at,
            "add_column cannot add a primary key or auto column",
        ));
    }
    if field.fk.is_some() {
        return Err(coded(
            Code::FkIndex,
            at,
            "add_column cannot carry an FK: add the column, an index on it, then add_fk",
        ));
    }
    if !c.nullable && c.default.is_none() && fill.is_none() && !created_here.contains(&table) {
        return Err(coded(
            Code::Fill,
            at,
            format!(
                "`{table}.{}` is NOT NULL with no default on an existing table: give `fill`",
                c.column
            ),
        ));
    }
    get_mut(s, at, table)?.columns.push(c.clone());
    Ok(())
}

fn rename_column(
    s: &mut State,
    at: &str,
    table: &str,
    from: &str,
    to: &str,
) -> Result<(), LoadError> {
    ident(at, to)?;
    let t = get(s, at, table)?;
    if t.column(from).is_none() {
        return Err(fail(at, format!("column `{table}.{from}` is not in state")));
    }
    if t.column(to).is_some() {
        return Err(fail(at, format!("column `{table}.{to}` exists")));
    }
    if let Some((_, what)) = first_user(s, t, from, true) {
        return Err(coded(
            Code::CheckColumn,
            at,
            format!(
                "rename_column `{table}.{from}`: {what} names it; write drop_check before and \
                 add_check (new text) after"
            ),
        ));
    }
    let rename = |c: &mut String| {
        if c == from {
            *c = to.to_owned();
        }
    };
    let t = get_mut(s, at, table)?;
    for c in &mut t.columns {
        rename(&mut c.column);
    }
    for i in &mut t.indexes {
        i.columns.iter_mut().chain(&mut i.include).for_each(rename);
    }
    for f in &mut t.fks {
        f.columns.iter_mut().for_each(rename);
    }
    for other in s.tables.values_mut() {
        let hit = |f: &FkDef| f.to == table && f.on.iter().any(|c| c == from);
        if other.fks.iter().any(hit) {
            for f in &mut Arc::make_mut(other).fks {
                if f.to == table {
                    f.on.iter_mut().for_each(rename);
                }
            }
        }
    }
    Ok(())
}

fn alter_column(
    s: &mut State,
    at: &str,
    table: &str,
    column: &str,
    ch: &ColumnChanges,
) -> Result<(), LoadError> {
    if ch.primary_key.is_some() || ch.auto.is_some() || ch.generated_as.is_some() {
        return Err(coded(
            Code::Unsupported,
            at,
            format!(
                "alter_column `{table}.{column}`: primary_key, auto and generated_as changes \
                 are not supported"
            ),
        ));
    }
    let t = get(s, at, table)?;
    let mut col = t
        .column(column)
        .ok_or_else(|| fail(at, format!("column `{table}.{column}` is not in state")))?
        .clone();
    macro_rules! change {
        ($field:ident) => {
            if let Some(c) = &ch.$field {
                if col.$field != c.from {
                    return Err(fail(
                        at,
                        format!(
                            "alter_column `{table}.{column}`: `{}.from` differs from state",
                            stringify!($field)
                        ),
                    ));
                }
                col.$field = c.to.clone();
            }
        };
    }
    change!(ty);
    change!(nullable);
    change!(default);
    change!(max_length);
    change!(comment);
    change!(case_insensitive);
    change!(min);
    change!(max);
    change!(sealed);
    if ch.ty.is_some() || ch.max_length.is_some() {
        if let Some(f) = fks_using(s, table, column) {
            return Err(coded(
                Code::FkType,
                at,
                format!(
                    "alter_column `{table}.{column}` changes its type under FK `{}`; write \
                     drop_fk before and add_fk after",
                    f.name
                ),
            ));
        }
    }
    *get_mut(s, at, table)?
        .columns
        .iter_mut()
        .find(|c| c.column == column)
        .expect("checked above") = col;
    Ok(())
}

/// `col` appears as an identifier token in `expr`.
fn mentions(expr: &str, col: &str) -> bool {
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|tok| tok.eq_ignore_ascii_case(col))
}

/// The first thing in or pointing at `t` that uses `col`: checks, generated
/// columns and predicates (MySQL 3959), and with `!expr_only` indexes and FKs.
fn first_user(s: &State, t: &TableState, col: &str, expr_only: bool) -> Option<(Code, String)> {
    if let Some(k) = t.checks.iter().find(|k| mentions(&k.expr, col)) {
        return Some((Code::CheckColumn, format!("check `{}`", k.name)));
    }
    if let Some(c) = t
        .columns
        .iter()
        .find(|c| c.generated_as.as_deref().is_some_and(|g| mentions(g, col)))
    {
        return Some((
            Code::CheckColumn,
            format!("generated column `{}`", c.column),
        ));
    }
    if let Some(i) = t
        .indexes
        .iter()
        .find(|i| i.predicate.as_deref().is_some_and(|p| mentions(p, col)))
    {
        return Some((
            Code::CheckColumn,
            format!("the predicate of index `{}`", i.name),
        ));
    }
    if expr_only {
        return None;
    }
    if let Some(i) = t
        .indexes
        .iter()
        .find(|i| i.columns.iter().chain(&i.include).any(|c| c == col))
    {
        return Some((Code::State, format!("index `{}`", i.name)));
    }
    fks_using(s, &t.name, col).map(|f| (Code::State, format!("FK `{}`", f.name)))
}

/// An FK on `t` or into `t` that uses `col`.
fn fks_using<'s>(s: &'s State, t: &str, col: &str) -> Option<&'s FkDef> {
    s.tables.iter().find_map(|(name, ts)| {
        ts.fks.iter().find(|f| {
            (&**name == t && f.columns.iter().any(|c| c == col))
                || (f.to == t && f.on.iter().any(|c| c == col))
        })
    })
}

/// `ix` leads with exactly `cols` (an FK's covering index), unconditionally.
fn covers(ix: &IndexDef, cols: &[String]) -> bool {
    ix.predicate.is_none() && ix.columns.len() >= cols.len() && ix.columns[..cols.len()] == *cols
}

/// `cols` lead the primary key or an index of `t`, `except` the named one.
fn covered(t: &TableState, cols: &[String], except: Option<&str>) -> bool {
    let pk: Vec<&str> = t.primary_key().collect();
    let by_pk = pk.len() >= cols.len() && pk.iter().zip(cols).all(|(a, b)| *a == b);
    by_pk
        || t.indexes
            .iter()
            .any(|i| Some(i.name.as_str()) != except && covers(i, cols))
}

fn sorted_skips(ix: &IndexDef) -> IndexDef {
    let mut ix = ix.clone();
    ix.skipped_on.sort_unstable();
    ix
}

/// The FK target is in this target's state and a unique key, each column
/// pair has one type, and an index covers the FK columns.
fn add_fk(s: &mut State, at: &str, ctx: &Ctx<'_>, table: &str, fk: FkDef) -> Result<(), LoadError> {
    claim(s, at, &fk.name)?;
    let owned = s.tables.get(fk.to.as_str());
    let ext = s.externs.get(fk.to.as_str());
    if owned.is_none() && ext.is_none() {
        if ctx.target == Target::Tenant
            && ctx
                .registry
                .is_some_and(|r| r.tables.contains_key(fk.to.as_str()))
        {
            return Err(coded(
                Code::CrossTarget,
                at,
                format!(
                    "tenant table `{table}.{}` references registry table `{}`. Tenant→registry \
                     FKs are not allowed in v2: in DB mode the tables live in different \
                     databases, and schema mode must behave the same. Keep the column plain and \
                     check it in app code.",
                    fk.columns.join(", "),
                    fk.to
                ),
            ));
        }
        return Err(fail(
            at,
            format!(
                "FK `{}` points at `{}`, which is not in this target's state",
                fk.name, fk.to
            ),
        ));
    }
    if fk.columns.is_empty() || fk.columns.len() != fk.on.len() {
        return Err(fail(at, format!("FK `{}`: column counts differ", fk.name)));
    }
    let unique = match (owned, ext) {
        (Some(t), _) => {
            let mut pk: Vec<&str> = t.primary_key().collect();
            let mut want: Vec<&str> = fk.on.iter().map(String::as_str).collect();
            pk.sort_unstable();
            want.sort_unstable();
            pk == want
                || t.indexes.iter().any(|i| {
                    let mut c: Vec<&str> = i.columns.iter().map(String::as_str).collect();
                    c.sort_unstable();
                    i.unique && i.predicate.is_none() && c == want
                })
        }
        (None, Some(e)) => {
            (fk.on.len() == 1 && e.columns.iter().any(|c| c.name == fk.on[0] && c.unique))
                || e.unique_sets.iter().any(|u| {
                    let mut a = u.clone();
                    let mut b = fk.on.clone();
                    a.sort();
                    b.sort();
                    a == b
                })
        }
        (None, None) => unreachable!("checked above"),
    };
    if !unique {
        return Err(fail(
            at,
            format!(
                "FK `{}`: `{}`({}) is not a primary or unique key",
                fk.name,
                fk.to,
                fk.on.join(", ")
            ),
        ));
    }
    let t = get(s, at, table)?;
    for (c, r) in fk.columns.iter().zip(&fk.on) {
        let mine = t.column(c).map(|c| (c.ty.as_str(), c.max_length));
        let theirs = match (owned, ext) {
            (Some(o), _) => o.column(r).map(|c| (c.ty.as_str(), c.max_length)),
            (None, Some(e)) => e
                .columns
                .iter()
                .find(|x| &x.name == r)
                .map(|x| (x.ty.as_str(), mine.and_then(|m| m.1))),
            (None, None) => None,
        };
        match (mine, theirs) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => {
                return Err(coded(
                    Code::FkType,
                    at,
                    format!(
                        "FK `{}`: `{table}.{c}` and `{}.{r}` differ in type; one type on both ends",
                        fk.name, fk.to
                    ),
                ))
            }
            _ => return Err(fail(at, format!("FK `{}`: missing column", fk.name))),
        }
    }
    if !covered(t, &fk.columns, None) {
        return Err(coded(
            Code::FkIndex,
            at,
            format!(
                "FK `{}` needs an index on `{table}`({}) first",
                fk.name,
                fk.columns.join(", ")
            ),
        ));
    }
    get_mut(s, at, table)?.fks.push(fk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::decode;
    use super::super::state::{HashMemo, OnDelete};
    use super::*;
    use serde_json::{json, Value};

    const CTX: Ctx<'static> = Ctx {
        target: Target::Tenant,
        registry: None,
    };

    fn file(ops: Value) -> File {
        decode(
            &json!({"engine": 2, "format": 1, "id": "0001_a", "app": "project",
                    "target": "tenant", "operations": ops}),
            "f.json",
        )
        .unwrap()
    }

    fn run(s: &mut State, ops: Value) -> Result<(), LoadError> {
        apply_file(s, &file(ops), "f.json", &CTX)
    }

    fn id() -> Value {
        json!({"column": "id", "ty": "i64", "primary_key": true, "auto": true, "nullable": false})
    }

    fn customer() -> Value {
        json!({"op": "create_table", "v": 1,
               "table": {"name": "customer", "scope": "tenant", "fields": [id()]}})
    }

    /// Example B: an inline FK covered by the table's own index.
    fn invoice() -> Value {
        json!({"op": "create_table", "v": 1,
            "table": {"name": "invoice", "scope": "tenant", "fields": [id(),
                {"column": "customer_id", "ty": "i64", "nullable": false,
                 "fk": {"to": "customer", "on": "id", "on_delete": "cascade",
                        "name": "invoice_customer_id_fkey"}},
                {"column": "total", "ty": "decimal", "nullable": false, "default": "0"}]},
            "indexes": [{"name": "invoice_customer_idx", "columns": ["customer_id"]}],
            "checks": [{"name": "invoice_total_ge0", "expr": "total >= 0"}]})
    }

    fn base() -> State {
        let mut s = State::default();
        run(&mut s, json!([customer(), invoice()])).unwrap();
        s
    }

    fn err(s: &mut State, ops: Value) -> Code {
        run(s, ops).unwrap_err().code
    }

    #[test]
    fn create_table_names_pg_objects() {
        let s = base();
        let inv = &s.tables["invoice"];
        assert_eq!(inv.pg_pkey.as_deref(), Some("invoice_pkey"));
        assert_eq!(inv.pg_sequence.as_deref(), Some("invoice_id_seq"));
        assert_eq!(inv.fks[0].on_delete, OnDelete::Cascade);
    }

    /// After a rename the old pkey name stays taken, so the next table gets `_pkey1`.
    #[test]
    fn a_renamed_table_keeps_its_pkey_name() {
        let mut s = base();
        run(
            &mut s,
            json!([
                {"op": "rename_table", "v": 1, "from": "customer", "to": "client"},
                {"op": "create_table", "v": 1,
                 "table": {"name": "customer", "scope": "tenant", "fields": [id()]}}
            ]),
        )
        .unwrap();
        assert_eq!(s.tables["client"].pg_pkey.as_deref(), Some("customer_pkey"));
        assert_eq!(
            s.tables["client"].pg_sequence.as_deref(),
            Some("client_id_seq")
        );
        assert_eq!(
            s.tables["customer"].pg_pkey.as_deref(),
            Some("customer_pkey1")
        );
        assert_eq!(s.tables["invoice"].fks[0].to, "client");
    }

    #[test]
    fn fk_rules() {
        // Target missing.
        let mut s = State::default();
        assert_eq!(err(&mut s, json!([invoice()])), Code::State);
        // No covering index.
        let mut s = State::default();
        let mut inv = invoice();
        inv["indexes"] = json!([]);
        assert_eq!(err(&mut s, json!([customer(), inv])), Code::FkIndex);
        // The only covering index cannot be dropped.
        let mut s = base();
        let drop = json!([{"op": "drop_index", "v": 1, "table": "invoice",
            "name": "invoice_customer_idx",
            "was": {"name": "invoice_customer_idx", "columns": ["customer_id"]}}]);
        assert_eq!(err(&mut s, drop), Code::FkIndex);
        // Type change under an FK (3780), on either end.
        let mut s = base();
        let alter = json!([{"op": "alter_column", "v": 1, "table": "customer", "column": "id",
            "changes": {"ty": {"from": "i64", "to": "i32"}}}]);
        assert_eq!(err(&mut s, alter), Code::FkType);
        // An FK in add_column is refused.
        let mut s = base();
        let add = json!([{"op": "add_column", "v": 1, "table": "invoice",
            "column": {"column": "other_id", "ty": "i64", "nullable": true,
                       "fk": {"to": "customer", "on": "id", "name": "x_fk"}}}]);
        assert_eq!(err(&mut s, add), Code::FkIndex);
    }

    /// A registry table is never in tenant state: E030, with the reason.
    #[test]
    fn tenant_to_registry_fk_is_refused() {
        let mut reg = State::default();
        let org = json!([{"op": "create_table", "v": 1,
            "table": {"name": "org", "scope": "registry", "fields": [id()]}}]);
        let f = decode(
            &json!({"engine": 2, "format": 1, "id": "0001_a", "app": "project",
                    "target": "registry", "operations": org}),
            "r.json",
        )
        .unwrap();
        let rctx = Ctx {
            target: Target::Registry,
            registry: None,
        };
        apply_file(&mut reg, &f, "r.json", &rctx).unwrap();
        let mut s = State::default();
        let ops = json!([{"op": "create_table", "v": 1,
            "table": {"name": "invoice", "scope": "tenant", "fields": [id(),
                {"column": "org_id", "ty": "i64", "nullable": false,
                 "fk": {"to": "org", "on": "id", "name": "invoice_org_fk"}}]},
            "indexes": [{"name": "invoice_org_idx", "columns": ["org_id"]}]}]);
        let ctx = Ctx {
            target: Target::Tenant,
            registry: Some(&reg),
        };
        let e = apply_file(&mut s, &file(ops), "f.json", &ctx).unwrap_err();
        assert_eq!(e.code, Code::CrossTarget, "{e}");
        assert!(e.msg.contains("registry table `org`"), "{e}");
    }

    #[test]
    fn check_column_rule() {
        let mut s = base();
        let ren = json!([{"op": "rename_column", "v": 1, "table": "invoice",
            "from": "total", "to": "amount"}]);
        assert_eq!(err(&mut s, ren), Code::CheckColumn);
        let mut s = base();
        let drop = json!([{"op": "drop_column", "v": 1, "table": "invoice", "column": "total",
            "was": {"column": "total", "ty": "decimal", "nullable": false, "default": "0"}}]);
        assert_eq!(err(&mut s, drop), Code::CheckColumn);
        // drop_check first, then the rename passes.
        let mut s = base();
        run(
            &mut s,
            json!([
                {"op": "drop_check", "v": 1, "table": "invoice", "name": "invoice_total_ge0",
                 "was": "total >= 0"},
                {"op": "rename_column", "v": 1, "table": "invoice", "from": "total", "to": "amount"}
            ]),
        )
        .unwrap();
        assert!(s.tables["invoice"].column("amount").is_some());
    }

    #[test]
    fn was_and_from_must_match_state() {
        let mut s = base();
        let drop = json!([{"op": "drop_column", "v": 1, "table": "invoice", "column": "total",
            "was": {"column": "total", "ty": "decimal", "nullable": true}}]);
        assert_eq!(err(&mut s, drop), Code::State);
        let mut s = base();
        let alter = json!([{"op": "alter_column", "v": 1, "table": "invoice", "column": "total",
            "changes": {"nullable": {"from": true, "to": false}}}]);
        assert_eq!(err(&mut s, alter), Code::State);
        let mut s = base();
        let was = serde_json::to_value(s.tables["customer"].as_ref()).unwrap();
        let drop = json!([{"op": "drop_table", "v": 1, "table": "customer", "was": was}]);
        assert_eq!(
            err(&mut s, drop.clone()),
            Code::State,
            "invoice still points at it"
        );
    }

    #[test]
    fn not_null_on_an_existing_table_needs_fill() {
        let col = json!({"column": "locale", "ty": "string", "nullable": false});
        let mut s = base();
        let add = json!([{"op": "add_column", "v": 1, "table": "invoice", "column": col}]);
        assert_eq!(err(&mut s, add), Code::Fill);
        let mut s = base();
        run(
            &mut s,
            json!([{"op": "add_column", "v": 1, "table": "invoice", "column": col,
            "fill": "'en'"}]),
        )
        .unwrap();
        // A table created in the same file needs no fill.
        let mut s = State::default();
        run(
            &mut s,
            json!([customer(),
            {"op": "add_column", "v": 1, "table": "customer", "column": col}]),
        )
        .unwrap();
    }

    #[test]
    fn unsupported_alters_and_long_names() {
        let mut s = base();
        let alter = json!([{"op": "alter_column", "v": 1, "table": "invoice", "column": "total",
            "changes": {"primary_key": {"from": false, "to": true}}}]);
        assert_eq!(err(&mut s, alter), Code::Unsupported);
        let mut s = base();
        let add = json!([{"op": "add_check", "v": 1, "table": "invoice",
            "name": "c".repeat(64), "expr": "1 = 1"}]);
        assert_eq!(err(&mut s, add), Code::Identifier);
        let mut s = base();
        let add = json!([{"op": "add_check", "v": 1, "table": "invoice",
            "name": "invoice_customer_idx", "expr": "1 = 1"}]);
        assert_eq!(err(&mut s, add), Code::State, "name taken");
    }

    /// raw_sql applies its declared state; copy-on-write leaves other tables shared.
    #[test]
    fn raw_sql_state_and_copy_on_write() {
        let mut s = base();
        let before = s.clone();
        run(
            &mut s,
            json!([{"op": "raw_sql", "v": 1, "id": "trgm",
            "up": {"postgres": "CREATE INDEX ..."}, "state": [
                {"op": "add_index", "v": 1, "table": "customer", "name": "customer_x",
                 "columns": ["id"], "method": "gin"}]}]),
        )
        .unwrap();
        assert_eq!(s.tables["customer"].indexes.len(), 1);
        assert!(Arc::ptr_eq(&s.tables["invoice"], &before.tables["invoice"]));
        assert!(!Arc::ptr_eq(
            &s.tables["customer"],
            &before.tables["customer"]
        ));
        let mut memo = HashMemo::default();
        assert_ne!(s.hash(&mut memo), before.hash(&mut memo));
    }
}
