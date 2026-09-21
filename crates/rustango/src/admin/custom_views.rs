//! Per-model custom admin views, like Django's
//! `ModelAdmin.get_urls()`.
//!
//! The admin ships the list, detail, new, edit and action routes. This
//! module lets a model add its own on top, such as
//! `/admin/blog_post/{id}/duplicate`. Registrations are collected by
//! inventory and mounted by the Builder at `build()` time.
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
//! The mount path is `{admin_prefix}/{table}/{suffix}`. The suffix may
//! hold axum path params, so `"copy/{id}"` mounts at
//! `…/blog_post/copy/{id}`. It must not clash with a built-in route:
//! `""`, `"new"`, `"__action"`, `"__autocomplete"`, `"{pk}"`,
//! `"{pk}/edit"` or `"{pk}/delete"`. On a clash the Builder logs a
//! warning and skips the view instead of panicking.
//!
//! Handlers run inside the admin's session-auth scope when one is
//! configured, so the operator must be signed in to reach them.

use axum::http::Method;
use axum::response::Response;
use std::future::Future;
use std::pin::Pin;

/// Boxed future returned by a [`CustomViewHandler`].
pub type CustomViewFuture = Pin<Box<dyn Future<Output = Response> + Send + 'static>>;

/// A custom admin view handler. Takes the admin's `Pool`, which hides
/// the backend, plus the raw request, and returns a response.
///
/// A plain `fn` pointer, not `Arc<dyn Fn>`, because `inventory::submit!`
/// stores it in a `static` and only const values fit. The macro wraps
/// the user's closure in a non-capturing function, so per-handler state
/// must live in a `static`, not in a capture.
pub type CustomViewHandler =
    fn(crate::sql::Pool, axum::http::Request<axum::body::Body>) -> CustomViewFuture;

/// One custom view registration, collected by inventory via
/// [`crate::register_admin_view!`].
pub struct AdminCustomView {
    /// Model table the view belongs to. Must equal
    /// `ModelSchema::table`; the Builder skips unregistered tables.
    pub table: &'static str,
    /// URL suffix after `{admin_prefix}/{table}/`. May hold axum path
    /// params, such as `"copy/{id}"`. See the module docs for the
    /// suffixes it must not use.
    pub suffix: &'static str,
    /// HTTP method the route answers on.
    pub method: Method,
    /// Short label for UI surfaces. An empty string means no UI entry;
    /// the route still mounts.
    pub label: &'static str,
    /// The handler. [`crate::register_admin_view!`] wraps a plain
    /// `async fn(Pool, Request) -> Response` into the boxed-future
    /// shape above.
    pub handler: CustomViewHandler,
}

inventory::collect!(AdminCustomView);

/// Every custom view registered for `table`, in registration order.
#[must_use]
pub fn for_table(table: &str) -> Vec<&'static AdminCustomView> {
    inventory::iter::<AdminCustomView>
        .into_iter()
        .filter(|v| v.table == table)
        .collect()
}

/// URL suffixes the admin Builder owns. A registration that uses one
/// is skipped with a warning at build time. Shared by the Builder and
/// the tests, so both read the same list.
pub(crate) const RESERVED_SUFFIXES: &[&str] = &["", "new", "__action", "__autocomplete"];

/// `true` when `suffix` clashes with a built-in admin route: either a
/// name in `RESERVED_SUFFIXES` or one of the pk paths `{pk}`,
/// `{pk}/edit` and `{pk}/delete`.
#[must_use]
pub(crate) fn is_reserved(suffix: &str) -> bool {
    let trimmed = suffix.trim_matches('/');
    if RESERVED_SUFFIXES.iter().any(|r| *r == trimmed) {
        return true;
    }
    // The framework also owns `/{table}/{pk}` and its `/edit` and
    // `/delete` forms.
    matches!(trimmed, "{pk}" | "{pk}/edit" | "{pk}/delete")
}

/// Register a custom admin view for one model.
///
/// See [the module-level docs](self) for the argument shape and the
/// reserved suffixes.
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
        // Wrap the user's expression in a non-capturing fn, so the
        // inventory entry holds a plain fn pointer, which is const
        // constructible. A closure with no captures fits.
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
