//! Extra admin routes that a single model owns.
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
//! warning and skips the view instead of panicking. A suffix whose
//! params also match one, such as `"{id}"` or `"{id}/edit"`, wins that
//! route for the table; the Builder warns about it.
//!
//! Handlers run inside the admin's session-auth scope when one is
//! configured, so the operator must be signed in to reach them.
//!
//! With `Builder::with_user_perms`, a view needs `{table}.view` for a
//! GET or QUERY and `{table}.change` for POST, PUT, PATCH or DELETE; the
//! rest get 403. A trailing `perm = "publish"` asks for
//! `{table}.publish` instead. Without `with_user_perms` the user acts as
//! superuser, who holds every codename, so a declared `perm` passes.

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
    /// The action codename the user needs, as in `{table}.<perm>`.
    /// `None` means `view` for a safe method and `change` for a write,
    /// checked only under `Builder::with_user_perms`.
    pub perm: Option<&'static str>,
}

impl AdminCustomView {
    /// The method the route answers on: an unsupported one mounts as GET.
    #[must_use]
    pub fn mount_method(&self) -> Method {
        match self.method.as_str() {
            "POST" | "PUT" | "PATCH" | "DELETE" | "QUERY" => self.method.clone(),
            _ => Method::GET,
        }
    }

    /// The action this view needs: its declared `perm`, else by the mounted method.
    #[must_use]
    pub fn required_perm(&self) -> &'static str {
        self.perm.unwrap_or(match self.mount_method().as_str() {
            "GET" | "QUERY" => "view",
            _ => "change",
        })
    }
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

/// URL suffixes the admin Builder mounts under `/{table}/`, in the
/// order a shadowing view is reported.
const BUILT_IN_SUFFIXES: &[&str] = &[
    "",
    "new",
    "__action",
    "__autocomplete",
    "{pk}",
    "{pk}/edit",
    "{pk}/delete",
];

/// How a custom view's suffix meets the built-in admin routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clash {
    /// The exact path of a built-in route: the Builder skips the view.
    Reserved,
    /// Its params also match this built-in route; axum would route the
    /// view first, so the Builder skips it too.
    Shadows(&'static str),
}

/// How `suffix` clashes with a built-in route, if it does.
#[must_use]
pub(crate) fn clash(suffix: &str) -> Option<Clash> {
    let trimmed = suffix.trim_matches('/');
    if BUILT_IN_SUFFIXES.contains(&trimmed) {
        return Some(Clash::Reserved);
    }
    BUILT_IN_SUFFIXES
        .iter()
        .find(|b| !b.is_empty() && covers(trimmed, b))
        .map(|b| Clash::Shadows(b))
}

/// `true` when every path `built_in` matches is also matched by `custom`.
fn covers(custom: &str, built_in: &str) -> bool {
    let is_param = |s: &str| s.starts_with('{') && s.ends_with('}');
    let (mut c, mut b) = (custom.split('/'), built_in.split('/'));
    loop {
        match (c.next(), b.next()) {
            (None, None) => return true,
            (Some(cs), Some(_)) if cs.starts_with("{*") && cs.ends_with('}') => return true,
            (Some(cs), Some(bs)) if is_param(cs) || (!is_param(bs) && cs == bs) => {}
            _ => return false,
        }
    }
}

/// Register a custom admin view for one model.
///
/// See [the module-level docs](self) for the argument shape, the
/// reserved suffixes and the optional trailing `perm = "…"`.
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
        $crate::register_admin_view!(@entry $table, $suffix, $method, $label, $handler, ::core::option::Option::None);
    };
    ($table:expr, $suffix:expr, $method:expr, $label:expr, $handler:expr, perm = $perm:expr $(,)?) => {
        $crate::register_admin_view!(@entry $table, $suffix, $method, $label, $handler, ::core::option::Option::Some($perm));
    };
    (@entry $table:expr, $suffix:expr, $method:expr, $label:expr, $handler:expr, $perm:expr) => {
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
                perm: $perm,
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_suffixes_includes_built_in_routes() {
        for s in [
            "",
            "new",
            "__action",
            "__autocomplete",
            "{pk}",
            "{pk}/edit",
            "{pk}/delete",
        ] {
            assert_eq!(clash(s), Some(Clash::Reserved), "{s}");
        }
    }

    #[test]
    fn reserved_suffixes_strips_surrounding_slashes() {
        for s in ["/new", "/new/", "new/"] {
            assert_eq!(clash(s), Some(Clash::Reserved), "{s}");
        }
    }

    #[test]
    fn user_suffixes_are_not_reserved() {
        for s in ["duplicate", "copy/{id}", "export.csv", "preview"] {
            assert_eq!(clash(s), None, "{s}");
        }
    }

    #[test]
    fn a_renamed_param_shadows_the_built_in_it_matches() {
        assert_eq!(clash("{id}"), Some(Clash::Shadows("new")));
        assert_eq!(clash("{id}/edit"), Some(Clash::Shadows("{pk}/edit")));
        assert_eq!(
            clash("/{slug}/delete/"),
            Some(Clash::Shadows("{pk}/delete"))
        );
        assert_eq!(clash("{a}/{b}"), Some(Clash::Shadows("{pk}/edit")));
        assert_eq!(clash("{*rest}"), Some(Clash::Shadows("new")));
    }

    #[test]
    fn suffixes_that_match_no_built_in_do_not_clash() {
        for s in [
            "duplicate",
            "copy/{id}",
            "{id}/duplicate",
            "export.csv",
            "{id}/edit/x",
        ] {
            assert_eq!(clash(s), None, "{s}");
        }
    }

    /// The premise: axum picks the custom view's static table segment over
    /// `{table}`, then answers 405 rather than falling back to the built-in.
    #[tokio::test]
    async fn axum_routes_a_shadowing_view_over_the_built_in() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use axum::routing::{get, post};
        use tower::ServiceExt as _;
        let app = axum::Router::new()
            .route("/{table}/new", get(|| async { "create" }))
            .route("/{table}/{pk}", get(|| async { "detail" }))
            .route("/post/{id}", post(|| async { "custom" }));
        for uri in ["/post/7", "/post/new"] {
            let req = Request::get(uri).body(Body::empty()).unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        }
    }

    #[test]
    fn for_table_returns_empty_when_no_registrations() {
        let v = for_table("nonexistent_table");
        assert!(v.is_empty());
    }
}
