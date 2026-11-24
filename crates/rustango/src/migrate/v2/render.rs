//! Per-op rendering: `(state before, state after, op, backend) -> units`.
//!
//! Pure: no pool, so nothing here reads the live catalog. Each op renders
//! only its own statements, in place; defaults go out exactly as written;
//! no `IF [NOT] EXISTS` except `create_extension`.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::format::{File, Kind, Op, OpTx, Probe as FileProbe, ProbeKind};
use super::replay::{apply, Ctx};
use super::state::{Backend, CheckDef, ColumnDef, FkDef, IndexDef, OnDelete, State, TableState};
use super::{Code, LoadError, MAX_IDENT};
use crate::migrate::diff::{self, DefaultText, SchemaChange};
use crate::migrate::rebuild::TableRebuild;
use crate::migrate::snapshot::{FieldSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot};
use crate::sql::{Dialect, TableTarget};

/// How a unit runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitKind {
    /// One transaction, with the ledger update.
    Tx,
    /// Each statement commits on its own (MySQL DDL, PG `CONCURRENTLY`).
    Autocommit,
    /// SQLite rebuild: FK enforcement off outside, `foreign_key_check` before commit.
    Rebuild,
}

/// What a probe checks after an autocommit unit: the state just after it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expect {
    Table(Arc<TableState>),
    NoTable(String),
    Column {
        table: String,
        column: ColumnDef,
    },
    NoColumn {
        table: String,
        column: String,
    },
    Index {
        table: String,
        index: IndexDef,
    },
    NoIndex {
        table: String,
        name: String,
    },
    Check {
        table: String,
        check: CheckDef,
    },
    NoCheck {
        table: String,
        name: String,
    },
    Fk {
        table: String,
        fk: FkDef,
    },
    NoFk {
        table: String,
        name: String,
    },
    /// The file's own probe SQL.
    Sql(String),
    /// The statement is safe to run again.
    Idempotent,
}

/// One unit: its op index, how it runs, its SQL and, for autocommit units,
/// the probe of the state after it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unit {
    pub(crate) op: usize,
    pub(crate) kind: UnitKind,
    pub(crate) sql: Vec<String>,
    pub(crate) probe: Option<Vec<Expect>>,
}

/// Session and transaction statements: refused in a transactional unit.
const SESSION_STMTS: &[&str] = &[
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "START TRANSACTION",
    "SET ROLE",
    "SET SEARCH_PATH",
    "SET SESSION",
    "USE ",
];

/// DDL MySQL commits on its own: refused in a transactional raw_sql unit there.
const MYSQL_DDL: &[&str] = &["CREATE ", "ALTER ", "DROP ", "RENAME ", "TRUNCATE "];

pub(crate) fn dialect(b: Backend) -> &'static dyn Dialect {
    match b {
        Backend::Postgres => &crate::sql::Postgres,
        Backend::Mysql => &crate::sql::MySql,
        Backend::Sqlite => &crate::sql::Sqlite,
    }
}

/// Render a whole file from `state`: one op at a time, each against the
/// state just before it. Returns the units and the state after.
///
/// # Errors
/// The first op that does not replay or render, or an autocommit unit
/// with no probe ([`Code::Probe`]).
pub(crate) fn render_file(
    state: &State,
    file: &File,
    at: &str,
    ctx: &Ctx<'_>,
    b: Backend,
) -> Result<(Vec<Unit>, State), LoadError> {
    if file.kind == Kind::Checkpoint {
        let mut after = state.clone();
        super::replay::apply_file(&mut after, file, at, ctx)?;
        return Ok((Vec::new(), after));
    }
    let mut st = state.clone();
    let mut created: BTreeSet<String> = BTreeSet::new();
    let mut units = Vec::new();
    for (i, env) in file.operations.iter().enumerate() {
        let op_at = format!("{at}#/operations/{i}");
        let mut after = st.clone();
        let here: Vec<&str> = created.iter().map(String::as_str).collect();
        apply(&mut after, &env.op, &op_at, ctx, &here)?;
        let r = Render {
            before: &st,
            after: &after,
            d: dialect(b),
            b,
            created: &created,
            at: &op_at,
        };
        for mut u in r.op(&env.op, env.tx)? {
            if u.kind == UnitKind::Autocommit && u.probe.is_none() {
                return Err(LoadError::new(
                    Code::Probe,
                    &op_at,
                    format!(
                        "a unit of this op commits on its own on {} and has no probe for the \
                         state after it; give `probe` (or `\"idempotent\": true`)",
                        b.as_str()
                    ),
                ));
            }
            u.op = i;
            units.push(u);
        }
        match &env.op {
            Op::CreateTable { table, .. } => {
                created.insert(table.name.clone());
            }
            Op::RenameTable { from, to } if created.remove(from) => {
                created.insert(to.clone());
            }
            _ => {}
        }
        st = after;
    }
    Ok((units, st))
}

/// Every file of `project` rendered for `b`, each target in canonical
/// order, registry first: `(file index, units)`.
///
/// # Errors
/// The first file that does not replay or render.
pub(crate) fn render_project(
    project: &super::load::Project,
    b: Backend,
) -> Result<Vec<(usize, Vec<Unit>)>, LoadError> {
    let mut out = Vec::new();
    let mut registry: Option<State> = None;
    for (&target, idx) in &project.order {
        let mut state = State::default();
        for &i in idx {
            let f = &project.files[i];
            let ctx = Ctx {
                target,
                registry: registry.as_ref(),
            };
            let (units, after) = render_file(&state, &f.file, &f.at, &ctx, b)?;
            out.push((i, units));
            state = after;
        }
        if target == super::state::Target::Registry {
            registry = Some(state);
        }
    }
    Ok(out)
}

struct Render<'a> {
    before: &'a State,
    after: &'a State,
    d: &'static dyn Dialect,
    b: Backend,
    /// Tables this file created before the op: no online recipe for them.
    created: &'a BTreeSet<String>,
    at: &'a str,
}

fn q(d: &dyn Dialect, s: &str) -> String {
    d.quote_ident(s)
}

fn list(d: &dyn Dialect, cols: &[String]) -> String {
    cols.iter().map(|c| q(d, c)).collect::<Vec<_>>().join(", ")
}

fn tgt(t: &str) -> TableTarget<'_> {
    TableTarget::in_schema(None, t)
}

fn tx(sql: Vec<String>) -> Vec<Unit> {
    if sql.is_empty() {
        return Vec::new();
    }
    vec![Unit {
        op: 0,
        kind: UnitKind::Tx,
        sql,
        probe: None,
    }]
}

fn auto(sql: String, probe: Vec<Expect>) -> Unit {
    Unit {
        op: 0,
        kind: UnitKind::Autocommit,
        sql: vec![sql],
        probe: Some(probe),
    }
}

fn snapshot(c: &ColumnDef) -> FieldSnapshot {
    FieldSnapshot {
        name: c.column.clone(),
        column: c.column.clone(),
        ty: c.ty.clone(),
        nullable: c.nullable,
        primary_key: c.primary_key,
        max_length: c.max_length,
        min: None,
        max: None,
        default: c.default.clone(),
        auto: c.auto,
        unique: false,
        case_insensitive: c.case_insensitive,
        generated_as: c.generated_as.clone(),
        db_comment: c.comment.clone(),
        fk: None,
    }
}

fn table_snapshot(t: &TableState) -> TableSnapshot {
    let mut snap: TableSnapshot = serde_json::from_value(serde_json::json!({
        "name": t.name, "model": "", "fields": [],
    }))
    .expect("a minimal TableSnapshot");
    snap.fields = t.columns.iter().map(snapshot).collect();
    snap.db_table_comment.clone_from(&t.comment);
    snap
}

fn index_snapshot(table: &str, i: &IndexDef) -> IndexSnapshot {
    serde_json::from_value(serde_json::json!({
        "name": i.name, "table": table, "columns": i.columns, "unique": i.unique,
        "method": i.method, "where_clause": i.predicate, "include": i.include,
    }))
    .expect("an IndexSnapshot")
}

/// The column as `CREATE TABLE`/`ADD COLUMN` writes it, default as written.
fn col_def(d: &dyn Dialect, c: &ColumnDef) -> String {
    diff::column_definition(&snapshot(c), d, DefaultText::AsWritten)
}

fn on_delete(o: OnDelete) -> Option<&'static str> {
    match o {
        OnDelete::NoAction => None,
        OnDelete::Restrict => Some("RESTRICT"),
        OnDelete::Cascade => Some("CASCADE"),
        OnDelete::SetNull => Some("SET NULL"),
        OnDelete::SetDefault => Some("SET DEFAULT"),
    }
}

fn fk_clause(d: &dyn Dialect, fk: &FkDef) -> String {
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        q(d, &fk.name),
        list(d, &fk.columns),
        q(d, &fk.to),
        list(d, &fk.on)
    );
    if let Some(a) = on_delete(fk.on_delete) {
        s.push_str(" ON DELETE ");
        s.push_str(a);
    }
    s
}

fn skipped(ix: &IndexDef, b: Backend) -> bool {
    ix.skipped_on.contains(&b)
}

fn is_serial_pk(d: &dyn Dialect, c: &ColumnDef) -> bool {
    c.auto
        && c.primary_key
        && crate::migrate::snapshot::is_integer_ty(&c.ty)
        && d.serial_type_includes_primary_key()
}

/// `CREATE [UNIQUE] INDEX` on PG/SQLite. No `IF NOT EXISTS`: it would keep
/// an invalid or different index of the same name.
fn create_index(d: &dyn Dialect, table: &str, ix: &IndexDef, concurrently: bool) -> String {
    let mut s = format!(
        "CREATE {}INDEX {}{} ON {}{} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        if concurrently { "CONCURRENTLY " } else { "" },
        q(d, &ix.name),
        q(d, table),
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

fn mysql_index_clause(d: &dyn Dialect, ix: &IndexDef) -> String {
    format!(
        "{}INDEX {} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        q(d, &ix.name),
        list(d, &ix.columns)
    )
}

/// `CREATE TABLE <name>` from state: columns, PK, checks, named FKs; on
/// MySQL also the indexes, so the op stays one statement.
fn create_table_sql(d: &dyn Dialect, t: &TableState, name: &str, inline: &[&IndexDef]) -> String {
    let mut parts: Vec<String> = t.columns.iter().map(|c| col_def(d, c)).collect();
    let pk: Vec<String> = t.primary_key().map(str::to_owned).collect();
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
        parts.push(mysql_index_clause(d, ix));
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

impl Render<'_> {
    fn err(&self, code: Code, msg: impl Into<String>) -> LoadError {
        LoadError::new(code, self.at, msg)
    }

    fn table(&self, s: &State, t: &str) -> Result<Arc<TableState>, LoadError> {
        s.tables
            .get(t)
            .cloned()
            .ok_or_else(|| self.err(Code::State, format!("table `{t}` is not in state")))
    }

    fn existed(&self, t: &str) -> bool {
        !self.created.contains(t)
    }

    /// One unit per statement on MySQL (each DDL commits), one tx elsewhere.
    /// `probe` is the state after the op, given to the last MySQL unit.
    fn ddl(&self, sql: Vec<String>, probe: Vec<Expect>) -> Vec<Unit> {
        if self.b != Backend::Mysql {
            return tx(sql);
        }
        let n = sql.len();
        sql.into_iter()
            .enumerate()
            .map(|(i, s)| Unit {
                op: 0,
                kind: UnitKind::Autocommit,
                sql: vec![s],
                probe: (i + 1 == n).then(|| probe.clone()),
            })
            .collect()
    }

    /// What the backend cannot build of `ix`, as a load error.
    fn index_supported(&self, ix: &IndexDef) -> Result<(), LoadError> {
        if skipped(ix, self.b) {
            return Ok(());
        }
        let btree = matches!(ix.method.as_str(), "" | "btree");
        let bad = match self.b {
            Backend::Postgres => None,
            Backend::Mysql if ix.predicate.is_some() => Some("a partial index"),
            _ if !ix.include.is_empty() => Some("INCLUDE"),
            _ if !btree => Some("a non-btree method"),
            _ => None,
        };
        match bad {
            Some(what) => Err(self.err(
                Code::Dialect,
                format!(
                    "index `{}` uses {what}; add `skipped_on` for {}",
                    ix.name,
                    self.b.as_str()
                ),
            )),
            None => Ok(()),
        }
    }

    /// The SQLite rebuild of `t`: new table, copy, swap, then its indexes.
    fn rebuild(&self, t: &TableState, copy: &[(String, String)]) -> Vec<Unit> {
        let d = self.d;
        let new = format!("_rustango_rebuild_{}", t.name);
        let mut rb = TableRebuild::new(&table_snapshot(t), Vec::new());
        for (c, e) in copy {
            rb = rb.copy_as(c, e.clone());
        }
        let mut sql = vec![create_table_sql(d, t, &new, &[])];
        sql.extend(rb.copy_and_swap(d, &new));
        for ix in t.indexes.iter().filter(|i| !skipped(i, self.b)) {
            sql.push(create_index(d, &t.name, ix, false));
        }
        vec![Unit {
            op: 0,
            kind: UnitKind::Rebuild,
            sql,
            probe: None,
        }]
    }

    /// PG on an existing table: `NOT VALID`, then `VALIDATE` in its own unit,
    /// so the scan holds only a SHARE UPDATE EXCLUSIVE lock.
    fn validate_later(&self, table: &str, name: &str, add: String) -> Vec<Unit> {
        let mut out = tx(vec![format!("{add} NOT VALID")]);
        out.extend(tx(vec![format!(
            "ALTER TABLE {} VALIDATE CONSTRAINT {}",
            q(self.d, table),
            q(self.d, name)
        )]));
        out
    }

    fn column_expect(&self, table: &str, column: &str) -> Vec<Expect> {
        self.after
            .tables
            .get(table)
            .and_then(|t| t.column(column))
            .map(|c| {
                vec![Expect::Column {
                    table: table.to_owned(),
                    column: c.clone(),
                }]
            })
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_lines)]
    fn op(&self, op: &Op, op_tx: Option<OpTx>) -> Result<Vec<Unit>, LoadError> {
        let d = self.d;
        let (pg, my, lite) = (
            self.b == Backend::Postgres,
            self.b == Backend::Mysql,
            self.b == Backend::Sqlite,
        );
        Ok(match op {
            Op::CreateTable { table, indexes, .. } => {
                for ix in indexes {
                    self.index_supported(ix)?;
                }
                let ts = self.table(self.after, &table.name)?;
                if my {
                    let inline: Vec<&IndexDef> =
                        ts.indexes.iter().filter(|i| !skipped(i, self.b)).collect();
                    vec![auto(
                        create_table_sql(d, &ts, &ts.name, &inline),
                        vec![Expect::Table(Arc::clone(&ts))],
                    )]
                } else {
                    let mut sql = vec![create_table_sql(d, &ts, &ts.name, &[])];
                    if let Some(c) = &ts.comment {
                        sql.extend(d.table_comment_statement(tgt(&ts.name), c));
                    }
                    for c in &ts.columns {
                        if let Some(cm) = &c.comment {
                            sql.extend(d.column_comment_statement(tgt(&ts.name), &c.column, cm));
                        }
                    }
                    for ix in ts.indexes.iter().filter(|i| !skipped(i, self.b)) {
                        sql.push(create_index(d, &ts.name, ix, false));
                    }
                    tx(sql)
                }
            }
            // PG without CASCADE: a missed FK fails loudly.
            Op::DropTable { table, .. } => self.ddl(
                vec![format!("DROP TABLE {}", q(d, table))],
                vec![Expect::NoTable(table.clone())],
            ),
            Op::RenameTable { from, to } => {
                let mut sql = vec![format!("ALTER TABLE {} RENAME TO {}", q(d, from), q(d, to))];
                if pg {
                    // The owned sequence follows; the pkey keeps its stored name.
                    let old = self.table(self.before, from)?;
                    let new = self.table(self.after, to)?;
                    if let (Some(a), Some(b)) = (&old.pg_sequence, &new.pg_sequence) {
                        if a != b {
                            sql.push(format!("ALTER SEQUENCE {} RENAME TO {}", q(d, a), q(d, b)));
                        }
                    }
                }
                let probe = vec![
                    Expect::Table(self.table(self.after, to)?),
                    Expect::NoTable(from.clone()),
                ];
                self.ddl(sql, probe)
            }
            Op::AlterTableComment { table, to, .. } => self.ddl(
                d.alter_table_comment_statement(tgt(table), to.as_deref().unwrap_or(""))
                    .into_iter()
                    .collect(),
                vec![Expect::Table(self.table(self.after, table)?)],
            ),
            Op::AddColumn {
                table,
                column,
                fill,
            } => self.add_column(table, &column.column, fill.as_deref())?,
            Op::DropColumn { table, column, .. } => self.ddl(
                vec![format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    q(d, table),
                    q(d, column)
                )],
                vec![Expect::NoColumn {
                    table: table.clone(),
                    column: column.clone(),
                }],
            ),
            Op::RenameColumn { table, from, to } => {
                let mut probe = self.column_expect(table, to);
                probe.push(Expect::NoColumn {
                    table: table.clone(),
                    column: from.clone(),
                });
                self.ddl(
                    vec![format!(
                        "ALTER TABLE {} RENAME COLUMN {} TO {}",
                        q(d, table),
                        q(d, from),
                        q(d, to)
                    )],
                    probe,
                )
            }
            Op::AlterColumn {
                table,
                column,
                using,
                using_down,
                ..
            } => {
                if using.is_some() || using_down.is_some() {
                    return Err(self.err(
                        Code::NotEnabled,
                        "`using` and `using_down` are not enabled in this release",
                    ));
                }
                self.alter_column(table, column)?
            }
            Op::AddIndex {
                table,
                index,
                concurrently,
            } => {
                self.index_supported(index)?;
                if skipped(index, self.b) {
                    return Ok(Vec::new());
                }
                let probe = vec![Expect::Index {
                    table: table.clone(),
                    index: index.clone(),
                }];
                if my {
                    let mut s = format!(
                        "ALTER TABLE {} ADD {}",
                        q(d, table),
                        mysql_index_clause(d, index)
                    );
                    if self.existed(table) {
                        // Fail fast rather than fall back to a copying, locking ALTER.
                        s.push_str(", ALGORITHM=INPLACE, LOCK=NONE");
                    }
                    return Ok(vec![auto(s, probe)]);
                }
                let conc = pg && concurrently.unwrap_or_else(|| self.existed(table));
                let sql = create_index(d, table, index, conc);
                if conc {
                    vec![auto(sql, probe)]
                } else {
                    tx(vec![sql])
                }
            }
            Op::DropIndex {
                table,
                name,
                was,
                concurrently,
            } => {
                if skipped(was, self.b) {
                    return Ok(Vec::new());
                }
                let probe = vec![Expect::NoIndex {
                    table: table.clone(),
                    name: name.clone(),
                }];
                if my {
                    return Ok(vec![auto(
                        format!("ALTER TABLE {} DROP INDEX {}", q(d, table), q(d, name)),
                        probe,
                    )]);
                }
                let conc = pg && concurrently.unwrap_or_else(|| self.existed(table));
                let sql = format!(
                    "DROP INDEX {}{}",
                    if conc { "CONCURRENTLY " } else { "" },
                    q(d, name)
                );
                if conc {
                    vec![auto(sql, probe)]
                } else {
                    tx(vec![sql])
                }
            }
            Op::RenameIndex { table, from, to } => {
                let t = self.table(self.after, table)?;
                let ix = t
                    .indexes
                    .iter()
                    .find(|i| &i.name == to)
                    .ok_or_else(|| self.err(Code::State, format!("index `{to}` missing")))?;
                if skipped(ix, self.b) {
                    return Ok(Vec::new());
                }
                match self.b {
                    Backend::Postgres => tx(vec![format!(
                        "ALTER INDEX {} RENAME TO {}",
                        q(d, from),
                        q(d, to)
                    )]),
                    Backend::Mysql => vec![auto(
                        format!(
                            "ALTER TABLE {} RENAME INDEX {} TO {}",
                            q(d, table),
                            q(d, from),
                            q(d, to)
                        ),
                        vec![
                            Expect::Index {
                                table: table.clone(),
                                index: ix.clone(),
                            },
                            Expect::NoIndex {
                                table: table.clone(),
                                name: from.clone(),
                            },
                        ],
                    )],
                    // No RENAME INDEX: drop and re-create from state.
                    Backend::Sqlite => tx(vec![
                        format!("DROP INDEX {}", q(d, from)),
                        create_index(d, table, ix, false),
                    ]),
                }
            }
            Op::AddCheck { table, name, expr } => {
                if lite {
                    return Ok(self.rebuild(&*self.table(self.after, table)?, &[]));
                }
                let add = format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({expr})",
                    q(d, table),
                    q(d, name)
                );
                if pg && self.existed(table) {
                    self.validate_later(table, name, add)
                } else {
                    self.ddl(
                        vec![add],
                        vec![Expect::Check {
                            table: table.clone(),
                            check: CheckDef {
                                name: name.clone(),
                                expr: expr.clone(),
                            },
                        }],
                    )
                }
            }
            Op::DropCheck { table, name, .. } => {
                if lite {
                    return Ok(self.rebuild(&*self.table(self.after, table)?, &[]));
                }
                self.ddl(
                    vec![self.drop_constraint(table, name, "CHECK")],
                    vec![Expect::NoCheck {
                        table: table.clone(),
                        name: name.clone(),
                    }],
                )
            }
            Op::AddFk { table, column, fk } => {
                let t = self.table(self.after, table)?;
                if lite {
                    return Ok(self.rebuild(&t, &[]));
                }
                let state_fk = fk.state(column);
                let add = format!(
                    "ALTER TABLE {} ADD {}",
                    q(d, table),
                    fk_clause(d, &state_fk)
                );
                if pg && self.existed(table) {
                    self.validate_later(table, &fk.name, add)
                } else {
                    self.ddl(
                        vec![add],
                        vec![Expect::Fk {
                            table: table.clone(),
                            fk: state_fk,
                        }],
                    )
                }
            }
            Op::DropFk { table, was, .. } => {
                if lite {
                    return Ok(self.rebuild(&*self.table(self.after, table)?, &[]));
                }
                self.ddl(
                    vec![self.drop_constraint(table, &was.name, "FOREIGN KEY")],
                    vec![Expect::NoFk {
                        table: table.clone(),
                        name: was.name.clone(),
                    }],
                )
            }
            Op::RawSql { up, probe, .. } => {
                let Some(sql) = up.get(self.b) else {
                    return Ok(Vec::new());
                };
                let autocommit = op_tx == Some(OpTx::Autocommit);
                let stmt = self.raw_statement(&sql.0, autocommit)?;
                if autocommit {
                    vec![Unit {
                        op: 0,
                        kind: UnitKind::Autocommit,
                        sql: vec![stmt],
                        probe: self.raw_probe(op, probe.as_ref())?,
                    }]
                } else {
                    tx(vec![stmt])
                }
            }
            Op::StateOnly { .. } | Op::ExternTable { .. } | Op::AdoptTable { .. } => Vec::new(),
            Op::CreateExtension {
                name,
                schema,
                skipped_on,
            } => {
                if skipped_on.contains(&self.b) {
                    return Ok(Vec::new());
                }
                if !pg {
                    return Err(self.err(
                        Code::Dialect,
                        format!(
                            "create_extension `{name}` needs `skipped_on` for {}",
                            self.b.as_str()
                        ),
                    ));
                }
                // The one allowed IF NOT EXISTS: provisioning may have installed it.
                let mut s = format!("CREATE EXTENSION IF NOT EXISTS {}", q(d, name));
                if let Some(sc) = schema {
                    s.push_str(&format!(" SCHEMA {}", q(d, sc)));
                }
                tx(vec![s])
            }
        })
    }

    /// `ALTER TABLE .. DROP CONSTRAINT`, MySQL's `DROP CHECK`/`DROP FOREIGN
    /// KEY`. Never `IF EXISTS`: a missing constraint means state is wrong.
    fn drop_constraint(&self, table: &str, name: &str, mysql_kind: &str) -> String {
        let what = if self.b == Backend::Mysql {
            mysql_kind
        } else {
            "CONSTRAINT"
        };
        format!(
            "ALTER TABLE {} DROP {what} {}",
            q(self.d, table),
            q(self.d, name)
        )
    }

    /// One `raw_sql` statement with templates expanded. Session statements
    /// are refused in a transactional unit, `PRAGMA` everywhere, and MySQL DDL
    /// unless the op is `tx: "autocommit"` (MySQL commits it anyway).
    fn raw_statement(&self, text: &str, autocommit: bool) -> Result<String, LoadError> {
        let s = self.expand(text.trim())?;
        let head = s.trim_start().to_ascii_uppercase();
        if head.starts_with("PRAGMA") {
            return Err(self.err(Code::Raw, "`PRAGMA` is not allowed in raw_sql"));
        }
        if !autocommit && SESSION_STMTS.iter().any(|p| head.starts_with(p)) {
            return Err(self.err(
                Code::Raw,
                format!("`{s}` is not allowed in a transactional unit"),
            ));
        }
        if !autocommit && self.b == Backend::Mysql && MYSQL_DDL.iter().any(|p| head.starts_with(p))
        {
            return Err(self.err(
                Code::Raw,
                format!(
                    "MySQL commits `{s}` on its own: mark the op `tx: \"autocommit\"` with a probe"
                ),
            ));
        }
        Ok(s)
    }

    /// The probe after a raw autocommit op: the declared state, the file's
    /// SQL, or idempotent.
    fn raw_probe(
        &self,
        op: &Op,
        probe: Option<&FileProbe>,
    ) -> Result<Option<Vec<Expect>>, LoadError> {
        Ok(match probe {
            None => None,
            Some(FileProbe::Named(ProbeKind::State)) => Some(self.declared(op)?),
            Some(FileProbe::Spec(spec)) if spec.idempotent => Some(vec![Expect::Idempotent]),
            Some(FileProbe::Spec(spec)) => spec
                .sql
                .as_ref()
                .and_then(|p| p.get(self.b))
                .map(|s| vec![Expect::Sql(s.0.clone())]),
        })
    }

    /// Expects for a raw op's declared state ops, read from the after-state.
    fn declared(&self, op: &Op) -> Result<Vec<Expect>, LoadError> {
        let mut out = Vec::new();
        for nested in op.nested(self.at)? {
            match &nested {
                Op::AddIndex { table, index, .. } if !skipped(index, self.b) => {
                    out.push(Expect::Index {
                        table: table.clone(),
                        index: index.clone(),
                    });
                }
                Op::AddIndex { .. } => {}
                Op::DropIndex { table, name, .. } => out.push(Expect::NoIndex {
                    table: table.clone(),
                    name: name.clone(),
                }),
                Op::CreateTable { table, .. } => {
                    out.push(Expect::Table(self.table(self.after, &table.name)?));
                }
                Op::DropTable { table, .. } => out.push(Expect::NoTable(table.clone())),
                Op::AddColumn { table, column, .. } => {
                    out.extend(self.column_expect(table, &column.column.column));
                }
                Op::DropColumn { table, column, .. } => out.push(Expect::NoColumn {
                    table: table.clone(),
                    column: column.clone(),
                }),
                Op::AddCheck { table, name, expr } => out.push(Expect::Check {
                    table: table.clone(),
                    check: CheckDef {
                        name: name.clone(),
                        expr: expr.clone(),
                    },
                }),
                Op::DropCheck { table, name, .. } => out.push(Expect::NoCheck {
                    table: table.clone(),
                    name: name.clone(),
                }),
                Op::AddFk { table, column, fk } => out.push(Expect::Fk {
                    table: table.clone(),
                    fk: fk.state(column),
                }),
                Op::DropFk { table, was, .. } => out.push(Expect::NoFk {
                    table: table.clone(),
                    name: was.name.clone(),
                }),
                other => {
                    // Anything else: the whole table it names, from the after-state.
                    if let Some(t) = touched_table(other).and_then(|t| self.after.tables.get(t)) {
                        out.push(Expect::Table(Arc::clone(t)));
                    }
                }
            }
        }
        Ok(out)
    }

    /// `{{t:name}}` and `{{c:name}}` quoted for the backend; a table must be in state.
    fn expand(&self, sql: &str) -> Result<String, LoadError> {
        let mut out = String::new();
        let mut rest = sql;
        while let Some(at) = rest.find("{{") {
            out.push_str(&rest[..at]);
            let end = rest[at..]
                .find("}}")
                .ok_or_else(|| self.err(Code::Raw, "unclosed `{{`"))?
                + at;
            let tag = &rest[at + 2..end];
            match tag.split_once(':') {
                Some(("t", name)) => {
                    if !self.before.tables.contains_key(name)
                        && !self.after.tables.contains_key(name)
                    {
                        return Err(
                            self.err(Code::State, format!("`{{{{t:{name}}}}}` is not in state"))
                        );
                    }
                    out.push_str(&q(self.d, name));
                }
                Some(("c", name)) => out.push_str(&q(self.d, name)),
                _ => return Err(self.err(Code::Raw, format!("unknown template `{tag}`"))),
            }
            rest = &rest[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn add_column(
        &self,
        table: &str,
        column: &ColumnDef,
        fill: Option<&str>,
    ) -> Result<Vec<Unit>, LoadError> {
        let d = self.d;
        if column.min.is_some() || column.max.is_some() {
            return Err(self.err(
                Code::Unsupported,
                "`min`/`max` are checks: write an add_check op instead",
            ));
        }
        let not_null_no_default =
            !column.nullable && column.default.is_none() && column.generated_as.is_none();
        if fill.is_some() && !not_null_no_default {
            return Err(self.err(
                Code::Fill,
                "`fill` is only for a NOT NULL column with no default",
            ));
        }
        let t = q(d, table);
        let c = q(d, &column.column);
        let after = self.table(self.after, table)?;
        match self.b {
            Backend::Sqlite => {
                let paren_default = column.default.as_deref().is_some_and(|x| {
                    let x = x.trim();
                    x.starts_with('(') || x.to_ascii_uppercase().starts_with("CURRENT_")
                });
                if column.generated_as.is_some() || paren_default || not_null_no_default {
                    let value = fill
                        .map(str::to_owned)
                        .or_else(|| column.default.clone())
                        .unwrap_or_else(|| "NULL".into());
                    let copy = if column.generated_as.is_some() {
                        vec![]
                    } else {
                        vec![(column.column.clone(), value)]
                    };
                    return Ok(self.rebuild(&after, &copy));
                }
                Ok(tx(vec![format!(
                    "ALTER TABLE {t} ADD COLUMN {}",
                    col_def(d, column)
                )]))
            }
            Backend::Mysql => {
                let probe = self.column_expect(table, &column.column);
                let Some(f) = fill else {
                    return Ok(vec![auto(
                        format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, column)),
                        probe,
                    )]);
                };
                // Three units: ADD and a DEFAULT drop in one ALTER is error 1054.
                let mut nullable = column.clone();
                nullable.nullable = true;
                Ok(vec![
                    auto(
                        format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, &nullable)),
                        vec![Expect::Column {
                            table: table.to_owned(),
                            column: nullable,
                        }],
                    ),
                    Unit {
                        op: 0,
                        kind: UnitKind::Tx,
                        sql: vec![format!("UPDATE {t} SET {c} = {f}")],
                        probe: None,
                    },
                    auto(
                        format!("ALTER TABLE {t} MODIFY COLUMN {}", col_def(d, column)),
                        probe,
                    ),
                ])
            }
            Backend::Postgres => {
                let mut sql = Vec::new();
                match fill {
                    // Metadata only on PG 11+: the default fills existing rows.
                    Some(f) => {
                        let mut with = column.clone();
                        with.default = Some(f.to_owned());
                        sql.push(format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, &with)));
                        sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP DEFAULT"));
                    }
                    None => sql.push(format!("ALTER TABLE {t} ADD COLUMN {}", col_def(d, column))),
                }
                if let Some(cm) = &column.comment {
                    sql.extend(d.column_comment_statement(tgt(table), &column.column, cm));
                }
                Ok(tx(sql))
            }
        }
    }

    /// `alter_column`: v1's `push_alter_changes` splits it; PG renders each
    /// change through the v1 arms, MySQL restates the column once, SQLite
    /// rebuilds.
    fn alter_column(&self, table: &str, column: &str) -> Result<Vec<Unit>, LoadError> {
        let d = self.d;
        let from = self.table(self.before, table)?;
        let after_t = self.table(self.after, table)?;
        let (Some(from), Some(to)) = (from.column(column), after_t.column(column)) else {
            return Err(self.err(Code::State, format!("column `{table}.{column}` missing")));
        };
        if to.min.is_some() || to.max.is_some() {
            return Err(self.err(
                Code::Unsupported,
                "`min`/`max` are checks: write add_check/drop_check ops instead",
            ));
        }
        let mut changes = Vec::new();
        diff::push_alter_changes(table, &snapshot(from), &snapshot(to), &mut changes);
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        match self.b {
            Backend::Mysql => Ok(vec![auto(
                format!(
                    "ALTER TABLE {} MODIFY COLUMN {}",
                    q(d, table),
                    col_def(d, to)
                ),
                vec![Expect::Column {
                    table: table.to_owned(),
                    column: to.clone(),
                }],
            )]),
            Backend::Postgres => {
                // NULL-ness is v2's: v1 fills NULLs with an UPDATE and never goes online.
                let (null, rest): (Vec<_>, Vec<_>) = changes
                    .into_iter()
                    .partition(|c| matches!(c, SchemaChange::AlterColumnNullable { .. }));
                let lowered = SchemaSnapshot {
                    tables: vec![table_snapshot(&after_t)],
                    indexes: after_t
                        .indexes
                        .iter()
                        .map(|i| index_snapshot(table, i))
                        .collect(),
                    ..SchemaSnapshot::default()
                };
                let batch = diff::render_changes_split_with_dialect(&rest, &lowered, d)
                    .map_err(|m| self.err(Code::State, m))?;
                let mut sql = batch.immediate;
                let mut out = Vec::new();
                let t = q(d, table);
                let c = q(d, column);
                match null.first() {
                    Some(SchemaChange::AlterColumnNullable { nullable: true, .. }) => {
                        sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} DROP NOT NULL"));
                    }
                    Some(_) if !self.existed(table) => {
                        sql.push(format!("ALTER TABLE {t} ALTER COLUMN {c} SET NOT NULL"));
                    }
                    Some(_) => {
                        let name = self.not_null_check(table, column)?;
                        let nn = q(d, &name);
                        out.extend(tx(std::mem::take(&mut sql)));
                        out.extend(self.validate_later(
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
                out.extend(tx(sql));
                Ok(out)
            }
            Backend::Sqlite => {
                // VARCHAR(n) is TEXT there and has no comments: nothing to write.
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
                Ok(self.rebuild(&after_t, &[]))
            }
        }
    }

    /// The temporary CHECK of PG's online NOT NULL: `<t>_<c>_v2nn`, which
    /// must fit 63 bytes and be free in state.
    fn not_null_check(&self, table: &str, column: &str) -> Result<String, LoadError> {
        let name = format!("{table}_{column}_v2nn");
        if name.len() > MAX_IDENT {
            return Err(self.err(
                Code::Identifier,
                format!("the temporary check `{name}` is longer than {MAX_IDENT} bytes"),
            ));
        }
        if super::replay::taken(self.before, &name) {
            return Err(self.err(Code::State, format!("the name `{name}` is taken in state")));
        }
        Ok(name)
    }
}

/// The table an op names, for a declared-state probe.
fn touched_table(op: &Op) -> Option<&str> {
    match op {
        Op::RenameTable { to, .. } => Some(to),
        Op::AlterTableComment { table, .. }
        | Op::RenameColumn { table, .. }
        | Op::AlterColumn { table, .. }
        | Op::RenameIndex { table, .. } => Some(table),
        Op::AdoptTable { table, .. } => Some(&table.name),
        _ => None,
    }
}

/// Units as golden text.
pub(crate) fn golden(units: &[Unit]) -> String {
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
