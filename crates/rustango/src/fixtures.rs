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
//! Each fixture is an array of JSON objects. For each object, the loader
//! emits an `INSERT INTO <table> (col1, col2, ...) VALUES (...)` against
//! the pool. Column names come from the JSON object's keys; values are
//! bound via sqlx parameter binding (no SQL injection).
//!
//! ## Ordering
//!
//! Fixtures load in registration order — register parent tables before
//! children to satisfy FK constraints.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::sql::sqlx::{self, PgPool};

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
        Self { name: name.into(), rows: Vec::new() }
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

    /// Insert every row into `table` against `pool`. Each row is a separate INSERT.
    ///
    /// # Errors
    /// [`FixtureError::Database`] on driver-level failures.
    pub async fn load_into(&self, table: &str, pool: &PgPool) -> Result<usize, FixtureError> {
        validate_ident(table)?;
        let mut count = 0;
        for row in &self.rows {
            insert_row(pool, table, row).await?;
            count += 1;
        }
        Ok(count)
    }
}

/// Load multiple fixtures in registration order. Stops at first error.
///
/// # Errors
/// First fixture error encountered.
pub async fn load_all(
    fixtures: &[(&str, &Fixture)],
    pool: &PgPool,
) -> Result<usize, FixtureError> {
    let mut total = 0;
    for (table, fixture) in fixtures {
        total += fixture.load_into(table, pool).await?;
    }
    Ok(total)
}

async fn insert_row(
    pool: &PgPool,
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
    let cols_sql: Vec<String> = columns.iter().map(|c| format!(r#""{c}""#)).collect();
    let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("${i}")).collect();
    let sql = format!(
        r#"INSERT INTO "{table}" ({}) VALUES ({})"#,
        cols_sql.join(", "),
        placeholders.join(", "),
    );

    let mut q = sqlx::query(&sql);
    for col in &columns {
        let val = &row[col.as_str()];
        q = bind_value(q, val);
    }
    q.execute(pool).await.map_err(|e| FixtureError::Database(e.to_string()))?;
    Ok(())
}

fn bind_value<'a>(
    q: sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments>,
    v: &'a Value,
) -> sqlx::query::Query<'a, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match v {
        Value::Null => q.bind(None::<i64>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else if let Some(f) = n.as_f64() {
                q.bind(f)
            } else {
                q.bind(n.to_string())
            }
        }
        Value::String(s) => q.bind(s.as_str()),
        Value::Array(_) | Value::Object(_) => q.bind(v.clone()),
    }
}

/// Reject identifiers (table / column names) with characters that could
/// break out of the quoted form. The full identifier is wrapped in `"..."`
/// so we just need to forbid `"`, NUL, and any control char.
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
        let path = std::env::temp_dir().join(format!(
            "rustango_fixture_bad_{}.json",
            std::process::id()
        ));
        std::fs::File::create(&path).unwrap().write_all(b"{not valid json").unwrap();
        let r = Fixture::new("x").from_file(&path);
        assert!(matches!(r, Err(FixtureError::Format { .. })));
        let _ = std::fs::remove_file(&path);
    }
}
