//! Scalar database functions — text, math, comparison.
//!
//! Closes ORM Expression-DSL issue #2. Builds on the [`crate::core::Expr`]
//! tree #1 introduced; each function here returns an [`Expr::Function`]
//! that composes freely with `F()`, arithmetic, other functions, and
//! literal values.
//!
//! ```ignore
//! use rustango::core::{F, funcs::{lower, concat, coalesce, greatest, abs, round}};
//!
//! // Normalize a name on the way in.
//! .set_expr("name_norm", lower(F("name")))
//!
//! // Build a display string from two columns + a separator.
//! // `.into()` on each element — array literals are homogeneous.
//! .set_expr("display", concat([F("first").into(), " ".into(), F("last").into()]))
//!
//! // Pick the first non-NULL.
//! .set_expr("nickname", coalesce([F("nickname").into(), F("username").into(), "anon".into()]))
//!
//! // Compose with arithmetic — `round_to` for the precision form.
//! .set_expr("rounded", round_to(F("score") * 100_i64, 0_i32))
//!
//! // Math.
//! .where_(Post::priority.eq_expr(greatest([F("a").into(), F("b").into(), 5_i64.into()])))
//! ```
//!
//! ## Per-dialect notes
//!
//! - **`concat`** falls back to `||` on SQLite (portable on every
//!   SQLite version; SQLite added `concat()` only in 3.44).
//! - **`greatest` / `least`** emit SQLite's scalar `MAX(a, b, …)` /
//!   `MIN(a, b, …)` forms — those are the scalar versions when given
//!   multiple args, distinct from the aggregate `MAX(col)` form.
//! - **`length`** is **char-count** on PG, **byte-count** on MySQL,
//!   **char-count for `TEXT`** on SQLite. For app code that mixes
//!   ASCII this matches; for unicode-heavy data prefer `CHAR_LENGTH`
//!   on MySQL (not yet exposed here — file follow-up if needed).
//! - **`round(x, n)`**: PG `ROUND(numeric, int)` doesn't accept float
//!   without a cast; MySQL / SQLite cast implicitly. Pass an integer
//!   column or wrap in a cast on PG when precision matters.
//!
//! ## Composition with `F()` + arithmetic
//!
//! Every builder takes `impl Into<Expr>`, so [`F`], primitives, and
//! [`SqlValue`] all pass directly:
//!
//! ```ignore
//! upper(concat([F("first"), " ".into(), F("last")]))
//! //    └─── concat returns Expr ──┘
//! ```
//!
//! [`F`]: crate::core::F
//! [`SqlValue`]: crate::core::SqlValue
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
