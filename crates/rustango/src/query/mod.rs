//! Query layer for rustango.
//!
//! v0.1 ships a typed `QuerySet<T>` that builds an `AND`-joined `WHERE`
//! clause and compiles to the dialect-neutral `SelectQuery` IR in
//! `rustango-core`. `UpdateBuilder<T>` mirrors the same shape for `UPDATE`,
//! and `QuerySet<T>` itself is the input to bulk delete. The dynamic
//! resolver lands in week 5.

use std::marker::PhantomData;

use crate::core::{
    AggregateExpr, AggregateQuery, Assignment, DeleteQuery, Expr, Filter, Model, ModelSchema, Op,
    QueryError, ScalarFn, SelectQuery, SqlValue, TypedAssignment, TypedExpr, UpdateQuery,
    WhereExpr,
};

mod q;
pub use q::Q;

/// A lazy builder for a `SELECT` over `T`.
///
/// Filters are accumulated in insertion order; nothing touches the schema
/// until `compile` is called, so the builder never panics on bad input.
///
/// Two filter shapes are accepted and may be mixed freely:
/// * [`Self::filter`] / [`Self::eq`] — string-keyed, validated at
///   `compile` time.
/// * [`Self::where_`] — typed (`User::id.gt(10)`); the column is already
///   resolved, so it bypasses the schema lookup at compile time.
///
/// Implements `Clone` manually so a half-built queryset can be
/// reused as a base for divergent branches — Eloquent
/// `Builder::clone()` / `Builder::tap(fn ($q) { ... })` parity.
/// Manual impl (rather than `#[derive(Clone)]`) avoids the
/// auto-derive's spurious `T: Clone` bound; `T` here is a
/// `Model` (compile-time `&'static SCHEMA` reference) and never
/// needs to itself be cloned.
pub struct QuerySet<T: Model> {
    pending: Vec<PendingFilter>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// Django `.distinct(*fields)` — issue #264 / T1.2. `None` (default)
    /// emits no DISTINCT clause. See [`crate::core::DistinctMode`] for
    /// the per-mode semantics.
    distinct: Option<crate::core::DistinctMode>,
    /// FK field names registered for [`Self::select_related`] — slice
    /// 9.0d. Each name resolves to a `Join` against the FK target at
    /// `compile()` time, so the SELECT pulls the parent rows along
    /// with the children in a single SQL round trip.
    select_related: Vec<String>,
    /// Ad-hoc joins registered via [`Self::join`] (issue #80). Stored
    /// pre-built rather than as Rust field names because the predicate
    /// is arbitrary; appended after `select_related` joins at compile
    /// time so explicit user-driven joins sit alongside the automatic
    /// FK ones in the SELECT list.
    ad_hoc_joins: Vec<crate::core::Join>,
    /// Derived-table joins registered via [`Self::join_sub`] /
    /// [`Self::join_lateral`] (Eloquent `joinSub` / `joinLateral`, issue
    /// #828). Carried straight onto the compiled
    /// [`SelectQuery::subquery_joins`].
    subquery_joins: Vec<crate::core::SubqueryJoin>,
    /// Unified pending `ORDER BY` list (slice 9.0b + issue #76).
    /// Carries `Field { name, desc, nulls }` entries from
    /// `.order_by(...)` / `.order_by_with_nulls(...)` plus `Expr { … }`
    /// entries from `.order_by_expr(...)`. Lowered at `compile()`
    /// time in registration order so chain order is preserved
    /// across mixed builder calls.
    order_by: Vec<PendingOrderItem>,
    /// Row-lock mode for `SELECT … FOR UPDATE` — Django's
    /// `select_for_update(skip_locked=, nowait=, of=, no_key=)`.
    /// Issue #21. `None` (default) emits no lock clause.
    lock_mode: Option<crate::core::LockMode>,
    /// Set-algebra branches — Django's `.union(other_qs, all=)` /
    /// `.intersection(other_qs)` / `.difference(other_qs)`. Issue #25.
    /// Each entry is a pre-compiled [`crate::core::CompoundBranch`].
    /// Empty (default) emits a plain SELECT.
    compound: Vec<crate::core::CompoundBranch>,
    /// #1034 — head-branch `ORDER BY` frozen at the first `.union()` /
    /// `.intersection()` / `.difference()` call. `order_by` / `limit` /
    /// `offset` set BEFORE the first set-op scope to the FIRST
    /// queryset (Django 4.0+ component-queryset slicing); the freeze
    /// snapshots them here so clauses chained AFTER the set-op
    /// accumulate fresh in the live slots and apply to the combined
    /// result. Empty until the first set-op call.
    head_order_by: Vec<PendingOrderItem>,
    /// #1034 — head-branch `LIMIT` frozen at the first set-op call.
    /// See [`Self::head_order_by`].
    head_limit: Option<i64>,
    /// #1034 — head-branch `OFFSET` frozen at the first set-op call.
    /// See [`Self::head_order_by`].
    head_offset: Option<i64>,
    /// Django `.none()` short-circuit — issue #331. When `true`, every
    /// terminal op compiles to a guaranteed-empty SQL statement: SELECT
    /// gets `LIMIT 0`, UPDATE / DELETE gain an `IS NULL` predicate
    /// against the (NOT NULL) primary key so no row matches. Builders
    /// chained after `.none()` keep accumulating filters / orderings,
    /// matching Django's "an immutable empty queryset" semantic.
    is_none: bool,
    /// Issue #820 — Eloquent `withoutGlobalScope($name)`. Names from
    /// `T::SCHEMA.global_scopes` whose auto-applied filters should be
    /// skipped on this queryset. Empty (default) keeps every scope
    /// active. Populated by [`Self::without_global_scope`]. When
    /// [`Self::disable_all_global_scopes`] is also set, this list is
    /// ignored (the wholesale opt-out wins).
    disabled_global_scopes: Vec<&'static str>,
    /// Issue #820 — Eloquent `withoutGlobalScopes()`. When `true`,
    /// every scope on `T::SCHEMA.global_scopes` is skipped (this
    /// queryset behaves like the model carries no scopes). Set by
    /// [`Self::without_global_scopes`]. Once true the
    /// [`Self::disabled_global_scopes`] per-name list becomes
    /// redundant — left intact so re-chaining `without_global_scope`
    /// after `without_global_scopes` doesn't surprise the caller.
    disable_all_global_scopes: bool,
    _model: PhantomData<fn() -> T>,
}

/// Issue #76: order-by entry in the QuerySet's unified pending list.
/// `Field` carries a string field name resolved against the schema
/// at `compile()` time (sugar shared between `.order_by(...)` and
/// `.order_by_with_nulls(...)`). `Expr` wraps an already-built
/// expression for `.order_by_expr(...)`. The list preserves
/// registration order so mixed builder chains compose predictably.
#[derive(Debug, Clone)]
enum PendingOrderItem {
    Field {
        name: String,
        desc: bool,
        nulls: crate::core::NullsOrder,
    },
    Expr {
        expr: crate::core::Expr,
        desc: bool,
        nulls: crate::core::NullsOrder,
    },
    /// `ORDER BY RANDOM()` / `RAND()` — issue #77. No fields; the
    /// writer picks the dialect-specific token at emit time.
    Random,
}

/// Filter accumulator entry — keeps insertion order across string-keyed and
/// typed filter calls. Each entry contributes one node to the final
/// `WhereExpr::And` clause.
#[derive(Clone)]
enum PendingFilter {
    /// String-keyed; resolved against the schema at `compile` time.
    Raw(RawFilter),
    /// Date-part transform lookup (`created__year__gte`, issue #829).
    /// The column reference is wrapped in the named scalar fn
    /// (`ExtractYear` / `TruncDate` / …) at resolve time, then
    /// compared against `value` using `op`. Composed via
    /// `WhereExpr::ExprCompare` so the same writer that handles JOIN
    /// `ON` predicates renders it.
    DateTransform(DateTransformFilter),
    /// Already resolved by a typed [`Column`](crate::core::Column).
    Resolved(Filter),
    /// Typed sub-expression (built via `.and()` / `.or()` on the
    /// typed-column API). Already validated; contributes a whole
    /// sub-tree to the WHERE clause.
    Expr(WhereExpr),
    /// Negated predicate — `NOT (<inner>)`. Backs
    /// [`QuerySet::exclude`] (#1030). The inner entry is resolved
    /// recursively then wrapped in [`WhereExpr::Not`], so `exclude`
    /// inherits the full lookup grammar (`__gt`, `__icontains`, …).
    Negated(Box<PendingFilter>),
    /// Relation-spanning lookup deferred from [`parse_lookup`] (#1031) —
    /// `author__name`, `author__profile__bio__icontains`. Resolved by a
    /// pre-pass in [`QuerySet::compile`] that converts it to a
    /// [`PendingFilter::Expr`] (an aliased `ExprCompare`) and registers
    /// the FK-chain JOINs. Reaching [`resolve_one_pending`] still as a
    /// `RelationSpan` means a non-SELECT path (update / delete /
    /// aggregate) — unsupported there in P1, so it errors clearly.
    RelationSpan { raw_key: String, value: SqlValue },
    /// Deferred error surfaced at `compile()` time. Used by
    /// [`QuerySet::filter`] (issue #71) when the lookup-suffix
    /// parser fails — keeps the builder API non-Result while
    /// surfacing the cause at the natural error-checking point.
    Error(QueryError),
}

#[derive(Debug, Clone)]
struct RawFilter {
    field: String,
    op: Op,
    value: SqlValue,
}

#[derive(Debug, Clone)]
struct DateTransformFilter {
    field: String,
    transform: ScalarFn,
    op: Op,
    value: SqlValue,
}

#[derive(Debug, Clone)]
struct RawAssignment {
    field: String,
    value: SqlValue,
}

/// Staged `field = <expression>` assignment for the [`F()`]-shaped
/// SET path. `field` is the Rust-side name resolved against the
/// schema at `compile()` time; `value` is an [`crate::core::Expr`]
/// tree that may contain column refs + arithmetic. Resolves to
/// [`crate::core::Assignment`] in `resolve_assignment_expr`.
///
/// [`F()`]: crate::core::F
#[derive(Debug, Clone)]
struct RawExprAssignment {
    field: String,
    value: crate::core::Expr,
}

impl<T: Model> Default for QuerySet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Model> Clone for QuerySet<T> {
    fn clone(&self) -> Self {
        Self {
            pending: self.pending.clone(),
            limit: self.limit,
            offset: self.offset,
            distinct: self.distinct.clone(),
            select_related: self.select_related.clone(),
            ad_hoc_joins: self.ad_hoc_joins.clone(),
            subquery_joins: self.subquery_joins.clone(),
            order_by: self.order_by.clone(),
            lock_mode: self.lock_mode.clone(),
            compound: self.compound.clone(),
            head_order_by: self.head_order_by.clone(),
            head_limit: self.head_limit,
            head_offset: self.head_offset,
            is_none: self.is_none,
            disabled_global_scopes: self.disabled_global_scopes.clone(),
            disable_all_global_scopes: self.disable_all_global_scopes,
            _model: std::marker::PhantomData,
        }
    }
}

impl<T: Model> QuerySet<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            limit: None,
            offset: None,
            distinct: None,
            select_related: Vec::new(),
            ad_hoc_joins: Vec::new(),
            subquery_joins: Vec::new(),
            order_by: Vec::new(),
            lock_mode: None,
            compound: Vec::new(),
            head_order_by: Vec::new(),
            head_limit: None,
            head_offset: None,
            is_none: false,
            disabled_global_scopes: Vec::new(),
            disable_all_global_scopes: false,
            _model: PhantomData,
        }
    }

    /// Issue #820 — Eloquent `Model::query()->withoutGlobalScope($name)`.
    /// Suppress one named entry from
    /// [`crate::core::ModelSchema::global_scopes`] on this queryset.
    /// Repeated calls accumulate — pass each name to suppress.
    ///
    /// Unknown names are silently ignored (matches Eloquent + lets
    /// downstream code add scopes without breaking call sites that
    /// pre-emptively opt out of names that don't exist yet).
    ///
    /// No-op when the model declares no global scopes.
    #[must_use]
    pub fn without_global_scope(mut self, name: &'static str) -> Self {
        if !self.disabled_global_scopes.contains(&name) {
            self.disabled_global_scopes.push(name);
        }
        self
    }

    /// Issue #820 — Eloquent `Model::query()->withoutGlobalScopes()`.
    /// Suppress *every* scope from
    /// [`crate::core::ModelSchema::global_scopes`] on this queryset
    /// — useful for admin or migration code paths that need to see
    /// soft-deleted / unpublished / cross-tenant rows the application
    /// hides by default.
    ///
    /// Composes with later [`Self::without_global_scope`] calls
    /// without changing the wholesale opt-out (once disabled,
    /// everything stays disabled on that queryset).
    #[must_use]
    pub fn without_global_scopes(mut self) -> Self {
        self.disable_all_global_scopes = true;
        self
    }

    /// Issue #820 — fold the schema-declared global scopes into the
    /// pending filter list, respecting per-queryset opt-outs. Called
    /// at the top of every `compile*` entry point so the WHERE walks
    /// the scope expressions through the same resolver as any other
    /// typed filter.
    ///
    /// The method **prepends** scope filters in declared order so the
    /// final WHERE reads `scope_a AND scope_b AND <user filters>`.
    /// `AND` is commutative; the order matters only for predicate-
    /// trace readability in `EXPLAIN`.
    ///
    /// No-op when the model carries no scopes or when
    /// `disable_all_global_scopes` is set.
    fn apply_global_scopes(&mut self) {
        if self.disable_all_global_scopes {
            return;
        }
        let schema = T::SCHEMA;
        if schema.global_scopes.is_empty() {
            return;
        }
        let mut prefixed: Vec<PendingFilter> = Vec::new();
        for scope in schema.global_scopes {
            if self.disabled_global_scopes.contains(&scope.name) {
                continue;
            }
            let expr = (scope.apply)();
            prefixed.push(PendingFilter::Expr(expr));
        }
        if !prefixed.is_empty() {
            prefixed.append(&mut self.pending);
            self.pending = prefixed;
        }
    }

    /// Django `.none()` — return a queryset guaranteed to match zero
    /// rows. Useful as a safe base for conditional filters
    /// (`if !user.has_perm: qs = qs.none()`) and for typed pipelines
    /// that must return an empty result deterministically.
    ///
    /// SELECTs compile to `LIMIT 0`; UPDATE / DELETE add an `IS NULL`
    /// predicate against the (NOT NULL) primary key so the DB rejects
    /// every row. Issue #331.
    #[must_use]
    pub fn none(mut self) -> Self {
        self.is_none = true;
        self
    }

    /// Django `.distinct()` — emit `SELECT DISTINCT ...`. Works on
    /// every dialect identically. Pair with `.order_by(...)` to
    /// disambiguate which row survives among duplicates (DB doesn't
    /// guarantee an order otherwise).
    ///
    /// For PG-shape "DISTINCT ON (cols)" (latest-per-group), use
    /// [`Self::distinct_on`] which is portable across all three
    /// backends via a window-function fallback.
    ///
    /// Issue #264 / T1.2.
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = Some(crate::core::DistinctMode::All);
        self
    }

    /// Django `.distinct(*fields)` — emit `SELECT DISTINCT ON (col1, col2)`
    /// on PG natively; lower to a `ROW_NUMBER() OVER (PARTITION BY cols
    /// ORDER BY <existing order_by>)` subquery wrapper on MySQL / SQLite.
    /// In all three the result is "first row per group", where "first"
    /// is determined by the queryset's `ORDER BY`.
    ///
    /// **Constraint**: the columns passed must appear at the head of
    /// `.order_by(...)` (matches Django's runtime check). `.compile()`
    /// returns [`QueryError::DistinctOnOrderBy`] otherwise — the order
    /// is what makes the "first row" deterministic.
    ///
    /// Empty `fields` is rejected (would degenerate to `.distinct()`).
    ///
    /// Issue #264 / T1.2.
    #[must_use]
    pub fn distinct_on(mut self, fields: &[&'static str]) -> Self {
        self.distinct = Some(crate::core::DistinctMode::On(fields.to_vec()));
        self
    }

    /// Issue #21 — Django's `QuerySet.select_for_update(skip_locked=,
    /// nowait=, of=, no_key=)`. Emits `SELECT … FOR UPDATE` (PG /
    /// MySQL 8+) or no-ops (SQLite has no row-lock syntax;
    /// transactions hold an implicit write lock for the whole DB).
    ///
    /// Must run inside a transaction — `FOR UPDATE` outside a tx is
    /// a no-op on PG and an error on MySQL. Acquire one via
    /// [`crate::sql::transaction`] / [`crate::sql::transaction_pg`]
    /// and call `.fetch_on(&mut *tx)` or `.fetch(&mut *tx)`.
    ///
    /// Default options ([`crate::core::LockMode::default`]) emit
    /// plain `FOR UPDATE`. Use the chained variants below to set
    /// individual flags.
    #[must_use]
    pub fn select_for_update(mut self) -> Self {
        self.lock_mode = Some(crate::core::LockMode::default());
        self
    }

    /// Eloquent `Builder::lockForUpdate()` — bare-name alias of
    /// [`Self::select_for_update`] matching Laravel muscle memory.
    /// Same `FOR UPDATE` semantics: must run inside a transaction
    /// (PG / MySQL); SQLite no-ops because it has no row-level lock
    /// syntax.
    #[must_use]
    pub fn lock_for_update(self) -> Self {
        self.select_for_update()
    }

    /// PG / MySQL 8+: append `SKIP LOCKED` to the lock clause —
    /// "claim next available row" pattern. Rows currently locked by
    /// another transaction are silently filtered out instead of
    /// blocking. No effect on SQLite. Implies
    /// [`Self::select_for_update`] if not already set.
    #[must_use]
    pub fn skip_locked(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.skip_locked = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG / MySQL 8+: append `NOWAIT` — return a driver error
    /// immediately if any matching row is currently locked. Mutually
    /// exclusive with `skip_locked` at the database; the writer
    /// emits `SKIP LOCKED` (the more permissive option) when both
    /// are set. Implies [`Self::select_for_update`] if not already set.
    #[must_use]
    pub fn nowait(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.nowait = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG 9.3+: `FOR NO KEY UPDATE` instead of `FOR UPDATE` — weaker
    /// lock that doesn't block other writers that aren't touching
    /// the row's PK / unique columns. Useful for high-concurrency
    /// update paths where the surrounding `UPDATE` doesn't change
    /// indexed columns. MySQL has no equivalent; the writer falls
    /// back to `FOR UPDATE` (the stricter lock).
    #[must_use]
    pub fn no_key(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.no_key = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG 9.3+ / MySQL 8.0.1+: `FOR UPDATE OF table1, table2, …` —
    /// restrict the row lock to the named tables / aliases when the
    /// query JOINs. Without `OF` the lock applies to every row of
    /// every joined table the result references. SQLite no-op.
    ///
    /// Each call appends. Pass either base table names or join
    /// aliases.
    #[must_use]
    pub fn of(mut self, tables: &[&'static str]) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.of.extend_from_slice(tables);
        self.lock_mode = Some(lock);
        self
    }

    /// Suppress the SQLite-no-locking warning emitted by the writer
    /// when this queryset's lock modifiers land against a SQLite pool.
    /// Issue #290 / T2.9.
    ///
    /// SQLite has no `FOR UPDATE` row-level lock syntax — locks are
    /// silently dropped, and the writer logs `tracing::warn!` by
    /// default so users debugging concurrency bugs against a sqlite-
    /// backed test fixture see the no-op. Call this on querysets
    /// where you know the SQLite global writer lock is sufficient
    /// (single-writer apps, test fixtures).
    ///
    /// No effect on PG / MySQL — locks emit normally there.
    #[must_use]
    pub fn silent_on_sqlite(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.silent_on_sqlite = true;
        self.lock_mode = Some(lock);
        self
    }

    /// Issue #25 — Django's `QuerySet.union(other_qs)`. Combines this
    /// queryset with `other` via SQL `UNION` (deduplicates). Both
    /// querysets must target the same model `T`; the column shape is
    /// guaranteed identical at compile time by the generic bound.
    ///
    /// Multiple `.union()` / `.union_all()` calls accumulate — every
    /// call appends a new branch. Mixing `union` with `intersection`
    /// or `difference` on the same chain is allowed but unusual; SQL
    /// evaluates them left-to-right.
    ///
    /// `.order_by()` / `.limit()` / `.offset()` set AFTER `.union()`
    /// apply to the COMBINED result (the merged resultset); the same
    /// methods called BEFORE `.union()` scope to the FIRST queryset —
    /// Django 4.0+ component-queryset slicing (#1034). Each argument
    /// branch likewise keeps its own per-branch ORDER BY / LIMIT. Every
    /// such component wraps in an aliased derived table
    /// (`SELECT * FROM (…) AS __rustango_bN`) so the clauses stay local
    /// on all three backends.
    ///
    /// Tri-dialect availability: every supported backend.
    ///
    /// # Panics
    /// If `other.compile()` fails (typo'd column on the branch's
    /// WHERE / ORDER BY, schema validation, etc.). Same posture as
    /// `.where_()` on the typed path — a malformed branch is a
    /// programmer error, not a runtime data condition. For fallible
    /// composition that surfaces the branch error as a `Result`,
    /// pre-compile and pass via [`Self::with_compound`].
    #[must_use]
    pub fn union(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Union, other)
    }

    /// `UNION ALL` — combine without deduplicating. Cheaper than
    /// `union` because the DB skips the DISTINCT pass; useful when
    /// you know the branches don't overlap.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn union_all(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::UnionAll, other)
    }

    /// Issue #25 — Django's `QuerySet.intersection(other_qs)`. Rows
    /// present in BOTH this queryset and `other`. Tri-dialect:
    /// Postgres, SQLite, MySQL 8.0.31+.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn intersection(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Intersection, other)
    }

    /// Issue #25 — Django's `QuerySet.difference(other_qs)`. Emits
    /// `EXCEPT`: rows in this queryset but NOT in `other`.
    /// Tri-dialect: Postgres, SQLite, MySQL 8.0.31+.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn difference(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Difference, other)
    }

    /// Fallible set-algebra entry point: takes a pre-compiled
    /// `SelectQuery` as the branch instead of a fresh `QuerySet`.
    /// Useful when the branch construction may fail (its `.compile()`
    /// returns `Result`) and the caller wants to surface the error
    /// before chaining. Generic over the operator — covers `union` /
    /// `union_all` / `intersection` / `difference` with one entry
    /// point. Issue #25.
    ///
    /// ```ignore
    /// // Caller wants to handle the branch's compile error explicitly:
    /// let branch = Post::objects()
    ///     .filter("typo_field__lt", 100_i64)  // may fail
    ///     .compile()?;                         // → Result
    /// let qs = Post::objects()
    ///     .where_(Post::active.eq(true))
    ///     .with_compound(SetOp::Union, branch);
    /// ```
    #[must_use]
    pub fn with_compound(self, op: crate::core::SetOp, branch: crate::core::SelectQuery) -> Self {
        self.add_compound_compiled(op, branch)
    }

    /// Shared lowering for [`Self::union`] / [`Self::union_all`] /
    /// [`Self::intersection`] / [`Self::difference`] — compiles the
    /// branch eagerly and appends a `CompoundBranch` to
    /// `self.compound`. Panics on branch compile error; for fallible
    /// composition use [`Self::with_compound`].
    fn add_compound(self, op: crate::core::SetOp, other: QuerySet<T>) -> Self {
        match other.compile() {
            Ok(branch) => self.add_compound_compiled(op, branch),
            Err(e) => panic!(
                "rustango: set-algebra branch failed to compile: {e}. \
                 Pre-compile the branch and pass via .with_compound(op, \
                 branch) to surface this error as a Result."
            ),
        }
    }

    fn add_compound_compiled(
        mut self,
        op: crate::core::SetOp,
        branch: crate::core::SelectQuery,
    ) -> Self {
        // #1034 — the FIRST set-op call freezes the pre-union
        // `ORDER BY` / `LIMIT` / `OFFSET` into the head-branch slots
        // (those clauses scope to the first queryset, Django 4.0+
        // component-slicing). Clearing the live slots means anything
        // chained AFTER this point accumulates fresh and applies to
        // the combined result. Subsequent set-op calls leave the
        // already-frozen head untouched.
        if self.compound.is_empty() {
            self.head_order_by = std::mem::take(&mut self.order_by);
            self.head_limit = self.limit.take();
            self.head_offset = self.offset.take();
        }
        self.compound.push(crate::core::CompoundBranch {
            op,
            query: Box::new(branch),
        });
        self
    }

    /// Append `ORDER BY` columns. Slice 9.0b.
    ///
    /// Each entry is a `(field_name, desc)` pair where `field_name`
    /// is a Rust-side field on the model — schema validation runs at
    /// `compile()` time. Multiple `.order_by(...)` calls compose;
    /// subsequent calls append after earlier ones (left-to-right
    /// precedence).
    ///
    /// ```ignore
    /// let posts = Post::objects()
    ///     .order_by(&[("published_at", true)])  // newest first
    ///     .fetch_on(conn).await?;
    /// ```
    ///
    /// To sort by multiple columns:
    ///
    /// ```ignore
    /// .order_by(&[("category", false), ("published_at", true)])
    /// ```
    #[must_use]
    pub fn order_by(mut self, items: &[(&str, bool)]) -> Self {
        for (field, desc) in items {
            self.order_by.push(PendingOrderItem::Field {
                name: (*field).to_owned(),
                desc: *desc,
                nulls: crate::core::NullsOrder::Default,
            });
        }
        self
    }

    /// Eloquent `Builder::reorder([col, asc?])` — **replace** the
    /// accumulated `ORDER BY` list (instead of appending like
    /// [`Self::order_by`]). Useful when overriding a default
    /// ordering inherited from a base queryset / manager:
    ///
    /// ```ignore
    /// let qs = Post::objects()
    ///     .with_default_order()                          // model's default
    ///     .reorder(&[("created_at", true)]);             // wipes default
    /// ```
    ///
    /// Equivalent to `.clear_order_by().order_by(items)`.
    /// Eloquent's `Builder::reorder()` with no args clears every
    /// sort key — call with an empty slice for the same shape:
    /// `.reorder(&[])`.
    #[must_use]
    pub fn reorder(mut self, items: &[(&str, bool)]) -> Self {
        self.order_by.clear();
        for (field, desc) in items {
            self.order_by.push(PendingOrderItem::Field {
                name: (*field).to_owned(),
                desc: *desc,
                nulls: crate::core::NullsOrder::Default,
            });
        }
        self
    }

    /// Single-column shortcut for `ORDER BY <col> DESC`. Appends to the
    /// existing ORDER BY list (use [`Self::reorder`] first to replace).
    ///
    /// Eloquent ships this as `Builder::latest($column)` but rustango's
    /// [`Self::latest`] is the Django-style async fetcher — so the
    /// chainable form lives here under the explicit name.
    ///
    /// ```ignore
    /// Post::objects().order_by_desc("created_at").fetch(&pool).await?;
    /// // SELECT ... FROM post ORDER BY created_at DESC
    /// ```
    #[must_use]
    pub fn order_by_desc(self, column: &str) -> Self {
        self.order_by(&[(column, true)])
    }

    /// Single-column shortcut for `ORDER BY <col> ASC`. Appends to the
    /// existing ORDER BY list.
    ///
    /// Counterpart of [`Self::order_by_desc`]; mirrors Eloquent's
    /// `Builder::oldest($column)`.
    ///
    /// ```ignore
    /// Post::objects().order_by_asc("created_at").fetch(&pool).await?;
    /// // SELECT ... FROM post ORDER BY created_at ASC
    /// ```
    #[must_use]
    pub fn order_by_asc(self, column: &str) -> Self {
        self.order_by(&[(column, false)])
    }

    /// Apply the schema-declared `default_order` for `T`. Issue #291
    /// / T2.5. Each entry from `#[rustango(default_order = "...")]`
    /// gets pre-pended to the queryset's pending ORDER BY list, so a
    /// later `.order_by(...)` call appends secondary sort keys.
    ///
    /// **Per-query opt-in by design** — unlike Django's
    /// `Meta.ordering`, the default ordering is NOT applied
    /// automatically. Every queryset starts unsorted; chain
    /// `.with_default_order()` when you want the schema default. This
    /// avoids the Django footgun where `.count()` / `.exists()` /
    /// `.delete()` pay for a sort they don't need.
    ///
    /// No-op when the model carries no `default_order`. Idempotent
    /// (calling it twice doesn't duplicate the entries).
    #[must_use]
    pub fn with_default_order(mut self) -> Self {
        let model = T::SCHEMA;
        // Build the new entries first so we can pre-pend them.
        let new_entries: Vec<PendingOrderItem> = model
            .default_order
            .iter()
            .map(|(name, desc)| PendingOrderItem::Field {
                name: (*name).to_owned(),
                desc: *desc,
                nulls: crate::core::NullsOrder::Default,
            })
            .collect();
        if new_entries.is_empty() {
            return self;
        }
        // Idempotency check: if the head of `order_by` already matches
        // the model's default sequence, skip re-prepending.
        let already_applied = self.order_by.len() >= new_entries.len()
            && self
                .order_by
                .iter()
                .zip(new_entries.iter())
                .all(|(a, b)| match (a, b) {
                    (
                        PendingOrderItem::Field {
                            name: an, desc: ad, ..
                        },
                        PendingOrderItem::Field {
                            name: bn, desc: bd, ..
                        },
                    ) => an == bn && ad == bd,
                    _ => false,
                });
        if already_applied {
            return self;
        }
        // Pre-pend.
        let mut combined = new_entries;
        combined.extend(self.order_by.drain(..));
        self.order_by = combined;
        self
    }

    /// Clear every accumulated `ORDER BY` entry on this queryset.
    /// Issue #291 / T2.5. Resets both legacy `.order_by(...)` columns
    /// and `.order_by_expr(...)` / `.with_default_order()` entries
    /// from earlier in the chain. Useful for explicitly bypassing a
    /// queryset's default order when re-borrowing from a builder
    /// helper that already chained `.with_default_order()`.
    #[must_use]
    pub fn unordered(mut self) -> Self {
        self.order_by.clear();
        self
    }

    /// Append `ORDER BY` columns with explicit `NULLS FIRST|LAST`
    /// control. Issue #76. PG + SQLite emit the `NULLS …` keyword
    /// natively; MySQL emulates via `<col> IS NULL` pre-sort
    /// because it has no `NULLS …` syntax.
    ///
    /// ```ignore
    /// use rustango::core::NullsOrder;
    /// Post::objects()
    ///     .order_by_with_nulls(&[("published_at", true, NullsOrder::Last)])
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// Composes with `.order_by(...)` — legacy `(name, desc)` entries
    /// Composes with `.order_by(...)` — entries from both methods
    /// appear in the SQL `ORDER BY` clause in **registration order**
    /// across the chain.
    #[must_use]
    pub fn order_by_with_nulls(
        mut self,
        items: &[(&'static str, bool, crate::core::NullsOrder)],
    ) -> Self {
        for (field, desc, nulls) in items {
            self.order_by.push(PendingOrderItem::Field {
                name: (*field).to_owned(),
                desc: *desc,
                nulls: *nulls,
            });
        }
        self
    }

    /// Append an `ORDER BY` item whose target is an arbitrary
    /// [`Expr`] — `lower(F("title"))`, `case(...)`, `F("a") + F("b")`,
    /// any builder result that lowers to `Expr`. Issue #76.
    ///
    /// `desc = true` → `DESC`; defaults to the dialect's native
    /// NULL ordering (use [`Self::order_by_expr_with_nulls`] to pin).
    ///
    /// [`Expr`]: crate::core::Expr
    #[must_use]
    pub fn order_by_expr(mut self, expr: impl Into<crate::core::Expr>, desc: bool) -> Self {
        self.order_by.push(PendingOrderItem::Expr {
            expr: expr.into(),
            desc,
            nulls: crate::core::NullsOrder::Default,
        });
        self
    }

    /// Order rows by pgvector similarity to `query` over the `column`
    /// vector column — Eloquent 13's `whereVectorSimilarTo(...)` auto-
    /// ordering (#824). Ascending distance, so the **nearest** rows come
    /// first (pgvector's `<#>` returns the negative inner product, so
    /// ascending ranks most-similar first for every metric).
    ///
    /// Emits `ORDER BY <column> <op> $vec` where `<op>` is the pgvector
    /// distance operator for `metric` (`<->` / `<=>` / `<#>`). **PG-only**
    /// — the writer raises `OpNotSupportedInDialect` on MySQL / SQLite.
    /// Pair with `.limit(k)` (or use [`Self::k_nearest`]) for a k-NN
    /// query. The column should be a [`crate::sql::Vector`] /
    /// `#[rustango(vector(dims = N))]` field.
    ///
    /// ```ignore
    /// use rustango::core::VectorMetric;
    /// // 5 nearest documents to `query_embedding` by cosine distance.
    /// let hits = Doc::objects()
    ///     .order_by_distance("embedding", query_embedding, VectorMetric::Cosine)
    ///     .limit(5)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn order_by_distance(
        self,
        column: &'static str,
        query: Vec<f32>,
        metric: crate::core::VectorMetric,
    ) -> Self {
        let expr = crate::core::Expr::BinOp {
            left: Box::new(crate::core::Expr::Column(column)),
            op: metric.to_binop(),
            right: Box::new(crate::core::Expr::Literal(SqlValue::Vector(query))),
        };
        self.order_by_expr(expr, /*desc=*/ false)
    }

    /// k-nearest-neighbour shortcut: [`Self::order_by_distance`] followed
    /// by `.limit(k)` — the `k` rows closest to `query` under `metric`.
    /// Issue #824, **PG-only**.
    ///
    /// ```ignore
    /// use rustango::core::VectorMetric;
    /// let top3 = Doc::objects()
    ///     .k_nearest("embedding", query_embedding, 3, VectorMetric::L2)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn k_nearest(
        self,
        column: &'static str,
        query: Vec<f32>,
        k: i64,
        metric: crate::core::VectorMetric,
    ) -> Self {
        self.order_by_distance(column, query, metric).limit(k)
    }

    /// PostGIS nearest-first ordering (#58): `ORDER BY
    /// ST_Distance(<column>, <point>)` ascending. Pair with `.limit(k)`
    /// for a k-nearest query. `column` should be a `geometry(Point, …)`
    /// / `#[rustango(geometry(...))]` field. **PG/PostGIS-only** — emits
    /// `OpNotSupportedInDialect` at compile time on MySQL / SQLite.
    ///
    /// ```ignore
    /// // The 5 places nearest to `here`.
    /// let near = Place::objects()
    ///     .order_by_distance_to("location", here)
    ///     .limit(5)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn order_by_distance_to(self, column: &'static str, point: crate::sql::Point) -> Self {
        let expr = crate::core::funcs::st_distance(crate::core::Expr::Column(column), point);
        self.order_by_expr(expr, /*desc=*/ false)
    }

    /// PostGIS "within radius" filter (#58): `WHERE ST_DWithin(<column>,
    /// <point>, <distance>)`. `distance` is in the column's SRID units
    /// (degrees for SRID 4326 — cast to `::geography` in raw SQL when you
    /// need metres). **PG/PostGIS-only.**
    ///
    /// ```ignore
    /// // Places within ~1km (in degrees) of `here`.
    /// let nearby = Place::objects()
    ///     .filter_dwithin("location", here, 0.01)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn filter_dwithin(
        self,
        column: &'static str,
        point: crate::sql::Point,
        distance: f64,
    ) -> Self {
        let pred = WhereExpr::ExprCompare {
            lhs: crate::core::funcs::st_dwithin(
                crate::core::Expr::Column(column),
                point,
                crate::core::Expr::Literal(SqlValue::F64(distance)),
            ),
            op: Op::Eq,
            rhs: crate::core::Expr::Literal(SqlValue::Bool(true)),
        };
        self.where_raw(pred)
    }

    /// Same as [`Self::order_by_expr`] but with an explicit
    /// `NullsOrder`. Issue #76.
    #[must_use]
    pub fn order_by_expr_with_nulls(
        mut self,
        expr: impl Into<crate::core::Expr>,
        desc: bool,
        nulls: crate::core::NullsOrder,
    ) -> Self {
        self.order_by.push(PendingOrderItem::Expr {
            expr: expr.into(),
            desc,
            nulls,
        });
        self
    }

    /// `ORDER BY RANDOM()` (PG / SQLite) or `ORDER BY RAND()` (MySQL)
    /// — Django's `.order_by('?')`. Useful for "random N rows" UI
    /// patterns like banner rotation, sample selection, A/B-test
    /// bucket assignment. Issue #77.
    ///
    /// ```ignore
    /// // Three random posts.
    /// Post::objects()
    ///     .order_random()
    ///     .limit(3)
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// **Performance caveat**: random ordering forces a full table
    /// scan + in-memory sort by a per-row random key. The query
    /// planner can't use an index. For tables much larger than memory,
    /// prefer the `WHERE pk >= <random_offset> LIMIT N` pattern
    /// (which can range-scan an index) and accept that adjacency in
    /// the result rows mirrors PK adjacency.
    ///
    /// Composes with other `.order_by*` calls — the random key sorts
    /// rows whose preceding sort columns tied. Most callers want a
    /// `.replace_order_by`-style reset first.
    #[must_use]
    pub fn order_random(mut self) -> Self {
        self.order_by.push(PendingOrderItem::Random);
        self
    }

    /// Eloquent `Builder::inRandomOrder()` alias for
    /// [`Self::order_random`] — appends a per-row random sort key.
    /// Same performance caveat (full scan + sort, no index use).
    ///
    /// ```ignore
    /// // Eloquent: Post::query()->inRandomOrder()->take(5)->get();
    /// let five = Post::objects()
    ///     .in_random_order()
    ///     .take(5)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn in_random_order(self) -> Self {
        self.order_random()
    }

    /// v0.45 — discard any previously-set `order_by` and apply
    /// `items` as the new ordering. Used by `earliest` and
    /// `latest` which declare their own sort. Clears every
    /// pending item (legacy + `_with_nulls` + `_expr`).
    #[must_use]
    pub fn replace_order_by(mut self, items: &[(&str, bool)]) -> Self {
        self.order_by.clear();
        self.order_by(items)
    }

    /// Django-shape `QuerySet.reverse()` — flip the direction of every
    /// pending `ORDER BY` clause. Each `Field { desc, .. }` and
    /// `Expr { desc, .. }` has its `desc` flag toggled; `Random` is
    /// untouched (no direction to invert). Issue #325.
    ///
    /// No-op when no ordering is set — Django's `reverse()` likewise
    /// does nothing in that case (the resulting iteration order is
    /// implementation-defined either way).
    ///
    /// ```ignore
    /// // newest first
    /// let q = Post::objects()
    ///     .order_by(&[("published_at", true)]);
    /// // oldest first — same predicate set, flipped sort
    /// let r = q.reverse();
    /// ```
    #[must_use]
    pub fn reverse(mut self) -> Self {
        for item in &mut self.order_by {
            match item {
                PendingOrderItem::Field { desc, .. } | PendingOrderItem::Expr { desc, .. } => {
                    *desc = !*desc
                }
                PendingOrderItem::Random => {}
            }
        }
        self
    }

    /// v0.45 — flip every ordering direction in place. Used by
    /// `last` to invert the queryset's natural sort and take
    /// the first row from the reversed sequence — avoids OFFSET +
    /// COUNT(*) and works on every dialect. Issue #76: also swaps
    /// `NullsOrder::First` ↔ `NullsOrder::Last` so the "NULLs at the
    /// same logical end as before" semantic survives an inversion.
    #[must_use]
    pub fn flip_order_by(mut self) -> Self {
        for entry in &mut self.order_by {
            match entry {
                PendingOrderItem::Field { desc, nulls, .. }
                | PendingOrderItem::Expr { desc, nulls, .. } => {
                    *desc = !*desc;
                    *nulls = match *nulls {
                        crate::core::NullsOrder::First => crate::core::NullsOrder::Last,
                        crate::core::NullsOrder::Last => crate::core::NullsOrder::First,
                        crate::core::NullsOrder::Default => crate::core::NullsOrder::Default,
                    };
                }
                // Random has no direction or NULLS clause — nothing
                // to flip. Issue #77.
                PendingOrderItem::Random => {}
            }
        }
        self
    }

    /// v0.45 — count of registered `ORDER BY` items. Used by
    /// `ensure_pk_ordering` (executor) to detect "no ordering set"
    /// without inspecting the variant types. Issue #76 dropped the
    /// `order_by_clauses() -> &[(String, bool)]` getter because the
    /// unified pending list now carries `Expr` items that can't
    /// flatten to that shape.
    #[must_use]
    pub fn has_order_by(&self) -> bool {
        !self.order_by.is_empty()
    }

    /// Eagerly load a `ForeignKey<Parent>` field via a `LEFT JOIN` —
    /// Django's `select_related`. Pass the field name on `T` (not the
    /// FK column or the parent table); subsequent `fetch_on` returns
    /// rows where each `ForeignKey<Parent>` is `Loaded` after a
    /// **single** SQL query, no N+1.
    ///
    /// ```ignore
    /// let posts: Vec<Post> = Post::objects()
    ///     .select_related("author")
    ///     .fetch_on(conn).await?;
    /// // post.author is ForeignKey::Loaded { pk, value }
    /// ```
    ///
    /// Multiple `.select_related()` calls compose: each adds another
    /// `LEFT JOIN` to the same SELECT. Schema validation (the field
    /// exists, is an FK, has a primary-key target) happens at
    /// `compile()` time.
    #[must_use]
    pub fn select_related(mut self, field: impl Into<String>) -> Self {
        self.select_related.push(field.into());
        self
    }

    /// Ad-hoc JOIN — issue #80. Append a fully specified
    /// [`crate::core::Join`] to the queryset. Unlike [`select_related`]
    /// (which auto-builds joins from FK metadata), this gives the
    /// caller full control over the JOIN kind, alias, predicate, and
    /// projected columns. The predicate is an arbitrary [`WhereExpr`];
    /// columns inside it qualify against the joined alias by default
    /// and against arbitrary aliases via [`crate::core::Expr::AliasedColumn`].
    ///
    /// Multiple `.join(...)` calls compose; each appends another JOIN
    /// after the FK-driven `select_related` ones. Aliases must be
    /// unique within the queryset.
    ///
    /// ```ignore
    /// use rustango::core::joins::aliased;
    /// use rustango::core::{Join, JoinKind, Op, WhereExpr};
    ///
    /// Post::objects()
    ///     .join(Join {
    ///         target: Comment::SCHEMA,
    ///         alias: "c",
    ///         kind: JoinKind::Inner,
    ///         on: WhereExpr::And(vec![
    ///             WhereExpr::ExprCompare {
    ///                 lhs: aliased("c", "post_id"),
    ///                 op: Op::Eq,
    ///                 rhs: aliased("post", "id"),
    ///             },
    ///             // Filter columns inside `on` qualify to the joined
    ///             // alias (`c`) by default — no need to `aliased()` here.
    ///             Comment::is_approved.eq(true).into(),
    ///         ]),
    ///         project: vec![],
    ///     })
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// [`select_related`]: Self::select_related
    /// [`WhereExpr`]: crate::core::WhereExpr
    #[must_use]
    pub fn join(mut self, join: crate::core::Join) -> Self {
        self.ad_hoc_joins.push(join);
        self
    }

    /// `INNER JOIN (<subquery>) AS <alias> ON <on>` — join against a
    /// **derived table**. Eloquent's `joinSub`. Issue #828.
    ///
    /// `sub` is a compiled `SelectQuery` (`Other::objects().….compile()?`).
    /// `on` references the derived table as `"<alias>"."<col>"` and the
    /// outer table by its own name — both via
    /// [`crate::core::joins::aliased`]. The derived table contributes no
    /// columns to the SELECT, so a typed fetch still returns the base
    /// model (this is a filtering / relating join). Portable across
    /// PG / MySQL / SQLite.
    ///
    /// ```ignore
    /// use rustango::core::joins::aliased;
    /// use rustango::core::{Op, WhereExpr};
    ///
    /// // Customers who have placed at least one order.
    /// let recent = Order::objects()
    ///     .values(&["customer_id"])      // derived table: distinct customer ids
    ///     .compile()?;
    /// let with_orders = Customer::objects()
    ///     .join_sub(recent, "o", WhereExpr::ExprCompare {
    ///         lhs: aliased("o", "customer_id"),
    ///         op: Op::Eq,
    ///         rhs: aliased("customer", "id"),
    ///     })
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn join_sub(self, sub: SelectQuery, alias: &'static str, on: WhereExpr) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Inner,
            on,
            /*lateral=*/ false,
        )
    }

    /// `LEFT JOIN (<subquery>) AS <alias> ON <on>` — left-join counterpart
    /// of [`Self::join_sub`] (Eloquent `leftJoinSub`). Issue #828.
    #[must_use]
    pub fn left_join_sub(self, sub: SelectQuery, alias: &'static str, on: WhereExpr) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Left,
            on,
            /*lateral=*/ false,
        )
    }

    /// `INNER JOIN LATERAL (<subquery>) AS <alias> ON <on>` — Eloquent
    /// `joinLateral`. Issue #828. The subquery may reference columns from
    /// the outer query (the "top-N rows per group" shape); put that
    /// correlation in the subquery's own `WHERE` (via
    /// [`crate::core::joins::aliased`] against the outer table) and pass
    /// `WhereExpr::And(vec![])` as `on` to emit `ON true`.
    ///
    /// **PostgreSQL / MySQL ≥ 8.0.14 only** — SQLite has no `LATERAL` and
    /// the writer raises [`crate::sql::SqlError::LateralJoinNotSupported`]
    /// at compile time.
    #[must_use]
    pub fn join_lateral(self, sub: SelectQuery, alias: &'static str, on: WhereExpr) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Inner,
            on,
            /*lateral=*/ true,
        )
    }

    /// `LEFT JOIN LATERAL (<subquery>) AS <alias> ON <on>` — left-join
    /// counterpart of [`Self::join_lateral`] (Eloquent `leftJoinLateral`).
    /// Keeps every outer row even when the lateral subquery yields none
    /// (the natural shape for "latest order per customer, customers
    /// without orders included"). **PG / MySQL only.** Issue #828.
    #[must_use]
    pub fn left_join_lateral(self, sub: SelectQuery, alias: &'static str, on: WhereExpr) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Left,
            on,
            /*lateral=*/ true,
        )
    }

    /// Shared builder for the four derived-table join methods.
    #[must_use]
    fn push_subquery_join(
        mut self,
        sub: SelectQuery,
        alias: &'static str,
        kind: crate::core::JoinKind,
        on: WhereExpr,
        lateral: bool,
    ) -> Self {
        self.subquery_joins.push(crate::core::SubqueryJoin {
            subquery: Box::new(sub),
            alias,
            kind,
            on,
            lateral,
        });
        self
    }

    /// Cap the number of returned rows. `None` removes any previously set limit.
    #[must_use]
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows. Pair with [`limit`](Self::limit) for paging.
    #[must_use]
    pub fn offset(mut self, n: i64) -> Self {
        self.offset = Some(n);
        self
    }

    /// Eloquent `Builder::take($n)` — alias of [`Self::limit`].
    /// Cap the result to at most `n` rows.
    #[must_use]
    pub fn take(self, n: i64) -> Self {
        self.limit(n)
    }

    /// Eloquent `Builder::skip($n)` — alias of [`Self::offset`].
    /// Skip the first `n` matching rows. Pair with `.take(...)` /
    /// `.limit(...)` for manual paging.
    #[must_use]
    pub fn skip(self, n: i64) -> Self {
        self.offset(n)
    }

    /// Append a `WHERE field <op> value` predicate using the
    /// **explicit-op** shape. This is the lower-level form used by
    /// callers that already know which `Op` they want; for the
    /// Django muscle-memory `filter("field__lookup", value)` shape
    /// see [`Self::filter`] (issue #71).
    ///
    /// `field` is the Rust-side field name; the column is looked up
    /// from the schema at compile time.
    #[must_use]
    pub fn filter_op(
        mut self,
        field: impl Into<String>,
        op: Op,
        value: impl Into<SqlValue>,
    ) -> Self {
        self.pending.push(PendingFilter::Raw(RawFilter {
            field: field.into(),
            op,
            value: value.into(),
        }));
        self
    }

    /// Django-shape `filter()` — parses a `"field"` or
    /// `"field__lookup"` key and dispatches to the matching `Op`.
    /// Issue #71.
    ///
    /// | Suffix | SQL | Value treatment |
    /// |---|---|---|
    /// | (none) / `__exact` | `<col> = ?` | as-is |
    /// | `__iexact` | `<col> ILIKE ?` | as-is (no wildcards) |
    /// | `__contains` | `<col> LIKE ?` | wrap `%value%` |
    /// | `__icontains` | `<col> ILIKE ?` | wrap `%value%` |
    /// | `__startswith` | `<col> LIKE ?` | wrap `value%` |
    /// | `__istartswith` | `<col> ILIKE ?` | wrap `value%` |
    /// | `__endswith` | `<col> LIKE ?` | wrap `%value` |
    /// | `__iendswith` | `<col> ILIKE ?` | wrap `%value` |
    /// | `__like` / `__ilike` / `__not_like` / `__not_ilike` | `<col> [NOT] (I)LIKE ?` | value bound verbatim — caller controls `%` / `_` placement. Eloquent `whereLike` / `whereNotLike` parity. |
    /// | `__gt` / `__gte` / `__lt` / `__lte` | direct map | as-is |
    /// | `__ne` | `<col> <> ?` | as-is |
    /// | `__in` | `<col> IN (...)` | value must be `SqlValue::List` |
    /// | `__not_in` | `<col> NOT IN (...)` | value must be `SqlValue::List`. Eloquent `whereNotIn` parity. |
    /// | `__isnull` | `<col> IS NULL` / `IS NOT NULL` | value must be `bool` |
    /// | `__between` / `__range` | `<col> BETWEEN ? AND ?` | value must be 2-element `SqlValue::List` |
    /// | `__not_between` / `__not_range` | `<col> NOT BETWEEN ? AND ?` | value must be 2-element `SqlValue::List`. Eloquent `whereNotBetween` parity. |
    /// | `__year` / `__month` / `__day` / `__hour` / `__minute` / `__second` / `__quarter` / `__week` / `__week_day` | `EXTRACT(<part> FROM <col>) = ?` | scalar value matches the date part (issue #829). Composes with trailing `__gte` / `__lt` / etc. `__quarter` is cross-dialect (SQLite synthesizes it from the month, #1037); `__week` numbering differs per backend. |
    /// | `__date` | `DATE(<col>) = ?` | value is a `chrono::NaiveDate`; strips the time component before comparison. |
    ///
    /// ```ignore
    /// Post::objects()
    ///     .filter("title__icontains", "rust")
    ///     .filter("views__gt", 100_i64)
    ///     .filter("author_id__in", rustango::core::SqlValue::List(vec![
    ///         1_i64.into(), 2_i64.into(), 3_i64.into(),
    ///     ]))
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// Unknown suffixes surface as [`QueryError::UnknownLookup`] at
    /// `compile()`. Value-shape mismatches (e.g. `__in` with a
    /// non-list value) surface as
    /// [`QueryError::InvalidLookupValue`].
    ///
    /// **Chained lookups** (`author__name__icontains`) are out of
    /// scope — that's join-traversal territory (`select_related` /
    /// `prefetch_related`). v1 supports single-table fields only.
    #[must_use]
    pub fn filter(mut self, key: &str, value: impl Into<SqlValue>) -> Self {
        self.pending.push(parse_to_pending(key, value.into()));
        self
    }

    /// Django `.exclude(key, value)` — the negation of [`Self::filter`].
    /// Each `exclude` call wraps **its own** predicate in `NOT (…)`;
    /// chained excludes AND together (identical to Django for
    /// single-kwarg excludes). The full `__lookup` suffix grammar works
    /// through `exclude` exactly as through `filter` (shared parser).
    ///
    /// ```ignore
    /// // Django: Post.objects.exclude(status="draft")
    /// Post::objects().exclude("status", "draft").fetch(&pool).await?;
    /// // with a lookup suffix:
    /// Post::objects().exclude("views__lt", 100_i64)
    /// ```
    ///
    /// **NULL semantics:** `NOT (col = v)` excludes rows where `col IS
    /// NULL` (SQL three-valued logic) — matching Django's emission for
    /// the same shape. For Django's multi-kwarg `exclude(a=1, b=2)` (one
    /// `NOT` over the whole group), compose `where_raw(!Q(...))`.
    #[must_use]
    pub fn exclude(mut self, key: &str, value: impl Into<SqlValue>) -> Self {
        self.pending
            .push(PendingFilter::Negated(Box::new(parse_to_pending(
                key,
                value.into(),
            ))));
        self
    }

    /// Stash a builder-time error to surface at `compile()` time.
    /// Used by [`Self::filter`] when the lookup-suffix parser fails.
    fn with_pending_error(mut self, e: QueryError) -> Self {
        // Add a `PendingFilter::Error` so `compile()`'s
        // `resolve_pending` walk surfaces it. Mirrors the deferred-
        // error pattern in `AggregateBuilder` for `HavingOpNotSupported`.
        self.pending.push(PendingFilter::Error(e));
        self
    }

    /// Sugar for `filter_op(field, Op::Eq, value)`. Kept for
    /// backward compatibility with the per-op shorthand surface.
    #[must_use]
    pub fn eq(self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.filter_op(field, Op::Eq, value)
    }

    /// Eloquent `Builder::whereKey($pk)` — filter on the model's
    /// primary-key column without spelling its name. Reads the PK
    /// field from `T::SCHEMA.primary_key()`.
    ///
    /// ```ignore
    /// // Eloquent: Post::query()->whereKey(42)->first();
    /// // rustango:
    /// let post = Post::objects()
    ///     .where_key(42_i64)
    ///     .first_pool(&pool).await?;
    /// ```
    ///
    /// Models without `#[rustango(primary_key)]` surface as
    /// [`QueryError::UnknownField`] (field `"<pk>"`) at compile time.
    #[must_use]
    pub fn where_key(self, pk: impl Into<SqlValue>) -> Self {
        match T::SCHEMA.primary_key() {
            Some(f) => self.filter_op(f.name, Op::Eq, pk),
            None => self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "<pk>".to_string(),
            }),
        }
    }

    /// Eloquent `Builder::whereKeyNot($pk)` — opposite of
    /// [`Self::where_key`]; matches every row whose PK ≠ `pk`.
    ///
    /// ```ignore
    /// // Eloquent: Post::query()->whereKeyNot(1)->get();
    /// let others = Post::objects()
    ///     .where_key_not(1_i64)
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// Errors as [`Self::where_key`] when the model carries no PK.
    #[must_use]
    pub fn where_key_not(self, pk: impl Into<SqlValue>) -> Self {
        match T::SCHEMA.primary_key() {
            Some(f) => self.filter_op(f.name, Op::Ne, pk),
            None => self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: "<pk>".to_string(),
            }),
        }
    }

    /// Eloquent `Builder::whereExists($closure)` — AND-join an
    /// `EXISTS (subquery)` predicate onto this queryset's WHERE.
    ///
    /// `subquery` is an already-compiled [`SelectQuery`] — build it
    /// with the inner model's `objects()` chain and call `.compile()`
    /// to propagate any column-typo errors at the inner queryset.
    /// Use [`crate::core::subquery::outer_ref`] to correlate against
    /// columns of the outer model.
    ///
    /// ```ignore
    /// use rustango::core::Column as _;
    /// use rustango::core::subquery::outer_ref;
    ///
    /// // Authors who have at least one book.
    /// let with_books = Book::objects()
    ///     .where_(Book::author_id.eq_expr(outer_ref("id")))
    ///     .compile()?;
    /// let authors = Author::objects()
    ///     .where_exists(with_books)
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// Sugar over `self.where_raw(subquery::exists(subquery))`.
    #[must_use]
    pub fn where_exists(self, subquery: SelectQuery) -> Self {
        self.where_raw(crate::core::subquery::exists(subquery))
    }

    /// Eloquent `Builder::whereNotExists($closure)` — AND-join a
    /// `NOT EXISTS (subquery)` predicate. Inverse of
    /// [`Self::where_exists`]; canonical "find rows in A with no
    /// related row in B" shape.
    ///
    /// ```ignore
    /// let no_books = Book::objects()
    ///     .where_(Book::author_id.eq_expr(outer_ref("id")))
    ///     .compile()?;
    /// let empty = Author::objects()
    ///     .where_not_exists(no_books)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_not_exists(self, subquery: SelectQuery) -> Self {
        self.where_raw(crate::core::subquery::not_exists(subquery))
    }

    /// Eloquent `Builder::whereIn($column, $closure)` with a
    /// subquery rather than an inline value list — emits
    /// `<column> IN (subquery)`.
    ///
    /// `subquery` is an already-compiled [`SelectQuery`] that should
    /// project a single column whose values are matched against
    /// `column` on the outer model. Sugar over
    /// `self.where_raw(subquery::in_subquery(col, subquery))`.
    ///
    /// ```ignore
    /// let public_cat_ids = Category::objects()
    ///     .filter("is_public", true)
    ///     .values_list_flat("id")
    ///     .compile()?;
    /// let visible = Post::objects()
    ///     .where_in_subquery("category_id", public_cat_ids)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_in_subquery(self, column: &'static str, subquery: SelectQuery) -> Self {
        self.where_raw(crate::core::subquery::in_subquery(column, subquery))
    }

    /// Eloquent `Builder::whereNotIn($column, $closure)` — inverse of
    /// [`Self::where_in_subquery`]. Emits `<column> NOT IN (subquery)`.
    #[must_use]
    pub fn where_not_in_subquery(self, column: &'static str, subquery: SelectQuery) -> Self {
        self.where_raw(crate::core::subquery::not_in_subquery(column, subquery))
    }

    /// Filter to rows that have at least one related row via the
    /// relation named `name` — Eloquent `has($rel)` / Django
    /// `filter(<rel>__isnull=False)`. Issue #830.
    ///
    /// `name` is resolved across all three relation kinds (in this
    /// order), so the same call works regardless of how the relation is
    /// declared on `T`:
    /// - **reverse-FK** — `#[rustango(reverse_has(name, child,
    ///   child_fk_column))]` → `EXISTS (SELECT 1 FROM <child> WHERE
    ///   <child_fk> = <outer>.<pk>)`.
    /// - **many-to-many** — `#[rustango(m2m(name, to, through, src,
    ///   dst))]` → `EXISTS (SELECT 1 FROM <through> WHERE <src> =
    ///   <outer>.<pk>)` over the junction table.
    /// - **generic-FK** — `#[rustango(generic_has(name, child,
    ///   ct_column, pk_column))]` → the same `EXISTS` over the
    ///   polymorphic child, AND-ed with a content-type discriminator so
    ///   only children pointing at *this* model match.
    ///
    /// All three lower identically across PG / MySQL / SQLite (the
    /// writer's scope stack threads the correlation). Unknown relation
    /// names surface as [`QueryError::UnknownField`] at `compile()` time.
    ///
    /// ```ignore
    /// // Authors with at least one book (reverse-FK relation "books").
    /// let with_books = Author::objects()
    ///     .where_has("books")
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_has(self, name: &str) -> Self {
        match Self::resolve_rel_exists(name, /*negated=*/ false) {
            Some(expr) => self.where_raw(expr),
            None => self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: name.to_string(),
            }),
        }
    }

    /// This model's primary-key column name (defaults to `"id"`). Used
    /// to correlate M2M relation subqueries back to the parent row.
    fn self_pk_column() -> &'static str {
        T::SCHEMA.primary_key().map_or("id", |f| f.column)
    }

    /// Resolve a relation `name` — reverse-FK, many-to-many, or
    /// generic-FK — into a correlated `[NOT ]EXISTS` predicate (issue
    /// #830). Returns `None` when the name matches no declared relation,
    /// so callers can surface a [`QueryError::UnknownField`]. Resolution
    /// order is reverse-FK → M2M → GFK; relation names are expected to
    /// be unique across the three within a model.
    fn resolve_rel_exists(name: &str, negated: bool) -> Option<WhereExpr> {
        use crate::core::subquery;
        if let Some(rel) = T::reverse_relations().iter().find(|r| r.name == name) {
            return Some(if negated {
                subquery::reverse_has_not_exists(rel)
            } else {
                subquery::reverse_has_exists(rel)
            });
        }
        if let Some(m2m) = T::SCHEMA.m2m.iter().find(|m| m.name == name) {
            return Some(subquery::m2m_has_exists(
                m2m,
                Self::self_pk_column(),
                negated,
            ));
        }
        if let Some(g) = T::generic_reverse_relations()
            .iter()
            .find(|g| g.name == name)
        {
            return Some(subquery::generic_has_exists(g, T::SCHEMA.table, negated));
        }
        None
    }

    /// Resolve a relation `name` into a correlated scalar `COUNT`
    /// [`Expr`] (the left-hand side of a `has($rel, op, n)` comparison),
    /// across reverse-FK / M2M / GFK. `None` if unknown. Issue #830.
    fn resolve_rel_count(name: &str) -> Option<Expr> {
        use crate::core::{subquery, RelAggKind};
        if let Some(rel) = T::reverse_relations().iter().find(|r| r.name == name) {
            return Some(subquery::reverse_has_count(rel));
        }
        if let Some(m2m) = T::SCHEMA.m2m.iter().find(|m| m.name == name) {
            return Some(subquery::m2m_has_aggregate(
                m2m,
                Self::self_pk_column(),
                RelAggKind::Count,
                None,
            ));
        }
        if let Some(g) = T::generic_reverse_relations()
            .iter()
            .find(|g| g.name == name)
        {
            return Some(subquery::generic_has_aggregate(
                g,
                T::SCHEMA.table,
                RelAggKind::Count,
                None,
            ));
        }
        None
    }

    /// Opposite of [`Self::where_has`] — filter to rows that have
    /// **no** related row via the named reverse-FK relation. Emits
    /// `NOT EXISTS (subquery)`. Same relation-resolution semantics +
    /// error shape.
    ///
    /// ```ignore
    /// // Authors with zero books.
    /// let empty = Author::objects()
    ///     .where_doesnt_have("books")
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_doesnt_have(self, name: &str) -> Self {
        match Self::resolve_rel_exists(name, /*negated=*/ true) {
            Some(expr) => self.where_raw(expr),
            None => self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: name.to_string(),
            }),
        }
    }

    /// Filter to rows that have at least one related row via the
    /// named reverse-FK relation **AND** the related row matches the
    /// caller-supplied inner predicate. Eloquent
    /// `Builder::whereHas($rel, fn ($q) => …)` parity — issue #830
    /// slice 2.
    ///
    /// `inner` is a fully-compiled `SelectQuery` on the child model
    /// (typically built via `Child::objects().filter(...).compile()?`).
    /// The method extracts its `WHERE` clause, AND-joins the
    /// correlated `<child_fk_column> = <outer>.<self_pk_column>`
    /// predicate, and wraps the result in `EXISTS (…)`.
    ///
    /// Schema validation: `inner.model` must point at the same
    /// `ModelSchema` as the relation's `child_schema`. Mismatches
    /// surface as [`QueryError::UnknownField`] at compile time.
    ///
    /// ```ignore
    /// // Eloquent: Author::whereHas('books', fn ($q) => $q->where('published', true))->get();
    /// let inner = Book::objects().filter("published", true).compile()?;
    /// let authors = Author::objects()
    ///     .where_has_filter("books", inner)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_has_filter(self, name: &str, inner: SelectQuery) -> Self {
        self.where_has_filter_impl(name, inner, /*negated=*/ false)
    }

    /// `whereDoesntHave($rel, fn ($q) => …)` counterpart of
    /// [`Self::where_has_filter`] — filter to rows that have **no**
    /// related row matching the inner predicate. Emits
    /// `NOT EXISTS (…)` over the same correlated inner SELECT.
    #[must_use]
    pub fn where_doesnt_have_filter(self, name: &str, inner: SelectQuery) -> Self {
        self.where_has_filter_impl(name, inner, /*negated=*/ true)
    }

    /// Internal worker shared by [`Self::where_has_filter`] and
    /// [`Self::where_doesnt_have_filter`]. `negated = true` wraps the
    /// merged inner SELECT in `NOT EXISTS` instead of `EXISTS`.
    fn where_has_filter_impl(self, name: &str, mut inner: SelectQuery, negated: bool) -> Self {
        let rel = match T::reverse_relations().iter().find(|r| r.name == name) {
            Some(r) => r,
            None => {
                return self.with_pending_error(QueryError::UnknownField {
                    model: T::SCHEMA.name,
                    field: name.to_string(),
                });
            }
        };
        // The user might pass an inner queryset built on the wrong
        // child model — guard against silently producing wrong SQL.
        // `Model::SCHEMA` is a `const &'static ModelSchema` (not a
        // `static`), so each use-site can get its own promoted
        // address — pointer equality is unreliable. Compare by
        // model name (unique within the crate per `#[derive(Model)]`).
        if inner.model.name != rel.child_schema.name {
            return self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: format!(
                    "where_has_filter({name}): inner queryset model mismatch — \
                     expected `{}`, got `{}`",
                    rel.child_schema.name, inner.model.name
                ),
            });
        }
        let correlated = WhereExpr::ExprCompare {
            lhs: crate::core::Expr::Column(rel.child_fk_column),
            op: Op::Eq,
            rhs: crate::core::Expr::OuterRef(rel.self_pk_column),
        };
        // AND the correlation predicate with whatever the user
        // already put on the inner SelectQuery. Flatten when the
        // user's predicate is itself an `And` to keep the tree
        // shallow.
        inner.where_clause =
            match std::mem::replace(&mut inner.where_clause, WhereExpr::And(Vec::new())) {
                WhereExpr::And(mut v) => {
                    v.push(correlated);
                    WhereExpr::And(v)
                }
                other => WhereExpr::And(vec![other, correlated]),
            };
        let wrapped = if negated {
            WhereExpr::NotExists(Box::new(inner))
        } else {
            WhereExpr::Exists(Box::new(inner))
        };
        self.where_raw(wrapped)
    }

    /// Filter to rows whose **count** of related rows (via the
    /// reverse-FK relation named `name`) satisfies `<count> <op> n` —
    /// Eloquent `has($rel, $op, $n)` / Django
    /// `.annotate(c=Count(<rel>)).filter(c__<op>=n)`. Issue #830
    /// slice 3.
    ///
    /// Emits a correlated scalar-aggregate subquery in the WHERE
    /// clause: `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
    /// <outer>.<self_pk>) <op> n`. The subquery's `OuterRef` is rewritten
    /// to the parent's table qualifier by the writer's scope stack, so
    /// the SQL is portable across PG / MySQL / SQLite.
    ///
    /// `op` is one of the binary comparison operators
    /// ([`Op::Eq`] / [`Op::Ne`] / [`Op::Lt`] / [`Op::Lte`] /
    /// [`Op::Gt`] / [`Op::Gte`]); other operators emit a dialect error
    /// at compile time. For the common "has at least one" / "has none"
    /// cases prefer [`Self::where_has`] / [`Self::where_doesnt_have`],
    /// which emit a cheaper `EXISTS` / `NOT EXISTS`.
    ///
    /// Unknown relation names surface as [`QueryError::UnknownField`]
    /// at compile time, identically to [`Self::where_has`].
    ///
    /// ```ignore
    /// // Eloquent: Author::has('books', '>', 3)->get();
    /// let prolific = Author::objects()
    ///     .where_has_count("books", Op::Gt, 3)
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_has_count(self, name: &str, op: Op, n: i64) -> Self {
        match Self::resolve_rel_count(name) {
            Some(lhs) => self.where_raw(WhereExpr::ExprCompare {
                lhs,
                op,
                rhs: Expr::Literal(SqlValue::I64(n)),
            }),
            None => self.with_pending_error(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: name.to_string(),
            }),
        }
    }

    /// Eager-aggregate the **count** of related rows via the reverse-FK
    /// relation named `name`, projected as a `<name>_count` column —
    /// Eloquent `withCount('comments')` / Django
    /// `.annotate(comments_count=Count('comments'))`. Issue #830 slice 4.
    ///
    /// Promotes the queryset to an [`AggregateBuilder`] whose result rows
    /// are `Vec<HashMap<String, SqlValue>>` (same untyped shape as every
    /// other [`QuerySet::annotate`] path — a typed `T` has no field to
    /// land the derived column in). Each row carries every scalar column
    /// of `T` plus the `<name>_count` key.
    ///
    /// The count comes from a **correlated subquery**
    /// (`(SELECT COUNT(*) FROM <child> WHERE <child_fk> = <outer>.<pk>)`),
    /// not a `JOIN`, so it never double-counts and emits no N+1 — the same
    /// portable lowering [`Self::where_has_count`] uses across PG / MySQL /
    /// SQLite.
    ///
    /// Unknown relation names surface as [`QueryError::UnknownField`] at
    /// `compile()` time.
    ///
    /// ```ignore
    /// // Eloquent: Author::withCount('books')->get();
    /// let rows = Author::objects()
    ///     .annotate_count("books")
    ///     .fetch(&pool).await?;
    /// // rows[0]["books_count"] => SqlValue::I64(n)
    /// ```
    #[must_use]
    pub fn annotate_count(self, name: &str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, None, AggregateExpr::Count(None), "count")
    }

    /// Eager-aggregate `SUM(<column>)` over related rows via the
    /// reverse-FK relation `name`, projected as `<name>_sum_<column>` —
    /// Eloquent `withSum('orders', 'total')`. `column` is a column on the
    /// **child** model (validated against the child schema at
    /// `compile()` time). See [`Self::annotate_count`] for the return
    /// shape and correlated-subquery semantics.
    #[must_use]
    pub fn annotate_sum(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Sum(column), "sum")
    }

    /// Eager-aggregate `AVG(<column>)` over related rows — Eloquent
    /// `withAvg('orders', 'total')`, projected as `<name>_avg_<column>`.
    /// See [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_avg(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Avg(column), "avg")
    }

    /// Eager-aggregate `MAX(<column>)` over related rows — Eloquent
    /// `withMax('orders', 'total')`, projected as `<name>_max_<column>`.
    /// See [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_max(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Max(column), "max")
    }

    /// Eager-aggregate `MIN(<column>)` over related rows — Eloquent
    /// `withMin('orders', 'total')`, projected as `<name>_min_<column>`.
    /// See [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_min(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Min(column), "min")
    }

    /// Eager-annotate whether **any** related row exists via the
    /// reverse-FK relation `name`, projected as a `<name>_exists` column
    /// — Eloquent `withExists('comments')` / Django
    /// `.annotate(has_comments=Exists(...))`. Issue #830 slice 5.
    ///
    /// Emits `CASE WHEN EXISTS (SELECT 1 FROM <child> WHERE <child_fk> =
    /// <outer>.<pk>) THEN 1 ELSE 0 END` — cheaper than
    /// [`Self::annotate_count`] when you only need presence (`EXISTS`
    /// short-circuits on the first matching child row). The column
    /// decodes as `SqlValue::I64(1)` / `I64(0)` on all three backends
    /// (integer literals are used rather than a native boolean so the
    /// dict-row value doesn't vary by dialect). See
    /// [`Self::annotate_count`] for the return shape.
    ///
    /// Unknown relation names surface as [`QueryError::UnknownField`] at
    /// `compile()` time.
    #[must_use]
    pub fn annotate_exists(self, name: &str) -> AggregateBuilder<T> {
        let mut builder = self.aggregate();
        // Reuse the existence resolver (reverse-FK / M2M / GFK), then
        // wrap whatever `[NOT ]EXISTS` it produced in a `CASE WHEN …
        // THEN 1 ELSE 0` so it projects as an `I64(0|1)` column.
        match Self::resolve_rel_exists(name, /*negated=*/ false) {
            Some(exists) => {
                let expr = AggregateExpr::RelatedAggregate(Box::new(
                    crate::core::subquery::exists_as_int(exists),
                ));
                builder
                    .aggregates
                    .push((std::borrow::Cow::Owned(format!("{name}_exists")), expr));
            }
            None => {
                builder
                    .deferred_error
                    .get_or_insert(QueryError::UnknownField {
                        model: T::SCHEMA.name,
                        field: name.to_owned(),
                    });
            }
        }
        builder
    }

    /// Shared worker for the `annotate_{count,sum,avg,max,min}` family
    /// (issue #830). Resolves the reverse relation, auto-names the
    /// projected column (`<name>_count` or `<name>_<suffix>_<column>`),
    /// wraps the correlated subquery in an
    /// [`AggregateExpr::RelatedAggregate`], and stages it on a fresh
    /// [`AggregateBuilder`].
    ///
    /// Errors are deferred onto the builder so they surface from
    /// `compile()` (mirroring the queryset-level deferred-error path):
    /// an unknown relation name → [`QueryError::UnknownField`] on `T`; a
    /// `column` that isn't a field of the child model → the same error
    /// keyed on the child schema.
    fn annotate_relation_aggregate(
        self,
        name: &str,
        column: Option<&'static str>,
        agg: AggregateExpr,
        suffix: &str,
    ) -> AggregateBuilder<T> {
        use crate::core::{subquery, RelAggKind};
        let mut builder = self.aggregate();
        // `<name>_count` (no column) or `<name>_<suffix>_<column>`.
        let alias = || match column {
            Some(col) => std::borrow::Cow::Owned(format!("{name}_{suffix}_{col}")),
            None => std::borrow::Cow::Owned(format!("{name}_{suffix}")),
        };
        // Map the reverse-FK `AggregateExpr` onto the raw-table
        // `RelAggKind` used by the M2M / GFK builders.
        let rel_kind = |a: &AggregateExpr| match a {
            AggregateExpr::Sum(_) => RelAggKind::Sum,
            AggregateExpr::Avg(_) => RelAggKind::Avg,
            AggregateExpr::Max(_) => RelAggKind::Max,
            AggregateExpr::Min(_) => RelAggKind::Min,
            _ => RelAggKind::Count,
        };

        // Reverse-FK relation — aggregate over the child table directly.
        if let Some(rel) = T::reverse_relations().iter().find(|r| r.name == name) {
            if let Some(col) = column {
                if rel.child_schema.field_by_column(col).is_none() {
                    builder
                        .deferred_error
                        .get_or_insert(QueryError::UnknownField {
                            model: rel.child_schema.name,
                            field: col.to_owned(),
                        });
                    return builder;
                }
            }
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::reverse_has_aggregate(
                rel, agg,
            )));
            builder.aggregates.push((alias(), expr));
            return builder;
        }

        // Many-to-many — `Count` over the junction; `Sum`/`Avg`/`Max`/
        // `Min` over a target column reached through the junction. The
        // target table has no `ModelSchema`, so `column` can't be
        // validated here (a bad column surfaces at the database).
        if let Some(m2m) = T::SCHEMA.m2m.iter().find(|m| m.name == name) {
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::m2m_has_aggregate(
                m2m,
                Self::self_pk_column(),
                rel_kind(&agg),
                column,
            )));
            builder.aggregates.push((alias(), expr));
            return builder;
        }

        // Generic-FK — aggregate over the polymorphic child table,
        // discriminated by content type.
        if let Some(g) = T::generic_reverse_relations()
            .iter()
            .find(|g| g.name == name)
        {
            if let Some(col) = column {
                if g.child_schema.field_by_column(col).is_none() {
                    builder
                        .deferred_error
                        .get_or_insert(QueryError::UnknownField {
                            model: g.child_schema.name,
                            field: col.to_owned(),
                        });
                    return builder;
                }
            }
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::generic_has_aggregate(
                g,
                T::SCHEMA.table,
                rel_kind(&agg),
                column,
            )));
            builder.aggregates.push((alias(), expr));
            return builder;
        }

        builder
            .deferred_error
            .get_or_insert(QueryError::UnknownField {
                model: T::SCHEMA.name,
                field: name.to_owned(),
            });
        builder
    }

    /// Eloquent `Builder::whereColumn($col1, $col2)` — emits
    /// `<col1> = <col2>`, comparing two columns instead of column
    /// vs literal. Equality is the overwhelming majority of uses;
    /// for any other comparison, see [`Self::where_column_op`].
    ///
    /// Both column names are validated by the schema at
    /// `compile()`-time via the existing
    /// [`WhereExpr::ExprCompare`](crate::core::WhereExpr::ExprCompare)
    /// path.
    ///
    /// ```ignore
    /// // Eloquent: User::whereColumn('first_name', 'last_name');
    /// User::objects()
    ///     .where_column("first_name", "last_name")
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_column(self, col1: &'static str, col2: &'static str) -> Self {
        self.where_column_op(col1, Op::Eq, col2)
    }

    /// Eloquent `Builder::whereColumn($col1, $op, $col2)` — column-
    /// vs-column comparison with an explicit operator. Use the
    /// shorter [`Self::where_column`] when the op is `Eq`.
    ///
    /// ```ignore
    /// use rustango::core::Op;
    /// // start_date < end_date — Eloquent: whereColumn('start_date', '<', 'end_date')
    /// Reservation::objects()
    ///     .where_column_op("start_date", Op::Lt, "end_date")
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn where_column_op(self, col1: &'static str, op: Op, col2: &'static str) -> Self {
        use crate::core::F;
        self.where_raw(WhereExpr::ExprCompare {
            lhs: F(col1).into(),
            op,
            rhs: F(col2).into(),
        })
    }

    /// Append a typed predicate or boolean expression built via the
    /// [`Column`](crate::core::Column) API. Accepts either a single
    /// [`TypedFilter`](crate::core::TypedFilter) (`User::id.gt(10)`)
    /// AND-join a raw [`WhereExpr`] into the accumulated WHERE clause.
    /// Useful when the model doesn't have typed columns derived (so
    /// [`Self::where_`] isn't available) and the caller needs to
    /// express OR / NOT / nested predicates that the string-keyed
    /// [`Self::filter`] can't reach.
    ///
    /// The expression is fully validated against `T::SCHEMA` at
    /// `compile()` time — passing an unknown column or a wrong-typed
    /// value returns [`QueryError::UnknownField`] /
    /// [`QueryError::TypeMismatch`] just like the typed paths.
    #[must_use]
    pub fn where_raw(mut self, expr: WhereExpr) -> Self {
        self.pending.push(PendingFilter::Expr(expr));
        self
    }

    /// Eloquent `Builder::when($condition, $callback)` — apply
    /// `f` to `self` only when `condition` is `true`, otherwise
    /// return `self` unchanged. Sugars the otherwise-noisy
    /// "conditionally compose a filter" pattern:
    ///
    /// ```ignore
    /// let qs = Post::objects()
    ///     .when(only_active, |qs| qs.filter("active", true))
    ///     .when(category_id.is_some(), |qs| {
    ///         qs.filter("category_id", category_id.unwrap())
    ///     });
    /// ```
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// let mut qs = Post::objects();
    /// if only_active { qs = qs.filter("active", true); }
    /// if let Some(id) = category_id { qs = qs.filter("category_id", id); }
    /// ```
    ///
    /// The closure form preserves builder-style chaining when the
    /// condition isn't a simple `bool` but the result of an
    /// in-line predicate. Issue [#830](https://github.com/ujeenet/rustango/issues/830) follow-up.
    #[must_use]
    pub fn when<F>(self, condition: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if condition {
            f(self)
        } else {
            self
        }
    }

    /// Eloquent `Builder::unless($condition, $callback)` — inverse
    /// of [`Self::when`]. Apply `f` only when `condition` is
    /// `false`. Mirrors Eloquent's twin helper so `.unless(...)`
    /// reads naturally in code that's clearer when the guard is
    /// stated negatively (e.g. `qs.unless(is_admin, |q|
    /// q.filter("public", true))`).
    #[must_use]
    pub fn unless<F>(self, condition: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if condition {
            self
        } else {
            f(self)
        }
    }

    /// Eloquent `Builder::tap($callback)` — run a side-effecting
    /// callback against the queryset and return it unchanged. Lets
    /// you slot logging / metrics / debug-print into a builder
    /// chain without breaking the fluent shape:
    ///
    /// ```ignore
    /// let rows = Post::objects()
    ///     .filter("active", true)
    ///     .tap(|qs| tracing::debug!(?qs, "about to fetch"))
    ///     .fetch(&pool).await?;
    /// ```
    #[must_use]
    pub fn tap<F>(self, f: F) -> Self
    where
        F: FnOnce(&Self),
    {
        f(&self);
        self
    }

    /// Filter to only rows that have NOT been soft-deleted.
    ///
    /// When `T` carries `#[rustango(soft_delete)]` on a nullable
    /// `DateTime<Utc>` column, AND-joins `<col> IS NULL` into the
    /// queryset's accumulated WHERE clause. When the model has no
    /// soft-delete column, the call is a no-op (matches
    /// [`crate::soft_delete::active_filter`]'s `None` shape — keeps
    /// templated code that wraps every fetch in `.active()`
    /// compiling regardless of whether a particular model is
    /// soft-delete-enabled). Issue #821.
    #[must_use]
    pub fn active(self) -> Self {
        match crate::soft_delete::active_filter(T::SCHEMA) {
            Some(expr) => self.where_raw(expr),
            None => self,
        }
    }

    /// Filter to ONLY soft-deleted rows. Mirror of [`Self::active`]
    /// — useful for "Trash" admin pages, restore flows, etc.
    /// When the model has no soft-delete column the call is a
    /// no-op. Eloquent `onlyTrashed`. Issue #821.
    #[must_use]
    pub fn only_trashed(self) -> Self {
        match crate::soft_delete::trashed_filter(T::SCHEMA) {
            Some(expr) => self.where_raw(expr),
            None => self,
        }
    }

    /// Explicit "include both active and trashed rows" no-op
    /// marker. Today every `QuerySet` defaults to including trashed
    /// rows (auto-scoping a.k.a. global-scope tracking is sibling
    /// issue [#820](https://github.com/ujeenet/rustango/issues/820));
    /// `with_trashed()` exists as forward-compat for code that wants
    /// to declare intent now and stay correct when auto-scoping
    /// lands. Eloquent `withTrashed`. Issue #821.
    #[must_use]
    pub fn with_trashed(self) -> Self {
        self
    }

    /// or a composed [`TypedExpr`] (`User::id.eq(1).or(User::id.eq(2))`).
    /// Every `.where_()` call AND-joins its argument into the
    /// queryset's accumulated WHERE clause.
    #[must_use]
    pub fn where_<E: Into<TypedExpr<T>>>(mut self, predicate: E) -> Self {
        let expr = predicate.into().into_expr();
        // Hoist a bare predicate into the legacy `Resolved` slot so
        // the resulting WhereExpr stays a flat AND-of-predicates for
        // simple chains — preserves the v0.6 `as_flat_and()` shape.
        match expr {
            WhereExpr::Predicate(filter) => {
                self.pending.push(PendingFilter::Resolved(filter));
            }
            other => {
                self.pending.push(PendingFilter::Expr(other));
            }
        }
        self
    }

    /// Validate the accumulated filters against `T::SCHEMA` and lower to
    /// the dialect-neutral `SelectQuery` IR.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] if a filter names a field not
    /// present on the model, and [`QueryError::TypeMismatch`] if the bound
    /// value's type does not match the field's declared type.
    pub fn compile(mut self) -> Result<SelectQuery, QueryError> {
        // Issue #820 — Eloquent global scopes. Fold the schema-declared
        // auto-applied filters into the pending list (respecting any
        // `without_global_scope` / `without_global_scopes` opt-outs)
        // BEFORE we hand `self.pending` to the resolver. From here on
        // the resolver doesn't know or care which filters were scopes
        // and which were user-provided.
        self.apply_global_scopes();
        let model: &'static ModelSchema = T::SCHEMA;
        // #1031 — lower relation-spanning lookups (`author__name`) into
        // aliased predicates + their FK-chain JOINs before the WHERE is
        // resolved. The JOINs merge into the select_related set below,
        // deduped by alias.
        let (pending, span_joins) = lower_relation_spans(model, self.pending)?;
        let where_clause = resolve_pending(model, pending)?;
        let mut joins = lower_select_related(model, &self.select_related)?;
        // Ad-hoc joins (issue #80) come AFTER FK-driven select_related
        // joins in the SELECT — preserves the existing column ordering
        // for legacy callers, and keeps user-driven joins next to the
        // user-driven WHERE.
        joins.extend(self.ad_hoc_joins);
        // Relation-span JOINs (#1031) — skip any alias already present
        // (a span over the same path as a `select_related` emits once).
        for j in span_joins {
            if !joins.iter().any(|e| e.alias == j.alias) {
                joins.push(j);
            }
        }
        // #1034 — split head-branch clauses from combined-result
        // clauses. With a compound, the head-branch `ORDER BY` /
        // `LIMIT` / `OFFSET` were frozen at the first set-op call
        // (`head_*`); the live slots hold whatever was chained AFTER
        // and apply to the merged result. Without a compound, the
        // live slots are the query's own and `head_*` are empty.
        let has_compound = !self.compound.is_empty();
        let (head_pending, head_limit, head_offset, comb_pending, comb_limit, comb_offset) =
            if has_compound {
                (
                    std::mem::take(&mut self.head_order_by),
                    self.head_limit,
                    self.head_offset,
                    std::mem::take(&mut self.order_by),
                    self.limit,
                    self.offset,
                )
            } else {
                (
                    std::mem::take(&mut self.order_by),
                    self.limit,
                    self.offset,
                    Vec::new(),
                    None,
                    None,
                )
            };
        // Lower the unified pending list to OrderItems. Insertion
        // order is preserved across legacy `.order_by(...)` and
        // issue-#76 `.order_by_with_nulls(...)` / `.order_by_expr(...)`
        // calls so a mixed chain emits in the order it was written.
        // `order_by` is the head-branch (or sole) ordering; the
        // combined-result ordering lowers separately below.
        let order_by = lower_order_items(model, head_pending)?;
        let compound_order_by = lower_order_items(model, comb_pending)?;
        // Issue #264 / T1.2 — `.distinct_on(&[...])` requires the
        // listed columns to head the ORDER BY (Django enforces this
        // at runtime; we catch it at builder time). Empty list is
        // rejected because it would degenerate to `.distinct()`.
        if let Some(crate::core::DistinctMode::On(cols)) = &self.distinct {
            if cols.is_empty() {
                return Err(QueryError::DistinctOnEmpty);
            }
            for col in cols {
                if model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            // First `cols.len()` order_by items must be the distinct-on
            // columns in the same order. Bare `OrderItem::Column` only;
            // expressions / random can't be DISTINCT-ON keys.
            if order_by.len() < cols.len() {
                return Err(QueryError::DistinctOnOrderByMismatch {
                    distinct_on: cols.iter().map(|s| (*s).to_owned()).collect(),
                    order_by: order_by_column_names(&order_by),
                });
            }
            for (i, col) in cols.iter().enumerate() {
                let head = match &order_by[i] {
                    crate::core::OrderItem::Column { column, .. } => *column,
                    _ => {
                        return Err(QueryError::DistinctOnOrderByMismatch {
                            distinct_on: cols.iter().map(|s| (*s).to_owned()).collect(),
                            order_by: order_by_column_names(&order_by),
                        });
                    }
                };
                if head != *col {
                    return Err(QueryError::DistinctOnOrderByMismatch {
                        distinct_on: cols.iter().map(|s| (*s).to_owned()).collect(),
                        order_by: order_by_column_names(&order_by),
                    });
                }
            }
        }
        // #331 — `.none()` forces an always-empty result via `LIMIT 0`.
        // Cheap on every dialect (PG/MySQL/SQLite skip plan
        // materialization), and the other terminal ops (count, exists,
        // first) read off the same SelectQuery so they see the same
        // empty result without further special-casing. The `LIMIT 0`
        // pins to the OUTERMOST slot — the combined result when a
        // compound is present, else the head/sole query.
        let (limit, compound_limit) = if self.is_none {
            if has_compound {
                (head_limit, Some(0))
            } else {
                (Some(0), comb_limit)
            }
        } else {
            (head_limit, comb_limit)
        };
        Ok(SelectQuery {
            model,
            where_clause,
            search: None,
            joins,
            subquery_joins: self.subquery_joins,
            order_by,
            limit,
            offset: head_offset,
            lock_mode: self.lock_mode,
            compound: self.compound,
            projection: None,
            distinct: self.distinct,
            compound_order_by,
            compound_limit,
            compound_offset: comb_offset,
        })
    }

    /// Project to `Vec<HashMap<String, SqlValue>>` — Django's
    /// `.values('id', 'name')`. Issue #22. Returns a
    /// [`ValuesQuerySet`] whose terminal `.fetch(&pool)` decodes
    /// rows into a `HashMap` keyed by column name.
    ///
    /// Skips the typed `Model` decode — useful when you only need a
    /// few fields off a wide table, or when the result feeds
    /// downstream code that wants dynamic column access (templates,
    /// JSON serialization, CSV export). The existing
    /// [`QuerySet::values`] method (which promotes to
    /// [`AggregateBuilder`] for GROUP BY) is unchanged; this is a
    /// separate, pure-projection entry point.
    ///
    /// Columns are validated against the model schema at `.compile()`
    /// time — typo'd column names surface
    /// [`QueryError::UnknownField`]. Empty `cols` surfaces
    /// [`QueryError::EmptyValuesProjection`].
    #[must_use]
    pub fn values_dict(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        ValuesQuerySet {
            qs: self,
            cols: cols.to_vec(),
        }
    }

    /// Project to `Vec<Vec<SqlValue>>` — Django's
    /// `.values_list('id', 'name')`. Issue #22. Same projection as
    /// [`Self::values_dict`] but each row comes back as an
    /// ordered `Vec<SqlValue>` (cell ordering matches the `cols`
    /// argument), not a `HashMap`.
    #[must_use]
    pub fn values_list(self, cols: &[&'static str]) -> ValuesListQuerySet<T> {
        ValuesListQuerySet {
            qs: self,
            cols: cols.to_vec(),
        }
    }

    /// Single-column flat projection — Django's
    /// `.values_list('id', flat=True)`. Issue #22. Returns
    /// [`ValuesFlatQuerySet`] whose terminal `.fetch::<U>(&pool)`
    /// decodes the single column into `Vec<U>` directly via sqlx's
    /// typed `query_scalar` path.
    ///
    /// `U` must be decodable from the column's SQL type on every
    /// dialect the binary targets. Common picks: `i64` / `i32` /
    /// `String` / `bool` / `f64`.
    #[must_use]
    pub fn values_list_flat(self, col: &'static str) -> ValuesFlatQuerySet<T> {
        ValuesFlatQuerySet { qs: self, col }
    }

    /// Project only the listed columns — Django's `.only('id', 'name')`.
    /// Issue #20. Equivalent to [`Self::values_dict`] semantically — the
    /// SQL is `SELECT cols FROM …` and the result is
    /// `Vec<HashMap<String, SqlValue>>` keyed by column name. The
    /// distinct entry point preserves Django muscle-memory at the
    /// chain site.
    ///
    /// Columns are validated at `.compile()` time — typos surface as
    /// [`QueryError::UnknownField`]; an empty list surfaces
    /// [`QueryError::EmptyValuesProjection`].
    ///
    /// Note: unlike Django's `.only()`, the return shape is a
    /// `HashMap`, not a partially-hydrated `Model` instance — rustango
    /// has no equivalent of Django's lazy-attribute descriptor magic.
    /// Typed partial-row decode is queued for a future slice.
    #[must_use]
    pub fn only(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        self.values_dict(cols)
    }

    /// Project every scalar column EXCEPT the listed ones — Django's
    /// `.defer('big_field', 'huge_blob')`. Issue #20. Compute the
    /// complement against the model schema; the resulting SELECT
    /// omits the named columns, saving IO on wide tables.
    ///
    /// Same return shape as [`Self::only`] (`Vec<HashMap<String, SqlValue>>`)
    /// with the same Django parity caveat.
    ///
    /// Typo'd defer columns surface as [`QueryError::UnknownField`]
    /// at `.compile()` time (any column the caller named that doesn't
    /// exist on the model is rejected just like an `.only(...)` typo).
    /// Empty defer list returns every scalar column on the model —
    /// semantically a no-op vs. `.values_dict(all_cols)`.
    #[must_use]
    pub fn defer(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        let model = T::SCHEMA;
        let exclude: std::collections::HashSet<&'static str> = cols.iter().copied().collect();
        let mut projection: Vec<&'static str> = model
            .scalar_fields()
            .filter(|f| !exclude.contains(f.column))
            .map(|f| f.column)
            .collect();
        // Typo'd defer cols get forwarded into the projection so
        // `values_dict`'s `UnknownField` check rejects them at compile.
        for &col in cols {
            if model.field_by_column(col).is_none() {
                projection.push(col);
            }
        }
        self.values_dict(&projection)
    }

    /// Lower this queryset to a `DeleteQuery` — same WHERE clause, no projection.
    ///
    /// # Errors
    /// As [`QuerySet::compile`].
    pub fn compile_delete(mut self) -> Result<DeleteQuery, QueryError> {
        // Issue #820 — fold global scopes into the WHERE so a
        // `Post.objects().delete_pool(&pool)` honors the same
        // auto-applied filter that `Post.objects().fetch(&pool)`
        // does (e.g. a `published_only` scope means a wholesale delete
        // still won't reach unpublished rows).
        self.apply_global_scopes();
        let model: &'static ModelSchema = T::SCHEMA;
        let where_clause = resolve_pending(model, self.pending)?;
        // #331 — `.none().delete()` is a guaranteed no-op. Append an
        // `IS NULL` predicate against the (NOT NULL) primary key so
        // the DB refuses to match any row.
        let where_clause = if self.is_none {
            never_match_clause(model, where_clause)?
        } else {
            where_clause
        };
        Ok(DeleteQuery {
            model,
            where_clause,
        })
    }

    /// Start an `UpdateBuilder` carrying this queryset's filters as the WHERE clause.
    #[must_use]
    pub fn update(self) -> UpdateBuilder<T> {
        UpdateBuilder {
            qs: self,
            set: Vec::new(),
        }
    }

    /// Start an [`AggregateBuilder`] carrying this queryset's filters as the
    /// WHERE clause. Chain `.group_by`, `.annotate`, `.having`, `.order_by`,
    /// `.limit`, `.offset` then call `.compile()` to get an [`AggregateQuery`].
    #[must_use]
    pub fn aggregate(self) -> AggregateBuilder<T> {
        AggregateBuilder {
            qs: self,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            aliases: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            deferred_error: None,
            values: None,
        }
    }

    /// Django-shape `.values(&[...]).annotate(...)` projection — issue #75.
    ///
    /// Switches the queryset into aggregate mode and records the projection
    /// columns. When followed by `.annotate("alias", aggregate)`, the writer
    /// emits `SELECT cols, AGGR(...) FROM t GROUP BY cols` — GROUP BY is
    /// inferred from the values list.
    ///
    /// This is **Django Shape 2** (`.values(cols).annotate(agg)` →
    /// `GROUP BY cols`). The same IR compiles to structurally-equivalent
    /// SQL on Postgres, MySQL, and SQLite; see
    /// `tests/values_annotate_parity.rs` for the cross-dialect parity pins.
    ///
    /// ```ignore
    /// // "Posts per author" — Django's canonical example.
    /// Post::objects()
    ///     .values(&["author_id"])
    ///     .annotate("n", count_all().into())
    ///     .compile()?;
    /// // → SELECT "author_id", COUNT(*) AS "n" FROM "post" GROUP BY "author_id"
    /// ```
    ///
    /// Filtering by an `.annotate()` alias routes to `HAVING`; filtering
    /// by a model column routes to `WHERE`:
    ///
    /// ```ignore
    /// // Authors with > 5 published posts.
    /// Post::objects()
    ///     .values(&["author_id"])
    ///     .annotate("n", count_all().into())
    ///     .filter("status", Op::Eq, "published") // → WHERE
    ///     .filter("n", Op::Gt, 5_i64)            // → HAVING
    ///     .compile()?;
    /// ```
    ///
    /// Calling `.values()` without a subsequent aggregating `.annotate(...)`
    /// is **Django Shape 1** — a pure projection (no GROUP BY emitted).
    #[must_use]
    pub fn values(self, columns: &[&'static str]) -> AggregateBuilder<T> {
        AggregateBuilder {
            qs: self,
            group_by: Vec::new(),
            aggregates: Vec::new(),
            aliases: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            deferred_error: None,
            values: Some(columns.to_vec()),
        }
    }

    /// Django-shape `.annotate(...)` without explicit `.aggregate()` — issue #75.
    ///
    /// Promotes to an [`AggregateBuilder`]. If the annotation aggregates rows
    /// (`Count`, `Sum`, `Avg`, …) and `.values()` wasn't called, `compile()`
    /// auto-populates GROUP BY with every non-aggregate scalar column on the
    /// model — this is **Django Shape 3** ("each row + a derived aggregate
    /// over its children").
    ///
    /// The same IR compiles to structurally-equivalent SQL on Postgres,
    /// MySQL, and SQLite; see `tests/values_annotate_parity.rs`.
    ///
    /// ```ignore
    /// // "Each author + their post count" — every column of `Author`
    /// // ends up in the SELECT list AND in GROUP BY.
    /// Author::objects()
    ///     .annotate("post_count", count_all().into())
    ///     .compile()?;
    /// ```
    ///
    /// # Django shape cheat-sheet
    ///
    /// | Shape | Call site                                | GROUP BY        |
    /// |-------|------------------------------------------|-----------------|
    /// | 1     | `.values_dict(&[cols])`                  | (none)          |
    /// | 2     | `.values(&[cols]).annotate(alias, agg)`  | the values cols |
    /// | 3     | `.annotate(alias, agg)` (alone)          | every scalar    |
    #[must_use]
    pub fn annotate(self, alias: &'static str, expr: AggregateExpr) -> AggregateBuilder<T> {
        self.aggregate().annotate(alias, expr)
    }

    /// Project a correlated scalar subquery under `alias`, promoting to
    /// an [`AggregateBuilder`] — Django's
    /// `annotate(newest=Subquery(...))` (#1036). Sugar over
    /// [`Self::annotate`] with [`crate::core::subquery::scalar_subquery`];
    /// shape `inner` to one column × ≤1 row (see that function's caller
    /// contract). Correlate via [`crate::core::subquery::outer_ref`].
    #[must_use]
    pub fn annotate_subquery(
        self,
        alias: &'static str,
        inner: crate::core::SelectQuery,
    ) -> AggregateBuilder<T> {
        self.aggregate()
            .annotate(alias, crate::core::subquery::scalar_subquery(inner))
    }
}

/// Accumulates `SET column = value` assignments, then compiles to an `UpdateQuery`.
///
/// Constructed via [`QuerySet::update`]. The queryset's filters become the
/// WHERE clause; an empty queryset produces an unfiltered update affecting
/// every row.
pub struct UpdateBuilder<T: Model> {
    qs: QuerySet<T>,
    set: Vec<PendingAssignment>,
}

enum PendingAssignment {
    Raw(RawAssignment),
    /// `field = <Expr>` — staging for the `F()` SET path. Resolved at
    /// `compile()` time so the field name validates against the schema.
    RawExpr(RawExprAssignment),
    Resolved(Assignment),
}

impl<T: Model> UpdateBuilder<T> {
    /// Append a `SET field = value` assignment. Last write wins for repeated fields.
    #[must_use]
    pub fn set(mut self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.set.push(PendingAssignment::Raw(RawAssignment {
            field: field.into(),
            value: value.into(),
        }));
        self
    }

    /// Append a typed `SET column = value` from a [`Column`](crate::core::Column).
    #[must_use]
    pub fn set_typed(mut self, assignment: TypedAssignment<T>) -> Self {
        self.set
            .push(PendingAssignment::Resolved(assignment.into_assignment()));
        self
    }

    /// Append a `SET field = <expression>` — the [`Expr`]-shaped form
    /// that powers `F()` column references and arithmetic:
    ///
    /// ```ignore
    /// // Atomic counter increment, no read-modify-write race:
    /// Post::objects()
    ///     .where_(Post::id.eq(7))
    ///     .update()
    ///     .set_expr("views", F("views") + 1)
    ///     .execute_pool(&pool).await?;
    /// ```
    ///
    /// Accepts a bare [`F`](crate::core::F), a literal (anything that
    /// `Into<SqlValue>`), or a full arithmetic tree.
    #[must_use]
    pub fn set_expr(
        mut self,
        field: impl Into<String>,
        expr: impl Into<crate::core::Expr>,
    ) -> Self {
        self.set.push(PendingAssignment::RawExpr(RawExprAssignment {
            field: field.into(),
            value: expr.into(),
        }));
        self
    }

    /// Validate against `T::SCHEMA` and lower to an `UpdateQuery`.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] if any `set` or filter names an
    /// unknown field, and [`QueryError::TypeMismatch`] if any bound value's
    /// type doesn't match the field's declared type.
    pub fn compile(mut self) -> Result<UpdateQuery, QueryError> {
        // Issue #820 — same rationale as the SELECT / DELETE paths:
        // an `.update().set(...)` shouldn't escape the model's global
        // scopes any more than a plain `.fetch` would.
        self.qs.apply_global_scopes();
        let model: &'static ModelSchema = T::SCHEMA;

        let assignments = self
            .set
            .into_iter()
            .map(|p| match p {
                PendingAssignment::Raw(raw) => resolve_assignment(model, raw),
                PendingAssignment::RawExpr(raw) => resolve_assignment_expr(model, raw),
                PendingAssignment::Resolved(assignment) => Ok(assignment),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let where_clause = resolve_pending(model, self.qs.pending)?;
        // #331 — `.none().update().set(...)` is a no-op for the same
        // reason `.none().delete()` is: the never-match predicate
        // forces zero affected rows.
        let where_clause = if self.qs.is_none {
            never_match_clause(model, where_clause)?
        } else {
            where_clause
        };

        Ok(UpdateQuery {
            model,
            set: assignments,
            where_clause,
        })
    }
}

/// Issue #76: lower the unified pending order-by list to a
/// `Vec<OrderItem>` at `compile()` time. `Field` variants resolve
/// the field name against the model schema; `Expr` variants pass
/// through verbatim (the writer surfaces DB-side errors for typos
/// inside expressions, matching the existing `set_expr` posture).
/// Render the column-only view of an `order_by` list for error
/// reporting (`DistinctOnOrderByMismatch`). `Expr` / `Random` entries
/// render as `<expr>` / `RANDOM` so the error message still locates
/// where the mismatch begins.
fn order_by_column_names(order_by: &[crate::core::OrderItem]) -> Vec<String> {
    order_by
        .iter()
        .map(|item| match item {
            crate::core::OrderItem::Column { column, .. } => (*column).to_owned(),
            crate::core::OrderItem::Expr { .. } => "<expr>".to_owned(),
            crate::core::OrderItem::Random => "RANDOM".to_owned(),
        })
        .collect()
}

fn lower_order_items(
    model: &'static ModelSchema,
    items: Vec<PendingOrderItem>,
) -> Result<Vec<crate::core::OrderItem>, QueryError> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            PendingOrderItem::Field { name, desc, nulls } => {
                let field = model.field(&name).ok_or_else(|| QueryError::UnknownField {
                    model: model.name,
                    field: name.clone(),
                })?;
                out.push(crate::core::OrderItem::column_with_nulls(
                    field.column,
                    desc,
                    nulls,
                ));
            }
            PendingOrderItem::Expr { expr, desc, nulls } => {
                out.push(crate::core::OrderItem::expr_with_nulls(expr, desc, nulls));
            }
            PendingOrderItem::Random => {
                out.push(crate::core::OrderItem::random());
            }
        }
    }
    Ok(out)
}

/// Convert `select_related` field names into `Join`s — slice 9.0d,
/// extended for multi-hop chains in issue #297 / T2.2.
///
/// **Single hop** (e.g. `"author"`): look up the field on `model`,
/// verify it's a `Relation::Fk` / `Relation::O2O`, find the target
/// schema in inventory, build a LEFT `Join` projecting all of the
/// target's columns.
///
/// **Multi-hop** (e.g. `"author__profile__country"`): split on `__`,
/// resolve each hop against the previous hop's target schema.
/// Successive hops use the **previous join's alias** as the LHS of
/// their `ON` predicate so the FK chain stitches in SQL. The
/// emitted join alias is the full dotted path (`"author__profile"`,
/// `"author__profile__country"`) — globally unique and
/// composition-safe even when two different roots share a tail name.
///
/// Errors out on any unresolvable hop with a clear
/// `SelectRelatedInvalid` reason, including the position in the
/// chain.
///
/// **Decoder caveat (T2.2 partial-ship note):** the LoadRelated
/// dispatcher in `crate::sql::executor` currently stitches single-
/// hop FKs only. Multi-hop chains emit the JOIN + projection
/// correctly, so callers can `.filter()` / `.where_raw()` against
/// deep columns via `Expr::AliasedColumn`, but the lazy
/// `post.author.profile` field stitching across the chain is
/// follow-up work (the macro-generated `__rustango_load_related` on
/// the parent only knows about its own model's FKs; recursive
/// dispatch on the loaded child instance needs additional macro +
/// executor wiring).
fn lower_select_related(
    model: &'static ModelSchema,
    names: &[String],
) -> Result<Vec<crate::core::Join>, QueryError> {
    use crate::core::{inventory, Expr, Join, JoinKind, ModelEntry, Op, Relation, WhereExpr};
    let mut out: Vec<Join> = Vec::with_capacity(names.len());
    for name in names {
        // Walk each `__`-separated hop. `current` tracks the schema
        // we look up the next hop's FK on; `prev_alias` tracks the
        // alias the next hop's ON predicate joins against on the
        // LHS. For the first hop, that LHS is the main table.
        let hops: Vec<&str> = name.split("__").collect();
        if hops.iter().any(|h| h.is_empty()) {
            return Err(QueryError::SelectRelatedInvalid {
                model: model.name,
                field: name.clone(),
                reason: format!(
                    "empty hop in chain `{name}` (use `field` or `field__nested`, not `field____nested`)"
                ),
            });
        }
        let mut current: &'static ModelSchema = model;
        let mut prev_alias: &'static str = model.table;
        let mut prev_alias_owned: String = String::new();
        for (depth, hop) in hops.iter().enumerate() {
            let field = current
                .field(hop)
                .ok_or_else(|| QueryError::SelectRelatedInvalid {
                    model: current.name,
                    field: name.clone(),
                    reason: format!(
                        "no field `{hop}` on `{}` (hop {} of chain `{name}`)",
                        current.name,
                        depth + 1
                    ),
                })?;
            let (to, on) = match field.relation {
                Some(Relation::Fk { to, on }) | Some(Relation::O2O { to, on }) => (to, on),
                _ => {
                    return Err(QueryError::SelectRelatedInvalid {
                        model: current.name,
                        field: name.clone(),
                        reason: format!(
                            "`{hop}` on `{}` is not a `ForeignKey<T>` field (hop {} of chain `{name}`)",
                            current.name,
                            depth + 1
                        ),
                    });
                }
            };
            let target = inventory::iter::<ModelEntry>
                .into_iter()
                .find(|e| e.schema.table == to)
                .map(|e| e.schema)
                .ok_or_else(|| QueryError::SelectRelatedInvalid {
                    model: current.name,
                    field: name.clone(),
                    reason: format!(
                        "target table `{to}` is not registered (hop {} of chain `{name}` — is `{}`'s `#[derive(Model)]` linked into the binary?)",
                        depth + 1,
                        current.name
                    ),
                })?;
            // Per-hop alias: the full dotted path up to and including
            // this hop. For single-hop chains this is just `field.name`
            // (matches pre-T2.2 behavior bit-identically). For multi-
            // hop, the alias accumulates: `author`, then
            // `author__profile`, then `author__profile__country`.
            //
            // Joins-alias names live for the lifetime of the QuerySet
            // compile, but the writer needs `&'static str`. For
            // single-hop we already have `field.name` (a `&'static
            // str` from the schema). For multi-hop, we leak the
            // composite alias intentionally: the count is small
            // (depth ≤ ~5 in practice) and bounded by the user's
            // schema, so this is a tiny one-time cost paid at
            // compile() time. Same trick `Join::alias` accepts a
            // `&'static str` elsewhere.
            let alias: &'static str = if hops.len() == 1 {
                field.name
            } else {
                // Build the cumulative alias.
                let owned = if depth == 0 {
                    (*hop).to_owned()
                } else {
                    format!("{prev_alias_owned}__{hop}")
                };
                prev_alias_owned = owned.clone();
                Box::leak(owned.into_boxed_str())
            };
            let project: Vec<&'static str> = target.scalar_fields().map(|f| f.column).collect();
            out.push(Join {
                target,
                alias,
                kind: JoinKind::Left,
                on: WhereExpr::ExprCompare {
                    lhs: Expr::AliasedColumn {
                        alias: prev_alias,
                        column: field.column,
                    },
                    op: Op::Eq,
                    rhs: Expr::AliasedColumn { alias, column: on },
                },
                project,
            });
            // Advance the chain for the next hop.
            current = target;
            prev_alias = alias;
        }
    }
    Ok(out)
}

/// Parse a Django-shape `"field"` or `"field__suffix"` key + raw
/// value into the matching `(field, Op, transformed_value)` triple.
/// Issue #71.
///
/// The suffix table is documented on [`QuerySet::filter`]. Failures:
///
/// - Unknown `__suffix` → [`QueryError::UnknownLookup`].
/// - Value-shape mismatch for `__in` / `__isnull` / `__between` /
///   `__range` → [`QueryError::InvalidLookupValue`].
///
/// Chained lookups (`a__b__icontains`) are NOT decomposed in v1 — the
/// FIRST `__` splits field from suffix, anything after stays as part
/// of the suffix and errors as `UnknownLookup`.
/// Result of [`parse_lookup`] — either a plain field/op/value triple
/// (existing v1 shape) or a date-transform wrapper that the resolver
/// re-renders as `EXTRACT(…) <op> ?` (issue #829).
enum ParsedLookup {
    Raw {
        field: String,
        op: Op,
        value: SqlValue,
    },
    DateTransform {
        field: String,
        transform: ScalarFn,
        op: Op,
        value: SqlValue,
    },
    /// Relation-spanning lookup — `author__name`, `author__name__icontains`,
    /// `author__profile__bio` (#1031). The key carries at least one
    /// `__`-separated FK hop before the terminal column + optional
    /// lookup suffix. Schema-agnostic at parse time: `parse_lookup`
    /// can't tell an FK hop from a scalar field, so it defers the whole
    /// key here and the resolver (`resolve_span`) walks it against the
    /// schema — building the JOINs and the aliased predicate, or
    /// surfacing the original `UnknownLookup` if no FK chain resolves.
    RelationSpan { raw_key: String, value: SqlValue },
}

/// Map a date-transform suffix token to its corresponding scalar fn.
/// `None` means "this isn't a date transform" — caller falls through
/// to the existing comparison-suffix table.
fn date_transform_fn(token: &str) -> Option<ScalarFn> {
    match token {
        "year" => Some(ScalarFn::ExtractYear),
        "month" => Some(ScalarFn::ExtractMonth),
        "day" => Some(ScalarFn::ExtractDay),
        "hour" => Some(ScalarFn::ExtractHour),
        "minute" => Some(ScalarFn::ExtractMinute),
        "second" => Some(ScalarFn::ExtractSecond),
        "quarter" => Some(ScalarFn::ExtractQuarter),
        "week" => Some(ScalarFn::ExtractWeek),
        "week_day" => Some(ScalarFn::ExtractWeekDay),
        "date" => Some(ScalarFn::TruncDate),
        _ => None,
    }
}

/// Map a trailing comparison suffix (`gte`, `lt`, …) to an `Op` for
/// date-transform lookups. The set is intentionally a strict subset
/// of the full lookup grammar — LIKE / ILIKE / IN / IS-NULL / regex
/// don't apply to scalar-extracted ints / dates.
fn date_compare_op(suffix: &str) -> Option<Op> {
    match suffix {
        "exact" => Some(Op::Eq),
        "ne" => Some(Op::Ne),
        "gt" => Some(Op::Gt),
        "gte" => Some(Op::Gte),
        "lt" => Some(Op::Lt),
        "lte" => Some(Op::Lte),
        _ => None,
    }
}

fn parse_lookup(key: &str, value: SqlValue) -> Result<ParsedLookup, QueryError> {
    // Bare field (no `__`) → exact match.
    let Some(split_at) = key.find("__") else {
        return Ok(ParsedLookup::Raw {
            field: key.to_owned(),
            op: Op::Eq,
            value,
        });
    };
    let field = key[..split_at].to_owned();
    let suffix = &key[split_at + 2..];

    // Date-transform suffixes (issue #829) — `__year`, `__year__gte`,
    // `__date`, etc. Split the suffix into (transform-token, optional
    // trailing comparison op). When the token doesn't match a date
    // transform, fall through to the legacy comparison-suffix table.
    let (transform_token, trailing) = match suffix.find("__") {
        Some(at) => (&suffix[..at], Some(&suffix[at + 2..])),
        None => (suffix, None),
    };
    if let Some(transform) = date_transform_fn(transform_token) {
        let op = match trailing {
            None => Op::Eq,
            Some(t) => date_compare_op(t).ok_or_else(|| QueryError::UnknownLookup {
                field: field.clone(),
                suffix: suffix.to_owned(),
            })?,
        };
        return Ok(ParsedLookup::DateTransform {
            field,
            transform,
            op,
            value,
        });
    }

    let pair = |field: String, op: Op, value: SqlValue| ParsedLookup::Raw { field, op, value };
    match suffix {
        "exact" => Ok(pair(field, Op::Eq, value)),
        "ne" => Ok(pair(field, Op::Ne, value)),
        "gt" => Ok(pair(field, Op::Gt, value)),
        "gte" => Ok(pair(field, Op::Gte, value)),
        "lt" => Ok(pair(field, Op::Lt, value)),
        "lte" => Ok(pair(field, Op::Lte, value)),
        "iexact" => Ok(pair(field, Op::ILike, value)),
        "contains" => {
            let v = wrap_like(&value, "%", "%", &field, suffix)?;
            Ok(pair(field, Op::Like, v))
        }
        "icontains" => {
            let v = wrap_like(&value, "%", "%", &field, suffix)?;
            Ok(pair(field, Op::ILike, v))
        }
        "startswith" => {
            let v = wrap_like(&value, "", "%", &field, suffix)?;
            Ok(pair(field, Op::Like, v))
        }
        "istartswith" => {
            let v = wrap_like(&value, "", "%", &field, suffix)?;
            Ok(pair(field, Op::ILike, v))
        }
        "endswith" => {
            let v = wrap_like(&value, "%", "", &field, suffix)?;
            Ok(pair(field, Op::Like, v))
        }
        "iendswith" => {
            let v = wrap_like(&value, "%", "", &field, suffix)?;
            Ok(pair(field, Op::ILike, v))
        }
        // Raw LIKE / ILIKE / NOT LIKE / NOT ILIKE — Eloquent
        // `whereLike` / `whereNotLike` parity. Unlike `__contains` /
        // `__startswith` / `__endswith` the value is bound verbatim
        // — the caller is responsible for `%` and `_` placement. Use
        // when you need a non-anchored pattern (`'%foo%bar%'`,
        // `'_o_'`, etc.) that the auto-wrap helpers can't express.
        "like" | "ilike" | "not_like" | "not_ilike" => {
            if !matches!(value, SqlValue::String(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::String(<LIKE pattern>)",
                    actual: sql_value_shape_name(&value),
                });
            }
            let op = match suffix {
                "like" => Op::Like,
                "ilike" => Op::ILike,
                "not_like" => Op::NotLike,
                "not_ilike" => Op::NotILike,
                _ => unreachable!(),
            };
            Ok(pair(field, op, value))
        }
        "in" | "not_in" => {
            // Eloquent `whereNotIn` parity. `viewset::build_lookup_filter`
            // has accepted `__not_in` for URL-bound query params since
            // v0.30; this arm closes the drift on the Rust-side
            // `.filter("field__not_in", SqlValue::List(...))` shape.
            if !matches!(value, SqlValue::List(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::List(...)",
                    actual: sql_value_shape_name(&value),
                });
            }
            let op = if suffix == "not_in" {
                Op::NotIn
            } else {
                Op::In
            };
            Ok(pair(field, op, value))
        }
        "isnull" => {
            if !matches!(value, SqlValue::Bool(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::Bool(true|false)",
                    actual: sql_value_shape_name(&value),
                });
            }
            Ok(pair(field, Op::IsNull, value))
        }
        "between" | "range" | "not_between" | "not_range" => {
            match &value {
                SqlValue::List(items) if items.len() == 2 => {}
                SqlValue::List(_) => {
                    return Err(QueryError::InvalidLookupValue {
                        field,
                        suffix: suffix.to_owned(),
                        expected: "SqlValue::List with exactly 2 elements [lo, hi]",
                        actual: "SqlValue::List with wrong arity",
                    });
                }
                other => {
                    return Err(QueryError::InvalidLookupValue {
                        field,
                        suffix: suffix.to_owned(),
                        expected: "SqlValue::List([lo, hi])",
                        actual: sql_value_shape_name(other),
                    });
                }
            }
            // Eloquent `whereNotBetween` parity — `__not_between` /
            // `__not_range` flip the predicate to `NOT BETWEEN`.
            let op = if matches!(suffix, "not_between" | "not_range") {
                Op::NotBetween
            } else {
                Op::Between
            };
            Ok(pair(field, op, value))
        }
        "regex" | "iregex" => {
            if !matches!(value, SqlValue::String(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::String(<regex pattern>)",
                    actual: sql_value_shape_name(&value),
                });
            }
            let op = if suffix == "regex" {
                Op::Regex
            } else {
                Op::IRegex
            };
            Ok(pair(field, op, value))
        }
        "trigram_similar" | "trigram_word_similar" => {
            if !matches!(value, SqlValue::String(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::String(<trigram pattern>)",
                    actual: sql_value_shape_name(&value),
                });
            }
            let op = if suffix == "trigram_similar" {
                Op::TrigramSimilar
            } else {
                Op::TrigramWordSimilar
            };
            Ok(pair(field, op, value))
        }
        "search" => {
            if !matches!(value, SqlValue::String(_)) {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::String(<search query>)",
                    actual: sql_value_shape_name(&value),
                });
            }
            Ok(pair(field, Op::Search, value))
        }
        // PG array operators (issue #30). The Django shape uses
        // `__contains` / `__contained_by` / `__overlap` on
        // ArrayField columns, but rustango can't dispatch by
        // field-type from the parser (the field-type info isn't
        // threaded through `.filter()`). We use explicit
        // `__array_*` suffixes to keep them disjoint from text
        // `__contains`; the typed-IR `Column::array_*` methods are
        // the preferred call site.
        "range_contains"
        | "range_contained_by"
        | "range_overlap"
        | "range_strictly_left"
        | "range_strictly_right"
        | "range_adjacent" => {
            // PG range operators. The bound value is a range literal
            // string (e.g. `"[1, 10)"`); PG implicit-casts to the
            // column's range type. Accept either a plain
            // `SqlValue::String` (auto-wrapped into `RangeLiteral`)
            // or a `SqlValue::RangeLiteral` from the typed-IR Column
            // helpers — both produce the same emit shape.
            let literal = match value {
                SqlValue::String(s) => s,
                SqlValue::RangeLiteral(s) => s,
                other => {
                    return Err(QueryError::InvalidLookupValue {
                        field,
                        suffix: suffix.to_owned(),
                        expected: "SqlValue::String(<PG range literal>) — e.g. \"[1, 10)\"",
                        actual: sql_value_shape_name(&other),
                    });
                }
            };
            let op = match suffix {
                "range_contains" => Op::RangeContains,
                "range_contained_by" => Op::RangeContainedBy,
                "range_overlap" => Op::RangeOverlap,
                "range_strictly_left" => Op::RangeStrictlyLeft,
                "range_strictly_right" => Op::RangeStrictlyRight,
                "range_adjacent" => Op::RangeAdjacent,
                _ => unreachable!(),
            };
            Ok(pair(field, op, SqlValue::RangeLiteral(literal)))
        }
        "array_contains" | "array_contained_by" | "array_overlap" => {
            // The bound value must be a list-of-elements; we
            // promote `SqlValue::List` to `SqlValue::Array` so the
            // writer binds it as a single PG array parameter.
            let SqlValue::List(elems) = value else {
                return Err(QueryError::InvalidLookupValue {
                    field,
                    suffix: suffix.to_owned(),
                    expected: "SqlValue::List(<elements>) — typed homogeneous array",
                    actual: sql_value_shape_name(&value),
                });
            };
            let op = match suffix {
                "array_contains" => Op::ArrayContains,
                "array_contained_by" => Op::ArrayContainedBy,
                "array_overlap" => Op::ArrayOverlap,
                _ => unreachable!(),
            };
            Ok(pair(field, op, SqlValue::Array(elems)))
        }
        // Unknown suffix — but the key may be a relation-spanning
        // lookup (`author__name`, `author__name__icontains`). We can't
        // tell here (no schema), so defer the whole key to the resolver.
        // It walks the FK chain against the schema; if the first segment
        // isn't an FK (a genuinely unknown suffix on a scalar field, e.g.
        // `pages__bogus`) it re-surfaces this exact `UnknownLookup`.
        // #1031.
        _ => Ok(ParsedLookup::RelationSpan {
            raw_key: key.to_owned(),
            value,
        }),
    }
}

/// Wrap a `SqlValue::String` with leading/trailing wildcard tokens
/// for `__contains` / `__startswith` / `__endswith` and their `i*`
/// case-insensitive variants. Issue #71.
///
/// Non-string values are rejected with [`QueryError::InvalidLookupValue`]
/// — LIKE patterns require text input.
fn wrap_like(
    value: &SqlValue,
    prefix: &str,
    suffix_char: &str,
    field: &str,
    suffix: &str,
) -> Result<SqlValue, QueryError> {
    let s = match value {
        SqlValue::String(s) => s,
        other => {
            return Err(QueryError::InvalidLookupValue {
                field: field.to_owned(),
                suffix: suffix.to_owned(),
                expected: "SqlValue::String(...)",
                actual: sql_value_shape_name(other),
            });
        }
    };
    Ok(SqlValue::String(format!("{prefix}{s}{suffix_char}")))
}

/// Human-readable shape name for an `SqlValue` — used in
/// [`QueryError::InvalidLookupValue`] messages.
fn sql_value_shape_name(v: &SqlValue) -> &'static str {
    match v {
        SqlValue::Null => "SqlValue::Null",
        SqlValue::I16(_) => "SqlValue::I16",
        SqlValue::I32(_) => "SqlValue::I32",
        SqlValue::I64(_) => "SqlValue::I64",
        SqlValue::F32(_) => "SqlValue::F32",
        SqlValue::F64(_) => "SqlValue::F64",
        SqlValue::Bool(_) => "SqlValue::Bool",
        SqlValue::String(_) => "SqlValue::String",
        SqlValue::List(_) => "SqlValue::List",
        _ => "SqlValue::<other>",
    }
}

/// #331 — append an always-false predicate onto `base` so the
/// surrounding statement (UPDATE / DELETE) cannot match any row.
/// Uses `<primary_key> IS NULL`: every Django/rustango model declares a
/// NOT NULL primary key, so this predicate is unsatisfiable across
/// every backend and every row. We deliberately don't try `1 = 0` /
/// `FALSE` literal predicates — those need raw-SQL escape hatches our
/// IR doesn't expose today, and the IS NULL trick lives entirely in
/// existing `Op::IsNull` machinery.
fn never_match_clause(
    model: &'static ModelSchema,
    base: WhereExpr,
) -> Result<WhereExpr, QueryError> {
    let pk = model
        .primary_key()
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: "<primary_key>".to_owned(),
        })?;
    let never = WhereExpr::Predicate(crate::core::Filter {
        column: pk.column,
        op: Op::IsNull,
        value: SqlValue::Bool(true),
    });
    Ok(match base {
        // Vacuously-true `And(vec![])` collapses to just the never-match
        // predicate — keeps the emitted WHERE clean (single column ref).
        WhereExpr::And(nodes) if nodes.is_empty() => never,
        WhereExpr::And(mut nodes) => {
            nodes.push(never);
            WhereExpr::And(nodes)
        }
        other => WhereExpr::And(vec![other, never]),
    })
}

/// Parse a `key`/`value` pair through the lookup grammar into a single
/// [`PendingFilter`]. Shared by [`QuerySet::filter`] and
/// [`QuerySet::exclude`] (#1030) so the `__lookup` suffix grammar can
/// never drift between the two.
fn parse_to_pending(key: &str, value: SqlValue) -> PendingFilter {
    match parse_lookup(key, value) {
        Ok(ParsedLookup::Raw { field, op, value }) => {
            PendingFilter::Raw(RawFilter { field, op, value })
        }
        Ok(ParsedLookup::DateTransform {
            field,
            transform,
            op,
            value,
        }) => PendingFilter::DateTransform(DateTransformFilter {
            field,
            transform,
            op,
            value,
        }),
        Ok(ParsedLookup::RelationSpan { raw_key, value }) => {
            PendingFilter::RelationSpan { raw_key, value }
        }
        Err(e) => PendingFilter::Error(e),
    }
}

fn resolve_pending(
    model: &'static ModelSchema,
    pending: Vec<PendingFilter>,
) -> Result<WhereExpr, QueryError> {
    let mut nodes: Vec<WhereExpr> = Vec::with_capacity(pending.len());
    for entry in pending {
        nodes.push(resolve_one_pending(model, entry)?);
    }
    Ok(WhereExpr::And(nodes))
}

/// Lower a single [`PendingFilter`] to a [`WhereExpr`]. Split out of
/// [`resolve_pending`]'s loop so [`PendingFilter::Negated`] can resolve
/// its inner entry recursively (#1030).
fn resolve_one_pending(
    model: &'static ModelSchema,
    entry: PendingFilter,
) -> Result<WhereExpr, QueryError> {
    Ok(match entry {
        PendingFilter::Raw(raw) => WhereExpr::Predicate(resolve_filter(model, raw)?),
        PendingFilter::DateTransform(dt) => resolve_date_transform(model, dt)?,
        PendingFilter::Resolved(filter) => WhereExpr::Predicate(filter),
        PendingFilter::Expr(expr) => expr,
        PendingFilter::Negated(inner) => {
            WhereExpr::Not(Box::new(resolve_one_pending(model, *inner)?))
        }
        // #1031 — relation spans are lowered to `Expr` + JOINs by the
        // SELECT compile() pre-pass (`lower_relation_spans`). Reaching
        // here still as a span means a context without join support
        // (update / delete / aggregate). Resolve first to distinguish a
        // genuine bad suffix (`status__bogus` → first segment isn't an
        // FK → `resolve_span` returns the original `UnknownLookup`,
        // which we preserve) from a real relation span (resolves OK, but
        // the JOIN can't be carried here → clear error).
        PendingFilter::RelationSpan { raw_key, value } => {
            return match resolve_span(model, &raw_key, value) {
                Ok(_) => Err(QueryError::RelationSpanUnsupportedHere { key: raw_key }),
                Err(e) => Err(e),
            }
        }
        // Issue #71 — bubble the deferred lookup-parse error up at the
        // natural `compile()` checkpoint.
        PendingFilter::Error(e) => return Err(e),
    })
}

fn resolve_filter(model: &'static ModelSchema, raw: RawFilter) -> Result<Filter, QueryError> {
    let field = model
        .field(&raw.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: raw.field.clone(),
        })?;

    // `IsNull` carries a Bool sentinel (true = IS NULL, false = IS NOT NULL),
    // not a value to compare against the field — skip the type check.
    // `In` carries a List; element-by-element checking is a follow-up.
    let skip_type_check = matches!(raw.op, Op::IsNull | Op::In);

    if !skip_type_check {
        if let Some(value_ty) = raw.value.field_type() {
            if value_ty != field.ty {
                return Err(QueryError::TypeMismatch {
                    model: model.name,
                    field: raw.field,
                    expected: field.ty,
                    actual: value_ty,
                });
            }
        }
    }

    Ok(Filter {
        column: field.column,
        op: raw.op,
        value: raw.value,
    })
}

/// #1031 — resolve a relation-spanning lookup key into its FK-chain
/// LEFT JOINs plus an aliased `ExprCompare` predicate.
///
/// `author__name` / `author__profile__bio__icontains` / `author__profile`
/// all decompose as: leading segments that name an FK/O2O field are
/// JOIN hops; the first non-FK (or the final) segment is the terminal
/// column; any remaining segments are a lookup suffix re-parsed through
/// the normal grammar ([`parse_lookup`]) and re-targeted onto the
/// aliased column. The terminal `ExprCompare` carries `rhs:
/// Expr::Literal(value)` for every op the [`write_expr_compare`] writer
/// supports (binary / LIKE / ILIKE / IN / BETWEEN / IS NULL).
///
/// When the first segment isn't an FK the key isn't a relation span at
/// all (e.g. `pages__bogus`) — we re-surface the original
/// [`QueryError::UnknownLookup`] so existing errors are unchanged.
fn resolve_span(
    model: &'static ModelSchema,
    raw_key: &str,
    value: SqlValue,
) -> Result<(Vec<crate::core::Join>, WhereExpr), QueryError> {
    use crate::core::{inventory, Expr, Join, JoinKind, ModelEntry, Relation};
    let segs: Vec<&str> = raw_key.split("__").collect();
    let mut joins: Vec<Join> = Vec::new();
    let mut current: &'static ModelSchema = model;
    let mut prev_alias: &'static str = model.table;
    let mut alias_path = String::new();
    let mut term_i = segs.len() - 1;
    for (i, seg) in segs.iter().enumerate() {
        let is_last = i + 1 == segs.len();
        let field = current.field(seg);
        let fk = field.and_then(|f| match f.relation {
            Some(Relation::Fk { to, on }) | Some(Relation::O2O { to, on }) => Some((f, to, on)),
            _ => None,
        });
        match fk {
            // An FK that isn't the final segment is a JOIN hop.
            Some((field, to, on)) if !is_last => {
                let target = inventory::iter::<ModelEntry>
                    .into_iter()
                    .find(|e| e.schema.table == to)
                    .map(|e| e.schema)
                    .ok_or_else(|| QueryError::UnknownField {
                        model: current.name,
                        field: format!("{seg} (target table `{to}` not registered)"),
                    })?;
                alias_path = if alias_path.is_empty() {
                    (*seg).to_owned()
                } else {
                    format!("{alias_path}__{seg}")
                };
                // Leak the cumulative dotted alias — bounded by the
                // user's schema depth, paid once at compile() time;
                // matches `lower_select_related`'s alias scheme so a
                // span + `select_related` over the same path dedupe.
                let alias: &'static str = Box::leak(alias_path.clone().into_boxed_str());
                let project: Vec<&'static str> = target.scalar_fields().map(|f| f.column).collect();
                joins.push(Join {
                    target,
                    alias,
                    kind: JoinKind::Left,
                    on: WhereExpr::ExprCompare {
                        lhs: Expr::AliasedColumn {
                            alias: prev_alias,
                            column: field.column,
                        },
                        op: Op::Eq,
                        rhs: Expr::AliasedColumn { alias, column: on },
                    },
                    project,
                });
                current = target;
                prev_alias = alias;
            }
            _ => {
                term_i = i;
                break;
            }
        }
    }

    // No hop consumed → the first segment isn't an FK, so this is a
    // plain unknown suffix on a scalar field — re-surface the original
    // error (`pages__bogus` → UnknownLookup{ pages, bogus }).
    if joins.is_empty() {
        let suffix = raw_key.splitn(2, "__").nth(1).unwrap_or("").to_owned();
        return Err(QueryError::UnknownLookup {
            field: segs.first().map_or_else(String::new, |s| (*s).to_owned()),
            suffix,
        });
    }

    let term_field = current
        .field(segs[term_i])
        .ok_or_else(|| QueryError::UnknownField {
            model: current.name,
            field: segs[term_i].to_owned(),
        })?;
    let suffix_segs = &segs[term_i + 1..];

    // Re-parse any trailing suffix through the normal grammar; bare
    // span (no suffix) is an exact match.
    let (op, value, transform) = if suffix_segs.is_empty() {
        (Op::Eq, value, None)
    } else {
        let synth = format!("{}__{}", term_field.name, suffix_segs.join("__"));
        match parse_lookup(&synth, value) {
            Ok(ParsedLookup::Raw { op, value, .. }) => (op, value, None),
            Ok(ParsedLookup::DateTransform {
                transform,
                op,
                value,
                ..
            }) => (op, value, Some(transform)),
            // A trailing suffix that's still unknown is a genuine bad
            // lookup on the terminal field.
            Ok(ParsedLookup::RelationSpan { .. }) => {
                return Err(QueryError::UnknownLookup {
                    field: term_field.name.to_owned(),
                    suffix: suffix_segs.join("__"),
                })
            }
            Err(e) => return Err(e),
        }
    };
    let column = Expr::AliasedColumn {
        alias: prev_alias,
        column: term_field.column,
    };
    let lhs = match transform {
        None => column,
        Some(kind) => Expr::Function {
            kind,
            args: vec![column],
        },
    };
    let predicate = WhereExpr::ExprCompare {
        lhs,
        op,
        rhs: Expr::Literal(value),
    };
    Ok((joins, predicate))
}

/// #1031 — pre-pass over the pending filters that lowers every
/// relation-spanning lookup ([`PendingFilter::RelationSpan`], including
/// inside [`PendingFilter::Negated`] from `exclude`) into a resolved
/// `Expr` predicate, accumulating the FK-chain JOINs. Returns the
/// rewritten pending list plus the JOINs to merge (deduped by alias) on
/// the SELECT path.
fn lower_relation_spans(
    model: &'static ModelSchema,
    pending: Vec<PendingFilter>,
) -> Result<(Vec<PendingFilter>, Vec<crate::core::Join>), QueryError> {
    let mut joins: Vec<crate::core::Join> = Vec::new();
    let mut out = Vec::with_capacity(pending.len());
    for pf in pending {
        out.push(convert_relation_span(model, pf, &mut joins)?);
    }
    Ok((out, joins))
}

fn convert_relation_span(
    model: &'static ModelSchema,
    pf: PendingFilter,
    joins: &mut Vec<crate::core::Join>,
) -> Result<PendingFilter, QueryError> {
    Ok(match pf {
        PendingFilter::RelationSpan { raw_key, value } => {
            let (jns, predicate) = resolve_span(model, &raw_key, value)?;
            for j in jns {
                if !joins.iter().any(|e| e.alias == j.alias) {
                    joins.push(j);
                }
            }
            PendingFilter::Expr(predicate)
        }
        PendingFilter::Negated(inner) => {
            PendingFilter::Negated(Box::new(convert_relation_span(model, *inner, joins)?))
        }
        other => other,
    })
}

/// Resolve a [`DateTransformFilter`] into a `WhereExpr::ExprCompare`
/// node — the field is looked up against the schema and wrapped in
/// the corresponding [`ScalarFn`] (`EXTRACT(YEAR FROM …)`, etc.) on
/// the LHS; the bound value lands as a literal on the RHS. Issue #829.
///
/// Skips the value/column type-equality check that
/// [`resolve_filter`] applies — date-transform LHS values are scalar
/// ints / dates rather than the column's native type, so the literal
/// shape isn't expected to match the field type.
fn resolve_date_transform(
    model: &'static ModelSchema,
    dt: DateTransformFilter,
) -> Result<WhereExpr, QueryError> {
    let field = model
        .field(&dt.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: dt.field.clone(),
        })?;
    Ok(WhereExpr::ExprCompare {
        lhs: Expr::Function {
            kind: dt.transform,
            args: vec![Expr::Column(field.column)],
        },
        op: dt.op,
        rhs: Expr::Literal(dt.value),
    })
}

fn resolve_assignment(
    model: &'static ModelSchema,
    raw: RawAssignment,
) -> Result<Assignment, QueryError> {
    let field = model
        .field(&raw.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: raw.field.clone(),
        })?;

    if let Some(value_ty) = raw.value.field_type() {
        if value_ty != field.ty {
            return Err(QueryError::TypeMismatch {
                model: model.name,
                field: raw.field,
                expected: field.ty,
                actual: value_ty,
            });
        }
    }

    Ok(Assignment {
        column: field.column,
        value: raw.value.into(),
    })
}

/// Resolve a [`RawExprAssignment`] (the `F()` SET path) against the
/// schema. Field name + every column reference inside the expression
/// tree are validated; the literal type-check that
/// [`resolve_assignment`] does for [`SqlValue`] doesn't apply because
/// the RHS may be a column ref or arithmetic, both of which only have
/// a resolved type at the row level.
fn resolve_assignment_expr(
    model: &'static ModelSchema,
    raw: RawExprAssignment,
) -> Result<Assignment, QueryError> {
    let field = model
        .field(&raw.field)
        .ok_or_else(|| QueryError::UnknownField {
            model: model.name,
            field: raw.field.clone(),
        })?;
    validate_expr_columns_in_model(model, &raw.value)?;
    Ok(Assignment {
        column: field.column,
        value: raw.value,
    })
}

/// Walk an [`crate::core::Expr`] and confirm every `Column`
/// reference resolves on `model`. Mirrors the same check that
/// `core::query::WhereExpr::validate` does for `ColumnCompare` —
/// duplicated here so the `UpdateBuilder` path catches typos at
/// `compile()` time rather than at the database.
fn validate_expr_columns_in_model(
    model: &'static ModelSchema,
    expr: &crate::core::Expr,
) -> Result<(), QueryError> {
    use crate::core::Expr;
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) => {
            if model.field_by_column(name).is_none() {
                Err(QueryError::UnknownField {
                    model: model.name,
                    field: (*name).to_owned(),
                })
            } else {
                Ok(())
            }
        }
        Expr::BinOp { left, right, .. } => {
            validate_expr_columns_in_model(model, left)?;
            validate_expr_columns_in_model(model, right)
        }
        Expr::Function { args, .. } => {
            for a in args {
                validate_expr_columns_in_model(model, a)?;
            }
            Ok(())
        }
        Expr::Cast { expr: inner, .. } => validate_expr_columns_in_model(model, inner),
        Expr::Case { branches, default } => {
            for b in branches {
                b.condition.validate(model)?;
                validate_expr_columns_in_model(model, &b.then)?;
            }
            if let Some(d) = default {
                validate_expr_columns_in_model(model, d)?;
            }
            Ok(())
        }
        // Subqueries validate against their own model at the time
        // they were compiled via QuerySet::compile(); OuterRef
        // names an outer column resolved when this Expr is embedded
        // in the outer queryset (the caller already validated it
        // there). `AliasedColumn` (issue #80) carries its own table
        // alias and is validated by the JOIN writer at emit time.
        // `AggregateSubquery` (issue #830) validates against its own
        // child model at construction; nothing resolves against this one.
        Expr::Subquery(_)
        | Expr::AggregateSubquery(_)
        | Expr::OuterRef(_)
        // `RelAggregate` (issue #830) aggregates a raw relation table;
        // its columns resolve there, not on this model.
        | Expr::RelAggregate { .. }
        | Expr::AliasedColumn { .. } => Ok(()),
        // Window (issue #7) — partition_by + order_by + arg columns
        // reference the model. Walk them.
        Expr::Window(w) => {
            for col in &w.partition_by {
                if model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            for o in &w.order_by {
                if model.field_by_column(o.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: o.column.to_owned(),
                    });
                }
            }
            for arg in &w.args {
                validate_expr_columns_in_model(model, arg)?;
            }
            Ok(())
        }
        // Aggregate (issue #74) — flat variants hold raw column
        // names that this validator doesn't traverse; pre-existing
        // gap. Window-shaped aggregates are validated via the
        // dedicated walker in `validate_aggregate_expr_columns`.
        Expr::Aggregate(_) => Ok(()),
        // JsonPath (issue #296 / T2.3) — the `source` is a column
        // expression to validate; the path steps are JSON-pointer
        // keys/indices and don't reference model columns themselves.
        Expr::JsonPath { source, .. } => validate_expr_columns_in_model(model, source),
    }
}

// ------------------------------------------------------------------ AggregateBuilder

/// Fluent builder for [`AggregateQuery`]. Constructed via [`QuerySet::aggregate`].
pub struct AggregateBuilder<T: Model> {
    qs: QuerySet<T>,
    group_by: Vec<&'static str>,
    aggregates: Vec<(std::borrow::Cow<'static, str>, AggregateExpr)>,
    /// Django 3.2 `.alias()` — non-projected annotations. Resolvable in
    /// `.filter(name, …)` and `.order_by([(name, …)])` (the builder lifts
    /// the expression into the predicate/ORDER item at `compile()` time),
    /// but never emitted in the SELECT list. Issue #268.
    aliases: Vec<(std::borrow::Cow<'static, str>, AggregateExpr)>,
    having: Option<WhereExpr>,
    order_by: Vec<(&'static str, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// Issue #74 — deferred error surfacing for builder-time
    /// validation failures (e.g. `.filter(alias, Op::JsonContains, …)`
    /// against an annotation alias, which the auto-routing rejects
    /// because the JSON-op family + null-safe equality need
    /// dialect-specific writers that don't compose against an
    /// aggregate LHS). Stored on first error; subsequent builder
    /// calls are no-ops so the original cause isn't masked. Surfaced
    /// from `compile()`. (Issue #87 widened the supported op set to
    /// include `IN`/`BETWEEN`/`IS NULL`/`LIKE`/`ILIKE` + negated
    /// variants.)
    deferred_error: Option<crate::core::QueryError>,
    /// Issue #75 — projection columns set via [`Self::values`] (or
    /// [`QuerySet::values`]). When `Some(cols)` and the user hasn't
    /// called `.group_by(...)` explicitly, `compile()` derives
    /// `GROUP BY cols`. When `None` and an aggregating annotation is
    /// present, `compile()` falls back to Django Shape 3 — `GROUP BY`
    /// every non-aggregate scalar column on the model.
    values: Option<Vec<&'static str>>,
}

impl<T: Model> AggregateBuilder<T> {
    /// Add a `GROUP BY` column. Call multiple times to group by multiple columns.
    ///
    /// Explicit `.group_by(...)` calls always win — when paired with
    /// [`Self::values`] / [`QuerySet::values`], the explicit list is what
    /// reaches the writer; the values list still drives the projection
    /// SELECT list (issue #75).
    #[must_use]
    pub fn group_by(mut self, column: &'static str) -> Self {
        self.group_by.push(column);
        self
    }

    /// Set the projection column list — same semantic as
    /// [`QuerySet::values`] but available mid-chain when you started
    /// from `.aggregate()` rather than `.values(...)`. Issue #75.
    #[must_use]
    pub fn values(mut self, columns: &[&'static str]) -> Self {
        self.values = Some(columns.to_vec());
        self
    }

    /// Add an aggregate expression under `alias` (e.g. `"post_count"`).
    #[must_use]
    pub fn annotate(mut self, alias: &'static str, expr: AggregateExpr) -> Self {
        self.aggregates
            .push((std::borrow::Cow::Borrowed(alias), expr));
        self
    }

    /// Project a correlated scalar subquery under `alias` — Django's
    /// `annotate(x=Subquery(...))` (#1036). Sugar over [`Self::annotate`]
    /// with [`crate::core::subquery::scalar_subquery`]; shape `inner` to
    /// one column × ≤1 row (see that function's caller contract).
    #[must_use]
    pub fn annotate_subquery(self, alias: &'static str, inner: crate::core::SelectQuery) -> Self {
        self.annotate(alias, crate::core::subquery::scalar_subquery(inner))
    }

    /// Django 3.2 `.alias()` — annotate without projecting. The expression
    /// is registered under `name` and resolvable in [`Self::filter`] /
    /// [`Self::order_by`] (the builder lifts it in-place at compile time),
    /// but the writer **omits it from the SELECT list**. Issue #268.
    ///
    /// Use this when you want to filter or order by a derived aggregate
    /// without paying the column-decode cost on every row:
    ///
    /// ```ignore
    /// // Authors with > 5 posts, ordered by post count desc — but no post
    /// // count column in the result row.
    /// Author::objects()
    ///     .aggregate()
    ///     .group_by("id")
    ///     .alias("c", count_all().into())
    ///     .filter("c", Op::Gt, 5_i64)
    ///     .order_by(&[("c", true)])
    ///     .compile()?;
    /// // SELECT "id" FROM "author"
    /// // GROUP BY "id"
    /// // HAVING COUNT(*) > $1
    /// // ORDER BY COUNT(*) DESC
    /// ```
    ///
    /// **Chain ordering matters** — same caveat as [`Self::annotate`]: call
    /// `.alias(name, ...)` BEFORE the corresponding `.filter(name, ...)` /
    /// `.order_by([(name, ...)])` so the registry lookup sees it.
    ///
    /// When `name` collides with an existing annotate alias, the existing
    /// annotate wins (it's projected; semantics-preserving).
    #[must_use]
    pub fn alias(mut self, name: &'static str, expr: AggregateExpr) -> Self {
        self.aliases.push((std::borrow::Cow::Borrowed(name), expr));
        self
    }

    /// Add a `HAVING` predicate. Multiple calls AND-join.
    #[must_use]
    pub fn having<E: Into<crate::core::TypedExpr<T>>>(mut self, predicate: E) -> Self {
        let expr = predicate.into().into_expr();
        match self.having {
            None => self.having = Some(expr),
            Some(ref mut existing) => existing.push_and(expr),
        }
        self
    }

    /// String-keyed filter with WHERE/HAVING auto-routing — issue #74.
    /// Django's `.filter()` semantic on an aggregating queryset: if
    /// `field` matches an annotation alias added via [`Self::annotate`],
    /// the predicate lands in `HAVING` (alongside any explicit
    /// [`Self::having`] calls). Otherwise it forwards to the WHERE
    /// path on the underlying `QuerySet`.
    ///
    /// ```ignore
    /// // Authors with > 10 published posts:
    /// Author::objects()
    ///     .aggregate()
    ///     .group_by("id")
    ///     .annotate("post_count", count_all().filter(Post::status.eq("published")).into())
    ///     .filter("post_count", Op::Gt, 10_i64)   // → HAVING (annotation alias)
    ///     .filter("active", Op::Eq, true)         // → WHERE  (model column)
    ///     .compile()?;
    /// ```
    ///
    /// **Chain ordering matters**: `.annotate(alias, ...)` must come
    /// BEFORE the corresponding `.filter(alias, ...)` so the
    /// alias-registry lookup sees it. (Django defers this resolution
    /// to query construction; rustango v1 resolves at call time —
    /// reordering may land in a future slice.)
    ///
    /// **Validator gap**: alias-routed HAVING predicates skip the
    /// model-schema column walk (the alias isn't a real column).
    /// Typo'd aliases surface at the DB, not at `compile()`.
    /// WHERE-routed predicates still go through `resolve_pending`'s
    /// schema validation.
    #[must_use]
    pub fn filter(
        mut self,
        field: &'static str,
        op: crate::core::Op,
        value: impl Into<crate::core::SqlValue>,
    ) -> Self {
        // Once we've recorded a deferred error, swallow subsequent
        // builder calls so the original cause isn't masked by a
        // downstream complaint.
        if self.deferred_error.is_some() {
            return self;
        }
        // Look up the AggregateExpr behind the alias if one exists.
        // PG strictly disallows SELECT-list aliases in HAVING (only
        // MySQL + SQLite allow it), so we LIFT the aggregate
        // expression into the predicate rather than passing the
        // alias by name — `HAVING COUNT(*) > $1` emits uniformly on
        // every backend.
        // Issue #268 — `.alias()` annotations also register as lift-able
        // names. Annotate wins on collision (projected entry first).
        let agg = self
            .aggregates
            .iter()
            .chain(self.aliases.iter())
            .find(|(alias, _)| *alias == field)
            .map(|(_, expr)| expr.clone());
        if let Some(agg) = agg {
            // Issue #87 — `write_expr_compare` now handles the SQL-92
            // standard predicates that compose against an aggregate
            // LHS (`IN` / `NOT IN` / `BETWEEN` / `IS NULL` / `LIKE` /
            // `NOT LIKE` / `ILIKE` / `NOT ILIKE`) in addition to the
            // binary-comparison set. JSON ops and null-safe equality
            // (`IS DISTINCT FROM` / `IS NOT DISTINCT FROM`) still
            // need dialect-specific writers that take a `&str` for
            // the LHS, so they keep rejecting with the targeted error.
            if matches!(
                op,
                crate::core::Op::JsonContains
                    | crate::core::Op::JsonContainedBy
                    | crate::core::Op::JsonHasKey
                    | crate::core::Op::JsonHasAnyKey
                    | crate::core::Op::JsonHasAllKeys
                    | crate::core::Op::IsDistinctFrom
                    | crate::core::Op::IsNotDistinctFrom
            ) {
                self.deferred_error = Some(crate::core::QueryError::HavingOpNotSupported {
                    alias: field.to_owned(),
                    op,
                });
                return self;
            }
            let pred = WhereExpr::ExprCompare {
                lhs: crate::core::Expr::Aggregate(Box::new(agg)),
                op,
                rhs: crate::core::Expr::Literal(value.into()),
            };
            match self.having {
                None => self.having = Some(pred),
                Some(ref mut existing) => existing.push_and(pred),
            }
        } else {
            // Forward to the underlying QuerySet's WHERE path. Same
            // schema-validation rules apply at `resolve_pending` time
            // — typo'd model columns get caught at `compile()`.
            self.qs = self.qs.filter_op(field, op, value);
        }
        self
    }

    /// Add `ORDER BY` columns. `desc = true` → DESC.
    #[must_use]
    pub fn order_by(mut self, items: &[(&'static str, bool)]) -> Self {
        self.order_by.extend_from_slice(items);
        self
    }

    /// Set `LIMIT`.
    #[must_use]
    pub fn limit(mut self, n: i64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Set `OFFSET`.
    #[must_use]
    pub fn offset(mut self, n: i64) -> Self {
        self.offset = Some(n);
        self
    }

    /// Compile to an [`AggregateQuery`] IR.
    ///
    /// # Errors
    /// Returns [`QueryError`] if any filter or having clause names an
    /// unknown field, or if `.values()` was called without a
    /// subsequent aggregating `.annotate(...)`.
    ///
    /// # GROUP BY inference (issue #75)
    ///
    /// When the user hasn't called `.group_by(...)` explicitly, the
    /// builder fills it in:
    ///
    /// * `.values(cols).annotate(agg)` → `GROUP BY cols` (Django Shape 2).
    /// * `.annotate(agg)` (no values) → `GROUP BY` every non-aggregate
    ///   scalar column on the model (Django Shape 3).
    /// * `.annotate(window)` only → no GROUP BY (window functions are
    ///   per-row).
    ///
    /// Explicit `.group_by(...)` always wins.
    pub fn compile(mut self) -> Result<AggregateQuery, QueryError> {
        // Surface any builder-time deferred error first (e.g. an
        // `Op::In` against an annotation alias) so the user sees the
        // real cause rather than a downstream compile failure.
        if let Some(e) = self.deferred_error {
            return Err(e);
        }
        // Issue #820 — fold global scopes into the WHERE so aggregates
        // honor the same auto-applied filter (e.g. `.count()`
        // against a `published_only`-scoped model counts published
        // rows only, not the table's full row count).
        self.qs.apply_global_scopes();
        let model = T::SCHEMA;
        let where_clause = resolve_pending(model, self.qs.pending)?;
        // Walk each AggregateExpr for column-name typos. Today this
        // catches partition_by / order_by / args inside an
        // `AggregateExpr::Window` (issue #7) — the older `Sum("col")` /
        // `Count(Some("col"))` shapes don't validate yet; that's a
        // pre-existing gap orthogonal to this slice.
        for (_alias, expr) in self.aggregates.iter().chain(self.aliases.iter()) {
            validate_aggregate_expr_columns(model, expr)?;
        }
        // Issue #268 — `.order_by((name, desc))` resolves against the same
        // alias registry as `.filter(name, ...)`. If the name matches an
        // annotate OR alias entry, we lift the aggregate expression so the
        // emitted `ORDER BY` references the full expression — required when
        // the name belongs to `.alias()` (not in SELECT) but also harmless
        // for `.annotate()` (the writer compares structurally).
        let alias_for = |name: &str| -> Option<crate::core::AggregateExpr> {
            self.aggregates
                .iter()
                .chain(self.aliases.iter())
                .find(|(a, _)| a.as_ref() == name)
                .map(|(_, e)| e.clone())
        };
        let order_by = self
            .order_by
            .into_iter()
            .map(|(col, desc)| match alias_for(col) {
                Some(agg) => {
                    crate::core::OrderItem::expr(crate::core::Expr::Aggregate(Box::new(agg)), desc)
                }
                None => crate::core::OrderItem::column(col, desc),
            })
            .collect();

        // Issue #75 — GROUP BY auto-inference. `.alias()` annotations
        // (issue #268) participate in this check: a pure `.alias(...,
        // Count(...))` still needs GROUP BY even though nothing is
        // projected.
        let has_aggregating = self
            .aggregates
            .iter()
            .chain(self.aliases.iter())
            .any(|(_, e)| e.is_aggregating());
        let group_by = if !self.group_by.is_empty() {
            // Explicit `.group_by(...)` always wins. Validate columns
            // belong to the model (caught early — typos otherwise
            // surface at the DB).
            for col in &self.group_by {
                if model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            self.group_by
        } else if let Some(cols) = self.values.as_ref() {
            // `.values(cols)` set. Without an aggregating annotation
            // we'd produce a degenerate `SELECT cols GROUP BY cols`
            // (distinct-projection) — refuse and point the user at
            // the right path for pure projection.
            if !has_aggregating {
                return Err(QueryError::ValuesRequiresAggregate { cols: cols.clone() });
            }
            for col in cols {
                if model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            cols.clone()
        } else if has_aggregating {
            // Shape 3 — group by every scalar column on the model.
            // Mirrors Django's "implicit GROUP BY all selected
            // non-aggregate columns".
            model.scalar_fields().map(|f| f.column).collect()
        } else {
            // No aggregating annotation, no values — pure window
            // annotation path (issue #7) or empty builder. No GROUP BY.
            Vec::new()
        };

        // #331 — `.none().count()` / `.none().aggregate(...)` must
        // return zero / empty result without scanning any row. Apply
        // the same never-match guard the UPDATE / DELETE path uses;
        // also clamp `LIMIT 0` so the executor short-circuits even if
        // group-by would produce rows from non-matched buckets.
        let (where_clause, limit) = if self.qs.is_none {
            (never_match_clause(model, where_clause)?, Some(0))
        } else {
            (where_clause, self.limit)
        };

        Ok(AggregateQuery {
            model,
            where_clause,
            group_by,
            aggregates: self.aggregates,
            aliases: self.aliases,
            having: self.having,
            order_by,
            limit,
            offset: self.offset,
        })
    }
}

/// Walk an [`AggregateExpr`] for column references that should
/// resolve against `model`. Today only `AggregateExpr::Window`
/// (issue #7) carries non-trivial column refs the schema can check —
/// partition_by + order_by columns + any `Expr::Column` arg. The
/// flat aggregate variants (`Sum("col")`, etc.) hold raw `&'static str`
/// column names that the existing validator chain doesn't visit; that
/// gap is orthogonal to this walk and worth a follow-up.
///
/// [`AggregateExpr`]: crate::core::AggregateExpr
fn validate_aggregate_expr_columns(
    model: &'static ModelSchema,
    expr: &crate::core::AggregateExpr,
) -> Result<(), QueryError> {
    use crate::core::AggregateExpr;
    match expr {
        AggregateExpr::Filtered { inner, filter } => {
            filter.validate(model)?;
            validate_aggregate_expr_columns(model, inner)
        }
        AggregateExpr::Coalesced { inner, .. } => validate_aggregate_expr_columns(model, inner),
        AggregateExpr::Window(w) => {
            for col in &w.partition_by {
                if model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            for o in &w.order_by {
                if model.field_by_column(o.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: o.column.to_owned(),
                    });
                }
            }
            for arg in &w.args {
                validate_expr_columns_in_model(model, arg)?;
            }
            Ok(())
        }
        // Flat aggregate variants (Count/Sum/Avg/Max/Min/CountDistinct/
        // StdDev*/Variance*) — the column they reference is a bare
        // `&'static str` not currently part of the validation chain.
        // Schema typos surface at execution. Pre-existing gap.
        _ => Ok(()),
    }
}

// ====================================================================
// Pure projection — Django `.values()` / `.values_list()` (issue #22)
// ====================================================================

/// Pure-projection queryset returned by [`QuerySet::values_dict`].
/// Compiles to a `SELECT <cols> FROM …` with the WHERE / ORDER BY /
/// LIMIT / OFFSET / set-algebra branches of the underlying queryset
/// preserved. Terminal `fetch` lives in
/// [`crate::sql::fetch_values_dict_pool`].
pub struct ValuesQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) cols: Vec<&'static str>,
}

/// Pure-projection queryset returned by [`QuerySet::values_list`].
/// Same shape as [`ValuesQuerySet`] but the terminal fetch yields
/// `Vec<Vec<SqlValue>>` (cells ordered to match `cols`) instead of a
/// `HashMap`.
pub struct ValuesListQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) cols: Vec<&'static str>,
}

/// Single-column flat-projection queryset returned by
/// [`QuerySet::values_list_flat`]. Terminal fetch decodes the column
/// directly into `Vec<U>` via sqlx's typed scalar path.
pub struct ValuesFlatQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) col: &'static str,
}

impl<T: Model> ValuesQuerySet<T> {
    /// Compile to a [`SelectQuery`] with `projection` set to the
    /// validated column list.
    ///
    /// # Errors
    /// - [`QueryError::EmptyValuesProjection`] if `cols` is empty.
    /// - [`QueryError::UnknownField`] if any column doesn't exist on
    ///   the model.
    /// - Anything [`QuerySet::compile`] surfaces.
    pub fn compile(self) -> Result<SelectQuery, QueryError> {
        compile_values_select(self.qs, self.cols)
    }

    /// The validated column list — exposed so the terminal fetch in
    /// [`crate::sql`] can pass it to the row decoder.
    #[must_use]
    pub fn columns(&self) -> &[&'static str] {
        &self.cols
    }
}

impl<T: Model> ValuesListQuerySet<T> {
    /// See [`ValuesQuerySet::compile`].
    ///
    /// # Errors
    /// As [`ValuesQuerySet::compile`].
    pub fn compile(self) -> Result<SelectQuery, QueryError> {
        compile_values_select(self.qs, self.cols)
    }

    /// The validated column list — exposed for the terminal fetch.
    #[must_use]
    pub fn columns(&self) -> &[&'static str] {
        &self.cols
    }
}

impl<T: Model> ValuesFlatQuerySet<T> {
    /// Compile to a [`SelectQuery`] with `projection` set to the
    /// single column.
    ///
    /// # Errors
    /// - [`QueryError::UnknownField`] if `col` doesn't exist on the
    ///   model.
    /// - Anything [`QuerySet::compile`] surfaces.
    pub fn compile(self) -> Result<SelectQuery, QueryError> {
        compile_values_select(self.qs, vec![self.col])
    }
}

// ====================================================================
// Django `.dates(field, kind)` — distinct truncated dates (issue #327)
// ====================================================================

/// Truncation granularity for [`QuerySet::dates`] / [`QuerySet::datetimes`].
/// Mirrors Django's `'year' | 'month' | 'day'` shape. Issue #327 / #328.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateKind {
    Year,
    Month,
    Day,
}

impl DateKind {
    /// Emit the dialect-portable SQL fragment that truncates the
    /// column to this granularity. `col_quoted` must already be a
    /// quoted identifier (`"name"` / `\`name\``).
    pub(crate) fn trunc_sql(self, dialect_name: &str, col_quoted: &str) -> String {
        match (dialect_name, self) {
            ("postgres", DateKind::Year) => format!("DATE_TRUNC('year', {col_quoted})::date"),
            ("postgres", DateKind::Month) => format!("DATE_TRUNC('month', {col_quoted})::date"),
            ("postgres", DateKind::Day) => format!("DATE({col_quoted})"),
            ("mysql", DateKind::Year) => {
                format!("DATE(DATE_FORMAT({col_quoted}, '%Y-01-01'))")
            }
            ("mysql", DateKind::Month) => {
                format!("DATE(DATE_FORMAT({col_quoted}, '%Y-%m-01'))")
            }
            ("mysql", DateKind::Day) => format!("DATE({col_quoted})"),
            ("sqlite", DateKind::Year) => {
                format!("date(strftime('%Y-01-01', {col_quoted}))")
            }
            ("sqlite", DateKind::Month) => {
                format!("date(strftime('%Y-%m-01', {col_quoted}))")
            }
            ("sqlite", DateKind::Day) => format!("date({col_quoted})"),
            // Unknown dialect — fall back to PG-shape DATE_TRUNC; the
            // driver will surface a clear syntax error if the dialect
            // doesn't support it.
            (_, DateKind::Year) => format!("DATE_TRUNC('year', {col_quoted})"),
            (_, DateKind::Month) => format!("DATE_TRUNC('month', {col_quoted})"),
            (_, DateKind::Day) => format!("DATE({col_quoted})"),
        }
    }
}

/// Builder returned by [`QuerySet::dates`]. The terminal
/// [`fetch`](Self::fetch) emits
/// `SELECT DISTINCT <trunc(col)> AS d FROM (<inner-query>) sub ORDER BY d`
/// — the wrap inherits the underlying queryset's WHERE / JOINs / LIMIT
/// / ORDER BY so filters on the QuerySet pass through to `.dates()`.
pub struct DatesQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) field: &'static str,
    pub(crate) kind: DateKind,
    pub(crate) descending: bool,
}

impl<T: Model> DatesQuerySet<T> {
    /// Reverse the ORDER BY direction. Default ascending (oldest
    /// first), matching Django.
    #[must_use]
    pub fn order_desc(mut self, desc: bool) -> Self {
        self.descending = desc;
        self
    }

    /// Validate the field + return the column name resolved on the
    /// model schema.
    pub fn resolve_column(&self) -> Result<&'static str, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let field = model
            .field(self.field)
            .ok_or_else(|| QueryError::UnknownField {
                model: model.name,
                field: self.field.to_owned(),
            })?;
        // Field must be a Date or DateTime — operators expect the
        // truncation to be meaningful. Other types (i64, String, etc.)
        // would silently produce garbage on some backends.
        if !matches!(
            field.ty,
            crate::core::FieldType::Date | crate::core::FieldType::DateTime
        ) {
            return Err(QueryError::TypeMismatch {
                model: model.name,
                field: self.field.to_owned(),
                // `.dates()` requires a Date or DateTime column; the
                // shape uses `expected: DateTime` as the canonical
                // representative since the truncation always produces
                // a Date regardless of input.
                expected: crate::core::FieldType::DateTime,
                actual: field.ty,
            });
        }
        Ok(field.column)
    }
}

impl<T: Model> QuerySet<T> {
    /// Django `.dates(field, kind)` — return the distinct date values
    /// of `field` truncated to `kind`. Issue #327.
    ///
    /// Output order is ascending by default (oldest first). Chain
    /// [`DatesQuerySet::order_desc(true)`] for newest-first.
    ///
    /// Filters / joins / limits set on the underlying queryset pass
    /// through to the truncation pipeline — `.filter(...).dates(...)`
    /// only considers matching rows.
    #[must_use]
    pub fn dates(self, field: &'static str, kind: DateKind) -> DatesQuerySet<T> {
        DatesQuerySet {
            qs: self,
            field,
            kind,
            descending: false,
        }
    }

    /// Django `.datetimes(field, kind)` — return the distinct datetime
    /// values of `field` truncated to `kind`. Issue #328.
    ///
    /// Supports finer granularity than [`Self::dates`]: in addition to
    /// `Year` / `Month` / `Day`, accepts `Hour` / `Minute` / `Second`.
    /// Returns `DateTime<Utc>` at the truncated instant.
    #[must_use]
    pub fn datetimes(self, field: &'static str, kind: DateTimeKind) -> DateTimesQuerySet<T> {
        DateTimesQuerySet {
            qs: self,
            field,
            kind,
            descending: false,
        }
    }
}

/// Truncation granularity for [`QuerySet::datetimes`]. Mirrors
/// Django's `'year' | 'month' | 'day' | 'hour' | 'minute' | 'second'`.
/// Issue #328.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeKind {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DateTimeKind {
    /// Dialect-portable SQL fragment that truncates a `TIMESTAMP` /
    /// `DATETIME` column to this granularity, returning a value
    /// shaped to decode as `DateTime<Utc>`.
    pub(crate) fn trunc_sql(self, dialect_name: &str, col_quoted: &str) -> String {
        match dialect_name {
            "postgres" => {
                let unit = match self {
                    DateTimeKind::Year => "year",
                    DateTimeKind::Month => "month",
                    DateTimeKind::Day => "day",
                    DateTimeKind::Hour => "hour",
                    DateTimeKind::Minute => "minute",
                    DateTimeKind::Second => "second",
                };
                format!("DATE_TRUNC('{unit}', {col_quoted})")
            }
            "mysql" => {
                let fmt = match self {
                    DateTimeKind::Year => "%Y-01-01 00:00:00",
                    DateTimeKind::Month => "%Y-%m-01 00:00:00",
                    DateTimeKind::Day => "%Y-%m-%d 00:00:00",
                    DateTimeKind::Hour => "%Y-%m-%d %H:00:00",
                    DateTimeKind::Minute => "%Y-%m-%d %H:%i:00",
                    DateTimeKind::Second => "%Y-%m-%d %H:%i:%s",
                };
                // CAST back to DATETIME so the decoder sees the right type.
                format!("CAST(DATE_FORMAT({col_quoted}, '{fmt}') AS DATETIME)")
            }
            _ => {
                // SQLite (and any other dialect): use strftime to
                // format, then re-parse via datetime() so the value
                // round-trips to the standard ISO-8601 shape.
                let fmt = match self {
                    DateTimeKind::Year => "%Y-01-01 00:00:00",
                    DateTimeKind::Month => "%Y-%m-01 00:00:00",
                    DateTimeKind::Day => "%Y-%m-%d 00:00:00",
                    DateTimeKind::Hour => "%Y-%m-%d %H:00:00",
                    DateTimeKind::Minute => "%Y-%m-%d %H:%M:00",
                    DateTimeKind::Second => "%Y-%m-%d %H:%M:%S",
                };
                format!("strftime('{fmt}', {col_quoted})")
            }
        }
    }
}

/// Builder returned by [`QuerySet::datetimes`]. Issue #328 — sibling
/// to [`DatesQuerySet`] with finer granularity + `DateTime<Utc>`
/// return type.
pub struct DateTimesQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) field: &'static str,
    pub(crate) kind: DateTimeKind,
    pub(crate) descending: bool,
}

impl<T: Model> DateTimesQuerySet<T> {
    #[must_use]
    pub fn order_desc(mut self, desc: bool) -> Self {
        self.descending = desc;
        self
    }

    pub fn resolve_column(&self) -> Result<&'static str, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let field = model
            .field(self.field)
            .ok_or_else(|| QueryError::UnknownField {
                model: model.name,
                field: self.field.to_owned(),
            })?;
        if !matches!(
            field.ty,
            crate::core::FieldType::Date | crate::core::FieldType::DateTime
        ) {
            return Err(QueryError::TypeMismatch {
                model: model.name,
                field: self.field.to_owned(),
                expected: crate::core::FieldType::DateTime,
                actual: field.ty,
            });
        }
        Ok(field.column)
    }
}

/// Shared compile path for the three values builders. Validates the
/// column list, then delegates to `QuerySet::compile` and stamps the
/// projection onto the resulting [`SelectQuery`].
fn compile_values_select<T: Model>(
    qs: QuerySet<T>,
    cols: Vec<&'static str>,
) -> Result<SelectQuery, QueryError> {
    if cols.is_empty() {
        return Err(QueryError::EmptyValuesProjection);
    }
    let model: &'static ModelSchema = T::SCHEMA;
    for col in &cols {
        if model.field_by_column(col).is_none() {
            return Err(QueryError::UnknownField {
                model: model.name,
                field: (*col).to_owned(),
            });
        }
    }
    let mut q = qs.compile()?;
    q.projection = Some(cols);
    Ok(q)
}
