//! Dynamic row-to-JSON decoders (`row_to_json` family) + tri-dialect
//! `select_rows_as_json` / `select_one_row_as_json` entry points.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 7. The
//! schema-driven JSON decoders are consumed by the admin/viewset
//! list/retrieve handlers + `contenttypes::fetch_row_as_json` —
//! pulling them out drops 500 LOC from mod.rs.

#[cfg(feature = "postgres")]
use sqlx::postgres::PgArguments;
#[cfg(feature = "postgres")]
use sqlx::query::Query;

#[cfg(feature = "postgres")]
use super::bind_query;
#[cfg(feature = "mysql")]
use super::bind_query_my;
#[cfg(feature = "sqlite")]
use super::bind_query_sqlite;
use super::ExecError;
use crate::core::SelectQuery;
use crate::sql::Pool;

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

// #562 — single `hex_encode` implementation lives in `crate::hex`;
// the `row_to_json` family used to ship a verbatim copy. Re-export
// so the local `hex_encode(...)` call sites in the per-backend
// `Binary` arms stay byte-identical.
use crate::hex::hex_encode;

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
