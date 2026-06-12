//! Dialect-agnostic SQL writers.
//!
//! Both `postgres::Postgres` and `mysql::MySql` route their `compile_*`
//! methods through these helpers. Identifier quoting, placeholder
//! shape, NULL-cast emission, and operator translation all dispatch
//! through the [`Dialect`] reference held by [`Sql`], so the writers
//! produce dialect-correct SQL without per-backend forks.
//!
//! Postgres-specific syntax that has no portable equivalent
//! (`ILIKE`, `IS DISTINCT FROM`, JSONB `@>` / `?` / `?|` / `?&`) is
//! gated on [`Dialect::supports_op`] — dialects that return `false`
//! produce a clear [`SqlError::OperatorNotSupportedInDialect`] error
//! instead of mis-translated SQL.

use std::fmt::Write as _;

use crate::core::{
    AggregateExpr, AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery,
    Filter, InsertQuery, ModelSchema, Op, SearchClause, SelectQuery, SqlValue, UpdateQuery,
    WhereExpr,
};

use super::{CompiledStatement, Dialect, SqlError};

/// Buffer-and-params bundle threaded through every writer below. Owns
/// a borrowed [`Dialect`] so each helper can ask for the right
/// identifier quote, parameter placeholder, NULL cast, etc. without
/// branching on backend.
#[allow(clippy::struct_field_names)] // `sql.sql` reads naturally for builder calls
pub(super) struct Sql<'d> {
    pub d: &'d dyn Dialect,
    pub sql: String,
    pub params: Vec<SqlValue>,
    /// Stack of active emission scopes (innermost last). Pushed by
    /// `write_select` / `write_update` / `write_delete` / `write_count`
    /// on entry, popped on exit. Issue #5 reads from this to resolve
    /// `Expr::OuterRef` inside a nested subquery — the OuterRef's
    /// referent is the second-from-top frame (the immediate enclosing
    /// query), and bare `Column` refs implicitly resolve against the
    /// top frame.
    pub scope_stack: Vec<&'static ModelSchema>,
    /// Issue #80. When `Some`, bare `Expr::Column(name)` refs emitted
    /// in the current scope are qualified as `"<alias>"."<name>"`
    /// instead of just `"<name>"`. Set + restored around JOIN `ON`
    /// emission so bare column references inside ON predicates
    /// (e.g., `Expr::Column` from `F()`) resolve to the joined alias.
    /// Unset everywhere else for backward compatibility with top-level
    /// WHERE / UPDATE-SET emission shapes.
    pub current_qualify_alias: Option<&'static str>,
    /// Issue #88 — whether `Expr::Aggregate` is allowed in the current
    /// emission context. Set to `true` only while writing the SELECT
    /// projection / HAVING predicate / ORDER BY of an aggregating
    /// query (where SQL natively accepts an aggregate call); `false`
    /// everywhere else (WHERE / UPDATE-SET / JOIN-ON / GROUP-BY /
    /// RETURNING / non-aggregate-SELECT). Mirrors the runtime gate
    /// `SqlError::OuterRefOutsideSubquery` uses for `Expr::OuterRef`.
    pub aggregate_allowed: bool,
}

impl<'d> Sql<'d> {
    pub(super) fn new(d: &'d dyn Dialect) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::new(),
            scope_stack: Vec::new(),
            current_qualify_alias: None,
            aggregate_allowed: false,
        }
    }

    pub(super) fn with_capacity(d: &'d dyn Dialect, cap: usize) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::with_capacity(cap),
            scope_stack: Vec::new(),
            current_qualify_alias: None,
            aggregate_allowed: false,
        }
    }

    /// Append a quoted identifier using the dialect's quoting rules.
    pub(super) fn write_ident(&mut self, name: &str) {
        self.sql.push_str(&self.d.quote_ident(name));
    }

    /// Push `value` to the param list and emit the dialect's
    /// placeholder for the new slot. For Postgres, also emit `::TYPE`
    /// (from [`Dialect::null_cast`]) when:
    /// - the value is `NULL` — types the otherwise-ambiguous parameter; or
    /// - the value is a [`SqlValue::RangeLiteral`] (#343) — a range column
    ///   value binds as a text literal, and PG won't assignment-cast
    ///   `text` → `int4range`/`daterange`/… in `INSERT`/`UPDATE SET`, so
    ///   the explicit `$N::<rangetype>` cast is required. (Range *operators*
    ///   take their type from the operator context and emit a bare
    ///   placeholder via a separate path, so they're unaffected.)
    pub(super) fn push_param_typed(&mut self, value: SqlValue, cast: Option<&'static str>) {
        // `Raster` (#444) is bound as hex text and cast `$N::raster` —
        // PostGIS `raster` has no binary input function, only a text one.
        let needs_cast = matches!(
            value,
            SqlValue::Null | SqlValue::RangeLiteral(_) | SqlValue::Raster(_)
        );
        self.params.push(value);
        let p = self.d.placeholder(self.params.len());
        self.sql.push_str(&p);
        if needs_cast {
            if let Some(ty) = cast {
                self.sql.push_str("::");
                self.sql.push_str(ty);
            }
        }
    }

    /// Same as [`Self::push_param_typed`] without a cast hint — used
    /// for values whose column type the writer can't determine
    /// (e.g. JSON-key list elements).
    pub(super) fn push_param(&mut self, value: SqlValue) {
        self.push_param_typed(value, None);
    }

    pub(super) fn finish(self) -> CompiledStatement {
        CompiledStatement {
            sql: self.sql,
            params: self.params,
        }
    }
}

/// Per-column NULL-cast lookup gated on the dialect. Postgres needs the
/// hint; `MySQL` doesn't and the writers will get `None`.
pub(super) fn null_cast_for(
    d: &dyn Dialect,
    model: &ModelSchema,
    column: &str,
) -> Option<&'static str> {
    let field = model.field_by_column(column)?;
    d.null_cast(field.ty)
}

// ====================================================================
// SELECT
// ====================================================================

pub(super) fn write_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    b.scope_stack.push(query.model);
    let result = if query.compound.is_empty() {
        write_select_inner(b, query)
    } else {
        write_compound_select(b, query)
    };
    b.scope_stack.pop();
    result
}

/// Emit a set-algebra compound SELECT (issue #25). Layout:
///
/// ```text
/// (SELECT … this query …)
/// UNION [ALL] | INTERSECT | EXCEPT
/// (SELECT … branch_1 …)
/// …
/// ORDER BY …    -- outer order_by, applied AFTER the union/intersect/except
/// LIMIT N
/// OFFSET M
/// FOR UPDATE …  -- outer lock_mode
/// ```
///
/// Each branch is rendered in parens so a per-branch `ORDER BY`/`LIMIT`
/// stays inside the parens (required by PG/SQLite SQL grammar; MySQL
/// is more forgiving but the parens harm nothing). The OUTER
/// SelectQuery's `order_by`/`limit`/`offset`/`lock_mode` get emitted
/// AFTER the last branch closes, so they apply to the whole merged
/// result.
///
/// The OUTER SelectQuery's own `where_clause` / `joins` / `search`
/// apply to ITS branch (the first one in the compound, since the
/// outer is itself a complete `SelectQuery`).
fn write_compound_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    // First branch: the outer query itself. Build a copy with the
    // compound stripped + the outer order_by / limit / offset /
    // lock_mode stripped (those apply to the WHOLE compound, not
    // the head branch).
    //
    // No parens around branches: SQLite's `compound-select-stmt`
    // grammar (`select-core (UNION select-core)*`) explicitly forbids
    // parenthesizing the head select; PG / MySQL accept either shape
    // identically. Per-branch `ORDER BY` / `LIMIT` are already
    // stripped from the outer head (see fields cleared below); branch
    // queries that *do* carry their own ORDER BY / LIMIT are
    // surrounded by `SELECT * FROM (<sub>)` rather than naked parens
    // (handled by the writer for nested subqueries — set ops with
    // per-branch LIMIT remain a stretch goal).
    // #562 — struct-update over SelectQuery::new; the head copies the
    // outer query's WHERE / search / joins but explicitly clears the
    // compound + outer-only fields (order_by / limit / offset /
    // lock_mode / compound) since those apply to the merged result,
    // not the head branch.
    let head = SelectQuery {
        where_clause: query.where_clause.clone(),
        search: query.search.clone(),
        joins: query.joins.clone(),
        ..SelectQuery::new(query.model)
    };
    write_select_inner(b, &head)?;

    for branch in &query.compound {
        b.sql.push(' ');
        b.sql.push_str(branch.op.keyword());
        b.sql.push(' ');
        // Push a scope frame for the branch so any subqueries inside
        // resolve `OuterRef` correctly. write_select handles the
        // push/pop on the outermost call but recursive branches
        // need their own frame.
        b.scope_stack.push(branch.query.model);
        // Branches that carry their own ORDER BY / LIMIT / OFFSET get
        // wrapped in `SELECT * FROM (<branch>)` so those clauses scope
        // to the branch instead of attaching to the outer compound.
        // SQLite's `compound-select-stmt` grammar forbids bare
        // parens around a select-core, so we use a derived-table
        // wrap (portable across PG / MySQL / SQLite). Plain branches
        // with no ORDER BY / LIMIT emit inline — `SELECT … UNION
        // SELECT …` is the shortest valid form on every backend.
        let scoped = !branch.query.order_by.is_empty()
            || branch.query.limit.is_some()
            || branch.query.offset.is_some();
        let r = if branch.query.compound.is_empty() {
            if scoped {
                b.sql.push_str("SELECT * FROM (");
                let r = write_select_inner(b, &branch.query);
                b.sql.push(')');
                r
            } else {
                write_select_inner(b, &branch.query)
            }
        } else {
            // Nested compound — recurse. The outer level handles its
            // own ORDER BY / LIMIT scoping the same way.
            b.sql.push_str("SELECT * FROM (");
            let r = write_compound_select(b, &branch.query);
            b.sql.push(')');
            r
        };
        b.scope_stack.pop();
        r?;
    }

    // Outer ORDER BY / LIMIT / OFFSET apply to the merged result.
    // No qualify (the compound's "table" is the merged resultset, not
    // a join target).
    write_order_limit_offset(b, &query.order_by, query.limit, query.offset, None)?;

    if let Some(lock) = &query.lock_mode {
        write_lock_clause(b, lock);
    }

    Ok(())
}

/// MySQL / SQLite portable fallback for PG's `SELECT DISTINCT ON (cols)`.
/// Wraps the original SELECT in a subquery that adds
/// `ROW_NUMBER() OVER (PARTITION BY cols ORDER BY <user_order>) AS __rn`
/// and selects the outer rows where `__rn = 1`. The user-specified
/// `ORDER BY` drives both the per-partition row pick (inside the
/// `OVER (...)`) and the outer row order. Issue #264 / T1.2.
///
/// Layout:
/// ```text
/// SELECT <orig_cols> FROM (
///   SELECT <orig_cols>,
///          ROW_NUMBER() OVER (PARTITION BY <cols> ORDER BY <order>) AS __rn
///   FROM <table> [JOIN ...] WHERE <where>
/// ) sub
/// WHERE __rn = 1
/// [ORDER BY <order>] [LIMIT N] [OFFSET M]
/// ```
fn write_distinct_on_via_window(
    b: &mut Sql<'_>,
    query: &SelectQuery,
    distinct_cols: &[&'static str],
) -> Result<(), SqlError> {
    // v1 limitation: the window-function rewrite doesn't yet handle
    // joins (`.select_related` / `.join`). Tracked as a follow-up;
    // surface a clear error so users on MySQL/SQLite know the gap
    // rather than seeing wrong-shape SQL.
    if !query.joins.is_empty() {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "DISTINCT ON combined with joins (workaround: pre-collapse \
                 with a subquery, or run on Postgres which supports DISTINCT \
                 ON natively)",
            dialect: b.d.name(),
        });
    }

    // ---------- Outer SELECT — projection columns only (no __rn). ----------
    b.sql.push_str("SELECT ");
    let mut first_col = true;
    if let Some(cols) = query.projection.as_ref() {
        for col in cols {
            if !first_col {
                b.sql.push_str(", ");
            }
            first_col = false;
            b.write_ident(col);
        }
    } else {
        for field in query.model.scalar_fields() {
            if !first_col {
                b.sql.push_str(", ");
            }
            first_col = false;
            b.write_ident(field.column);
        }
    }
    b.sql.push_str(" FROM (");

    // ---------- Inner SELECT — original projection + __rn. ----------
    b.sql.push_str("SELECT ");
    let mut inner_first = true;
    if let Some(cols) = query.projection.as_ref() {
        for col in cols {
            if !inner_first {
                b.sql.push_str(", ");
            }
            inner_first = false;
            b.write_ident(col);
        }
    } else {
        for field in query.model.scalar_fields() {
            if !inner_first {
                b.sql.push_str(", ");
            }
            inner_first = false;
            b.write_ident(field.column);
        }
    }
    // ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...) AS __rn
    b.sql.push_str(", ROW_NUMBER() OVER (PARTITION BY ");
    for (i, col) in distinct_cols.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        b.write_ident(col);
    }
    if !query.order_by.is_empty() {
        b.sql.push_str(" ORDER BY ");
        for (i, item) in query.order_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            match item {
                crate::core::OrderItem::Column { column, desc, .. } => {
                    b.write_ident(column);
                    if *desc {
                        b.sql.push_str(" DESC");
                    }
                }
                crate::core::OrderItem::Expr { expr, desc, .. } => {
                    write_expr(b, expr, None)?;
                    if *desc {
                        b.sql.push_str(" DESC");
                    }
                }
                crate::core::OrderItem::Random => {
                    b.sql.push_str(if b.d.name() == "mysql" {
                        "RAND()"
                    } else {
                        "RANDOM()"
                    });
                }
            }
        }
    }
    b.sql.push_str(") AS __rn FROM ");
    b.write_ident(query.model.table);
    write_where(b, &query.where_clause, Some(query.model))?;

    b.sql.push_str(") sub WHERE sub.__rn = 1");

    // Outer ORDER BY / LIMIT / OFFSET — applied to the survivors.
    write_order_limit_offset(b, &query.order_by, query.limit, query.offset, None)?;

    Ok(())
}

fn write_select_inner(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    // Issue #264 / T1.2 — `.distinct_on(cols)` lowers natively on PG
    // (`SELECT DISTINCT ON (...)`); on MySQL / SQLite the writer
    // wraps the query in a ROW_NUMBER() OVER (PARTITION BY ...)
    // subquery with an outer `WHERE __rn = 1`. The wrap path returns
    // early because it owns the whole SQL emission.
    if let Some(crate::core::DistinctMode::On(cols)) = &query.distinct {
        if b.d.name() != "postgres" {
            return write_distinct_on_via_window(b, query, cols);
        }
    }
    let qualify = !query.joins.is_empty() || !query.subquery_joins.is_empty();

    b.sql.push_str("SELECT ");
    // PG native DISTINCT / DISTINCT ON. `.distinct()` (DistinctMode::All)
    // emits on every dialect uniformly.
    if let Some(distinct) = &query.distinct {
        match distinct {
            crate::core::DistinctMode::All => b.sql.push_str("DISTINCT "),
            crate::core::DistinctMode::On(cols) => {
                // Reached only on PG (non-PG returned early above).
                b.sql.push_str("DISTINCT ON (");
                for (i, col) in cols.iter().enumerate() {
                    if i > 0 {
                        b.sql.push_str(", ");
                    }
                    b.write_ident(col);
                }
                b.sql.push_str(") ");
            }
        }
    }
    let mut first_col = true;
    if let Some(cols) = query.projection.as_ref() {
        // Pure projection (issue #22 — `.values_dict()` /
        // `.values_list()` / `.values_list_flat()`): emit exactly
        // the listed columns in the given order. Validation that
        // each column resolves on the model is the builder's job.
        for col in cols {
            if !first_col {
                b.sql.push_str(", ");
            }
            first_col = false;
            if qualify {
                b.write_ident(query.model.table);
                b.sql.push('.');
            }
            b.write_ident(col);
        }
    } else {
        for field in query.model.scalar_fields() {
            if !first_col {
                b.sql.push_str(", ");
            }
            first_col = false;
            if qualify {
                b.write_ident(query.model.table);
                b.sql.push('.');
            }
            b.write_ident(field.column);
        }
    }
    for join in &query.joins {
        for col in &join.project {
            b.sql.push_str(", ");
            b.write_ident(join.alias);
            b.sql.push('.');
            b.write_ident(col);
            b.sql.push_str(" AS ");
            b.write_ident(&format!("{}__{}", join.alias, col));
        }
    }

    b.sql.push_str(" FROM ");
    b.write_ident(query.model.table);

    for join in &query.joins {
        use crate::core::JoinKind;
        // Reject dialect-incompatible kinds before emitting anything —
        // gives users a clear error rather than a parse failure at the
        // driver. PG supports all four; MySQL has no FULL OUTER JOIN;
        // SQLite has neither RIGHT nor FULL.
        let kind_kw = match (join.kind, b.d.name()) {
            (JoinKind::Inner, _) => "INNER JOIN",
            (JoinKind::Left, _) => "LEFT JOIN",
            (JoinKind::Right, "sqlite") => {
                return Err(SqlError::JoinKindNotSupported {
                    kind: "RIGHT",
                    dialect: b.d.name(),
                });
            }
            (JoinKind::Right, _) => "RIGHT JOIN",
            (JoinKind::Full, "postgres") => "FULL OUTER JOIN",
            (JoinKind::Full, _) => {
                return Err(SqlError::JoinKindNotSupported {
                    kind: "FULL",
                    dialect: b.d.name(),
                });
            }
        };
        // Empty `on` (e.g. `WhereExpr::And(vec![])`) is the legitimate
        // "no WHERE filter" marker at the top of a SELECT/UPDATE, but
        // inside an ON it would emit `ON ` with a literal hole — a
        // parse error on every backend. Mirror of `EmptyCaseWhenCondition`.
        if join.on.is_empty() {
            return Err(SqlError::EmptyJoinOnCondition);
        }
        b.sql.push(' ');
        b.sql.push_str(kind_kw);
        b.sql.push(' ');
        b.write_ident(join.target.table);
        b.sql.push_str(" AS ");
        b.write_ident(join.alias);
        b.sql.push_str(" ON ");
        // The ON predicate's unqualified `Filter` / `ColumnFilter`
        // columns and bare `Expr::Column` refs (via `F()`) resolve
        // to the joined alias for the duration of this write. Cross-
        // references back to the outer (or to another joined alias)
        // use `Expr::AliasedColumn` to escape the default.
        let prior_qualify = b.current_qualify_alias.replace(join.alias);
        let on_result = write_where_expr(b, &join.on, Some(join.alias), Some(join.target));
        b.current_qualify_alias = prior_qualify;
        on_result?;
    }

    // Derived-table joins — `JOIN [LATERAL] (<subquery>) AS alias ON …`
    // (Eloquent joinSub / joinLateral, issue #828). Emitted after the
    // model joins. No projected columns — a filtering / correlation join.
    for sj in &query.subquery_joins {
        use crate::core::JoinKind;
        let kind_kw = match sj.kind {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            // The QuerySet builders only construct Inner / Left; reject
            // anything else rather than emit a derived-table RIGHT/FULL
            // JOIN whose semantics nobody asked for.
            JoinKind::Right => {
                return Err(SqlError::JoinKindNotSupported {
                    kind: "RIGHT (subquery)",
                    dialect: b.d.name(),
                });
            }
            JoinKind::Full => {
                return Err(SqlError::JoinKindNotSupported {
                    kind: "FULL (subquery)",
                    dialect: b.d.name(),
                });
            }
        };
        b.sql.push(' ');
        b.sql.push_str(kind_kw);
        if sj.lateral {
            // PG + MySQL ≥ 8.0.14 only; SQLite has no LATERAL.
            if b.d.name() == "sqlite" {
                return Err(SqlError::LateralJoinNotSupported {
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str(" LATERAL");
        }
        b.sql.push_str(" (");
        // `write_select` pushes/pops the subquery's own scope frame, so a
        // correlated `OuterRef` inside a LATERAL subquery resolves to the
        // enclosing query; explicit `AliasedColumn` refs to the outer
        // table work regardless (LATERAL exposes earlier FROM items).
        write_select(b, &sj.subquery)?;
        b.sql.push_str(") AS ");
        b.write_ident(sj.alias);
        // Empty `on` → `ON true` (the LATERAL shape, where the
        // correlation lives in the subquery's WHERE). Non-empty → the
        // caller's predicate, with unqualified columns resolving to the
        // derived table's alias.
        if sj.on.is_empty() {
            b.sql.push_str(" ON true");
        } else {
            b.sql.push_str(" ON ");
            let prior_qualify = b.current_qualify_alias.replace(sj.alias);
            let on_result = write_where_expr(b, &sj.on, Some(sj.alias), None);
            b.current_qualify_alias = prior_qualify;
            on_result?;
        }
    }

    write_where_with_search(
        b,
        &query.where_clause,
        query.search.as_ref(),
        qualify.then_some(query.model.table),
        Some(query.model),
    )?;

    write_order_limit_offset(
        b,
        &query.order_by,
        query.limit,
        query.offset,
        qualify.then_some(query.model.table),
    )?;

    if let Some(lock) = &query.lock_mode {
        write_lock_clause(b, lock);
    }

    Ok(())
}

/// Emit `FOR UPDATE [NO KEY] [OF t1, t2] [SKIP LOCKED | NOWAIT]` —
/// Django's `select_for_update(...)`. Issue #21.
///
/// Tri-dialect dispatch:
/// - **PG**: full support — `FOR [NO KEY] UPDATE [OF …] [SKIP LOCKED | NOWAIT]`.
/// - **MySQL 8.0.1+**: supports `FOR UPDATE [OF] [NOWAIT | SKIP LOCKED]`.
///   `NO KEY` has no equivalent — the writer falls back to plain
///   `FOR UPDATE` (the stricter lock).
/// - **SQLite**: no row-lock syntax. Transactions hold an implicit
///   write lock for the whole database, so the lock clause is a no-op
///   here. Callers wanting "claim next row" semantics on SQLite need
///   a different strategy (typically a busy-wait loop on the
///   transaction).
///
/// `SKIP LOCKED` wins over `NOWAIT` when both are set — `SKIP LOCKED`
/// is the more permissive option, and both can't appear in the same
/// statement at the database level.
fn write_lock_clause(b: &mut Sql<'_>, lock: &crate::core::LockMode) {
    if b.d.name() == "sqlite" {
        // No row-lock syntax — transaction-scope locks are implicit.
        // Issue #290 / T2.9: warn at write time so callers debugging
        // concurrency bugs against a sqlite-backed test fixture see
        // the no-op. Users who knowingly use the SQLite global writer
        // lock (test apps, single-writer CLIs) can opt out via
        // `LockMode { silent_on_sqlite: true, .. }`.
        if !lock.silent_on_sqlite {
            tracing::warn!(
                target: "rustango::sql::lock",
                skip_locked = lock.skip_locked,
                nowait = lock.nowait,
                "select_for_update modifier dropped — SQLite has no row-level lock syntax. \
                 The transaction's implicit global writer lock applies. \
                 Set `LockMode {{ silent_on_sqlite: true, .. }}` (or call \
                 `.silent_on_sqlite()` on the QuerySet) to suppress this warning."
            );
        }
        return;
    }
    b.sql.push_str(" FOR ");
    if lock.no_key && b.d.name() == "postgres" {
        b.sql.push_str("NO KEY UPDATE");
    } else {
        b.sql.push_str("UPDATE");
    }
    if !lock.of.is_empty() {
        b.sql.push_str(" OF ");
        let mut first = true;
        for t in &lock.of {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.sql.push_str(&b.d.quote_ident(t));
        }
    }
    if lock.skip_locked {
        b.sql.push_str(" SKIP LOCKED");
    } else if lock.nowait {
        b.sql.push_str(" NOWAIT");
    }
}

// ====================================================================
// COUNT
// ====================================================================

pub(super) fn write_count(b: &mut Sql<'_>, query: &CountQuery) -> Result<(), SqlError> {
    b.scope_stack.push(query.model);
    let r = (|| {
        b.sql.push_str("SELECT COUNT(*) FROM ");
        b.write_ident(query.model.table);
        write_where_with_search(
            b,
            &query.where_clause,
            query.search.as_ref(),
            None,
            Some(query.model),
        )?;
        Ok(())
    })();
    b.scope_stack.pop();
    r
}

// ====================================================================
// AGGREGATE
// ====================================================================

pub(super) fn write_aggregate(b: &mut Sql<'_>, query: &AggregateQuery) -> Result<(), SqlError> {
    // Push the model onto the scope stack so any `Expr::Aggregate`
    // (issue #74) emitted inside the HAVING predicate has a model
    // to resolve its COALESCE-default cast against. Mirrors the
    // pattern in `write_select`.
    b.scope_stack.push(query.model);
    let r = write_aggregate_inner(b, query);
    b.scope_stack.pop();
    r
}

fn write_aggregate_inner(b: &mut Sql<'_>, query: &AggregateQuery) -> Result<(), SqlError> {
    b.sql.push_str("SELECT ");

    for (i, col) in query.group_by.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        b.write_ident(col);
    }
    for (i, (alias, expr)) in query.aggregates.iter().enumerate() {
        if !query.group_by.is_empty() || i > 0 {
            b.sql.push_str(", ");
        }
        write_aggregate_expr(b, expr, query.model)?;
        b.sql.push_str(" AS ");
        b.write_ident(alias.as_ref());
    }

    b.sql.push_str(" FROM ");
    b.write_ident(query.model.table);
    write_where(b, &query.where_clause, Some(query.model))?;

    if !query.group_by.is_empty() {
        b.sql.push_str(" GROUP BY ");
        for (i, col) in query.group_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.write_ident(col);
        }
    }

    if let Some(having) = &query.having {
        b.sql.push_str(" HAVING ");
        // Issue #88 — `Expr::Aggregate` lhs of a HAVING predicate is its
        // natural home. Toggle the gate on for the predicate, restore
        // after so nested subqueries don't inherit permission.
        let prev = b.aggregate_allowed;
        b.aggregate_allowed = true;
        let r = write_where_expr(b, having, None, Some(query.model));
        b.aggregate_allowed = prev;
        r?;
    }

    // Issue #88 — aggregating queries are allowed to ORDER BY an
    // aggregate expression (`ORDER BY COUNT(*) DESC`). Plain SELECT's
    // ORDER BY keeps the default `false` and rejects `Expr::Aggregate`.
    let prev = b.aggregate_allowed;
    b.aggregate_allowed = true;
    let r = write_order_limit_offset(b, &query.order_by, query.limit, query.offset, None);
    b.aggregate_allowed = prev;
    r?;

    Ok(())
}

/// What kind of decoder-side cast a base aggregate needs.
/// PG widens `SUM(bigint)` to NUMERIC and `AVG/STDDEV/VAR_*(bigint)`
/// to NUMERIC too; MySQL widens to DECIMAL/DOUBLE. The SqlValue
/// decoder only tries `i64`/`f64`, so the writer post-wraps the
/// aggregate call to a target type per dialect.
#[derive(Debug, Clone, Copy)]
enum AggCast {
    Int,
    Float,
}

/// Return the cast a flat aggregate variant needs, or `None` for
/// aggregates whose native return type the decoder already handles
/// (Count, CountDistinct, Max, Min — return i64 on every dialect).
fn aggregate_cast_kind(expr: &AggregateExpr) -> Option<AggCast> {
    match expr {
        AggregateExpr::Sum(_) => Some(AggCast::Int),
        AggregateExpr::Avg(_)
        | AggregateExpr::StdDev(_)
        | AggregateExpr::StdDevPop(_)
        | AggregateExpr::Variance(_)
        | AggregateExpr::VariancePop(_) => Some(AggCast::Float),
        _ => None,
    }
}

/// Apply the dialect's cast helper to an already-emitted aggregate
/// call (or filtered aggregate call). For PG the form is
/// `<expr>::bigint` / `<expr>::double precision`; MySQL wraps with
/// `CAST(... AS SIGNED/DOUBLE)`; SQLite with `CAST(... AS INTEGER/REAL)`.
fn apply_agg_cast(d: &dyn Dialect, kind: AggCast, inner: &str) -> String {
    match kind {
        AggCast::Int => d.cast_aggregate_to_int(inner),
        AggCast::Float => d.cast_aggregate_to_float(inner),
    }
}

/// Format the bare aggregate-call SQL (no decoder-side cast) for one
/// of the flat variants. Doesn't write to `b.sql` — the caller
/// composes it (optionally inside a FILTER clause, then post-wraps
/// with `apply_agg_cast` for cast-needing kinds).
fn format_bare_aggregate(b: &Sql<'_>, expr: &AggregateExpr) -> Result<String, SqlError> {
    Ok(match expr {
        AggregateExpr::Count(None) => "COUNT(*)".into(),
        AggregateExpr::Count(Some(col)) => format!("COUNT({})", b.d.quote_ident(col)),
        AggregateExpr::CountDistinct(col) => {
            format!("COUNT(DISTINCT {})", b.d.quote_ident(col))
        }
        AggregateExpr::Sum(col) => format!("SUM({})", b.d.quote_ident(col)),
        AggregateExpr::Avg(col) => format!("AVG({})", b.d.quote_ident(col)),
        AggregateExpr::Max(col) => format!("MAX({})", b.d.quote_ident(col)),
        AggregateExpr::Min(col) => format!("MIN({})", b.d.quote_ident(col)),
        AggregateExpr::StdDev(col)
        | AggregateExpr::StdDevPop(col)
        | AggregateExpr::Variance(col)
        | AggregateExpr::VariancePop(col) => {
            if b.d.name() == "sqlite" {
                return Err(SqlError::AggregateNotSupported {
                    aggregate: stddev_variance_name(expr),
                    dialect: b.d.name(),
                });
            }
            format!("{}({})", stddev_variance_name(expr), b.d.quote_ident(col))
        }
        AggregateExpr::Filtered { .. } | AggregateExpr::Coalesced { .. } => {
            // `format_bare_aggregate` is only called with a flat
            // variant — the wrappers are unwrapped first.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "wrapper at format_bare_aggregate site",
            });
        }
        AggregateExpr::Window(_) => {
            // Window-in-aggregate is emitted by the dedicated
            // `write_aggregate_expr` arm, not the bare-aggregate
            // helper. Reaching this point means the writer treated
            // a Window like a flat aggregate — a programmer error.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Window at format_bare_aggregate site",
            });
        }
        AggregateExpr::ArrayAgg { .. }
        | AggregateExpr::StringAgg { .. }
        | AggregateExpr::JsonbAgg { .. } => {
            // PG-specific aggregates are emitted by the dedicated
            // arms in `write_aggregate_expr` (which bind the
            // string_agg delimiter as a parameter). Wrapping them in
            // `Filtered` or `Coalesced` would route here — not yet
            // designed (semantics of `array_agg(x) FILTER (...)` is
            // valid PG but the cast-aware fallback path doesn't
            // model array return types). Reject upfront.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "PG-aggregate at format_bare_aggregate site",
            });
        }
        AggregateExpr::RelatedAggregate(_) => {
            // Correlated relation aggregates (issue #830) are emitted by
            // the dedicated `write_aggregate_expr` arm via `write_expr`.
            // Reaching the bare-aggregate helper means someone wrapped one
            // in `Filtered`/`Coalesced` — not supported.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "RelatedAggregate at format_bare_aggregate site",
            });
        }
    })
}

/// Emit one aggregate expression. Recursive for `Filtered` and
/// `Coalesced` wrappers (issue #6). Dialect dispatch happens here:
/// PG + SQLite (3.30+) emit native `FILTER (WHERE …)`; MySQL rewrites
/// to `<agg>(CASE WHEN … THEN <arg> END)`. `Coalesced` always emits
/// `COALESCE(<inner>, <default>)` regardless of dialect.
fn write_aggregate_expr(
    b: &mut Sql<'_>,
    expr: &AggregateExpr,
    model: &'static ModelSchema,
) -> Result<(), SqlError> {
    match expr {
        AggregateExpr::Coalesced { inner, default } => {
            if matches!(inner.as_ref(), AggregateExpr::Coalesced { .. }) {
                return Err(SqlError::NestedAggregateWrapper {
                    wrapper: "Coalesced",
                });
            }
            b.sql.push_str("COALESCE(");
            write_aggregate_expr(b, inner, model)?;
            b.sql.push_str(", ");
            let cast = aggregate_column(inner).and_then(|c| null_cast_for(b.d, model, c));
            b.push_param_typed(default.clone(), cast);
            b.sql.push(')');
            Ok(())
        }
        AggregateExpr::Filtered { inner, filter } => {
            if matches!(inner.as_ref(), AggregateExpr::Filtered { .. }) {
                return Err(SqlError::NestedAggregateWrapper {
                    wrapper: "Filtered",
                });
            }
            if matches!(inner.as_ref(), AggregateExpr::Coalesced { .. }) {
                return Err(SqlError::NestedAggregateWrapper {
                    wrapper: "Filtered(Coalesced)",
                });
            }
            // `Filtered { Window }` — combining FILTER with a window
            // function isn't dispatched today (PG allows it for
            // aggregate-window funcs, not ranking ones; writer hasn't
            // been taught the per-fn rule). Reject upfront with a
            // consistent message before either the PG-FILTER or the
            // MySQL-CASE-WHEN path renames it to a per-dialect string.
            if matches!(inner.as_ref(), AggregateExpr::Window(_)) {
                return Err(SqlError::NestedAggregateWrapper {
                    wrapper: "Filtered(Window)",
                });
            }
            // MySQL: no FILTER keyword — rewrite via CASE WHEN. The
            // helper applies the dialect's cast helper post-emit for
            // Sum/Avg/StdDev/etc.
            if b.d.name() == "mysql" {
                return write_aggregate_as_case_when(b, inner, filter);
            }
            // PG + SQLite (3.30+): native FILTER. Emit
            // `<bare> FILTER (WHERE <pred>)` into a slice, then
            // post-wrap with the dialect's cast helper (the cast can't
            // sit between `<bare>` and `FILTER` on PG — `SUM(x)::bigint
            // FILTER (...)` is a parse error — so we apply the cast
            // around `(<bare> FILTER (...))`).
            let bare = format_bare_aggregate(b, inner)?;
            let prior = b.sql.len();
            b.sql.push_str(&bare);
            b.sql.push_str(" FILTER (WHERE ");
            write_where_expr(b, filter, None, Some(model))?;
            b.sql.push(')');
            if let Some(kind) = aggregate_cast_kind(inner) {
                let emitted = b.sql[prior..].to_string();
                b.sql.truncate(prior);
                let wrapped = apply_agg_cast(b.d, kind, &format!("({emitted})"));
                b.sql.push_str(&wrapped);
            }
            Ok(())
        }
        AggregateExpr::Window(w) => write_window_expr(b, w),
        AggregateExpr::ArrayAgg { column, distinct } => {
            if b.d.name() != "postgres" {
                return Err(SqlError::AggregateNotSupportedInDialect {
                    aggregate: "array_agg",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("array_agg(");
            if *distinct {
                b.sql.push_str("DISTINCT ");
            }
            b.write_ident(column);
            b.sql.push(')');
            Ok(())
        }
        AggregateExpr::StringAgg {
            column,
            delimiter,
            distinct,
        } => {
            if b.d.name() != "postgres" {
                return Err(SqlError::AggregateNotSupportedInDialect {
                    aggregate: "string_agg",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("string_agg(");
            if *distinct {
                b.sql.push_str("DISTINCT ");
            }
            b.write_ident(column);
            b.sql.push_str(", ");
            b.push_param(crate::core::SqlValue::String(delimiter.clone()));
            b.sql.push(')');
            Ok(())
        }
        AggregateExpr::JsonbAgg { column } => {
            if b.d.name() != "postgres" {
                return Err(SqlError::AggregateNotSupportedInDialect {
                    aggregate: "jsonb_agg",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("jsonb_agg(");
            b.write_ident(column);
            b.sql.push(')');
            Ok(())
        }
        // Issue #830 — correlated relation aggregate. The wrapped `Expr`
        // is an `Expr::AggregateSubquery` over the child table; emit it
        // directly. `write_aggregate` (reached via `write_expr`) pushes
        // the child model's scope frame, and the inner `OuterRef` reads
        // the parent frame this `write_aggregate_inner` already pushed,
        // so the correlation resolves identically on every dialect. Any
        // numeric cast the inner aggregate needs is applied inside the
        // subquery, so no outer cast is required here.
        AggregateExpr::RelatedAggregate(e) => write_expr(b, e, None),
        _ => write_aggregate_kind(b, expr),
    }
}

/// Emit one of the flat aggregate variants (no `Filtered` /
/// `Coalesced` wrappers). Pairs `format_bare_aggregate` with a
/// dialect-aware cast wrap for the kinds whose native return type
/// the decoder can't otherwise unwrap.
fn write_aggregate_kind(b: &mut Sql<'_>, expr: &AggregateExpr) -> Result<(), SqlError> {
    let bare = format_bare_aggregate(b, expr)?;
    let out = match aggregate_cast_kind(expr) {
        Some(kind) => apply_agg_cast(b.d, kind, &bare),
        None => bare,
    };
    b.sql.push_str(&out);
    Ok(())
}

/// MySQL fallback for `<inner> FILTER (WHERE predicate)`: rewrite to
/// `<agg>(CASE WHEN predicate THEN <argument> END)`. The `<argument>`
/// depends on the aggregate kind: `COUNT(*)` becomes
/// `COUNT(CASE WHEN p THEN 1 END)`, everything else becomes
/// `<AGG>(CASE WHEN p THEN <col> END)`.
fn write_aggregate_as_case_when(
    b: &mut Sql<'_>,
    inner: &AggregateExpr,
    filter: &WhereExpr,
) -> Result<(), SqlError> {
    // Pick the aggregate keyword (and CASE-THEN argument) for the
    // emission. COUNT(*) gets `THEN 1`; everything else gets `THEN
    // <col>`. CountDistinct prefixes the CASE with `DISTINCT`.
    let (agg_kw, case_then, distinct_prefix) = match inner {
        AggregateExpr::Count(None) => ("COUNT", None, ""),
        AggregateExpr::Count(Some(col)) => ("COUNT", Some(*col), ""),
        AggregateExpr::CountDistinct(col) => ("COUNT", Some(*col), "DISTINCT "),
        AggregateExpr::Sum(col) => ("SUM", Some(*col), ""),
        AggregateExpr::Avg(col) => ("AVG", Some(*col), ""),
        AggregateExpr::Max(col) => ("MAX", Some(*col), ""),
        AggregateExpr::Min(col) => ("MIN", Some(*col), ""),
        AggregateExpr::StdDev(col)
        | AggregateExpr::StdDevPop(col)
        | AggregateExpr::Variance(col)
        | AggregateExpr::VariancePop(col) => (stddev_variance_name(inner), Some(*col), ""),
        AggregateExpr::Filtered { .. } | AggregateExpr::Coalesced { .. } => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "wrapper inside Filtered fallback",
            });
        }
        AggregateExpr::Window(_) => {
            // `<window_fn>(...) OVER (...) FILTER (WHERE ...)` is
            // valid SQL on some backends (PG since 9.4 for aggregate
            // window functions; not for ranking ones), but mixing
            // `Filtered` + `Window` requires careful per-function
            // dispatch we haven't designed yet. Reject for v1.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Filtered(Window)",
            });
        }
        AggregateExpr::ArrayAgg { .. }
        | AggregateExpr::StringAgg { .. }
        | AggregateExpr::JsonbAgg { .. } => {
            // PG-specific aggregates inside Filtered{} aren't supported
            // — MySQL has no equivalent native syntax (GROUP_CONCAT
            // semantics differ), and the CASE-WHEN fallback would lose
            // the PG-only nature. Reject upfront.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Filtered(PG-aggregate)",
            });
        }
        AggregateExpr::RelatedAggregate(_) => {
            // Correlated relation aggregates (issue #830) can't be
            // rewritten through the MySQL CASE-WHEN FILTER fallback.
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Filtered(RelatedAggregate)",
            });
        }
    };
    let prior = b.sql.len();
    b.sql.push_str(agg_kw);
    b.sql.push('(');
    b.sql.push_str(distinct_prefix);
    b.sql.push_str("CASE WHEN ");
    write_where_expr(b, filter, None, None)?;
    b.sql.push_str(" THEN ");
    match case_then {
        Some(col) => b.write_ident(col),
        None => b.sql.push('1'),
    }
    b.sql.push_str(" END)");
    // Apply the dialect's cast wrap for Sum/Avg/StdDev/Variance —
    // same shape the flat path uses.
    if let Some(kind) = aggregate_cast_kind(inner) {
        let emitted = b.sql[prior..].to_string();
        b.sql.truncate(prior);
        let wrapped = apply_agg_cast(b.d, kind, &emitted);
        b.sql.push_str(&wrapped);
    }
    Ok(())
}

/// Look up the column referenced by a flat aggregate variant (or by
/// the inner of a wrapper). Returns `None` for `Count(None)` (no column).
fn aggregate_column(expr: &AggregateExpr) -> Option<&'static str> {
    match expr {
        AggregateExpr::Count(c) => *c,
        AggregateExpr::CountDistinct(c)
        | AggregateExpr::Sum(c)
        | AggregateExpr::Avg(c)
        | AggregateExpr::Max(c)
        | AggregateExpr::Min(c)
        | AggregateExpr::StdDev(c)
        | AggregateExpr::StdDevPop(c)
        | AggregateExpr::Variance(c)
        | AggregateExpr::VariancePop(c) => Some(c),
        AggregateExpr::ArrayAgg { column, .. }
        | AggregateExpr::StringAgg { column, .. }
        | AggregateExpr::JsonbAgg { column } => Some(column),
        AggregateExpr::Filtered { inner, .. } | AggregateExpr::Coalesced { inner, .. } => {
            aggregate_column(inner)
        }
        AggregateExpr::Window(w) => w.args.iter().find_map(|a| match a {
            crate::core::Expr::Column(c) => Some(*c),
            _ => None,
        }),
        // The aggregated column lives on the child table inside the
        // correlated subquery, not on the outer model — there's no outer
        // column to surface for null-cast resolution. Issue #830.
        AggregateExpr::RelatedAggregate(_) => None,
    }
}

fn stddev_variance_name(expr: &AggregateExpr) -> &'static str {
    match expr {
        AggregateExpr::StdDev(_) => "STDDEV_SAMP",
        AggregateExpr::StdDevPop(_) => "STDDEV_POP",
        AggregateExpr::Variance(_) => "VAR_SAMP",
        AggregateExpr::VariancePop(_) => "VAR_POP",
        _ => "(unknown)", // not reachable from public paths
    }
}

// ====================================================================
// INSERT
// ====================================================================

pub(super) fn write_insert(b: &mut Sql<'_>, query: &InsertQuery) -> Result<(), SqlError> {
    if query.columns.is_empty() && query.returning.is_empty() {
        return Err(SqlError::EmptyInsert);
    }
    if query.columns.len() != query.values.len() {
        return Err(SqlError::InsertShapeMismatch {
            columns: query.columns.len(),
            values: query.values.len(),
        });
    }

    b.sql.push_str("INSERT INTO ");
    b.write_ident(query.model.table);

    if query.columns.is_empty() {
        b.sql.push_str(" DEFAULT VALUES");
    } else {
        b.sql.push_str(" (");
        let mut first = true;
        for col in &query.columns {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.write_ident(col);
        }
        b.sql.push_str(") VALUES (");
        let mut first = true;
        for (col, value) in query.columns.iter().zip(&query.values) {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            let cast = null_cast_for(b.d, query.model, col);
            b.push_param_typed(value.clone(), cast);
        }
        b.sql.push(')');
    }

    if let Some(conflict) = &query.on_conflict {
        b.d.write_conflict_clause(&mut b.sql, conflict)?;
    }

    write_returning(b, &query.returning)?;
    Ok(())
}

// ====================================================================
// BULK INSERT
// ====================================================================

pub(super) fn write_bulk_insert(b: &mut Sql<'_>, query: &BulkInsertQuery) -> Result<(), SqlError> {
    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.columns.is_empty() && query.returning.is_empty() {
        return Err(SqlError::EmptyInsert);
    }
    for row in &query.rows {
        if row.len() != query.columns.len() {
            return Err(SqlError::InsertShapeMismatch {
                columns: query.columns.len(),
                values: row.len(),
            });
        }
    }

    b.sql.push_str("INSERT INTO ");
    b.write_ident(query.model.table);

    if query.columns.is_empty() {
        let pk = query
            .returning
            .first()
            .copied()
            .ok_or(SqlError::EmptyInsert)?;
        b.sql.push_str(" (");
        b.write_ident(pk);
        b.sql.push_str(") VALUES ");
        let mut first_row = true;
        for _ in &query.rows {
            if !first_row {
                b.sql.push_str(", ");
            }
            first_row = false;
            b.sql.push_str("(DEFAULT)");
        }
    } else {
        b.sql.push_str(" (");
        let mut first = true;
        for col in &query.columns {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.write_ident(col);
        }
        b.sql.push_str(") VALUES ");

        let mut first_row = true;
        for row in &query.rows {
            if !first_row {
                b.sql.push_str(", ");
            }
            first_row = false;
            b.sql.push('(');
            let mut first_v = true;
            for (col, value) in query.columns.iter().zip(row) {
                if !first_v {
                    b.sql.push_str(", ");
                }
                first_v = false;
                let cast = null_cast_for(b.d, query.model, col);
                b.push_param_typed(value.clone(), cast);
            }
            b.sql.push(')');
        }
    }

    if let Some(conflict) = &query.on_conflict {
        b.d.write_conflict_clause(&mut b.sql, conflict)?;
    }

    write_returning(b, &query.returning)?;
    Ok(())
}

// ====================================================================
// UPDATE
// ====================================================================

pub(super) fn write_update(b: &mut Sql<'_>, query: &UpdateQuery) -> Result<(), SqlError> {
    if query.set.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    b.scope_stack.push(query.model);
    let r = (|| {
        b.sql.push_str("UPDATE ");
        b.write_ident(query.model.table);
        b.sql.push_str(" SET ");

        let mut first = true;
        for assignment in &query.set {
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            b.write_ident(assignment.column);
            b.sql.push_str(" = ");
            let cast = null_cast_for(b.d, query.model, assignment.column);
            write_expr(b, &assignment.value, cast)?;
        }

        write_where(b, &query.where_clause, Some(query.model))?;
        Ok(())
    })();
    b.scope_stack.pop();
    r
}

/// Render a [`crate::core::Expr`] — the recursive RHS form that
/// powers `F()` column references and arithmetic. Literal `Expr`s
/// route through [`Sql::push_param_typed`] so cast hinting (PG
/// `::TEXT` on NULL) still fires. `Column` writes a quoted ident;
/// `BinOp` emits `(<left> <op> <right>)` with both sides recursed.
fn write_expr(
    b: &mut Sql<'_>,
    expr: &crate::core::Expr,
    cast: Option<&'static str>,
) -> Result<(), SqlError> {
    use crate::core::{BinOp as BO, Expr};
    match expr {
        Expr::Literal(v) => {
            b.push_param_typed(v.clone(), cast);
            Ok(())
        }
        Expr::Column(name) => {
            // When a JOIN-ON emission context has set
            // `current_qualify_alias`, bare `Column` refs from `F()` /
            // arithmetic / `eq_expr` rhs slots qualify to that alias —
            // emits `"<alias>"."<col>"` instead of bare `"<col>"`.
            // Outside that context (top-level WHERE, UPDATE-SET, etc.)
            // the original unqualified emission is preserved for
            // backward compatibility.
            if let Some(alias) = b.current_qualify_alias {
                let qualified = format!("{}.{}", b.d.quote_ident(alias), b.d.quote_ident(name),);
                b.sql.push_str(&qualified);
            } else {
                b.write_ident(name);
            }
            Ok(())
        }
        Expr::BinOp { left, op, right } => {
            // SQLite doesn't have a bitwise XOR operator; surface a
            // clear error rather than emitting silently-wrong SQL.
            if matches!(op, BO::BitXor) && b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "BitXor",
                    dialect: b.d.name(),
                });
            }
            // pgvector distance operators are Postgres-only (#824).
            if matches!(op, BO::L2Distance | BO::CosineDistance | BO::InnerProduct)
                && b.d.name() != "postgres"
            {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: match op {
                        BO::L2Distance => "<-> (pgvector L2 distance)",
                        BO::CosineDistance => "<=> (pgvector cosine distance)",
                        _ => "<#> (pgvector inner product)",
                    },
                    dialect: b.d.name(),
                });
            }
            b.sql.push('(');
            // Nested casts only apply at the literal leaf — clear here
            // so an outer NULL cast doesn't bleed into the operand.
            write_expr(b, left, None)?;
            b.sql.push(' ');
            b.sql.push_str(match op {
                BO::Add => "+",
                BO::Sub => "-",
                BO::Mul => "*",
                BO::Div => "/",
                BO::Mod => "%",
                BO::BitAnd => "&",
                BO::BitOr => "|",
                // PG spells XOR `#`; MySQL uses `^`. SQLite already
                // bounced above.
                BO::BitXor => {
                    if b.d.name() == "postgres" {
                        "#"
                    } else {
                        "^"
                    }
                }
                BO::BitShl => "<<",
                BO::BitShr => ">>",
                // pgvector distance operators (#824); PG-gated above.
                BO::L2Distance => "<->",
                BO::CosineDistance => "<=>",
                BO::InnerProduct => "<#>",
            });
            b.sql.push(' ');
            write_expr(b, right, None)?;
            b.sql.push(')');
            Ok(())
        }
        Expr::Function { kind, args } => write_function(b, *kind, args),
        Expr::Cast { expr, ty } => {
            // CAST(<expr> AS <ty>) — identical syntax on PG/MySQL/SQLite;
            // only the type token varies (handled by `cast_type`).
            let ty_token =
                b.d.cast_type(*ty)
                    .ok_or(SqlError::OpNotSupportedInDialect {
                        op: "CAST: dialect cannot map this FieldType",
                        dialect: b.d.name(),
                    })?;
            b.sql.push_str("CAST(");
            write_expr(b, expr, None)?;
            b.sql.push_str(" AS ");
            b.sql.push_str(ty_token);
            b.sql.push(')');
            Ok(())
        }
        Expr::Case { branches, default } => write_case(b, branches, default.as_deref()),
        Expr::Subquery(inner) => {
            // `(SELECT … FROM …)` — write_select pushes/pops its own
            // scope frame so any nested OuterRef inside `inner` looks
            // up the right enclosing model.
            //
            // Issue #88 — subqueries today carry a plain `SelectQuery`
            // (not an aggregating one), so any `Expr::Aggregate` inside
            // the inner SELECT's projection / WHERE / ORDER BY would
            // be in a non-aggregate context. Force the gate off across
            // the boundary so the outer's permission doesn't leak in.
            b.sql.push('(');
            let prev = b.aggregate_allowed;
            b.aggregate_allowed = false;
            let r = write_select(b, inner);
            b.aggregate_allowed = prev;
            r?;
            b.sql.push(')');
            Ok(())
        }
        Expr::AggregateSubquery(inner) => {
            // `(SELECT COUNT(*) FROM … WHERE … = OuterRef)` — a
            // correlated scalar-aggregate subquery (issue #830 slice 3).
            // `write_aggregate` pushes the child model's scope frame so
            // the inner WHERE's `OuterRef` resolves to the enclosing
            // parent query. The aggregate call sits in the subquery's own
            // projection (emitted directly by `write_aggregate_inner`,
            // not the gated `Expr::Aggregate` path), so reset the
            // aggregate gate across the boundary exactly like
            // `Expr::Subquery` does — the outer's HAVING permission must
            // not leak in.
            b.sql.push('(');
            let prev = b.aggregate_allowed;
            b.aggregate_allowed = false;
            let r = write_aggregate(b, inner);
            b.aggregate_allowed = prev;
            r?;
            b.sql.push(')');
            Ok(())
        }
        Expr::RelAggregate {
            kind,
            column,
            table,
            correlation,
        } => {
            use crate::core::RelAggKind;
            // `(SELECT <kind>(<col>|*) FROM <table> WHERE <correlation>)`
            // over a raw relation table (M2M junction/target or GFK
            // child, issue #830). SUM/AVG get the dialect's decoder-side
            // cast so the scalar decodes as i64/f64 just like the flat
            // aggregate path; MAX/MIN/COUNT need none.
            let col_sql = |c: Option<&'static str>, k: &'static str| -> Result<String, SqlError> {
                c.map(|c| b.d.quote_ident(c))
                    .ok_or(SqlError::RelAggregateMissingColumn { kind: k })
            };
            let agg = match kind {
                RelAggKind::Count => "COUNT(*)".to_string(),
                RelAggKind::Sum => {
                    b.d.cast_aggregate_to_int(&format!("SUM({})", col_sql(*column, "SUM")?))
                }
                RelAggKind::Avg => {
                    b.d.cast_aggregate_to_float(&format!("AVG({})", col_sql(*column, "AVG")?))
                }
                RelAggKind::Max => format!("MAX({})", col_sql(*column, "MAX")?),
                RelAggKind::Min => format!("MIN({})", col_sql(*column, "MIN")?),
            };
            b.sql.push_str("(SELECT ");
            b.sql.push_str(&agg);
            b.sql.push_str(" FROM ");
            b.write_ident(table);
            b.sql.push_str(" WHERE ");
            write_rel_correlation(b, table, correlation)?;
            b.sql.push(')');
            Ok(())
        }
        Expr::OuterRef(col) => {
            // Resolve against the immediate enclosing scope. The top
            // frame is the *current* query (the subquery emitting
            // this OuterRef); the next-most-recent frame is the outer
            // the user is referring to.
            let len = b.scope_stack.len();
            if len < 2 {
                return Err(SqlError::OuterRefOutsideSubquery { column: col });
            }
            let outer = b.scope_stack[len - 2];
            let qualified = format!("{}.{}", b.d.quote_ident(outer.table), b.d.quote_ident(col),);
            b.sql.push_str(&qualified);
            Ok(())
        }
        Expr::AliasedColumn { alias, column } => {
            // Explicit `<alias>.<col>` — used in JOIN ON predicates and
            // anywhere a column reference needs a table prefix that
            // isn't the current scope. No stack lookup, no validation
            // beyond what the user passed in.
            let qualified = format!("{}.{}", b.d.quote_ident(alias), b.d.quote_ident(column),);
            b.sql.push_str(&qualified);
            Ok(())
        }
        Expr::Window(w) => write_window_expr(b, w),
        Expr::Aggregate(agg) => {
            // Issue #88 — refuse to emit an aggregate call into a SQL
            // slot that doesn't accept one. Every backend rejects
            // aggregates in WHERE / UPDATE-SET / JOIN-ON / GROUP-BY /
            // RETURNING / non-aggregate SELECT lists; only HAVING,
            // an aggregating SELECT's projection, and that query's
            // ORDER BY toggle the gate on (see `write_aggregate_inner`).
            if !b.aggregate_allowed {
                return Err(SqlError::AggregateOutsideAggregateContext);
            }
            // Issue #74 — lift the aggregate expression into the
            // current writer scope (e.g., a HAVING predicate's lhs).
            // The Aggregate writer handles dialect casting + filter
            // wrapping internally; we just need an enclosing model
            // context for the COALESCE cast lookup. Reach for the
            // current scope frame; if none, fall back to a dummy.
            // Scope-stack is set in `write_select` for SELECT/HAVING
            // emission, so by the time HAVING is being walked the
            // top frame is the right model.
            let model = b
                .scope_stack
                .last()
                .copied()
                .expect("Expr::Aggregate emitted outside any scope frame");
            write_aggregate_expr(b, agg, model)
        }
        Expr::JsonPath {
            source,
            path,
            as_text,
        } => write_json_path(b, source, path, *as_text),
    }
}

/// Emit a JSON-path traversal — issue #296 / T2.3.
///
/// **PG**: chained `->` operators, with the final hop using `->>`
/// when `as_text` (which returns the unwrapped scalar text instead of
/// JSON). Each key/index is bound as a parameter so user-supplied
/// strings can't break out of the SQL.
///
/// **MySQL / SQLite**: a single `JSON_EXTRACT` / `json_extract` call
/// with a JSON-pointer-style path string (`$.k1.k2[0]`). The path is
/// inlined into the SQL because both backends require a literal
/// string for the path argument. To keep the inlined path safe,
/// keys are validated against a strict charset (`A-Za-z0-9_`) at
/// write time — anything outside emits `OpNotSupportedInDialect`.
fn write_json_path(
    b: &mut Sql<'_>,
    source: &crate::core::Expr,
    path: &[crate::core::JsonPathStep],
    as_text: bool,
) -> Result<(), SqlError> {
    use crate::core::{JsonPathStep, SqlValue};
    if path.is_empty() {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "JsonPath requires at least one path step",
            dialect: b.d.name(),
        });
    }
    // Key safety: ASCII alphanumeric + `_` for MySQL / SQLite inlining.
    // PG binds keys as parameters so the validation isn't strictly
    // necessary there, but apply the same rule for consistency.
    let key_safe =
        |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    for step in path {
        if let JsonPathStep::Key(k) = step {
            if !key_safe(k) {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "JsonPath key contains characters outside the safe set [A-Za-z0-9_]",
                    dialect: b.d.name(),
                });
            }
        }
    }
    let dialect = b.d.name();
    if dialect == "postgres" {
        // Chained `->` … `->>` form. Negative array indices are PG-
        // native (count-from-end).
        write_expr(b, source, None)?;
        for (i, step) in path.iter().enumerate() {
            let is_last = i + 1 == path.len();
            let op = if is_last && as_text { " ->> " } else { " -> " };
            b.sql.push_str(op);
            match step {
                JsonPathStep::Key(k) => {
                    b.params.push(SqlValue::String(k.clone()));
                    let p = b.d.placeholder(b.params.len());
                    b.sql.push_str(&p);
                }
                JsonPathStep::Index(n) => {
                    // PG's `->` accepts integer literals inline; binding
                    // as a parameter forces a `text` cast (PG infers
                    // `text` from the unknown-typed `$N`) and the
                    // operator overload resolution then fails. Inline
                    // the integer.
                    b.sql.push_str(&n.to_string());
                }
            }
        }
        return Ok(());
    }
    // MySQL + SQLite — build `$.<k>.<k>[<n>]` path string and call
    // the dialect's JSON_EXTRACT.
    let mut json_path = String::from("$");
    for step in path {
        match step {
            JsonPathStep::Key(k) => {
                json_path.push('.');
                json_path.push_str(k);
            }
            JsonPathStep::Index(n) => {
                if *n < 0 {
                    return Err(SqlError::OpNotSupportedInDialect {
                        op: "JsonPath negative indices are PG-only (MySQL/SQLite require non-negative)",
                        dialect,
                    });
                }
                json_path.push('[');
                json_path.push_str(&n.to_string());
                json_path.push(']');
            }
        }
    }
    if dialect == "mysql" {
        if as_text {
            b.sql.push_str("JSON_UNQUOTE(JSON_EXTRACT(");
            write_expr(b, source, None)?;
            b.sql.push_str(", '");
            b.sql.push_str(&json_path);
            b.sql.push_str("'))");
        } else {
            b.sql.push_str("JSON_EXTRACT(");
            write_expr(b, source, None)?;
            b.sql.push_str(", '");
            b.sql.push_str(&json_path);
            b.sql.push_str("')");
        }
        return Ok(());
    }
    // SQLite — json_extract returns scalars unquoted already, so
    // `as_text` is a no-op on this backend.
    let _ = as_text;
    b.sql.push_str("json_extract(");
    write_expr(b, source, None)?;
    b.sql.push_str(", '");
    b.sql.push_str(&json_path);
    b.sql.push_str("')");
    Ok(())
}

/// Emit a window expression — `<fn>(args) OVER (PARTITION BY … ORDER
/// BY … [frame])`. Tri-dialect uniform: PG ≥ 9.0, MySQL ≥ 8.0, and
/// SQLite ≥ 3.25 all accept this SQL-standard form verbatim.
fn write_window_expr(b: &mut Sql<'_>, w: &crate::core::WindowExpr) -> Result<(), SqlError> {
    use crate::core::{Expr, WindowFn};
    let fn_name = match w.kind {
        WindowFn::RowNumber => "ROW_NUMBER",
        WindowFn::Rank => "RANK",
        WindowFn::DenseRank => "DENSE_RANK",
        WindowFn::Ntile => "NTILE",
        WindowFn::Lag => "LAG",
        WindowFn::Lead => "LEAD",
        WindowFn::FirstValue => "FIRST_VALUE",
        WindowFn::LastValue => "LAST_VALUE",
    };
    b.sql.push_str(fn_name);
    b.sql.push('(');
    // PG's LAG/LEAD/NTILE require `offset`/`buckets` as `integer`, not
    // `bigint`. Binding `i64` as a parameter (which PG types as
    // `bigint`) makes function lookup fail with
    // `function lag(bigint, bigint, bigint) does not exist`. The
    // offset/bucket count is a compile-time constant in user code
    // anyway — emit it inline as a SQL integer literal. The
    // value-arg (Lag's first, Lead's first) and the default-arg
    // (Lag's third, Lead's third) bind normally via params.
    let integer_arg_index: Option<usize> = match w.kind {
        WindowFn::Lag | WindowFn::Lead => Some(1),
        WindowFn::Ntile => Some(0),
        _ => None,
    };
    for (i, arg) in w.args.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        if integer_arg_index == Some(i) {
            if let Expr::Literal(SqlValue::I64(n)) = arg {
                use std::fmt::Write as _;
                let _ = write!(b.sql, "{n}");
                continue;
            }
        }
        write_expr(b, arg, None)?;
    }
    b.sql.push_str(") OVER (");
    let mut first_clause = true;
    if !w.partition_by.is_empty() {
        b.sql.push_str("PARTITION BY ");
        for (i, col) in w.partition_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.write_ident(col);
        }
        first_clause = false;
    }
    if !w.order_by.is_empty() {
        if !first_clause {
            b.sql.push(' ');
        }
        b.sql.push_str("ORDER BY ");
        for (i, o) in w.order_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.write_ident(o.column);
            if o.desc {
                b.sql.push_str(" DESC");
            }
        }
        first_clause = false;
    }
    if let Some(frame) = &w.frame {
        if !first_clause {
            b.sql.push(' ');
        }
        write_window_frame(b, frame);
    }
    b.sql.push(')');
    Ok(())
}

fn write_window_frame(b: &mut Sql<'_>, frame: &crate::core::WindowFrame) {
    use crate::core::{FrameBoundary, FrameKind};
    b.sql.push_str(match frame.kind {
        FrameKind::Rows => "ROWS",
        FrameKind::Range => "RANGE",
    });
    b.sql.push(' ');
    if frame.end.is_some() {
        b.sql.push_str("BETWEEN ");
    }
    write_frame_boundary(b, frame.start);
    if let Some(end) = frame.end {
        b.sql.push_str(" AND ");
        write_frame_boundary(b, end);
    }

    fn write_frame_boundary(b: &mut Sql<'_>, bound: FrameBoundary) {
        match bound {
            FrameBoundary::UnboundedPreceding => b.sql.push_str("UNBOUNDED PRECEDING"),
            FrameBoundary::Preceding(n) => {
                use std::fmt::Write as _;
                let _ = write!(b.sql, "{n} PRECEDING");
            }
            FrameBoundary::CurrentRow => b.sql.push_str("CURRENT ROW"),
            FrameBoundary::Following(n) => {
                use std::fmt::Write as _;
                let _ = write!(b.sql, "{n} FOLLOWING");
            }
            FrameBoundary::UnboundedFollowing => b.sql.push_str("UNBOUNDED FOLLOWING"),
        }
    }
}

/// Emit `CASE WHEN c1 THEN t1 [WHEN c2 THEN t2 …] [ELSE d] END`.
/// Standard SQL-92, identical across PG / MySQL / SQLite — no
/// dialect dispatch needed.
///
/// Rejects empty `branches` at emit time: a `CASE` with no `WHEN`
/// clauses is a parse error on every backend, so surfacing it as
/// `SqlError::EmptyCaseBranches` at compile gives a clearer message
/// than letting the database complain.
fn write_case(
    b: &mut Sql<'_>,
    branches: &[crate::core::CaseBranch],
    default: Option<&crate::core::Expr>,
) -> Result<(), SqlError> {
    if branches.is_empty() {
        return Err(SqlError::EmptyCaseBranches);
    }
    b.sql.push_str("CASE");
    for branch in branches {
        // Empty `And(vec![])` is the legal "no WHERE filter" marker
        // at the top of an UPDATE/DELETE, but inside a WHEN it would
        // produce `WHEN  THEN …` with a hole. Reject it before the
        // database does.
        if branch.condition.is_empty() {
            return Err(SqlError::EmptyCaseWhenCondition);
        }
        b.sql.push_str(" WHEN ");
        // `write_where_expr` handles And/Or/Not nesting + parameter
        // binding. We pass no qualify-with / no model — `Case`
        // conditions are emitted in the context of the surrounding
        // statement which already knows the table name.
        write_where_expr(b, &branch.condition, None, None)?;
        b.sql.push_str(" THEN ");
        write_expr(b, &branch.then, None)?;
    }
    if let Some(d) = default {
        b.sql.push_str(" ELSE ");
        write_expr(b, d, None)?;
    }
    b.sql.push_str(" END");
    Ok(())
}

/// Emit a scalar function call. Most variants are straight `FN(args…)`
/// across all three dialects; the divergent ones (`Concat` on SQLite,
/// `Greatest`/`Least` on SQLite, `Substr` PG `FROM…FOR…` form) get
/// special-cased.
#[allow(clippy::too_many_lines)] // Per-fn arms are inherently linear.
fn write_function(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    match kind {
        // -------- text: simple FN(arg) — unary, arity-checked --------
        F::Lower => write_call_unary(b, "LOWER", args),
        F::Upper => write_call_unary(b, "UPPER", args),
        F::Length => write_call_unary(b, "LENGTH", args),
        F::Trim => write_call_unary(b, "TRIM", args),
        F::LTrim => write_call_unary(b, "LTRIM", args),
        F::RTrim => write_call_unary(b, "RTRIM", args),

        // -------- text: 3-ary FN(s, from, to) --------
        F::Replace => {
            if args.len() != 3 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "REPLACE",
                    expected: "3",
                    got: args.len(),
                });
            }
            write_call(b, "REPLACE", args)
        }

        // -------- CONCAT: PG/MySQL native, SQLite `||` --------
        F::Concat => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "CONCAT",
                    expected: ">= 1",
                    got: 0,
                });
            }
            if b.d.name() == "sqlite" {
                // `||` chain. Parenthesize so precedence is unambiguous
                // when wrapped in another expression.
                b.sql.push('(');
                let mut first = true;
                for a in args {
                    if !first {
                        b.sql.push_str(" || ");
                    }
                    first = false;
                    write_expr(b, a, None)?;
                }
                b.sql.push(')');
                Ok(())
            } else {
                write_call(b, "CONCAT", args)
            }
        }

        // -------- SUBSTR: PG uses `FROM…FOR…`, MySQL/SQLite use commas --------
        F::Substr => {
            if args.len() != 3 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "SUBSTRING",
                    expected: "3",
                    got: args.len(),
                });
            }
            if b.d.name() == "postgres" {
                b.sql.push_str("SUBSTRING(");
                write_expr(b, &args[0], None)?;
                b.sql.push_str(" FROM ");
                write_expr(b, &args[1], None)?;
                b.sql.push_str(" FOR ");
                write_expr(b, &args[2], None)?;
                b.sql.push(')');
                Ok(())
            } else {
                // MySQL spells it SUBSTRING; SQLite spells it substr.
                // Both accept the comma form.
                let name = if b.d.name() == "mysql" {
                    "SUBSTRING"
                } else {
                    "SUBSTR"
                };
                write_call(b, name, args)
            }
        }

        // -------- math: simple unary, arity-checked --------
        F::Abs => write_call_unary(b, "ABS", args),
        F::Floor => write_call_unary(b, "FLOOR", args),
        F::Ceil => {
            // MySQL accepts both `CEIL` and `CEILING`; PG / SQLite use
            // `CEIL` (SQLite 3.35+). Emit `CEIL` everywhere for the
            // narrowest portable token.
            write_call_unary(b, "CEIL", args)
        }
        F::Round => {
            // 1- or 2-ary. The shape is identical across PG / MySQL /
            // SQLite at the SQL surface; precision-arg type semantics
            // diverge (PG `numeric` only), documented at the builder.
            if args.is_empty() || args.len() > 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "ROUND",
                    expected: "1 or 2",
                    got: args.len(),
                });
            }
            write_call(b, "ROUND", args)
        }

        // -------- comparison / NULL --------
        F::Coalesce => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "COALESCE",
                    expected: ">= 1",
                    got: 0,
                });
            }
            write_call(b, "COALESCE", args)
        }
        F::Greatest => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "GREATEST",
                    expected: ">= 1",
                    got: 0,
                });
            }
            // SQLite has no GREATEST keyword. Its scalar `MAX(a, b, …)`
            // form requires 2+ args; with 1 arg SQLite parses `MAX(x)` as
            // the AGGREGATE form, which is a misuse-of-aggregate error
            // inside `UPDATE SET` and the wrong semantic in `WHERE`.
            // Surface a clear error rather than emit silently-wrong SQL.
            if b.d.name() == "sqlite" && args.len() == 1 {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "GREATEST with 1 argument (SQLite collides with the aggregate MAX)",
                    dialect: "sqlite",
                });
            }
            let name = if b.d.name() == "sqlite" {
                "MAX"
            } else {
                "GREATEST"
            };
            write_call(b, name, args)
        }
        F::Least => {
            if args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "LEAST",
                    expected: ">= 1",
                    got: 0,
                });
            }
            // See `Greatest` above for the SQLite 1-arg rationale.
            if b.d.name() == "sqlite" && args.len() == 1 {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "LEAST with 1 argument (SQLite collides with the aggregate MIN)",
                    dialect: "sqlite",
                });
            }
            let name = if b.d.name() == "sqlite" {
                "MIN"
            } else {
                "LEAST"
            };
            write_call(b, name, args)
        }
        F::NullIf => {
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "NULLIF",
                    expected: "2",
                    got: args.len(),
                });
            }
            write_call(b, "NULLIF", args)
        }

        // -------- date/time (issue #3) --------
        F::Now => {
            if !args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "NOW",
                    expected: "0",
                    got: args.len(),
                });
            }
            // SQLite uses `CURRENT_TIMESTAMP` (a keyword, no parens);
            // PG and MySQL accept `NOW()` and treat `CURRENT_TIMESTAMP`
            // as an equivalent alias.
            b.sql.push_str(if b.d.name() == "sqlite" {
                "CURRENT_TIMESTAMP"
            } else {
                "NOW()"
            });
            Ok(())
        }
        F::ExtractYear
        | F::ExtractMonth
        | F::ExtractDay
        | F::ExtractHour
        | F::ExtractMinute
        | F::ExtractSecond
        | F::ExtractWeek => write_extract_int(b, kind, args),
        F::ExtractWeekDay => write_extract_weekday(b, args),
        F::ExtractQuarter => {
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "EXTRACT(QUARTER)",
                    expected: "1",
                    got: args.len(),
                });
            }
            if b.d.name() == "sqlite" {
                // SQLite has no quarter token in strftime and no
                // QUARTER() function. Surface a clear error rather
                // than synthesize a multi-clause CASE expression.
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "EXTRACT(QUARTER) (SQLite has no native quarter token)",
                    dialect: "sqlite",
                });
            }
            write_extract_int(b, kind, args)
        }
        F::TruncDate => {
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "DATE",
                    expected: "1",
                    got: args.len(),
                });
            }
            // All three dialects spell this the same: DATE(x) / date(x).
            b.sql.push_str("DATE(");
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
            Ok(())
        }
        F::TruncYear | F::TruncMonth | F::TruncDay => write_trunc(b, kind, args),
        F::JsonArrayLength => {
            // Issue #826 — JSON array length, tri-dialect.
            // PG: jsonb_array_length(x)
            // MySQL: JSON_LENGTH(x)
            // SQLite: json_array_length(x)
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "JSON_ARRAY_LENGTH",
                    expected: "1",
                    got: args.len(),
                });
            }
            let fname = match b.d.name() {
                "postgres" => "jsonb_array_length",
                "mysql" => "JSON_LENGTH",
                // SQLite + ANSI-leaning fallback. `json_array_length`
                // ships in SQLite >= 3.38.
                _ => "json_array_length",
            };
            b.sql.push_str(fname);
            b.sql.push('(');
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
            Ok(())
        }

        // -------- pg_trgm: PG-only, arity 2 --------
        F::TrigramSimilarity | F::TrigramWordSimilarity => {
            let fname = if matches!(kind, F::TrigramWordSimilarity) {
                "WORD_SIMILARITY"
            } else {
                "SIMILARITY"
            };
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: if matches!(kind, F::TrigramWordSimilarity) {
                        "WORD_SIMILARITY"
                    } else {
                        "SIMILARITY"
                    },
                    expected: "2",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: if matches!(kind, F::TrigramWordSimilarity) {
                        "WORD_SIMILARITY (pg_trgm) is Postgres-only"
                    } else {
                        "SIMILARITY (pg_trgm) is Postgres-only"
                    },
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str(fname);
            b.sql.push('(');
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", ");
            write_expr(b, &args[1], None)?;
            b.sql.push(')');
            Ok(())
        }

        // -------- Postgres FTS: PG-only --------
        F::ToTsVector | F::PlainToTsQuery => {
            let fname = if matches!(kind, F::ToTsVector) {
                "to_tsvector"
            } else {
                "plainto_tsquery"
            };
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: if matches!(kind, F::ToTsVector) {
                        "to_tsvector"
                    } else {
                        "plainto_tsquery"
                    },
                    expected: "1",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: if matches!(kind, F::ToTsVector) {
                        "to_tsvector (FTS) is Postgres-only"
                    } else {
                        "plainto_tsquery (FTS) is Postgres-only"
                    },
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str(fname);
            b.sql.push('(');
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
            Ok(())
        }
        F::TsRank => {
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "ts_rank",
                    expected: "2",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "ts_rank (FTS) is Postgres-only",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("ts_rank(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", ");
            write_expr(b, &args[1], None)?;
            b.sql.push(')');
            Ok(())
        }
        F::TsHeadline => {
            if args.len() != 2 && args.len() != 3 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "ts_headline",
                    expected: "2 or 3",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "ts_headline (FTS) is Postgres-only",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("ts_headline(");
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    b.sql.push_str(", ");
                }
                write_expr(b, a, None)?;
            }
            b.sql.push(')');
            Ok(())
        }
        F::PhraseToTsQuery | F::WebsearchToTsQuery | F::ToTsQuery => {
            let fname = match kind {
                F::PhraseToTsQuery => "phraseto_tsquery",
                F::WebsearchToTsQuery => "websearch_to_tsquery",
                F::ToTsQuery => "to_tsquery",
                _ => unreachable!(),
            };
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: fname,
                    expected: "1",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: match kind {
                        F::PhraseToTsQuery => "phraseto_tsquery (FTS) is Postgres-only",
                        F::WebsearchToTsQuery => "websearch_to_tsquery (FTS) is Postgres-only",
                        F::ToTsQuery => "to_tsquery (FTS) is Postgres-only",
                        _ => unreachable!(),
                    },
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str(fname);
            b.sql.push('(');
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
            Ok(())
        }
        F::TsRankCd => {
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "ts_rank_cd",
                    expected: "2",
                    got: args.len(),
                });
            }
            if b.d.name() != "postgres" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "ts_rank_cd (FTS) is Postgres-only",
                    dialect: b.d.name(),
                });
            }
            b.sql.push_str("ts_rank_cd(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", ");
            write_expr(b, &args[1], None)?;
            b.sql.push(')');
            Ok(())
        }

        // -------- DB functions batch 1 (issue #266 / T1.4) --------
        F::LPad | F::RPad => write_pad(b, kind, args),
        F::Md5 | F::Sha1 | F::Sha256 => write_hash(b, kind, args),
        F::Position => write_position(b, args),
        F::Repeat => write_repeat(b, args),
        F::Reverse => {
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "REVERSE",
                    expected: "1",
                    got: args.len(),
                });
            }
            if b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "REVERSE (SQLite has no built-in; reverse the string in Rust before binding)",
                    dialect: b.d.name(),
                });
            }
            write_call(b, "REVERSE", args)
        }
        F::Sign => write_sign(b, args),
        F::Power => {
            if args.len() != 2 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "POWER",
                    expected: "2",
                    got: args.len(),
                });
            }
            // SQLite's `POWER`/`SQRT` ship in libsqlite3 3.35+ but only
            // when compiled with `SQLITE_ENABLE_MATH_FUNCTIONS`. sqlx's
            // default sqlite build does not enable it, so emitting
            // `POWER(...)` would produce a `no such function` runtime
            // error. Surface the limitation at write time.
            if b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "POWER (SQLite needs SQLITE_ENABLE_MATH_FUNCTIONS at build time; not enabled in sqlx-sqlite default build)",
                    dialect: b.d.name(),
                });
            }
            write_call(b, "POWER", args)
        }
        F::Sqrt => {
            if b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "SQRT (SQLite needs SQLITE_ENABLE_MATH_FUNCTIONS at build time; not enabled in sqlx-sqlite default build)",
                    dialect: b.d.name(),
                });
            }
            write_call_unary(b, "SQRT", args)
        }

        // -------- DB functions batch 2 (issue #294 / T2.7) --------
        F::Log => write_log(b, args),
        F::LogWithBase => write_log_with_base(b, args),
        F::Exp => write_exp(b, args),
        F::Pi => write_pi(b, args),
        F::Random => write_random(b, args),
        F::MakeInterval => write_make_interval(b, args),
        F::Age => write_age(b, args),
        F::TruncWithTz => write_trunc_with_tz(b, args),

        // -------- Full-text search builder (issue #295 / T2.4) --------
        F::SetWeight => write_setweight(b, args),
        F::TsConcat => write_ts_concat(b, args),
    }
}

/// `LPAD(s, len, fill)` / `RPAD(s, len, fill)` — PG/MySQL native;
/// SQLite gets a `substr(s || repeat(fill, len), 1, len)`-style
/// workaround for left-pad and a symmetric form for right-pad.
fn write_pad(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    if args.len() != 3 {
        return Err(SqlError::FunctionArityMismatch {
            func: match kind {
                F::LPad => "LPAD",
                _ => "RPAD",
            },
            expected: "3",
            got: args.len(),
        });
    }
    let name = match kind {
        F::LPad => "LPAD",
        _ => "RPAD",
    };
    if b.d.name() != "sqlite" {
        return write_call(b, name, args);
    }
    // SQLite has no LPAD/RPAD. Build it from `substr` + `replace(printf)`.
    // For LPad: `substr(replace(printf('%.*c', len, ' '), ' ', fill) || s, -len)`
    // For RPad: `substr(s || replace(printf('%.*c', len, ' '), ' ', fill), 1, len)`
    //
    // The `printf('%.*c', n, ' ')` trick generates `n` space characters,
    // which we then `replace` with the fill character. Wrapped in `substr`
    // to clip to `len` characters (handles the case where the source
    // string is already >= len, where the spec says to truncate to `len`
    // from one side — LPAD keeps the right side, RPAD keeps the left).
    b.sql.push_str("substr(");
    if matches!(kind, F::LPad) {
        b.sql.push_str("replace(printf('%.*c', ");
        write_expr(b, &args[1], None)?;
        b.sql.push_str(", ' '), ' ', ");
        write_expr(b, &args[2], None)?;
        b.sql.push_str(") || ");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(", -");
        write_expr(b, &args[1], None)?;
        b.sql.push(')');
    } else {
        write_expr(b, &args[0], None)?;
        b.sql.push_str(" || replace(printf('%.*c', ");
        write_expr(b, &args[1], None)?;
        b.sql.push_str(", ' '), ' ', ");
        write_expr(b, &args[2], None)?;
        b.sql.push_str("), 1, ");
        write_expr(b, &args[1], None)?;
        b.sql.push(')');
    }
    Ok(())
}

/// `MD5(s)` / `SHA1(s)` / `SHA256(s)` — PG via `pgcrypto`'s `digest`
/// + `encode(..., 'hex')`, MySQL via native `MD5/SHA1/SHA2`, SQLite
/// errors (no built-in hash).
fn write_hash(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: match kind {
                F::Md5 => "MD5",
                F::Sha1 => "SHA1",
                _ => "SHA256",
            },
            expected: "1",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: match kind {
                F::Md5 => "MD5 (SQLite has no built-in hash; hash before binding)",
                F::Sha1 => "SHA1 (SQLite has no built-in hash; hash before binding)",
                _ => "SHA256 (SQLite has no built-in hash; hash before binding)",
            },
            dialect: b.d.name(),
        });
    }
    if b.d.name() == "postgres" {
        // Postgres has a native md5() returning hex; SHA1/SHA256 need
        // pgcrypto's digest(). Emit the right shape per kind.
        match kind {
            F::Md5 => {
                b.sql.push_str("md5(");
                write_expr(b, &args[0], None)?;
                b.sql.push(')');
            }
            F::Sha1 => {
                b.sql.push_str("encode(digest(");
                write_expr(b, &args[0], None)?;
                b.sql.push_str(", 'sha1'), 'hex')");
            }
            _ => {
                b.sql.push_str("encode(digest(");
                write_expr(b, &args[0], None)?;
                b.sql.push_str(", 'sha256'), 'hex')");
            }
        }
        Ok(())
    } else {
        // MySQL.
        match kind {
            F::Md5 => write_call(b, "MD5", args),
            F::Sha1 => write_call(b, "SHA1", args),
            _ => {
                b.sql.push_str("SHA2(");
                write_expr(b, &args[0], None)?;
                b.sql.push_str(", 256)");
                Ok(())
            }
        }
    }
}

/// `POSITION(needle, hay)` — diverges most across dialects.
fn write_position(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "POSITION",
            expected: "2",
            got: args.len(),
        });
    }
    let needle = &args[0];
    let hay = &args[1];
    match b.d.name() {
        "postgres" => {
            b.sql.push_str("POSITION(");
            write_expr(b, needle, None)?;
            b.sql.push_str(" IN ");
            write_expr(b, hay, None)?;
            b.sql.push(')');
        }
        "mysql" => {
            b.sql.push_str("LOCATE(");
            write_expr(b, needle, None)?;
            b.sql.push_str(", ");
            write_expr(b, hay, None)?;
            b.sql.push(')');
        }
        _ => {
            // SQLite: INSTR(hay, needle) — argument order is reversed.
            b.sql.push_str("INSTR(");
            write_expr(b, hay, None)?;
            b.sql.push_str(", ");
            write_expr(b, needle, None)?;
            b.sql.push(')');
        }
    }
    Ok(())
}

/// `REPEAT(s, n)` — PG/MySQL native; SQLite fakes it via
/// `replace(printf('%.*c', n, '_'), '_', s)`. The `_` placeholder is
/// arbitrary — we just need a single char `printf` can stamp `n`
/// times and `replace` can swap for `s`.
fn write_repeat(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "REPEAT",
            expected: "2",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        // Use `_` as the placeholder. SQLite's `printf('%.*c', n, '_')`
        // produces `n` underscores; `replace(...)` swaps each for `s`.
        // Caveat: if `s` itself contains `_`, the underscore form of
        // the input is lost — for that corner case the caller should
        // pre-compute the repeated value in Rust.
        b.sql.push_str("replace(printf('%.*c', ");
        write_expr(b, &args[1], None)?;
        b.sql.push_str(", '_'), '_', ");
        write_expr(b, &args[0], None)?;
        b.sql.push(')');
        return Ok(());
    }
    write_call(b, "REPEAT", args)
}

/// `SIGN(x)` — PG/MySQL native; SQLite gets a CASE-WHEN expansion.
fn write_sign(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "SIGN",
            expected: "1",
            got: args.len(),
        });
    }
    if b.d.name() != "sqlite" {
        return write_call(b, "SIGN", args);
    }
    // SQLite: CASE WHEN x > 0 THEN 1 WHEN x < 0 THEN -1 ELSE 0 END.
    // We can't use bind params here without exploding the parameter
    // count three-fold; literal `0` / `1` / `-1` are inlined.
    b.sql.push_str("(CASE WHEN ");
    write_expr(b, &args[0], None)?;
    b.sql.push_str(" > 0 THEN 1 WHEN ");
    write_expr(b, &args[0], None)?;
    b.sql.push_str(" < 0 THEN -1 ELSE 0 END)");
    Ok(())
}

// ====================================================================
// DB functions batch 2 (issue #294 / T2.7)
// ====================================================================

/// `LN(x)` — natural log. Same name on PG / MySQL; SQLite errors
/// because `ln` ships only when libsqlite3 was built with
/// `SQLITE_ENABLE_MATH_FUNCTIONS` (sqlx-sqlite's default build does
/// not enable it).
fn write_log(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "LN",
            expected: "1",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "LN (SQLite needs SQLITE_ENABLE_MATH_FUNCTIONS at build time; not enabled in sqlx-sqlite default build)",
            dialect: b.d.name(),
        });
    }
    write_call_unary(b, "LN", args)
}

/// `LOG(base, x)` — log of `x` in base `base`. PG / MySQL native (same
/// argument order); SQLite errors (build-flag caveat per [`write_log`]).
fn write_log_with_base(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "LOG",
            expected: "2",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "LOG(base, x) (SQLite needs SQLITE_ENABLE_MATH_FUNCTIONS at build time)",
            dialect: b.d.name(),
        });
    }
    write_call(b, "LOG", args)
}

/// `EXP(x)` — `e^x`. PG / MySQL native; SQLite errors (build-flag).
fn write_exp(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "EXP",
            expected: "1",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "EXP (SQLite needs SQLITE_ENABLE_MATH_FUNCTIONS at build time; not enabled in sqlx-sqlite default build)",
            dialect: b.d.name(),
        });
    }
    write_call_unary(b, "EXP", args)
}

/// `PI()` — π. PG `pi()`, MySQL `PI()`. SQLite has no native pi, so
/// the writer inlines the IEEE-754 double constant.
fn write_pi(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if !args.is_empty() {
        return Err(SqlError::FunctionArityMismatch {
            func: "PI",
            expected: "0",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        // Inline f64::consts::PI to 16 sig digits. SQLite has no pi()
        // even with the math-functions build flag.
        b.sql.push_str("3.141592653589793");
    } else {
        b.sql.push_str("PI()");
    }
    Ok(())
}

/// `RANDOM()` / `RAND()` — pseudo-random number. **Return-range
/// divergence is documented** at [`crate::core::ScalarFn::Random`].
fn write_random(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if !args.is_empty() {
        return Err(SqlError::FunctionArityMismatch {
            func: "RANDOM",
            expected: "0",
            got: args.len(),
        });
    }
    match b.d.name() {
        "postgres" => b.sql.push_str("random()"),
        "mysql" => b.sql.push_str("RAND()"),
        // SQLite returns a 64-bit signed integer; range divergence
        // documented at the constructor + enum variant.
        _ => b.sql.push_str("random()"),
    }
    Ok(())
}

/// `make_interval(years => ..., months => ..., days => ..., hours => ...,
/// mins => ..., secs => ...)` — **PG-only**.
fn write_make_interval(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 6 {
        return Err(SqlError::FunctionArityMismatch {
            func: "MAKE_INTERVAL",
            expected: "6",
            got: args.len(),
        });
    }
    if b.d.name() != "postgres" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "MAKE_INTERVAL (PG-only; MySQL/SQLite have no native interval type — compute the duration app-side)",
            dialect: b.d.name(),
        });
    }
    let kws = ["years", "months", "days", "hours", "mins", "secs"];
    b.sql.push_str("make_interval(");
    for (i, kw) in kws.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        b.sql.push_str(kw);
        b.sql.push_str(" => ");
        write_expr(b, &args[i], None)?;
    }
    b.sql.push(')');
    Ok(())
}

/// `AGE(ts1, ts2)` — duration between two timestamps. **Return type
/// diverges**: PG `interval`; MySQL / SQLite numeric seconds.
fn write_age(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "AGE",
            expected: "2",
            got: args.len(),
        });
    }
    match b.d.name() {
        "postgres" => {
            b.sql.push_str("age(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", ");
            write_expr(b, &args[1], None)?;
            b.sql.push(')');
        }
        "mysql" => {
            // TIMESTAMPDIFF takes (ts_a, ts_b) and returns ts_b - ts_a;
            // swap so the result is ts1 - ts2 to match PG's `age(ts1, ts2)`
            // = ts1 - ts2 sign convention.
            b.sql.push_str("TIMESTAMPDIFF(SECOND, ");
            write_expr(b, &args[1], None)?;
            b.sql.push_str(", ");
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
        }
        _ => {
            b.sql.push_str("((julianday(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(") - julianday(");
            write_expr(b, &args[1], None)?;
            b.sql.push_str(")) * 86400.0)");
        }
    }
    Ok(())
}

/// `date_trunc(unit, ts AT TIME ZONE tz)` (PG) and equivalents.
/// `unit` and `tz` arrive as `Expr::Literal(SqlValue::String(...))`
/// from [`crate::core::funcs::trunc_with_tz`]; this writer extracts
/// them at emission time (no rebound — the strings are inlined into
/// the SQL since per-dialect format tokens vary).
fn write_trunc_with_tz(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    use crate::core::SqlValue;
    if args.len() != 3 {
        return Err(SqlError::FunctionArityMismatch {
            func: "TRUNC_WITH_TZ",
            expected: "3",
            got: args.len(),
        });
    }
    let (unit, tz) = match (&args[1], &args[2]) {
        (
            crate::core::Expr::Literal(SqlValue::String(u)),
            crate::core::Expr::Literal(SqlValue::String(z)),
        ) => (u.as_str(), z.as_str()),
        _ => {
            return Err(SqlError::OpNotSupportedInDialect {
                op: "TRUNC_WITH_TZ requires string literals for `unit` and `tz` (build via funcs::trunc_with_tz)",
                dialect: b.d.name(),
            });
        }
    };
    let unit_lc = unit.to_ascii_lowercase();
    if !matches!(
        unit_lc.as_str(),
        "year" | "month" | "day" | "hour" | "minute" | "second"
    ) {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "TRUNC_WITH_TZ unit (allowed: year, month, day, hour, minute, second)",
            dialect: b.d.name(),
        });
    }
    // Cheap defense against SQL injection via a bogus unit/tz string.
    // Reject anything outside `[A-Za-z0-9_/+\-: ]` since the value is
    // inlined into the SQL.
    let safe_charset = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '+' | '-' | ':' | ' '))
    };
    if !safe_charset(tz) {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "TRUNC_WITH_TZ tz contains characters outside the safe set [A-Za-z0-9_/+-: ]",
            dialect: b.d.name(),
        });
    }
    match b.d.name() {
        "postgres" => {
            b.sql.push_str("date_trunc('");
            b.sql.push_str(&unit_lc);
            b.sql.push_str("', ");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(" AT TIME ZONE '");
            b.sql.push_str(tz);
            b.sql.push_str("')");
        }
        "mysql" => {
            // CONVERT_TZ takes a from/to pair; assume the stored ts is
            // UTC (the rustango ORM defaults are TIMESTAMPTZ-equivalent).
            // The DATE_FORMAT mask truncates to the requested unit.
            let mask = mysql_trunc_mask(&unit_lc);
            b.sql.push_str("DATE_FORMAT(CONVERT_TZ(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", '+00:00', '");
            b.sql.push_str(tz);
            b.sql.push_str("'), '");
            b.sql.push_str(mask);
            b.sql.push_str("')");
        }
        _ => {
            // SQLite has no TZ DB. The user passes a `±HH:MM` offset or
            // a plain IANA-shaped string; strftime applies it as a
            // modifier. The mask truncates to the requested unit.
            let mask = sqlite_trunc_mask(&unit_lc);
            b.sql.push_str("strftime('");
            b.sql.push_str(mask);
            b.sql.push_str("', ");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", '");
            b.sql.push_str(tz);
            b.sql.push_str("')");
        }
    }
    Ok(())
}

fn mysql_trunc_mask(unit: &str) -> &'static str {
    match unit {
        "year" => "%Y-01-01 00:00:00",
        "month" => "%Y-%m-01 00:00:00",
        "day" => "%Y-%m-%d 00:00:00",
        "hour" => "%Y-%m-%d %H:00:00",
        "minute" => "%Y-%m-%d %H:%i:00",
        _ => "%Y-%m-%d %H:%i:%S",
    }
}

fn sqlite_trunc_mask(unit: &str) -> &'static str {
    match unit {
        "year" => "%Y-01-01 00:00:00",
        "month" => "%Y-%m-01 00:00:00",
        "day" => "%Y-%m-%d 00:00:00",
        "hour" => "%Y-%m-%d %H:00:00",
        "minute" => "%Y-%m-%d %H:%M:00",
        _ => "%Y-%m-%d %H:%M:%S",
    }
}

// ====================================================================
// Full-text-search builder (issue #295 / T2.4)
// ====================================================================

/// `setweight(<tsvector>, '<weight>')`. **PG-only**. The weight is
/// carried as `Expr::Literal(SqlValue::String("A"))` so the writer can
/// inline it (PG's `setweight` requires the weight literal to live in
/// the SQL text, not in a bound parameter — `bind` rejects char-typed
/// inputs and a quoted `'A'` is the only portable form).
fn write_setweight(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    use crate::core::{Expr, SqlValue};
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "setweight",
            expected: "2",
            got: args.len(),
        });
    }
    if b.d.name() != "postgres" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "setweight (FTS) is Postgres-only",
            dialect: b.d.name(),
        });
    }
    let weight = match &args[1] {
        Expr::Literal(SqlValue::String(w)) => w.as_str(),
        _ => {
            return Err(SqlError::OpNotSupportedInDialect {
                op: "setweight weight argument must be a string literal — build via fts::SearchVector::weighted",
                dialect: b.d.name(),
            });
        }
    };
    if !matches!(weight, "A" | "B" | "C" | "D") {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "setweight weight must be one of 'A' | 'B' | 'C' | 'D'",
            dialect: b.d.name(),
        });
    }
    b.sql.push_str("setweight(");
    write_expr(b, &args[0], None)?;
    b.sql.push_str(", '");
    b.sql.push_str(weight);
    b.sql.push_str("')");
    Ok(())
}

/// `(a || b || c)` — Postgres tsvector concatenation. **PG-only**;
/// MySQL / SQLite reject because their FTS shapes are schema-bound
/// (FULLTEXT INDEX + MATCH/AGAINST on MySQL, FTS5 virtual tables on
/// SQLite) and don't compose with `||`.
fn write_ts_concat(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() < 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "ts_concat (||)",
            expected: ">= 2",
            got: args.len(),
        });
    }
    if b.d.name() != "postgres" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "tsvector `||` concatenation (FTS) is Postgres-only",
            dialect: b.d.name(),
        });
    }
    b.sql.push('(');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(" || ");
        }
        write_expr(b, a, None)?;
    }
    b.sql.push(')');
    Ok(())
}

/// Emit an `EXTRACT(<field> FROM x)` family call. PG uses the SQL
/// standard syntax + cast to integer; MySQL has direct per-field
/// functions; SQLite routes through `strftime` + cast.
fn write_extract_int(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "EXTRACT",
            expected: "1",
            got: args.len(),
        });
    }
    let field = match kind {
        F::ExtractYear => "YEAR",
        F::ExtractMonth => "MONTH",
        F::ExtractDay => "DAY",
        F::ExtractHour => "HOUR",
        F::ExtractMinute => "MINUTE",
        F::ExtractSecond => "SECOND",
        F::ExtractWeek => "WEEK",
        F::ExtractQuarter => "QUARTER",
        _ => unreachable!("write_extract_int called with non-extract kind: {kind:?}"),
    };
    let dialect = b.d.name();
    if dialect == "postgres" {
        // EXTRACT returns NUMERIC; cast to INTEGER for return-type
        // parity with MySQL's per-field functions.
        b.sql.push_str("CAST(EXTRACT(");
        b.sql.push_str(field);
        b.sql.push_str(" FROM ");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(") AS INTEGER)");
    } else if dialect == "mysql" {
        b.sql.push_str(field);
        b.sql.push('(');
        write_expr(b, &args[0], None)?;
        b.sql.push(')');
    } else {
        let token = match field {
            "YEAR" => "%Y",
            "MONTH" => "%m",
            "DAY" => "%d",
            "HOUR" => "%H",
            "MINUTE" => "%M",
            "SECOND" => "%S",
            "WEEK" => "%W",
            _ => unreachable!("sqlite extract: {field}"),
        };
        b.sql.push_str("CAST(strftime('");
        b.sql.push_str(token);
        b.sql.push_str("', ");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(") AS INTEGER)");
    }
    Ok(())
}

/// Day-of-week with cross-dialect normalization. Picks PG's convention
/// (0 = Sunday, 6 = Saturday) and adjusts MySQL's `DAYOFWEEK()`
/// (1 = Sunday) by subtracting 1. SQLite's `strftime('%w')` already
/// returns 0 = Sunday.
fn write_extract_weekday(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "EXTRACT(WEEKDAY)",
            expected: "1",
            got: args.len(),
        });
    }
    let dialect = b.d.name();
    if dialect == "postgres" {
        b.sql.push_str("CAST(EXTRACT(DOW FROM ");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(") AS INTEGER)");
    } else if dialect == "mysql" {
        b.sql.push_str("(DAYOFWEEK(");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(") - 1)");
    } else {
        b.sql.push_str("CAST(strftime('%w', ");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(") AS INTEGER)");
    }
    Ok(())
}

/// `DATE_TRUNC` family — diverges most across dialects. PG has the
/// canonical `DATE_TRUNC('unit', x)` returning a timestamp. MySQL has
/// no direct trunc-to-unit; emit `DATE_FORMAT(x, '%Y-...-...')`
/// returning text. SQLite emits `strftime(...)` also returning text.
/// The result-type caveat is documented at the builder.
fn write_trunc(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: "DATE_TRUNC",
            expected: "1",
            got: args.len(),
        });
    }
    let dialect = b.d.name();
    let pg_unit = match kind {
        F::TruncYear => "year",
        F::TruncMonth => "month",
        F::TruncDay => "day",
        _ => unreachable!("write_trunc: non-trunc kind: {kind:?}"),
    };
    let format_str = match kind {
        F::TruncYear => "%Y-01-01",
        F::TruncMonth => "%Y-%m-01",
        F::TruncDay => "%Y-%m-%d",
        _ => unreachable!(),
    };
    if dialect == "postgres" {
        b.sql.push_str("DATE_TRUNC('");
        b.sql.push_str(pg_unit);
        b.sql.push_str("', ");
        write_expr(b, &args[0], None)?;
        b.sql.push(')');
    } else if dialect == "mysql" {
        if matches!(kind, F::TruncDay) {
            b.sql.push_str("DATE(");
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
        } else {
            b.sql.push_str("DATE_FORMAT(");
            write_expr(b, &args[0], None)?;
            b.sql.push_str(", '");
            b.sql.push_str(format_str);
            b.sql.push_str("')");
        }
    } else {
        if matches!(kind, F::TruncDay) {
            b.sql.push_str("date(");
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
        } else {
            b.sql.push_str("strftime('");
            b.sql.push_str(format_str);
            b.sql.push_str("', ");
            write_expr(b, &args[0], None)?;
            b.sql.push(')');
        }
    }
    Ok(())
}

/// Standard `NAME(arg, arg, …)` emit. Used by every function variant
/// whose dialect emission is identical across PG / MySQL / SQLite.
fn write_call(b: &mut Sql<'_>, name: &str, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    b.sql.push_str(name);
    b.sql.push('(');
    let mut first = true;
    for a in args {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        write_expr(b, a, None)?;
    }
    b.sql.push(')');
    Ok(())
}

/// `write_call` with an arity-1 assertion. Used for unary functions
/// (LOWER, UPPER, LENGTH, TRIM, LTRIM, RTRIM, ABS, CEIL, FLOOR) so a
/// hand-rolled `Expr::Function { args: vec![] }` or `vec![a, b]` fails
/// at emit-time with a clear error rather than reaching the database
/// with malformed SQL like `LOWER()` or `LENGTH(a, b)`. The public
/// builder API is type-locked to a single arg, so this only fires for
/// callers that construct the IR directly.
fn write_call_unary(
    b: &mut Sql<'_>,
    name: &'static str,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    if args.len() != 1 {
        return Err(SqlError::FunctionArityMismatch {
            func: name,
            expected: "1",
            got: args.len(),
        });
    }
    write_call(b, name, args)
}

// ====================================================================
// DELETE
// ====================================================================

pub(super) fn write_delete(b: &mut Sql<'_>, query: &DeleteQuery) -> Result<(), SqlError> {
    b.scope_stack.push(query.model);
    let r = (|| {
        b.sql.push_str("DELETE FROM ");
        b.write_ident(query.model.table);
        write_where(b, &query.where_clause, Some(query.model))?;
        Ok(())
    })();
    b.scope_stack.pop();
    r
}

// ====================================================================
// BULK UPDATE — Postgres-only `UPDATE … FROM (VALUES …)` shape.
// MySQL would need a different translation (CASE WHEN, or
// CREATE TEMP TABLE + JOIN); leaving that for batch4. For now the
// MySql dialect routes bulk_update through a clear "not supported"
// error in its own compile_bulk_update.
// ====================================================================

/// #560 — SQLite variant of `bulk_update`. SQLite supports
/// `UPDATE … FROM <subquery>` since 3.33, but **not** the
/// column-list-alias-on-inline-VALUES form Postgres uses
/// (`FROM (VALUES …) AS __data(pk, col, …)`); SQLite raises
/// `near "(": syntax error` at the alias parens. The
/// CTE + correlated-subquery shape below parses everywhere
/// SQLite has supported CTEs (3.8.3, 2014).
#[cfg(feature = "sqlite")]
pub(super) fn write_bulk_update_sqlite(
    b: &mut Sql<'_>,
    query: &BulkUpdateQuery,
) -> Result<(), SqlError> {
    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.update_columns.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    let pk_field = query
        .model
        .primary_key()
        .ok_or(SqlError::MissingPrimaryKey)?;

    // WITH __data(<pk>, <c1>, …) AS (VALUES (?, …), (?, …))
    b.sql.push_str("WITH __data(");
    b.write_ident(pk_field.column);
    for col in &query.update_columns {
        b.sql.push_str(", ");
        b.write_ident(col);
    }
    b.sql.push_str(") AS (VALUES ");
    let mut first_row = true;
    for row in &query.rows {
        if !first_row {
            b.sql.push_str(", ");
        }
        first_row = false;
        b.sql.push('(');
        for (i, val) in row.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.push_param(val.clone());
        }
        b.sql.push(')');
    }
    b.sql.push_str(") UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" SET ");
    let mut first_col = true;
    for col in &query.update_columns {
        if !first_col {
            b.sql.push_str(", ");
        }
        first_col = false;
        b.write_ident(col);
        b.sql.push_str(" = (SELECT ");
        b.write_ident(col);
        b.sql.push_str(" FROM __data WHERE __data.");
        b.write_ident(pk_field.column);
        b.sql.push_str(" = ");
        b.write_ident(query.model.table);
        b.sql.push('.');
        b.write_ident(pk_field.column);
        b.sql.push(')');
    }
    b.sql.push_str(" WHERE ");
    b.write_ident(pk_field.column);
    b.sql.push_str(" IN (SELECT ");
    b.write_ident(pk_field.column);
    b.sql.push_str(" FROM __data)");
    Ok(())
}

pub(super) fn write_bulk_update_pg(
    b: &mut Sql<'_>,
    query: &BulkUpdateQuery,
) -> Result<(), SqlError> {
    if query.rows.is_empty() {
        return Err(SqlError::EmptyBulkInsert);
    }
    if query.update_columns.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    let pk_field = query
        .model
        .primary_key()
        .ok_or(SqlError::MissingPrimaryKey)?;

    b.sql.push_str("UPDATE ");
    b.write_ident(query.model.table);
    b.sql.push_str(" SET ");
    let mut first = true;
    for col in &query.update_columns {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(col);
        b.sql.push_str(" = __data.");
        b.write_ident(col);
    }
    b.sql.push_str(" FROM (VALUES ");
    let mut first_row = true;
    for row in &query.rows {
        if !first_row {
            b.sql.push_str(", ");
        }
        first_row = false;
        b.sql.push('(');
        for (i, val) in row.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            b.push_param(val.clone());
        }
        b.sql.push(')');
    }
    b.sql.push_str(") AS __data(");
    b.write_ident(pk_field.column);
    for col in &query.update_columns {
        b.sql.push_str(", ");
        b.write_ident(col);
    }
    b.sql.push_str(") WHERE ");
    b.write_ident(query.model.table);
    b.sql.push('.');
    b.write_ident(pk_field.column);
    b.sql.push_str(" = __data.");
    b.write_ident(pk_field.column);
    Ok(())
}

// ====================================================================
// WHERE / Filters
// ====================================================================

pub(super) fn write_where(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if where_clause.is_empty() {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    write_where_expr(b, where_clause, None, model)
}

pub(super) fn write_where_with_search(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let has_search = search.is_some_and(|s| !s.columns.is_empty() && !s.query.is_empty());
    let has_where = !where_clause.is_empty();
    if !has_where && !has_search {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    if has_where {
        write_where_expr(b, where_clause, qualify_with, model)?;
    }
    if has_search {
        let s = search.expect("checked above");
        if has_where {
            b.sql.push_str(" AND ");
        }
        // Search routes through the dialect's `write_ilike` so each
        // backend emits the case-insensitive LIKE shape it actually
        // supports — Postgres native `ILIKE`, MySQL/SQLite the
        // `LOWER(col) LIKE LOWER(?)` fallback.
        //
        // #438 — push a separate param + placeholder per column. PG
        // could share one (`$N` references repeat), but MySQL/SQLite
        // bind positionally so each `?` needs its own pushed param.
        // Sharing one placeholder across N columns silently failed
        // on multi-column search on non-PG dialects (only the first
        // column got the bound value; subsequent ones bound to NULL
        // / empty / nothing depending on driver). Keeping per-column
        // binds also keeps the writer dialect-agnostic.
        b.sql.push('(');
        for (i, col) in s.columns.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(" OR ");
            }
            b.params.push(SqlValue::String(format!("%{}%", s.query)));
            let placeholder = b.d.placeholder(b.params.len());
            // Build the qualified column identifier the same way the
            // rest of the writer does, then hand it to `write_ilike`.
            let mut qualified = String::new();
            if let Some(table) = qualify_with {
                qualified.push_str(&b.d.quote_ident(table));
                qualified.push('.');
            }
            qualified.push_str(&b.d.quote_ident(col));
            b.d.write_ilike(&mut b.sql, &qualified, &placeholder, false);
        }
        b.sql.push(')');
    }
    Ok(())
}

pub(super) fn write_where_expr(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, qualify_with, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, qualify_with, model),
        WhereExpr::And(items) => write_joined(b, items, " AND ", qualify_with, model),
        WhereExpr::Or(items) => {
            if items.is_empty() {
                return Err(SqlError::EmptyOrBranch);
            }
            write_joined(b, items, " OR ", qualify_with, model)
        }
        WhereExpr::Xor(items) => write_xor(b, items, qualify_with, model),
        WhereExpr::Not(child) => {
            b.sql.push_str("NOT (");
            write_where_expr(b, child, qualify_with, model)?;
            b.sql.push(')');
            Ok(())
        }
        WhereExpr::Exists(subq) => {
            b.sql.push_str("EXISTS (");
            write_select(b, subq)?;
            b.sql.push(')');
            Ok(())
        }
        WhereExpr::NotExists(subq) => {
            b.sql.push_str("NOT EXISTS (");
            write_select(b, subq)?;
            b.sql.push(')');
            Ok(())
        }
        WhereExpr::InSubquery {
            column,
            negated,
            subquery,
        } => {
            let qualified = render_qualified_col(b.d, qualify_with, column);
            b.sql.push_str(&qualified);
            b.sql.push_str(if *negated { " NOT IN (" } else { " IN (" });
            write_select(b, subquery)?;
            b.sql.push(')');
            Ok(())
        }
        WhereExpr::ExprCompare { lhs, op, rhs } => write_expr_compare(b, lhs, *op, rhs),
        WhereExpr::RelExists {
            table,
            correlation,
            negated,
        } => {
            b.sql
                .push_str(if *negated { "NOT EXISTS (" } else { "EXISTS (" });
            b.sql.push_str("SELECT 1 FROM ");
            b.write_ident(table);
            b.sql.push_str(" WHERE ");
            write_rel_correlation(b, table, correlation)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Emit the WHERE-clause body that correlates a raw-table relation
/// subquery ([`WhereExpr::RelExists`] / [`Expr::RelAggregate`], issue
/// #830) back to the enclosing row. These nodes don't embed a
/// [`SelectQuery`] and therefore push no scope frame of their own, so
/// the enclosing query is the **top** of the scope stack (unlike
/// [`Expr::OuterRef`], whose referent is the second-from-top frame).
fn write_rel_correlation(
    b: &mut Sql<'_>,
    table: &str,
    correlation: &crate::core::RelCorrelation,
) -> Result<(), SqlError> {
    use crate::core::RelCorrelation;
    let outer_table = match b.scope_stack.last() {
        Some(m) => m.table,
        // Relation subqueries are only ever emitted inside a query that
        // pushed its model frame; reaching here means a bug.
        None => {
            return Err(SqlError::OuterRefOutsideSubquery {
                column: "<relation>",
            })
        }
    };
    match correlation {
        RelCorrelation::Fk {
            fk_column,
            outer_column,
            ct,
        } => {
            // <table>.<fk_column> = <outer>.<outer_column>
            b.write_ident(table);
            b.sql.push('.');
            b.write_ident(fk_column);
            b.sql.push_str(" = ");
            b.write_ident(outer_table);
            b.sql.push('.');
            b.write_ident(outer_column);
            // GFK content-type discriminator:
            //   AND <table>.<ct_column> =
            //       (SELECT <ct_pk> FROM <ct_table> WHERE <ct_table_col> = $param)
            if let Some(ct) = ct {
                b.sql.push_str(" AND ");
                b.write_ident(table);
                b.sql.push('.');
                b.write_ident(ct.ct_column);
                b.sql.push_str(" = (SELECT ");
                b.write_ident(ct.ct_pk);
                b.sql.push_str(" FROM ");
                b.write_ident(ct.ct_table);
                b.sql.push_str(" WHERE ");
                b.write_ident(ct.ct_table_col);
                b.sql.push_str(" = ");
                b.push_param(crate::core::SqlValue::String(ct.parent_table.to_owned()));
                b.sql.push(')');
            }
            Ok(())
        }
        RelCorrelation::Membership {
            target_pk,
            through,
            dst_col,
            src_col,
            outer_column,
        } => {
            // <table>.<target_pk> IN
            //   (SELECT <dst_col> FROM <through> WHERE <src_col> = <outer>.<outer_column>)
            b.write_ident(table);
            b.sql.push('.');
            b.write_ident(target_pk);
            b.sql.push_str(" IN (SELECT ");
            b.write_ident(dst_col);
            b.sql.push_str(" FROM ");
            b.write_ident(through);
            b.sql.push_str(" WHERE ");
            b.write_ident(through);
            b.sql.push('.');
            b.write_ident(src_col);
            b.sql.push_str(" = ");
            b.write_ident(outer_table);
            b.sql.push('.');
            b.write_ident(outer_column);
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Emit `<lhs-expr> <op> <rhs-expr>` for [`WhereExpr::ExprCompare`].
///
/// Covers the binary-comparison set (`Eq`/`Ne`/`Lt`/`Lte`/`Gt`/`Gte`)
/// plus the SQL-92 standard predicates that work uniformly across
/// every dialect inside HAVING: `IN`/`NOT IN`, `BETWEEN`, `IS NULL`/
/// `IS NOT NULL`, `LIKE`/`NOT LIKE`. The Postgres-specific case-
/// insensitive `ILIKE`/`NOT ILIKE` route through the dialect's
/// `write_ilike` helper so non-PG backends fall back to `LOWER()` +
/// `LIKE` shape automatically (issue #87).
///
/// JSON ops (`JsonContains` / `JsonHasKey` / etc.) and null-safe
/// equality (`IsDistinctFrom` / `IsNotDistinctFrom`) aren't supported
/// against an aggregate LHS — those need dialect-specific writers
/// that take a `&str` for the LHS. The `AggregateBuilder::filter`
/// gate rejects them at build time with [`crate::core::QueryError::HavingOpNotSupported`]
/// so the misuse surfaces with a clear message rather than as a
/// raw dialect error here.
fn write_expr_compare(
    b: &mut Sql<'_>,
    lhs: &crate::core::Expr,
    op: crate::core::Op,
    rhs: &crate::core::Expr,
) -> Result<(), SqlError> {
    use crate::core::{Expr, Op};

    let binary_op_str = match op {
        Op::Eq => Some(" = "),
        Op::Ne => Some(" <> "),
        Op::Lt => Some(" < "),
        Op::Lte => Some(" <= "),
        Op::Gt => Some(" > "),
        Op::Gte => Some(" >= "),
        Op::Like => Some(" LIKE "),
        Op::NotLike => Some(" NOT LIKE "),
        _ => None,
    };
    if let Some(kw) = binary_op_str {
        write_expr(b, lhs, None)?;
        b.sql.push_str(kw);
        write_expr(b, rhs, None)?;
        return Ok(());
    }

    match op {
        Op::Search => {
            // PG full-text search match: `<tsvector> @@ <tsquery>`.
            // **PG-only by language semantics** — MySQL's MATCH … AGAINST
            // and SQLite's FTS5 virtual tables don't compose with bare
            // expression operands. `require_op` routes through the
            // dialect to surface the same OpNotSupportedInDialect that
            // the column-bound Op::Search path emits on non-PG.
            require_op(b.d, op)?;
            write_expr(b, lhs, None)?;
            b.sql.push_str(" @@ ");
            write_expr(b, rhs, None)?;
            Ok(())
        }
        Op::ILike | Op::NotILike => {
            require_op(b.d, op)?;
            // Render lhs first so its params land in the param vector
            // before the rhs literal — keeps `?`-positional dialects
            // (MySQL / SQLite) in textual order. Then carve the lhs
            // SQL out of the buffer, bind the rhs as a string param,
            // and ask the dialect to compose `LOWER(<lhs>) LIKE
            // LOWER(<p>)` (or PG's native `ILIKE`).
            let lhs_start = b.sql.len();
            write_expr(b, lhs, None)?;
            let lhs_str = b.sql.split_off(lhs_start);
            let Expr::Literal(v) = rhs else {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "ILIKE with non-literal RHS in ExprCompare",
                    dialect: b.d.name(),
                });
            };
            b.params.push(v.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_ilike(&mut b.sql, &lhs_str, &p, matches!(op, Op::NotILike));
            Ok(())
        }
        Op::In | Op::NotIn => {
            let Expr::Literal(SqlValue::List(elements)) = rhs else {
                return Err(SqlError::InRequiresList);
            };
            if elements.is_empty() {
                return Err(SqlError::EmptyInList);
            }
            write_expr(b, lhs, None)?;
            b.sql.push_str(if matches!(op, Op::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            let mut first = true;
            for elem in elements {
                if !first {
                    b.sql.push_str(", ");
                }
                first = false;
                b.push_param_typed(elem.clone(), None);
            }
            b.sql.push(')');
            Ok(())
        }
        Op::Between | Op::NotBetween => {
            let Expr::Literal(SqlValue::List(bounds)) = rhs else {
                return Err(SqlError::BetweenRequiresTwoElementList);
            };
            if bounds.len() != 2 {
                return Err(SqlError::BetweenRequiresTwoElementList);
            }
            write_expr(b, lhs, None)?;
            b.sql.push_str(if matches!(op, Op::NotBetween) {
                " NOT BETWEEN "
            } else {
                " BETWEEN "
            });
            b.push_param_typed(bounds[0].clone(), None);
            b.sql.push_str(" AND ");
            b.push_param_typed(bounds[1].clone(), None);
            Ok(())
        }
        Op::IsNull => {
            let Expr::Literal(SqlValue::Bool(is_null)) = rhs else {
                return Err(SqlError::IsNullRequiresBool);
            };
            write_expr(b, lhs, None)?;
            b.sql
                .push_str(if *is_null { " IS NULL" } else { " IS NOT NULL" });
            Ok(())
        }
        // JSON-op family + IsDistinctFrom / IsNotDistinctFrom — the
        // AggregateBuilder::filter gate rejects these at build time
        // with `HavingOpNotSupported`. If we reach here, someone
        // hand-built an `ExprCompare` with one of these ops directly;
        // surface the same shape of error as before.
        _ => Err(SqlError::OpNotSupportedInDialect {
            op: "non-binary comparison in ExprCompare",
            dialect: b.d.name(),
        }),
    }
}

/// Render `<col> <op> <rhs-expr>` for a [`crate::core::ColumnFilter`].
/// Only the binary-comparison `Op` variants are valid here — anything
/// else (`In`, `Between`, `IsNull`, JSON ops, etc.) is a builder error
/// and surfaces as [`SqlError::OpNotSupportedInDialect`] so the test
/// suite catches it.
fn write_column_compare(
    b: &mut Sql<'_>,
    cf: &crate::core::ColumnFilter,
    qualify_with: Option<&str>,
    _model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified = render_qualified_col(b.d, qualify_with, cf.column);
    b.sql.push_str(&qualified);
    let op_str = match cf.op {
        crate::core::Op::Eq => " = ",
        crate::core::Op::Ne => " <> ",
        crate::core::Op::Lt => " < ",
        crate::core::Op::Lte => " <= ",
        crate::core::Op::Gt => " > ",
        crate::core::Op::Gte => " >= ",
        // Other ops don't fit a `col <op> col` shape; reject loudly.
        _ => {
            return Err(SqlError::OpNotSupportedInDialect {
                op: "non-binary comparison in ColumnCompare",
                dialect: b.d.name(),
            });
        }
    };
    b.sql.push_str(op_str);
    write_expr(b, &cf.rhs, None)?;
    Ok(())
}

fn write_joined(
    b: &mut Sql<'_>,
    items: &[WhereExpr],
    sep: &str,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let mut first = true;
    for child in items {
        if !first {
            b.sql.push_str(sep);
        }
        first = false;
        write_child(b, child, qualify_with, model)?;
    }
    Ok(())
}

fn write_child(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, qualify_with, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, qualify_with, model),
        // Subquery-shaped leaves emit their own parens (EXISTS(…) /
        // col IN (…)) so we don't need to add a second layer here.
        // ExprCompare is also a flat `lhs op rhs` leaf — no nesting.
        WhereExpr::Exists(_)
        | WhereExpr::NotExists(_)
        | WhereExpr::InSubquery { .. }
        | WhereExpr::ExprCompare { .. }
        | WhereExpr::RelExists { .. } => write_where_expr(b, expr, qualify_with, model),
        WhereExpr::And(_) | WhereExpr::Or(_) | WhereExpr::Xor(_) | WhereExpr::Not(_) => {
            b.sql.push('(');
            write_where_expr(b, expr, qualify_with, model)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Emit a [`WhereExpr::Xor`] node (issue #27). Django 4.1+ added
/// `Q(a) ^ Q(b)` with the semantic "odd number of operands evaluate
/// to true". Native logical XOR exists on MySQL but not on PG or
/// SQLite, so the writer uses portable SQL-92 rewrites for every
/// backend:
///
/// * 0 children → [`SqlError::EmptyXorBranch`] (mirrors the empty-OR
///   rejection — a vacuously-false predicate is almost always a bug).
/// * 1 child   → the child itself (XOR over a single operand is the
///   operand, no rewrite needed).
/// * 2 children → `((a) AND NOT (b)) OR (NOT (a) AND (b))` —
///   canonical binary form per the issue acceptance criteria.
///   **Caveat**: each operand is emitted twice and therefore evaluated
///   twice on the database side. Deterministic predicates (column =
///   literal, range checks) are unaffected, but volatile expressions
///   (`RANDOM()`, `NOW()`, correlated subqueries with side effects) may
///   return different values across the two evaluations. If you need
///   single-evaluation semantics for binary XOR, hand-build the
///   3-element form `Xor([a, b, FALSE])` so the writer falls into the
///   parity-tally branch below.
/// * 3+ children → `((CASE WHEN q1 THEN 1 ELSE 0 END) + … +
///   (CASE WHEN qN THEN 1 ELSE 0 END)) % 2 = 1` — Django's "odd
///   number of trues" generalization. Portable across PG / MySQL /
///   SQLite; uses standard `CASE WHEN` and the SQL `%` modulus. Each
///   child is evaluated exactly once.
fn write_xor(
    b: &mut Sql<'_>,
    items: &[WhereExpr],
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match items.len() {
        0 => Err(SqlError::EmptyXorBranch),
        1 => write_where_expr(b, &items[0], qualify_with, model),
        2 => {
            // (a AND NOT (b)) OR (NOT (a) AND b)
            b.sql.push('(');
            write_child(b, &items[0], qualify_with, model)?;
            b.sql.push_str(" AND NOT (");
            write_where_expr(b, &items[1], qualify_with, model)?;
            b.sql.push_str(")) OR (NOT (");
            write_where_expr(b, &items[0], qualify_with, model)?;
            b.sql.push_str(") AND ");
            write_child(b, &items[1], qualify_with, model)?;
            b.sql.push(')');
            Ok(())
        }
        _ => {
            // (sum of CASE WHEN q THEN 1 ELSE 0 END) % 2 = 1.
            // `write_child` (not `write_where_expr`) so a composite
            // child — And / Or / nested Xor / Not — gets parenthesized
            // before the `THEN 1`. SQL operator precedence (NOT > AND
            // > OR) would parse it correctly without the wrap, but the
            // explicit parens are belt-and-suspenders against future
            // additions to the precedence ladder.
            b.sql.push('(');
            let mut first = true;
            for child in items {
                if !first {
                    b.sql.push_str(" + ");
                }
                first = false;
                b.sql.push_str("(CASE WHEN ");
                write_child(b, child, qualify_with, model)?;
                b.sql.push_str(" THEN 1 ELSE 0 END)");
            }
            b.sql.push_str(") % 2 = 1");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)] // The op match arms inflate this; splitting per op group hurts readability.
fn write_filter(
    b: &mut Sql<'_>,
    filter: &Filter,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified_col = render_qualified_col(b.d, qualify_with, filter.column);
    let cast = model.and_then(|m| null_cast_for(b.d, m, filter.column));

    match filter.op {
        Op::Eq => simple_op(b, &qualified_col, " = ", filter.value.clone(), cast),
        Op::Ne => simple_op(b, &qualified_col, " <> ", filter.value.clone(), cast),
        Op::Lt => simple_op(b, &qualified_col, " < ", filter.value.clone(), cast),
        Op::Lte => simple_op(b, &qualified_col, " <= ", filter.value.clone(), cast),
        Op::Gt => simple_op(b, &qualified_col, " > ", filter.value.clone(), cast),
        Op::Gte => simple_op(b, &qualified_col, " >= ", filter.value.clone(), cast),
        Op::Like => simple_op(b, &qualified_col, " LIKE ", filter.value.clone(), cast),
        Op::NotLike => simple_op(b, &qualified_col, " NOT LIKE ", filter.value.clone(), cast),
        Op::ILike | Op::NotILike => {
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_ilike(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::NotILike),
            );
        }
        Op::Regex | Op::NotRegex | Op::IRegex | Op::NotIRegex => {
            // Pattern shape is checked upstream — the typed builder
            // `Column::regex(...)` only accepts `impl Into<String>`,
            // and the Django-shape parser rejects non-strings with
            // `QueryError::InvalidLookupValue`. Mirroring LIKE we
            // pass the param through and let the DB surface any
            // residual type mismatch.
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_regex(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::Regex | Op::NotRegex),
                matches!(filter.op, Op::NotRegex | Op::NotIRegex),
            );
        }
        Op::TrigramSimilar | Op::TrigramWordSimilar => {
            // pg_trgm operators — Postgres-only by language semantic.
            // The dialect's `write_trigram_similar` default returns
            // `OpNotSupportedInDialect` for MySQL/SQLite; Postgres
            // emits `<col> % <p>` or `<col> %> <p>`.
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_trigram_similar(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::TrigramWordSimilar),
            )?;
        }
        Op::Search => {
            // Postgres full-text search. The default writer emits
            // `to_tsvector(<col>) @@ plainto_tsquery(<p>)`; MySQL +
            // SQLite override to reject with
            // `OpNotSupportedInDialect` (their FTS shapes are
            // schema-bound and don't compose with a bare column).
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_search(&mut b.sql, &qualified_col, &p)?;
        }
        Op::ArrayContains | Op::ArrayContainedBy | Op::ArrayOverlap => {
            // Postgres ArrayField operators (`@>`, `<@`, `&&`). The
            // dialect default emits the PG shape; MySQL + SQLite
            // override to reject with `OpNotSupportedInDialect`. The
            // bound value MUST be `SqlValue::Array(_)` so it binds
            // as a single PG array parameter — `List` would expand
            // to comma-separated placeholders, which is the wrong
            // shape for array comparison.
            if !matches!(filter.value, SqlValue::Array(_)) {
                return Err(SqlError::ArrayOpRequiresArray);
            }
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            let op_str: &'static str = match filter.op {
                Op::ArrayContains => "@>",
                Op::ArrayContainedBy => "<@",
                Op::ArrayOverlap => "&&",
                _ => unreachable!(),
            };
            b.d.write_array_op(&mut b.sql, &qualified_col, &p, op_str)?;
        }
        Op::RangeContains
        | Op::RangeContainedBy
        | Op::RangeOverlap
        | Op::RangeStrictlyLeft
        | Op::RangeStrictlyRight
        | Op::RangeAdjacent => {
            // Postgres range operators. The bound value is either:
            // - `SqlValue::RangeLiteral(s)` — typical "range vs range"
            //   shape (e.g. `<col> && '[5,10)'::int4range`). PG
            //   implicit-casts the text to the column's range type.
            // - A scalar (`I32`/`I64`/`Date`/`DateTime`) — for the
            //   element-containment case `<col> @> <element>`.
            // Both shapes route through the same writer because the
            // SQL emission is identical (`<col> <op> <placeholder>`).
            require_op(b.d, filter.op)?;
            // #343 — when the filtered column is a typed `Range<T>`
            // (`FieldType::Range`) and the RHS is a *range literal*, cast
            // the bound text to the column's range type so PG resolves
            // the operator (`int4range @> $1::int4range`); without it PG
            // errors `operator does not exist: int4range @> text`. A
            // scalar RHS is element-containment (`int4range @> integer`)
            // and must stay un-cast; a non-`Range` column (the legacy
            // "declare the range as String + raw migration" shape) keeps
            // the bare placeholder.
            let range_cast = if matches!(filter.value, SqlValue::RangeLiteral(_)) {
                model
                    .and_then(|m| m.field_by_column(filter.column))
                    .filter(|f| matches!(f.ty, crate::core::FieldType::Range(_)))
                    .and_then(|f| b.d.cast_type(f.ty))
            } else {
                None
            };
            b.params.push(filter.value.clone());
            let mut p = b.d.placeholder(b.params.len());
            if let Some(ty) = range_cast {
                p.push_str("::");
                p.push_str(ty);
            }
            let op_str: &'static str = match filter.op {
                Op::RangeContains => "@>",
                Op::RangeContainedBy => "<@",
                Op::RangeOverlap => "&&",
                Op::RangeStrictlyLeft => "<<",
                Op::RangeStrictlyRight => ">>",
                Op::RangeAdjacent => "-|-",
                _ => unreachable!(),
            };
            b.d.write_range_op(&mut b.sql, &qualified_col, &p, op_str)?;
        }
        Op::In | Op::NotIn => {
            let SqlValue::List(elements) = &filter.value else {
                return Err(SqlError::InRequiresList);
            };
            if elements.is_empty() {
                return Err(SqlError::EmptyInList);
            }
            b.sql.push_str(&qualified_col);
            b.sql.push_str(if matches!(filter.op, Op::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            let mut first = true;
            for elem in elements {
                if !first {
                    b.sql.push_str(", ");
                }
                first = false;
                b.push_param_typed(elem.clone(), cast);
            }
            b.sql.push(')');
        }
        Op::Between | Op::NotBetween => {
            let SqlValue::List(bounds) = &filter.value else {
                return Err(SqlError::BetweenRequiresTwoElementList);
            };
            if bounds.len() != 2 {
                return Err(SqlError::BetweenRequiresTwoElementList);
            }
            b.sql.push_str(&qualified_col);
            b.sql.push_str(if matches!(filter.op, Op::NotBetween) {
                " NOT BETWEEN "
            } else {
                " BETWEEN "
            });
            b.push_param_typed(bounds[0].clone(), cast);
            b.sql.push_str(" AND ");
            b.push_param_typed(bounds[1].clone(), cast);
        }
        Op::IsNull => {
            let SqlValue::Bool(is_null) = filter.value else {
                return Err(SqlError::IsNullRequiresBool);
            };
            b.sql.push_str(&qualified_col);
            b.sql
                .push_str(if is_null { " IS NULL" } else { " IS NOT NULL" });
        }
        Op::IsDistinctFrom | Op::IsNotDistinctFrom => {
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_null_safe_eq(
                &mut b.sql,
                &qualified_col,
                &p,
                matches!(filter.op, Op::IsDistinctFrom),
            );
        }
        Op::JsonContains => {
            require_op(b.d, filter.op)?;
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_contains(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonContainedBy => {
            require_op(b.d, filter.op)?;
            let SqlValue::Json(_) = &filter.value else {
                return Err(SqlError::JsonOpRequiresJson);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_contained_by(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonHasKey => {
            require_op(b.d, filter.op)?;
            let SqlValue::String(_) = &filter.value else {
                return Err(SqlError::JsonKeyRequiresString);
            };
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_json_has_key(&mut b.sql, &qualified_col, &p);
        }
        Op::JsonHasAnyKey | Op::JsonHasAllKeys => {
            require_op(b.d, filter.op)?;
            let SqlValue::List(keys) = &filter.value else {
                return Err(SqlError::JsonKeysRequiresList);
            };
            // Bind each key as its own param, collect the placeholder
            // strings, then ask the dialect to compose the predicate.
            // PG produces `col ?| ARRAY[$1,$2]` / `col ?& ARRAY[$1,$2]`;
            // MySQL produces `JSON_CONTAINS_PATH(col, 'one'|'all',
            // CONCAT('$.', ?), CONCAT('$.', ?))`.
            let placeholders = bind_param_list(b, keys);
            if matches!(filter.op, Op::JsonHasAnyKey) {
                b.d.write_json_has_any_keys(&mut b.sql, &qualified_col, &placeholders);
            } else {
                b.d.write_json_has_all_keys(&mut b.sql, &qualified_col, &placeholders);
            }
        }
    }
    Ok(())
}

fn simple_op(
    b: &mut Sql<'_>,
    qualified_col: &str,
    kw: &str,
    value: SqlValue,
    cast: Option<&'static str>,
) {
    b.sql.push_str(qualified_col);
    b.sql.push_str(kw);
    b.push_param_typed(value, cast);
}

/// Render `[<table>.]<col>` using the dialect's quoting rules.
/// Allocated up front so `write_filter`'s op handlers can either emit
/// it directly or wrap it (e.g. `LOWER(<col>) LIKE …`) without having
/// to backtrack writes already on the buffer.
fn render_qualified_col(d: &dyn Dialect, qualify_with: Option<&str>, column: &str) -> String {
    let mut s = String::new();
    if let Some(table) = qualify_with {
        s.push_str(&d.quote_ident(table));
        s.push('.');
    }
    s.push_str(&d.quote_ident(column));
    s
}

/// Bind each value in `values` as a param without writing anything to
/// `b.sql`; return the placeholder strings (`$1`, `$2`, … on Postgres;
/// `?`, `?`, … on MySQL) so a per-dialect predicate writer can compose
/// them into the final fragment.
fn bind_param_list(b: &mut Sql<'_>, values: &[SqlValue]) -> Vec<String> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        b.params.push(v.clone());
        out.push(b.d.placeholder(b.params.len()));
    }
    out
}

fn require_op(d: &dyn Dialect, op: Op) -> Result<(), SqlError> {
    if d.supports_op(op) {
        Ok(())
    } else {
        Err(SqlError::OperatorNotSupportedInDialect {
            op: op_label(op),
            dialect: d.name(),
        })
    }
}

fn op_label(op: Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Ne => "<>",
        Op::Lt => "<",
        Op::Lte => "<=",
        Op::Gt => ">",
        Op::Gte => ">=",
        Op::In => "IN",
        Op::NotIn => "NOT IN",
        Op::Like => "LIKE",
        Op::NotLike => "NOT LIKE",
        Op::ILike => "ILIKE",
        Op::NotILike => "NOT ILIKE",
        Op::Between => "BETWEEN",
        Op::NotBetween => "NOT BETWEEN",
        Op::IsNull => "IS NULL",
        Op::IsDistinctFrom => "IS DISTINCT FROM",
        Op::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        Op::JsonContains => "@>",
        Op::JsonContainedBy => "<@",
        Op::JsonHasKey => "? (json)",
        Op::JsonHasAnyKey => "?| (json)",
        Op::JsonHasAllKeys => "?& (json)",
        Op::Regex => "~ (regex)",
        Op::NotRegex => "!~ (regex)",
        Op::TrigramSimilar => "% (trigram_similar)",
        Op::TrigramWordSimilar => "%> (trigram_word_similar)",
        Op::Search => "@@ (search)",
        Op::ArrayContains => "@> (array_contains)",
        Op::ArrayContainedBy => "<@ (array_contained_by)",
        Op::ArrayOverlap => "&& (array_overlap)",
        Op::RangeContains => "@> (range_contains)",
        Op::RangeContainedBy => "<@ (range_contained_by)",
        Op::RangeOverlap => "&& (range_overlap)",
        Op::RangeStrictlyLeft => "<< (range_strictly_left)",
        Op::RangeStrictlyRight => ">> (range_strictly_right)",
        Op::RangeAdjacent => "-|- (range_adjacent)",
        Op::IRegex => "~* (iregex)",
        Op::NotIRegex => "!~* (iregex)",
    }
}

// ====================================================================
// ORDER BY / LIMIT / OFFSET
// ====================================================================

fn write_order_limit_offset(
    b: &mut Sql<'_>,
    order_by: &[crate::core::OrderItem],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
) -> Result<(), SqlError> {
    use crate::core::{NullsOrder, OrderItem};
    if !order_by.is_empty() {
        b.sql.push_str(" ORDER BY ");
        let supports_nulls = b.d.supports_nulls_order();
        let mut first = true;
        for item in order_by {
            // Resolve the (target, desc, nulls) triple uniformly.
            // Column targets get quoted ident; Expr targets get
            // written through the standard expr writer.
            let (desc, nulls) = match item {
                OrderItem::Column { desc, nulls, .. } => (*desc, *nulls),
                OrderItem::Expr { desc, nulls, .. } => (*desc, *nulls),
                // Random ordering: no direction (unordered by
                // definition), no NULLS clause (the random key is
                // per-row + non-NULL). Skip the desc/nulls dance.
                OrderItem::Random => (false, NullsOrder::Default),
            };
            // Emulate NULLS FIRST/LAST on MySQL via a leading
            // `<target> IS NULL <asc|desc>` term, then fall through
            // to the actual sort.
            if !supports_nulls && !matches!(nulls, NullsOrder::Default) {
                if !first {
                    b.sql.push_str(", ");
                }
                first = false;
                write_order_target(b, item, qualify_with)?;
                b.sql.push_str(" IS NULL");
                // NULLS FIRST → group NULLs to the top → IS NULL DESC.
                // NULLS LAST  → group NULLs to the bottom → IS NULL ASC.
                match nulls {
                    NullsOrder::First => b.sql.push_str(" DESC"),
                    NullsOrder::Last => b.sql.push_str(" ASC"),
                    NullsOrder::Default => unreachable!(),
                }
            }
            if !first {
                b.sql.push_str(", ");
            }
            first = false;
            write_order_target(b, item, qualify_with)?;
            if desc {
                b.sql.push_str(" DESC");
            }
            if supports_nulls {
                match nulls {
                    NullsOrder::First => b.sql.push_str(" NULLS FIRST"),
                    NullsOrder::Last => b.sql.push_str(" NULLS LAST"),
                    NullsOrder::Default => {}
                }
            }
        }
    }
    if let Some(n) = limit {
        let _ = write!(b.sql, " LIMIT {n}");
    } else if offset.is_some() {
        // #560 — `OFFSET` without `LIMIT` is illegal on MySQL
        // (`ERROR 1064`). Dialects that need a placeholder LIMIT to
        // make a bare OFFSET parse return one via
        // `Dialect::offset_without_limit_clause()`. PG + SQLite
        // return `None` and accept the bare OFFSET below.
        if let Some(clause) = b.d.offset_without_limit_clause() {
            b.sql.push_str(clause);
        }
    }
    if let Some(n) = offset {
        let _ = write!(b.sql, " OFFSET {n}");
    }
    Ok(())
}

/// Emit just the column or expression part of an `OrderItem` — the
/// `<target>` half of `<target> [DESC] [NULLS …]`. Used twice when
/// a MySQL `NULLS …` emulation needs the target both for the
/// `IS NULL` pre-sort term and the main sort term.
fn write_order_target(
    b: &mut Sql<'_>,
    item: &crate::core::OrderItem,
    qualify_with: Option<&str>,
) -> Result<(), SqlError> {
    use crate::core::OrderItem;
    match item {
        OrderItem::Column { column, .. } => {
            if let Some(table) = qualify_with {
                b.write_ident(table);
                b.sql.push('.');
            }
            b.write_ident(column);
        }
        OrderItem::Expr { expr, .. } => {
            write_expr(b, expr, None)?;
        }
        // Issue #77 — tri-dialect random ordering. PG + SQLite use
        // `RANDOM()`; MySQL uses `RAND()`. Both are 0-arg, return
        // a per-row pseudorandom value; the surrounding `ORDER BY`
        // sorts by that value.
        OrderItem::Random => {
            b.sql.push_str(b.d.random_fn());
            b.sql.push_str("()");
        }
    }
    Ok(())
}

// ====================================================================
// RETURNING
// ====================================================================

fn write_returning(b: &mut Sql<'_>, returning: &[&'static str]) -> Result<(), SqlError> {
    if returning.is_empty() {
        return Ok(());
    }
    if !b.d.supports_returning() {
        // Caller is expected to fall back to LAST_INSERT_ID() / similar
        // — but that decision belongs at the executor layer (where we
        // know whether the model has an Auto<T> PK). Surface a clear
        // error here so the executor can detect + handle it instead of
        // silently producing SQL the backend rejects.
        return Err(SqlError::OperatorNotSupportedInDialect {
            op: "RETURNING",
            dialect: b.d.name(),
        });
    }
    b.sql.push_str(" RETURNING ");
    let mut first = true;
    for col in returning {
        if !first {
            b.sql.push_str(", ");
        }
        first = false;
        b.write_ident(col);
    }
    Ok(())
}

// ====================================================================
// Helper exposed to the legacy `compile_where_order_tail` shim
// (annotate_count_children calls this directly).
// ====================================================================

#[allow(unused)]
#[allow(clippy::too_many_arguments)] // 8 args mirrors the existing public shim signature; refactoring the call-sites is a v0.24 cleanup.
pub(crate) fn compile_where_order_tail(
    d: &dyn Dialect,
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    order_by: &[crate::core::OrderItem],
    limit: Option<i64>,
    offset: Option<i64>,
    qualify_with: Option<&str>,
    model: Option<&'static ModelSchema>,
) -> Result<CompiledStatement, SqlError> {
    let mut b = Sql::new(d);
    write_where_with_search(&mut b, where_clause, search, qualify_with, model)?;
    write_order_limit_offset(&mut b, order_by, limit, offset, qualify_with)?;
    Ok(b.finish())
}
