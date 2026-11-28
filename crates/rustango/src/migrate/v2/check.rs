//! `dbmigrate check` (11.2): load, replay and render for every configured
//! backend, the defaults rules of 2.6, and models vs the chain.

use std::path::Path;

use super::format::Op;
use super::replay::Ctx;
use super::state::{Backend, CheckBody, ColumnDef, State};
use super::LoadError;
use crate::migrate::snapshot::SchemaSnapshot;
use crate::sql::Pool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,
    Warning,
}

/// One finding: errors fail `check`, warnings print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) severity: Severity,
    pub(crate) at: String,
    pub(crate) msg: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{tag}: {}: {}", self.at, self.msg)
    }
}

const DOCS: &str = "see the defaults table in the migrations docs";

/// Check the v2 project around `dir` for `backends`. `models` adds the
/// "models match the chain" check; `pool` adds the defaults probe on the
/// connected backend.
///
/// # Errors
/// The first load, replay or render error.
#[allow(clippy::too_many_lines)]
pub(crate) async fn check(
    dir: &Path,
    backends: &[Backend],
    models: Option<&SchemaSnapshot>,
    pool: Option<&Pool>,
) -> Result<Vec<Finding>, LoadError> {
    let project = super::load::load_project(dir)?;
    super::load::replay(&project)?;
    for &b in backends {
        super::render::render_project(&project, b)?;
    }
    let mut out = Vec::new();
    // Every default and fill the chain writes, with the column it is for,
    // and every hand-written check expression.
    let mut exprs: Vec<(String, ColumnDef, String)> = Vec::new();
    let mut checks: Vec<(String, String)> = Vec::new();
    // Each target in canonical order, so an op sees the state it runs on.
    let all = &project.files;
    let files = project
        .order
        .iter()
        .flat_map(|(target, idx)| idx.iter().map(move |&i| (*target, &all[i])));
    let mut states: std::collections::BTreeMap<super::state::Target, State> =
        std::collections::BTreeMap::new();
    for (target, f) in files {
        let state = states.entry(target).or_default();
        let ctx = Ctx {
            target,
            registry: None,
        };
        // A checkpoint brings its state in whole; its kept ops are data ops.
        if f.file.kind == super::format::Kind::Checkpoint {
            super::replay::apply_file(state, &f.file, &f.at, &ctx)?;
        }
        let mut created: Vec<String> = Vec::new();
        for (i, env) in f.file.operations.iter().enumerate() {
            if f.file.kind == super::format::Kind::Checkpoint {
                break;
            }
            let op_at = format!("{}#/operations/{i}", f.at);
            let mut after = state.clone();
            let here: Vec<&str> = created.iter().map(String::as_str).collect();
            super::replay::apply(&mut after, &env.op, &op_at, &ctx, &here)?;
            if let Op::CreateTable { table, .. } = &env.op {
                created.push(table.name.clone());
            }
            let mut ops = vec![env.op.clone()];
            ops.extend(env.op.nested(&f.at)?);
            for op in &ops {
                let mut col = |table: &str, c: &ColumnDef, text: &Option<String>| {
                    if let Some(t) = text {
                        let at = format!("{}: {table}.{}", f.at, c.column);
                        exprs.push((at, c.clone(), t.clone()));
                    }
                };
                match op {
                    Op::CreateTable {
                        table, checks: ks, ..
                    }
                    | Op::AdoptTable {
                        table, checks: ks, ..
                    } => {
                        for fd in &table.fields {
                            col(&table.name, &fd.column, &fd.column.default);
                        }
                        for k in ks {
                            if let CheckBody::Sql(e) = &k.body {
                                checks.push((format!("{}: check `{}`", f.at, k.name), e.clone()));
                            }
                        }
                    }
                    Op::AddCheck { check: k, .. } => {
                        if let CheckBody::Sql(e) = &k.body {
                            checks.push((format!("{}: check `{}`", f.at, k.name), e.clone()));
                        }
                    }
                    Op::AddColumn {
                        table,
                        column,
                        fill,
                    } => {
                        col(table, &column.column, &column.column.default);
                        col(table, &column.column, fill);
                    }
                    // A default-only change checks against the column's real type.
                    Op::AlterColumn {
                        table,
                        column,
                        changes,
                        ..
                    } => {
                        if let (Some(ch), Some(c)) = (
                            &changes.default,
                            after
                                .tables
                                .get(table.as_str())
                                .and_then(|t| t.column(column)),
                        ) {
                            col(table, c, &ch.to);
                        }
                    }
                    _ => {}
                }
            }
            *state = after;
        }
        if let Some(doc) = &f.file.state {
            for t in doc.tables.values() {
                for k in &t.checks {
                    if let CheckBody::Sql(e) = &k.body {
                        checks.push((format!("{}: check `{}`", f.at, k.name), e.clone()));
                    }
                }
                for c in &t.columns {
                    if let Some(d) = &c.default {
                        exprs.push((
                            format!("{}: {}.{}", f.at, t.name, c.column),
                            c.clone(),
                            d.clone(),
                        ));
                    }
                }
            }
        }
    }
    for (at, sql) in &checks {
        out.extend(reserved_names(sql, backends, at));
    }
    for (at, c, text) in &exprs {
        for &b in backends {
            if let Some(f) = classify(b, backends, c, text, at) {
                out.push(f);
            }
        }
        #[cfg(feature = "sqlite")]
        if backends.contains(&Backend::Sqlite) {
            if let Err(e) = sqlite_memory(c, text).await {
                out.push(Finding {
                    severity: Severity::Error,
                    at: at.clone(),
                    msg: format!("`{text}` does not run as a default on sqlite: {e} ({DOCS})"),
                });
            }
        }
        if let Some(pool) = pool {
            let b = super::run::backend_of(pool);
            if let Err(e) = super::catalog::default_runs(pool, c, text).await {
                out.push(Finding {
                    severity: Severity::Error,
                    at: at.clone(),
                    msg: format!(
                        "`{text}` does not run as a default on {}: {e} ({DOCS})",
                        b.as_str()
                    ),
                });
            }
        }
    }
    out.extend(step_findings(&project, dir, pool).await?);
    out.extend(batch_findings(&project, pool).await?);
    out.extend(contract_findings(&project, pool).await?);
    if let Some(models) = models {
        let opts = super::make::MakeOptions {
            backends: backends.to_vec(),
            dry_run: true,
            ..super::make::MakeOptions::default()
        };
        if let Some(made) = super::make::make(dir, dir, models, &opts)? {
            out.push(Finding {
                severity: Severity::Error,
                at: dir.display().to_string(),
                msg: format!(
                    "the models have changes no migration has ({}): run `manage dbmigrate make`",
                    made.lines.join("; ")
                ),
            });
        }
    }
    out.dedup();
    Ok(out)
}

/// Data step registrations: a duplicate key and a `data_rust` op with no
/// step are errors; a step no op names is a warning, as is (online) a
/// step whose code changed after it ran.
async fn step_findings(
    project: &super::load::Project,
    dir: &Path,
    pool: Option<&Pool>,
) -> Result<Vec<Finding>, LoadError> {
    let at = dir.display().to_string();
    let mut out = Vec::new();
    if let Err(e) = super::step::refuse_duplicates() {
        out.push(Finding {
            severity: Severity::Error,
            at: at.clone(),
            msg: e,
        });
    }
    let b = pool.map_or(Backend::Postgres, super::run::backend_of);
    let mut named = std::collections::BTreeSet::new();
    let mut apps = std::collections::BTreeSet::new();
    for f in &project.files {
        apps.insert(f.file.app.clone());
        for env in &f.file.operations {
            if let Op::DataRust { id, .. } = &env.op {
                named.insert((f.file.app.clone(), id.clone()));
            }
        }
        for miss in super::data::misses(&f.file, b, false)
            .into_iter()
            .filter(|m| m.contains("#[data_step"))
        {
            out.push(Finding {
                severity: Severity::Error,
                at: f.at.clone(),
                msg: miss,
            });
        }
    }
    for s in inventory::iter::<super::step::DataStep> {
        if apps.contains(s.app) && !named.contains(&(s.app.to_owned(), s.id.to_owned())) {
            out.push(Finding {
                severity: Severity::Warning,
                at: s.type_name.to_owned(),
                msg: format!(
                    "data step `{}:{}` is registered but no data_rust op names it",
                    s.app, s.id
                ),
            });
        }
    }
    if let Some(pool) = pool {
        let recorded = super::ledger::Ledger::default_names()
            .step_hashes(pool)
            .await
            .map_err(|e| LoadError::new(super::Code::Io, &at, e.to_string()))?;
        for ((app, id), was) in recorded {
            if let Some(s) = super::step::registered(&app, &id).first() {
                if s.code_hash() != was {
                    out.push(Finding {
                        severity: Severity::Warning,
                        at: s.type_name.to_owned(),
                        msg: format!("data step `{app}:{id}` changed after it ran"),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Expand/contract (6.8): a batched data op and a contract op in one file
/// is an error unless `offline`; online, a pending contract file whose
/// expand ancestor is pending too is `MV2-W012` (old replicas may still
/// read the shape it drops).
async fn contract_findings(
    project: &super::load::Project,
    pool: Option<&Pool>,
) -> Result<Vec<Finding>, LoadError> {
    use super::format::Phase;
    let mut out = Vec::new();
    for f in &project.files {
        let batched = f.file.operations.iter().any(|e| {
            matches!(
                &e.op,
                Op::DataSql { batch: Some(_), .. } | Op::DataRust { batch: Some(_), .. }
            )
        });
        let contract = f.file.phase == Phase::Contract
            || f.file
                .operations
                .iter()
                .any(|e| super::make::is_contract_op(&e.op));
        if batched && contract && !f.file.offline {
            out.push(Finding {
                severity: Severity::Error,
                at: f.at.clone(),
                msg: "a batched data op and a contract op in one file: split them, or mark the \
                      file `\"offline\": true`"
                    .into(),
            });
        }
    }
    let Some(pool) = pool else {
        return Ok(out);
    };
    let done: std::collections::HashSet<(String, String)> = super::ledger::Ledger::default_names()
        .rows(pool)
        .await
        .map_err(|e| LoadError::new(super::Code::Io, "ledger", e.to_string()))?
        .into_iter()
        .filter(|r| r.state == super::ledger::RowState::Applied)
        .map(|r| (r.app, r.id))
        .collect();
    let pending =
        |l: &super::load::Loaded| !done.contains(&(l.file.app.clone(), l.file.id.clone()));
    let by_id: std::collections::HashMap<(&str, &str), &super::load::Loaded> = project
        .files
        .iter()
        .map(|l| ((l.file.app.as_str(), l.file.id.as_str()), l))
        .collect();
    for c in project
        .files
        .iter()
        .filter(|l| l.file.phase == Phase::Contract && pending(l))
    {
        // The nearest ancestors: parents and depends_on.
        let up = c
            .file
            .parents
            .iter()
            .map(|p| (c.file.app.as_str(), p.as_str()))
            .chain(
                c.file
                    .depends_on
                    .iter()
                    .map(|(a, d)| (a.as_str(), d.as_str())),
            );
        for k in up {
            if let Some(e) = by_id.get(&k) {
                if e.file.phase != Phase::Contract && pending(e) {
                    out.push(Finding {
                        severity: Severity::Warning,
                        at: c.at.clone(),
                        msg: format!(
                            "contract file `{}` and its expand file `{}` are both pending: \
                             ship the expand one in an earlier release, old replicas may \
                             still use the shape this drops [MV2-W012]",
                            c.file.id, e.file.id
                        ),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Every batched op: `MV2-W010` (it runs inline; the app waits), unless
/// allowed; online, `MV2-W011` above the long-move row threshold.
async fn batch_findings(
    project: &super::load::Project,
    pool: Option<&Pool>,
) -> Result<Vec<Finding>, LoadError> {
    use super::run::LONG_MOVE_ROWS;
    let mut out = Vec::new();
    for f in &project.files {
        for (i, env) in f.file.operations.iter().enumerate() {
            let (Op::DataSql { batch: Some(b), .. } | Op::DataRust { batch: Some(b), .. }) =
                &env.op
            else {
                continue;
            };
            let at = format!("{}#/operations/{i}", f.at);
            if !env.allow.iter().any(|a| a.code == "MV2-W010") {
                out.push(Finding {
                    severity: Severity::Warning,
                    at: at.clone(),
                    msg: format!(
                        "a batched move over `{}` runs inline: the app waits until it finishes \
                         [MV2-W010]",
                        b.table
                    ),
                });
            }
            if let Some(pool) = pool {
                let est = super::catalog::estimate_rows(pool, &b.table)
                    .await
                    .map_err(|e| LoadError::new(super::Code::Io, &at, e.to_string()))?;
                if let Some(n) = est.filter(|n| u64::try_from(*n).unwrap_or(0) > LONG_MOVE_ROWS) {
                    out.push(Finding {
                        severity: Severity::Warning,
                        at,
                        msg: format!(
                            "`{}` holds ~{n} rows, above {LONG_MOVE_ROWS}: a long move [MV2-W011]",
                            b.table
                        ),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// The real SQLite check: the default in an in-memory database.
#[cfg(feature = "sqlite")]
async fn sqlite_memory(c: &ColumnDef, text: &str) -> Result<(), String> {
    let pool = Pool::connect("sqlite::memory:")
        .await
        .map_err(|e| e.to_string())?;
    let r = super::catalog::default_runs(&pool, c, text).await;
    pool.close().await;
    r
}

/// Words a check may write bare though some backend reserves them:
/// operators, literals and the functions SQL calls without parens.
const EXPR_WORDS: &[&str] = &[
    "all",
    "and",
    "any",
    "array",
    "as",
    "between",
    "binary",
    "case",
    "cast",
    "collate",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "distinct",
    "div",
    "else",
    "end",
    "escape",
    "exists",
    "false",
    "ilike",
    "in",
    "interval",
    "is",
    "isnull",
    "like",
    "localtime",
    "localtimestamp",
    "mod",
    "not",
    "notnull",
    "null",
    "or",
    "regexp",
    "rlike",
    "session_user",
    "similar",
    "some",
    "system_user",
    "then",
    "true",
    "user",
    "when",
    "xor",
];

/// A bare identifier in a hand-written check that a configured backend
/// reserves: it breaks there unless quoted.
fn reserved_names(sql: &str, backends: &[Backend], at: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for &b in backends {
        let kind = super::render::dialect(b).kind();
        let Ok(toks) = super::norm::tokenize(kind, sql) else {
            continue;
        };
        for (i, t) in toks.iter().enumerate() {
            let super::norm::Tok::Ident(w) = &t.tok else {
                continue;
            };
            let bare = sql[t.start..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            let call = matches!(
                toks.get(i + 1).map(|n| &n.tok),
                Some(super::norm::Tok::LParen)
            );
            let cast = i > 0 && matches!(&toks[i - 1].tok, super::norm::Tok::Op(o) if o == "::");
            if !bare || call || cast || EXPR_WORDS.contains(&w.as_str()) || !kind.is_reserved(w) {
                continue;
            }
            out.push(Finding {
                severity: Severity::Error,
                at: at.to_owned(),
                msg: format!(
                    "`{w}` is reserved on {}: quote it, or name the column otherwise",
                    b.as_str()
                ),
            });
        }
    }
    out
}

/// A string literal: `'…'` with `''` escapes, nothing after it.
fn string_literal(t: &str) -> Option<&str> {
    let inner = t.strip_prefix('\'')?.strip_suffix('\'')?;
    let mut rest = inner;
    while let Some(i) = rest.find('\'') {
        if rest[i + 1..].starts_with('\'') {
            rest = &rest[i + 2..];
        } else {
            return None;
        }
    }
    Some(inner)
}

fn number(t: &str) -> bool {
    let t = t.strip_prefix('-').unwrap_or(t);
    let (a, b) = t.split_once('.').unwrap_or((t, "0"));
    !a.is_empty() && a.bytes().all(|c| c.is_ascii_digit()) && b.bytes().all(|c| c.is_ascii_digit())
}

/// Outer parens off, repeatedly.
fn bare(t: &str) -> &str {
    let mut t = t.trim();
    while let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        t = inner.trim();
    }
    t
}

/// The expressions of the 2.6 table, per backend.
fn known(b: Backend, t: &str) -> bool {
    let n: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = n.as_str();
    match b {
        Backend::Postgres => {
            matches!(
                n,
                "now()" | "gen_random_uuid()" | "'{}'::jsonb" | "'[]'::jsonb"
            )
        }
        Backend::Mysql => matches!(n, "(UTC_TIMESTAMP(6))" | "(UUID())" | "('{}')" | "('[]')"),
        Backend::Sqlite => {
            n == format!("(strftime('{}','now'))", crate::sql::SQLITE_DATETIME_FORMAT)
                || n == crate::sql::SQLITE_UUID_V4
        }
    }
}

/// The offline rules for one default on one backend.
fn classify(b: Backend, all: &[Backend], c: &ColumnDef, text: &str, at: &str) -> Option<Finding> {
    let error = |msg: String| {
        Some(Finding {
            severity: Severity::Error,
            at: at.to_owned(),
            msg: format!("{msg} ({DOCS})"),
        })
    };
    let raw = text.trim();
    let lit = bare(raw);
    let string = string_literal(lit);
    if b == Backend::Sqlite && raw.to_ascii_uppercase().contains("CURRENT_TIMESTAMP") {
        return error(format!(
            "policy: `{text}` on sqlite does not sort against the values rustango writes; write \
             the strftime expression"
        ));
    }
    if b == Backend::Sqlite && raw.contains("::") {
        return error(format!(
            "`{text}` does not run on sqlite: it has no `::` casts"
        ));
    }
    if b == Backend::Mysql && all.len() > 1 && string.is_some_and(|s| s.contains('\\')) {
        return error(format!(
            "`{text}`: a backslash in a string reads as an escape on mysql only; the same text \
             stores a different value there"
        ));
    }
    let unbounded =
        matches!(c.ty.as_str(), "json" | "binary") || (c.ty == "string" && c.max_length.is_none());
    if b == Backend::Mysql && unbounded && string.is_some() && !raw.starts_with('(') {
        return error(format!(
            "`{text}` does not run on mysql: a TEXT, JSON or BLOB column takes only the \
             parenthesised form `({raw})`"
        ));
    }
    let literal = string.is_some()
        || number(lit)
        || matches!(lit.to_ascii_uppercase().as_str(), "TRUE" | "FALSE" | "NULL");
    if literal || known(b, raw) {
        return None;
    }
    // SQLite gets the real check in memory.
    if b == Backend::Sqlite && cfg!(feature = "sqlite") {
        return None;
    }
    Some(Finding {
        severity: Severity::Warning,
        at: at.to_owned(),
        msg: format!(
            "`{text}` is not verified offline on {} [MV2-W020]; `check` on that backend probes it",
            b.as_str()
        ),
    })
}

#[cfg(test)]
#[path = "check_tests.rs"]
pub(super) mod tests;
