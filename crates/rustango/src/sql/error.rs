//! SQL writer & executor errors.

use crate::core::QueryError;

/// Raised while lowering a `SelectQuery` to a parameterized statement.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    /// `Op::In` was used with something other than `SqlValue::List`.
    #[error("`Op::In` requires `SqlValue::List`")]
    InRequiresList,

    /// `Op::IsNull` was used with something other than `SqlValue::Bool`.
    #[error("`Op::IsNull` requires `SqlValue::Bool` (true = IS NULL, false = IS NOT NULL)")]
    IsNullRequiresBool,

    /// `Op::In` with an empty list — Postgres does not accept `IN ()`.
    #[error("empty `IN` list is not supported")]
    EmptyInList,

    /// `InsertQuery` had no columns — Postgres does not accept zero-column inserts.
    #[error("INSERT requires at least one column")]
    EmptyInsert,

    /// `InsertQuery.columns.len() != InsertQuery.values.len()`.
    #[error("INSERT columns ({columns}) and values ({values}) length mismatch")]
    InsertShapeMismatch { columns: usize, values: usize },

    /// `UpdateQuery` had no assignments — `UPDATE ... SET` requires at least one.
    #[error("UPDATE requires at least one assignment in `set`")]
    EmptyUpdateSet,

    /// `BulkInsertQuery` had no rows — caller should short-circuit.
    #[error("bulk INSERT requires at least one row")]
    EmptyBulkInsert,

    /// Macro-generated `Model::bulk_insert` was called with rows that
    /// disagree on whether their `Auto<T>` PKs are `Set` or `Unset`.
    /// Mixed-shape inserts aren't supported in v0.4 — the column list
    /// must be consistent across the batch. Either set every PK or
    /// leave every PK unset; for surgical mixes, call `insert` per row.
    #[error("bulk INSERT requires every row's `Auto<T>` PKs to agree on Set vs Unset; mixed Set/Unset is not supported")]
    BulkAutoMixed,

    /// `bulk_insert` returned a different number of rows than were
    /// requested — sanity check before populating Auto fields.
    #[error("bulk INSERT RETURNING returned {actual} rows but {expected} were inserted")]
    BulkInsertReturningMismatch { expected: usize, actual: usize },

    /// `WhereExpr::Or(vec![])` — a disjunction with no children
    /// matches no rows. The writer rejects it so the user catches the
    /// programming error instead of silently fetching an empty
    /// result. (`WhereExpr::And(vec![])` is fine — represents
    /// "no filters" and is the default.)
    #[error("`WhereExpr::Or` with an empty branch list matches no rows; was that intentional?")]
    EmptyOrBranch,
}

/// Raised while compiling, writing, or executing a query end-to-end.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Query(#[from] QueryError),

    #[error(transparent)]
    Sql(#[from] SqlError),

    #[error(transparent)]
    Driver(#[from] sqlx::Error),

    /// `insert_returning` was called with an `InsertQuery` carrying no
    /// `RETURNING` columns. Use `insert` for those.
    #[error("`insert_returning` requires `query.returning` to be non-empty; use `insert` instead")]
    EmptyReturning,

    /// `ForeignKey::get` resolved a PK that didn't match any row in
    /// the target table. Means the parent was deleted under a
    /// non-CASCADE constraint, or the FK was constructed by hand with
    /// an out-of-band value.
    #[error("foreign-key target `{table}` has no row with primary key {pk}")]
    ForeignKeyTargetMissing {
        table: &'static str,
        pk: i64,
    },

    /// Used when traversing schema metadata to resolve a foreign key
    /// or build a `WHERE pk = …` filter — the target model declares
    /// no `#[rustango(primary_key)]` field. Programming error;
    /// surfaces only if a model deriving `Model` somehow lacks a PK.
    #[error("model `{table}` has no `#[rustango(primary_key)]` field — required for FK lookup")]
    MissingPrimaryKey {
        table: &'static str,
    },
}
