//! Django-shape `ModelAdmin.get_urls()` per-model custom views —
//! issue #363.
//!
//! The admin already ships every "list / detail / new / edit /
//! action" route built-in, but Django's `get_urls()` lets a
//! ModelAdmin add arbitrary routes scoped to one model:
//! `/admin/blog/post/<id>/duplicate/`,
//! `/admin/orders/order/<id>/print/`, etc. This module exposes the
//! equivalent in rustango as an inventory-collected registry that
//! the admin Builder walks at `build()` time.
//!
//! ## Usage
//!
//! ```ignore
//! use axum::body::Body;
//! use axum::http::{Method, Request};
//! use axum::response::{Html, IntoResponse, Response};
//! use rustango::sql::Pool;
//!
//! async fn duplicate(_pool: Pool, _req: Request<Body>) -> Response {
//!     Html("<p>duplicated!</p>").into_response()
//! }
//!
//! rustango::register_admin_view!(
//!     "blog_post",          // ModelSchema::table
//!     "duplicate",          // URL suffix → mounted at /<admin>/blog_post/duplicate
//!     Method::POST,         // HTTP method
//!     "Duplicate post",     // human label (used by future UI surfaces)
//!     duplicate,            // async fn(Pool, Request<Body>) -> Response
//! );
//! ```
//!
//! Mount path resolves to `{admin_prefix}/{table}/{suffix}`. The
//! suffix may contain axum path params: `"copy/{id}"` mounts as
//! `…/blog_post/copy/{id}`. Suffix MUST NOT collide with the
//! framework's built-in routes (`""`, `"new"`, `"__action"`,
//! `"__autocomplete"`, `"{pk}"`, `"{pk}/edit"`, `"{pk}/delete"`);
//! the Builder logs a warning + skips on collision rather than
//! panicking at startup.
//!
//! Handlers run inside the admin's session-auth scope when one is
//! configured — the operator must be logged in to reach them.

use axum::http::Method;
use axum::response::Response;
use std::future::Future;
use std::pin::Pin;

/// Boxed future returned by a [`CustomViewHandler`].
pub type CustomViewFuture = Pin<Box<dyn Future<Output = Response> + Send + 'static>>;

/// Signature for a custom admin view handler. Receives the admin's
/// `Pool` (backend-erasing — PG / MySQL / SQLite) and the raw
/// `axum::http::Request`. Returns a response.
///
/// Plain `fn` pointer (not `Arc<dyn Fn>`) so the registration can
/// live in `inventory::submit!`'s `static` storage, which can only
/// hold const-constructible values. The macro side wraps the user's
/// closure in a non-capturing inner function — any per-handler
/// state has to live in `static`s the closure references, not
/// closure captures.
pub type CustomViewHandler =
    fn(crate::sql::Pool, axum::http::Request<axum::body::Body>) -> CustomViewFuture;

/// One per-model custom view registration. Inventory-collected via
/// [`crate::register_admin_view!`].
pub struct AdminCustomView {
    /// Model table the view belongs to — must match
    /// `ModelSchema::table` exactly. The Builder skips views whose
    /// table isn't registered.
    pub table: &'static str,
    /// URL suffix appended after `{admin_prefix}/{table}/`. May
    /// contain axum path params (e.g. `"copy/{id}"`). Must not
    /// collide with built-in admin routes (see module docs).
    pub suffix: &'static str,
    /// HTTP method — `GET` / `POST` / `PUT` / `DELETE` / `PATCH`.
    pub method: Method,
    /// Short human label — surfaces in future detail-page action
    /// menus / breadcrumbs. Empty string suppresses any UI hook
    /// (the route still mounts).
    pub label: &'static str,
    /// The handler callable. Built by [`crate::register_admin_view!`]
    /// to wrap a plain `async fn(Pool, Request) -> Response` into
    /// the boxed-future shape above.
    pub handler: CustomViewHandler,
}

inventory::collect!(AdminCustomView);

/// Return every custom view registered for `table`, in registration
/// order.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static AdminCustomView> {
    inventory::iter::<AdminCustomView>
        .into_iter()
        .filter(|v| v.table == table)
        .collect()
}

/// Reserved URL suffixes the admin Builder hard-codes — registrations
/// that collide are skipped with a tracing warning at build time.
/// Exposed so tests + the Builder share the same canonical list.
pub(crate) const RESERVED_SUFFIXES: &[&str] = &["", "new", "__action", "__autocomplete"];

/// Returns `true` when `suffix` collides with a built-in admin route.
/// Static suffixes match exactly; pk-positional suffixes (anything
/// that starts with the axum capture `{`) match any single-segment
/// pk path (`{pk}`, `{pk}/edit`, `{pk}/delete`).
#[must_use]
pub(crate) fn is_reserved(suffix: &str) -> bool {
    let trimmed = suffix.trim_matches('/');
    if RESERVED_SUFFIXES.iter().any(|r| *r == trimmed) {
        return true;
    }
    // The framework owns `/{table}/{pk}`, `/{table}/{pk}/edit`,
    // `/{table}/{pk}/delete`. Anything that matches one of those
    // shapes is also reserved.
    matches!(trimmed, "{pk}" | "{pk}/edit" | "{pk}/delete")
}

/// Register a custom admin view scoped to one model.
///
/// See [the module-level docs](self) for the macro shape and the
/// list of reserved suffixes.
///
/// ```ignore
/// rustango::register_admin_view!(
///     "blog_post",
///     "duplicate",
///     axum::http::Method::POST,
///     "Duplicate post",
///     |_pool, _req| async move {
///         use axum::response::{Html, IntoResponse};
///         Html("<p>duplicated!</p>").into_response()
///     },
/// );
/// ```
#[macro_export]
macro_rules! register_admin_view {
    ($table:expr, $suffix:expr, $method:expr, $label:expr, $handler:expr $(,)?) => {
        // Wrap the user's expression in a non-capturing fn so the
        // inventory entry can use a plain fn-pointer (const-
        // constructible, unlike `Arc::new(...)`). The user's
        // `$handler` runs inside the body — closures with no
        // captures fit fine.
        $crate::inventory::submit! {
            $crate::admin::custom_views::AdminCustomView {
                table: $table,
                suffix: $suffix,
                method: $method,
                label: $label,
                handler: {
                    fn __rustango_admin_view_handler(
                        pool: $crate::sql::Pool,
                        req: ::axum::http::Request<::axum::body::Body>,
                    ) -> $crate::admin::custom_views::CustomViewFuture {
                        ::std::boxed::Box::pin(($handler)(pool, req))
                    }
                    __rustango_admin_view_handler
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_suffixes_includes_built_in_routes() {
        assert!(is_reserved(""));
        assert!(is_reserved("new"));
        assert!(is_reserved("__action"));
        assert!(is_reserved("__autocomplete"));
        assert!(is_reserved("{pk}"));
        assert!(is_reserved("{pk}/edit"));
        assert!(is_reserved("{pk}/delete"));
    }

    #[test]
    fn reserved_suffixes_strips_surrounding_slashes() {
        assert!(is_reserved("/new"));
        assert!(is_reserved("/new/"));
        assert!(is_reserved("new/"));
    }

    #[test]
    fn user_suffixes_are_not_reserved() {
        assert!(!is_reserved("duplicate"));
        assert!(!is_reserved("copy/{id}"));
        assert!(!is_reserved("export.csv"));
        assert!(!is_reserved("preview"));
    }

    #[test]
    fn for_table_returns_empty_when_no_registrations() {
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
    }
}
