//! Query layer for rustango.
//!
//! `QuerySet<T>` is a typed builder for `SELECT`. It joins filters with
//! `AND` and compiles to the dialect-neutral `SelectQuery` IR in
//! `rustango-core`. `UpdateBuilder<T>` does the same for `UPDATE`, and
//! `QuerySet<T>` is also the input to bulk delete.

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
/// Filters are kept in the order you add them. Nothing touches the schema
/// until `compile` is called, so the builder never panics on bad input.
///
/// You can mix two filter shapes freely:
/// * [`Self::filter`] / [`Self::eq`] — string keys, checked at `compile`
///   time.
/// * [`Self::where_`] — typed (`User::id.gt(10)`); the column is already
///   resolved, so `compile` skips the schema lookup.
///
/// `Clone` is written by hand so a half-built queryset can be the base for
/// two different branches. The derive would add a spurious `T: Clone`
/// bound, and `T` is only a compile-time schema reference.
pub struct QuerySet<T: Model> {
    pending: Vec<PendingFilter>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// DISTINCT mode. `None` emits no DISTINCT clause.
    /// See [`crate::core::DistinctMode`] for what each mode means.
    distinct: Option<crate::core::DistinctMode>,
    /// FK field names registered by [`Self::select_related`]. Each one
    /// becomes a `Join` against the FK target at `compile()` time, so
    /// parents and children come back in one round trip.
    select_related: Vec<String>,
    /// Joins added by [`Self::join`]. Stored pre-built, not as field
    /// names, because the predicate is arbitrary. Appended after the
    /// `select_related` joins.
    ad_hoc_joins: Vec<crate::core::Join>,
    /// Derived-table joins from [`Self::join_sub`] /
    /// [`Self::join_lateral`]. Copied straight onto the compiled
    /// [`SelectQuery::subquery_joins`].
    subquery_joins: Vec<crate::core::SubqueryJoin>,
    /// Pending `ORDER BY` list. Holds `Field` entries from
    /// `.order_by(...)` / `.order_by_with_nulls(...)` and `Expr` entries
    /// from `.order_by_expr(...)`. Lowered in registration order, so
    /// chain order survives mixed builder calls.
    order_by: Vec<PendingOrderItem>,
    /// Row-lock mode for `SELECT … FOR UPDATE`. `None` emits no lock
    /// clause.
    lock_mode: Option<crate::core::LockMode>,
    /// Set-algebra branches from `.union()` / `.intersection()` /
    /// `.difference()`. Each entry is a compiled
    /// [`crate::core::CompoundBranch`]. Empty emits a plain SELECT.
    compound: Vec<crate::core::CompoundBranch>,
    /// Head-branch `ORDER BY`, frozen at the first set-op call.
    /// `order_by` / `limit` / `offset` set BEFORE the first set-op belong
    /// to the first queryset only.
    /// Freezing them here lets anything chained after the set-op apply
    /// to the combined result. Empty until the first set-op call.
    head_order_by: Vec<PendingOrderItem>,
    /// Head-branch `LIMIT`, frozen at the first set-op call.
    /// See [`Self::head_order_by`].
    head_limit: Option<i64>,
    /// Head-branch `OFFSET`, frozen at the first set-op call.
    /// See [`Self::head_order_by`].
    head_offset: Option<i64>,
    /// `.none()` short-circuit. When `true`, every terminal op
    /// compiles to a statement that matches nothing: SELECT gets
    /// `LIMIT 0`; UPDATE / DELETE get an `IS NULL` test against the
    /// NOT NULL primary key. Later calls still accumulate filters and
    /// orderings; the queryset stays empty.
    is_none: bool,
    /// Scope names from `T::SCHEMA.global_scopes` to skip on this
    /// queryset (Eloquent `withoutGlobalScope($name)`). Empty keeps every
    /// scope active. Populated by [`Self::without_global_scope`], and
    /// ignored when [`Self::disable_all_global_scopes`] is set.
    disabled_global_scopes: Vec<&'static str>,
    /// When `true`, skip every scope on `T::SCHEMA.global_scopes`
    /// (Eloquent `withoutGlobalScopes()`). Set by
    /// [`Self::without_global_scopes`]. The
    /// [`Self::disabled_global_scopes`] list is then unused, but stays
    /// intact so adding a name afterwards is not surprising.
    disable_all_global_scopes: bool,
    _model: PhantomData<fn() -> T>,
}

/// One entry in the QuerySet's pending `ORDER BY` list. `Field` holds a
/// field name resolved against the schema at `compile()` time; `Expr`
/// holds an already-built expression. The list keeps registration order,
/// so mixed builder chains compose predictably.
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
    /// `ORDER BY RANDOM()` / `RAND()`. The writer picks the dialect
    /// token at emit time.
    Random,
}

/// Filter accumulator entry. Keeps insertion order across string-keyed and
/// typed filter calls. Each entry adds one node to the final
/// `WhereExpr::And` clause.
#[derive(Clone)]
enum PendingFilter {
    /// String-keyed; resolved against the schema at `compile` time.
    Raw(RawFilter),
    /// Date-part transform lookup, such as `created__year__gte`. At
    /// resolve time the column is wrapped in the named scalar fn
    /// (`ExtractYear`, `TruncDate`, …) and compared with `value` using
    /// `op`.
    DateTransform(DateTransformFilter),
    /// Already resolved by a typed [`Column`](crate::core::Column).
    Resolved(Filter),
    /// Typed sub-expression built with `.and()` / `.or()` on the
    /// typed-column API. Already validated; adds a whole sub-tree to the
    /// WHERE clause.
    Expr(WhereExpr),
    /// Negated predicate, `NOT (<inner>)`. Backs [`QuerySet::exclude`].
    /// The inner entry resolves first, then gets wrapped in
    /// [`WhereExpr::Not`], so `exclude` supports the full lookup grammar
    /// (`__gt`, `__icontains`, …).
    Negated(Box<PendingFilter>),
    /// Relation-spanning lookup deferred from [`parse_lookup`], such as
    /// `author__profile__bio__icontains`. A pre-pass in
    /// [`QuerySet::compile`] turns it into a [`PendingFilter::Expr`] and
    /// registers the FK-chain JOINs. If one still reaches
    /// [`resolve_one_pending`], the caller is on an update, delete or
    /// aggregate path, where it is not supported, so it errors.
    RelationSpan { raw_key: String, value: SqlValue },
    /// Error held back until `compile()`. [`QuerySet::filter`] uses it
    /// when the lookup-suffix parser fails, so the builder API stays
    /// non-`Result` and the cause still surfaces.
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

/// Staged `field = <expression>` assignment for the [`F()`] SET path.
/// `field` is the Rust-side name, resolved against the schema at
/// `compile()` time. `value` is an [`crate::core::Expr`] tree that may
/// hold column refs and arithmetic. Resolves to
/// [`crate::core::Assignment`].
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

    /// Eloquent `withoutGlobalScope($name)`. Skip one named entry from
    /// [`crate::core::ModelSchema::global_scopes`] on this queryset.
    /// Repeated calls add up.
    ///
    /// Unknown names are ignored, like Eloquent, so a call site can opt
    /// out of a scope that does not exist yet.
    #[must_use]
    pub fn without_global_scope(mut self, name: &'static str) -> Self {
        if !self.disabled_global_scopes.contains(&name) {
            self.disabled_global_scopes.push(name);
        }
        self
    }

    /// Eloquent `withoutGlobalScopes()`. Skip *every* scope from
    /// [`crate::core::ModelSchema::global_scopes`]. Useful for admin or
    /// migration code that must see soft-deleted, unpublished or
    /// cross-tenant rows the app normally hides.
    ///
    /// Once set, later [`Self::without_global_scope`] calls change
    /// nothing: everything stays disabled on this queryset.
    #[must_use]
    pub fn without_global_scopes(mut self) -> Self {
        self.disable_all_global_scopes = true;
        self
    }

    /// Fold the schema's global scopes into the pending filter list,
    /// honouring the per-queryset opt-outs. Called at the top of every
    /// `compile*` entry point, so scope expressions go through the same
    /// resolver as any other typed filter.
    ///
    /// Scope filters are **prepended** in declared order, so the WHERE
    /// reads `scope_a AND scope_b AND <user filters>`. `AND` is
    /// commutative, so this only affects how an `EXPLAIN` trace reads.
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

    /// A queryset that matches zero rows. Handy as a
    /// safe base for conditional filters, such as
    /// `if !user.has_perm { qs = qs.none() }`.
    ///
    /// SELECT compiles to `LIMIT 0`. UPDATE / DELETE add an `IS NULL`
    /// test against the NOT NULL primary key, so no row matches.
    #[must_use]
    pub fn none(mut self) -> Self {
        self.is_none = true;
        self
    }

    /// Emit `SELECT DISTINCT ...`. Behaves the
    /// same on every dialect. Add `.order_by(...)` to decide which row
    /// of a duplicate group survives; without it the order is undefined.
    ///
    /// For "first row per group", use [`Self::distinct_on`].
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = Some(crate::core::DistinctMode::All);
        self
    }

    /// Distinct on named columns. Emits `DISTINCT ON (col1, col2)` on
    /// Postgres. On MySQL / SQLite it lowers to a `ROW_NUMBER() OVER
    /// (PARTITION BY cols ORDER BY <order_by>)` subquery. All three give
    /// "first row per group", and the queryset's `ORDER BY` decides
    /// which row is first.
    ///
    /// The columns must come first in `.order_by(...)`.
    /// Otherwise `.compile()` returns
    /// [`QueryError::DistinctOnOrderByMismatch`]. Empty `fields`
    /// returns [`QueryError::DistinctOnEmpty`]; use
    /// [`Self::distinct`] for that instead.
    #[must_use]
    pub fn distinct_on(mut self, fields: &[&'static str]) -> Self {
        self.distinct = Some(crate::core::DistinctMode::On(fields.to_vec()));
        self
    }

    /// Take a row lock. Emits `SELECT … FOR UPDATE` on
    /// Postgres and MySQL 8+. SQLite has no row-lock syntax, so it is a
    /// no-op there; a SQLite transaction locks the whole database.
    ///
    /// Run it inside a transaction. `FOR UPDATE` outside one does
    /// nothing on Postgres and errors on MySQL. Open one with
    /// [`crate::sql::transaction_pool`], then call
    /// `.fetch_on(&mut *tx)`.
    ///
    /// The defaults ([`crate::core::LockMode::default`]) emit plain
    /// `FOR UPDATE`. Use the methods below to set individual flags.
    #[must_use]
    pub fn select_for_update(mut self) -> Self {
        self.lock_mode = Some(crate::core::LockMode::default());
        self
    }

    /// Eloquent `Builder::lockForUpdate()`. Alias of
    /// [`Self::select_for_update`], with the same rules.
    #[must_use]
    pub fn lock_for_update(self) -> Self {
        self.select_for_update()
    }

    /// PG / MySQL 8+: add `SKIP LOCKED` — the "claim the next free row"
    /// pattern. Rows another transaction holds are skipped instead of
    /// blocking. No effect on SQLite. Implies
    /// [`Self::select_for_update`].
    #[must_use]
    pub fn skip_locked(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.skip_locked = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG / MySQL 8+: add `NOWAIT` — return a driver error at once if
    /// any matching row is locked. The database forbids it together
    /// with `SKIP LOCKED`, so if both are set the writer emits
    /// `SKIP LOCKED`. Implies [`Self::select_for_update`].
    #[must_use]
    pub fn nowait(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.nowait = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG 9.3+: `FOR NO KEY UPDATE` instead of `FOR UPDATE`. A weaker
    /// lock that does not block writers leaving the row's PK and unique
    /// columns alone. Good for busy update paths that never change
    /// indexed columns. MySQL has no equivalent, so the writer falls
    /// back to `FOR UPDATE`.
    #[must_use]
    pub fn no_key(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.no_key = true;
        self.lock_mode = Some(lock);
        self
    }

    /// PG 9.3+ / MySQL 8.0.1+: `FOR UPDATE OF table1, table2, …` — lock
    /// only the named tables when the query joins. Without `OF` the
    /// lock covers every joined table the result touches. No-op on
    /// SQLite.
    ///
    /// Each call appends. Pass base table names or join aliases.
    #[must_use]
    pub fn of(mut self, tables: &[&'static str]) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.of.extend_from_slice(tables);
        self.lock_mode = Some(lock);
        self
    }

    /// Stop the writer from warning that SQLite has no row locks.
    ///
    /// SQLite has no `FOR UPDATE` syntax, so the lock modifiers are
    /// dropped. The writer logs a `tracing::warn!` by default, so nobody
    /// chases a phantom concurrency bug on a SQLite fixture. Call this
    /// when SQLite's single global write lock is enough for you.
    ///
    /// No effect on PG / MySQL; locks emit normally there.
    #[must_use]
    pub fn silent_on_sqlite(mut self) -> Self {
        let mut lock = self.lock_mode.take().unwrap_or_default();
        lock.silent_on_sqlite = true;
        self.lock_mode = Some(lock);
        self
    }

    /// Combines this queryset with `other`
    /// using SQL `UNION`, which removes duplicates. Both target model
    /// `T`, so the column shape always matches.
    ///
    /// Every `.union()` / `.union_all()` call appends another branch.
    /// Mixing in `intersection` or `difference` is allowed; SQL
    /// evaluates the chain left to right.
    ///
    /// `.order_by()` / `.limit()` / `.offset()` called AFTER `.union()`
    /// apply to the combined result. The same calls BEFORE it apply to
    /// the first queryset only. Each
    /// branch keeps its own clauses, because every component is wrapped
    /// in an aliased derived table (`SELECT * FROM (…) AS
    /// __rustango_bN`) on all three backends.
    ///
    /// Works on every supported backend.
    ///
    /// # Panics
    /// If `other.compile()` fails, for example on a typo'd column in
    /// the branch. A malformed branch is a programmer error. To get a
    /// `Result` instead, pre-compile the branch and pass it to
    /// [`Self::with_compound`].
    #[must_use]
    pub fn union(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Union, other)
    }

    /// `UNION ALL` — combine without removing duplicates. Cheaper than
    /// [`Self::union`] because the database skips the DISTINCT pass.
    /// Use it when you know the branches do not overlap.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn union_all(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::UnionAll, other)
    }

    /// Rows present in BOTH
    /// querysets. Postgres, SQLite and MySQL 8.0.31+.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn intersection(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Intersection, other)
    }

    /// Emits `EXCEPT`: rows in this
    /// queryset but not in `other`. Postgres, SQLite and MySQL 8.0.31+.
    ///
    /// # Panics
    /// As [`Self::union`].
    #[must_use]
    pub fn difference(self, other: QuerySet<T>) -> Self {
        self.add_compound(crate::core::SetOp::Difference, other)
    }

    /// Set-algebra entry point that takes an already-compiled
    /// `SelectQuery` branch. Use it when building the branch may fail
    /// and you want to handle that error yourself. The operator
    /// argument covers union, union_all, intersection and difference.
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

    /// Shared lowering for [`Self::union`], [`Self::union_all`],
    /// [`Self::intersection`] and [`Self::difference`]. Compiles the
    /// branch and appends a `CompoundBranch`. Panics if the branch
    /// fails to compile; use [`Self::with_compound`] for the fallible
    /// form.
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
        // The first set-op call freezes the pre-union ORDER BY / LIMIT /
        // OFFSET into the head slots: those clauses belong to the first
        // queryset. Clearing the live slots means anything chained after
        // this point applies to the combined result. Later set-op calls
        // leave the frozen head alone.
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

    /// Append `ORDER BY` columns.
    ///
    /// Each entry is a `(field_name, desc)` pair naming a Rust-side
    /// field on the model. The schema check runs at `compile()` time.
    /// Calls compose: later ones append after earlier ones.
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
    /// accumulated `ORDER BY` list instead of appending to it like
    /// [`Self::order_by`]. Use it to override a default ordering that
    /// came from a base queryset or manager:
    ///
    /// ```ignore
    /// let qs = Post::objects()
    ///     .with_default_order()                          // model's default
    ///     .reorder(&[("created_at", true)]);             // wipes default
    /// ```
    ///
    /// Pass an empty slice, `.reorder(&[])`, to clear every sort key,
    /// like Eloquent's `reorder()` with no arguments.
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

    /// Shortcut for `ORDER BY <col> DESC`. Appends to the existing
    /// list; call [`Self::reorder`] first to replace it.
    ///
    /// Eloquent calls this `Builder::latest($column)`, but rustango's
    /// [`Self::latest`] is the async fetcher, so the chainable form
    /// uses this name.
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

    /// Apply the schema's `default_order` for `T`. Each entry from
    /// `#[rustango(default_order = "...")]` is prepended to the pending
    /// ORDER BY list, so a later `.order_by(...)` adds secondary keys.
    ///
    /// **You must opt in per query.** A model's default order is not
    /// applied on its own; every queryset starts unsorted. This keeps
    /// `.count()`, `.exists()` and
    /// `.delete()` from paying for a sort they do not need.
    ///
    /// A no-op when the model has no `default_order`, and calling it
    /// twice does not duplicate the entries.
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

    /// Clear every accumulated `ORDER BY` entry, whatever added it
    /// (`.order_by(...)`, `.order_by_expr(...)`,
    /// `.with_default_order()`). Use it to drop the default order of a
    /// queryset you got from a builder helper.
    #[must_use]
    pub fn unordered(mut self) -> Self {
        self.order_by.clear();
        self
    }

    /// Append `ORDER BY` columns and say where NULLs go. PG and SQLite
    /// emit the `NULLS …` keyword. MySQL has no such syntax, so it
    /// sorts by `<col> IS NULL` first instead.
    ///
    /// ```ignore
    /// use rustango::core::NullsOrder;
    /// Post::objects()
    ///     .order_by_with_nulls(&[("published_at", true, NullsOrder::Last)])
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// Composes with `.order_by(...)`: entries from both methods appear
    /// in the `ORDER BY` clause in **registration order**.
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

    /// Append an `ORDER BY` item over any [`Expr`], such as
    /// `lower(F("title"))`, `case(...)` or `F("a") + F("b")`.
    ///
    /// `desc = true` gives `DESC`. NULL placement follows the dialect
    /// default; use [`Self::order_by_expr_with_nulls`] to pin it.
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

    /// Order rows by pgvector distance from `query` to the `column`
    /// vector. Distance ascends, so the **nearest** rows come first for
    /// every metric.
    ///
    /// Emits `ORDER BY <column> <op> $vec`, where `<op>` is the pgvector
    /// operator for `metric` (`<->`, `<=>`, `<#>`). **PG only**: the
    /// writer raises `OpNotSupportedInDialect` on MySQL / SQLite. Add
    /// `.limit(k)`, or use [`Self::k_nearest`], for a k-NN query. The
    /// column should be a [`crate::sql::Vector`] field.
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

    /// k-nearest-neighbour shortcut: [`Self::order_by_distance`] plus
    /// `.limit(k)`. Returns the `k` rows closest to `query` under
    /// `metric`. **PG only.**
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

    /// PostGIS nearest-first ordering: `ORDER BY ST_Distance(<column>,
    /// <point>)` ascending. Add `.limit(k)` for a k-nearest query.
    /// `column` should be a `#[rustango(geometry(...))]` field.
    /// **PG / PostGIS only**: MySQL and SQLite raise
    /// `OpNotSupportedInDialect` at compile time.
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

    /// PostGIS "within radius" filter: `WHERE ST_DWithin(<column>,
    /// <point>, <distance>)`. `distance` uses the column's SRID units,
    /// which is degrees for SRID 4326. For metres, cast to
    /// `::geography` in raw SQL. **PG / PostGIS only.**
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

    /// Same as [`Self::order_by_expr`], but with an explicit
    /// `NullsOrder`.
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

    /// `ORDER BY RANDOM()` (PG / SQLite) or `ORDER BY RAND()` (MySQL).
    /// Good for "random N rows" needs
    /// such as banner rotation or sampling.
    ///
    /// ```ignore
    /// // Three random posts.
    /// Post::objects()
    ///     .order_random()
    ///     .limit(3)
    ///     .fetch(&pool).await?;
    /// ```
    ///
    /// **Cost**: this forces a full table scan and an in-memory sort on
    /// a random key per row. No index can help. On tables much larger
    /// than memory, use `WHERE pk >= <random_offset> LIMIT N` instead,
    /// which can range-scan an index, and accept that the rows come out
    /// next to each other by primary key.
    ///
    /// It composes with other `.order_by*` calls: the random key only
    /// breaks ties. Most callers want [`Self::replace_order_by`] first.
    #[must_use]
    pub fn order_random(mut self) -> Self {
        self.order_by.push(PendingOrderItem::Random);
        self
    }

    /// Eloquent `Builder::inRandomOrder()`. Alias of
    /// [`Self::order_random`], with the same cost.
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

    /// Drop every pending `ORDER BY` item and use `items` instead.
    /// Used by `earliest` and `latest`, which set their own sort.
    #[must_use]
    pub fn replace_order_by(mut self, items: &[(&str, bool)]) -> Self {
        self.order_by.clear();
        self.order_by(items)
    }

    /// Flips the direction of every pending `ORDER BY` entry.
    /// `Random` has no direction, so it stays as it is.
    ///
    /// No-op when no ordering is set.
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

    /// Flip every ordering direction in place. `last` uses it to take
    /// the first row of the reversed sort, which avoids `OFFSET` plus
    /// `COUNT(*)` and works on every dialect. It also swaps
    /// `NullsOrder::First` and `NullsOrder::Last`, so NULLs stay at the
    /// same logical end.
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
                // Random has no direction or NULLS clause to flip.
                PendingOrderItem::Random => {}
            }
        }
        self
    }

    /// Whether any `ORDER BY` item is registered. The executor's
    /// `ensure_pk_ordering` uses it to spot "no ordering set" without
    /// looking at the entry variants.
    #[must_use]
    pub fn has_order_by(&self) -> bool {
        !self.order_by.is_empty()
    }

    /// Load a `ForeignKey<Parent>` field with a `LEFT JOIN`, in the
    /// same query. Pass the field name on `T`, not the
    /// FK column or the parent table. `fetch_on` then returns rows
    /// whose `ForeignKey<Parent>` is already `Loaded`, from a single
    /// query, with no N+1.
    ///
    /// ```ignore
    /// let posts: Vec<Post> = Post::objects()
    ///     .select_related("author")
    ///     .fetch_on(conn).await?;
    /// // post.author is ForeignKey::Loaded { pk, value }
    /// ```
    ///
    /// Calls compose: each adds another `LEFT JOIN` to the same SELECT.
    /// The schema check — field exists, is an FK, target has a primary
    /// key — runs at `compile()` time.
    #[must_use]
    pub fn select_related(mut self, field: impl Into<String>) -> Self {
        self.select_related.push(field.into());
        self
    }

    /// Append a fully specified [`crate::core::Join`]. Unlike
    /// [`select_related`], which builds joins from FK metadata, you
    /// choose the join kind, alias, predicate and projected columns.
    /// The predicate is any [`WhereExpr`]; its columns qualify against
    /// the joined alias by default, or against another alias via
    /// [`crate::core::Expr::AliasedColumn`].
    ///
    /// Calls compose; each JOIN lands after the `select_related` ones.
    /// Aliases must be unique within the queryset.
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

    /// `INNER JOIN (<subquery>) AS <alias> ON <on>` — join a **derived
    /// table**, like Eloquent's `joinSub`.
    ///
    /// `sub` is a compiled `SelectQuery`. In `on`, name the derived
    /// table's columns and the outer table's columns with
    /// [`crate::core::joins::aliased`]. The derived table adds no
    /// columns to the SELECT, so a typed fetch still returns the base
    /// model. Works on PG, MySQL and SQLite.
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
    pub fn join_sub(
        self,
        sub: impl Into<crate::core::DerivedSource>,
        alias: &'static str,
        on: WhereExpr,
    ) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Inner,
            on,
            /*lateral=*/ false,
        )
    }

    /// `LEFT JOIN (<subquery>) AS <alias> ON <on>` — the left-join form
    /// of [`Self::join_sub`] (Eloquent `leftJoinSub`).
    #[must_use]
    pub fn left_join_sub(
        self,
        sub: impl Into<crate::core::DerivedSource>,
        alias: &'static str,
        on: WhereExpr,
    ) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Left,
            on,
            /*lateral=*/ false,
        )
    }

    /// `INNER JOIN LATERAL (<subquery>) AS <alias> ON <on>` — Eloquent
    /// `joinLateral`. The subquery may read columns from the outer
    /// query, which is how you get "top-N rows per group". Put that
    /// correlation in the subquery's own `WHERE`, using
    /// [`crate::core::joins::aliased`] against the outer table, and
    /// pass `WhereExpr::And(vec![])` as `on` to emit `ON true`.
    ///
    /// **PostgreSQL and MySQL 8.0.14+ only.** SQLite has no `LATERAL`,
    /// so the writer raises
    /// [`crate::sql::SqlError::LateralJoinNotSupported`].
    #[must_use]
    pub fn join_lateral(
        self,
        sub: impl Into<crate::core::DerivedSource>,
        alias: &'static str,
        on: WhereExpr,
    ) -> Self {
        self.push_subquery_join(
            sub,
            alias,
            crate::core::JoinKind::Inner,
            on,
            /*lateral=*/ true,
        )
    }

    /// `LEFT JOIN LATERAL (<subquery>) AS <alias> ON <on>` — the
    /// left-join form of [`Self::join_lateral`] (Eloquent
    /// `leftJoinLateral`). Keeps every outer row even when the lateral
    /// subquery returns none, as in "latest order per customer,
    /// including customers with no orders". **PG / MySQL only.**
    #[must_use]
    pub fn left_join_lateral(
        self,
        sub: impl Into<crate::core::DerivedSource>,
        alias: &'static str,
        on: WhereExpr,
    ) -> Self {
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
        sub: impl Into<crate::core::DerivedSource>,
        alias: &'static str,
        kind: crate::core::JoinKind,
        on: WhereExpr,
        lateral: bool,
    ) -> Self {
        self.subquery_joins.push(crate::core::SubqueryJoin {
            subquery: sub.into(),
            alias,
            kind,
            on,
            lateral,
        });
        self
    }

    /// Cap the number of returned rows. A later call replaces the cap.
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
    #[must_use]
    pub fn skip(self, n: i64) -> Self {
        self.offset(n)
    }

    /// Append a `WHERE field <op> value` predicate with an explicit
    /// `Op`. Use it when you already know the operator; for the
    /// `filter("field__lookup", value)` shape see [`Self::filter`].
    ///
    /// `field` is the Rust-side field name. The column is looked up in
    /// the schema at compile time.
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

    /// Append a `WHERE` predicate. Parses a `"field"` or
    /// `"field__lookup"` key and picks the matching `Op`.
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
    /// | `__year` / `__month` / `__day` / `__hour` / `__minute` / `__second` / `__quarter` / `__week` / `__week_day` | `EXTRACT(<part> FROM <col>) = ?` | scalar value matches the date part. Composes with a trailing `__gte` / `__lt` / etc. `__quarter` works on every dialect; SQLite derives it from the month. `__week` numbering differs per backend. |
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
    /// `compile()`. A wrong value shape, such as `__in` with a non-list
    /// value, surfaces as [`QueryError::InvalidLookupValue`].
    ///
    /// Relation-spanning keys such as `author__name__icontains` work on
    /// the SELECT path: `compile()` adds the FK-chain JOINs for you.
    /// They are not supported on update, delete or aggregate paths,
    /// which error instead.
    #[must_use]
    pub fn filter(mut self, key: &str, value: impl Into<SqlValue>) -> Self {
        self.pending.push(parse_to_pending(key, value.into()));
        self
    }

    /// The negation of [`Self::filter`]. Each call wraps **its own**
    /// predicate in `NOT (…)`, and chained excludes AND together. The
    /// full `__lookup` grammar works here too.
    ///
    /// ```ignore
    /// Post::objects().exclude("status", "draft").fetch(&pool).await?;
    /// // with a lookup suffix:
    /// Post::objects().exclude("views__lt", 100_i64)
    /// ```
    ///
    /// **NULLs:** `NOT (col = v)` also drops rows where `col IS NULL`,
    /// because of SQL three-valued logic. To put one `NOT` over a
    /// whole group of conditions, use `where_raw(!Q(...))`.
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
        // `compile()`'s `resolve_pending` walk surfaces the error.
        self.pending.push(PendingFilter::Error(e));
        self
    }

    /// Shorthand for `filter_op(field, Op::Eq, value)`.
    #[must_use]
    pub fn eq(self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.filter_op(field, Op::Eq, value)
    }

    /// Eloquent `Builder::whereKey($pk)`. Filter on the model's
    /// primary key without naming the column; it comes from
    /// `T::SCHEMA.primary_key()`.
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

    /// Eloquent `Builder::whereKeyNot($pk)`. The opposite of
    /// [`Self::where_key`]: matches every row whose PK is not `pk`.
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

    /// Eloquent `Builder::whereExists($closure)`. ANDs an
    /// `EXISTS (subquery)` predicate onto this queryset's WHERE.
    ///
    /// `subquery` is an already-compiled [`SelectQuery`]. Build it from
    /// the inner model's `objects()` chain and call `.compile()`, so a
    /// column typo surfaces there. Use
    /// [`crate::core::subquery::outer_ref`] to refer to columns of the
    /// outer model.
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

    /// Eloquent `Builder::whereNotExists($closure)`. ANDs a
    /// `NOT EXISTS (subquery)` predicate. The inverse of
    /// [`Self::where_exists`], and the usual way to find rows in A with
    /// no related row in B.
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

    /// Eloquent `Builder::whereIn($column, $closure)` with a subquery
    /// instead of a value list. Emits `<column> IN (subquery)`.
    ///
    /// `subquery` is an already-compiled [`SelectQuery`] that should
    /// project one column, matched against `column` on the outer model.
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

    /// Eloquent `Builder::whereNotIn($column, $closure)`. The inverse
    /// of [`Self::where_in_subquery`]; emits
    /// `<column> NOT IN (subquery)`.
    #[must_use]
    pub fn where_not_in_subquery(self, column: &'static str, subquery: SelectQuery) -> Self {
        self.where_raw(crate::core::subquery::not_in_subquery(column, subquery))
    }

    /// Keep rows that have at least one related row through the
    /// relation named `name`.
    ///
    /// `name` is looked up in three relation kinds, in this order, so
    /// the same call works however the relation is declared on `T`:
    /// - **reverse-FK** — `EXISTS (SELECT 1 FROM <child> WHERE
    ///   <child_fk> = <outer>.<pk>)`.
    /// - **many-to-many** — the same `EXISTS` over the junction table.
    /// - **generic-FK** — the same `EXISTS` over the polymorphic child,
    ///   AND-ed with a content-type check so only children pointing at
    ///   *this* model match.
    ///
    /// All three emit the same way on PG, MySQL and SQLite. An unknown
    /// relation name surfaces as [`QueryError::UnknownField`] at
    /// `compile()` time.
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

    /// Turn a relation `name` (reverse-FK, many-to-many or generic-FK)
    /// into a correlated `[NOT ]EXISTS` predicate. Returns `None` when
    /// no relation matches, so callers can raise
    /// [`QueryError::UnknownField`]. Lookup order is reverse-FK, M2M,
    /// then GFK; names should be unique across the three.
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

    /// Turn a relation `name` into a correlated scalar `COUNT` [`Expr`],
    /// the left side of a `has($rel, op, n)` comparison. Covers
    /// reverse-FK, M2M and GFK. `None` if the name is unknown.
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

    /// The opposite of [`Self::where_has`]: keep rows that have **no**
    /// related row. Emits `NOT EXISTS (subquery)`. Same relation lookup
    /// and same errors.
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

    /// Keep rows with at least one related row through the named
    /// reverse-FK relation **and** matching your inner predicate.
    /// Eloquent `Builder::whereHas($rel, fn ($q) => …)`.
    ///
    /// `inner` is a compiled `SelectQuery` on the child model, usually
    /// `Child::objects().filter(...).compile()?`. Its `WHERE` is ANDed
    /// with the correlation `<child_fk_column> = <outer>.<self_pk>`,
    /// and the result is wrapped in `EXISTS (…)`.
    ///
    /// `inner.model` must be the relation's child model. A mismatch
    /// surfaces as [`QueryError::UnknownField`] at compile time.
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

    /// `whereDoesntHave($rel, fn ($q) => …)`, the counterpart of
    /// [`Self::where_has_filter`]. Keeps rows with **no** related row
    /// matching the inner predicate, using `NOT EXISTS (…)`.
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
        // Guard against an inner queryset built on the wrong child
        // model, which would silently produce wrong SQL. `Model::SCHEMA`
        // is a `const`, so each use site may get its own address and
        // pointer equality is unreliable. Compare by model name, which
        // is unique within the crate.
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
        // AND the correlation onto whatever is already on the inner
        // SelectQuery. Flatten a nested `And` to keep the tree shallow.
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

    /// Keep rows whose **count** of related rows satisfies
    /// `<count> <op> n`.
    ///
    /// Emits a correlated aggregate subquery in the WHERE clause:
    /// `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
    /// <outer>.<self_pk>) <op> n`. It runs the same on PG, MySQL and
    /// SQLite.
    ///
    /// `op` must be a comparison: [`Op::Eq`], [`Op::Ne`], [`Op::Lt`],
    /// [`Op::Lte`], [`Op::Gt`] or [`Op::Gte`]. Anything else errors at
    /// compile time. For "has at least one" or "has none", prefer
    /// [`Self::where_has`] / [`Self::where_doesnt_have`], which emit a
    /// cheaper `EXISTS`.
    ///
    /// An unknown relation name surfaces as
    /// [`QueryError::UnknownField`], as in [`Self::where_has`].
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

    /// Add the **count** of related rows as a `<name>_count` column.
    ///
    /// This turns the queryset into an [`AggregateBuilder`], whose rows
    /// are `Vec<HashMap<String, SqlValue>>`, like every other
    /// [`QuerySet::annotate`] path: a typed `T` has no field for the
    /// derived column. Each row carries every scalar column of `T` plus
    /// the `<name>_count` key.
    ///
    /// The count comes from a correlated subquery, not a `JOIN`, so it
    /// never double-counts and causes no N+1.
    ///
    /// An unknown relation name surfaces as
    /// [`QueryError::UnknownField`] at `compile()` time.
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

    /// Add `SUM(<column>)` over related rows as `<name>_sum_<column>`.
    /// Eloquent `withSum('orders', 'total')`. `column` belongs to the
    /// **child** model and is checked at `compile()` time. See
    /// [`Self::annotate_count`] for the return shape.
    #[must_use]
    pub fn annotate_sum(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Sum(column), "sum")
    }

    /// Add `AVG(<column>)` over related rows as `<name>_avg_<column>`.
    /// Eloquent `withAvg('orders', 'total')`. See
    /// [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_avg(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Avg(column), "avg")
    }

    /// Add `MAX(<column>)` over related rows as `<name>_max_<column>`.
    /// Eloquent `withMax('orders', 'total')`. See
    /// [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_max(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Max(column), "max")
    }

    /// Add `MIN(<column>)` over related rows as `<name>_min_<column>`.
    /// Eloquent `withMin('orders', 'total')`. See
    /// [`Self::annotate_sum`].
    #[must_use]
    pub fn annotate_min(self, name: &str, column: &'static str) -> AggregateBuilder<T> {
        self.annotate_relation_aggregate(name, Some(column), AggregateExpr::Min(column), "min")
    }

    /// Add a `<name>_exists` column saying whether **any** related row
    /// exists.
    ///
    /// Emits `CASE WHEN EXISTS (…) THEN 1 ELSE 0 END`, which is cheaper
    /// than [`Self::annotate_count`] when you only need presence,
    /// because `EXISTS` stops at the first matching child row. The
    /// column decodes as `SqlValue::I64(1)` or `I64(0)` on all three
    /// backends; integer literals keep the value the same everywhere.
    /// See [`Self::annotate_count`] for the return shape.
    ///
    /// An unknown relation name surfaces as
    /// [`QueryError::UnknownField`] at `compile()` time.
    #[must_use]
    pub fn annotate_exists(self, name: &str) -> AggregateBuilder<T> {
        let mut builder = self.aggregate();
        builder.push_relation_exists(name);
        builder
    }

    /// Shared worker for the `annotate_{count,sum,avg,max,min}` family.
    /// Resolves the reverse relation, names the projected column
    /// (`<name>_count` or `<name>_<suffix>_<column>`), wraps the
    /// correlated subquery in an [`AggregateExpr::RelatedAggregate`],
    /// and stages it on a fresh [`AggregateBuilder`].
    ///
    /// Errors are held on the builder and surface from `compile()`: an
    /// unknown relation name gives [`QueryError::UnknownField`] on `T`,
    /// and a `column` that is not a child field gives the same error
    /// keyed on the child schema.
    fn annotate_relation_aggregate(
        self,
        name: &str,
        column: Option<&'static str>,
        agg: AggregateExpr,
        suffix: &str,
    ) -> AggregateBuilder<T> {
        // The resolve-and-push worker lives on `AggregateBuilder` so a
        // query can chain more than one relation aggregate. Here we
        // just open a builder and delegate.
        let mut builder = self.aggregate();
        builder.push_relation_aggregate(name, column, agg, suffix);
        builder
    }

    /// Eloquent `Builder::whereColumn($col1, $col2)`. Emits
    /// `<col1> = <col2>`, comparing two columns instead of a column
    /// and a literal. For any other operator, see
    /// [`Self::where_column_op`].
    ///
    /// Both column names are checked against the schema at
    /// `compile()` time, through
    /// [`WhereExpr::ExprCompare`](crate::core::WhereExpr::ExprCompare).
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

    /// Eloquent `Builder::whereColumn($col1, $op, $col2)`. Compares
    /// two columns with an explicit operator. Use the shorter
    /// [`Self::where_column`] when the operator is `Eq`.
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

    /// AND a raw [`WhereExpr`] into the accumulated WHERE clause. Use
    /// it when the model has no derived typed columns, so
    /// [`Self::where_`] is unavailable, and you need OR / NOT / nested
    /// predicates that the string-keyed [`Self::filter`] cannot reach.
    ///
    /// The expression is checked against `T::SCHEMA` at `compile()`
    /// time. An unknown column or a wrong-typed value returns
    /// [`QueryError::UnknownField`] or [`QueryError::TypeMismatch`],
    /// as on the typed paths.
    #[must_use]
    pub fn where_raw(mut self, expr: WhereExpr) -> Self {
        self.pending.push(PendingFilter::Expr(expr));
        self
    }

    /// Eloquent `Builder::when($condition, $callback)`. Applies `f` to
    /// `self` only when `condition` is `true`, and otherwise returns
    /// `self` unchanged. It keeps a conditional filter inside the
    /// chain:
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
    /// The closure form keeps the chain readable when the condition is
    /// an inline expression rather than a plain `bool`.
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

    /// Eloquent `Builder::unless($condition, $callback)`. The inverse
    /// of [`Self::when`]: applies `f` only when `condition` is `false`.
    /// Use it when the guard reads better stated negatively, such as
    /// `qs.unless(is_admin, |q| q.filter("public", true))`.
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

    /// Eloquent `Builder::tap($callback)`. Runs a callback against the
    /// queryset and returns it unchanged, so you can add logging,
    /// metrics or a debug print without breaking the chain:
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

    /// Keep only rows that are NOT soft-deleted.
    ///
    /// When `T` has `#[rustango(soft_delete)]` on a nullable
    /// `DateTime<Utc>` column, this ANDs `<col> IS NULL` into the WHERE
    /// clause. Models without a soft-delete column get a no-op, so
    /// generic code can call `.active()` on any model. See
    /// [`crate::soft_delete::active_filter`].
    #[must_use]
    pub fn active(self) -> Self {
        match crate::soft_delete::active_filter(T::SCHEMA) {
            Some(expr) => self.where_raw(expr),
            None => self,
        }
    }

    /// Keep ONLY soft-deleted rows — the mirror of [`Self::active`],
    /// for trash pages and restore flows. A no-op when the model has
    /// no soft-delete column. Eloquent `onlyTrashed`.
    #[must_use]
    pub fn only_trashed(self) -> Self {
        match crate::soft_delete::trashed_filter(T::SCHEMA) {
            Some(expr) => self.where_raw(expr),
            None => self,
        }
    }

    /// Eloquent `withTrashed`. A no-op marker that states intent: a
    /// `QuerySet` already includes trashed rows unless you call
    /// [`Self::active`] or the model declares a global scope that
    /// filters them out.
    #[must_use]
    pub fn with_trashed(self) -> Self {
        self
    }

    /// Append a typed predicate or boolean expression built with the
    /// [`Column`](crate::core::Column) API. Takes a single
    /// [`TypedFilter`](crate::core::TypedFilter) (`User::id.gt(10)`) or
    /// a composed [`TypedExpr`] (`User::id.eq(1).or(User::id.eq(2))`).
    /// Every call ANDs its argument into the WHERE clause.
    #[must_use]
    pub fn where_<E: Into<TypedExpr<T>>>(mut self, predicate: E) -> Self {
        let expr = predicate.into().into_expr();
        // Put a bare predicate in the `Resolved` slot so a simple chain
        // stays a flat AND-of-predicates, which `as_flat_and()` needs.
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
        // Fold the schema's global scopes into the pending list, minus
        // any opt-outs, before the resolver sees it. After this the
        // resolver cannot tell a scope filter from a user filter.
        self.apply_global_scopes();
        let model: &'static ModelSchema = T::SCHEMA;
        // Lower relation-spanning lookups (`author__name`) into aliased
        // predicates plus their FK-chain JOINs before resolving the
        // WHERE. Those JOINs merge into the select_related set below.
        let (pending, span_joins) = lower_relation_spans(model, self.pending)?;
        let where_clause = resolve_pending(model, pending)?;
        let mut joins = lower_select_related(model, &self.select_related)?;
        // Ad-hoc joins come after the FK-driven select_related ones, so
        // column order stays stable and user joins sit next to the user
        // WHERE.
        joins.extend(self.ad_hoc_joins);
        // Skip a span JOIN whose alias is already here, so a span over
        // the same path as a select_related emits only once.
        for j in span_joins {
            if !joins.iter().any(|e| e.alias == j.alias) {
                joins.push(j);
            }
        }
        // Split head-branch clauses from combined-result clauses. With
        // a compound, `head_*` holds what was frozen at the first
        // set-op call and the live slots apply to the merged result.
        // Without one, the live slots are the query's own.
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
        // Lower the pending list to OrderItems, keeping insertion order
        // across all the `.order_by*` variants so a mixed chain emits
        // as written. `order_by` is the head (or only) ordering; the
        // combined-result ordering lowers on the next line.
        let (order_by, order_joins) = lower_order_items(model, head_pending)?;
        let (compound_order_by, comb_order_joins) = lower_order_items(model, comb_pending)?;
        // Merge relation-span `order_by` JOINs into the join set,
        // skipping aliases already added above.
        for j in order_joins.into_iter().chain(comb_order_joins) {
            if !joins.iter().any(|e| e.alias == j.alias) {
                joins.push(j);
            }
        }
        // `.distinct_on(&[...])` needs its columns at the head of the
        // ORDER BY, so catch a mismatch at compile time.
        // An empty list is rejected: it would just mean `.distinct()`.
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
            // The first `cols.len()` order_by items must be those
            // columns, in order. Only bare `OrderItem::Column` counts;
            // an expression or random cannot be a DISTINCT ON key.
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
        // `.none()` forces an empty result with `LIMIT 0`. It is cheap
        // on every dialect, and count / exists / first read the same
        // SelectQuery, so they need no special case. The `LIMIT 0` goes
        // on the outermost slot: the combined result when a compound is
        // present, otherwise the single query.
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

    /// Project to `Vec<HashMap<String, SqlValue>>` — named columns
    /// only, no typed model. Returns a [`ValuesQuerySet`] whose
    /// `.fetch(&pool)` decodes rows into a `HashMap` keyed by column
    /// name.
    ///
    /// It skips the typed `Model` decode. Use it for a few fields off
    /// a wide table, or when the result feeds code that wants dynamic
    /// column access, such as templates, JSON or CSV export. For
    /// GROUP BY, use [`QuerySet::values`] instead, which promotes to
    /// [`AggregateBuilder`].
    ///
    /// Columns are checked against the schema at `.compile()` time. A
    /// typo gives [`QueryError::UnknownField`], and empty `cols` gives
    /// [`QueryError::EmptyValuesProjection`].
    #[must_use]
    pub fn values_dict(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        ValuesQuerySet {
            qs: self,
            cols: cols.to_vec(),
        }
    }

    /// Project to `Vec<Vec<SqlValue>>`. Same as [`Self::values_dict`],
    /// but each row is an ordered `Vec<SqlValue>` matching the `cols`
    /// order instead of a `HashMap`.
    #[must_use]
    pub fn values_list(self, cols: &[&'static str]) -> ValuesListQuerySet<T> {
        ValuesListQuerySet {
            qs: self,
            cols: cols.to_vec(),
        }
    }

    /// Single-column projection, flattened. Returns a
    /// [`ValuesFlatQuerySet`] whose `.fetch::<U>(&pool)` decodes the
    /// column into `Vec<U>` through sqlx's `query_scalar`.
    ///
    /// `U` must decode from the column's SQL type on every dialect the
    /// binary targets. Usually `i64`, `i32`, `String`, `bool` or `f64`.
    #[must_use]
    pub fn values_list_flat(self, col: &'static str) -> ValuesFlatQuerySet<T> {
        ValuesFlatQuerySet { qs: self, col }
    }

    /// Project only the listed columns.
    /// Same behaviour as [`Self::values_dict`]:
    /// the SQL is `SELECT cols FROM …` and each row is a
    /// `HashMap<String, SqlValue>` keyed by column name. The separate
    /// name just reads better at the call site.
    ///
    /// Columns are checked at `.compile()` time. A typo gives
    /// [`QueryError::UnknownField`], and an empty list gives
    /// [`QueryError::EmptyValuesProjection`].
    ///
    /// You get a `HashMap` rather than a partly filled `Model`: there
    /// are no lazy-loading field descriptors, so a half-built struct
    /// would silently read as zeroed.
    #[must_use]
    pub fn only(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        self.values_dict(cols)
    }

    /// Project every scalar column EXCEPT the listed ones — the
    /// inverse of [`Self::only`]. The SELECT leaves
    /// out the named columns, which saves IO on wide tables.
    ///
    /// Same return shape as [`Self::only`].
    ///
    /// A column that does not exist on the model gives
    /// [`QueryError::UnknownField`] at `.compile()` time, as it would
    /// in `.only(...)`. An empty list returns every scalar column.
    #[must_use]
    pub fn defer(self, cols: &[&'static str]) -> ValuesQuerySet<T> {
        let model = T::SCHEMA;
        let exclude: std::collections::HashSet<&'static str> = cols.iter().copied().collect();
        let mut projection: Vec<&'static str> = model
            .scalar_fields()
            .filter(|f| !exclude.contains(f.column))
            .map(|f| f.column)
            .collect();
        // Pass unknown defer columns into the projection so
        // `values_dict`'s `UnknownField` check rejects them.
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
        // Fold global scopes into the WHERE, so a bulk delete honours
        // the same filters as a fetch. A `published_only` scope means
        // a wholesale delete still cannot reach unpublished rows.
        self.apply_global_scopes();
        let model: &'static ModelSchema = T::SCHEMA;
        let where_clause = resolve_pending(model, self.pending)?;
        // `.none().delete()` must delete nothing. An `IS NULL` test on
        // the NOT NULL primary key makes sure no row matches.
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

    /// Group by the named columns.
    ///
    /// Switches the queryset into aggregate mode and records the
    /// projection columns. Followed by `.annotate("alias", aggregate)`,
    /// it emits `SELECT cols, AGGR(...) FROM t GROUP BY cols`; the
    /// GROUP BY comes from the values list.
    ///
    /// This is **shape 2** in the cheat-sheet on [`Self::annotate`].
    /// The same IR gives equivalent SQL on Postgres, MySQL and SQLite.
    ///
    /// ```ignore
    /// // "Posts per author".
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
    /// is **shape 1** — a pure projection, with no GROUP BY emitted.
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

    /// Add a derived column without calling `.aggregate()`.
    ///
    /// Promotes to an [`AggregateBuilder`]. If the annotation
    /// aggregates rows (`Count`, `Sum`, `Avg`, …) and you did not call
    /// `.values()`, `compile()` fills GROUP BY with every
    /// non-aggregate scalar column. This is **shape 3**: each row plus
    /// a derived aggregate over its children.
    ///
    /// ```ignore
    /// // "Each author + their post count" — every column of `Author`
    /// // ends up in the SELECT list AND in GROUP BY.
    /// Author::objects()
    ///     .annotate("post_count", count_all().into())
    ///     .compile()?;
    /// ```
    ///
    /// # Projection cheat-sheet
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
    /// an [`AggregateBuilder`].
    /// Shorthand for [`Self::annotate`] with
    /// [`crate::core::subquery::scalar_subquery`]. `inner` must return
    /// one column and at most one row. Correlate it with
    /// [`crate::core::subquery::outer_ref`].
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
        // As on the SELECT and DELETE paths: an `.update().set(...)`
        // must not escape the model's global scopes.
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
        // `.none().update().set(...)` affects nothing, for the same
        // reason `.none().delete()` does.
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

/// Render the column-only view of an `order_by` list for the
/// `DistinctOnOrderByMismatch` error. `Expr` and `Random` entries
/// render as `<expr>` and `RANDOM`, so the message still shows where
/// the mismatch starts.
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

/// Lower the pending order-by list to `Vec<OrderItem>` at `compile()`
/// time. `Field` entries resolve their name against the model schema.
/// `Expr` entries pass through as they are, so a typo inside an
/// expression surfaces as a database error, like `set_expr`.
fn lower_order_items(
    model: &'static ModelSchema,
    items: Vec<PendingOrderItem>,
) -> Result<(Vec<crate::core::OrderItem>, Vec<crate::core::Join>), QueryError> {
    let mut out = Vec::with_capacity(items.len());
    let mut joins: Vec<crate::core::Join> = Vec::new();
    for item in items {
        match item {
            PendingOrderItem::Field { name, desc, nulls } => match model.field(&name) {
                Some(field) => {
                    out.push(crate::core::OrderItem::column_with_nulls(
                        field.column,
                        desc,
                        nulls,
                    ));
                }
                // Relation span, such as `order_by("author__name")`:
                // resolve the FK chain into LEFT JOINs plus an aliased
                // ORDER BY term, reusing the `filter()` walk. `order_by`
                // has no lookup suffixes, so anything after the terminal
                // column makes the key invalid.
                None if name.contains("__") => {
                    let segs: Vec<&str> = name.split("__").collect();
                    let (jns, prev_alias, current, term_i) = resolve_span_chain(model, &name)?;
                    if term_i + 1 < segs.len() {
                        return Err(QueryError::UnknownField {
                            model: current.name,
                            field: name.clone(),
                        });
                    }
                    let term_field =
                        current
                            .field(segs[term_i])
                            .ok_or_else(|| QueryError::UnknownField {
                                model: current.name,
                                field: segs[term_i].to_owned(),
                            })?;
                    // Dedupe by alias, so a path already joined by
                    // `select_related` or a filter span emits once.
                    // `compile()` dedupes again against the full set.
                    for j in jns {
                        if !joins.iter().any(|e| e.alias == j.alias) {
                            joins.push(j);
                        }
                    }
                    out.push(crate::core::OrderItem::expr_with_nulls(
                        crate::core::Expr::AliasedColumn {
                            alias: prev_alias,
                            column: term_field.column,
                        },
                        desc,
                        nulls,
                    ));
                }
                None => {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: name,
                    })
                }
            },
            PendingOrderItem::Expr { expr, desc, nulls } => {
                out.push(crate::core::OrderItem::expr_with_nulls(expr, desc, nulls));
            }
            PendingOrderItem::Random => {
                out.push(crate::core::OrderItem::random());
            }
        }
    }
    Ok((out, joins))
}

/// Convert `select_related` field names into `Join`s.
///
/// **Single hop**, such as `"author"`: look up the field on `model`,
/// check it is a `Relation::Fk` or `Relation::O2O`, find the target
/// schema in inventory, and build a LEFT `Join` projecting all the
/// target's columns.
///
/// **Multi-hop**, such as `"author__profile__country"`: split on `__`
/// and resolve each hop against the previous hop's target schema. Each
/// hop uses the previous join's alias on the left of its `ON`
/// predicate, so the FK chain stitches together in SQL. The join alias
/// is the full dotted path, which stays unique even when two roots
/// share a tail name.
///
/// Any hop that does not resolve returns `SelectRelatedInvalid` naming
/// its position in the chain.
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
            // Per-hop alias: the dotted path up to this hop. A single
            // hop is just `field.name`; a chain accumulates `author`,
            // `author__profile`, `author__profile__country`.
            //
            // The writer needs `&'static str`. Single-hop already has
            // one from the schema. For a chain we leak the composite
            // alias on purpose: depth is small and bounded by the
            // user's schema, so it is a tiny one-time cost.
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

/// Result of [`parse_lookup`]: a plain field/op/value triple, a
/// date-transform wrapper the resolver re-renders as
/// `EXTRACT(…) <op> ?`, or a relation span for the resolver to walk.
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
    /// Relation-spanning lookup such as `author__name` or
    /// `author__profile__bio__icontains`: one or more FK hops before
    /// the terminal column, with an optional lookup suffix.
    /// `parse_lookup` cannot tell an FK hop from a scalar field, so it
    /// defers the whole key here. `resolve_span` walks it against the
    /// schema and builds the JOINs and the aliased predicate, or
    /// reports the original `UnknownLookup` if no FK chain matches.
    RelationSpan { raw_key: String, value: SqlValue },
}

/// Map a date-transform suffix token to its scalar fn. `None` means
/// the token is not a date transform, so the caller falls through to
/// the comparison-suffix table.
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
/// date-transform lookups. This is a small subset of the full lookup
/// grammar on purpose: LIKE, IN, IS NULL and regex make no sense on an
/// extracted integer or date.
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

/// Parse a `"field"` or `"field__suffix"` key and its
/// value into a [`ParsedLookup`]. The suffix table lives on
/// [`QuerySet::filter`].
///
/// An unknown `__suffix` gives [`QueryError::UnknownLookup`]. A wrong
/// value shape for `__in`, `__isnull`, `__between` or `__range` gives
/// [`QueryError::InvalidLookupValue`].
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

    // Date-transform suffixes: `__year`, `__year__gte`, `__date` and
    // so on. Split the suffix into a transform token and an optional
    // trailing comparison. If the token is not a date transform, fall
    // through to the comparison-suffix table.
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
        "iexact" => {
            // Case-insensitive equality, not a pattern. Escape the
            // value so `%` and `_` match themselves; otherwise
            // `email__iexact` with a `%` would match every row.
            let v = wrap_like(&value, "", "", &field, suffix)?;
            Ok(pair(field, Op::ILikeEscaped, v))
        }
        "contains" => {
            let v = wrap_like(&value, "%", "%", &field, suffix)?;
            Ok(pair(field, Op::LikeEscaped, v))
        }
        "icontains" => {
            let v = wrap_like(&value, "%", "%", &field, suffix)?;
            Ok(pair(field, Op::ILikeEscaped, v))
        }
        "startswith" => {
            let v = wrap_like(&value, "", "%", &field, suffix)?;
            Ok(pair(field, Op::LikeEscaped, v))
        }
        "istartswith" => {
            let v = wrap_like(&value, "", "%", &field, suffix)?;
            Ok(pair(field, Op::ILikeEscaped, v))
        }
        "endswith" => {
            let v = wrap_like(&value, "%", "", &field, suffix)?;
            Ok(pair(field, Op::LikeEscaped, v))
        }
        "iendswith" => {
            let v = wrap_like(&value, "%", "", &field, suffix)?;
            Ok(pair(field, Op::ILikeEscaped, v))
        }
        // Raw LIKE / ILIKE / NOT LIKE / NOT ILIKE. Unlike
        // `__contains` and friends, the value is bound as given, so
        // the caller places `%` and `_`. Use it for patterns the
        // auto-wrapping suffixes cannot express, like `'%foo%bar%'`.
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
            // `__not_between` / `__not_range` flip it to `NOT BETWEEN`.
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
        // The array operators get explicit `__array_*` suffixes: the
        // parser has no field type to dispatch on, so a bare
        // `__contains` would collide with the text one. Prefer
        // the typed `Column::array_*` methods where you can.
        "range_contains"
        | "range_contained_by"
        | "range_overlap"
        | "range_strictly_left"
        | "range_strictly_right"
        | "range_adjacent" => {
            // PG range operators. The value is a range literal string
            // such as `"[1, 10)"`, which PG casts to the column's
            // range type. A plain `SqlValue::String` is wrapped into a
            // `RangeLiteral`; a `RangeLiteral` from the typed helpers
            // is taken as is. Both emit the same SQL.
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
            // The value must be a list of elements. Promote
            // `SqlValue::List` to `SqlValue::Array` so the writer
            // binds it as one PG array parameter.
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
        // Unknown suffix — but the key may be a relation span such as
        // `author__name__icontains`. There is no schema here to tell,
        // so defer the whole key to the resolver. It walks the FK
        // chain, and if the first segment is not an FK (a real typo
        // like `pages__bogus`) it raises `UnknownLookup`.
        _ => Ok(ParsedLookup::RelationSpan {
            raw_key: key.to_owned(),
            value,
        }),
    }
}

/// Wrap a `SqlValue::String` with leading and trailing wildcards for
/// `__contains` / `__startswith` / `__endswith` and their `i*` forms.
///
/// A non-string value is rejected with
/// [`QueryError::InvalidLookupValue`]: LIKE needs text.
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
    // Escape LIKE metacharacters in the user value so `%` and `_`
    // match literally. The wrapping wildcards go on after escaping, so
    // they keep their pattern meaning. The result is only correct
    // under `ESCAPE '!'`, so every caller pairs it with
    // `Op::LikeEscaped` or `Op::ILikeEscaped`.
    let escaped = crate::core::escape_like(s);
    Ok(SqlValue::String(format!("{prefix}{escaped}{suffix_char}")))
}

/// Readable shape name for an `SqlValue`, used in
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

/// Add an always-false predicate to `base` so the surrounding UPDATE
/// or DELETE matches no row.
///
/// It uses `<primary_key> IS NULL`. Every model declares a NOT NULL
/// primary key, so that can never be true, on any backend. A `1 = 0`
/// literal would need a raw-SQL escape hatch the IR does not expose,
/// while `IS NULL` reuses the existing `Op::IsNull` path.
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
        // An empty `And` collapses to the never-match predicate alone,
        // which keeps the emitted WHERE to a single column reference.
        WhereExpr::And(nodes) if nodes.is_empty() => never,
        WhereExpr::And(mut nodes) => {
            nodes.push(never);
            WhereExpr::And(nodes)
        }
        other => WhereExpr::And(vec![other, never]),
    })
}

/// Parse a `key`/`value` pair through the lookup grammar into one
/// [`PendingFilter`]. Shared by [`QuerySet::filter`] and
/// [`QuerySet::exclude`], so the `__lookup` grammar cannot drift
/// between them.
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
/// [`resolve_pending`]'s loop so [`PendingFilter::Negated`] can
/// resolve its inner entry recursively.
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
        // `lower_relation_spans` turns spans into `Expr` plus JOINs on
        // the SELECT path. One arriving here means update / delete /
        // aggregate, which cannot carry a JOIN. Resolve it first to
        // tell a real span (resolves, so report "unsupported here")
        // from a bad suffix like `status__bogus` (whose first segment
        // is not an FK, so keep the original `UnknownLookup`).
        PendingFilter::RelationSpan { raw_key, value } => {
            return match resolve_span(model, &raw_key, value) {
                Ok(_) => Err(QueryError::RelationSpanUnsupportedHere { key: raw_key }),
                Err(e) => Err(e),
            }
        }
        // Surface the deferred lookup-parse error at `compile()`.
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

    // `IsNull` carries a Bool flag, not a value to compare with the
    // field, so skip the type check. `In` carries a List, whose
    // elements are not checked one by one.
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

/// Resolve a relation-spanning lookup key into its FK-chain LEFT JOINs
/// plus an aliased `ExprCompare` predicate.
///
/// A key such as `author__profile__bio__icontains` splits into three
/// parts. Leading segments naming an FK or O2O field are JOIN hops.
/// The first non-FK segment, or the last one, is the terminal column.
/// Anything after that is a lookup suffix, re-parsed through
/// [`parse_lookup`] and applied to the aliased column.
///
/// If the first segment is not an FK, the key is not a span at all,
/// for example `pages__bogus`. The original
/// [`QueryError::UnknownLookup`] is returned so the error stays the
/// same as before.
fn resolve_span(
    model: &'static ModelSchema,
    raw_key: &str,
    value: SqlValue,
) -> Result<(Vec<crate::core::Join>, WhereExpr), QueryError> {
    use crate::core::Expr;
    let segs: Vec<&str> = raw_key.split("__").collect();
    // The FK-chain walk is shared with the `order_by` path.
    let (joins, prev_alias, current, term_i) = resolve_span_chain(model, raw_key)?;

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
            // A trailing suffix still unknown here is a real typo on
            // the terminal field.
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

/// Walk a relation-span path's FK / O2O hops into LEFT JOINs. Shared
/// by [`resolve_span`] (the `filter()` path) and [`lower_order_items`]
/// (the `order_by()` path), so the two cannot drift.
///
/// Returns the JOINs, the alias of the table holding the terminal
/// column, that table's schema, and the index of the terminal segment
/// in the `__`-split key. The terminal column is
/// `current.field(segs[term_i])`; callers turn it into a predicate or
/// an `OrderItem`.
///
/// Returns the original [`QueryError::UnknownLookup`] when the first
/// segment is not an FK, so non-span keys keep their existing error.
fn resolve_span_chain(
    model: &'static ModelSchema,
    raw_key: &str,
) -> Result<
    (
        Vec<crate::core::Join>,
        &'static str,
        &'static ModelSchema,
        usize,
    ),
    QueryError,
> {
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
                // Leak the dotted alias: depth is bounded by the
                // schema and the cost is paid once at compile() time.
                // It matches `lower_select_related`'s scheme, so a
                // span and a `select_related` over the same path
                // dedupe against each other.
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

    // No hop consumed means the first segment is not an FK, so this is
    // an unknown suffix on a scalar field. Return the original error:
    // `pages__bogus` gives `UnknownLookup { pages, bogus }`.
    if joins.is_empty() {
        let suffix = raw_key.splitn(2, "__").nth(1).unwrap_or("").to_owned();
        return Err(QueryError::UnknownLookup {
            field: segs.first().map_or_else(String::new, |s| (*s).to_owned()),
            suffix,
        });
    }

    Ok((joins, prev_alias, current, term_i))
}

/// Pre-pass over the pending filters. Lowers every
/// [`PendingFilter::RelationSpan`], including ones nested in a
/// [`PendingFilter::Negated`] from `exclude`, into a resolved `Expr`
/// predicate and collects the FK-chain JOINs. Returns the rewritten
/// pending list and the JOINs for the SELECT path to merge by alias.
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

/// Resolve a [`DateTransformFilter`] into a `WhereExpr::ExprCompare`.
/// The field is looked up in the schema and wrapped in its
/// [`ScalarFn`], such as `EXTRACT(YEAR FROM …)`, on the left. The
/// bound value becomes a literal on the right.
///
/// This skips the type check [`resolve_filter`] does: the extracted
/// value is an integer or date, not the column's own type.
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
/// schema. The field name and every column reference in the
/// expression tree are checked. The literal type check that
/// [`resolve_assignment`] does for [`SqlValue`] is skipped: the right
/// side may be a column reference or arithmetic, whose type is only
/// known per row.
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

/// Walk an [`crate::core::Expr`] and check every `Column` reference
/// resolves on `model`. Same check `core::query::WhereExpr::validate`
/// does for `ColumnCompare`, repeated here so the `UpdateBuilder` path
/// catches a typo at `compile()` time instead of at the database.
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
        // None of these resolve against this model. A subquery was
        // already checked by its own `compile()`. An `OuterRef` was
        // checked by the outer queryset. An `AliasedColumn` belongs to
        // a joined alias and is checked by the JOIN writer.
        // `AggregateSubquery` and `RelAggregate` resolve against the
        // child model or relation table.
        Expr::Subquery(_)
        | Expr::AggregateSubquery(_)
        | Expr::OuterRef(_)
        | Expr::RelAggregate { .. }
        | Expr::AliasedColumn { .. } => Ok(()),
        // A window's partition_by, order_by and arg columns do belong
        // to this model, so walk them.
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
        // The flat aggregate variants hold raw column names this
        // walker does not visit — a known gap. Window-shaped
        // aggregates go through `validate_aggregate_expr_columns`.
        Expr::Aggregate(_) => Ok(()),
        // Only a JSON path's `source` is a column expression; the
        // steps are keys and indices, not model columns.
        Expr::JsonPath { source, .. } => validate_expr_columns_in_model(model, source),
    }
}

// ------------------------------------------------------------------ AggregateBuilder

/// Fluent builder for [`AggregateQuery`]. Constructed via [`QuerySet::aggregate`].
pub struct AggregateBuilder<T: Model> {
    qs: QuerySet<T>,
    group_by: Vec<&'static str>,
    aggregates: Vec<(std::borrow::Cow<'static, str>, AggregateExpr)>,
    /// Aliases: annotations you can use in
    /// `.filter(name, …)` and `.order_by([(name, …)])`, but that never
    /// appear in the SELECT list. `compile()` lifts the expression
    /// into the predicate or ORDER item.
    aliases: Vec<(std::borrow::Cow<'static, str>, AggregateExpr)>,
    having: Option<WhereExpr>,
    order_by: Vec<(&'static str, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// First builder-time validation error, surfaced from `compile()`.
    /// An example is `.filter(alias, Op::JsonContains, …)` on an
    /// annotation alias: the JSON ops need dialect-specific writers
    /// that do not work against an aggregate on the left. Only the
    /// first error is kept, so later calls cannot mask the cause.
    deferred_error: Option<crate::core::QueryError>,
    /// Projection columns from [`Self::values`] or [`QuerySet::values`].
    /// With `Some(cols)` and no explicit `.group_by(...)`, `compile()`
    /// derives `GROUP BY cols`. With `None` and an aggregating
    /// annotation, it groups by every non-aggregate scalar column
    /// (shape 3).
    values: Option<Vec<&'static str>>,
}

impl<T: Model> AggregateBuilder<T> {
    /// Add a `GROUP BY` column. Call multiple times to group by multiple columns.
    ///
    /// An explicit `.group_by(...)` always wins. Paired with
    /// [`Self::values`], the explicit list reaches the writer while
    /// the values list still drives the SELECT projection.
    #[must_use]
    pub fn group_by(mut self, column: &'static str) -> Self {
        self.group_by.push(column);
        self
    }

    /// Set the projection column list. Same as [`QuerySet::values`],
    /// but usable mid-chain when you started from `.aggregate()`.
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

    /// Project a correlated scalar subquery under `alias`.
    /// Shorthand for [`Self::annotate`]
    /// with [`crate::core::subquery::scalar_subquery`]. `inner` must
    /// return one column and at most one row.
    #[must_use]
    pub fn annotate_subquery(self, alias: &'static str, inner: crate::core::SelectQuery) -> Self {
        self.annotate(alias, crate::core::subquery::scalar_subquery(inner))
    }

    /// Resolve a relation `name` (reverse-FK, M2M or generic-FK) into
    /// a correlated `RelatedAggregate` and stage it on this builder.
    /// Shared with [`QuerySet::annotate_count`] and friends, so a
    /// query can chain several relation aggregates. Errors go to
    /// `self.deferred_error` and surface from `compile()`.
    fn push_relation_aggregate(
        &mut self,
        name: &str,
        column: Option<&'static str>,
        agg: AggregateExpr,
        suffix: &str,
    ) {
        use crate::core::{subquery, RelAggKind};
        // `<name>_count` (no column) or `<name>_<suffix>_<column>`.
        let alias = || match column {
            Some(col) => std::borrow::Cow::Owned(format!("{name}_{suffix}_{col}")),
            None => std::borrow::Cow::Owned(format!("{name}_{suffix}")),
        };
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
                    self.deferred_error.get_or_insert(QueryError::UnknownField {
                        model: rel.child_schema.name,
                        field: col.to_owned(),
                    });
                    return;
                }
            }
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::reverse_has_aggregate(
                rel, agg,
            )));
            self.aggregates.push((alias(), expr));
            return;
        }

        // Many-to-many: `Count` over the junction table, and the other
        // aggregates over a target column reached through it. The
        // target table has no `ModelSchema` here, so a bad `column`
        // only surfaces at the database.
        if let Some(m2m) = T::SCHEMA.m2m.iter().find(|m| m.name == name) {
            let pk = T::SCHEMA.primary_key().map_or("id", |f| f.column);
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::m2m_has_aggregate(
                m2m,
                pk,
                rel_kind(&agg),
                column,
            )));
            self.aggregates.push((alias(), expr));
            return;
        }

        // Generic-FK — aggregate over the polymorphic child table,
        // discriminated by content type.
        if let Some(g) = T::generic_reverse_relations()
            .iter()
            .find(|g| g.name == name)
        {
            if let Some(col) = column {
                if g.child_schema.field_by_column(col).is_none() {
                    self.deferred_error.get_or_insert(QueryError::UnknownField {
                        model: g.child_schema.name,
                        field: col.to_owned(),
                    });
                    return;
                }
            }
            let expr = AggregateExpr::RelatedAggregate(Box::new(subquery::generic_has_aggregate(
                g,
                T::SCHEMA.table,
                rel_kind(&agg),
                column,
            )));
            self.aggregates.push((alias(), expr));
            return;
        }

        self.deferred_error.get_or_insert(QueryError::UnknownField {
            model: T::SCHEMA.name,
            field: name.to_owned(),
        });
    }

    /// The `<name>_exists` companion of
    /// [`Self::push_relation_aggregate`], projecting
    /// `CASE WHEN EXISTS(...) THEN 1 ELSE 0`. It reuses the `QuerySet`
    /// resolver, so the reverse-FK / M2M / GFK logic lives in one
    /// place.
    fn push_relation_exists(&mut self, name: &str) {
        match QuerySet::<T>::resolve_rel_exists(name, /*negated=*/ false) {
            Some(exists) => {
                let expr = AggregateExpr::RelatedAggregate(Box::new(
                    crate::core::subquery::exists_as_int(exists),
                ));
                self.aggregates
                    .push((std::borrow::Cow::Owned(format!("{name}_exists")), expr));
            }
            None => {
                self.deferred_error.get_or_insert(QueryError::UnknownField {
                    model: T::SCHEMA.name,
                    field: name.to_owned(),
                });
            }
        }
    }

    /// Chain a relation `COUNT` onto an existing aggregate builder,
    /// like [`QuerySet::annotate_count`], so two relation counts fit in
    /// one query:
    /// `Post::objects().annotate_count("comments").annotate_count("likes")`.
    #[must_use]
    pub fn annotate_count(mut self, name: &str) -> Self {
        self.push_relation_aggregate(name, None, AggregateExpr::Count(None), "count");
        self
    }

    /// Chain a relation `SUM(<column>)`. See [`QuerySet::annotate_sum`].
    #[must_use]
    pub fn annotate_sum(mut self, name: &str, column: &'static str) -> Self {
        self.push_relation_aggregate(name, Some(column), AggregateExpr::Sum(column), "sum");
        self
    }

    /// Chain a relation `AVG(<column>)`. See [`QuerySet::annotate_avg`].
    #[must_use]
    pub fn annotate_avg(mut self, name: &str, column: &'static str) -> Self {
        self.push_relation_aggregate(name, Some(column), AggregateExpr::Avg(column), "avg");
        self
    }

    /// Chain a relation `MAX(<column>)`. See [`QuerySet::annotate_max`].
    #[must_use]
    pub fn annotate_max(mut self, name: &str, column: &'static str) -> Self {
        self.push_relation_aggregate(name, Some(column), AggregateExpr::Max(column), "max");
        self
    }

    /// Chain a relation `MIN(<column>)`. See [`QuerySet::annotate_min`].
    #[must_use]
    pub fn annotate_min(mut self, name: &str, column: &'static str) -> Self {
        self.push_relation_aggregate(name, Some(column), AggregateExpr::Min(column), "min");
        self
    }

    /// Chain a relation `<name>_exists` projection. See
    /// [`QuerySet::annotate_exists`].
    #[must_use]
    pub fn annotate_exists(mut self, name: &str) -> Self {
        self.push_relation_exists(name);
        self
    }

    /// Annotate without projecting. The
    /// expression is registered under `name` and usable in
    /// [`Self::filter`] and [`Self::order_by`], but the writer
    /// **leaves it out of the SELECT list**.
    ///
    /// Use it to filter or order by a derived aggregate without
    /// decoding that column on every row:
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
    /// **Order matters**, as with [`Self::annotate`]: call
    /// `.alias(name, ...)` BEFORE the matching `.filter(name, ...)` or
    /// `.order_by([(name, ...)])`, so the lookup can find it.
    ///
    /// If `name` clashes with an annotate alias, the annotate wins.
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

    /// String-keyed filter that routes itself to WHERE or HAVING.
    /// If `field`
    /// matches an annotation alias from [`Self::annotate`], the
    /// predicate joins `HAVING`. Otherwise it goes to the WHERE path
    /// on the underlying `QuerySet`.
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
    /// **Order matters**: `.annotate(alias, ...)` must come BEFORE the
    /// matching `.filter(alias, ...)`, because the alias is resolved
    /// at call time, not when the query compiles.
    ///
    /// **Gap**: an alias-routed HAVING predicate skips the schema
    /// column check, because the alias is not a real column, so a
    /// typo surfaces at the database rather than at `compile()`.
    /// WHERE-routed predicates are still checked.
    #[must_use]
    pub fn filter(
        mut self,
        field: &'static str,
        op: crate::core::Op,
        value: impl Into<crate::core::SqlValue>,
    ) -> Self {
        // Once an error is recorded, ignore later builder calls so the
        // original cause is not masked.
        if self.deferred_error.is_some() {
            return self;
        }
        // Find the AggregateExpr behind the alias. PG forbids
        // SELECT-list aliases in HAVING, so lift the whole expression
        // into the predicate instead of naming the alias:
        // `HAVING COUNT(*) > $1` works on every backend.
        // `.alias()` entries are lift-able too; annotate wins a clash
        // because it comes first.
        let agg = self
            .aggregates
            .iter()
            .chain(self.aliases.iter())
            .find(|(alias, _)| *alias == field)
            .map(|(_, expr)| expr.clone());
        if let Some(agg) = agg {
            // `write_expr_compare` handles the comparison operators
            // plus IN, BETWEEN, IS NULL, LIKE and ILIKE against an
            // aggregate on the left. JSON ops and null-safe equality
            // still need dialect-specific writers that take a `&str`
            // left side, so they are rejected here.
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
            // Forward to the QuerySet's WHERE path, where
            // `resolve_pending` catches a typo'd column at compile().
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
    /// # GROUP BY inference
    ///
    /// Without an explicit `.group_by(...)`, the builder fills it in:
    ///
    /// * `.values(cols).annotate(agg)` → `GROUP BY cols` (shape 2).
    /// * `.annotate(agg)` (no values) → `GROUP BY` every non-aggregate
    ///   scalar column on the model (shape 3).
    /// * `.annotate(window)` only → no GROUP BY (window functions are
    ///   per-row).
    ///
    /// Explicit `.group_by(...)` always wins.
    pub fn compile(mut self) -> Result<AggregateQuery, QueryError> {
        // Report any deferred builder error first, so the user sees
        // the real cause and not a later compile failure.
        if let Some(e) = self.deferred_error {
            return Err(e);
        }
        // Fold global scopes into the WHERE so aggregates honour them
        // too: `.count()` on a `published_only` model counts published
        // rows, not the whole table.
        self.qs.apply_global_scopes();
        // Carry the QuerySet's ad-hoc JOINs over so the aggregate can
        // group by a related column, as in `group_by("author.name")`.
        // Empty for a plain single-table aggregate.
        let joins = std::mem::take(&mut self.qs.ad_hoc_joins);
        let model = T::SCHEMA;
        let where_clause = resolve_pending(model, self.qs.pending)?;
        // Check each AggregateExpr for column typos. This covers the
        // partition_by, order_by and args of a window aggregate. The
        // flat `Sum("col")` / `Count(Some("col"))` shapes are a known
        // gap and are not checked.
        for (_alias, expr) in self.aggregates.iter().chain(self.aliases.iter()) {
            validate_aggregate_expr_columns(model, expr)?;
        }
        // `.order_by((name, desc))` uses the same alias registry as
        // `.filter(name, ...)`. On a match, lift the whole aggregate
        // expression into the `ORDER BY`. That is required for an
        // `.alias()` name, which is not in the SELECT, and harmless
        // for an `.annotate()` one.
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

        // GROUP BY inference. `.alias()` entries count here too: an
        // `.alias("c", Count(...))` still needs a GROUP BY even
        // though nothing is projected.
        let has_aggregating = self
            .aggregates
            .iter()
            .chain(self.aliases.iter())
            .any(|(_, e)| e.is_aggregating());
        let group_by = if !self.group_by.is_empty() {
            // An explicit `.group_by(...)` always wins. Check the
            // columns now, or a typo would only surface at the DB.
            for col in &self.group_by {
                // A dotted `alias.col` names a joined table, so the
                // JOIN validates it, not the base model.
                if !col.contains('.') && model.field_by_column(col).is_none() {
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
                if !col.contains('.') && model.field_by_column(col).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*col).to_owned(),
                    });
                }
            }
            cols.clone()
        } else if has_aggregating {
            // Shape 3: an implicit GROUP BY over every non-aggregate
            // scalar column.
            model.scalar_fields().map(|f| f.column).collect()
        } else {
            // No aggregating annotation and no values: a pure window
            // annotation or an empty builder. No GROUP BY.
            Vec::new()
        };

        // `.none().count()` must return zero without scanning a row.
        // Use the same never-match guard as UPDATE / DELETE, and add
        // `LIMIT 0` so the executor stops early even when a GROUP BY
        // could otherwise produce rows.
        let (where_clause, limit) = if self.qs.is_none {
            (never_match_clause(model, where_clause)?, Some(0))
        } else {
            (where_clause, self.limit)
        };

        Ok(AggregateQuery {
            model,
            joins,
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

/// Walk an [`AggregateExpr`] for column references that must resolve
/// against `model`. Only `AggregateExpr::Window` carries columns this
/// can check: its `partition_by`, `order_by` and any `Expr::Column`
/// argument. The flat variants such as `Sum("col")` hold a bare
/// `&'static str` that no validator visits — a known gap.
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
        // The flat variants (Count, Sum, Avg, Max, Min, CountDistinct,
        // StdDev*, Variance*) name their column with a bare
        // `&'static str` that nothing here checks, so a typo only
        // surfaces at execution. Known gap.
        _ => Ok(()),
    }
}

// ====================================================================
// Pure projection — `.values_dict()` / `.values_list()`
// ====================================================================

/// Pure-projection queryset returned by [`QuerySet::values_dict`].
/// Compiles to a `SELECT <cols> FROM …` with the WHERE / ORDER BY /
/// LIMIT / OFFSET / set-algebra branches of the underlying queryset
/// preserved. The terminal is [`Self::fetch`], added by the executor.
pub struct ValuesQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) cols: Vec<&'static str>,
}

/// Pure-projection queryset returned by [`QuerySet::values_list`].
/// Like [`ValuesQuerySet`], but the fetch yields `Vec<Vec<SqlValue>>`
/// with cells in `cols` order instead of a `HashMap`.
pub struct ValuesListQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) cols: Vec<&'static str>,
}

/// Single-column projection queryset returned by
/// [`QuerySet::values_list_flat`]. The fetch decodes the column into
/// `Vec<U>` through sqlx's typed scalar path.
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

    /// The validated column list, so the fetch in [`crate::sql`] can
    /// pass it to the row decoder.
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

    /// The validated column list, for the terminal fetch.
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
// `.dates(field, kind)` — distinct truncated dates
// ====================================================================

/// Truncation granularity for [`QuerySet::dates`] / [`QuerySet::datetimes`].
/// One of year, month or day.
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
            // Unknown dialect: fall back to PG-shape DATE_TRUNC. The
            // driver reports a clear syntax error if it is unsupported.
            (_, DateKind::Year) => format!("DATE_TRUNC('year', {col_quoted})"),
            (_, DateKind::Month) => format!("DATE_TRUNC('month', {col_quoted})"),
            (_, DateKind::Day) => format!("DATE({col_quoted})"),
        }
    }
}

/// Builder returned by [`QuerySet::dates`]. Run it with
/// [`crate::sql::fetch_dates_pool`], which emits
/// `SELECT DISTINCT <trunc(col)> AS d FROM (<inner-query>) sub ORDER BY d`.
/// The wrapper keeps the queryset's WHERE, JOINs, LIMIT and ORDER BY,
/// so filters set before `.dates()` still apply.
pub struct DatesQuerySet<T: Model> {
    pub(crate) qs: QuerySet<T>,
    pub(crate) field: &'static str,
    pub(crate) kind: DateKind,
    pub(crate) descending: bool,
}

impl<T: Model> DatesQuerySet<T> {
    /// Reverse the ORDER BY direction. The default is ascending,
    /// oldest first.
    #[must_use]
    pub fn order_desc(mut self, desc: bool) -> Self {
        self.descending = desc;
        self
    }

    /// Check the field and return its column name from the schema.
    ///
    /// # Errors
    /// [`QueryError::UnknownField`] when the model has no such field,
    /// and [`QueryError::TypeMismatch`] when it is not a date or
    /// datetime column.
    pub fn resolve_column(&self) -> Result<&'static str, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let field = model
            .field(self.field)
            .ok_or_else(|| QueryError::UnknownField {
                model: model.name,
                field: self.field.to_owned(),
            })?;
        // The field must be a Date or DateTime, or the truncation
        // means nothing. Other types would quietly produce garbage on
        // some backends.
        if !matches!(
            field.ty,
            crate::core::FieldType::Date | crate::core::FieldType::DateTime
        ) {
            return Err(QueryError::TypeMismatch {
                model: model.name,
                field: self.field.to_owned(),
                // Either Date or DateTime is fine; report DateTime as
                // the representative, since truncation always yields
                // a Date whatever the input.
                expected: crate::core::FieldType::DateTime,
                actual: field.ty,
            });
        }
        Ok(field.column)
    }
}

impl<T: Model> QuerySet<T> {
    /// The distinct date values of `field`, truncated to `kind`.
    ///
    /// Output is ascending by default, oldest first. Chain
    /// [`DatesQuerySet::order_desc`] for newest first.
    ///
    /// Filters, joins and limits on the queryset still apply, so
    /// `.filter(...).dates(...)` only sees matching rows.
    #[must_use]
    pub fn dates(self, field: &'static str, kind: DateKind) -> DatesQuerySet<T> {
        DatesQuerySet {
            qs: self,
            field,
            kind,
            descending: false,
        }
    }

    /// The distinct datetime values of `field`, truncated to `kind`.
    ///
    /// Finer than [`Self::dates`]: as well as `Year`, `Month` and
    /// `Day`, it takes `Hour`, `Minute` and `Second`. Each value is a
    /// `DateTime<Utc>` at the truncated instant.
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

/// Truncation granularity for [`QuerySet::datetimes`]: year, month,
/// day, hour, minute or second.
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
                // CAST back to DATETIME so the decoder sees the right
                // type.
                format!("CAST(DATE_FORMAT({col_quoted}, '{fmt}') AS DATETIME)")
            }
            _ => {
                // SQLite and anything else: format with strftime so
                // the value comes back in the standard ISO-8601 shape.
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

/// Builder returned by [`QuerySet::datetimes`]. Like
/// [`DatesQuerySet`], but with finer granularity and a
/// `DateTime<Utc>` return type.
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
