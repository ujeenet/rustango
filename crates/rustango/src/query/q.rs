//! `Q()` — a composable boolean predicate, Django style.
//!
//! It wraps the dialect-neutral [`WhereExpr`] tree in a builder with
//! operator overloads. Django writes:
//!
//! ```python
//! qs.filter(Q(name__startswith='A') | (Q(age__gt=18) & ~Q(banned=True)))
//! ```
//!
//! The Rust shape:
//!
//! ```ignore
//! use rustango::query::Q;
//!
//! User::objects()
//!     .where_(
//!         Q::startswith("name", "A")
//!             | (Q::gt("age", 18_i64) & !Q::eq("banned", true))
//!     )
//!     .fetch(&pool).await?;
//! ```
//!
//! ## `Q()` vs the `Q!()` macro vs typed methods
//!
//! * `Q!(User.name = "alice")` — safest. The field name resolves at
//!   parse time. Use it when the name is known at compile time.
//! * `User::name.eq("alice")` — same safety, more typing.
//! * `Q::eq("name", "alice")` — built at runtime, so you can assemble
//!   filter trees from admin chips or query params. The field name is
//!   checked when the queryset compiles, so a typo errors there.
//!
//! Every `Q` lowers to [`WhereExpr`], which the per-dialect writers
//! already handle, so all three backends work.

use std::ops::{BitAnd, BitOr, BitXor, Not};

use crate::core::{Filter, Op, SqlValue, WhereExpr};

/// Composable boolean predicate. Build one with a lookup constructor
/// (`Q::eq`, `Q::ilike`, `Q::in_`, …), then combine with `&`, `|`, `^`
/// and `!`, or with the `.and()` / `.or()` / `.xor()` / `.negate()`
/// methods.
///
/// It lowers to [`WhereExpr`] via `Into`, so both `.where_(q)` and
/// `.where_raw(q.into())` work.
#[derive(Debug, Clone)]
pub struct Q(WhereExpr);

impl Q {
    /// Wrap any [`WhereExpr`] in a `Q`. Escape hatch for predicates the
    /// constructors do not cover, such as `WhereExpr::ColumnCompare`
    /// for `F()` comparisons.
    #[must_use]
    pub fn raw(expr: WhereExpr) -> Self {
        Self(expr)
    }

    /// Internal constructor for the standard `<column> <op> <value>`
    /// predicate shape.
    fn predicate(column: &'static str, op: Op, value: SqlValue) -> Self {
        Self(WhereExpr::Predicate(Filter { column, op, value }))
    }

    /// `column = value`.
    #[must_use]
    pub fn eq(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Eq, value.into())
    }

    /// `column <> value`.
    #[must_use]
    pub fn ne(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Ne, value.into())
    }

    /// `column > value`.
    #[must_use]
    pub fn gt(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Gt, value.into())
    }

    /// `column >= value`.
    #[must_use]
    pub fn gte(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Gte, value.into())
    }

    /// `column < value`.
    #[must_use]
    pub fn lt(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Lt, value.into())
    }

    /// `column <= value`.
    #[must_use]
    pub fn lte(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Lte, value.into())
    }

    /// `column LIKE value`, case-sensitive. The value is bound as-is,
    /// with no wildcards added. Pass `"%alice%"` yourself, or use
    /// [`Self::contains`].
    #[must_use]
    pub fn like(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::Like, value.into())
    }

    /// `column ILIKE value`, case-insensitive. Same wildcard rule as
    /// [`Self::like`].
    #[must_use]
    pub fn ilike(column: &'static str, value: impl Into<SqlValue>) -> Self {
        Self::predicate(column, Op::ILike, value.into())
    }

    /// Build one escaped-LIKE predicate. The value goes through
    /// [`crate::core::escape_like`] and gets the matching `*Escaped`
    /// op, so any `%` or `_` in it matches itself on every dialect.
    /// The `prefix` / `suffix` wildcards are added after escaping, so
    /// they still act as wildcards. Escaping and op stay together here
    /// so the callers below cannot pair them wrongly.
    fn wrap_escaped(
        column: &'static str,
        prefix: &str,
        suffix: &str,
        value: impl AsRef<str>,
        case_insensitive: bool,
    ) -> Self {
        let escaped = crate::core::escape_like(value.as_ref());
        let op = if case_insensitive {
            Op::ILikeEscaped
        } else {
            Op::LikeEscaped
        };
        Self::predicate(
            column,
            op,
            SqlValue::String(format!("{prefix}{escaped}{suffix}")),
        )
    }

    /// Django `__contains`. The value is a literal substring: `%` and
    /// `_` in it match themselves. Use [`Q::like`] for a raw pattern.
    #[must_use]
    pub fn contains(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "%", "%", value, false)
    }

    /// Django `__icontains`. Case-insensitive literal substring match.
    #[must_use]
    pub fn icontains(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "%", "%", value, true)
    }

    /// Django `__startswith`. Literal prefix match.
    #[must_use]
    pub fn startswith(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "", "%", value, false)
    }

    /// Django `__istartswith`. Case-insensitive literal prefix match.
    #[must_use]
    pub fn istartswith(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "", "%", value, true)
    }

    /// Django `__endswith`. Literal suffix match.
    #[must_use]
    pub fn endswith(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "%", "", value, false)
    }

    /// Django `__iendswith`. Case-insensitive literal suffix match.
    #[must_use]
    pub fn iendswith(column: &'static str, value: impl AsRef<str>) -> Self {
        Self::wrap_escaped(column, "%", "", value, true)
    }

    /// `column IN (v1, v2, …)`. An empty iterator is accepted here and
    /// rejected later by the writer. Named `in_` because `in` is a
    /// Rust keyword.
    #[must_use]
    pub fn in_<V, I>(column: &'static str, values: I) -> Self
    where
        V: Into<SqlValue>,
        I: IntoIterator<Item = V>,
    {
        let list: Vec<SqlValue> = values.into_iter().map(Into::into).collect();
        Self::predicate(column, Op::In, SqlValue::List(list))
    }

    /// `column NOT IN (v1, v2, …)`.
    #[must_use]
    pub fn not_in<V, I>(column: &'static str, values: I) -> Self
    where
        V: Into<SqlValue>,
        I: IntoIterator<Item = V>,
    {
        let list: Vec<SqlValue> = values.into_iter().map(Into::into).collect();
        Self::predicate(column, Op::NotIn, SqlValue::List(list))
    }

    /// `column IS NULL`.
    #[must_use]
    pub fn is_null(column: &'static str) -> Self {
        Self::predicate(column, Op::IsNull, SqlValue::Bool(true))
    }

    /// `column IS NOT NULL`.
    #[must_use]
    pub fn is_not_null(column: &'static str) -> Self {
        Self::predicate(column, Op::IsNull, SqlValue::Bool(false))
    }

    /// `column BETWEEN lo AND hi`. Both bounds inclusive.
    #[must_use]
    pub fn between<V: Into<SqlValue>>(column: &'static str, lo: V, hi: V) -> Self {
        Self::predicate(
            column,
            Op::Between,
            SqlValue::List(vec![lo.into(), hi.into()]),
        )
    }

    /// Compose with `AND`. Method-style alias for the `&` operator.
    #[must_use]
    pub fn and(self, rhs: Self) -> Self {
        self & rhs
    }

    /// Compose with `OR`. Method-style alias for the `|` operator.
    #[must_use]
    pub fn or(self, rhs: Self) -> Self {
        self | rhs
    }

    /// Compose with `XOR`. Method-style alias for `^`. PG and MySQL
    /// emit a native XOR; SQLite rejects it at compile time.
    #[must_use]
    pub fn xor(self, rhs: Self) -> Self {
        self ^ rhs
    }

    /// Negate. Method-style alias for the unary `!` operator.
    #[must_use]
    pub fn negate(self) -> Self {
        !self
    }

    /// Unwrap to the dialect-neutral [`WhereExpr`].
    #[must_use]
    pub fn into_where_expr(self) -> WhereExpr {
        self.0
    }
}

impl BitAnd for Q {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        // Flatten adjacent `And` nodes, as `TypedExpr::and` does.
        let inner = match (self.0, rhs.0) {
            (WhereExpr::And(mut a), WhereExpr::And(b)) => {
                a.extend(b);
                WhereExpr::And(a)
            }
            (WhereExpr::And(mut a), b) => {
                a.push(b);
                WhereExpr::And(a)
            }
            (a, WhereExpr::And(mut b)) => {
                let mut v = vec![a];
                v.append(&mut b);
                WhereExpr::And(v)
            }
            (a, b) => WhereExpr::And(vec![a, b]),
        };
        Q(inner)
    }
}

impl BitOr for Q {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        let inner = match (self.0, rhs.0) {
            (WhereExpr::Or(mut a), WhereExpr::Or(b)) => {
                a.extend(b);
                WhereExpr::Or(a)
            }
            (WhereExpr::Or(mut a), b) => {
                a.push(b);
                WhereExpr::Or(a)
            }
            (a, WhereExpr::Or(mut b)) => {
                let mut v = vec![a];
                v.append(&mut b);
                WhereExpr::Or(v)
            }
            (a, b) => WhereExpr::Or(vec![a, b]),
        };
        Q(inner)
    }
}

impl BitXor for Q {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self {
        Q(WhereExpr::Xor(vec![self.0, rhs.0]))
    }
}

impl Not for Q {
    type Output = Self;
    fn not(self) -> Self {
        // Double-negate elision keeps the tree shallow.
        if let WhereExpr::Not(inner) = self.0 {
            Q(*inner)
        } else {
            Q(WhereExpr::Not(Box::new(self.0)))
        }
    }
}

impl From<Q> for WhereExpr {
    fn from(q: Q) -> Self {
        q.0
    }
}

impl<M: crate::core::Model> From<Q> for crate::core::TypedExpr<M> {
    fn from(q: Q) -> Self {
        crate::core::TypedExpr::from_where_expr(q.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_predicate_lowers_to_filter() {
        let q = Q::eq("name", "alice");
        let we: WhereExpr = q.into();
        match we {
            WhereExpr::Predicate(f) => {
                assert_eq!(f.column, "name");
                assert_eq!(f.op, Op::Eq);
            }
            other => panic!("expected Predicate, got {other:?}"),
        }
    }

    #[test]
    fn bitand_flattens_left_and_node() {
        let q = Q::eq("a", 1_i64) & Q::eq("b", 2_i64) & Q::eq("c", 3_i64);
        let we: WhereExpr = q.into();
        let WhereExpr::And(items) = we else {
            panic!("expected flat And");
        };
        assert_eq!(items.len(), 3, "AND should flatten: {items:?}");
    }

    #[test]
    fn bitor_flattens_left_or_node() {
        let q = Q::eq("a", 1_i64) | Q::eq("b", 2_i64) | Q::eq("c", 3_i64);
        let we: WhereExpr = q.into();
        let WhereExpr::Or(items) = we else {
            panic!("expected flat Or");
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn double_not_elides() {
        let q = !!Q::eq("a", 1_i64);
        let we: WhereExpr = q.into();
        // After elision the outer `!!` should be a bare predicate.
        assert!(
            matches!(we, WhereExpr::Predicate(_)),
            "double-NOT should elide, got {we:?}"
        );
    }

    #[test]
    fn contains_wraps_with_percent() {
        let q = Q::contains("email", "alice");
        let we: WhereExpr = q.into();
        let WhereExpr::Predicate(f) = we else {
            panic!()
        };
        // `contains` escapes the value and uses the escaped op.
        assert_eq!(f.op, Op::LikeEscaped);
        assert_eq!(f.value, SqlValue::String("%alice%".into()));
    }

    #[test]
    fn is_null_routes_via_op_is_null() {
        let q = Q::is_null("deleted_at");
        let we: WhereExpr = q.into();
        let WhereExpr::Predicate(f) = we else {
            panic!()
        };
        assert_eq!(f.op, Op::IsNull);
        assert_eq!(f.value, SqlValue::Bool(true));
    }
}
