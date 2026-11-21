//! `CompiledStatement` — the result of writing a query to a specific dialect.

use crate::core::SqlValue;

use super::{Dialect, SqlError};

/// A parameterized statement ready to hand to a driver.
///
/// `sql` uses dialect-specific placeholder syntax (e.g. `$1` for Postgres).
/// The params are in placeholder order: `params()[i - 1]` binds `$i`.
/// Built only by a dialect's writer or [`Self::new`], so every bind is
/// one its dialect's driver can take:
///
/// ```compile_fail
/// let _ = rustango::sql::CompiledStatement { sql: String::new(), params: Default::default() };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledStatement {
    pub sql: String,
    pub(in crate::sql) params: BindParams,
}

impl CompiledStatement {
    /// A hand-written statement for `d`.
    ///
    /// # Errors
    /// [`SqlError::ValueNotSupportedInDialect`] for a bind `d` cannot take.
    pub fn new(
        d: &dyn Dialect,
        sql: impl Into<String>,
        params: Vec<SqlValue>,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            sql: sql.into(),
            params: BindParams::check(d, params)?,
        })
    }

    /// A statement whose binds are all text, which every backend takes.
    pub(crate) fn with_text(
        sql: impl Into<String>,
        params: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            sql: sql.into(),
            params: BindParams(params.into_iter().map(SqlValue::String).collect()),
        }
    }

    /// The binds, in placeholder order.
    #[must_use]
    pub fn params(&self) -> &[SqlValue] {
        &self.params.0
    }

    /// The text and the checked binds, for the executor.
    pub(crate) fn into_parts(self) -> (String, BindParams) {
        (self.sql, self.params)
    }
}

/// Bind values checked against the dialect that will bind them: no raw
/// `List`, and no Postgres-only value off Postgres. Only
/// [`BindParams::check`] (and the text-only constructor) builds one.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BindParams(Vec<SqlValue>);

impl BindParams {
    /// The one check for compiled and raw statements alike.
    ///
    /// # Errors
    /// [`SqlError::ValueNotSupportedInDialect`] naming the first bind `d`
    /// cannot take.
    pub(crate) fn check(d: &dyn Dialect, values: Vec<SqlValue>) -> Result<Self, SqlError> {
        let pg = d.binds_pg_only_values();
        match values.iter().find_map(|v| unbindable(v, pg)) {
            Some(value) => Err(SqlError::ValueNotSupportedInDialect {
                value,
                dialect: d.name(),
            }),
            None => Ok(Self(values)),
        }
    }

    /// The binds, in placeholder order.
    #[must_use]
    pub fn as_slice(&self) -> &[SqlValue] {
        &self.0
    }

    pub(crate) fn into_values(self) -> Vec<SqlValue> {
        self.0
    }
}

/// What a bind arm cannot take, by name: a `List` (the writer expands
/// those), an array of a type Postgres does not bind, or, off Postgres
/// (`pg` false), any Postgres-only value, named even inside a list.
pub(crate) fn unbindable(v: &SqlValue, pg: bool) -> Option<&'static str> {
    if !pg {
        if let Some(kind) = v.pg_only_kind() {
            return Some(kind);
        }
    }
    match v {
        SqlValue::List(_) => Some("list"),
        SqlValue::Array(elems) => {
            // One element type per typed array; NULLs bind as NULL elements.
            let mut typed = elems.iter().filter(|e| !matches!(e, SqlValue::Null));
            let Some(first) = typed.next() else {
                return None;
            };
            if !typed.all(|e| std::mem::discriminant(e) == std::mem::discriminant(first)) {
                return Some("mixed array");
            }
            match first {
                SqlValue::I16(_)
                | SqlValue::I32(_)
                | SqlValue::I64(_)
                | SqlValue::String(_)
                | SqlValue::Bool(_)
                | SqlValue::Uuid(_) => None,
                _ => Some("array element"),
            }
        }
        _ => None,
    }
}
