//! Django-shape `date_hierarchy` — clickable year/month/day drill-down
//! strip above the admin list view. Issue #355.
//!
//! When a model declares `#[rustango(admin(date_hierarchy = "<field>"))]`,
//! the list view:
//!
//! 1. Reads `?year=YYYY` `&month=MM` `&day=DD` from the query string.
//! 2. Injects two filters (`>= lo`, `< hi`) on the named column, where
//!    `[lo, hi)` is the half-open range implied by the selection.
//! 3. Computes the child buckets at the *current* drill level — years
//!    when nothing is selected, months when only year is, days when
//!    year+month are — via one tri-dialect `GROUP BY` query.
//! 4. Renders a breadcrumb + clickable child list above the table.
//!
//! The bucket query is dialect-routed:
//!
//! - Postgres / MySQL: `EXTRACT(YEAR FROM <col>)` etc.
//! - SQLite:           `CAST(strftime('%Y', <col>) AS INTEGER)` etc.
//!
//! Date range comparison binds two `SqlValue::DateTime` (or `Date`)
//! parameters — no string literals in the SQL — so naive query
//! params can never become an injection vector.

use chrono::{NaiveDate, TimeZone, Utc};

use crate::core::{FieldType, Filter, ModelSchema, Op, SqlValue};

/// Selection extracted from the URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DateSelection {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
}

impl DateSelection {
    /// Parse year/month/day from the list view's query-param map.
    pub(crate) fn parse(params: &std::collections::HashMap<String, String>) -> Self {
        let year = params
            .get("year")
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|y| (1..=9999).contains(y));
        let month = year.and(
            params
                .get("month")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|m| (1..=12).contains(m)),
        );
        let day = month.and(
            params
                .get("day")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|d| (1..=31).contains(d)),
        );
        Self { year, month, day }
    }
}

/// Half-open `[lo, hi)` range implied by the selection. Returns `None`
/// when no year is set (i.e. no narrowing required).
pub(crate) fn range(sel: DateSelection) -> Option<(NaiveDate, NaiveDate)> {
    let year = sel.year?;
    let lo = NaiveDate::from_ymd_opt(year, sel.month.unwrap_or(1), sel.day.unwrap_or(1))?;
    let hi = if sel.day.is_some() {
        lo.checked_add_signed(chrono::Duration::days(1))?
    } else if let Some(m) = sel.month {
        // Next month, rolling year on December → January.
        if m == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)?
        } else {
            NaiveDate::from_ymd_opt(year, m + 1, 1)?
        }
    } else {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    };
    Some((lo, hi))
}

/// Build the two predicates that narrow the list view to the selected
/// date range. Returns an empty Vec when no selection is active or the
/// field isn't a Date/DateTime.
pub(crate) fn predicates(
    model: &'static ModelSchema,
    field_name: &str,
    sel: DateSelection,
) -> Vec<Filter> {
    let Some(field) = model.field(field_name) else {
        return Vec::new();
    };
    let Some((lo, hi)) = range(sel) else {
        return Vec::new();
    };
    match field.ty {
        FieldType::Date => {
            vec![
                Filter {
                    column: field.column,
                    op: Op::Gte,
                    value: SqlValue::Date(lo),
                },
                Filter {
                    column: field.column,
                    op: Op::Lt,
                    value: SqlValue::Date(hi),
                },
            ]
        }
        FieldType::DateTime => {
            // Anchor at UTC midnight — the standard Django reading of
            // a date-hierarchy bucket boundary.
            let lo_dt = Utc.from_utc_datetime(&lo.and_hms_opt(0, 0, 0).unwrap());
            let hi_dt = Utc.from_utc_datetime(&hi.and_hms_opt(0, 0, 0).unwrap());
            vec![
                Filter {
                    column: field.column,
                    op: Op::Gte,
                    value: SqlValue::DateTime(lo_dt),
                },
                Filter {
                    column: field.column,
                    op: Op::Lt,
                    value: SqlValue::DateTime(hi_dt),
                },
            ]
        }
        _ => Vec::new(),
    }
}

/// Drill level at which child buckets are enumerated.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DrillLevel {
    Year,
    Month,
    Day,
}

impl DrillLevel {
    pub(crate) fn for_selection(sel: DateSelection) -> Option<Self> {
        match (sel.year, sel.month, sel.day) {
            (None, _, _) => Some(Self::Year),
            (Some(_), None, _) => Some(Self::Month),
            (Some(_), Some(_), None) => Some(Self::Day),
            (Some(_), Some(_), Some(_)) => None, // terminal — no children
        }
    }

    /// Dialect-routed SQL fragment that extracts the bucket value as an
    /// integer for this level. `col` must already be quoted.
    pub(crate) fn bucket_expr(self, dialect: &dyn crate::sql::Dialect, col_quoted: &str) -> String {
        let part = match self {
            Self::Year => "YEAR",
            Self::Month => "MONTH",
            Self::Day => "DAY",
        };
        let strftime_token = match self {
            Self::Year => "%Y",
            Self::Month => "%m",
            Self::Day => "%d",
        };
        if dialect.name() == "sqlite" {
            format!("CAST(strftime('{strftime_token}', {col_quoted}) AS INTEGER)")
        } else {
            format!("EXTRACT({part} FROM {col_quoted})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(y: Option<i32>, m: Option<u32>, d: Option<u32>) -> DateSelection {
        DateSelection {
            year: y,
            month: m,
            day: d,
        }
    }

    #[test]
    fn range_for_year_only_spans_calendar_year() {
        let (lo, hi) = range(sel(Some(2025), None, None)).unwrap();
        assert_eq!(lo, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());
        assert_eq!(hi, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    }

    #[test]
    fn range_for_year_and_month_rolls_at_december() {
        let (lo, hi) = range(sel(Some(2025), Some(12), None)).unwrap();
        assert_eq!(lo, NaiveDate::from_ymd_opt(2025, 12, 1).unwrap());
        assert_eq!(hi, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    }

    #[test]
    fn range_for_year_month_day_is_single_day() {
        let (lo, hi) = range(sel(Some(2025), Some(11), Some(15))).unwrap();
        assert_eq!(lo, NaiveDate::from_ymd_opt(2025, 11, 15).unwrap());
        assert_eq!(hi, NaiveDate::from_ymd_opt(2025, 11, 16).unwrap());
    }

    #[test]
    fn range_returns_none_with_no_year() {
        assert!(range(sel(None, None, None)).is_none());
    }

    #[test]
    fn drill_level_terminal_when_day_set() {
        assert!(DrillLevel::for_selection(sel(Some(2025), Some(11), Some(15))).is_none());
    }

    #[test]
    fn parse_drops_partial_selections() {
        // `?month=03` with no year is ignored — month/day require parent.
        let mut p = std::collections::HashMap::new();
        p.insert("month".into(), "3".into());
        let s = DateSelection::parse(&p);
        assert_eq!(s, DateSelection::default());
    }
}
