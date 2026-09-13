//! Django-shape custom template tags + filters — issue #383.
//!
//! Django lets app code register custom filters / functions / block
//! tags via the `@register.filter` / `@register.simple_tag` /
//! `@register.tag` decorators, which load automatically when the
//! app is in `INSTALLED_APPS`. This module ships the equivalent for
//! Tera: an inventory-collected registry of filters + functions
//! that the framework's Tera builders apply at template-engine
//! construction time.
//!
//! Tera doesn't expose a block-tag plugin API (no parser-level
//! extension point — see [`crate::cache_fragment`] for the same
//! limitation surface). So Django's `{% mytag %}` block-tag shape
//! stays out of reach; filters + functions are everything app code
//! actually needs in practice.
//!
//! ## Usage
//!
//! ```ignore
//! use std::collections::HashMap;
//! use serde_json::Value;
//!
//! fn shout(value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
//!     Ok(Value::String(
//!         value.as_str().unwrap_or("").to_uppercase() + "!"
//!     ))
//! }
//! rustango::register_template_filter!("shout", shout);
//!
//! fn build_version(_args: &HashMap<String, Value>) -> tera::Result<Value> {
//!     Ok(Value::String(env!("CARGO_PKG_VERSION").to_string()))
//! }
//! rustango::register_template_function!("build_version", build_version);
//! ```
//!
//! Then in a template:
//!
//! ```jinja
//! {{ "hello" | shout }}      {# → HELLO! #}
//! {{ build_version() }}      {# → 0.42.0 #}
//! ```
//!
//! Filters + functions register globally — every Tera instance the
//! framework builds (admin, template_views, email_templates) picks
//! them up via [`apply_to_tera`].
//!
//! ## Wiring into your own Tera instances
//!
//! Apps building their own `Tera` (outside the framework's CBVs)
//! must call [`apply_to_tera`] explicitly. It's a no-op when no
//! extensions are registered, so call it unconditionally:
//!
//! ```ignore
//! let mut tera = tera::Tera::new("templates/**/*.html")?;
//! rustango::default_filters::register_filters(&mut tera);
//! rustango::template_extensions::apply_to_tera(&mut tera);
//! ```
//!
//! ## Why inventory storage requires `fn` pointers
//!
//! Same reason as [`crate::admin::custom_views::CustomViewHandler`]
//! and [`crate::template_context_processors::ContextProcessorFn`]:
//! `inventory::submit!`'s `static` storage can only hold const-
//! constructible values, so the registration MUST be a plain
//! `fn` pointer rather than `Arc<dyn Fn>`. Both registry macros
//! coerce the user's expression to the typed pointer up front so
//! closure-body type inference still works.

use std::collections::HashMap;

use serde_json::Value;
use tera::Tera;

/// Tera filter signature. Matches the type Tera's
/// `register_filter` expects when given a plain fn.
pub type TeraFilterFn = fn(&Value, &HashMap<String, Value>) -> tera::Result<Value>;

/// Tera function signature. Matches the type Tera's
/// `register_function` expects when given a plain fn.
pub type TeraFunctionFn = fn(&HashMap<String, Value>) -> tera::Result<Value>;

/// One filter registration. Inventory-collected via
/// [`crate::register_template_filter!`].
pub struct TemplateFilter {
    /// Name templates use: `{{ value | foo }}`.
    pub name: &'static str,
    /// The callable.
    pub filter: TeraFilterFn,
}

inventory::collect!(TemplateFilter);

/// One function registration. Inventory-collected via
/// [`crate::register_template_function!`].
pub struct TemplateFunction {
    /// Name templates use: `{{ foo(arg1=...) }}` or
    /// `{% set x = foo() %}`.
    pub name: &'static str,
    /// The callable.
    pub function: TeraFunctionFn,
}

inventory::collect!(TemplateFunction);

/// Apply every inventory-registered filter + function to `tera`.
/// Idempotent — Tera's `register_filter` / `register_function`
/// overwrite the previous binding with the same name, so calling
/// `apply_to_tera` twice is safe.
///
/// Re-registering a built-in name (`length`, `upper`, …) replaces
/// the built-in with the user version. Same trade-off Django
/// makes; document accordingly if you override a stock filter.
pub fn apply_to_tera(tera: &mut Tera) {
    for entry in inventory::iter::<TemplateFilter> {
        tera.register_filter(entry.name, entry.filter);
    }
    for entry in inventory::iter::<TemplateFunction> {
        tera.register_function(entry.name, entry.function);
    }
}

/// Register a custom Tera filter globally. Picked up by
/// [`apply_to_tera`] at template-engine construction time.
///
/// ```ignore
/// use std::collections::HashMap;
/// use serde_json::Value;
///
/// fn shout(v: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
///     Ok(Value::String(v.as_str().unwrap_or("").to_uppercase()))
/// }
/// rustango::register_template_filter!("shout", shout);
/// ```
#[macro_export]
macro_rules! register_template_filter {
    ($name:expr, $filter:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::template_extensions::TemplateFilter {
                name: $name,
                filter: {
                    const _FILTER: $crate::template_extensions::TeraFilterFn = $filter;
                    _FILTER
                },
            }
        }
    };
}

/// Register a custom Tera function globally. Picked up by
/// [`apply_to_tera`] at template-engine construction time.
///
/// ```ignore
/// use std::collections::HashMap;
/// use serde_json::Value;
///
/// fn version(_args: &HashMap<String, Value>) -> tera::Result<Value> {
///     Ok(Value::String(env!("CARGO_PKG_VERSION").to_string()))
/// }
/// rustango::register_template_function!("version", version);
/// ```
#[macro_export]
macro_rules! register_template_function {
    ($name:expr, $function:expr $(,)?) => {
        $crate::inventory::submit! {
            $crate::template_extensions::TemplateFunction {
                name: $name,
                function: {
                    const _FUNCTION: $crate::template_extensions::TeraFunctionFn = $function;
                    _FUNCTION
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_to_tera_is_a_noop_with_no_registrations() {
        // Lib unit-test binary has no `register_template_filter!`
        // calls, so the apply walks empty iters and leaves Tera
        // untouched. Smoke-checks that the inventory link doesn't
        // panic when nothing's submitted.
        let mut tera = Tera::default();
        apply_to_tera(&mut tera);
        // A built-in filter still resolves — sanity that we didn't
        // somehow corrupt the Tera registry.
        tera.add_raw_template("smoke.html", "{{ [1,2,3] | length }}")
            .unwrap();
        let rendered = tera
            .render("smoke.html", &tera::Context::new())
            .expect("built-in filter still works");
        assert_eq!(rendered, "3");
    }

    #[test]
    fn apply_to_tera_is_idempotent_when_called_twice() {
        // Defensive: registering the same filter twice should be
        // safe (Tera::register_filter overwrites).
        let mut tera = Tera::default();
        apply_to_tera(&mut tera);
        apply_to_tera(&mut tera);
        // Nothing crashed, that's the test.
    }

    /// Spot-check the macro shape on a synthetic non-inventory
    /// registration — proves the typed fn-pointer coercion works.
    /// Real inventory-tying happens in the live test file because
    /// `inventory::submit!` can't live inside a function body.
    #[test]
    fn fn_pointer_coercion_smoke_test() {
        fn upper(v: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
            Ok(Value::String(v.as_str().unwrap_or("").to_uppercase()))
        }
        let f: TeraFilterFn = upper;
        let mut tera = Tera::default();
        tera.register_filter("upper_custom", f);
        tera.add_raw_template("t.html", r#"{{ "hi" | upper_custom }}"#)
            .unwrap();
        let r = tera.render("t.html", &tera::Context::new()).unwrap();
        assert_eq!(r, "HI");
    }
}
