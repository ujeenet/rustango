//! DDL writer.
//!
//! Walks a `ModelSchema` and emits `CREATE TABLE` / `DROP TABLE` strings.
//! Foreign-key constraints are emitted separately as `ALTER TABLE` so the
//! caller doesn't have to topologically sort tables.
//!
//! ## v0.23.0-batch10 — bi-dialect dispatch
//!
//! All emitters now have a `_with_dialect` variant that takes
//! `&dyn Dialect`. The existing PG-typed entry points
//! (`create_table_sql`, `drop_table_sql`, `create_constraints_sql`)
//! delegate to the new variants with [`crate::sql::Postgres`] —
//! every existing call site stays byte-identical. New code that
//! has a [`crate::sql::Pool`] picks `pool.dialect()` and emits the
//! right shape for the active backend.
//!
//! ## Type mapping
//!
//! Postgres-shape (default `Dialect` impl):
//! * `i32`     → `INTEGER` / `i64` → `BIGINT`
//! * `f32`     → `REAL`   / `f64` → `DOUBLE PRECISION`
//! * `bool`    → `BOOLEAN`
//! * `String`  → `VARCHAR(N)` if `max_length` is set, otherwise `TEXT`
//! * `DateTime<Utc>` → `TIMESTAMPTZ`
//! * `NaiveDate`     → `DATE`
//! * `Uuid`    → `UUID`
//! * `serde_json::Value` → `JSONB`
//!
//! `MySQL`-shape (overrides via [`crate::sql::Dialect::column_type`]):
//! * `bool`    → `TINYINT(1)` / `DateTime<Utc>` → `DATETIME(6)`
//! * `Uuid`    → `CHAR(36)` / `serde_json::Value` → `JSON`
//! * `f32`/`f64` → `FLOAT`/`DOUBLE`
//!
//! ## Bound mapping
//! * `nullable: false`  → `NOT NULL`
//! * `primary_key: true` → `PRIMARY KEY`
//! * `min` / `max`      → `CHECK ("col" >= N AND "col" <= M)`
//! * `default`          → `DEFAULT <raw expression>`
//! * `Relation::Fk` / `Relation::O2O` → emitted via [`create_constraints_sql_with_dialect`]

use std::fmt::Write as _;

use crate::core::{FieldSchema, ModelSchema, Relation};
use crate::sql::{Dialect, Postgres};

// ============================================================ Postgres-typed shims (existing API)

/// `CREATE TABLE "model.table" ( … )` without FK constraints. Postgres
/// shape — for bi-dialect emission see
/// [`create_table_sql_with_dialect`].
#[must_use]
pub fn create_table_sql(model: &ModelSchema) -> String {
    create_table_sql_with_dialect(&Postgres, model)
}

/// `CREATE TABLE IF NOT EXISTS …` — handy for idempotent dev bootstrapping.
#[must_use]
pub fn create_table_if_not_exists_sql(model: &ModelSchema) -> String {
    create_table_if_not_exists_sql_with_dialect(&Postgres, model)
}

/// `DROP TABLE [IF EXISTS] "model.table" [CASCADE]`.
#[must_use]
pub fn drop_table_sql(model: &ModelSchema, if_exists: bool, cascade: bool) -> String {
    drop_table_sql_with_dialect(&Postgres, model, if_exists, cascade)
}

/// One `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` per FK / O2O field.
#[must_use]
pub fn create_constraints_sql(model: &ModelSchema) -> Vec<String> {
    create_constraints_sql_with_dialect(&Postgres, model)
}

// ============================================================ dialect-aware emitters (batch 10)

/// `CREATE TABLE` for `model` using `dialect`'s identifier quoting +
/// type names + `Auto<T>` serial spelling. Identical output to the
/// PG-typed shim when `dialect` is [`crate::sql::Postgres`].
#[must_use]
pub fn create_table_sql_with_dialect(dialect: &dyn Dialect, model: &ModelSchema) -> String {
    let mut s = String::new();
    s.push_str("CREATE TABLE ");
    s.push_str(&dialect.quote_ident(model.table));
    s.push_str(" (");
    let mut first = true;
    for field in model.scalar_fields() {
        if !first {
            s.push_str(", ");
        }
        first = false;
        write_column_def(&mut s, dialect, field);
    }
    s.push(')');
    s
}

/// `CREATE TABLE IF NOT EXISTS …` variant of
/// [`create_table_sql_with_dialect`].
#[must_use]
pub fn create_table_if_not_exists_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> String {
    let mut s = create_table_sql_with_dialect(dialect, model);
    debug_assert!(s.starts_with("CREATE TABLE "));
    s.replace_range(.."CREATE TABLE".len(), "CREATE TABLE IF NOT EXISTS");
    s
}

/// `DROP TABLE [IF EXISTS] …` using `dialect`'s identifier quoting.
/// Note: `CASCADE` isn't supported on `DROP TABLE` in `MySQL`
/// (`MySQL` `DROP TABLE` always cascades FKs internally and rejects
/// the keyword); this emitter writes the keyword regardless and
/// relies on the caller to know whether the dialect accepts it.
/// Future batch will gate on a `Dialect::supports_drop_cascade()`.
#[must_use]
pub fn drop_table_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
    if_exists: bool,
    cascade: bool,
) -> String {
    let mut s = String::from("DROP TABLE ");
    if if_exists {
        s.push_str("IF EXISTS ");
    }
    s.push_str(&dialect.quote_ident(model.table));
    if cascade {
        // PG accepts CASCADE; MySQL silently ignores when emitted in
        // some clients but rejects on the wire. The runner currently
        // only invokes this on Postgres.
        s.push_str(" CASCADE");
    }
    s
}

/// One `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` per FK / O2O field.
/// MySQL accepts the same syntax — only the identifier quoting differs.
#[must_use]
pub fn create_constraints_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
        };
        let mut s = String::from("ALTER TABLE ");
        s.push_str(&dialect.quote_ident(model.table));
        s.push_str(" ADD CONSTRAINT ");
        s.push_str(&dialect.quote_ident(&format!("{}_{}_fkey", model.table, field.column)));
        s.push_str(" FOREIGN KEY (");
        s.push_str(&dialect.quote_ident(field.column));
        s.push_str(") REFERENCES ");
        s.push_str(&dialect.quote_ident(to));
        s.push_str(" (");
        s.push_str(&dialect.quote_ident(on));
        s.push(')');
        out.push(s);
    }
    out
}

// ============================================================ internals

fn write_column_def(s: &mut String, dialect: &dyn Dialect, field: &FieldSchema) {
    s.push_str(&dialect.quote_ident(field.column));
    s.push(' ');
    s.push_str(&sql_type(dialect, field));
    if let Some(expr) = field.default {
        let _ = write!(s, " DEFAULT {expr}");
    }
    if !field.nullable {
        s.push_str(" NOT NULL");
    }
    if field.primary_key {
        s.push_str(" PRIMARY KEY");
    }
    if field.unique && !field.primary_key {
        s.push_str(" UNIQUE");
    }
    write_check_constraint(s, dialect, field);
}

fn write_check_constraint(s: &mut String, dialect: &dyn Dialect, field: &FieldSchema) {
    if field.min.is_none() && field.max.is_none() {
        return;
    }
    s.push_str(" CHECK (");
    let mut wrote = false;
    if let Some(min) = field.min {
        s.push_str(&dialect.quote_ident(field.column));
        let _ = write!(s, " >= {min}");
        wrote = true;
    }
    if let Some(max) = field.max {
        if wrote {
            s.push_str(" AND ");
        }
        s.push_str(&dialect.quote_ident(field.column));
        let _ = write!(s, " <= {max}");
    }
    s.push(')');
}

/// Per-field SQL type — `Auto<T>` PKs delegate to
/// [`Dialect::serial_type`] (PG: `BIGSERIAL`/`SERIAL`, MySQL: `BIGINT
/// AUTO_INCREMENT`/`INT AUTO_INCREMENT`); everything else delegates to
/// [`Dialect::column_type`] for the per-backend type spelling.
fn sql_type(dialect: &dyn Dialect, field: &FieldSchema) -> String {
    if field.auto {
        return dialect.serial_type(field.ty).to_owned();
    }
    dialect.column_type(field.ty, field.max_length)
}
