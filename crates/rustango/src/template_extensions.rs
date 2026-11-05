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
//! let mut tera = rustango::template_extensions::html_tera_from_glob("templates/**/*")?;
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

/// Every name ends with `""`, so this escapes all templates, not just
/// Tera's default `.html`/`.htm`/`.xml` (#1721).
const ESCAPE_EVERY_TEMPLATE: &str = "";

/// An empty Tera that HTML-escapes every template, whatever its name.
/// Use `{{ value | safe }}` for markup you trust.
#[must_use]
pub fn html_tera() -> Tera {
    let mut tera = Tera::default();
    tera.autoescape_on(vec![ESCAPE_EVERY_TEMPLATE]);
    tera
}

/// Field help text as HTML: escaped, with each `` `…` `` span as `<code>`.
/// An unpaired backtick stays as text.
#[must_use]
pub fn help_html(text: &str) -> String {
    let parts: Vec<&str> = text.split('`').collect();
    let mut out = String::with_capacity(text.len() + 16);
    for (i, part) in parts.iter().enumerate() {
        let escaped = tera::escape_html(part);
        if i % 2 == 1 && i + 1 < parts.len() {
            out.push_str("<code>");
            out.push_str(&escaped);
            out.push_str("</code>");
        } else {
            if i % 2 == 1 {
                out.push('`');
            }
            out.push_str(&escaped);
        }
    }
    out
}

/// Register [`help_html`] as the `help_html` filter; its output is safe markup.
pub fn register_help_filter(tera: &mut Tera) {
    tera.register_filter(
        "help_html",
        |v: &serde_json::Value, _: &std::collections::HashMap<String, serde_json::Value>| {
            Ok(serde_json::Value::String(help_html(
                v.as_str().unwrap_or_default(),
            )))
        },
    );
}

/// [`html_tera`] loaded from `glob`, e.g. `"templates/**/*"`. A template
/// saved with a UTF-8 BOM is re-read without it, so the BOM is not rendered.
///
/// # Errors
/// The Tera error when a template does not parse or the glob is bad.
pub fn html_tera_from_glob(glob: &str) -> tera::Result<Tera> {
    let mut tera = Tera::new(glob)?;
    let with_bom: Vec<(String, String)> = tera
        .templates
        .values()
        .filter_map(|t| {
            let path = t.path.as_deref()?;
            let bytes = std::fs::read(path).ok()?;
            bytes.starts_with(&[0xEF, 0xBB, 0xBF]).then_some(())?;
            let text = crate::text_file::read(std::path::Path::new(path)).ok()?;
            Some((t.name.clone(), text))
        })
        .collect();
    if !with_bom.is_empty() {
        tera.add_raw_templates(with_bom)?;
    }
    tera.autoescape_on(vec![ESCAPE_EVERY_TEMPLATE]);
    Ok(tera)
}

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

    /// A template an editor saved with a UTF-8 BOM renders without it.
    #[test]
    fn a_bom_in_a_template_file_is_not_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(b"<p>{{ x }}</p>");
        std::fs::write(dir.path().join("page.html"), bytes).unwrap();
        let tera = html_tera_from_glob(&format!("{}/*.html", dir.path().display())).unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("x", "hi");
        assert_eq!(tera.render("page.html", &ctx).unwrap(), "<p>hi</p>");
    }

    #[test]
    fn help_html_escapes_and_marks_code_spans() {
        assert_eq!(
            help_html("Use `a<b>` & `c`."),
            "Use <code>a&lt;b&gt;</code> &amp; <code>c</code>."
        );
        assert_eq!(help_html("one ` tick"), "one ` tick");
        assert_eq!(help_html("plain"), "plain");
    }

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
    fn html_tera_escapes_templates_of_any_name() {
        let mut ctx = tera::Context::new();
        ctx.insert("x", "<script>");
        let mut tera = html_tera();
        for name in ["page", "a.tera", "a.j2", "a.html"] {
            tera.add_raw_template(name, "{{ x }}").unwrap();
            assert_eq!(tera.render(name, &ctx).unwrap(), "&lt;script&gt;", "{name}");
        }
        // Tera's own default leaves a `.tera` template raw (#1721).
        let mut bare = Tera::default();
        bare.add_raw_template("a.tera", "{{ x }}").unwrap();
        assert_eq!(bare.render("a.tera", &ctx).unwrap(), "<script>");
    }

    #[test]
    fn html_tera_from_glob_escapes_tera_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("p.tera"), "{{ x }}").unwrap();
        let tera = html_tera_from_glob(&format!("{}/**/*", dir.path().display())).unwrap();
        let mut ctx = tera::Context::new();
        ctx.insert("x", "<script>");
        assert_eq!(tera.render("p.tera", &ctx).unwrap(), "&lt;script&gt;");
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
