//! Dialect-agnostic SQL writers.
//!
//! Every backend's `compile_*` method routes through these helpers.
//! Quoting, placeholders, NULL casts and operator translation all go
//! through the [`Dialect`] held by [`Sql`], so there is no per-backend
//! fork.
//!
//! Syntax with no portable form — `ILIKE`, `IS DISTINCT FROM`, the
//! JSONB operators — is gated on [`Dialect::supports_op`]. A dialect
//! that says `false` gets a clear
//! [`SqlError::OperatorNotSupportedInDialect`] instead of wrong SQL.

use std::fmt::Write as _;

use crate::core::{
    AggregateExpr, AggregateQuery, BulkInsertQuery, BulkUpdateQuery, CountQuery, DeleteQuery,
    Filter, InsertQuery, ModelSchema, Op, SearchClause, SelectQuery, SqlValue, UpdateQuery,
    WhereExpr, LIKE_ESCAPE_CLAUSE,
};

use super::{CompiledStatement, Dialect, SqlError};

/// The SQL buffer and bind list every writer appends to. It carries
/// the [`Dialect`], so helpers can ask for the right quoting,
/// placeholder and NULL cast without branching on the backend.
#[allow(clippy::struct_field_names)] // `sql.sql` reads naturally for builder calls
pub(super) struct Sql<'d> {
    pub d: &'d dyn Dialect,
    pub sql: String,
    pub params: Vec<SqlValue>,
    /// Open emission scopes, innermost last. A bare `Expr::Column`
    /// resolves against the top frame; `Expr::OuterRef` resolves
    /// against the one below it, the enclosing query.
    pub scope_stack: Vec<Scope>,
    /// `(scope depth, qualifier)` for bare columns: the one carrier for
    /// predicates, ORDER BY and `Expr::Column`. A nested query pushes a
    /// scope frame, so it never inherits it (#2411).
    pub qualifier: Option<(usize, Qualifier)>,
    /// Whether `Expr::Aggregate` is legal here. True only inside an
    /// aggregating query's projection, HAVING or ORDER BY, which are
    /// the places SQL accepts an aggregate call.
    pub aggregate_allowed: bool,
    /// Set while writing a grouped aggregate's HAVING and ORDER BY over
    /// a derived table. See [`DerivedJoins`].
    pub derived_joins: Option<DerivedJoins>,
    /// `(scope depth, alias, target)` of every model join in the open
    /// queries, so the writer can type an aliased column.
    pub join_types: Vec<(usize, &'static str, &'static ModelSchema)>,
    /// Count the next SELECT body writes as its `__rustango_total` column.
    pub total: Option<CountQuery>,
    /// Children the next SELECT body counts as `__annotated_count`.
    pub child_count: Option<ChildCount>,
    /// A MySQL `GROUP_CONCAT` was written: lift its 1024-byte cap.
    pub group_concat: bool,
    /// The UPDATE/DELETE target being written, if any.
    pub dml_target: Option<&'static str>,
    /// A FROM or JOIN item named [`Self::dml_target`] since the flag was last cleared.
    pub read_dml_target: bool,
}

/// The FROM name of a correlated subquery over its enclosing query's
/// table, so the inner table does not hide the outer row.
const SELF_ALIAS: &str = "__rustango_self";

/// One open query: its model and the name its FROM exposes.
#[derive(Clone, Copy)]
pub(super) struct Scope {
    model: &'static ModelSchema,
    /// `model.table`, or [`SELF_ALIAS`] for a correlated subquery over
    /// the enclosing query's table.
    name: &'static str,
}

impl Scope {
    /// A scope that is not correlated to the one it sits in: a top-level
    /// query, a derived table, a set-operation branch.
    fn plain(model: &'static ModelSchema) -> Self {
        Self {
            model,
            name: model.table,
        }
    }

    /// `<table>` or `<table> AS <alias>`, as a FROM item.
    fn write_from(self, b: &mut Sql<'_>) {
        b.write_table(self.model.table);
        if self.name != self.model.table {
            b.sql.push_str(" AS ");
            b.write_ident(self.name);
        }
    }

    /// An explicit `<table>.` reference to this scope's own table reads
    /// its FROM name.
    fn rename<'a>(self, alias: &'a str) -> &'a str {
        if alias == self.model.table {
            self.name
        } else {
            alias
        }
    }
}

/// How a bare column of the current scope is written.
#[derive(Clone, Copy)]
pub(super) enum Qualifier {
    /// Inside a JOIN `ON`: every bare column is the joined alias's.
    Join(&'static str),
    /// A SELECT with joins: the base model's columns get its table.
    Base(&'static str),
}

/// The joins a grouped aggregate's derived table hides. An aliased
/// column on one of them reads the derived `"<table>"."<alias>__<col>"`,
/// but only at `depth`: a nested query pushes a scope frame and sees
/// its own aliases.
pub(super) struct DerivedJoins {
    depth: usize,
    table: &'static str,
    aliases: Vec<&'static str>,
}

impl<'d> Sql<'d> {
    pub(super) fn new(d: &'d dyn Dialect) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::new(),
            scope_stack: Vec::new(),
            qualifier: None,
            aggregate_allowed: false,
            derived_joins: None,
            join_types: Vec::new(),
            total: None,
            child_count: None,
            group_concat: false,
            dml_target: None,
            read_dml_target: false,
        }
    }

    pub(super) fn with_capacity(d: &'d dyn Dialect, cap: usize) -> Self {
        Self {
            d,
            sql: String::new(),
            params: Vec::with_capacity(cap),
            scope_stack: Vec::new(),
            qualifier: None,
            aggregate_allowed: false,
            derived_joins: None,
            join_types: Vec::new(),
            total: None,
            child_count: None,
            group_concat: false,
            dml_target: None,
            read_dml_target: false,
        }
    }

    /// A scope correlated to the innermost open one: aliased when both
    /// read the same table.
    fn correlated_scope(&self, model: &'static ModelSchema) -> Scope {
        match self.scope_stack.last() {
            Some(outer) if outer.name == model.table => Scope {
                model,
                name: SELF_ALIAS,
            },
            _ => Scope::plain(model),
        }
    }

    /// The innermost open scope, or `model`'s plain one when none is.
    fn scope_or(&self, model: &'static ModelSchema) -> Scope {
        self.scope_stack
            .last()
            .copied()
            .unwrap_or_else(|| Scope::plain(model))
    }

    /// The model of the innermost open scope.
    fn scope_model(&self) -> Option<&'static ModelSchema> {
        self.scope_stack.last().map(|s| s.model)
    }

    /// The qualifier of the innermost open scope, if one is set.
    fn qualifier(&self) -> Option<Qualifier> {
        self.qualifier
            .filter(|(depth, _)| *depth == self.scope_stack.len())
            .map(|(_, q)| q)
    }

    /// The table or alias `column` is written under. `Base` only names
    /// the base model's own columns, so a joined-only one stays bare.
    fn qualify_alias(&self, column: &str) -> Option<std::borrow::Cow<'static, str>> {
        match self.qualifier()? {
            Qualifier::Join(a) => Some(self.alias_ref(a)),
            Qualifier::Base(t) => self
                .scope_model()
                .map_or(true, |m| m.field_by_column(column).is_some())
                .then_some(t.into()),
        }
    }

    /// A model join's alias as written: PG cuts names past 63 bytes, so
    /// two long chain aliases would collide.
    fn write_alias(&mut self, alias: &str) {
        self.write_ident(&crate::core::joins::sql_label(alias));
    }

    /// `alias` in an `<alias>.<col>` reference: shortened like
    /// [`Self::write_alias`] when it names a model join.
    fn alias_ref<'a>(&self, alias: &'a str) -> std::borrow::Cow<'a, str> {
        if self.join_types.iter().any(|(_, a, _)| *a == alias) {
            crate::core::joins::sql_label(alias)
        } else {
            alias.into()
        }
    }

    /// Run `f` with `q` as this scope's qualifier.
    fn with_qualifier<R>(&mut self, q: Option<Qualifier>, f: impl FnOnce(&mut Self) -> R) -> R {
        let depth = self.scope_stack.len();
        let prior = std::mem::replace(&mut self.qualifier, q.map(|q| (depth, q)));
        let r = f(self);
        self.qualifier = prior;
        r
    }

    /// Append a quoted identifier using the dialect's quoting rules.
    pub(super) fn write_ident(&mut self, name: &str) {
        self.sql.push_str(&self.d.quote_ident(name));
    }

    /// A column of the current scope, under its qualifier if one is set.
    fn write_scope_col(&mut self, col: &str) {
        let s = render_qualified_col(self, col);
        self.sql.push_str(&s);
    }

    /// [`Self::write_ident`] for a table a query reads: every FROM and JOIN
    /// item goes through here, so MySQL knows which subqueries hit 1093.
    fn write_table(&mut self, name: &str) {
        self.read_dml_target |= self.dml_target == Some(name);
        self.write_ident(name);
    }

    /// Add `value` to the bind list and write its placeholder.
    ///
    /// On Postgres it also writes a `::TYPE` cast (from
    /// [`Dialect::null_cast`]) in two cases: a `NULL`, which would
    /// otherwise have no type, and a [`SqlValue::RangeLiteral`], which
    /// binds as text and which PG will not cast to a range type on its
    /// own in `INSERT` or `UPDATE SET`. A [`BindCast::Always`] is written
    /// for every value.
    pub(super) fn push_param_typed(&mut self, value: SqlValue, cast: Option<BindCast>) {
        let untyped = matches!(value, SqlValue::Null | SqlValue::RangeLiteral(_));
        let p = self.bind(value);
        self.sql.push_str(&p);
        let ty = match cast {
            Some(BindCast::Always(ty)) => Some(ty),
            Some(BindCast::Untyped(ty)) if untyped => Some(ty),
            _ => None,
        };
        if let Some(ty) = ty {
            self.sql.push_str("::");
            self.sql.push_str(ty);
        }
    }

    /// Add `value` to the bind list and return its placeholder, which
    /// a JSON or Decimal value gets wrapped in by the dialect.
    pub(super) fn bind(&mut self, value: SqlValue) -> String {
        let wrap: Option<fn(&dyn Dialect, &mut String, &str)> = match value {
            SqlValue::Json(_) => Some(|d, s, p| d.write_json_param(s, p)),
            SqlValue::Decimal(_) => Some(|d, s, p| d.write_decimal_param(s, p)),
            _ => None,
        };
        self.params.push(value);
        let p = self.d.placeholder(self.params.len());
        let Some(wrap) = wrap else {
            return p;
        };
        let mut wrapped = String::new();
        wrap(self.d, &mut wrapped, &p);
        wrapped
    }

    /// [`Self::push_param_typed`] with no cast, for values whose
    /// column type the writer cannot work out.
    pub(super) fn push_param(&mut self, value: SqlValue) {
        self.push_param_typed(value, None);
    }

    /// # Errors
    /// [`SqlError::ValueNotSupportedInDialect`] for a bind whose bind arm
    /// can only panic, as [`super::refuse_unbindable_values`] decides.
    pub(super) fn finish(mut self) -> Result<CompiledStatement, SqlError> {
        super::refuse_unbindable_values(self.d, &self.params)?;
        if self.group_concat {
            // A statement-level hint: it covers the GROUP_CONCAT in any block.
            let start = self.sql.len() - self.sql.trim_start_matches('(').len();
            let kw = ["SELECT", "INSERT", "UPDATE", "DELETE"]
                .into_iter()
                .find(|kw| self.sql[start..].starts_with(kw));
            if let Some(kw) = kw {
                self.sql.insert_str(
                    start + kw.len(),
                    " /*+ SET_VAR(group_concat_max_len = 4294967295) */",
                );
            }
        }
        Ok(CompiledStatement {
            sql: self.sql,
            params: self.params,
        })
    }
}

/// The `::TYPE` a bind for a column gets on Postgres.
#[derive(Clone, Copy)]
pub(super) enum BindCast {
    /// Only a `NULL` or range literal, which binds without a usable type.
    Untyped(&'static str),
    /// Every value: a text bind would pick `text = text`, not `citext = citext`.
    Always(&'static str),
}

/// Look up a column's bind cast. Postgres needs one; other dialects
/// get `None`.
pub(super) fn null_cast_for(
    d: &dyn Dialect,
    model: &ModelSchema,
    column: &str,
) -> Option<BindCast> {
    let field = model.field_by_column(column)?;
    if field.case_insensitive && field.ty == crate::core::FieldType::String {
        if let Some(ty) = d.ci_text_bind_cast() {
            return Some(BindCast::Always(ty));
        }
    }
    d.null_cast(field.ty).map(BindCast::Untyped)
}

// ---- SELECT ----

/// A SELECT that may correlate to the enclosing query through `OuterRef`.
pub(super) fn write_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    let scope = b.correlated_scope(query.model);
    write_select_as(b, query, scope)
}

/// A derived-table SELECT, which cannot see the query it feeds.
fn write_derived_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    write_select_as(b, query, Scope::plain(query.model))
}

fn write_select_as(b: &mut Sql<'_>, query: &SelectQuery, scope: Scope) -> Result<(), SqlError> {
    // Only the outermost SELECT may widen its select list (#2567).
    let top_level = b.scope_stack.is_empty();
    b.scope_stack.push(scope);
    let result = if query.compound.is_empty() {
        write_select_inner(b, query, top_level)
    } else {
        write_compound_select(b, query)
    };
    b.scope_stack.pop();
    result
}

/// A compound SELECT plus a `__rustango_total` column. The set operation
/// becomes a derived table, so the count covers every branch before LIMIT.
pub(super) fn write_compound_with_total(
    b: &mut Sql<'_>,
    query: &SelectQuery,
) -> Result<(), SqlError> {
    let merged = SelectQuery {
        compound_order_by: Vec::new(),
        compound_limit: None,
        compound_offset: None,
        lock_mode: None,
        ..query.clone()
    };
    b.sql.push_str("SELECT ");
    b.write_ident("__rustango_page");
    b.sql.push_str(".*, COUNT(*) OVER () AS ");
    b.write_ident("__rustango_total");
    b.sql.push_str(" FROM (");
    write_select(b, &merged)?;
    b.sql.push_str(") AS ");
    b.write_ident("__rustango_page");
    write_order_limit_offset(
        b,
        &query.compound_order_by,
        query.compound_limit,
        query.compound_offset,
    )
}

/// A DISTINCT SELECT plus a `__rustango_total` column. `COUNT(*) OVER ()`
/// runs before DISTINCT, so the total is a counting subquery instead (#1966).
pub(super) fn write_distinct_with_total(
    b: &mut Sql<'_>,
    query: &SelectQuery,
) -> Result<(), SqlError> {
    b.total = Some(CountQuery::from_select(SelectQuery {
        limit: None,
        offset: None,
        lock_mode: None,
        ..query.clone()
    }));
    write_select(b, query)
}

/// Emit a compound SELECT (`UNION`, `INTERSECT`, `EXCEPT`):
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
/// A branch with its own `ORDER BY` or `LIMIT` is wrapped in parens,
/// so those clauses stay inside that branch. The outer query's
/// `compound_*` clauses and `lock_mode` come after the last branch
/// and apply to the merged result.
///
/// The outer query is itself the first branch, so its `where_clause`,
/// `joins` and `search` apply only to that branch.
fn write_compound_select(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    // The first branch is the outer query itself. Its order_by /
    // limit / offset were set before the first `.union()`, so they
    // belong to this branch; the merged-result versions live in
    // `compound_order_by` and friends and emit after the last branch.
    // The copy clears `compound` and `lock_mode`, which belong to the
    // whole statement.
    //
    // A branch that has its own ORDER BY or LIMIT is wrapped as
    // `SELECT * FROM (<branch>) AS __rustango_bN` to keep those
    // clauses local. The alias is required: MySQL rejects a derived
    // table without one, and SQLite's grammar forbids bare parens
    // around a select-core. The head takes `__rustango_b0`.
    // No `..`: a new SelectQuery field must say which side it is on.
    let SelectQuery {
        model,
        where_clause,
        search,
        joins,
        subquery_joins,
        order_by,
        limit,
        offset,
        lock_mode: _,
        compound: _,
        projection,
        distinct,
        compound_order_by: _,
        compound_limit: _,
        compound_offset: _,
        select_related: _,
    } = query;
    let head = SelectQuery {
        where_clause: where_clause.clone(),
        search: search.clone(),
        joins: joins.clone(),
        subquery_joins: subquery_joins.clone(),
        order_by: order_by.clone(),
        limit: *limit,
        offset: *offset,
        projection: projection.clone(),
        distinct: distinct.clone(),
        ..SelectQuery::new(model)
    };
    let head_scoped = !head.order_by.is_empty() || head.limit.is_some() || head.offset.is_some();
    if head_scoped {
        b.sql.push_str("SELECT * FROM (");
        write_select_inner(b, &head, false)?;
        b.sql.push(')');
        b.sql.push_str(" AS ");
        b.write_ident("__rustango_b0");
    } else {
        write_select_inner(b, &head, false)?;
    }

    for (i, branch) in query.compound.iter().enumerate() {
        b.sql.push(' ');
        b.sql.push_str(branch.op.keyword());
        b.sql.push(' ');
        // A branch is the head's sibling: its frame replaces the head's,
        // so its own `OuterRef` reaches the enclosing query.
        let head_frame = b.scope_stack.pop();
        let frame = b.correlated_scope(branch.query.model);
        b.scope_stack.push(frame);
        // Plain branches emit inline; `SELECT … UNION SELECT …` is
        // valid everywhere and needs no alias.
        let scoped = !branch.query.order_by.is_empty()
            || branch.query.limit.is_some()
            || branch.query.offset.is_some();
        let r = if branch.query.compound.is_empty() {
            if scoped {
                b.sql.push_str("SELECT * FROM (");
                let r = write_select_inner(b, &branch.query, false);
                b.sql.push(')');
                if r.is_ok() {
                    b.sql.push_str(" AS ");
                    b.write_ident(&format!("__rustango_b{}", i + 1));
                }
                r
            } else {
                write_select_inner(b, &branch.query, false)
            }
        } else {
            // Nested compound: recurse, wrapped the same way.
            b.sql.push_str("SELECT * FROM (");
            let r = write_compound_select(b, &branch.query);
            b.sql.push(')');
            if r.is_ok() {
                b.sql.push_str(" AS ");
                b.write_ident(&format!("__rustango_b{}", i + 1));
            }
            r
        };
        b.scope_stack.pop();
        b.scope_stack.extend(head_frame);
        r?;
    }

    // Clauses for the merged result. No qualifier: the "table" here
    // is the merged rows, not a join target.
    write_order_limit_offset(
        b,
        &query.compound_order_by,
        query.compound_limit,
        query.compound_offset,
    )?;

    if let Some(lock) = &query.lock_mode {
        write_lock_clause(b, lock);
    }

    Ok(())
}

/// Write a model column for the distinct-on fallback's inner SELECT.
/// With joins present it is qualified, so it cannot clash with a
/// joined column of the same name. The outer SELECT reads from the
/// derived table, so it keeps the bare name.
fn write_distinct_inner_col(b: &mut Sql<'_>, table: &str, col: &str, qualify: bool) {
    if qualify {
        b.write_ident(table);
        b.sql.push('.');
        b.write_ident(col);
    } else {
        b.write_ident(col);
    }
}

/// Portable stand-in for PG's `SELECT DISTINCT ON (cols)` on MySQL
/// and SQLite. It ranks rows per partition with `ROW_NUMBER()` and
/// keeps the first of each. The caller's `ORDER BY` decides both
/// which row wins and the final row order.
///
/// Layout:
/// ```text
/// SELECT <orig_cols>, <join_cols> FROM (
///   SELECT <orig_cols>, <join_cols>, <order> AS __rustango_order<i>,
///          ROW_NUMBER() OVER (PARTITION BY <cols> ORDER BY <order>) AS __rn
///   FROM <table> [JOIN ...] WHERE <where>
/// ) sub
/// WHERE __rn = 1
/// [ORDER BY __rustango_order<i>] [LIMIT N] [OFFSET M]
/// ```
fn write_distinct_on_via_window(
    b: &mut Sql<'_>,
    query: &SelectQuery,
    distinct_cols: &[&'static str],
) -> Result<(), SqlError> {
    // The inner SELECT does the joins, so it qualifies the model's own
    // columns to avoid a name clash with a joined one. The derived
    // table `sub` then exposes them under their bare names.
    let qualify = !query.joins.is_empty() || !query.subquery_joins.is_empty();
    let scope = b.scope_or(query.model);

    // Outer SELECT: projection columns only, no __rn.
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
    for join in &query.joins {
        for col in &join.project {
            b.sql.push_str(", ");
            b.write_ident(&crate::core::joins::joined_label(join.alias, col));
        }
    }
    // Counted over the survivors, which `sub` exposes.
    if let Some(child) = b.child_count.take() {
        write_child_count(b, "sub", child);
    }
    b.sql.push_str(" FROM (");

    // Inner SELECT: the same projection plus __rn.
    b.sql.push_str("SELECT ");
    let mut inner_first = true;
    if let Some(cols) = query.projection.as_ref() {
        for col in cols {
            if !inner_first {
                b.sql.push_str(", ");
            }
            inner_first = false;
            write_distinct_inner_col(b, scope.name, col, qualify);
        }
    } else {
        for field in query.model.scalar_fields() {
            if !inner_first {
                b.sql.push_str(", ");
            }
            inner_first = false;
            write_distinct_inner_col(b, scope.name, field.column, qualify);
        }
    }
    for join in &query.joins {
        for col in &join.project {
            b.sql.push_str(", ");
            b.write_alias(join.alias);
            b.sql.push('.');
            b.write_ident(col);
            b.sql.push_str(" AS ");
            b.write_ident(&crate::core::joins::joined_label(join.alias, col));
        }
    }
    // Each sort key, so the outer ORDER BY can read relation and unprojected ones.
    for (i, item) in query.order_by.iter().enumerate() {
        if !matches!(item, crate::core::OrderItem::Random) {
            b.sql.push_str(", ");
            write_order_target(b, item)?;
            b.sql.push_str(" AS ");
            b.write_ident(&distinct_on_order_label(i));
        }
    }
    // ROW_NUMBER() OVER (PARTITION BY ... ORDER BY ...) AS __rn
    b.sql.push_str(", ROW_NUMBER() OVER (PARTITION BY ");
    for (i, col) in distinct_cols.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        write_distinct_inner_col(b, scope.name, col, qualify);
    }
    if !query.order_by.is_empty() {
        b.sql.push_str(" ORDER BY ");
        for (i, item) in query.order_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            match item {
                crate::core::OrderItem::Column { column, desc, .. } => {
                    write_distinct_inner_col(b, scope.name, column, qualify);
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
    scope.write_from(b);
    // The joins sit in the inner SELECT next to the ROW_NUMBER()
    // partition, so they shape the rows the window ranks.
    write_model_joins(b, &query.joins)?;
    write_subquery_joins(b, &query.subquery_joins)?;
    write_where_with_search(
        b,
        &query.where_clause,
        query.search.as_ref(),
        Some(query.model),
    )?;

    b.sql.push_str(") sub WHERE sub.__rn = 1");

    // Outer ORDER BY / LIMIT / OFFSET — applied to the survivors, by the
    // sort keys `sub` exposes.
    b.with_qualifier(None, |b| {
        write_order_limit_offset_with(
            b,
            &query.order_by,
            query.limit,
            query.offset,
            |b, i, item| {
                if matches!(item, crate::core::OrderItem::Random) {
                    return write_order_target(b, item);
                }
                b.write_ident(&distinct_on_order_label(i));
                Ok(())
            },
        )
    })?;

    Ok(())
}

fn distinct_on_order_label(i: usize) -> String {
    format!("__rustango_order{i}")
}

/// Derived-table joins: `JOIN [LATERAL] (<subquery>) AS alias ON …`.
/// They project no columns; they only filter or correlate.
fn write_subquery_joins(
    b: &mut Sql<'_>,
    subquery_joins: &[crate::core::SubqueryJoin],
) -> Result<(), SqlError> {
    for sj in subquery_joins {
        use crate::core::JoinKind;
        let kind_kw = match sj.kind {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            // The builders only produce Inner / Left here.
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
        // The subquery writers push their own scope frame, so an
        // `OuterRef` inside a LATERAL subquery resolves to the
        // enclosing query.
        // Only a LATERAL subquery can see the query it joins.
        match (&sj.subquery, sj.lateral) {
            (crate::core::DerivedSource::Select(s), true) => write_select(b, s)?,
            (crate::core::DerivedSource::Select(s), false) => write_derived_select(b, s)?,
            (crate::core::DerivedSource::Aggregate(a), true) => write_aggregate(b, a)?,
            (crate::core::DerivedSource::Aggregate(a), false) => {
                write_aggregate_as(b, a, Scope::plain(a.model))?;
            }
        }
        b.sql.push_str(") AS ");
        b.write_ident(sj.alias);
        // An empty `on` gives `ON true`, the LATERAL shape where the
        // correlation lives in the subquery's WHERE. Otherwise the
        // caller's predicate, with bare columns bound to the alias.
        if sj.on.is_empty() {
            b.sql.push_str(" ON true");
        } else {
            b.sql.push_str(" ON ");
            let alias = sj.alias;
            b.with_qualifier(Some(Qualifier::Join(alias)), |b| {
                write_where_expr(b, &sj.on, None)
            })?;
        }
    }
    Ok(())
}

/// `order_helpers`: a lone top-level SELECT, not a subquery or set-operation
/// branch, so it may add DISTINCT's ORDER BY helper columns.
fn write_select_inner(
    b: &mut Sql<'_>,
    query: &SelectQuery,
    order_helpers: bool,
) -> Result<(), SqlError> {
    with_join_types(b, &query.joins, None, |b| {
        write_select_body(b, query, order_helpers)
    })
}

/// Run `f` with `joins` (and a derived `source`'s joins) typed at the
/// current scope depth.
fn with_join_types(
    b: &mut Sql<'_>,
    joins: &[crate::core::Join],
    source: Option<&SelectQuery>,
    f: impl FnOnce(&mut Sql<'_>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mark = b.join_types.len();
    let depth = b.scope_stack.len();
    let src = source.map_or(&[][..], |s| &s.joins[..]);
    for j in joins.iter().chain(src) {
        b.join_types.push((depth, j.alias, j.target));
    }
    let r = f(b);
    b.join_types.truncate(mark);
    r
}

fn write_select_body(
    b: &mut Sql<'_>,
    query: &SelectQuery,
    order_helpers: bool,
) -> Result<(), SqlError> {
    let qualify = !query.joins.is_empty() || !query.subquery_joins.is_empty();
    let q = qualify.then_some(Qualifier::Base(b.scope_or(query.model).name));
    b.with_qualifier(q, |b| {
        write_select_body_qualified(b, query, qualify, order_helpers)
    })
}

fn write_select_body_qualified(
    b: &mut Sql<'_>,
    query: &SelectQuery,
    qualify: bool,
    order_helpers: bool,
) -> Result<(), SqlError> {
    // `.distinct_on(cols)` is native on PG. Elsewhere the window
    // fallback emits the whole statement, so return early.
    if let Some(crate::core::DistinctMode::On(cols)) = &query.distinct {
        if b.d.name() != "postgres" {
            return write_distinct_on_via_window(b, query, cols);
        }
    }

    let scope = b.scope_or(query.model);
    b.sql.push_str("SELECT ");
    // Plain `DISTINCT` works on every dialect; `DISTINCT ON` is PG
    // only, and the other dialects already returned above.
    if let Some(distinct) = &query.distinct {
        match distinct {
            crate::core::DistinctMode::All => b.sql.push_str("DISTINCT "),
            crate::core::DistinctMode::On(cols) => {
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
        // A `.values()`-style projection: emit exactly these columns,
        // in this order. The builder already checked they resolve.
        for col in cols {
            if !first_col {
                b.sql.push_str(", ");
            }
            first_col = false;
            if qualify {
                b.write_ident(scope.name);
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
                b.write_ident(scope.name);
                b.sql.push('.');
            }
            b.write_ident(field.column);
        }
    }
    for join in &query.joins {
        for col in &join.project {
            b.sql.push_str(", ");
            b.write_alias(join.alias);
            b.sql.push('.');
            b.write_ident(col);
            b.sql.push_str(" AS ");
            b.write_ident(&crate::core::joins::joined_label(join.alias, col));
        }
    }
    let needs_helpers = query.distinct.is_some() && distinct_order_helpers(query).next().is_some();
    if order_helpers && needs_helpers {
        write_distinct_order_helpers(b, query)?;
    }
    // Written before FROM, so the WHERE binds appear twice, in text order.
    if let Some(count) = b.total.take() {
        b.sql.push_str(", (");
        write_count(b, &count)?;
        b.sql.push_str(") AS ");
        b.write_ident("__rustango_total");
    }
    if let Some(child) = b.child_count.take() {
        write_child_count(b, scope.name, child);
    }

    b.sql.push_str(" FROM ");
    scope.write_from(b);

    write_model_joins(b, &query.joins)?;

    write_subquery_joins(b, &query.subquery_joins)?;

    write_where_with_search(
        b,
        &query.where_clause,
        query.search.as_ref(),
        Some(query.model),
    )?;

    // A nested DISTINCT can't carry the helpers; with no LIMIT/OFFSET its
    // ORDER BY changes nothing, so drop it rather than fail on PG/MySQL.
    let order_by: &[crate::core::OrderItem] =
        if needs_helpers && !order_helpers && query.limit.is_none() && query.offset.is_none() {
            &[]
        } else {
            &query.order_by
        };
    write_order_limit_offset(b, order_by, query.limit, query.offset)?;

    if let Some(lock) = &query.lock_mode {
        write_lock_clause(b, lock);
    }

    Ok(())
}

/// PG and MySQL want a DISTINCT query's ORDER BY terms in the select
/// list. Each relation term no join projects is added last, so value
/// rows can drop it (see `values_width`).
fn write_distinct_order_helpers(b: &mut Sql<'_>, query: &SelectQuery) -> Result<(), SqlError> {
    for (i, expr) in distinct_order_helpers(query) {
        b.sql.push_str(", ");
        write_expr(b, expr, None)?;
        b.sql.push_str(" AS ");
        b.write_ident(&format!("__rustango_order{i}"));
    }
    Ok(())
}

/// The relation ORDER BY terms no join projects, with their position.
fn distinct_order_helpers(
    query: &SelectQuery,
) -> impl Iterator<Item = (usize, &crate::core::Expr)> + '_ {
    use crate::core::{Expr, OrderItem};
    query
        .order_by
        .iter()
        .enumerate()
        .filter_map(|(i, item)| match item {
            OrderItem::Expr {
                expr: expr @ Expr::AliasedColumn { alias, column },
                ..
            } => {
                let projected = query
                    .joins
                    .iter()
                    .any(|j| j.alias == *alias && j.project.contains(column));
                (!projected).then_some((i, expr))
            }
            _ => None,
        })
}

/// Emit `FOR UPDATE [NO KEY] [OF t1, t2] [SKIP LOCKED | NOWAIT]` for
/// `select_for_update(...)`.
///
/// Postgres supports all of it. MySQL 8.0.1+ has everything but
/// `NO KEY`, which falls back to the stricter plain `FOR UPDATE`.
/// SQLite has no row locks at all, so the clause is dropped there;
/// a transaction locks the whole database instead.
///
/// `SKIP LOCKED` wins over `NOWAIT` when both are set. They cannot
/// both appear in one statement.
fn write_lock_clause(b: &mut Sql<'_>, lock: &crate::core::LockMode) {
    if b.d.name() == "sqlite" {
        // Warn so someone debugging concurrency against a SQLite
        // fixture sees that the clause did nothing. Apps that rely on
        // SQLite's single-writer lock can silence it.
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

// ---- COUNT ----

pub(super) fn write_count(b: &mut Sql<'_>, query: &CountQuery) -> Result<(), SqlError> {
    let scope = Scope::plain(query.model);
    b.scope_stack.push(scope);
    let r = (|| {
        b.sql.push_str("SELECT COUNT(*)");
        write_from_source(b, scope, query.source.as_deref())?;
        write_where_with_search(
            b,
            &query.where_clause,
            query.search.as_ref(),
            Some(query.model),
        )?;
        Ok(())
    })();
    b.scope_stack.pop();
    r
}

/// ` FROM "<table>"`, or ` FROM (<source>) AS "<table>"` so the outer
/// clauses resolve against the derived rows unchanged.
fn write_from_source(
    b: &mut Sql<'_>,
    scope: Scope,
    source: Option<&SelectQuery>,
) -> Result<(), SqlError> {
    b.sql.push_str(" FROM ");
    if let Some(sub) = source {
        b.sql.push('(');
        write_derived_select(b, sub)?;
        b.sql.push_str(") AS ");
        b.write_ident(scope.name);
    } else {
        scope.write_from(b);
    }
    Ok(())
}

// ---- AGGREGATE ----

/// Emit the `KIND JOIN <target> AS <alias> ON <pred>` clauses shared
/// by the SELECT and aggregate writers.
fn write_model_joins(b: &mut Sql<'_>, joins: &[crate::core::Join]) -> Result<(), SqlError> {
    use crate::core::JoinKind;
    for join in joins {
        // Reject a join kind the dialect lacks before writing
        // anything, so the user gets a clear error instead of a
        // driver parse failure. PG has all four; MySQL has no FULL;
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
        b.write_table(join.target.table);
        b.sql.push_str(" AS ");
        b.write_alias(join.alias);
        b.sql.push_str(" ON ");
        // Bare columns in the ON predicate resolve to the joined
        // alias here. Use `Expr::AliasedColumn` to point elsewhere.
        b.with_qualifier(Some(Qualifier::Join(join.alias)), |b| {
            write_where_expr(b, &join.on, Some(join.target))
        })?;
    }
    Ok(())
}

/// An aggregate that may correlate to the enclosing query through `OuterRef`.
pub(super) fn write_aggregate(b: &mut Sql<'_>, query: &AggregateQuery) -> Result<(), SqlError> {
    let scope = b.correlated_scope(query.model);
    write_aggregate_as(b, query, scope)
}

fn write_aggregate_as(
    b: &mut Sql<'_>,
    query: &AggregateQuery,
    scope: Scope,
) -> Result<(), SqlError> {
    // The scope frame gives a HAVING predicate's aggregates a model
    // to resolve their COALESCE-default cast against.
    b.scope_stack.push(scope);
    let r = with_join_types(b, &query.joins, query.source.as_deref(), |b| {
        write_aggregate_inner(b, query, scope)
    });
    b.scope_stack.pop();
    r
}

/// Emit one group-by or projection column for the aggregate writer.
/// A dotted `alias.col` becomes `"alias"."col"`. A bare `col` is
/// qualified with the model table when there are joins, to keep it
/// apart from a joined column, and left bare otherwise. With
/// `project`, it also gets a stable `AS` label — `alias__col` for a
/// dotted ref, the bare name otherwise — so dict-row keys are
/// predictable.
fn write_agg_group_col(
    b: &mut Sql<'_>,
    col: &str,
    scope: Scope,
    has_joins: bool,
    derived: bool,
    project: bool,
) {
    let model_table = scope.name;
    if let Some((alias, c)) = col.split_once('.') {
        let flat = crate::core::joins::joined_label(alias, c);
        // A derived source already projects the joined column as `alias__col`.
        if derived && alias != scope.model.table {
            b.write_ident(model_table);
            b.sql.push('.');
            b.write_ident(&flat);
        } else {
            let alias = b.alias_ref(scope.rename(alias)).into_owned();
            b.write_ident(&alias);
            b.sql.push('.');
            b.write_ident(c);
        }
        if project {
            b.sql.push_str(" AS ");
            b.write_ident(&flat);
        }
    } else if has_joins {
        b.write_ident(model_table);
        b.sql.push('.');
        b.write_ident(col);
        if project {
            b.sql.push_str(" AS ");
            b.write_ident(col);
        }
    } else {
        b.write_ident(col);
    }
}

fn write_aggregate_inner(
    b: &mut Sql<'_>,
    query: &AggregateQuery,
    scope: Scope,
) -> Result<(), SqlError> {
    b.sql.push_str("SELECT ");
    let has_joins = !query.joins.is_empty();
    let derived = query.source.is_some();

    for (i, col) in query.group_by.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        write_agg_group_col(b, col, scope, has_joins, derived, /*project=*/ true);
    }
    // A joined table may share a column name, so base columns name their table.
    let q = has_joins.then_some(Qualifier::Base(scope.name));
    b.with_qualifier(q, |b| {
        for (i, (alias, expr)) in query.aggregates.iter().enumerate() {
            if !query.group_by.is_empty() || i > 0 {
                b.sql.push_str(", ");
            }
            write_aggregate_expr(b, expr, query.model)?;
            b.sql.push_str(" AS ");
            b.write_ident(alias.as_ref());
        }
        Ok::<_, SqlError>(())
    })?;

    write_from_source(b, scope, query.source.as_deref())?;
    write_model_joins(b, &query.joins)?;
    b.with_qualifier(q, |b| {
        write_where(b, &query.where_clause, Some(query.model))
    })?;

    if !query.group_by.is_empty() {
        b.sql.push_str(" GROUP BY ");
        for (i, col) in query.group_by.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(", ");
            }
            write_agg_group_col(b, col, scope, has_joins, derived, /*project=*/ false);
        }
    }

    // HAVING and ORDER BY see the derived table, not its joins.
    let prev_derived = b.derived_joins.take();
    if let Some(src) = query.source.as_deref() {
        b.derived_joins = Some(DerivedJoins {
            depth: b.scope_stack.len(),
            table: scope.name,
            aliases: src.joins.iter().map(|j| j.alias).collect(),
        });
    }
    let r = b.with_qualifier(q, |b| write_aggregate_tail(b, query));
    b.derived_joins = prev_derived;
    r
}

/// The HAVING, ORDER BY and LIMIT of a grouped aggregate.
fn write_aggregate_tail(b: &mut Sql<'_>, query: &AggregateQuery) -> Result<(), SqlError> {
    if let Some(having) = &query.having {
        b.sql.push_str(" HAVING ");
        // Aggregates are legal in HAVING. Restore afterwards so
        // nested subqueries do not inherit the permission.
        let prev = b.aggregate_allowed;
        b.aggregate_allowed = true;
        let r = write_where_expr(b, having, Some(query.model));
        b.aggregate_allowed = prev;
        r?;
    }

    // An aggregating query may `ORDER BY COUNT(*) DESC`; a plain
    // SELECT may not.
    let prev = b.aggregate_allowed;
    b.aggregate_allowed = true;
    let r = write_order_limit_offset(b, &query.order_by, query.limit, query.offset);
    b.aggregate_allowed = prev;
    r
}

/// The cast an aggregate needs so the decoder can read it. Databases
/// widen `SUM` and `AVG` to NUMERIC or DECIMAL; the writer casts them
/// back to `i64` or `f64`, except a decimal column's `SUM`, which stays exact.
#[derive(Debug, Clone, Copy)]
enum AggCast {
    Int,
    Float,
}

/// The cast a flat aggregate needs, or `None` when the decoder
/// already handles its type. Count, Max and Min return i64
/// everywhere. `SUM` follows its column, read from the current scope.
fn aggregate_cast_kind(b: &Sql<'_>, expr: &AggregateExpr) -> Option<AggCast> {
    match expr {
        AggregateExpr::Sum(col) => sum_cast(
            b.scope_model()
                .and_then(|m| m.field_by_column(col))
                .map(|f| f.ty),
        ),
        AggregateExpr::Avg(_)
        | AggregateExpr::StdDev(_)
        | AggregateExpr::StdDevPop(_)
        | AggregateExpr::Variance(_)
        | AggregateExpr::VariancePop(_) => Some(AggCast::Float),
        _ => None,
    }
}

/// The cast a `SUM` over a column of type `ty` needs.
fn sum_cast(ty: Option<crate::core::FieldType>) -> Option<AggCast> {
    use crate::core::FieldType;
    match ty {
        Some(FieldType::F32 | FieldType::F64) => Some(AggCast::Float),
        // Exact NUMERIC / DECIMAL, which the decoders read as-is.
        Some(FieldType::Decimal) => None,
        _ => Some(AggCast::Int),
    }
}

/// Wrap an already-written aggregate call in the dialect's cast:
/// `<expr>::bigint` on PG, `CAST(… AS SIGNED)` on MySQL, and
/// `CAST(… AS INTEGER)` on SQLite, or the float equivalents.
fn apply_agg_cast(d: &dyn Dialect, kind: AggCast, inner: &str) -> String {
    match kind {
        AggCast::Int => d.cast_aggregate_to_int(inner),
        AggCast::Float => d.cast_aggregate_to_float(inner),
    }
}

/// Render an aggregate's inner `ORDER BY`, e.g. `" ORDER BY "name"
/// DESC"`, or `""` when there is none. Every dialect spells it the
/// same way; only its position in the call differs.
fn render_agg_order_by(b: &Sql<'_>, order_by: &[crate::core::OrderClause]) -> String {
    if order_by.is_empty() {
        return String::new();
    }
    let mut s = String::from(" ORDER BY ");
    for (i, o) in order_by.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&render_qualified_col(b, o.column));
        if o.desc {
            s.push_str(" DESC");
        }
    }
    s
}

/// Build the bare aggregate call for a flat variant, with no cast.
/// It returns the string instead of writing it, so the caller can put
/// it inside a `FILTER` clause and then apply [`apply_agg_cast`].
fn format_bare_aggregate(b: &Sql<'_>, expr: &AggregateExpr) -> Result<String, SqlError> {
    let col_sql = |col: &str| render_qualified_col(b, col);
    Ok(match expr {
        AggregateExpr::Count(None) => "COUNT(*)".into(),
        AggregateExpr::Count(Some(col)) => format!("COUNT({})", col_sql(col)),
        AggregateExpr::CountDistinct(col) => {
            format!("COUNT(DISTINCT {})", col_sql(col))
        }
        AggregateExpr::Sum(col) => format!("SUM({})", col_sql(col)),
        AggregateExpr::Avg(col) => format!("AVG({})", col_sql(col)),
        AggregateExpr::Max(col) => format!("MAX({})", col_sql(col)),
        AggregateExpr::Min(col) => format!("MIN({})", col_sql(col)),
        AggregateExpr::AnyValue(col) => {
            // PG 16+ and MySQL have `any_value()`. SQLite does not,
            // so use `min()`: any value satisfies the contract.
            let ident = col_sql(col);
            match b.d.name() {
                "mysql" => format!("ANY_VALUE({ident})"),
                "sqlite" => format!("min({ident})"),
                _ => format!("any_value({ident})"),
            }
        }
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
            format!("{}({})", stddev_variance_name(expr), col_sql(col))
        }
        // The arms below are not flat aggregates. Each has its own
        // arm in `write_aggregate_expr`, and reaching this helper
        // means one was wrapped in `Filtered` or `Coalesced`, which
        // is not supported.
        AggregateExpr::Filtered { .. } | AggregateExpr::Coalesced { .. } => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "wrapper at format_bare_aggregate site",
            });
        }
        AggregateExpr::Window(_) => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Window at format_bare_aggregate site",
            });
        }
        AggregateExpr::ArrayAgg { .. }
        | AggregateExpr::StringAgg { .. }
        | AggregateExpr::JsonbAgg { .. } => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "PG-aggregate at format_bare_aggregate site",
            });
        }
        AggregateExpr::RelatedAggregate(_) => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "RelatedAggregate at format_bare_aggregate site",
            });
        }
    })
}

/// Emit one aggregate expression, recursing through the `Filtered`
/// and `Coalesced` wrappers.
///
/// PG and SQLite 3.30+ get a native `FILTER (WHERE …)`; MySQL gets
/// `<agg>(CASE WHEN … THEN <arg> END)` instead. `Coalesced` is
/// `COALESCE(<inner>, <default>)` on every dialect.
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
            // FILTER on a window function is not supported. PG allows
            // it for aggregate-window functions but not ranking ones,
            // and the writer does not know the per-function rule.
            if matches!(inner.as_ref(), AggregateExpr::Window(_)) {
                return Err(SqlError::NestedAggregateWrapper {
                    wrapper: "Filtered(Window)",
                });
            }
            // MySQL has no FILTER keyword; use CASE WHEN.
            if b.d.name() == "mysql" {
                return write_aggregate_as_case_when(b, inner, filter);
            }
            // PG and SQLite 3.30+ have FILTER. The cast goes around
            // the whole `(<bare> FILTER (…))`: on PG,
            // `SUM(x)::bigint FILTER (…)` is a parse error.
            let bare = format_bare_aggregate(b, inner)?;
            let prior = b.sql.len();
            b.sql.push_str(&bare);
            b.sql.push_str(" FILTER (WHERE ");
            write_where_expr(b, filter, Some(model))?;
            b.sql.push(')');
            if let Some(kind) = aggregate_cast_kind(b, inner) {
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
            b.write_scope_col(column);
            b.sql.push(')');
            Ok(())
        }
        AggregateExpr::StringAgg {
            column,
            delimiter,
            distinct,
            order_by,
        } => {
            // A DISTINCT aggregate may only ORDER BY the aggregated
            // column. PG requires this; we apply it everywhere.
            if *distinct && order_by.iter().any(|o| o.column != *column) {
                return Err(SqlError::AggregateNotSupportedInDialect {
                    aggregate: "string_agg(DISTINCT) ORDER BY a non-aggregated column",
                    dialect: b.d.name(),
                });
            }
            let order_sql = render_agg_order_by(b, order_by);
            // `string_agg` on PG, `GROUP_CONCAT` on MySQL,
            // `group_concat` on SQLite.
            match b.d.name() {
                "mysql" => {
                    // `SEPARATOR` takes no bound parameter, so the
                    // delimiter is inlined as a quoted literal.
                    // ORDER BY goes before SEPARATOR.
                    b.group_concat = true;
                    b.sql.push_str("GROUP_CONCAT(");
                    if *distinct {
                        b.sql.push_str("DISTINCT ");
                    }
                    b.write_scope_col(column);
                    b.sql.push_str(&order_sql);
                    b.sql.push_str(" SEPARATOR ");
                    b.sql.push_str(&b.d.quote_literal(delimiter));
                    b.sql.push(')');
                }
                "sqlite" => {
                    // A SQLite DISTINCT aggregate takes exactly one
                    // argument, so DISTINCT works only with the
                    // default ',' separator. ORDER BY inside the
                    // aggregate needs SQLite 3.44+.
                    if *distinct {
                        if delimiter.as_str() != "," {
                            return Err(SqlError::AggregateNotSupportedInDialect {
                                aggregate: "string_agg(DISTINCT) with a custom delimiter (SQLite group_concat DISTINCT takes one arg)",
                                dialect: b.d.name(),
                            });
                        }
                        b.sql.push_str("group_concat(DISTINCT ");
                        b.write_scope_col(column);
                        b.sql.push_str(&order_sql);
                        b.sql.push(')');
                    } else {
                        b.sql.push_str("group_concat(");
                        b.write_scope_col(column);
                        b.sql.push_str(", ");
                        b.push_param(crate::core::SqlValue::String(delimiter.clone()));
                        b.sql.push_str(&order_sql);
                        b.sql.push(')');
                    }
                }
                // Postgres (+ any future dialect): the standard form. ORDER
                // BY goes after the delimiter, inside the parens.
                _ => {
                    b.sql.push_str("string_agg(");
                    if *distinct {
                        b.sql.push_str("DISTINCT ");
                    }
                    b.write_scope_col(column);
                    b.sql.push_str(", ");
                    b.push_param(crate::core::SqlValue::String(delimiter.clone()));
                    b.sql.push_str(&order_sql);
                    b.sql.push(')');
                }
            }
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
            b.write_scope_col(column);
            b.sql.push(')');
            Ok(())
        }
        // A correlated aggregate over a child table. The subquery
        // writer pushes the child's scope frame, and the inner
        // `OuterRef` reads the parent frame pushed above, so the
        // correlation resolves the same on every dialect. Any cast
        // is applied inside the subquery.
        AggregateExpr::RelatedAggregate(e) => write_expr(b, e, None),
        _ => write_aggregate_kind(b, expr),
    }
}

/// Emit a flat aggregate: [`format_bare_aggregate`] plus the cast
/// the decoder needs, if any.
fn write_aggregate_kind(b: &mut Sql<'_>, expr: &AggregateExpr) -> Result<(), SqlError> {
    let bare = format_bare_aggregate(b, expr)?;
    let out = match aggregate_cast_kind(b, expr) {
        Some(kind) => apply_agg_cast(b.d, kind, &bare),
        None => bare,
    };
    b.sql.push_str(&out);
    Ok(())
}

/// MySQL stand-in for `<inner> FILTER (WHERE p)`, written as
/// `<agg>(CASE WHEN p THEN <arg> END)`. `COUNT(*)` uses `THEN 1`,
/// every other aggregate uses `THEN <col>`.
fn write_aggregate_as_case_when(
    b: &mut Sql<'_>,
    inner: &AggregateExpr,
    filter: &WhereExpr,
) -> Result<(), SqlError> {
    let (agg_kw, case_then, distinct_prefix) = match inner {
        AggregateExpr::Count(None) => ("COUNT", None, ""),
        AggregateExpr::Count(Some(col)) => ("COUNT", Some(*col), ""),
        AggregateExpr::CountDistinct(col) => ("COUNT", Some(*col), "DISTINCT "),
        AggregateExpr::Sum(col) => ("SUM", Some(*col), ""),
        AggregateExpr::Avg(col) => ("AVG", Some(*col), ""),
        AggregateExpr::Max(col) => ("MAX", Some(*col), ""),
        AggregateExpr::Min(col) => ("MIN", Some(*col), ""),
        AggregateExpr::AnyValue(col) => ("ANY_VALUE", Some(*col), ""),
        AggregateExpr::StdDev(col)
        | AggregateExpr::StdDevPop(col)
        | AggregateExpr::Variance(col)
        | AggregateExpr::VariancePop(col) => (stddev_variance_name(inner), Some(*col), ""),
        AggregateExpr::Filtered { .. } | AggregateExpr::Coalesced { .. } => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "wrapper inside Filtered fallback",
            });
        }
        // None of these can go through the CASE WHEN rewrite.
        AggregateExpr::Window(_) => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Filtered(Window)",
            });
        }
        AggregateExpr::ArrayAgg { .. }
        | AggregateExpr::StringAgg { .. }
        | AggregateExpr::JsonbAgg { .. } => {
            return Err(SqlError::NestedAggregateWrapper {
                wrapper: "Filtered(PG-aggregate)",
            });
        }
        AggregateExpr::RelatedAggregate(_) => {
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
    write_where_expr(b, filter, None)?;
    b.sql.push_str(" THEN ");
    match case_then {
        Some(col) => b.write_scope_col(col),
        None => b.sql.push('1'),
    }
    b.sql.push_str(" END)");
    if let Some(kind) = aggregate_cast_kind(b, inner) {
        let emitted = b.sql[prior..].to_string();
        b.sql.truncate(prior);
        let wrapped = apply_agg_cast(b.d, kind, &emitted);
        b.sql.push_str(&wrapped);
    }
    Ok(())
}

/// The column an aggregate refers to, looking through wrappers.
/// `None` when there is none, as for `COUNT(*)`.
fn aggregate_column(expr: &AggregateExpr) -> Option<&'static str> {
    match expr {
        AggregateExpr::Count(c) => *c,
        AggregateExpr::CountDistinct(c)
        | AggregateExpr::Sum(c)
        | AggregateExpr::Avg(c)
        | AggregateExpr::Max(c)
        | AggregateExpr::Min(c)
        | AggregateExpr::AnyValue(c)
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
        // The column is on the child table inside the subquery, so
        // there is no outer column to report.
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

// ---- INSERT ----

pub(super) fn write_insert(b: &mut Sql<'_>, query: &InsertQuery) -> Result<(), SqlError> {
    if query.columns.is_empty() && query.returning.is_empty() {
        return Err(SqlError::EmptyInsert);
    }
    write_insert_rows(b, query)?;
    write_returning(b, &query.returning)
}

/// [`write_insert`] without `RETURNING`: MySQL reads the id from the reply.
pub(super) fn write_insert_rows(b: &mut Sql<'_>, query: &InsertQuery) -> Result<(), SqlError> {
    if query.columns.len() != query.values.len() {
        return Err(SqlError::InsertShapeMismatch {
            columns: query.columns.len(),
            values: query.values.len(),
        });
    }

    b.sql.push_str("INSERT INTO ");
    b.write_ident(query.model.table);

    if query.columns.is_empty() {
        b.sql.push_str(b.d.default_values_clause());
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
        write_on_conflict(b, query.model, &query.columns, &query.generated, conflict)?;
    }
    Ok(())
}

/// The conflict clause of an INSERT. A `DoUpdate` with no columns has no SET to write.
fn write_on_conflict(
    b: &mut Sql<'_>,
    model: &ModelSchema,
    columns: &[&'static str],
    generated: &[&str],
    conflict: &crate::core::ConflictClause,
) -> Result<(), SqlError> {
    if let crate::core::ConflictClause::DoUpdate { update_columns, .. } = conflict {
        if update_columns.is_empty() {
            return Err(SqlError::EmptyUpdateSet);
        }
    }
    let set = caller_set(columns, generated);
    b.d.write_conflict_clause(&mut b.sql, model, &set, conflict)
}

/// The bound columns whose value the caller chose, not the writer.
fn caller_set(columns: &[&'static str], generated: &[&str]) -> Vec<&'static str> {
    columns
        .iter()
        .copied()
        .filter(|c| !generated.contains(c))
        .collect()
}

// ---- BULK INSERT ----

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
        let cell = query
            .model
            .field_by_column(pk)
            .map_or("DEFAULT", |f| b.d.default_pk_cell(f));
        b.sql.push_str(" (");
        b.write_ident(pk);
        b.sql.push_str(") VALUES ");
        let mut first_row = true;
        for _ in &query.rows {
            if !first_row {
                b.sql.push_str(", ");
            }
            first_row = false;
            b.sql.push('(');
            b.sql.push_str(cell);
            b.sql.push(')');
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
        write_on_conflict(b, query.model, &query.columns, &query.generated, conflict)?;
    }

    write_returning(b, &query.returning)?;
    Ok(())
}

// ---- UPDATE ----

pub(super) fn write_update(b: &mut Sql<'_>, query: &UpdateQuery) -> Result<(), SqlError> {
    if query.set.is_empty() {
        return Err(SqlError::EmptyUpdateSet);
    }
    b.scope_stack.push(Scope::plain(query.model));
    b.dml_target = Some(query.model.table);
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

/// Write a [`crate::core::Expr`], the right-hand-side form behind
/// `F()` column references and arithmetic. A literal goes through
/// [`Sql::push_param_typed`], so NULL casts still fire; a `Column`
/// becomes a quoted ident; a `BinOp` becomes `(<left> <op> <right>)`.
fn write_expr(
    b: &mut Sql<'_>,
    expr: &crate::core::Expr,
    cast: Option<BindCast>,
) -> Result<(), SqlError> {
    use crate::core::{BinOp as BO, Expr};
    match expr {
        Expr::Literal(v) => {
            b.push_param_typed(v.clone(), cast);
            Ok(())
        }
        Expr::Column(name) => {
            if let Some(alias) = b.qualify_alias(name) {
                let qualified = format!("{}.{}", b.d.quote_ident(&alias), b.d.quote_ident(name),);
                b.sql.push_str(&qualified);
            } else {
                b.write_ident(name);
            }
            Ok(())
        }
        Expr::BinOp { left, op, right } => {
            // SQLite has no bitwise XOR.
            if matches!(op, BO::BitXor) && b.d.name() == "sqlite" {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "BitXor",
                    dialect: b.d.name(),
                });
            }
            // pgvector distance operators are Postgres-only.
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
                // MySQL's `/` returns a decimal even for two integers.
                BO::Div if b.d.name() == "mysql" && is_int_expr(b, expr) => "DIV",
                BO::Div => "/",
                BO::Mod => "%",
                BO::BitAnd => "&",
                BO::BitOr => "|",
                // PG spells XOR `#`, MySQL `^`.
                BO::BitXor => {
                    if b.d.name() == "postgres" {
                        "#"
                    } else {
                        "^"
                    }
                }
                BO::BitShl => "<<",
                BO::BitShr => ">>",
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
            // `CAST(x AS t)` is the same everywhere; only the type
            // token differs.
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
            // `write_select` pushes its own scope frame, so a nested
            // `OuterRef` finds the right enclosing model. A plain
            // subquery is not an aggregating query, so turn the
            // aggregate gate off across the boundary.
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
            // A correlated scalar aggregate, e.g.
            // `(SELECT COUNT(*) FROM … WHERE … = OuterRef)`.
            // `write_aggregate` pushes the child's scope frame, so the
            // inner `OuterRef` resolves to the parent query. Its
            // aggregate is written by the projection, not the gated
            // `Expr::Aggregate` path, so turn the gate off here too.
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
            // `(SELECT <kind>(<col>) FROM <table> WHERE <correlation>)`
            // over a relation table, such as an M2M junction. SUM and
            // AVG get the decoder cast; MAX, MIN and COUNT need none.
            let col_sql = |c: Option<&'static str>, k: &'static str| -> Result<String, SqlError> {
                c.map(|c| b.d.quote_ident(c))
                    .ok_or(SqlError::RelAggregateMissingColumn { kind: k })
            };
            let agg = match kind {
                RelAggKind::Count => "COUNT(*)".to_string(),
                RelAggKind::Sum => {
                    let bare = format!("SUM({})", col_sql(*column, "SUM")?);
                    // The relation table has no scope frame; find its model by name.
                    let ty = crate::core::ModelEntry::for_table(table)
                        .and_then(|e| e.schema.field_by_column((*column)?))
                        .map(|f| f.ty);
                    match sum_cast(ty) {
                        Some(kind) => apply_agg_cast(b.d, kind, &bare),
                        None => bare,
                    }
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
            write_rel_correlation(b, table, correlation)?;
            b.sql.push(')');
            Ok(())
        }
        Expr::OuterRef(col) => {
            // The top frame is this subquery; the one below it is the
            // enclosing query the `OuterRef` points at.
            let len = b.scope_stack.len();
            if len < 2 {
                return Err(SqlError::OuterRefOutsideSubquery { column: col });
            }
            let outer = b.scope_stack[len - 2];
            let qualified = format!("{}.{}", b.d.quote_ident(outer.name), b.d.quote_ident(col),);
            b.sql.push_str(&qualified);
            Ok(())
        }
        Expr::AliasedColumn { alias, column } => {
            // An explicit `<alias>.<col>`, written as given unless the
            // join lives inside a derived table.
            let qualified = match &b.derived_joins {
                Some(dj) if dj.depth == b.scope_stack.len() && dj.aliases.contains(alias) => {
                    format!(
                        "{}.{}",
                        b.d.quote_ident(dj.table),
                        b.d.quote_ident(&crate::core::joins::joined_label(alias, column))
                    )
                }
                _ => {
                    // The current table, written under its FROM name.
                    let alias = b.scope_stack.last().map_or(*alias, |s| s.rename(alias));
                    format!(
                        "{}.{}",
                        b.d.quote_ident(&b.alias_ref(alias)),
                        b.d.quote_ident(column)
                    )
                }
            };
            b.sql.push_str(&qualified);
            Ok(())
        }
        Expr::Window(w) => write_window_expr(b, w),
        Expr::Aggregate(agg) => {
            // Databases reject an aggregate in WHERE, UPDATE SET,
            // JOIN ON, GROUP BY, RETURNING and a plain SELECT list,
            // so refuse to write one there.
            if !b.aggregate_allowed {
                return Err(SqlError::AggregateOutsideAggregateContext);
            }
            // The aggregate writer needs a model for the COALESCE
            // cast lookup; the top scope frame is the right one.
            let model = b
                .scope_model()
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

/// The type of `expr` when the writer can tell: a literal, a column of
/// the current model or a join, or integer arithmetic over those.
fn expr_type(b: &Sql<'_>, expr: &crate::core::Expr) -> Option<crate::core::FieldType> {
    use crate::core::{BinOp as BO, Expr, FieldType};
    match expr {
        Expr::Literal(v) => v.field_type(),
        Expr::Column(c) if !matches!(b.qualifier(), Some(Qualifier::Join(_))) => {
            b.scope_model()?.field_by_column(c).map(|f| f.ty)
        }
        Expr::AliasedColumn { alias, column } => {
            alias_model(b, alias)?.field_by_column(column).map(|f| f.ty)
        }
        Expr::BinOp {
            op: BO::Add | BO::Sub | BO::Mul | BO::Div | BO::Mod,
            ..
        } if is_int_expr(b, expr) => Some(FieldType::I64),
        _ => None,
    }
}

/// The model `alias` names, innermost query first: its joins, then its
/// table.
fn alias_model(b: &Sql<'_>, alias: &str) -> Option<&'static ModelSchema> {
    (1..=b.scope_stack.len()).rev().find_map(|depth| {
        b.join_types
            .iter()
            .rev()
            .find(|(d, a, _)| *d == depth && *a == alias)
            .map(|(_, _, m)| *m)
            .or_else(|| {
                let s = b.scope_stack[depth - 1];
                (s.name == alias || s.model.table == alias).then_some(s.model)
            })
    })
}

/// True for arithmetic whose operands are both known integers.
fn is_int_expr(b: &Sql<'_>, expr: &crate::core::Expr) -> bool {
    use crate::core::{Expr, FieldType as T};
    let int = |e: &Expr| matches!(expr_type(b, e), Some(T::I16 | T::I32 | T::I64));
    matches!(expr, Expr::BinOp { left, right, .. } if int(left) && int(right))
}

/// Write a PG datetime argument, a timestamptz shifted to UTC so the
/// result ignores the session TimeZone, as MySQL and SQLite do.
fn write_pg_utc_arg(b: &mut Sql<'_>, arg: &crate::core::Expr) -> Result<(), SqlError> {
    if expr_type(b, arg) != Some(crate::core::FieldType::DateTime) {
        return write_expr(b, arg, None);
    }
    b.sql.push('(');
    write_expr(b, arg, None)?;
    b.sql.push_str(" AT TIME ZONE 'UTC')");
    Ok(())
}

/// Emit a JSON-path traversal.
///
/// Postgres chains `->` operators, ending in `->>` when `as_text`,
/// and binds each key as a parameter.
///
/// MySQL and SQLite use one `json_extract` call with a path string
/// like `$."k1"."k2"[0]`. Both require a literal there, so the path is
/// inlined. To keep that safe, keys must match `[A-Za-z0-9_]`;
/// anything else returns `OpNotSupportedInDialect`.
fn write_json_path(
    b: &mut Sql<'_>,
    source: &crate::core::Expr,
    path: &[crate::core::JsonPathStep],
    as_text: bool,
) -> Result<(), SqlError> {
    use crate::core::{JsonPathStep, SqlValue};
    let dialect = b.d.name();
    if dialect == "postgres" {
        check_json_path(path, dialect)?;
        // Chained `->` … `->>`. PG counts a negative index from the
        // end on its own.
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
                    // Inline the integer. Bound as a parameter, PG
                    // would type it as text and fail to pick a `->`
                    // overload.
                    b.sql.push_str(&n.to_string());
                }
            }
        }
        return Ok(());
    }
    let json_path = json_path_text(path, dialect)?;
    if dialect == "mysql" {
        if as_text {
            // A JSON null reads as SQL NULL, as PG's `->>` gives.
            b.sql.push_str("JSON_UNQUOTE(NULLIF(JSON_EXTRACT(");
            write_expr(b, source, None)?;
            b.sql.push_str(", '");
            b.sql.push_str(&json_path);
            b.sql.push_str("'), CAST('null' AS JSON)))");
        } else {
            b.sql.push_str("JSON_EXTRACT(");
            write_expr(b, source, None)?;
            b.sql.push_str(", '");
            b.sql.push_str(&json_path);
            b.sql.push_str("')");
        }
        return Ok(());
    }
    if as_text {
        // json_extract returns 1/0 for booleans and numbers as numbers;
        // PG's `->>` returns text such as 'true' and '1'.
        b.sql.push_str("CASE json_type(");
        write_expr(b, source, None)?;
        b.sql.push_str(", '");
        b.sql.push_str(&json_path);
        b.sql.push_str(
            "') WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' ELSE CAST(json_extract(",
        );
        write_expr(b, source, None)?;
        b.sql.push_str(", '");
        b.sql.push_str(&json_path);
        b.sql.push_str("') AS TEXT) END");
        return Ok(());
    }
    b.sql.push_str("json_extract(");
    write_expr(b, source, None)?;
    b.sql.push_str(", '");
    b.sql.push_str(&json_path);
    b.sql.push_str("')");
    Ok(())
}

/// Refuse an empty path or an unsafe key. MySQL and SQLite inline the
/// key, so it must be safe; PG binds it but uses the same rule.
fn check_json_path(
    path: &[crate::core::JsonPathStep],
    dialect: &'static str,
) -> Result<(), SqlError> {
    use crate::core::JsonPathStep;
    if path.is_empty() {
        return Err(SqlError::OpNotSupportedInDialect {
            op: "JsonPath requires at least one path step",
            dialect,
        });
    }
    let key_safe =
        |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    for step in path {
        if let JsonPathStep::Key(k) = step {
            if !key_safe(k) {
                return Err(SqlError::OpNotSupportedInDialect {
                    op: "JsonPath key contains characters outside the safe set [A-Za-z0-9_]",
                    dialect,
                });
            }
        }
    }
    Ok(())
}

/// The checked MySQL `$."<k>"[<n>]` / SQLite `$.<k>[<n>]` path string.
fn json_path_text(
    path: &[crate::core::JsonPathStep],
    dialect: &'static str,
) -> Result<String, SqlError> {
    use crate::core::JsonPathStep;
    check_json_path(path, dialect)?;
    let mut json_path = String::from("$");
    for step in path {
        match step {
            // MySQL rejects a bare `$.2fa`; SQLite takes it. The safe set keeps `"` out.
            JsonPathStep::Key(k) if dialect == "mysql" => {
                json_path.push_str(".\"");
                json_path.push_str(k);
                json_path.push('"');
            }
            JsonPathStep::Key(k) => {
                json_path.push('.');
                json_path.push_str(k);
            }
            JsonPathStep::Index(n) => {
                if *n < 0 {
                    // SQLite 3.31+ counts from the end with `$[#-1]`.
                    // MySQL's path grammar has no negative form.
                    if dialect == "sqlite" {
                        json_path.push_str("[#");
                        json_path.push_str(&n.to_string());
                        json_path.push(']');
                    } else {
                        return Err(SqlError::OpNotSupportedInDialect {
                            op: "JsonPath negative indices are unsupported on MySQL (the $[N] path grammar has no negative form; PG + SQLite support them)",
                            dialect,
                        });
                    }
                } else {
                    json_path.push('[');
                    json_path.push_str(&n.to_string());
                    json_path.push(']');
                }
            }
        }
    }
    Ok(json_path)
}

/// SQLite `json_path = <JSON null, object or array>` by value, as PG's
/// `jsonb` compares: `json_extract` gives NULL for a JSON null and the
/// stored text for an object, key order included. `Ok(false)`
/// when the comparison is not one of those.
fn write_sqlite_json_value_eq(
    b: &mut Sql<'_>,
    lhs: &crate::core::Expr,
    op: Op,
    rhs: &crate::core::Expr,
) -> Result<bool, SqlError> {
    use crate::core::Expr;
    let (
        Expr::JsonPath {
            source,
            path,
            as_text: false,
        },
        Expr::Literal(SqlValue::Json(v)),
        Op::Eq | Op::Ne,
        "sqlite",
    ) = (lhs, rhs, op, b.d.name())
    else {
        return Ok(false);
    };
    if !(v.is_null() || v.is_object() || v.is_array()) {
        return Ok(false);
    }
    let p = json_path_text(path, "sqlite")?;
    let ne = matches!(op, Op::Ne);
    if !v.is_null() {
        b.sql.push('(');
    }
    b.sql.push_str("json_type(");
    write_expr(b, source, None)?;
    if v.is_null() {
        b.sql.push_str(&format!(", '{p}') "));
        b.sql.push_str(if ne { "<> 'null'" } else { "= 'null'" });
        return Ok(true);
    }
    // Same (relative key, type, atom) rows both ways; arrays keep order.
    // integer and real are one type, so 1 = 1.0 as on PG and MySQL.
    let ty = "CASE WHEN type IN ('integer', 'real') THEN 'number' ELSE type END";
    // Cut at the root row's fullkey: SQLite < 3.45 rewrites it, so `p` can differ.
    let col = format!(
        "SELECT substr(fullkey, min(length(fullkey)) OVER () + 1), {ty}, atom FROM json_tree("
    );
    let lit = format!("SELECT substr(fullkey, 2), {ty}, atom FROM json_tree(");
    b.sql.push_str(&format!(", '{p}') IS NOT NULL AND "));
    b.sql.push_str(if ne { "NOT (" } else { "(" });
    b.sql.push_str("NOT EXISTS (");
    b.sql.push_str(&col);
    write_expr(b, source, None)?;
    b.sql.push_str(&format!(", '{p}') EXCEPT {lit}"));
    b.push_param(SqlValue::Json(v.clone()));
    b.sql.push_str(&format!(")) AND NOT EXISTS ({lit}"));
    b.push_param(SqlValue::Json(v.clone()));
    b.sql.push_str(&format!(") EXCEPT {col}"));
    write_expr(b, source, None)?;
    b.sql.push_str(&format!(", '{p}'))))"));
    Ok(true)
}

/// Emit `<fn>(args) OVER (PARTITION BY … ORDER BY … [frame])`. This
/// is standard SQL, accepted as-is by PG 9.0+, MySQL 8.0+ and
/// SQLite 3.25+.
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
        WindowFn::Sum => "SUM",
        WindowFn::Avg => "AVG",
        WindowFn::Min => "MIN",
        WindowFn::Max => "MAX",
        WindowFn::Count => "COUNT",
    };
    b.sql.push_str(fn_name);
    b.sql.push('(');
    // `COUNT()` is not valid SQL, so a bare `count_over()` means
    // the windowed `COUNT(*)`.
    if matches!(w.kind, WindowFn::Count) && w.args.is_empty() {
        b.sql.push('*');
    }
    // PG's LAG, LEAD and NTILE want their offset or bucket count as
    // `integer`. A bound `i64` arrives as `bigint` and the function
    // lookup fails, so write that one argument inline. The other
    // arguments bind normally.
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
        // An empty `And` means "no filter" at the top of an UPDATE,
        // but here it would leave a hole in `WHEN … THEN`.
        if branch.condition.is_empty() {
            return Err(SqlError::EmptyCaseWhenCondition);
        }
        b.sql.push_str(" WHEN ");
        // No qualifier or model: the surrounding statement already
        // set the table context.
        write_where_expr(b, &branch.condition, None)?;
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

/// Emit a scalar function call. Most are a plain `FN(args…)` on
/// every dialect; the ones that differ get their own arm.
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
                // A `||` chain, in parens to keep precedence clear.
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
        // `CEIL` works on all three; SQLite needs 3.35+.
        F::Ceil => write_call_unary(b, "CEIL", args),
        F::Round => {
            // Takes 1 or 2 arguments, spelled the same everywhere.
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
            // SQLite has no GREATEST; its scalar `MAX(a, b, …)` needs
            // two or more arguments. With one, SQLite reads `MAX(x)`
            // as the aggregate, which means something else entirely.
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
            // Same one-argument problem as `Greatest` above.
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

        // -------- date/time --------
        F::Now => {
            if !args.is_empty() {
                return Err(SqlError::FunctionArityMismatch {
                    func: "NOW",
                    expected: "0",
                    got: args.len(),
                });
            }
            // PG has `NOW()`; MySQL needs `NOW(6)` to keep microseconds.
            //
            // SQLite must use the same format as every other write
            // path. Its datetime columns are TEXT compared as text,
            // so a bare `CURRENT_TIMESTAMP` would put a second shape
            // in the column and break both `SET col = now()` and
            // `WHERE col < now()`.
            if b.d.name() == "sqlite" {
                let _ = write!(
                    b.sql,
                    "strftime('{}','now')",
                    crate::sql::SQLITE_DATETIME_FORMAT
                );
            } else if b.d.name() == "mysql" {
                b.sql.push_str("NOW(6)");
            } else {
                b.sql.push_str("NOW()");
            }
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
                // strftime has no quarter token, so derive it from
                // the month.
                write_extract_quarter_sqlite(b, &args[0])
            } else {
                write_extract_int(b, kind, args)
            }
        }
        F::TruncDate => {
            if args.len() != 1 {
                return Err(SqlError::FunctionArityMismatch {
                    func: "DATE",
                    expected: "1",
                    got: args.len(),
                });
            }
            // Every dialect spells this `DATE(x)`.
            b.sql.push_str("DATE(");
            if b.d.name() == "postgres" {
                write_pg_utc_arg(b, &args[0])?;
            } else {
                write_expr(b, &args[0], None)?;
            }
            b.sql.push(')');
            Ok(())
        }
        F::TruncYear | F::TruncMonth | F::TruncDay => write_trunc(b, kind, args),
        F::JsonArrayLength => {
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
                // SQLite 3.38+.
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

        // -------- text and math helpers --------
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
            // SQLite only has POWER and SQRT when built with
            // SQLITE_ENABLE_MATH_FUNCTIONS, which sqlx's default
            // build does not set.
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

        // -------- more math and date helpers --------
        F::Log => write_log(b, args),
        F::LogWithBase => write_log_with_base(b, args),
        F::Exp => write_exp(b, args),
        F::Pi => write_pi(b, args),
        F::Random => write_random(b, args),
        F::MakeInterval => write_make_interval(b, args),
        F::Age => write_age(b, args),
        F::TruncWithTz => write_trunc_with_tz(b, args),

        // -------- full-text search --------
        F::SetWeight => write_setweight(b, args),
        F::TsConcat => write_ts_concat(b, args),

        // -------- PostGIS, Postgres-only --------
        F::StDistance | F::StDWithin | F::StContains | F::StWithin | F::StIntersects => {
            write_spatial_fn(b, kind, args)
        }
    }
}

/// PostGIS `ST_*` functions. Postgres only; the others return
/// `OpNotSupportedInDialect`. Arity is checked here so a hand-built
/// `Expr::Function` fails before it reaches the database.
fn write_spatial_fn(
    b: &mut Sql<'_>,
    kind: crate::core::ScalarFn,
    args: &[crate::core::Expr],
) -> Result<(), SqlError> {
    use crate::core::ScalarFn as F;
    let (name, arity): (&'static str, usize) = match kind {
        F::StDistance => ("ST_Distance", 2),
        F::StDWithin => ("ST_DWithin", 3),
        F::StContains => ("ST_Contains", 2),
        F::StWithin => ("ST_Within", 2),
        F::StIntersects => ("ST_Intersects", 2),
        _ => unreachable!("write_spatial_fn only handles ST_* variants"),
    };
    if args.len() != arity {
        return Err(SqlError::FunctionArityMismatch {
            func: name,
            expected: if arity == 3 { "3" } else { "2" },
            got: args.len(),
        });
    }
    if b.d.name() != "postgres" {
        return Err(SqlError::OpNotSupportedInDialect {
            op: match kind {
                F::StDistance => "ST_Distance (PostGIS) is Postgres-only",
                F::StDWithin => "ST_DWithin (PostGIS) is Postgres-only",
                F::StContains => "ST_Contains (PostGIS) is Postgres-only",
                F::StWithin => "ST_Within (PostGIS) is Postgres-only",
                _ => "ST_Intersects (PostGIS) is Postgres-only",
            },
            dialect: b.d.name(),
        });
    }
    write_call(b, name, args)
}

/// `LPAD(s, len, fill)` and `RPAD(s, len, fill)`. Native on PG and
/// MySQL; SQLite gets a `substr`-based stand-in.
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
    // SQLite has no LPAD or RPAD, so build one:
    //   LPad: substr(replace(printf('%.*c', len, ' '), ' ', fill) || s, -len)
    //   RPad: substr(s || replace(printf('%.*c', len, ' '), ' ', fill), 1, len)
    //
    // `printf('%.*c', n, ' ')` makes `n` spaces, which `replace`
    // turns into the fill character. The outer `substr` clips to
    // `len` when the input is already that long: LPAD keeps the
    // right side, RPAD the left.
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

/// `MD5(s)`, `SHA1(s)` and `SHA256(s)`. PG uses `pgcrypto`'s
/// `digest` with `encode(…, 'hex')`, MySQL its own `MD5`/`SHA1`/
/// `SHA2`. SQLite has no hash function and returns an error.
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
        // `md5()` is built in and returns hex; SHA1 and SHA256 need
        // pgcrypto's `digest()`.
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

/// `POSITION(needle, hay)`. Every dialect spells this differently.
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

/// `REPEAT(s, n)`. Native on PG and MySQL; SQLite builds it from
/// `printf` and `replace`.
fn write_repeat(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if args.len() != 2 {
        return Err(SqlError::FunctionArityMismatch {
            func: "REPEAT",
            expected: "2",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        // `printf('%.*c', n, '_')` makes `n` underscores, and
        // `replace` swaps each for `s`. If `s` itself contains `_`,
        // this breaks; repeat the string in Rust instead.
        b.sql.push_str("replace(printf('%.*c', ");
        write_expr(b, &args[1], None)?;
        b.sql.push_str(", '_'), '_', ");
        write_expr(b, &args[0], None)?;
        b.sql.push(')');
        return Ok(());
    }
    write_call(b, "REPEAT", args)
}

/// `SIGN(x)`. Native on PG and MySQL; SQLite gets a CASE WHEN.
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
    // The `0`, `1` and `-1` are inlined rather than bound, to keep
    // the parameter count down.
    b.sql.push_str("(CASE WHEN ");
    write_expr(b, &args[0], None)?;
    b.sql.push_str(" > 0 THEN 1 WHEN ");
    write_expr(b, &args[0], None)?;
    b.sql.push_str(" < 0 THEN -1 ELSE 0 END)");
    Ok(())
}

/// `LN(x)`, the natural log. Same name on PG and MySQL. SQLite only
/// has it when built with `SQLITE_ENABLE_MATH_FUNCTIONS`, which
/// sqlx's default build does not set, so it returns an error.
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

/// `LOG(base, x)`. Same on PG and MySQL; SQLite errors, see
/// [`write_log`].
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

/// `EXP(x)`. Native on PG and MySQL; SQLite errors, see
/// [`write_log`].
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

/// `PI()`. SQLite has no such function, so the constant is inlined.
fn write_pi(b: &mut Sql<'_>, args: &[crate::core::Expr]) -> Result<(), SqlError> {
    if !args.is_empty() {
        return Err(SqlError::FunctionArityMismatch {
            func: "PI",
            expected: "0",
            got: args.len(),
        });
    }
    if b.d.name() == "sqlite" {
        b.sql.push_str("3.141592653589793");
    } else {
        b.sql.push_str("PI()");
    }
    Ok(())
}

/// `RANDOM()` or `RAND()`. The value range differs per backend; see
/// [`crate::core::ScalarFn::Random`].
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
        // SQLite returns a signed 64-bit integer, not a 0..1 float.
        _ => b.sql.push_str("random()"),
    }
    Ok(())
}

/// `make_interval(years => …, months => …, …)`. Postgres only.
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

/// `AGE(ts1, ts2)`, the gap between two timestamps. The result type
/// differs: an `interval` on PG, a number of seconds elsewhere.
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
            // TIMESTAMPDIFF(a, b) returns b - a, so swap the args to
            // match PG's `age(ts1, ts2)` = ts1 - ts2 sign.
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

/// `date_trunc(unit, ts AT TIME ZONE tz)` and its equivalents.
/// `unit` and `tz` arrive as string literals from
/// [`crate::core::funcs::trunc_with_tz`] and are inlined, because
/// each dialect needs a different format token. Both are checked
/// against a safe charset first.
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
    // The tz is inlined into the SQL, so restrict its characters.
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
            // CONVERT_TZ needs a from/to pair; stored timestamps are
            // UTC. The DATE_FORMAT mask does the truncation.
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
            // SQLite has no timezone database, so `tz` is passed to
            // strftime as a modifier, usually a `±HH:MM` offset.
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

/// `setweight(<tsvector>, '<weight>')`. Postgres only. The weight
/// must sit in the SQL text, not in a bound parameter, so it arrives
/// as a string literal and is inlined after a check that it is one
/// of A, B, C or D.
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

/// `(a || b || c)`, Postgres tsvector concatenation. The other
/// backends tie full-text search to the schema — a FULLTEXT index on
/// MySQL, an FTS5 table on SQLite — so `||` has no meaning there.
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

/// The quarter on SQLite. `strftime` has no quarter token, so derive
/// it from the month: `((month + 2) / 3)` gives 1 for January to
/// March, 2 for April to June, and so on.
fn write_extract_quarter_sqlite(b: &mut Sql<'_>, expr: &crate::core::Expr) -> Result<(), SqlError> {
    b.sql.push_str("((CAST(strftime('%m', ");
    write_expr(b, expr, None)?;
    b.sql.push_str(") AS INTEGER) + 2) / 3)");
    Ok(())
}

/// Emit an `EXTRACT(<field> FROM x)` call. PG uses the standard
/// syntax with a cast to integer, MySQL has a function per field,
/// and SQLite goes through `strftime` plus a cast.
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
        // parity with MySQL's per-field functions. SECOND has a
        // fraction the cast would round (59.7 to 60), so FLOOR it.
        let floor = field == "SECOND";
        b.sql.push_str(if floor {
            "CAST(FLOOR(EXTRACT("
        } else {
            "CAST(EXTRACT("
        });
        b.sql.push_str(field);
        b.sql.push_str(" FROM ");
        write_pg_utc_arg(b, &args[0])?;
        b.sql.push_str(if floor {
            ")) AS INTEGER)"
        } else {
            ") AS INTEGER)"
        });
    } else if dialect == "mysql" {
        b.sql.push_str(field);
        b.sql.push('(');
        write_expr(b, &args[0], None)?;
        // Mode 3 is ISO 8601, as PG's EXTRACT(WEEK).
        b.sql.push_str(if field == "WEEK" { ", 3)" } else { ")" });
    } else if field == "WEEK" {
        // ISO week: the day of year of this week's Thursday, over seven.
        b.sql.push_str("((CAST(strftime('%j', date(");
        write_expr(b, &args[0], None)?;
        b.sql.push_str(", '-3 days', 'weekday 4')) AS INTEGER) - 1) / 7 + 1)");
    } else {
        let token = match field {
            "YEAR" => "%Y",
            "MONTH" => "%m",
            "DAY" => "%d",
            "HOUR" => "%H",
            "MINUTE" => "%M",
            "SECOND" => "%S",
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

/// Day of the week, normalised to PG's numbering: 0 is Sunday, 6 is
/// Saturday. MySQL's `DAYOFWEEK()` starts at 1, so subtract one.
/// SQLite's `strftime('%w')` already starts at 0.
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
        write_pg_utc_arg(b, &args[0])?;
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

/// The `DATE_TRUNC` family. PG returns a timestamp from
/// `DATE_TRUNC('unit', x)`. MySQL and SQLite have no such function,
/// so they format the value instead and return **text**.
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
        // Truncate a timestamptz in UTC, not the session TimeZone.
        if expr_type(b, &args[0]) == Some(crate::core::FieldType::DateTime) {
            b.sql.push_str(", 'UTC'");
        }
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

/// Write `NAME(arg, arg, …)`, for the functions every dialect spells
/// the same way.
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

/// [`write_call`] for a one-argument function. The check only fires
/// for code that builds the IR by hand; the public builders already
/// take exactly one argument.
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

// ---- DELETE ----

pub(super) fn write_delete(b: &mut Sql<'_>, query: &DeleteQuery) -> Result<(), SqlError> {
    b.scope_stack.push(Scope::plain(query.model));
    b.dml_target = Some(query.model.table);
    let r = (|| {
        b.sql.push_str("DELETE FROM ");
        b.write_ident(query.model.table);
        write_where(b, &query.where_clause, Some(query.model))?;
        Ok(())
    })();
    b.scope_stack.pop();
    r
}

/// An owner `DELETE` and the junction deletes that must run before it,
/// in one transaction. Empty `links` when the dialect folded them in.
#[doc(hidden)]
pub struct DeletePlan {
    pub(crate) links: Vec<CompiledStatement>,
    pub(crate) owner: CompiledStatement,
}

/// Compile `query` with its generic M2M junction cleanup: one statement
/// with data-modifying CTEs where the dialect has them, else a list.
pub(crate) fn compile_delete_plan(
    d: &dyn Dialect,
    query: &DeleteQuery,
) -> Result<DeletePlan, SqlError> {
    let links = super::m2m::generic_m2m_links(query);
    if links.is_empty() || !d.supports_dml_in_cte() {
        return Ok(DeletePlan {
            links: links
                .iter()
                .map(|l| d.compile_delete(l))
                .collect::<Result<_, _>>()?,
            owner: d.compile_delete(query)?,
        });
    }
    // Every CTE reads the same snapshot, so each sees the owner rows.
    let mut b = Sql::new(d);
    b.sql.push_str("WITH ");
    for (i, link) in links.iter().enumerate() {
        if i > 0 {
            b.sql.push_str(", ");
        }
        b.write_ident(&format!("_rustango_link{i}"));
        b.sql.push_str(" AS (");
        write_delete(&mut b, link)?;
        b.sql.push(')');
    }
    b.sql.push(' ');
    write_delete(&mut b, query)?;
    Ok(DeletePlan {
        links: Vec::new(),
        owner: b.finish()?,
    })
}

// ---- BULK UPDATE ----
//
// Postgres uses `UPDATE … FROM (VALUES …)`. MySQL has no equivalent
// shape, so its dialect returns a "not supported" error instead.

/// `bulk_update` for SQLite. SQLite has `UPDATE … FROM <subquery>`
/// since 3.33, but not Postgres' column-list alias on inline VALUES:
/// `AS __data(pk, col, …)` is a syntax error there. A CTE with
/// correlated subqueries works on every SQLite with CTE support.
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
    // A NULL takes its column's cast: an all-NULL column in VALUES
    // is otherwise typed text. Only here does a vector NULL need one (#1970).
    let casts: Vec<Option<BindCast>> = std::iter::once(pk_field.column)
        .chain(query.update_columns.iter().copied())
        .map(|c| {
            let vector = query
                .model
                .field_by_column(c)
                .is_some_and(|f| matches!(f.ty, crate::core::FieldType::Vector(_)));
            null_cast_for(b.d, query.model, c).or(vector.then_some(BindCast::Untyped("vector")))
        })
        .collect();
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
            b.push_param_typed(val.clone(), casts.get(i).copied().flatten());
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

// ---- WHERE / filters ----

pub(super) fn write_where(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    if where_clause.is_empty() {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    write_where_expr(b, where_clause, model)
}

pub(super) fn write_where_with_search(
    b: &mut Sql<'_>,
    where_clause: &WhereExpr,
    search: Option<&SearchClause>,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    // A query with no column to match it matches nothing (#2391).
    let search = search.filter(|s| !s.query.is_empty());
    let has_search = search.is_some();
    let has_where = !where_clause.is_empty();
    if !has_where && !has_search {
        return Ok(());
    }
    b.sql.push_str(" WHERE ");
    // Parenthesized when search follows, so a top-level OR stays inside it.
    let wrap = has_where && has_search;
    if has_where {
        if wrap {
            b.sql.push('(');
        }
        write_where_expr(b, where_clause, model)?;
        if wrap {
            b.sql.push(')');
        }
    }
    if let Some(s) = search {
        if has_where {
            b.sql.push_str(" AND ");
        }
        if s.columns.is_empty() {
            b.sql.push_str("1 = 0");
            return Ok(());
        }
        // `write_ilike` picks each backend's case-insensitive LIKE:
        // native `ILIKE` on PG, `LOWER(col) LIKE LOWER(?)` elsewhere.
        //
        // Each column pushes its own param and placeholder. PG could
        // reuse one `$N`, but MySQL and SQLite bind positionally, so
        // a shared placeholder would leave every column after the
        // first bound to nothing.
        //
        // The query is escaped before the `%…%` wrap, and each column
        // gets the ESCAPE clause, so a `%` or `_` a user types
        // matches literally instead of acting as a wildcard.
        let pattern = format!("%{}%", crate::core::escape_like(&s.query));
        b.sql.push('(');
        for (i, col) in s.columns.iter().enumerate() {
            if i > 0 {
                b.sql.push_str(" OR ");
            }
            b.params.push(SqlValue::String(pattern.clone()));
            let placeholder = b.d.placeholder(b.params.len());
            let mut qualified = String::new();
            if let Some(table) = b.qualify_alias(col) {
                qualified.push_str(&b.d.quote_ident(&table));
                qualified.push('.');
            }
            qualified.push_str(&b.d.quote_ident(col));
            let ty = model.and_then(|m| m.field_by_column(col)).map(|f| f.ty);
            b.d.write_ilike_typed(&mut b.sql, &qualified, ty, &placeholder, false);
            b.sql.push_str(LIKE_ESCAPE_CLAUSE);
        }
        b.sql.push(')');
    }
    Ok(())
}

pub(super) fn write_where_expr(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, model),
        // A nested empty AND is TRUE; the top level skips WHERE instead.
        WhereExpr::And(items) if items.is_empty() => {
            b.sql.push_str("1 = 1");
            Ok(())
        }
        WhereExpr::And(items) => write_joined(b, items, " AND ", model),
        WhereExpr::Or(items) => {
            if items.is_empty() {
                return Err(SqlError::EmptyOrBranch);
            }
            write_joined(b, items, " OR ", model)
        }
        WhereExpr::Xor(items) => write_xor(b, items, model),
        WhereExpr::Not(child) => {
            b.sql.push_str("NOT (");
            write_where_expr(b, child, model)?;
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
            let qualified = render_qualified_col(b, column);
            b.sql.push_str(&qualified);
            b.sql.push_str(if *negated { " NOT IN (" } else { " IN (" });
            let limited = subquery.limit.is_some()
                || subquery.offset.is_some()
                || subquery.compound_limit.is_some()
                || subquery.compound_offset.is_some();
            // Only a subquery reading the write's target needs it (1093): a
            // correlated derived table re-runs per outer row.
            let outer_read = std::mem::take(&mut b.read_dml_target);
            let start = b.sql.len();
            write_select(b, subquery)?;
            let reads_target = b.read_dml_target;
            let wrap = (limited || reads_target) && b.d.in_subquery_limit_needs_derived_table();
            // A materialized derived table hides its read from the enclosing query.
            b.read_dml_target = outer_read || (reads_target && !wrap);
            if wrap {
                b.sql.insert_str(start, "SELECT * FROM (");
                b.sql.push_str(") AS ");
                b.write_ident("_rustango_in");
            }
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
            write_rel_correlation(b, table, correlation)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Write `<table> WHERE <correlation>`, tying a raw-table relation
/// subquery back to the enclosing row. These nodes hold no
/// [`SelectQuery`] and push no scope frame, so the enclosing query is
/// the **top** of the stack — unlike [`Expr::OuterRef`], which looks
/// one frame down.
fn write_rel_correlation(
    b: &mut Sql<'_>,
    table: &str,
    correlation: &crate::core::RelCorrelation,
) -> Result<(), SqlError> {
    use crate::core::RelCorrelation;
    let outer_table = match b.scope_stack.last() {
        Some(s) => s.name,
        // These only appear inside a query that pushed a frame.
        None => {
            return Err(SqlError::OuterRefOutsideSubquery {
                column: "<relation>",
            })
        }
    };
    b.write_table(table);
    // A self relation must not hide the outer row.
    let table = if table == outer_table {
        b.sql.push_str(" AS ");
        b.write_ident(SELF_ALIAS);
        SELF_ALIAS
    } else {
        table
    };
    b.sql.push_str(" WHERE ");
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
            // Generic-FK content-type check.
            if let Some(ct) = ct {
                b.sql.push_str(" AND ");
                b.write_ident(table);
                b.sql.push('.');
                b.write_ident(ct.ct_column);
                b.sql.push_str(" = (SELECT ");
                b.write_ident(ct.ct_pk);
                b.sql.push_str(" FROM ");
                b.write_table(ct.ct_table);
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
            b.write_table(through);
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

/// `= NULL` matches no row, so an `Eq` / `Ne` against NULL is written
/// `IS NULL` / `IS NOT NULL`, the rule for every predicate (#2413).
fn null_test(op: Op, value: &SqlValue) -> Option<&'static str> {
    match (op, value) {
        (Op::Eq, SqlValue::Null) => Some(" IS NULL"),
        (Op::Ne, SqlValue::Null) => Some(" IS NOT NULL"),
        _ => None,
    }
}

/// Emit `<lhs> <op> <rhs>` for [`WhereExpr::ExprCompare`].
///
/// Handles the binary comparisons plus `IN`, `BETWEEN`, `IS NULL` and
/// `LIKE`, which every dialect accepts in HAVING. `ILIKE` goes
/// through the dialect's `write_ilike`, so non-PG backends fall back
/// to `LOWER(a) LIKE LOWER(b)`.
///
/// JSON operators and the null-safe comparisons do not work against
/// an aggregate left-hand side. `AggregateBuilder::filter` rejects
/// them at build time with
/// [`crate::core::QueryError::HavingOpNotSupported`].
fn write_expr_compare(
    b: &mut Sql<'_>,
    lhs: &crate::core::Expr,
    op: crate::core::Op,
    rhs: &crate::core::Expr,
) -> Result<(), SqlError> {
    use crate::core::{Expr, Op};

    if let Expr::Literal(v) = rhs {
        if let Some(test) = null_test(op, v) {
            write_expr(b, lhs, None)?;
            b.sql.push_str(test);
            return Ok(());
        }
    }
    let binary_op_str = match op {
        Op::Eq => Some(" = "),
        Op::Ne => Some(" <> "),
        Op::Lt => Some(" < "),
        Op::Lte => Some(" <= "),
        Op::Gt => Some(" > "),
        Op::Gte => Some(" >= "),
        _ => None,
    };
    if write_sqlite_json_value_eq(b, lhs, op, rhs)? {
        return Ok(());
    }
    if let Some(kw) = binary_op_str {
        write_expr(b, lhs, None)?;
        b.sql.push_str(kw);
        return write_compare_rhs(b, lhs, rhs);
    }
    // Lookups that span a relation, such as `author__name__contains`,
    // land here. The lhs is typed so Postgres can cast a non-text column.
    if matches!(op, Op::Like | Op::NotLike | Op::LikeEscaped) {
        let lhs_start = b.sql.len();
        write_expr(b, lhs, None)?;
        let lhs_str = b.sql.split_off(lhs_start);
        let p_start = b.sql.len();
        write_expr(b, rhs, None)?;
        let p = b.sql.split_off(p_start);
        let ty = expr_type(b, lhs);
        b.d.write_like_typed(&mut b.sql, &lhs_str, ty, &p, matches!(op, Op::NotLike));
        if matches!(op, Op::LikeEscaped) {
            b.sql.push_str(LIKE_ESCAPE_CLAUSE);
        }
        return Ok(());
    }

    match op {
        Op::Search => {
            // PG full-text match, `<tsvector> @@ <tsquery>`. MySQL's
            // MATCH … AGAINST and SQLite's FTS5 tables do not work on
            // bare expressions, so `require_op` rejects them.
            require_op(b.d, op)?;
            write_expr(b, lhs, None)?;
            b.sql.push_str(" @@ ");
            write_expr(b, rhs, None)?;
            Ok(())
        }
        Op::ILike | Op::NotILike | Op::ILikeEscaped => {
            // `ILikeEscaped` is `ILike` with an ESCAPE clause added.
            require_op(
                b.d,
                if matches!(op, Op::ILikeEscaped) {
                    Op::ILike
                } else {
                    op
                },
            )?;
            // Write the lhs first so its binds land before the rhs
            // literal: MySQL and SQLite bind in text order. Then take
            // that SQL back out of the buffer, bind the rhs, and let
            // the dialect compose the final comparison.
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
            let ty = expr_type(b, lhs);
            b.d.write_ilike_typed(&mut b.sql, &lhs_str, ty, &p, matches!(op, Op::NotILike));
            if matches!(op, Op::ILikeEscaped) {
                b.sql.push_str(LIKE_ESCAPE_CLAUSE);
            }
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
        // The builder rejects the remaining ops, so reaching this
        // means an `ExprCompare` was built by hand.
        _ => Err(SqlError::OpNotSupportedInDialect {
            op: "non-binary comparison in ExprCompare",
            dialect: b.d.name(),
        }),
    }
}

/// The right side of `<lhs> <op> <rhs>`. SQLite's `json_extract` gives an
/// SQL value, so a JSON literal is unwrapped the same way; its as-text path
/// has no affinity, so a number gets NUMERIC to compare as one.
fn write_compare_rhs(
    b: &mut Sql<'_>,
    lhs: &crate::core::Expr,
    rhs: &crate::core::Expr,
) -> Result<(), SqlError> {
    use crate::core::Expr;
    let (Expr::JsonPath { as_text, .. }, Expr::Literal(v)) = (lhs, rhs) else {
        // A relation lookup on a citext column needs its cast too.
        let cast = match lhs {
            Expr::Column(c) if !matches!(b.qualifier(), Some(Qualifier::Join(_))) => {
                b.scope_model().and_then(|m| null_cast_for(b.d, m, c))
            }
            Expr::AliasedColumn { alias, column } => {
                alias_model(b, alias).and_then(|m| null_cast_for(b.d, m, column))
            }
            _ => None,
        };
        return write_expr(b, rhs, cast);
    };
    let (open, close) = match (as_text, v) {
        _ if b.d.name() != "sqlite" => return write_expr(b, rhs, None),
        (false, SqlValue::Json(_)) => ("json_extract(", ", '$')"),
        (
            true,
            SqlValue::I16(_)
            | SqlValue::I32(_)
            | SqlValue::I64(_)
            | SqlValue::F32(_)
            | SqlValue::F64(_),
        ) => ("CAST(", " AS NUMERIC)"),
        _ => return write_expr(b, rhs, None),
    };
    b.sql.push_str(open);
    b.push_param(v.clone());
    b.sql.push_str(close);
    Ok(())
}

/// Write `<col> <op> <rhs>` for a [`crate::core::ColumnFilter`].
/// Only binary comparisons fit this shape; anything else is a
/// builder bug and gives [`SqlError::OpNotSupportedInDialect`].
fn write_column_compare(
    b: &mut Sql<'_>,
    cf: &crate::core::ColumnFilter,
    _model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified = render_qualified_col(b, cf.column);
    b.sql.push_str(&qualified);
    if let crate::core::Expr::Literal(v) = &cf.rhs {
        if let Some(test) = null_test(cf.op, v) {
            b.sql.push_str(test);
            return Ok(());
        }
    }
    let op_str = match cf.op {
        crate::core::Op::Eq => " = ",
        crate::core::Op::Ne => " <> ",
        crate::core::Op::Lt => " < ",
        crate::core::Op::Lte => " <= ",
        crate::core::Op::Gt => " > ",
        crate::core::Op::Gte => " >= ",
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
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let mut first = true;
    for child in items {
        if !first {
            b.sql.push_str(sep);
        }
        first = false;
        write_child(b, child, model)?;
    }
    Ok(())
}

fn write_child(
    b: &mut Sql<'_>,
    expr: &WhereExpr,
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match expr {
        WhereExpr::Predicate(filter) => write_filter(b, filter, model),
        WhereExpr::ColumnCompare(cf) => write_column_compare(b, cf, model),
        // These leaves already write their own parens, or are flat,
        // so they need no extra layer.
        WhereExpr::Exists(_)
        | WhereExpr::NotExists(_)
        | WhereExpr::InSubquery { .. }
        | WhereExpr::ExprCompare { .. }
        | WhereExpr::RelExists { .. } => write_where_expr(b, expr, model),
        WhereExpr::And(_) | WhereExpr::Or(_) | WhereExpr::Xor(_) | WhereExpr::Not(_) => {
            b.sql.push('(');
            write_where_expr(b, expr, model)?;
            b.sql.push(')');
            Ok(())
        }
    }
}

/// Emit a [`WhereExpr::Xor`] node, which is true when an odd number
/// of its operands are true — what `Q(a) ^ Q(b)` builds. Only MySQL
/// has a logical XOR, so the writer rewrites it in portable SQL:
///
/// * 0 children: [`SqlError::EmptyXorBranch`]. A predicate that can
///   never be true is almost always a mistake.
/// * 1 child: the child itself.
/// * 2 children: `((a) AND NOT (b)) OR (NOT (a) AND (b))`.
///   **Each operand is written twice, so the database evaluates it
///   twice.** That is fine for a plain comparison, but a volatile
///   expression such as `RANDOM()` or `NOW()` can give a different
///   answer each time. To get one evaluation, build the three-element
///   `Xor([a, b, FALSE])` and take the branch below.
/// * 3 or more: `((CASE WHEN q1 THEN 1 ELSE 0 END) + … ) % 2 = 1`.
///   Every child is evaluated exactly once.
fn write_xor(
    b: &mut Sql<'_>,
    items: &[WhereExpr],
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    match items.len() {
        0 => Err(SqlError::EmptyXorBranch),
        1 => write_where_expr(b, &items[0], model),
        2 => {
            // (a AND NOT (b)) OR (NOT (a) AND b)
            b.sql.push('(');
            write_child(b, &items[0], model)?;
            b.sql.push_str(" AND NOT (");
            write_where_expr(b, &items[1], model)?;
            b.sql.push_str(")) OR (NOT (");
            write_where_expr(b, &items[0], model)?;
            b.sql.push_str(") AND ");
            write_child(b, &items[1], model)?;
            b.sql.push(')');
            Ok(())
        }
        _ => {
            // `write_child` so a composite child is parenthesized
            // before the `THEN 1`.
            b.sql.push('(');
            let mut first = true;
            for child in items {
                if !first {
                    b.sql.push_str(" + ");
                }
                first = false;
                b.sql.push_str("(CASE WHEN ");
                write_child(b, child, model)?;
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
    model: Option<&'static ModelSchema>,
) -> Result<(), SqlError> {
    let qualified_col = render_qualified_col(b, filter.column);
    if let Some(test) = null_test(filter.op, &filter.value) {
        b.sql.push_str(&qualified_col);
        b.sql.push_str(test);
        return Ok(());
    }
    let cast = model.and_then(|m| null_cast_for(b.d, m, filter.column));

    match filter.op {
        Op::Eq => simple_op(b, &qualified_col, " = ", filter.value.clone(), cast),
        Op::Ne => simple_op(b, &qualified_col, " <> ", filter.value.clone(), cast),
        Op::Lt => simple_op(b, &qualified_col, " < ", filter.value.clone(), cast),
        Op::Lte => simple_op(b, &qualified_col, " <= ", filter.value.clone(), cast),
        Op::Gt => simple_op(b, &qualified_col, " > ", filter.value.clone(), cast),
        Op::Gte => simple_op(b, &qualified_col, " >= ", filter.value.clone(), cast),
        // The value of `LikeEscaped` came from `core::escape_like`, so
        // the ESCAPE clause is required: SQLite has no default escape
        // character. `!` is the portable escape; MySQL eats a backslash
        // as a string escape before LIKE ever sees it.
        Op::Like | Op::NotLike | Op::LikeEscaped => {
            let p = b.bind(filter.value.clone());
            let ty = model
                .and_then(|m| m.field_by_column(filter.column))
                .map(|f| f.ty);
            b.d.write_like_typed(
                &mut b.sql,
                &qualified_col,
                ty,
                &p,
                matches!(filter.op, Op::NotLike),
            );
            if matches!(filter.op, Op::LikeEscaped) {
                b.sql.push_str(LIKE_ESCAPE_CLAUSE);
            }
        }
        // `ILikeEscaped` is `ILike` with an ESCAPE clause added.
        Op::ILike | Op::NotILike | Op::ILikeEscaped => {
            require_op(
                b.d,
                if matches!(filter.op, Op::ILikeEscaped) {
                    Op::ILike
                } else {
                    filter.op
                },
            )?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            let ty = model
                .and_then(|m| m.field_by_column(filter.column))
                .map(|f| f.ty);
            b.d.write_ilike_typed(
                &mut b.sql,
                &qualified_col,
                ty,
                &p,
                matches!(filter.op, Op::NotILike),
            );
            if matches!(filter.op, Op::ILikeEscaped) {
                b.sql.push_str(LIKE_ESCAPE_CLAUSE);
            }
        }
        Op::Regex | Op::NotRegex | Op::IRegex | Op::NotIRegex => {
            // The builder already checked the pattern is a string.
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
            // pg_trgm's `%` and `%>`. Postgres only.
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
            // Postgres full-text search. MySQL and SQLite tie their
            // FTS to the schema, so a bare column cannot be searched
            // and they reject this.
            require_op(b.d, filter.op)?;
            b.params.push(filter.value.clone());
            let p = b.d.placeholder(b.params.len());
            b.d.write_search(&mut b.sql, &qualified_col, &p)?;
        }
        Op::ArrayContains | Op::ArrayContainedBy | Op::ArrayOverlap => {
            // Postgres array operators `@>`, `<@` and `&&`. The value
            // must be `SqlValue::Array`, which binds as one array
            // parameter; a `List` would expand to separate
            // placeholders, which is the wrong shape here.
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
            // Postgres range operators. The value is either a
            // `RangeLiteral`, comparing range to range, or a scalar,
            // asking whether the range contains that element. Both
            // emit `<col> <op> <placeholder>`.
            require_op(b.d, filter.op)?;
            // A range literal binds as text, and PG will not resolve
            // `int4range @> text`, so cast it to the column's range
            // type. A scalar must stay uncast: that is the
            // element-containment form.
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
            let p = b.bind(filter.value.clone());
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
            // Bind one param per key, then let the dialect build the
            // predicate from the placeholders.
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
    cast: Option<BindCast>,
) {
    b.sql.push_str(qualified_col);
    b.sql.push_str(kw);
    b.push_param_typed(value, cast);
}

/// Render `[<table>.]<col>` with the dialect's quoting. Built up
/// front so an op handler can either write it as-is or wrap it, for
/// example in `LOWER(…)`, without editing the buffer afterwards.
fn render_qualified_col(b: &Sql<'_>, column: &str) -> String {
    let mut s = String::new();
    if let Some(table) = b.qualify_alias(column) {
        s.push_str(&b.d.quote_ident(&table));
        s.push('.');
    }
    s.push_str(&b.d.quote_ident(column));
    s
}

/// Bind each value as a param without writing to `b.sql`, and return
/// the placeholders so a per-dialect writer can place them itself.
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
        Op::LikeEscaped => "LIKE ... ESCAPE",
        Op::ILikeEscaped => "ILIKE ... ESCAPE",
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

// ---- ORDER BY / LIMIT / OFFSET ----

fn write_order_limit_offset(
    b: &mut Sql<'_>,
    order_by: &[crate::core::OrderItem],
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<(), SqlError> {
    write_order_limit_offset_with(b, order_by, limit, offset, |b, _, item| {
        write_order_target(b, item)
    })
}

/// [`write_order_limit_offset`] with `target` writing each item's sort key,
/// given its position in `order_by`.
fn write_order_limit_offset_with(
    b: &mut Sql<'_>,
    order_by: &[crate::core::OrderItem],
    limit: Option<i64>,
    offset: Option<i64>,
    target: impl Fn(&mut Sql<'_>, usize, &crate::core::OrderItem) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    use crate::core::{NullsOrder, OrderItem};
    if !order_by.is_empty() {
        b.sql.push_str(" ORDER BY ");
        let supports_nulls = b.d.supports_nulls_order();
        let mut first = true;
        for (i, item) in order_by.iter().enumerate() {
            let (desc, nulls) = match item {
                OrderItem::Column { desc, nulls, .. } => (*desc, *nulls),
                OrderItem::Expr { desc, nulls, .. } => (*desc, *nulls),
                // Random needs neither a direction nor a NULLS
                // clause: its key is per-row and never null.
                OrderItem::Random => (false, NullsOrder::Default),
            };
            // Where there is no NULLS clause, sort on
            // `<target> IS NULL` first, then on the target itself.
            if !supports_nulls && !matches!(nulls, NullsOrder::Default) {
                if !first {
                    b.sql.push_str(", ");
                }
                first = false;
                target(b, i, item)?;
                b.sql.push_str(" IS NULL");
                // NULLS FIRST puts nulls on top, so IS NULL DESC.
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
            target(b, i, item)?;
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
        // MySQL and SQLite reject `OFFSET` with no `LIMIT`, so they
        // supply a stand-in LIMIT here. PG takes the bare OFFSET.
        if let Some(clause) = b.d.offset_without_limit_clause() {
            b.sql.push_str(clause);
        }
    }
    if let Some(n) = offset {
        let _ = write!(b.sql, " OFFSET {n}");
    }
    Ok(())
}

/// Write the `<target>` half of `<target> [DESC] [NULLS …]`. The
/// MySQL nulls workaround needs it twice, once for the `IS NULL`
/// term and once for the real sort.
fn write_order_target(b: &mut Sql<'_>, item: &crate::core::OrderItem) -> Result<(), SqlError> {
    use crate::core::OrderItem;
    match item {
        OrderItem::Column { column, .. } => {
            let qualified = render_qualified_col(b, column);
            b.sql.push_str(&qualified);
        }
        OrderItem::Expr { expr, .. } => {
            write_expr(b, expr, None)?;
        }
        // `RANDOM()` on PG and SQLite, `RAND()` on MySQL.
        OrderItem::Random => {
            b.sql.push_str(b.d.random_fn());
            b.sql.push_str("()");
        }
    }
    Ok(())
}

// ---- RETURNING ----

fn write_returning(b: &mut Sql<'_>, returning: &[&'static str]) -> Result<(), SqlError> {
    if returning.is_empty() {
        return Ok(());
    }
    if !b.d.supports_returning() {
        // Error rather than emit SQL the backend rejects. Only the
        // executor knows whether a `LAST_INSERT_ID()` fallback fits,
        // so it catches this and decides.
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

/// `select` plus a correlated `__annotated_count` of each row's children,
/// so the query keeps its WHERE, joins, ORDER BY and LIMIT.
pub(crate) fn compile_select_with_child_count(
    d: &dyn Dialect,
    select: &SelectQuery,
    child: ChildCount,
) -> Result<CompiledStatement, SqlError> {
    let mut b = Sql::new(d);
    if select.compound.is_empty() {
        b.child_count = Some(child);
        write_select(&mut b, select)?;
        return b.finish();
    }
    // A set operation counts over its merged rows, or only the head would
    // carry the column.
    const PARENT: &str = "__rustango_parent";
    let merged = SelectQuery {
        compound_order_by: Vec::new(),
        compound_limit: None,
        compound_offset: None,
        lock_mode: None,
        ..select.clone()
    };
    b.sql.push_str("SELECT ");
    b.write_ident(PARENT);
    b.sql.push_str(".*");
    write_child_count(&mut b, PARENT, child);
    b.sql.push_str(" FROM (");
    write_select(&mut b, &merged)?;
    b.sql.push_str(") AS ");
    b.write_ident(PARENT);
    write_order_limit_offset(
        &mut b,
        &select.compound_order_by,
        select.compound_limit,
        select.compound_offset,
    )?;
    b.finish()
}

/// The children a SELECT counts per row: `child_table.fk_column = parent.parent_pk`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildCount {
    pub child_table: &'static str,
    pub fk_column: &'static str,
    pub parent_pk: &'static str,
}

/// `, (SELECT COUNT(*) FROM child AS c WHERE c.fk = parent.pk) AS
/// __annotated_count`. The alias keeps a self-referencing child apart.
fn write_child_count(b: &mut Sql<'_>, parent: &str, child: ChildCount) {
    const CHILD: &str = "__rustango_child";
    b.sql.push_str(", (SELECT COUNT(*) FROM ");
    b.write_ident(child.child_table);
    b.sql.push_str(" AS ");
    b.write_ident(CHILD);
    b.sql.push_str(" WHERE ");
    b.write_ident(CHILD);
    b.sql.push('.');
    b.write_ident(child.fk_column);
    b.sql.push_str(" = ");
    b.write_ident(parent);
    b.sql.push('.');
    b.write_ident(child.parent_pk);
    b.sql.push_str(") AS ");
    b.write_ident("__annotated_count");
}

/// Granularity of a `.dates()` / `.datetimes()` bucket.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DateBucket {
    Date(crate::query::DateKind),
    DateTime(crate::query::DateTimeKind),
}

/// `SELECT DISTINCT <bucket(column)> FROM (<select>) ORDER BY 1`, the
/// statement behind `.dates()` / `.datetimes()`.
pub(crate) fn compile_date_buckets(
    d: &dyn Dialect,
    select: &SelectQuery,
    column: &'static str,
    bucket: DateBucket,
    descending: bool,
) -> Result<CompiledStatement, SqlError> {
    let mut b = Sql::new(d);
    b.sql.push_str("SELECT DISTINCT ");
    let col = d.quote_ident(column);
    b.sql.push_str(&match bucket {
        DateBucket::Date(kind) => date_trunc_sql(kind, d.name(), &col),
        DateBucket::DateTime(kind) => datetime_trunc_sql(kind, d.name(), &col),
    });
    b.sql.push_str(" AS ");
    b.write_ident("rs_bucket");
    b.sql.push_str(" FROM (");
    write_select(&mut b, select)?;
    b.sql.push_str(") AS ");
    b.write_ident("rs_bucket_sub");
    b.sql.push_str(" ORDER BY ");
    b.write_ident("rs_bucket");
    b.sql.push_str(if descending { " DESC" } else { " ASC" });
    b.finish()
}

/// Truncate `col_quoted` to a date bucket, typed as a `DATE`.
fn date_trunc_sql(kind: crate::query::DateKind, dialect_name: &str, col_quoted: &str) -> String {
    use crate::query::DateKind;
    match (dialect_name, kind) {
        ("postgres", DateKind::Year) => format!("DATE_TRUNC('year', {col_quoted})::date"),
        ("postgres", DateKind::Month) => format!("DATE_TRUNC('month', {col_quoted})::date"),
        ("mysql", DateKind::Year) => format!("DATE(DATE_FORMAT({col_quoted}, '%Y-01-01'))"),
        ("mysql", DateKind::Month) => format!("DATE(DATE_FORMAT({col_quoted}, '%Y-%m-01'))"),
        ("postgres" | "mysql", DateKind::Day) => format!("DATE({col_quoted})"),
        (_, DateKind::Year) => format!("date(strftime('%Y-01-01', {col_quoted}))"),
        (_, DateKind::Month) => format!("date(strftime('%Y-%m-01', {col_quoted}))"),
        (_, DateKind::Day) => format!("date({col_quoted})"),
    }
}

/// Truncate `col_quoted` to a timestamp bucket that decodes as `DateTime<Utc>`.
fn datetime_trunc_sql(
    kind: crate::query::DateTimeKind,
    dialect_name: &str,
    col_quoted: &str,
) -> String {
    use crate::query::DateTimeKind as K;
    if dialect_name == "postgres" {
        let unit = match kind {
            K::Year => "year",
            K::Month => "month",
            K::Day => "day",
            K::Hour => "hour",
            K::Minute => "minute",
            K::Second => "second",
        };
        return format!("DATE_TRUNC('{unit}', {col_quoted})");
    }
    // MySQL spells minutes `%i` and seconds `%s`; SQLite `%M` / `%S`.
    let (min, sec) = if dialect_name == "mysql" {
        ("%i", "%s")
    } else {
        ("%M", "%S")
    };
    let fmt = match kind {
        K::Year => "%Y-01-01 00:00:00".to_owned(),
        K::Month => "%Y-%m-01 00:00:00".to_owned(),
        K::Day => "%Y-%m-%d 00:00:00".to_owned(),
        K::Hour => "%Y-%m-%d %H:00:00".to_owned(),
        K::Minute => format!("%Y-%m-%d %H:{min}:00"),
        K::Second => format!("%Y-%m-%d %H:{min}:{sec}"),
    };
    if dialect_name == "mysql" {
        // CAST back so the decoder sees a DATETIME.
        format!("CAST(DATE_FORMAT({col_quoted}, '{fmt}') AS DATETIME)")
    } else {
        format!("strftime('{fmt}', {col_quoted})")
    }
}
