//! Django-shape template context processors — issue #384.
//!
//! Context processors are callables that receive an
//! `axum::http::request::Parts` (Django's `request`) and produce a
//! `{key → value}` map that's merged into every Tera template's
//! context. They let cross-cutting concerns (request user, active
//! locale, feature flags, build version, …) appear in every
//! template without each handler having to thread them in manually.
//!
//! ## Usage
//!
//! ```ignore
//! use serde_json::json;
//!
//! rustango::register_template_context_processor!(|_parts| {
//!     // Single key — pulled from anywhere (env, settings,
//!     // request state). Map entries land directly in the Tera
//!     // context, accessible as `{{ build_version }}` etc.
//!     [("build_version", json!(env!("CARGO_PKG_VERSION")))].into()
//! });
//! ```
//!
//! Then in a handler that opts into the processor merge:
//!
//! ```ignore
//! use tera::Context;
//!
//! let mut ctx = Context::new();
//! ctx.insert("title", "Hello");
//! rustango::template_context_processors::apply_to_context(&mut ctx, parts);
//! tera.render("page.html", &ctx)?
//! ```
//!
//! ## Why a per-call helper instead of an automatic middleware?
//!
//! Tera's `tera.render(name, ctx)` takes a fully-built `Context`;
//! there's no extension point between Tera and the bytes-out
//! pipeline where we could splice processors in invisibly. So the
//! helper sits at the caller's side, and the framework's
//! CBV/template-views call it on the user's behalf where it makes
//! sense. Handlers rolling their own `tera.render` either call
//! `apply_to_context` explicitly or skip the merge — both are
//! deliberate.
//!
//! ## Override semantics
//!
//! When a handler-supplied key collides with a processor-supplied
//! key, **the handler wins** — `apply_to_context` only inserts
//! keys that aren't already present. Same shape as Django's
//! processor + render-context merge order. Lets a per-handler
//! override neutralize a sitewide default without unregistering
//! the processor.

use std::collections::HashMap;

use axum::http::request::Parts;
use serde_json::Value;
use tera::Context;

/// Signature of a template context processor. Pure `fn` pointer
/// (not `Arc<dyn Fn>`) so the registration can live in
/// `inventory::submit!`'s `static` storage — see the same shape on
/// [`crate::admin::custom_views::CustomViewHandler`].
pub type ContextProcessorFn = fn(&Parts) -> HashMap<String, Value>;

/// One registration. Inventory-collected via
/// [`crate::register_template_context_processor!`].
pub struct ContextProcessor {
    /// The callable. Receives the request's
    /// `axum::http::request::Parts` (headers + uri + method + …,
    /// minus the body) and returns the keys to merge.
    pub processor: ContextProcessorFn,
}

inventory::collect!(ContextProcessor);

/// Walk every registered context processor + merge their keys into
/// `ctx`. Handler-supplied keys win on collision.
///
/// Cheap to call on every request — `inventory::iter` is `O(N)`
/// over a typically-small N.
pub fn apply_to_context(ctx: &mut Context, parts: &Parts) {
    for entry in inventory::iter::<ContextProcessor> {
        let kv = (entry.processor)(parts);
        for (k, v) in kv {
            // Django-shape: handler-supplied keys WIN. Skip when
            // the caller already inserted the key — same merge
            // order as Django's render-context vs. processor
            // merge.
            if ctx.contains_key(&k) {
                continue;
            }
            ctx.insert(&k, &v);
        }
    }
}

/// Same as [`apply_to_context`] but takes a fresh
/// [`tera::Context`] and returns it built — for handlers that
/// don't have any caller-side context to merge into.
#[must_use]
pub fn context_from_processors(parts: &Parts) -> Context {
    let mut ctx = Context::new();
    apply_to_context(&mut ctx, parts);
    ctx
}

/// Register a template context processor. Pair with
/// [`apply_to_context`] at the handler site (or rely on the
/// framework's CBV wrappers, which call it automatically).
///
/// ```ignore
/// rustango::register_template_context_processor!(|parts| {
///     let path = parts.uri.path().to_string();
///     [("request_path".into(), serde_json::json!(path))].into()
/// });
/// ```
#[macro_export]
macro_rules! register_template_context_processor {
    ($processor:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::template_context_processors::ContextProcessor {
                processor: {
                    // Coerce the user's expression to the typed
                    // fn-pointer up front so type inference flows
                    // into a closure body (e.g. `parts.uri.path()`
                    // would otherwise fail with an unknown-type
                    // error). Inventory::submit! requires a const-
                    // constructible value, hence the explicit
                    // signature on the inner fn rather than an
                    // `Arc<dyn Fn>`.
                    const _PROCESSOR: $crate::template_context_processors::ContextProcessorFn =
                        $processor;
                    _PROCESSOR
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use serde_json::json;

    fn parts_for_path(path: &str) -> Parts {
        let req: Request<()> = Request::builder().uri(path).body(()).unwrap();
        let (parts, ()) = req.into_parts();
        parts
    }

    #[test]
    fn apply_to_context_is_noop_with_no_registrations() {
        let parts = parts_for_path("/x");
        let mut ctx = Context::new();
        ctx.insert("a", &"original");
        apply_to_context(&mut ctx, &parts);
        // Nothing else gets injected — the only key is the one
        // we pre-inserted.
        assert_eq!(
            ctx.into_json().as_object().unwrap().len(),
            1,
            "no registrations → no injected keys"
        );
    }

    #[test]
    fn handler_keys_win_over_processor_keys() {
        // Drive `apply_to_context` directly with a one-shot
        // processor lookalike — we can't run `inventory::submit!`
        // inside a function body, and registrations leak across
        // every test binary, so this tests the merge semantics
        // independently by walking a synthetic input.
        let parts = parts_for_path("/test");

        // Caller pre-inserted `winner` — processor's value must not
        // overwrite it.
        let mut ctx = Context::new();
        ctx.insert("winner", &"caller");

        // Simulate one processor run inline. The real
        // `apply_to_context` does the same loop over inventory;
        // testing the loop body keeps this assertion deterministic
        // across binaries.
        let processor: ContextProcessorFn = |_parts| {
            [
                ("winner".to_owned(), json!("processor")),
                ("only_in_processor".to_owned(), json!(42)),
            ]
            .into()
        };
        let injected = processor(&parts);
        for (k, v) in injected {
            if ctx.contains_key(&k) {
                continue;
            }
            ctx.insert(&k, &v);
        }
        let json = ctx.into_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("winner").unwrap(), &json!("caller"));
        assert_eq!(obj.get("only_in_processor").unwrap(), &json!(42));
    }

    #[test]
    fn context_from_processors_returns_fresh_context_without_registrations() {
        let parts = parts_for_path("/");
        let ctx = context_from_processors(&parts);
        assert!(ctx.into_json().as_object().unwrap().is_empty());
    }
}
