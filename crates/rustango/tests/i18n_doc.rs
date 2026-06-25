//! Backing test for `docs/i18n.md` — the `Translator` (catalogs, fallback,
//! `{name}` placeholders, pluralization) and Accept-Language negotiation.
//! Pure, no DB. (The locale/timezone *middleware* is covered in middleware.md.)
//!
//! Run: `cargo test -p rustango --test i18n_doc`

use std::collections::HashMap;

use rustango::i18n::{is_rtl_language, negotiate_language, Locale, Translator};

fn catalog(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

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
        .add_locale(Locale::new("fr"), catalog(&[("greeting", "Bonjour")]))
}

#[test]
fn gettext_looks_up_then_falls_back() {
    let t = translator();
    assert_eq!(t.gettext("en", "greeting"), "Hello");
    assert_eq!(t.gettext("fr", "greeting"), "Bonjour");

    // Missing key → returns the key itself (so the UI degrades, not panics).
    assert_eq!(t.gettext("en", "missing.key"), "missing.key");
    // Unknown locale → falls back to the default locale (en).
    assert_eq!(t.gettext("de", "greeting"), "Hello");
}

#[test]
fn base_language_fallback() {
    let t = translator();
    // "fr-CA" has no catalog → falls back to base language "fr".
    assert_eq!(t.gettext("fr-CA", "greeting"), "Bonjour");
}

#[test]
fn placeholders_are_substituted() {
    let t = translator();
    assert_eq!(
        t.translate("en", "welcome", &[("name", "Ada")]),
        "Welcome, Ada!"
    );
}

#[test]
fn pluralization_picks_singular_or_plural() {
    let t = translator();
    // count == 1 → the singular key; otherwise the plural, with {count} bound.
    assert_eq!(t.ngettext("en", "cart.one", "cart.other", 1), "1 item");
    assert_eq!(t.ngettext("en", "cart.one", "cart.other", 5), "5 items");
}

#[test]
fn accept_language_negotiation() {
    // Pick the best supported language from a browser Accept-Language header.
    assert_eq!(
        negotiate_language("fr-FR,fr;q=0.9,en;q=0.8", &["en", "fr"]).as_deref(),
        Some("fr")
    );
    // Nothing supported → None (caller uses its default).
    assert_eq!(negotiate_language("de,ja;q=0.5", &["en", "fr"]), None);
}

#[test]
fn rtl_detection() {
    assert!(is_rtl_language("ar")); // Arabic
    assert!(is_rtl_language("he")); // Hebrew
    assert!(!is_rtl_language("en"));
}
