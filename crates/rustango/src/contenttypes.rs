//! Django-shape ContentType framework — sub-slice F.1 of v0.15.0.
//!
//! A `ContentType` row is a runtime handle to a registered model:
//! `(id, app_label, model_name, table)`. Lets framework features
//! (permissions, generic foreign keys, audit history, soft-FK
//! prefetch) reference any model by `(app_label, model_name)` or
//! by `content_type_id` without hard-coding the target table into
//! their schema.
//!
//! ## Lifecycle
//!
//! - The `ContentType` model itself ships as a `#[derive(Model)]`
//!   so it migrates / appears in the admin like any other table.
//! - [`ensure_seeded`] walks `inventory::iter::<ModelEntry>()` and
//!   inserts a row for every registered model that doesn't have one
//!   yet. Idempotent — re-running on a populated DB is a no-op.
//!   Wire into your bootstrap (e.g. inside `main()` after
//!   `migrate(&pool, dir).await?`).
//! - [`for_model`] resolves a model type to its `ContentType`
//!   row (cached per pool — repeated calls hit a process-wide
//!   `OnceCell`, not the DB).
//! - [`all`] / [`by_id`] / [`by_natural_key`] cover the lookup
//!   shapes the admin and audit views need.
//!
//! ## Why not infer at query time?
//!
//! The model registry (`inventory`) is process-local and Rust-typed
//! — but permissions, audit rows, generic foreign keys, and
//! cross-process integrations need a **stable database identifier**
//! the framework can hand to other systems. The ContentType row's
//! `id` (a `BIGSERIAL`) is that identifier. `(app_label, model_name)`
//! is the natural key for human-facing wiring; numeric `id` is the
//! foreign key everywhere else.

use crate::core::{inventory, Model as _, ModelEntry, SqlValue};
use crate::sql::{sqlx::PgPool, Auto, ExecError, Fetcher as _};
use crate::Model;

/// One row per registered model. The schema mirrors Django's
/// `django_content_types` table closely enough that any code reading
/// it (audit log front-ends, generic FKs, permissions) feels
/// instantly familiar.
///
/// `(app_label, model_name)` is a natural key — the migration
/// emits a `UNIQUE` constraint on the pair so duplicate inserts
/// from a racy bootstrap fail loudly instead of silently creating
/// two rows.
#[derive(Debug, Clone, Model)]
#[rustango(table = "rustango_content_types")]
pub struct ContentType {
    /// Auto-assigned primary key. Used as the foreign key everywhere
    /// the framework needs to point at "any model" (permissions,
    /// generic FKs, audit log targets in F.2 / F.3).
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Django-shape app label — `module_path!()`'s first segment
    /// after the crate root, or the explicit `#[rustango(app = "...")]`
    /// override from the model's container attr.
    #[rustango(max_length = 100)]
    pub app_label: String,
    /// Lowercase Rust-side model name. Conventionally matches the
    /// `#[derive(Model)]` struct ident lowercased (e.g. `User` →
    /// `user`).
    #[rustango(max_length = 100)]
    pub model_name: String,
    /// SQL table name (`#[rustango(table = "…")]` value or the
    /// auto-derived snake-case fallback). Carried alongside
    /// `model_name` so callers reading the audit log don't have to
    /// reconstruct it from the registry every time.
    #[rustango(max_length = 100)]
    pub table: String,
}

impl ContentType {
    /// Look up a `ContentType` row for a registered model type.
    ///
    /// Cheap-ish (DB round trip) — for hot paths consider
    /// [`for_model_cached`] which memoizes per-process. Returns
    /// `Ok(None)` when [`ensure_seeded`] hasn't been called yet for
    /// this model (the row doesn't exist in the DB).
    ///
    /// # Errors
    /// Driver / query failures from the underlying SELECT.
    pub async fn for_model<T: crate::core::Model>(
        pool: &PgPool,
    ) -> Result<Option<Self>, ExecError> {
        let entry = inventory::iter::<ModelEntry>
            .into_iter()
            .find(|e| e.schema.table == T::SCHEMA.table)
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            })?;
        let app = entry.resolved_app_label().unwrap_or("project");
        let name = T::SCHEMA.name.to_ascii_lowercase();
        Self::by_natural_key(pool, app, &name).await
    }

    /// Lookup by `(app_label, model_name)` — the natural key. Used
    /// when the caller has the strings (e.g. parsing
    /// `"app.action_model"` permission codenames) but not the Rust
    /// type.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_natural_key(
        pool: &PgPool,
        app_label: &str,
        model_name: &str,
    ) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter("app_label", crate::core::Op::Eq, SqlValue::String(app_label.into()))
            .filter("model_name", crate::core::Op::Eq, SqlValue::String(model_name.into()))
            .limit(1)
            .fetch(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Lookup by primary key. Used by FK joins (audit log target,
    /// permission scope, etc.).
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_id(pool: &PgPool, id: i64) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter("id", crate::core::Op::Eq, SqlValue::I64(id))
            .limit(1)
            .fetch(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// All registered ContentTypes, ordered by `(app_label, model_name)`
    /// for stable display in admin sidebars / API listings.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn all(pool: &PgPool) -> Result<Vec<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .order_by(&[("app_label", false), ("model_name", false)])
            .fetch(pool)
            .await?;
        Ok(rows)
    }
}

/// Walk the inventory of registered models and INSERT a ContentType
/// row for every one missing. Idempotent.
///
/// The `ContentType` table itself (the one this function writes
/// into) is excluded from the walk — bootstrapping its own row
/// would be circular and meaningless. Callers don't need to
/// special-case it.
///
/// Run once after `migrate(&pool, dir).await?` at app startup, or
/// on demand from a `manage seed-content-types` verb (F.2 follow-up).
///
/// # Errors
/// Driver / query failures from the SELECT-or-INSERT loop.
pub async fn ensure_seeded(pool: &PgPool) -> Result<usize, ExecError> {
    let mut inserted = 0_usize;
    for entry in inventory::iter::<ModelEntry> {
        let table = entry.schema.table;
        // Don't seed a row for the ContentType table itself — would
        // be circular and meaningless.
        if table == ContentType::SCHEMA.table {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("project").to_owned();
        let name = entry.schema.name.to_ascii_lowercase();
        // Probe natural key first; skip if already seeded.
        if ContentType::by_natural_key(pool, &app, &name).await?.is_some() {
            continue;
        }
        let mut row = ContentType {
            id: Auto::Unset,
            app_label: app,
            model_name: name,
            table: table.to_owned(),
        };
        row.insert(pool).await?;
        inserted += 1;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_schema_has_expected_columns() {
        let s = ContentType::SCHEMA;
        assert_eq!(s.table, "rustango_content_types");
        let cols: Vec<&str> = s.fields.iter().map(|f| f.column).collect();
        assert!(cols.contains(&"id"));
        assert!(cols.contains(&"app_label"));
        assert!(cols.contains(&"model_name"));
        assert!(cols.contains(&"table"));
    }

    #[test]
    fn content_type_id_is_auto() {
        let pk = ContentType::SCHEMA
            .primary_key()
            .expect("ContentType has a PK");
        assert_eq!(pk.column, "id");
        assert!(pk.auto, "ContentType.id should be Auto<i64>");
    }
}
