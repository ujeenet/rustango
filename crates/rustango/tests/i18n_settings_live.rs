//! Issue #403 — `Translator::from_settings(&I18nSettings)`
//! bootstraps a translator from TOML's `LANGUAGE_CODE` /
//! `LANGUAGES` / `LOCALE_PATHS` shape so deployments don't need to
//! instantiate the translator in code.

#![cfg(feature = "config")]

use rustango::config::I18nSettings;
use rustango::i18n::Translator;

fn tempdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let mut p = std::env::temp_dir();
    p.push(format!("rustango_i18n_settings_{pid}_{n}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write_catalog(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

#[test]
fn from_settings_with_empty_defaults_yields_translator_with_default_en() {
    let settings = I18nSettings::default();
    let t = Translator::from_settings(&settings).expect("default settings");
    // No catalog → key returned verbatim.
    assert_eq!(t.translate("en", "hello", &[]), "hello");
}

#[test]
fn from_settings_loads_catalogs_from_locale_paths() {
    let dir = tempdir();
    write_catalog(&dir, "en.json", r#"{"welcome": "Welcome, {name}!"}"#);
    write_catalog(&dir, "fr.json", r#"{"welcome": "Bienvenue, {name} !"}"#);

    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec![],
        locale_paths: vec![dir.to_string_lossy().to_string()],
        fallback_chain: vec![],
    };
    let t = Translator::from_settings(&settings).expect("load ok");
    assert_eq!(
        t.translate("en", "welcome", &[("name", "Alice")]),
        "Welcome, Alice!"
    );
    assert_eq!(
        t.translate("fr", "welcome", &[("name", "Alice")]),
        "Bienvenue, Alice !"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn from_settings_narrows_to_languages_allowlist() {
    let dir = tempdir();
    write_catalog(&dir, "en.json", r#"{"hi": "Hello"}"#);
    write_catalog(&dir, "fr.json", r#"{"hi": "Bonjour"}"#);
    write_catalog(&dir, "ja.json", r#"{"hi": "konnichiwa"}"#);

    // `languages = ["en", "fr"]` should NOT load ja.json.
    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec!["en".into(), "fr".into()],
        locale_paths: vec![dir.to_string_lossy().to_string()],
        fallback_chain: vec![],
    };
    let t = Translator::from_settings(&settings).expect("load ok");
    assert!(t.has_locale("en"));
    assert!(t.has_locale("fr"));
    assert!(
        !t.has_locale("ja"),
        "ja catalog must be filtered out by languages allowlist"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn from_settings_threads_fallback_chain() {
    let dir = tempdir();
    write_catalog(&dir, "en.json", r#"{"key": "EN"}"#);
    write_catalog(&dir, "pt.json", r#"{"key": "PT"}"#);
    // Note: no `fr.json` — the chain should fall through `fr → pt → en`.
    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec![],
        locale_paths: vec![dir.to_string_lossy().to_string()],
        fallback_chain: vec!["pt".into()],
    };
    let t = Translator::from_settings(&settings).expect("load ok");
    // fr isn't a registered locale; pt is the explicit fallback.
    assert_eq!(t.translate("fr", "key", &[]), "PT");
    // The configured chain is queryable.
    assert_eq!(t.fallback_chain().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn from_settings_default_locale_falls_back_to_en_when_unset() {
    let settings = I18nSettings::default();
    let t = Translator::from_settings(&settings).expect("default");
    // Empty catalog — the implicit default-locale terminus is still
    // "en" so default-locale lookups return the key verbatim, not
    // an error.
    assert_eq!(t.translate("anything", "missing", &[]), "missing");
}

#[test]
fn from_settings_io_error_on_missing_directory() {
    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec![],
        locale_paths: vec!["/this/path/does/not/exist/and/never/will".into()],
        fallback_chain: vec![],
    };
    // `Translator` doesn't implement Debug, so `.expect_err()`
    // doesn't apply; match on Err directly.
    match Translator::from_settings(&settings) {
        Ok(_) => panic!("missing dir should surface an error"),
        Err(rustango::i18n::I18nError::Io(_)) => {}
        Err(other) => panic!("expected I18nError::Io, got {other:?}"),
    }
}

/// `languages = ["pt-BR"]` admits `pt_BR.json`: both spell one locale (#2288).
#[test]
fn from_settings_allowlist_matches_normalised_locales() {
    let dir = tempdir();
    write_catalog(&dir, "pt_BR.json", r#"{"hi": "Olá"}"#);
    write_catalog(&dir, "en-gb.json", r#"{"hi": "Hello"}"#);
    write_catalog(&dir, "ja.json", r#"{"hi": "konnichiwa"}"#);

    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec!["pt-BR".into(), "en_GB".into()],
        locale_paths: vec![dir.to_string_lossy().to_string()],
        fallback_chain: vec![],
    };
    let t = Translator::from_settings(&settings).expect("load ok");
    assert_eq!(t.translate("pt-BR", "hi", &[]), "Olá");
    assert_eq!(t.translate("en-GB", "hi", &[]), "Hello");
    assert!(!t.has_locale("ja"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `xx-YY.json` and `xx_YY.json` are one locale: the first by name (`-`)
/// wins whatever order the directory lists them in. Several pairs, written
/// in both orders, so an unsorted read loses on any filesystem.
fn write_spelling_pairs(dir: &std::path::Path) -> Vec<&'static str> {
    let tags = [
        "pt-BR", "en-GB", "zh-CN", "fr-CA", "es-MX", "de-AT", "sr-RS", "nl-BE",
    ];
    for (i, tag) in tags.iter().enumerate() {
        let hyphen = (format!("{tag}.json"), r#"{"hi": "hyphen"}"#);
        let underscore = (
            format!("{}.json", tag.replace('-', "_")),
            r#"{"hi": "underscore"}"#,
        );
        let (a, b) = if i % 2 == 0 {
            (hyphen, underscore)
        } else {
            (underscore, hyphen)
        };
        write_catalog(dir, &a.0, a.1);
        write_catalog(dir, &b.0, b.1);
    }
    tags.to_vec()
}

#[test]
fn from_settings_keeps_the_first_of_two_spellings() {
    let dir = tempdir();
    let tags = write_spelling_pairs(&dir);
    let settings = I18nSettings {
        default_locale: Some("en".into()),
        languages: vec![],
        locale_paths: vec![dir.to_string_lossy().to_string()],
        fallback_chain: vec![],
    };
    let t = Translator::from_settings(&settings).expect("load ok");
    for tag in tags {
        assert_eq!(t.translate(tag, "hi", &[]), "hyphen", "{tag}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn from_directory_keeps_the_first_of_two_spellings() {
    let dir = tempdir();
    let tags = write_spelling_pairs(&dir);
    let t = Translator::from_directory(&dir, rustango::i18n::Locale::new("en")).expect("load ok");
    for tag in tags {
        assert_eq!(t.translate(tag, "hi", &[]), "hyphen", "{tag}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
