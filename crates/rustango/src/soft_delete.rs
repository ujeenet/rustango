//! Soft-delete query helpers.
//!
//! `#[rustango(soft_delete)]` on a model marks one column as the
//! "deleted-at" timestamp; the admin DELETE handler already routes
//! through it (sets the column to `NOW()` instead of running `DELETE`).
//! This module finishes the story for the read side: helpers that
//! filter out soft-deleted rows, restore them, or purge them.
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
//! For models without a soft-delete column, the helpers are no-ops:
//! [`active_filter`] returns `None`, [`compose_with_active`] returns
//! the input unchanged, and [`soft_delete`] errors with a clear message
//! so callers don't silently leak rows that were supposed to be hidden.

use crate::core::{
    Assignment, DeleteQuery, Filter, ModelSchema, Op, SqlValue, UpdateQuery, WhereExpr,
};
use crate::sql::sqlx::PgPool;
use crate::sql::{delete as sql_delete, update as sql_update, ExecError};

/// Returns `Some(<col> IS NULL)` when `model` is soft-delete-enabled,
/// `None` otherwise. Use to filter for rows that haven't been trashed.
#[must_use]
pub fn active_filter(model: &'static ModelSchema) -> Option<WhereExpr> {
    let col = model.soft_delete_column?;
    Some(WhereExpr::Predicate(Filter {
        column: col,
        op: Op::IsNull,
        value: SqlValue::Bool(true),
    }))
}

/// Returns `Some(<col> IS NOT NULL)` when `model` is soft-delete-enabled,
/// `None` otherwise. Use to filter for rows that ARE trashed (e.g. a
/// "Trash" admin page).
#[must_use]
pub fn trashed_filter(model: &'static ModelSchema) -> Option<WhereExpr> {
    let col = model.soft_delete_column?;
    Some(WhereExpr::Predicate(Filter {
        column: col,
        op: Op::IsNull,
        value: SqlValue::Bool(false),
    }))
}

/// Wrap `existing` so trashed rows are excluded.
///
/// - If the model has no soft-delete column, returns `existing` unchanged.
/// - If `existing` is empty (`WhereExpr::And(vec![])`), returns the
///   active filter alone.
/// - Otherwise returns `WhereExpr::And([existing, active_filter])`.
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

/// Same as [`compose_with_active`] but selects trashed rows instead.
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

/// Set `model`'s soft-delete column to `NOW()` for the row whose `pk`
/// equals `pk_value`. Returns the number of rows affected.
///
/// # Errors
/// [`SoftDeleteError::NotSoftDeleteEnabled`] when the model has no
/// `#[rustango(soft_delete)]` field.
/// [`SoftDeleteError::Exec`] for the underlying sqlx error.
pub async fn soft_delete(
    pool: &PgPool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, SoftDeleteError> {
    let col = model
        .soft_delete_column
        .ok_or(SoftDeleteError::NotSoftDeleteEnabled(model.name))?;
    let n = sql_update(
        pool,
        &UpdateQuery {
            model,
            set: vec![Assignment {
                column: col,
                value: SqlValue::from(chrono::Utc::now()),
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

/// Reverse a soft-delete: set the soft-delete column back to `NULL`.
/// Returns the number of rows affected.
///
/// # Errors
/// [`SoftDeleteError::NotSoftDeleteEnabled`] when the model has no
/// `#[rustango(soft_delete)]` field.
/// [`SoftDeleteError::Exec`] for the underlying sqlx error.
pub async fn restore(
    pool: &PgPool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, SoftDeleteError> {
    let col = model
        .soft_delete_column
        .ok_or(SoftDeleteError::NotSoftDeleteEnabled(model.name))?;
    let n = sql_update(
        pool,
        &UpdateQuery {
            model,
            set: vec![Assignment {
                column: col,
                value: SqlValue::Null,
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

/// Hard-delete the row, bypassing soft-delete entirely. Use sparingly —
/// this is the irreversible "purge from trash" operation. Returns the
/// number of rows affected.
///
/// Works on any model, soft-delete-enabled or not.
///
/// # Errors
/// Underlying sqlx error.
pub async fn purge(
    pool: &PgPool,
    model: &'static ModelSchema,
    pk_column: &'static str,
    pk_value: SqlValue,
) -> Result<u64, ExecError> {
    sql_delete(
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
        m2m: &[],
        composite_relations: &[],
        generic_relations: &[],
        scope: crate::core::ModelScope::Tenant,
    };

    static MODEL_WITHOUT_SD: ModelSchema = ModelSchema {
        name: "Tag",
        table: "tags",
        fields: FIELDS_WITH_SD, // share the slice; soft_delete_column = None
        display: None,
        app_label: None,
        admin: None,
        soft_delete_column: None,
        audit_track: None,
        permissions: false,
        indexes: &[],
        check_constraints: &[],
        m2m: &[],
        composite_relations: &[],
        generic_relations: &[],
        scope: crate::core::ModelScope::Tenant,
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
                // First child is the existing predicate, second is the active filter.
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
    async fn soft_delete_on_unsupported_model_returns_clear_error() {
        // Use a connect_lazy pool — never actually dialed because the
        // function returns the error before any SQL runs.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let err = soft_delete(&pool, &MODEL_WITHOUT_SD, "id", SqlValue::I64(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SoftDeleteError::NotSoftDeleteEnabled("Tag")));
    }

    #[tokio::test]
    async fn restore_on_unsupported_model_returns_clear_error() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost:1/none")
            .unwrap();
        let err = restore(&pool, &MODEL_WITHOUT_SD, "id", SqlValue::I64(1))
            .await
            .unwrap_err();
        assert!(matches!(err, SoftDeleteError::NotSoftDeleteEnabled("Tag")));
    }
}
