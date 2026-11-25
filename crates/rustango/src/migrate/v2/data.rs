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
    let tables = match op {
        Op::DataSql { tables, .. } | Op::DataRust { tables, .. } => tables,
        _ => return Ok(()),
    };
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
                resolve(
                    &text.0,
                    b,
                    state,
                    tables,
                    binds.len(),
                    &format!("{at}/{key}"),
                )?;
            }
        }
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
    let d = dialect(b);
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
                d.push_placeholder(&mut out, n);
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
                return Err(LoadError::new(
                    Code::NotEnabled,
                    at,
                    "`{{batch_range}}` belongs to batched data ops, which come in a later release",
                ));
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
    if n != binds {
        return Err(LoadError::new(
            Code::File,
            at,
            format!("{n} `{{{{?}}}}` on {} but {binds} bind(s)", b.as_str()),
        ));
    }
    // A table named bare, outside the footprint, found statically.
    if let Ok(toks) = super::norm::tokenize(d.kind(), &out) {
        for t in &toks {
            let super::norm::Tok::Ident(w) = &t.tok else {
                continue;
            };
            let bare = out[t.start..]
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
