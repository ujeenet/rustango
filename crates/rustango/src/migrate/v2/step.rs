//! Data steps' runtime (5.2-5.5): the step context on the unit's
//! transaction, the `#[data_step]` registry and the historical tables.
//!
//! A `HistTable` is a table at one state point. It compiles through the
//! dialect IR, one `dialect.compile_*` statement per call: never a
//! `QuerySet`, a delete plan, relation spans or audit.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use futures_util::future::BoxFuture;
use sha2::{Digest, Sha256};

use super::format::Footprint;
use super::state::TableState;
use crate::core::{
    Assignment, BulkInsertQuery, ConflictClause, CountQuery, DeleteQuery, FieldSchema, FieldType,
    Filter, InsertQuery, ModelSchema, Op, OrderClause, OrderItem, SelectQuery, SqlValue,
    UpdateQuery, WhereExpr,
};
use crate::migrate::snapshot::{field_type_from_name, TableSnapshot};
use crate::sql::{CompiledStatement, Dialect, ExecError, PoolTx};

/// What a step's future resolves to.
pub type StepResult = Result<Progress, StepError>;
/// A step's boxed future.
pub type StepFuture<'a> = BoxFuture<'a, StepResult>;
/// The shim `#[data_step]` registers for `up` and `down`.
pub type StepFn = for<'a, 'b> fn(&'a mut StepCtx<'b>, Batch) -> StepFuture<'a>;

/// The rows a step call covers: keys above [`Batch::lo`], at most
/// [`Batch::size`] of them. Unbatched steps get the whole table.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Batch {
    lo: Option<SqlValue>,
    size: u64,
}

impl Batch {
    pub(crate) fn whole() -> Self {
        Self {
            lo: None,
            size: u64::MAX,
        }
    }

    pub(crate) fn range(lo: Option<SqlValue>, size: u64) -> Self {
        Self { lo, size }
    }

    /// The last key the previous batch covered; `None` on the first.
    #[must_use]
    pub fn lo(&self) -> Option<&SqlValue> {
        self.lo.as_ref()
    }

    /// Rows this batch may cover.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The step is done.
    #[must_use]
    pub fn done(&self) -> Progress {
        Progress::Done
    }

    /// Next batch starts above `last`, the last key this one covered; `None`
    /// (no row) means done. The runner refuses a key that does not advance.
    #[must_use]
    pub fn advance_to(&self, last: Option<SqlValue>) -> Progress {
        last.map_or(Progress::Done, Progress::Next)
    }
}

/// How far a step got.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Progress {
    Done,
    /// The last key covered; the next batch starts above it.
    Next(SqlValue),
}

/// Why a step failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StepError {
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error(transparent)]
    Hist(#[from] HistError),
    #[error("{0}")]
    Message(String),
}

impl From<crate::sql::SqlError> for StepError {
    fn from(e: crate::sql::SqlError) -> Self {
        Self::Exec(e.into())
    }
}

/// One `#[data_step]` registration, keyed `(app, id)`.
#[derive(Debug)]
pub struct DataStep {
    pub app: &'static str,
    pub id: &'static str,
    pub type_name: &'static str,
    pub up: StepFn,
    pub down: Option<StepFn>,
    /// The impl's source tokens: the code hash covers them.
    pub code: &'static str,
}

inventory::collect!(DataStep);

impl DataStep {
    /// sha256 of the impl's tokens, hex: stored per step in the ledger.
    #[must_use]
    pub fn code_hash(&self) -> String {
        super::checksum::hex(&Sha256::digest(self.code.as_bytes()))
    }
}

/// Every registration of `(app, id)`.
pub(crate) fn registered(app: &str, id: &str) -> Vec<&'static DataStep> {
    inventory::iter::<DataStep>
        .into_iter()
        .filter(|s| s.app == app && s.id == id)
        .collect()
}

/// Two registrations of one key: a hard error naming both types.
///
/// # Errors
/// Each duplicated key with its types.
pub(crate) fn refuse_duplicates() -> Result<(), String> {
    refuse_duplicates_in(
        inventory::iter::<DataStep>
            .into_iter()
            .map(|s| (s.app, s.id, s.type_name)),
    )
}

/// [`refuse_duplicates`] over `(app, id, type)` registrations.
pub(crate) fn refuse_duplicates_in<'a>(
    steps: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
) -> Result<(), String> {
    let mut seen: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for (app, id, ty) in steps {
        seen.entry((app, id)).or_default().push(ty);
    }
    let dups: Vec<String> = seen
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|((app, id), v)| format!("`{app}:{id}` by {}", v.join(" and ")))
        .collect();
    if dups.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "data steps registered twice: {}; a step id is unique per app",
            dups.join("; ")
        ))
    }
}

/// Registered steps near `id` in `app`, for a "not registered" error.
pub(crate) fn near(app: &str, id: &str) -> Vec<String> {
    let mut out: Vec<String> = inventory::iter::<DataStep>
        .into_iter()
        .filter(|s| (s.app == app && distance(s.id, id) <= 3) || (s.app != app && s.id == id))
        .map(|s| format!("{}:{}", s.app, s.id))
        .collect();
    out.sort();
    out
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// What a step may touch: its footprint and the tables at its op.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub(crate) app: String,
    pub(crate) footprint: Footprint,
    pub(crate) tables: BTreeMap<String, TableState>,
}

/// What a data step runs on: the unit's transaction on the runner's own
/// connection. Any failed statement poisons the unit (5.4).
pub struct StepCtx<'t> {
    pub(crate) tx: PoolTx<'t>,
    pub(crate) poisoned: bool,
    /// The statement that poisoned the unit was a deadlock or lock timeout.
    pub(crate) retryable: bool,
    pub(crate) scope: Scope,
    /// PG tables whose auto pk sequence is reset at the end of the unit.
    pub(crate) resets: BTreeSet<(String, &'static str)>,
}

impl<'t> StepCtx<'t> {
    pub(crate) fn new(tx: PoolTx<'t>, scope: Scope) -> Self {
        Self {
            tx,
            poisoned: false,
            retryable: false,
            scope,
            resets: BTreeSet::new(),
        }
    }

    /// The backend's dialect.
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        self.tx.dialect()
    }

    /// An opaque label of the target, for logs only.
    #[must_use]
    pub fn target_label(&self) -> String {
        format!("{}@{}", self.scope.app, self.dialect().name())
    }

    /// `table` as the state holds it at this op.
    ///
    /// # Errors
    /// A table outside the op's `tables` footprint, or not in state.
    pub fn table(&self, table: &str) -> Result<HistTable, HistError> {
        if !self.scope.footprint.tables().any(|t| t == table) {
            return Err(HistError::Footprint(format!(
                "`{table}` is not in the op's `tables`"
            )));
        }
        let t =
            self.scope.tables.get(table).ok_or_else(|| {
                HistError::Footprint(format!("`{table}` is not in state at this op"))
            })?;
        let snap = super::render::table_snapshot(t);
        let mut h = HistTable::from_snapshot(&self.scope.app, &snap)?;
        h.sealed = t
            .columns
            .iter()
            .filter(|c| c.sealed)
            .map(|c| c.column.clone())
            .collect();
        h.writes = self
            .scope
            .footprint
            .write
            .iter()
            .filter_map(|w| match w.split_once('.') {
                Some((wt, c)) if wt == table => Some(Some(c.to_owned())),
                None if w == table => Some(None),
                _ => None,
            })
            .collect();
        Ok(h)
    }

    /// Run one statement in the unit.
    ///
    /// # Errors
    /// The driver error; the unit is poisoned.
    pub(crate) async fn execute(
        &mut self,
        sql: &str,
        binds: Vec<SqlValue>,
    ) -> Result<u64, ExecError> {
        let r = crate::sql::raw_execute_tx(&mut self.tx, sql, binds).await;
        if let Err(e) = &r {
            if !self.poisoned {
                self.retryable = retryable(e);
            }
            self.poisoned = true;
        }
        r
    }

    async fn run(&mut self, stmt: CompiledStatement) -> Result<u64, HistError> {
        let params = stmt.params().to_vec();
        Ok(self.execute(&stmt.sql, params).await?)
    }

    /// A raw statement: `{{t:}}`, `{{c:}}` and `{{?}}` as in `data_sql`,
    /// against this op's state and footprint. With `columns`, the rows it
    /// returns, decoded strictly as those columns of `table`.
    ///
    /// # Errors
    /// A placeholder outside the footprint, a bad cell, or the driver.
    pub async fn raw(
        &mut self,
        template: &str,
        binds: Vec<SqlValue>,
        columns: Option<(&str, &[&str])>,
    ) -> Result<Vec<HistRow>, HistError> {
        let b = super::state::Backend::of(self.dialect().kind());
        let mut state = super::state::State::default();
        for (n, t) in &self.scope.tables {
            state
                .tables
                .insert(n.as_str().into(), std::sync::Arc::new(t.clone()));
        }
        let sql = super::data::resolve(
            template,
            b,
            &state,
            &self.scope.footprint,
            binds.len(),
            "ctx.raw",
        )
        .map_err(|e| HistError::Footprint(e.to_string()))?;
        let Some((table, cols)) = columns else {
            self.execute(&sql, binds).await?;
            return Ok(Vec::new());
        };
        let t = self.table(table)?;
        let fields: Vec<&'static FieldSchema> =
            cols.iter().map(|c| t.field(c)).collect::<Result<_, _>>()?;
        let rows = crate::sql::raw_rows_as_json_strict_tx(&mut self.tx, &sql, binds, &fields)
            .await
            .map_err(|e| {
                self.poisoned = true;
                bad_cell(&fields, e)
            })?;
        decode_rows(&fields, rows)
    }

    /// Run `f` under a savepoint: an error inside rolls back to it and
    /// does not poison the unit.
    ///
    /// # Errors
    /// `f`'s error, after the rollback to the savepoint.
    pub async fn savepoint<T, E, F>(&mut self, f: F) -> Result<T, E>
    where
        E: From<ExecError>,
        F: for<'a> FnOnce(&'a mut StepCtx<'t>) -> BoxFuture<'a, Result<T, E>>,
    {
        let name = self.tx.next_savepoint("rgo_step");
        let was = self.poisoned;
        if let Err(e) = self
            .tx
            .execute_unprepared(&format!("SAVEPOINT {name}"))
            .await
        {
            self.poisoned = true;
            return Err(ExecError::Driver(e).into());
        }
        let r = f(self).await;
        let end = if r.is_ok() {
            format!("RELEASE SAVEPOINT {name}")
        } else {
            format!("ROLLBACK TO SAVEPOINT {name}")
        };
        match self.tx.execute_unprepared(&end).await {
            Ok(()) => self.poisoned = was || (r.is_ok() && self.poisoned),
            Err(_) => self.poisoned = true,
        }
        r
    }

    /// PG: move each reset table's sequence past its largest key.
    pub(crate) async fn reset_sequences(&mut self) -> Result<(), ExecError> {
        if self.dialect().kind() != crate::sql::DialectKind::Postgres {
            return Ok(());
        }
        let d = self.dialect();
        for (t, c) in std::mem::take(&mut self.resets) {
            let sql = format!(
                "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE((SELECT MAX({}) FROM {}), 0) + 1, false)",
                quote_lit(&d.quote_ident(&t)),
                quote_lit(c),
                d.quote_ident(c),
                d.quote_ident(&t),
            );
            self.tx.execute_unprepared(&sql).await?;
        }
        Ok(())
    }
}

fn quote_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A deadlock, serialization failure or lock timeout: the unit is retried.
pub(crate) fn retryable(e: &ExecError) -> bool {
    matches!(e, ExecError::Driver(d) if retryable_driver(d))
}

/// [`retryable`] for a bare driver error.
pub(crate) fn retryable_driver(e: &sqlx::Error) -> bool {
    let sqlx::Error::Database(db) = e else {
        return false;
    };
    let code = db.code().unwrap_or_default();
    let msg = db.message().to_ascii_lowercase();
    // PG: deadlock, serialization, lock_timeout. MySQL: 1213 deadlock
    // (SQLSTATE 40001), 1205 lock wait timeout. SQLite: BUSY, LOCKED.
    matches!(
        &*code,
        "40P01" | "40001" | "55P03" | "5" | "6" | "517" | "262"
    ) || msg.contains("lock wait timeout")
        || msg.contains("deadlock")
        || msg.contains("database is locked")
}

impl UnitError {
    /// Whether running the unit again may succeed.
    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::Poisoned { retryable } => *retryable,
            Self::Step(StepError::Exec(e) | StepError::Hist(HistError::Exec(e))) => retryable(e),
            _ => false,
        }
    }
}

/// Why a unit did not commit.
#[derive(Debug)]
pub(crate) enum UnitError {
    Step(StepError),
    /// A statement failed and the step went on: the unit is rolled back.
    /// `retryable` when that failure was a deadlock or lock timeout.
    Poisoned {
        retryable: bool,
    },
    /// The step panicked: the unit is rolled back, and the row resumable.
    Panicked(String),
}

impl std::fmt::Display for UnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Step(e) => e.fmt(f),
            Self::Poisoned { .. } => f.write_str(
                "a statement in this data step failed, so the unit is rolled back even though \
                 the step went on; use a savepoint to recover on purpose",
            ),
            Self::Panicked(why) => write!(f, "the data step panicked: {why}"),
        }
    }
}

/// Run `step` on `tx` with the escape guard set; a panic is caught. Returns
/// the transaction to commit, or the reason to roll it back.
pub(crate) async fn run_step<'t, T, F>(
    tx: PoolTx<'t>,
    scope: Scope,
    step: F,
) -> (PoolTx<'t>, Result<T, UnitError>)
where
    F: for<'a> FnOnce(&'a mut StepCtx<'t>) -> BoxFuture<'a, Result<T, StepError>>,
{
    use futures_util::FutureExt as _;
    let mut ctx = StepCtx::new(tx, scope);
    let r = std::panic::AssertUnwindSafe(crate::sql::migration_step(step(&mut ctx)))
        .catch_unwind()
        .await;
    let r = match r {
        Err(p) => Err(UnitError::Panicked(
            p.downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default(),
        )),
        Ok(Err(e)) => Err(UnitError::Step(e)),
        Ok(Ok(_)) if ctx.poisoned => Err(UnitError::Poisoned {
            retryable: ctx.retryable,
        }),
        Ok(Ok(v)) => ctx
            .reset_sequences()
            .await
            .map(|()| v)
            .map_err(|e| UnitError::Step(e.into())),
    };
    (ctx.tx, r)
}

// ---- historical tables ----

/// Errors of the historical tables.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistError {
    #[error("{0}")]
    Footprint(String),
    #[error("column `{column}` has unknown snapshot type `{ty}`")]
    UnknownType { column: String, ty: String },
    #[error("table `{table}` has no column `{column}` at this state")]
    UnknownColumn { table: String, column: String },
    #[error("column `{column}` is {expected}; cannot take {got:?} without narrowing")]
    Coerce {
        column: &'static str,
        expected: &'static str,
        got: SqlValue,
    },
    #[error("column `{column}` is NOT NULL with no value and no default")]
    MissingValue { column: &'static str },
    #[error("column `{column}` is generated and cannot be written")]
    Generated { column: &'static str },
    #[error("column `{column}` is sealed: raw writes are refused")]
    Sealed { column: &'static str },
    #[error(
        "column `{column}` is an auto key: an explicit value needs `.reset_sequence()` so the \
         sequence moves past it"
    )]
    ExplicitAuto { column: &'static str },
    #[error("bad cell in `{column}`: {why}")]
    BadCell { column: &'static str, why: String },
    #[error(transparent)]
    Exec(#[from] ExecError),
}

impl From<crate::sql::SqlError> for HistError {
    fn from(e: crate::sql::SqlError) -> Self {
        Self::Exec(e.into())
    }
}

/// Prefix of every interned schema's `name`: never a model's.
pub(crate) const HIST_PREFIX: &str = "__hist:";

static LEAKED: AtomicUsize = AtomicUsize::new(0);

/// Bytes leaked by every historical schema interned so far in this process.
#[must_use]
pub fn leaked_bytes() -> usize {
    LEAKED.load(Ordering::Relaxed)
}

fn leak_str(s: &str) -> &'static str {
    LEAKED.fetch_add(s.len(), Ordering::Relaxed);
    Box::leak(s.to_owned().into_boxed_str())
}

/// DB-relevant form: Rust names stripped, collections sorted.
fn normalise(snap: &TableSnapshot) -> TableSnapshot {
    let mut s = snap.clone();
    s.model = String::new();
    for f in &mut s.fields {
        f.name = String::new();
    }
    s.fields.sort_by(|a, b| a.column.cmp(&b.column));
    s.composite_fks.sort_by(|a, b| a.name.cmp(&b.name));
    s
}

/// The intern key: sha256 over the app label and the normalised snapshot.
fn snapshot_hash(app: &str, snap: &TableSnapshot) -> [u8; 32] {
    let json = serde_json::to_vec(&normalise(snap)).expect("a snapshot always serialises");
    let mut h = Sha256::new();
    h.update(app.as_bytes());
    h.update([0]);
    h.update(&json);
    h.finalize().into()
}

/// One table at one state point.
#[derive(Debug, Clone)]
pub struct HistTable {
    schema: &'static ModelSchema,
    /// `None` = the whole table is writable; `Some(c)` = column `c`.
    writes: Vec<Option<String>>,
    /// Sealed columns: written only through seal/unseal, never raw.
    sealed: Vec<String>,
    reset: bool,
}

impl HistTable {
    fn from_snapshot(app: &str, snap: &TableSnapshot) -> Result<Self, HistError> {
        type Cache = HashMap<[u8; 32], &'static ModelSchema>;
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
        let hash = snapshot_hash(app, snap);
        let mut cache = CACHE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let schema = if let Some(schema) = cache.get(&hash) {
            *schema
        } else {
            let norm = normalise(snap);
            let mut fields = Vec::with_capacity(norm.fields.len());
            for f in &norm.fields {
                let ty = field_type_from_name(&f.ty).ok_or_else(|| HistError::UnknownType {
                    column: f.column.clone(),
                    ty: f.ty.clone(),
                })?;
                let column = leak_str(&f.column);
                // Physical names only: `name` is the column, relations dropped.
                let mut fs = FieldSchema::new(column, column, ty);
                fs.nullable = f.nullable;
                fs.primary_key = f.primary_key;
                fs.max_length = f.max_length;
                fs.default = f.default.as_deref().map(leak_str);
                fs.auto = f.auto;
                fs.unique = f.unique;
                fs.case_insensitive = f.case_insensitive;
                fs.generated_as = f.generated_as.as_deref().map(leak_str);
                fields.push(fs);
            }
            LEAKED.fetch_add(
                std::mem::size_of::<FieldSchema>() * fields.len()
                    + std::mem::size_of::<ModelSchema>(),
                Ordering::Relaxed,
            );
            let name = format!(
                "{HIST_PREFIX}{app}.{}@{}",
                norm.name,
                crate::hex::hex_encode(&hash[..6])
            );
            let mut schema = ModelSchema::new(leak_str(&name), leak_str(&norm.name));
            schema.fields = Box::leak(fields.into_boxed_slice());
            let schema: &'static ModelSchema = Box::leak(Box::new(schema));
            cache.insert(hash, schema);
            schema
        };
        Ok(Self {
            schema,
            writes: Vec::new(),
            sealed: Vec::new(),
            reset: false,
        })
    }

    /// The interned schema, for checks.
    #[must_use]
    pub fn schema(&self) -> &'static ModelSchema {
        self.schema
    }

    /// Allow explicit values in an auto key; on PG the sequence moves past
    /// them at the end of the unit.
    #[must_use]
    pub fn reset_sequence(mut self) -> Self {
        self.reset = true;
        self
    }

    fn field(&self, column: &str) -> Result<&'static FieldSchema, HistError> {
        self.schema
            .fields
            .iter()
            .find(|f| f.column == column)
            .ok_or_else(|| HistError::UnknownColumn {
                table: self.schema.table.to_owned(),
                column: column.to_owned(),
            })
    }

    fn writable(&self, column: Option<&str>) -> Result<(), HistError> {
        let ok = self.writes.iter().any(|w| match (w, column) {
            (None, _) => true,
            (Some(c), Some(col)) => c == col,
            (Some(_), None) => false,
        });
        if ok {
            return Ok(());
        }
        Err(HistError::Footprint(format!(
            "`{}{}` is not in the op's `tables.write`",
            self.schema.table,
            column.map(|c| format!(".{c}")).unwrap_or_default()
        )))
    }

    fn check_write(&self, f: &'static FieldSchema) -> Result<(), HistError> {
        if f.generated_as.is_some() {
            return Err(HistError::Generated { column: f.column });
        }
        if self.sealed.iter().any(|c| c == f.column) {
            return Err(HistError::Sealed { column: f.column });
        }
        Ok(())
    }

    /// A filter/select/update/delete builder on this table.
    #[must_use]
    pub fn query(&self) -> HistQuery {
        HistQuery {
            table: self.clone(),
            filters: Vec::new(),
            order: Vec::new(),
            limit: None,
            err: None,
        }
    }

    fn row(
        &self,
        row: &[(&str, SqlValue)],
    ) -> Result<(Vec<&'static str>, Vec<SqlValue>, Vec<&'static str>), HistError> {
        for (col, _) in row {
            self.field(col)?;
        }
        let mut columns = Vec::new();
        let mut values = Vec::new();
        let mut generated = Vec::new();
        for f in self.schema.fields {
            let given = row.iter().find(|(c, _)| *c == f.column).map(|(_, v)| v);
            match given {
                Some(_) if f.auto && f.primary_key && !self.reset => {
                    return Err(HistError::ExplicitAuto { column: f.column })
                }
                Some(v) => {
                    self.check_write(f)?;
                    columns.push(f.column);
                    values.push(coerce(f, v.clone())?);
                }
                None if f.auto && f.ty == FieldType::Uuid => {
                    columns.push(f.column);
                    values.push(SqlValue::Uuid(uuid::Uuid::now_v7()));
                    generated.push(f.column);
                }
                None if f.auto || f.generated_as.is_some() || f.nullable || f.default.is_some() => {
                }
                None => return Err(HistError::MissingValue { column: f.column }),
            }
        }
        Ok((columns, values, generated))
    }

    fn note_reset(&self, ctx: &mut StepCtx<'_>, columns: &[&'static str]) {
        if !self.reset {
            return;
        }
        for f in self
            .schema
            .fields
            .iter()
            .filter(|f| f.auto && f.primary_key)
        {
            if columns.contains(&f.column) {
                ctx.resets.insert((self.schema.table.to_owned(), f.column));
            }
        }
    }

    /// The single `INSERT` [`Self::insert`] runs.
    ///
    /// # Errors
    /// Footprint, unknown column, coercion, a missing NOT NULL value, a
    /// sealed or generated column, an explicit auto key without reset.
    pub fn compile_insert(
        &self,
        d: &dyn Dialect,
        row: &[(&str, SqlValue)],
        conflict: Option<ConflictClause>,
    ) -> Result<CompiledStatement, HistError> {
        self.writable(None)?;
        let (columns, values, generated) = self.row(row)?;
        let mut q = InsertQuery::new(self.schema, columns, values).generated(generated);
        q.on_conflict = conflict;
        Ok(d.compile_insert(&q)?)
    }

    /// One `INSERT`. Auto integer keys and generated columns are left to
    /// the DB; an auto uuid key gets a v7.
    ///
    /// # Errors
    /// As [`Self::compile_insert`], or the driver.
    pub async fn insert(
        &self,
        ctx: &mut StepCtx<'_>,
        row: &[(&str, SqlValue)],
    ) -> Result<u64, HistError> {
        let stmt = self.compile_insert(ctx.dialect(), row, None)?;
        let cols: Vec<&'static str> = row
            .iter()
            .filter_map(|(c, _)| self.field(c).ok().map(|f| f.column))
            .collect();
        self.note_reset(ctx, &cols);
        ctx.run(stmt).await
    }

    /// One `INSERT … ON CONFLICT DO NOTHING` (MySQL `INSERT IGNORE`).
    ///
    /// # Errors
    /// As [`Self::insert`].
    pub async fn insert_ignore(
        &self,
        ctx: &mut StepCtx<'_>,
        row: &[(&str, SqlValue)],
    ) -> Result<u64, HistError> {
        let stmt = self.compile_insert(ctx.dialect(), row, Some(ConflictClause::DoNothing))?;
        ctx.run(stmt).await
    }

    /// One upsert on the unique `conflict` columns: the other given columns
    /// are updated.
    ///
    /// # Errors
    /// As [`Self::insert`].
    pub async fn upsert(
        &self,
        ctx: &mut StepCtx<'_>,
        conflict: &[&str],
        row: &[(&str, SqlValue)],
    ) -> Result<u64, HistError> {
        let target = conflict
            .iter()
            .map(|c| self.field(c).map(|f| f.column))
            .collect::<Result<Vec<_>, _>>()?;
        let update_columns = row
            .iter()
            .filter(|(c, _)| !conflict.contains(c))
            .map(|(c, _)| self.field(c).map(|f| f.column))
            .collect::<Result<Vec<_>, _>>()?;
        let clause = ConflictClause::DoUpdate {
            target,
            update_columns,
        };
        let stmt = self.compile_insert(ctx.dialect(), row, Some(clause))?;
        ctx.run(stmt).await
    }

    /// Multi-row `INSERT`s, chunked by the dialect's bind ceiling; every
    /// row names the same columns.
    ///
    /// # Errors
    /// As [`Self::insert`].
    pub async fn insert_many(
        &self,
        ctx: &mut StepCtx<'_>,
        rows: &[Vec<(&str, SqlValue)>],
    ) -> Result<u64, HistError> {
        self.writable(None)?;
        let Some(first) = rows.first() else {
            return Ok(0);
        };
        let (columns, _, generated) = self.row(first)?;
        let mut all = Vec::with_capacity(rows.len());
        for r in rows {
            let (cols, values, _) = self.row(r)?;
            if cols != columns {
                return Err(HistError::Footprint(
                    "insert_many: every row names the same columns".into(),
                ));
            }
            all.push(values);
        }
        self.note_reset(ctx, &columns);
        let per = (ctx.dialect().max_bind_params() / columns.len().max(1)).max(1);
        let mut n = 0;
        for chunk in all.chunks(per) {
            let q = BulkInsertQuery::new(self.schema, columns.clone(), chunk.to_vec())
                .generated(generated.clone());
            let stmt = ctx.dialect().compile_bulk_insert(&q)?;
            n += ctx.run(stmt).await?;
        }
        Ok(n)
    }
}

/// Lossless widening only; a narrowing or cross-type value is refused.
fn coerce(field: &'static FieldSchema, v: SqlValue) -> Result<SqlValue, HistError> {
    use FieldType as T;
    use SqlValue as V;
    let out = match (field.ty, v) {
        (_, V::Null) if field.nullable => V::Null,
        (T::I16, V::I16(n)) => V::I16(n),
        (T::I32, V::I16(n)) => V::I32(n.into()),
        (T::I32, V::I32(n)) => V::I32(n),
        (T::I64, V::I16(n)) => V::I64(n.into()),
        (T::I64, V::I32(n)) => V::I64(n.into()),
        (T::I64, V::I64(n)) => V::I64(n),
        (T::F32, V::F32(n)) => V::F32(n),
        (T::F64, V::F32(n)) => V::F64(n.into()),
        (T::F64, V::F64(n)) => V::F64(n),
        (T::Bool, V::Bool(b)) => V::Bool(b),
        (T::String, V::String(s)) => V::String(s),
        (T::DateTime, V::DateTime(d)) => V::DateTime(d),
        (T::Date, V::Date(d)) => V::Date(d),
        (T::Time, V::Time(t)) => V::Time(t),
        (T::Uuid, V::Uuid(u)) => V::Uuid(u),
        (T::Json, V::Json(j)) => V::Json(j),
        (T::Decimal, V::Decimal(d)) => V::Decimal(d),
        (T::Binary, V::Binary(b)) => V::Binary(b),
        (_, got) => {
            return Err(HistError::Coerce {
                column: field.column,
                expected: field.ty.as_str(),
                got,
            })
        }
    };
    Ok(out)
}

/// One decoded row: `(column, value)` in the table's column order.
#[derive(Debug, Clone, PartialEq)]
pub struct HistRow(pub Vec<(&'static str, SqlValue)>);

impl HistRow {
    #[must_use]
    pub fn get(&self, column: &str) -> Option<&SqlValue> {
        self.0.iter().find(|(c, _)| *c == column).map(|(_, v)| v)
    }

    /// `column`'s value, for [`Batch::advance_to`].
    #[must_use]
    pub fn key(&self, column: &str) -> Option<SqlValue> {
        self.get(column).cloned()
    }
}

fn bad_cell(fields: &[&'static FieldSchema], e: ExecError) -> HistError {
    match e {
        ExecError::Driver(sqlx::Error::ColumnDecode { index, source }) => {
            let bare = index.trim_matches('"');
            HistError::BadCell {
                column: fields
                    .iter()
                    .find(|f| f.column == bare)
                    .map_or("?", |f| f.column),
                why: source.to_string(),
            }
        }
        e => HistError::Exec(e),
    }
}

/// JSON rows from the strict reader as typed values.
fn decode_rows(
    fields: &[&'static FieldSchema],
    rows: Vec<serde_json::Value>,
) -> Result<Vec<HistRow>, HistError> {
    rows.into_iter()
        .map(|json| {
            fields
                .iter()
                .map(|f| {
                    let cell = json.get(f.name).unwrap_or(&serde_json::Value::Null);
                    crate::migrate::manage::fixtures::json_to_sql_value(cell, f)
                        .map(|v| (f.column, v))
                        .map_err(|why| HistError::BadCell {
                            column: f.column,
                            why,
                        })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(HistRow)
        })
        .collect()
}

/// Filters plus the terminal operations. The first bad column or value
/// is kept and returned by the terminal call.
#[derive(Debug)]
pub struct HistQuery {
    table: HistTable,
    filters: Vec<Filter>,
    order: Vec<OrderItem>,
    limit: Option<i64>,
    err: Option<HistError>,
}

impl HistQuery {
    fn push(mut self, column: &str, op: Op, value: SqlValue) -> Self {
        if self.err.is_some() {
            return self;
        }
        let value = self.table.field(column).and_then(|f| {
            let v = match (op, value) {
                (Op::IsNull, v) => Ok(v),
                (Op::In, SqlValue::List(vs)) => vs
                    .into_iter()
                    .map(|v| coerce(f, v))
                    .collect::<Result<_, _>>()
                    .map(SqlValue::List),
                (_, v) => coerce(f, v),
            }?;
            Ok((f.column, v))
        });
        match value {
            Ok((column, value)) => self.filters.push(Filter { column, op, value }),
            Err(e) => self.err = Some(e),
        }
        self
    }

    #[must_use]
    pub fn filter_eq(self, column: &str, v: impl Into<SqlValue>) -> Self {
        self.push(column, Op::Eq, v.into())
    }
    #[must_use]
    pub fn filter_ne(self, column: &str, v: impl Into<SqlValue>) -> Self {
        self.push(column, Op::Ne, v.into())
    }
    #[must_use]
    pub fn filter_gt(self, column: &str, v: impl Into<SqlValue>) -> Self {
        self.push(column, Op::Gt, v.into())
    }
    #[must_use]
    pub fn filter_lt(self, column: &str, v: impl Into<SqlValue>) -> Self {
        self.push(column, Op::Lt, v.into())
    }
    #[must_use]
    pub fn filter_in(self, column: &str, vs: Vec<SqlValue>) -> Self {
        self.push(column, Op::In, SqlValue::List(vs))
    }
    #[must_use]
    pub fn filter_is_null(self, column: &str, is_null: bool) -> Self {
        self.push(column, Op::IsNull, SqlValue::Bool(is_null))
    }
    #[must_use]
    pub fn order_by(mut self, column: &str, desc: bool) -> Self {
        match self.table.field(column) {
            Ok(f) => self.order.push(
                OrderClause {
                    column: f.column,
                    desc,
                }
                .into(),
            ),
            Err(e) => {
                self.err.get_or_insert(e);
            }
        }
        self
    }
    #[must_use]
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    fn parts(self) -> Result<(HistTable, WhereExpr, Vec<OrderItem>, Option<i64>), HistError> {
        match self.err {
            Some(e) => Err(e),
            None => Ok((
                self.table,
                WhereExpr::and_predicates(self.filters),
                self.order,
                self.limit,
            )),
        }
    }

    /// Every column, strictly decoded: a cell that does not read as its
    /// state type is an error, never `NULL`.
    ///
    /// # Errors
    /// Builder errors, bad cells, or the driver.
    pub async fn fetch(self, ctx: &mut StepCtx<'_>) -> Result<Vec<HistRow>, HistError> {
        let (t, where_clause, order, limit) = self.parts()?;
        let mut q = SelectQuery::new(t.schema).where_clause(where_clause);
        q.order_by = order;
        q.limit = limit;
        let fields: Vec<&'static FieldSchema> = t.schema.fields.iter().collect();
        q.projection = Some(fields.iter().map(|f| f.column).collect());
        let rows = crate::sql::select_rows_as_json_strict_tx(&mut ctx.tx, &q, &fields)
            .await
            .map_err(|e| {
                ctx.poisoned = true;
                bad_cell(&fields, e)
            })?;
        decode_rows(&fields, rows)
    }

    /// `SELECT COUNT(*)` with the filters.
    ///
    /// # Errors
    /// Builder errors or the driver.
    pub async fn count(self, ctx: &mut StepCtx<'_>) -> Result<i64, HistError> {
        let (t, where_clause, _, _) = self.parts()?;
        let stmt = ctx
            .dialect()
            .compile_count(&CountQuery::new(t.schema, where_clause))?;
        let params = stmt.params().to_vec();
        let rows: Vec<(i64,)> = crate::sql::raw_query_tx(&mut ctx.tx, &stmt.sql, params)
            .await
            .inspect_err(|_| ctx.poisoned = true)?;
        Ok(rows.first().map_or(0, |r| r.0))
    }

    /// Whether any row matches.
    ///
    /// # Errors
    /// As [`Self::count`].
    pub async fn exists(self, ctx: &mut StepCtx<'_>) -> Result<bool, HistError> {
        Ok(self.limit(1).count(ctx).await? > 0)
    }

    /// The single `UPDATE` [`Self::update`] runs.
    ///
    /// # Errors
    /// Builder errors, footprint, coercion, a sealed or generated column.
    pub fn compile_update(
        self,
        d: &dyn Dialect,
        set: &[(&str, SqlValue)],
    ) -> Result<CompiledStatement, HistError> {
        let (t, where_clause, _, _) = self.parts()?;
        let set = set
            .iter()
            .map(|(c, v)| {
                let f = t.field(c)?;
                t.writable(Some(f.column))?;
                t.check_write(f)?;
                Ok(Assignment::new(f.column, coerce(f, v.clone())?))
            })
            .collect::<Result<Vec<_>, HistError>>()?;
        Ok(d.compile_update(&UpdateQuery::new(t.schema, set, where_clause))?)
    }

    /// One `UPDATE`; rows affected.
    ///
    /// # Errors
    /// As [`Self::compile_update`], or the driver.
    pub async fn update(
        self,
        ctx: &mut StepCtx<'_>,
        set: &[(&str, SqlValue)],
    ) -> Result<u64, HistError> {
        let stmt = self.compile_update(ctx.dialect(), set)?;
        ctx.run(stmt).await
    }

    /// The single `DELETE` [`Self::delete`] runs: no junction cleanup, no audit.
    ///
    /// # Errors
    /// Builder errors or footprint.
    pub fn compile_delete(self, d: &dyn Dialect) -> Result<CompiledStatement, HistError> {
        let (t, where_clause, _, _) = self.parts()?;
        t.writable(None)?;
        Ok(d.compile_delete(&DeleteQuery::new(t.schema, where_clause))?)
    }

    /// One `DELETE`; rows affected.
    ///
    /// # Errors
    /// As [`Self::compile_delete`], or the driver.
    pub async fn delete(self, ctx: &mut StepCtx<'_>) -> Result<u64, HistError> {
        let stmt = self.compile_delete(ctx.dialect())?;
        ctx.run(stmt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_key_names_both_types() {
        let e = refuse_duplicates_in([
            ("acc", "move_v1", "a::MoveV1"),
            ("acc", "move_v1", "b::MoveAgain"),
            ("acc", "other_v1", "a::Other"),
        ])
        .unwrap_err();
        assert!(
            e.contains("`acc:move_v1` by a::MoveV1 and b::MoveAgain"),
            "{e}"
        );
        refuse_duplicates_in([("acc", "move_v1", "a::MoveV1"), ("b", "move_v1", "b::M")]).unwrap();
    }

    #[test]
    fn near_matches_are_close_ids() {
        assert_eq!(distance("move_v1", "move_v2"), 1);
        assert!(distance("move_v1", "unrelated") > 3);
    }
}
