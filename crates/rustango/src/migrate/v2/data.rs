//! Data steps (section 5): `data_sql` placeholders and footprint, typed
//! binds, and the step context with its poisoned-unit rule (5.4).

use super::format::{Bind, DataDown, Footprint, Op, PerBackend, RustDown};
use super::render::dialect;
use super::state::{Backend, State};
use super::{Code, LoadError};
use crate::core::SqlValue;

fn fp_err(at: impl Into<String>, msg: impl Into<String>) -> LoadError {
    LoadError::new(Code::Footprint, at, msg)
}

/// The binds as values, typed by their `ty`.
///
/// # Errors
/// An unknown type, or a value that does not fit it.
pub(crate) fn bind_values(binds: &[Bind]) -> Result<Vec<SqlValue>, String> {
    binds
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let ty = crate::migrate::snapshot::field_type_from_name(&b.ty)
                .ok_or_else(|| format!("bind {i}: unknown type `{}`", b.ty))?;
            crate::migrate::manage::fixtures::json_to_sql_value_of(&b.value, ty)
                .map_err(|e| format!("bind {i}: {e}"))
        })
        .collect()
}

/// Check a data op against the state it runs on: the footprint names
/// tables and columns that exist, every placeholder resolves on every
/// backend it has SQL for, and the binds fit.
///
/// # Errors
/// [`Code::Footprint`] for a name outside the footprint or state,
/// [`Code::File`] for a bad placeholder or bind.
pub(crate) fn check_op(state: &State, op: &Op, at: &str) -> Result<(), LoadError> {
    let (tables, batch) = match op {
        Op::DataSql { tables, batch, .. } | Op::DataRust { tables, batch, .. } => {
            (tables, batch.as_ref())
        }
        _ => return Ok(()),
    };
    if let Some(spec) = batch {
        check_batch(state, tables, spec, &format!("{at}/batch"))?;
    }
    for t in tables.tables() {
        if !state.tables.contains_key(t) && !state.externs.contains_key(t) {
            return Err(fp_err(
                format!("{at}/tables"),
                format!("`{t}` is not in state at this op"),
            ));
        }
    }
    for w in &tables.write {
        if let Some((t, c)) = w.split_once('.') {
            if state.tables.get(t).is_some_and(|ts| ts.column(c).is_none()) {
                return Err(fp_err(
                    format!("{at}/tables/write"),
                    format!("`{t}.{c}` is not a column in state at this op"),
                ));
            }
        }
    }
    let Op::DataSql {
        up,
        binds,
        down,
        down_binds,
        ..
    } = op
    else {
        return Ok(());
    };
    let key_col = batch.map(|b| b.key.0.as_str());
    for (sql, binds, key) in [(up, binds, "up")].into_iter().chain(match down {
        DataDown::Sql(d) => Some((d, down_binds, "down")),
        _ => None,
    }) {
        bind_values(binds).map_err(|e| {
            LoadError::new(
                Code::File,
                format!("{at}/{}", if key == "up" { "binds" } else { "down_binds" }),
                e,
            )
        })?;
        for b in Backend::ALL {
            if let Some(text) = sql.get(b) {
                // A batched op's down walks back by batch when it names the range.
                let range = if key == "up" || text.0.contains("{{batch_range}}") {
                    key_col
                } else {
                    None
                };
                resolve_with(
                    &text.0,
                    b,
                    state,
                    tables,
                    binds.len(),
                    range,
                    &format!("{at}/{key}"),
                )?;
            }
        }
    }
    Ok(())
}

/// The column types a batch can walk: integers, strings (compared by the
/// database) and uuids.
pub(crate) const BATCH_KEY_TYPES: [&str; 5] = ["i16", "i32", "i64", "string", "uuid"];

/// A batch walks one unique, ordered, NOT NULL column of a footprint table.
fn check_batch(
    state: &State,
    fp: &Footprint,
    spec: &super::format::BatchSpec,
    at: &str,
) -> Result<(), LoadError> {
    let file = |msg: String| LoadError::new(Code::File, at, msg);
    if !fp.tables().any(|t| t == spec.table) {
        return Err(fp_err(
            at,
            format!("batch table `{}` is not in `tables`", spec.table),
        ));
    }
    let t = state
        .tables
        .get(spec.table.as_str())
        .ok_or_else(|| fp_err(at, format!("batch table `{}` is not in state", spec.table)))?;
    let key = &spec.key.0;
    let c = t
        .column(key)
        .ok_or_else(|| file(format!("batch key `{}.{key}` is not a column", t.name)))?;
    if c.nullable {
        return Err(file(format!("batch key `{}.{key}` is nullable", t.name)));
    }
    if !BATCH_KEY_TYPES.contains(&c.ty.as_str()) {
        return Err(file(format!(
            "batch key `{}.{key}` is {}; a batch walks {}",
            t.name,
            c.ty,
            BATCH_KEY_TYPES.join(", ")
        )));
    }
    let pk: Vec<&str> = t.primary_key().collect();
    let unique = pk == [key.as_str()]
        || t.indexes
            .iter()
            .any(|i| i.unique && i.predicate.is_none() && i.columns == [key.clone()]);
    if !unique {
        return Err(file(format!(
            "batch key `{}.{key}` is not unique on its own",
            t.name
        )));
    }
    if spec.size == 0 {
        return Err(file("`size` is at least 1".into()));
    }
    Ok(())
}

/// The SQL `sql` names for `b` in `map`, or `None` when nothing is there:
/// no key for `b` and no `any`. An explicit `null` is `Some(None)`.
pub(crate) fn sql_for(map: &PerBackend, b: Backend) -> Option<Option<&str>> {
    let own = match b {
        Backend::Postgres => &map.postgres,
        Backend::Mysql => &map.mysql,
        Backend::Sqlite => &map.sqlite,
    };
    match own.as_ref().or(map.any.as_ref()) {
        None => None,
        Some(v) => Some(v.as_ref().map(|s| s.0.as_str())),
    }
}

/// `sql` for `b`: `{{t:name}}` and `{{c:name}}` (or `{{c:table.name}}`)
/// quoted, `{{?}}` as the dialect's placeholders in text order.
///
/// # Errors
/// An unknown placeholder, a name outside the footprint or state, a
/// write through a table the footprint only reads, a bare table name
/// outside the footprint, or a bind count that differs from `binds`.
pub(crate) fn resolve(
    sql: &str,
    b: Backend,
    state: &State,
    fp: &Footprint,
    binds: usize,
    at: &str,
) -> Result<String, LoadError> {
    resolve_with(sql, b, state, fp, binds, None, at)
}

/// The column a batched op ranges over, quoted for `b`.
pub(crate) type RangeKey<'a> = Option<&'a str>;

/// [`resolve`]; with `range` (a batched op's key) `{{?}}` and
/// `{{batch_range}}` stay as markers, numbered per batch by [`expand`].
///
/// # Errors
/// As [`resolve`]; `{{batch_range}}` missing from a batched op, or written
/// in an op that is not batched.
pub(crate) fn resolve_with(
    sql: &str,
    b: Backend,
    state: &State,
    fp: &Footprint,
    binds: usize,
    range: RangeKey<'_>,
    at: &str,
) -> Result<String, LoadError> {
    let d = dialect(b);
    let mut ranged = false;
    let in_fp = |t: &str| fp.tables().any(|x| x == t);
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    let mut n = 0;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after
            .find("}}")
            .ok_or_else(|| LoadError::new(Code::File, at, "`{{` with no `}}`"))?;
        let inner = after[..j].trim();
        match inner.split_once(':') {
            _ if inner == "?" => {
                n += 1;
                if range.is_some() {
                    out.push_str("{{?}}");
                } else {
                    d.push_placeholder(&mut out, n);
                }
            }
            Some(("t", name)) => {
                if !in_fp(name) {
                    return Err(fp_err(at, format!("`{name}` is not in `tables`")));
                }
                if !state.tables.contains_key(name) && !state.externs.contains_key(name) {
                    return Err(fp_err(at, format!("table `{name}` is not in state")));
                }
                let before = out.trim_end().to_ascii_uppercase();
                let writes = ["UPDATE", "INTO", "DELETE FROM"]
                    .iter()
                    .any(|k| before.ends_with(k));
                if writes && !fp.write_tables().any(|w| w == name) {
                    return Err(fp_err(
                        at,
                        format!("this statement writes `{name}`, which `tables.write` lacks"),
                    ));
                }
                out.push_str(&d.quote_ident(name));
            }
            Some(("c", name)) => {
                let (table, col) = match name.split_once('.') {
                    Some((t, c)) if in_fp(t) => (t.to_owned(), c),
                    Some((t, _)) => {
                        return Err(fp_err(at, format!("`{t}` is not in `tables`")));
                    }
                    None => {
                        let mut owners: Vec<&str> = fp
                            .tables()
                            .filter(|t| {
                                state
                                    .tables
                                    .get(*t)
                                    .is_some_and(|ts| ts.column(name).is_some())
                            })
                            .collect();
                        owners.sort_unstable();
                        owners.dedup();
                        match owners.as_slice() {
                            [one] => ((*one).to_owned(), name),
                            [] => {
                                return Err(fp_err(
                                    at,
                                    format!("no table in `tables` has a column `{name}`"),
                                ))
                            }
                            many => return Err(ambiguous(at, name, many)),
                        }
                    }
                };
                if state
                    .tables
                    .get(table.as_str())
                    .is_none_or(|ts| ts.column(col).is_none())
                {
                    return Err(fp_err(at, format!("`{table}.{col}` is not in state")));
                }
                out.push_str(&d.quote_ident(col));
            }
            _ if inner == "batch_range" => {
                if range.is_none() {
                    return Err(LoadError::new(
                        Code::File,
                        at,
                        "`{{batch_range}}` belongs to a batched op: give `batch`",
                    ));
                }
                ranged = true;
                out.push_str("{{batch_range}}");
            }
            _ => {
                return Err(LoadError::new(
                    Code::File,
                    at,
                    format!("unknown placeholder `{{{{{inner}}}}}`"),
                ))
            }
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    if range.is_some() && !ranged {
        return Err(LoadError::new(
            Code::File,
            at,
            "a batched data_sql needs `{{batch_range}}` where its rows are chosen",
        ));
    }
    if n != binds {
        return Err(LoadError::new(
            Code::File,
            at,
            format!("{n} `{{{{?}}}}` on {} but {binds} bind(s)", b.as_str()),
        ));
    }
    // A table named bare, outside the footprint, found statically.
    let scan = out
        .replace("{{?}}", "?")
        .replace("{{batch_range}}", "1 = 1");
    if let Ok(toks) = super::norm::tokenize(d.kind(), &scan) {
        for t in &toks {
            let super::norm::Tok::Ident(w) = &t.tok else {
                continue;
            };
            let bare = scan[t.start..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if bare && state.tables.contains_key(w.as_str()) && !in_fp(w) {
                return Err(fp_err(
                    at,
                    format!("the SQL names table `{w}`, which `tables` does not list"),
                ));
            }
        }
    }
    Ok(out)
}

/// Data steps of `file` with nothing to run on `b`: forward, `up` with no
/// SQL there; in reverse, an irreversible `down` or one with no SQL there.
pub(crate) fn misses(file: &super::format::File, b: Backend, reverse_plan: bool) -> Vec<String> {
    let mut out = Vec::new();
    for (i, env) in file.operations.iter().enumerate() {
        if let Op::DataRust {
            id, down, reverse, ..
        } = &env.op
        {
            let at = format!("{}:{} op {i} (`{id}`)", file.app, file.id);
            let found = super::step::registered(&file.app, id);
            let miss = if reverse_plan && !*reverse {
                match down {
                    RustDown::Noop(_) => None,
                    RustDown::Irreversible(why) => Some(format!("{at}: irreversible: {why}")),
                    RustDown::Rust if found.iter().any(|s| s.down.is_some()) => None,
                    RustDown::Rust => Some(not_registered(&at, &file.app, id, true)),
                }
            } else if found.is_empty() {
                Some(not_registered(&at, &file.app, id, false))
            } else {
                None
            };
            out.extend(miss);
            continue;
        }
        let Op::DataSql { id, up, down, .. } = &env.op else {
            continue;
        };
        let at = format!("{}:{} op {i} (`{id}`)", file.app, file.id);
        let miss = if reverse_plan {
            match down {
                DataDown::Noop(_) => None,
                DataDown::Irreversible(why) => Some(format!("{at}: irreversible: {why}")),
                DataDown::Sql(m) => sql_for(m, b)
                    .is_none()
                    .then(|| format!("{at}: `down` has no SQL for {}", b.as_str())),
            }
        } else {
            sql_for(up, b)
                .is_none()
                .then(|| format!("{at}: `up` has no SQL for {}", b.as_str()))
        };
        out.extend(miss);
    }
    out
}

/// A batched SQL step's markers for one batch: `{{?}}` takes the next bind,
/// `{{batch_range}}` the range `key > lo AND key <= hi` (a side left out
/// when the batch has no bound there), all numbered in text order.
pub(crate) fn expand(
    sql: &str,
    d: &dyn crate::sql::Dialect,
    binds: Vec<SqlValue>,
    key: &str,
    lo: Option<&SqlValue>,
    hi: Option<&SqlValue>,
) -> (String, Vec<SqlValue>) {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut params = Vec::with_capacity(binds.len() + 2);
    let mut binds = binds.into_iter();
    let mut rest = sql;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let j = after.find("}}").unwrap_or(after.len());
        match &after[..j] {
            "?" => {
                params.push(binds.next().unwrap_or(SqlValue::Null));
                d.push_placeholder(&mut out, params.len());
            }
            "batch_range" => {
                let k = d.quote_ident(key);
                let mut sides = Vec::new();
                for (v, op) in [(lo, ">"), (hi, "<=")] {
                    if let Some(v) = v {
                        params.push(v.clone());
                        let mut s = format!("{k} {op} ");
                        d.push_placeholder(&mut s, params.len());
                        sides.push(s);
                    }
                }
                if sides.is_empty() {
                    out.push_str("(1 = 1)");
                } else {
                    out.push('(');
                    out.push_str(&sides.join(" AND "));
                    out.push(')');
                }
            }
            other => {
                out.push_str("{{");
                out.push_str(other);
                out.push_str("}}");
            }
        }
        rest = after.get(j + 2..).unwrap_or("");
    }
    out.push_str(rest);
    (out, params)
}

/// "not registered", with near matches and the binary that looked.
fn not_registered(at: &str, app: &str, id: &str, down: bool) -> String {
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "this binary".into());
    let near = super::step::near(app, id);
    let what = if down { " with a `down`" } else { "" };
    let hint = if near.is_empty() {
        String::new()
    } else {
        format!(" (near: {})", near.join(", "))
    };
    format!("{at}: no `#[data_step(app = \"{app}\", id = \"{id}\")]`{what} is registered in `{bin}`{hint}")
}

/// One error listing every miss, or `Ok` when there is none.
///
/// # Errors
/// [`Code::Unresolved`] naming each miss.
pub(crate) fn refuse_misses(misses: &[String], what: &str) -> Result<(), LoadError> {
    if misses.is_empty() {
        return Ok(());
    }
    Err(LoadError::new(
        Code::Unresolved,
        what,
        format!(
            "{} data step(s) have nothing to run here, so nothing ran: {}",
            misses.len(),
            misses.join("; ")
        ),
    ))
}

fn ambiguous(at: &str, name: &str, owners: &[&str]) -> LoadError {
    let hint = format!("{{{{c:<table>.{name}}}}}");
    fp_err(
        at,
        format!(
            "`{name}` is a column of {}: write `{hint}`",
            owners.join(" and ")
        ),
    )
}

#[cfg(test)]
#[path = "data_tests.rs"]
mod tests;
