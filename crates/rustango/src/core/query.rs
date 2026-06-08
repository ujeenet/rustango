//! Dialect-neutral query IR.
//!
//! The query crate compiles a typed `QuerySet<T>` into a [`SelectQuery`].
//! The SQL crate then walks that IR and writes a parameterized statement
//! per dialect. Anything in this module is therefore visible to both.

use super::expr::Expr;
use super::{validate::validate_value, ModelSchema, QueryError, SqlValue};

/// Comparison operator on a single column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    /// Right-hand side must be `SqlValue::List`.
    In,
    /// Right-hand side must be `SqlValue::List`. Emits `NOT IN (…)`.
    NotIn,
    /// Case-sensitive `LIKE`. Pattern characters live inside the bound value.
    Like,
    /// Case-sensitive `NOT LIKE`.
    NotLike,
    /// Case-insensitive `ILIKE` (Postgres).
    ILike,
    /// Case-insensitive `NOT ILIKE` (Postgres).
    NotILike,
    /// Range check. The bound value must be `SqlValue::List([lo, hi])`.
    /// Emits `col BETWEEN $lo AND $hi`.
    Between,
    /// Negated range check. Bound value shape matches [`Between`]
    /// (`SqlValue::List([lo, hi])`). Emits `col NOT BETWEEN $lo AND
    /// $hi`. Eloquent `whereNotBetween` parity.
    NotBetween,
    /// Compares against `NULL`. The bound value must be `SqlValue::Bool` —
    /// `true` means `IS NULL`, `false` means `IS NOT NULL`.
    IsNull,
    /// Null-safe equality: `IS DISTINCT FROM`. Unlike `<>`, this treats
    /// `NULL` as a comparable value — `NULL IS NOT DISTINCT FROM NULL` is
    /// `true`. Bind any `SqlValue`.
    IsDistinctFrom,
    /// Null-safe equality: `IS NOT DISTINCT FROM`. The inverse of
    /// [`IsDistinctFrom`](Op::IsDistinctFrom).
    IsNotDistinctFrom,
    /// JSONB `@>` — left operand contains the right operand. Bind a
    /// `SqlValue::Json` value.
    JsonContains,
    /// JSONB `<@` — left operand is contained by the right operand.
    JsonContainedBy,
    /// JSONB `?` — the text key exists as a top-level key. Bind a
    /// `SqlValue::String`.
    JsonHasKey,
    /// JSONB `?|` — any of the text keys exist. Bind a `SqlValue::List`
    /// of `SqlValue::String`.
    JsonHasAnyKey,
    /// JSONB `?&` — all of the text keys exist. Bind a `SqlValue::List`
    /// of `SqlValue::String`.
    JsonHasAllKeys,
    /// POSIX regex match — Django's `__regex` lookup. Case-sensitive.
    /// Bind a `SqlValue::String` containing the regex pattern.
    /// Tri-dialect: PG `~`, MySQL `REGEXP`, SQLite `REGEXP`. SQLite's
    /// `REGEXP` delegates to a `regexp(pattern, value)` user-function
    /// the connection must register — sqlx-sqlite does not register
    /// it automatically. Issue #26.
    Regex,
    /// POSIX regex non-match — Django's `~Q(name__regex=...)` shape
    /// rolled into a single op. PG `!~`, MySQL `NOT REGEXP`,
    /// SQLite `NOT REGEXP` (same user-function caveat as [`Op::Regex`]).
    /// Issue #26.
    NotRegex,
    /// Case-insensitive POSIX regex match — Django's `__iregex`.
    /// PG `~*` (native ASCII-folded match). MySQL and SQLite have
    /// no native case-insensitive regex operator, so the writer
    /// unconditionally wraps both sides in `LOWER(...)` for
    /// collation-independent ASCII case folding. Issue #26.
    IRegex,
    /// Case-insensitive POSIX regex non-match — Django's
    /// `~Q(name__iregex=...)`. PG `!~*`. MySQL and SQLite use
    /// `LOWER(<col>) NOT REGEXP LOWER(<pattern>)` for the same
    /// reason as [`Op::IRegex`]. Issue #26.
    NotIRegex,
    /// pg_trgm trigram similarity — Django's `__trigram_similar`
    /// lookup. PG emits `<col> % <pattern>` using the `%` operator
    /// supplied by the `pg_trgm` extension (default similarity
    /// threshold is `0.3`; adjust via `SET pg_trgm.similarity_threshold`).
    /// Requires `CREATE EXTENSION pg_trgm` on the database.
    /// **PG-only** — MySQL / SQLite have no equivalent and reject
    /// at compile time. Bind a `SqlValue::String`. Issue #29.
    TrigramSimilar,
    /// pg_trgm word-similarity — Django's `__trigram_word_similar`
    /// lookup. PG emits `<col> %> <pattern>` using the `%>` operator.
    /// Matches when any **word** in `<col>` is trigram-similar to the
    /// pattern (the bare `%` requires the WHOLE string be similar).
    /// **PG-only**, same `pg_trgm` extension requirement. Issue #29.
    TrigramWordSimilar,
    /// Postgres full-text search — Django's `__search` lookup. PG
    /// emits `to_tsvector(<col>) @@ plainto_tsquery(<pattern>)`,
    /// using the database's default text-search config (typically
    /// `english`). This is the simplest FTS shape — for explicit
    /// language config, weighted SearchVectors, or websearch-style
    /// query parsing, build the SearchVector / SearchQuery exprs
    /// directly (follow-up work for issue #28). Bind a
    /// `SqlValue::String`. **PG-only** — MySQL has `MATCH … AGAINST`
    /// and SQLite has FTS5 `MATCH`, both with incompatible semantics,
    /// so both reject at compile time. Issue #28.
    Search,
    /// Postgres array containment — Django's `__contains` lookup on
    /// `ArrayField`. PG emits `<col> @> <value>` where the rhs is
    /// the bound array literal. Returns rows whose array column
    /// fully contains every element of the value array. **PG-only**
    /// — MySQL and SQLite have no native array type and reject at
    /// compile time with `OpNotSupportedInDialect`. Issue #30.
    ArrayContains,
    /// Inverse of [`Self::ArrayContains`]: `<col> <@ <value>` —
    /// rows whose array column is fully contained by the value
    /// array. Django's `__contained_by` lookup. **PG-only**.
    /// Issue #30.
    ArrayContainedBy,
    /// Postgres array overlap — `<col> && <value>`. Rows whose
    /// array shares at least one element with the value array.
    /// Django's `__overlap` lookup. **PG-only**. Issue #30.
    ArrayOverlap,
    /// Postgres range containment — `<col> @> <value>`. Django's
    /// `__range_contains` lookup on a `RangeField`. The rhs is
    /// either a single element (range contains scalar) or a range
    /// literal (range contains range). Same SQL operator as
    /// [`Self::ArrayContains`] — separate enum variant keeps the
    /// call-site intent clear and lets the bind path stamp the
    /// right value shape (`SqlValue::RangeLiteral` for range-vs-
    /// range, scalar for range-vs-element). **PG-only** — MySQL
    /// and SQLite reject at compile time. Issue #31.
    RangeContains,
    /// Inverse of [`Self::RangeContains`]: `<col> <@ <value>`.
    /// Django's `__range_contained_by`. **PG-only**. Issue #31.
    RangeContainedBy,
    /// Range overlap — `<col> && <value>`. Rows whose range
    /// overlaps the value range. Django's `__range_overlap`.
    /// **PG-only**. Issue #31.
    RangeOverlap,
    /// Range strictly-left-of — `<col> << <value>`. Rows whose
    /// entire range falls below the value range. **PG-only**.
    /// Issue #31.
    RangeStrictlyLeft,
    /// Range strictly-right-of — `<col> >> <value>`. **PG-only**.
    /// Issue #31.
    RangeStrictlyRight,
    /// Range adjacent — `<col> -|- <value>`. Rows whose range
    /// abuts the value range (no overlap, no gap). **PG-only**.
    /// Issue #31.
    RangeAdjacent,
}

/// One predicate in a `WHERE` clause: `column <op> value`. Always
/// the leaf of a [`WhereExpr`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// `WHERE` predicate that compares two columns from the same row —
/// the rustango analog of Django's `F()` on the right side of a filter.
///
/// Emits `<left_col> <op> <right>` where `right` is an arbitrary
/// [`Expr`] (typically `Expr::Column` for a plain column-vs-column
/// compare, or a `BinOp` tree for column-vs-arithmetic). The lhs is
/// the model column being filtered on; the rhs lives in the `Expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnFilter {
    /// Left-hand column (the field being filtered on, schema-resolved).
    pub column: &'static str,
    /// Comparison operator. Subset of [`Op`] — only the binary
    /// comparison variants make sense here (`Eq`, `Ne`, `Lt`, `Lte`,
    /// `Gt`, `Gte`). Other ops (`In`, `Between`, `IsNull`, JSON ops, etc.)
    /// are rejected at compile/emit time.
    pub op: Op,
    /// Right-hand side. Most commonly `Expr::Column(other)` for the
    /// column-vs-column case; can be any expression tree.
    pub rhs: Expr,
}

/// Boolean expression in a `WHERE` clause — leaf [`Filter`]s composed
/// with `AND` / `OR` to arbitrary depth.
///
/// ```ignore
/// // a AND (b OR c)
/// WhereExpr::And(vec![
///     WhereExpr::Predicate(a),
///     WhereExpr::Or(vec![WhereExpr::Predicate(b), WhereExpr::Predicate(c)]),
/// ])
/// ```
///
/// Empty conjunctions and disjunctions are valid. By convention they
/// represent SQL `TRUE` and `FALSE` respectively, but you should
/// usually avoid building them — `WhereExpr::And(vec![])` is the
/// "no filters" case used internally to represent a query with an
/// unfiltered WHERE clause; the writer skips emitting `WHERE` for it.
/// `WhereExpr::Or(vec![])` is rejected by the writer as it would
/// silently match nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum WhereExpr {
    /// Leaf — a single column predicate.
    Predicate(Filter),
    /// Leaf — a column-vs-expression predicate (F() comparisons).
    ColumnCompare(ColumnFilter),
    /// All children must match. Empty list = vacuously true (no
    /// `WHERE` emitted by the writer).
    And(Vec<WhereExpr>),
    /// Any child must match. Empty list = vacuously false (rejected
    /// by the writer).
    Or(Vec<WhereExpr>),
    /// Logical negation. Emits `NOT (child)`.
    Not(Box<WhereExpr>),
    /// Logical XOR — Django `Q(a) ^ Q(b)` (added in Django 4.1). Issue
    /// #27. Matches rows for which an odd number of children evaluate
    /// to `true`. Binary form is the common case and emits the
    /// canonical SQL-92 rewrite `(a AND NOT b) OR (NOT a AND b)`; the
    /// N-ary form (3+) folds to a CASE-WHEN-1/0 sum compared `% 2 = 1`
    /// to mirror Django's "odd number of trues" semantic.
    ///
    /// Native logical XOR exists on MySQL but not on PG or SQLite —
    /// the rewrite is portable across every backend, so the writer
    /// uses it uniformly. Empty children = vacuously false (rejected
    /// by the writer, same as `Or(vec![])`). Single child is
    /// equivalent to the child itself.
    ///
    /// **Double-eval caveat (binary form only)**: the 2-child rewrite
    /// emits each operand twice, so the database evaluates each
    /// predicate twice. Deterministic predicates are unaffected, but
    /// volatile expressions (`RANDOM()`, `NOW()`, correlated subqueries
    /// with side effects) may return different values across the two
    /// evaluations. If single-evaluation semantics matter, force the
    /// parity-tally branch by adding a `FALSE` third child:
    /// `Xor([a, b, WhereExpr::And(vec![WhereExpr::Predicate(..)])])`
    /// or build via `a.xor(b).xor(c)` chains where the typed builder
    /// flattens to ≥3 children. The N-ary form evaluates each child
    /// exactly once.
    Xor(Vec<WhereExpr>),
    /// `EXISTS (<subquery>)` — issue #5. True when the inner
    /// `SelectQuery` returns at least one row. Boxed because
    /// `SelectQuery` already carries its own `WhereExpr`, which would
    /// make the enum size unbounded otherwise.
    Exists(Box<SelectQuery>),
    /// `NOT EXISTS (<subquery>)` — issue #5. Django's `~Exists` shorthand.
    NotExists(Box<SelectQuery>),
    /// `<col> IN (<subquery>)` / `<col> NOT IN (<subquery>)` — issue
    /// #5. Sibling to `Op::In` / `Op::NotIn` over a literal list, but
    /// the RHS is a correlated or non-correlated SELECT.
    InSubquery {
        column: &'static str,
        negated: bool,
        subquery: Box<SelectQuery>,
    },
    /// `<lhs-expr> <op> <rhs-expr>` — both sides arbitrary [`Expr`]s
    /// (issue #80). Used inside JOIN `ON` predicates where one or
    /// both sides typically need to be qualified with a table alias
    /// (via [`Expr::AliasedColumn`]) — outside JOIN context the
    /// narrower [`ColumnFilter`] variant remains the right tool.
    ///
    /// Only the binary-comparison ops (`Eq`, `Ne`, `Lt`, `Lte`,
    /// `Gt`, `Gte`) make sense here; the writer rejects other ops
    /// the same way it does for `ColumnFilter`.
    ExprCompare { lhs: Expr, op: Op, rhs: Expr },
}

impl WhereExpr {
    /// `true` when this expression carries no predicates (i.e. an
    /// empty `And`). Used by the writer to skip emitting `WHERE`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::And(items) if items.is_empty())
    }

    /// Build an AND of leaf filters. Convenience for the common case
    /// of "a list of predicates joined with AND" (the legacy
    /// `Vec<Filter>` shape).
    #[must_use]
    pub fn and_predicates(filters: Vec<Filter>) -> Self {
        Self::And(filters.into_iter().map(Self::Predicate).collect())
    }

    /// Append an AND predicate. If `self` is already `And(_)`, the
    /// child is pushed in place; otherwise `self` is wrapped in a new
    /// `And` together with the new child.
    pub fn push_and(&mut self, child: Self) {
        match self {
            Self::And(items) => items.push(child),
            _ => {
                let prev = std::mem::replace(self, Self::And(Vec::new()));
                if let Self::And(items) = self {
                    items.push(prev);
                    items.push(child);
                }
            }
        }
    }

    /// If this expression is a flat AND of leaf predicates (or a
    /// single `Predicate`), return the predicate list. Returns `None`
    /// for any tree containing `Or` or nested `And`. Useful for
    /// callers that want to inspect a legacy "AND-only" WHERE without
    /// pattern-matching the full tree.
    #[must_use]
    pub fn as_flat_and(&self) -> Option<Vec<&Filter>> {
        match self {
            Self::Predicate(f) => Some(vec![f]),
            Self::And(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Self::Predicate(f) => out.push(f),
                        _ => return None,
                    }
                }
                Some(out)
            }
            // ColumnCompare is a leaf predicate but it doesn't carry a
            // `Filter` (the rhs is an `Expr`, not a `SqlValue`), so the
            // flat-AND view can't surface it as a `&Filter` reference.
            // Callers using `as_flat_and` only handle literal `Filter`
            // predicates anyway. Subquery-shaped predicates (#5) also
            // fall outside the legacy flat-AND view.
            Self::ColumnCompare(_)
            | Self::Or(_)
            | Self::Xor(_)
            | Self::Not(_)
            | Self::Exists(_)
            | Self::NotExists(_)
            | Self::InSubquery { .. }
            | Self::ExprCompare { .. } => None,
        }
    }

    /// Walk the tree and validate every leaf predicate against `model`.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] for a predicate whose
    /// column is missing from the model, propagated up through
    /// composite nodes.
    pub fn validate(&self, model: &'static ModelSchema) -> Result<(), QueryError> {
        match self {
            Self::Predicate(f) => {
                if model.field_by_column(f.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: f.column.to_owned(),
                    });
                }
                Ok(())
            }
            Self::ColumnCompare(cf) => {
                if model.field_by_column(cf.column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: cf.column.to_owned(),
                    });
                }
                // Validate every column reference inside the rhs Expr
                // tree against the model schema.
                validate_expr_columns(model, &cf.rhs)?;
                Ok(())
            }
            Self::And(items) | Self::Or(items) | Self::Xor(items) => {
                for child in items {
                    child.validate(model)?;
                }
                Ok(())
            }
            Self::Not(child) => child.validate(model),
            // Subquery predicates (#5) validate their inner SELECT
            // against the inner SELECT's own model — that walk runs
            // when the inner queryset was compiled. From the outer
            // model's perspective there is nothing to check beyond
            // the column on the LHS of `InSubquery`.
            Self::Exists(_) | Self::NotExists(_) => Ok(()),
            Self::InSubquery { column, .. } => {
                if model.field_by_column(column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*column).to_owned(),
                    });
                }
                Ok(())
            }
            // ExprCompare is used inside JOIN ON predicates where both
            // sides typically carry their own table alias via
            // `Expr::AliasedColumn`. Validating against a single
            // `model` would either be wrong (mismatching alias) or
            // duplicate work (the alias-side schema isn't reachable
            // here). Surface column typos at runtime; the JOIN writer
            // emits clean SQL on the happy path.
            Self::ExprCompare { .. } => Ok(()),
        }
    }
}

/// Recursively walk an [`Expr`] and confirm every `Column` reference
/// resolves on `model`. Literals + arithmetic ops are passed through.
fn validate_expr_columns(model: &'static ModelSchema, expr: &Expr) -> Result<(), QueryError> {
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
            validate_expr_columns(model, left)?;
            validate_expr_columns(model, right)
        }
        Expr::Function { args, .. } => {
            for a in args {
                validate_expr_columns(model, a)?;
            }
            Ok(())
        }
        Expr::Cast { expr: inner, .. } => validate_expr_columns(model, inner),
        Expr::Case { branches, default } => {
            for b in branches {
                b.condition.validate(model)?;
                validate_expr_columns(model, &b.then)?;
            }
            if let Some(d) = default {
                validate_expr_columns(model, d)?;
            }
            Ok(())
        }
        // Inner subquery validates against its own model when the
        // user compiles it; nothing to check from the outer model
        // beyond that. `OuterRef` names a column on the outer
        // model — and from the perspective of the inner-walk it's
        // a free name; the outer query's compile() validates it
        // against the outer schema when it embeds this subquery.
        // `AliasedColumn` (issue #80) carries its own table alias so
        // it doesn't resolve against the passed-in model.
        // `AggregateSubquery` (issue #830) wraps an AggregateQuery over
        // the *child* model; its columns resolve there, and any
        // `OuterRef` names an outer column validated when the outer
        // query embeds it — nothing to check against this model.
        Expr::Subquery(_)
        | Expr::AggregateSubquery(_)
        | Expr::OuterRef(_)
        | Expr::AliasedColumn { .. } => Ok(()),
        // Window (issue #7) — args / partition_by / order_by all
        // reference the outer model's columns. Validate them.
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
                validate_expr_columns(model, arg)?;
            }
            Ok(())
        }
        // Aggregate (issue #74) — bare-column args (Sum("col"), etc.)
        // hold raw `&'static str` names that this validator doesn't
        // visit today; that's a pre-existing gap. Window-shaped
        // aggregates are validated via the dedicated walker called
        // from AggregateBuilder::compile().
        Expr::Aggregate(_) => Ok(()),
        // JsonPath (issue #296 / T2.3) — only the `source` expression
        // references a model column; path steps are JSON-pointer
        // keys/indices.
        Expr::JsonPath { source, .. } => validate_expr_columns(model, source),
    }
}

impl Default for WhereExpr {
    fn default() -> Self {
        Self::And(Vec::new())
    }
}

impl From<Filter> for WhereExpr {
    fn from(f: Filter) -> Self {
        Self::Predicate(f)
    }
}

/// Compiled `SELECT` over a single model with an optional WHERE
/// clause expressed as a [`WhereExpr`] tree.
///
/// v0.7 ships full AND/OR/nested support. The legacy "flat AND of
/// predicates" shape is `WhereExpr::and_predicates(filters)` for
/// callers who built up a `Vec<Filter>` directly.
///
/// `limit` and `offset` are `None` by default and emit no clauses.
/// `search`, when present, adds a parenthesized `(col ILIKE $N OR …)`
/// clause AND-joined with `where_clause`. `joins` adds `LEFT JOIN`
/// clauses and pulls extra columns into the projection under aliased
/// names.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    pub search: Option<SearchClause>,
    pub joins: Vec<Join>,
    /// Derived-table joins — `JOIN [LATERAL] (<subquery>) AS alias ON …`
    /// (Eloquent `joinSub` / `joinLateral`, issue #828). Emitted in the
    /// `FROM` clause after the model [`Self::joins`]. Empty by default.
    pub subquery_joins: Vec<SubqueryJoin>,
    /// `ORDER BY` clauses, in the order they should appear in SQL.
    /// Slice 9.0b + issue #76. Emitted after WHERE / JOIN / GROUP BY
    /// but before LIMIT / OFFSET. Empty = no `ORDER BY`.
    pub order_by: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Row-lock mode appended after LIMIT/OFFSET — Django's
    /// `select_for_update(skip_locked=, of=, nowait=, no_key=)`. Issue
    /// #21. `None` (default) emits no lock clause. Must run inside a
    /// transaction on PG and MySQL; SQLite has no row-level lock
    /// syntax (transaction-scope locks are implicit), so the writer
    /// no-ops on that backend.
    pub lock_mode: Option<LockMode>,
    /// Set-algebra branches that combine with this query — Django's
    /// `.union(other_qs, all=)` / `.intersection(other_qs)` /
    /// `.difference(other_qs)`. Issue #25. Empty (default) emits a
    /// plain `SELECT …`. Non-empty wraps every branch in parens and
    /// joins them with the matching keyword:
    ///
    /// ```text
    /// (SELECT … this query …)
    /// UNION [ALL] | INTERSECT | EXCEPT
    /// (SELECT … branch_1 …)
    /// …
    /// ORDER BY …      ← outer order_by applies to the whole compound
    /// LIMIT N         ← outer limit/offset apply to the combined result
    /// ```
    ///
    /// Each branch keeps its own WHERE / ORDER BY / LIMIT inside the
    /// parens. The compound's outer `order_by` / `limit` / `offset` /
    /// `lock_mode` apply to the merged result.
    pub compound: Vec<CompoundBranch>,
    /// Column-list override for pure projection — Django's `.values()`
    /// / `.values_list()` shape (issue #22). `None` (default) emits
    /// every scalar field on the model; `Some(cols)` emits exactly
    /// `cols` in the order given. Joins still contribute their
    /// `project` columns. Validated at builder time so every column
    /// resolves on the model schema.
    pub projection: Option<Vec<&'static str>>,
    /// Django `.distinct(*fields)` — issue #264 / T1.2. `None` is the
    /// default (no DISTINCT clause). `Some(DistinctMode::All)` emits
    /// `SELECT DISTINCT ...`. `Some(DistinctMode::On(cols))` emits PG
    /// `SELECT DISTINCT ON (cols) ...` natively; on MySQL/SQLite the
    /// writer wraps the query in a `ROW_NUMBER() OVER (PARTITION BY
    /// cols ORDER BY <order_by>) AS __rn` subquery with an outer
    /// `WHERE __rn = 1` so the "first row per group" semantics carry.
    pub distinct: Option<DistinctMode>,
}

impl SelectQuery {
    /// Construct an empty `SelectQuery` against `model` — every
    /// non-required field defaults to its identity value
    /// (`WhereExpr::And(vec![])` = vacuously true, empty Vecs,
    /// `None` Options). Callers layer the actual filter / order /
    /// limit on top via the struct-update shorthand:
    ///
    /// ```ignore
    /// use rustango::core::SelectQuery;
    ///
    /// let q = SelectQuery {
    ///     limit: Some(10),
    ///     ..SelectQuery::new(MyModel::SCHEMA)
    /// };
    /// ```
    ///
    /// #562 — the 11-field struct literal recurs verbatim in ~50
    /// call sites. Every time a new field gets added (`distinct`,
    /// `compound`, ...), all 50 sites have to be touched. Using
    /// `SelectQuery::new(...)` + struct-update syntax means new
    /// fields just slot into the constructor and existing callers
    /// compile unchanged.
    #[must_use]
    pub fn new(model: &'static ModelSchema) -> Self {
        Self {
            model,
            // `And(vec![])` is the writer's vacuously-true shape
            // (`Or(vec![])` is rejected) — see `WhereExpr` doc.
            where_clause: WhereExpr::And(Vec::new()),
            search: None,
            joins: Vec::new(),
            subquery_joins: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            lock_mode: None,
            compound: Vec::new(),
            projection: None,
            distinct: None,
        }
    }

    /// Construct a single-PK-lookup `SelectQuery` — the most
    /// common shape across the framework (`admin/views.rs`,
    /// `template_views.rs`, `viewset/mod.rs`).
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// SelectQuery {
    ///     where_clause: WhereExpr::Predicate(Filter {
    ///         column: pk_column,
    ///         op: Op::Eq,
    ///         value: pk_value,
    ///     }),
    ///     limit: Some(1),
    ///     ..SelectQuery::new(model)
    /// }
    /// ```
    ///
    /// Use when the WHERE shape is a single `<pk_column> = <value>`
    /// and you want one row back.
    #[must_use]
    pub fn by_pk(model: &'static ModelSchema, pk_column: &'static str, pk_value: SqlValue) -> Self {
        Self {
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::Eq,
                value: pk_value,
            }),
            limit: Some(1),
            ..Self::new(model)
        }
    }

    /// Multi-PK `IN (...)` lookup. Companion to [`by_pk`] for the
    /// bulk-fetch / bulk-delete-by-id shapes that recur in
    /// `template_views::run_delete_selected_pool`, FK display fetches,
    /// and "fetch rows for these N pks" routines.
    ///
    /// Equivalent to:
    ///
    /// ```ignore
    /// SelectQuery {
    ///     where_clause: WhereExpr::Predicate(Filter {
    ///         column: pk_column,
    ///         op: Op::In,
    ///         value: SqlValue::List(pk_values),
    ///     }),
    ///     ..SelectQuery::new(model)
    /// }
    /// ```
    ///
    /// No `LIMIT` defaulted — caller can layer one on via struct-update
    /// syntax if needed (`SelectQuery { limit: Some(50),
    /// ..SelectQuery::by_pk_in(...) }`).
    #[must_use]
    pub fn by_pk_in(
        model: &'static ModelSchema,
        pk_column: &'static str,
        pk_values: Vec<SqlValue>,
    ) -> Self {
        Self {
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::In,
                value: SqlValue::List(pk_values),
            }),
            ..Self::new(model)
        }
    }
}

/// Distinct mode — Django's `.distinct()` / `.distinct(*fields)`.
/// Issue #264 / T1.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistinctMode {
    /// `SELECT DISTINCT ...` — works on every dialect identically.
    All,
    /// `SELECT DISTINCT ON (cols) ...` — PG-native syntax with portable
    /// `ROW_NUMBER()` fallback on MySQL / SQLite. Empty `cols` is
    /// rejected at builder time (would degenerate to `DISTINCT`).
    On(Vec<&'static str>),
}

/// One branch of a set-algebra compound query. Issue #25.
#[derive(Debug, Clone)]
pub struct CompoundBranch {
    /// `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`.
    pub op: SetOp,
    /// The branch itself — a complete `SelectQuery` whose projection
    /// must match the outer query's column shape (same model).
    pub query: Box<SelectQuery>,
}

/// SQL set-algebra operator — Django's
/// `QuerySet.union(all=)` / `.intersection()` / `.difference()`.
/// Issue #25.
///
/// Tri-dialect availability:
/// - **Postgres**: all four ops.
/// - **SQLite**: all four ops.
/// - **MySQL 8.0+**: `UNION` / `UNION ALL` only. `INTERSECT` / `EXCEPT`
///   landed in MySQL 8.0.31; older versions return a syntax error
///   from the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION` — combine + deduplicate.
    Union,
    /// `UNION ALL` — combine, keep duplicates. Cheaper than `UNION`
    /// because no DISTINCT pass.
    UnionAll,
    /// `INTERSECT` — rows present in every branch.
    Intersection,
    /// `EXCEPT` — rows in the first branch but not the others
    /// (Django's `.difference()`).
    Difference,
}

impl SetOp {
    /// SQL keyword for this operator.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Union => "UNION",
            Self::UnionAll => "UNION ALL",
            Self::Intersection => "INTERSECT",
            Self::Difference => "EXCEPT",
        }
    }
}

/// Manual PartialEq for `CompoundBranch` — nests `SelectQuery` (which
/// has its own ptr-eq impl on `ModelSchema`).
impl PartialEq for CompoundBranch {
    fn eq(&self, other: &Self) -> bool {
        self.op == other.op && self.query == other.query
    }
}

/// `SELECT … FOR UPDATE` row-lock options — Django's
/// `QuerySet.select_for_update(skip_locked=, nowait=, of=, no_key=)`.
/// Issue #21.
///
/// `#[non_exhaustive]` — future per-backend lock flags (e.g. PG's
/// `FOR KEY SHARE`, MySQL's `LOCK IN SHARE MODE`) can be added without
/// breaking downstream code that constructs `LockMode { … }` directly.
/// Build via [`LockMode::default`] + field assignment, or chain the
/// [`crate::query::QuerySet`] builder methods (`.select_for_update()`,
/// `.skip_locked()`, `.nowait()`, `.no_key()`, `.of(…)`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockMode {
    /// PG 9.3+: `FOR NO KEY UPDATE` instead of `FOR UPDATE`. Holds a
    /// weaker lock that doesn't block other writers that aren't
    /// touching the row's PK / unique columns. MySQL has no
    /// equivalent — the writer falls back to `FOR UPDATE`.
    pub no_key: bool,
    /// PG / MySQL 8+: `SKIP LOCKED`. Rows currently locked by another
    /// transaction are silently filtered out instead of waiting.
    /// Canonical "claim next available row" pattern.
    pub skip_locked: bool,
    /// PG / MySQL 8+: `NOWAIT`. Returns an error immediately if any
    /// row in the result set is currently locked. Mutually exclusive
    /// with `skip_locked` at the database level — if both are set,
    /// the writer emits `SKIP LOCKED` (the more permissive option).
    pub nowait: bool,
    /// PG 9.3+: `FOR UPDATE OF table1, table2, …`. Restricts the lock
    /// to the named tables when the query JOINs. Aliases / table
    /// names go in; empty vec emits no `OF` clause. MySQL accepts
    /// `OF` since 8.0.1. SQLite no-op.
    pub of: Vec<&'static str>,
    /// Suppress the `tracing::warn!` the writer emits when this
    /// `LockMode` lands on a SQLite query. Issue #290 / T2.9. SQLite
    /// has no row-level lock syntax — locks are silently dropped, and
    /// the writer logs at warn-level by default so users notice. Set
    /// this `true` on test fixtures or single-writer apps where you
    /// know the SQLite global writer lock is sufficient.
    pub silent_on_sqlite: bool,
}

/// PartialEq for `SelectQuery` — needed so [`crate::core::Expr`] (which
/// embeds `Box<SelectQuery>` for issue #5 subqueries) can keep its
/// `#[derive(PartialEq)]`. `ModelSchema` doesn't implement `PartialEq`
/// (its fields are heterogeneous + behind static refs), so the `model`
/// pointer is compared by identity — two queries against the same
/// schema register equal, which matches every legitimate use case
/// since model schemas are singletons.
impl PartialEq for SelectQuery {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.model, other.model)
            && self.where_clause == other.where_clause
            && self.search == other.search
            && self.joins == other.joins
            && self.subquery_joins == other.subquery_joins
            && self.order_by == other.order_by
            && self.limit == other.limit
            && self.offset == other.offset
            && self.lock_mode == other.lock_mode
            && self.compound == other.compound
            && self.projection == other.projection
    }
}

/// Same ptr-eq treatment for `Join` — it also holds `&'static
/// ModelSchema` (the join target).
impl PartialEq for Join {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.target, other.target)
            && self.alias == other.alias
            && self.kind == other.kind
            && self.on == other.on
            && self.project == other.project
    }
}

/// Single column in an `ORDER BY` clause. Slice 9.0b — the simple
/// "field name + ASC/DESC" form that pre-dates [`OrderItem`].
///
/// New code should prefer [`OrderItem`] (issue #76) which adds Expr
/// items and `NULLS FIRST/LAST` control. `OrderClause` remains as a
/// convenience-constructor and converts via `Into<OrderItem>` for
/// every `SelectQuery` / `AggregateQuery` / window-OVER ORDER BY slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    /// SQL column name on the main table — already resolved by
    /// `QuerySet::order_by` from a Rust-side field name (so the
    /// writer doesn't re-walk the schema).
    pub column: &'static str,
    /// `true` for `DESC`, `false` for the default `ASC`.
    pub desc: bool,
}

/// Where NULLs sort relative to non-NULL values. Issue #76.
///
/// Default semantics differ per dialect: PG and SQLite sort NULLs
/// LAST on `ASC` and FIRST on `DESC` (the SQL-standard); MySQL
/// treats NULLs as smaller than every value so they land FIRST on
/// `ASC` and LAST on `DESC`. Pinning the order explicitly via
/// `First` or `Last` produces consistent behavior across all three
/// dialects (the writer emits `IFNULL(…)` workarounds on MySQL,
/// which has no native `NULLS FIRST`/`NULLS LAST` keywords).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullsOrder {
    /// Backend's native default — emits no `NULLS …` clause.
    #[default]
    Default,
    /// `NULLS FIRST` on PG/SQLite; emulated via `IFNULL(col, '') = ''
    /// DESC, …` style trick on MySQL.
    First,
    /// `NULLS LAST` on PG/SQLite; emulated on MySQL similarly.
    Last,
}

/// One item in a generalized `ORDER BY` list (issue #76). Carries
/// either a plain column reference (with optional NULL-ordering) or
/// an arbitrary [`Expr`] — `lower(col)`, `case(…)`, `F(a) + F(b)`,
/// any builder result that lowers to `Expr`.
///
/// Old-shape `OrderClause` values convert into the `Column` variant
/// via `Into<OrderItem>` so existing constructors keep working.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderItem {
    /// `<col> [DESC] [NULLS FIRST|LAST]`. Equivalent of the legacy
    /// `OrderClause` with the addition of `NullsOrder`.
    Column {
        column: &'static str,
        desc: bool,
        nulls: NullsOrder,
    },
    /// `<expr> [DESC] [NULLS FIRST|LAST]`. The `expr` is emitted via
    /// the standard `Expr` writer, so function calls / `CASE` /
    /// arithmetic / etc. all compose.
    Expr {
        expr: Expr,
        desc: bool,
        nulls: NullsOrder,
    },
    /// `ORDER BY RANDOM()` (PG / SQLite) or `ORDER BY RAND()` (MySQL).
    /// Issue #77 — Django's `.order_by('?')` shape. No direction, no
    /// NULLS clause: random ordering is by definition unordered, and
    /// the random value is computed per-row so there are no NULLs.
    ///
    /// **Performance**: forces a full table scan + in-memory sort
    /// by a per-row random key. The optimizer can't use an index.
    /// For large tables, prefer a `WHERE pk >= rand_offset LIMIT N`
    /// pattern instead.
    Random,
}

impl From<OrderClause> for OrderItem {
    fn from(c: OrderClause) -> Self {
        Self::Column {
            column: c.column,
            desc: c.desc,
            nulls: NullsOrder::Default,
        }
    }
}

impl OrderItem {
    /// Convenience for the most-common case — `<col> [DESC]`, default
    /// NULL ordering.
    #[must_use]
    pub fn column(column: &'static str, desc: bool) -> Self {
        Self::Column {
            column,
            desc,
            nulls: NullsOrder::Default,
        }
    }

    /// Convenience for `<col> [DESC] [NULLS FIRST|LAST]`.
    #[must_use]
    pub fn column_with_nulls(column: &'static str, desc: bool, nulls: NullsOrder) -> Self {
        Self::Column {
            column,
            desc,
            nulls,
        }
    }

    /// Convenience for `<expr> [DESC]` with default NULL ordering.
    #[must_use]
    pub fn expr(expr: Expr, desc: bool) -> Self {
        Self::Expr {
            expr,
            desc,
            nulls: NullsOrder::Default,
        }
    }

    /// Convenience for `<expr> [DESC] [NULLS FIRST|LAST]`.
    #[must_use]
    pub fn expr_with_nulls(expr: Expr, desc: bool, nulls: NullsOrder) -> Self {
        Self::Expr { expr, desc, nulls }
    }

    /// Construct a `Random` item — `ORDER BY RANDOM()` / `RAND()`.
    /// Issue #77.
    #[must_use]
    pub fn random() -> Self {
        Self::Random
    }

    /// Bare column name when this item is a `Column` variant; `None`
    /// for `Expr` / `Random` variants. Used by callers that pre-dated
    /// the enum (admin / template_views / older tests).
    #[must_use]
    pub fn column_name(&self) -> Option<&'static str> {
        match self {
            Self::Column { column, .. } => Some(column),
            Self::Expr { .. } | Self::Random => None,
        }
    }

    /// `true` if this item sorts descending. `Random` ordering has no
    /// direction (it's unordered by definition) — returns `false`.
    #[must_use]
    pub fn is_desc(&self) -> bool {
        match self {
            Self::Column { desc, .. } | Self::Expr { desc, .. } => *desc,
            Self::Random => false,
        }
    }

    /// The `NullsOrder` setting for this item. `Random` returns
    /// `Default` — the random key is per-row and non-NULL, so the
    /// clause has no effect.
    #[must_use]
    pub fn nulls_order(&self) -> NullsOrder {
        match self {
            Self::Column { nulls, .. } | Self::Expr { nulls, .. } => *nulls,
            Self::Random => NullsOrder::Default,
        }
    }
}

/// Which SQL `JOIN` keyword the writer should emit. Issue #80.
///
/// `Left` is the default — it matches the original FK-driven
/// `select_related` semantics where every outer row is preserved
/// regardless of whether a related row exists on the target side.
/// `Inner` is the common ad-hoc-join shape (drop outer rows with no
/// match). `Right` and `Full` are accepted by the IR but only emit
/// successfully on dialects that support them — the writer raises
/// [`crate::sql::SqlError::JoinKindNotSupported`] on attempts to
/// emit `Right` on SQLite or `Full` on MySQL / SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinKind {
    Inner,
    #[default]
    Left,
    Right,
    Full,
}

/// A JOIN against a target model. Issue #80 generalized this from
/// the original FK-only `LEFT JOIN main.fk = alias.target_pk` shape
/// to carry an arbitrary `WhereExpr` predicate + an explicit
/// [`JoinKind`].
///
/// The writer emits `<kind> JOIN "<target.table>" AS "<alias>" ON
/// <on>` and includes each `project` column in the SELECT list
/// aliased as `"<alias>"."<col>" AS "<alias>__<col>"`. Callers read
/// joined values from the resulting row by the suffixed name.
///
/// When a `SelectQuery` has any joins, the writer also qualifies the
/// main table's columns as `"<table>"."<col>"` to avoid ambiguity.
/// Cross-table column references inside `on` use
/// [`Expr::AliasedColumn`] for explicit `<alias>.<col>` qualification.
///
/// [`Expr::AliasedColumn`]: crate::core::Expr::AliasedColumn
#[derive(Debug, Clone)]
pub struct Join {
    pub target: &'static ModelSchema,
    pub alias: &'static str,
    pub kind: JoinKind,
    pub on: WhereExpr,
    pub project: Vec<&'static str>,
}

/// A JOIN whose right-hand side is a **derived table** (a subquery)
/// rather than a model table — Eloquent's `joinSub` / `leftJoinSub`, and
/// (with `lateral = true`) `joinLateral` / `leftJoinLateral`. Issue #828.
///
/// The writer emits `<kind> JOIN [LATERAL] (<subquery>) AS "<alias>" ON
/// <on>`. The derived table's columns are referenced from `on` (and from
/// the outer query) via [`Expr::AliasedColumn`] (`"<alias>"."<col>"`) —
/// there's no model schema to resolve unqualified names against, so the
/// `on` predicate must qualify every column.
///
/// Unlike [`Join`], a `SubqueryJoin` contributes **no** columns to the
/// SELECT projection: it's a filtering / correlation join, so a typed
/// fetch still decodes the base model. Read derived values with an
/// explicit `.values()` projection if needed.
///
/// `lateral` requests the `LATERAL` keyword, which lets the subquery
/// reference columns from earlier `FROM` items (e.g. the outer table) —
/// the "top-N rows per group" shape. **PG + MySQL ≥ 8.0.14 only**; the
/// writer raises [`crate::sql::SqlError::LateralJoinNotSupported`] on
/// SQLite.
///
/// [`Expr::AliasedColumn`]: crate::core::Expr::AliasedColumn
#[derive(Debug, Clone, PartialEq)]
pub struct SubqueryJoin {
    /// The derived table — a compiled `SELECT`.
    pub subquery: Box<SelectQuery>,
    /// Alias the derived table is exposed under (`AS "<alias>"`).
    pub alias: &'static str,
    /// `INNER` or `LEFT` (the only kinds meaningful for a derived-table
    /// join; the QuerySet builders only construct these two).
    pub kind: JoinKind,
    /// Join predicate. References the derived table as `"<alias>"."<col>"`
    /// and the outer table by its own name — both via
    /// [`Expr::AliasedColumn`]. For a `LATERAL` join the correlation
    /// usually lives in the subquery's own `WHERE`, so `on` is left empty
    /// (`WhereExpr::And(vec![])`) and the writer emits `ON true`.
    pub on: WhereExpr,
    /// Emit the `LATERAL` keyword (PG / MySQL only).
    pub lateral: bool,
}

/// `(col1 ILIKE %q% OR col2 ILIKE %q% …)` — single-parameter case-insensitive
/// substring match across multiple columns. Used by the admin's `?q=…` box.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    /// SQL columns to search across. Empty = no clause emitted.
    pub columns: Vec<&'static str>,
    /// User-supplied query text. The writer wraps it in `%…%` for `ILIKE`.
    pub query: String,
}

/// Compiled `INSERT` of a single row.
///
/// `columns` and `values` are positional: `values[i]` binds to `columns[i]`.
/// Conflict resolution for `INSERT … ON CONFLICT` (Postgres-specific).
///
/// Attach to [`InsertQuery::on_conflict`] or [`BulkInsertQuery::on_conflict`]
/// to control what happens when the insert would violate a unique constraint.
#[derive(Debug, Clone)]
pub enum ConflictClause {
    /// `ON CONFLICT DO NOTHING` — silently skip duplicate rows.
    DoNothing,
    /// `ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col` for each
    /// column in `update_columns`. `target` names the column(s) whose
    /// uniqueness constraint defines the conflict (typically the PK or a
    /// `#[rustango(unique)]` column).
    DoUpdate {
        target: Vec<&'static str>,
        update_columns: Vec<&'static str>,
    },
}

/// `returning` names columns the writer should append after `RETURNING` —
/// used for `Auto<T>` PKs, where the row is inserted with the column
/// omitted so Postgres' sequence DEFAULT fires, and the assigned value
/// is then read back into the model.
#[derive(Debug, Clone)]
pub struct InsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub values: Vec<SqlValue>,
    /// Columns to emit in a `RETURNING` clause. Empty = no clause; the
    /// executor uses `execute()`. Non-empty = the executor uses
    /// `fetch_one()` and the caller reads the returned row.
    pub returning: Vec<&'static str>,
    /// Optional `ON CONFLICT` clause. `None` = plain INSERT with no
    /// conflict handling (errors on constraint violation).
    pub on_conflict: Option<ConflictClause>,
}

impl InsertQuery {
    /// Walk each `(column, value)` pair and check it against the field's
    /// declared bounds (`max_length`, `min`, `max`).
    ///
    /// # Errors
    /// Returns [`QueryError::MaxLengthExceeded`] or [`QueryError::OutOfRange`]
    /// for any violating value, or [`QueryError::UnknownField`] if a column
    /// in the IR doesn't correspond to any field in `model`.
    pub fn validate(&self) -> Result<(), QueryError> {
        for (column, value) in self.columns.iter().zip(self.values.iter()) {
            let field =
                self.model
                    .field_by_column(column)
                    .ok_or_else(|| QueryError::UnknownField {
                        model: self.model.name,
                        field: (*column).to_owned(),
                    })?;
            validate_value(self.model.name, field, value)?;
        }
        Ok(())
    }
}

/// Compiled multi-row `INSERT` — one round-trip for N rows.
///
/// `rows[i]` is positional against `columns`: every row supplies the
/// same column list in the same order. `returning` works the same way
/// as on [`InsertQuery`]; non-empty means the executor uses
/// `fetch_all` and returns one row per input row.
///
/// Mixed-shape inserts (some rows opting a column out via the
/// Postgres `DEFAULT` keyword) are not supported in v0.4 — every row
/// must carry a value for every column. Models with `Auto<T>` PKs
/// can either pass `Auto::Unset` for every row (the macro drops the
/// Auto column from `columns` entirely and the sequence fires) or
/// `Auto::Set(v)` for every row (the column is included with the
/// supplied value). Mixed Set/Unset within one bulk_insert is
/// rejected by the macro at validate time.
#[derive(Debug, Clone)]
pub struct BulkInsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub rows: Vec<Vec<SqlValue>>,
    pub returning: Vec<&'static str>,
    /// Optional `ON CONFLICT` clause applied to every row in the batch.
    pub on_conflict: Option<ConflictClause>,
}

impl BulkInsertQuery {
    /// Chainable builder — Django's `bulk_create(update_conflicts=True,
    /// unique_fields=..., update_fields=...)`. Sets the `on_conflict`
    /// clause to `DoUpdate { target, update_columns }`. Issue #267 / T1.5.
    ///
    /// Per-dialect emission (handled by the writer):
    /// * Postgres / SQLite (3.24+): `ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col`
    /// * MySQL: `ON DUPLICATE KEY UPDATE col = VALUES(col)` — target is
    ///   not emitted (MySQL matches on every UNIQUE index automatically).
    #[must_use]
    pub fn on_conflict_do_update(
        mut self,
        target: &[&'static str],
        update_columns: &[&'static str],
    ) -> Self {
        self.on_conflict = Some(ConflictClause::DoUpdate {
            target: target.to_vec(),
            update_columns: update_columns.to_vec(),
        });
        self
    }

    /// Chainable builder — Django's `bulk_create(ignore_conflicts=True)`.
    /// Sets the `on_conflict` clause to `DoNothing`. Issue #267 / T1.5.
    ///
    /// Per-dialect emission:
    /// * Postgres / SQLite: `ON CONFLICT DO NOTHING`
    /// * MySQL: `ON DUPLICATE KEY UPDATE <pivot> = <pivot>` — the
    ///   no-op write trick.
    #[must_use]
    pub fn on_conflict_do_nothing(mut self) -> Self {
        self.on_conflict = Some(ConflictClause::DoNothing);
        self
    }
}

impl BulkInsertQuery {
    /// Walk every `(column, value)` pair in every row and check it
    /// against the field's declared bounds.
    ///
    /// # Errors
    /// As [`InsertQuery::validate`].
    pub fn validate(&self) -> Result<(), QueryError> {
        for row in &self.rows {
            for (column, value) in self.columns.iter().zip(row.iter()) {
                let field =
                    self.model
                        .field_by_column(column)
                        .ok_or_else(|| QueryError::UnknownField {
                            model: self.model.name,
                            field: (*column).to_owned(),
                        })?;
                validate_value(self.model.name, field, value)?;
            }
        }
        Ok(())
    }
}

/// One `column = value` pair in an `UPDATE ... SET ...` clause.
///
/// `value` is an [`Expr`] — it can be a literal (most common, `Expr::Literal`),
/// a column reference (`Expr::Column` / `F("col")` — column-to-column copy),
/// or an arithmetic tree (`F("col") + 1` — Django's atomic counter pattern).
///
/// Existing call sites that pass an [`SqlValue`] lift transparently
/// via `impl From<SqlValue> for Expr` — the field's `Into`-bound public
/// builders (`Column::set`, `UpdateBuilder::set`) keep their original
/// signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: &'static str,
    pub value: Expr,
}

/// Compiled `UPDATE`.
///
/// `set` are emitted in order before `WHERE`, so their placeholders
/// come first. An empty `where_clause` (the default `WhereExpr::And(vec![])`)
/// runs an unfiltered update affecting every row — the caller is
/// responsible for that being intentional.
#[derive(Debug, Clone)]
pub struct UpdateQuery {
    pub model: &'static ModelSchema,
    pub set: Vec<Assignment>,
    pub where_clause: WhereExpr,
}

impl UpdateQuery {
    /// Walk each `SET column = value` and check it against the field's
    /// declared bounds. Filters are not checked — they compare against
    /// existing rows, not write targets.
    ///
    /// # Errors
    /// As [`InsertQuery::validate`].
    pub fn validate(&self) -> Result<(), QueryError> {
        for assignment in &self.set {
            let field = self
                .model
                .field_by_column(assignment.column)
                .ok_or_else(|| QueryError::UnknownField {
                    model: self.model.name,
                    field: assignment.column.to_owned(),
                })?;
            // Only literal rhs values are checkable against the field's
            // declared bounds; column refs and arithmetic trees don't
            // resolve to a single concrete value at compile time.
            if let Some(literal) = assignment.value.as_literal() {
                validate_value(self.model.name, field, literal)?;
            }
        }
        Ok(())
    }
}

/// Compiled `DELETE`.
///
/// As with `UpdateQuery`, an empty `where_clause` deletes every row.
#[derive(Debug, Clone)]
pub struct DeleteQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
}

impl DeleteQuery {
    /// Construct a `DELETE … WHERE <pk_column> = <pk_value>` shape.
    /// Companion to [`SelectQuery::by_pk`] for the bulk- and
    /// single-PK-delete patterns. #562 / #810.
    #[must_use]
    pub fn by_pk(model: &'static ModelSchema, pk_column: &'static str, pk_value: SqlValue) -> Self {
        Self {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::Eq,
                value: pk_value,
            }),
        }
    }

    /// Construct a `DELETE … WHERE <pk_column> IN (...)` shape — the
    /// "delete-selected" admin / API pattern. Companion to
    /// [`SelectQuery::by_pk_in`]. #810.
    #[must_use]
    pub fn by_pk_in(
        model: &'static ModelSchema,
        pk_column: &'static str,
        pk_values: Vec<SqlValue>,
    ) -> Self {
        Self {
            model,
            where_clause: WhereExpr::Predicate(Filter {
                column: pk_column,
                op: Op::In,
                value: SqlValue::List(pk_values),
            }),
        }
    }
}

/// Compiled `SELECT COUNT(*)` — same shape as a `DeleteQuery` (model +
/// where clause); the writer emits `COUNT(*)` projection and no
/// `LIMIT`/`OFFSET`.
#[derive(Debug, Clone)]
pub struct CountQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    /// Optional ILIKE search across the supplied columns. When set
    /// the count includes only rows that *also* match the search —
    /// without this the page-number list endpoint reported the
    /// wrong total whenever `?search=...` was active.
    pub search: Option<SearchClause>,
}

/// Bulk per-row UPDATE using `UPDATE t SET … FROM (VALUES …)`. One row
/// in the VALUES clause per input item; the PK identifies which table row
/// to update.
///
/// All rows must supply the same `update_columns` list in the same order.
/// The PK column must match `model.primary_key()`.
///
/// Built via [`crate::sql::bulk_update`] or directly.
#[derive(Debug, Clone)]
pub struct BulkUpdateQuery {
    pub model: &'static ModelSchema,
    /// The column names to update (not including the PK).
    pub update_columns: Vec<&'static str>,
    /// One inner `Vec<SqlValue>` per row: `[pk_value, col1_value, col2_value, …]`.
    /// The first element is always the PK; the rest align with `update_columns`.
    pub rows: Vec<Vec<SqlValue>>,
}

/// One aggregate expression in an [`AggregateQuery`].
///
/// The flat variants (`Count`, `Sum`, etc.) emit the standard
/// `AGG(col)` shape; the two recursive wrappers ([`Filtered`] and
/// [`Coalesced`], issue #6) compose on top to add a `FILTER (WHERE …)`
/// predicate or a `COALESCE(…, default)` empty-result fallback.
///
/// Build via the higher-level helpers in [`crate::core::aggregates`]
/// (`count`/`sum`/`avg`/`max`/`min`/`count_distinct`/`stddev`/
/// `stddev_pop`/`variance`/`variance_pop`) rather than constructing
/// these variants directly — the builder enforces the right wrap
/// order (`Coalesced` outside `Filtered`).
///
/// [`Filtered`]: AggregateExpr::Filtered
/// [`Coalesced`]: AggregateExpr::Coalesced
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateExpr {
    /// `COUNT(*)` or `COUNT(column)` when `column` is `Some`.
    Count(Option<&'static str>),
    /// `COUNT(DISTINCT column)` — counts distinct values in a column.
    /// v0.45. Works on PG / MySQL 8+ / SQLite 3.35+.
    CountDistinct(&'static str),
    /// `SUM(column)`.
    Sum(&'static str),
    /// `AVG(column)`.
    Avg(&'static str),
    /// `MAX(column)`.
    Max(&'static str),
    /// `MIN(column)`.
    Min(&'static str),
    /// `STDDEV_SAMP(column)` — sample standard deviation. Issue #6.
    /// Native on PG + MySQL 8+; the writer raises
    /// [`crate::sql::SqlError::AggregateNotSupported`] on SQLite,
    /// which has no built-in stddev (matches Django behavior).
    StdDev(&'static str),
    /// `STDDEV_POP(column)` — population standard deviation. Issue #6.
    /// Same dialect-support story as [`StdDev`](AggregateExpr::StdDev).
    StdDevPop(&'static str),
    /// `VAR_SAMP(column)` — sample variance. Issue #6.
    /// Same dialect-support story as [`StdDev`](AggregateExpr::StdDev).
    Variance(&'static str),
    /// `VAR_POP(column)` — population variance. Issue #6.
    /// Same dialect-support story as [`StdDev`](AggregateExpr::StdDev).
    VariancePop(&'static str),
    /// `<inner> FILTER (WHERE <filter>)` on PG / SQLite (3.30+);
    /// `<inner-with-CASE-WHEN-arg>` on MySQL. Issue #6. Wraps any
    /// base aggregate (Count/Sum/Avg/Max/Min/CountDistinct/StdDev/
    /// Variance/etc.) — nested `Filtered` is rejected at emit time
    /// to keep emission unambiguous.
    Filtered {
        inner: Box<AggregateExpr>,
        filter: WhereExpr,
    },
    /// `COALESCE(<inner>, <default>)` — empty-result fallback. Issue #6.
    /// Always outermost when combined with `Filtered` (builder enforces
    /// the order); nested `Coalesced` is rejected at emit time.
    Coalesced {
        inner: Box<AggregateExpr>,
        default: SqlValue,
    },
    /// Window function — `<fn>(args) OVER (PARTITION BY … ORDER BY …)`.
    /// Issue #7. Conceptually distinct from an aggregate (operates
    /// over a frame, not a group), but reuses the `annotate()` slot
    /// because the projection shape is the same. Use the builders in
    /// [`crate::core::window`] rather than constructing this variant
    /// directly.
    Window(Box<super::window::WindowExpr>),
    /// PG: `array_agg(column)` — collects column values into a Postgres
    /// array. With `distinct = true` emits `array_agg(DISTINCT column)`.
    /// Issue #33. **Postgres-only**: MySQL/SQLite emit
    /// `SqlError::AggregateNotSupportedInDialect`. The returned column
    /// is a `text[]` / `int[]` depending on the input type; decode it as
    /// `Vec<T>` via `serde_json::Value` if the SqlValue decoder doesn't
    /// recognise the array type natively.
    ArrayAgg {
        column: &'static str,
        distinct: bool,
    },
    /// PG: `string_agg(column, delimiter)` — concatenates column values
    /// with `delimiter`. With `distinct = true` emits
    /// `string_agg(DISTINCT column, delimiter)`. `delimiter` is bound as
    /// a parameter so SQL injection through the delimiter is impossible.
    /// Issue #33. **Postgres-only**.
    StringAgg {
        column: &'static str,
        delimiter: String,
        distinct: bool,
    },
    /// PG: `jsonb_agg(column)` — collects column values into a JSONB
    /// array. Issue #33. **Postgres-only**.
    JsonbAgg { column: &'static str },
}

impl AggregateExpr {
    /// Whether this annotation actually aggregates rows (and therefore
    /// triggers GROUP BY auto-inference, issue #75).
    ///
    /// - `Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` /
    ///   `StdDev*` / `Variance*` / `ArrayAgg` / `StringAgg` / `JsonbAgg`
    ///   → **aggregating** (collapses rows).
    /// - `Window` → **not aggregating** (per-row computation over a frame).
    /// - `Filtered { inner }` / `Coalesced { inner }` → recurse on `inner`.
    #[must_use]
    pub fn is_aggregating(&self) -> bool {
        match self {
            AggregateExpr::Count(_)
            | AggregateExpr::CountDistinct(_)
            | AggregateExpr::Sum(_)
            | AggregateExpr::Avg(_)
            | AggregateExpr::Max(_)
            | AggregateExpr::Min(_)
            | AggregateExpr::StdDev(_)
            | AggregateExpr::StdDevPop(_)
            | AggregateExpr::Variance(_)
            | AggregateExpr::VariancePop(_)
            | AggregateExpr::ArrayAgg { .. }
            | AggregateExpr::StringAgg { .. }
            | AggregateExpr::JsonbAgg { .. } => true,
            AggregateExpr::Window(_) => false,
            AggregateExpr::Filtered { inner, .. } | AggregateExpr::Coalesced { inner, .. } => {
                inner.is_aggregating()
            }
        }
    }

    /// Ergonomic constructor for [`AggregateExpr::ArrayAgg`] without
    /// `DISTINCT`. Issue #33.
    #[must_use]
    pub const fn array_agg(column: &'static str) -> Self {
        Self::ArrayAgg {
            column,
            distinct: false,
        }
    }

    /// Ergonomic constructor for `array_agg(DISTINCT column)`. Issue #33.
    #[must_use]
    pub const fn array_agg_distinct(column: &'static str) -> Self {
        Self::ArrayAgg {
            column,
            distinct: true,
        }
    }

    /// Ergonomic constructor for [`AggregateExpr::StringAgg`] without
    /// `DISTINCT`. Issue #33.
    #[must_use]
    pub fn string_agg(column: &'static str, delimiter: impl Into<String>) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: false,
        }
    }

    /// Ergonomic constructor for `string_agg(DISTINCT column, delimiter)`.
    /// Issue #33.
    #[must_use]
    pub fn string_agg_distinct(column: &'static str, delimiter: impl Into<String>) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: true,
        }
    }

    /// Ergonomic constructor for [`AggregateExpr::JsonbAgg`]. Issue #33.
    #[must_use]
    pub const fn jsonb_agg(column: &'static str) -> Self {
        Self::JsonbAgg { column }
    }
}

/// A `SELECT … GROUP BY … HAVING …` query. Returned rows are untyped
/// (`HashMap<String, SqlValue>`) because the projection is dynamic.
///
/// Build via [`crate::query::QuerySet::aggregate`].
#[derive(Debug, Clone)]
pub struct AggregateQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    /// Columns to group by. Must be valid column names on `model`.
    pub group_by: Vec<&'static str>,
    /// `(alias, expr)` pairs — the alias becomes the key in each result row.
    pub aggregates: Vec<(&'static str, AggregateExpr)>,
    /// Non-projected annotations — Django 3.2 `.alias()`. Same `(name, expr)`
    /// shape as [`Self::aggregates`] but the writer omits these from the SELECT
    /// projection. They remain resolvable inside `HAVING` and `ORDER BY` (the
    /// builder lifts the expression in-place at compile time). Useful when you
    /// want to filter/order by a derived aggregate without paying the
    /// column-decode cost. Issue #268.
    pub aliases: Vec<(&'static str, AggregateExpr)>,
    /// Optional HAVING clause (applied after GROUP BY).
    pub having: Option<WhereExpr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// PartialEq for `AggregateQuery` — needed so [`crate::core::Expr`]
/// (which embeds `Box<AggregateQuery>` for issue #830 correlated
/// count-comparator subqueries) can keep its `#[derive(PartialEq)]`.
/// Same treatment as [`SelectQuery`]: `ModelSchema` doesn't implement
/// `PartialEq`, so `model` is compared by pointer identity — two
/// queries against the same (singleton) schema register equal.
impl PartialEq for AggregateQuery {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.model, other.model)
            && self.where_clause == other.where_clause
            && self.group_by == other.group_by
            && self.aggregates == other.aggregates
            && self.aliases == other.aliases
            && self.having == other.having
            && self.order_by == other.order_by
            && self.limit == other.limit
            && self.offset == other.offset
    }
}
