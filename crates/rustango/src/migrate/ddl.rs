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
    // For dialects that REQUIRE FKs inline in CREATE TABLE (SQLite —
    // `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` doesn't exist), emit
    // every FK clause inside the same CREATE TABLE statement. PG +
    // MySQL get nothing here; their FKs continue to be emitted as
    // post-hoc `ALTER TABLE ADD CONSTRAINT` via
    // [`create_constraints_sql_with_dialect`] so cross-table cycles
    // resolve cleanly within a single migration batch.
    if dialect.inline_fks_in_create_table() {
        for clause in inline_fk_clauses(dialect, model) {
            s.push_str(", ");
            s.push_str(&clause);
        }
    }
    s.push(')');
    // Django-shape `Meta.db_table_comment` — MySQL spells it as an
    // inline trailer (`) COMMENT='...'`); PG + SQLite emit nothing
    // inline (PG runs a post-hoc `COMMENT ON TABLE`, SQLite is a
    // no-op). See `table_comment_statements_with_dialect`.
    if let Some(comment) = model.db_table_comment {
        if let Some(inline) = dialect.write_inline_table_comment(comment) {
            s.push_str(&inline);
        }
    }
    s
}

/// Per-model post-CREATE-TABLE statements for `Meta.db_table_comment`.
/// PG emits `COMMENT ON TABLE`, MySQL handles it inline (see
/// `create_table_sql_with_dialect` above) and returns nothing here,
/// SQLite has no native table comments and returns nothing.
#[must_use]
pub fn table_comment_statements_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(comment) = model.db_table_comment {
        if let Some(stmt) = dialect.table_comment_statement(model.table, comment) {
            out.push(stmt);
        }
    }
    out
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

/// The inverse of [`create_constraints_sql_with_dialect`]: one statement
/// per FK / O2O field and per composite FK, dropping the constraint that
/// the create emitter named `{table}_{column}_fkey`.
///
/// Needed because `DROP TABLE` is only FK-safe on two of the three
/// dialects: Postgres has `CASCADE`, SQLite leaves `foreign_keys` off by
/// default, but MySQL enforces FKs and rejects `CASCADE` — so dropping a
/// parent before its child fails outright (#1277). Dropping constraints
/// first makes table drop order irrelevant, mirroring the way
/// [`create_constraints_sql_with_dialect`] makes create order irrelevant.
///
/// Returns empty for dialects that inline FKs in `CREATE TABLE` (SQLite):
/// there is no named constraint to drop, and the table drop is unimpeded.
///
/// The statements are best-effort by nature — a constraint may already be
/// gone, and only Postgres can say `IF EXISTS` here (MySQL's
/// `DROP FOREIGN KEY` has no such form), so callers should ignore errors.
#[must_use]
pub fn drop_constraints_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    if dialect.inline_fks_in_create_table() {
        return Vec::new();
    }
    // MySQL spells it `DROP FOREIGN KEY`; Postgres `DROP CONSTRAINT`,
    // which additionally accepts `IF EXISTS`.
    let mysql = dialect.name() == "mysql";
    let mut out = Vec::new();
    let mut push = |name: String| {
        let mut s = String::from("ALTER TABLE ");
        s.push_str(&dialect.quote_ident(model.table));
        if mysql {
            s.push_str(" DROP FOREIGN KEY ");
        } else {
            s.push_str(" DROP CONSTRAINT IF EXISTS ");
        }
        s.push_str(&dialect.quote_ident(&name));
        out.push(s);
    };
    for field in model.scalar_fields() {
        if field.relation.is_some() {
            push(format!("{}_{}_fkey", model.table, field.column));
        }
    }
    for rel in model.composite_relations {
        push(format!("{}_{}_fkey", model.table, rel.name));
    }
    out
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
    // SQLite has no `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` — FKs
    // were emitted inline in CREATE TABLE via `inline_fk_clauses`.
    // Returning the post-hoc ALTER statements here would silently
    // fail at apply time AND, worse, the previous workaround skipped
    // FKs entirely on SQLite (silent loss of referential integrity).
    // Return empty so the runner skips the post-hoc emission cleanly.
    if dialect.inline_fks_in_create_table() {
        return Vec::new();
    }
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
        if let Some(action) = field.fk_on_delete {
            s.push_str(" ON DELETE ");
            s.push_str(action.as_sql());
        }
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

/// Emit FK clauses for inclusion inside a `CREATE TABLE (...)` —
/// used on dialects where `inline_fks_in_create_table()` is true
/// (currently SQLite). Output is a `Vec<String>` whose entries are
/// joined into the CREATE TABLE body with `, `.
///
/// Two kinds of clauses emitted:
/// * Single-column FK / O2O — one `CONSTRAINT <name> FOREIGN KEY
///   (<col>) REFERENCES <to> (<on>) [ON DELETE <action>]` per
///   field that carries `Relation::Fk` or `Relation::O2O`.
/// * Composite FK — one `CONSTRAINT <name> FOREIGN KEY (col_a,
///   col_b, ...) REFERENCES <to> (col_x, col_y, ...)` per
///   `composite_relations` entry.
///
/// Identifier quoting goes through `dialect.quote_ident()`.
fn inline_fk_clauses(dialect: &dyn Dialect, model: &ModelSchema) -> Vec<String> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
        };
        let mut s = String::from("CONSTRAINT ");
        s.push_str(&dialect.quote_ident(&format!("{}_{}_fkey", model.table, field.column)));
        s.push_str(" FOREIGN KEY (");
        s.push_str(&dialect.quote_ident(field.column));
        s.push_str(") REFERENCES ");
        s.push_str(&dialect.quote_ident(to));
        s.push_str(" (");
        s.push_str(&dialect.quote_ident(on));
        s.push(')');
        if let Some(action) = field.fk_on_delete {
            s.push_str(" ON DELETE ");
            s.push_str(action.as_sql());
        }
        out.push(s);
    }
    for rel in model.composite_relations {
        let mut s = String::from("CONSTRAINT ");
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

fn write_column_def(s: &mut String, dialect: &dyn Dialect, field: &FieldSchema) {
    s.push_str(&dialect.quote_ident(field.column));
    s.push(' ');
    s.push_str(&sql_type(dialect, field));
    // Generated columns: emit `GENERATED ALWAYS AS (<expr>) STORED`
    // and skip DEFAULT / PRIMARY KEY / UNIQUE / CHECK — Postgres
    // rejects all of these on generated columns. NOT NULL is still
    // permitted (the expression must always evaluate to non-NULL).
    if let Some(expr) = field.generated_as {
        let _ = write!(s, " GENERATED ALWAYS AS ({expr}) STORED");
        if !field.nullable {
            s.push_str(" NOT NULL");
        }
        return;
    }
    if let Some(expr) = field.default {
        let ty_name = crate::migrate::snapshot::field_type_name(field.ty);
        // An empty-string default (`#[rustango(default = "")]`) means the
        // literal empty string, not an empty raw expression — render it as
        // `''` rather than nothing, or we emit `DEFAULT  NOT NULL` which the
        // driver rejects with `near "NOT": syntax error` (#1161). `''` is a
        // valid empty-string literal in Postgres, MySQL, and SQLite.
        //
        // Still route the `''` literal through `translate_default_expr` so a
        // LOB column (MySQL TEXT/JSON/BLOB) gets the parenthesized
        // expression form `DEFAULT ('')` it requires — MySQL rejects a
        // *literal* default on those types (error 1101), only the 8.0.13+
        // expression form is legal. PG/SQLite leave `''` untouched (#1174).
        let expr_to_render = if expr.is_empty() { "''" } else { expr };
        let rendered = dialect.translate_default_expr(expr_to_render, ty_name, field.max_length);
        let _ = write!(s, " DEFAULT {rendered}");
    }
    if !field.nullable {
        s.push_str(" NOT NULL");
    }
    // SQLite's `Auto<T>` PK type is `INTEGER PRIMARY KEY AUTOINCREMENT`
    // — the PRIMARY KEY clause is part of the type name itself. Skip
    // the standalone `PRIMARY KEY` append in that case so we don't
    // emit it twice.
    let serial_pk_inline = field.auto
        && matches!(field.ty, FieldType::I16 | FieldType::I32 | FieldType::I64)
        && dialect.serial_type_includes_primary_key();
    if field.primary_key && !serial_pk_inline {
        s.push_str(" PRIMARY KEY");
    }
    if field.unique && !field.primary_key {
        s.push_str(" UNIQUE");
    }
    write_check_constraint(s, dialect, field);
    // #450 — MySQL splices `COMMENT '...'` into the column line. PG +
    // SQLite get nothing here; PG gets a separate `COMMENT ON COLUMN`
    // statement via `column_comment_statements_with_dialect`, SQLite
    // is a no-op (no native column comments).
    if let Some(comment) = field.db_comment {
        if let Some(inline) = dialect.write_inline_column_comment(comment) {
            s.push_str(&inline);
        }
    }
}

/// Per-model post-CREATE-TABLE statements for `db_comment` (#450) — one
/// `COMMENT ON COLUMN "<table>"."<col>" IS '...'` per field on Postgres;
/// empty `Vec` on MySQL (already inlined) and SQLite (no-op).
#[must_use]
pub fn column_comment_statements_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        let Some(comment) = field.db_comment else {
            continue;
        };
        if let Some(stmt) = dialect.column_comment_statement(model.table, field.column, comment) {
            out.push(stmt);
        }
    }
    out
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
    // #344 — CITextField / case-insensitive string columns. Only
    // meaningful for `String`; other types fall through to the
    // normal type emit.
    if field.case_insensitive && matches!(field.ty, FieldType::String) {
        return dialect.ci_text_type(field.max_length);
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
            generated_as: None,
            help_text: None,
            choices: None,
            db_comment: None,
            verbose_name: None,
            editable: true,
            blank: false,
            case_insensitive: false,
            fk_on_delete: None,
            validators: &[],
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
    fn empty_string_default_renders_as_quoted_empty_literal() {
        // #1161 — `#[rustango(default = "")]` must emit `DEFAULT ''`, not
        // `DEFAULT ` (nothing), which collapses to `DEFAULT  NOT NULL` and the
        // driver rejects with `near "NOT": syntax error`. `''` is a valid
        // empty-string literal on Postgres, MySQL, and SQLite.
        let mut f = fld("name", FieldType::String, false, Some(""));
        f.max_length = Some(64);
        // Postgres is always available in this build; MySQL/SQLite are
        // feature-gated, so add them only when compiled in.
        let mut dialects: Vec<&dyn Dialect> = vec![&crate::sql::Postgres];
        #[cfg(feature = "mysql")]
        dialects.push(&crate::sql::MySql);
        #[cfg(feature = "sqlite")]
        dialects.push(&crate::sql::Sqlite);
        for dialect in dialects {
            let mut s = String::new();
            write_column_def(&mut s, dialect, &f);
            assert!(
                s.contains("DEFAULT ''"),
                "[{}] expected DEFAULT '': {s}",
                dialect.name()
            );
            assert!(
                !s.contains("DEFAULT  "),
                "[{}] empty default leaked a blank: {s}",
                dialect.name()
            );
        }
    }

    #[test]
    fn empty_string_default_on_lob_uses_mysql_expression_form() {
        // #1174 — an empty-string default on a MySQL LOB column (TEXT/JSON/
        // BLOB, i.e. `String` with no `max_length`) must emit the
        // parenthesized expression form `DEFAULT ('')`; MySQL rejects a
        // *literal* default on those types (error 1101). PG/SQLite still emit
        // the plain `DEFAULT ''` (they accept literal defaults on TEXT).
        let f = fld("body", FieldType::String, false, Some("")); // no max_length → TEXT
        #[cfg(feature = "mysql")]
        {
            let mut s = String::new();
            write_column_def(&mut s, &crate::sql::MySql, &f);
            assert!(
                s.contains("DEFAULT ('')"),
                "[mysql] LOB empty default must be paren-wrapped: {s}"
            );
        }
        let mut s = String::new();
        write_column_def(&mut s, &crate::sql::Postgres, &f);
        assert!(
            s.contains("DEFAULT ''") && !s.contains("DEFAULT ('')"),
            "[postgres] LOB empty default stays a literal: {s}"
        );
    }

    #[test]
    fn nonempty_string_default_is_unchanged() {
        // A non-empty default is still a raw expression, untouched.
        let f = fld("status", FieldType::String, false, Some("'active'"));
        let mut s = String::new();
        write_column_def(&mut s, &crate::sql::Postgres, &f);
        assert!(s.contains("DEFAULT 'active'"), "got: {s}");
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

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_auto_pk_does_not_double_emit_primary_key() {
        // SQLite's `Auto<T>` PK must emit `INTEGER PRIMARY KEY
        // AUTOINCREMENT` (PK clause inline with the type) and NOT
        // an additional `PRIMARY KEY` keyword from the standard
        // append path. Doubled-PK CREATE TABLE crashes the SQLite
        // parser.
        let dialect = crate::sql::Sqlite;
        let mut col_def = String::new();
        let mut field = fld("id", FieldType::I64, true, None);
        field.primary_key = true;
        write_column_def(&mut col_def, &dialect, &field);
        let n_pk = col_def.matches(" PRIMARY KEY").count();
        assert_eq!(
            n_pk, 1,
            "SQLite Auto PK should emit exactly one PRIMARY KEY token, got: {col_def}"
        );
        assert!(col_def.contains("AUTOINCREMENT"), "got: {col_def}");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_non_auto_pk_still_appends_primary_key() {
        // A plain (non-Auto) PK column on SQLite still wants the
        // standard PRIMARY KEY append — the inline-pk shortcut only
        // applies when the type itself is `INTEGER PRIMARY KEY
        // AUTOINCREMENT`.
        let dialect = crate::sql::Sqlite;
        let mut col_def = String::new();
        let mut field = fld("slug", FieldType::String, false, None);
        field.primary_key = true;
        write_column_def(&mut col_def, &dialect, &field);
        assert!(col_def.contains(" PRIMARY KEY"), "got: {col_def}");
    }

    // -------- Inline FK on SQLite (#559: silent referential-integrity loss) --------
    //
    // Before this PR, SQLite tables were created without FK clauses
    // and `create_constraints_sql_with_dialect` would (separately)
    // emit `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` — which SQLite
    // doesn't support. The earlier workaround skipped FKs entirely
    // on SQLite, silently losing referential integrity.
    //
    // Fix: when `dialect.inline_fks_in_create_table()` is true, the
    // FK clauses are emitted INSIDE the CREATE TABLE statement and
    // `create_constraints_sql_with_dialect` returns empty.

    fn fk_model() -> ModelSchema {
        let mut fk_field = fld("author_id", FieldType::I64, false, None);
        fk_field.relation = Some(Relation::Fk {
            to: "authors",
            on: "id",
        });
        let id_field = {
            let mut f = fld("id", FieldType::I64, true, None);
            f.primary_key = true;
            f
        };
        ModelSchema {
            name: "Post",
            table: "posts",
            fields: Box::leak(Box::new([id_field, fk_field])),
            display: None,
            app_label: None,
            admin: None,
            soft_delete_column: None,
            permissions: false,
            audit_track: None,
            m2m: &[],
            indexes: &[],
            check_constraints: &[],
            exclusion_constraints: &[],
            default_permissions: &[],
            composite_relations: &[],
            generic_relations: &[],
            scope: crate::core::ModelScope::Tenant,
            default_order: &[],
            is_view: false,
            verbose_name: None,
            verbose_name_plural: None,
            managed: true,
            db_table_comment: None,
            default_related_name: None,
            base_manager_name: None,
            required_db_vendor: None,
            required_db_features: &[],
            order_with_respect_to: None,
            proxy: false,
            get_latest_by: None,
            extra_permissions: &[],
            global_scopes: &[],
        }
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_inlines_fk_in_create_table() {
        let model = fk_model();
        let sql = create_table_sql_with_dialect(&crate::sql::Sqlite, &model);
        // FK constraint emitted INSIDE the CREATE TABLE statement.
        assert!(
            sql.contains(r#"CONSTRAINT "posts_author_id_fkey" FOREIGN KEY ("author_id") REFERENCES "authors" ("id")"#),
            "expected inline FK clause; got: {sql}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_returns_empty_post_hoc_constraint_list() {
        // The runner asks for post-hoc ALTER ADD CONSTRAINT — return
        // empty since the FK is already in CREATE TABLE.
        let model = fk_model();
        let post_hoc = create_constraints_sql_with_dialect(&crate::sql::Sqlite, &model);
        assert!(
            post_hoc.is_empty(),
            "SQLite must return empty post-hoc constraint list (FKs are inline): {post_hoc:?}"
        );
    }

    #[test]
    fn postgres_keeps_post_hoc_alter_path() {
        // PG path unchanged — FKs go through post-hoc ALTER ADD
        // CONSTRAINT so cross-table cycles resolve cleanly.
        let model = fk_model();
        let sql = create_table_sql_with_dialect(&crate::sql::Postgres, &model);
        assert!(
            !sql.contains("FOREIGN KEY"),
            "PG CREATE TABLE must NOT contain inline FK: {sql}"
        );
        let post_hoc = create_constraints_sql_with_dialect(&crate::sql::Postgres, &model);
        assert_eq!(post_hoc.len(), 1);
        assert!(post_hoc[0].contains("ALTER TABLE"));
        assert!(post_hoc[0].contains("ADD CONSTRAINT"));
        assert!(post_hoc[0].contains(r#"REFERENCES "authors" ("id")"#));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn mysql_keeps_post_hoc_alter_path() {
        let model = fk_model();
        let sql = create_table_sql_with_dialect(&crate::sql::MySql, &model);
        assert!(
            !sql.contains("FOREIGN KEY"),
            "MySQL CREATE TABLE must NOT contain inline FK: {sql}"
        );
        let post_hoc = create_constraints_sql_with_dialect(&crate::sql::MySql, &model);
        assert_eq!(post_hoc.len(), 1);
        assert!(post_hoc[0].contains("ALTER TABLE"));
        assert!(post_hoc[0].contains("ADD CONSTRAINT"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_inlines_fk_with_on_delete_cascade() {
        // ON DELETE action should also land inline.
        let id_field = {
            let mut f = fld("id", FieldType::I64, true, None);
            f.primary_key = true;
            f
        };
        let mut fk_field = fld("author_id", FieldType::I64, false, None);
        fk_field.relation = Some(Relation::Fk {
            to: "authors",
            on: "id",
        });
        fk_field.fk_on_delete = Some(crate::core::OnDeleteAction::Cascade);
        let model = ModelSchema {
            fields: Box::leak(Box::new([id_field, fk_field])),
            ..fk_model()
        };
        let sql = create_table_sql_with_dialect(&crate::sql::Sqlite, &model);
        assert!(
            sql.contains("ON DELETE CASCADE"),
            "expected inline ON DELETE CASCADE; got: {sql}"
        );
    }
}
