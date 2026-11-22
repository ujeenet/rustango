//! SPIKE S6: probe specs and probes (3.7).
//!
//! A probe answers "is this op's effect in the catalog?" against the
//! op's after-state: `Present` = all compared properties match the
//! after-state, `Absent` = the catalog still matches the before-state,
//! `Partial` = neither (with the mismatches against the after-state).
//! S5 resumes from these: Present → skip, Absent → run, Partial → refuse
//! or repair (an invalid PG index is dropped and rebuilt).

use super::catalog::{read_table, CatalogError, LiveColumn, LiveIndex, LiveTable};
use super::norm::{self, DefaultCtx};
use super::ops::{CheckDecl, ColumnDecl, FkDecl, IndexDecl, SpikeOp, TableDecl};
use crate::core::SqlValue;
use crate::sql::{DialectKind, Pool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Prop {
    TableExists,
    TableComment,
    ColumnSet,
    ColumnExists,
    ColType,
    ColNullable,
    ColDefault,
    ColGenerated,
    ColComment,
    IndexExists,
    IndexColumns,
    IndexUnique,
    IndexMethod,
    IndexPredicate,
    IndexInclude,
    IndexValid,
    CheckExists,
    CheckExpr,
    CheckValidated,
    FkExists,
    FkColumns,
    FkTarget,
    FkOnDelete,
    FkValidated,
}

/// The properties a probe compares on a dialect, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeSpec {
    Props(Vec<Prop>),
    None(&'static str),
}

/// Properties a dialect's catalog cannot report (or that cannot differ there).
#[must_use]
pub fn not_expressible(d: DialectKind) -> &'static [(Prop, &'static str)] {
    match d {
        DialectKind::Postgres => &[],
        DialectKind::Mysql => &[
            (Prop::IndexMethod, "InnoDB reports BTREE for USING HASH"),
            (
                Prop::IndexPredicate,
                "no partial indexes (load error unless skipped_on)",
            ),
            (
                Prop::IndexInclude,
                "no INCLUDE (load error unless skipped_on)",
            ),
            (Prop::IndexValid, "index DDL is atomic; no invalid state"),
            (
                Prop::CheckValidated,
                "no NOT VALID; v2 never writes NOT ENFORCED",
            ),
            (Prop::FkValidated, "no NOT VALID"),
        ],
        _ => &[
            (Prop::TableComment, "no comments"),
            (Prop::ColComment, "no comments"),
            (Prop::IndexMethod, "btree only"),
            (Prop::IndexInclude, "no INCLUDE"),
            (Prop::IndexValid, "no invalid state"),
            (Prop::CheckValidated, "no NOT VALID"),
            (Prop::FkValidated, "no NOT VALID"),
        ],
    }
}

const COLUMN: &[Prop] = &[
    Prop::ColumnExists,
    Prop::ColType,
    Prop::ColNullable,
    Prop::ColDefault,
    Prop::ColGenerated,
    Prop::ColComment,
];
const TABLE: &[Prop] = &[
    Prop::TableExists,
    Prop::TableComment,
    Prop::ColumnSet,
    Prop::ColType,
    Prop::ColNullable,
    Prop::ColDefault,
    Prop::ColGenerated,
    Prop::ColComment,
    Prop::CheckExpr,
    Prop::CheckValidated,
    Prop::FkColumns,
    Prop::FkTarget,
    Prop::FkOnDelete,
    Prop::FkValidated,
];
const INDEX: &[Prop] = &[
    Prop::IndexExists,
    Prop::IndexColumns,
    Prop::IndexUnique,
    Prop::IndexMethod,
    Prop::IndexPredicate,
    Prop::IndexInclude,
    Prop::IndexValid,
];
const CHECK: &[Prop] = &[Prop::CheckExists, Prop::CheckExpr, Prop::CheckValidated];
const FK: &[Prop] = &[
    Prop::FkExists,
    Prop::FkColumns,
    Prop::FkTarget,
    Prop::FkOnDelete,
    Prop::FkValidated,
];

/// The probe spec of `op` on `d`.
#[must_use]
pub fn spec(op: &SpikeOp, d: DialectKind) -> ProbeSpec {
    let base: &[Prop] = match op {
        SpikeOp::CreateTable(_) | SpikeOp::DropTable { .. } => TABLE,
        SpikeOp::AddColumn { .. } | SpikeOp::DropColumn { .. } | SpikeOp::AlterColumn { .. } => {
            COLUMN
        }
        SpikeOp::AddIndex(_) | SpikeOp::DropIndex { .. } => INDEX,
        SpikeOp::AddCheck { .. } | SpikeOp::DropCheck { .. } => CHECK,
        SpikeOp::AddFk { .. } | SpikeOp::DropFk { .. } => FK,
    };
    let skip = not_expressible(d);
    ProbeSpec::Props(
        base.iter()
            .copied()
            .filter(|p| !skip.iter().any(|(s, _)| s == p))
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub prop: Prop,
    pub object: String,
    pub want: String,
    pub got: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Present,
    Absent,
    Partial(Vec<Mismatch>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub verdict: Verdict,
    pub compared: Vec<Prop>,
}

/// Probe `op` on `pool`.
///
/// # Errors
/// Catalog read failures.
pub async fn probe(pool: &Pool, op: &SpikeOp) -> Result<ProbeResult, CatalogError> {
    let d = pool.dialect().kind();
    let ProbeSpec::Props(compared) = spec(op, d) else {
        return Err(CatalogError("probe: none".into()));
    };
    let c = Cmp {
        pool,
        d,
        props: &compared,
    };
    let verdict = match op {
        SpikeOp::CreateTable(t) => match read_table(pool, &t.name).await? {
            None => Verdict::Absent,
            Some(live) => to_verdict(c.table(t, &live).await?),
        },
        SpikeOp::DropTable { was } => match read_table(pool, &was.name).await? {
            None => Verdict::Present,
            Some(live) => before(c.table(was, &live).await?),
        },
        SpikeOp::AddColumn { table, column } => match col_of(pool, table, &column.name).await? {
            None => Verdict::Absent,
            Some((live, lc)) => to_verdict(c.column(&live, column, lc).await?),
        },
        SpikeOp::DropColumn { table, was } => match col_of(pool, table, &was.name).await? {
            None => Verdict::Present,
            Some((live, lc)) => before(c.column(&live, was, lc).await?),
        },
        SpikeOp::AlterColumn { table, from, to } => match col_of(pool, table, &to.name).await? {
            None => Verdict::Partial(vec![missing(Prop::ColumnExists, &to.name)]),
            Some((live, lc)) => {
                let after = c.column(&live, to, lc).await?;
                if after.is_empty() {
                    Verdict::Present
                } else if c.column(&live, from, lc).await?.is_empty() {
                    Verdict::Absent
                } else {
                    Verdict::Partial(after)
                }
            }
        },
        SpikeOp::AddIndex(ix) => match index_of(pool, &ix.table, &ix.name).await? {
            None => Verdict::Absent,
            Some((live, li)) => to_verdict(c.index(&live, ix, &li).await?),
        },
        SpikeOp::DropIndex { was } => match index_of(pool, &was.table, &was.name).await? {
            None => Verdict::Present,
            Some((live, li)) => before(c.index(&live, was, &li).await?),
        },
        SpikeOp::AddCheck { table, check } => match read_table(pool, table).await? {
            Some(live)
                if live
                    .checks
                    .iter()
                    .any(|k| k.name.as_deref() == Some(&check.name)) =>
            {
                to_verdict(c.check(&live, check).await?)
            }
            _ => Verdict::Absent,
        },
        SpikeOp::DropCheck { table, was } => match read_table(pool, table).await? {
            Some(live)
                if live
                    .checks
                    .iter()
                    .any(|k| k.name.as_deref() == Some(&was.name)) =>
            {
                before(c.check(&live, was).await?)
            }
            _ => Verdict::Present,
        },
        SpikeOp::AddFk { table, fk } => match read_table(pool, table).await? {
            Some(live) if find_fk(&live, fk).is_some() => to_verdict(c.fk(&live, fk)),
            _ => Verdict::Absent,
        },
        SpikeOp::DropFk { table, was } => match read_table(pool, table).await? {
            Some(live) if find_fk(&live, was).is_some() => before(c.fk(&live, was)),
            _ => Verdict::Present,
        },
    };
    Ok(ProbeResult { verdict, compared })
}

fn to_verdict(m: Vec<Mismatch>) -> Verdict {
    if m.is_empty() {
        Verdict::Present
    } else {
        Verdict::Partial(m)
    }
}

/// For a drop: still matching `was` means not applied.
fn before(m: Vec<Mismatch>) -> Verdict {
    if m.is_empty() {
        Verdict::Absent
    } else {
        Verdict::Partial(m)
    }
}

fn missing(prop: Prop, object: &str) -> Mismatch {
    Mismatch {
        prop,
        object: object.into(),
        want: "present".into(),
        got: "absent".into(),
    }
}

async fn col_of(
    pool: &Pool,
    table: &str,
    col: &str,
) -> Result<Option<(LiveTable, usize)>, CatalogError> {
    Ok(read_table(pool, table)
        .await?
        .and_then(|t| t.columns.iter().position(|c| c.name == col).map(|i| (t, i))))
}

async fn index_of(
    pool: &Pool,
    table: &str,
    name: &str,
) -> Result<Option<(LiveTable, LiveIndex)>, CatalogError> {
    Ok(read_table(pool, table).await?.and_then(|t| {
        let ix = t.indexes.iter().find(|i| i.name == name).cloned();
        ix.map(|i| (t, i))
    }))
}

/// An FK by name; an unnamed SQLite FK by its columns.
fn find_fk<'a>(live: &'a LiveTable, fk: &FkDecl) -> Option<&'a super::catalog::LiveFk> {
    live.fks
        .iter()
        .find(|f| f.name.as_deref() == Some(&fk.name))
        .or_else(|| {
            live.fks
                .iter()
                .find(|f| f.name.is_none() && f.columns == fk.columns)
        })
}

/// RESTRICT and NO ACTION both refuse the delete; the catalog may report either.
fn action(a: Option<&str>) -> String {
    match a.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NO ACTION" | "RESTRICT") => "NO ACTION".into(),
        Some(o) => o.into(),
    }
}

struct Cmp<'a> {
    pool: &'a Pool,
    d: DialectKind,
    props: &'a [Prop],
}

/// What an expression is, for the server round-trip.
#[derive(Debug, Clone)]
pub enum ExprKind<'a> {
    Default { column: &'a ColumnDecl },
    Check,
    Predicate { columns: &'a [String] },
    Generated { column: &'a ColumnDecl },
}

impl Cmp<'_> {
    fn on(&self, p: Prop) -> bool {
        self.props.contains(&p)
    }

    async fn table(&self, t: &TableDecl, live: &LiveTable) -> Result<Vec<Mismatch>, CatalogError> {
        let mut out = Vec::new();
        if self.on(Prop::TableComment) && t.comment != live.comment {
            out.push(Mismatch {
                prop: Prop::TableComment,
                object: t.name.clone(),
                want: format!("{:?}", t.comment),
                got: format!("{:?}", live.comment),
            });
        }
        let want: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        let got: Vec<&str> = live.columns.iter().map(|c| c.name.as_str()).collect();
        let mut ws = want.clone();
        let mut gs = got.clone();
        ws.sort_unstable();
        gs.sort_unstable();
        if ws != gs {
            out.push(Mismatch {
                prop: Prop::ColumnSet,
                object: t.name.clone(),
                want: want.join(","),
                got: got.join(","),
            });
        }
        for c in &t.columns {
            if let Some(i) = live.columns.iter().position(|l| l.name == c.name) {
                out.extend(
                    self.column(live, c, i)
                        .await?
                        .into_iter()
                        .filter(|m| m.prop != Prop::ColumnExists),
                );
            }
        }
        for k in &t.checks {
            if live
                .checks
                .iter()
                .any(|l| l.name.as_deref() == Some(&k.name))
            {
                out.extend(self.check(live, k).await?);
            } else {
                out.push(missing(Prop::CheckExpr, &k.name));
            }
        }
        for f in &t.fks {
            if find_fk(live, f).is_some() {
                out.extend(self.fk(live, f));
            } else {
                out.push(missing(Prop::FkColumns, &f.name));
            }
        }
        Ok(out)
    }

    async fn column(
        &self,
        live: &LiveTable,
        c: &ColumnDecl,
        i: usize,
    ) -> Result<Vec<Mismatch>, CatalogError> {
        let l: &LiveColumn = &live.columns[i];
        let dialect = self.pool.dialect();
        let mut out = Vec::new();
        let obj = format!("{}.{}", live.name, c.name);
        let mut push = |prop, want: String, got: String| {
            out.push(Mismatch {
                prop,
                object: obj.clone(),
                want,
                got,
            })
        };
        if self.on(Prop::ColType) {
            let (w, g) = (
                norm::norm_type(self.d, &c.sql_type(dialect)),
                norm::norm_type(self.d, &l.raw_type),
            );
            if w != g {
                push(Prop::ColType, w, g);
            }
        }
        let want_null = c.nullable && !c.primary_key;
        if self.on(Prop::ColNullable) && want_null != l.nullable {
            push(
                Prop::ColNullable,
                want_null.to_string(),
                l.nullable.to_string(),
            );
        }
        if self.on(Prop::ColComment) && c.comment != l.comment {
            push(
                Prop::ColComment,
                format!("{:?}", c.comment),
                format!("{:?}", l.comment),
            );
        }
        if self.on(Prop::ColDefault) {
            match (&c.default, &l.default) {
                (None, None) => {}
                (Some(w), Some(g)) => {
                    if !self
                        .expr_eq(live, ExprKind::Default { column: c }, w, g)
                        .await?
                    {
                        push(Prop::ColDefault, w.clone(), g.clone());
                    }
                }
                (w, g) => push(Prop::ColDefault, format!("{w:?}"), format!("{g:?}")),
            }
        }
        if self.on(Prop::ColGenerated) {
            match (&c.generated, &l.generated) {
                (None, None) => {}
                (Some(w), Some(g)) if g.stored => {
                    if !self
                        .expr_eq(live, ExprKind::Generated { column: c }, w, &g.expr)
                        .await?
                    {
                        push(Prop::ColGenerated, w.clone(), g.expr.clone());
                    }
                }
                (w, g) => push(Prop::ColGenerated, format!("{w:?}"), format!("{g:?}")),
            }
        }
        Ok(out)
    }

    async fn index(
        &self,
        live: &LiveTable,
        ix: &IndexDecl,
        l: &LiveIndex,
    ) -> Result<Vec<Mismatch>, CatalogError> {
        let mut out = Vec::new();
        let mut push = |prop, want: String, got: String| {
            out.push(Mismatch {
                prop,
                object: ix.name.clone(),
                want,
                got,
            })
        };
        if self.on(Prop::IndexColumns) && ix.columns != l.columns {
            push(
                Prop::IndexColumns,
                ix.columns.join(","),
                l.columns.join(","),
            );
        }
        if self.on(Prop::IndexUnique) && ix.unique != l.unique {
            push(
                Prop::IndexUnique,
                ix.unique.to_string(),
                l.unique.to_string(),
            );
        }
        if self.on(Prop::IndexMethod) && ix.method != l.method {
            push(Prop::IndexMethod, ix.method.clone(), l.method.clone());
        }
        if self.on(Prop::IndexInclude) && ix.include != l.include {
            push(
                Prop::IndexInclude,
                ix.include.join(","),
                l.include.join(","),
            );
        }
        if self.on(Prop::IndexValid) && !l.valid {
            push(Prop::IndexValid, "valid".into(), "invalid".into());
        }
        if self.on(Prop::IndexPredicate) {
            match (&ix.predicate, &l.predicate) {
                (None, None) => {}
                (Some(w), Some(g)) => {
                    if !self
                        .expr_eq(
                            live,
                            ExprKind::Predicate {
                                columns: &ix.columns,
                            },
                            w,
                            g,
                        )
                        .await?
                    {
                        out.push(Mismatch {
                            prop: Prop::IndexPredicate,
                            object: ix.name.clone(),
                            want: w.clone(),
                            got: g.clone(),
                        });
                    }
                }
                (w, g) => out.push(Mismatch {
                    prop: Prop::IndexPredicate,
                    object: ix.name.clone(),
                    want: format!("{w:?}"),
                    got: format!("{g:?}"),
                }),
            }
        }
        Ok(out)
    }

    async fn check(&self, live: &LiveTable, k: &CheckDecl) -> Result<Vec<Mismatch>, CatalogError> {
        let Some(l) = live
            .checks
            .iter()
            .find(|l| l.name.as_deref() == Some(&k.name))
        else {
            return Ok(vec![missing(Prop::CheckExists, &k.name)]);
        };
        let mut out = Vec::new();
        if self.on(Prop::CheckExpr)
            && !self
                .expr_eq(live, ExprKind::Check, &k.expr, &l.expr)
                .await?
        {
            out.push(Mismatch {
                prop: Prop::CheckExpr,
                object: k.name.clone(),
                want: k.expr.clone(),
                got: l.expr.clone(),
            });
        }
        if self.on(Prop::CheckValidated) && !l.validated {
            out.push(Mismatch {
                prop: Prop::CheckValidated,
                object: k.name.clone(),
                want: "validated".into(),
                got: "NOT VALID".into(),
            });
        }
        Ok(out)
    }

    fn fk(&self, live: &LiveTable, f: &FkDecl) -> Vec<Mismatch> {
        let Some(l) = find_fk(live, f) else {
            return vec![missing(Prop::FkExists, &f.name)];
        };
        let mut out = Vec::new();
        let mut push = |prop, want: String, got: String| {
            out.push(Mismatch {
                prop,
                object: f.name.clone(),
                want,
                got,
            })
        };
        if self.on(Prop::FkColumns) && f.columns != l.columns {
            push(Prop::FkColumns, f.columns.join(","), l.columns.join(","));
        }
        if self.on(Prop::FkTarget) && (f.ref_table != l.ref_table || f.ref_columns != l.ref_columns)
        {
            push(
                Prop::FkTarget,
                format!("{}({})", f.ref_table, f.ref_columns.join(",")),
                format!("{}({})", l.ref_table, l.ref_columns.join(",")),
            );
        }
        let (w, g) = (action(f.on_delete.as_deref()), action(Some(&l.on_delete)));
        if self.on(Prop::FkOnDelete) && w != g {
            push(Prop::FkOnDelete, w, g);
        }
        if self.on(Prop::FkValidated) && !l.validated {
            push(Prop::FkValidated, "validated".into(), "NOT VALID".into());
        }
        out
    }

    /// Pure normalisation first; on PG and MySQL, a server round-trip of
    /// the declared text when the pure forms differ.
    async fn expr_eq(
        &self,
        live: &LiveTable,
        kind: ExprKind<'_>,
        declared: &str,
        got: &str,
    ) -> Result<bool, CatalogError> {
        let ctx = match &kind {
            ExprKind::Default { column } => DefaultCtx {
                boolean: column.is_bool(),
            },
            _ => DefaultCtx::default(),
        };
        let g = norm::norm_default(self.d, got, ctx)?;
        if norm::norm_default(self.d, declared, ctx)? == g {
            return Ok(true);
        }
        match roundtrip(self.pool, live, &kind, declared).await? {
            Some(canon) => Ok(norm::norm_default(self.d, &canon, ctx)? == g),
            None => Ok(false),
        }
    }
}

/// The server's own spelling of `declared`, read back from a scratch temp
/// table with `live`'s columns on one pinned connection that is closed
/// afterwards (MySQL temp tables are session state and survive ROLLBACK).
/// `None` where no round-trip exists (SQLite stores the text as written).
///
/// # Errors
/// Driver failures, including the declared text being invalid.
pub async fn roundtrip(
    pool: &Pool,
    live: &LiveTable,
    kind: &ExprKind<'_>,
    declared: &str,
) -> Result<Option<String>, CatalogError> {
    use crate::sql::{raw_execute_tx, raw_query_tx};
    let d = pool.dialect();
    let k = d.kind();
    if !matches!(k, DialectKind::Postgres | DialectKind::Mysql) {
        return Ok(None);
    }
    let name = "_rgo_s6_canon";
    let mut cols: Vec<String> = live
        .columns
        .iter()
        .filter(|c| !matches!(kind, ExprKind::Default { column } if column.name == c.name))
        .map(|c| format!("{} {}", d.quote_ident(&c.name), c.raw_type))
        .collect();
    match kind {
        ExprKind::Default { column } => cols.push(format!(
            "{} {} DEFAULT {declared}",
            d.quote_ident(&column.name),
            column.sql_type(d)
        )),
        ExprKind::Generated { column } => cols.push(format!(
            "{} {} GENERATED ALWAYS AS ({declared}) STORED",
            d.quote_ident("_rgo_g"),
            column.sql_type(d)
        )),
        ExprKind::Check => cols.push(format!(
            "CONSTRAINT {} CHECK ({declared})",
            d.quote_ident("_rgo_c")
        )),
        ExprKind::Predicate { .. } => {}
    }
    let temp = if k == DialectKind::Postgres {
        "TEMP"
    } else {
        "TEMPORARY"
    };
    let tail = if k == DialectKind::Postgres {
        " ON COMMIT DROP"
    } else {
        ""
    };
    let create = format!(
        "CREATE {temp} TABLE {} ({}){tail}",
        d.quote_ident(name),
        cols.join(", ")
    );
    let mut conn = pool.acquire().await?;
    conn.close_on_drop();
    let mut tx = conn.begin().await?;
    raw_execute_tx(&mut tx, &create, Vec::new()).await?;
    let out: Option<String> = if k == DialectKind::Postgres {
        let reg = vec![SqlValue::from(format!("pg_temp.{name}"))];
        match kind {
            ExprKind::Default { column } => raw_query_tx::<(String,)>(
                &mut tx,
                "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a \
                 ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = to_regclass($1) AND a.attname = $2",
                vec![reg[0].clone(), SqlValue::from(column.name.clone())],
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Generated { .. } => raw_query_tx::<(String,)>(
                &mut tx,
                "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a \
                 ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE d.adrelid = to_regclass($1) AND a.attname = '_rgo_g'",
                reg,
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Check => raw_query_tx::<(String,)>(
                &mut tx,
                "SELECT pg_get_expr(conbin, conrelid) FROM pg_constraint WHERE conrelid = to_regclass($1) AND conname = '_rgo_c'",
                reg,
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Predicate { columns } => {
                let keys = columns.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", ");
                raw_execute_tx(&mut tx, &format!("CREATE INDEX _rgo_i ON {} ({keys}) WHERE {declared}", d.quote_ident(name)), Vec::new())
                    .await?;
                raw_query_tx::<(String,)>(
                    &mut tx,
                    "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i WHERE i.indrelid = to_regclass($1)",
                    reg,
                )
                .await?
                .pop()
                .map(|r| r.0)
            }
        }
    } else {
        let show: Vec<(String, String)> = raw_query_tx(
            &mut tx,
            &format!("SHOW CREATE TABLE {}", d.quote_ident(name)),
            Vec::new(),
        )
        .await?;
        let ddl = show.into_iter().next().map(|r| r.1).unwrap_or_default();
        match kind {
            ExprKind::Check => body_after(&ddl, "CONSTRAINT `_rgo_c` CHECK ("),
            ExprKind::Generated { .. } => ddl
                .find("`_rgo_g`")
                .and_then(|at| body_after(&ddl[at..], "GENERATED ALWAYS AS (")),
            // MySQL's default printer is close to its input; the pure form covers it.
            ExprKind::Default { .. } | ExprKind::Predicate { .. } => None,
        }
    };
    let _ = tx.rollback().await;
    Ok(out)
}

/// The balanced-paren body that follows `marker` (which ends in `(`).
fn body_after(s: &str, marker: &str) -> Option<String> {
    let start = s.find(marker)? + marker.len();
    let mut depth = 1i32;
    let mut in_str = false;
    let b = s.as_bytes();
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'\'' {
                in_str = false;
            }
        } else if c == b'\'' {
            in_str = true;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(s[start..i].to_owned());
            }
        }
        i += 1;
    }
    None
}
