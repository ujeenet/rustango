//! Dialect-neutral query IR.
//!
//! The query crate compiles a typed `QuerySet<T>` into a
//! [`SelectQuery`]. The SQL crate then walks that IR and writes one
//! parameterized statement per dialect, so both crates use the types
//! in this module.

use std::borrow::Cow;

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
    /// Case-sensitive `LIKE` over a value escaped by [`escape_like`].
    /// Emits `LIKE ? ESCAPE '!'`, so the escaping holds on every
    /// dialect: SQLite has **no** default LIKE escape, and `ESCAPE '\'`
    /// is not portable because MySQL reads `\` as a string escape.
    /// Used by the `contains` / `startswith` / `endswith` lookups,
    /// which wrap **user input** in wildcards. Plain [`Op::Like`] is
    /// for caller-owned patterns and is never escaped.
    LikeEscaped,
    /// Case-insensitive form of [`Op::LikeEscaped`]: the dialect's
    /// ILIKE shape plus `ESCAPE '!'`.
    ILikeEscaped,
    /// Range check. The bound value must be `SqlValue::List([lo, hi])`.
    /// Emits `col BETWEEN $lo AND $hi`.
    Between,
    /// Negated range check. Same value shape as [`Op::Between`]
    /// (`SqlValue::List([lo, hi])`). Emits `col NOT BETWEEN $lo AND
    /// $hi`.
    NotBetween,
    /// Compares against `NULL`. The bound value must be
    /// `SqlValue::Bool`: `true` means `IS NULL`, `false` means
    /// `IS NOT NULL`.
    IsNull,
    /// Null-safe inequality: `IS DISTINCT FROM`. Unlike `<>`, it
    /// treats `NULL` as a comparable value, so
    /// `NULL IS NOT DISTINCT FROM NULL` is `true`. Bind any
    /// `SqlValue`.
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
    /// POSIX regex match, case-sensitive — the `__regex` lookup. Bind a
    /// `SqlValue::String` holding the pattern. PG `~`, MySQL `REGEXP`,
    /// SQLite `REGEXP`. On SQLite the connection must register a
    /// `regexp(pattern, value)` function; sqlx-sqlite does not add it.
    Regex,
    /// POSIX regex non-match. PG `!~`, MySQL `NOT REGEXP`, SQLite
    /// `NOT REGEXP` (same SQLite caveat as [`Op::Regex`]).
    NotRegex,
    /// Case-insensitive regex match — the `__iregex` lookup. PG `~*`.
    /// MySQL and SQLite have no such operator, so the writer wraps
    /// both sides in `LOWER(...)` for ASCII case folding.
    IRegex,
    /// Case-insensitive regex non-match. PG `!~*`. MySQL and SQLite
    /// use `LOWER(<col>) NOT REGEXP LOWER(<pattern>)`, for the same
    /// reason as [`Op::IRegex`].
    NotIRegex,
    /// pg_trgm similarity — the `__trigram_similar` lookup. Emits
    /// `<col> % <pattern>`; bind a `SqlValue::String`. Needs
    /// `CREATE EXTENSION pg_trgm` (default threshold `0.3`, change it
    /// with `SET pg_trgm.similarity_threshold`). **PG-only** — MySQL
    /// and SQLite reject it when the query compiles.
    TrigramSimilar,
    /// pg_trgm word similarity — the `__trigram_word_similar` lookup.
    /// Emits `<col> %> <pattern>`, which matches when any single
    /// **word** in the column is similar; the bare `%` needs the whole
    /// string to be similar. **PG-only**, same extension requirement.
    TrigramWordSimilar,
    /// Postgres full-text search — the `__search` lookup. Emits
    /// `to_tsvector(<col>) @@ plainto_tsquery(<pattern>)` with the
    /// database's default text-search config; bind a
    /// `SqlValue::String`. For a chosen language, weighted vectors or
    /// websearch syntax, build the query with [`crate::core::fts`].
    /// **PG-only** — MySQL `MATCH … AGAINST` and SQLite FTS5 `MATCH`
    /// mean something different, so both reject at compile time.
    Search,
    /// Postgres array containment — the `__array_contains` lookup.
    /// Emits `<col> @> <value>`: rows whose array holds
    /// every element of the value array. **PG-only** — MySQL and
    /// SQLite have no array type and reject with
    /// `OpNotSupportedInDialect`.
    ArrayContains,
    /// Inverse of [`Self::ArrayContains`]: `<col> <@ <value>`, rows
    /// whose array is held by the value array — the
    /// `__array_contained_by` lookup. **PG-only**.
    ArrayContainedBy,
    /// Postgres array overlap — `<col> && <value>`: the two arrays
    /// share at least one element — the `__array_overlap` lookup.
    /// **PG-only**.
    ArrayOverlap,
    /// Postgres range containment — `<col> @> <value>`, the
    /// `__range_contains` lookup. The right side is a
    /// single element or a range literal. Same SQL operator as
    /// [`Self::ArrayContains`], but a separate variant so the intent
    /// is clear and the bind path can pick the value shape
    /// (`SqlValue::RangeLiteral` for range-vs-range, a scalar for
    /// range-vs-element). **PG-only**.
    RangeContains,
    /// Inverse of [`Self::RangeContains`]: `<col> <@ <value>`.
    /// The `__range_contained_by` lookup. **PG-only**.
    RangeContainedBy,
    /// Range overlap — `<col> && <value>`, the
    /// `__range_overlap` lookup. **PG-only**.
    RangeOverlap,
    /// Range strictly left of — `<col> << <value>`: the whole range
    /// falls below the value range. **PG-only**.
    RangeStrictlyLeft,
    /// Range strictly right of — `<col> >> <value>`. **PG-only**.
    RangeStrictlyRight,
    /// Range adjacent — `<col> -|- <value>`: the ranges touch, with
    /// no overlap and no gap. **PG-only**.
    RangeAdjacent,
}

/// The LIKE escape character for [`Op::LikeEscaped`] /
/// [`Op::ILikeEscaped`]. `!` is the portable choice: `ESCAPE '\'`
/// breaks on MySQL, where `\` is the string escape, and SQLite has no
/// default escape at all.
pub const LIKE_ESCAPE_CHAR: char = '!';

/// The SQL suffix the escaped ops add after their `LIKE`. A unit test
/// pins it to [`LIKE_ESCAPE_CHAR`], so the two cannot drift apart.
pub const LIKE_ESCAPE_CLAUSE: &str = " ESCAPE '!'";

/// Escape LIKE metacharacters in **user input**, so `%` and `_` match
/// as plain characters — `__contains` means a literal substring,
/// not a pattern.
///
/// The result only means anything under [`LIKE_ESCAPE_CLAUSE`], so
/// bind it to an [`Op::LikeEscaped`] / [`Op::ILikeEscaped`] predicate.
/// Wildcards the *caller* adds around the escaped value (the `%…%` of
/// a contains) stay unescaped and keep their meaning.
#[must_use]
pub fn escape_like(input: &str) -> String {
    let e = LIKE_ESCAPE_CHAR;
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        // Escape the escape character itself and the two LIKE
        // metacharacters.
        if ch == e || ch == '%' || ch == '_' {
            out.push(e);
        }
        out.push(ch);
    }
    out
}

/// One predicate in a `WHERE` clause: `column <op> value`. Always
/// the leaf of a [`WhereExpr`] tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: &'static str,
    pub op: Op,
    pub value: SqlValue,
}

/// `WHERE` predicate that compares two columns of the same row, such
/// as `WHERE updated_at > created_at`.
///
/// Emits `<column> <op> <rhs>`. The left side is the model column
/// being filtered; `rhs` is any [`Expr`], usually `Expr::Column` for a
/// plain column-vs-column compare or a `BinOp` tree for arithmetic.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnFilter {
    /// Left-hand column (the field being filtered on, schema-resolved).
    pub column: &'static str,
    /// Comparison operator. Only the binary compares in [`Op`] make
    /// sense here (`Eq`, `Ne`, `Lt`, `Lte`, `Gt`, `Gte`); the rest
    /// (`In`, `Between`, `IsNull`, the JSON ops) are rejected when the
    /// query is emitted.
    pub op: Op,
    /// Right-hand side. Usually `Expr::Column(other)`, but any
    /// expression tree works.
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
/// An empty `And` or `Or` is allowed and means `TRUE` and `FALSE`.
/// `And(vec![])` is the internal "no filters" shape and the writer
/// emits no `WHERE` for it. `Or(vec![])` would quietly match nothing,
/// so the writer rejects it.
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
    /// Logical XOR. True when an odd number of
    /// children are true. Only MySQL has a native XOR, so the writer
    /// always rewrites: two children become
    /// `(a AND NOT b) OR (NOT a AND b)`, three or more fold into a
    /// CASE-WHEN-1/0 sum tested with `% 2 = 1`. No children matches
    /// nothing and is rejected, like `Or(vec![])`; one child means the
    /// child itself.
    ///
    /// The two-child rewrite prints each operand twice, so the
    /// database evaluates each one twice. That only matters for
    /// volatile expressions such as `RANDOM()` or `NOW()`. Three or
    /// more children evaluate each child exactly once.
    Xor(Vec<WhereExpr>),
    /// `EXISTS (<subquery>)` — true when the inner `SelectQuery`
    /// returns at least one row. Boxed because `SelectQuery` carries
    /// its own `WhereExpr`, which would make the enum unbounded.
    Exists(Box<SelectQuery>),
    /// `NOT EXISTS (<subquery>)` — the negation of [`Self::Exists`].
    NotExists(Box<SelectQuery>),
    /// `<col> IN (<subquery>)` / `<col> NOT IN (<subquery>)`. Like
    /// `Op::In` / `Op::NotIn` over a literal list, but the right side
    /// is a `SELECT`, correlated or not.
    InSubquery {
        column: &'static str,
        negated: bool,
        subquery: Box<SelectQuery>,
    },
    /// `<lhs> <op> <rhs>` with any [`Expr`] on both sides. Used in
    /// JOIN `ON` predicates, where a side usually needs a table alias
    /// ([`Expr::AliasedColumn`]). Outside a JOIN, use the narrower
    /// [`ColumnFilter`].
    ///
    /// Only the binary compares (`Eq`, `Ne`, `Lt`, `Lte`, `Gt`,
    /// `Gte`) make sense here; the writer rejects the rest, as it does
    /// for `ColumnFilter`.
    ExprCompare { lhs: Expr, op: Op, rhs: Expr },
    /// `[NOT ]EXISTS (SELECT 1 FROM <table> WHERE <correlation>)` over
    /// a **raw table** — the M2M / GFK arm of the relation-existence
    /// family. It holds no [`SelectQuery`], unlike [`Self::Exists`],
    /// because an M2M junction table has no model and the GFK case
    /// only needs a literal `SELECT 1`. [`RelCorrelation`] carries the
    /// link back to the outer row and, for a GFK, the content-type
    /// check.
    RelExists {
        table: &'static str,
        correlation: RelCorrelation,
        negated: bool,
    },
}

/// Aggregate function for a correlated raw-table relation aggregate
/// ([`Expr::RelAggregate`]). Separate from [`AggregateExpr`]: the
/// raw-table path (M2M / GFK) has no `ModelSchema` and supports only
/// `COUNT(*)` / `SUM` / `AVG` / `MAX` / `MIN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelAggKind {
    Count,
    Sum,
    Avg,
    Max,
    Min,
}

/// Content-type check for a generic FK. AND-ed into a
/// [`RelCorrelation::Fk`] so a polymorphic child table keeps only the
/// rows that point at *this* parent model. Emits `<ct_column> =
/// (SELECT <ct_pk> FROM <ct_table> WHERE <ct_table_col> =
/// '<parent_table>')`: the parent's content-type id is resolved in
/// SQL, so no async lookup is needed while the query is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtFilter {
    pub ct_column: &'static str,
    pub parent_table: &'static str,
    pub ct_table: &'static str,
    pub ct_pk: &'static str,
    pub ct_table_col: &'static str,
}

/// How a raw-table relation subquery ([`WhereExpr::RelExists`] /
/// [`Expr::RelAggregate`]) correlates back to the enclosing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelCorrelation {
    /// `<table>.<fk_column> = <outer>.<outer_column>`, plus an
    /// optional GFK content-type check. Covers an M2M junction
    /// (`fk_column` is the junction's source column) and a generic
    /// child (`fk_column` is the `object_pk` column, with `ct` set).
    Fk {
        fk_column: &'static str,
        outer_column: &'static str,
        ct: Option<CtFilter>,
    },
    /// `<table>.<target_pk> IN (SELECT <dst_col> FROM <through> WHERE
    /// <src_col> = <outer>.<outer_column>)` — an M2M aggregate over a
    /// column on the *target* table, reached through the junction.
    Membership {
        target_pk: &'static str,
        through: &'static str,
        dst_col: &'static str,
        src_col: &'static str,
        outer_column: &'static str,
    },
}

impl WhereExpr {
    /// `true` when this expression carries no predicates (i.e. an
    /// empty `And`). Used by the writer to skip emitting `WHERE`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::And(items) if items.is_empty())
    }

    /// Build an AND of leaf filters — the common "list of predicates
    /// joined with AND" case.
    #[must_use]
    pub fn and_predicates(filters: Vec<Filter>) -> Self {
        Self::And(filters.into_iter().map(Self::Predicate).collect())
    }

    /// Add a predicate with AND. An existing `And(_)` takes the child
    /// in place; anything else is wrapped in a new `And` next to it.
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

    /// The predicate list when this is a single `Predicate` or a flat
    /// AND of predicates; `None` for any tree with `Or` or a nested
    /// `And`. Lets a caller inspect an AND-only WHERE without walking
    /// the whole tree.
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
            // None of these hold a plain `Filter`: the compare
            // variants carry an `Expr` right side, and the subquery
            // shapes carry a whole query, so the flat-AND view cannot
            // hand any of them back as a `&Filter`.
            Self::ColumnCompare(_)
            | Self::Or(_)
            | Self::Xor(_)
            | Self::Not(_)
            | Self::Exists(_)
            | Self::NotExists(_)
            | Self::InSubquery { .. }
            | Self::ExprCompare { .. }
            | Self::RelExists { .. } => None,
        }
    }

    /// Walk the tree and check every leaf predicate against `model`.
    ///
    /// # Errors
    /// Returns [`QueryError::UnknownField`] when a predicate names a
    /// column the model does not have, at any depth.
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
                // Check every column named inside the rhs Expr tree.
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
            // A subquery predicate checks its inner SELECT against
            // that SELECT's own model, when the inner queryset is
            // compiled. The outer model only owns the left column of
            // `InSubquery`.
            Self::Exists(_) | Self::NotExists(_) => Ok(()),
            // Raw-table relation existence (M2M / GFK): the table and
            // correlation columns live on a junction or polymorphic
            // child, not on `model`, and the framework builds them
            // from trusted relation metadata.
            Self::RelExists { .. } => Ok(()),
            Self::InSubquery { column, .. } => {
                if model.field_by_column(column).is_none() {
                    return Err(QueryError::UnknownField {
                        model: model.name,
                        field: (*column).to_owned(),
                    });
                }
                Ok(())
            }
            // ExprCompare is used in JOIN ON predicates, where both
            // sides usually carry their own alias. `model` is the
            // wrong schema for the alias side, and that schema is not
            // reachable here, so a bad column shows up at runtime.
            Self::ExprCompare { .. } => Ok(()),
        }
    }
}

/// Walk an [`Expr`] and confirm every `Column` it names resolves on
/// `model`. Literals and arithmetic pass straight through.
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
        // None of these resolve against `model`. A `Subquery` or
        // `AggregateSubquery` is checked against its own model when
        // that query is compiled. An `OuterRef` names a column on the
        // outer model, checked when the outer query embeds this one.
        // `AliasedColumn` carries its own table alias, and
        // `RelAggregate` reads a raw relation table (M2M junction or
        // GFK child) built from trusted relation metadata.
        Expr::Subquery(_)
        | Expr::AggregateSubquery(_)
        | Expr::OuterRef(_)
        | Expr::RelAggregate { .. }
        | Expr::AliasedColumn { .. } => Ok(()),
        // A window's args, partition_by and order_by all name columns
        // on this model, so check them.
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
        // Known gap: bare-column aggregate args (`Sum("col")`) hold
        // raw names this walker never visits. Window-shaped
        // aggregates are checked by the dedicated walker that
        // `AggregateBuilder::compile()` calls.
        Expr::Aggregate(_) => Ok(()),
        // Only `source` names a model column; the path steps are JSON
        // keys and indices.
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

/// Compiled `SELECT` over one model, with an optional `WHERE` given
/// as a [`WhereExpr`] tree.
///
/// `limit` and `offset` are `None` by default and emit no clause.
/// `search`, when set, adds a parenthesized `(col ILIKE $N OR …)`
/// group AND-ed to `where_clause`. `joins` adds JOIN clauses and
/// pulls extra columns into the projection under aliased names.
#[derive(Debug, Clone)]
pub struct SelectQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    pub search: Option<SearchClause>,
    pub joins: Vec<Join>,
    /// Derived-table joins — `JOIN [LATERAL] (<subquery>) AS alias ON
    /// …` (Eloquent `joinSub` / `joinLateral`). Emitted in the `FROM`
    /// clause after the model [`Self::joins`]. Empty by default.
    pub subquery_joins: Vec<SubqueryJoin>,
    /// `ORDER BY` items, in the order they appear in SQL. Emitted
    /// after WHERE / JOIN / GROUP BY and before LIMIT / OFFSET.
    /// Empty = no `ORDER BY`.
    pub order_by: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Row lock appended after LIMIT/OFFSET.
    /// `None` emits no lock clause. Run it inside a transaction on PG
    /// and MySQL; SQLite has no row-level lock syntax, so the writer
    /// does nothing there.
    pub lock_mode: Option<LockMode>,
    /// Set-algebra branches combined with this query, via
    /// `.union()` / `.intersection()` /
    /// `.difference()`. Empty emits a plain `SELECT …`.
    /// Non-empty wraps every branch in parens and joins them with the
    /// matching keyword:
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
    /// parens. The outer `order_by` / `limit` / `offset` /
    /// `lock_mode` apply to the merged result.
    pub compound: Vec<CompoundBranch>,
    /// Column list for a pure projection, from `.values_dict()` /
    /// `.values_list()`. `None` emits every scalar field on the
    /// model; `Some(cols)` emits exactly `cols`, in that order. Joins
    /// still add their `project` columns. Checked when the query is
    /// built, so every column resolves on the model schema.
    pub projection: Option<Vec<&'static str>>,
    /// DISTINCT mode. `None` emits no DISTINCT clause.
    /// `Some(DistinctMode::All)` emits `SELECT DISTINCT ...`.
    /// `Some(DistinctMode::On(cols))` emits PG `SELECT DISTINCT ON
    /// (cols) ...`; on MySQL and SQLite the writer wraps the query in
    /// a `ROW_NUMBER() OVER (PARTITION BY cols ORDER BY <order_by>)
    /// AS __rn` subquery with an outer `WHERE __rn = 1`, which keeps
    /// the "first row per group" meaning.
    pub distinct: Option<DistinctMode>,
    /// `ORDER BY` for the COMBINED result of a set operation: the
    /// clauses chained AFTER the first `.union()` / `.intersection()`
    /// / `.difference()` call. Empty when there is no compound, or
    /// when the ordering was set BEFORE the first set-op call — that
    /// one belongs to the head branch and lives in
    /// [`Self::order_by`], which the writer wraps. Emitted after the
    /// last branch.
    pub compound_order_by: Vec<OrderItem>,
    /// `LIMIT` on the merged result, chained after the first set-op
    /// call. See [`Self::compound_order_by`].
    pub compound_limit: Option<i64>,
    /// `OFFSET` on the merged result, chained after the first set-op
    /// call. See [`Self::compound_order_by`].
    pub compound_offset: Option<i64>,
}

impl SelectQuery {
    /// Construct an empty `SelectQuery` against `model`: no filters
    /// (`WhereExpr::And(vec![])`, vacuously true), empty lists, `None`
    /// options. Layer the real filter / order / limit on top with
    /// struct update, so a new field does not break existing callers:
    ///
    /// ```ignore
    /// use rustango::core::SelectQuery;
    ///
    /// let q = SelectQuery {
    ///     limit: Some(10),
    ///     ..SelectQuery::new(MyModel::SCHEMA)
    /// };
    /// ```
    #[must_use]
    pub fn new(model: &'static ModelSchema) -> Self {
        Self {
            model,
            // Vacuously true; the writer rejects `Or(vec![])` —
            // see the `WhereExpr` doc.
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
            compound_order_by: Vec::new(),
            compound_limit: None,
            compound_offset: None,
        }
    }

    /// A single-PK lookup — the most common shape in the framework.
    /// Use it when the WHERE is one `<pk_column> = <value>` and you
    /// want one row back.
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

    /// Multi-PK `IN (...)` lookup. Companion to [`Self::by_pk`] for bulk
    /// fetch, bulk delete by id, and FK display fetches.
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
    /// No `LIMIT` is set. Add one with struct update if you need it:
    /// `SelectQuery { limit: Some(50), ..SelectQuery::by_pk_in(...) }`.
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

/// Distinct mode — all columns, or a named subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistinctMode {
    /// `SELECT DISTINCT ...` — the same on every dialect.
    All,
    /// `SELECT DISTINCT ON (cols) ...` on PG, with a portable
    /// `ROW_NUMBER()` fallback on MySQL / SQLite. Empty `cols` is
    /// rejected when the query compiles; use `All` instead.
    On(Vec<&'static str>),
}

/// One branch of a set-algebra compound query.
#[derive(Debug, Clone)]
pub struct CompoundBranch {
    /// `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`.
    pub op: SetOp,
    /// The branch itself — a complete `SelectQuery` whose projection
    /// must match the outer query's column shape (same model).
    pub query: Box<SelectQuery>,
}

/// SQL set-algebra operator, behind `.union()` / `.intersection()` /
/// `.difference()`.
///
/// Dialect support:
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
    /// `EXCEPT` — rows in the first branch but not the others.
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

/// Manual `PartialEq`: `CompoundBranch` nests `SelectQuery`, which
/// compares its `ModelSchema` by pointer.
impl PartialEq for CompoundBranch {
    fn eq(&self, other: &Self) -> bool {
        self.op == other.op && self.query == other.query
    }
}

/// `SELECT … FOR UPDATE` row-lock options — `skip_locked`, `nowait`,
/// `of` and `no_key`.
///
/// It is `#[non_exhaustive]`, so a new per-backend flag can be added
/// without breaking code that builds a `LockMode` directly. Build one
/// with [`LockMode::default`] plus field assignment, or chain the
/// [`crate::query::QuerySet`] methods (`.select_for_update()`,
/// `.skip_locked()`, `.nowait()`, `.no_key()`, `.of(…)`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockMode {
    /// PG 9.3+: `FOR NO KEY UPDATE` instead of `FOR UPDATE`. A weaker
    /// lock that does not block writers which leave the row's PK and
    /// unique columns alone. MySQL has no equivalent, so the writer
    /// falls back to `FOR UPDATE`.
    pub no_key: bool,
    /// PG / MySQL 8+: `SKIP LOCKED`. Rows another transaction holds
    /// are dropped from the result instead of waited for — the usual
    /// "claim the next free row" pattern.
    pub skip_locked: bool,
    /// PG / MySQL 8+: `NOWAIT`. Fails at once if any row in the
    /// result is locked. The database cannot combine it with
    /// `skip_locked`, so if both are set the writer emits
    /// `SKIP LOCKED`, the more forgiving one.
    pub nowait: bool,
    /// PG 9.3+: `FOR UPDATE OF table1, table2, …`. Locks only the
    /// named tables when the query joins. Pass table names or
    /// aliases; an empty vec emits no `OF` clause. MySQL supports it
    /// since 8.0.1; on SQLite it does nothing.
    pub of: Vec<&'static str>,
    /// Do not log the `tracing::warn!` the writer emits when this
    /// `LockMode` reaches a SQLite query. SQLite has no row-level
    /// lock syntax, so the clause is dropped and the warning says so.
    /// Set it on test fixtures or single-writer apps where SQLite's
    /// global writer lock is enough.
    pub silent_on_sqlite: bool,
}

/// `PartialEq` for `SelectQuery`, so [`crate::core::Expr`] (which
/// boxes one for subqueries) can keep its derive. `ModelSchema` has no
/// `PartialEq`, so `model` is compared by pointer: two queries against
/// the same schema are equal, which is right because schemas are
/// singletons.
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
            && self.compound_order_by == other.compound_order_by
            && self.compound_limit == other.compound_limit
            && self.compound_offset == other.compound_offset
    }
}

/// Same pointer comparison for `Join`, which also holds a
/// `&'static ModelSchema` (the join target).
impl PartialEq for Join {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.target, other.target)
            && self.alias == other.alias
            && self.kind == other.kind
            && self.on == other.on
            && self.project == other.project
    }
}

/// One column plus a direction in an `ORDER BY` — the simple
/// "field name + ASC/DESC" form.
///
/// Prefer [`OrderItem`] in new code: it also takes an `Expr` and
/// controls `NULLS FIRST/LAST`. `OrderClause` stays as a short
/// constructor and converts with `Into<OrderItem>` in every
/// `SelectQuery` / `AggregateQuery` / window ORDER BY slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    /// SQL column name on the main table. `QuerySet::order_by`
    /// already resolved it from the Rust field name, so the writer
    /// does not walk the schema again.
    pub column: &'static str,
    /// `true` for `DESC`, `false` for the default `ASC`.
    pub desc: bool,
}

/// Where NULLs sort next to real values.
///
/// The default differs per dialect: PG and SQLite put NULLs last on
/// `ASC` and first on `DESC` (the SQL standard), while MySQL treats
/// NULL as smaller than any value, so it comes first on `ASC` and
/// last on `DESC`. `First` or `Last` gives the same order on all
/// three; MySQL has no `NULLS` keywords, so the writer emits an
/// `IFNULL(…)` workaround there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullsOrder {
    /// Backend's native default — emits no `NULLS …` clause.
    #[default]
    Default,
    /// `NULLS FIRST` on PG / SQLite; on MySQL an
    /// `IFNULL(col, '') = '' DESC, …` trick.
    First,
    /// `NULLS LAST` on PG / SQLite; the same kind of trick on MySQL.
    Last,
}

/// One item of an `ORDER BY` list: a plain column with optional NULL
/// ordering, any [`Expr`] (`lower(col)`, `case(…)`, `F(a) + F(b)`, or
/// any builder result), or random order.
///
/// An [`OrderClause`] converts into the `Column` variant, so older
/// constructors keep working.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderItem {
    /// `<col> [DESC] [NULLS FIRST|LAST]` — an [`OrderClause`] plus a
    /// [`NullsOrder`].
    Column {
        column: &'static str,
        desc: bool,
        nulls: NullsOrder,
    },
    /// `<expr> [DESC] [NULLS FIRST|LAST]`. The `expr` goes through
    /// the normal `Expr` writer, so function calls, `CASE` and
    /// arithmetic all work.
    Expr {
        expr: Expr,
        desc: bool,
        nulls: NullsOrder,
    },
    /// `ORDER BY RANDOM()` (PG / SQLite) or `ORDER BY RAND()`
    /// (MySQL). No direction and no NULLS
    /// clause: the random key is computed per row and is never NULL.
    ///
    /// **Performance**: this forces a full table scan and an
    /// in-memory sort, with no index to help. On a big table prefer a
    /// `WHERE pk >= rand_offset LIMIT N` pattern.
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
    /// The common case: `<col> [DESC]` with default NULL ordering.
    #[must_use]
    pub fn column(column: &'static str, desc: bool) -> Self {
        Self::Column {
            column,
            desc,
            nulls: NullsOrder::Default,
        }
    }

    /// `<col> [DESC] [NULLS FIRST|LAST]`.
    #[must_use]
    pub fn column_with_nulls(column: &'static str, desc: bool, nulls: NullsOrder) -> Self {
        Self::Column {
            column,
            desc,
            nulls,
        }
    }

    /// `<expr> [DESC]` with default NULL ordering.
    #[must_use]
    pub fn expr(expr: Expr, desc: bool) -> Self {
        Self::Expr {
            expr,
            desc,
            nulls: NullsOrder::Default,
        }
    }

    /// `<expr> [DESC] [NULLS FIRST|LAST]`.
    #[must_use]
    pub fn expr_with_nulls(expr: Expr, desc: bool, nulls: NullsOrder) -> Self {
        Self::Expr { expr, desc, nulls }
    }

    /// A `Random` item — `ORDER BY RANDOM()` / `RAND()`.
    #[must_use]
    pub fn random() -> Self {
        Self::Random
    }

    /// The bare column name for a `Column` item; `None` for `Expr`
    /// and `Random`.
    #[must_use]
    pub fn column_name(&self) -> Option<&'static str> {
        match self {
            Self::Column { column, .. } => Some(column),
            Self::Expr { .. } | Self::Random => None,
        }
    }

    /// `true` if this item sorts descending. `Random` has no
    /// direction, so it returns `false`.
    #[must_use]
    pub fn is_desc(&self) -> bool {
        match self {
            Self::Column { desc, .. } | Self::Expr { desc, .. } => *desc,
            Self::Random => false,
        }
    }

    /// The `NullsOrder` of this item. `Random` returns `Default`: its
    /// key is per row and never NULL, so the clause does nothing.
    #[must_use]
    pub fn nulls_order(&self) -> NullsOrder {
        match self {
            Self::Column { nulls, .. } | Self::Expr { nulls, .. } => *nulls,
            Self::Random => NullsOrder::Default,
        }
    }
}

/// Which SQL `JOIN` keyword the writer emits.
///
/// `Left` is the default and matches FK-driven `select_related`,
/// which keeps every outer row even with no match on the target side.
/// `Inner` is the usual ad-hoc join and drops unmatched outer rows.
/// The IR accepts `Right` and `Full`, but not every dialect does: the
/// writer raises [`crate::sql::SqlError::JoinKindNotSupported`] for
/// `Right` on SQLite and `Full` on MySQL / SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoinKind {
    Inner,
    #[default]
    Left,
    Right,
    Full,
}

/// A JOIN against a target model, with a [`JoinKind`] and any
/// `WhereExpr` predicate (not only the FK shape
/// `main.fk = alias.target_pk`).
///
/// The writer emits `<kind> JOIN "<target.table>" AS "<alias>" ON
/// <on>` and adds each `project` column to the SELECT list as
/// `"<alias>"."<col>" AS "<alias>__<col>"`. Read joined values from
/// the row by that suffixed name.
///
/// When a `SelectQuery` has any join, the writer also qualifies the
/// main table's columns as `"<table>"."<col>"`, so nothing is
/// ambiguous. Inside `on`, point at another table with
/// [`Expr::AliasedColumn`].
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

/// What a derived-table join selects from: a plain typed `SELECT` or
/// an aggregate / window query.
///
/// Window functions compile to an [`AggregateQuery`], so
/// [`DerivedSource::Aggregate`] is how a windowed result becomes a
/// derived table. That is the way to filter on a window result — for
/// example, keep rows whose per-group `rank` is `<= N`, which a bare
/// `WHERE` cannot do.
///
/// The four derived-table join builders
/// ([`crate::query::QuerySet::join_sub`] and friends) take
/// `impl Into<DerivedSource>`, so both types work directly.
#[derive(Debug, Clone, PartialEq)]
pub enum DerivedSource {
    /// A plain typed `SELECT` derived table.
    Select(Box<SelectQuery>),
    /// An aggregate / window query as the derived table.
    Aggregate(Box<AggregateQuery>),
}

impl From<SelectQuery> for DerivedSource {
    fn from(q: SelectQuery) -> Self {
        DerivedSource::Select(Box::new(q))
    }
}

impl From<AggregateQuery> for DerivedSource {
    fn from(q: AggregateQuery) -> Self {
        DerivedSource::Aggregate(Box::new(q))
    }
}

/// A JOIN whose right side is a **derived table** (a subquery) rather
/// than a model table — Eloquent's `joinSub` / `leftJoinSub`, or, with
/// `lateral = true`, `joinLateral` / `leftJoinLateral`.
///
/// The writer emits `<kind> JOIN [LATERAL] (<subquery>) AS "<alias>"
/// ON <on>`. There is no model schema behind the derived table, so
/// `on` must qualify every one of its columns with
/// [`Expr::AliasedColumn`] (`"<alias>"."<col>"`).
///
/// Unlike [`Join`], this adds **no** columns to the SELECT
/// projection: it only filters or correlates, so a typed fetch still
/// decodes the base model. Use an explicit `.values()` projection to
/// read derived values.
///
/// `lateral` emits the `LATERAL` keyword, which lets the subquery read
/// columns from earlier `FROM` items, such as the outer table — the
/// "top-N rows per group" shape. **PG and MySQL ≥ 8.0.14 only**; the
/// writer raises [`crate::sql::SqlError::LateralJoinNotSupported`] on
/// SQLite.
///
/// [`Expr::AliasedColumn`]: crate::core::Expr::AliasedColumn
#[derive(Debug, Clone, PartialEq)]
pub struct SubqueryJoin {
    /// The derived table — a plain `SELECT` or an aggregate/window query.
    pub subquery: DerivedSource,
    /// Alias the derived table is exposed under (`AS "<alias>"`).
    pub alias: &'static str,
    /// `INNER` or `LEFT` — the only kinds that make sense for a
    /// derived table, and the only two the QuerySet builders make.
    pub kind: JoinKind,
    /// Join predicate. It names the derived table as
    /// `"<alias>"."<col>"` and the outer table by its own name, both
    /// through [`Expr::AliasedColumn`]. A `LATERAL` join usually
    /// correlates inside the subquery's own `WHERE`, so `on` stays
    /// empty (`WhereExpr::And(vec![])`) and the writer emits
    /// `ON true`.
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

/// What to do when an insert hits a unique constraint. Set it on
/// [`InsertQuery::on_conflict`] or [`BulkInsertQuery::on_conflict`];
/// the writer emits the right shape for each dialect.
#[derive(Debug, Clone)]
pub enum ConflictClause {
    /// `ON CONFLICT DO NOTHING` — skip the duplicate rows.
    DoNothing,
    /// `ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col` for
    /// every column in `update_columns`. `target` names the columns
    /// whose unique constraint defines the conflict, usually the PK
    /// or a `#[rustango(unique)]` column.
    DoUpdate {
        target: Vec<&'static str>,
        update_columns: Vec<&'static str>,
    },
}

/// Compiled `INSERT` of a single row.
///
/// `columns` and `values` are positional: `values[i]` binds to
/// `columns[i]`.
#[derive(Debug, Clone)]
pub struct InsertQuery {
    pub model: &'static ModelSchema,
    pub columns: Vec<&'static str>,
    pub values: Vec<SqlValue>,
    /// Columns for the `RETURNING` clause. Empty = no clause and the
    /// executor calls `execute()`. Non-empty = it calls `fetch_one()`
    /// and the caller reads the row back. This is how an `Auto<T>` PK
    /// works: the column is left out of the insert so the sequence
    /// default fires, then the new value is read back into the model.
    pub returning: Vec<&'static str>,
    /// Optional `ON CONFLICT` clause. `None` = a plain INSERT that
    /// fails on a constraint violation.
    pub on_conflict: Option<ConflictClause>,
}

impl InsertQuery {
    /// Check each `(column, value)` pair against the field's declared
    /// bounds (`max_length`, `min`, `max`).
    ///
    /// # Errors
    /// Returns [`QueryError::MaxLengthExceeded`] or
    /// [`QueryError::OutOfRange`] for a bad value, or
    /// [`QueryError::UnknownField`] if a column is not a field on
    /// `model`.
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

/// Compiled multi-row `INSERT` — one round trip for N rows.
///
/// `rows[i]` is positional against `columns`: every row gives the
/// same columns in the same order. `returning` behaves as on
/// [`InsertQuery`]; non-empty makes the executor use `fetch_all` and
/// return one row per input row.
///
/// Rows cannot differ in shape: no row may drop a column with the
/// Postgres `DEFAULT` keyword, so every row needs a value for every
/// column. With an `Auto<T>` PK, pass `Auto::Unset` for every row
/// (the macro drops that column and the sequence fires) or
/// `Auto::Set(v)` for every row. Mixing the two in one call is
/// rejected at validate time.
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
    /// Chainable builder — upsert on conflict. Sets `on_conflict` to
    /// `DoUpdate { target, update_columns }`.
    ///
    /// The writer emits:
    /// * Postgres / SQLite (3.24+): `ON CONFLICT (target) DO UPDATE SET col = EXCLUDED.col`
    /// * MySQL: `ON DUPLICATE KEY UPDATE col = VALUES(col)` — no
    ///   target, because MySQL matches every UNIQUE index by itself.
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

    /// Chainable builder — skip rows that conflict.
    /// Sets `on_conflict` to `DoNothing`.
    ///
    /// The writer emits:
    /// * Postgres / SQLite: `ON CONFLICT DO NOTHING`
    /// * MySQL: `ON DUPLICATE KEY UPDATE <pivot> = <pivot>`, the
    ///   no-op write trick.
    #[must_use]
    pub fn on_conflict_do_nothing(mut self) -> Self {
        self.on_conflict = Some(ConflictClause::DoNothing);
        self
    }
}

impl BulkInsertQuery {
    /// Check every `(column, value)` pair in every row against the
    /// field's declared bounds.
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

/// One `column = value` pair in an `UPDATE … SET …`.
///
/// `value` is an [`Expr`]: a literal (`Expr::Literal`, the common
/// case), a column reference (`F("col")`, a column-to-column copy),
/// or arithmetic (`F("col") + 1`, the atomic counter pattern). An
/// [`SqlValue`] converts on its own through
/// `impl From<SqlValue> for Expr`, so `Column::set` and
/// `UpdateBuilder::set` keep their signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: &'static str,
    pub value: Expr,
}

/// Compiled `UPDATE`.
///
/// `set` is emitted in order before `WHERE`, so its placeholders bind
/// first. An empty `where_clause` (the default
/// `WhereExpr::And(vec![])`) updates every row; the caller must mean
/// that.
#[derive(Debug, Clone)]
pub struct UpdateQuery {
    pub model: &'static ModelSchema,
    pub set: Vec<Assignment>,
    pub where_clause: WhereExpr,
}

impl UpdateQuery {
    /// Check each `SET column = value` against the field's declared
    /// bounds. Filters are not checked: they read existing rows
    /// instead of writing them.
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
            // Only a literal right side can be checked against the
            // field's bounds; a column reference or arithmetic tree
            // has no single value yet.
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
    /// A `DELETE … WHERE <pk_column> = <pk_value>`. Companion to
    /// [`SelectQuery::by_pk`].
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

    /// A `DELETE … WHERE <pk_column> IN (...)` — the "delete
    /// selected" admin and API pattern. Companion to
    /// [`SelectQuery::by_pk_in`].
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

/// Compiled `SELECT COUNT(*)` — model plus where clause, like a
/// `DeleteQuery`. The writer emits a `COUNT(*)` projection and no
/// `LIMIT` / `OFFSET`.
#[derive(Debug, Clone)]
pub struct CountQuery {
    pub model: &'static ModelSchema,
    pub where_clause: WhereExpr,
    /// Optional ILIKE search over the given columns. When set, the
    /// count keeps only rows that *also* match the search, so a
    /// paginated list shows the right total while `?search=...` is
    /// active.
    pub search: Option<SearchClause>,
}

/// Bulk per-row UPDATE using `UPDATE t SET … FROM (VALUES …)`: one
/// VALUES row per input item, and the PK picks the table row to
/// update.
///
/// Every row must give the same `update_columns` in the same order,
/// and the PK column must match `model.primary_key()`.
///
/// Run it with [`crate::sql::bulk_update_pool`], or build it directly.
#[derive(Debug, Clone)]
pub struct BulkUpdateQuery {
    pub model: &'static ModelSchema,
    /// The columns to update, not counting the PK.
    pub update_columns: Vec<&'static str>,
    /// One `Vec<SqlValue>` per row:
    /// `[pk_value, col1_value, col2_value, …]`. The first element is
    /// always the PK; the rest line up with `update_columns`.
    pub rows: Vec<Vec<SqlValue>>,
}

/// One aggregate expression in an [`AggregateQuery`].
///
/// The flat variants (`Count`, `Sum`, …) emit the plain `AGG(col)`
/// shape. [`Filtered`] and [`Coalesced`] wrap another aggregate to
/// add a `FILTER (WHERE …)` predicate or a `COALESCE(…, default)`
/// fallback for an empty result.
///
/// Build these with the helpers in [`crate::core::aggregates`]
/// (`count`/`sum`/`avg`/`max`/`min`/`count_distinct`/`stddev`/
/// `stddev_pop`/`variance`/`variance_pop`) instead of by hand: they
/// apply the required wrap order, `Coalesced` outside `Filtered`.
///
/// [`Filtered`]: AggregateExpr::Filtered
/// [`Coalesced`]: AggregateExpr::Coalesced
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateExpr {
    /// `COUNT(*)` or `COUNT(column)` when `column` is `Some`.
    Count(Option<&'static str>),
    /// `COUNT(DISTINCT column)`. PG, MySQL 8+, SQLite 3.35+.
    CountDistinct(&'static str),
    /// `SUM(column)`.
    Sum(&'static str),
    /// `AVG(column)`.
    Avg(&'static str),
    /// `MAX(column)`.
    Max(&'static str),
    /// `MIN(column)`.
    Min(&'static str),
    /// `ANY_VALUE(column)` — some value from the group's non-null
    /// inputs. It lets a functionally dependent column
    /// be projected without adding it to GROUP BY. PG 16+
    /// `any_value()`, MySQL `ANY_VALUE()`; on SQLite the writer uses
    /// `min()`, which is deterministic and still meets the contract.
    AnyValue(&'static str),
    /// `STDDEV_SAMP(column)` — sample standard deviation. Native on
    /// PG and MySQL 8+. SQLite has none, so the writer raises
    /// [`crate::sql::SqlError::AggregateNotSupported`].
    StdDev(&'static str),
    /// `STDDEV_POP(column)` — population standard deviation. Same
    /// dialect support as [`StdDev`](AggregateExpr::StdDev).
    StdDevPop(&'static str),
    /// `VAR_SAMP(column)` — sample variance. Same dialect support as
    /// [`StdDev`](AggregateExpr::StdDev).
    Variance(&'static str),
    /// `VAR_POP(column)` — population variance. Same dialect support
    /// as [`StdDev`](AggregateExpr::StdDev).
    VariancePop(&'static str),
    /// `<inner> FILTER (WHERE <filter>)` on PG / SQLite 3.30+; a
    /// CASE-WHEN argument on MySQL. Wraps any base aggregate. A
    /// nested `Filtered` is rejected at emit time, so the emission
    /// stays unambiguous.
    Filtered {
        inner: Box<AggregateExpr>,
        filter: WhereExpr,
    },
    /// `COALESCE(<inner>, <default>)` — a fallback for an empty
    /// result. Always outermost when combined with `Filtered` (the
    /// builder enforces that); a nested `Coalesced` is rejected at
    /// emit time.
    Coalesced {
        inner: Box<AggregateExpr>,
        default: SqlValue,
    },
    /// Window function — `<fn>(args) OVER (PARTITION BY … ORDER BY
    /// …)`. Not really an aggregate, since it works over a frame
    /// rather than a group, but it shares the `annotate()` slot
    /// because the projection shape is the same. Build it with
    /// [`crate::core::window`].
    Window(Box<super::window::WindowExpr>),
    /// PG `array_agg(column)`, collecting values into an array, or
    /// `array_agg(DISTINCT column)` when `distinct`. **Postgres-only**:
    /// MySQL and SQLite raise
    /// `SqlError::AggregateNotSupportedInDialect`. The result column
    /// is a `text[]` or `int[]`; decode it as `Vec<T>` through
    /// `serde_json::Value` if the `SqlValue` decoder does not know
    /// that array type.
    ArrayAgg {
        column: &'static str,
        distinct: bool,
    },
    /// PG `string_agg(column, delimiter)`, joining values with
    /// `delimiter`, or `string_agg(DISTINCT column, delimiter)` when
    /// `distinct`. The delimiter is bound as a parameter, so it
    /// cannot carry SQL injection. **Postgres-only**.
    StringAgg {
        column: &'static str,
        delimiter: String,
        distinct: bool,
        /// `ORDER BY` inside the aggregate. Empty = the backend picks the
        /// order. With `distinct`, every clause must order by
        /// `column` itself; that is checked at emit time.
        order_by: Vec<OrderClause>,
    },
    /// PG `jsonb_agg(column)`, collecting values into a JSONB array.
    /// **Postgres-only**.
    JsonbAgg { column: &'static str },
    /// Correlated relation aggregate — `withCount` / `withSum` /
    /// `withAvg` / `withMax` / `withMin` by relation name.
    ///
    /// It wraps the correlated subquery [`Expr`] (always an
    /// [`Expr::AggregateSubquery`]) built by the
    /// [`crate::core::subquery::reverse_has_aggregate`] family. The
    /// aggregation happens *inside* that subquery, over the **child**
    /// table; the outer query sees one scalar per row.
    ///
    /// [`is_aggregating()`](AggregateExpr::is_aggregating) returns
    /// `true`, so the builder's GROUP BY inference also projects the
    /// parent's scalar columns and rows come back as
    /// `{parent cols…, <rel>_<agg>}`. The value comes from a
    /// correlated subquery, not a JOIN, so it never double-counts.
    RelatedAggregate(Box<Expr>),
}

impl AggregateExpr {
    /// Whether this annotation collapses rows, and so makes the
    /// builder infer a GROUP BY.
    ///
    /// - `Count` / `Sum` / `Avg` / `Max` / `Min` / `CountDistinct` /
    ///   `StdDev*` / `Variance*` / `ArrayAgg` / `StringAgg` / `JsonbAgg`
    ///   → **aggregating**.
    /// - `Window` → **not aggregating** (per row, over a frame).
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
            | AggregateExpr::AnyValue(_)
            | AggregateExpr::StdDev(_)
            | AggregateExpr::StdDevPop(_)
            | AggregateExpr::Variance(_)
            | AggregateExpr::VariancePop(_)
            | AggregateExpr::ArrayAgg { .. }
            | AggregateExpr::StringAgg { .. }
            | AggregateExpr::JsonbAgg { .. }
            // The wrapped subquery aggregates the *child* table, and
            // the outer query sees a scalar. Report `true` anyway, so
            // the builder adds the parent's scalar columns to GROUP
            // BY and to the SELECT, giving `{parent cols…,
            // <rel>_<agg>}` rows. The PK is in GROUP BY, so the
            // correlated value is functionally determined and MySQL
            // `ONLY_FULL_GROUP_BY` accepts it.
            | AggregateExpr::RelatedAggregate(_) => true,
            AggregateExpr::Window(_) => false,
            AggregateExpr::Filtered { inner, .. } | AggregateExpr::Coalesced { inner, .. } => {
                inner.is_aggregating()
            }
        }
    }

    /// [`AggregateExpr::ArrayAgg`] without `DISTINCT`.
    #[must_use]
    pub const fn array_agg(column: &'static str) -> Self {
        Self::ArrayAgg {
            column,
            distinct: false,
        }
    }

    /// `array_agg(DISTINCT column)`.
    #[must_use]
    pub const fn array_agg_distinct(column: &'static str) -> Self {
        Self::ArrayAgg {
            column,
            distinct: true,
        }
    }

    /// [`AggregateExpr::StringAgg`] without `DISTINCT`.
    #[must_use]
    pub fn string_agg(column: &'static str, delimiter: impl Into<String>) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: false,
            order_by: Vec::new(),
        }
    }

    /// `string_agg(DISTINCT column, delimiter)`.
    #[must_use]
    pub fn string_agg_distinct(column: &'static str, delimiter: impl Into<String>) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: true,
            order_by: Vec::new(),
        }
    }

    /// `string_agg(column, delimiter ORDER BY …)` — ordered
    /// concatenation. `order` is
    /// a slice of `(column, desc)` pairs, as in
    /// `WindowBuilder::order_by`.
    #[must_use]
    pub fn string_agg_ordered(
        column: &'static str,
        delimiter: impl Into<String>,
        order: &[(&'static str, bool)],
    ) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: false,
            order_by: order
                .iter()
                .map(|(c, desc)| OrderClause {
                    column: c,
                    desc: *desc,
                })
                .collect(),
        }
    }

    /// `string_agg(DISTINCT column, delimiter ORDER BY …)`. With
    /// DISTINCT, `order` may only name `column` itself; PG requires
    /// that, and the emit step checks it on every dialect.
    #[must_use]
    pub fn string_agg_distinct_ordered(
        column: &'static str,
        delimiter: impl Into<String>,
        order: &[(&'static str, bool)],
    ) -> Self {
        Self::StringAgg {
            column,
            delimiter: delimiter.into(),
            distinct: true,
            order_by: order
                .iter()
                .map(|(c, desc)| OrderClause {
                    column: c,
                    desc: *desc,
                })
                .collect(),
        }
    }

    /// [`AggregateExpr::JsonbAgg`].
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
    /// FK-chain and ad-hoc JOINs, so an aggregate can group by a
    /// *related* column (`values("author.name").annotate(Count)`).
    /// Empty for a single-table aggregate. Emitted between `FROM` and
    /// `WHERE`, same shape as [`SelectQuery::joins`].
    pub joins: Vec<Join>,
    pub where_clause: WhereExpr,
    /// Columns to group by. A bare name (`"status"`) is a column on
    /// `model`; a dotted name (`"author.name"`) points at a JOINed
    /// alias, is emitted qualified, and skips model validation.
    pub group_by: Vec<&'static str>,
    /// `(alias, expr)` pairs — the alias is the key in each result
    /// row.
    ///
    /// The alias is a [`Cow<'static, str>`], not a bare
    /// `&'static str`: most call sites pass a literal (such as
    /// `Cow::Borrowed("post_count")`), but the relation-aggregate
    /// shortcuts ([`crate::query::QuerySet::annotate_count`] and
    /// friends) name the column from a runtime relation name
    /// (`{rel}_count`, `{rel}_sum_{col}`), which needs an owned
    /// string.
    pub aggregates: Vec<(Cow<'static, str>, AggregateExpr)>,
    /// Non-projected annotations, from `.alias()`. Same
    /// `(name, expr)` shape as [`Self::aggregates`], but the writer
    /// leaves them out of the SELECT projection. They still resolve
    /// in `HAVING` and `ORDER BY`, because the builder inlines the
    /// expression at compile time. Use them to filter or order by a
    /// derived aggregate without paying to decode the column.
    pub aliases: Vec<(Cow<'static, str>, AggregateExpr)>,
    /// Optional HAVING clause (applied after GROUP BY).
    pub having: Option<WhereExpr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// `PartialEq` for `AggregateQuery`, so [`crate::core::Expr`] (which
/// boxes one for correlated count-comparator subqueries) can keep its
/// derive. Same as [`SelectQuery`]: `ModelSchema` has no `PartialEq`,
/// so `model` is compared by pointer, and two queries against the
/// same singleton schema are equal.
impl PartialEq for AggregateQuery {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.model, other.model)
            && self.joins == other.joins
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
