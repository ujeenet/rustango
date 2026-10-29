//! `Page<T>` result + the paginated SELECT that counts its total.
//!
//! Extracted from `executor/mod.rs` as part of #116 step 2. The
//! pagination shape is small and depends on nothing else from the
//! executor's internals, so it lives in its own file.

/// Result of [`QuerySet::fetch_paginated_on`](crate::query::QuerySet)
/// — a slice of rows
/// alongside the total count of matching rows in the underlying
/// query (i.e. the count *before* LIMIT/OFFSET).
///
/// Both pieces come from a single SQL round trip via
/// `COUNT(*) OVER ()`, so paginated endpoints don't pay the
/// customary "one query for the page, one for the count" cost.
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
fn inject_total_count(sql: &str) -> String {
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

/// The page query: `select` plus a `__rustango_total` column holding the
/// pre-LIMIT row count.
pub(super) fn paginated_statement(
    d: &dyn crate::sql::Dialect,
    select: &crate::core::SelectQuery,
) -> Result<crate::sql::CompiledStatement, crate::sql::SqlError> {
    // Off PG, DISTINCT ON is a window rewrite whose outer level counts right.
    let distinct = match select.distinct {
        Some(crate::core::DistinctMode::All) => true,
        Some(crate::core::DistinctMode::On(_)) => d.name() == "postgres",
        None => false,
    };
    if select.compound.is_empty() && distinct {
        let mut b = crate::sql::writers::Sql::new(d);
        crate::sql::writers::write_distinct_with_total(&mut b, select)?;
        return b.finish();
    }
    if select.compound.is_empty() {
        let stmt = d.compile_select(select)?;
        return Ok(crate::sql::CompiledStatement {
            sql: inject_total_count(&stmt.sql),
            params: stmt.params,
        });
    }
    let mut b = crate::sql::writers::Sql::new(d);
    crate::sql::writers::write_compound_with_total(&mut b, select)?;
    b.finish()
}

/// The pre-LIMIT count for a page past the last row, which has no row to
/// carry `__rustango_total`. `None` when the query has no OFFSET.
pub(super) fn total_statement(
    d: &dyn crate::sql::Dialect,
    select: &crate::core::SelectQuery,
) -> Result<Option<crate::sql::CompiledStatement>, crate::sql::SqlError> {
    let offset = if select.compound.is_empty() {
        select.offset
    } else {
        select.compound_offset
    };
    if offset.unwrap_or(0) <= 0 {
        return Ok(None);
    }
    let mut all = select.clone();
    all.limit = None;
    all.offset = None;
    all.compound_limit = None;
    all.compound_offset = None;
    d.compile_count(&crate::core::CountQuery::from_select(all))
        .map(Some)
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use crate::core::{
        CompoundBranch, FieldSchema, FieldType, ModelSchema, NullsOrder, OrderItem, SelectQuery,
        SetOp,
    };

    const ITEM: &ModelSchema = &{
        const FIELDS: &[FieldSchema] = &[
            FieldSchema::new("id", "id", FieldType::I64),
            FieldSchema::new("name", "name", FieldType::String),
            FieldSchema::new("rnk", "rnk", FieldType::I64),
        ];
        let mut s = ModelSchema::new("PageItem", "page_item");
        s.fields = FIELDS;
        s
    };

    /// `name` values of two branches joined by `op`, paged by `rnk`.
    fn paged(op: SetOp) -> SelectQuery {
        let mut q = SelectQuery::new(ITEM);
        q.projection = Some(vec!["name"]);
        q.compound = vec![CompoundBranch {
            op,
            query: Box::new(q.clone()),
        }];
        q.compound_order_by = vec![OrderItem::Column {
            column: "rnk",
            desc: false,
            nulls: NullsOrder::Default,
        }];
        q.compound_limit = Some(2);
        q
    }

    /// A paged set op ordered outside its projection counts its total over
    /// the projected columns, and refuses one that removes duplicates.
    #[test]
    fn paged_set_op_ordered_outside_the_projection() {
        let d = &crate::sql::Sqlite;
        let sql = super::paginated_statement(d, &paged(SetOp::UnionAll))
            .expect("UNION ALL")
            .sql;
        assert!(
            sql.starts_with(r#"SELECT "name", COUNT(*) OVER () AS "__rustango_total""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#") AS "__rustango_set" ORDER BY "rnk""#),
            "{sql}"
        );
        let err = super::paginated_statement(d, &paged(SetOp::Union)).expect_err("UNION");
        assert!(
            matches!(
                err,
                crate::sql::SqlError::SetOpOrderOutsideProjection { column: "rnk" }
            ),
            "{err:?}"
        );
    }
}
