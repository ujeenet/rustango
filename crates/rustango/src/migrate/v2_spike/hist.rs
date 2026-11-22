//! SPIKE S1: a historical table built from a migration-state
//! [`TableSnapshot`], leaked once per distinct normalised snapshot and
//! compiled through the dialect IR. One statement per call; no
//! `QuerySet`, no delete plan, no audit, no inventory lookups.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use sha2::{Digest, Sha256};

use crate::core::{
    Assignment, CountQuery, DeleteQuery, FieldSchema, FieldType, Filter, InsertQuery, ModelSchema,
    Op, OrderClause, OrderItem, SelectQuery, SqlValue, UpdateQuery, WhereExpr,
};
use crate::migrate::snapshot::{field_type_from_name, TableSnapshot};
use crate::sql::{CompiledStatement, Dialect, ExecError, Pool};

/// Prefix of every leaked schema's `name`, so `ModelEntry::for_schema`
/// (table *and* name) never matches a registered model.
pub const HIST_PREFIX: &str = "__hist:";

/// Errors of the historical ORM.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistError {
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

static LEAKED: AtomicUsize = AtomicUsize::new(0);

/// Bytes leaked by every historical schema built so far in this process.
#[must_use]
pub fn leaked_bytes() -> usize {
    LEAKED.load(Ordering::Relaxed)
}

fn leak_str(s: &str) -> &'static str {
    LEAKED.fetch_add(s.len(), Ordering::Relaxed);
    Box::leak(s.to_owned().into_boxed_str())
}

/// DB-relevant form: Rust names stripped, collections sorted.
#[must_use]
pub fn normalise(snap: &TableSnapshot) -> TableSnapshot {
    let mut s = snap.clone();
    s.model = String::new();
    for f in &mut s.fields {
        f.name = String::new();
    }
    s.fields.sort_by(|a, b| a.column.cmp(&b.column));
    s.composite_fks.sort_by(|a, b| a.name.cmp(&b.name));
    s
}

/// sha256 over the app label and the normalised snapshot JSON.
#[must_use]
pub fn snapshot_hash(app: &str, snap: &TableSnapshot) -> [u8; 32] {
    let json = serde_json::to_vec(&normalise(snap)).expect("a snapshot always serialises");
    let mut h = Sha256::new();
    h.update(app.as_bytes());
    h.update([0]);
    h.update(&json);
    h.finalize().into()
}

/// The reserved schema name: never a Rust identifier, so never a model's.
fn reserved_name(app: &str, table: &str, hash: &[u8; 32]) -> String {
    format!(
        "{HIST_PREFIX}{app}.{table}@{}",
        crate::hex::hex_encode(&hash[..6])
    )
}

/// One table at one state point.
#[derive(Debug, Clone, Copy)]
pub struct HistTable {
    schema: &'static ModelSchema,
}

impl HistTable {
    /// The table at `snap`, interned by [`snapshot_hash`].
    ///
    /// # Errors
    /// [`HistError::UnknownType`] for a type name this build cannot read.
    pub fn from_snapshot(app: &str, snap: &TableSnapshot) -> Result<Self, HistError> {
        type Cache = HashMap<[u8; 32], &'static ModelSchema>;
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
        let hash = snapshot_hash(app, snap);
        let mut cache = CACHE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(schema) = cache.get(&hash) {
            return Ok(Self { schema });
        }
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
            fs.min = f.min;
            fs.max = f.max;
            fs.default = f.default.as_deref().map(leak_str);
            fs.auto = f.auto;
            fs.unique = f.unique;
            fs.case_insensitive = f.case_insensitive;
            fs.generated_as = f.generated_as.as_deref().map(leak_str);
            fields.push(fs);
        }
        LEAKED.fetch_add(
            std::mem::size_of::<FieldSchema>() * fields.len() + std::mem::size_of::<ModelSchema>(),
            Ordering::Relaxed,
        );
        let mut schema = ModelSchema::new(
            leak_str(&reserved_name(app, &norm.name, &hash)),
            leak_str(&norm.name),
        );
        schema.fields = Box::leak(fields.into_boxed_slice());
        let schema: &'static ModelSchema = Box::leak(Box::new(schema));
        cache.insert(hash, schema);
        Ok(Self { schema })
    }

    /// The leaked schema, for checks only.
    #[must_use]
    pub fn schema(&self) -> &'static ModelSchema {
        self.schema
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

    /// A filter/select/update/delete builder on this table.
    #[must_use]
    pub fn query(&self) -> HistQuery {
        HistQuery {
            table: *self,
            filters: Vec::new(),
            order: Vec::new(),
            limit: None,
            err: None,
        }
    }

    /// One `INSERT`. Auto integer pks and generated columns are left to
    /// the DB; an auto uuid pk gets a v7.
    ///
    /// # Errors
    /// Unknown column, coercion, missing NOT NULL value, or the driver.
    pub async fn insert(&self, pool: &Pool, row: &[(&str, SqlValue)]) -> Result<u64, HistError> {
        let stmt = self.compile_insert(pool.dialect(), row)?;
        run(pool, stmt).await
    }

    /// The single statement [`Self::insert`] runs.
    ///
    /// # Errors
    /// As [`Self::insert`], minus the driver.
    pub fn compile_insert(
        &self,
        d: &dyn Dialect,
        row: &[(&str, SqlValue)],
    ) -> Result<CompiledStatement, HistError> {
        for (col, _) in row {
            self.field(col)?;
        }
        let mut columns = Vec::new();
        let mut values = Vec::new();
        let mut generated = Vec::new();
        for f in self.schema.fields {
            let given = row.iter().find(|(c, _)| *c == f.column).map(|(_, v)| v);
            match given {
                Some(_) if f.generated_as.is_some() => {
                    return Err(HistError::Generated { column: f.column })
                }
                Some(v) => {
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
        let q = InsertQuery::new(self.schema, columns, values).generated(generated);
        Ok(d.compile_insert(&q)?)
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
    pub async fn fetch(self, pool: &Pool) -> Result<Vec<HistRow>, HistError> {
        let (t, where_clause, order, limit) = self.parts()?;
        let mut q = SelectQuery::new(t.schema).where_clause(where_clause);
        q.order_by = order;
        q.limit = limit;
        let fields: Vec<&'static FieldSchema> = t.schema.fields.iter().collect();
        q.projection = Some(fields.iter().map(|f| f.column).collect());
        let rows =
            crate::sql::select_rows_as_json_with(pool, &q, &fields, crate::sql::CellDecode::STRICT)
                .await
                .map_err(|e| match e {
                    ExecError::Driver(sqlx::Error::ColumnDecode { index, source }) => {
                        HistError::BadCell {
                            column: column_named(&fields, &index),
                            why: source.to_string(),
                        }
                    }
                    e => HistError::Exec(e),
                })?;
        rows.into_iter()
            .map(|json| {
                fields
                    .iter()
                    .map(|f| {
                        let cell = json.get(f.name).unwrap_or(&serde_json::Value::Null);
                        crate::migrate::manage::json_to_sql_value(cell, f)
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

    /// `SELECT COUNT(*)` with the filters.
    ///
    /// # Errors
    /// Builder errors or the driver.
    pub async fn count(self, pool: &Pool) -> Result<i64, HistError> {
        let (t, where_clause, _, _) = self.parts()?;
        Ok(crate::sql::count_rows_on(pool, &CountQuery::new(t.schema, where_clause)).await?)
    }

    /// The single `UPDATE` [`Self::update`] runs.
    ///
    /// # Errors
    /// Builder errors or coercion.
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
                if f.generated_as.is_some() {
                    return Err(HistError::Generated { column: f.column });
                }
                Ok(Assignment::new(f.column, coerce(f, v.clone())?))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(d.compile_update(&UpdateQuery::new(t.schema, set, where_clause))?)
    }

    /// One `UPDATE`; rows affected.
    ///
    /// # Errors
    /// As [`Self::compile_update`], or the driver.
    pub async fn update(self, pool: &Pool, set: &[(&str, SqlValue)]) -> Result<u64, HistError> {
        let stmt = self.compile_update(pool.dialect(), set)?;
        run(pool, stmt).await
    }

    /// The single `DELETE` [`Self::delete`] runs: no junction cleanup, no audit.
    ///
    /// # Errors
    /// Builder errors.
    pub fn compile_delete(self, d: &dyn Dialect) -> Result<CompiledStatement, HistError> {
        let (t, where_clause, _, _) = self.parts()?;
        Ok(d.compile_delete(&DeleteQuery::new(t.schema, where_clause))?)
    }

    /// One `DELETE`; rows affected.
    ///
    /// # Errors
    /// Builder errors or the driver.
    pub async fn delete(self, pool: &Pool) -> Result<u64, HistError> {
        let stmt = self.compile_delete(pool.dialect())?;
        run(pool, stmt).await
    }
}

/// The column a driver decode error names (`"\"col\""` or an index).
fn column_named(fields: &[&'static FieldSchema], index: &str) -> &'static str {
    let bare = index.trim_matches('"');
    fields
        .iter()
        .find(|f| f.column == bare)
        .map_or("?", |f| f.column)
}

async fn run(pool: &Pool, stmt: CompiledStatement) -> Result<u64, HistError> {
    let params = stmt.params().to_vec();
    Ok(crate::sql::raw_execute_pool(pool, &stmt.sql, params).await?)
}
