//! Scalar database functions — text, math, comparison, date/time.
//!
//! Closes ORM Expression-DSL issues #2 (text/math/comparison) and #3
//! (date/time). Builds on the [`crate::core::Expr`] tree introduced
//! in #1; each function here returns an [`Expr::Function`] that
//! composes freely with `F()`, arithmetic, other functions, and
//! literal values.
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
//! ### Text / math / comparison (#2)
//!
//! - **`concat`** falls back to `||` on SQLite (portable on every
//!   SQLite version; SQLite added `concat()` only in 3.44).
//! - **`greatest` / `least`** emit SQLite's scalar `MAX(a, b, …)` /
//!   `MIN(a, b, …)` forms — those are the scalar versions when given
//!   2+ args, distinct from the aggregate `MAX(col)` form. The single-
//!   arg case errors on SQLite (would collide with the aggregate).
//! - **`length`** is **char-count** on PG, **byte-count** on MySQL,
//!   **char-count for `TEXT`** on SQLite. For ASCII this matches; for
//!   unicode-heavy data prefer `CHAR_LENGTH` on MySQL (not yet
//!   exposed; file follow-up if needed).
//! - **`round(x, n)`**: PG `ROUND(numeric, int)` doesn't accept float
//!   without a cast; MySQL / SQLite cast implicitly. Pass an integer
//!   column or wrap in a cast on PG when precision matters.
//!
//! ### Date / time (#3)
//!
//! - **`now()`** emits `NOW()` on PG / MySQL, `CURRENT_TIMESTAMP` on
//!   SQLite (no parens — `NOW()` isn't a SQLite keyword).
//! - **`extract_*`** return-type is normalized to integer everywhere
//!   (PG via `CAST(... AS INTEGER)`, MySQL native, SQLite via
//!   `CAST(strftime(...) AS INTEGER)`).
//! - **`extract_weekday` is normalized to 0 = Sunday, 6 = Saturday**.
//!   MySQL's native `DAYOFWEEK()` returns 1=Sunday; the writer
//!   subtracts 1 to align with PG's `EXTRACT(DOW)`. SQLite's
//!   `strftime('%w')` already matches.
//! - **`extract_quarter` errors on SQLite** — no native quarter
//!   token; emitter returns `OpNotSupportedInDialect`.
//! - **⚠ `extract_week` is NOT cross-dialect**. Each backend uses a
//!   different week-numbering convention (PG: ISO 8601; MySQL:
//!   Sunday-start range 0–53; SQLite: Monday-start range 00–53). For
//!   the same date the value differs. Single-backend deployments can
//!   use it; cross-dialect code should compute the week boundary as
//!   a typed `chrono::DateTime` in Rust and filter on the timestamp
//!   column instead.
//! - **`trunc_year / trunc_month` return-type diverges**: timestamp
//!   on PG (`DATE_TRUNC('unit', x)`), text on MySQL/SQLite
//!   (`DATE_FORMAT(x, '...')` / `strftime('...', x)`). Cast on the
//!   app side when reading if you need a typed
//!   `chrono::NaiveDate`. `trunc_date` is the one trunc-family
//!   builder with identical SQL across every dialect.
//!
//! ## Composition with `F()` + arithmetic
//!
//! Every builder takes `impl Into<Expr>` for its argument(s), so
//! [`F`], primitives, [`SqlValue`], and any other [`Expr`] (including
//! the result of another builder call) pass directly:
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

/// `CEIL(arg)` — ceiling. Emits `CEIL` (PG/SQLite) / `CEILING` (MySQL
/// alias of CEIL) — the writer picks the dialect-correct token.
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

/// `SUBSTRING(s, start, length)` — 1-indexed. PG emits the
/// `FROM…FOR…` form, MySQL/SQLite the comma form. Both produce
/// identical results.
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
/// Takes `IntoIterator<Item = Expr>`. Array literals work as long as
/// every element is already an [`Expr`] — call `.into()` on each
/// non-Expr argument:
///
/// ```ignore
/// concat([F("first").into(), " ".into(), F("last").into()])
/// ```
///
/// (Rust arrays are homogeneous, so a heterogeneous mix of `F` and
/// `&str` won't infer to a common type. The `.into()` per element is
/// the price of variadic-arity at the type level.)
#[must_use]
pub fn concat<I>(args: I) -> Expr
where
    I: IntoIterator<Item = Expr>,
{
    variadic(ScalarFn::Concat, args)
}

/// `COALESCE(a, b, c, …)` — first non-NULL argument.
/// See [`concat`] re: passing args as already-lifted `Expr`.
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

// ---------- Date / time functions (issue #3) ----------

/// `NOW()` — server-side wall-clock timestamp. SQLite emits
/// `CURRENT_TIMESTAMP` (no parens). 0-arg.
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
/// **⚠ NOT portable.** Each backend uses a different week-numbering
/// convention; for the same date the value differs:
/// - PG: ISO 8601, weeks start Monday, range 1–53.
/// - MySQL (default mode 0): weeks start **Sunday**, range **0**–53.
/// - SQLite (`strftime('%W')`): weeks start Monday, first
///   Monday-of-year is week 01.
///
/// For 2024-01-01 (Monday): PG=1, MySQL=0, SQLite=01.
///
/// Single-backend deployments can use this freely. Cross-dialect code
/// should compute a typed week-start `chrono::DateTime` in Rust and
/// filter with `Column::gte()` against the timestamp, or denormalize
/// the week into an integer column with semantics under your control.
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

/// `EXTRACT(QUARTER FROM x)` — quarter (1–4) as integer.
/// **Not supported on SQLite** (no native `strftime` token); emits
/// `OpNotSupportedInDialect`.
#[must_use]
pub fn extract_quarter(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::ExtractQuarter, arg)
}

/// `DATE(x)` — strip the time component from a timestamp. Same shape
/// on all three backends; returns `DATE`.
#[must_use]
pub fn trunc_date(arg: impl Into<Expr>) -> Expr {
    unary(ScalarFn::TruncDate, arg)
}

/// `DATE_TRUNC('year', x)` (PG) / `DATE_FORMAT(x, '%Y-01-01')` (MySQL)
/// / `strftime('%Y-01-01', x)` (SQLite). **Returns timestamp on PG,
/// text on MySQL/SQLite** — cast app-side if a typed value is needed.
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
