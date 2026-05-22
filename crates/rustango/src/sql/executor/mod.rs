//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use crate::core::{
    AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery, InsertQuery, Model,
    SelectQuery, SqlValue, UpdateQuery,
};
use crate::query::{QuerySet, UpdateBuilder};

// PG-typed helpers below import these. Sqlite/MySQL-only builds only
// see the bi/tri-dialect `_pool` entry points from this module.
#[cfg(feature = "postgres")]
use sqlx::postgres::{PgArguments, PgPool, PgRow};
#[cfg(feature = "postgres")]
use sqlx::query::{Query, QueryAs};

use super::Dialect;
use super::ExecError;
#[cfg(feature = "postgres")]
use super::Postgres;

/// Hidden trait every `#[derive(Model)]` type implements via the
/// macro — slice 9.0e's bridge between `fetch_with_prefetch` and
/// the per-Model FK-PK accessor. For each `ForeignKey<T>` field on
/// a Child model, the macro generates an arm that returns the FK's
/// stored PK (regardless of `Loaded` / `Unloaded` state) so the
/// prefetch grouper can stitch children to the right parent.
///
/// Models with no `ForeignKey<T>` fields get a no-op impl
/// (returns `None` for any field name).
#[doc(hidden)]
pub trait FkPkAccess {
    /// Read the i64 PK stored in a `ForeignKey<T>` field by name.
    /// `None` for unknown field names, non-FK fields, or FKs whose
    /// PK type isn't `i64` (use [`Self::__rustango_fk_pk_value`] for
    /// those).
    fn __rustango_fk_pk(&self, field_name: &str) -> Option<i64>;

    /// Read the PK stored in a `ForeignKey<T, K>` field by name as a
    /// dialect-neutral [`crate::core::SqlValue`]. Works for every PK
    /// type — `i64`, `i32`, `String`, `Uuid`, etc. — so
    /// `fetch_with_prefetch` no longer has to force-cast to `i64`.
    /// `None` for unknown field names or non-FK fields.
    fn __rustango_fk_pk_value(&self, field_name: &str) -> Option<crate::core::SqlValue>;
}

/// Hidden trait every `#[derive(Model)]` type implements via the
/// macro — slice 9.0d's bridge between `QuerySet::fetch_on` and the
/// per-Model `__rustango_load_related` dispatcher. Loaders for
/// individual FK fields live on the Model's inherent impl; this
/// trait makes them callable polymorphically from generic
/// fetch_on code.
///
/// Models with no `ForeignKey<T>` fields get a no-op impl
/// (returns `Ok(false)` for any field name), so the trait bound on
/// `fetch_on` is universally satisfied — users don't have to think
/// about it.
#[doc(hidden)]
#[cfg(feature = "postgres")]
pub trait LoadRelated {
    /// Stitch a `select_related`-loaded parent onto this instance's
    /// FK field. `field_name` is the FK field's Rust name (e.g.
    /// `"author"`); `alias` is the SELECT writer's alias prefix
    /// for that JOIN's projected columns (typically the same as
    /// `field_name`). Returns `Ok(false)` for unknown field names —
    /// callers may pass select directives that don't apply to this
    /// model and get a graceful skip.
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related(
        &mut self,
        row: &PgRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

/// Always-on marker when `postgres` is off — the macro emits an
/// empty impl unconditionally so generic bounds resolve.
#[cfg(not(feature = "postgres"))]
#[doc(hidden)]
pub trait LoadRelated {}
#[cfg(not(feature = "postgres"))]
impl<T> LoadRelated for T {}

#[cfg(feature = "postgres")]
impl<T> QuerySet<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    /// Like [`Fetcher::fetch`] but takes any sqlx executor — `&PgPool`,
    /// `&mut PgConnection`, or a `Transaction`. The escape hatch for
    /// tenant-scoped queries: schema-mode tenants share the registry
    /// pool but rely on a per-checkout `SET search_path`, so passing
    /// `&PgPool` would silently hit the wrong schema. Acquire a
    /// connection via `TenantPools::acquire(&org)` and pass that here.
    ///
    /// # Errors
    /// As [`Fetcher::fetch`].
    pub async fn fetch_on<'c, E>(self, executor: E) -> Result<Vec<T>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
        T: LoadRelated,
    {
        let select = self.compile()?;
        let select_related_aliases: Vec<&'static str> =
            select.joins.iter().map(|j| j.alias).collect();
        let stmt = Postgres.compile_select(&select)?;

        if select_related_aliases.is_empty() {
            // No JOINs — fast path identical to the v0.8.1 shape.
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for value in stmt.params {
                q = bind_query_as(q, value);
            }
            let rows = q.fetch_all(executor).await?;
            return Ok(rows);
        }

        // Slice 9.0d: select_related path. Fetch raw rows so we can
        // both decode `T` via `from_row` AND call
        // `T::__rustango_load_related(&mut t, &row, alias, alias)`
        // for each JOINed target — single SQL round trip, no N+1.
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let raw_rows = q.fetch_all(executor).await?;
        let mut out = Vec::with_capacity(raw_rows.len());
        for row in &raw_rows {
            let mut t = T::from_row(row)?;
            for alias in &select_related_aliases {
                let _ = t.__rustango_load_related(row, alias, alias)?;
            }
            out.push(t);
        }
        Ok(out)
    }

    /// Fetch a page of rows **and** the total matching count in a
    /// single SQL round trip. Postgres' `COUNT(*) OVER ()` window
    /// function returns the pre-LIMIT total alongside each row, so a
    /// paginated endpoint never needs the customary second
    /// `SELECT COUNT(*)` Django's `Paginator` triggers.
    ///
    /// ```ignore
    /// let page: Page<Post> = Post::objects()
    ///     .where_(Post::published.eq(true))
    ///     .limit(20).offset(40)
    ///     .fetch_paginated_on(tenant.conn()).await?;
    /// assert!(page.total >= page.rows.len() as i64);
    /// ```
    ///
    /// SQL emitted (abridged):
    ///
    /// ```text
    /// SELECT id, title, ..., COUNT(*) OVER () AS "__rustango_total"
    /// FROM post WHERE ...
    /// ORDER BY ... LIMIT 20 OFFSET 40
    /// ```
    ///
    /// Empty result set → `Page { rows: vec![], total: 0 }` (no
    /// driver round trip is wasted on an extra COUNT).
    ///
    /// # Errors
    /// As [`Self::fetch_on`].
    pub async fn fetch_paginated_on<'c, E>(self, executor: E) -> Result<Page<T>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;
        let sql = inject_total_count(&stmt.sql);
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let raw_rows: Vec<PgRow> = q.fetch_all(executor).await?;
        let total: i64 = raw_rows
            .first()
            .map(|row| sqlx::Row::try_get::<i64, _>(row, "__rustango_total"))
            .transpose()?
            .unwrap_or(0);
        let mut rows = Vec::with_capacity(raw_rows.len());
        for row in &raw_rows {
            rows.push(T::from_row(row)?);
        }
        Ok(Page { rows, total })
    }

    /// Pool-side companion to [`Self::fetch_paginated_on`] — same
    /// query, ergonomics for non-tenant code.
    ///
    /// # Errors
    /// As [`Self::fetch_paginated_on`].
    pub async fn fetch_paginated(self, pool: &PgPool) -> Result<Page<T>, ExecError> {
        self.fetch_paginated_on(pool).await
    }

    /// Tenant-scoped companion to [`QuerySet::in_bulk`] — same
    /// semantic but takes any sqlx executor (`&PgPool`,
    /// `&mut PgConnection`, or a `Transaction`) so schema-mode tenant
    /// queries route through the per-checkout `SET search_path`
    /// connection. Issue #24.
    ///
    /// # Errors
    /// As [`Self::fetch_on`].
    pub async fn in_bulk_on<'c, E, C, K, I, F>(
        self,
        column: C,
        ids: I,
        extract: F,
        executor: E,
    ) -> Result<std::collections::HashMap<K, T>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
        T: LoadRelated,
        C: crate::core::Column<Model = T>,
        K: Eq + std::hash::Hash + Into<crate::core::SqlValue>,
        I: IntoIterator<Item = K>,
        F: Fn(&T) -> K,
    {
        let _ = column;
        let id_values: Vec<crate::core::SqlValue> = ids.into_iter().map(|v| v.into()).collect();
        if id_values.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = self
            .filter_op(
                C::COLUMN,
                crate::core::Op::In,
                crate::core::SqlValue::List(id_values),
            )
            .fetch_on(executor)
            .await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let key = extract(&row);
            out.insert(key, row);
        }
        Ok(out)
    }
}

mod page;
use page::inject_total_count;
pub use page::Page;

/// Like [`insert`] but accepts any sqlx executor — `&PgPool`,
/// `&mut PgConnection`, or a transaction. Tenant-scoped writes need
/// this: schema-mode tenants share the registry pool and rely on the
/// per-checkout `SET search_path`, so passing `&PgPool` would silently
/// hit the wrong schema.
///
/// # Errors
/// As [`insert`].
#[cfg(feature = "postgres")]
pub async fn insert_on<'c, E>(executor: E, query: &InsertQuery) -> Result<(), ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    q.execute(executor).await?;
    Ok(())
}

/// Like [`insert_returning`] but accepts any sqlx executor.
///
/// # Errors
/// As [`insert_returning`].
#[cfg(feature = "postgres")]
pub async fn insert_returning_on<'c, E>(
    executor: E,
    query: &InsertQuery,
) -> Result<PgRow, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    query.validate()?;
    let stmt = Postgres.compile_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let row = q.fetch_one(executor).await?;
    Ok(row)
}

/// Like [`bulk_insert`] but accepts any sqlx executor.
///
/// # Errors
/// As [`bulk_insert`].
#[cfg(feature = "postgres")]
pub async fn bulk_insert_on<'c, E>(
    executor: E,
    query: &BulkInsertQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_bulk_insert(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    if query.returning.is_empty() {
        q.execute(executor).await?;
        Ok(Vec::new())
    } else {
        Ok(q.fetch_all(executor).await?)
    }
}

/// Like [`update`] but accepts any sqlx executor.
///
/// # Errors
/// As [`update`].
#[cfg(feature = "postgres")]
pub async fn update_on<'c, E>(executor: E, query: &UpdateQuery) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    query.validate()?;
    let stmt = Postgres.compile_update(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(executor).await?;
    Ok(result.rows_affected())
}

/// Like [`delete`] but accepts any sqlx executor.
///
/// # Errors
/// As [`delete`].
#[cfg(feature = "postgres")]
pub async fn delete_on<'c, E>(executor: E, query: &DeleteQuery) -> Result<u64, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_delete(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    let result = q.execute(executor).await?;
    Ok(result.rows_affected())
}

/// Schema-driven decode of a Postgres row into a JSON object.
/// Walks `fields` and pulls each column out via `try_get`,
/// mapping the model's `FieldType` to the right Rust type, then
/// to JSON. Used by the viewset list/retrieve handlers (#80) and
/// by `contenttypes::fetch_row_as_json` (#89).
///
/// Failures on individual columns degrade gracefully to
/// `Value::Null` — the response shape stays stable even if one
/// field's bytes are unexpected (e.g. a NULL where the schema
/// says NOT NULL because of a manual SQL edit). Strict
/// row-to-T decoding lives on the Model derive's `from_row` path
/// and is the right tool when you control the data shape.
#[must_use]
#[cfg(feature = "postgres")]
pub fn row_to_json(
    row: &sqlx::postgres::PgRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    use crate::core::FieldType;
    use serde_json::{json, Value};
    use sqlx::Row as _;
    let mut map = serde_json::Map::new();
    for field in fields {
        let value = match field.ty {
            FieldType::I16 => row
                .try_get::<i16, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I32 => row
                .try_get::<i32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I64 => row
                .try_get::<i64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F32 => row
                .try_get::<f32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F64 => row
                .try_get::<f64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::Bool => row
                .try_get::<bool, _>(field.column)
                .map(|b| json!(b))
                .unwrap_or(Value::Null),
            FieldType::String => row
                .try_get::<String, _>(field.column)
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
            FieldType::Date => row
                .try_get::<chrono::NaiveDate, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or(Value::Null),
            FieldType::DateTime => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(field.column)
                .map(|dt| json!(dt.to_rfc3339()))
                .unwrap_or(Value::Null),
            FieldType::Uuid => row
                .try_get::<uuid::Uuid, _>(field.column)
                .map(|u| json!(u.to_string()))
                .unwrap_or(Value::Null),
            FieldType::Json => row
                .try_get::<serde_json::Value, _>(field.column)
                .unwrap_or(Value::Null),
            FieldType::Decimal => row
                .try_get::<rust_decimal::Decimal, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or(Value::Null),
            FieldType::Binary => row
                .try_get::<Vec<u8>, _>(field.column)
                .map(|b| json!(hex_encode(&b)))
                .unwrap_or(Value::Null),
            FieldType::Time => row
                .try_get::<chrono::NaiveTime, _>(field.column)
                .map(|t| json!(t.to_string()))
                .unwrap_or(Value::Null),
        };
        map.insert(field.name.to_owned(), value);
    }
    Value::Object(map)
}

/// Render `bytes` as a lowercase hex string (no separator). Used by
/// the `row_to_json` family for [`FieldType::Binary`] columns — the
/// always-on crate can't pull in `base64` (gated behind several
/// features), so hex is the lowest-dep neutral encoding.
#[inline]
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// MySQL counterpart of [`row_to_json`]. Decodes each column by
/// `field.ty` against `&MySqlRow`. Type mappings mirror the
/// `sqlx::Type<MySql>` impls emitted by `#[derive(Model)]` —
/// `chrono::DateTime<Utc>` ↔ `DATETIME(6)`, `serde_json::Value` ↔
/// `JSON`, `uuid::Uuid` ↔ `CHAR(36)` (sqlx-mysql's default).
#[cfg(feature = "mysql")]
#[must_use]
pub fn row_to_json_my(
    row: &sqlx::mysql::MySqlRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    use crate::core::FieldType;
    use serde_json::{json, Value};
    use sqlx::Row as _;
    let mut map = serde_json::Map::new();
    for field in fields {
        let value = match field.ty {
            FieldType::I16 => row
                .try_get::<i16, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I32 => row
                .try_get::<i32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I64 => row
                .try_get::<i64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F32 => row
                .try_get::<f32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F64 => row
                .try_get::<f64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::Bool => row
                .try_get::<bool, _>(field.column)
                .map(|b| json!(b))
                .unwrap_or(Value::Null),
            FieldType::String => row
                .try_get::<String, _>(field.column)
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
            FieldType::Date => row
                .try_get::<chrono::NaiveDate, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or(Value::Null),
            FieldType::DateTime => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(field.column)
                .map(|dt| json!(dt.to_rfc3339()))
                .unwrap_or(Value::Null),
            FieldType::Uuid => row
                .try_get::<uuid::Uuid, _>(field.column)
                .map(|u| json!(u.to_string()))
                .unwrap_or(Value::Null),
            FieldType::Json => row
                .try_get::<serde_json::Value, _>(field.column)
                .unwrap_or(Value::Null),
            FieldType::Decimal => row
                .try_get::<rust_decimal::Decimal, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or(Value::Null),
            FieldType::Binary => row
                .try_get::<Vec<u8>, _>(field.column)
                .map(|b| json!(hex_encode(&b)))
                .unwrap_or(Value::Null),
            FieldType::Time => row
                .try_get::<chrono::NaiveTime, _>(field.column)
                .map(|t| json!(t.to_string()))
                .unwrap_or(Value::Null),
        };
        map.insert(field.name.to_owned(), value);
    }
    Value::Object(map)
}

/// SQLite counterpart of [`row_to_json`]. SQLite's storage is more
/// permissive (TEXT for VARCHAR + JSON + UUID, NUMERIC for DATE,
/// REAL for f32/f64); decode targets here match the column types
/// `crate::migrate::ddl::CREATE_TABLE_SQL_SQLITE` emits. Best-effort:
/// any `try_get` failure yields `Value::Null` (matches the PG path's
/// laxity for admin rendering).
#[cfg(feature = "sqlite")]
#[must_use]
pub fn row_to_json_sqlite(
    row: &sqlx::sqlite::SqliteRow,
    fields: &[&'static crate::core::FieldSchema],
) -> serde_json::Value {
    use crate::core::FieldType;
    use serde_json::{json, Value};
    use sqlx::Row as _;
    let mut map = serde_json::Map::new();
    for field in fields {
        let value = match field.ty {
            FieldType::I16 => row
                .try_get::<i16, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I32 => row
                .try_get::<i32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::I64 => row
                .try_get::<i64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F32 => row
                .try_get::<f32, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::F64 => row
                .try_get::<f64, _>(field.column)
                .map(|n| json!(n))
                .unwrap_or(Value::Null),
            FieldType::Bool => row
                .try_get::<bool, _>(field.column)
                .map(|b| json!(b))
                .unwrap_or(Value::Null),
            FieldType::String => row
                .try_get::<String, _>(field.column)
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
            FieldType::Date => row
                .try_get::<chrono::NaiveDate, _>(field.column)
                .map(|d| json!(d.to_string()))
                .unwrap_or_else(|_| {
                    // SQLite often stores DATE as TEXT — fall back to
                    // a raw string decode so callers see what's there
                    // instead of `null`.
                    row.try_get::<String, _>(field.column)
                        .map(|s| json!(s))
                        .unwrap_or(Value::Null)
                }),
            FieldType::DateTime => row
                .try_get::<chrono::DateTime<chrono::Utc>, _>(field.column)
                .map(|dt| json!(dt.to_rfc3339()))
                .unwrap_or_else(|_| {
                    row.try_get::<String, _>(field.column)
                        .map(|s| json!(s))
                        .unwrap_or(Value::Null)
                }),
            FieldType::Uuid => row
                .try_get::<String, _>(field.column)
                .map(|u| json!(u))
                .unwrap_or(Value::Null),
            FieldType::Json => {
                // SQLite stores JSON as TEXT; try parsing back to
                // Value, else surface the raw string.
                match row.try_get::<String, _>(field.column) {
                    Ok(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                    Err(_) => Value::Null,
                }
            }
            FieldType::Decimal => {
                // SQLite has no `rust_decimal::Decimal: Decode<Sqlite>`
                // impl, so we read NUMERIC-affinity columns as TEXT.
                // The `bind_match_sqlite!` macro round-trips via
                // `.to_string()` so the stored representation lines up.
                row.try_get::<String, _>(field.column)
                    .map(|s| json!(s))
                    .or_else(|_| {
                        // Small integers / floats may land in their
                        // native affinity — fall back gracefully.
                        row.try_get::<f64, _>(field.column)
                            .map(|n| json!(n.to_string()))
                    })
                    .unwrap_or(Value::Null)
            }
            FieldType::Binary => row
                .try_get::<Vec<u8>, _>(field.column)
                .map(|b| json!(hex_encode(&b)))
                .unwrap_or(Value::Null),
            FieldType::Time => row
                .try_get::<chrono::NaiveTime, _>(field.column)
                .map(|t| json!(t.to_string()))
                .unwrap_or_else(|_| {
                    // SQLite stores TIME as TEXT — fall back to raw
                    // string decode for non-`HH:MM:SS` shapes.
                    row.try_get::<String, _>(field.column)
                        .map(|s| json!(s))
                        .unwrap_or(Value::Null)
                }),
        };
        map.insert(field.name.to_owned(), value);
    }
    Value::Object(map)
}

/// Tri-dialect SELECT → JSON: run `query` against `pool` and return
/// each row as a `serde_json::Value` map (`field.name → value`). The
/// canonical fetch path for admin / API surfaces that need to render
/// rows without a typed `T: FromRow` struct.
///
/// Dispatches per [`Pool`] variant to `row_to_json` / `row_to_json_my`
/// / `row_to_json_sqlite` and uses the appropriate sqlx query type.
/// Field-by-field decode is best-effort (decode errors → `Value::Null`)
/// to match the existing PG-only `row_to_json`'s laxity around admin
/// rendering of dirty rows.
///
/// # Errors
/// SQL compilation / driver failures only — per-cell decode errors
/// are swallowed into `Value::Null`.
pub async fn select_rows_as_json(
    pool: &Pool,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Vec<serde_json::Value>, ExecError> {
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let rows = q.fetch_all(pg).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json(r, fields);
                    augment_joined_columns_pg(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let rows = q.fetch_all(my).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json_my(r, fields);
                    augment_joined_columns_my(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let rows = q.fetch_all(sq).await?;
            Ok(rows
                .iter()
                .map(|r| {
                    let mut json = row_to_json_sqlite(r, fields);
                    augment_joined_columns_sqlite(&mut json, r, &query.joins);
                    json
                })
                .collect())
        }
    }
}

/// v0.37 — copy joined-table columns (`<alias>__<col>`) into the
/// JSON row. The compile_select writer aliases joined columns this
/// way and the admin's `read_joined_value_as_html_json` reads them
/// out by the same key. Decoded as nullable strings — the admin only
/// uses these for FK display HTML rendering.
#[cfg(feature = "postgres")]
fn augment_joined_columns_pg(
    out: &mut serde_json::Value,
    row: &sqlx::postgres::PgRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = format!("{}__{}", join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(key.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

#[cfg(feature = "mysql")]
fn augment_joined_columns_my(
    out: &mut serde_json::Value,
    row: &sqlx::mysql::MySqlRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = format!("{}__{}", join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(key.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

#[cfg(feature = "sqlite")]
fn augment_joined_columns_sqlite(
    out: &mut serde_json::Value,
    row: &sqlx::sqlite::SqliteRow,
    joins: &[crate::core::Join],
) {
    use sqlx::Row as _;
    let Some(map) = out.as_object_mut() else {
        return;
    };
    for join in joins {
        for col in &join.project {
            let key = format!("{}__{}", join.alias, col);
            let v = row
                .try_get::<Option<String>, _>(key.as_str())
                .ok()
                .flatten();
            map.insert(
                key,
                v.map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

/// Single-row companion of [`select_rows_as_json`]. Returns
/// `Ok(None)` when no rows match.
///
/// # Errors
/// As [`select_rows_as_json`].
pub async fn select_one_row_as_json(
    pool: &Pool,
    query: &SelectQuery,
    fields: &[&'static crate::core::FieldSchema],
) -> Result<Option<serde_json::Value>, ExecError> {
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            Ok(q.fetch_optional(pg).await?.as_ref().map(|r| {
                let mut json = row_to_json(r, fields);
                augment_joined_columns_pg(&mut json, r, &query.joins);
                json
            }))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            Ok(q.fetch_optional(my).await?.as_ref().map(|r| {
                let mut json = row_to_json_my(r, fields);
                augment_joined_columns_my(&mut json, r, &query.joins);
                json
            }))
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.fetch_optional(sq).await?.as_ref().map(|r| {
                let mut json = row_to_json_sqlite(r, fields);
                augment_joined_columns_sqlite(&mut json, r, &query.joins);
                json
            }))
        }
    }
}

/// Like [`select_rows`] but accepts any sqlx executor — `&PgPool`,
/// `&mut PgConnection`, or a `Transaction`. Required for tenancy
/// projects whose per-request connection comes from
/// [`crate::extractors::Tenant`] rather than a single global pool.
///
/// # Errors
/// As [`select_rows`].
#[cfg(feature = "postgres")]
pub async fn select_rows_on<'c, E>(
    executor: E,
    query: &SelectQuery,
) -> Result<Vec<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_all(executor).await?)
}

/// Like [`select_one_row`] but accepts any sqlx executor.
///
/// # Errors
/// As [`select_one_row`].
#[cfg(feature = "postgres")]
pub async fn select_one_row_on<'c, E>(
    executor: E,
    query: &SelectQuery,
) -> Result<Option<PgRow>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_select(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for value in stmt.params {
        q = bind_query(q, value);
    }
    Ok(q.fetch_optional(executor).await?)
}

/// Slice 9.0b — annotate each parent row with the COUNT of its
/// children, returning `Vec<(Parent, i64)>` from a **single** SQL:
///
/// ```text
///   SELECT parent.<every-column>, COUNT(child.<pk>) AS __annotated_count
///   FROM parent
///   LEFT JOIN child ON child.<fk_column> = parent.<pk>
///   GROUP BY parent.<every-column>
///   [WHERE / ORDER BY clauses from `parent_qs` apply]
/// ```
///
/// Closes the demo's per-parent `count_on` loop (which was N+1) with
/// the canonical Django `Author.objects.annotate(post_count=Count('post'))`
/// shape. Restricted to a single Count aggregate over a single
/// reverse-FK relation in this MVP — full Django aggregation
/// (`.annotate(other_field=Sum(...), Avg(...), ...)`) is queued for
/// a follow-on slice.
///
/// `child_table` is the SQL table of the child model; `child_fk_column`
/// is the column on that table that stores the parent's PK.
///
/// # Errors
/// SQL-writing or driver failures from the single SELECT.
#[cfg(feature = "postgres")]
pub async fn annotate_count_children<P>(
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, i64)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    annotate_count_children_on(parent_qs, child_table, child_fk_column, pool).await
}

/// Like [`annotate_count_children`] but accepts any sqlx executor —
/// `&PgPool`, `&mut PgConnection`, or a transaction handle. Lets
/// tenant-scoped admin / API code use the optimized one-query form
/// against a `Tenant::conn()` connection (search_path scoped to the
/// tenant's schema), instead of falling back to a per-parent
/// `count_on` loop (N+1).
///
/// # Errors
/// As [`annotate_count_children`].
#[cfg(feature = "postgres")]
pub async fn annotate_count_children_on<'c, P, E>(
    parent_qs: crate::query::QuerySet<P>,
    child_table: &'static str,
    child_fk_column: &'static str,
    executor: E,
) -> Result<Vec<(P, i64)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    use std::fmt::Write as _;
    let select = parent_qs.compile()?;
    let parent = select.model;
    let pk_field = parent.primary_key().ok_or(ExecError::MissingPrimaryKey {
        table: parent.table,
    })?;

    // Build the SQL by hand — the existing compile_select doesn't
    // emit GROUP BY or aggregate columns. We mirror its conventions
    // (qualified columns, $N placeholders) for consistency.
    let cols: Vec<&'static str> = parent.scalar_fields().map(|f| f.column).collect();
    let mut sql = String::from("SELECT ");
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, "\"{}\".\"{col}\"", parent.table);
    }
    let _ = write!(
        sql,
        ", COUNT(\"{child_table}\".\"{child_pk}\") AS \"__annotated_count\" FROM \"{parent_table}\" LEFT JOIN \"{child_table}\" ON \"{child_table}\".\"{child_fk_column}\" = \"{parent_table}\".\"{parent_pk}\"",
        parent_table = parent.table,
        parent_pk = pk_field.column,
        child_pk = "id",
    );

    // Forward WHERE / ORDER BY / LIMIT / OFFSET from the parent queryset.
    let tail = crate::sql::postgres::compile_where_order_tail(
        &select.where_clause,
        select.search.as_ref(),
        &select.order_by,
        select.limit,
        select.offset,
        Some(parent.table),
        Some(parent),
    )?;

    // GROUP BY before the WHERE tail.
    sql.push_str(" GROUP BY ");
    for (i, col) in cols.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let _ = write!(sql, "\"{}\".\"{col}\"", parent.table);
    }
    sql.push_str(&tail.sql);

    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
    for param in tail.params {
        q = bind_query(q, param);
    }
    let raw_rows = q.fetch_all(executor).await?;
    let mut out = Vec::with_capacity(raw_rows.len());
    for row in &raw_rows {
        let parent_obj = P::from_row(row)?;
        let count: i64 = sqlx::Row::try_get(row, "__annotated_count")?;
        out.push((parent_obj, count));
    }
    Ok(out)
}

/// Slice 9.0e — `prefetch_related` Django-shape: fetch a list of
/// parents and, for each one, the children that point at it via a
/// foreign key. **Two SQL queries total**, regardless of how many
/// parents:
///
/// ```text
///   SELECT * FROM <parent>;
///   SELECT * FROM <child> WHERE <fk_column> IN ($1, $2, ...);
/// ```
///
/// Returns `Vec<(Parent, Vec<Child>)>` — each parent paired with its
/// children. Parents with no matching children get an empty `Vec`.
/// The order of parents matches the queryset; the order of children
/// within each group matches the order of the second query (lex by
/// PK is the typical default; pass `.limit()` / `.offset()` on the
/// child queryset if you need to scope).
///
/// `child_fk_column` is the SQL column on the child table that
/// stores the parent's PK — for `Post { author: ForeignKey<Author> }`,
/// that's `"author"`. The function looks up child rows where
/// `<child_fk_column> IN (parent_pks)` and groups them by reading
/// the same column on each fetched child via the
/// macro-generated [`FkPkAccess`] impl.
///
/// Closes the multi-parent gap left by v0.8.2's `<parent>::<child>_set`
/// helper (which fetches one parent's children at a time, requiring
/// N queries for N parents).
///
/// # Errors
/// Anything either of the underlying `fetch` calls returns.
#[cfg(feature = "postgres")]
pub async fn fetch_with_prefetch<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &PgPool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin + LoadRelated + HasPkValue,
    C: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin + LoadRelated + FkPkAccess,
{
    let parents: Vec<P> = parent_qs.fetch_on(pool).await?;
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    // Collect parent PKs as dialect-neutral `SqlValue`s — works for
    // every PK type (i64, i32, String, Uuid). Pre-v0.26 this path
    // force-cast to `i64` and silently dropped non-integer-keyed
    // parents; closes the P10 gap from `orm-improvements.md`.
    let pk_field = P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    let mut parent_pks: Vec<crate::core::SqlValue> = Vec::with_capacity(parents.len());
    for parent in &parents {
        let pk = extract_pk_value(parent);
        if !matches!(pk, crate::core::SqlValue::Null) {
            parent_pks.push(pk);
        }
    }
    // Dedupe via the display-string form so parents with the same PK
    // (shouldn't happen, but cheap insurance) only land once in the
    // IN clause.
    {
        let mut seen = std::collections::HashSet::new();
        parent_pks.retain(|v| seen.insert(v.to_display_string()));
    }
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    // Batch-fetch the children where their FK column points at any
    // of the parent PKs.
    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(parent_pks),
        )
        .fetch_on(pool)
        .await?;

    // Group children by FK PK. Key is the `SqlValue::to_display_string`
    // form — unambiguous for every PK type used as a key (integers,
    // strings, UUIDs all stringify uniquely; floats wouldn't but PKs
    // aren't floats).
    let mut grouped: std::collections::HashMap<String, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk_value(child_fk_column) else {
            continue;
        };
        grouped
            .entry(fk_pk.to_display_string())
            .or_default()
            .push(child);
    }

    // Stitch.
    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = extract_pk_value(&parent).to_display_string();
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field; // suppress unused-warning when only the PK lookup ran
    Ok(out)
}

/// Extract a model's PK as a `SqlValue` via the macro-generated
/// `__rustango_pk_value`. The trait bound `LoadRelated` is satisfied
/// by every Model derive but doesn't expose `__rustango_pk_value`,
/// so we go through `sqlx::Row` instead — every Model also impls
/// `FromRow`, and we already have an instance.
///
/// Actually we have the instance; the macro emits
/// `__rustango_pk_value` as an inherent method. Calling it through
/// a trait object would force a new trait. Punt: use sqlx-side
/// extraction via `sqlx::Encode` against the schema field. Cleaner:
/// just have callers' Models implement `PrefetchableParent`.
///
/// For the v0.9 MVP we leverage the fact that every Model with a
/// PK has `__rustango_pk_value`. We add a small trait `HasPkValue`
/// that the macro impls; its body just calls the inherent method.
fn extract_pk_value<P: HasPkValue>(parent: &P) -> crate::core::SqlValue {
    parent.__rustango_pk_value_impl()
}

/// Hidden trait — exposes the macro-generated inherent
/// `__rustango_pk_value` method polymorphically so generic
/// `fetch_with_prefetch` can read parent PKs without forcing the
/// caller to write a closure.
#[doc(hidden)]
pub trait HasPkValue {
    fn __rustango_pk_value_impl(&self) -> crate::core::SqlValue;
}

#[cfg(feature = "postgres")]
impl<T: Model + Send> QuerySet<T> {
    /// Count rows matching the queryset's filters. Issue #270 / T1.8
    /// wave 3 — replaces the `Counter` extension trait (deleted).
    /// Accepts any sqlx executor (`&PgPool` or `&mut PgConnection` for
    /// tenancy-scoped reads), same as [`Self::fetch_on`].
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    pub async fn count_on<'c, E>(self, executor: E) -> Result<i64, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        let stmt = Postgres.compile_count(&CountQuery {
            model: select.model,
            where_clause: select.where_clause,
            search: select.search,
        })?;
        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let row = q.fetch_one(executor).await?;
        let count: i64 = sqlx::Row::try_get(&row, 0)?;
        Ok(count)
    }

    /// Run `EXPLAIN [(...)] <select>` against this queryset and return
    /// the planner output as a `Vec<String>` (one row per plan line).
    ///
    /// Use [`Self::explain_on`] for full control over the executor +
    /// `EXPLAIN` options. This shorthand runs against `&PgPool` with
    /// the default options (plain `EXPLAIN`, no `ANALYZE` — safe to
    /// call without executing the query).
    ///
    /// ```ignore
    /// use rustango::sql::ExplainOptions;
    /// let plan = Post::objects()
    ///     .where_(Post::author_id.eq(7_i64))
    ///     .explain(&pool)
    ///     .await?;
    /// for line in plan { println!("{line}"); }
    /// ```
    ///
    /// # Errors
    /// SQL-writing or driver failures from the EXPLAIN.
    pub async fn explain(self, pool: &PgPool) -> Result<Vec<String>, ExecError> {
        self.explain_on(pool, ExplainOptions::default()).await
    }

    /// Like [`Self::explain`] but accepts any sqlx executor + custom
    /// [`ExplainOptions`]. Setting `analyze = true` actually runs the
    /// query — caveat: side effects, slow scans — so it's opt-in.
    ///
    /// # Errors
    /// As [`Self::explain`].
    pub async fn explain_on<'c, E>(
        self,
        executor: E,
        options: ExplainOptions,
    ) -> Result<Vec<String>, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;
        let mut sql = String::with_capacity(stmt.sql.len() + 32);
        sql.push_str("EXPLAIN ");
        let prefix = options.to_clause();
        if !prefix.is_empty() {
            sql.push_str(&prefix);
            sql.push(' ');
        }
        sql.push_str(&stmt.sql);

        let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
        for value in stmt.params {
            q = bind_query(q, value);
        }
        let rows = q.fetch_all(executor).await?;
        let mut out = Vec::with_capacity(rows.len());
        // EXPLAIN's row-shape varies by `FORMAT`: text/yaml/xml come
        // back as `TEXT`, but `FORMAT JSON` returns column 0 as the
        // `JSON` SQL type. Try the json decoder first when that's
        // the requested format; otherwise the text path.
        for row in &rows {
            let line: String = match options.format {
                ExplainFormat::Json => {
                    let v: serde_json::Value = sqlx::Row::try_get(row, 0)?;
                    v.to_string()
                }
                ExplainFormat::Text | ExplainFormat::Yaml | ExplainFormat::Xml => {
                    sqlx::Row::try_get(row, 0)?
                }
            };
            out.push(line);
        }
        Ok(out)
    }
}

// ====================================================================
// EXPLAIN — extracted to explain.rs (#116 step 5)
// ====================================================================

mod explain;
pub use explain::{explain_pool, ExplainFormat, ExplainOptions};

/// Extension trait that drives a `QuerySet` to a bulk `DELETE`.
///
/// Pulled in via `use rustango::sql::Deleter;`.
#[cfg(feature = "postgres")]
#[cfg(feature = "postgres")]
impl<T: Model + Send> QuerySet<T> {
    /// Bulk-DELETE every row matching this queryset's filters. Issue
    /// #270 / T1.8 wave 3 — replaces the `Deleter` extension trait
    /// (deleted). Accepts any sqlx executor (`&PgPool` or `&mut
    /// PgConnection` for tenancy-scoped writes), same as
    /// [`Self::fetch_on`]. Returns rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    pub async fn delete_on<'c, E>(self, executor: E) -> Result<u64, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let query = self.compile_delete()?;
        delete_on(executor, &query).await
    }
}

#[cfg(feature = "postgres")]
impl<T: Model + Send> UpdateBuilder<T> {
    /// Compile + execute this `UpdateBuilder` against any sqlx
    /// executor. Issue #270 / T1.8 wave 3 — replaces the `Updater`
    /// extension trait (deleted). Returns rows affected.
    ///
    /// # Errors
    /// Returns [`ExecError`] for schema, SQL-writing, or driver failures.
    pub async fn execute_on<'c, E>(self, executor: E) -> Result<u64, ExecError>
    where
        E: sqlx::Executor<'c, Database = sqlx::Postgres>,
    {
        let query = self.compile()?;
        update_on(executor, &query).await
    }
}

/// Backend-agnostic counterpart of [`Updater`]. `UpdateBuilder` gets
/// an `execute_pool(&Pool)` method via this trait that dispatches the
/// compiled `UpdateQuery` through [`update_pool`] — works on any
/// backend rustango supports.
///
/// Pulled in via `use rustango::sql::UpdaterPool;`.
pub trait UpdaterPool<T: Model + Send> {
    /// Compile and execute the update against `pool`. Returns rows
    /// affected.
    ///
    /// # Errors
    /// As [`Updater::execute`].
    fn execute_pool(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<u64, ExecError>> + Send;
}

impl<T: Model + Send> UpdaterPool<T> for UpdateBuilder<T> {
    async fn execute_pool(self, pool: &Pool) -> Result<u64, ExecError> {
        let query = self.compile()?;
        update_pool(pool, &query).await
    }
}

/// Match on `SqlValue` and bind to a sqlx query builder. Used twice below for
/// `Query` and `QueryAs`, which don't share a bind trait. PG-only — the
/// macro depends on `sqlx::types::Json` round-tripping through
/// `PgArguments` and `SqlValue::Array` binding as a typed PG array,
/// neither of which exists on MySQL / SQLite. The bi-directional
/// counterparts are `bind_match_mysql!` + `bind_match_sqlite!`.
// Macros are made visible to sibling modules below via `pub(super) use`.
#[cfg(feature = "postgres")]
macro_rules! bind_match {
    ($q:expr, $value:expr) => {
        match $value {
            // `None::<String>` produces a typed NULL Postgres accepts in any context.
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => $q.bind(v),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            // PG range literal — text-bound, implicit-cast by PG to
            // the column's range type. Issue #31.
            SqlValue::RangeLiteral(s) => $q.bind(s),
            // PG single-parameter array (issue #30). v1 supports
            // I32/I64/String/Bool elements; other element kinds
            // panic at bind time. Homogeneous-element arrays are
            // required by PG's typed array shape.
            SqlValue::Array(elems) => match elems.first() {
                None => $q.bind(Vec::<i32>::new()),
                Some(SqlValue::I64(_)) => {
                    let v: Vec<i64> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::I64(n) = e { Some(n) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::I32(_)) => {
                    let v: Vec<i32> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::I32(n) = e { Some(n) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::String(_)) => {
                    let v: Vec<String> = elems
                        .into_iter()
                        .filter_map(|e| {
                            if let SqlValue::String(s) = e {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .collect();
                    $q.bind(v)
                }
                Some(SqlValue::Bool(_)) => {
                    let v: Vec<bool> = elems
                        .into_iter()
                        .filter_map(|e| if let SqlValue::Bool(b) = e { Some(b) } else { None })
                        .collect();
                    $q.bind(v)
                }
                Some(_) => unreachable!(
                    "SqlValue::Array elements other than I32/I64/String/Bool are not yet supported (v1, issue #30)"
                ),
            },
        }
    };
}

/// MySQL-only counterpart of [`bind_match`]. MySQL has no array
/// type, so the `Array` arm is `unreachable!()` — the SQL writer
/// rejects array operators via `write_array_op` before any bind
/// is attempted. Otherwise identical to the PG bind_match. Issue
/// #30.
#[cfg(feature = "mysql")]
macro_rules! bind_match_mysql {
    ($q:expr, $value:expr) => {
        match $value {
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            SqlValue::Decimal(v) => $q.bind(v),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            SqlValue::Array(_) => unreachable!(
                "MySQL has no array type; `write_array_op` rejects before bind. Issue #30."
            ),
            SqlValue::RangeLiteral(_) => unreachable!(
                "MySQL has no range type; `write_range_op` rejects before bind. Issue #31."
            ),
        }
    };
}

/// SQLite-only counterpart: `sqlx-sqlite` doesn't ship a
/// `rust_decimal::Decimal: Type<Sqlite>` impl (only PG + MySQL get
/// that via sqlx's `rust_decimal` feature), so the `Decimal` arm
/// here serializes via `to_string()` and lands on SQLite's NUMERIC
/// affinity as TEXT — round-trips through the matching `try_get::<String>`
/// path in `row_to_json_sqlite`.
#[cfg(feature = "sqlite")]
macro_rules! bind_match_sqlite {
    ($q:expr, $value:expr) => {
        match $value {
            SqlValue::Null => $q.bind(None::<String>),
            SqlValue::I16(v) => $q.bind(v),
            SqlValue::I32(v) => $q.bind(v),
            SqlValue::I64(v) => $q.bind(v),
            SqlValue::F32(v) => $q.bind(v),
            SqlValue::F64(v) => $q.bind(v),
            SqlValue::Bool(v) => $q.bind(v),
            SqlValue::String(v) => $q.bind(v),
            SqlValue::DateTime(v) => $q.bind(v),
            SqlValue::Date(v) => $q.bind(v),
            SqlValue::Time(v) => $q.bind(v),
            SqlValue::Uuid(v) => $q.bind(v),
            SqlValue::Json(v) => $q.bind(sqlx::types::Json(v)),
            // sqlite-only string round-trip — see macro doc.
            SqlValue::Decimal(v) => $q.bind(v.to_string()),
            SqlValue::Binary(v) => $q.bind(v),
            SqlValue::List(_) => {
                unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
            }
            // SQLite has no array type — the writer rejects array
            // ops via `write_array_op` long before bind is reached.
            SqlValue::Array(_) => unreachable!(
                "SQLite has no array type; `write_array_op` rejects before bind. Issue #30."
            ),
            SqlValue::RangeLiteral(_) => unreachable!(
                "SQLite has no range type; `write_range_op` rejects before bind. Issue #31."
            ),
        }
    };
}

// Sibling-module access to the bind macros (values.rs and future
// extractions need them at `super::` scope).
#[cfg(feature = "postgres")]
pub(super) use bind_match;
#[cfg(feature = "mysql")]
pub(super) use bind_match_mysql;
#[cfg(feature = "sqlite")]
pub(super) use bind_match_sqlite;

#[cfg(feature = "postgres")]
fn bind_query_as<T>(
    q: QueryAs<'_, sqlx::Postgres, T, PgArguments>,
    value: SqlValue,
) -> QueryAs<'_, sqlx::Postgres, T, PgArguments> {
    bind_match!(q, value)
}

#[cfg(feature = "postgres")]
pub(super) fn bind_query(
    q: Query<'_, sqlx::Postgres, PgArguments>,
    value: SqlValue,
) -> Query<'_, sqlx::Postgres, PgArguments> {
    bind_match!(q, value)
}

// ------------------------------------------------------------------ bulk UPDATE

// ------------------------------------------------------------------ raw SQL escape hatch

/// Like [`raw_query`] but accepts any sqlx executor.
///
/// # Errors
/// As [`raw_query`].
#[cfg(feature = "postgres")]
pub async fn raw_query_on<'c, T, E>(
    sql: &str,
    binds: Vec<SqlValue>,
    executor: E,
) -> Result<Vec<T>, ExecError>
where
    T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as(sql);
    for b in binds {
        q = bind_query_as(q, b);
    }
    Ok(q.fetch_all(executor).await?)
}

// ------------------------------------------------------------------ aggregate

/// Like [`fetch_aggregate`] but accepts any sqlx executor.
///
/// # Errors
/// As [`fetch_aggregate`].
#[cfg(feature = "postgres")]
pub async fn fetch_aggregate_on<'c, E>(
    query: &AggregateQuery,
    executor: E,
) -> Result<Vec<std::collections::HashMap<String, SqlValue>>, ExecError>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    let stmt = Postgres.compile_aggregate(query)?;
    let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
    for p in stmt.params {
        q = bind_query(q, p);
    }
    let raw_rows = q.fetch_all(executor).await?;

    // Collect result column names from the first row's columns.
    let mut out = Vec::with_capacity(raw_rows.len());
    for row in &raw_rows {
        use sqlx::{Column as _, Row as _};
        let mut map = std::collections::HashMap::new();
        for (i, col) in row.columns().iter().enumerate() {
            let name = col.name().to_owned();
            // Try to decode as each possible SqlValue type, falling back to Null.
            // Order matters: try cheaper / more-specific decoders first.
            // PG-specific composite types (jsonb, arrays) handled after
            // scalars so int8/text aren't accidentally decoded as JSON.
            let val: SqlValue = if let Ok(v) = row.try_get::<i64, _>(i) {
                SqlValue::I64(v)
            } else if let Ok(v) = row.try_get::<i32, _>(i) {
                SqlValue::I32(v)
            } else if let Ok(v) = row.try_get::<f64, _>(i) {
                SqlValue::F64(v)
            } else if let Ok(v) = row.try_get::<bool, _>(i) {
                SqlValue::Bool(v)
            } else if let Ok(v) = row.try_get::<String, _>(i) {
                SqlValue::String(v)
            } else if let Ok(v) = row.try_get::<serde_json::Value, _>(i) {
                // jsonb / json — issue #33's jsonb_agg returns this.
                SqlValue::Json(v)
            } else if let Ok(v) = row.try_get::<Vec<String>, _>(i) {
                // text[] / varchar[] — issue #33's array_agg on a text col
                // returns this. Wrap as a JSON array so the aggregate
                // result map stays a Vec<HashMap<String, SqlValue>> shape
                // — SqlValue has no Vec<T> variant.
                SqlValue::Json(serde_json::Value::Array(
                    v.into_iter().map(serde_json::Value::String).collect(),
                ))
            } else if let Ok(v) = row.try_get::<Vec<i64>, _>(i) {
                // bigint[] — array_agg on an integer column.
                SqlValue::Json(serde_json::Value::Array(
                    v.into_iter()
                        .map(|n| serde_json::Value::Number(n.into()))
                        .collect(),
                ))
            } else {
                SqlValue::Null
            };
            map.insert(name, val);
        }
        out.push(map);
    }
    Ok(out)
}

// ====================================================================
// `transaction_pool` + PoolTx — extracted to tx.rs (#116 step 4)
// ====================================================================

mod tx;
pub use tx::{transaction_pool, PoolTx};

// ====================================================================
// atomic() + on_commit() — extracted to atomic.rs (#116 step 4)
// ====================================================================

mod atomic;
pub use atomic::{atomic, on_commit, on_commit_pending};

// ====================================================================
// `&Pool` dispatch — bi-dialect executor surface (v0.23.0-batch5)
// ====================================================================
//
// Phase A of the v0.23.0 executor migration: the **non-`FromRow`**
// operations (insert, update, delete, count, bulk_insert, bulk_update,
// raw_execute) now have `_pool` variants that accept a [`super::Pool`]
// and dispatch to the right sqlx driver. SQL is compiled via
// `pool.dialect()`, so the same call works against either backend.
//
// The `FromRow`-bound operations (select_rows, fetch, insert_returning,
// fetch_aggregate, raw_query, fetch_with_prefetch) stay
// `&PgPool`-typed for now — they require macro changes so models
// derive `FromRow<MySqlRow>` alongside `FromRow<PgRow>`. Phase B
// in batch6 covers that.
//
// Existing `&PgPool` callers keep working — we don't touch the
// existing functions. New code that already has `&Pool` (e.g. via
// `Pool::connect_from_env`) can call the `_pool` variants directly.

use super::Pool;

/// Bind a `Query<MySql, MySqlArguments>` from a `SqlValue`. Mirrors
/// the Postgres-typed [`bind_query`] using the same polymorphic
/// `bind_match!` body.
#[cfg(feature = "mysql")]
pub(super) fn bind_query_my(
    q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    value: SqlValue,
) -> sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    bind_match_mysql!(q, value)
}

/// SQLite counterpart of [`bind_query_my`] / [`bind_query`]. sqlx-sqlite
/// supports the same `bind` API for the scalar `SqlValue` variants this
/// crate emits (chrono types route through the `chrono` feature, JSON
/// values go through the `json` feature into TEXT — both feature flags
/// are pulled in by the runtime feature set when `sqlite` is on).
#[cfg(feature = "sqlite")]
pub(super) fn bind_query_sqlite<'a>(
    q: sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>>,
    value: SqlValue,
) -> sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'a>> {
    bind_match_sqlite!(q, value)
}

/// `INSERT` against either backend. Equivalent to [`insert`] but
/// dispatches via [`Pool`] — runs against the dialect's compiled SQL
/// and the matching sqlx driver.
///
/// # Errors
/// As [`insert`].
pub async fn insert_pool(pool: &Pool, query: &InsertQuery) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = pool.dialect().compile_insert(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await?;
    Ok(())
}

/// `INSERT … RETURNING <cols>` (Postgres) or `INSERT … ; SELECT
/// LAST_INSERT_ID()` (MySQL) against either backend. Returns the
/// auto-assigned PK as `i64`.
///
/// MySQL contract:
/// - The query's `returning` list must contain exactly one column,
///   and that column must be the model's `Auto<T>` PK. MySQL's
///   `LAST_INSERT_ID()` only reports the most recently auto-generated
///   value of an `AUTO_INCREMENT` column, so multi-column `RETURNING`
///   is not expressible in MySQL syntax.
/// - The INSERT and `SELECT LAST_INSERT_ID()` run on the **same
///   acquired connection** — `LAST_INSERT_ID()` is connection-scoped,
///   so reading it on a fresh checkout would see a stale (or zero)
///   value if another task ran an INSERT in between.
///
/// Postgres contract: the IR's `returning` list is honored as-is and
/// the row is returned with all requested columns (the executor's
/// caller pulls each via `try_get`).
///
/// # Errors
/// - [`ExecError::EmptyReturning`] when `query.returning` is empty.
/// - [`SqlError::OperatorNotSupportedInDialect`] from the writer when
///   MySQL is asked for a multi-column RETURNING (translation isn't
///   expressible in MySQL syntax).
/// - Validation, SQL-writing, or driver failures otherwise.
///
/// Returns the PG `PgRow` directly when the pool is Postgres so
/// existing callers can use `try_get` for any column. For MySQL,
/// returns the single auto-assigned i64 PK wrapped in
/// [`InsertReturningPool::MySqlAutoId`] — callers handle the two
/// shapes with a `match`.
pub async fn insert_returning_pool(
    pool: &Pool,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError> {
    crate::test_assertions::query_counter::bump();
    query.validate()?;
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let row = insert_returning_on(pg, query).await?;
            Ok(InsertReturningPool::PgRow(row))
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            // Compile a plain INSERT (no RETURNING — MySQL can't
            // express it) and run it + LAST_INSERT_ID() on the same
            // checked-out connection.
            let plain = InsertQuery {
                model: query.model,
                columns: query.columns.clone(),
                values: query.values.clone(),
                returning: ::std::vec::Vec::new(),
                on_conflict: query.on_conflict.clone(),
            };
            let stmt = pool.dialect().compile_insert(&plain)?;
            let mut conn = my.acquire().await?;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            q.execute(&mut *conn).await?;
            // SELECT LAST_INSERT_ID() — connection-scoped, returns
            // the most recently AUTO_INCREMENT-assigned value on
            // *this* connection.
            use sqlx::Row as _;
            let row = sqlx::query("SELECT LAST_INSERT_ID()")
                .fetch_one(&mut *conn)
                .await?;
            // sqlx-mysql decodes LAST_INSERT_ID() as u64; we surface
            // it as i64 to match the Auto<i64>/Auto<i32> convention
            // and the rest of the framework.
            let id_u64: u64 = row.try_get::<u64, _>(0)?;
            // i64::try_from would only fail at >2^63 IDs — a 9.2e18
            // table that no realistic app will hit.
            let id = i64::try_from(id_u64).unwrap_or(i64::MAX);
            Ok(InsertReturningPool::MySqlAutoId(id))
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            // SQLite ≥ 3.35 supports `INSERT … RETURNING <cols>` with
            // the same shape as Postgres, so the flow mirrors PG: bind
            // params, fetch the row, hand it to the macro-emitted
            // `__rustango_assign_from_sqlite_row` body via
            // `apply_auto_pk`.
            let stmt = pool.dialect().compile_insert(query)?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let row = q.fetch_one(sq).await?;
            Ok(InsertReturningPool::SqliteRow(row))
        }
    }
}

/// Two-shape return from [`insert_returning_pool`] — a full Postgres
/// row (caller picks columns via `try_get`) or a single MySQL
/// auto-assigned `i64` PK from `LAST_INSERT_ID()`.
///
/// Macro-generated `Model::insert_pool` will pattern-match this:
/// store every `RETURNING` column from the PG variant; store the
/// single `i64` into the model's `Auto<T>` PK field on the MySQL
/// variant.
pub enum InsertReturningPool {
    #[cfg(feature = "postgres")]
    PgRow(PgRow),
    #[cfg(feature = "mysql")]
    MySqlAutoId(i64),
    #[cfg(feature = "sqlite")]
    SqliteRow(sqlx::sqlite::SqliteRow),
}

impl ::core::fmt::Debug for InsertReturningPool {
    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
        match self {
            #[cfg(feature = "postgres")]
            Self::PgRow(_) => f.debug_tuple("PgRow").field(&"<PgRow>").finish(),
            #[cfg(feature = "mysql")]
            Self::MySqlAutoId(id) => f.debug_tuple("MySqlAutoId").field(id).finish(),
            #[cfg(feature = "sqlite")]
            Self::SqliteRow(_) => f.debug_tuple("SqliteRow").field(&"<SqliteRow>").finish(),
        }
    }
}

/// `UPDATE` against either backend; returns rows affected.
///
/// # Errors
/// As [`update`].
pub async fn update_pool(pool: &Pool, query: &UpdateQuery) -> Result<u64, ExecError> {
    let stmt = pool.dialect().compile_update(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await
}

/// `DELETE` against either backend; returns rows affected.
///
/// # Errors
/// As [`delete`].
pub async fn delete_pool(pool: &Pool, query: &DeleteQuery) -> Result<u64, ExecError> {
    let stmt = pool.dialect().compile_delete(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await
}

/// `SELECT COUNT(*)` against either backend.
///
/// # Errors
/// As [`count_rows`].
pub async fn count_rows_pool(pool: &Pool, query: &CountQuery) -> Result<i64, ExecError> {
    let stmt = pool.dialect().compile_count(query)?;
    fetch_scalar_pool(pool, &stmt.sql, stmt.params).await
}

/// Multi-row `INSERT`. Bypasses any `Auto<T>` PK reconciliation that
/// [`bulk_insert`] does for Postgres' `RETURNING` shape — the macro
/// layer in batch6 will route Auto<T>-bearing models to a different
/// path on MySQL (`LAST_INSERT_ID()` follow-up).
///
/// # Errors
/// As [`bulk_insert`].
pub async fn bulk_insert_pool(pool: &Pool, query: &BulkInsertQuery) -> Result<(), ExecError> {
    if query.rows.is_empty() {
        return Ok(());
    }
    let stmt = pool.dialect().compile_bulk_insert(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await?;
    Ok(())
}

/// `UPDATE … FROM (VALUES …)` (Postgres) / `UPDATE … INNER JOIN
/// (VALUES …)` (MySQL); returns rows affected.
///
/// # Errors
/// As [`bulk_update`].
pub async fn bulk_update_pool(pool: &Pool, query: &BulkUpdateQuery) -> Result<u64, ExecError> {
    if query.rows.is_empty() {
        return Ok(0);
    }
    let stmt = pool.dialect().compile_bulk_update(query)?;
    execute_pool(pool, &stmt.sql, stmt.params).await
}

/// Execute arbitrary SQL with bound `SqlValue` params; returns rows
/// affected. SQL must use the **dialect's** placeholder shape (`$1`
/// for Postgres, `?` for MySQL) — read it from `pool.dialect().placeholder(n)`
/// when constructing dynamic queries.
///
/// # Errors
/// Driver / SQL failures.
pub async fn raw_execute_pool(
    pool: &Pool,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    execute_pool(pool, sql, binds).await
}

// ---- internal dispatch helpers ----

/// Execute a parameterized statement that doesn't return rows. Used
/// by every non-`FromRow` `_pool` function.
async fn execute_pool(pool: &Pool, sql: &str, binds: Vec<SqlValue>) -> Result<u64, ExecError> {
    // #431 — bump the per-task query counter when an `assert_num_queries`
    // scope is active. No-op in production (try_with returns Err).
    crate::test_assertions::query_counter::bump();
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
            for v in binds {
                q = bind_query(q, v);
            }
            Ok(q.execute(pg).await?.rows_affected())
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_my(q, v);
            }
            Ok(q.execute(my).await?.rows_affected())
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.execute(sq).await?.rows_affected())
        }
    }
}

// ====================================================================
// `&mut PoolTx` dispatch — tri-dialect transaction executor surface
// ====================================================================
//
// Mirrors the `_pool` non-`FromRow` helpers above but executes against
// an open transaction (a `PoolTx` obtained from `transaction_pool`).
// Each helper compiles SQL through `tx.dialect()` and dispatches to
// the correct sqlx driver branch via `match tx { ... }`.
//
// `execute_tx` is the internal building-block (private); the rest
// are the public API consumed by macro-generated `_tx` model methods.

async fn execute_tx(
    tx: &mut PoolTx<'_>,
    sql: &str,
    binds: Vec<SqlValue>,
) -> Result<u64, ExecError> {
    match tx {
        #[cfg(feature = "postgres")]
        PoolTx::Postgres(t) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
            for v in binds {
                q = bind_query(q, v);
            }
            Ok(q.execute(&mut **t).await?.rows_affected())
        }
        #[cfg(feature = "mysql")]
        PoolTx::Mysql(t) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_my(q, v);
            }
            Ok(q.execute(&mut **t).await?.rows_affected())
        }
        #[cfg(feature = "sqlite")]
        PoolTx::Sqlite(t) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_sqlite(q, v);
            }
            Ok(q.execute(&mut **t).await?.rows_affected())
        }
    }
}

/// `INSERT` inside an open transaction. Equivalent to [`insert_pool`]
/// but executes against `tx` so the write participates in the
/// caller's transaction boundary.
///
/// # Errors
/// As [`insert_pool`].
pub async fn insert_tx(tx: &mut PoolTx<'_>, query: &InsertQuery) -> Result<(), ExecError> {
    query.validate()?;
    let stmt = tx.dialect().compile_insert(query)?;
    execute_tx(tx, &stmt.sql, stmt.params).await?;
    Ok(())
}

/// `INSERT … RETURNING` / `LAST_INSERT_ID()` inside an open
/// transaction. Returns the same [`InsertReturningPool`] shape as
/// [`insert_returning_pool`], but all operations run against `tx`.
///
/// MySQL contract: the INSERT and `SELECT LAST_INSERT_ID()` both
/// execute on the transaction's connection, so the auto-id is always
/// correct even under concurrent inserts on other connections.
///
/// # Errors
/// As [`insert_returning_pool`].
pub async fn insert_returning_tx(
    tx: &mut PoolTx<'_>,
    query: &InsertQuery,
) -> Result<InsertReturningPool, ExecError> {
    query.validate()?;
    if query.returning.is_empty() {
        return Err(ExecError::EmptyReturning);
    }
    match tx {
        #[cfg(feature = "postgres")]
        PoolTx::Postgres(t) => {
            let row = insert_returning_on(&mut **t, query).await?;
            Ok(InsertReturningPool::PgRow(row))
        }
        #[cfg(feature = "mysql")]
        PoolTx::Mysql(t) => {
            let plain = InsertQuery {
                model: query.model,
                columns: query.columns.clone(),
                values: query.values.clone(),
                returning: ::std::vec::Vec::new(),
                on_conflict: query.on_conflict.clone(),
            };
            let stmt = super::mysql::DIALECT.compile_insert(&plain)?;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            q.execute(&mut **t).await?;
            use sqlx::Row as _;
            let row = sqlx::query("SELECT LAST_INSERT_ID()")
                .fetch_one(&mut **t)
                .await?;
            let id_u64: u64 = row.try_get::<u64, _>(0)?;
            let id = i64::try_from(id_u64).unwrap_or(i64::MAX);
            Ok(InsertReturningPool::MySqlAutoId(id))
        }
        #[cfg(feature = "sqlite")]
        PoolTx::Sqlite(t) => {
            let stmt = super::sqlite::DIALECT.compile_insert(query)?;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let row = q.fetch_one(&mut **t).await?;
            Ok(InsertReturningPool::SqliteRow(row))
        }
    }
}

/// `UPDATE` inside an open transaction; returns rows affected.
///
/// # Errors
/// As [`update_pool`].
pub async fn update_tx(tx: &mut PoolTx<'_>, query: &UpdateQuery) -> Result<u64, ExecError> {
    let stmt = tx.dialect().compile_update(query)?;
    execute_tx(tx, &stmt.sql, stmt.params).await
}

/// `DELETE` inside an open transaction; returns rows affected.
///
/// # Errors
/// As [`delete_pool`].
pub async fn delete_tx(tx: &mut PoolTx<'_>, query: &DeleteQuery) -> Result<u64, ExecError> {
    let stmt = tx.dialect().compile_delete(query)?;
    execute_tx(tx, &stmt.sql, stmt.params).await
}

/// `SELECT` inside an open transaction, with optional `select_related`
/// join decoding. Mirrors [`select_rows_pool_with_related`] but
/// executes against `tx`.
///
/// # Errors
/// As [`select_rows_pool_with_related`].
pub async fn select_rows_tx_with_related<T>(
    tx: &mut PoolTx<'_>,
    query: &SelectQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    let stmt = tx.dialect().compile_select(query)?;
    let aliases: Vec<&'static str> = query.joins.iter().map(|j| j.alias).collect();
    match tx {
        #[cfg(feature = "postgres")]
        PoolTx::Postgres(t) => {
            if aliases.is_empty() {
                let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                    sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as(q, v);
                }
                return Ok(q.fetch_all(&mut **t).await?);
            }
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let raw_rows = q.fetch_all(&mut **t).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut item = T::from_row(row)?;
                for alias in &aliases {
                    let _ = item.__rustango_load_related(row, alias, alias)?;
                }
                out.push(item);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        PoolTx::Mysql(t) => {
            if aliases.is_empty() {
                let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                    sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as_my(q, v);
                }
                return Ok(q.fetch_all(&mut **t).await?);
            }
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let raw_rows = q.fetch_all(&mut **t).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut item = <T as sqlx::FromRow<sqlx::mysql::MySqlRow>>::from_row(row)?;
                for alias in &aliases {
                    let _ = item.__rustango_load_related_my(row, alias, alias)?;
                }
                out.push(item);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        PoolTx::Sqlite(t) => {
            if aliases.is_empty() {
                let mut q: sqlx::query::QueryAs<
                    '_,
                    sqlx::Sqlite,
                    T,
                    sqlx::sqlite::SqliteArguments<'_>,
                > = sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as_sqlite(q, v);
                }
                return Ok(q.fetch_all(&mut **t).await?);
            }
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let raw_rows = q.fetch_all(&mut **t).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut item = <T as sqlx::FromRow<sqlx::sqlite::SqliteRow>>::from_row(row)?;
                for alias in &aliases {
                    let _ = item.__rustango_load_related_sqlite(row, alias, alias)?;
                }
                out.push(item);
            }
            Ok(out)
        }
    }
}

/// Run a SELECT that returns a single scalar `i64` (used by
/// [`count_rows_pool`]). Inlined per-backend so we can use the
/// driver-specific `Row::try_get` directly.
async fn fetch_scalar_pool(pool: &Pool, sql: &str, binds: Vec<SqlValue>) -> Result<i64, ExecError> {
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            use sqlx::Row as _;
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(sql);
            for v in binds {
                q = bind_query(q, v);
            }
            let row = q.fetch_one(pg).await?;
            Ok(row.try_get::<i64, _>(0)?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            use sqlx::Row as _;
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_my(q, v);
            }
            let row = q.fetch_one(my).await?;
            Ok(row.try_get::<i64, _>(0)?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            use sqlx::Row as _;
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(sql);
            for v in binds {
                q = bind_query_sqlite(q, v);
            }
            let row = q.fetch_one(sq).await?;
            Ok(row.try_get::<i64, _>(0)?)
        }
    }
}

// ====================================================================
// `&Pool` FromRow dispatch — Phase B (v0.23.0-batch6)
// ====================================================================
//
// `select_rows_pool` / `Fetcher::fetch` for `&Pool`. The trait bound on
// `T` needs to flex: when rustango is built with the `mysql` feature,
// it must also include `FromRow<MySqlRow>`; without it, the MySql
// variant doesn't exist and the bound shouldn't either. The
// [`MaybeMyFromRow`] marker trait below absorbs that conditionality —
// it's auto-implemented for every type when `mysql` is off, and
// requires `FromRow<MySqlRow>` when on. Models derived via
// `#[derive(Model)]` get the right impl automatically: the proc macro
// emits a call to the cfg-gated `__impl_my_from_row!` macro_rules so
// the MySQL impl materializes when (and only when) it's needed.

/// Marker trait used as a feature-gated `FromRow<MySqlRow>` bound on
/// the `_pool` `FromRow`-using executor functions. Auto-implemented
/// for every `T` when rustango is built without the `mysql` feature
/// (so PG-only call sites compile unchanged); requires
/// `FromRow<MySqlRow>` when `mysql` is on.
#[cfg(feature = "mysql")]
pub trait MaybeMyFromRow: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::mysql::MySqlRow> {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyFromRow {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyFromRow for T {}

/// MySQL counterpart of [`LoadRelated`]. The proc-macro emits this
/// alongside the existing `LoadRelated` impl whenever rustango is
/// built with the `mysql` feature, so `select_related` joins can
/// stitch parents onto FK fields when decoding from a `MySqlRow`.
///
/// FK-less models get a no-op impl; the `MaybeMyLoadRelated` marker
/// trait wraps this in the same cfg-gated way as
/// `MaybeMyFromRow` so executor bounds resolve in either feature
/// config.
#[cfg(feature = "mysql")]
pub trait LoadRelatedMy {
    /// Same contract as [`LoadRelated::__rustango_load_related`] —
    /// returns `Ok(true)` when `field_name` matched a known FK and
    /// the parent was decoded successfully, `Ok(false)` for unknown
    /// field names (graceful skip).
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related_my(
        &mut self,
        row: &sqlx::mysql::MySqlRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

/// Marker trait used as a feature-gated `LoadRelatedMy` bound on
/// future `_pool` join-decoding executor functions. Same shape as
/// [`MaybeMyFromRow`] — auto-implemented for every `T` when
/// rustango is built without `mysql`; requires `LoadRelatedMy`
/// when on.
#[cfg(feature = "mysql")]
pub trait MaybeMyLoadRelated: LoadRelatedMy {}
#[cfg(feature = "mysql")]
impl<T> MaybeMyLoadRelated for T where T: LoadRelatedMy {}
#[cfg(not(feature = "mysql"))]
pub trait MaybeMyLoadRelated {}
#[cfg(not(feature = "mysql"))]
impl<T> MaybeMyLoadRelated for T {}

// ---- v0.35 — Postgres parallel of the MySQL/SQLite marker traits ----
//
// Lets the tri-dialect `_pool` executor functions bound `T` with
// `MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow` so the
// signature is satisfiable in any feature configuration. When
// `postgres` is on the trait requires `FromRow<PgRow>`; when off
// it's a marker trait blanket-impl'd for every `T`, so the bound
// trivially holds and the `Pool::Postgres` arm (also gated) never
// instantiates.

#[cfg(feature = "postgres")]
pub trait MaybePgFromRow: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> {}
#[cfg(feature = "postgres")]
impl<T> MaybePgFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> {}
#[cfg(not(feature = "postgres"))]
pub trait MaybePgFromRow {}
#[cfg(not(feature = "postgres"))]
impl<T> MaybePgFromRow for T {}

// ---- v0.27 Phase 3 — SQLite parallels of the MySQL marker traits ----
//
// Same shape as `MaybeMyFromRow` / `LoadRelatedMy` /
// `MaybeMyLoadRelated`, gated on `sqlite` instead of `mysql`. The
// `_pool` executor functions add these as additional bounds on `T`
// so a `Pool::Sqlite` arm can decode `T` from a `SqliteRow`.

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteFromRow: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteFromRow for T where T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> {}
#[cfg(not(feature = "sqlite"))]
#[allow(dead_code)]
pub trait MaybeSqliteFromRow {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteFromRow for T {}

/// SQLite counterpart of [`LoadRelated`] / [`LoadRelatedMy`]. The
/// proc-macro emits this alongside the Postgres + MySQL impls so
/// `select_related` joins can stitch parents onto FK fields when
/// decoding from a `SqliteRow`.
#[cfg(feature = "sqlite")]
pub trait LoadRelatedSqlite {
    /// Same contract as [`LoadRelated::__rustango_load_related`].
    ///
    /// # Errors
    /// `sqlx::Error` from `try_get` decoding the joined columns.
    fn __rustango_load_related_sqlite(
        &mut self,
        row: &sqlx::sqlite::SqliteRow,
        field_name: &str,
        alias: &str,
    ) -> Result<bool, sqlx::Error>;
}

#[cfg(feature = "sqlite")]
pub trait MaybeSqliteLoadRelated: LoadRelatedSqlite {}
#[cfg(feature = "sqlite")]
impl<T> MaybeSqliteLoadRelated for T where T: LoadRelatedSqlite {}
#[cfg(not(feature = "sqlite"))]
#[allow(dead_code)]
pub trait MaybeSqliteLoadRelated {}
#[cfg(not(feature = "sqlite"))]
impl<T> MaybeSqliteLoadRelated for T {}

/// Run a `SelectQuery` against either backend and decode each row
/// into `T`. Equivalent to [`select_rows`] but takes [`Pool`] and
/// dispatches per backend. Joins (`select_related`) are not yet
/// supported on the `&Pool` path — use the `&PgPool` variant on
/// Postgres until the join decoder migrates in batch7.
///
/// # Errors
/// As [`select_rows`].
pub async fn select_rows_pool<T>(pool: &Pool, query: &SelectQuery) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

/// Single-row variant of [`select_rows_pool`]. Returns `Ok(None)`
/// when no rows match.
///
/// # Errors
/// As [`select_one_row`] but routed through `&Pool`.
pub async fn select_one_row_pool<T>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Option<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_optional(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_optional(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_optional(sq).await?)
        }
    }
}

/// MySQL-typed `QueryAs` binding helper, symmetric with [`bind_query_as`].
#[cfg(feature = "mysql")]
fn bind_query_as_my<T>(
    q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments>,
    value: SqlValue,
) -> sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> {
    bind_match_mysql!(q, value)
}

/// SQLite-typed `QueryAs` binding helper, symmetric with [`bind_query_as`].
#[cfg(feature = "sqlite")]
fn bind_query_as_sqlite<'a, T>(
    q: sqlx::query::QueryAs<'a, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'a>>,
    value: SqlValue,
) -> sqlx::query::QueryAs<'a, sqlx::Sqlite, T, sqlx::sqlite::SqliteArguments<'a>> {
    bind_match_sqlite!(q, value)
}

/// `fetch_aggregate` against either backend — runs an
/// `AggregateQuery` (GROUP BY / HAVING / aggregate exprs) and
/// decodes each row into `T` via `FromRow`. Bi-dialect counterpart
/// of [`fetch_aggregate`].
///
/// Bound on `T` adds [`MaybeMyFromRow`] over the `&PgPool` version's
/// bound — universally satisfied by `#[derive(Model)]` types and by
/// any tuple/struct deriving sqlx's `FromRow`.
///
/// # Errors
/// As [`fetch_aggregate`].
pub async fn fetch_aggregate_pool<T>(
    pool: &Pool,
    query: &AggregateQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    let stmt = pool.dialect().compile_aggregate(query)?;
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(&stmt.sql);
            for v in stmt.params {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

// ====================================================================
// Pure projection — Django `.values()` / `.values_list()` (issue #22)
// Extracted to values.rs (#116 step 3).
// ====================================================================

mod values;
#[allow(unused_imports)]
pub use values::{
    fetch_values_dict, fetch_values_flat, fetch_values_list, MaybeMyScalar, MaybePgScalar,
    MaybeSqliteScalar,
};

/// Execute arbitrary SQL with bound `SqlValue` params and decode each
/// row into `T` via `FromRow`. Bi-dialect counterpart of
/// [`raw_query`].
///
/// SQL must use the **dialect's** placeholder shape (`$1` for
/// Postgres, `?` for MySQL) — read it from `pool.dialect().placeholder(n)`
/// when constructing dynamic queries. Apps writing literal SQL pick
/// the right shape themselves.
///
/// # Errors
/// As [`raw_query`].
pub async fn raw_query_pool<T>(
    sql: &str,
    binds: Vec<SqlValue>,
    pool: &Pool,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as(q, v);
            }
            Ok(q.fetch_all(pg).await?)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as_my(q, v);
            }
            Ok(q.fetch_all(my).await?)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::QueryAs<
                '_,
                sqlx::Sqlite,
                T,
                sqlx::sqlite::SqliteArguments<'_>,
            > = sqlx::query_as::<_, T>(sql);
            for v in binds {
                q = bind_query_as_sqlite(q, v);
            }
            Ok(q.fetch_all(sq).await?)
        }
    }
}

/// Django `.dates(field, kind)` terminal — issue #327. Compiles the
/// underlying queryset to a `SELECT … WHERE …` statement, then wraps
/// it in `SELECT DISTINCT <trunc(col)> FROM (<inner>) ORDER BY d` to
/// extract distinct truncated date values. Params from the WHERE
/// clause pass through unchanged.
///
/// # Errors
/// - [`ExecError::Query`] forwarded from the underlying
///   [`raw_query_pool`] call.
/// - Driver / SQL errors on decode (e.g. NULL date columns surface as
///   sqlx decode errors when the column type isn't `Option<NaiveDate>`).
pub async fn fetch_dates_pool<T: crate::core::Model + Send>(
    pool: &Pool,
    qs: crate::query::DatesQuerySet<T>,
) -> Result<Vec<chrono::NaiveDate>, ExecError> {
    let descending = qs.descending;
    let kind = qs.kind;
    let column = qs.resolve_column()?;
    // Compile the underlying queryset into a SelectQuery — preserves
    // WHERE / JOINs / ORDER BY / LIMIT. The wrap below then ignores
    // the inner ORDER BY (the truncated bucket order is what callers
    // expect from `.dates()`).
    let select_query = qs.qs.compile()?;
    let dialect = pool.dialect();
    let inner = dialect.compile_select(&select_query)?;
    let col_quoted = dialect.quote_ident(column);
    let trunc_sql = kind.trunc_sql(dialect.name(), &col_quoted);
    let order_dir = if descending { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT DISTINCT {trunc_sql} AS rs_dates_bucket FROM ({inner_sql}) AS rs_dates_sub ORDER BY rs_dates_bucket {order_dir}",
        inner_sql = inner.sql,
    );
    let rows: Vec<(chrono::NaiveDate,)> = raw_query_pool(&sql, inner.params, pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Django `.datetimes(field, kind)` terminal — issue #328. Sibling
/// to [`fetch_dates_pool`] with finer granularity (`Hour` / `Minute` /
/// `Second`) and `DateTime<Utc>` return type.
///
/// # Errors
/// Same shape as [`fetch_dates_pool`].
pub async fn fetch_datetimes_pool<T: crate::core::Model + Send>(
    pool: &Pool,
    qs: crate::query::DateTimesQuerySet<T>,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>, ExecError> {
    let descending = qs.descending;
    let kind = qs.kind;
    let column = qs.resolve_column()?;
    let select_query = qs.qs.compile()?;
    let dialect = pool.dialect();
    let inner = dialect.compile_select(&select_query)?;
    let col_quoted = dialect.quote_ident(column);
    let trunc_sql = kind.trunc_sql(dialect.name(), &col_quoted);
    let order_dir = if descending { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT DISTINCT {trunc_sql} AS rs_datetimes_bucket FROM ({inner_sql}) AS rs_datetimes_sub ORDER BY rs_datetimes_bucket {order_dir}",
        inner_sql = inner.sql,
    );
    let rows: Vec<(chrono::DateTime<chrono::Utc>,)> =
        raw_query_pool(&sql, inner.params, pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// `Counter::count` against either backend — fills the QuerySet
/// counter gap from batches 5/15. Counts rows matching the queryset's
/// filters via `count_rows_pool`.
///
/// Pulled in via `use rustango::sql::CounterPool;`.
pub trait CounterPool<T: Model + Send> {
    /// Count rows matching the queryset's filters against either backend.
    ///
    /// # Errors
    /// As [`Counter::count`].
    fn count_pool(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<i64, ExecError>> + Send;
}

impl<T: Model + Send> CounterPool<T> for QuerySet<T> {
    async fn count_pool(self, pool: &Pool) -> Result<i64, ExecError> {
        let select = self.compile()?;
        count_rows_pool(
            pool,
            &CountQuery {
                model: select.model,
                where_clause: select.where_clause,
                search: select.search,
            },
        )
        .await
    }
}

/// Django-shape `QuerySet.exists()` + `QuerySet.contains(obj)`
/// (issue #330) — boolean predicates on top of the existing count
/// path.
///
/// `exists_pool` returns `Ok(true)` when at least one row matches the
/// queryset's filters, `Ok(false)` otherwise. Internally runs the
/// same `COUNT(*)` as [`CounterPool::count_pool`] and compares to
/// zero — simple + dialect-agnostic. Future optimization: emit
/// `SELECT 1 FROM … LIMIT 1` instead of `COUNT(*)`, which doesn't
/// scan all matching rows. Tracked as a follow-up; the count
/// path is correct semantically.
///
/// `contains_pk` adds the convenience of "is THIS pk in the
/// queryset?" — looks up the model's primary key column from the
/// schema, builds an `Eq` predicate, and forwards to `exists_pool`.
/// Equivalent to `qs.filter("<pk_col>", pk).exists_pool(pool)` with
/// the column lookup baked in.
///
/// Pulled in via `use rustango::sql::ExistsPool;`.
pub trait ExistsPool<T: Model + Send> {
    /// `Ok(true)` when at least one row matches the queryset's
    /// filters, `Ok(false)` otherwise. #330.
    ///
    /// # Errors
    /// As [`CounterPool::count_pool`].
    fn exists_pool(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;

    /// `Ok(true)` when a row with `pk = pk_value` is contained in the
    /// queryset. #330. Looks up the PK column from `T::SCHEMA`; errors
    /// when the model has no primary key (very rare — view-backed
    /// models without an explicit `#[rustango(primary_key)]`).
    ///
    /// # Errors
    /// As [`Self::exists_pool`], plus a model-without-PK error wrapped
    /// in `ExecError::Query`.
    fn contains_pk(
        self,
        pool: &Pool,
        pk_value: impl Into<crate::core::SqlValue> + Send,
    ) -> impl std::future::Future<Output = Result<bool, ExecError>> + Send;
}

impl<T: Model + Send> ExistsPool<T> for QuerySet<T> {
    async fn exists_pool(self, pool: &Pool) -> Result<bool, ExecError> {
        let count = self.count_pool(pool).await?;
        Ok(count > 0)
    }

    async fn contains_pk(
        self,
        pool: &Pool,
        pk_value: impl Into<crate::core::SqlValue> + Send,
    ) -> Result<bool, ExecError> {
        let Some(pk_field) = T::SCHEMA.primary_key() else {
            return Err(ExecError::Query(crate::core::QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "primary_key".into(),
            }));
        };
        self.filter_op(pk_field.column, crate::core::Op::Eq, pk_value.into())
            .exists_pool(pool)
            .await
    }
}

/// `fetch_paginated` against either backend — fetches a page of rows
/// AND the pre-LIMIT total count in a single SQL round trip via
/// `COUNT(*) OVER ()`. Bi-dialect counterpart of
/// [`QuerySet::fetch_paginated_on`].
///
/// Both PG and MySQL 8.0+ support `COUNT(*) OVER ()` window
/// functions, so the SQL splice is identical across backends — only
/// the placeholder shape and identifier quoting differ, and those
/// already come from `pool.dialect()`.
///
/// MySQL caveat: `COUNT(*) OVER ()` requires MySQL 8.0+ (window
/// functions weren't supported pre-8.0). Apps targeting MySQL 5.7
/// must use a separate `count_rows_pool` + `select_rows_pool`
/// instead.
///
/// Empty result set → `Page { rows: vec![], total: 0 }` (no extra
/// driver round trip wasted on a separate COUNT).
///
/// # Errors
/// As [`QuerySet::fetch_paginated_on`].
pub async fn fetch_paginated_pool<T>(
    qs: crate::query::QuerySet<T>,
    pool: &Pool,
) -> Result<Page<T>, ExecError>
where
    T: Model + MaybePgFromRow + MaybeMyFromRow + MaybeSqliteFromRow + Send + Unpin,
{
    let select = qs.compile()?;
    let stmt = pool.dialect().compile_select(&select)?;
    let sql = inject_total_count(&stmt.sql);

    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            use sqlx::Row as _;
            let raw_rows: Vec<PgRow> = q.fetch_all(pg).await?;
            let total: i64 = raw_rows
                .first()
                .map(|row| row.try_get::<i64, _>("__rustango_total"))
                .transpose()?
                .unwrap_or(0);
            let mut rows = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                rows.push(T::from_row(row)?);
            }
            Ok(Page { rows, total })
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            use sqlx::Row as _;
            let raw_rows: Vec<sqlx::mysql::MySqlRow> = q.fetch_all(my).await?;
            // sqlx-mysql exposes COUNT(*) OVER () as i64 (BIGINT).
            let total: i64 = raw_rows
                .first()
                .map(|row| row.try_get::<i64, _>("__rustango_total"))
                .transpose()?
                .unwrap_or(0);
            let mut rows = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                rows.push(<T as sqlx::FromRow<sqlx::mysql::MySqlRow>>::from_row(row)?);
            }
            Ok(Page { rows, total })
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            use sqlx::Row as _;
            let raw_rows: Vec<sqlx::sqlite::SqliteRow> = q.fetch_all(sq).await?;
            let total: i64 = raw_rows
                .first()
                .map(|row| row.try_get::<i64, _>("__rustango_total"))
                .transpose()?
                .unwrap_or(0);
            let mut rows = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                rows.push(<T as sqlx::FromRow<sqlx::sqlite::SqliteRow>>::from_row(
                    row,
                )?);
            }
            Ok(Page { rows, total })
        }
    }
}

/// `fetch_with_prefetch` against either backend. Bi-dialect counterpart
/// of [`fetch_with_prefetch`] — fetches parents + children in two
/// round trips and stitches each child onto its parent via
/// [`FkPkAccess`]. Bounds add [`MaybeMyFromRow`] + [`MaybeMyLoadRelated`]
/// over the `&PgPool` version's bounds; every `#[derive(Model)]`
/// type satisfies them automatically.
///
/// `FkPkAccess` doesn't need a MySQL counterpart — it reads i64
/// values from struct fields after decoding, with no Row dependency,
/// so the same trait works against either backend.
///
/// # Errors
/// As [`fetch_with_prefetch`].
pub async fn fetch_with_prefetch_pool<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + FkPkAccess
        + Send
        + Unpin,
{
    let parents: Vec<P> = parent_qs.fetch_pool(pool).await?;
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    // Same `SqlValue`-keyed grouping as `fetch_with_prefetch` so
    // non-i64 FK PKs work end-to-end on both Postgres and MySQL.
    let pk_field = P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    let mut parent_pks: Vec<crate::core::SqlValue> = Vec::with_capacity(parents.len());
    for parent in &parents {
        let pk = extract_pk_value(parent);
        if !matches!(pk, crate::core::SqlValue::Null) {
            parent_pks.push(pk);
        }
    }
    {
        let mut seen = std::collections::HashSet::new();
        parent_pks.retain(|v| seen.insert(v.to_display_string()));
    }
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(parent_pks),
        )
        .fetch_pool(pool)
        .await?;

    let mut grouped: std::collections::HashMap<String, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk_value(child_fk_column) else {
            continue;
        };
        grouped
            .entry(fk_pk.to_display_string())
            .or_default()
            .push(child);
    }

    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = extract_pk_value(&parent).to_display_string();
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field;
    Ok(out)
}

/// `fetch_with_prefetch_pool` with a user-supplied **filtered** child
/// queryset — Django's `Prefetch(queryset=...)` shape. Issue #298 /
/// T2.1.
///
/// Drop-in replacement for [`fetch_with_prefetch_pool`] when the
/// caller wants to **filter / order / limit** the child fetch:
///
/// ```ignore
/// use rustango::core::Column as _;
/// use rustango::query::QuerySet;
///
/// // Authors + their PUBLISHED posts, newest first.
/// let published_posts = QuerySet::<Post>::default()
///     .filter("published", true)
///     .order_by(&[("created", true)]);
///
/// let authors_with_posts: Vec<(Author, Vec<Post>)> =
///     fetch_with_prefetch_filtered_pool(
///         Author::objects(),
///         "author",
///         published_posts,
///         &pool,
///     ).await?;
/// ```
///
/// The function:
///
///  1. Compiles `parent_qs`, fetches parents.
///  2. Collects parent PKs.
///  3. Takes the user-supplied `child_qs` and **injects an
///     `AND child_fk_column IN (parent_pks)` predicate** into its
///     filter list (any filters / order / limit the user already
///     wrote are preserved).
///  4. Compiles + executes the augmented child query.
///  5. Groups children by FK PK and stitches.
///
/// # Caveat — global vs. per-parent `LIMIT`
///
/// `child_qs.limit(N)` applies **globally** across the joined fetch,
/// not per parent. Django's per-parent slice on prefetch (`.filter(
/// post_set__lt=...)` semantics + LATERAL JOIN on PG, `ROW_NUMBER()
/// OVER (PARTITION BY ...)` elsewhere) is a follow-up — the per-
/// dialect LATERAL/window-fn dispatch is substantial work.
///
/// For the common shapes ("authors + every published post" / "authors
/// + every post ordered DESC"), this signature is the right tool. For
/// "authors + their N most-recent posts", today the calling pattern
/// is per-parent fetch in a loop (or wait for the LATERAL follow-up).
///
/// # Errors
/// As [`fetch_with_prefetch_pool`].
pub async fn fetch_with_prefetch_filtered_pool<P, C>(
    parent_qs: crate::query::QuerySet<P>,
    child_fk_column: &'static str,
    child_qs: crate::query::QuerySet<C>,
    pool: &Pool,
) -> Result<Vec<(P, Vec<C>)>, ExecError>
where
    P: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + HasPkValue
        + Send
        + Unpin,
    C: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + FkPkAccess
        + Send
        + Unpin,
{
    let parents: Vec<P> = parent_qs.fetch_pool(pool).await?;
    if parents.is_empty() {
        return Ok(Vec::new());
    }

    let pk_field = P::SCHEMA
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: P::SCHEMA.table,
        })?;
    let mut parent_pks: Vec<crate::core::SqlValue> = Vec::with_capacity(parents.len());
    for parent in &parents {
        let pk = extract_pk_value(parent);
        if !matches!(pk, crate::core::SqlValue::Null) {
            parent_pks.push(pk);
        }
    }
    {
        let mut seen = std::collections::HashSet::new();
        parent_pks.retain(|v| seen.insert(v.to_display_string()));
    }
    if parent_pks.is_empty() {
        return Ok(parents.into_iter().map(|p| (p, Vec::new())).collect());
    }

    // Inject the IN-predicate by `filter_op` on top of whatever the
    // user already chained — preserves their order_by / limit /
    // existing filters. The IN-predicate AND-composes with the user's
    // WHERE clause (QuerySet default-ANDs raw filters).
    let children: Vec<C> = child_qs
        .filter_op(
            child_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(parent_pks),
        )
        .fetch_pool(pool)
        .await?;

    let mut grouped: std::collections::HashMap<String, Vec<C>> = std::collections::HashMap::new();
    for child in children {
        let Some(fk_pk) = child.__rustango_fk_pk_value(child_fk_column) else {
            continue;
        };
        grouped
            .entry(fk_pk.to_display_string())
            .or_default()
            .push(child);
    }

    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let pk = extract_pk_value(&parent).to_display_string();
        let kids = grouped.remove(&pk).unwrap_or_default();
        out.push((parent, kids));
    }
    let _ = pk_field;
    Ok(out)
}

/// `select_rows_pool` with `select_related` join decoding. When the
/// query carries no joins, behaves identically to [`select_rows_pool`]
/// (fast `query_as` path). When joins are present, fetches raw rows
/// and dispatches to `T::__rustango_load_related` (Postgres) or
/// `T::__rustango_load_related_my` (MySQL) for each join alias.
///
/// Bound on `T` adds [`LoadRelated`] + [`MaybeMyLoadRelated`] over
/// [`select_rows_pool`]'s bound — every `#[derive(Model)]` type
/// satisfies these (FK-less models get empty-arm impls so the trait
/// bound is universal).
///
/// # Errors
/// As [`select_rows_pool`].
pub async fn select_rows_pool_with_related<T>(
    pool: &Pool,
    query: &SelectQuery,
) -> Result<Vec<T>, ExecError>
where
    T: MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    crate::test_assertions::query_counter::bump();
    let stmt = pool.dialect().compile_select(query)?;
    let aliases: Vec<&'static str> = query.joins.iter().map(|j| j.alias).collect();

    match pool {
        #[cfg(feature = "postgres")]
        Pool::Postgres(pg) => {
            if aliases.is_empty() {
                let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> =
                    sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as(q, v);
                }
                return Ok(q.fetch_all(pg).await?);
            }
            // Join path — fetch raw rows so we can both decode T and
            // stitch each JOIN target via __rustango_load_related.
            let mut q: Query<'_, sqlx::Postgres, PgArguments> = sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query(q, v);
            }
            let raw_rows = q.fetch_all(pg).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut t = T::from_row(row)?;
                for alias in &aliases {
                    let _ = t.__rustango_load_related(row, alias, alias)?;
                }
                out.push(t);
            }
            Ok(out)
        }
        #[cfg(feature = "mysql")]
        Pool::Mysql(my) => {
            if aliases.is_empty() {
                let mut q: sqlx::query::QueryAs<'_, sqlx::MySql, T, sqlx::mysql::MySqlArguments> =
                    sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as_my(q, v);
                }
                return Ok(q.fetch_all(my).await?);
            }
            // Join path on MySQL — symmetric with PG arm but routes
            // through LoadRelatedMy::__rustango_load_related_my.
            let mut q: sqlx::query::Query<'_, sqlx::MySql, sqlx::mysql::MySqlArguments> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_my(q, v);
            }
            let raw_rows = q.fetch_all(my).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut t = <T as sqlx::FromRow<sqlx::mysql::MySqlRow>>::from_row(row)?;
                for alias in &aliases {
                    let _ = t.__rustango_load_related_my(row, alias, alias)?;
                }
                out.push(t);
            }
            Ok(out)
        }
        #[cfg(feature = "sqlite")]
        Pool::Sqlite(sq) => {
            if aliases.is_empty() {
                let mut q: sqlx::query::QueryAs<
                    '_,
                    sqlx::Sqlite,
                    T,
                    sqlx::sqlite::SqliteArguments<'_>,
                > = sqlx::query_as::<_, T>(&stmt.sql);
                for v in stmt.params {
                    q = bind_query_as_sqlite(q, v);
                }
                return Ok(q.fetch_all(sq).await?);
            }
            // Join path on SQLite — same shape as PG / MySQL arms,
            // routed through LoadRelatedSqlite::__rustango_load_related_sqlite.
            let mut q: sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'_>> =
                sqlx::query(&stmt.sql);
            for v in stmt.params {
                q = bind_query_sqlite(q, v);
            }
            let raw_rows = q.fetch_all(sq).await?;
            let mut out = Vec::with_capacity(raw_rows.len());
            for row in &raw_rows {
                let mut t = <T as sqlx::FromRow<sqlx::sqlite::SqliteRow>>::from_row(row)?;
                for alias in &aliases {
                    let _ = t.__rustango_load_related_sqlite(row, alias, alias)?;
                }
                out.push(t);
            }
            Ok(out)
        }
    }
}

/// `QuerySet::fetch` variant that takes `&Pool` — works against
/// either backend when the model derives `Model` (the macro emits
/// both `FromRow<PgRow>` and the cfg-gated `FromRow<MySqlRow>`).
///
/// `select_related` joins are decoded automatically via
/// [`LoadRelated`] (PG) and [`MaybeMyLoadRelated`] (MySQL); both
/// traits are universally implemented on `#[derive(Model)]` types
/// (FK-less models get empty-arm impls), so the bound is satisfied
/// without user action.
pub trait FetcherPool<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Compile the queryset and run `fetch_all` against either backend.
    /// Stitches `select_related` joins automatically when the queryset
    /// declared any.
    ///
    /// # Errors
    /// As [`Fetcher::fetch`].
    fn fetch_pool(
        self,
        pool: &Pool,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> FetcherPool<T> for QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    async fn fetch_pool(self, pool: &Pool) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        select_rows_pool_with_related(pool, &select).await
    }
}

// v0.45 — single-row sugar on top of FetcherPool. Each method
// applies the appropriate `order_by` + `limit(1)` then forwards to
// `fetch_pool`. All four are inherent methods on `QuerySet<T>` (not
// trait methods) because adding default methods to `FetcherPool`
// would require RTN syntax against `Self::Future` and bound shuffling
// we don't need — `QuerySet<T>` is the only Self that matters.
impl<T> crate::query::QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Fetch the first row by the current ordering, or `None` when
    /// the result is empty.
    ///
    /// If no `order_by` is set, "first" means "first by primary key
    /// ASC" — the natural insertion order on Auto<T> PKs and stable
    /// across drivers. Django's `QuerySet.first()` behaves the same
    /// way (it falls back to PK ordering for determinism).
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    pub async fn first(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        let qs = ensure_pk_ordering(self, /*reverse=*/ false);
        let rows = qs.limit(1).fetch_pool(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the last row by the current ordering, or `None` when
    /// the result is empty.
    ///
    /// Implemented as "flip every ordering direction, take the first"
    /// — avoids `OFFSET COUNT(*) - 1` and works on every dialect.
    /// If no `order_by` is set, sorts by PK DESC.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    pub async fn last(self, pool: &Pool) -> Result<Option<T>, ExecError> {
        let qs = ensure_pk_ordering(self, /*reverse=*/ true);
        let rows = qs.limit(1).fetch_pool(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the smallest row by `field` (ASC ordering), or `None`
    /// when the result is empty. Django's `QuerySet.earliest("field")`.
    ///
    /// Any previously-set `order_by` is **replaced** — `earliest`
    /// declares the sort itself.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    pub async fn earliest(mut self, field: &str, pool: &Pool) -> Result<Option<T>, ExecError> {
        self = self.replace_order_by(&[(field, false)]);
        let rows = self.limit(1).fetch_pool(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Fetch the largest row by `field` (DESC ordering), or `None`
    /// when the result is empty. Django's `QuerySet.latest("field")`.
    ///
    /// Any previously-set `order_by` is **replaced** — `latest`
    /// declares the sort itself.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    pub async fn latest(mut self, field: &str, pool: &Pool) -> Result<Option<T>, ExecError> {
        self = self.replace_order_by(&[(field, true)]);
        let rows = self.limit(1).fetch_pool(pool).await?;
        Ok(rows.into_iter().next())
    }

    /// Issue #23 — Django's `QuerySet.iterator(chunk_size=2000)`.
    /// Return a chunked iterator over results, fetching `chunk_size`
    /// rows at a time via `LIMIT N OFFSET M`. Never buffers the full
    /// result set — apps processing million-row exports can stream
    /// rows without OOM.
    ///
    /// Compiles the queryset eagerly so any schema validation error
    /// surfaces here rather than mid-stream. The compiled `SelectQuery`
    /// is then re-issued per chunk with rotating `OFFSET`.
    ///
    /// ```ignore
    /// // Two iteration styles, both work:
    /// let mut iter = Post::objects()
    ///     .where_(Post::published.eq(true))
    ///     .order_by(&[("id", false)])
    ///     .iterator(2_000)?;
    ///
    /// // 1. Whole-chunk loop:
    /// while let Some(chunk) = iter.next_chunk(&pool).await? {
    ///     for post in chunk { /* … */ }
    /// }
    ///
    /// // 2. Row-by-row loop (buffer one chunk internally):
    /// while let Some(post) = iter.next_row(&pool).await? {
    ///     /* … */
    /// }
    /// ```
    ///
    /// **Order-by recommended.** `OFFSET` without a stable sort returns
    /// unpredictable rows across chunks — set `.order_by(&[("pk", …)])`
    /// before `.iterator()` so each chunk picks up where the previous
    /// left off. The method doesn't enforce it (some queries
    /// legitimately want no ordering, e.g. a one-shot drain).
    ///
    /// **Trade-off vs server-side cursors.** This is a simple
    /// LIMIT/OFFSET chunker — each chunk re-runs the query with a
    /// larger offset. On a btree-indexed column with `OFFSET N`,
    /// Postgres scans the first N rows before returning the (N+1)th,
    /// so deep pagination is O(n²) total work. For truly streaming
    /// reads on PG, callers can drop into `transaction()` + the raw
    /// `sqlx::query(...).fetch(...)` Stream API directly — that uses
    /// the extended protocol with no offset reseek. The chunker is the
    /// simple choice that works on every backend.
    ///
    /// **Concurrent-write hazard.** Each chunk is its own query, so
    /// rows inserted ahead of the current offset between fetches can
    /// be skipped, and rows deleted can shift a row down into the
    /// next chunk and be returned twice. **The chunker API is
    /// `&Pool`-only — it can't run inside a `&mut Transaction`** —
    /// so for write-concurrent tables you have to hand-roll the
    /// LIMIT/OFFSET loop against [`select_rows_on`] inside your
    /// `pool.begin()` + `SET TRANSACTION ISOLATION LEVEL REPEATABLE
    /// READ` block. See the cookbook for the boilerplate. For
    /// read-only / append-only tables (the typical export use case)
    /// this isn't a concern.
    ///
    /// **`select_for_update()` does NOT propagate.** Row locks
    /// acquired by a `.select_for_update()` call on the queryset are
    /// released between chunks because each chunk runs in its own
    /// implicit transaction, and the chunker API doesn't take a
    /// transaction. Two compromises for a locked drain:
    ///
    /// * `.fetch_on(&mut *tx)` — single round trip, returns full
    ///   `Vec<T>`; fine when the result fits in memory.
    /// * Hand-roll LIMIT/OFFSET inside the tx via [`select_rows_on`]
    ///   — same shape as the snapshot-isolation pattern; streams
    ///   chunks but outside the [`ChunkedIter`] API.
    ///
    /// A future `iterator_on(&mut *tx, chunk_size)` companion would
    /// close this gap; not in scope for issue #23.
    ///
    /// # Errors
    /// Returns [`QueryError`] if the queryset fails to compile.
    ///
    /// # Panics
    /// If `chunk_size <= 0`. Zero or negative chunk sizes silently
    /// yield no rows, which is almost always a programmer error
    /// (e.g. `iterator(unchecked_user_input as i64)`); the assert
    /// surfaces it loudly.
    pub fn iterator(self, chunk_size: i64) -> Result<ChunkedIter<T>, crate::core::QueryError> {
        assert!(
            chunk_size > 0,
            "QuerySet::iterator: chunk_size must be > 0; got {chunk_size}"
        );
        let query = self.compile()?;
        Ok(ChunkedIter {
            query,
            chunk_size,
            offset: 0,
            exhausted: false,
            buffer: std::collections::VecDeque::new(),
            seen: 0,
            _model: std::marker::PhantomData,
        })
    }

    /// Issue #24 — Django's `Model.objects.in_bulk(ids, field_name=)`:
    /// fetch a set of rows by a column value list and return them
    /// keyed by that column in a `HashMap`.
    ///
    /// `column` is a typed [`crate::core::Column`] reference (e.g.
    /// `User::id` or `Book::isbn`) so the filter column is checked
    /// against the model at compile time. `ids` is an iterable of
    /// values — the SQL becomes `… WHERE <column> IN ($1, $2, …)`.
    /// `extract` reads the key off each fetched row so the map can be
    /// built without re-decoding the column from the raw `sqlx::Row`
    /// (a closure also gives callers full control over `Auto<T>` /
    /// `ForeignKey<T, K>` unwrap shape).
    ///
    /// Empty `ids` short-circuits with an empty `HashMap` — no SQL is
    /// issued, sidestepping `Op::In` with an empty list (which the
    /// writer rejects with [`crate::sql::SqlError::EmptyInList`]).
    ///
    /// ```ignore
    /// use std::collections::HashMap;
    /// use rustango::sql::Auto;
    ///
    /// // Default — keyed by the Auto<i64> PK. The closure handles
    /// // Auto::Set unwrap (every fetched row has an `Auto::Set`
    /// // value; `Auto::Unset` would be a programming error).
    /// let books: HashMap<i64, Book> = Book::objects()
    ///     .in_bulk(Book::id, [1_i64, 2, 3], |b| match b.id {
    ///         Auto::Set(v) => v,
    ///         Auto::Unset  => unreachable!("fetched row has PK"),
    ///     }, &pool)
    ///     .await?;
    ///
    /// // `field_name=` equivalent — key by any unique column.
    /// let books_by_isbn: HashMap<String, Book> = Book::objects()
    ///     .in_bulk(Book::isbn, ["isbn-1", "isbn-2"], |b| b.isbn.clone(), &pool)
    ///     .await?;
    /// ```
    ///
    /// When the result contains multiple rows sharing the same key
    /// (only possible if `column` is not unique), the *later* row
    /// wins — matches `HashMap::insert` semantics. Pair with a
    /// unique column to avoid surprises.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    pub async fn in_bulk<C, K, I, F>(
        self,
        column: C,
        ids: I,
        extract: F,
        pool: &Pool,
    ) -> Result<std::collections::HashMap<K, T>, ExecError>
    where
        C: crate::core::Column<Model = T>,
        K: Eq + std::hash::Hash + Into<crate::core::SqlValue>,
        I: IntoIterator<Item = K>,
        F: Fn(&T) -> K,
    {
        // `column` is consumed only to thread the `Column<Model = T>`
        // bound — its `COLUMN` const drives the WHERE filter below.
        // Discarding the value keeps the ZST instance from triggering
        // an unused-variable warning.
        let _ = column;
        let id_values: Vec<crate::core::SqlValue> = ids.into_iter().map(|v| v.into()).collect();
        if id_values.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let rows = self
            .filter_op(
                C::COLUMN,
                crate::core::Op::In,
                crate::core::SqlValue::List(id_values),
            )
            .fetch_pool(pool)
            .await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let key = extract(&row);
            out.insert(key, row);
        }
        Ok(out)
    }
}

mod get_or_create;
pub use get_or_create::{get_or_create, update_or_create};

/// v0.45 helper — ensure the queryset has *some* deterministic
/// ordering before slicing to one row. Used by `first` and
/// `last`. If the caller already provided an `order_by`, we
/// either keep it (forward) or flip every direction (reverse). If
/// they didn't, we fall back to the model's primary key.
fn ensure_pk_ordering<T: Model>(
    qs: crate::query::QuerySet<T>,
    reverse: bool,
) -> crate::query::QuerySet<T> {
    if !qs.has_order_by() {
        let pk = T::SCHEMA.primary_key().map(|f| f.column);
        if let Some(pk_col) = pk {
            return qs.replace_order_by(&[(pk_col, reverse)]);
        }
        // No PK on this model — leave the order_by empty. The caller
        // is going to get a row deterministic-by-row-order, which is
        // dialect-defined but stable enough for the no-pk-no-order
        // edge case.
        qs
    } else if reverse {
        qs.flip_order_by()
    } else {
        qs
    }
}

/// `QuerySet::fetch` variant that takes `&mut PoolTx` — executes the
/// SELECT inside an open transaction so the read and subsequent writes
/// share the same transaction boundary.
///
/// Mirrors [`FetcherPool`] but routes through
/// [`select_rows_tx_with_related`] instead of
/// [`select_rows_pool_with_related`]. All model types that derive
/// `Model` automatically satisfy the bounds (`#[derive(Model)]` emits
/// both PG and cfg-gated MySQL/SQLite `FromRow` impls).
pub trait FetcherTx<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    /// Compile the queryset and run `fetch_all` against the open
    /// transaction. Stitches `select_related` joins automatically.
    ///
    /// # Errors
    /// As [`FetcherPool::fetch_pool`].
    fn fetch_tx(
        self,
        tx: &mut PoolTx<'_>,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> FetcherTx<T> for QuerySet<T>
where
    T: Model
        + MaybePgFromRow
        + MaybeMyFromRow
        + MaybeSqliteFromRow
        + LoadRelated
        + MaybeMyLoadRelated
        + MaybeSqliteLoadRelated
        + Send
        + Unpin,
{
    async fn fetch_tx(self, tx: &mut PoolTx<'_>) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        select_rows_tx_with_related(tx, &select).await
    }
}

mod iter;
pub use iter::ChunkedIter;

#[cfg(test)]
mod pool_dispatch_tests {
    // All inner tests are `#[cfg(feature = "mysql")]` gated; without
    // that feature the imports show as unused.
    #[allow(unused_imports)]
    use super::*;

    /// Smoke test: a `Pool::Mysql` from a `connect_lazy` handle picks
    /// the MySQL dialect when compiling, so a `count_rows_pool` call
    /// would ship MySQL-shape SQL (backticks + `?`). We can't actually
    /// execute without a live DB, but we can confirm the dispatch
    /// finds the right compiler via `pool.dialect()`.
    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn mysql_pool_dispatch_uses_mysql_dialect() {
        let my = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect_lazy("mysql://user:pass@localhost:1/none")
            .unwrap();
        let pool: Pool = my.into();
        // Confirm the dispatch path's compile step is routed to the
        // MySQL dialect — this is what protects against regressions
        // where a future refactor accidentally hard-codes Postgres.
        assert_eq!(pool.dialect().name(), "mysql");
        assert_eq!(pool.dialect().quote_ident("col"), "`col`");
        assert_eq!(pool.dialect().placeholder(1), "?");
    }

    /// Same shape for Postgres — confirms the dispatch matrix has
    /// both arms reachable via the public `Pool` enum.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_pool_dispatch_uses_postgres_dialect() {
        let pg = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let pool: Pool = pg.into();
        assert_eq!(pool.dialect().name(), "postgres");
        assert_eq!(pool.dialect().quote_ident("col"), "\"col\"");
        assert_eq!(pool.dialect().placeholder(1), "$1");
    }

    /// Compile-time guard for the `MaybeMyFromRow` blanket impl.
    /// `()` implements `FromRow<R>` for any `R` in sqlx, so it
    /// satisfies the bound under both feature configs and is the
    /// safest universal probe. The integration test
    /// `tests/mysql_from_row.rs` covers the `#[derive(Model)]`
    /// emission end-to-end.
    #[test]
    fn maybe_my_from_row_resolves_for_unit_type() {
        fn check<T: super::MaybeMyFromRow>() {}
        check::<()>();
    }
}
