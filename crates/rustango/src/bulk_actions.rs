//! Pluggable bulk actions for the auto-admin — the "Action" dropdown
//! above a list view.
//!
//! A [`BulkAction`](crate::bulk_actions::BulkAction) has a codename, a
//! label, and a `run` that acts on a [`PkSet`] of selected primary keys.
//! The admin's "Action" dropdown is built from a
//! [`BulkActionRegistry`](crate::bulk_actions::BulkActionRegistry) mounted
//! on the [`crate::server::Builder`].
//!
//! The built-in actions write through the ORM, so they work on PG, MySQL
//! and SQLite, and audited models get one audit row per changed row.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::{
    Assignment, FieldSchema, FieldType, Filter, ModelSchema, Op, SqlValue, WhereExpr,
};
use crate::sql::Pool;

#[derive(Debug, thiserror::Error)]
pub enum BulkActionError {
    #[error("unknown action: {0}")]
    UnknownAction(String),
    #[error("invalid table or column identifier: {0}")]
    InvalidIdent(String),
    /// A key that does not fit the model's primary key type.
    #[error("invalid primary key: {0}")]
    InvalidPk(String),
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

/// Selected primary keys, each typed from the model's PK field, so a key
/// can never be bound against a PK column of another type (#1817).
#[derive(Debug, Clone)]
pub struct PkSet {
    model: &'static ModelSchema,
    keys: Vec<SqlValue>,
}

impl PkSet {
    /// Parse raw keys (form or URL values) with the PK field's type.
    ///
    /// # Errors
    /// [`BulkActionError::InvalidPk`] when the model has no PK or a key
    /// does not parse as its type.
    pub fn parse<S: AsRef<str>>(
        model: &'static ModelSchema,
        raw: impl IntoIterator<Item = S>,
    ) -> Result<Self, BulkActionError> {
        let pk = pk_field(model)?;
        let keys = raw
            .into_iter()
            .map(|r| {
                crate::forms::parse_pk_string(pk, r.as_ref())
                    .map_err(|e| BulkActionError::InvalidPk(e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { model, keys })
    }

    /// Typed keys. Integers are narrowed to the PK's width; any other
    /// type mismatch is refused.
    ///
    /// # Errors
    /// [`BulkActionError::InvalidPk`] when the model has no PK or a key
    /// does not fit its type.
    pub fn new<V: Into<SqlValue>>(
        model: &'static ModelSchema,
        keys: impl IntoIterator<Item = V>,
    ) -> Result<Self, BulkActionError> {
        let pk = pk_field(model)?;
        let keys = keys
            .into_iter()
            .map(|k| coerce_key(pk, k.into()))
            .collect::<Result<_, _>>()?;
        Ok(Self { model, keys })
    }

    /// The model the keys belong to.
    #[must_use]
    pub fn model(&self) -> &'static ModelSchema {
        self.model
    }

    /// The typed keys.
    #[must_use]
    pub fn keys(&self) -> &[SqlValue] {
        &self.keys
    }

    /// `true` when no key is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `<pk column> IN (keys)`.
    fn filter(&self) -> WhereExpr {
        // `pk_field` passed at construction, so the PK is there.
        let column = self.model.primary_key().map_or("id", |pk| pk.column);
        WhereExpr::Predicate(Filter::new(
            column,
            Op::In,
            SqlValue::List(self.keys.clone()),
        ))
    }

    /// The model to write through: the audited one on this table, if any (#1794).
    fn target(&self) -> &'static ModelSchema {
        first_audited(inventory::iter::<crate::core::ModelEntry>, self.model.table)
            .unwrap_or(self.model)
    }
}

fn pk_field(model: &'static ModelSchema) -> Result<&'static FieldSchema, BulkActionError> {
    model
        .primary_key()
        .ok_or_else(|| BulkActionError::InvalidPk(format!("`{}` has no primary key", model.table)))
}

fn coerce_key(pk: &FieldSchema, v: SqlValue) -> Result<SqlValue, BulkActionError> {
    let int = match v {
        SqlValue::I16(n) => Some(i64::from(n)),
        SqlValue::I32(n) => Some(i64::from(n)),
        SqlValue::I64(n) => Some(n),
        _ => None,
    };
    let typed = match pk.ty {
        FieldType::I64 => int.map(SqlValue::I64),
        FieldType::I32 => int.and_then(|n| i32::try_from(n).ok()).map(SqlValue::I32),
        FieldType::I16 => int.and_then(|n| i16::try_from(n).ok()).map(SqlValue::I16),
        FieldType::String if matches!(v, SqlValue::String(_)) => Some(v.clone()),
        FieldType::Uuid if matches!(v, SqlValue::Uuid(_)) => Some(v.clone()),
        _ => None,
    };
    typed.ok_or_else(|| {
        BulkActionError::InvalidPk(format!("`{}` is {}, got {v:?}", pk.name, pk.ty.as_str()))
    })
}

/// Pluggable bulk action.
#[async_trait]
pub trait BulkAction: Send + Sync + 'static {
    /// Action codename used in URLs / select dropdowns (e.g. `"delete_selected"`).
    fn name(&self) -> &str;

    /// Human-readable label rendered in the UI dropdown.
    fn label(&self) -> &str;

    /// Apply this action to the rows whose primary keys are in `pks`.
    async fn run(&self, pks: &PkSet, pool: &Pool) -> Result<BulkActionResult, BulkActionError>;
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
        Self {
            actions: HashMap::new(),
        }
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
        pks: &PkSet,
        pool: &Pool,
    ) -> Result<BulkActionResult, BulkActionError> {
        let action = self
            .get(name)
            .ok_or_else(|| BulkActionError::UnknownAction(name.to_owned()))?;
        action.run(pks, pool).await
    }
}

/// Both conditions in one `find`, so an unaudited proxy on the table cannot win.
fn first_audited<'a>(
    entries: impl IntoIterator<Item = &'a crate::core::ModelEntry>,
    table: &str,
) -> Option<&'static ModelSchema> {
    entries
        .into_iter()
        .find(|e| e.schema.table == table && e.audited_delete().is_some())
        .map(|e| e.schema)
}

/// `SET column = value WHERE pk IN (…) AND column IS [NOT] NULL`, audited as `op`.
async fn set_column(
    pool: &Pool,
    pks: &PkSet,
    column: &'static str,
    value: SqlValue,
    op: crate::audit::AuditOp,
    only_null: bool,
) -> Result<u64, BulkActionError> {
    let model = pks.target();
    let field = model
        .field_by_column(column)
        .ok_or_else(|| BulkActionError::InvalidIdent(format!("{}.{column}", model.table)))?;
    if pks.is_empty() {
        return Ok(0);
    }
    let mut filter = pks.filter();
    filter.push_and(WhereExpr::Predicate(Filter::new(
        field.column,
        Op::IsNull,
        only_null,
    )));
    let set = vec![Assignment::new(field.column, value)];
    let q = crate::core::UpdateQuery::new(model, set, filter);
    crate::audit::update_as(pool, &q, op).await.map_err(db_err)
}

fn done(action: &dyn BulkAction, pks: &PkSet, affected: u64) -> BulkActionResult {
    BulkActionResult {
        affected,
        action: action.name().to_owned(),
        table: pks.model().table.to_owned(),
    }
}

fn db_err(e: crate::sql::ExecError) -> BulkActionError {
    BulkActionError::Database(e.to_string())
}

// ------------------------------------------------------------------ Built-in actions

/// Hard-delete every selected row.
pub struct BulkDeleteAction;

#[async_trait]
impl BulkAction for BulkDeleteAction {
    fn name(&self) -> &str {
        "delete_selected"
    }
    fn label(&self) -> &str {
        "Delete selected"
    }

    async fn run(&self, pks: &PkSet, pool: &Pool) -> Result<BulkActionResult, BulkActionError> {
        if pks.is_empty() {
            return Ok(done(self, pks, 0));
        }
        let q = crate::core::DeleteQuery::new(pks.target(), pks.filter());
        let affected = crate::audit::delete(pool, &q).await.map_err(db_err)?;
        Ok(done(self, pks, affected))
    }
}

/// Soft-delete: set the named column (typically `deleted_at`) to the
/// current UTC time for every selected row. Only updates rows where
/// the column is currently NULL.
pub struct BulkSoftDeleteAction {
    /// SQL column name to set (e.g. `"deleted_at"`).
    pub column: &'static str,
}

#[async_trait]
impl BulkAction for BulkSoftDeleteAction {
    fn name(&self) -> &str {
        "soft_delete_selected"
    }
    fn label(&self) -> &str {
        "Soft-delete selected"
    }

    async fn run(&self, pks: &PkSet, pool: &Pool) -> Result<BulkActionResult, BulkActionError> {
        let now = SqlValue::DateTime(chrono::Utc::now());
        let op = crate::audit::AuditOp::SoftDelete;
        let affected = set_column(pool, pks, self.column, now, op, true).await?;
        Ok(done(self, pks, affected))
    }
}

/// Restore: set a soft-delete column back to NULL for every selected
/// deleted row.
pub struct BulkRestoreAction {
    pub column: &'static str,
}

#[async_trait]
impl BulkAction for BulkRestoreAction {
    fn name(&self) -> &str {
        "restore_selected"
    }
    fn label(&self) -> &str {
        "Restore selected"
    }

    async fn run(&self, pks: &PkSet, pool: &Pool) -> Result<BulkActionResult, BulkActionError> {
        // Only deleted rows, so an active one writes no audit row.
        let op = crate::audit::AuditOp::Restore;
        let affected = set_column(pool, pks, self.column, SqlValue::Null, op, false).await?;
        Ok(done(self, pks, affected))
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
        fn name(&self) -> &str {
            self.name
        }
        fn label(&self) -> &str {
            self.label
        }
        async fn run(
            &self,
            pks: &PkSet,
            _pool: &Pool,
        ) -> Result<BulkActionResult, BulkActionError> {
            Ok(done(self, pks, 0))
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
            .register(Arc::new(Dummy {
                name: "a",
                label: "A",
            }))
            .register(Arc::new(Dummy {
                name: "b",
                label: "B",
            }));
        let list = r.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], ("a".to_owned(), "A".to_owned()));
        assert_eq!(list[1], ("b".to_owned(), "B".to_owned()));
    }

    #[test]
    fn get_returns_registered_action() {
        let r = BulkActionRegistry::new().register(Arc::new(Dummy {
            name: "x",
            label: "X",
        }));
        assert!(r.get("x").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn list_is_alphabetically_sorted() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy {
                name: "zebra",
                label: "Z",
            }))
            .register(Arc::new(Dummy {
                name: "apple",
                label: "A",
            }))
            .register(Arc::new(Dummy {
                name: "mango",
                label: "M",
            }));
        let list = r.list();
        assert_eq!(list[0].0, "apple");
        assert_eq!(list[1].0, "mango");
        assert_eq!(list[2].0, "zebra");
    }

    #[test]
    fn re_registering_same_name_replaces() {
        let r = BulkActionRegistry::new()
            .register(Arc::new(Dummy {
                name: "k",
                label: "old",
            }))
            .register(Arc::new(Dummy {
                name: "k",
                label: "new",
            }));
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].1, "new");
    }

    const fn pk(ty: FieldType) -> FieldSchema {
        let mut f = FieldSchema::new("code", "code", ty);
        f.primary_key = true;
        f
    }
    static TEXT_FIELDS: [FieldSchema; 1] = [pk(FieldType::String)];
    static TEXT_PK: ModelSchema = {
        let mut s = ModelSchema::new("Text", "text_pk");
        s.fields = &TEXT_FIELDS;
        s
    };
    static I32_FIELDS: [FieldSchema; 1] = [pk(FieldType::I32)];
    static I32_PK: ModelSchema = {
        let mut s = ModelSchema::new("Small", "small_pk");
        s.fields = &I32_FIELDS;
        s
    };

    /// An integer key cannot reach a text PK column (#1817).
    #[test]
    fn pk_set_refuses_keys_of_another_type() {
        assert!(matches!(
            PkSet::new(&TEXT_PK, [1_i64]),
            Err(BulkActionError::InvalidPk(_))
        ));
        let keys = PkSet::new(&TEXT_PK, ["a".to_owned()]).unwrap();
        assert_eq!(keys.keys(), [SqlValue::String("a".into())]);
        assert!(PkSet::parse(&I32_PK, ["x"]).is_err());
        assert!(PkSet::new(&I32_PK, [i64::MAX]).is_err());
        let keys = PkSet::new(&I32_PK, [7_i64]).unwrap();
        assert_eq!(keys.keys(), [SqlValue::I32(7)]);
    }

    #[test]
    fn pk_set_filters_on_the_schema_pk_column() {
        let keys = PkSet::parse(&TEXT_PK, ["a"]).unwrap();
        match keys.filter() {
            WhereExpr::Predicate(f) => assert_eq!(f.column, "code"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pk_set_needs_a_primary_key() {
        static NO_PK: ModelSchema = ModelSchema::new("NoPk", "no_pk");
        assert!(PkSet::parse(&NO_PK, ["1"]).is_err());
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn unknown_action_returns_error() {
        // Lazy pool: the lookup fails before any SQL runs.
        let sq = crate::sql::sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let pool: Pool = sq.into();
        let r = BulkActionRegistry::new();
        let keys = PkSet::parse(&TEXT_PK, ["a"]).unwrap();
        let err = r.run("nonexistent", &keys, &pool).await.unwrap_err();
        assert!(matches!(err, BulkActionError::UnknownAction(_)));
    }

    fn fake_delete<'a>(
        _: &'a Pool,
        _: &'a crate::core::DeleteQuery,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<u64, crate::sql::ExecError>> + Send + 'a>,
    > {
        Box::pin(async { Ok(0) })
    }

    /// An unaudited proxy listed first on the same table must not win.
    #[test]
    fn first_audited_skips_an_unaudited_model_on_the_table() {
        static PROXY: ModelSchema = ModelSchema::new("Proxy", "shared");
        static REAL: ModelSchema = ModelSchema::new("Real", "shared");
        let entries = [
            crate::core::ModelEntry::new(&PROXY, "t"),
            crate::core::ModelEntry::new(&REAL, "t").with_audited(|| None, || Some(fake_delete)),
        ];
        let got = first_audited(&entries, "shared").map(|m| m.name);
        assert_eq!(got, Some("Real"));
        assert!(first_audited(&entries[..1], "shared").is_none());
    }

    #[test]
    fn builtin_action_names() {
        assert_eq!(BulkDeleteAction.name(), "delete_selected");
        assert_eq!(
            BulkSoftDeleteAction {
                column: "deleted_at"
            }
            .name(),
            "soft_delete_selected"
        );
        assert_eq!(
            BulkRestoreAction {
                column: "deleted_at"
            }
            .name(),
            "restore_selected"
        );
    }
}
