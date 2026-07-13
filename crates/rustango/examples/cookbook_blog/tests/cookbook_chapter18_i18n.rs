//! Cookbook Chapter 18 — Internationalization (i18n).
//!
//! `rustango::i18n::Translator` is Django's `gettext` family in Rust: per-
//! locale message catalogs with base-language fallback, `{name}`
//! placeholders, and CLDR-correct pluralization. Plus `Accept-Language`
//! negotiation and RTL detection. All **in-process, no DB** — the DB
//! override layer + admin editor (#532) are a separate concern (see
//! `docs/i18n.md`).
//!
//! Run: `cargo test --test cookbook_chapter13_i18n`

use std::collections::HashMap;
use std::sync::Arc;

use rustango::i18n::{
    is_rtl_language, negotiate_language, plural_category, tera_tags, text_direction, Locale,
    Translator,
};

fn catalog(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// A translator with `en` (default) + `fr` catalogs.
fn translator() -> Translator {
    Translator::new(Locale::new("en"))
        .add_locale(
            Locale::new("en"),
            catalog(&[
                ("greeting", "Hello"),
                ("welcome", "Welcome, {name}!"),
                ("cart.one", "1 item"),
                ("cart.other", "{count} items"),
            ]),
        )
        .add_locale(
            Locale::new("fr"),
            catalog(&[("greeting", "Bonjour"), ("welcome", "Bienvenue, {name} !")]),
        )
}

// §18.140 — gettext lookup, missing-key + unknown-locale fallback.
#[test]
fn gettext_lookup_and_fallback() {
    let t = translator();
    assert_eq!(t.gettext("en", "greeting"), "Hello");
    assert_eq!(t.gettext("fr", "greeting"), "Bonjour");
    // Missing key → returns the key (UI degrades, never panics).
    assert_eq!(t.gettext("en", "nope.key"), "nope.key");
    // Unknown locale → default locale (en).
    assert_eq!(t.gettext("de", "greeting"), "Hello");
    // Regional locale with no catalog → base language: fr-CA → fr.
    assert_eq!(t.gettext("fr-CA", "greeting"), "Bonjour");
}

// §18.141 — `{name}` placeholder substitution via `translate`.
#[test]
fn placeholder_substitution() {
    let t = translator();
    assert_eq!(t.translate("en", "welcome", &[("name", "Ada")]), "Welcome, Ada!");
    assert_eq!(
        t.translate("fr", "welcome", &[("name", "Ada")]),
        "Bienvenue, Ada !"
    );
}

// §18.142 — ngettext (singular/plural) + CLDR-correct plural categories.
#[test]
fn pluralization() {
    let t = translator();
    // Simple singular/plural: count==1 → singular, else plural with {count}.
    assert_eq!(t.ngettext("en", "cart.one", "cart.other", 1), "1 item");
    assert_eq!(t.ngettext("en", "cart.one", "cart.other", 5), "5 items");

    // CLDR categories differ per language (drives `translate_plural`).
    assert_eq!(plural_category("en", 1), "one");
    assert_eq!(plural_category("en", 5), "other");
    assert_eq!(plural_category("pl", 2), "few"); // Polish 2–4 → few
    assert_eq!(plural_category("pl", 5), "many"); // 5+ → many

    // A per-category plural catalog: one key → one form per CLDR category.
    let mut pl: HashMap<String, HashMap<String, String>> = HashMap::new();
    pl.insert(
        "deleted".to_owned(),
        catalog(&[
            ("one", "Usunięto {count} stronę."),
            ("few", "Usunięto {count} strony."),
            ("many", "Usunięto {count} stron."),
        ]),
    );
    let t = Translator::new(Locale::new("en")).add_plural_locale(Locale::new("pl"), pl);
    let say = |n: i64| t.translate_plural("pl", "deleted", n, &[("count", &n.to_string())]);
    assert_eq!(say(1), "Usunięto 1 stronę."); // one
    assert_eq!(say(2), "Usunięto 2 strony."); // few
    assert_eq!(say(5), "Usunięto 5 stron."); // many
}

// §18.143 — Accept-Language negotiation picks the best supported language.
#[test]
fn accept_language_negotiation() {
    assert_eq!(
        negotiate_language("fr-FR,fr;q=0.9,en;q=0.8", &["en", "fr"]).as_deref(),
        Some("fr")
    );
    // Nothing supported → None (caller falls back to its default).
    assert_eq!(negotiate_language("de,ja;q=0.5", &["en", "fr"]), None);
}

// §18.144 — RTL detection + text direction (for `dir="rtl"` in templates).
#[test]
fn rtl_and_direction() {
    assert!(is_rtl_language("ar")); // Arabic
    assert!(is_rtl_language("he")); // Hebrew
    assert!(!is_rtl_language("en"));
    assert_eq!(text_direction("ar"), "rtl");
    assert_eq!(text_direction("en"), "ltr");
}

// §18.145 — the `{% translate %}` Tera tag (function form) renders through
// the same catalogs, driven by the active `LANG` locale.
#[test]
fn tera_translate_function() {
    let translator = Arc::new(translator());
    let mut tera = tera::Tera::default();
    tera_tags::register(&mut tera, Arc::clone(&translator));

    tera.add_raw_template(
        "t",
        "{{ translate(key='welcome', locale='fr', name='Ada') }}",
    )
    .unwrap();
    let out = tera.render("t", &tera::Context::new()).unwrap();
    assert_eq!(out, "Bienvenue, Ada !");
}
