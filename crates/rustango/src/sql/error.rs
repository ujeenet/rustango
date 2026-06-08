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

    /// `Op::Between` requires `SqlValue::List` with exactly two elements `[lo, hi]`.
    #[error("`Op::Between` requires `SqlValue::List([lo, hi])` with exactly two elements")]
    BetweenRequiresTwoElementList,

    /// `Op::JsonHasKey` requires `SqlValue::String`.
    #[error("`Op::JsonHasKey` requires `SqlValue::String`")]
    JsonKeyRequiresString,

    /// `Op::JsonHasAnyKey` / `Op::JsonHasAllKeys` require `SqlValue::List` of strings.
    #[error("`Op::JsonHasAnyKey` / `Op::JsonHasAllKeys` require `SqlValue::List` of strings")]
    JsonKeysRequiresList,

    /// `Op::JsonContains` / `Op::JsonContainedBy` require `SqlValue::Json`.
    #[error("`Op::JsonContains` / `Op::JsonContainedBy` require `SqlValue::Json`")]
    JsonOpRequiresJson,

    /// PG ArrayField operators (`@>`, `<@`, `&&`) require
    /// [`crate::core::SqlValue::Array`]. A plain `List` would expand
    /// to comma-separated placeholders — the wrong shape for array
    /// comparison, which needs a single PG array parameter.
    #[error(
        "`Op::ArrayContains` / `Op::ArrayContainedBy` / `Op::ArrayOverlap` require `SqlValue::Array`"
    )]
    ArrayOpRequiresArray,

    /// `BulkUpdateQuery` was used on a model with no `#[rustango(primary_key)]`
    /// field — the WHERE clause cannot be formed.
    #[error("bulk UPDATE requires a primary key on the model")]
    MissingPrimaryKey,

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

    /// `WhereExpr::Xor(vec![])` — issue #27. XOR over zero operands
    /// is vacuously false (an empty "odd-number-of-trues" tally is
    /// `0 % 2 = 1` → false), almost always a programming error.
    /// Sibling to [`Self::EmptyOrBranch`].
    #[error("`WhereExpr::Xor` with an empty branch list matches no rows; was that intentional?")]
    EmptyXorBranch,

    /// `Dialect::compile_*` was called on a dialect whose query
    /// compiler hasn't shipped yet.
    #[error(
        "{dialect} dialect query compilation is not implemented yet — \
         lands in a future rustango v0.23.0 batch."
    )]
    DialectQueryCompilationNotImplemented { dialect: &'static str },

    /// A query operator is supported by the rustango IR but has no
    /// equivalent (or no equivalent yet) in the active dialect.
    /// Examples: `ILIKE` and the JSONB `?` / `?|` / `?&` / `@>` / `<@`
    /// operators are Postgres-only; `IS DISTINCT FROM` is in standard
    /// SQL but `MySQL` only ships the inverse `<=>` (null-safe equal)
    /// — translation is on the v0.23.0-batch4 punch-list.
    #[error("operator `{op}` is not supported by the `{dialect}` dialect")]
    OperatorNotSupportedInDialect {
        op: &'static str,
        dialect: &'static str,
    },

    /// An ON CONFLICT clause shape isn't expressible in the active
    /// dialect's syntax. Postgres supports
    /// `ON CONFLICT (col) DO UPDATE SET col = EXCLUDED.col`; `MySQL`'s
    /// `ON DUPLICATE KEY UPDATE` doesn't take a target column list
    /// (it triggers on any unique violation), so a `DoUpdate` with a
    /// non-empty `target` cannot be translated 1:1.
    #[error("ON CONFLICT shape `{shape}` is not supported by the `{dialect}` dialect")]
    ConflictNotSupportedInDialect {
        shape: &'static str,
        dialect: &'static str,
    },

    /// A [`crate::core::BinOp`] variant has no dialect-portable
    /// translation on this backend. Today raised only for
    /// `BinOp::BitXor` on SQLite (SQLite has `&`, `|`, `<<`, `>>` but
    /// no bitwise-XOR operator). Caller can either route to a different
    /// op (e.g. `(a | b) - (a & b)`) or restrict the feature to
    /// PG / MySQL.
    #[error("operator `{op}` is not supported by the `{dialect}` dialect")]
    OpNotSupportedInDialect {
        op: &'static str,
        dialect: &'static str,
    },

    /// A Postgres-specific aggregate (`array_agg`, `string_agg`,
    /// `jsonb_agg`, etc.) was requested on a non-PG backend. Issue #33.
    /// MySQL has `GROUP_CONCAT` and `JSON_ARRAYAGG` that overlap
    /// semantically but the syntax differs enough that we don't
    /// auto-translate — caller should branch on `pool.dialect().name()`
    /// or restrict the feature to PG-only deployments.
    #[error("aggregate `{aggregate}` is not supported by the `{dialect}` dialect")]
    AggregateNotSupportedInDialect {
        aggregate: &'static str,
        dialect: &'static str,
    },

    /// A scalar function (issue #2) was built with the wrong number of
    /// arguments — e.g. `Substr` with 2 args, `NullIf` with 3, or
    /// `Concat` with 0. The builder-side API constrains arity for
    /// fixed-arity calls (compile error), but the IR is permissive
    /// enough that hand-rolled `Expr::Function { args: vec![...] }`
    /// could trip this — the emitter catches it before reaching the
    /// database with a confusing parse error.
    #[error("function `{func}` expects {expected} arg(s), got {got}")]
    FunctionArityMismatch {
        func: &'static str,
        expected: &'static str,
        got: usize,
    },

    /// A `CASE WHEN … END` expression was built with no branches.
    /// SQL requires at least one `WHEN` clause; the public builder
    /// API ([`crate::core::case()`]) doesn't prevent zero-branch
    /// construction, so the writer surfaces this here.
    #[error("CASE expression must have at least one WHEN branch")]
    EmptyCaseBranches,

    /// A `CASE WHEN <cond> …` branch had an empty predicate (e.g.
    /// `WhereExpr::And(vec![])`). The standard "no WHERE filter"
    /// marker is legal at the top of an UPDATE/DELETE, but inside a
    /// `WHEN` it would produce `WHEN  THEN …` with a hole — a parse
    /// error on every backend. Reject it loudly here.
    #[error("CASE WHEN branch condition must not be empty")]
    EmptyCaseWhenCondition,

    /// `Expr::OuterRef("col")` was emitted outside any subquery
    /// scope (issue #5). `OuterRef` only makes sense inside a
    /// correlated subquery — the writer needs at least two scope
    /// frames on the stack (outer + subquery) to resolve the column
    /// against the enclosing query. Programming error.
    #[error(
        "`OuterRef(\"{column}\")` used outside of a subquery — \
         it can only appear inside Exists / NotExists / InSubquery / \
         Subquery wrappers that know the enclosing query's table"
    )]
    OuterRefOutsideSubquery { column: &'static str },

    /// `Expr::Aggregate(...)` was emitted in a SQL slot that doesn't
    /// allow aggregate function calls (issue #88). Every dialect
    /// (PG, MySQL, SQLite) rejects aggregates in `WHERE` /
    /// `UPDATE SET` / `JOIN ON` / `GROUP BY` / `RETURNING` /
    /// non-aggregate `SELECT` projections; only `HAVING`, the SELECT
    /// list of an aggregating query, and that query's `ORDER BY`
    /// are valid homes for an aggregate call. The writer enforces
    /// this upfront with a clear error rather than passing the SQL
    /// through to the database, which would surface a less
    /// helpful "aggregate functions are not allowed in WHERE" or
    /// equivalent. Programming error — restructure the query to
    /// reference an aggregate annotation alias via the auto-routing
    /// `QuerySet::filter(...)` (which goes to HAVING) or move the
    /// aggregate to the SELECT list.
    #[error(
        "`Expr::Aggregate(...)` used outside of an aggregate-accepting \
         SQL slot — aggregates may only appear in SELECT projection, \
         HAVING predicate, or ORDER BY of an aggregating query"
    )]
    AggregateOutsideAggregateContext,

    /// A JOIN was constructed with an empty `on` predicate
    /// (`WhereExpr::And(vec![])` — the legitimate "no WHERE filter"
    /// marker at the top of an UPDATE/DELETE/SELECT). Inside a JOIN's
    /// ON it would emit `ON ` with a literal hole, which every
    /// backend rejects at parse. Mirror of `EmptyCaseWhenCondition`
    /// for the JOIN-ON context.
    #[error("JOIN `on` predicate must not be empty")]
    EmptyJoinOnCondition,

    /// An aggregate function isn't supported by the active dialect
    /// (issue #6). Today raised only for `StdDev` / `StdDevPop` /
    /// `Variance` / `VariancePop` on SQLite, which has no built-in
    /// statistical aggregates. Caller can either switch dialects,
    /// drop the offending annotation, or compute the variance
    /// formula in app code.
    #[error("aggregate `{aggregate}` is not supported by the `{dialect}` dialect")]
    AggregateNotSupported {
        aggregate: &'static str,
        dialect: &'static str,
    },

    /// An ill-formed `AggregateExpr` tree was passed in (issue #6) —
    /// e.g. `Coalesced { Coalesced { … } }` or `Filtered { Filtered {
    /// … } }`. The public [`crate::core::aggregates`] builder never
    /// produces these, so this is a "hand-rolled IR" programmer
    /// error. `wrapper` names the offending shape for the error
    /// message.
    #[error("nested aggregate wrapper `{wrapper}` is not supported")]
    NestedAggregateWrapper { wrapper: &'static str },

    /// A [`crate::core::JoinKind`] was used on a dialect that doesn't
    /// support it (issue #80). Today: `Right` on SQLite, `Full` on
    /// SQLite + MySQL. Caller can either switch dialects, restructure
    /// the query (e.g. swap operands and use `Left` instead of `Right`,
    /// or emulate `Full` via two `Left`/`Right` joins UNION'd), or
    /// gate the feature behind a `cfg`-flag.
    #[error("`{kind} JOIN` is not supported by the `{dialect}` dialect")]
    JoinKindNotSupported {
        kind: &'static str,
        dialect: &'static str,
    },

    /// A `JOIN LATERAL (...)` (Eloquent `joinLateral`, issue #828) was
    /// emitted against a dialect without `LATERAL` support. PostgreSQL
    /// and MySQL ≥ 8.0.14 support it; SQLite does not. Rewrite as a
    /// correlated subquery / window function, or use a non-lateral
    /// `join_sub` against a pre-aggregated derived table.
    #[error(
        "`JOIN LATERAL` is not supported by the `{dialect}` dialect (PostgreSQL / MySQL only)"
    )]
    LateralJoinNotSupported { dialect: &'static str },
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
        /// Display-formatted PK value. `String` rather than `i64` so
        /// the variant covers UUID, String, and other non-integer
        /// `ForeignKey<T, K>` shapes — `K`'s `Into<SqlValue>` lowering
        /// drives the rendering in `ForeignKey::get_on`.
        pk: String,
    },

    /// Used when traversing schema metadata to resolve a foreign key
    /// or build a `WHERE pk = …` filter — the target model declares
    /// no `#[rustango(primary_key)]` field. Programming error;
    /// surfaces only if a model deriving `Model` somehow lacks a PK.
    #[error("model `{table}` has no `#[rustango(primary_key)]` field — required for FK lookup")]
    MissingPrimaryKey { table: &'static str },

    /// A polymorphic relation (generic FK / generic M2M, issues #818 /
    /// #64) needed the owning model's `content_type_id`, but no
    /// `rustango_content_types` row exists for `{table}`. Content types
    /// are normally auto-seeded at migrate time; run the contenttypes
    /// seed (or `migrate`) so the discriminator can be resolved.
    #[error("no content type registered for model `{table}` — seed `rustango_content_types` (run migrate)")]
    ContentTypeNotRegistered { table: &'static str },

    /// `get_or_create` / `update_or_create` (v0.45) was called with a
    /// filter that matches more than one row. Django's
    /// `MultipleObjectsReturned`. Tighten the filter or use
    /// [`crate::query::QuerySet::first`] when ambiguity is
    /// acceptable.
    #[error("`{op}` filter matched {count} rows on `{table}`; expected at most 1")]
    MultipleRowsReturned {
        op: &'static str,
        table: &'static str,
        count: usize,
    },
}

// =====================================================================
// Shared driver-error predicates
// =====================================================================
//
// #561 — recognizing "duplicate index name" on MySQL used to live as
// 4 byte-identical copies in `audit.rs`, `contenttypes.rs`,
// `jobs/pg.rs`, and `media/tag.rs` (plus `#[cfg(not(mysql))]`
// stubs returning `false`). Migration / DDL idempotency code that
// runs `CREATE INDEX IF NOT EXISTS` against MySQL — which has no
// such syntax — has to swallow the duplicate error to stay
// idempotent. Predicate exposed once here; callers route through
// `crate::sql::is_mysql_dup_index_error`.

/// `true` when `e` is MySQL's `ER_DUP_KEYNAME` (1061) — raised by
/// `CREATE INDEX` against a name that already exists. MySQL has no
/// `CREATE INDEX IF NOT EXISTS`, so the idempotent ensure-table code
/// catches this error and continues.
///
/// On a build without the `mysql` cargo feature the predicate
/// returns `false` for every error — there's no MySQL driver
/// compiled in so this code path can't fire.
#[cfg(feature = "mysql")]
pub fn is_mysql_dup_index_error(e: &crate::sql::sqlx::Error) -> bool {
    if let crate::sql::sqlx::Error::Database(db) = e {
        return db.code().as_deref() == Some("42000")
            || db.message().contains("Duplicate key name");
    }
    false
}

/// `cfg(not(mysql))` stub — see the documented variant above.
#[cfg(not(feature = "mysql"))]
pub fn is_mysql_dup_index_error(_e: &crate::sql::sqlx::Error) -> bool {
    false
}
