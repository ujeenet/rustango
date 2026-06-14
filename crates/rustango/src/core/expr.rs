//! `Expr` — RHS expression for [`crate::core::query::Assignment`] +
//! column-vs-column comparisons in [`crate::core::query::ColumnFilter`].
//!
//! Closes ORM Expression-DSL issue #1 (`F()` expressions). Lets the
//! ORM refer to a column by name inside an UPDATE SET or a WHERE
//! predicate:
//!
//! ```ignore
//! // Atomic counter increment — no read-modify-write race.
//! Post::objects()
//!     .where_(Post::id.eq(42))
//!     .update()
//!     .set("views", F("views") + 1)
//!     .execute_pool(&pool).await?;
//!
//! // Column-vs-column filter.
//! Reservation::objects()
//!     .where_col(Reservation::start_date, Op::Lt, F("end_date"))
//!     .fetch(&pool).await?;
//! ```
//!
//! `Expr` carries three variants:
//! - [`Expr::Literal`] — a bound parameter, indistinguishable from
//!   the legacy `SqlValue` path. Every existing call site that passes
//!   a value into an `Assignment` lifts through `impl From<SqlValue>
//!   for Expr` and lands here.
//! - [`Expr::Column`] — a column reference. Emitter quotes the
//!   identifier per dialect (`"col"` on PG/SQLite, `` `col` `` on
//!   MySQL).
//! - [`Expr::BinOp`] — `<left> <op> <right>` arithmetic, recursive.
//!
//! Parenthesization on emit is conservative: every `BinOp` is wrapped
//! in `()`. This is more parens than strictly necessary for `(a + b)
//! + c` but matches what the planner sees from Django's emitter and
//! keeps the writer trivially correct.
//!
//! # Why a fresh `Expr` instead of shoehorning into `SqlValue`
//!
//! `SqlValue` is a *value* — its `Display`, `field_type()`, sqlx
//! `Encode`/`Decode`, JSON serialization paths all assume a concrete
//! literal. A column reference and an arithmetic tree have no
//! `field_type()` until the schema resolves them, and they don't bind
//! as a parameter. Keeping them in a separate enum keeps both types
//! honest.

use std::ops;

use super::value::SqlValue;

/// Binary arithmetic operator. Emits its SQL keyword/symbol verbatim.
///
/// Tri-dialect compatibility:
/// - `Add`, `Sub`, `Mul`, `Div`, `Mod`: portable across PG / MySQL / SQLite.
/// - `BitAnd`, `BitOr`, `BitXor`, `BitShl`, `BitShr`: PG and MySQL spell
///   these the same (`&`, `|`, `#` for XOR on PG vs `^` on MySQL — emitter
///   picks dialect-correct symbol). SQLite supports `&`, `|`, `<<`, `>>`
///   but lacks a bitwise XOR operator; the emitter surfaces a clear
///   `SqlError::OpNotSupportedInDialect` for `BitXor` on SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+` — addition (numeric / date-interval, dialect-dependent).
    Add,
    /// `-` — subtraction.
    Sub,
    /// `*` — multiplication.
    Mul,
    /// `/` — division.
    Div,
    /// `%` (PG/SQLite) or `MOD` (MySQL alt): emitted as `%` everywhere.
    Mod,
    /// Bitwise AND.
    BitAnd,
    /// Bitwise OR.
    BitOr,
    /// Bitwise XOR. PG: `#`. MySQL: `^`. SQLite: not supported.
    BitXor,
    /// Left shift.
    BitShl,
    /// Right shift.
    BitShr,
    /// pgvector L2 (Euclidean) distance — `<->`. **PG-only** (#824); the
    /// writer raises `OpNotSupportedInDialect` on MySQL / SQLite.
    L2Distance,
    /// pgvector cosine distance — `<=>`. **PG-only** (#824).
    CosineDistance,
    /// pgvector (negative) inner product — `<#>`. **PG-only** (#824).
    /// pgvector returns the negative inner product, so ascending order
    /// still ranks most-similar first.
    InnerProduct,
}

/// Distance metric for pgvector similarity search (#824) — the
/// user-facing selector for [`crate::query::QuerySet::order_by_distance`]
/// / [`k_nearest`](crate::query::QuerySet::k_nearest). Maps to the
/// pgvector distance operator ([`BinOp::L2Distance`] / `CosineDistance`
/// / `InnerProduct`). Ascending order always ranks the most-similar row
/// first (pgvector's `<#>` returns the *negative* inner product).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VectorMetric {
    /// L2 / Euclidean distance — pgvector `<->`.
    L2,
    /// Cosine distance — pgvector `<=>`.
    Cosine,
    /// (Negative) inner product — pgvector `<#>`.
    InnerProduct,
}

impl VectorMetric {
    /// The [`BinOp`] distance operator this metric lowers to.
    #[must_use]
    pub const fn to_binop(self) -> BinOp {
        match self {
            Self::L2 => BinOp::L2Distance,
            Self::Cosine => BinOp::CosineDistance,
            Self::InnerProduct => BinOp::InnerProduct,
        }
    }
}

/// RHS expression — literal, column reference, arithmetic tree, or
/// scalar function call.
///
/// `Expr` is what the writer renders to the right side of `=` in an
/// UPDATE assignment and to the right side of a column predicate in
/// a WHERE clause. The variants are recursive so arbitrarily nested
/// arithmetic and function calls are expressible.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Bound value. Emitter pushes a parameter.
    Literal(SqlValue),
    /// Column reference. Emitter writes the quoted identifier.
    /// `&'static str` matches the rest of the IR's column-name
    /// convention (model schemas embed `&'static str` field names).
    Column(&'static str),
    /// Binary arithmetic: `left <op> right`. Recursive — either side
    /// may be a nested `BinOp`.
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    /// Scalar function call — `FN(arg, arg, …)`. Variadic-arity is
    /// folded into `args` so the writer doesn't switch per-fn on
    /// argument count. Issue #2 (Database functions DSL); see
    /// [`crate::core::funcs`] for the public builder API.
    Function { kind: ScalarFn, args: Vec<Expr> },
    /// `CASE WHEN c1 THEN t1 [WHEN c2 THEN t2 …] [ELSE d] END` —
    /// conditional expression (issue #4). Standard SQL; identical
    /// emission across PG / MySQL / SQLite. `branches` carries the
    /// `WHEN` clauses in source order; `default` is the optional
    /// `ELSE` branch (omitted in SQL when `None`).
    ///
    /// Build via [`crate::core::case::case()`] rather than
    /// constructing this variant by hand — the builder chain reads
    /// closer to the SQL and handles the boxing.
    Case {
        branches: Vec<CaseBranch>,
        default: Option<Box<Expr>>,
    },
    /// Scalar subquery — `(SELECT col FROM … LIMIT 1)` (issue #5).
    /// Used in `set_expr` / `eq_expr` / WHERE-rhs slots where a single
    /// value is expected. The inner [`SelectQuery`] is built by
    /// calling `QuerySet::compile()` upfront — that way schema
    /// validation errors surface at construction rather than at
    /// outer-queryset compile time, and the same compiled subquery
    /// can be reused across statements.
    ///
    /// Caller is responsible for shaping the inner queryset to a
    /// single row + single column when the slot expects a scalar.
    ///
    /// [`SelectQuery`]: crate::core::SelectQuery
    Subquery(Box<super::query::SelectQuery>),
    /// Scalar **aggregate** subquery — `(SELECT COUNT(*) FROM … WHERE …)`.
    /// Issue #830 slice 3. Like [`Self::Subquery`] but the inner query is
    /// an [`AggregateQuery`] so its projection is a single aggregate
    /// (`COUNT(*)`, `SUM(col)`, …) rather than the child model's columns.
    ///
    /// Backs the count-comparator shortcut
    /// [`crate::query::QuerySet::where_has_count`], which embeds a
    /// correlated `(SELECT COUNT(*) FROM <child> WHERE <child_fk> =
    /// <outer>.<pk>) <op> n` predicate. The writer pushes the inner
    /// model's scope frame so any [`Self::OuterRef`] inside correlates
    /// to the enclosing parent query — identically across PG / MySQL /
    /// SQLite.
    ///
    /// Build via [`crate::core::subquery::reverse_has_count`] rather than
    /// constructing this variant directly.
    ///
    /// [`AggregateQuery`]: crate::core::AggregateQuery
    AggregateSubquery(Box<super::query::AggregateQuery>),
    /// Correlated aggregate over a **raw relation table** — the M2M /
    /// GFK counterpart of [`AggregateSubquery`](Expr::AggregateSubquery)
    /// for tables that have no `ModelSchema` (issue #830). Emits
    /// `(SELECT <kind>(<column> | *) FROM <table> WHERE <correlation>)`.
    /// `column` is `None` for `COUNT(*)` and `Some(col)` for
    /// `SUM`/`AVG`/`MAX`/`MIN`. The correlation back to the outer row
    /// (and, for M2M target aggregates, the junction membership) lives
    /// in [`RelCorrelation`](super::query::RelCorrelation).
    RelAggregate {
        kind: super::query::RelAggKind,
        column: Option<&'static str>,
        table: &'static str,
        correlation: super::query::RelCorrelation,
    },
    /// Reference to an outer query's column from inside a correlated
    /// subquery (issue #5). Emitted as `"<outer_table>"."<col>"`; the
    /// outer table is threaded through the writer at emit time so
    /// nested `EXISTS` / `IN (SELECT …)` / scalar subqueries can
    /// reference the outer row.
    ///
    /// Build via [`crate::core::subquery::outer_ref`] rather than this
    /// variant directly — the helper reads closer to Django's
    /// `OuterRef('col')`.
    OuterRef(&'static str),
    /// Column reference qualified with an explicit table alias —
    /// `"<alias>"."<column>"`. Used inside JOIN `ON` predicates (issue
    /// #80) where both sides may reference columns on tables other
    /// than the implicit "current" one, and a bare `Column(name)`
    /// would qualify against the wrong scope.
    ///
    /// Build via [`crate::core::joins::aliased`] rather than this
    /// variant directly.
    AliasedColumn {
        alias: &'static str,
        column: &'static str,
    },
    /// Window function — `<fn>(args) OVER (PARTITION BY … ORDER BY …
    /// [frame])`. Issue #7. Boxed because [`WindowExpr`] carries a
    /// `Vec<Expr>` for arguments, which makes the enum size
    /// unbounded otherwise.
    ///
    /// Build via [`crate::core::window`] (`row_number`, `rank`,
    /// `dense_rank`, `lag`, `lead`, `first_value`, `last_value`,
    /// `ntile`) rather than this variant directly.
    ///
    /// [`WindowExpr`]: crate::core::WindowExpr
    Window(Box<super::window::WindowExpr>),
    /// Aggregate function lifted into the Expr tree — issue #74.
    /// Lets aggregate expressions appear in `HAVING` predicates
    /// (which PG strictly requires the expression in, not the SELECT
    /// alias) via [`super::query::WhereExpr::ExprCompare`]. Also
    /// composable inside `Case` / `Coalesce` / set_expr slots when
    /// the surrounding query is aggregating.
    ///
    /// Boxed because `AggregateExpr` itself carries `Box<...>`
    /// wrappers (`Filtered { inner, ... }`, `Coalesced { inner, ... }`)
    /// which would make the enum size unbounded otherwise.
    Aggregate(Box<super::query::AggregateExpr>),
    /// `CAST(<expr> AS <ty>)` — explicit type coercion. Issue #266 / T1.4.
    /// `ty` is a dialect-neutral [`crate::core::FieldType`]; the writer
    /// maps it to the dialect's SQL token via [`crate::sql::Dialect::null_cast`].
    /// Emits identical `CAST(... AS ...)` syntax on PG / MySQL / SQLite —
    /// only the type token differs.
    Cast {
        expr: Box<Expr>,
        ty: super::field_type::FieldType,
    },
    /// JSON path extraction — `<source> -> 'k1' -> 'k2' ->> 'k3'` on PG,
    /// `JSON_UNQUOTE(JSON_EXTRACT(<source>, '$.k1.k2.k3'))` on MySQL,
    /// `json_extract(<source>, '$.k1.k2.k3')` on SQLite. Issue #296 /
    /// T2.3. `as_text = true` requests the unwrapped-text form
    /// (`->>` / `JSON_UNQUOTE` / `json_extract` returns text by default
    /// for scalars); `false` keeps the JSON-typed form for further
    /// chaining or for use as a JSON-shape value.
    JsonPath {
        source: Box<Expr>,
        path: Vec<JsonPathStep>,
        as_text: bool,
    },
}

/// One segment of a [`Expr::JsonPath`] traversal. Keys are dictionary
/// lookups (`{"k": v}` → `JsonPathStep::Key("k")`); indices are array
/// lookups (`[a, b, c]` → `JsonPathStep::Index(0)`).
///
/// Negative indices ("count from the end") work on PG (native `->`) and
/// SQLite (the `$[#-1]` from-the-end anchor, #1027). MySQL's `$[N]` path
/// grammar has no negative form, so the writer rejects negative indices
/// on MySQL with a clear error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathStep {
    /// Object key lookup. The string is emitted as a quoted SQL
    /// literal on PG (`-> 'name'`) and as part of the JSON-pointer
    /// path on MySQL / SQLite (`$.name`). Caller-supplied keys must
    /// be JSON-pointer-safe: ASCII alphanumeric plus `_`. Other
    /// characters are rejected at writer time to keep the inlined
    /// path string injection-safe.
    Key(String),
    /// Array index lookup, 0-based.
    Index(i64),
}

/// One arm of a [`Expr::Case`] expression — `WHEN <condition> THEN <then>`.
///
/// `condition` is a full [`crate::core::WhereExpr`] tree, so the same
/// `Column::eq()` / `.and()` / `.or()` machinery that powers `WHERE`
/// clauses works inside `CASE` predicates. `then` is any [`Expr`]:
/// a literal, a column reference, a function call, another nested
/// `Case`, etc.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    pub condition: super::query::WhereExpr,
    pub then: Expr,
}

/// Scalar database functions surfaced by [`crate::core::funcs`].
///
/// v1 ships the text + math + comparison subset (~17 functions). The
/// emitter handles per-dialect divergence — `Concat` falls back to
/// `||` on SQLite, `Greatest`/`Least` are emitted as MAX/MIN scalars
/// on SQLite to match PG/MySQL semantics. Hash functions, trig, and
/// `Cast` ship in a follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFn {
    // --- Text (8) ---
    /// `LOWER(s)` — lowercase a string.
    Lower,
    /// `UPPER(s)` — uppercase a string.
    Upper,
    /// `LENGTH(s)` — string length. Char count on PG (`length`),
    /// byte count on MySQL (`LENGTH` — use `CHAR_LENGTH` for chars).
    /// SQLite: `length()` returns char count for `TEXT`. v2 may
    /// split this into `Length` (char) + `OctetLength` (byte).
    Length,
    /// `CONCAT(a, b, …)` — string concatenation. SQLite emits
    /// `a || b || …` (the only portable form on pre-3.44 SQLite).
    /// NULL handling: PG / SQLite return NULL on any NULL operand;
    /// MySQL's `CONCAT` does the same — this matches.
    Concat,
    /// `SUBSTRING(s FROM start FOR length)` (PG) /
    /// `SUBSTRING(s, start, length)` (MySQL) /
    /// `substr(s, start, length)` (SQLite). All 1-indexed.
    Substr,
    /// `TRIM(s)` — strip leading + trailing whitespace.
    Trim,
    /// `LTRIM(s)` — strip leading whitespace.
    LTrim,
    /// `RTRIM(s)` — strip trailing whitespace.
    RTrim,
    /// `REPLACE(s, from, to)` — replace every occurrence.
    Replace,

    // --- Math (4) ---
    /// `ABS(x)` — absolute value.
    Abs,
    /// `CEIL(x)` / `CEILING(x)` — ceiling.
    Ceil,
    /// `FLOOR(x)` — floor.
    Floor,
    /// `ROUND(x)` or `ROUND(x, n)` — bankers' rounding on PG, round-
    /// half-away-from-zero on MySQL / SQLite. Document the caveat
    /// for app code; for query work the precision matters less than
    /// the cross-dialect call shape.
    Round,

    // --- Comparison / NULL handling (4) ---
    /// `COALESCE(a, b, c, …)` — first non-NULL argument. Variadic.
    /// Returns NULL only if every argument is NULL.
    Coalesce,
    /// `GREATEST(a, b, …)` — largest non-NULL value. PG and MySQL
    /// have native operators; SQLite uses scalar `MAX(a, b, …)`.
    /// NULL semantics: PG / SQLite return NULL if any operand is
    /// NULL; MySQL ignores NULL. Caller wraps args in `COALESCE`
    /// for consistent cross-dialect behaviour when nulls are
    /// possible.
    Greatest,
    /// `LEAST(a, b, …)` — mirror of `Greatest`.
    Least,
    /// `NULLIF(a, b)` — `NULL` when `a == b`, else `a`. Universal.
    NullIf,

    // --- Date / time (issue #3) ---
    /// `NOW()` (PG/MySQL) / `CURRENT_TIMESTAMP` (SQLite). 0-arg. Returns
    /// the database server's wall-clock timestamp.
    Now,
    /// `EXTRACT(YEAR FROM x)` family. SQLite wraps in
    /// `CAST(strftime('%Y', x) AS INTEGER)`. PG casts to `integer` so
    /// the return type is consistent across dialects.
    ExtractYear,
    /// Month component (1–12).
    ExtractMonth,
    /// Day-of-month (1–31).
    ExtractDay,
    /// Hour (0–23).
    ExtractHour,
    /// Minute (0–59).
    ExtractMinute,
    /// Second (0–59).
    ExtractSecond,
    /// Week-of-year. **NOT portable across dialects** — each backend
    /// uses a different week-numbering convention, so the same date
    /// returns different values:
    /// - PG (`EXTRACT(WEEK FROM x)`): ISO 8601, weeks start Monday,
    ///   range 1–53.
    /// - MySQL (`WEEK(x)` default mode 0): weeks start **Sunday**,
    ///   range **0**–53.
    /// - SQLite (`strftime('%W', x)`): weeks start Monday, first
    ///   Monday-of-year is week 01, range 00–53.
    ///
    /// For 2024-01-01 (Monday): PG returns 1, MySQL returns 0, SQLite
    /// returns 01. Use this only when you stay on one backend, or
    /// compute the week boundary in app code via a typed
    /// `chrono::DateTime` literal.
    ExtractWeek,
    /// Day-of-week. **Normalized to PG's convention: 0 = Sunday, 6 =
    /// Saturday** across all three dialects. MySQL's `DAYOFWEEK()`
    /// returns 1=Sunday..7=Saturday natively; the writer subtracts 1
    /// to align. SQLite's `strftime('%w')` already matches 0=Sunday.
    ExtractWeekDay,
    /// Quarter (1–4). Not supported on SQLite (no native `strftime`
    /// token); emitter errors with `OpNotSupportedInDialect`.
    ExtractQuarter,
    /// `DATE(x)` — strip the time component, returning a `DATE`. Same
    /// shape on all three backends.
    TruncDate,
    /// Truncate timestamp to the start of the year. PG: `DATE_TRUNC('year', x)`
    /// (returns timestamp). MySQL: `DATE_FORMAT(x, '%Y-01-01')` (returns
    /// string). SQLite: `strftime('%Y-01-01', x)` (returns string).
    /// **Result-type caveat**: MySQL and SQLite return text — cast on
    /// the app side if a typed date/datetime is needed.
    TruncYear,
    /// Truncate timestamp to the start of the month. See `TruncYear`
    /// re: return-type divergence on MySQL/SQLite.
    TruncMonth,
    /// Truncate timestamp to the start of the day. PG: `DATE_TRUNC('day', x)`
    /// (returns timestamp). MySQL / SQLite: `DATE(x)` / `date(x)` (date).
    TruncDay,

    // --- JSON helpers (issue #826) ---
    /// `JSON_ARRAY_LENGTH(x)` — number of elements in a JSON array.
    /// Eloquent `whereJsonLength` / Django `JSONField` length lookup.
    /// PG: `jsonb_array_length(x)` (errors on non-array). MySQL:
    /// `JSON_LENGTH(x)` (returns 1 for non-array / non-object; for
    /// arrays returns the element count, matching the others). SQLite:
    /// `json_array_length(x)` (returns 0 on non-array). Arity 1. Pair
    /// with the comparison ops to filter arrays by length:
    /// `WHERE JSON_ARRAY_LENGTH(opts -> 'tags') > 0`. Issue #826.
    JsonArrayLength,

    // --- pg_trgm (issue #29 follow-up) ---
    /// `SIMILARITY(a, b)` — pg_trgm whole-string trigram similarity,
    /// returns a `real` in `[0, 1]`. Useful as an annotation +
    /// ORDER BY for ranked fuzzy search. Requires `CREATE EXTENSION
    /// pg_trgm`. **PG-only** — MySQL / SQLite emit
    /// `OpNotSupportedInDialect`. Pairs with `Op::TrigramSimilar`
    /// (the WHERE-clause `%` operator) shipped earlier in the same
    /// issue. Arity 2.
    TrigramSimilarity,
    /// `WORD_SIMILARITY(a, b)` — pg_trgm word-level similarity.
    /// **PG-only**, same dialect rules as [`Self::TrigramSimilarity`].
    /// Pairs with `Op::TrigramWordSimilar`. Arity 2.
    TrigramWordSimilarity,

    // --- Postgres full-text search (issue #28 follow-up) ---
    /// `to_tsvector(<expr>)` — build a `tsvector` from a text
    /// expression using the database's default text-search config.
    /// Arity 1. **PG-only** — MySQL / SQLite emit
    /// `OpNotSupportedInDialect`. Pairs with `Op::Search` (the
    /// WHERE-clause `@@ plainto_tsquery` operator) shipped earlier
    /// in the same issue.
    ToTsVector,
    /// `plainto_tsquery(<expr>)` — parse a plain user-provided
    /// string into a `tsquery`. Arity 1. **PG-only**.
    PlainToTsQuery,
    /// `ts_rank(<tsvector>, <tsquery>)` — FTS relevance score
    /// (`real`). Use with `to_tsvector(col)` + `plainto_tsquery(q)`
    /// for ranked search ordering. Arity 2. **PG-only**.
    TsRank,
    /// `ts_headline(<doc>, <tsquery> [, <options>])` — Postgres FTS
    /// snippet generator. Returns the document with matching terms
    /// wrapped in highlight markers (`<b>…</b>` by default; override
    /// via the optional `options` string, e.g.
    /// `"StartSel='<mark>', StopSel='</mark>', MaxFragments=1"`).
    /// Arity 2 or 3. **PG-only** — MySQL / SQLite emit
    /// `OpNotSupportedInDialect`. Pairs with `to_tsvector` /
    /// `plainto_tsquery` / `ts_rank`. Issue #28 follow-up.
    TsHeadline,
    /// `phraseto_tsquery(<expr>)` — Postgres FTS query parser that
    /// preserves word order (`'rust orm'` → `'rust' <-> 'orm'`).
    /// Use when "exact phrase" semantics matter. Arity 1. **PG-only**.
    /// Issue #28 follow-up.
    PhraseToTsQuery,
    /// `websearch_to_tsquery(<expr>)` — Postgres FTS query parser
    /// that accepts Google-style operators: quoted "exact phrase",
    /// unary `-exclude`, the literal `OR`. Arity 1. **PG-only**.
    /// Issue #28 follow-up.
    WebsearchToTsQuery,
    /// `to_tsquery(<expr>)` — Postgres FTS query parser for the
    /// raw `tsquery` syntax (`'rust & orm'`, `'rust | python'`,
    /// `'rust & !python'`). Lower-level than `plainto_tsquery`;
    /// expects pre-parsed input. Arity 1. **PG-only**. Issue #28
    /// follow-up.
    ToTsQuery,
    /// `ts_rank_cd(<tsvector>, <tsquery>)` — cover-density variant
    /// of `ts_rank`. Same shape, different ranking algorithm
    /// (better for short documents). Arity 2. **PG-only**. Issue
    /// #28 follow-up.
    TsRankCd,

    // --- DB functions batch 1 (issue #266 / T1.4) ---
    /// `LPAD(s, len, fill)` — left-pad string `s` to `len` characters
    /// using `fill`. PG/MySQL native; SQLite gets a `printf`/`substr`
    /// fallback because the function isn't built-in. Arity 3.
    LPad,
    /// `RPAD(s, len, fill)` — right-pad. Same dialect map as `LPad`.
    RPad,
    /// `MD5(s)` → hex string. PG `md5()` (built-in), MySQL `MD5()`
    /// (built-in). **SQLite errors** with `OpNotSupportedInDialect`
    /// (no built-in hash; the app should hash before binding). Arity 1.
    Md5,
    /// `SHA1(s)` → hex string. PG `encode(digest(s, 'sha1'), 'hex')`
    /// (requires `pgcrypto`), MySQL `SHA1()` (built-in). **SQLite errors**.
    /// Arity 1.
    Sha1,
    /// `SHA256(s)` → hex string. PG `encode(digest(s, 'sha256'), 'hex')`
    /// (requires `pgcrypto`), MySQL `SHA2(s, 256)`. **SQLite errors**.
    /// Arity 1.
    Sha256,
    /// `POSITION(needle IN hay)` (PG) / `LOCATE(needle, hay)` (MySQL) /
    /// `INSTR(hay, needle)` (SQLite). All return the 1-indexed position
    /// of the first occurrence, or 0 if not found. Arity 2: `(needle, hay)`.
    Position,
    /// `REPEAT(s, n)` — repeat string `s`, `n` times. PG/MySQL native;
    /// SQLite gets a `replace(printf('%.*c', n, ' '), ' ', s)` workaround.
    /// Arity 2.
    Repeat,
    /// `REVERSE(s)` — reverse a string. PG and MySQL native;
    /// **SQLite errors** (no built-in). Arity 1.
    Reverse,
    /// `SIGN(x)` → -1, 0, or 1. PG/MySQL native; SQLite emits a
    /// `CASE WHEN x>0 THEN 1 WHEN x<0 THEN -1 ELSE 0 END` expansion.
    /// Arity 1.
    Sign,
    /// `POWER(a, b)` — `a` raised to the `b`th. PG/MySQL ship native
    /// `power`/`POWER`. **SQLite caveat**: the function exists in 3.35+
    /// only when `SQLITE_ENABLE_MATH_FUNCTIONS` was set at build time;
    /// sqlx-sqlite does not enable it by default, so the writer errors
    /// with `OpNotSupportedInDialect` rather than emit SQL that the
    /// runtime would reject. Arity 2.
    Power,
    /// `SQRT(x)` — square root. PG/MySQL native; SQLite has the same
    /// build-flag caveat as [`Self::Power`]. Arity 1.
    Sqrt,

    // --- DB functions batch 2 (issue #294 / T2.7) ---
    /// `LN(x)` — natural log (base e). PG `ln(x)`, MySQL `LN(x)`,
    /// SQLite `ln(x)` (3.35+ with `SQLITE_ENABLE_MATH_FUNCTIONS`;
    /// emitter errors on default sqlx-sqlite builds). Arity 1.
    Log,
    /// `LOG(base, x)` — log of `x` in base `base`. PG `log(base, x)`,
    /// MySQL `LOG(base, x)`, SQLite `log(base, x)` (3.35+ with the
    /// math-functions build flag; emitter errors otherwise). Arity 2:
    /// `(base, x)`.
    LogWithBase,
    /// `EXP(x)` — `e^x`. PG `exp(x)`, MySQL `EXP(x)`, SQLite `exp(x)`
    /// (3.35+ build-flag caveat; emitter errors otherwise). Arity 1.
    Exp,
    /// `PI()` — π as a numeric constant. PG `pi()`, MySQL `PI()`,
    /// SQLite emits the literal `3.141592653589793` (no native fn;
    /// always available). Arity 0.
    Pi,
    /// `RANDOM()` — pseudo-random number. **Return-range divergence**:
    /// PG `random()` and MySQL `RAND()` return a float in `[0, 1)`,
    /// while SQLite `random()` returns a signed 64-bit integer in
    /// `[-2^63, 2^63)`. Callers that need cross-dialect `[0, 1)`
    /// normalization should do the math app-side. Arity 0.
    Random,
    /// `MAKE_INTERVAL(years, months, days, hours, minutes, seconds)`.
    /// **PG-only**: MySQL has no native `interval` type and SQLite has
    /// neither — both emit `OpNotSupportedInDialect`. The emitter uses
    /// PG's keyword-arg shape (`make_interval(years => $1, …)`). Arity 6.
    MakeInterval,
    /// `AGE(ts1, ts2)` — duration between two timestamps. **Return-type
    /// divergence**:
    /// - PG `age(ts1, ts2)` returns `interval`.
    /// - MySQL `TIMESTAMPDIFF(SECOND, ts2, ts1)` returns numeric seconds.
    /// - SQLite `(julianday(ts1) - julianday(ts2)) * 86400.0` returns
    ///   a `REAL` count of seconds.
    ///
    /// PG callers receive an interval; MySQL / SQLite callers receive
    /// a numeric count of seconds. Cross-dialect app code should cast
    /// to a canonical numeric or call only on a single backend. Arity 2:
    /// `(ts1, ts2)`.
    Age,
    /// `TRUNC_WITH_TZ(ts, unit, tz)` — timezone-aware date_trunc.
    /// PG: `date_trunc(unit, ts AT TIME ZONE tz)`. MySQL:
    /// `DATE_FORMAT(CONVERT_TZ(ts, '+00:00', tz), '<unit-format>')`.
    /// SQLite: `strftime(<unit-format>, ts, <tz-modifier>)` — TZ awareness
    /// is approximate (sqlite has no TZ DB; pass a fixed `±HH:MM`).
    ///
    /// `unit` and `tz` are bound as string literals at write time; the
    /// caller passes `unit` as one of `"year" | "month" | "day" |
    /// "hour" | "minute" | "second"`. Other values emit
    /// `OpNotSupportedInDialect`. Arity 3: `(ts, unit, tz)`.
    TruncWithTz,

    // --- Full-text-search builder (issue #295 / T2.4) ---
    /// `setweight(<tsvector>, <'A'|'B'|'C'|'D'>)` — Postgres weighting
    /// modifier used by [`crate::core::fts::SearchVector::weighted`].
    /// **PG-only**; MySQL / SQLite reject with
    /// `OpNotSupportedInDialect`. Arity 2: `(tsvector, weight_literal)`
    /// where the weight literal is an `Expr::Literal(SqlValue::String("A"))`-shaped value.
    SetWeight,
    /// `(a || b || c)` over tsvector operands — Postgres uses the `||`
    /// operator to concatenate tsvectors. The builder uses this rather
    /// than `Concat` (which would emit `CONCAT(...)` returning text
    /// and dropping tsvector semantics). **PG-only**; MySQL / SQLite
    /// reject with `OpNotSupportedInDialect`. Variadic ≥ 2 args.
    TsConcat,

    // --- PostGIS spatial functions (GeoDjango queries, issue #58) ---
    /// `ST_Distance(a, b)` — distance between two geometries in the
    /// column's SRID units (degrees for 4326). Returns `double
    /// precision`; use for nearest-neighbour ordering. Arity 2.
    /// **PG/PostGIS-only** — MySQL / SQLite reject with
    /// `OpNotSupportedInDialect`. Pairs with the [`crate::sql::Point`]
    /// type (#443).
    StDistance,
    /// `ST_DWithin(a, b, distance)` — `true` when `a` is within
    /// `distance` (SRID units) of `b`. The index-friendly "within
    /// radius" / geofence predicate. Returns `boolean`. Arity 3.
    /// **PG/PostGIS-only**.
    StDWithin,
    /// `ST_Contains(a, b)` — `true` when geometry `a` completely
    /// contains `b`. Returns `boolean`. Arity 2. **PG/PostGIS-only**.
    StContains,
    /// `ST_Within(a, b)` — `true` when geometry `a` is completely
    /// inside `b` (the converse of [`Self::StContains`]). Returns
    /// `boolean`. Arity 2. **PG/PostGIS-only**.
    StWithin,
    /// `ST_Intersects(a, b)` — `true` when the geometries share any
    /// point. Returns `boolean`. Arity 2. **PG/PostGIS-only**.
    StIntersects,
}

impl Expr {
    /// Build a column-reference expression. Sugar for `Expr::Column`.
    #[must_use]
    pub fn col(name: &'static str) -> Self {
        Self::Column(name)
    }

    /// Compose a `BinOp` with `self` on the left and `rhs` on the right.
    /// Boxes both sides for you.
    #[must_use]
    pub fn binop(self, op: BinOp, rhs: impl Into<Expr>) -> Self {
        Self::BinOp {
            left: Box::new(self),
            op,
            right: Box::new(rhs.into()),
        }
    }

    /// `true` when this expression is a pure literal — no column refs,
    /// no arithmetic. Used by writers as a fast path that skips the
    /// dialect dispatch on the column-emit branch.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        matches!(self, Self::Literal(_))
    }

    /// Extract the underlying `SqlValue` if `self` is `Literal`,
    /// otherwise `None`. Convenience for writers that have a fast
    /// literal-only path.
    #[must_use]
    pub fn as_literal(&self) -> Option<&SqlValue> {
        match self {
            Self::Literal(v) => Some(v),
            _ => None,
        }
    }
}

// ---------- From impls — let existing call sites transparently lift ----------

impl From<SqlValue> for Expr {
    fn from(v: SqlValue) -> Self {
        Self::Literal(v)
    }
}

/// Generate `From<$primitive> for Expr` for each primitive whose
/// `Into<SqlValue>` already exists. Avoids a blanket
/// `impl<T: Into<SqlValue>> From<T> for Expr`, which would conflict
/// with `From<SqlValue> for Expr` (SqlValue: Into<SqlValue> trivially).
macro_rules! expr_from_primitive {
    ($($t:ty),+ $(,)?) => {
        $(
            impl From<$t> for Expr {
                fn from(v: $t) -> Self { Self::Literal(SqlValue::from(v)) }
            }
        )+
    };
}

expr_from_primitive! {
    i16, i32, i64, f32, f64, bool, String, &'static str,
    chrono::DateTime<chrono::Utc>, chrono::NaiveDate, uuid::Uuid,
    serde_json::Value,
}

// ---------- F() public sugar ----------

/// Django-shape `F("col")` builder. Produces an [`Expr::Column`] when
/// passed to anywhere that expects `impl Into<Expr>`. The whole point
/// is to give the call site a tiny visible marker that something is
/// a column reference rather than a string literal value:
///
/// ```ignore
/// .update().set("views", F("views") + 1).execute_pool(&pool).await?;
/// //              ^^^^^^^ column ref      ^^^ literal
/// ```
///
/// Operator-overloaded for the common shapes — every `F(_) <op> rhs`
/// where `rhs: Into<Expr>` returns an [`Expr::BinOp`] directly, no
/// `.into()` call needed at the use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)] // Django's `F(...)` is the established name.
pub struct F(pub &'static str);

impl F {
    /// Construct an F by name. Equivalent to the tuple struct
    /// constructor but reads as a function call.
    #[must_use]
    pub fn new(column: &'static str) -> Self {
        Self(column)
    }
}

impl From<F> for Expr {
    fn from(f: F) -> Self {
        Self::Column(f.0)
    }
}

// ---------- Operator overloads on both `F` and `Expr` ----------

/// Generate `impl ops::$Trait<R> for $Lhs` for every supported RHS
/// expression-convertible type. `$Trait` is the std ops trait; `$method`
/// is its single method; `$op` is the [`BinOp`] variant.
macro_rules! impl_binop {
    ($($Trait:ident :: $method:ident => $op:ident),+ $(,)?) => {
        $(
            impl<R: Into<Expr>> ops::$Trait<R> for F {
                type Output = Expr;
                fn $method(self, rhs: R) -> Expr {
                    Expr::Column(self.0).binop(BinOp::$op, rhs)
                }
            }
            impl<R: Into<Expr>> ops::$Trait<R> for Expr {
                type Output = Expr;
                fn $method(self, rhs: R) -> Expr {
                    self.binop(BinOp::$op, rhs)
                }
            }
        )+
    };
}

impl_binop! {
    Add::add => Add,
    Sub::sub => Sub,
    Mul::mul => Mul,
    Div::div => Div,
    Rem::rem => Mod,
    BitAnd::bitand => BitAnd,
    BitOr::bitor => BitOr,
    BitXor::bitxor => BitXor,
    Shl::shl => BitShl,
    Shr::shr => BitShr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f_lifts_to_column_expr() {
        let e: Expr = F("views").into();
        assert_eq!(e, Expr::Column("views"));
    }

    #[test]
    fn f_add_int_builds_binop() {
        let e: Expr = F("views") + 1;
        assert_eq!(
            e,
            Expr::BinOp {
                left: Box::new(Expr::Column("views")),
                op: BinOp::Add,
                right: Box::new(Expr::Literal(SqlValue::I32(1))),
            }
        );
    }

    #[test]
    fn f_add_f_builds_column_column_binop() {
        let e: Expr = F("a") + F("b");
        assert_eq!(
            e,
            Expr::BinOp {
                left: Box::new(Expr::Column("a")),
                op: BinOp::Add,
                right: Box::new(Expr::Column("b")),
            }
        );
    }

    #[test]
    fn arithmetic_chains_left_assoc() {
        // `(F("a") + 1) - 2` — Rust's precedence + left-assoc gives a
        // BinOp where the left is the inner BinOp.
        let e: Expr = F("a") + 1 - 2;
        let Expr::BinOp { left, op, right } = e else {
            panic!("expected outer BinOp");
        };
        assert_eq!(op, BinOp::Sub);
        assert_eq!(*right, Expr::Literal(SqlValue::I32(2)));
        let Expr::BinOp { op: inner_op, .. } = *left else {
            panic!("expected nested BinOp")
        };
        assert_eq!(inner_op, BinOp::Add);
    }

    #[test]
    fn sqlvalue_lifts_into_expr_literal() {
        let e: Expr = SqlValue::I64(42).into();
        assert_eq!(e, Expr::Literal(SqlValue::I64(42)));
    }

    #[test]
    fn primitives_lift_into_expr_literal() {
        let e: Expr = 7i64.into();
        assert_eq!(e, Expr::Literal(SqlValue::I64(7)));
        let e: Expr = "hi".into();
        assert_eq!(e, Expr::Literal(SqlValue::String("hi".to_owned())));
    }

    #[test]
    fn is_literal_distinguishes() {
        assert!(Expr::Literal(SqlValue::I32(1)).is_literal());
        assert!(!Expr::Column("x").is_literal());
        assert!(!(F("a") + 1).is_literal());
    }

    #[test]
    fn bitwise_operators_compile_and_compose() {
        let e: Expr = F("mask") & 0xff_i32;
        assert!(matches!(
            e,
            Expr::BinOp {
                op: BinOp::BitAnd,
                ..
            }
        ));
        let e: Expr = F("a") | F("b");
        assert!(matches!(
            e,
            Expr::BinOp {
                op: BinOp::BitOr,
                ..
            }
        ));
        let e: Expr = F("a") << 4_i32;
        assert!(matches!(
            e,
            Expr::BinOp {
                op: BinOp::BitShl,
                ..
            }
        ));
    }
}
