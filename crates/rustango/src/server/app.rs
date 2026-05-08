//! `rustango::server::AppBuilder` — single-pool bi-dialect bootstrap.
//!
//! The Django-style multi-tenant [`super::Builder`] is hardcoded to
//! `PgPool` (it owns a `TenantPools` registry whose connections are
//! Postgres). For apps that don't need tenancy and want to run on
//! SQLite (or MySQL), this is a parallel, simpler builder that takes
//! any [`Pool`] and skips the tenant-resolver / operator-console /
//! tenant-admin stack.
//!
//! ```ignore
//! use rustango::server::AppBuilder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     AppBuilder::from_env().await?
//!         .bootstrap(&[my_app::Author::SCHEMA, my_app::Post::SCHEMA])
//!         .await?
//!         .api(my_app::routes())
//!         .serve("0.0.0.0:8080").await
//! }
//! ```
//!
//! With `DATABASE_URL=sqlite:./var/app.db?mode=rwc` the app boots
//! against a file-backed SQLite database, creating tables on first
//! run. The same code with `DATABASE_URL=postgres://...` boots
//! against Postgres unchanged.

use std::sync::Arc;

use axum::{Extension, Router};

use crate::core::ModelSchema;
use crate::sql::{raw_execute_pool, Pool};

/// Single-pool builder. Use [`Self::from_env`] (reads `DATABASE_URL`)
/// or [`Self::from_pool`] (you supply the pool) to construct.
pub struct AppBuilder {
    pool: Pool,
    api: Option<Router>,
    schemas: Vec<&'static ModelSchema>,
}

impl AppBuilder {
    /// Connect via [`Pool::connect`] using `DATABASE_URL`. Recognized
    /// URL schemes (each gated on the matching feature):
    ///
    /// - `postgres://` / `postgresql://`  (feature `postgres`)
    /// - `mysql://`                        (feature `mysql`)
    /// - `sqlite:` / `sqlite://`           (feature `sqlite`)
    ///
    /// SQLite forms accepted by sqlx: `sqlite::memory:`,
    /// `sqlite:./relative.db?mode=rwc`, `sqlite:///abs/path.db`.
    ///
    /// # Errors
    /// `DATABASE_URL` unset, unsupported scheme for the active
    /// feature set, or driver connect failure.
    pub async fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let url = std::env::var("DATABASE_URL").map_err(|_| {
            "DATABASE_URL not set. Examples:\n  \
             sqlite:./var/app.db?mode=rwc\n  \
             postgres://user:pass@localhost:5432/dbname\n  \
             mysql://user:pass@localhost:3306/dbname"
        })?;
        let pool = Pool::connect(&url).await?;
        Ok(Self::from_pool(pool))
    }

    /// Construct from an existing pool — useful for tests that want
    /// to inject a `sqlite::memory:` pool, or for apps that need
    /// custom `SqlitePoolOptions` (e.g. `max_connections(1)` for
    /// in-memory sharing).
    #[must_use]
    pub fn from_pool(pool: Pool) -> Self {
        Self {
            pool,
            api: None,
            schemas: Vec::new(),
        }
    }

    /// Borrow the connected pool. Pre-`serve` access lets seed code
    /// or one-shot setup run without rebuilding the pool.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Run `CREATE TABLE IF NOT EXISTS` for each supplied model
    /// schema, using the dialect-aware DDL emitter. Idempotent.
    ///
    /// **Why explicit and not `apply_all_pool`?** The latter walks
    /// every registered model in `inventory`, which on a default
    /// rustango build includes framework models (Org, Operator,
    /// Job, etc.) that emit Postgres-shape DDL. Passing the explicit
    /// list of *your* model schemas keeps SQLite happy.
    ///
    /// FK constraints are emitted via `ALTER TABLE … ADD CONSTRAINT`
    /// on Postgres / MySQL but skipped on SQLite (SQLite only
    /// accepts inline FK constraints at CREATE TABLE time — separate
    /// fix tracked for v0.28).
    ///
    /// # Errors
    /// Driver / SQL failures from the CREATE TABLE / ALTER TABLE
    /// statements.
    pub async fn bootstrap(
        mut self,
        schemas: &[&'static ModelSchema],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use crate::migrate::ddl;
        let dialect = self.pool.dialect();
        for schema in schemas {
            let sql = ddl::create_table_sql_with_dialect(dialect, schema);
            raw_execute_pool(&self.pool, &sql, vec![]).await?;
        }
        // SQLite parser rejects ALTER TABLE … ADD CONSTRAINT FOREIGN
        // KEY — FK constraints have to be declared inline at CREATE
        // TABLE time. Skip the FK loop on SQLite.
        if dialect.name() != "sqlite" {
            for schema in schemas {
                for sql in ddl::create_constraints_sql_with_dialect(dialect, schema) {
                    raw_execute_pool(&self.pool, &sql, vec![]).await?;
                }
            }
        }
        self.schemas.extend(schemas);
        Ok(self)
    }

    /// Mount user API routes. Inside handlers, extract the pool with
    /// `Extension<Arc<Pool>>`:
    ///
    /// ```ignore
    /// async fn list(Extension(pool): Extension<Arc<Pool>>) -> ... { ... }
    /// ```
    #[must_use]
    pub fn api(mut self, router: Router) -> Self {
        self.api = Some(router);
        self
    }

    /// Bind + serve. Injects `Extension<Arc<Pool>>` into every
    /// request so handlers can run ORM calls without a stateful
    /// router.
    ///
    /// # Errors
    /// `bind` failure, or the underlying `axum::serve` returning
    /// an error.
    pub async fn serve(self, addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        let pool = Arc::new(self.pool);
        let app = self.api.unwrap_or_else(Router::new).layer(Extension(pool));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn from_pool_sqlite_in_memory_smoke() {
        // Confirms `AppBuilder::from_pool` accepts `Pool::Sqlite`
        // and the dialect dispatch works end-to-end. We don't
        // bootstrap a model here (a Model derive inside a test mod
        // can't satisfy inventory's `super::` lookup); the live
        // round-trip check is in `tests/sqlite_live.rs`.
        let sqlx_pool = crate::sql::sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let pool: Pool = sqlx_pool.into();
        let builder = AppBuilder::from_pool(pool);
        assert_eq!(builder.pool().backend_name(), "sqlite");
        assert!(builder.pool().as_sqlite().is_some());
    }
}
