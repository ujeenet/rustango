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

    /// The array operators need a [`crate::core::SqlValue::Array`],
    /// which binds as one parameter. A `List` would expand to
    /// separate placeholders, which is the wrong shape.
    #[error(
        "`Op::ArrayContains` / `Op::ArrayContainedBy` / `Op::ArrayOverlap` require `SqlValue::Array`"
    )]
    ArrayOpRequiresArray,

    /// `BulkUpdateQuery` on a model with no primary key, so there is
    /// nothing to match rows on.
    #[error("bulk UPDATE requires a primary key on the model")]
    MissingPrimaryKey,

    /// A relation aggregate other than `COUNT` arrived with no
    /// target column. The builder always supplies one, so this
    /// catches a hand-built node.
    #[error("relation aggregate `{kind}` requires a target column")]
    RelAggregateMissingColumn { kind: &'static str },

    /// `Op::In` with an empty list; SQL has no `IN ()`.
    #[error("empty `IN` list is not supported")]
    EmptyInList,

    /// `InsertQuery` had no columns.
    #[error("INSERT requires at least one column")]
    EmptyInsert,

    /// `InsertQuery.columns.len() != InsertQuery.values.len()`.
    #[error("INSERT columns ({columns}) and values ({values}) length mismatch")]
    InsertShapeMismatch { columns: usize, values: usize },

    /// `UpdateQuery` had no assignments; `SET` needs at least one.
    #[error("UPDATE requires at least one assignment in `set`")]
    EmptyUpdateSet,

    /// `BulkInsertQuery` had no rows; return early instead.
    #[error("bulk INSERT requires at least one row")]
    EmptyBulkInsert,

    /// `bulk_insert` got rows that disagree on whether their
    /// `Auto<T>` PKs are set, but one statement needs one column
    /// list. Set every PK or none; for a mix, insert row by row.
    #[error("bulk INSERT requires every row's `Auto<T>` PKs to agree on Set vs Unset; mixed Set/Unset is not supported")]
    BulkAutoMixed,

    /// `bulk_insert` returned a different number of rows than it was
    /// given, checked before filling in the `Auto` fields.
    #[error("bulk INSERT RETURNING returned {actual} rows but {expected} were inserted")]
    BulkInsertReturningMismatch { expected: usize, actual: usize },

    /// An `Or` with no children, which would match nothing. It is
    /// rejected so the mistake shows up instead of an empty result.
    /// An empty `And` is fine: that means "no filters".
    #[error("`WhereExpr::Or` with an empty branch list matches no rows; was that intentional?")]
    EmptyOrBranch,

    /// An `Xor` with no children, which is always false. As with
    /// [`Self::EmptyOrBranch`], that is almost always a mistake.
    #[error("`WhereExpr::Xor` with an empty branch list matches no rows; was that intentional?")]
    EmptyXorBranch,

    /// `Dialect::compile_*` was called on a dialect whose query
    /// compiler hasn't shipped yet.
    #[error(
        "{dialect} dialect query compilation is not implemented yet — \
         lands in a future rustango v0.23.0 batch."
    )]
    DialectQueryCompilationNotImplemented { dialect: &'static str },

    /// The IR has this operator but the active dialect has nothing
    /// to write it as. The JSONB operators, for one, are
    /// Postgres-only.
    #[error("operator `{op}` is not supported by the `{dialect}` dialect")]
    OperatorNotSupportedInDialect {
        op: &'static str,
        dialect: &'static str,
    },

    /// The active dialect cannot express this `ON CONFLICT` shape.
    /// MySQL, for one, has no target column list.
    #[error("ON CONFLICT shape `{shape}` is not supported by the `{dialect}` dialect")]
    ConflictNotSupportedInDialect {
        shape: &'static str,
        dialect: &'static str,
    },

    /// A [`crate::core::BinOp`] this backend cannot write. Only
    /// `BitXor` on SQLite so far, which has the other bitwise
    /// operators but not that one; `(a | b) - (a & b)` is the same
    /// thing.
    #[error("operator `{op}` is not supported by the `{dialect}` dialect")]
    OpNotSupportedInDialect {
        op: &'static str,
        dialect: &'static str,
    },

    /// A Postgres-only aggregate such as `array_agg` or `jsonb_agg`
    /// on another backend. MySQL's nearest equivalents differ enough
    /// that they are not translated automatically, so branch on
    /// `pool.dialect().name()` yourself.
    #[error("aggregate `{aggregate}` is not supported by the `{dialect}` dialect")]
    AggregateNotSupportedInDialect {
        aggregate: &'static str,
        dialect: &'static str,
    },

    /// A scalar function with the wrong number of arguments. The
    /// builders fix the count at compile time, so this catches a
    /// hand-built `Expr::Function` before the database sees it.
    #[error("function `{func}` expects {expected} arg(s), got {got}")]
    FunctionArityMismatch {
        func: &'static str,
        expected: &'static str,
        got: usize,
    },

    /// A `CASE` with no branches. SQL needs at least one `WHEN`, and
    /// [`crate::core::case()`] does not stop you building none.
    #[error("CASE expression must have at least one WHEN branch")]
    EmptyCaseBranches,

    /// A `CASE` branch with an empty predicate. An empty `And` means
    /// "no filter" at the top of an UPDATE, but inside a `WHEN` it
    /// would leave a hole that no backend parses.
    #[error("CASE WHEN branch condition must not be empty")]
    EmptyCaseWhenCondition,

    /// An `OuterRef` outside any subquery. It only means something
    /// inside a correlated subquery, where there is an enclosing
    /// query to resolve the column against.
    #[error(
        "`OuterRef(\"{column}\")` used outside of a subquery — \
         it can only appear inside Exists / NotExists / InSubquery / \
         Subquery wrappers that know the enclosing query's table"
    )]
    OuterRefOutsideSubquery { column: &'static str },

    /// An aggregate call where SQL does not allow one. Only
    /// `HAVING`, an aggregating query's SELECT list, and that
    /// query's `ORDER BY` accept them.
    ///
    /// Filter on an annotation alias instead, which
    /// `QuerySet::filter` routes to `HAVING`, or move the aggregate
    /// into the SELECT list.
    #[error(
        "`Expr::Aggregate(...)` used outside of an aggregate-accepting \
         SQL slot — aggregates may only appear in SELECT projection, \
         HAVING predicate, or ORDER BY of an aggregating query"
    )]
    AggregateOutsideAggregateContext,

    /// A JOIN with an empty `on` predicate, which would leave a hole
    /// after `ON`. [`Self::EmptyCaseWhenCondition`] for joins.
    #[error("JOIN `on` predicate must not be empty")]
    EmptyJoinOnCondition,

    /// The active dialect does not have this aggregate. Only the
    /// statistical ones on SQLite so far, which has none built in;
    /// compute them in your own code instead.
    #[error("aggregate `{aggregate}` is not supported by the `{dialect}` dialect")]
    AggregateNotSupported {
        aggregate: &'static str,
        dialect: &'static str,
    },

    /// A badly nested `AggregateExpr`, such as a `Coalesced` inside
    /// a `Coalesced`. The [`crate::core::aggregates`] builders never
    /// make one, so this catches hand-built IR. `wrapper` names the
    /// shape.
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
    /// filter that matches more than one row, so there is no single
    /// object to return. Tighten the filter or use
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
#[must_use]
pub fn is_mysql_dup_index_error(e: &crate::sql::sqlx::Error) -> bool {
    if let crate::sql::sqlx::Error::Database(db) = e {
        return db
            .try_downcast_ref::<crate::sql::sqlx::mysql::MySqlDatabaseError>()
            .is_some_and(|my| mysql_duplicate_decision(my.number()));
    }
    false
}

/// The decision, over the error *number*, so a test can reach it
/// without a driver error.
///
/// Numbers, not `SQLSTATE`s. This predicate used to match SQLSTATE
/// `42000`, which on MySQL 8 is the catch-all for DDL errors — it also
/// covers 1064 syntax error, 1071 key-too-long, 1072 unknown key column
/// and 1170 TEXT-in-index. `run_ddl_idempotent` therefore returned `Ok`
/// for statements that never ran (#1646). The `|| contains("Duplicate
/// key name")` arm was English-only on top of that; MySQL localises.
#[must_use]
pub(crate) fn mysql_duplicate_decision(number: u16) -> bool {
    matches!(
        number,
        1050  // ER_TABLE_EXISTS_ERROR
        | 1061  // ER_DUP_KEYNAME
        | 1826 // ER_FK_DUP_NAME
    )
}

/// `cfg(not(mysql))` stub — see the documented variant above.
#[cfg(not(feature = "mysql"))]
#[must_use]
pub fn is_mysql_dup_index_error(_e: &crate::sql::sqlx::Error) -> bool {
    false
}

/// `true` when `e` is `PostgreSQL` losing a race to create an object that
/// another session created first (#1458).
///
/// `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` are
/// **not atomic** in `PostgreSQL`. Two sessions can both pass the
/// existence check and both try to insert the catalogue row; the loser
/// gets an error even though the object it asked for now exists. This
/// is documented Postgres behaviour, not a version quirk.
///
/// That is exactly what two processes starting together do — the
/// documented web + worker topology, where both call
/// `DatabaseJobQueue::ensure_table_pool` at boot. Before this predicate
/// the loser's error propagated and killed the process; under a
/// container restart policy the only trace was a restart count.
///
/// Two SQLSTATEs, because the race has two shapes:
///
/// * `23505` — `unique_violation` on `pg_class_relname_nsp_index`,
///   raised by the concurrent `CREATE INDEX`. The constraint name is
///   checked as well as the code, so an ordinary unique violation in
///   application data is never swallowed.
/// * `42P07` — `duplicate_table` / `duplicate_object`, raised by the
///   concurrent `CREATE TABLE`.
///
/// On a build without the `postgres` feature the predicate returns
/// `false` for every error — there is no Postgres driver compiled in,
/// so this path cannot fire.
#[cfg(feature = "postgres")]
#[must_use]
pub fn is_pg_dup_object_error(e: &crate::sql::sqlx::Error) -> bool {
    if let crate::sql::sqlx::Error::Database(db) = e {
        return pg_dup_object_decision(db.code().as_deref(), &db.message());
    }
    false
}

/// The decision, separated from the driver type so it can be tested.
///
/// The predicate above needs a `sqlx::Error::Database`, which cannot be
/// constructed outside the driver — so a test of the *narrowing* had to
/// go through a live query, and the obvious one (`raw_execute_pool` with
/// a duplicate key) never reaches this code at all: `run_ddl_idempotent`
/// is the only caller. The narrowing was therefore untested, and
/// widening `23505` to `true` would have gone unnoticed.
///
/// Which catalogue indexes matter is not obvious, and getting it wrong
/// leaves half the race unfixed:
///
/// * `pg_class_relname_nsp_index` — the relation row. Raised by a racing
///   `CREATE INDEX IF NOT EXISTS`, and by `CREATE TABLE` for the table
///   itself.
/// * `pg_type_typname_nsp_index` — Postgres creates a composite **type**
///   for every table, so a racing `CREATE TABLE IF NOT EXISTS` can lose
///   on the type row instead of the relation row. Omitting this leaves
///   the table half of the race crashing exactly as before.
/// * `pg_namespace_nspname_index` — the schema row, for a racing
///   `CREATE SCHEMA IF NOT EXISTS`. Tenant provisioning in schema mode
///   issues one per tenant.
///
/// Everything else under `23505` stays an error: a bare unique violation
/// is ordinary application data, and swallowing those would hide real
/// bugs — a worse failure than the one being fixed.
// Its only non-test caller is the Postgres error path, so a
// SQLite- or MySQL-only build sees it as dead. Kept compiled there
// anyway: the unit tests that pin these SQLSTATEs must run on every
// build, not only the one that can reach the caller.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn pg_dup_object_decision(code: Option<&str>, message: &str) -> bool {
    match code {
        // duplicate_table / duplicate_object — the non-racing spelling
        // of "it already exists", which is the whole post-condition.
        Some("42P07" | "42710") => true,
        Some("23505") => [
            "pg_class_relname_nsp_index",
            "pg_type_typname_nsp_index",
            "pg_namespace_nspname_index",
        ]
        .iter()
        .any(|idx| message.contains(idx)),
        _ => false,
    }
}

#[cfg(all(test, feature = "postgres"))]
mod pg_dup_object_tests {
    use super::pg_dup_object_decision as decide;

    /// Every shape the concurrent-DDL race actually produces.
    #[test]
    fn the_race_shapes_are_swallowed() {
        assert!(decide(
            Some("42P07"),
            "relation \"rustango_jobs\" already exists"
        ));
        assert!(decide(Some("42710"), "object already exists"));
        assert!(decide(
            Some("23505"),
            "duplicate key value violates unique constraint \
             \"pg_class_relname_nsp_index\""
        ));
        // The table half of the race, which the first version missed.
        assert!(
            decide(
                Some("23505"),
                "duplicate key value violates unique constraint \
                 \"pg_type_typname_nsp_index\""
            ),
            "a racing CREATE TABLE can lose on the composite-type row rather than \
             the relation row; missing it leaves half of #1458 unfixed"
        );
        assert!(decide(
            Some("23505"),
            "duplicate key value violates unique constraint \
             \"pg_namespace_nspname_index\""
        ));
    }

    /// The narrowing. This is the assertion that was missing: flip the
    /// `23505` arm to an unconditional `true` and this fails.
    #[test]
    fn an_ordinary_unique_violation_is_not_swallowed() {
        assert!(
            !decide(
                Some("23505"),
                "duplicate key value violates unique constraint \"users_email_key\""
            ),
            "a unique violation on application data must stay an error — swallowing \
             it would hide real bugs, which is worse than the race being fixed"
        );
        assert!(!decide(Some("23503"), "foreign key violation"));
        assert!(!decide(Some("42P01"), "relation does not exist"));
        assert!(!decide(None, "pg_class_relname_nsp_index"));
    }
}

/// `cfg(not(postgres))` stub — see the documented variant above.
#[cfg(not(feature = "postgres"))]
#[must_use]
pub fn is_pg_dup_object_error(_e: &crate::sql::sqlx::Error) -> bool {
    false
}

#[cfg(test)]
mod mysql_duplicate_tests {
    use super::mysql_duplicate_decision as decide;

    #[test]
    fn the_three_duplicate_numbers_are_swallowed() {
        assert!(decide(1050), "ER_TABLE_EXISTS_ERROR");
        assert!(decide(1061), "ER_DUP_KEYNAME");
        assert!(decide(1826), "ER_FK_DUP_NAME");
    }

    /// The regression this function exists to stop. Every one of these
    /// reports SQLSTATE `42000`, so the old predicate swallowed them
    /// and `run_ddl_idempotent` reported success for an index that was
    /// never created (#1646). Measured on MySQL 8.0.46.
    #[test]
    fn the_rest_of_sqlstate_42000_still_propagates() {
        assert!(!decide(1064), "syntax error");
        assert!(!decide(1071), "key too long");
        assert!(!decide(1072), "unknown column in key");
        assert!(!decide(1170), "BLOB/TEXT in key without a length");
        assert!(!decide(1062), "ER_DUP_ENTRY: the index genuinely failed");
    }
}
