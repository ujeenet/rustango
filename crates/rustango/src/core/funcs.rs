//! Scalar database functions — text, math, comparison, date/time.
//!
//! Each builder returns an [`Expr::Function`]. Calls compose freely
//! with `F()`, arithmetic, other functions, and literal values.
//!
//! ```ignore
//! use rustango::core::F;
//! use rustango::core::funcs::{
//!     lower, concat, coalesce, greatest, round_to,
//!     now, extract_year, extract_month, extract_weekday, trunc_date,
//! };
//!
//! // Text — normalize a name on the way in.
//! .set_expr("name_norm", lower(F("name")))
//!
//! // Text — build a display string. Array elements must be homogeneous;
//! // `.into()` lifts each one to `Expr`.
//! .set_expr("display", concat([F("first").into(), " ".into(), F("last").into()]))
//!
//! // Comparison — pick the first non-NULL.
//! .set_expr("nickname", coalesce([F("nickname").into(), F("username").into(), "anon".into()]))
//!
//! // Math composed with arithmetic from #1.
//! .set_expr("rounded", round_to(F("score") * 100_i64, 0_i32))
//! .where_(Post::priority.eq_expr(greatest([F("a").into(), F("b").into(), 5_i64.into()])))
//!
//! // Date/time — server-side wall-clock + denormalize date components
//! // into indexable integer columns for cheap cohort queries.
//! .set_expr("published_at", now())
//! .set_expr("bucket_year", extract_year(F("created_at")))
//! .set_expr("weekday", extract_weekday(F("created_at")))    // 0 = Sunday
//! .set_expr("day_bucket", trunc_date(F("created_at")))      // DATE on every backend
//! ```
//!
//! ## Per-dialect notes
//!
//! - **`concat`** emits `||` on SQLite. That form works on every
//!   SQLite version; `concat()` only arrived in 3.44.
//! - **`greatest` / `least`** emit SQLite's scalar `MAX(a, b, …)` /
//!   `MIN(a, b, …)`. A single argument errors on SQLite, where it
//!   would mean the aggregate instead.
//! - **`length`** counts chars on PG and on SQLite `TEXT`, but bytes
//!   on MySQL. Same answer for ASCII, different for other text.
//! - **`round(x, n)`** wants a numeric on PG; a float needs a cast.
//!   MySQL and SQLite cast for you.
//! - **`now()`** emits `NOW()` on PG / MySQL and an RFC3339
//!   `strftime(…, 'now')` on SQLite, so the value matches what every
//!   other write path stores in a SQLite datetime column.
//! - **`extract_*`** always return an integer.
//! - **`extract_weekday`** is normalized to 0 = Sunday, 6 = Saturday
//!   on all three backends.
//! - **`extract_quarter`** gives the same value on all three backends.
//! - **⚠ `extract_week` does not.** Each backend numbers weeks in its
//!   own way, so one date gives three different values. Use it on a
//!   single backend only. Otherwise compute the week start as a
//!   `chrono::DateTime` in Rust and filter on the timestamp column.
//! - **`trunc_year` / `trunc_month`** return a timestamp on PG but
//!   text on MySQL / SQLite. Cast app-side if you need a typed
//!   `chrono::NaiveDate`. `trunc_date` is the one trunc builder with
//!   the same SQL everywhere.
//!
//! ## Composition with `F()` + arithmetic
//!
//! Every builder takes `impl Into<Expr>`, so [`F`], primitives,
//! [`SqlValue`] and any other [`Expr`] pass straight in:
//!
//! ```ignore
//! // Functions nest freely — each returns Expr.
//! upper(trim(F("name")))
//!
//! // Variadic builders take IntoIterator<Item = Expr>; lift each
//! // element with .into() at the call site.
//! concat([F("first").into(), " ".into(), F("last").into()])
//!
//! // Arithmetic from #1 composes with function results.
//! round_to(abs(F("score") * 100_i64), 2_i32)
//! ```
//!
//! [`F`]: crate::core::F
//! [`SqlValue`]: crate::core::SqlValue
//! [`Expr`]: crate::core::Expr
//! [`Expr::Function`]: crate::core::Expr::Function

use super::expr::{Expr, ScalarFn};

// ---------- Unary functions ----------

/// `LOWER(arg)`.
#[must_use]
pub fn lower(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Lower, arg)
}

/// `UPPER(arg)`.
#[must_use]
pub fn upper(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Upper, arg)
}

/// `LENGTH(arg)`. See module docs for char-vs-byte semantics.
#[must_use]
pub fn length(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Length, arg)
}

/// `TRIM(arg)` — strip leading and trailing whitespace.
#[must_use]
pub fn trim(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Trim, arg)
}

/// `LTRIM(arg)` — strip leading whitespace.
#[must_use]
pub fn ltrim(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::LTrim, arg)
}

/// `RTRIM(arg)` — strip trailing whitespace.
#[must_use]
pub fn rtrim(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::RTrim, arg)
}

/// `ABS(arg)` — absolute value.
#[must_use]
pub fn abs(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Abs, arg)
}

/// `CEIL(arg)` — ceiling. Emits `CEIL` on all three; SQLite needs 3.35+.
#[must_use]
pub fn ceil(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Ceil, arg)
}

/// `FLOOR(arg)` — floor.
#[must_use]
pub fn floor(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Floor, arg)
}

// ---------- Binary / 3-ary ----------

/// `ROUND(x)` — round to integer. See [`round_to`] for precision arg.
#[must_use]
pub fn round(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Round, arg)
}

/// `ROUND(x, n)` — round to `n` decimal places. `n` is typically an
/// integer literal; pass `0` for integer rounding.
#[must_use]
pub fn round_to(arg: impl Into<Expr>, n: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Round,
        args: vec![arg.into(), n.into()],
    }
}

/// `SUBSTRING(s, start, length)` — 1-indexed. PG uses the `FROM…FOR…`
/// form, MySQL / SQLite the comma form. Same result.
#[must_use]
pub fn substr(s: impl Into<Expr>, start: impl Into<Expr>, length: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Substr,
        args: vec![s.into(), start.into(), length.into()],
    }
}

/// `REPLACE(s, from, to)` — replace every non-overlapping match.
#[must_use]
pub fn replace(s: impl Into<Expr>, from: impl Into<Expr>, to: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Replace,
        args: vec![s.into(), from.into(), to.into()],
    }
}

/// `NULLIF(a, b)` — `NULL` when `a == b`, else `a`.
#[must_use]
pub fn nullif(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::NullIf,
        args: vec![a.into(), b.into()],
    }
}

// ---------- Variadic ----------

/// `CONCAT(a, b, …)` — string concatenation. SQLite emits `||`.
///
/// Takes `IntoIterator<Item = Expr>`. A Rust array is homogeneous, so
/// call `.into()` on every element:
///
/// ```ignore
/// concat([F("first").into(), " ".into(), F("last").into()])
/// ```
#[must_use]
pub fn concat<I>(args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    variadic(ScalarFn::Concat, args)
}

/// `COALESCE(a, b, c, …)` — first non-NULL argument.
/// See [`concat()`](crate::core::funcs::concat) re: passing args as
/// already-lifted `Expr`.
#[must_use]
pub fn coalesce<I>(args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    variadic(ScalarFn::Coalesce, args)
}

/// `GREATEST(a, b, …)` (PG/MySQL) / `MAX(a, b, …)` scalar (SQLite).
#[must_use]
pub fn greatest<I>(args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    variadic(ScalarFn::Greatest, args)
}

/// `LEAST(a, b, …)` (PG/MySQL) / `MIN(a, b, …)` scalar (SQLite).
#[must_use]
pub fn least<I>(args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    variadic(ScalarFn::Least, args)
}

// ---------- Date / time functions ----------

/// `NOW()` — server-side wall-clock timestamp. 0-arg. SQLite emits an
/// RFC3339 `strftime(…, 'now')` so the text matches other write paths.
#[must_use]
pub fn now() -> Expr {
    Expr::Function {
        kind: ScalarFn::Now,
        args: Vec::new(),
    }
}

/// `EXTRACT(YEAR FROM x)` — calendar year as integer.
#[must_use]
pub fn extract_year(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractYear, arg)
}

/// `EXTRACT(MONTH FROM x)` — month component (1–12) as integer.
#[must_use]
pub fn extract_month(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractMonth, arg)
}

/// `EXTRACT(DAY FROM x)` — day-of-month (1–31) as integer.
#[must_use]
pub fn extract_day(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractDay, arg)
}

/// `EXTRACT(HOUR FROM x)` — hour (0–23) as integer.
#[must_use]
pub fn extract_hour(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractHour, arg)
}

/// `EXTRACT(MINUTE FROM x)` — minute (0–59) as integer.
#[must_use]
pub fn extract_minute(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractMinute, arg)
}

/// `EXTRACT(SECOND FROM x)` — second (0–59) as integer.
#[must_use]
pub fn extract_second(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractSecond, arg)
}

/// `EXTRACT(WEEK FROM x)` — week-of-year as integer.
///
/// **⚠ Not portable.** Each backend numbers weeks its own way:
/// - PG: ISO 8601, Monday start, range 1–53.
/// - MySQL (default mode 0): **Sunday** start, range **0**–53.
/// - SQLite (`strftime('%W')`): Monday start, the first Monday of the
///   year begins week 01.
///
/// For 2024-01-01 (a Monday): PG=1, MySQL=0, SQLite=01.
///
/// Use it on a single backend only. For portable code, compute the
/// week start as a `chrono::DateTime` in Rust and filter the
/// timestamp column with `Column::gte()`.
#[must_use]
pub fn extract_week(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractWeek, arg)
}

/// `EXTRACT(DOW FROM x)` — day-of-week. **Normalized to 0 = Sunday,
/// 6 = Saturday** across all three dialects. See [`ScalarFn::ExtractWeekDay`].
#[must_use]
pub fn extract_weekday(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractWeekDay, arg)
}

/// `EXTRACT(QUARTER FROM x)` — quarter (1–4) as integer. Native on
/// PG / MySQL; SQLite computes it from the month as `((month + 2) / 3)`.
#[must_use]
pub fn extract_quarter(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractQuarter, arg)
}

/// `DATE(x)` — drop the time part of a timestamp. Returns `DATE`,
/// with the same SQL on all three backends.
#[must_use]
pub fn trunc_date(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::TruncDate, arg)
}

/// `DATE_TRUNC('year', x)` (PG) / `DATE_FORMAT(x, '%Y-01-01')`
/// (MySQL) / `strftime('%Y-01-01', x)` (SQLite). **PG returns a
/// timestamp, the others text** — cast app-side if you need a type.
#[must_use]
pub fn trunc_year(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::TruncYear, arg)
}

/// `DATE_TRUNC('month', x)` etc. See [`trunc_year`] re: return type.
#[must_use]
pub fn trunc_month(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::TruncMonth, arg)
}

/// `DATE_TRUNC('day', x)` (PG, timestamp) / `DATE(x)` (MySQL, date) /
/// `date(x)` (SQLite, text).
#[must_use]
pub fn trunc_day(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::TruncDay, arg)
}

// ---------- Internal helpers ----------

fn unary(kind: ScalarFn, arg: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind,
        args: vec![arg.into()],
    }
}

fn variadic<I>(kind: ScalarFn, args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    Expr::Function {
        kind,
        args: args.into_iter().collect(),
    }
}

// ---------- pg_trgm functions ----------

/// `SIMILARITY(a, b)` — pg_trgm trigram similarity, a `real` in
/// `[0, 1]`. Annotate with it and order by it for ranked fuzzy search:
///
/// ```ignore
/// use rustango::core::F;
/// use rustango::core::funcs::trigram_similarity;
/// Article::objects()
///     .annotate("rank", trigram_similarity(F("title"), "rusty programming"))
///     .order_by_desc("rank")
/// ```
///
/// Needs `CREATE EXTENSION pg_trgm`. **PG-only.**
#[must_use]
pub fn trigram_similarity(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::TrigramSimilarity,
        args: vec![a.into(), b.into()],
    }
}

/// `WORD_SIMILARITY(a, b)` — the per-word form of
/// [`trigram_similarity`]. Pairs with the `__trigram_word_similar`
/// lookup. Needs `CREATE EXTENSION pg_trgm`. **PG-only.**
#[must_use]
pub fn trigram_word_similarity(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::TrigramWordSimilarity,
        args: vec![a.into(), b.into()],
    }
}

// ---------- Postgres FTS scalar fns ----------

/// `to_tsvector(<expr>)` — build a `tsvector` from a text expression
/// using the database's default text-search config. Pairs with
/// [`plainto_tsquery`] + [`ts_rank`] for ranked search:
///
/// ```ignore
/// use rustango::core::funcs::{to_tsvector, plainto_tsquery, ts_rank};
/// use rustango::core::F;
/// Article::objects()
///     .annotate(
///         "rank",
///         ts_rank(to_tsvector(F("body")), plainto_tsquery("rust orm")),
///     )
///     .order_by_desc("rank")
/// ```
///
/// **PG-only.**
#[must_use]
pub fn to_tsvector(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ToTsVector, arg)
}

/// `plainto_tsquery(<expr>)` — parse a plain user string into a
/// `tsquery`. **PG-only.**
#[must_use]
pub fn plainto_tsquery(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::PlainToTsQuery, arg)
}

/// `ts_rank(<tsvector>, <tsquery>)` — FTS relevance score (`real`).
/// Order by it with [`to_tsvector`] + [`plainto_tsquery`].
/// **PG-only.**
#[must_use]
pub fn ts_rank(vector: impl Into<Expr>, query: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::TsRank,
        args: vec![vector.into(), query.into()],
    }
}

/// `ts_headline(<doc>, <tsquery>)` — FTS snippet with the default
/// `<b>…</b>` highlighting. Use [`ts_headline_with`] for custom
/// markers or fragment counts.
///
/// ```ignore
/// use rustango::core::funcs::{ts_headline, plainto_tsquery};
/// use rustango::core::F;
/// Article::objects()
///     .annotate("snippet", ts_headline(F("body"), plainto_tsquery("rust orm")))
/// ```
///
/// **PG-only**.
#[must_use]
pub fn ts_headline(doc: impl Into<Expr>, query: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::TsHeadline,
        args: vec![doc.into(), query.into()],
    }
}

/// `ts_headline(<doc>, <tsquery>, <options>)` — FTS snippet with
/// custom options. `options` is a key=value string, for example
/// `"StartSel='<mark>', StopSel='</mark>', MaxFragments=1"`.
/// **PG-only.**
#[must_use]
pub fn ts_headline_with(
    doc: impl Into<Expr>,
    query: impl Into<Expr>,
    options: impl Into<Expr>,
) -> Expr {
    Expr::Function {
        kind: ScalarFn::TsHeadline,
        args: vec![doc.into(), query.into(), options.into()],
    }
}

/// `phraseto_tsquery(<expr>)` — builds a `tsquery` that keeps word
/// order: `"rust orm"` → `'rust' <-> 'orm'`. Use it when the exact
/// phrase matters; [`plainto_tsquery`] ignores order. **PG-only.**
#[must_use]
pub fn phraseto_tsquery(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::PhraseToTsQuery, arg)
}

/// `websearch_to_tsquery(<expr>)` — accepts Google-style syntax:
/// quoted `"exact phrase"`, `-exclude`, the literal `OR`. The best
/// fit for a user-facing search box. **PG-only.**
#[must_use]
pub fn websearch_to_tsquery(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::WebsearchToTsQuery, arg)
}

/// `to_tsquery(<expr>)` — the raw `tsquery` operator syntax
/// (`'rust & orm'`, `'rust | python'`, `'rust & !python'`). Lower
/// level than [`plainto_tsquery`]: you must pass valid syntax, so use
/// it for queries the app builds, not raw user input. **PG-only.**
#[must_use]
pub fn to_tsquery(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ToTsQuery, arg)
}

/// `ts_rank_cd(<tsvector>, <tsquery>)` — cover-density ranking. Same
/// shape as [`ts_rank`], but it also weighs how close the matched
/// terms sit. Better for short documents and phrases. **PG-only.**
#[must_use]
pub fn ts_rank_cd(vector: impl Into<Expr>, query: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::TsRankCd,
        args: vec![vector.into(), query.into()],
    }
}

// ---------- Cast, padding, hashes, more math ----------

/// `CAST(<expr> AS <ty>)` — explicit type coercion. The writer maps
/// `ty` to the right SQL type token per dialect via
/// [`crate::sql::Dialect::null_cast`].
///
/// ```ignore
/// use rustango::core::{funcs, FieldType, F};
/// funcs::cast(F("amount"), FieldType::I64);
/// // PG/MySQL/SQLite: CAST("amount" AS BIGINT|BIGINT|BIGINT)
/// ```
#[must_use]
pub fn cast(expr: impl Into<Expr>, ty: super::field_type::FieldType) -> Expr {
    Expr::Cast {
        expr: Box::new(expr.into()),
        ty,
    }
}

/// `LPAD(s, len, fill)` — left-pad `s` to `len` characters with
/// `fill`. SQLite has no such function, so it gets a
/// `substr(printf(...))` fallback.
#[must_use]
pub fn lpad(s: impl Into<Expr>, len: impl Into<Expr>, fill: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::LPad,
        args: vec![s.into(), len.into(), fill.into()],
    }
}

/// `RPAD(s, len, fill)` — right-pad. Mirror of [`lpad`].
#[must_use]
pub fn rpad(s: impl Into<Expr>, len: impl Into<Expr>, fill: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::RPad,
        args: vec![s.into(), len.into(), fill.into()],
    }
}

/// `MD5(s)` → hex string. Built in on PG and MySQL. **SQLite errors**
/// with `OpNotSupportedInDialect`.
#[must_use]
pub fn md5(s: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Md5, s)
}

/// `SHA1(s)` → hex string. PG uses `pgcrypto`'s `digest()`, so it
/// needs `CREATE EXTENSION pgcrypto`. MySQL uses `SHA1()`.
/// **SQLite errors.**
#[must_use]
pub fn sha1(s: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Sha1, s)
}

/// `SHA256(s)` → hex string. PG uses `pgcrypto`'s `digest()`, MySQL
/// `SHA2(s, 256)`. **SQLite errors.**
#[must_use]
pub fn sha256(s: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Sha256, s)
}

/// `POSITION(needle IN hay)` (PG) / `LOCATE(needle, hay)` (MySQL) /
/// `INSTR(hay, needle)` (SQLite). All return the 1-indexed position
/// of the first match, or 0 when there is none. Argument order is
/// `(needle, hay)`; the SQLite writer swaps the two for you.
#[must_use]
pub fn position(needle: impl Into<Expr>, hay: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Position,
        args: vec![needle.into(), hay.into()],
    }
}

/// `REPEAT(s, n)` — repeat `s` `n` times. SQLite has no such
/// function, so it gets a `replace(printf(...))` fallback.
#[must_use]
pub fn repeat(s: impl Into<Expr>, n: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Repeat,
        args: vec![s.into(), n.into()],
    }
}

/// `REVERSE(s)` — reverse a string. PG and MySQL native;
/// **SQLite errors** (no built-in).
#[must_use]
pub fn reverse(s: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Reverse, s)
}

/// `SIGN(x)` → -1, 0, or 1. PG/MySQL native; SQLite emits a CASE
/// expression with equivalent semantics.
#[must_use]
pub fn sign(x: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Sign, x)
}

/// `a % b` — modulo. Every dialect uses `%`, so this lowers to
/// [`Expr::BinOp`] with [`super::expr::BinOp::Mod`] rather than a
/// function call. The function spelling matches Django's
/// `Mod(F('a'), F('b'))`.
#[must_use]
pub fn mod_(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    a.into().binop(super::expr::BinOp::Mod, b)
}

/// `POWER(a, b)` — `a` raised to the `b`th. Native on PG / MySQL.
/// **SQLite errors**: the function needs
/// `SQLITE_ENABLE_MATH_FUNCTIONS`, which sqlx-sqlite does not build
/// with, so the writer fails early instead of at runtime.
#[must_use]
pub fn power(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Power,
        args: vec![a.into(), b.into()],
    }
}

/// `SQRT(x)` — square root. Native on PG / MySQL; SQLite has the
/// same build-flag limit as [`power`].
#[must_use]
pub fn sqrt(x: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Sqrt, x)
}

// ---------- Logs, constants, intervals ----------

/// `LN(x)` — natural log (base e). Native on PG / MySQL. On SQLite it
/// needs 3.35+ built with `SQLITE_ENABLE_MATH_FUNCTIONS`, so the
/// writer errors on default sqlx-sqlite builds.
#[must_use]
pub fn log(x: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Log, x)
}

/// `LOG(base, x)` — log of `x` in base `base`. Same SQLite build-flag
/// limit as [`log`].
#[must_use]
pub fn log_with_base(base: impl Into<Expr>, x: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::LogWithBase,
        args: vec![base.into(), x.into()],
    }
}

/// `EXP(x)` — `e^x`. Native on PG / MySQL; same SQLite build-flag
/// limit as [`log`].
#[must_use]
pub fn exp(x: impl Into<Expr>) -> Expr {
    unary(ScalarFn::Exp, x)
}

/// `PI()` — π as a numeric constant. PG `pi()`, MySQL `PI()`, SQLite
/// emits the literal `3.141592653589793`.
#[must_use]
pub fn pi() -> Expr {
    Expr::Function {
        kind: ScalarFn::Pi,
        args: Vec::new(),
    }
}

/// `RANDOM()` — pseudo-random number. **The range differs**: PG and
/// MySQL return a float in `[0, 1)`; SQLite returns a 64-bit integer
/// in `[-2^63, 2^63)`. Normalize app-side for portable code.
#[must_use]
pub fn random() -> Expr {
    Expr::Function {
        kind: ScalarFn::Random,
        args: Vec::new(),
    }
}

/// `MAKE_INTERVAL(years, months, days, hours, minutes, seconds)` —
/// **PG-only**; MySQL and SQLite have no `interval` type and emit
/// `OpNotSupportedInDialect`. Pass zeros for unused fields.
#[must_use]
pub fn make_interval(
    years: impl Into<Expr>,
    months: impl Into<Expr>,
    days: impl Into<Expr>,
    hours: impl Into<Expr>,
    minutes: impl Into<Expr>,
    seconds: impl Into<Expr>,
) -> Expr {
    Expr::Function {
        kind: ScalarFn::MakeInterval,
        args: vec![
            years.into(),
            months.into(),
            days.into(),
            hours.into(),
            minutes.into(),
            seconds.into(),
        ],
    }
}

/// `AGE(ts1, ts2)` — time between two timestamps. **The return type
/// differs**: PG gives an `interval`, MySQL numeric seconds, SQLite a
/// `REAL` count of seconds. Use it on one backend, or wrap the PG
/// call in `EXTRACT(EPOCH FROM …)` to get seconds everywhere.
#[must_use]
pub fn age(ts1: impl Into<Expr>, ts2: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::Age,
        args: vec![ts1.into(), ts2.into()],
    }
}

/// `date_trunc(unit, ts AT TIME ZONE tz)` (PG) and the equivalents on
/// MySQL / SQLite. `unit` is one of `"year" | "month" | "day" |
/// "hour" | "minute" | "second"`. `tz` is an IANA name on PG / MySQL
/// (`"America/New_York"`) or a `±HH:MM` offset on SQLite. Any other
/// unit emits `OpNotSupportedInDialect`.
///
/// PG returns a timestamp, MySQL / SQLite return text. Cast app-side
/// if you need a typed value.
#[must_use]
pub fn trunc_with_tz(ts: impl Into<Expr>, unit: &'static str, tz: &'static str) -> Expr {
    use super::SqlValue;
    Expr::Function {
        kind: ScalarFn::TruncWithTz,
        args: vec![
            ts.into(),
            Expr::Literal(SqlValue::String(unit.to_owned())),
            Expr::Literal(SqlValue::String(tz.to_owned())),
        ],
    }
}

// ---------- JSON path extraction ----------

/// `<source> -> 'k1' -> 'k2' ->> 'k3'` (PG) /
/// `JSON_UNQUOTE(JSON_EXTRACT(...))` (MySQL) /
/// `json_extract(...)` (SQLite).
///
/// Builds an [`Expr::JsonPath`] over a JSON column or expression.
/// Each `&str` in `keys` is an object-key step (`$.<key>`).
/// `as_text = true` unwraps to a scalar (PG's `->>`); `false` keeps
/// the JSON-typed form.
///
/// ```ignore
/// use rustango::core::F;
/// use rustango::core::funcs::json_path;
///
/// // data->'address'->>'city' (PG) /
/// // JSON_UNQUOTE(JSON_EXTRACT(data, '$.address.city')) (MySQL) /
/// // json_extract(data, '$.address.city') (SQLite)
/// json_path(F("data"), &["address", "city"], true);
/// ```
///
/// Use [`json_path_indexed`] when array indices are needed.
#[must_use]
pub fn json_path(source: impl Into<Expr>, keys: &[&str], as_text: bool) -> Expr {
    use super::JsonPathStep;
    Expr::JsonPath {
        source: Box::new(source.into()),
        path: keys
            .iter()
            .map(|k| JsonPathStep::Key((*k).to_owned()))
            .collect(),
        as_text,
    }
}

/// `JSON_ARRAY_LENGTH(x)` — number of elements in a JSON array.
/// Emits `jsonb_array_length` on PG, `JSON_LENGTH` on MySQL,
/// `json_array_length` on SQLite.
///
/// ```ignore
/// use rustango::core::funcs::{json_array_length, json_path};
/// use rustango::core::SqlValue;
///
/// // SELECT … WHERE jsonb_array_length(data -> 'tags') > 0
/// QuerySet::<Post>::default()
///     .where_raw(WhereExpr::ExprCompare {
///         lhs: json_array_length(json_path(F("data"), &["tags"], false)),
///         op: Op::Gt,
///         rhs: Expr::Literal(SqlValue::I64(0)),
///     })
/// ```
///
/// Non-array input behaves differently on each backend: PG errors,
/// MySQL returns 1, SQLite (3.38+) returns 0. If your data is mixed,
/// wrap the PG call in `COALESCE(jsonb_array_length(x), 0)`.
#[must_use]
pub fn json_array_length(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::JsonArrayLength, arg)
}

/// Like [`json_path`], but the steps may mix keys and array indices:
/// `[JsonPathStep::Key("items"), JsonPathStep::Index(0),
/// JsonPathStep::Key("name")]` reads `data.items[0].name`.
#[must_use]
pub fn json_path_indexed(
    source: impl Into<Expr>,
    steps: impl IntoIterator<Item = super::JsonPathStep>,
    as_text: bool,
) -> Expr {
    Expr::JsonPath {
        source: Box::new(source.into()),
        path: steps.into_iter().collect(),
        as_text,
    }
}

// ---- PostGIS spatial functions -------
//
// All PG/PostGIS-only — MySQL / SQLite emit `OpNotSupportedInDialect`.
// Pass a column as `F("col")` and a literal point as a bare
// `crate::sql::Point`, which is `Into<Expr>`.

/// `ST_Distance(a, b)` — distance between two geometries in SRID
/// units, as `double precision`. Order by it for nearest-neighbour
/// queries, or use [`crate::query::QuerySet::order_by_distance_to`].
#[must_use]
pub fn st_distance(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::StDistance,
        args: vec![a.into(), b.into()],
    }
}

/// `ST_DWithin(a, b, distance)` — true when `a` is within `distance`
/// (SRID units) of `b`. Use it in `.where_raw(...)`, or use the
/// [`crate::query::QuerySet::filter_dwithin`] shortcut.
#[must_use]
pub fn st_dwithin(a: impl Into<Expr>, b: impl Into<Expr>, distance: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::StDWithin,
        args: vec![a.into(), b.into(), distance.into()],
    }
}

/// `ST_Contains(a, b)` — boolean "geometry `a` completely contains `b`".
#[must_use]
pub fn st_contains(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::StContains,
        args: vec![a.into(), b.into()],
    }
}

/// `ST_Within(a, b)` — boolean "geometry `a` is completely inside `b`".
#[must_use]
pub fn st_within(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::StWithin,
        args: vec![a.into(), b.into()],
    }
}

/// `ST_Intersects(a, b)` — boolean "the geometries share any point".
#[must_use]
pub fn st_intersects(a: impl Into<Expr>, b: impl Into<Expr>) -> Expr {
    Expr::Function {
        kind: ScalarFn::StIntersects,
        args: vec![a.into(), b.into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{SqlValue, F};

    #[test]
    fn unary_builds_function_with_one_arg() {
        let e = lower(F("name"));
        let Expr::Function { kind, args } = e else {
            panic!("expected Function variant")
        };
        assert_eq!(kind, ScalarFn::Lower);
        assert_eq!(args, vec![Expr::Column("name")]);
    }

    #[test]
    fn variadic_collects_iter() {
        let e = concat([F("a").into(), " ".into(), F("b").into()]);
        let Expr::Function { kind, args } = e else {
            panic!()
        };
        assert_eq!(kind, ScalarFn::Concat);
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], Expr::Column("a"));
        assert_eq!(args[1], Expr::Literal(SqlValue::String(" ".into())));
        assert_eq!(args[2], Expr::Column("b"));
    }

    #[test]
    fn coalesce_variadic_takes_vec() {
        let args: Vec<Expr> = vec![F("a").into(), F("b").into(), 0_i32.into()];
        let e = coalesce(args);
        let Expr::Function { kind, args } = e else {
            panic!()
        };
        assert_eq!(kind, ScalarFn::Coalesce);
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn substr_is_3ary() {
        let e = substr(F("title"), 1_i64, 10_i64);
        let Expr::Function { kind, args } = e else {
            panic!()
        };
        assert_eq!(kind, ScalarFn::Substr);
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn round_one_arg_vs_two() {
        let e = round(F("score"));
        let Expr::Function { args, .. } = e else {
            panic!()
        };
        assert_eq!(args.len(), 1);

        let e = round_to(F("score"), 2_i32);
        let Expr::Function { args, .. } = e else {
            panic!()
        };
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn functions_compose_via_into_expr() {
        // upper(concat([first, " ", last]))
        let e = upper(concat([F("first").into(), " ".into(), F("last").into()]));
        let Expr::Function {
            kind: outer_kind,
            args: outer_args,
        } = e
        else {
            panic!()
        };
        assert_eq!(outer_kind, ScalarFn::Upper);
        assert_eq!(outer_args.len(), 1);
        let Expr::Function {
            kind: inner_kind, ..
        } = &outer_args[0]
        else {
            panic!("inner should be a Function")
        };
        assert_eq!(*inner_kind, ScalarFn::Concat);
    }
}
