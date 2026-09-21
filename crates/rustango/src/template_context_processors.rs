//! Template context processors.
//!
//! A processor takes the request's `Parts` and returns a map of keys
//! to merge into a template context. Use one for values every page
//! needs, such as the current user, the locale or the build version,
//! so no handler has to pass them in.
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
//! `tera.render` takes a finished `Context`, so there is no place to
//! splice processors in behind your back. The framework's class-based
//! views call
//! [`apply_to_context`](crate::template_context_processors::apply_to_context)
//! for you; a hand-written handler
//! calls it itself, or skips the merge.
//!
//! ## Key collisions
//!
//! The handler wins.
//! [`apply_to_context`](crate::template_context_processors::apply_to_context)
//! only adds keys that are not
//! there yet, so one page can override a sitewide default without
//! unregistering the processor.

use std::collections::HashMap;

use axum::http::request::Parts;
use serde_json::Value;
use tera::Context;

/// Signature of a context processor. A plain `fn` pointer, because
/// `inventory::submit!` stores the registration in a `static`.
pub type ContextProcessorFn = fn(&Parts) -> HashMap<String, Value>;

/// One registration. Build it with
/// [`crate::register_template_context_processor!`].
pub struct ContextProcessor {
    /// The callable. It gets the request head (method, uri, headers,
    /// but no body) and returns the keys to merge.
    pub processor: ContextProcessorFn,
}

inventory::collect!(ContextProcessor);

/// Merge every registered processor's keys into `ctx`. Keys the
/// handler already set are left alone.
///
/// Cheap enough to call on every request.
pub fn apply_to_context(ctx: &mut Context, parts: &Parts) {
    for entry in inventory::iter::<ContextProcessor> {
        let kv = (entry.processor)(parts);
        for (k, v) in kv {
            // The handler's own keys win.
            if ctx.contains_key(&k) {
                continue;
            }
            ctx.insert(&k, &v);
        }
    }
}

/// [`apply_to_context`] on a fresh [`tera::Context`], for handlers
/// with nothing of their own to merge in.
#[must_use]
pub fn context_from_processors(parts: &Parts) -> Context {
    let mut ctx = Context::new();
    apply_to_context(&mut ctx, parts);
    ctx
}

/// Register a context processor. [`apply_to_context`] runs it.
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
                    // Naming the fn-pointer type here lets inference
                    // reach into a closure body, so `parts.uri.path()`
                    // compiles. `inventory::submit!` also needs a
                    // const value, which rules out `Arc<dyn Fn>`.
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
        // Only the key we inserted is present.
        assert_eq!(
            ctx.into_json().as_object().unwrap().len(),
            1,
            "no registrations → no injected keys"
        );
    }

    #[test]
    fn handler_keys_win_over_processor_keys() {
        // `inventory::submit!` cannot go in a function body, and a
        // real registration would leak into other tests, so run the
        // same merge loop over a local processor instead.
        let parts = parts_for_path("/test");

        // The caller set `winner` first, so it must survive.
        let mut ctx = Context::new();
        ctx.insert("winner", &"caller");

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
