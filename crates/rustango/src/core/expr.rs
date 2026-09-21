//! `Expr` — RHS expression for [`crate::core::query::Assignment`] +
//! column-vs-column comparisons in [`crate::core::query::ColumnFilter`].
//!
//! Lets a query refer to a column by name inside an UPDATE SET or a
//! WHERE predicate:
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
//! The base variants are [`Expr::Literal`] (a bound parameter),
//! [`Expr::Column`] (a column reference the writer quotes per dialect:
//! `"col"` on PG/SQLite, `` `col` `` on MySQL) and [`Expr::BinOp`]
//! (recursive arithmetic). Functions, `CASE`, subqueries and windows
//! build on those.
//!
//! Every `BinOp` is wrapped in `()` on emit. That is more parens than
//! needed, but it keeps the writer simple and always correct.
//!
//! # Why `Expr` is separate from `SqlValue`
//!
//! `SqlValue` is a *value*: its `Display`, `field_type()` and sqlx
//! encode/decode paths all assume a real literal. A column reference
//! has no `field_type()` until the schema resolves it, and it does not
//! bind as a parameter. Two types keep both honest.

use std::ops;

use super::value::SqlValue;

/// Binary arithmetic operator. Emits its SQL symbol as-is.
///
/// `Add`, `Sub`, `Mul`, `Div` and `Mod` work on all three dialects.
/// So do the bitwise ops, except `BitXor`: SQLite has no XOR operator,
/// so the writer returns `SqlError::OpNotSupportedInDialect` there.
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
    /// pgvector L2 (Euclidean) distance — `<->`. **PG-only**; the
    /// writer raises `OpNotSupportedInDialect` on MySQL / SQLite.
    L2Distance,
    /// pgvector cosine distance — `<=>`. **PG-only.**
    CosineDistance,
    /// pgvector inner product — `<#>`. **PG-only.** pgvector negates
    /// it, so ascending order still ranks the most similar first.
    InnerProduct,
}

/// Distance metric for pgvector similarity search — what you pass to
/// [`crate::query::QuerySet::order_by_distance`] and
/// [`k_nearest`](crate::query::QuerySet::k_nearest). Each one maps to
/// a pgvector operator such as [`BinOp::L2Distance`]. Ascending order
/// always ranks the most similar row first.
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

/// RHS expression — literal, column reference, arithmetic tree,
/// function call, and more.
///
/// The writer renders an `Expr` to the right of `=` in an UPDATE
/// assignment, and to the right of a column predicate in a WHERE
/// clause. The variants are recursive, so arithmetic and calls nest
/// as deep as you need.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Bound value. Emitter pushes a parameter.
    Literal(SqlValue),
    /// Column reference. Emitter writes the quoted identifier.
    /// `&'static str` matches how the rest of the IR names columns.
    Column(&'static str),
    /// Binary arithmetic: `left <op> right`. Recursive — either side
    /// may be a nested `BinOp`.
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    /// Scalar function call — `FN(arg, arg, …)`. Every arity folds
    /// into `args`, so the writer never switches on argument count.
    /// See [`crate::core::funcs`] for the builders.
    Function { kind: ScalarFn, args: Vec<Expr> },
    /// `CASE WHEN c1 THEN t1 [WHEN c2 THEN t2 …] [ELSE d] END`.
    /// Standard SQL, emitted the same way on all three dialects.
    /// `branches` holds the `WHEN` arms in order; `default` is the
    /// optional `ELSE`, left out of the SQL when `None`.
    ///
    /// Build it with [`crate::core::case::case()`] — the chain reads
    /// closer to the SQL and does the boxing for you.
    Case {
        branches: Vec<CaseBranch>,
        default: Option<Box<Expr>>,
    },
    /// Scalar subquery — `(SELECT col FROM … LIMIT 1)`. Fits any slot
    /// that wants one value: `set_expr`, `eq_expr`, a WHERE rhs.
    /// The inner [`SelectQuery`] comes from `QuerySet::compile()`, so
    /// schema errors surface when you build it, and one compiled
    /// subquery can be reused across statements.
    ///
    /// Shaping the inner query to one row and one column is on you.
    ///
    /// [`SelectQuery`]: crate::core::SelectQuery
    Subquery(Box<super::query::SelectQuery>),
    /// Scalar **aggregate** subquery — `(SELECT COUNT(*) FROM … WHERE …)`.
    /// Like [`Self::Subquery`], but the inner query is an
    /// [`AggregateQuery`], so it projects one aggregate instead of the
    /// child model's columns.
    ///
    /// Backs [`crate::query::QuerySet::where_has_count`]. The writer
    /// pushes the inner model's scope frame, so any [`Self::OuterRef`]
    /// inside points at the parent row, the same on all three dialects.
    ///
    /// Build it with [`crate::core::subquery::reverse_has_count`].
    ///
    /// [`AggregateQuery`]: crate::core::AggregateQuery
    AggregateSubquery(Box<super::query::AggregateQuery>),
    /// Correlated aggregate over a **raw relation table** — the M2M /
    /// GFK counterpart of [`AggregateSubquery`](Expr::AggregateSubquery)
    /// for tables with no `ModelSchema`. Emits
    /// `(SELECT <kind>(<column> | *) FROM <table> WHERE <correlation>)`.
    /// `column` is `None` for `COUNT(*)`, `Some(col)` otherwise. The
    /// link back to the outer row lives in
    /// [`RelCorrelation`](super::query::RelCorrelation).
    RelAggregate {
        kind: super::query::RelAggKind,
        column: Option<&'static str>,
        table: &'static str,
        correlation: super::query::RelCorrelation,
    },
    /// A reference to an outer query's column from inside a correlated
    /// subquery. Emitted as `"<outer_table>"."<col>"`; the writer
    /// threads the outer table through at emit time, so nested
    /// `EXISTS`, `IN (SELECT …)` and scalar subqueries can all read
    /// the outer row.
    ///
    /// Build it with [`crate::core::subquery::outer_ref`], which reads
    /// like Django's `OuterRef('col')`.
    OuterRef(&'static str),
    /// Column qualified by an explicit table alias —
    /// `"<alias>"."<column>"`. Needed in JOIN `ON` predicates, where
    /// either side may name a column on another table and a bare
    /// `Column(name)` would resolve against the wrong one.
    ///
    /// Build it with [`crate::core::joins::aliased`].
    AliasedColumn {
        alias: &'static str,
        column: &'static str,
    },
    /// Window function — `<fn>(args) OVER (PARTITION BY … ORDER BY …
    /// [frame])`. Boxed to keep `Expr` small, since [`WindowExpr`]
    /// carries its own `Vec<Expr>`.
    ///
    /// Build it with [`crate::core::window`] (`row_number`, `rank`,
    /// `dense_rank`, `lag`, `lead`, `first_value`, `last_value`,
    /// `ntile`).
    ///
    /// [`WindowExpr`]: crate::core::WindowExpr
    Window(Box<super::window::WindowExpr>),
    /// An aggregate lifted into the `Expr` tree, so it can sit in a
    /// `HAVING` predicate through
    /// [`super::query::WhereExpr::ExprCompare`]. PG needs the whole
    /// expression there, not the SELECT alias. It also composes
    /// inside `Case`, `Coalesce` and `set_expr` when the query
    /// aggregates. Boxed to keep `Expr` small.
    Aggregate(Box<super::query::AggregateExpr>),
    /// `CAST(<expr> AS <ty>)` — explicit type coercion. `ty` is a
    /// dialect-neutral [`crate::core::FieldType`]; the writer maps it
    /// to the dialect's SQL token via
    /// [`crate::sql::Dialect::null_cast`]. Only that token differs
    /// between PG / MySQL / SQLite.
    Cast {
        expr: Box<Expr>,
        ty: super::field_type::FieldType,
    },
    /// JSON path extraction — `<source> -> 'k1' -> 'k2' ->> 'k3'` on
    /// PG, `JSON_UNQUOTE(JSON_EXTRACT(<source>, '$.k1.k2.k3'))` on
    /// MySQL, `json_extract(<source>, '$.k1.k2.k3')` on SQLite.
    /// `as_text = true` asks for the unwrapped text form; `false`
    /// keeps the JSON-typed form for further chaining.
    JsonPath {
        source: Box<Expr>,
        path: Vec<JsonPathStep>,
        as_text: bool,
    },
}

/// One step of a [`Expr::JsonPath`] traversal. A key looks up an
/// object member (`{"k": v}` → `Key("k")`); an index looks up an
/// array element (`[a, b, c]` → `Index(0)`).
///
/// A negative index counts from the end. That works on PG and SQLite.
/// MySQL's path grammar has no negative form, so the writer rejects
/// one there with a clear error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathStep {
    /// Object key lookup. Emitted as a quoted SQL literal on PG
    /// (`-> 'name'`) and inside the path string on MySQL / SQLite
    /// (`$.name`). A key may only hold ASCII letters, digits and `_`.
    /// The writer rejects anything else, because the path is inlined
    /// and must stay injection-safe.
    Key(String),
    /// Array index lookup, 0-based.
    Index(i64),
}

/// One arm of a [`Expr::Case`] — `WHEN <condition> THEN <then>`.
///
/// `condition` is a full [`crate::core::WhereExpr`] tree, so the same
/// `Column::eq()` / `.and()` / `.or()` builders used for `WHERE` work
/// here too. `then` is any [`Expr`], including a nested `Case`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    pub condition: super::query::WhereExpr,
    pub then: Expr,
}

/// Scalar database functions, built through [`crate::core::funcs`].
///
/// The writer handles the per-dialect differences. Each variant below
/// notes the ones a caller has to know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFn {
    // --- Text ---
    /// `LOWER(s)` — lowercase a string.
    Lower,
    /// `UPPER(s)` — uppercase a string.
    Upper,
    /// `LENGTH(s)` — string length. Chars on PG and on SQLite `TEXT`,
    /// but bytes on MySQL, where `CHAR_LENGTH` counts chars.
    Length,
    /// `CONCAT(a, b, …)` — string concatenation. SQLite emits
    /// `a || b || …`, the only form that works before 3.44. All three
    /// dialects return NULL when any operand is NULL.
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

    // --- Math ---
    /// `ABS(x)` — absolute value.
    Abs,
    /// `CEIL(x)` — ceiling.
    Ceil,
    /// `FLOOR(x)` — floor.
    Floor,
    /// `ROUND(x)` or `ROUND(x, n)`. PG rounds half to even; MySQL and
    /// SQLite round half away from zero.
    Round,

    // --- Comparison / NULL handling ---
    /// `COALESCE(a, b, c, …)` — first non-NULL argument. Variadic.
    /// Returns NULL only if every argument is NULL.
    Coalesce,
    /// `GREATEST(a, b, …)` — the largest value. Native on PG and
    /// MySQL; SQLite uses scalar `MAX(a, b, …)`. NULL differs: PG and
    /// SQLite return NULL if any operand is NULL, MySQL skips NULLs.
    /// Wrap the args in `COALESCE` when nulls are possible.
    Greatest,
    /// `LEAST(a, b, …)` — mirror of `Greatest`.
    Least,
    /// `NULLIF(a, b)` — `NULL` when `a == b`, else `a`. Universal.
    NullIf,

    // --- Date / time ---
    /// The server's wall-clock timestamp. 0-arg. `NOW()` on PG and
    /// MySQL; on SQLite an RFC3339 `strftime(…, 'now')`, so the text
    /// matches what every other write path stores.
    Now,
    /// `EXTRACT(YEAR FROM x)` family. SQLite wraps in
    /// `CAST(strftime('%Y', x) AS INTEGER)`, PG casts to `integer`, so
    /// the return type is the same everywhere.
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
    /// Week-of-year. **Not portable**: each backend numbers weeks its
    /// own way, so one date gives three values.
    /// - PG (`EXTRACT(WEEK FROM x)`): ISO 8601, Monday start, 1–53.
    /// - MySQL (`WEEK(x)` mode 0): **Sunday** start, **0**–53.
    /// - SQLite (`strftime('%W', x)`): Monday start, 00–53.
    ///
    /// For 2024-01-01 (a Monday): PG=1, MySQL=0, SQLite=01. Use it on
    /// one backend only, or compute the week start in app code as a
    /// typed `chrono::DateTime`.
    ExtractWeek,
    /// Day-of-week, **normalized to 0 = Sunday, 6 = Saturday** on all
    /// three dialects. MySQL's `DAYOFWEEK()` counts from 1, so the
    /// writer subtracts 1; SQLite's `strftime('%w')` already matches.
    ExtractWeekDay,
    /// Quarter (1–4). Native on PG and MySQL; SQLite has no quarter
    /// token, so the writer computes it from the month.
    ExtractQuarter,
    /// `DATE(x)` — drop the time part, returning a `DATE`. Same SQL
    /// on all three backends.
    TruncDate,
    /// Start of the year. PG: `DATE_TRUNC('year', x)`. MySQL:
    /// `DATE_FORMAT(x, '%Y-01-01')`. SQLite: `strftime('%Y-01-01', x)`.
    /// **PG returns a timestamp, the other two text** — cast app-side
    /// if you need a typed date.
    TruncYear,
    /// Start of the month. Same return-type split as `TruncYear`.
    TruncMonth,
    /// Start of the day. PG: `DATE_TRUNC('day', x)` (timestamp).
    /// MySQL / SQLite: `DATE(x)` / `date(x)` (date).
    TruncDay,

    // --- JSON helpers ---
    /// `JSON_ARRAY_LENGTH(x)` — element count of a JSON array.
    /// Arity 1. PG: `jsonb_array_length(x)`, MySQL: `JSON_LENGTH(x)`,
    /// SQLite: `json_array_length(x)`. Non-array input differs: PG
    /// errors, MySQL returns 1, SQLite returns 0. Compare it to
    /// filter by length: `WHERE JSON_ARRAY_LENGTH(opts -> 'tags') > 0`.
    JsonArrayLength,

    // --- pg_trgm ---
    /// `SIMILARITY(a, b)` — whole-string trigram similarity, a `real`
    /// in `[0, 1]`. Arity 2. Needs `CREATE EXTENSION pg_trgm`.
    /// **PG-only** — MySQL / SQLite emit `OpNotSupportedInDialect`.
    /// Pairs with the `Op::TrigramSimilar` WHERE operator.
    TrigramSimilarity,
    /// `WORD_SIMILARITY(a, b)` — word-level similarity. Arity 2.
    /// **PG-only**, same rules as [`Self::TrigramSimilarity`]. Pairs
    /// with `Op::TrigramWordSimilar`.
    TrigramWordSimilarity,

    // --- Postgres full-text search ---
    /// `to_tsvector(<expr>)` — build a `tsvector` from text using the
    /// database's default search config. Arity 1. **PG-only** —
    /// MySQL / SQLite emit `OpNotSupportedInDialect`. Pairs with the
    /// `Op::Search` WHERE operator.
    ToTsVector,
    /// `plainto_tsquery(<expr>)` — parse a plain user string into a
    /// `tsquery`. Arity 1. **PG-only.**
    PlainToTsQuery,
    /// `ts_rank(<tsvector>, <tsquery>)` — FTS relevance score
    /// (`real`). Order by it with `to_tsvector` + `plainto_tsquery`.
    /// Arity 2. **PG-only.**
    TsRank,
    /// `ts_headline(<doc>, <tsquery> [, <options>])` — FTS snippet.
    /// Returns the document with matches wrapped in markers
    /// (`<b>…</b>` by default). `options` overrides them, for example
    /// `"StartSel='<mark>', StopSel='</mark>', MaxFragments=1"`.
    /// Arity 2 or 3. **PG-only.**
    TsHeadline,
    /// `phraseto_tsquery(<expr>)` — keeps word order
    /// (`'rust orm'` → `'rust' <-> 'orm'`). Use it when the exact
    /// phrase matters. Arity 1. **PG-only.**
    PhraseToTsQuery,
    /// `websearch_to_tsquery(<expr>)` — accepts Google-style syntax:
    /// quoted "exact phrase", `-exclude`, the literal `OR`. Arity 1.
    /// **PG-only.**
    WebsearchToTsQuery,
    /// `to_tsquery(<expr>)` — the raw `tsquery` syntax
    /// (`'rust & orm'`, `'rust | python'`, `'rust & !python'`). Lower
    /// level than `plainto_tsquery`: the input must already be valid.
    /// Arity 1. **PG-only.**
    ToTsQuery,
    /// `ts_rank_cd(<tsvector>, <tsquery>)` — cover-density ranking.
    /// Same shape as `ts_rank`, better for short documents. Arity 2.
    /// **PG-only.**
    TsRankCd,

    // --- Cast, padding, hashes, more math ---
    /// `LPAD(s, len, fill)` — left-pad `s` to `len` characters with
    /// `fill`. Arity 3. Native on PG / MySQL; SQLite has no such
    /// function, so it gets a `printf`/`substr` fallback.
    LPad,
    /// `RPAD(s, len, fill)` — right-pad. Same dialect map as `LPad`.
    RPad,
    /// `MD5(s)` → hex string. Arity 1. Built in on PG and MySQL.
    /// **SQLite errors** with `OpNotSupportedInDialect`; hash in app
    /// code before binding instead.
    Md5,
    /// `SHA1(s)` → hex string. Arity 1. PG uses
    /// `encode(digest(s, 'sha1'), 'hex')`, so it needs `pgcrypto`;
    /// MySQL has `SHA1()`. **SQLite errors.**
    Sha1,
    /// `SHA256(s)` → hex string. Arity 1. PG uses
    /// `encode(digest(s, 'sha256'), 'hex')` (needs `pgcrypto`), MySQL
    /// `SHA2(s, 256)`. **SQLite errors.**
    Sha256,
    /// `POSITION(needle IN hay)` (PG) / `LOCATE(needle, hay)` (MySQL) /
    /// `INSTR(hay, needle)` (SQLite). All return the 1-indexed position
    /// of the first match, or 0 when there is none. Arity 2:
    /// `(needle, hay)`.
    Position,
    /// `REPEAT(s, n)` — repeat `s` `n` times. Arity 2. Native on PG /
    /// MySQL; SQLite gets a `replace(printf('%.*c', n, ' '), ' ', s)`
    /// fallback.
    Repeat,
    /// `REVERSE(s)` — reverse a string. Arity 1. Native on PG and
    /// MySQL; **SQLite errors**, it has no such function.
    Reverse,
    /// `SIGN(x)` → -1, 0, or 1. Arity 1. Native on PG / MySQL; SQLite
    /// gets a `CASE WHEN x>0 THEN 1 WHEN x<0 THEN -1 ELSE 0 END`.
    Sign,
    /// `POWER(a, b)` — `a` raised to the `b`th. Arity 2. Native on PG
    /// and MySQL. **SQLite errors**: the function needs 3.35+ built
    /// with `SQLITE_ENABLE_MATH_FUNCTIONS`, which sqlx-sqlite does not
    /// set, so the writer fails early instead of at runtime.
    Power,
    /// `SQRT(x)` — square root. Arity 1. Native on PG / MySQL; SQLite
    /// has the same build-flag limit as [`Self::Power`].
    Sqrt,

    // --- Logs, constants, intervals ---
    /// `LN(x)` — natural log (base e). Arity 1. Native on PG / MySQL;
    /// same SQLite build-flag limit as [`Self::Power`].
    Log,
    /// `LOG(base, x)` — log of `x` in base `base`. Arity 2. Same
    /// SQLite build-flag limit as [`Self::Power`].
    LogWithBase,
    /// `EXP(x)` — `e^x`. Arity 1. Same SQLite build-flag limit as
    /// [`Self::Power`].
    Exp,
    /// `PI()` — π as a numeric constant. Arity 0. PG `pi()`, MySQL
    /// `PI()`; SQLite has no such function, so the writer emits the
    /// literal `3.141592653589793`.
    Pi,
    /// `RANDOM()` — pseudo-random number. Arity 0. **The range
    /// differs**: PG and MySQL return a float in `[0, 1)`, SQLite a
    /// signed 64-bit integer in `[-2^63, 2^63)`. Normalize app-side
    /// for portable code.
    Random,
    /// `MAKE_INTERVAL(years, months, days, hours, minutes, seconds)`.
    /// Arity 6. **PG-only**; MySQL and SQLite have no `interval` type
    /// and emit `OpNotSupportedInDialect`. The writer uses PG's
    /// keyword-arg shape, `make_interval(years => $1, …)`.
    MakeInterval,
    /// `AGE(ts1, ts2)` — time between two timestamps. Arity 2.
    /// **The return type differs**:
    /// - PG `age(ts1, ts2)` → `interval`.
    /// - MySQL `TIMESTAMPDIFF(SECOND, ts2, ts1)` → numeric seconds.
    /// - SQLite `(julianday(ts1) - julianday(ts2)) * 86400.0` → a
    ///   `REAL` count of seconds.
    ///
    /// Portable code should cast to one numeric type, or call this on
    /// a single backend.
    Age,
    /// `TRUNC_WITH_TZ(ts, unit, tz)` — timezone-aware date_trunc.
    /// Arity 3. PG: `date_trunc(unit, ts AT TIME ZONE tz)`. MySQL:
    /// `DATE_FORMAT(CONVERT_TZ(ts, '+00:00', tz), '<unit-format>')`.
    /// SQLite: `strftime(<unit-format>, ts, <tz-modifier>)`, which is
    /// approximate — SQLite has no TZ database, so pass a fixed
    /// `±HH:MM`.
    ///
    /// `unit` and `tz` are written as string literals. `unit` must be
    /// one of `"year" | "month" | "day" | "hour" | "minute" |
    /// "second"`; anything else emits `OpNotSupportedInDialect`.
    TruncWithTz,

    // --- Full-text-search builder ---
    /// `setweight(<tsvector>, <'A'|'B'|'C'|'D'>)` — the weighting
    /// modifier behind [`crate::core::fts::SearchVector::weighted`].
    /// Arity 2: `(tsvector, weight_literal)`, where the weight is an
    /// `Expr::Literal(SqlValue::String("A"))`. **PG-only.**
    SetWeight,
    /// `(a || b || c)` over tsvector operands. PG concatenates
    /// tsvectors with `||`. `Concat` would emit `CONCAT(...)`, which
    /// returns text and loses the tsvector type, so this is separate.
    /// Two or more args. **PG-only.**
    TsConcat,

    // --- PostGIS spatial functions ---
    /// `ST_Distance(a, b)` — distance between two geometries in the
    /// column's SRID units (degrees for 4326), as `double precision`.
    /// Arity 2. Use it for nearest-neighbour ordering. Pairs with
    /// [`crate::sql::Point`]. **PG/PostGIS-only** — MySQL / SQLite
    /// emit `OpNotSupportedInDialect`.
    StDistance,
    /// `ST_DWithin(a, b, distance)` — `true` when `a` is within
    /// `distance` (SRID units) of `b`. The index-friendly geofence
    /// predicate. Arity 3. **PG/PostGIS-only.**
    StDWithin,
    /// `ST_Contains(a, b)` — `true` when `a` fully contains `b`.
    /// Arity 2. **PG/PostGIS-only.**
    StContains,
    /// `ST_Within(a, b)` — `true` when `a` sits fully inside `b`, the
    /// converse of [`Self::StContains`]. Arity 2. **PG/PostGIS-only.**
    StWithin,
    /// `ST_Intersects(a, b)` — `true` when the geometries share any
    /// point. Arity 2. **PG/PostGIS-only.**
    StIntersects,
}

impl Expr {
    /// Build a column-reference expression. Sugar for `Expr::Column`.
    #[must_use]
    pub fn col(name: &'static str) -> Self {
        Self::Column(name)
    }

    /// Build `self <op> rhs`. Boxes both sides for you.
    #[must_use]
    pub fn binop(self, op: BinOp, rhs: impl Into<Expr>) -> Self {
        Self::BinOp {
            left: Box::new(self),
            op,
            right: Box::new(rhs.into()),
        }
    }

    /// `true` when this is a plain literal: no column refs, no
    /// arithmetic. Writers use it as a fast path.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        matches!(self, Self::Literal(_))
    }

    /// The inner `SqlValue` when `self` is `Literal`, else `None`.
    #[must_use]
    pub fn as_literal(&self) -> Option<&SqlValue> {
        match self {
            Self::Literal(v) => Some(v),
            _ => None,
        }
    }
}

// ---------- From impls — let call sites lift a value into an Expr ----------

impl From<SqlValue> for Expr {
    fn from(v: SqlValue) -> Self {
        Self::Literal(v)
    }
}

/// Generate `From<$primitive> for Expr` for each primitive that
/// already has `Into<SqlValue>`. A blanket
/// `impl<T: Into<SqlValue>> From<T> for Expr` would clash with
/// `From<SqlValue> for Expr`, so they are listed one by one.
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

/// Django-shape `F("col")` builder. It becomes an [`Expr::Column`]
/// anywhere `impl Into<Expr>` is accepted. The point is to mark at a
/// glance that an argument is a column, not a string value:
///
/// ```ignore
/// .update().set("views", F("views") + 1).execute_pool(&pool).await?;
/// //              ^^^^^^^ column ref      ^^^ literal
/// ```
///
/// The operators are overloaded, so `F(_) <op> rhs` gives an
/// [`Expr::BinOp`] with no `.into()` at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)] // Django's `F(...)` is the established name.
pub struct F(pub &'static str);

impl F {
    /// Build an `F` by name. Same as the tuple constructor, but it
    /// reads as a function call.
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

/// Generate the operator impls on both `F` and `Expr`. `$Trait` is
/// the std ops trait, `$method` its one method, `$op` the [`BinOp`].
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
