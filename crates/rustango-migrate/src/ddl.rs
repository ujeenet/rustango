//! Postgres DDL writer.
//!
//! Walks a `ModelSchema` and emits `CREATE TABLE` / `DROP TABLE` strings.
//! Foreign-key constraints are emitted separately as `ALTER TABLE` so the
//! caller doesn't have to topologically sort tables.
//!
//! Type mapping:
//! * `i32`     → `INTEGER`
//! * `i64`     → `BIGINT`
//! * `f32`     → `REAL`
//! * `f64`     → `DOUBLE PRECISION`
//! * `bool`    → `BOOLEAN`
//! * `String`  → `VARCHAR(N)` if `max_length` is set, otherwise `TEXT`
//! * `DateTime<Utc>` → `TIMESTAMPTZ`
//! * `NaiveDate`     → `DATE`
//! * `Uuid`    → `UUID`
//! * `serde_json::Value` → `JSONB`
//!
//! Bound mapping:
//! * `nullable: false`  → `NOT NULL`
//! * `primary_key: true` → `PRIMARY KEY`
//! * `min` / `max`      → `CHECK ("col" >= N AND "col" <= M)`
//! * `Relation::Fk` / `Relation::O2O` → emitted via [`create_constraints_sql`]

use std::fmt::Write as _;

use rustango_core::{FieldSchema, FieldType, ModelSchema, Relation};

/// `CREATE TABLE "model.table" ( … )` without FK constraints.
#[must_use]
pub fn create_table_sql(model: &ModelSchema) -> String {
    let mut s = String::new();
    s.push_str("CREATE TABLE ");
    write_ident(&mut s, model.table);
    s.push_str(" (");
    let mut first = true;
    for field in model.scalar_fields() {
        if !first {
            s.push_str(", ");
        }
        first = false;
        write_column_def(&mut s, field);
    }
    s.push(')');
    s
}

/// `CREATE TABLE IF NOT EXISTS …` — handy for idempotent dev bootstrapping.
#[must_use]
pub fn create_table_if_not_exists_sql(model: &ModelSchema) -> String {
    let mut s = create_table_sql(model);
    debug_assert!(s.starts_with("CREATE TABLE "));
    s.replace_range(.."CREATE TABLE".len(), "CREATE TABLE IF NOT EXISTS");
    s
}

/// `DROP TABLE [IF EXISTS] "model.table" [CASCADE]`.
#[must_use]
pub fn drop_table_sql(model: &ModelSchema, if_exists: bool, cascade: bool) -> String {
    let mut s = String::from("DROP TABLE ");
    if if_exists {
        s.push_str("IF EXISTS ");
    }
    write_ident(&mut s, model.table);
    if cascade {
        s.push_str(" CASCADE");
    }
    s
}

/// One `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` per FK / O2O field on
/// `model`. Emitted separately from `CREATE TABLE` so the caller can run
/// every model's `create_table_sql` first, then every model's
/// `create_constraints_sql`, sidestepping table-creation order entirely.
#[must_use]
pub fn create_constraints_sql(model: &ModelSchema) -> Vec<String> {
    let mut out = Vec::new();
    for field in model.scalar_fields() {
        let Some(rel) = field.relation else { continue };
        let (to, on) = match rel {
            Relation::Fk { to, on } | Relation::O2O { to, on } => (to, on),
            // M2M is reserved for v0.2; nothing to emit yet.
            Relation::M2M { .. } => continue,
        };
        let mut s = String::from("ALTER TABLE ");
        write_ident(&mut s, model.table);
        s.push_str(" ADD CONSTRAINT ");
        write_ident(&mut s, &format!("{}_{}_fkey", model.table, field.column));
        s.push_str(" FOREIGN KEY (");
        write_ident(&mut s, field.column);
        s.push_str(") REFERENCES ");
        write_ident(&mut s, to);
        s.push_str(" (");
        write_ident(&mut s, on);
        s.push(')');
        out.push(s);
    }
    out
}

fn write_column_def(s: &mut String, field: &FieldSchema) {
    write_ident(s, field.column);
    s.push(' ');
    s.push_str(&sql_type(field));
    if !field.nullable {
        s.push_str(" NOT NULL");
    }
    if field.primary_key {
        s.push_str(" PRIMARY KEY");
    }
    write_check_constraint(s, field);
}

fn write_check_constraint(s: &mut String, field: &FieldSchema) {
    if field.min.is_none() && field.max.is_none() {
        return;
    }
    s.push_str(" CHECK (");
    let mut wrote = false;
    if let Some(min) = field.min {
        write_ident(s, field.column);
        let _ = write!(s, " >= {min}");
        wrote = true;
    }
    if let Some(max) = field.max {
        if wrote {
            s.push_str(" AND ");
        }
        write_ident(s, field.column);
        let _ = write!(s, " <= {max}");
    }
    s.push(')');
}

fn sql_type(field: &FieldSchema) -> String {
    match field.ty {
        FieldType::I32 => "INTEGER".into(),
        FieldType::I64 => "BIGINT".into(),
        FieldType::F32 => "REAL".into(),
        FieldType::F64 => "DOUBLE PRECISION".into(),
        FieldType::Bool => "BOOLEAN".into(),
        FieldType::String => match field.max_length {
            Some(n) => format!("VARCHAR({n})"),
            None => "TEXT".into(),
        },
        FieldType::DateTime => "TIMESTAMPTZ".into(),
        FieldType::Date => "DATE".into(),
        FieldType::Uuid => "UUID".into(),
        FieldType::Json => "JSONB".into(),
    }
}

fn write_ident(sql: &mut String, name: &str) {
    sql.push('"');
    for ch in name.chars() {
        if ch == '"' {
            sql.push_str("\"\"");
        } else {
            sql.push(ch);
        }
    }
    sql.push('"');
}
