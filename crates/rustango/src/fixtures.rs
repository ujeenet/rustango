//! Test fixture loader — seed a test database from JSON files.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::fixtures::Fixture;
//!
//! // fixtures/users.json:
//! // [
//! //   {"username": "alice", "email": "a@x.com"},
//! //   {"username": "bob",   "email": "b@x.com"}
//! // ]
//!
//! Fixture::new("users")
//!     .from_file("fixtures/users.json")?
//!     .load_into("rustango_users", &pool).await?;
//! ```
//!
//! ## How it works
//!
//! A fixture is an array of JSON objects. Each object becomes one
//! `INSERT INTO <table> (col, ...) VALUES (...)`, all in one transaction.
//! When a registered model owns the table, keys must be its fields and
//! values are typed from them; values are always bound as parameters.
//!
//! Fixtures load in the order you register them, so load parent tables
//! before child tables to satisfy foreign keys.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

#[cfg(feature = "postgres")]
use crate::sql::sqlx::PgPool;
use crate::sql::{Pool, PoolTx};

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid fixture format in {file}: {detail}")]
    Format { file: String, detail: String },
    #[error("database error: {0}")]
    Database(String),
}

/// One named fixture — a list of JSON object rows.
pub struct Fixture {
    name: String,
    rows: Vec<serde_json::Map<String, Value>>,
}

impl Fixture {
    /// New empty fixture with the given name (used in error messages).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            rows: Vec::new(),
        }
    }

    /// Add one row to the fixture.
    #[must_use]
    pub fn with_row(mut self, row: serde_json::Map<String, Value>) -> Self {
        self.rows.push(row);
        self
    }

    /// Number of rows in the fixture.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Load rows from a JSON file. The file must contain an array of objects.
    ///
    /// # Errors
    /// [`FixtureError::Io`] when the file can't be read.
    /// [`FixtureError::Format`] when the JSON isn't an array of objects.
    pub fn from_file(mut self, path: impl AsRef<Path>) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| FixtureError::Io(e.to_string()))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| FixtureError::Format {
            file: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let arr = v.as_array().ok_or_else(|| FixtureError::Format {
            file: path.display().to_string(),
            detail: "expected top-level array".into(),
        })?;
        for (i, item) in arr.iter().enumerate() {
            let obj = item.as_object().ok_or_else(|| FixtureError::Format {
                file: path.display().to_string(),
                detail: format!("entry {i} is not an object"),
            })?;
            self.rows.push(obj.clone());
        }
        Ok(self)
    }

    /// Load rows from a JSON `Value` (must be an array of objects).
    ///
    /// # Errors
    /// [`FixtureError::Format`] when not an array of objects.
    pub fn from_value(mut self, v: Value) -> Result<Self, FixtureError> {
        let arr = v.as_array().ok_or_else(|| FixtureError::Format {
            file: self.name.clone(),
            detail: "expected top-level array".into(),
        })?;
        for (i, item) in arr.iter().enumerate() {
            let obj = item.as_object().ok_or_else(|| FixtureError::Format {
                file: self.name.clone(),
                detail: format!("entry {i} is not an object"),
            })?;
            self.rows.push(obj.clone());
        }
        Ok(self)
    }

    /// Insert every row into `table` on any supported backend, in one
    /// transaction. When a registered model owns `table`, values are
    /// typed from its fields and its id sequence is moved past the
    /// loaded ids; other tables bind JSON scalars as-is.
    ///
    /// # Errors
    /// [`FixtureError::Format`] for a key the model has no field for or
    /// a value of the wrong type; [`FixtureError::Database`] on driver
    /// failures. Nothing is inserted on error.
    pub async fn load_into_pool(&self, table: &str, pool: &Pool) -> Result<usize, FixtureError> {
        load_all_pool(&[(table, self)], pool).await
    }

    async fn load_tx(&self, table: &str, tx: &mut PoolTx<'_>) -> Result<usize, FixtureError> {
        validate_ident(table)?;
        let schema = model_for_table(table, &self.rows, &self.name)?;
        for row in &self.rows {
            match schema {
                Some(schema) => insert_typed_row(tx, schema, row, &self.name).await?,
                None => insert_row_raw(tx, table, row).await?,
            }
        }
        if let Some((sql, binds)) =
            schema.and_then(|s| crate::migrate::manage::reset_sequence_stmt(tx.dialect(), s))
        {
            crate::sql::raw_execute_tx(tx, &sql, binds)
                .await
                .map_err(|e| FixtureError::Database(e.to_string()))?;
        }
        Ok(self.rows.len())
    }

    /// PG-typed back-compat shim around [`Self::load_into_pool`].
    ///
    /// # Errors
    /// As [`Self::load_into_pool`].
    #[cfg(feature = "postgres")]
    pub async fn load_into(&self, table: &str, pool: &PgPool) -> Result<usize, FixtureError> {
        self.load_into_pool(table, &Pool::Postgres(pool.clone()))
            .await
    }
}

/// Load several fixtures in order on any supported backend, in one
/// transaction: the first error rolls every fixture back.
///
/// # Errors
/// First fixture error encountered.
pub async fn load_all_pool(
    fixtures: &[(&str, &Fixture)],
    pool: &Pool,
) -> Result<usize, FixtureError> {
    let db = |e: crate::sql::ExecError| FixtureError::Database(e.to_string());
    let mut scope = crate::sql::TxScope::begin(pool, crate::sql::Begin::Immediate)
        .await
        .map_err(db)?;
    let r: Result<usize, FixtureError> = async {
        let mut total = 0;
        for (table, fixture) in fixtures {
            total += fixture.load_tx(table, scope.tx()).await?;
        }
        Ok(total)
    }
    .await;
    let ended = scope.finish(r.is_ok()).await;
    let total = r?;
    ended.map_err(db)?;
    Ok(total)
}

/// PG-typed back-compat shim around [`load_all_pool`].
///
/// # Errors
/// First fixture error encountered.
#[cfg(feature = "postgres")]
pub async fn load_all(fixtures: &[(&str, &Fixture)], pool: &PgPool) -> Result<usize, FixtureError> {
    load_all_pool(fixtures, &Pool::Postgres(pool.clone())).await
}

/// The registered model whose table is `table`, if any.
fn model_for_table(
    table: &str,
    rows: &[serde_json::Map<String, Value>],
    fixture: &str,
) -> Result<Option<&'static crate::core::ModelSchema>, FixtureError> {
    let candidates: Vec<&crate::core::ModelEntry> = inventory::iter::<crate::core::ModelEntry>()
        .filter(|e| e.schema.table == table)
        .collect();
    pick_model(&candidates, rows, fixture)
}

/// Two models can share a table (a custom user model on `rustango_users`):
/// prefer the one whose fields cover every key, then the project's own.
fn pick_model(
    candidates: &[&crate::core::ModelEntry],
    rows: &[serde_json::Map<String, Value>],
    fixture: &str,
) -> Result<Option<&'static crate::core::ModelSchema>, FixtureError> {
    if candidates.len() <= 1 {
        return Ok(candidates.first().map(|e| e.schema));
    }
    let covers = |e: &&&crate::core::ModelEntry| {
        rows.iter().flat_map(|r| r.keys()).all(|key| {
            e.schema
                .scalar_fields()
                .any(|f| f.name == key || f.column == key)
        })
    };
    let covering: Vec<&crate::core::ModelEntry> =
        candidates.iter().filter(covers).copied().collect();
    let fits = if covering.is_empty() {
        candidates
    } else {
        &covering[..]
    };
    let project: Vec<&crate::core::ModelEntry> =
        fits.iter().filter(|e| !e.is_framework()).copied().collect();
    match (fits, &project[..]) {
        ([only], _) | (_, [only]) => Ok(Some(only.schema)),
        _ => Err(FixtureError::Format {
            file: fixture.to_owned(),
            detail: format!(
                "models {} all fit table `{}`; remove one",
                fits.iter()
                    .map(|e| format!("`{}::{}`", e.module_path, e.schema.name))
                    .collect::<Vec<_>>()
                    .join(", "),
                fits[0].schema.table,
            ),
        }),
    }
}

/// One row through the ORM insert, each value typed by its field.
async fn insert_typed_row(
    tx: &mut PoolTx<'_>,
    schema: &'static crate::core::ModelSchema,
    row: &serde_json::Map<String, Value>,
    fixture: &str,
) -> Result<(), FixtureError> {
    let bad = |detail: String| FixtureError::Format {
        file: fixture.to_owned(),
        detail,
    };
    if row.is_empty() {
        return Err(bad("row has no columns".into()));
    }
    let mut columns = Vec::with_capacity(row.len());
    let mut values = Vec::with_capacity(row.len());
    for (key, raw) in row {
        let field = schema
            .scalar_fields()
            .find(|f| f.name == key || f.column == key)
            .ok_or_else(|| bad(format!("`{}` has no field `{key}`", schema.table)))?;
        let value = crate::migrate::manage::json_to_sql_value(raw, field)
            .map_err(|e| bad(format!("field `{key}`: {e}")))?;
        columns.push(field.column);
        values.push(value);
    }
    let query = crate::core::InsertQuery {
        model: schema,
        columns,
        values,
        returning: vec![],
        on_conflict: None,
    };
    crate::sql::insert_tx(tx, &query)
        .await
        .map_err(|e| FixtureError::Database(e.to_string()))
}

async fn insert_row_raw(
    tx: &mut PoolTx<'_>,
    table: &str,
    row: &serde_json::Map<String, Value>,
) -> Result<(), FixtureError> {
    if row.is_empty() {
        return Err(FixtureError::Format {
            file: table.to_owned(),
            detail: "row has no columns".into(),
        });
    }
    let columns: Vec<&String> = row.keys().collect();
    for col in &columns {
        validate_ident(col)?;
    }
    let dialect = tx.dialect();
    let cols_sql: Vec<String> = columns.iter().map(|c| dialect.quote_ident(c)).collect();
    let placeholders: Vec<String> = (1..=columns.len())
        .map(|i| dialect.placeholder(i))
        .collect();
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        dialect.quote_ident(table),
        cols_sql.join(", "),
        placeholders.join(", "),
    );
    let binds: Vec<crate::core::SqlValue> = columns
        .iter()
        .map(|col| value_to_sqlvalue(&row[col.as_str()]))
        .collect();
    crate::sql::raw_execute_tx(tx, &sql, binds)
        .await
        .map_err(|e| FixtureError::Database(e.to_string()))?;
    Ok(())
}

/// `serde_json::Value` → `SqlValue`. Scalars map to typed variants;
/// arrays and objects become `SqlValue::Json`, which the executor
/// binds as JSONB, JSON or TEXT depending on the backend.
fn value_to_sqlvalue(v: &Value) -> crate::core::SqlValue {
    use crate::core::SqlValue;
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::I64(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::F64(f)
            } else {
                SqlValue::String(n.to_string())
            }
        }
        Value::String(s) => SqlValue::String(s.clone()),
        Value::Array(_) | Value::Object(_) => SqlValue::Json(v.clone()),
    }
}

/// Reject a table or column name that could break out of the quoting.
/// The name is wrapped in quotes, so forbidding `"`, backslash, NUL and
/// control characters is enough.
fn validate_ident(name: &str) -> Result<(), FixtureError> {
    if name.is_empty() {
        return Err(FixtureError::Format {
            file: "<ident>".into(),
            detail: "identifier is empty".into(),
        });
    }
    let bad: HashSet<char> = ['"', '\0', '\n', '\r', '\\'].into();
    if name.chars().any(|c| bad.contains(&c) || c.is_control()) {
        return Err(FixtureError::Format {
            file: "<ident>".into(),
            detail: format!("identifier `{name}` contains forbidden characters"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(dead_code)]
    #[derive(crate::Model, Debug)]
    #[rustango(table = "fx_pick_users")]
    pub struct FxPlainUser {
        #[rustango(primary_key)]
        pub id: crate::sql::Auto<i64>,
        #[rustango(max_length = 32)]
        pub username: String,
    }

    #[allow(dead_code)]
    #[derive(crate::Model, Debug)]
    #[rustango(table = "fx_pick_users")]
    pub struct FxCustomUser {
        #[rustango(primary_key)]
        pub id: crate::sql::Auto<i64>,
        #[rustango(max_length = 32)]
        pub username: String,
        #[rustango(max_length = 32)]
        pub display_name: String,
    }

    fn entry(
        schema: &'static crate::core::ModelSchema,
        framework: bool,
    ) -> crate::core::ModelEntry {
        let path = if framework {
            "rustango::tenancy::auth"
        } else {
            "app::models"
        };
        crate::core::ModelEntry::new(schema, path)
    }

    /// The picked model's name, trying both registration orders.
    fn pick(
        a: &crate::core::ModelEntry,
        b: &crate::core::ModelEntry,
        keys: serde_json::Value,
    ) -> Result<&'static str, FixtureError> {
        let rows = vec![keys.as_object().unwrap().clone()];
        let one = pick_model(&[a, b], &rows, "fx")?.unwrap().name;
        let two = pick_model(&[b, a], &rows, "fx")?.unwrap().name;
        assert_eq!(one, two, "the pick depends on registration order");
        Ok(one)
    }

    #[test]
    fn pick_model_prefers_the_model_covering_the_keys() {
        use crate::core::Model as _;
        let (plain, custom) = (
            entry(FxPlainUser::SCHEMA, true),
            entry(FxCustomUser::SCHEMA, true),
        );
        let keys = json!({"username": "a", "display_name": "A"});
        assert_eq!(pick(&plain, &custom, keys).unwrap(), "FxCustomUser");
    }

    #[test]
    fn pick_model_prefers_the_project_model_when_both_cover() {
        use crate::core::Model as _;
        let keys = || json!({"username": "a"});
        let (fw, app) = (
            entry(FxPlainUser::SCHEMA, true),
            entry(FxCustomUser::SCHEMA, false),
        );
        assert_eq!(pick(&fw, &app, keys()).unwrap(), "FxCustomUser");
        let (fw, app) = (
            entry(FxCustomUser::SCHEMA, true),
            entry(FxPlainUser::SCHEMA, false),
        );
        assert_eq!(pick(&fw, &app, keys()).unwrap(), "FxPlainUser");
        // No model covers `nickname`: still the project model, whose insert reports the key.
        let keys = json!({"nickname": "a"});
        assert_eq!(pick(&fw, &app, keys).unwrap(), "FxPlainUser");
    }

    #[test]
    fn pick_model_errors_naming_both_when_ambiguous() {
        use crate::core::Model as _;
        let (a, b) = (
            entry(FxPlainUser::SCHEMA, false),
            entry(FxCustomUser::SCHEMA, false),
        );
        let err = pick(&a, &b, json!({"username": "a"}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("FxPlainUser") && err.contains("FxCustomUser"),
            "{err}"
        );
    }

    #[test]
    fn fixture_with_row_increments_count() {
        let f = Fixture::new("test")
            .with_row(json!({"a": 1}).as_object().unwrap().clone())
            .with_row(json!({"a": 2}).as_object().unwrap().clone());
        assert_eq!(f.row_count(), 2);
    }

    #[test]
    fn from_value_parses_array() {
        let v = json!([{"name": "alice"}, {"name": "bob"}]);
        let f = Fixture::new("users").from_value(v).unwrap();
        assert_eq!(f.row_count(), 2);
    }

    #[test]
    fn from_value_rejects_non_array() {
        let v = json!({"not": "an array"});
        let r = Fixture::new("x").from_value(v);
        assert!(matches!(r, Err(FixtureError::Format { .. })));
    }

    #[test]
    fn from_value_rejects_non_object_entry() {
        let v = json!([{"ok": 1}, "scalar-not-object"]);
        let r = Fixture::new("x").from_value(v);
        assert!(matches!(r, Err(FixtureError::Format { .. })));
    }

    #[test]
    fn validate_ident_accepts_normal() {
        assert!(validate_ident("users").is_ok());
        assert!(validate_ident("user_id").is_ok());
        assert!(validate_ident("rustango_audit_log").is_ok());
    }

    #[test]
    fn validate_ident_rejects_quote() {
        assert!(validate_ident("evil\"name").is_err());
    }

    #[test]
    fn validate_ident_rejects_newline() {
        assert!(validate_ident("a\nb").is_err());
    }

    #[test]
    fn validate_ident_rejects_empty() {
        assert!(validate_ident("").is_err());
    }

    #[test]
    fn from_file_loads_array() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "rustango_fixture_test_{}_{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(br#"[{"id": 1, "name": "one"}, {"id": 2, "name": "two"}]"#)
            .unwrap();
        let f = Fixture::new("test").from_file(&path).unwrap();
        assert_eq!(f.row_count(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_missing_file_is_io_error() {
        let r = Fixture::new("x").from_file("/no/such/file/exists.json");
        assert!(matches!(r, Err(FixtureError::Io(_))));
    }

    #[test]
    fn from_file_invalid_json_is_format_error() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("rustango_fixture_bad_{}.json", std::process::id()));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"{not valid json")
            .unwrap();
        let r = Fixture::new("x").from_file(&path);
        assert!(matches!(r, Err(FixtureError::Format { .. })));
        let _ = std::fs::remove_file(&path);
    }
}
