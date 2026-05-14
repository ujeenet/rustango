//! Query layer for rustango.
//!
//! v0.1 ships a typed `QuerySet<T>` that builds an `AND`-joined `WHERE`
//! clause and compiles to the dialect-neutral `SelectQuery` IR in
//! `rustango-core`. `UpdateBuilder<T>` mirrors the same shape for `UPDATE`,
//! and `QuerySet<T>` itself is the input to bulk delete. The dynamic
//! resolver lands in week 5.

use std::marker::PhantomData;

use crate::core::{
    AggregateExpr, AggregateQuery, Assignment, DeleteQuery, Filter, Model, ModelSchema, Op,
    OrderClause, QueryError, SelectQuery, SqlValue, TypedAssignment, TypedExpr, UpdateQuery,
    WhereExpr,
};

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
pub struct QuerySet<T: Model> {
    pending: Vec<PendingFilter>,
    limit: Option<i64>,
    offset: Option<i64>,
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
    /// `(field_name, desc)` pairs registered via [`Self::order_by`].
    /// Slice 9.0b. Resolved against the schema at `compile()` time.
    order_by: Vec<(String, bool)>,
    _model: PhantomData<fn() -> T>,
}

/// Filter accumulator entry — keeps insertion order across string-keyed and
/// typed filter calls. Each entry contributes one node to the final
/// `WhereExpr::And` clause.
enum PendingFilter {
    /// String-keyed; resolved against the schema at `compile` time.
    Raw(RawFilter),
    /// Already resolved by a typed [`Column`](crate::core::Column).
    Resolved(Filter),
    /// Typed sub-expression (built via `.and()` / `.or()` on the
    /// typed-column API). Already validated; contributes a whole
    /// sub-tree to the WHERE clause.
    Expr(WhereExpr),
}

#[derive(Debug, Clone)]
struct RawFilter {
    field: String,
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

impl<T: Model> QuerySet<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            limit: None,
            offset: None,
            select_related: Vec::new(),
            ad_hoc_joins: Vec::new(),
            order_by: Vec::new(),
            _model: PhantomData,
        }
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
            self.order_by.push(((*field).to_owned(), *desc));
        }
        self
    }

    /// v0.45 — discard any previously-set `order_by` and apply
    /// `items` as the new ordering. Used by `earliest_pool` and
    /// `latest_pool` which declare their own sort.
    #[must_use]
    pub fn replace_order_by(mut self, items: &[(&str, bool)]) -> Self {
        self.order_by.clear();
        self.order_by(items)
    }

    /// v0.45 — flip every ordering direction in place. Used by
    /// `last_pool` to invert the queryset's natural sort and take
    /// the first row from the reversed sequence — avoids OFFSET +
    /// COUNT(*) and works on every dialect.
    #[must_use]
    pub fn flip_order_by(mut self) -> Self {
        for entry in &mut self.order_by {
            entry.1 = !entry.1;
        }
        self
    }

    /// v0.45 — read-only view of the current `order_by` for callers
    /// that need to inspect it (e.g. inserting a PK fallback).
    #[must_use]
    pub fn order_by_clauses(&self) -> &[(String, bool)] {
        &self.order_by
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

    /// Append a `WHERE field <op> value` predicate.
    ///
    /// `field` is the Rust-side field name; the column is looked up from the
    /// schema at compile time.
    #[must_use]
    pub fn filter(mut self, field: impl Into<String>, op: Op, value: impl Into<SqlValue>) -> Self {
        self.pending.push(PendingFilter::Raw(RawFilter {
            field: field.into(),
            op,
            value: value.into(),
        }));
        self
    }

    /// Sugar for `filter(field, Op::Eq, value)`.
    #[must_use]
    pub fn eq(self, field: impl Into<String>, value: impl Into<SqlValue>) -> Self {
        self.filter(field, Op::Eq, value)
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
    pub fn compile(self) -> Result<SelectQuery, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let where_clause = resolve_pending(model, self.pending)?;
        let mut joins = lower_select_related(model, &self.select_related)?;
        // Ad-hoc joins (issue #80) come AFTER FK-driven select_related
        // joins in the SELECT — preserves the existing column ordering
        // for legacy callers, and keeps user-driven joins next to the
        // user-driven WHERE.
        joins.extend(self.ad_hoc_joins);
        let order_by = lower_order_by(model, &self.order_by)?;
        Ok(SelectQuery {
            model,
            where_clause,
            search: None,
            joins,
            order_by,
            limit: self.limit,
            offset: self.offset,
        })
    }

    /// Lower this queryset to a `DeleteQuery` — same WHERE clause, no projection.
    ///
    /// # Errors
    /// As [`QuerySet::compile`].
    pub fn compile_delete(self) -> Result<DeleteQuery, QueryError> {
        let model: &'static ModelSchema = T::SCHEMA;
        let where_clause = resolve_pending(model, self.pending)?;
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
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }
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
    pub fn compile(self) -> Result<UpdateQuery, QueryError> {
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

        Ok(UpdateQuery {
            model,
            set: assignments,
            where_clause,
        })
    }
}

/// Convert `(field_name, desc)` pairs into `OrderClause`s by
/// resolving each field name on the schema. Slice 9.0b.
fn lower_order_by(
    model: &'static ModelSchema,
    items: &[(String, bool)],
) -> Result<Vec<crate::core::OrderClause>, QueryError> {
    let mut out = Vec::with_capacity(items.len());
    for (field_name, desc) in items {
        let field = model
            .field(field_name)
            .ok_or_else(|| QueryError::UnknownField {
                model: model.name,
                field: field_name.clone(),
            })?;
        out.push(crate::core::OrderClause {
            column: field.column,
            desc: *desc,
        });
    }
    Ok(out)
}

/// Convert `select_related` field names into `Join`s — slice 9.0d.
///
/// For each name: look up the field on `model`, verify it's a
/// `Relation::Fk`, find the target schema in inventory, build a
/// `Join` projecting all of the target's columns. Errors out on any
/// unresolvable name with a clear `SelectRelatedInvalid` reason.
fn lower_select_related(
    model: &'static ModelSchema,
    names: &[String],
) -> Result<Vec<crate::core::Join>, QueryError> {
    use crate::core::{inventory, Expr, Join, JoinKind, ModelEntry, Op, Relation, WhereExpr};
    let mut out: Vec<Join> = Vec::with_capacity(names.len());
    for name in names {
        let field = model
            .field(name)
            .ok_or_else(|| QueryError::SelectRelatedInvalid {
                model: model.name,
                field: name.clone(),
                reason: format!("no field `{name}` on this model"),
            })?;
        let (to, on) = match field.relation {
            Some(Relation::Fk { to, on }) | Some(Relation::O2O { to, on }) => (to, on),
            _ => {
                return Err(QueryError::SelectRelatedInvalid {
                    model: model.name,
                    field: name.clone(),
                    reason: "not a `ForeignKey<T>` field".into(),
                });
            }
        };
        let target = inventory::iter::<ModelEntry>
            .into_iter()
            .find(|e| e.schema.table == to)
            .map(|e| e.schema)
            .ok_or_else(|| QueryError::SelectRelatedInvalid {
                model: model.name,
                field: name.clone(),
                reason: format!(
                    "target table `{to}` is not registered (is the parent's `#[derive(Model)]` linked into the binary?)"
                ),
            })?;
        // Project every column on the target so the decoder has the
        // full row to rebuild a `Target` instance.
        let project: Vec<&'static str> = target.scalar_fields().map(|f| f.column).collect();
        // The Rust field name is unique within the model, so it makes
        // a clean alias prefix that doesn't collide with other JOINs
        // the writer or admin might add later.
        let alias = field.name;
        out.push(Join {
            target,
            alias,
            kind: JoinKind::Left,
            // `<main_table>.<fk_col> = <alias>.<target_pk>` — both
            // sides aliased so the writer doesn't need to remember
            // which side is "outer". Same SQL the legacy emitter
            // produced; just expressed as a WhereExpr now.
            on: WhereExpr::ExprCompare {
                lhs: Expr::AliasedColumn {
                    alias: model.table,
                    column: field.column,
                },
                op: Op::Eq,
                rhs: Expr::AliasedColumn { alias, column: on },
            },
            project,
        });
    }
    Ok(out)
}

fn resolve_pending(
    model: &'static ModelSchema,
    pending: Vec<PendingFilter>,
) -> Result<WhereExpr, QueryError> {
    let mut nodes: Vec<WhereExpr> = Vec::with_capacity(pending.len());
    for entry in pending {
        match entry {
            PendingFilter::Raw(raw) => {
                nodes.push(WhereExpr::Predicate(resolve_filter(model, raw)?));
            }
            PendingFilter::Resolved(filter) => {
                nodes.push(WhereExpr::Predicate(filter));
            }
            PendingFilter::Expr(expr) => {
                nodes.push(expr);
            }
        }
    }
    Ok(WhereExpr::And(nodes))
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
        Expr::Subquery(_) | Expr::OuterRef(_) | Expr::AliasedColumn { .. } => Ok(()),
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
    }
}

// ------------------------------------------------------------------ AggregateBuilder

/// Fluent builder for [`AggregateQuery`]. Constructed via [`QuerySet::aggregate`].
pub struct AggregateBuilder<T: Model> {
    qs: QuerySet<T>,
    group_by: Vec<&'static str>,
    aggregates: Vec<(&'static str, AggregateExpr)>,
    having: Option<WhereExpr>,
    order_by: Vec<(&'static str, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
}

impl<T: Model> AggregateBuilder<T> {
    /// Add a `GROUP BY` column. Call multiple times to group by multiple columns.
    #[must_use]
    pub fn group_by(mut self, column: &'static str) -> Self {
        self.group_by.push(column);
        self
    }

    /// Add an aggregate expression under `alias` (e.g. `"post_count"`).
    #[must_use]
    pub fn annotate(mut self, alias: &'static str, expr: AggregateExpr) -> Self {
        self.aggregates.push((alias, expr));
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
    /// Returns [`QueryError`] if any filter or having clause names an unknown field.
    pub fn compile(self) -> Result<AggregateQuery, QueryError> {
        let model = T::SCHEMA;
        let where_clause = resolve_pending(model, self.qs.pending)?;
        let order_by = self
            .order_by
            .into_iter()
            .map(|(col, desc)| OrderClause { column: col, desc })
            .collect();
        Ok(AggregateQuery {
            model,
            where_clause,
            group_by: self.group_by,
            aggregates: self.aggregates,
            having: self.having,
            order_by,
            limit: self.limit,
            offset: self.offset,
        })
    }
}
