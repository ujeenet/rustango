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
//!     .fetch_pool(&pool).await?;
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
}

/// RHS expression — literal, column reference, or arithmetic tree.
///
/// `Expr` is what the writer renders to the right side of `=` in an
/// UPDATE assignment and to the right side of a column predicate in
/// a WHERE clause. The variants are recursive so arbitrarily nested
/// arithmetic is expressible.
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
