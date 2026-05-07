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

use crate::core::{FieldSchema, FieldType, ModelSchema, Relation};
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

/// One `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` per FK / O2O field,
/// plus one per composite FK declared via `#[rustango(fk_composite(...))]`
/// (sub-slice F.2 of the v0.15.0 ContentType plan). MySQL accepts the
/// same FK syntax — only the identifier quoting differs.
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
    // Composite FKs — `(col_a, col_b, …) REFERENCES target (col_x, col_y, …)`.
    // The macro ensures `from.len() == on.len()` at compile time.
    for rel in model.composite_relations {
        let mut s = String::from("ALTER TABLE ");
        s.push_str(&dialect.quote_ident(model.table));
        s.push_str(" ADD CONSTRAINT ");
        s.push_str(&dialect.quote_ident(&format!("{}_{}_fkey", model.table, rel.name)));
        s.push_str(" FOREIGN KEY (");
        for (i, col) in rel.from.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&dialect.quote_ident(col));
        }
        s.push_str(") REFERENCES ");
        s.push_str(&dialect.quote_ident(rel.to));
        s.push_str(" (");
        for (i, col) in rel.on.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&dialect.quote_ident(col));
        }
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

/// Per-field SQL type — integer `Auto<T>` PKs delegate to
/// [`Dialect::serial_type`] (PG: `BIGSERIAL`/`SERIAL`, MySQL: `BIGINT
/// AUTO_INCREMENT`/`INT AUTO_INCREMENT`); non-integer Auto fields
/// (`Auto<Uuid>` w/ `auto_uuid`, `Auto<DateTime<Utc>>` w/
/// `auto_now_add`/`auto_now`) fall through to [`Dialect::column_type`]
/// — they're DB-default-supplied via the explicit `default`
/// expression on the field, NOT a sequence.
///
/// Without this gate, a column like
/// `#[rustango(auto_now_add)] created_at: Auto<DateTime<Utc>>`
/// gets emitted as `BIGSERIAL DEFAULT now() NOT NULL` — Postgres'
/// `BIGSERIAL` macro already supplies `DEFAULT nextval(...)`, so the
/// CREATE TABLE rejects with `multiple default values specified for
/// column "created_at"`. The migration-replay path
/// (`crate::migrate::diff::sql_type_for_field`) already had this
/// guard; this mirror brings the apply_all (ephemeral / test) path
/// in line.
fn sql_type(dialect: &dyn Dialect, field: &FieldSchema) -> String {
    if field.auto && matches!(field.ty, FieldType::I16 | FieldType::I32 | FieldType::I64) {
        return dialect.serial_type(field.ty).to_owned();
    }
    dialect.column_type(field.ty, field.max_length)
}

#[cfg(test)]
mod tests {
    //! Regression tests for the `auto = true` × non-integer field-type
    //! case that crashed `apply_all` against `rustango_api_keys` in
    //! v0.24.0 — `Auto<DateTime<Utc>>` with `auto_now_add` was
    //! rendering as `BIGSERIAL DEFAULT now()` and Postgres rejected
    //! the duplicate default.
    //!
    //! Coverage:
    //! 1. `Auto<i32>` / `Auto<i64>` PKs still emit SERIAL / BIGSERIAL
    //!    (no regression on the integer path).
    //! 2. `Auto<DateTime>` with `auto_now_add` emits `TIMESTAMPTZ`
    //!    (column type only) so the field's `DEFAULT now()` lands
    //!    cleanly.
    //! 3. `Auto<Uuid>` with `auto_uuid` emits `UUID` so the field's
    //!    `DEFAULT gen_random_uuid()` lands cleanly.
    //! 4. The end-to-end CREATE TABLE has exactly one DEFAULT clause
    //!    per column (smoke test against full DDL).

    use super::*;
    use crate::core::FieldType;

    fn pg() -> Postgres {
        Postgres
    }

    fn fld(
        name: &'static str,
        ty: FieldType,
        auto: bool,
        default: Option<&'static str>,
    ) -> FieldSchema {
        FieldSchema {
            name,
            column: name,
            ty,
            nullable: false,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default,
            auto,
            unique: false,
        }
    }

    #[test]
    fn auto_i32_emits_serial() {
        let f = fld("id", FieldType::I32, true, None);
        assert_eq!(sql_type(&pg(), &f), "SERIAL");
    }

    #[test]
    fn auto_i64_emits_bigserial() {
        let f = fld("id", FieldType::I64, true, None);
        assert_eq!(sql_type(&pg(), &f), "BIGSERIAL");
    }

    #[test]
    fn auto_datetime_emits_timestamptz_not_bigserial() {
        // Regression for the `multiple default values specified for
        // column "created_at"` panic: `Auto<DateTime<Utc>>` w/
        // auto_now_add fed `BIGSERIAL` into Postgres which already
        // supplies `DEFAULT nextval(...)`.
        let f = fld("created_at", FieldType::DateTime, true, Some("now()"));
        assert_eq!(sql_type(&pg(), &f), "TIMESTAMPTZ");
    }

    #[test]
    fn auto_uuid_emits_uuid_not_bigserial() {
        let f = fld("id", FieldType::Uuid, true, Some("gen_random_uuid()"));
        assert_eq!(sql_type(&pg(), &f), "UUID");
    }

    #[test]
    fn full_create_table_has_single_default_per_column() {
        // Smoke: render a full CREATE TABLE for a table that mixes
        // `Auto<i64>` PK + `auto_now_add` timestamp, and confirm no
        // column carries two DEFAULT clauses.
        let mut col_def = String::new();
        write_column_def(
            &mut col_def,
            &pg(),
            &fld("created_at", FieldType::DateTime, true, Some("now()")),
        );
        // Should be: `"created_at" TIMESTAMPTZ DEFAULT now() NOT NULL`
        // — exactly one " DEFAULT " token.
        let n_defaults = col_def.matches(" DEFAULT ").count();
        assert_eq!(
            n_defaults, 1,
            "expected exactly one DEFAULT clause, got {n_defaults} in: {col_def}"
        );
        assert!(col_def.contains("TIMESTAMPTZ"), "got: {col_def}");
        assert!(col_def.contains("DEFAULT now()"), "got: {col_def}");
        assert!(
            !col_def.contains("BIGSERIAL"),
            "must not emit BIGSERIAL: {col_def}"
        );
    }

    #[test]
    fn full_create_table_uuid_auto_has_single_default() {
        let mut col_def = String::new();
        write_column_def(
            &mut col_def,
            &pg(),
            &fld("id", FieldType::Uuid, true, Some("gen_random_uuid()")),
        );
        let n_defaults = col_def.matches(" DEFAULT ").count();
        assert_eq!(n_defaults, 1, "got: {col_def}");
        assert!(col_def.contains("UUID"));
        assert!(col_def.contains("DEFAULT gen_random_uuid()"));
    }

    #[test]
    fn auto_i64_default_clause_passthrough() {
        // Sanity: an `Auto<i64>` PK with no explicit default still
        // emits `BIGSERIAL` and NO `DEFAULT` clause (BIGSERIAL implies
        // its own nextval default).
        let mut col_def = String::new();
        write_column_def(&mut col_def, &pg(), &fld("id", FieldType::I64, true, None));
        assert!(col_def.contains("BIGSERIAL"), "got: {col_def}");
        assert!(
            !col_def.contains(" DEFAULT "),
            "BIGSERIAL must not get an explicit DEFAULT: {col_def}"
        );
    }
}
