//! Bulk-action runner — apply one operation to a set of selected PKs.
//!
//! Pairs with admin list views or any UI where the user picks rows and
//! triggers an action. Built-in actions handle the common cases
//! (delete, soft-delete, restore, set-field-to-value); custom actions
//! plug in via the [`BulkAction`] trait.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::bulk_actions::{BulkActionRegistry, BulkDeleteAction};
//! use std::sync::Arc;
//!
//! let registry = BulkActionRegistry::new()
//!     .register(Arc::new(BulkDeleteAction));
//!
//! // From a handler:
//! let result = registry
//!     .run("delete", "posts", &[1, 2, 3], &pool)
//!     .await?;
//! println!("affected {} rows", result.affected);
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::sql::sqlx::{self, PgPool};

#[derive(Debug, thiserror::Error)]
pub enum BulkActionError {
    #[error("unknown action: {0}")]
    UnknownAction(String),
    #[error("invalid table or column identifier: {0}")]
    InvalidIdent(String),
    #[error("database error: {0}")]
    Database(String),
}

/// Outcome of a bulk action invocation.
#[derive(Debug, Clone)]
pub struct BulkActionResult {
    /// Number of database rows affected.
    pub affected: u64,
    /// Action name that ran.
    pub action: String,
    /// Table the action targeted.
    pub table: String,
}

/// Pluggable bulk action.
#[async_trait]
pub trait BulkAction: Send + Sync + 'static {
    /// Action codename used in URLs / select dropdowns (e.g. `"delete_selected"`).
    fn name(&self) -> &str;

    /// Human-readable label rendered in the UI dropdown.
    fn label(&self) -> &str;

    /// Apply this action to the rows whose primary keys appear in `pks`.
    async fn run(
        &self,
        table: &str,
        pks: &[i64],
        pool: &PgPool,
    ) -> Result<BulkActionResult, BulkActionError>;
}

/// `Arc<dyn BulkAction>` alias.
pub type BoxedAction = Arc<dyn BulkAction>;

/// Action registry — keyed by `name()`.
pub struct BulkActionRegistry {
    actions: HashMap<String, BoxedAction>,
}

impl Default for BulkActionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BulkActionRegistry {
    /// New empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { actions: HashMap::new() }
    }

    /// Register an action. Builder-style — chain multiple `register` calls.
    #[must_use]
    pub fn register(mut self, action: BoxedAction) -> Self {
        self.actions.insert(action.name().to_owned(), action);
        self
    }

    /// Look up a registered action by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&BoxedAction> {
        self.actions.get(name)
    }

    /// All registered (name, label) pairs — for rendering dropdowns.
    #[must_use]
    pub fn list(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .actions
            .values()
            .map(|a| (a.name().to_owned(), a.label().to_owned()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Run a registered action by name. Returns [`BulkActionError::UnknownAction`]
    /// if `name` is not registered.
    pub async fn run(
        &self,
        name: &str,
        table: &str,
        pks: &[i64],
        pool: &PgPool,
    ) -> Result<BulkActionResult, BulkActionError> {
        let action = self
            .get(name)
            .ok_or_else(|| BulkActionError::UnknownAction(name.to_owned()))?;
        action.run(table, pks, pool).await
    }
}

/// Reject identifiers (table / column names) with characters that could
/// break out of the quoted form.
pub(crate) fn validate_ident(name: &str) -> Result<(), BulkActionError> {
    if name.is_empty() {
        return Err(BulkActionError::InvalidIdent("empty".into()));
    }
    let bad = ['"', '\0', '\n', '\r', '\\', ';', ' '];
    if name.chars().any(|c| bad.contains(&c) || c.is_control()) {
        return Err(BulkActionError::InvalidIdent(name.to_owned()));
    }
    Ok(())
}

// ------------------------------------------------------------------ Built-in actions

/// Hard-delete every selected row.
pub struct BulkDeleteAction;

#[async_trait]
impl BulkAction for BulkDeleteAction {
    fn name(&self) -> &str { "delete_selected" }
    fn label(&self) -> &str { "Delete selected" }

    async fn run(
        &self,
        table: &str,
        pks: &[i64],
        pool: &PgPool,
    ) -> Result<BulkActionResult, BulkActionError> {
        validate_ident(table)?;
        if pks.is_empty() {
            return Ok(BulkActionResult {
                affected: 0,
                action: self.name().to_owned(),
                table: table.to_owned(),
            });
        }
        let sql = format!(r#"DELETE FROM "{table}" WHERE "id" = ANY($1)"#);
        let result = sqlx::query(&sql)
            .bind(pks)
            .execute(pool)
            .await
            .map_err(|e| BulkActionError::Database(e.to_string()))?;
        Ok(BulkActionResult {
            affected: result.rows_affected(),
            action: self.name().to_owned(),
            table: table.to_owned(),
        })
    }
}

/// Soft-delete: set the named column (typically `deleted_at`) to `NOW()`
/// for every selected row. Only updates rows where the column is currently NULL.
pub struct BulkSoftDeleteAction {
    /// SQL column name to set (e.g. `"deleted_at"`).
    pub column: &'static str,
}

#[async_trait]
impl BulkAction for BulkSoftDeleteAction {
    fn name(&self) -> &str { "soft_delete_selected" }
    fn label(&self) -> &str { "Soft-delete selected" }

    async fn run(
        &self,
        table: &str,
        pks: &[i64],
        pool: &PgPool,
    ) -> Result<BulkActionResult, BulkActionError> {
        validate_ident(table)?;
        validate_ident(self.column)?;
        if pks.is_empty() {
            return Ok(BulkActionResult {
                affected: 0,
                action: self.name().to_owned(),
                table: table.to_owned(),
            });
        }
        let col = self.column;
        let sql = format!(
            r#"UPDATE "{table}" SET "{col}" = NOW() WHERE "id" = ANY($1) AND "{col}" IS NULL"#,
        );
        let result = sqlx::query(&sql)
            .bind(pks)
            .execute(pool)
            .await
            .map_err(|e| BulkActionError::Database(e.to_string()))?;
        Ok(BulkActionResult {
            affected: result.rows_affected(),
            action: self.name().to_owned(),
            table: table.to_owned(),
        })
    }
}

/// Restore: set a soft-delete column back to NULL for every selected row.
pub struct BulkRestoreAction {
    pub column: &'static str,
}

#[async_trait]
impl BulkAction for BulkRestoreAction {
    fn name(&self) -> &str { "restore_selected" }
    fn label(&self) -> &str { "Restore selected" }

    async fn run(
        &self,
        table: &str,
        pks: &[i64],
        pool: &PgPool,
    ) -> Result<BulkActionResult, BulkActionError> {
        validate_ident(table)?;
        validate_ident(self.column)?;
        if pks.is_empty() {
            return Ok(BulkActionResult {
                affected: 0,
                action: self.name().to_owned(),
                table: table.to_owned(),
            });
        }
        let col = self.column;
        let sql = format!(
            r#"UPDATE "{table}" SET "{col}" = NULL WHERE "id" = ANY($1)"#,
        );
        let result = sqlx::query(&sql)
            .bind(pks)
            .execute(pool)
            .await
            .map_err(|e| BulkActionError::Database(e.to_string()))?;
        Ok(BulkActionResult {
            affected: result.rows_affected(),
            action: self.name().to_owned(),
            table: table.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy {
        name: &'static str,
        label: &'static str,
    }

    #[async_trait]
    impl BulkAction for Dummy {
        fn name(&self) -> &str { self.name }
        fn label(&self) -> &str { self.label }
        async fn run(
            &self,
            table: &str,
            _pks: &[i64],
            _pool: &PgPool,
        ) -> Result<BulkActionResult, BulkActionError> {
            Ok(BulkActionResult {
                affected: 0,
                action: self.name.to_owned(),
                table: table.to_owned(),
            })
        }
    }

    #[test]
    fn registry_starts_empty() {
        let r = BulkActionRegistry::new();
        assert!(r.list().is_empty());
    }

    #[test]
    fn register_adds_action() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy { name: "a", label: "A" }))
            .register(Arc::new(Dummy { name: "b", label: "B" }));
        let list = r.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], ("a".to_owned(), "A".to_owned()));
        assert_eq!(list[1], ("b".to_owned(), "B".to_owned()));
    }

    #[test]
    fn get_returns_registered_action() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy { name: "x", label: "X" }));
        assert!(r.get("x").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn list_is_alphabetically_sorted() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy { name: "zebra", label: "Z" }))
            .register(Arc::new(Dummy { name: "apple", label: "A" }))
            .register(Arc::new(Dummy { name: "mango", label: "M" }));
        let list = r.list();
        assert_eq!(list[0].0, "apple");
        assert_eq!(list[1].0, "mango");
        assert_eq!(list[2].0, "zebra");
    }

    #[test]
    fn re_registering_same_name_replaces() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy { name: "k", label: "old" }))
            .register(Arc::new(Dummy { name: "k", label: "new" }));
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].1, "new");
    }

    #[test]
    fn validate_ident_accepts_normal() {
        assert!(validate_ident("posts").is_ok());
        assert!(validate_ident("rustango_users").is_ok());
        assert!(validate_ident("deleted_at").is_ok());
    }

    #[test]
    fn validate_ident_rejects_dangerous_chars() {
        assert!(validate_ident("evil\"").is_err());
        assert!(validate_ident("a;b").is_err());
        assert!(validate_ident("a b").is_err());
        assert!(validate_ident("a\nb").is_err());
        assert!(validate_ident("").is_err());
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        // Use a lazy pool — the action lookup fails before the SQL fires
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/dummy").unwrap();
        let r = BulkActionRegistry::new();
        let err = r.run("nonexistent", "posts", &[1], &pool).await.unwrap_err();
        assert!(matches!(err, BulkActionError::UnknownAction(_)));
    }

    #[test]
    fn builtin_action_names() {
        assert_eq!(BulkDeleteAction.name(), "delete_selected");
        assert_eq!(BulkSoftDeleteAction { column: "deleted_at" }.name(), "soft_delete_selected");
        assert_eq!(BulkRestoreAction { column: "deleted_at" }.name(), "restore_selected");
    }
}
