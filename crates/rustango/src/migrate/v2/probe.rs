//! Probes (3.7) and the drift check (9.2), on the live catalog reader.
//!
//! A probe answers "is this unit's effect in the catalog?" against the
//! state after the unit: `Present` = every compared property matches,
//! `Absent` = the unit did not run, `Partial` = neither, with the diff.

use super::catalog::{read_table, CatalogError, LiveColumn, LiveFk, LiveIndex, LiveTable};
use super::norm::{self, DefaultCtx};
use super::render::{snapshot, Expect};
use super::state::{CheckDef, ColumnDef, FkDef, IndexDef, OnDelete, State, TableState};
use crate::core::SqlValue;
use crate::sql::{DialectKind, Pool};

/// One property that differs from what state says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mismatch {
    pub(crate) object: String,
    pub(crate) what: &'static str,
    pub(crate) want: String,
    pub(crate) got: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: want {}, got {}",
            self.object, self.what, self.want, self.got
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Present,
    Absent,
    Partial(Vec<Mismatch>),
}

fn mm(
    object: &str,
    what: &'static str,
    want: impl Into<String>,
    got: impl Into<String>,
) -> Mismatch {
    Mismatch {
        object: object.to_owned(),
        what,
        want: want.into(),
        got: got.into(),
    }
}

/// How names are matched: a probe compares the names v2 wrote; the drift
/// check of an adopted state also accepts names engine v1 left to the
/// backend (an unnamed SQLite constraint, a MySQL `_chk_<n>`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Names {
    Exact,
    Shape,
}

/// Probe the facts a unit's spec lists.
///
/// # Errors
/// Catalog read failures.
pub(crate) async fn probe(pool: &Pool, expects: &[Expect]) -> Result<Verdict, CatalogError> {
    let mut present = 0;
    let mut absent = 0;
    let mut diffs = Vec::new();
    for e in expects {
        match one(pool, e).await? {
            Verdict::Present => present += 1,
            Verdict::Absent => absent += 1,
            Verdict::Partial(m) => diffs.extend(m),
        }
    }
    Ok(if diffs.is_empty() && absent == 0 {
        Verdict::Present
    } else if diffs.is_empty() && present == 0 {
        Verdict::Absent
    } else {
        if diffs.is_empty() {
            diffs.push(mm("unit", "facts", "all present or all absent", "a mix"));
        }
        Verdict::Partial(diffs)
    })
}

fn verdict(m: Vec<Mismatch>) -> Verdict {
    if m.is_empty() {
        Verdict::Present
    } else {
        Verdict::Partial(m)
    }
}

async fn one(pool: &Pool, e: &Expect) -> Result<Verdict, CatalogError> {
    let c = Cmp {
        pool,
        d: pool.dialect().kind(),
        names: Names::Exact,
    };
    Ok(match e {
        Expect::Table { table } => match read_table(pool, &table.name).await? {
            None => Verdict::Absent,
            Some(live) => verdict(c.table(table, &live).await?),
        },
        Expect::NoTable { table } => match read_table(pool, table).await? {
            None => Verdict::Present,
            Some(_) => Verdict::Absent,
        },
        Expect::Column {
            table,
            column,
            before,
        } => {
            let live = read_table(pool, table).await?;
            match live
                .as_ref()
                .and_then(|l| l.columns.iter().find(|x| x.name == column.column))
            {
                None => Verdict::Absent,
                Some(lc) => {
                    let live = live.as_ref().expect("found a column");
                    let after = c.column(live, column, lc).await?;
                    if after.is_empty() {
                        Verdict::Present
                    } else if let Some(b) = before {
                        if c.column(live, b, lc).await?.is_empty() {
                            Verdict::Absent
                        } else {
                            Verdict::Partial(after)
                        }
                    } else {
                        Verdict::Partial(after)
                    }
                }
            }
        }
        Expect::NoColumn { table, column } => match read_table(pool, table).await? {
            Some(l) if l.columns.iter().any(|x| &x.name == column) => Verdict::Absent,
            _ => Verdict::Present,
        },
        Expect::Index { table, index } => match read_table(pool, table).await? {
            Some(l) => match l.indexes.iter().find(|i| i.name == index.name) {
                Some(li) => verdict(c.index(&l, index, li).await?),
                None => Verdict::Absent,
            },
            None => Verdict::Absent,
        },
        Expect::NoIndex { table, name } => match read_table(pool, table).await? {
            Some(l) => match l.indexes.iter().find(|i| &i.name == name) {
                // A drop left an invalid index behind: Partial, so resume drops again.
                Some(li) if !li.valid => {
                    Verdict::Partial(vec![mm(name, "index", "dropped", "still there, invalid")])
                }
                Some(_) => Verdict::Absent,
                None => Verdict::Present,
            },
            None => Verdict::Present,
        },
        Expect::Check { table, check } => match read_table(pool, table).await? {
            Some(l)
                if l.checks
                    .iter()
                    .any(|k| k.name.as_deref() == Some(&check.name)) =>
            {
                verdict(c.check(&l, check).await?)
            }
            _ => Verdict::Absent,
        },
        Expect::NoCheck { table, name } => match read_table(pool, table).await? {
            Some(l) if l.checks.iter().any(|k| k.name.as_deref() == Some(name)) => Verdict::Absent,
            _ => Verdict::Present,
        },
        Expect::Fk { table, fk } => match read_table(pool, table).await? {
            Some(l) if c.find_fk(&l, fk).is_some() => verdict(c.fk(&l, fk)),
            _ => Verdict::Absent,
        },
        Expect::NoFk { table, name } => match read_table(pool, table).await? {
            Some(l) if l.fks.iter().any(|f| f.name.as_deref() == Some(name)) => Verdict::Absent,
            _ => Verdict::Present,
        },
        Expect::Sql { sql } => {
            let rows: Vec<(i64,)> = crate::sql::raw_query_pool(
                &format!("SELECT COUNT(*) FROM ({sql}) AS probe"),
                Vec::new(),
                pool,
            )
            .await?;
            if rows.first().is_some_and(|r| r.0 > 0) {
                Verdict::Present
            } else {
                Verdict::Absent
            }
        }
        Expect::Idempotent => Verdict::Absent,
    })
}

/// Every table of `state` against the catalog: what differs, extra
/// indexes, checks and FKs included. Names v1 left to the backend match by
/// shape.
///
/// # Errors
/// Catalog read failures.
pub(crate) async fn drift(pool: &Pool, state: &State) -> Result<Vec<Mismatch>, CatalogError> {
    let c = Cmp {
        pool,
        d: pool.dialect().kind(),
        names: Names::Shape,
    };
    let mut out = Vec::new();
    for t in state.tables.values() {
        match read_table(pool, &t.name).await? {
            None => out.push(mm(&t.name, "table", "present", "absent")),
            Some(live) => out.extend(c.table(t, &live).await?),
        }
    }
    Ok(out)
}

/// What the per-database switch renames to the checkpoint's name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Renamed {
    Index(IndexDef),
    Check(CheckDef),
    Fk(FkDef),
}

/// An object found by shape under another name than the checkpoint's.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rename {
    pub(crate) table: String,
    pub(crate) old: String,
    pub(crate) to: Renamed,
}

/// Objects of `state` the catalog holds under another name. Unnamed ones
/// (SQLite, an autoindex) have no name to change and are left alone.
///
/// # Errors
/// Catalog read failures.
pub(crate) async fn renames(pool: &Pool, state: &State) -> Result<Vec<Rename>, CatalogError> {
    let c = Cmp {
        pool,
        d: pool.dialect().kind(),
        names: Names::Shape,
    };
    let b = backend(c.d);
    let mut out = Vec::new();
    for t in state.tables.values() {
        let Some(live) = read_table(pool, &t.name).await? else {
            continue;
        };
        for ix in t.indexes.iter().filter(|i| !i.skipped_on.contains(&b)) {
            if live.indexes.iter().any(|l| l.name == ix.name) {
                continue;
            }
            if let Some(l) = live.indexes.iter().find(|l| {
                !l.primary && !l.name.starts_with("sqlite_autoindex") && c.same_index(l, ix)
            }) {
                out.push(Rename {
                    table: t.name.clone(),
                    old: l.name.clone(),
                    to: Renamed::Index(ix.clone()),
                });
            }
        }
        for k in &t.checks {
            if live
                .checks
                .iter()
                .any(|l| l.name.as_deref() == Some(&k.name))
            {
                continue;
            }
            for l in &live.checks {
                let Some(name) = &l.name else { continue };
                let sql = k.body.sql(pool.dialect());
                if c.expr_eq(&live, ExprKind::Check, &sql, &l.expr).await? {
                    out.push(Rename {
                        table: t.name.clone(),
                        old: name.clone(),
                        to: Renamed::Check(k.clone()),
                    });
                    break;
                }
            }
        }
        for f in &t.fks {
            if live.fks.iter().any(|l| l.name.as_deref() == Some(&f.name)) {
                continue;
            }
            if let Some(name) = c.find_fk(&live, f).and_then(|l| l.name.clone()) {
                out.push(Rename {
                    table: t.name.clone(),
                    old: name,
                    to: Renamed::Fk(f.clone()),
                });
            }
        }
    }
    Ok(out)
}

/// RESTRICT and NO ACTION are apart in state; the catalog of MySQL and
/// SQLite cannot tell them apart, so there they compare equal.
fn action(d: DialectKind, a: &str) -> String {
    let a = a.to_ascii_uppercase();
    match (d, a.as_str()) {
        (DialectKind::Mysql | DialectKind::Sqlite, "RESTRICT" | "NO ACTION" | "") => {
            "NO ACTION".into()
        }
        (_, "") => "NO ACTION".into(),
        _ => a,
    }
}

fn on_delete_sql(o: OnDelete) -> &'static str {
    match o {
        OnDelete::NoAction => "NO ACTION",
        OnDelete::Restrict => "RESTRICT",
        OnDelete::Cascade => "CASCADE",
        OnDelete::SetNull => "SET NULL",
        OnDelete::SetDefault => "SET DEFAULT",
    }
}

struct Cmp<'a> {
    pool: &'a Pool,
    d: DialectKind,
    names: Names,
}

/// What an expression is, for the server round-trip.
enum ExprKind<'a> {
    Default { column: &'a ColumnDef },
    Check,
    Predicate { columns: &'a [String] },
    Generated { column: &'a ColumnDef },
}

impl Cmp<'_> {
    async fn table(&self, t: &TableState, live: &LiveTable) -> Result<Vec<Mismatch>, CatalogError> {
        let mut out = Vec::new();
        if self.d != DialectKind::Sqlite && t.comment != live.comment {
            out.push(mm(
                &t.name,
                "comment",
                format!("{:?}", t.comment),
                format!("{:?}", live.comment),
            ));
        }
        let mut want: Vec<&str> = t.columns.iter().map(|c| c.column.as_str()).collect();
        let mut got: Vec<&str> = live.columns.iter().map(|c| c.name.as_str()).collect();
        want.sort_unstable();
        got.sort_unstable();
        if want != got {
            out.push(mm(&t.name, "columns", want.join(","), got.join(",")));
        }
        for c in &t.columns {
            if let Some(lc) = live.columns.iter().find(|l| l.name == c.column) {
                out.extend(self.column(live, c, lc).await?);
            }
        }
        for k in &t.checks {
            out.extend(self.check(live, k).await?);
        }
        for f in &t.fks {
            out.extend(self.fk(live, f));
        }
        let wanted: Vec<&IndexDef> = t
            .indexes
            .iter()
            .filter(|i| !i.skipped_on.contains(&backend(self.d)))
            .collect();
        for ix in &wanted {
            match self.find_index(live, ix) {
                Some(li) => out.extend(self.index(live, ix, li).await?),
                None => out.push(mm(&ix.name, "index", "present", "absent")),
            }
        }
        if self.d == DialectKind::Postgres {
            let pk = live
                .indexes
                .iter()
                .find(|i| i.primary)
                .map(|i| i.name.clone());
            if t.pg_pkey.is_some() && pk != t.pg_pkey && self.names == Names::Exact {
                out.push(mm(
                    &t.name,
                    "pkey name",
                    format!("{:?}", t.pg_pkey),
                    format!("{pk:?}"),
                ));
            }
        }
        if self.names == Names::Shape {
            for li in live.indexes.iter().filter(|i| !i.primary) {
                let known = wanted.iter().any(|ix| self.same_index(li, ix));
                if !known {
                    out.push(mm(
                        &li.name,
                        "index",
                        "absent",
                        "an index state does not hold",
                    ));
                }
            }
            if live.fks.len() > t.fks.len() {
                out.push(mm(
                    &t.name,
                    "FKs",
                    t.fks.len().to_string(),
                    live.fks.len().to_string(),
                ));
            }
            if live.checks.len() > t.checks.len() {
                out.push(mm(
                    &t.name,
                    "checks",
                    t.checks.len().to_string(),
                    live.checks.len().to_string(),
                ));
            }
        }
        Ok(out)
    }

    async fn column(
        &self,
        live: &LiveTable,
        c: &ColumnDef,
        l: &LiveColumn,
    ) -> Result<Vec<Mismatch>, CatalogError> {
        let dialect = self.pool.dialect();
        let obj = format!("{}.{}", live.name, c.column);
        let mut out = Vec::new();
        let want_type = crate::migrate::diff::sql_type_with_dialect(&snapshot(c), dialect);
        let (w, g) = (
            norm::norm_type(self.d, &want_type),
            norm::norm_type(self.d, &l.raw_type),
        );
        if w != g {
            out.push(mm(&obj, "type", w, g));
        }
        let want_null = c.nullable && !c.primary_key;
        if want_null != l.nullable {
            out.push(mm(
                &obj,
                "nullable",
                want_null.to_string(),
                l.nullable.to_string(),
            ));
        }
        if self.d != DialectKind::Sqlite && c.comment != l.comment {
            out.push(mm(
                &obj,
                "comment",
                format!("{:?}", c.comment),
                format!("{:?}", l.comment),
            ));
        }
        match (&c.default, &l.default) {
            (None, None) => {}
            (Some(w), Some(g)) => {
                if !self
                    .expr_eq(live, ExprKind::Default { column: c }, w, g)
                    .await?
                {
                    out.push(mm(&obj, "default", w.clone(), g.clone()));
                }
            }
            (w, g) => out.push(mm(&obj, "default", format!("{w:?}"), format!("{g:?}"))),
        }
        match (&c.generated_as, &l.generated) {
            (None, None) => {}
            (Some(w), Some(g)) if g.stored => {
                if !self
                    .expr_eq(live, ExprKind::Generated { column: c }, w, &g.expr)
                    .await?
                {
                    out.push(mm(&obj, "generated", w.clone(), g.expr.clone()));
                }
            }
            (w, g) => out.push(mm(&obj, "generated", format!("{w:?}"), format!("{g:?}"))),
        }
        Ok(out)
    }

    fn find_index<'l>(&self, live: &'l LiveTable, ix: &IndexDef) -> Option<&'l LiveIndex> {
        live.indexes.iter().find(|i| i.name == ix.name).or_else(|| {
            (self.names == Names::Shape)
                .then(|| {
                    live.indexes
                        .iter()
                        .find(|i| !i.primary && self.same_index(i, ix))
                })
                .flatten()
        })
    }

    fn same_index(&self, li: &LiveIndex, ix: &IndexDef) -> bool {
        li.name == ix.name
            || (self.names == Names::Shape && li.columns == ix.columns && li.unique == ix.unique)
    }

    async fn index(
        &self,
        live: &LiveTable,
        ix: &IndexDef,
        l: &LiveIndex,
    ) -> Result<Vec<Mismatch>, CatalogError> {
        let mut out = Vec::new();
        if ix.columns != l.columns {
            out.push(mm(
                &ix.name,
                "index columns",
                ix.columns.join(","),
                l.columns.join(","),
            ));
        }
        if ix.unique != l.unique {
            out.push(mm(
                &ix.name,
                "unique",
                ix.unique.to_string(),
                l.unique.to_string(),
            ));
        }
        if self.d == DialectKind::Postgres {
            if ix.method != l.method {
                out.push(mm(&ix.name, "method", ix.method.clone(), l.method.clone()));
            }
            if ix.include != l.include {
                out.push(mm(
                    &ix.name,
                    "include",
                    ix.include.join(","),
                    l.include.join(","),
                ));
            }
            if !l.valid {
                out.push(mm(&ix.name, "index", "valid", "invalid"));
            }
        }
        if self.d != DialectKind::Mysql {
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
                        out.push(mm(&ix.name, "predicate", w.clone(), g.clone()));
                    }
                }
                (w, g) => out.push(mm(
                    &ix.name,
                    "predicate",
                    format!("{w:?}"),
                    format!("{g:?}"),
                )),
            }
        }
        Ok(out)
    }

    async fn check(&self, live: &LiveTable, k: &CheckDef) -> Result<Vec<Mismatch>, CatalogError> {
        let sql = k.body.sql(self.pool.dialect());
        let by_name = live
            .checks
            .iter()
            .find(|l| l.name.as_deref() == Some(&k.name));
        let l = match by_name {
            Some(l) => l,
            None if self.names == Names::Shape => {
                let mut found = None;
                for l in &live.checks {
                    if self.expr_eq(live, ExprKind::Check, &sql, &l.expr).await? {
                        found = Some(l);
                        break;
                    }
                }
                match found {
                    Some(l) => l,
                    None => return Ok(vec![mm(&k.name, "check", "present", "absent")]),
                }
            }
            None => return Ok(vec![mm(&k.name, "check", "present", "absent")]),
        };
        let mut out = Vec::new();
        if !self.expr_eq(live, ExprKind::Check, &sql, &l.expr).await? {
            out.push(mm(&k.name, "check expr", sql, l.expr.clone()));
        }
        if self.d == DialectKind::Postgres && !l.validated {
            out.push(mm(&k.name, "check", "validated", "NOT VALID"));
        }
        Ok(out)
    }

    /// An FK by name; an unnamed one (SQLite, or v1's) by columns and target.
    fn find_fk<'l>(&self, live: &'l LiveTable, fk: &FkDef) -> Option<&'l LiveFk> {
        live.fks
            .iter()
            .find(|f| f.name.as_deref() == Some(&fk.name))
            .or_else(|| {
                live.fks.iter().find(|f| {
                    (f.name.is_none() || self.names == Names::Shape)
                        && f.columns == fk.columns
                        && f.ref_table == fk.to
                })
            })
    }

    fn fk(&self, live: &LiveTable, f: &FkDef) -> Vec<Mismatch> {
        let Some(l) = self.find_fk(live, f) else {
            return vec![mm(&f.name, "FK", "present", "absent")];
        };
        let mut out = Vec::new();
        if f.columns != l.columns {
            out.push(mm(
                &f.name,
                "FK columns",
                f.columns.join(","),
                l.columns.join(","),
            ));
        }
        if f.to != l.ref_table || f.on != l.ref_columns {
            out.push(mm(
                &f.name,
                "FK target",
                format!("{}({})", f.to, f.on.join(",")),
                format!("{}({})", l.ref_table, l.ref_columns.join(",")),
            ));
        }
        let (w, g) = (
            action(self.d, on_delete_sql(f.on_delete)),
            action(self.d, &l.on_delete),
        );
        if w != g {
            out.push(mm(&f.name, "on_delete", w, g));
        }
        if self.d == DialectKind::Postgres && !l.validated {
            out.push(mm(&f.name, "FK", "validated", "NOT VALID"));
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
                boolean: column.ty == "bool",
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

fn backend(d: DialectKind) -> super::state::Backend {
    match d {
        DialectKind::Postgres => super::state::Backend::Postgres,
        DialectKind::Mysql => super::state::Backend::Mysql,
        _ => super::state::Backend::Sqlite,
    }
}

/// Whether `declared` works as `c`'s DEFAULT here (2.6): a scratch temp
/// table and one default row, in a transaction rolled back on a pinned
/// connection closed afterwards. `Err` carries the server's message.
pub(crate) async fn default_runs(pool: &Pool, c: &ColumnDef, declared: &str) -> Result<(), String> {
    use crate::sql::raw_execute_tx;
    let d = pool.dialect();
    let name = d.quote_ident("_rgo_v2_default");
    let ty = crate::migrate::diff::sql_type_with_dialect(&snapshot(c), d);
    let not_null = if c.nullable { "" } else { " NOT NULL" };
    let (temp, tail) = match d.kind() {
        DialectKind::Postgres => ("TEMP", " ON COMMIT DROP"),
        DialectKind::Mysql => ("TEMPORARY", ""),
        _ => ("TEMP", ""),
    };
    let create = format!(
        "CREATE {temp} TABLE {name} ({} {ty} DEFAULT {declared}{not_null}){tail}",
        d.quote_ident(&c.column)
    );
    let insert = format!("INSERT INTO {name}{}", d.default_values_clause());
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    conn.close_on_drop();
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;
    raw_execute_tx(&mut tx, &create, Vec::new())
        .await
        .map_err(|e| e.to_string())?;
    raw_execute_tx(&mut tx, &insert, Vec::new())
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The server's own spelling of `declared`, read back from a scratch temp
/// table on one pinned connection that is closed afterwards (MySQL temp
/// tables are session state and survive ROLLBACK). `None` on SQLite, which
/// stores the text as written.
async fn roundtrip(
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
    let sql_type = |c: &ColumnDef| crate::migrate::diff::sql_type_with_dialect(&snapshot(c), d);
    let name = "_rgo_v2_canon";
    let mut cols: Vec<String> = live
        .columns
        .iter()
        .filter(|c| !matches!(kind, ExprKind::Default { column } if column.column == c.name))
        .map(|c| format!("{} {}", d.quote_ident(&c.name), c.raw_type))
        .collect();
    match kind {
        ExprKind::Default { column } => cols.push(format!(
            "{} {} DEFAULT {declared}",
            d.quote_ident(&column.column),
            sql_type(column)
        )),
        ExprKind::Generated { column } => cols.push(format!(
            "{} {} GENERATED ALWAYS AS ({declared}) STORED",
            d.quote_ident("_rgo_g"),
            sql_type(column)
        )),
        ExprKind::Check => cols.push(format!(
            "CONSTRAINT {} CHECK ({declared})",
            d.quote_ident("_rgo_c")
        )),
        ExprKind::Predicate { .. } => {}
    }
    let (temp, tail) = if k == DialectKind::Postgres {
        ("TEMP", " ON COMMIT DROP")
    } else {
        ("TEMPORARY", "")
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
        let reg = SqlValue::from(format!("pg_temp.{name}"));
        let attr = "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d JOIN pg_attribute a \
                    ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
                    WHERE d.adrelid = to_regclass($1) AND a.attname = $2";
        match kind {
            ExprKind::Default { column } => raw_query_tx::<(String,)>(
                &mut tx,
                attr,
                vec![reg, SqlValue::from(column.column.clone())],
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Generated { .. } => raw_query_tx::<(String,)>(
                &mut tx,
                attr,
                vec![reg, SqlValue::from("_rgo_g".to_owned())],
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Check => raw_query_tx::<(String,)>(
                &mut tx,
                "SELECT pg_get_expr(conbin, conrelid) FROM pg_constraint \
                 WHERE conrelid = to_regclass($1) AND conname = '_rgo_c'",
                vec![reg],
            )
            .await?
            .pop()
            .map(|r| r.0),
            ExprKind::Predicate { columns } => {
                let keys = columns
                    .iter()
                    .map(|c| d.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                raw_execute_tx(
                    &mut tx,
                    &format!(
                        "CREATE INDEX _rgo_i ON {} ({keys}) WHERE {declared}",
                        d.quote_ident(name)
                    ),
                    Vec::new(),
                )
                .await?;
                raw_query_tx::<(String,)>(
                    &mut tx,
                    "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i WHERE i.indrelid = to_regclass($1)",
                    vec![reg],
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
