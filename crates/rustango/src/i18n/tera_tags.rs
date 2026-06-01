//! Tera bindings for `rustango::i18n::Translator` — Django's
//! `{% translate %}` / `{% blocktranslate %}` template tags.
//! Issue #18 (partial).
//!
//! Tera doesn't support custom block tags (only filters + functions),
//! so the Django `{% blocktranslate %}…{% endblocktranslate %}` and
//! `{% language %}…{% endlanguage %}` block shapes can't be a 1:1
//! port. This module ships the filter/function shape, which covers
//! the common cases:
//!
//! ```ignore
//! use std::sync::Arc;
//! use rustango::i18n::{Translator, Locale};
//!
//! let translator = Arc::new(
//!     Translator::new(Locale::new("en"))
//!         .add_locale(Locale::new("en"), [
//!             ("welcome".to_owned(), "Welcome, {name}!".to_owned()),
//!         ].into())
//! );
//! let mut tera = tera::Tera::default();
//! rustango::i18n::tera_tags::register(&mut tera, Arc::clone(&translator));
//!
//! // Inject the active locale into the per-request context (typically
//! // done by middleware that reads Accept-Language).
//! let mut ctx = tera::Context::new();
//! ctx.insert("LANG", "en");
//!
//! // Now templates can call:
//! //   {{ translate(key="welcome", locale=LANG, name=user.name) }}
//! //   {{ "welcome" | translate(locale=LANG, name=user.name) }}
//! ```
//!
//! ## Convention: `LANG` / `TIME_ZONE` context keys
//!
//! By Django convention rustango uses the context keys `LANG` for
//! the active locale and `TIME_ZONE` for the active timezone. App
//! code (or `LocaleMiddleware` once it lands) injects them before
//! render; templates read them as plain variables — no special tag
//! needed for `{% get_current_language %}` etc.
//!
//! ## What's missing vs Django
//! - `{% blocktranslate %}…{% endblocktranslate %}` block form (Tera
//!   has no block-tag extension API).
//! - `{% plural %}` pluralization rules (no plural-rules table yet
//!   in `rustango::i18n`).
//! - `{% language 'fr' %}…{% endlanguage %}` override blocks.
//!
//! Workaround for blocks: precompute the translated string in
//! handler code and pass it through the context.

use std::collections::HashMap;
use std::sync::Arc;

use tera::{Tera, Value};

use super::Translator;

/// Register the i18n filter + function on `tera`. Both forms share
/// the same translator instance; clone it cheaply on each call via
/// the `Arc`.
pub fn register(tera: &mut Tera, translator: Arc<Translator>) {
    let t_fn = Arc::clone(&translator);
    tera.register_function(
        "translate",
        move |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let key = args
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| tera::Error::msg("translate(): missing required `key` argument"))?;
            Ok(Value::String(do_translate(&t_fn, key, args)))
        },
    );

    let t_filter = Arc::clone(&translator);
    tera.register_filter(
        "translate",
        move |value: &Value, args: &HashMap<String, Value>| -> tera::Result<Value> {
            let key = value.as_str().ok_or_else(|| {
                tera::Error::msg("translate filter: input must be a string (the translation key)")
            })?;
            Ok(Value::String(do_translate(&t_filter, key, args)))
        },
    );

    // #429 — RTL layout support. Django's `{% get_current_language_bidi %}`
    // returns `True` for RTL; rustango ships two shapes:
    //
    //   {{ get_text_direction(locale=LANG) }}    → "ltr" / "rtl"
    //   {{ is_rtl(locale=LANG) }}                → true / false
    //
    // Templates can then write:
    //   <html dir="{{ get_text_direction(locale=LANG) }}">
    //   {% if is_rtl(locale=LANG) %}<link rel="stylesheet" href="rtl.css">{% endif %}
    tera.register_function(
        "get_text_direction",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let locale = args.get("locale").and_then(Value::as_str).unwrap_or("");
            Ok(Value::String(super::text_direction(locale).to_owned()))
        },
    );
    tera.register_function(
        "is_rtl",
        |args: &HashMap<String, Value>| -> tera::Result<Value> {
            let locale = args.get("locale").and_then(Value::as_str).unwrap_or("");
            Ok(Value::Bool(super::is_rtl_language(locale)))
        },
    );
}

/// Shared body for the function + filter shapes — extract the locale
/// arg, collect every other string arg as an interpolation pair, and
/// dispatch to [`Translator::translate`]. Non-string args are
/// silently dropped (matches Django: only `string|string` placeholder
/// substitutions are supported).
fn do_translate(translator: &Translator, key: &str, args: &HashMap<String, Value>) -> String {
    let locale = args.get("locale").and_then(Value::as_str).unwrap_or("");
    // Collect string-valued interp args. Allocate owned strings up
    // front so the `&str` slice we pass into `translate` stays alive.
    let interp_owned: Vec<(String, String)> = args
        .iter()
        .filter(|(k, _)| k.as_str() != "key" && k.as_str() != "locale")
        .filter_map(|(k, v)| {
            // Strings pass through; numbers / bools render via Display
            // so `{{ translate(key=..., count=3) }}` interpolates as
            // "3" rather than failing.
            let rendered = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            Some((k.clone(), rendered))
        })
        .collect();
    let interp_refs: Vec<(&str, &str)> = interp_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    translator.translate(locale, key, &interp_refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use tera::Context;

    fn make_translator() -> Arc<Translator> {
        let mut en = HashMap::new();
        en.insert("welcome".to_owned(), "Welcome, {name}!".to_owned());
        en.insert(
            "items_count".to_owned(),
            "You have {count} items".to_owned(),
        );
        en.insert("plain".to_owned(), "Hello world".to_owned());
        let mut fr = HashMap::new();
        fr.insert("welcome".to_owned(), "Bienvenue, {name} !".to_owned());
        fr.insert("plain".to_owned(), "Bonjour le monde".to_owned());
        Arc::new(
            Translator::new(Locale::new("en"))
                .add_locale(Locale::new("en"), en)
                .add_locale(Locale::new("fr"), fr),
        )
    }

    fn render(template: &str, ctx: &Context, translator: Arc<Translator>) -> String {
        let mut tera = Tera::default();
        register(&mut tera, translator);
        tera.add_raw_template("t", template).unwrap();
        tera.render("t", ctx).unwrap()
    }

    #[test]
    fn translate_function_with_key_only() {
        let out = render(
            r#"{{ translate(key="plain", locale="en") }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn translate_function_with_interpolation() {
        let out = render(
            r#"{{ translate(key="welcome", locale="en", name="Alice") }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "Welcome, Alice!");
    }

    #[test]
    fn translate_function_picks_locale() {
        let out = render(
            r#"{{ translate(key="welcome", locale="fr", name="Alice") }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "Bienvenue, Alice !");
    }

    #[test]
    fn translate_function_falls_back_to_default_locale() {
        let out = render(
            r#"{{ translate(key="items_count", locale="fr", count=5) }}"#,
            &Context::new(),
            make_translator(),
        );
        // fr catalog has no `items_count` → falls back to en.
        assert_eq!(out, "You have 5 items");
    }

    #[test]
    fn translate_function_uses_lang_context_key() {
        let mut ctx = Context::new();
        ctx.insert("LANG", "fr");
        let out = render(
            r#"{{ translate(key="plain", locale=LANG) }}"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, "Bonjour le monde");
    }

    #[test]
    fn translate_filter_with_string_literal_key() {
        let out = render(
            r#"{{ "welcome" | translate(locale="en", name="Bob") }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "Welcome, Bob!");
    }

    #[test]
    fn translate_filter_with_variable_key_from_context() {
        let mut ctx = Context::new();
        ctx.insert("KEY", "plain");
        let out = render(
            r#"{{ KEY | translate(locale="en") }}"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn translate_function_with_numeric_arg_interpolates_display() {
        let out = render(
            r#"{{ translate(key="items_count", locale="en", count=42) }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "You have 42 items");
    }

    #[test]
    fn translate_function_returns_key_when_missing() {
        let out = render(
            r#"{{ translate(key="missing_key", locale="en") }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "missing_key");
    }

    // ---- #429 RTL helpers in templates ----

    #[test]
    fn get_text_direction_returns_rtl_for_arabic() {
        let mut ctx = Context::new();
        ctx.insert("LANG", "ar");
        let out = render(
            r#"<html dir="{{ get_text_direction(locale=LANG) }}">"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, r#"<html dir="rtl">"#);
    }

    #[test]
    fn get_text_direction_returns_ltr_for_english() {
        let mut ctx = Context::new();
        ctx.insert("LANG", "en");
        let out = render(
            r#"<html dir="{{ get_text_direction(locale=LANG) }}">"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, r#"<html dir="ltr">"#);
    }

    #[test]
    fn is_rtl_function_branches_on_locale() {
        let mut ctx = Context::new();
        ctx.insert("LANG", "he-IL");
        let out = render(
            r#"{% if is_rtl(locale=LANG) %}RTL{% else %}LTR{% endif %}"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, "RTL");

        ctx.insert("LANG", "en-US");
        let out = render(
            r#"{% if is_rtl(locale=LANG) %}RTL{% else %}LTR{% endif %}"#,
            &ctx,
            make_translator(),
        );
        assert_eq!(out, "LTR");
    }

    #[test]
    fn rtl_helpers_default_to_ltr_when_locale_missing() {
        // No `locale=` arg → blank → not RTL (LTR is the safe default).
        let out = render(
            r#"{{ get_text_direction() }}|{{ is_rtl() }}"#,
            &Context::new(),
            make_translator(),
        );
        assert_eq!(out, "ltr|false");
    }

    #[test]
    fn translate_function_errors_without_key() {
        let mut tera = Tera::default();
        register(&mut tera, make_translator());
        tera.add_raw_template("t", r#"{{ translate(locale="en") }}"#)
            .unwrap();
        let err = tera.render("t", &Context::new()).unwrap_err();
        // Tera wraps the function error in a "Failed to render" outer.
        // Walk the source chain to find our message about `key`.
        let mut chain: Vec<String> = vec![format!("{err}")];
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&err);
        while let Some(e) = src {
            chain.push(format!("{e}"));
            src = e.source();
        }
        let joined = chain.join(" | ");
        assert!(
            joined.contains("key"),
            "expected error chain to mention `key`, got: {joined}"
        );
    }
}
