//! Soft-delete query helpers.
//!
//! `#[rustango(soft_delete)]` marks one column as the "deleted at"
//! timestamp. The admin delete handler sets that column instead of
//! running a real `DELETE`. This module covers the rest: hide deleted
//! rows from reads, restore them, or remove them for good.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::soft_delete;
//!
//! // Read path — wrap your existing where-clause to hide trashed rows:
//! let where_active = soft_delete::compose_with_active(Post::SCHEMA, my_where);
//!
//! // Write path — set deleted_at = NOW(), keep the row:
//! soft_delete::soft_delete(&pool, Post::SCHEMA, "id", SqlValue::I64(42)).await?;
//!
//! // Restore — set deleted_at = NULL:
//! soft_delete::restore(&pool, Post::SCHEMA, "id", SqlValue::I64(42)).await?;
//!
//! // Purge — bypass soft-delete and actually DELETE:
//! soft_delete::purge(&pool, Post::SCHEMA, "id", SqlValue::I64(42)).await?;
//! ```
//!
//! On a model with no soft-delete column, [`active_filter`] returns
//! `None` and [`compose_with_active`] returns its input unchanged.
//! [`soft_delete`] returns an error there rather than doing nothing
//! quietly, so rows that should be hidden cannot slip through.
//!
//! [`active_filter`]: crate::soft_delete::active_filter
//! [`compose_with_active`]: crate::soft_delete::compose_with_active

use crate::core::{
    Assignment, DeleteQuery, Filter, ModelSchema, Op, SqlValue, UpdateQuery, WhereExpr,
};
use crate::sql::{delete_pool as sql_delete_pool, update_pool as sql_update_pool, ExecError, Pool};

/// `Some(<col> IS NULL)` for a soft-delete model, else `None`. It
/// matches the rows that are still live.
#[must_use]
pub fn active_filter(model: &'static ModelSchema) -> Option<WhereExpr> {
    let col = model.soft_delete_column?;
    Some(WhereExpr::Predicate(Filter {
        column: col,
        op: Op::IsNull,
        value: SqlValue::Bool(true),
    }))
}

/// `Some(<col> IS NOT NULL)` for a soft-delete model, else `None`. It
/// matches the deleted rows, as a Trash page needs.
#[must_use]
pub fn trashed_filter(model: &'static ModelSchema) -> Option<WhereExpr> {
    let col = model.soft_delete_column?;
    Some(WhereExpr::Predicate(Filter {
        column: col,
        op: Op::IsNull,
        value: SqlValue::Bool(false),
    }))
}

/// Add "not deleted" to `existing`. A model with no soft-delete column
/// gets `existing` back unchanged, and an empty `existing` gives the
/// active filter on its own.
#[must_use]
pub fn compose_with_active(model: &'static ModelSchema, existing: WhereExpr) -> WhereExpr {
    let Some(active) = active_filter(model) else {
        return existing;
    };
    if existing.is_empty() {
        return active;
    }
    WhereExpr::And(vec![existing, active])
}

/// Like [`compose_with_active`], but matches the deleted rows.
#[must_use]
pub fn compose_with_trashed(model: &'static ModelSchema, existing: WhereExpr) -> WhereExpr {
    let Some(trashed) = trashed_filter(model) else {
        return existing;
    };
    if existing.is_empty() {
        return trashed;
    }
    WhereExpr::And(vec![existing, trashed])
}

#[derive(Debug, thiserror::Error)]
pub enum SoftDeleteError {
    #[error("model `{0}` is not soft-delete-enabled (missing #[rustango(soft_delete)])")]
    NotSoftDeleteEnabled(&'static str),
    #[error(transparent)]
    Exec(#[from] ExecError),
}

/// Mark one row deleted by setting the soft-delete column to now.
/// Returns how many rows changed.
///
/// # Errors
/// [`SoftDeleteError::NotSoftDeleteEnabled`] when the model has no
/// `#[rustango(soft_delete)]` field, or [`SoftDeleteError::Exec`] on a
/// database error.
pub async fn soft_delete(
    pool: &Pool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, SoftDeleteError> {
    let col = model
        .soft_delete_column
        .ok_or(SoftDeleteError::NotSoftDeleteEnabled(model.name))?;
    let n = sql_update_pool(
        pool,
        &UpdateQuery {
            model,
            set: vec![Assignment {
                column: col,
                value: SqlValue::from(chrono::Utc::now()).into(),
            }],
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::Eq,
                value: pk_value,
            }),
        },
    )
    .await?;
    Ok(n)
}

/// Undo a soft delete by setting the column back to `NULL`. Returns
/// how many rows changed.
///
/// # Errors
/// [`SoftDeleteError::NotSoftDeleteEnabled`] when the model has no
/// `#[rustango(soft_delete)]` field, or [`SoftDeleteError::Exec`] on a
/// database error.
pub async fn restore(
    pool: &Pool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, SoftDeleteError> {
    let col = model
        .soft_delete_column
        .ok_or(SoftDeleteError::NotSoftDeleteEnabled(model.name))?;
    let n = sql_update_pool(
        pool,
        &UpdateQuery {
            model,
            set: vec![Assignment {
                column: col,
                value: SqlValue::Null.into(),
            }],
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::Eq,
                value: pk_value,
            }),
        },
    )
    .await?;
    Ok(n)
}

/// Really delete the row. This cannot be undone. Returns how many
/// rows changed. Works on any model.
///
/// # Errors
/// A database error.
pub async fn purge(
    pool: &Pool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, ExecError> {
    sql_delete_pool(
        pool,
        &DeleteQuery {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::Eq,
                value: pk_value,
            }),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FieldSchema, FieldType};

    static FIELDS_WITH_SD: &[FieldSchema] = &[
        FieldSchema {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: true,
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
        },
        FieldSchema {
            name: "title",
            column: "title",
            ty: FieldType::String,
            nullable: false,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
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
        },
        FieldSchema {
            name: "deleted_at",
            column: "deleted_at",
            ty: FieldType::DateTime,
            nullable: true,
            primary_key: false,
            relation: None,
            max_length: None,
            min: None,
            max: None,
            default: None,
            auto: false,
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
        },
    ];

    static MODEL_WITH_SD: ModelSchema = ModelSchema {
        name: "Post",
        table: "posts",
        fields: FIELDS_WITH_SD,
        display: None,
        app_label: None,
        admin: None,
        soft_delete_column: Some("deleted_at"),
        audit_track: None,
        permissions: false,
        indexes: &[],
        check_constraints: &[],
        exclusion_constraints: &[],
        default_permissions: &[],
        m2m: &[],
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
    };

    static MODEL_WITHOUT_SD: ModelSchema = ModelSchema {
        name: "Tag",
        table: "tags",
        fields: FIELDS_WITH_SD, // same fields, but no soft_delete_column
        display: None,
        app_label: None,
        admin: None,
        soft_delete_column: None,
        audit_track: None,
        permissions: false,
        indexes: &[],
        check_constraints: &[],
        exclusion_constraints: &[],
        default_permissions: &[],
        m2m: &[],
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
    };

    #[test]
    fn active_filter_returns_is_null_predicate() {
        let f = active_filter(&MODEL_WITH_SD).unwrap();
        match f {
            WhereExpr::Predicate(Filter { column, op, value }) => {
                assert_eq!(column, "deleted_at");
                assert_eq!(op, Op::IsNull);
                assert_eq!(value, SqlValue::Bool(true));
            }
            other => panic!("expected predicate, got {other:?}"),
        }
    }

    #[test]
    fn trashed_filter_returns_is_not_null_predicate() {
        let f = trashed_filter(&MODEL_WITH_SD).unwrap();
        match f {
            WhereExpr::Predicate(Filter { column, op, value }) => {
                assert_eq!(column, "deleted_at");
                assert_eq!(op, Op::IsNull);
                assert_eq!(value, SqlValue::Bool(false));
            }
            other => panic!("expected predicate, got {other:?}"),
        }
    }

    #[test]
    fn no_filter_when_model_lacks_soft_delete_column() {
        assert!(active_filter(&MODEL_WITHOUT_SD).is_none());
        assert!(trashed_filter(&MODEL_WITHOUT_SD).is_none());
    }

    #[test]
    fn compose_with_active_returns_input_when_no_sd_column() {
        let existing = WhereExpr::Predicate(Filter {
            column: "title",
            op: Op::Eq,
            value: SqlValue::String("hi".into()),
        });
        let composed = compose_with_active(&MODEL_WITHOUT_SD, existing.clone());
        assert_eq!(composed, existing);
    }

    #[test]
    fn compose_with_active_returns_filter_when_existing_is_empty() {
        let composed = compose_with_active(&MODEL_WITH_SD, WhereExpr::And(vec![]));
        assert!(matches!(
            composed,
            WhereExpr::Predicate(Filter { op: Op::IsNull, .. })
        ));
    }

    #[test]
    fn compose_with_active_ands_when_existing_nonempty() {
        let existing = WhereExpr::Predicate(Filter {
            column: "title",
            op: Op::Eq,
            value: SqlValue::String("hi".into()),
        });
        let composed = compose_with_active(&MODEL_WITH_SD, existing);
        match composed {
            WhereExpr::And(items) => {
                assert_eq!(items.len(), 2);
                // Existing predicate first, active filter second.
                assert!(matches!(&items[0], WhereExpr::Predicate(f) if f.column == "title"));
                assert!(matches!(&items[1], WhereExpr::Predicate(f) if f.column == "deleted_at"));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn compose_with_trashed_mirrors_active_for_consistency() {
        let composed = compose_with_trashed(&MODEL_WITH_SD, WhereExpr::And(vec![]));
        match composed {
            WhereExpr::Predicate(Filter { op, value, .. }) => {
                assert_eq!(op, Op::IsNull);
                assert_eq!(value, SqlValue::Bool(false));
            }
            other => panic!("expected trashed predicate, got {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(feature = "postgres")]
    async fn soft_delete_on_unsupported_model_returns_clear_error() {
        // A lazy pool: nothing connects, because the function errors
        // before any SQL runs.
        let pg = crate::sql::sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let pool: crate::sql::Pool = pg.into();
        let err = soft_delete(&pool, &MODEL_WITHOUT_SD, "id", SqlValue::I64(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SoftDeleteError::NotSoftDeleteEnabled("Tag")));
    }

    #[tokio::test]
    #[cfg(feature = "postgres")]
    async fn restore_on_unsupported_model_returns_clear_error() {
        let pg = crate::sql::sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let pool: crate::sql::Pool = pg.into();
        let err = restore(&pool, &MODEL_WITHOUT_SD, "id", SqlValue::I64(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SoftDeleteError::NotSoftDeleteEnabled("Tag")));
    }
}
