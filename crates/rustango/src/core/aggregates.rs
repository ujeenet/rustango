//! Aggregate builders with an optional `FILTER` clause and default.
//!
//! Gives the Django shapes `Count("id", filter=Q(...))` and
//! `Sum("price", default=0)`, plus the `StdDev` and `Variance`
//! families. On MySQL the `FILTER (WHERE …)` clause is rewritten as
//! `CASE WHEN`.
//!
//! ```ignore
//! use rustango::core::aggregates::{count, sum, avg};
//! use rustango::core::Column as _;
//!
//! Post::objects()
//!     .aggregate("posts_total", count("id"))
//!     .aggregate("active_posts", count("id").filter(Post::active.eq(true)))
//!     .aggregate(
//!         "revenue_or_zero",
//!         sum("price")
//!             .filter(Post::status.eq("published"))
//!             .default(0_i64),
//!     )
//!     .compute(&pool).await?;
//! ```
//!
//! ## Wrap order
//!
//! `Filtered` always sits inside `Coalesced`, so the SQL is
//! `COALESCE(SUM(col) FILTER (WHERE pred), default)`. Chain order does
//! not matter; `.build()` lowers both fields.
//!
//! ## Dialect support
//!
//! | Aggregate / wrapper | PG | MySQL | SQLite |
//! |---|---|---|---|
//! | `Count` / `Sum` / `Avg` / `Max` / `Min` | ✓ | ✓ | ✓ |
//! | `CountDistinct` | ✓ | ✓ | ✓ (3.35+) |
//! | `StdDev` / `StdDevPop` | ✓ | ✓ (8.0+) | ✗ |
//! | `Variance` / `VariancePop` | ✓ | ✓ (8.0+) | ✗ |
//! | `Filtered { … }` (`FILTER (WHERE …)`) | ✓ native | ✓ via `CASE WHEN` | ✓ native (3.30+) |
//! | `Coalesced { …, default }` (`COALESCE`) | ✓ | ✓ | ✓ |

use super::query::{AggregateExpr, WhereExpr};
use super::SqlValue;

/// Builder for an [`AggregateExpr`]. Start from a free function in
/// this module ([`count`], [`sum`], …) and pass the result to anything
/// taking `impl Into<AggregateExpr>`, such as `QuerySet::aggregate`.
#[must_use]
pub struct AggregateBuilder {
    kind: AggregateExpr,
    filter: Option<WhereExpr>,
    default: Option<SqlValue>,
}

impl AggregateBuilder {
    fn new(kind: AggregateExpr) -> Self {
        Self {
            kind,
            filter: None,
            default: None,
        }
    }

    /// Count only the rows matching `predicate`. PG and SQLite 3.30+
    /// get `FILTER (WHERE …)`; MySQL gets
    /// `<agg>(CASE WHEN … THEN <arg> END)`. The predicate takes the
    /// same shape as a `where_()` clause.
    ///
    /// ```ignore
    /// count("id").filter(Post::status.eq("published").and(Post::pages.gt(100)))
    /// ```
    pub fn filter(mut self, predicate: impl Into<WhereExpr>) -> Self {
        self.filter = Some(predicate.into());
        self
    }

    /// Fall back to `value` when the queryset matches no rows and the
    /// aggregate would be `NULL`. Emits `COALESCE`.
    ///
    /// ```ignore
    /// sum("price").default(0_i64)
    /// // → COALESCE(SUM("price"), 0)
    /// ```
    pub fn default(mut self, value: impl Into<SqlValue>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Finalize to an [`AggregateExpr`]. Same as `Into<AggregateExpr>`,
    /// but a method helps when type inference needs it. With both set,
    /// `Filtered` goes inside `Coalesced`.
    #[must_use]
    pub fn build(self) -> AggregateExpr {
        let mut out = self.kind;
        if let Some(f) = self.filter {
            out = AggregateExpr::Filtered {
                inner: Box::new(out),
                filter: f,
            };
        }
        if let Some(d) = self.default {
            out = AggregateExpr::Coalesced {
                inner: Box::new(out),
                default: d,
            };
        }
        out
    }
}

impl From<AggregateBuilder> for AggregateExpr {
    fn from(b: AggregateBuilder) -> Self {
        b.build()
    }
}

/// `COUNT(column)` — counts non-NULL values in `column`. For
/// `COUNT(*)`, use [`count_all`].
#[must_use]
pub fn count(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Count(Some(column)))
}

/// `COUNT(*)` — counts every row regardless of NULL.
#[must_use]
pub fn count_all() -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Count(None))
}

/// `COUNT(DISTINCT column)` — counts distinct non-NULL values.
#[must_use]
pub fn count_distinct(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::CountDistinct(column))
}

/// `SUM(column)`. Combined with `.default(0)`, produces the
/// "treat empty-result as zero" Django shape.
#[must_use]
pub fn sum(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Sum(column))
}

/// `AVG(column)`.
#[must_use]
pub fn avg(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Avg(column))
}

/// `MAX(column)`.
#[must_use]
pub fn max(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Max(column))
}

/// `MIN(column)`.
#[must_use]
pub fn min(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Min(column))
}

/// `ANY_VALUE(column)` — any one value from the group. Use it to
/// select a column that is not in GROUP BY. Emits `any_value()` on PG
/// 16+, `ANY_VALUE()` on MySQL and `min()` on SQLite. PG below 16 has
/// no `any_value()`, so the server errors at run time.
#[must_use]
pub fn any_value(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::AnyValue(column))
}

/// `STDDEV_SAMP(column)` — sample standard deviation. Works on PG and
/// MySQL 8+. SQLite has no stddev, so the writer returns
/// `SqlError::AggregateNotSupported`.
#[must_use]
pub fn stddev(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::StdDev(column))
}

/// `STDDEV_POP(column)` — population standard deviation. Same dialect
/// support as [`stddev`].
#[must_use]
pub fn stddev_pop(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::StdDevPop(column))
}

/// `VAR_SAMP(column)` — sample variance. Same dialect support as
/// [`stddev`].
#[must_use]
pub fn variance(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::Variance(column))
}

/// `VAR_POP(column)` — population variance. Same dialect support as
/// [`stddev`].
#[must_use]
pub fn variance_pop(column: &'static str) -> AggregateBuilder {
    AggregateBuilder::new(AggregateExpr::VariancePop(column))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Filter, Op};

    fn predicate(col: &'static str) -> WhereExpr {
        WhereExpr::Predicate(Filter {
            column: col,
            op: Op::Eq,
            value: SqlValue::Bool(true),
        })
    }

    #[test]
    fn bare_count_lowers_to_count_variant() {
        let e: AggregateExpr = count("id").into();
        assert!(matches!(e, AggregateExpr::Count(Some("id"))));
    }

    #[test]
    fn count_all_emits_count_none() {
        let e: AggregateExpr = count_all().into();
        assert!(matches!(e, AggregateExpr::Count(None)));
    }

    #[test]
    fn filter_wraps_in_filtered_variant() {
        let e: AggregateExpr = count("id").filter(predicate("active")).into();
        assert!(matches!(e, AggregateExpr::Filtered { .. }));
    }

    #[test]
    fn default_wraps_in_coalesced_variant() {
        let e: AggregateExpr = sum("price").default(0_i64).into();
        assert!(matches!(e, AggregateExpr::Coalesced { .. }));
    }

    #[test]
    fn filter_then_default_wraps_coalesced_outside_filtered() {
        let e: AggregateExpr = sum("price")
            .filter(predicate("active"))
            .default(0_i64)
            .into();
        match e {
            AggregateExpr::Coalesced { inner, .. } => match *inner {
                AggregateExpr::Filtered { inner, .. } => {
                    assert!(matches!(*inner, AggregateExpr::Sum("price")));
                }
                _ => panic!("expected Filtered inside Coalesced"),
            },
            _ => panic!("expected Coalesced at the top"),
        }
    }

    #[test]
    fn default_then_filter_still_wraps_coalesced_outside_filtered() {
        // Chain order does not matter; `.build()` lowers both fields.
        let e: AggregateExpr = sum("price")
            .default(0_i64)
            .filter(predicate("active"))
            .into();
        match e {
            AggregateExpr::Coalesced { inner, .. } => match *inner {
                AggregateExpr::Filtered { .. } => {}
                _ => panic!("expected Filtered inside Coalesced"),
            },
            _ => panic!("expected Coalesced at the top"),
        }
    }

    #[test]
    fn stddev_family_lowers_to_dedicated_variants() {
        assert!(matches!(stddev("x").build(), AggregateExpr::StdDev("x")));
        assert!(matches!(
            stddev_pop("x").build(),
            AggregateExpr::StdDevPop("x")
        ));
        assert!(matches!(
            variance("x").build(),
            AggregateExpr::Variance("x")
        ));
        assert!(matches!(
            variance_pop("x").build(),
            AggregateExpr::VariancePop("x")
        ));
    }
}
