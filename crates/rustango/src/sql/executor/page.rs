//! `Page<T>` result + `inject_total_count` helper.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2. The
//! pagination shape is small and depends on nothing else from the
//! executor's internals, so it lives in its own file.

/// Result of [`QuerySet::fetch_paginated_on`] — a slice of rows
/// alongside the total count of matching rows in the underlying
/// query (i.e. the count *before* LIMIT/OFFSET).
///
/// Both pieces come from a single SQL round trip via
/// `COUNT(*) OVER ()`, so paginated endpoints don't pay the
/// customary "two queries per page" cost Django's `Paginator`
/// imposes.
pub struct Page<T> {
    pub rows: Vec<T>,
    pub total: i64,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            total: 0,
        }
    }
}

/// Splice `, COUNT(*) OVER () AS "__rustango_total"` into the
/// compiled SELECT's column list, just before the first ` FROM `.
/// The Postgres compile_select writer emits the shape
/// `SELECT <cols> FROM <table> ...` (single space before `FROM`), and
/// quoted column literals never contain the bare token ` FROM ` —
/// quoted strings are SqlValue parameters, not part of the column
/// list. The wrapper-subquery fallback handles unexpected shapes
/// safely (`COUNT(*) OVER ()` at the OUTER level still counts inner
/// rows correctly when the inner has no LIMIT, but with LIMIT the
/// outer COUNT would only see the limited slice — so we depend on
/// the fast path matching).
pub(super) fn inject_total_count(sql: &str) -> String {
    if let Some(idx) = sql.find(" FROM ") {
        let mut out = String::with_capacity(sql.len() + 48);
        out.push_str(&sql[..idx]);
        out.push_str(", COUNT(*) OVER () AS \"__rustango_total\"");
        out.push_str(&sql[idx..]);
        out
    } else {
        // Should not reach this branch with the current Postgres
        // writer — surface the unexpected SQL clearly rather than
        // silently producing wrong totals.
        format!(
            "/* rustango: fetch_paginated_on could not splice COUNT(*) OVER () \
             into the compiled SELECT — anchor ` FROM ` not found. The query \
             below will run unchanged but `total` will be 0. */ {sql}"
        )
    }
}
