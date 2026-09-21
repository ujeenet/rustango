//! Custom template filters and functions.
//!
//! Register them once with the macros below and every Tera instance
//! the framework builds picks them up.
//!
//! Tera has no plugin API for block tags, so a custom
//! `{% mytag %}…{% endmytag %}` is not available. See
//! [`crate::cache_fragment`] for the same limit.
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
//! {{ build_version() }}      {# → the running crate's version #}
//! ```
//!
//! ## Your own Tera instances
//!
//! If you build a `Tera` yourself, call [`apply_to_tera`] on it. It
//! does nothing when no extensions are registered, so call it always:
//!
//! ```ignore
//! let mut tera = tera::Tera::new("templates/**/*.html")?;
//! rustango::default_filters::register_filters(&mut tera);
//! rustango::template_extensions::apply_to_tera(&mut tera);
//! ```
//!
//! Registrations must be plain `fn` pointers, not `Arc<dyn Fn>`,
//! because `inventory::submit!` stores them in a `static`. The macros
//! coerce your expression to the right pointer type for you.
//!
//! [`apply_to_tera`]: crate::template_extensions::apply_to_tera

use std::collections::HashMap;

use serde_json::Value;
use tera::Tera;

/// Signature Tera's `register_filter` expects from a plain fn.
pub type TeraFilterFn = fn(&Value, &HashMap<String, Value>) -> tera::Result<Value>;

/// Signature Tera's `register_function` expects from a plain fn.
pub type TeraFunctionFn = fn(&HashMap<String, Value>) -> tera::Result<Value>;

/// One filter registration. Build it with
/// [`crate::register_template_filter!`].
pub struct TemplateFilter {
    /// Name templates use: `{{ value | foo }}`.
    pub name: &'static str,
    /// The callable.
    pub filter: TeraFilterFn,
}

inventory::collect!(TemplateFilter);

/// One function registration. Build it with
/// [`crate::register_template_function!`].
pub struct TemplateFunction {
    /// Name templates use: `{{ foo(arg=...) }}`.
    pub name: &'static str,
    /// The callable.
    pub function: TeraFunctionFn,
}

inventory::collect!(TemplateFunction);

/// Add every registered filter and function to `tera`. Safe to call
/// twice, since a repeat registration just replaces the old one.
///
/// A registration that reuses a built-in name, such as `length` or
/// `upper`, replaces that built-in.
pub fn apply_to_tera(tera: &mut Tera) {
    for entry in inventory::iter::<TemplateFilter> {
        tera.register_filter(entry.name, entry.filter);
    }
    for entry in inventory::iter::<TemplateFunction> {
        tera.register_function(entry.name, entry.function);
    }
}

/// Register a Tera filter. [`apply_to_tera`] installs it.
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

/// Register a Tera function. [`apply_to_tera`] installs it.
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
        // This test binary registers nothing, so the walk is empty.
        let mut tera = Tera::default();
        apply_to_tera(&mut tera);
        // A built-in filter still works.
        tera.add_raw_template("smoke.html", "{{ [1,2,3] | length }}")
            .unwrap();
        let rendered = tera
            .render("smoke.html", &tera::Context::new())
            .expect("built-in filter still works");
        assert_eq!(rendered, "3");
    }

    #[test]
    fn apply_to_tera_is_idempotent_when_called_twice() {
        let mut tera = Tera::default();
        apply_to_tera(&mut tera);
        apply_to_tera(&mut tera);
        // Not panicking is the assertion.
    }

    /// The fn-pointer coercion works. `inventory::submit!` cannot go
    /// in a function body, so the real wiring is tested elsewhere.
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
