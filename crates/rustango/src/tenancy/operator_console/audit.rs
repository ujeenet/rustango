//! Reading the registry's audit trail from the console.
//!
//! Every console mutation already writes here — `emit_op_audit` on
//! tenant edits, branding, impersonation, and now operator and hostname
//! changes. Nothing read it back. The record an operator would want
//! during an incident ("who deactivated this tenant, and when?") existed
//! and was reachable only with a SQL client on the registry.
//!
//! ## Registry only
//!
//! `rustango_audit_log` exists in the registry *and* in every tenant's
//! storage, and this page reads the registry's. Tenant-side rows are the
//! tenant's own history — a tenant admin's edits to tenant data — and
//! belong to the tenant admin, which already has a viewer. Mixing them
//! would mean fanning out a query over every tenant pool to render one
//! page.
//!
//! ## Read-only, and mounted for every console
//!
//! There is no write path, so this is not behind the edit gate: a
//! read-only console is exactly the deployment where "what happened?"
//! still needs answering. Any authenticated operator can already see and
//! change everything the log describes, so showing them the log grants
//! nothing new.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::Extension;
use serde::Deserialize;
use tera::Context;

use super::super::auth;
use super::{inject_op_brand, render, ConsoleState};

/// A screenful. The table grows with every console action, so the page
/// pages rather than pretending the history is small.
const PAGE_SIZE: i64 = 50;

#[derive(Deserialize)]
pub(super) struct AuditQuery {
    #[serde(default)]
    entity_table: Option<String>,
    #[serde(default)]
    entity_pk: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    page: Option<i64>,
}

pub(super) async fn audit_list(
    State(state): State<ConsoleState>,
    Extension(op): Extension<auth::Operator>,
    Query(q): Query<AuditQuery>,
) -> Response<Body> {
    // 1-based in the URL, 0-based in the query: a `?page=0` or a
    // negative should read as the first page rather than as an offset
    // the database has to reject.
    //
    // Saturating, not plain arithmetic: `?page=1000000000000000000`
    // parses into an `i64` happily and then overflows when multiplied,
    // which panicked the worker in debug and wrapped to a negative
    // offset in release. Clamped, an absurd page is simply an empty one.
    let page = q.page.unwrap_or(1).max(1);
    let offset = page.saturating_sub(1).saturating_mul(PAGE_SIZE);

    let filter = crate::audit::AuditFilter {
        entity_table: blank_to_none(q.entity_table.as_deref()),
        entity_pk: blank_to_none(q.entity_pk.as_deref()),
        operation: blank_to_none(q.operation.as_deref()),
        source: None,
    };

    // One extra row, so "is there a next page?" is answered without a
    // second COUNT over a table that only grows.
    let mut entries =
        match crate::audit::list(&state.registry, &filter, PAGE_SIZE + 1, offset).await {
            Ok(e) => e,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not read the audit log: {e}"),
                )
                    .into_response();
            }
        };
    let page_len = usize::try_from(PAGE_SIZE).unwrap_or(usize::MAX);
    let has_next = entries.len() > page_len;
    entries.truncate(page_len);

    let view: Vec<_> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "occurred_at": e.occurred_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                "entity_table": e.entity_table,
                "entity_pk": e.entity_pk,
                "operation": e.operation,
                "source": e.source,
                "who": who(&e.source),
                "what": what(&e.source),
                // Pretty-printed: `changes` is the column that actually
                // answers "what changed", and one long line of JSON is
                // not an answer anybody reads.
                "changes": serde_json::to_string_pretty(&e.changes)
                    .unwrap_or_else(|_| e.changes.to_string()),
            })
        })
        .collect();

    let mut ctx = Context::new();
    inject_op_brand(&mut ctx, &state.op_brand);
    ctx.insert("section", "audit");
    ctx.insert("operator_username", &op.username);
    ctx.insert("entries", &view);
    ctx.insert("page", &page);
    ctx.insert("has_next", &has_next);
    ctx.insert("filter_entity_table", &q.entity_table.unwrap_or_default());
    ctx.insert("filter_entity_pk", &q.entity_pk.unwrap_or_default());
    ctx.insert("filter_operation", &q.operation.unwrap_or_default());
    ctx.insert("query_suffix", &filter_suffix(&filter));

    render(&state, "op_audit.html", &ctx)
}

fn blank_to_none(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// The filters, re-encoded for the paging links so moving to page 2
/// does not silently drop what the operator filtered by.
fn filter_suffix(filter: &crate::audit::AuditFilter) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (key, value) in [
        ("entity_table", filter.entity_table.as_deref()),
        ("entity_pk", filter.entity_pk.as_deref()),
        ("operation", filter.operation.as_deref()),
    ] {
        if let Some(v) = value {
            let _ = write!(out, "&{key}={}", super::urlencoding_lite(v));
        }
    }
    out
}

/// The actor in `operator:<id>:<verb>`, or the raw source.
///
/// `AuditSource::Custom` renders that shape for every console action —
/// see `emit_registry_audit` — and a column of `operator:1:host_add`
/// makes the reader parse the same string on every row.
fn who(source: &str) -> String {
    match source.strip_prefix("operator:") {
        Some(rest) => match rest.split_once(':') {
            Some((id, _)) => format!("operator {id}"),
            None => format!("operator {rest}"),
        },
        None => source.to_owned(),
    }
}

/// The verb half of the same shape.
fn what(source: &str) -> String {
    source
        .strip_prefix("operator:")
        .and_then(|rest| rest.split_once(':'))
        .map_or_else(|| source.to_owned(), |(_, verb)| verb.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_source_shape_splits_into_who_and_what() {
        assert_eq!(who("operator:1:host_add"), "operator 1");
        assert_eq!(what("operator:1:host_add"), "host_add");
        // The verb may itself contain a colon (`operator:1:impersonating`
        // is the two-part case, but nothing forbids more).
        assert_eq!(what("operator:7:a:b"), "a:b");
    }

    /// Anything not written by the console — a tenant-side source, a
    /// system one — has to render as itself rather than as a mangled
    /// "operator".
    #[test]
    fn a_source_that_is_not_an_operator_is_left_alone() {
        assert_eq!(who("system"), "system");
        assert_eq!(what("system"), "system");
        assert_eq!(who("user:42"), "user:42");
    }

    #[test]
    fn blank_filters_do_not_become_empty_string_matches() {
        assert_eq!(blank_to_none(Some("   ")), None);
        assert_eq!(blank_to_none(Some("")), None);
        assert_eq!(blank_to_none(None), None);
        assert_eq!(blank_to_none(Some(" orgs ")).as_deref(), Some("orgs"));
    }

    #[test]
    fn paging_links_keep_the_filters() {
        let f = crate::audit::AuditFilter {
            entity_table: Some("rustango_orgs".into()),
            entity_pk: Some("acme".into()),
            operation: None,
            source: None,
        };
        let suffix = filter_suffix(&f);
        assert!(suffix.contains("entity_table=rustango_orgs"), "{suffix}");
        assert!(suffix.contains("entity_pk=acme"), "{suffix}");
        assert!(!suffix.contains("operation="), "unset filters stay out");
    }
}
