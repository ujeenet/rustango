//! Apply DDL against a live Postgres pool.
//!
//! Walks the inventory registry — every `#[derive(Model)]` in the binary
//! shows up here — and runs the writer over each model.

use rustango_core::{inventory, ModelEntry, ModelSchema};
use rustango_sql::sqlx::{self, PgPool};

use crate::{ddl, MigrateError};

/// Collect every registered model's schema into a `Vec`. Order is the
/// order of registration (linker order); callers that care should sort.
#[must_use]
pub fn registered_models() -> Vec<&'static ModelSchema> {
    inventory::iter::<ModelEntry>
        .into_iter()
        .map(|e| e.schema)
        .collect()
}

/// Run `CREATE TABLE` for every registered model, then every model's FK
/// `ALTER TABLE` constraints. Two-phase so create order doesn't matter.
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure (connection, syntax,
/// constraint violation).
pub async fn apply_all(pool: &PgPool) -> Result<(), MigrateError> {
    let models = registered_models();

    for model in &models {
        let sql = ddl::create_table_sql(model);
        sqlx::query(&sql).execute(pool).await?;
    }
    for model in &models {
        for sql in ddl::create_constraints_sql(model) {
            sqlx::query(&sql).execute(pool).await?;
        }
    }
    Ok(())
}

/// `DROP TABLE IF EXISTS … CASCADE` for every registered model. CASCADE
/// makes order irrelevant — FKs go away with the parent table.
///
/// # Errors
/// Returns [`MigrateError`] for any sqlx failure.
pub async fn drop_all(pool: &PgPool) -> Result<(), MigrateError> {
    for model in registered_models() {
        let sql = ddl::drop_table_sql(model, /* if_exists */ true, /* cascade */ true);
        sqlx::query(&sql).execute(pool).await?;
    }
    Ok(())
}
