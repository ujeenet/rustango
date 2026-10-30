//! Internationalization (i18n) — translation lookups + Accept-Language negotiation.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::i18n::{Translator, Locale};
//! use std::collections::HashMap;
//!
//! let mut en = HashMap::new();
//! en.insert("welcome".to_owned(), "Welcome, {name}!".to_owned());
//! let mut fr = HashMap::new();
//! fr.insert("welcome".to_owned(), "Bienvenue, {name} !".to_owned());
//!
//! let t = Translator::new(Locale::new("en"))
//!     .add_locale(Locale::new("en"), en)
//!     .add_locale(Locale::new("fr"), fr);
//!
//! let s = t.translate("fr", "welcome", &[("name", "Alice")]);
//! assert_eq!(s, "Bienvenue, Alice !");
//! ```
//!
//! ## Loading from JSON files
//!
//! ```ignore
//! // locales/en.json: {"welcome": "Hello, {name}"}
//! let t = Translator::from_directory("./locales", Locale::new("en"))?;
//! ```
//!
//! ## Accept-Language negotiation
//!
//! ```ignore
//! use rustango::i18n::negotiate_language;
//!
//! // Picks the best-matching locale present in the translator
//! let lang = negotiate_language("fr-FR,fr;q=0.9,en;q=0.8", &["en", "fr"]);
//! assert_eq!(lang.as_deref(), Some("fr"));
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

pub mod admin;
pub mod db;
// Both gated since #1208: `middleware` is a tower `Service`/`Layer` impl and
// `tera_tags` is a Tera integration, but `tower` and `tera` are optional
// dependencies. Left unconditional, they broke every feature set that didn't
// happen to pull those crates in. `middleware` also needs axum (#2329).
#[cfg(all(feature = "_axum", feature = "_tower"))]
pub mod middleware;
#[cfg(feature = "_tera")]
pub mod tera_tags;
pub mod timezone;

// ------------------------------------------------------------------ Locale

/// A locale identifier (e.g. `"en"`, `"en-US"`, `"fr"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale(String);

impl Locale {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        // `pt_BR` (a file stem) and `pt-BR` (a header) are one locale.
        Self(s.into().to_lowercase().replace('_', "-"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The base language portion of a locale: `"en-US"` → `"en"`.
    #[must_use]
    pub fn base_language(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }

    /// `true` for right-to-left scripts. The check is on the base
    /// language (`ar-EG` ≡ `ar`).
    ///
    /// Covered RTL families: Arabic (`ar`), Hebrew (`he`, plus its
    /// retired ISO code `iw`), Persian / Farsi (`fa`), Urdu (`ur`),
    /// Pashto (`ps`), Yiddish (`yi`, plus retired `ji`), Divehi /
    /// Dhivehi (`dv`), Sorani Kurdish (`ckb`), Uyghur (`ug`), Sindhi
    /// (`sd`), Aramaic / Syriac (`syr`). Everything else is LTR.
    #[must_use]
    pub fn is_rtl(&self) -> bool {
        is_rtl_base(self.base_language())
    }

    /// `"rtl"` for right-to-left scripts, `"ltr"` otherwise — the
    /// value you'd hand to an HTML `dir` attribute or CSS
    /// `direction` property.
    #[must_use]
    pub fn direction(&self) -> &'static str {
        if self.is_rtl() {
            "rtl"
        } else {
            "ltr"
        }
    }
}

/// `true` for an RTL base-language code. Public so callers that
/// only have a bare `&str` (e.g. a context-injected `LANG`) can
/// route through the same table without first allocating a
/// [`Locale`]. The check is case-insensitive and ignores any
/// region subtag.
#[must_use]
pub fn is_rtl_language(locale: &str) -> bool {
    is_rtl_base(Locale::new(locale).base_language())
}

/// `"rtl"` / `"ltr"` for a bare locale string — the bidi sibling of
/// [`is_rtl_language`].
#[must_use]
pub fn text_direction(locale: &str) -> &'static str {
    if is_rtl_language(locale) {
        "rtl"
    } else {
        "ltr"
    }
}

impl Locale {
    /// English display name for the base language — what an English
    /// speaker would call this locale. Returns the base-language code
    /// itself for unknown locales so callers always get *something*
    /// to render. Companion to [`Self::native_name`] for language
    /// picker UIs.
    ///
    /// Examples: `"en"` → `"English"`, `"fr-FR"` → `"French"`,
    /// `"zh-CN"` → `"Chinese"`, `"ar-EG"` → `"Arabic"`.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        language_display_name(self.base_language())
    }

    /// Native name for the base language — what a speaker of that
    /// language would call it. Returns the base-language code for
    /// unknown locales. Pair with [`Self::display_name`] for
    /// bidi-aware language pickers:
    ///
    /// ```ignore
    /// for code in ["en", "fr", "ja", "ar"] {
    ///     let loc = rustango::i18n::Locale::new(code);
    ///     println!("{} — {}", loc.native_name(), loc.display_name());
    /// }
    /// // English — English
    /// // français — French
    /// // 日本語 — Japanese
    /// // العربية — Arabic
    /// ```
    #[must_use]
    pub fn native_name(&self) -> &'static str {
        language_native_name(self.base_language())
    }
}

/// Canonical per-base-language display metadata: `(base code, English
/// name, native name)`. Single source of truth for
/// [`language_display_name`], [`language_native_name`], and
/// [`known_locales`]. Retired / variant subtags (`iw`→`he`, `nb`/`nn`→
/// `no`, `ji`→`yi`) resolve to their canonical row via `canonical_base`.
const LANGUAGE_NAMES: &[(&str, &str, &str)] = &[
    ("en", "English", "English"),
    ("fr", "French", "français"),
    ("de", "German", "Deutsch"),
    ("es", "Spanish", "español"),
    ("it", "Italian", "italiano"),
    ("pt", "Portuguese", "português"),
    ("nl", "Dutch", "Nederlands"),
    ("ru", "Russian", "русский"),
    ("ja", "Japanese", "日本語"),
    ("zh", "Chinese", "中文"),
    ("ko", "Korean", "한국어"),
    ("ar", "Arabic", "العربية"),
    ("he", "Hebrew", "עברית"),
    ("fa", "Persian", "فارسی"),
    ("ur", "Urdu", "اردو"),
    ("tr", "Turkish", "Türkçe"),
    ("pl", "Polish", "polski"),
    ("uk", "Ukrainian", "українська"),
    ("cs", "Czech", "čeština"),
    ("sk", "Slovak", "slovenčina"),
    ("hu", "Hungarian", "magyar"),
    ("ro", "Romanian", "română"),
    ("bg", "Bulgarian", "български"),
    ("el", "Greek", "Ελληνικά"),
    ("sv", "Swedish", "svenska"),
    ("no", "Norwegian", "norsk"),
    ("da", "Danish", "dansk"),
    ("fi", "Finnish", "suomi"),
    ("hi", "Hindi", "हिन्दी"),
    ("bn", "Bengali", "বাংলা"),
    ("ta", "Tamil", "தமிழ்"),
    ("th", "Thai", "ไทย"),
    ("vi", "Vietnamese", "Tiếng Việt"),
    ("id", "Indonesian", "Bahasa Indonesia"),
    ("ms", "Malay", "Bahasa Melayu"),
    ("ps", "Pashto", "پښتو"),
    ("yi", "Yiddish", "ייִדיש"),
    ("dv", "Divehi", "ދިވެހި"),
    ("ckb", "Sorani Kurdish", "کوردیی ناوەندی"),
    ("ug", "Uyghur", "ئۇيغۇرچە"),
    ("sd", "Sindhi", "سنڌي"),
    ("syr", "Syriac", "ܠܫܢܐ ܣܘܪܝܝܐ"),
];

/// Resolve retired / variant language subtags to the canonical base code
/// keyed in [`LANGUAGE_NAMES`].
fn canonical_base(base: &str) -> &str {
    match base {
        "iw" => "he",        // retired Hebrew alias
        "nb" | "nn" => "no", // Norwegian Bokmål / Nynorsk
        "ji" => "yi",        // retired Yiddish alias
        other => other,
    }
}

/// The [`LANGUAGE_NAMES`] row for a bare locale string (region-stripped,
/// alias-resolved), or `None` when core has no metadata for the language.
fn language_row(locale: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    let locale = Locale::new(locale);
    let base = canonical_base(locale.base_language());
    LANGUAGE_NAMES.iter().find(|(code, _, _)| *code == base)
}

/// English display name for a bare locale string — public so callers
/// holding an `Accept-Language`-shaped `&str` can render it without
/// constructing a [`Locale`]. Returns `"Unknown"` for locales core has
/// no metadata for.
#[must_use]
pub fn language_display_name(locale: &str) -> &'static str {
    language_row(locale).map_or("Unknown", |(_, en, _)| *en)
}

/// Native name for a bare locale string — sibling to
/// [`language_display_name`]. Returns `"Unknown"` for unknown locales.
#[must_use]
pub fn language_native_name(locale: &str) -> &'static str {
    language_row(locale).map_or("Unknown", |(_, _, native)| *native)
}

/// Every locale code core has display metadata for, as `(code, English
/// name)` — canonical base codes only (retired aliases excluded). Powers
/// "known locale" pickers / datalists so operators can pick codes core
/// fully supports; free-text codes remain valid content locales, just
/// without core metadata (see [`locale_info`]).
pub fn known_locales() -> impl Iterator<Item = (&'static str, &'static str)> {
    LANGUAGE_NAMES.iter().map(|(code, en, _)| (*code, *en))
}

/// Everything core statically knows about a locale, in one query — so a
/// caller (e.g. a CMS managing its own DB locale roster) can decide how
/// gracefully to degrade without string-comparing `"Unknown"` or
/// re-implementing the metadata tables.
///
/// `known` is `true` when core has display metadata (name/RTL);
/// `has_plural_rules` is `true` when the language hits a language-specific
/// CLDR rule (see [`plural_category_is_explicit`] — note this is `false`
/// for the generic English-style *one/other* rule, which is nonetheless
/// *correct* for English/German/Spanish/etc.). A locale can be a perfectly
/// usable content locale even when `known == false` — content translation
/// is code-agnostic; only chrome/plural/RTL degrade.
#[derive(Debug, Clone)]
pub struct LocaleInfo {
    /// The queried code, lowercased.
    pub code: String,
    /// Core has display/RTL metadata for this language.
    pub known: bool,
    /// English name, or `"Unknown"`.
    pub display_name: &'static str,
    /// Endonym, or `"Unknown"`.
    pub native_name: &'static str,
    /// `"ltr"` or `"rtl"`.
    pub direction: &'static str,
    /// Right-to-left script.
    pub is_rtl: bool,
    /// Core models a language-specific CLDR plural rule (not the generic
    /// one/other fallback).
    pub has_plural_rules: bool,
}

/// Query core's static knowledge of `code`. See [`LocaleInfo`].
#[must_use]
pub fn locale_info(code: &str) -> LocaleInfo {
    let display_name = language_display_name(code);
    LocaleInfo {
        code: code.to_ascii_lowercase(),
        known: display_name != "Unknown",
        display_name,
        native_name: language_native_name(code),
        direction: text_direction(code),
        is_rtl: is_rtl_language(code),
        has_plural_rules: plural_category_is_explicit(code),
    }
}

/// Lowercase, region-stripped base-language match against the
/// canonical RTL table. Inlined into `Locale::is_rtl` +
/// `is_rtl_language` so neither needs to allocate.
fn is_rtl_base(base: &str) -> bool {
    matches!(
        base,
        "ar"   // Arabic
        | "he" // Hebrew
        | "iw" // Hebrew (retired ISO 639-1 code, still emitted by some clients)
        | "fa" // Persian / Farsi
        | "ur" // Urdu
        | "ps" // Pashto
        | "yi" // Yiddish
        | "ji" // Yiddish (retired ISO 639-1 code)
        | "dv" // Divehi / Dhivehi
        | "ckb" // Sorani Kurdish
        | "ug" // Uyghur
        | "sd" // Sindhi
        | "syr" // Syriac / Aramaic
    )
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ------------------------------------------------------------------ I18nError

#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("io error: {0}")]
    Io(String),
    #[error("parse error in {file}: {detail}")]
    Parse { file: String, detail: String },
}

// ------------------------------------------------------------------ Translator

/// Translation backend — keyed by `(locale, key)`.
pub struct Translator {
    default_locale: Locale,
    /// Optional explicit fallback chain — checked AFTER the
    /// requested locale + its base language, BEFORE `default_locale`.
    /// Lowercased on insert.
    fallback_chain: Vec<Locale>,
    catalogs: RwLock<HashMap<Locale, HashMap<String, String>>>,
    /// DB-sourced override layer (#532). Same `locale → key → value`
    /// shape as `catalogs`, consulted before the file catalog at each
    /// locale of the lookup order, so operator edits persisted to
    /// `rustango_translations` win over the file-loaded defaults. Empty unless [`Self::load_overrides`] /
    /// [`Self::set_override`] is called (e.g. by
    /// [`crate::i18n::db::refresh_overrides_pool`]). Refreshing from the
    /// DB keeps `translate` synchronous — no per-render query.
    overrides: RwLock<HashMap<Locale, HashMap<String, String>>>,
    /// Plural catalogs (#1102): `locale → key → (CLDR category → template)`.
    /// Parallel to `catalogs`, consulted only by [`Self::translate_plural`], so
    /// the flat scalar path is untouched. Each entry holds the per-form variants
    /// (`one`/`few`/`many`/`other`) for one source key.
    plural_catalogs: RwLock<HashMap<Locale, HashMap<String, HashMap<String, String>>>>,
}

impl Translator {
    /// New translator with the given fallback locale.
    #[must_use]
    pub fn new(default_locale: Locale) -> Self {
        Self {
            default_locale,
            fallback_chain: Vec::new(),
            catalogs: RwLock::new(HashMap::new()),
            overrides: RwLock::new(HashMap::new()),
            plural_catalogs: RwLock::new(HashMap::new()),
        }
    }

    /// Set an explicit fallback chain. When
    /// `translate(locale, key, ...)` doesn't find a key in `locale`
    /// or its base language, the lookup walks `chain` in order
    /// before falling through to the default locale.
    ///
    /// ```ignore
    /// // Spanish missing? try Portuguese, then English (default).
    /// let t = Translator::new(Locale::new("en"))
    ///     .with_fallback_chain(&["pt"]);
    /// ```
    ///
    /// Duplicate entries and entries equal to `default_locale` are
    /// silently dropped (the default-locale check is already at the
    /// end of the chain).
    #[must_use]
    pub fn with_fallback_chain(mut self, chain: &[&str]) -> Self {
        let mut seen: std::collections::HashSet<Locale> = std::collections::HashSet::new();
        seen.insert(self.default_locale.clone());
        self.fallback_chain = chain
            .iter()
            .map(|s| Locale::new(*s))
            .filter(|loc| seen.insert(loc.clone()))
            .collect();
        self
    }

    /// Current explicit fallback chain (without the implicit
    /// default-locale terminus). Useful for diagnostics.
    #[must_use]
    pub fn fallback_chain(&self) -> &[Locale] {
        &self.fallback_chain
    }

    /// Add a translation catalog for `locale`. Replaces any existing
    /// catalog for the same locale.
    #[must_use]
    pub fn add_locale(self, locale: Locale, catalog: HashMap<String, String>) -> Self {
        self.catalogs
            .write()
            .expect("translator poisoned")
            .insert(locale, catalog);
        self
    }

    /// Mutably add a catalog (use with `let mut t = ...`).
    pub fn insert_locale(&self, locale: Locale, catalog: HashMap<String, String>) {
        self.catalogs
            .write()
            .expect("translator poisoned")
            .insert(locale, catalog);
    }

    /// Add a **plural** catalog for `locale` (#1102). Each key maps to a small
    /// object of CLDR plural-category → template, e.g.
    /// `{"one": "Deleted {count} page.", "other": "Deleted {count} pages."}`.
    /// Replaces any existing plural catalog for the same locale.
    #[must_use]
    pub fn add_plural_locale(
        self,
        locale: Locale,
        catalog: HashMap<String, HashMap<String, String>>,
    ) -> Self {
        self.plural_catalogs
            .write()
            .expect("translator poisoned")
            .insert(locale, catalog);
        self
    }

    /// Mutably add a plural catalog (use with `let mut t = ...`).
    pub fn insert_plural_locale(
        &self,
        locale: Locale,
        catalog: HashMap<String, HashMap<String, String>>,
    ) {
        self.plural_catalogs
            .write()
            .expect("translator poisoned")
            .insert(locale, catalog);
    }

    /// Look up `key` in `locale`'s catalog, walking the fallback
    /// chain in order: exact locale → base language → explicit
    /// `fallback_chain` (#425) → default locale → `key` itself. At each
    /// locale a DB override beats the file catalog.
    ///
    /// `params` substitutes `{name}` placeholders in the result.
    #[must_use]
    pub fn translate(&self, locale: &str, key: &str, params: &[(&str, &str)]) -> String {
        let template = self
            .resolve(&Locale::new(locale), |_| (key, None))
            .unwrap_or_else(|| key.to_owned());
        substitute(&template, params)
    }

    /// The one lookup order: each locale of exact → base language →
    /// `fallback_chain` → default, and at each one the DB override, then the
    /// plural entry (when a category is set), then the flat file entry.
    /// `lookup` names the key and plural category per locale, so a plural
    /// form follows the rule of the locale the text comes from.
    fn resolve<'k>(
        &self,
        req: &Locale,
        lookup: impl Fn(&Locale) -> (&'k str, Option<&'static str>),
    ) -> Option<String> {
        let ov = self.overrides.read().expect("translator poisoned");
        let cats = self.catalogs.read().expect("translator poisoned");
        let pc = self.plural_catalogs.read().expect("translator poisoned");
        // Within a plural entry: the exact category, then `other`; past that the
        // flat entry decides, never an arbitrary form in HashMap order.
        let pick = |entry: &HashMap<String, String>, category: &str| -> Option<String> {
            entry.get(category).or_else(|| entry.get("other")).cloned()
        };
        let base = Locale::new(req.base_language());
        let mut seen: Vec<&Locale> = Vec::with_capacity(self.fallback_chain.len() + 3);
        let order = [req, &base]
            .into_iter()
            .chain(&self.fallback_chain)
            .chain(std::iter::once(&self.default_locale));
        for loc in order {
            if seen.contains(&loc) {
                continue;
            }
            seen.push(loc);
            let (key, category) = lookup(loc);
            if let Some(v) = ov.get(loc).and_then(|c| c.get(key)) {
                return Some(v.clone());
            }
            if let Some(category) = category {
                if let Some(v) = pc
                    .get(loc)
                    .and_then(|c| c.get(key))
                    .and_then(|e| pick(e, category))
                {
                    return Some(v);
                }
            }
            if let Some(v) = cats.get(loc).and_then(|c| c.get(key)) {
                return Some(v.clone());
            }
        }
        None
    }

    /// Count-aware translation (#1102) — the `ngettext` shape. Picks the plural
    /// form for `n` via the CLDR rule ([`plural_category`]) of the locale the
    /// entry is found in (a fallback locale uses its own rule), then
    /// substitutes `params` (pass the count itself, e.g.
    /// `&[("count", &n.to_string())]`, so the chosen form's `{count}` fills in).
    ///
    /// Same locale order as [`Self::translate`]; at each locale a DB override
    /// wins, then the plural entry, then a flat entry. With none anywhere the
    /// source `key` renders, with `{count}` interpolated.
    ///
    /// ```ignore
    /// // en plural catalog: "Deleted {count} page(s)." =>
    /// //   {"one": "Deleted {count} page.", "other": "Deleted {count} pages."}
    /// let n = 3;
    /// t.translate_plural("en", "Deleted {count} page(s).", n, &[("count", &n.to_string())]);
    /// // => "Deleted 3 pages."
    /// ```
    #[must_use]
    pub fn translate_plural(
        &self,
        locale: &str,
        key: &str,
        n: i64,
        params: &[(&str, &str)],
    ) -> String {
        let template = self
            .resolve(&Locale::new(locale), |loc| {
                (key, Some(plural_category(loc.as_str(), n)))
            })
            .unwrap_or_else(|| key.to_owned());
        substitute(&template, params)
    }

    /// The `ngettext` pick: at each locale the key its own rule selects; with
    /// no entry anywhere, the source pair by the default locale's rule.
    fn ngettext_template(&self, locale: &str, singular: &str, plural: &str, count: i64) -> String {
        let pick = |loc: &Locale| {
            if plural_category(loc.as_str(), count) == "one" {
                singular
            } else {
                plural
            }
        };
        self.resolve(&Locale::new(locale), |loc| (pick(loc), None))
            .unwrap_or_else(|| pick(&self.default_locale).to_owned())
    }

    /// `true` when strings are registered for `locale` (or its base
    /// language) in **either** the file/programmatic catalogs or the DB
    /// override layer (#532) — so a locale supplied only via
    /// [`Self::load_overrides`] is reported as available too.
    #[must_use]
    pub fn has_locale(&self, locale: &str) -> bool {
        let req = Locale::new(locale);
        let base = Locale::new(req.base_language());
        let cats = self.catalogs.read().expect("translator poisoned");
        if cats.contains_key(&req) || cats.contains_key(&base) {
            return true;
        }
        let ov = self.overrides.read().expect("translator poisoned");
        ov.contains_key(&req) || ov.contains_key(&base)
    }

    /// All registered locale identifiers (for `negotiate_language`).
    #[must_use]
    pub fn locales(&self) -> Vec<String> {
        self.catalogs
            .read()
            .expect("translator poisoned")
            .keys()
            .map(|l| l.0.clone())
            .collect()
    }

    /// Every file-catalog entry as `(locale, key, value)` triples — used
    /// to seed the editable DB layer from the on-disk defaults (#532).
    #[must_use]
    pub fn entries(&self) -> Vec<(String, String, String)> {
        let cats = self.catalogs.read().expect("translator poisoned");
        let mut out = Vec::new();
        for (loc, catalog) in cats.iter() {
            for (k, v) in catalog {
                out.push((loc.0.clone(), k.clone(), v.clone()));
            }
        }
        out
    }

    /// Replace the entire DB-override layer (#532) with `rows`
    /// (`(locale, key, value)` triples — typically every
    /// `rustango_translations` row). Locale strings are normalized the
    /// same way as catalog locales. See
    /// [`crate::i18n::db::refresh_overrides_pool`].
    pub fn load_overrides(&self, rows: impl IntoIterator<Item = (String, String, String)>) {
        let mut map: HashMap<Locale, HashMap<String, String>> = HashMap::new();
        for (locale, key, value) in rows {
            map.entry(Locale::new(&locale))
                .or_default()
                .insert(key, value);
        }
        *self.overrides.write().expect("translator poisoned") = map;
    }

    /// Set a single override entry (#532) without a full reload — used by
    /// the admin edit path so a save takes effect immediately.
    pub fn set_override(&self, locale: &str, key: impl Into<String>, value: impl Into<String>) {
        self.overrides
            .write()
            .expect("translator poisoned")
            .entry(Locale::new(locale))
            .or_default()
            .insert(key.into(), value.into());
    }

    /// Drop every DB override, falling back to the file catalogs (#532).
    pub fn clear_overrides(&self) {
        self.overrides.write().expect("translator poisoned").clear();
    }

    /// Total number of override entries across all locales (#532) —
    /// handy for diagnostics and the admin coverage panel.
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.overrides
            .read()
            .expect("translator poisoned")
            .values()
            .map(HashMap::len)
            .sum()
    }

    /// Insert one file's flat and plural entries for `locale`.
    fn insert_catalog_file(&self, locale: Locale, path: &Path) -> Result<(), I18nError> {
        let (flat, plural) = read_catalog(path)?;
        self.insert_plural_locale(locale.clone(), plural);
        self.insert_locale(locale, flat);
        Ok(())
    }

    /// Load every `*.json` file in `dir` as a locale catalog. The file
    /// stem becomes the locale identifier (e.g. `en.json` → `Locale::new("en")`).
    /// Of two spellings of one locale (`pt_BR.json`, `pt-BR.json`) the first
    /// by name wins; the other is skipped with a warning.
    ///
    /// Each file is a JSON object; a value is a string, or a plural object
    /// of CLDR category → string (`{"one": "…", "other": "…"}`).
    ///
    /// # Errors
    /// [`I18nError::Io`] when the dir can't be read or a file is unreadable.
    /// [`I18nError::Parse`] when a JSON file is malformed.
    pub fn from_directory(dir: &Path, default_locale: Locale) -> Result<Self, I18nError> {
        let t = Translator::new(default_locale);
        for (locale, path) in catalog_files(dir)? {
            t.insert_catalog_file(locale, &path)?;
        }
        Ok(t)
    }

    /// Build a `Translator` from a [`crate::config::I18nSettings`].
    /// Reads each `locale_paths` entry as a directory of
    ///
    /// Gated behind the `config` feature since it consults the
    /// loader's typed sections.
    /// `<lang>.json` catalogs, narrows the active set to
    /// `languages` (if non-empty), and pins the default locale +
    /// fallback chain.
    ///
    /// ```toml
    /// # config/default.toml
    /// [i18n]
    /// default_locale = "en"
    /// languages = ["en", "fr", "es"]
    /// locale_paths = ["locales", "vendor/locales"]
    /// fallback_chain = ["en"]
    /// ```
    ///
    /// ```ignore
    /// let settings = rustango::config::load()?;
    /// let translator = rustango::i18n::Translator::from_settings(&settings.i18n)?;
    /// ```
    ///
    /// Defaults when fields are unset:
    /// - `default_locale = None` → `"en"`.
    /// - `languages = []` → every catalog discovered via `locale_paths` stays active.
    /// - `languages` and file stems match as [`Locale`]s, so `pt-BR` loads
    ///   `pt_BR.json`; a second spelling in one directory is skipped.
    /// - `locale_paths = []` → no directory scan; resulting translator has
    ///   no registered catalogs (apps that build catalogs programmatically
    ///   via `add_locale` skip the TOML path entirely).
    /// - `fallback_chain = []` → only the implicit
    ///   `exact-locale → base-language → default-locale → key`
    ///   chain applies.
    ///
    /// # Errors
    /// [`I18nError::Io`] / [`I18nError::Parse`] forwarded from any
    /// `locale_paths` entry that fails to load. A missing directory
    /// is treated as a hard error so deployment mistakes surface
    /// immediately rather than silently producing an empty
    /// translator.
    #[cfg(feature = "config")]
    pub fn from_settings(settings: &crate::config::I18nSettings) -> Result<Self, I18nError> {
        let default = settings.default_locale.as_deref().unwrap_or("en");
        let mut t = Translator::new(Locale::new(default));

        // Build the active-language allowlist. Empty `languages`
        // means "no narrowing"; non-empty means we only insert
        // catalogs whose stem matches one of the entries. Both sides are
        // `Locale`s, so `pt-BR` admits `pt_BR.json` (#2288).
        let allowlist: Option<std::collections::HashSet<Locale>> = if settings.languages.is_empty()
        {
            None
        } else {
            Some(settings.languages.iter().map(Locale::new).collect())
        };

        for raw_path in &settings.locale_paths {
            let path = std::path::Path::new(raw_path);
            for (locale, p) in catalog_files(path)? {
                if let Some(allow) = &allowlist {
                    if !allow.contains(&locale) {
                        continue;
                    }
                }
                t.insert_catalog_file(locale, &p)?;
            }
        }

        if !settings.fallback_chain.is_empty() {
            let chain: Vec<&str> = settings.fallback_chain.iter().map(String::as_str).collect();
            t = t.with_fallback_chain(&chain);
        }

        Ok(t)
    }

    /// gettext-shape alias for [`Self::translate`]: look up the key
    /// in the supplied locale and return the translated string, or
    /// the key itself when no translation is found. This is the raw
    /// lookup, with no parameter substitution.
    ///
    /// For interpolation use [`Self::translate`] or
    /// [`Self::gettext_fmt`], which accepts a placeholder map.
    #[must_use]
    pub fn gettext(&self, locale: &str, key: &str) -> String {
        self.translate(locale, key, &[])
    }

    /// gettext-shape lookup with placeholder substitution. Same
    /// substitution rules as [`Self::translate`] (`{name}` →
    /// `params["name"]`): `gettext_fmt(locale, "Hi, {name}",
    /// &[("name", &user.name)])`.
    #[must_use]
    pub fn gettext_fmt(&self, locale: &str, key: &str, params: &[(&str, &str)]) -> String {
        self.translate(locale, key, params)
    }

    /// gettext-shape `pgettext(context, message)` — context-
    /// disambiguated translation. Identical keys with different
    /// contexts can resolve to different translations. Issue #422.
    ///
    /// Catalog key format: `"<context>\u{4}<message>"` (the same
    /// `` (`EOT`) delimiter gettext uses internally). When
    /// no context-prefixed entry exists, falls back to the bare
    /// `message` lookup so the function degrades gracefully on
    /// catalogs that haven't yet been authored with contexts.
    #[must_use]
    pub fn pgettext(&self, locale: &str, context: &str, key: &str) -> String {
        let composite = format!("{context}\u{0004}{key}");
        let translated = self.translate(locale, &composite, &[]);
        // `translate` returns the key itself on miss — detect that
        // and fall through to the bare-key lookup so callers don't
        // see the raw `contextmessage` string when the
        // context-prefixed entry isn't in the catalog yet.
        if translated == composite {
            return self.translate(locale, key, &[]);
        }
        translated
    }

    /// gettext-shape `pgettext` with placeholder substitution.
    /// Mirrors [`Self::gettext_fmt`] semantics.
    #[must_use]
    pub fn pgettext_fmt(
        &self,
        locale: &str,
        context: &str,
        key: &str,
        params: &[(&str, &str)],
    ) -> String {
        let raw = self.pgettext(locale, context, key);
        substitute(&raw, params)
    }

    /// gettext-shape `ngettext(singular, plural, count)` —
    /// pluralization. Returns `singular` when `count == 1`,
    /// `plural` otherwise.
    ///
    /// Catalog format mirrors gettext's `msgid_plural` convention:
    /// register two keys, one for each form. The fallback chain is
    /// the same as [`Self::translate`]; missing entries fall back
    /// to the supplied source strings.
    ///
    /// The plural form is selected by the CLDR rule ([`plural_category`]) of
    /// the locale the entry is found in, or of the default locale when the
    /// source strings render: the `"one"` category picks `singular`, every
    /// other category picks `plural`. This is correct for two-form languages
    /// (en/de: `n == 1`; fr: `0` and `1` are singular). Languages with three+
    /// forms (Polish/Ukrainian `few`/`many`, Arabic's six) can't be fully
    /// expressed with a singular/plural pair — use
    /// [`Self::translate_plural`] with a per-category catalog for those (#1102).
    ///
    /// `count` is bound to the `{count}` placeholder automatically
    /// so templates can read `"You have {count} unread messages"`
    /// without the caller threading it in.
    #[must_use]
    pub fn ngettext(&self, locale: &str, singular: &str, plural: &str, count: i64) -> String {
        self.ngettext_fmt(locale, singular, plural, count, &[])
    }

    /// Pluralization with additional placeholder substitution.
    /// `{count}` is bound from the `count` argument; everything in
    /// `params` overlays on top so callers can pass extra keys
    /// (`{name}`, `{item}`, etc.).
    #[must_use]
    pub fn ngettext_fmt(
        &self,
        locale: &str,
        singular: &str,
        plural: &str,
        count: i64,
        params: &[(&str, &str)],
    ) -> String {
        let template = self.ngettext_template(locale, singular, plural, count);
        let count_str = count.to_string();
        let mut all: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 1);
        all.push(("count", &count_str));
        all.extend_from_slice(params);
        substitute(&template, &all)
    }
}

/// Substitute `{name}` placeholders in `template` with values from `params`.
/// One pass over the template, so an inserted value is never re-scanned; the
/// last pair with a name wins.
fn substitute(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            params
                .iter()
                .rev()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (*v, close))
        });
        match value {
            Some((v, close)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// CLDR cardinal plural category for integer `n` in `locale` (#1102) — one of
/// `"zero"`, `"one"`, `"two"`, `"few"`, `"many"`, `"other"`. Drives [`Translator::translate_plural`]
/// (and the binary [`Translator::ngettext`], which maps `one`→singular).
///
/// Selection is on the base language (`pl-PL` ≡ `pl`) and the absolute value of
/// `n`. The launch set + common European/East-Asian languages are modeled;
/// unknown locales use the English one/other rule. Fraction-only categories
/// aren't modeled — admin counts are whole numbers.
///
/// ```
/// use rustango::i18n::plural_category;
/// assert_eq!(plural_category("en", 1), "one");
/// assert_eq!(plural_category("en", 5), "other");
/// assert_eq!(plural_category("fr", 0), "one");        // French: 0 is "one"
/// assert_eq!(plural_category("pl", 2), "few");        // Polish three-form
/// assert_eq!(plural_category("pl", 5), "many");
/// assert_eq!(plural_category("uk", 21), "one");       // Ukrainian: …1 (not 11)
/// assert_eq!(plural_category("ja", 7), "other");      // no count distinction
/// ```
/// The CLDR plural *family* a base language belongs to, or `None` for the
/// generic one/other rule (Germanic / Romance / everything unmodeled).
/// Extracted so [`plural_category`] and [`plural_category_is_explicit`]
/// share one classification and can't drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PluralFamily {
    /// No count-based form distinction (always `other`).
    NoDistinction,
    /// Explicit one (1) / other, where the base language is French-style.
    OneOther,
    /// French / Brazilian-Portuguese: 0 and 1 are `one`.
    FrenchStyle,
    /// West-Slavic (Polish): one / few / many.
    WestSlavic,
    /// East-Slavic (Ukrainian, Russian, Belarusian): one / few / many.
    EastSlavic,
    /// Czech / Slovak: one / few (2–4) / other.
    CzechSlovak,
    /// Arabic: zero / one / two / few / many / other.
    Arabic,
    /// Lithuanian: one / few / other.
    Lithuanian,
    /// Romanian: one (1) / few (0, or n % 100 in 1..=19 except 1) / other.
    Romanian,
    /// Hebrew: one / two / other.
    Hebrew,
    /// Slovenian: one / two / few by the last two digits.
    Slovenian,
}

fn plural_family(locale: &Locale) -> Option<PluralFamily> {
    // European Portuguese counts 0 as plural, unlike `pt` / `pt-BR`.
    if locale.as_str() == "pt-pt" {
        return Some(PluralFamily::OneOther);
    }
    match locale.base_language() {
        "zh" | "ja" | "ko" | "th" | "vi" | "id" | "ms" | "lo" | "km" | "my" => {
            Some(PluralFamily::NoDistinction)
        }
        "fr" | "pt" | "ff" | "hy" | "kab" => Some(PluralFamily::FrenchStyle),
        "pl" => Some(PluralFamily::WestSlavic),
        "uk" | "ru" | "be" => Some(PluralFamily::EastSlavic),
        "cs" | "sk" => Some(PluralFamily::CzechSlovak),
        "ar" => Some(PluralFamily::Arabic),
        "lt" => Some(PluralFamily::Lithuanian),
        "ro" | "mo" => Some(PluralFamily::Romanian),
        "he" | "iw" => Some(PluralFamily::Hebrew),
        "sl" => Some(PluralFamily::Slovenian),
        _ => None,
    }
}

#[must_use]
pub fn plural_category(locale: &str, n: i64) -> &'static str {
    let locale = Locale::new(locale);
    let n = n.unsigned_abs();
    let r10 = n % 10;
    let r100 = n % 100;
    match plural_family(&locale) {
        Some(PluralFamily::NoDistinction) => "other",
        Some(PluralFamily::OneOther) => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
        Some(PluralFamily::FrenchStyle) => {
            if n == 0 || n == 1 {
                "one"
            } else {
                "other"
            }
        }
        Some(PluralFamily::WestSlavic) => {
            if n == 1 {
                "one"
            } else if (2..=4).contains(&r10) && !(12..=14).contains(&r100) {
                "few"
            } else {
                "many"
            }
        }
        Some(PluralFamily::EastSlavic) => {
            if r10 == 1 && r100 != 11 {
                "one"
            } else if (2..=4).contains(&r10) && !(12..=14).contains(&r100) {
                "few"
            } else {
                "many"
            }
        }
        Some(PluralFamily::CzechSlovak) => match n {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        Some(PluralFamily::Arabic) => match (n, r100) {
            (0, _) => "zero",
            (1, _) => "one",
            (2, _) => "two",
            (_, 3..=10) => "few",
            (_, 11..=99) => "many",
            _ => "other",
        },
        Some(PluralFamily::Lithuanian) => {
            if (11..=19).contains(&r100) {
                "other"
            } else if r10 == 1 {
                "one"
            } else if r10 >= 2 {
                "few"
            } else {
                "other"
            }
        }
        Some(PluralFamily::Romanian) => {
            if n == 1 {
                "one"
            } else if n == 0 || (1..=19).contains(&r100) {
                "few"
            } else {
                "other"
            }
        }
        Some(PluralFamily::Hebrew) => match n {
            1 => "one",
            2 => "two",
            _ => "other",
        },
        Some(PluralFamily::Slovenian) => match r100 {
            1 => "one",
            2 => "two",
            3 | 4 => "few",
            _ => "other",
        },
        // Germanic / Romance / default: one / other (n == 1 → one).
        None => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// `true` when core models a *language-specific* CLDR plural rule for
/// `locale` (French-style, West/East-Slavic, or no-distinction) — as
/// opposed to the generic English one/other fallback. Note: this returns
/// `false` for English/German/Spanish/etc., for which one/other is
/// nonetheless correct; callers surfacing it should frame the fallback as
/// "basic one/other", not an error. Backs [`LocaleInfo::has_plural_rules`].
#[must_use]
pub fn plural_category_is_explicit(locale: &str) -> bool {
    plural_family(&Locale::new(locale)).is_some()
}

/// `dir`'s `*.json` catalogs by locale, sorted by file name. A second
/// spelling of one locale (`pt_BR` after `pt-BR`) is skipped with a warning.
fn catalog_files(dir: &Path) -> Result<Vec<(Locale, std::path::PathBuf)>, I18nError> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| I18nError::Io(format!("{}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut out: Vec<(Locale, std::path::PathBuf)> = Vec::new();
    for p in files {
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let locale = Locale::new(stem);
        if out.iter().any(|(l, _)| *l == locale) {
            tracing::warn!(
                file = %p.display(),
                locale = locale.as_str(),
                "i18n: duplicate catalog for one locale in the same directory, ignored"
            );
            continue;
        }
        out.push((locale, p));
    }
    Ok(out)
}

/// One catalog entry: a string, or a plural object of category → string.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum CatalogEntry {
    Flat(String),
    Plural(HashMap<String, String>),
}

type PluralCatalog = HashMap<String, HashMap<String, String>>;

/// One JSON catalog, split into its flat and plural entries.
fn read_catalog(path: &Path) -> Result<(HashMap<String, String>, PluralCatalog), I18nError> {
    let raw = std::fs::read_to_string(path).map_err(|e| I18nError::Io(e.to_string()))?;
    let entries: HashMap<String, CatalogEntry> =
        serde_json::from_str(&raw).map_err(|e| I18nError::Parse {
            file: path.display().to_string(),
            detail: e.to_string(),
        })?;
    let mut flat = HashMap::new();
    let mut plural = HashMap::new();
    for (key, entry) in entries {
        match entry {
            CatalogEntry::Flat(v) => {
                flat.insert(key, v);
            }
            CatalogEntry::Plural(forms) => {
                plural.insert(key, forms);
            }
        }
    }
    Ok((flat, plural))
}

// ------------------------------------------------------------------ Accept-Language negotiation

/// Pick the best-matching language from `Accept-Language`.
///
/// `accept_language` is the raw header value (e.g. `"fr-FR,fr;q=0.9,en;q=0.8"`).
/// `available` is the list of locales the app supports.
///
/// A bare base language beats a sibling region: `en-US` picks `en` over `en-GB`.
///
/// Returns the best match or `None` if no acceptable language is supported.
#[must_use]
pub fn negotiate_language<S: AsRef<str>>(accept_language: &str, available: &[S]) -> Option<String> {
    let mut prefs = parse_accept_language(accept_language);
    // Quality desc; stable, so equal q keeps header order.
    prefs.sort_by_key(|p| std::cmp::Reverse(p.1));

    let avail_lower: Vec<String> = available
        .iter()
        .map(|s| Locale::new(s.as_ref()).0)
        .collect();

    // `q=0` means "not acceptable" (RFC 9110 §12.4.2).
    for (lang, _q) in prefs.into_iter().filter(|(_, q)| *q > 0) {
        let lang_lower = Locale::new(lang).0;
        // Exact match
        if let Some(matched) = avail_lower.iter().find(|a| **a == lang_lower) {
            return Some(matched.clone());
        }
        // The bare base language beats a sibling region: `en-US` takes `en`
        // over `en-GB` (#2289).
        let base = lang_lower.split('-').next().unwrap_or(&lang_lower);
        let sibling = format!("{base}-");
        if let Some(matched) = avail_lower
            .iter()
            .find(|a| **a == base)
            .or_else(|| avail_lower.iter().find(|a| a.starts_with(&sibling)))
        {
            return Some(matched.clone());
        }
    }
    None
}

/// `(tag, q)` with q in thousandths, so a hostile `q=NaN` cannot break the sort.
fn parse_accept_language(header: &str) -> Vec<(String, u16)> {
    header
        .split(',')
        .filter_map(|raw| {
            let mut parts = raw.split(';').map(str::trim);
            let lang = parts.next()?.to_owned();
            if lang.is_empty() {
                return None;
            }
            let mut q = 1000;
            for kv in parts {
                if let Some(rest) = kv.strip_prefix("q=") {
                    // A non-finite q is as unusable as an unparsable one.
                    if let Some(parsed) = rest.parse::<f32>().ok().filter(|p| p.is_finite()) {
                        q = (parsed.clamp(0.0, 1.0) * 1000.0).round() as u16;
                    }
                }
            }
            Some((lang, q))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_translator() -> Translator {
        let mut en = HashMap::new();
        en.insert("hello".into(), "Hello".into());
        en.insert("greet".into(), "Hi, {name}!".into());
        let mut fr = HashMap::new();
        fr.insert("hello".into(), "Bonjour".into());
        fr.insert("greet".into(), "Salut, {name} !".into());

        Translator::new(Locale::new("en"))
            .add_locale(Locale::new("en"), en)
            .add_locale(Locale::new("fr"), fr)
    }

    #[test]
    fn translate_basic() {
        let t = make_translator();
        assert_eq!(t.translate("en", "hello", &[]), "Hello");
        assert_eq!(t.translate("fr", "hello", &[]), "Bonjour");
    }

    #[test]
    fn translate_substitutes_params() {
        let t = make_translator();
        assert_eq!(
            t.translate("en", "greet", &[("name", "Alice")]),
            "Hi, Alice!"
        );
        assert_eq!(
            t.translate("fr", "greet", &[("name", "Alice")]),
            "Salut, Alice !"
        );
    }

    #[test]
    fn unknown_locale_falls_back_to_default() {
        let t = make_translator();
        assert_eq!(t.translate("ja", "hello", &[]), "Hello");
    }

    #[test]
    fn unknown_key_returns_key_itself() {
        let t = make_translator();
        assert_eq!(t.translate("en", "unknown.key", &[]), "unknown.key");
    }

    #[test]
    fn region_falls_back_to_base_language() {
        let t = make_translator();
        // "fr-FR" → falls back to "fr"
        assert_eq!(t.translate("fr-FR", "hello", &[]), "Bonjour");
    }

    #[test]
    fn has_locale_with_base_match() {
        let t = make_translator();
        assert!(t.has_locale("en"));
        assert!(t.has_locale("fr"));
        assert!(t.has_locale("en-US"));
        assert!(!t.has_locale("ja"));
    }

    /// Issue #425 — explicit fallback chain. Build a translator where
    /// only Spanish is missing the key; it should walk pt → en (default).
    #[test]
    fn fallback_chain_walks_in_order() {
        let mut es: HashMap<String, String> = HashMap::new();
        es.insert("welcome".into(), "Bienvenido".into());
        // No `hello` in `es`.
        let mut pt: HashMap<String, String> = HashMap::new();
        pt.insert("hello".into(), "Olá".into());
        let mut en: HashMap<String, String> = HashMap::new();
        en.insert("hello".into(), "Hello".into());

        let t = Translator::new(Locale::new("en"))
            .with_fallback_chain(&["pt"])
            .add_locale(Locale::new("es"), es)
            .add_locale(Locale::new("pt"), pt)
            .add_locale(Locale::new("en"), en);

        // `welcome` exists in `es` — direct hit.
        assert_eq!(t.translate("es", "welcome", &[]), "Bienvenido");
        // `hello` is missing in `es`; chain says try `pt` next.
        assert_eq!(t.translate("es", "hello", &[]), "Olá");
    }

    /// Issue #425 — default locale is still the last terminus when
    /// nothing in the chain has the key.
    #[test]
    fn fallback_chain_then_default() {
        let mut en: HashMap<String, String> = HashMap::new();
        en.insert("hello".into(), "Hello".into());
        let pt: HashMap<String, String> = HashMap::new();

        let t = Translator::new(Locale::new("en"))
            .with_fallback_chain(&["pt"])
            .add_locale(Locale::new("pt"), pt)
            .add_locale(Locale::new("en"), en);

        // `es` not in catalogs, chain entry `pt` doesn't have the key,
        // default `en` does.
        assert_eq!(t.translate("es", "hello", &[]), "Hello");
    }

    /// Issue #425 — chain accessor returns what was set, sans duplicates
    /// and sans the default-locale entry.
    #[test]
    fn fallback_chain_accessor_deduplicates() {
        let t = Translator::new(Locale::new("en")).with_fallback_chain(&["pt", "pt", "en", "es"]);
        let chain: Vec<&str> = t.fallback_chain().iter().map(Locale::as_str).collect();
        assert_eq!(chain, vec!["pt", "es"]);
    }

    #[test]
    fn locales_lists_registered() {
        let t = make_translator();
        let mut locales = t.locales();
        locales.sort();
        assert_eq!(locales, vec!["en".to_string(), "fr".to_string()]);
    }

    // ---- #429 RTL detection ----

    #[test]
    fn locale_is_rtl_for_arabic_hebrew_persian_urdu() {
        for code in ["ar", "he", "fa", "ur", "ps", "yi", "dv", "ckb", "ug", "sd"] {
            let loc = Locale::new(code);
            assert!(loc.is_rtl(), "{code} should be RTL");
            assert_eq!(loc.direction(), "rtl", "{code} direction should be rtl");
        }
    }

    #[test]
    fn locale_is_ltr_for_western_and_cjk() {
        for code in ["en", "fr", "de", "es", "ja", "zh", "ko", "ru", "tr", "pt"] {
            let loc = Locale::new(code);
            assert!(!loc.is_rtl(), "{code} should be LTR");
            assert_eq!(loc.direction(), "ltr", "{code} direction should be ltr");
        }
    }

    #[test]
    fn rtl_check_uses_base_language_for_region_subtag() {
        // Region-specific Arabic / Hebrew variants still RTL.
        assert!(Locale::new("ar-EG").is_rtl());
        assert!(Locale::new("AR-SA").is_rtl()); // case-insensitive ctor
        assert!(Locale::new("he-IL").is_rtl());
        // Region-specific LTR locales stay LTR.
        assert!(!Locale::new("en-US").is_rtl());
        assert!(!Locale::new("pt-BR").is_rtl());
    }

    #[test]
    fn rtl_check_handles_retired_iso_codes() {
        // Some Accept-Language headers still emit the old ISO 639-1 codes.
        assert!(Locale::new("iw").is_rtl()); // Hebrew (retired alias)
        assert!(Locale::new("ji").is_rtl()); // Yiddish (retired alias)
    }

    // ---- Language display/native names ----

    #[test]
    fn display_name_returns_english_label_for_known_locales() {
        assert_eq!(Locale::new("en").display_name(), "English");
        assert_eq!(Locale::new("fr-FR").display_name(), "French");
        assert_eq!(Locale::new("zh-CN").display_name(), "Chinese");
        assert_eq!(Locale::new("ar-EG").display_name(), "Arabic");
        assert_eq!(Locale::new("he-IL").display_name(), "Hebrew");
        assert_eq!(Locale::new("iw").display_name(), "Hebrew"); // retired alias
    }

    #[test]
    fn native_name_returns_endonym_for_known_locales() {
        assert_eq!(Locale::new("en").native_name(), "English");
        assert_eq!(Locale::new("fr-CA").native_name(), "français");
        assert_eq!(Locale::new("ja").native_name(), "日本語");
        assert_eq!(Locale::new("zh-TW").native_name(), "中文");
        assert_eq!(Locale::new("ar").native_name(), "العربية");
        assert_eq!(Locale::new("he").native_name(), "עברית");
    }

    #[test]
    fn unknown_locale_falls_back_to_unknown_label() {
        assert_eq!(Locale::new("xx").display_name(), "Unknown");
        assert_eq!(Locale::new("xx").native_name(), "Unknown");
    }

    #[test]
    fn norwegian_variants_share_one_label() {
        // Norwegian Bokmål / Nynorsk should collapse to one label.
        for code in ["no", "nb", "nn", "nb-NO", "nn-NO"] {
            assert_eq!(Locale::new(code).display_name(), "Norwegian", "{code}");
            assert_eq!(Locale::new(code).native_name(), "norsk", "{code}");
        }
    }

    #[test]
    fn locale_info_summarizes_core_support() {
        let fr = locale_info("fr-CA");
        assert!(fr.known);
        assert_eq!(fr.display_name, "French");
        assert!(!fr.is_rtl && fr.direction == "ltr");
        assert!(fr.has_plural_rules); // French-style family

        let ar = locale_info("ar");
        assert!(ar.known && ar.is_rtl && ar.direction == "rtl");

        let en = locale_info("EN");
        assert_eq!(en.code, "en"); // lowercased
        assert!(en.known && !en.has_plural_rules); // generic one/other

        let xx = locale_info("xx");
        assert!(!xx.known && xx.display_name == "Unknown" && !xx.is_rtl && !xx.has_plural_rules);
    }

    #[test]
    fn known_locales_lists_canonical_codes_only() {
        let codes: Vec<&str> = known_locales().map(|(c, _)| c).collect();
        assert!(codes.contains(&"en") && codes.contains(&"zh") && codes.contains(&"ar"));
        // Retired / variant aliases are excluded from the picker set …
        assert!(!codes.contains(&"iw") && !codes.contains(&"nb") && !codes.contains(&"ji"));
        // … but still resolve through the name lookups.
        assert_eq!(language_display_name("iw"), "Hebrew");
    }

    #[test]
    fn plural_explicit_flag_matches_families() {
        for c in ["pl", "uk", "ru", "zh-Hans", "fr", "pt-BR", "ja"] {
            assert!(
                plural_category_is_explicit(c),
                "{c} has a language-specific rule"
            );
        }
        for c in ["en", "de", "es", "xx"] {
            assert!(!plural_category_is_explicit(c), "{c} uses the generic rule");
        }
    }

    #[test]
    fn bare_string_helpers_match_locale_methods() {
        for code in ["ar", "fa-IR", "he-IL", "iw"] {
            assert!(is_rtl_language(code), "{code} should be RTL");
            assert_eq!(text_direction(code), "rtl");
        }
        for code in ["en", "fr-FR", "ja", "zh-CN"] {
            assert!(!is_rtl_language(code), "{code} should be LTR");
            assert_eq!(text_direction(code), "ltr");
        }
    }

    #[test]
    fn negotiate_skips_q_zero() {
        assert_eq!(negotiate_language("fr;q=0", &["en", "fr"]), None);
        assert_eq!(
            negotiate_language("fr;q=0,en;q=0.5", &["en", "fr"]).as_deref(),
            Some("en")
        );
    }

    #[test]
    fn negotiate_clamps_q_above_one() {
        let lang = negotiate_language("en,fr;q=5", &["en", "fr"]);
        assert_eq!(lang.as_deref(), Some("en"));
    }

    #[test]
    fn negotiate_keeps_header_order_on_equal_q() {
        assert_eq!(
            negotiate_language("fr,en", &["en", "fr"]).as_deref(),
            Some("fr")
        );
        assert_eq!(
            negotiate_language("de;q=0.5,fr;q=0.5,en;q=0.5", &["en", "fr"]).as_deref(),
            Some("fr")
        );
        // A shape an unstable sort reorders (measured on rustc 1.88).
        let header: Vec<String> = std::iter::once("fr".to_owned())
            .chain((0..34).map(|i| match i % 3 {
                0 => format!("x{i}"),
                _ => format!("x{i};q=0.5"),
            }))
            .chain(["en".to_owned()])
            .collect();
        let lang = negotiate_language(&header.join(","), &["en", "fr"]);
        assert_eq!(lang.as_deref(), Some("fr"));
    }

    #[test]
    fn negotiate_survives_nan_q_values() {
        let header: Vec<String> = (0..60)
            .map(|i| match i % 3 {
                0 => format!("x{i};q=NaN"),
                1 => format!("y{i};q=0.{}", i % 10),
                _ => format!("z{i}"),
            })
            .chain(["fr;q=0.95".to_owned(), "en;q=nan".to_owned()])
            .collect();
        let lang = negotiate_language(&header.join(","), &["en", "fr"]);
        // A NaN q is ignored like an unparsable one, so `en` keeps q=1.
        assert_eq!(lang.as_deref(), Some("en"));
    }

    #[test]
    fn underscore_locales_match_hyphenated_ones() {
        assert_eq!(Locale::new("pt_BR"), Locale::new("pt-BR"));
        assert_eq!(Locale::new("uk_ua").base_language(), "uk");
        assert!(is_rtl_language("ar_EG"));
        assert_eq!(
            negotiate_language("pt-BR", &["en", "pt_BR"]).as_deref(),
            Some("pt-br")
        );
    }

    #[test]
    fn substitute_does_not_rescan_inserted_values() {
        assert_eq!(
            substitute("{name} has {count}", &[("count", "5"), ("name", "{count}")]),
            "{count} has 5"
        );
        assert_eq!(substitute("{a}{", &[("a", "x")]), "x{");
        assert_eq!(substitute("{missing}", &[]), "{missing}");
        // Later pairs override earlier ones (ngettext_fmt's overlay).
        assert_eq!(
            substitute("{count}", &[("count", "1"), ("count", "2")]),
            "2"
        );
    }

    #[test]
    fn plural_rules_for_more_families() {
        let cats = |loc: &str, ns: &[i64]| {
            ns.iter()
                .map(|n| plural_category(loc, *n))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            cats("ar", &[0, 1, 2, 3, 11, 100, 102]),
            ["zero", "one", "two", "few", "many", "other", "other"]
        );
        assert_eq!(cats("cs", &[1, 3, 5]), ["one", "few", "other"]);
        assert_eq!(cats("sk", &[1, 4, 22]), ["one", "few", "other"]);
        assert_eq!(
            cats("lt", &[1, 2, 11, 10, 21]),
            ["one", "few", "other", "other", "one"]
        );
        assert_eq!(
            cats("ro", &[1, 0, 19, 20, 101]),
            ["one", "few", "few", "other", "few"]
        );
        assert_eq!(cats("he", &[1, 2, 3]), ["one", "two", "other"]);
        assert_eq!(cats("sl", &[1, 102, 3, 5]), ["one", "two", "few", "other"]);
        assert_eq!(cats("pt-PT", &[0, 1, 2]), ["other", "one", "other"]);
        assert!(plural_category_is_explicit("pt-PT"));
        assert!(plural_category_is_explicit("pt_PT"));
        assert_eq!(cats("pt_BR", &[0, 1]), ["one", "one"]);
    }

    #[test]
    fn negotiate_prefers_the_base_over_a_sibling_region() {
        assert_eq!(
            negotiate_language("en-US", &["en-GB", "en"]).as_deref(),
            Some("en")
        );
        // With no bare base, a sibling region is still better than nothing.
        assert_eq!(
            negotiate_language("en-US", &["fr", "en-GB"]).as_deref(),
            Some("en-gb")
        );
    }

    #[test]
    fn negotiate_picks_highest_q() {
        let lang = negotiate_language("en;q=0.5,fr;q=0.9,de;q=0.1", &["en", "fr", "de"]);
        assert_eq!(lang.as_deref(), Some("fr"));
    }

    #[test]
    fn negotiate_falls_back_to_base() {
        let lang = negotiate_language("fr-FR,fr;q=0.9,en;q=0.8", &["en", "fr"]);
        assert_eq!(lang.as_deref(), Some("fr"));
    }

    #[test]
    fn negotiate_no_match_returns_none() {
        let lang = negotiate_language("ja,zh", &["en", "fr"]);
        assert_eq!(lang, None);
    }

    #[test]
    fn negotiate_uses_default_q_of_1() {
        // "en" without q is 1.0; "fr;q=0.5" is 0.5 → en wins
        let lang = negotiate_language("en,fr;q=0.5", &["en", "fr"]);
        assert_eq!(lang.as_deref(), Some("en"));
    }

    #[test]
    fn negotiate_empty_accept_language_returns_none() {
        let lang = negotiate_language("", &["en", "fr"]);
        assert_eq!(lang, None);
    }

    // ============================================================
    //
    // gettext-shape aliases: `gettext`, `pgettext` (context
    // disambiguation), `ngettext` (English plural rule), + their
    // `_fmt` placeholder-substitution variants.

    fn translator_with_en_fr() -> Translator {
        let mut en = HashMap::new();
        en.insert("welcome".into(), "Welcome, {name}!".into());
        en.insert(
            "count_unread".into(),
            "You have {count} unread message.".into(),
        );
        en.insert(
            "count_unread_plural".into(),
            "You have {count} unread messages.".into(),
        );
        en.insert("verb\u{0004}save".into(), "Save".into()); // pgettext context=verb, key=save
        en.insert("noun\u{0004}save".into(), "Discount".into()); // pgettext context=noun, key=save
        en.insert("save".into(), "Save (bare)".into());

        let mut fr = HashMap::new();
        fr.insert("welcome".into(), "Bienvenue, {name} !".into());
        fr.insert(
            "count_unread".into(),
            "Vous avez {count} message non lu.".into(),
        );
        fr.insert(
            "count_unread_plural".into(),
            "Vous avez {count} messages non lus.".into(),
        );

        Translator::new(Locale::new("en"))
            .add_locale(Locale::new("en"), en)
            .add_locale(Locale::new("fr"), fr)
    }

    #[test]
    fn gettext_returns_translated_string_no_params() {
        let t = translator_with_en_fr();
        // No interpolation — gettext returns the raw template.
        assert_eq!(t.gettext("en", "welcome"), "Welcome, {name}!");
    }

    #[test]
    fn gettext_fmt_substitutes_placeholders() {
        let t = translator_with_en_fr();
        assert_eq!(
            t.gettext_fmt("fr", "welcome", &[("name", "Alice")]),
            "Bienvenue, Alice !"
        );
    }

    #[test]
    fn gettext_falls_back_to_key_on_miss() {
        let t = translator_with_en_fr();
        // Missing keys fall through to the literal source string, so
        // an untranslated message surfaces unchanged.
        assert_eq!(t.gettext("en", "no_such_key"), "no_such_key");
    }

    #[test]
    fn pgettext_uses_context_disambiguated_entry() {
        let t = translator_with_en_fr();
        assert_eq!(t.pgettext("en", "verb", "save"), "Save");
        assert_eq!(t.pgettext("en", "noun", "save"), "Discount");
    }

    #[test]
    fn pgettext_falls_back_to_bare_key_when_context_entry_missing() {
        let t = translator_with_en_fr();
        // Unknown context → fall through to the bare-key entry.
        assert_eq!(t.pgettext("en", "adjective", "save"), "Save (bare)");
    }

    #[test]
    fn pgettext_fmt_substitutes_placeholders() {
        let mut en = HashMap::new();
        en.insert("button\u{0004}greet".into(), "Hi, {name}".into());
        let t = Translator::new(Locale::new("en")).add_locale(Locale::new("en"), en);
        assert_eq!(
            t.pgettext_fmt("en", "button", "greet", &[("name", "Bob")]),
            "Hi, Bob"
        );
    }

    #[test]
    fn ngettext_picks_singular_when_count_is_one() {
        let t = translator_with_en_fr();
        let s = t.ngettext("en", "count_unread", "count_unread_plural", 1);
        assert_eq!(s, "You have 1 unread message.");
    }

    #[test]
    fn ngettext_picks_plural_for_zero_and_many() {
        let t = translator_with_en_fr();
        let s0 = t.ngettext("en", "count_unread", "count_unread_plural", 0);
        let s5 = t.ngettext("en", "count_unread", "count_unread_plural", 5);
        // English plural rule — `n != 1` is plural, so n=0 plural too.
        assert_eq!(s0, "You have 0 unread messages.");
        assert_eq!(s5, "You have 5 unread messages.");
    }

    #[test]
    fn ngettext_works_in_french_locale() {
        let t = translator_with_en_fr();
        let s1 = t.ngettext("fr", "count_unread", "count_unread_plural", 1);
        let s3 = t.ngettext("fr", "count_unread", "count_unread_plural", 3);
        assert_eq!(s1, "Vous avez 1 message non lu.");
        assert_eq!(s3, "Vous avez 3 messages non lus.");
    }

    #[test]
    fn ngettext_fmt_overlays_extra_placeholders() {
        let mut en = HashMap::new();
        en.insert(
            "item_singular".into(),
            "{count} {item} sold to {customer}.".into(),
        );
        en.insert(
            "item_plural".into(),
            "{count} {item}s sold to {customer}.".into(),
        );
        let t = Translator::new(Locale::new("en")).add_locale(Locale::new("en"), en);
        let s = t.ngettext_fmt(
            "en",
            "item_singular",
            "item_plural",
            2,
            &[("item", "book"), ("customer", "Alice")],
        );
        assert_eq!(s, "2 books sold to Alice.");
    }

    // ---- #1102 CLDR plural rules + translate_plural ----

    #[test]
    fn plural_category_covers_launch_locales() {
        // one / other (English, German, default).
        assert_eq!(plural_category("en", 1), "one");
        for n in [0, 2, 5, 11, 21, 100] {
            assert_eq!(plural_category("en", n), "other", "en {n}");
        }
        assert_eq!(plural_category("de", 1), "one");
        assert_eq!(plural_category("de", 7), "other");
        // French: 0 and 1 are "one".
        assert_eq!(plural_category("fr", 0), "one");
        assert_eq!(plural_category("fr", 1), "one");
        assert_eq!(plural_category("fr", 2), "other");
        assert_eq!(plural_category("fr-FR", 0), "one"); // base-language match
                                                        // East-Asian: always "other".
        for n in [0, 1, 2, 5, 100] {
            assert_eq!(plural_category("zh-Hans", n), "other", "zh {n}");
            assert_eq!(plural_category("ja", n), "other", "ja {n}");
        }
        // Polish: one / few / many.
        assert_eq!(plural_category("pl", 1), "one");
        for n in [2, 3, 4, 22, 23, 24] {
            assert_eq!(plural_category("pl", n), "few", "pl {n}");
        }
        for n in [0, 5, 11, 12, 14, 25, 111] {
            assert_eq!(plural_category("pl", n), "many", "pl {n}");
        }
        // Ukrainian: …1 (not 11) is one; …2-4 (not 12-14) few; rest many.
        for n in [1, 21, 31, 101] {
            assert_eq!(plural_category("uk", n), "one", "uk {n}");
        }
        for n in [2, 3, 4, 22, 23, 24] {
            assert_eq!(plural_category("uk", n), "few", "uk {n}");
        }
        for n in [0, 5, 11, 12, 13, 14, 25] {
            assert_eq!(plural_category("uk", n), "many", "uk {n}");
        }
    }

    #[test]
    fn translate_plural_selects_form_three_way() {
        let mut entry = HashMap::new();
        entry.insert("one".to_owned(), "Usunięto {count} stronę.".to_owned());
        entry.insert("few".to_owned(), "Usunięto {count} strony.".to_owned());
        entry.insert("many".to_owned(), "Usunięto {count} stron.".to_owned());
        let key = "Deleted {count} page(s).";
        let mut pl: HashMap<String, HashMap<String, String>> = HashMap::new();
        pl.insert(key.to_owned(), entry);
        let t = Translator::new(Locale::new("en")).add_plural_locale(Locale::new("pl"), pl);

        let tr = |n: i64| t.translate_plural("pl", key, n, &[("count", &n.to_string())]);
        assert_eq!(tr(1), "Usunięto 1 stronę."); // one
        assert_eq!(tr(2), "Usunięto 2 strony."); // few
        assert_eq!(tr(22), "Usunięto 22 strony."); // few
        assert_eq!(tr(5), "Usunięto 5 stron."); // many
        assert_eq!(tr(25), "Usunięto 25 stron."); // many
    }

    #[test]
    fn plural_form_follows_the_locale_the_text_came_from() {
        let mut entry = HashMap::new();
        entry.insert("one".to_owned(), "Deleted {count} page.".to_owned());
        entry.insert("other".to_owned(), "Deleted {count} pages.".to_owned());
        let key = "deleted_pages";
        let en_pl: HashMap<String, HashMap<String, String>> =
            [(key.to_owned(), entry)].into_iter().collect();
        let en: HashMap<String, String> = [
            ("item_one".to_owned(), "{count} item".to_owned()),
            ("item_many".to_owned(), "{count} items".to_owned()),
        ]
        .into_iter()
        .collect();
        let t = Translator::new(Locale::new("en"))
            .add_plural_locale(Locale::new("en"), en_pl)
            .add_locale(Locale::new("en"), en);
        let tp = |loc: &str, n: i64| t.translate_plural(loc, key, n, &[("count", &n.to_string())]);
        // ru says "one" for 21 and ja says "other" for 1; the text is English.
        assert_eq!(tp("ru", 21), "Deleted 21 pages.");
        assert_eq!(tp("ja", 1), "Deleted 1 page.");
        assert_eq!(t.ngettext("ru", "item_one", "item_many", 21), "21 items");
        assert_eq!(t.ngettext("ja", "item_one", "item_many", 1), "1 item");
        assert_eq!(
            t.ngettext_fmt("ru", "item_one", "item_many", 21, &[]),
            "21 items"
        );
        // Nothing translated: the source pair follows the default locale.
        assert_eq!(
            t.ngettext("ru", "{count} file", "{count} files", 21),
            "21 files"
        );
        // A ru entry still uses the ru rule.
        let ru: HashMap<String, String> = [("item_one".to_owned(), "{count} штука".to_owned())]
            .into_iter()
            .collect();
        t.insert_locale(Locale::new("ru"), ru);
        assert_eq!(t.ngettext("ru", "item_one", "item_many", 21), "21 штука");
    }

    #[test]
    fn translate_plural_picks_other_when_category_absent() {
        // Entry only has one/other → French "other" is used for n=2, and a
        // Polish-style "few"/"many" request also degrades to "other".
        let mut entry = HashMap::new();
        entry.insert("one".to_owned(), "{count} élément".to_owned());
        entry.insert("other".to_owned(), "{count} éléments".to_owned());
        let key = "count_items";
        let mut fr: HashMap<String, HashMap<String, String>> = HashMap::new();
        fr.insert(key.to_owned(), entry);
        let t = Translator::new(Locale::new("en")).add_plural_locale(Locale::new("fr"), fr);
        assert_eq!(
            t.translate_plural("fr", key, 1, &[("count", "1")]),
            "1 élément"
        );
        assert_eq!(
            t.translate_plural("fr", key, 9, &[("count", "9")]),
            "9 éléments"
        );
    }

    #[test]
    fn translate_plural_falls_back_to_scalar_and_source() {
        // No plural catalog at all → source key, with {count} interpolated
        // (so English / untranslated still renders, never a blank).
        let t = Translator::new(Locale::new("en"));
        assert_eq!(
            t.translate_plural("en", "Deleted {count} page(s).", 3, &[("count", "3")]),
            "Deleted 3 page(s)."
        );
        // A flat scalar catalog entry is honoured when there's no plural entry.
        let mut en = HashMap::new();
        en.insert("flat_msg".to_owned(), "{count} thing(s)".to_owned());
        let t = Translator::new(Locale::new("en")).add_locale(Locale::new("en"), en);
        assert_eq!(
            t.translate_plural("en", "flat_msg", 2, &[("count", "2")]),
            "2 thing(s)"
        );
    }

    #[test]
    fn plural_beats_flat_at_one_locale_and_flat_beats_a_fallback_plural() {
        let entry = |v: &str| {
            let mut e = HashMap::new();
            e.insert("other".to_owned(), v.to_owned());
            [("k".to_owned(), e)].into_iter().collect::<HashMap<_, _>>()
        };
        let flat = |v: &str| [("k".to_owned(), v.to_owned())].into_iter().collect();
        let t = Translator::new(Locale::new("en"))
            .add_locale(Locale::new("fr"), flat("fr flat"))
            .add_plural_locale(Locale::new("fr"), entry("fr plural"));
        assert_eq!(t.translate_plural("fr", "k", 2, &[]), "fr plural");
        let t = Translator::new(Locale::new("en"))
            .add_locale(Locale::new("fr"), flat("fr flat"))
            .add_plural_locale(Locale::new("en"), entry("en plural"));
        assert_eq!(t.translate_plural("fr", "k", 2, &[]), "fr flat");
    }

    #[test]
    fn plural_entry_without_category_or_other_falls_to_flat() {
        let mut e = HashMap::new();
        e.insert("one".to_owned(), "one form".to_owned());
        let plural = [("k".to_owned(), e)].into_iter().collect();
        let flat = [("k".to_owned(), "flat".to_owned())].into_iter().collect();
        let t = Translator::new(Locale::new("en"))
            .add_locale(Locale::new("en"), flat)
            .add_plural_locale(Locale::new("en"), plural);
        assert_eq!(t.translate_plural("en", "k", 1, &[]), "one form");
        assert_eq!(t.translate_plural("en", "k", 5, &[]), "flat");
    }

    #[test]
    fn language_names_accept_underscore_locales() {
        assert_eq!(language_display_name("pt_BR"), "Portuguese");
        assert_eq!(language_native_name("de_DE"), language_native_name("de"));
        assert_ne!(language_native_name("de_DE"), "Unknown");
        assert_eq!(Locale::new("pt_BR").display_name(), "Portuguese");
    }

    #[test]
    fn default_locale_override_reaches_locales_missing_the_key() {
        let t = make_translator();
        t.set_override("en", "only_en", "Edited");
        t.set_override("en", "hello", "Hello there");
        assert_eq!(t.gettext("de", "only_en"), "Edited");
        assert_eq!(t.gettext("fr", "only_en"), "Edited");
        // A locale with its own file entry keeps it.
        assert_eq!(t.gettext("fr", "hello"), "Bonjour");
        assert_eq!(t.gettext("de", "hello"), "Hello there");
    }

    #[test]
    fn fallback_chain_override_is_seen() {
        let t = Translator::new(Locale::new("en")).with_fallback_chain(&["pt"]);
        t.set_override("pt", "k", "Olá");
        assert_eq!(t.gettext("es", "k"), "Olá");
    }

    #[test]
    fn base_language_override_does_not_hide_a_region_file_entry() {
        let mut br = HashMap::new();
        br.insert("k".to_owned(), "Oi".to_owned());
        let t = Translator::new(Locale::new("en")).add_locale(Locale::new("pt-BR"), br);
        t.set_override("pt", "k", "Olá");
        assert_eq!(t.gettext("pt-BR", "k"), "Oi");
        assert_eq!(t.gettext("pt", "k"), "Olá");
    }

    #[test]
    fn override_wins_over_a_plural_entry() {
        let mut entry = HashMap::new();
        entry.insert("one".to_owned(), "{count} page".to_owned());
        entry.insert("other".to_owned(), "{count} pages".to_owned());
        let mut en: HashMap<String, HashMap<String, String>> = HashMap::new();
        en.insert("pages".to_owned(), entry);
        let t = Translator::new(Locale::new("en")).add_plural_locale(Locale::new("en"), en);
        t.set_override("en", "pages", "{count} page(s)");
        assert_eq!(
            t.translate_plural("en", "pages", 2, &[("count", "2")]),
            "2 page(s)"
        );
    }

    #[test]
    fn catalog_files_may_hold_plural_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fr.json"),
            r#"{"hi": "Salut", "items": {"one": "{count} élément", "other": "{count} éléments"}}"#,
        )
        .unwrap();
        let t = Translator::from_directory(dir.path(), Locale::new("en")).unwrap();
        assert_eq!(t.gettext("fr", "hi"), "Salut");
        assert_eq!(
            t.translate_plural("fr", "items", 1, &[("count", "1")]),
            "1 élément"
        );
        assert_eq!(
            t.translate_plural("fr", "items", 3, &[("count", "3")]),
            "3 éléments"
        );
    }
}
