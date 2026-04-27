//! Async executor — binds a `CompiledStatement` to sqlx and runs it.

use rustango_core::{Model, SqlValue};
use rustango_query::QuerySet;
use sqlx::postgres::{PgArguments, PgPool, PgRow};
use sqlx::query::QueryAs;

use crate::{Dialect, ExecError, Postgres};

/// Extension trait that drives a `QuerySet` to completion against a Postgres pool.
///
/// Adds `.fetch(&pool)` to any `QuerySet<T>` whose `T` is `Model + FromRow`.
/// Pulled in via `use rustango::sql::Fetcher;`.
pub trait Fetcher<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    /// Compile the queryset, write Postgres SQL, and run `fetch_all`.
    ///
    /// # Errors
    /// Returns [`ExecError`] if any of the three stages fails: schema
    /// validation, SQL writing, or the underlying sqlx call.
    fn fetch(
        self,
        pool: &PgPool,
    ) -> impl std::future::Future<Output = Result<Vec<T>, ExecError>> + Send;
}

impl<T> Fetcher<T> for QuerySet<T>
where
    T: Model + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin,
{
    async fn fetch(self, pool: &PgPool) -> Result<Vec<T>, ExecError> {
        let select = self.compile()?;
        let stmt = Postgres.compile_select(&select)?;

        let mut q: QueryAs<'_, sqlx::Postgres, T, PgArguments> = sqlx::query_as::<_, T>(&stmt.sql);
        for value in stmt.params {
            q = bind(q, value);
        }
        let rows = q.fetch_all(pool).await?;
        Ok(rows)
    }
}

fn bind<T>(
    q: QueryAs<'_, sqlx::Postgres, T, PgArguments>,
    value: SqlValue,
) -> QueryAs<'_, sqlx::Postgres, T, PgArguments> {
    match value {
        // `None::<String>` produces a typed NULL Postgres accepts in any context.
        SqlValue::Null => q.bind(None::<String>),
        SqlValue::I32(v) => q.bind(v),
        SqlValue::I64(v) => q.bind(v),
        SqlValue::F32(v) => q.bind(v),
        SqlValue::F64(v) => q.bind(v),
        SqlValue::Bool(v) => q.bind(v),
        SqlValue::String(v) => q.bind(v),
        SqlValue::DateTime(v) => q.bind(v),
        SqlValue::Date(v) => q.bind(v),
        SqlValue::Uuid(v) => q.bind(v),
        SqlValue::Json(_) => {
            unreachable!("`SqlValue::Json` requires the `sqlx/json` feature, not enabled in v0.1")
        }
        SqlValue::List(_) => {
            unreachable!("`SqlValue::List` is expanded to scalars by the SQL writer")
        }
    }
}
