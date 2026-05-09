//! `manage inspectdb` — emit `#[derive(Model)]` source for every
//! table in a live database. Mirrors Django's `inspectdb` shape:
//! point at an existing schema, get a copy-paste-ready Rust file,
//! adopt rustango against the existing data without rewriting it.
//!
//! ## What it covers
//!
//! - Tables in the `public` schema (default; override via `--schema`)
//! - Standard column types: integers, text/varchar, bool, timestamptz,
//!   date, uuid, jsonb, numeric/decimal
//! - PRIMARY KEY → `#[rustango(primary_key)]`
//! - NOT NULL → required field; nullable → `Option<T>`
//! - `varchar(N)` → `#[rustango(max_length = N)]`
//! - SERIAL / IDENTITY columns → `Auto<T>` PK wrapper
//! - FK references → `#[rustango(fk = "<target_table>")]`
//!
//! ## What it skips (v1)
//!
//! - Views, materialized views, foreign tables
//! - Composite primary keys (emits `id` as a PK, comments others
//!   as a marker for the user to decide)
//! - CHECK constraints (no rustango-side equivalent yet)
//! - Triggers, sequences, generated columns
//! - Custom enum types (mapped to `String` with a `// TODO` comment)
//! - Index definitions (use `manage makemigrations` after to capture)
//!
//! ## Usage
//!
//! ```sh
//! # Print every table in the `public` schema to stdout
//! cargo run -- inspectdb
//!
//! # Single table
//! cargo run -- inspectdb --table users
//!
//! # Different schema
//! cargo run -- inspectdb --schema reporting
//!
//! # Pipe to a file the user reviews + edits
//! cargo run -- inspectdb > src/legacy/models.rs
//! ```

use std::io::Write;

use crate::sql::sqlx::{self, PgPool, Row as _};

use super::error::MigrateError;

/// Parsed args for the `inspectdb` verb.
#[derive(Debug, Default)]
pub(super) struct InspectdbArgs {
    /// PostgreSQL schema name to scan. Default `"public"`.
    pub schema: String,
    /// When `Some(name)`, only emit that one table. When `None`,
    /// emit every base table in the schema.
    pub table: Option<String>,
}

/// Parse `[--schema <name>] [--table <name>]` into `InspectdbArgs`.
/// Unknown flags surface as a clear error; positional args are
/// rejected (every input is flag-based to avoid ambiguity).
pub(super) fn parse_inspectdb_args(args: &[String]) -> Result<InspectdbArgs, MigrateError> {
    let mut out = InspectdbArgs {
        schema: "public".to_owned(),
        table: None,
    };
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--schema" => {
                let v = iter.next().ok_or_else(|| {
                    MigrateError::Validation("`--schema` requires a value".into())
                })?;
                out.schema = v.clone();
            }
            "--table" => {
                let v = iter
                    .next()
                    .ok_or_else(|| MigrateError::Validation("`--table` requires a value".into()))?;
                out.table = Some(v.clone());
            }
            "--help" | "-h" => {
                return Err(MigrateError::Validation(
                    "USAGE: manage inspectdb [--schema <name>] [--table <name>]".into(),
                ));
            }
            other => {
                return Err(MigrateError::Validation(format!(
                    "unknown flag `{other}` for inspectdb (try --help)"
                )));
            }
        }
    }
    Ok(out)
}

/// Top-level entry — read the live schema, emit Rust source.
pub(super) async fn inspectdb_cmd<W: Write>(
    pool: &PgPool,
    args: &[String],
    w: &mut W,
) -> Result<(), MigrateError> {
    let parsed = parse_inspectdb_args(args)?;
    let tables = list_tables(pool, &parsed.schema, parsed.table.as_deref()).await?;
    if tables.is_empty() {
        writeln!(
            w,
            "// no tables found in schema `{}`",
            escape_for_comment(&parsed.schema)
        )?;
        return Ok(());
    }
    write_header(w, &parsed.schema)?;
    for table in &tables {
        let columns = list_columns(pool, &parsed.schema, table).await?;
        let pk_columns = list_pk_columns(pool, &parsed.schema, table).await?;
        let fks = list_fks(pool, &parsed.schema, table).await?;
        write_model(w, table, &columns, &pk_columns, &fks)?;
        writeln!(w)?;
    }
    Ok(())
}

fn write_header<W: Write>(w: &mut W, schema: &str) -> std::io::Result<()> {
    writeln!(
        w,
        "//! Auto-emitted by `manage inspectdb` — review before committing.\n\
         //!\n\
         //! Source schema: `{}`\n\
         //!\n\
         //! Edits you may need to make:\n\
         //! - Composite primary keys aren't supported by `#[derive(Model)]`;\n\
         //!   inspectdb picks the first PK column. Tighten as needed.\n\
         //! - Custom enum types map to `String` — wrap in a typed enum +\n\
         //!   `From<String>` if you want compile-time safety.\n\
         //! - CHECK constraints, triggers, generated columns, and indexes\n\
         //!   aren't reflected. After editing, run `manage makemigrations`\n\
         //!   to lock in the rest.\n",
        escape_for_comment(schema)
    )?;
    writeln!(w, "use rustango::sql::Auto;")?;
    writeln!(w, "use rustango::Model;\n")?;
    Ok(())
}

/// One column row from `information_schema.columns`. Materialized
/// into a struct (rather than queried per-field) so the emit pass
/// can fold `is_nullable` + `column_default` + `udt_name` together
/// without re-querying.
#[derive(Debug, Clone)]
pub(super) struct ColumnRow {
    pub name: String,
    /// Postgres `udt_name` (e.g. `int8`, `varchar`, `timestamptz`).
    pub udt_name: String,
    pub nullable: bool,
    pub max_length: Option<i32>,
    pub default: Option<String>,
    /// `true` when the column is auto-incremented (SERIAL/BIGSERIAL/IDENTITY).
    /// Detected via `column_default` matching `nextval(...)` or
    /// `is_identity = 'YES'`.
    pub is_auto: bool,
}

async fn list_tables(
    pool: &PgPool,
    schema: &str,
    only: Option<&str>,
) -> Result<Vec<String>, MigrateError> {
    let sql = if only.is_some() {
        // Single-table mode: filter both schema AND name.
        r#"SELECT table_name
           FROM information_schema.tables
           WHERE table_schema = $1
             AND table_type = 'BASE TABLE'
             AND table_name = $2
           ORDER BY table_name"#
    } else {
        r#"SELECT table_name
           FROM information_schema.tables
           WHERE table_schema = $1
             AND table_type = 'BASE TABLE'
           ORDER BY table_name"#
    };
    let mut q = sqlx::query(sql).bind(schema);
    if let Some(t) = only {
        q = q.bind(t);
    }
    let rows = q.fetch_all(pool).await.map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("table_name").unwrap_or_default())
        .collect())
}

async fn list_columns(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnRow>, MigrateError> {
    let rows = sqlx::query(
        r#"SELECT
              column_name,
              udt_name,
              is_nullable,
              character_maximum_length,
              column_default,
              COALESCE(is_identity, 'NO') AS is_identity
           FROM information_schema.columns
           WHERE table_schema = $1 AND table_name = $2
           ORDER BY ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.try_get("column_name").unwrap_or_default();
        let udt_name: String = r.try_get("udt_name").unwrap_or_default();
        let nullable: String = r.try_get("is_nullable").unwrap_or_else(|_| "NO".into());
        let max_length: Option<i32> = r.try_get("character_maximum_length").ok().flatten();
        let default: Option<String> = r.try_get("column_default").ok().flatten();
        let is_identity: String = r.try_get("is_identity").unwrap_or_else(|_| "NO".into());
        let is_auto = is_identity == "YES"
            || default
                .as_deref()
                .is_some_and(|d| d.starts_with("nextval("));
        out.push(ColumnRow {
            name,
            udt_name,
            nullable: nullable == "YES",
            max_length,
            default,
            is_auto,
        });
    }
    Ok(out)
}

async fn list_pk_columns(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, MigrateError> {
    // information_schema.table_constraints + key_column_usage —
    // the standard ANSI shape that's portable across PG versions.
    let rows = sqlx::query(
        r#"SELECT kcu.column_name
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
           WHERE tc.constraint_type = 'PRIMARY KEY'
             AND tc.table_schema = $1
             AND tc.table_name = $2
           ORDER BY kcu.ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| r.try_get::<String, _>("column_name").unwrap_or_default())
        .collect())
}

/// `(local_column, foreign_table_name)` per FK on this table. We
/// intentionally drop `foreign_column` since rustango's `fk = "..."`
/// attribute targets the table only (FK column is always the
/// target's PK).
async fn list_fks(
    pool: &PgPool,
    schema: &str,
    table: &str,
) -> Result<Vec<(String, String)>, MigrateError> {
    let rows = sqlx::query(
        r#"SELECT
              kcu.column_name AS local_column,
              ccu.table_name  AS foreign_table
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON tc.constraint_name = kcu.constraint_name
            AND tc.table_schema = kcu.table_schema
           JOIN information_schema.constraint_column_usage ccu
             ON ccu.constraint_name = tc.constraint_name
            AND ccu.table_schema = tc.table_schema
           WHERE tc.constraint_type = 'FOREIGN KEY'
             AND tc.table_schema = $1
             AND tc.table_name = $2
           ORDER BY kcu.ordinal_position"#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(MigrateError::Driver)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.try_get::<String, _>("local_column").unwrap_or_default(),
                r.try_get::<String, _>("foreign_table").unwrap_or_default(),
            )
        })
        .collect())
}

/// Map a Postgres type name (`udt_name`) to a `(rust_type,
/// notes_for_template)` tuple. Unknown types fall back to `String`
/// with a `// TODO` comment so the user notices.
pub(super) fn pg_type_to_rust(udt_name: &str) -> (&'static str, Option<&'static str>) {
    match udt_name {
        "bool" => ("bool", None),
        "int2" => ("i16", None),
        "int4" => ("i32", None),
        "int8" => ("i64", None),
        "float4" => ("f32", None),
        "float8" => ("f64", None),
        "numeric" => (
            "rust_decimal::Decimal",
            Some("// TODO: numeric → Decimal requires the `rust_decimal` dep"),
        ),
        "varchar" | "bpchar" | "text" | "citext" => ("String", None),
        "uuid" => ("uuid::Uuid", None),
        "jsonb" | "json" => ("serde_json::Value", None),
        "timestamptz" => ("chrono::DateTime<chrono::Utc>", None),
        "timestamp" => (
            "chrono::NaiveDateTime",
            Some("// TODO: `timestamp` (no tz) — prefer `timestamptz` if the column is a real point-in-time"),
        ),
        "date" => ("chrono::NaiveDate", None),
        "time" => ("chrono::NaiveTime", None),
        "bytea" => (
            "Vec<u8>",
            Some("// TODO: `bytea` is not a first-class rustango FieldType yet"),
        ),
        _ => (
            "String",
            Some(
                "// TODO: unknown PG type — falling back to String. \
                 Map to a real Rust type before persisting.",
            ),
        ),
    }
}

/// PascalCase the table name into a Rust type identifier.
/// `"user_profile"` → `"UserProfile"`, `"users"` → `"Users"`.
/// Single-word lowercase tables become PascalCase singletons —
/// `inspectdb` doesn't try to singularize (English rules are
/// thorny; the user can rename after).
pub(super) fn table_to_struct_name(table: &str) -> String {
    let mut out = String::with_capacity(table.len());
    let mut next_upper = true;
    for c in table.chars() {
        if c == '_' || c == '-' || c == ' ' {
            next_upper = true;
            continue;
        }
        if next_upper {
            out.extend(c.to_uppercase());
            next_upper = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push_str("Unnamed");
    }
    out
}

/// Emit one `#[derive(Model)]` block for a table. Returns when the
/// writer accepts every byte; downstream errors propagate via `?`.
fn write_model<W: Write>(
    w: &mut W,
    table: &str,
    columns: &[ColumnRow],
    pk_columns: &[String],
    fks: &[(String, String)],
) -> std::io::Result<()> {
    let struct_name = table_to_struct_name(table);
    writeln!(w, "/// Auto-emitted from `{table}` by `inspectdb`.")?;
    if pk_columns.len() > 1 {
        writeln!(
            w,
            "/// NOTE: composite primary key — `inspectdb` picked `{}`. \
             Adjust as needed.",
            pk_columns.first().map(String::as_str).unwrap_or("(none)")
        )?;
    }
    writeln!(w, "#[derive(Model, Debug, Clone)]")?;
    writeln!(w, r#"#[rustango(table = "{table}")]"#)?;
    writeln!(w, "pub struct {struct_name} {{")?;
    let pk_first = pk_columns.first().cloned();
    for col in columns {
        emit_field(w, col, pk_first.as_deref(), fks)?;
    }
    writeln!(w, "}}")?;
    Ok(())
}

fn emit_field<W: Write>(
    w: &mut W,
    col: &ColumnRow,
    pk_column: Option<&str>,
    fks: &[(String, String)],
) -> std::io::Result<()> {
    let (rust_ty, todo_note) = pg_type_to_rust(&col.udt_name);
    let is_pk = pk_column == Some(col.name.as_str());
    let fk_target = fks
        .iter()
        .find(|(local, _)| local == &col.name)
        .map(|(_, target)| target.as_str());

    // Build the `#[rustango(...)]` attribute body.
    let mut attrs: Vec<String> = Vec::new();
    if is_pk {
        attrs.push("primary_key".into());
    }
    if let Some(target) = fk_target {
        attrs.push(format!(r#"fk = "{target}""#));
    }
    if let Some(max) = col.max_length {
        if col.udt_name == "varchar" || col.udt_name == "bpchar" {
            attrs.push(format!("max_length = {max}"));
        }
    }
    // DEFAULT: only echo when it's a SQL fragment we can quote.
    // Skip auto-increment defaults (they're implied by Auto<T>).
    if !col.is_auto {
        if let Some(default) = &col.default {
            // Strip the typecast suffix Postgres adds:
            // "'pending'::character varying" → "'pending'"
            let cleaned = default.split("::").next().unwrap_or(default).trim();
            if !cleaned.is_empty() {
                attrs.push(format!(r#"default = "{cleaned}""#));
            }
        }
    }

    // Apply Auto<T> wrapper when the column is auto-increment.
    let final_ty = if col.is_auto && is_pk {
        format!("Auto<{rust_ty}>")
    } else if col.nullable {
        format!("Option<{rust_ty}>")
    } else {
        rust_ty.to_owned()
    };

    if let Some(note) = todo_note {
        writeln!(w, "    {note}")?;
    }
    if !attrs.is_empty() {
        writeln!(w, "    #[rustango({})]", attrs.join(", "))?;
    }
    writeln!(w, "    pub {}: {final_ty},", sanitize_field_name(&col.name))?;
    Ok(())
}

/// Rust-keyword-safe field name. Wraps the rare collisions
/// (`type`, `match`, `box`, …) in `r#`. Most database column
/// names already match Rust naming conventions, so this is a
/// rare-path safety net.
fn sanitize_field_name(name: &str) -> String {
    const RUST_KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
    ];
    if RUST_KEYWORDS.iter().any(|k| *k == name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

/// Newlines and `*/` would break a `//` comment; strip both. Used
/// for any user-supplied string echoed into the file header.
fn escape_for_comment(s: &str) -> String {
    s.replace('\n', " ").replace("*/", "* /")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults_to_public_schema() {
        let a = parse_inspectdb_args(&[]).unwrap();
        assert_eq!(a.schema, "public");
        assert!(a.table.is_none());
    }

    #[test]
    fn parse_args_picks_up_schema_and_table() {
        let a = parse_inspectdb_args(&[
            "--schema".into(),
            "reporting".into(),
            "--table".into(),
            "events".into(),
        ])
        .unwrap();
        assert_eq!(a.schema, "reporting");
        assert_eq!(a.table.as_deref(), Some("events"));
    }

    #[test]
    fn parse_args_rejects_missing_value() {
        assert!(parse_inspectdb_args(&["--schema".into()]).is_err());
        assert!(parse_inspectdb_args(&["--table".into()]).is_err());
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_inspectdb_args(&["--bogus".into()]).unwrap_err();
        assert!(format!("{err}").contains("--bogus"));
    }

    #[test]
    fn pg_type_to_rust_covers_common_types() {
        assert_eq!(pg_type_to_rust("int8").0, "i64");
        assert_eq!(pg_type_to_rust("int4").0, "i32");
        assert_eq!(pg_type_to_rust("varchar").0, "String");
        assert_eq!(pg_type_to_rust("uuid").0, "uuid::Uuid");
        assert_eq!(
            pg_type_to_rust("timestamptz").0,
            "chrono::DateTime<chrono::Utc>"
        );
        assert_eq!(pg_type_to_rust("jsonb").0, "serde_json::Value");
        assert_eq!(pg_type_to_rust("bool").0, "bool");
    }

    /// Unknown types fall back to `String` with a `// TODO` note
    /// stamped above the field — important so the user notices
    /// before persisting.
    #[test]
    fn pg_type_to_rust_unknown_falls_back_to_string_with_note() {
        let (ty, note) = pg_type_to_rust("hstore");
        assert_eq!(ty, "String");
        assert!(note.is_some_and(|n| n.contains("TODO")));
    }

    #[test]
    fn table_to_struct_name_pascalcases() {
        assert_eq!(table_to_struct_name("user"), "User");
        assert_eq!(table_to_struct_name("user_profile"), "UserProfile");
        assert_eq!(table_to_struct_name("audit_log_entry"), "AuditLogEntry");
        assert_eq!(table_to_struct_name(""), "Unnamed");
    }

    #[test]
    fn sanitize_field_name_wraps_keywords() {
        assert_eq!(sanitize_field_name("type"), "r#type");
        assert_eq!(sanitize_field_name("match"), "r#match");
        assert_eq!(sanitize_field_name("normal"), "normal");
    }

    /// Auto-increment + PK → `Auto<i64>`. NOT NULL non-PK → bare type.
    /// Nullable non-PK → `Option<T>`. Field-level integration test.
    #[test]
    fn emit_field_chooses_correct_wrapper_per_state() {
        let mut buf: Vec<u8> = Vec::new();
        let cols = [
            ColumnRow {
                name: "id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: Some("nextval('users_id_seq')".into()),
                is_auto: true,
            },
            ColumnRow {
                name: "name".into(),
                udt_name: "varchar".into(),
                nullable: false,
                max_length: Some(80),
                default: None,
                is_auto: false,
            },
            ColumnRow {
                name: "bio".into(),
                udt_name: "text".into(),
                nullable: true,
                max_length: None,
                default: None,
                is_auto: false,
            },
        ];
        for col in &cols {
            emit_field(&mut buf, col, Some("id"), &[]).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("pub id: Auto<i64>"), "got: {out}");
        assert!(out.contains("pub name: String"), "got: {out}");
        assert!(out.contains("pub bio: Option<String>"), "got: {out}");
        assert!(out.contains("max_length = 80"));
        assert!(out.contains("primary_key"));
    }

    /// FK references emit `fk = "<target>"` on the local column.
    #[test]
    fn emit_field_attaches_fk_attribute() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "author_id".into(),
            udt_name: "int8".into(),
            nullable: false,
            max_length: None,
            default: None,
            is_auto: false,
        };
        emit_field(
            &mut buf,
            &col,
            Some("id"),
            &[("author_id".into(), "users".into())],
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(r#"fk = "users""#), "got: {out}");
        assert!(out.contains("pub author_id: i64"), "got: {out}");
    }

    /// Composite PK → only the first column gets `primary_key`;
    /// the others are bare fields. The struct-level doc comment
    /// flags the limitation so the user notices.
    #[test]
    fn write_model_handles_composite_pk_with_warning() {
        let mut buf: Vec<u8> = Vec::new();
        let cols = vec![
            ColumnRow {
                name: "user_id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: None,
                is_auto: false,
            },
            ColumnRow {
                name: "role_id".into(),
                udt_name: "int8".into(),
                nullable: false,
                max_length: None,
                default: None,
                is_auto: false,
            },
        ];
        let pk = vec!["user_id".to_owned(), "role_id".to_owned()];
        write_model(&mut buf, "user_roles", &cols, &pk, &[]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("composite primary key"),
            "expected composite-PK warning, got: {out}"
        );
        assert!(out.contains("pub struct UserRoles"));
    }

    /// Default values get echoed (with the typecast suffix
    /// stripped) — except for nextval(...) which is implied by
    /// Auto<T>.
    #[test]
    fn emit_field_strips_typecast_from_default() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "status".into(),
            udt_name: "varchar".into(),
            nullable: false,
            max_length: Some(20),
            default: Some("'pending'::character varying".into()),
            is_auto: false,
        };
        emit_field(&mut buf, &col, None, &[]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains(r#"default = "'pending'""#),
            "expected stripped default, got: {out}"
        );
        assert!(
            !out.contains("character varying"),
            "typecast leaked through: {out}"
        );
    }

    /// `nextval(...)` defaults are silently dropped — they're
    /// implied by the `Auto<T>` wrapper.
    #[test]
    fn emit_field_drops_nextval_default_when_auto() {
        let mut buf: Vec<u8> = Vec::new();
        let col = ColumnRow {
            name: "id".into(),
            udt_name: "int8".into(),
            nullable: false,
            max_length: None,
            default: Some("nextval('foo_id_seq')".into()),
            is_auto: true,
        };
        emit_field(&mut buf, &col, Some("id"), &[]).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains("default ="),
            "nextval default should not surface, got: {out}"
        );
        assert!(out.contains("Auto<i64>"));
    }

    #[test]
    fn escape_for_comment_strips_dangerous_sequences() {
        assert_eq!(escape_for_comment("foo\nbar"), "foo bar");
        assert_eq!(escape_for_comment("a*/b"), "a* /b");
    }
}
