//! DDL writer.
//!
//! Walks a `ModelSchema` and emits `CREATE TABLE` / `DROP TABLE` strings.
//! Foreign-key constraints are emitted separately as `ALTER TABLE` so the
//! caller doesn't have to topologically sort tables.
//!
//! ## Picking a dialect
//!
//! Every emitter has a `_with_dialect` variant taking `&dyn Dialect`.
//! The plain names (`create_table_sql`, `drop_table_sql`,
//! `create_constraints_sql`) are shims that pass
//! [`crate::sql::Postgres`]. Code holding a [`crate::sql::Pool`] should
//! pass `pool.dialect()` instead.
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

/// `CREATE TABLE "model.table" ( … )` without FK constraints, in
/// Postgres shape. For other backends use
/// [`create_table_sql_with_dialect`].
#[must_use]
pub fn create_table_sql(model: &ModelSchema) -> String {
    create_table_sql_with_dialect(&Postgres, model)
}

/// `CREATE TABLE IF NOT EXISTS …`, for repeatable dev bootstrapping.
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

// ============================================================ dialect-aware emitters

/// `CREATE TABLE` for `model`, using `dialect` for identifier quoting,
/// type names and the `Auto<T>` serial spelling.
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
    // SQLite has no `ALTER TABLE ADD CONSTRAINT FOREIGN KEY`, so its
    // FKs must go inside this statement. PG and MySQL get theirs
    // afterwards from `create_constraints_sql_with_dialect`, which
    // lets cross-table cycles resolve in one migration batch.
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
///
/// **`cascade` is not checked against the dialect.** `MySQL` rejects
/// the `CASCADE` keyword on `DROP TABLE`, so only pass `true` when you
/// know the backend accepts it.
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
        // PG accepts CASCADE, MySQL rejects it. The runner only asks
        // for it on Postgres.
        s.push_str(" CASCADE");
    }
    s
}

/// The inverse of [`create_constraints_sql_with_dialect`]: one statement
/// per FK / O2O field and per composite FK, dropping the constraint that
/// the create emitter named `{table}_{column}_fkey`.
///
/// Run these before dropping tables so drop order does not matter.
/// Postgres has `DROP TABLE ... CASCADE` and SQLite leaves
/// `foreign_keys` off by default, but **MySQL enforces FKs and has no
/// `CASCADE`**, so dropping a parent before its child fails there.
///
/// Empty for dialects that inline FKs in `CREATE TABLE` (SQLite):
/// there is no named constraint to drop.
///
/// Callers should ignore errors. A constraint may already be gone, and
/// only Postgres accepts `IF EXISTS` here.
#[must_use]
pub fn drop_constraints_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    if dialect.inline_fks_in_create_table() {
        return Vec::new();
    }
    // The dialect owns the spelling: `DROP FOREIGN KEY` on MySQL,
    // `DROP CONSTRAINT IF EXISTS` elsewhere. `extend`, not `push`,
    // because a dialect with no drop-constraint syntax returns `None`
    // and must emit nothing rather than Postgres DDL.
    let mut out = Vec::new();
    let mut push = |name: String| out.extend(dialect.drop_foreign_key_sql(model.table, &name));
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

/// One `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` per FK or O2O
/// field, plus one per `#[rustango(fk_composite(...))]`. PG and MySQL
/// share the syntax; only identifier quoting differs.
#[must_use]
pub fn create_constraints_sql_with_dialect(
    dialect: &dyn Dialect,
    model: &ModelSchema,
) -> Vec<String> {
    // SQLite has no `ALTER TABLE ADD CONSTRAINT FOREIGN KEY`. Its FKs
    // already went inline via `inline_fk_clauses`, so emit nothing.
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
    // Composite FKs. The derive macro already checks that `from` and
    // `on` have the same length.
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

/// FK clauses to join with `, ` into a `CREATE TABLE (...)` body, for
/// dialects where `inline_fks_in_create_table()` is true (SQLite).
///
/// One clause per single-column FK or O2O field, and one per
/// `composite_relations` entry.
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
    // A generated column takes no DEFAULT, PRIMARY KEY, UNIQUE or
    // CHECK: Postgres rejects all of them here. NOT NULL is allowed.
    if let Some(expr) = field.generated_as {
        let _ = write!(s, " GENERATED ALWAYS AS ({expr}) STORED");
        if !field.nullable {
            s.push_str(" NOT NULL");
        }
        return;
    }
    if let Some(expr) = field.default {
        let ty_name = crate::migrate::snapshot::field_type_name(field.ty);
        // `#[rustango(default = "")]` means the empty string, not an
        // empty expression, so render `''`. Writing nothing would give
        // `DEFAULT  NOT NULL`, which every driver rejects.
        //
        // Still pass `''` through `translate_default_expr`: MySQL
        // rejects a literal default on a LOB column (TEXT/JSON/BLOB)
        // and needs the `DEFAULT ('')` form. PG and SQLite keep `''`.
        let expr_to_render = if expr.is_empty() { "''" } else { expr };
        let rendered = dialect.translate_default_expr(expr_to_render, ty_name, field.max_length);
        let _ = write!(s, " DEFAULT {rendered}");
    }
    if !field.nullable {
        s.push_str(" NOT NULL");
    }
    // On SQLite the `Auto<T>` PK type is `INTEGER PRIMARY KEY
    // AUTOINCREMENT`, so PRIMARY KEY is already in the type name.
    // Appending it again gives a doubled clause SQLite cannot parse.
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
    // MySQL puts `COMMENT '...'` on the column line. PG uses a
    // separate `COMMENT ON COLUMN` statement from
    // `column_comment_statements_with_dialect`; SQLite has none.
    if let Some(comment) = field.db_comment {
        if let Some(inline) = dialect.write_inline_column_comment(comment) {
            s.push_str(&inline);
        }
    }
}

/// Statements to run after `CREATE TABLE` for `db_comment`: one
/// `COMMENT ON COLUMN` per field on Postgres. Empty on MySQL, which
/// inlines them, and on SQLite, which has no column comments.
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

/// SQL type for one field.
///
/// Only an **integer** `Auto<T>` primary key becomes a serial type via
/// [`Dialect::serial_type`]. Other `Auto` fields (`Auto<Uuid>`,
/// `Auto<DateTime<Utc>>`) use [`Dialect::column_type`]: their value
/// comes from the field's own `default` expression, not a sequence.
///
/// Without that split, `#[rustango(auto_now_add)] created_at:
/// Auto<DateTime<Utc>>` would emit `BIGSERIAL DEFAULT now()`, and
/// Postgres rejects two defaults on one column.
fn sql_type(dialect: &dyn Dialect, field: &FieldSchema) -> String {
    if field.auto && matches!(field.ty, FieldType::I16 | FieldType::I32 | FieldType::I64) {
        return dialect.serial_type(field.ty).to_owned();
    }
    // Case-insensitive text only means something for `String`.
    if field.case_insensitive && matches!(field.ty, FieldType::String) {
        return dialect.ci_text_type(field.max_length);
    }
    dialect.column_type(field.ty, field.max_length)
}

#[cfg(test)]
mod tests {
    //! `auto = true` on a non-integer field must not emit a serial
    //! type, or the column ends up with two DEFAULT clauses and
    //! Postgres rejects the `CREATE TABLE`.
    //!
    //! Covers: integer `Auto` PKs still emit SERIAL / BIGSERIAL;
    //! `Auto<DateTime>` emits `TIMESTAMPTZ` and `Auto<Uuid>` emits
    //! `UUID`; and the full DDL has one DEFAULT per column.

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
        // `BIGSERIAL` here would collide with the field's own
        // `DEFAULT now()`, and Postgres rejects two defaults.
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
        // `default = ""` must emit `DEFAULT ''`. Writing nothing gives
        // `DEFAULT  NOT NULL`, a syntax error on every backend.
        let mut f = fld("name", FieldType::String, false, Some(""));
        f.max_length = Some(64);
        // Postgres is always built; the others are feature-gated.
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
        // MySQL rejects a literal default on a LOB column, so an
        // empty default there must be `DEFAULT ('')`. PG and SQLite
        // accept the plain literal.
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
        let mut col_def = String::new();
        write_column_def(
            &mut col_def,
            &pg(),
            &fld("created_at", FieldType::DateTime, true, Some("now()")),
        );
        // Expect `"created_at" TIMESTAMPTZ DEFAULT now() NOT NULL`.
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
        // `BIGSERIAL` brings its own nextval default, so no explicit
        // `DEFAULT` clause should appear.
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
        // `INTEGER PRIMARY KEY AUTOINCREMENT` already carries the PK
        // clause. A second `PRIMARY KEY` breaks the SQLite parser.
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
        // The inline-PK shortcut only applies to the AUTOINCREMENT
        // type, so a plain PK column still gets the appended clause.
        let dialect = crate::sql::Sqlite;
        let mut col_def = String::new();
        let mut field = fld("slug", FieldType::String, false, None);
        field.primary_key = true;
        write_column_def(&mut col_def, &dialect, &field);
        assert!(col_def.contains(" PRIMARY KEY"), "got: {col_def}");
    }

    // -------- Inline FK on SQLite --------
    //
    // SQLite has no `ALTER TABLE ADD CONSTRAINT FOREIGN KEY`, so when
    // `inline_fks_in_create_table()` is true the FK clauses must be
    // inside `CREATE TABLE` and `create_constraints_sql_with_dialect`
    // must return empty. Otherwise SQLite tables get no FKs at all.

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
        // The FK is already in CREATE TABLE, so there is nothing for
        // the runner's post-hoc ALTER step to do.
        let model = fk_model();
        let post_hoc = create_constraints_sql_with_dialect(&crate::sql::Sqlite, &model);
        assert!(
            post_hoc.is_empty(),
            "SQLite must return empty post-hoc constraint list (FKs are inline): {post_hoc:?}"
        );
    }

    #[test]
    fn postgres_keeps_post_hoc_alter_path() {
        // PG keeps FKs in post-hoc ALTER ADD CONSTRAINT so
        // cross-table cycles resolve cleanly.
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
