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

pub mod middleware;
pub mod tera_tags;
pub mod timezone;

// ------------------------------------------------------------------ Locale

/// A locale identifier (e.g. `"en"`, `"en-US"`, `"fr"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Locale(String);

impl Locale {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_lowercase())
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
    /// Django parity #425. Lowercased on insert.
    fallback_chain: Vec<Locale>,
    catalogs: RwLock<HashMap<Locale, HashMap<String, String>>>,
}

impl Translator {
    /// New translator with the given fallback locale.
    #[must_use]
    pub fn new(default_locale: Locale) -> Self {
        Self {
            default_locale,
            fallback_chain: Vec::new(),
            catalogs: RwLock::new(HashMap::new()),
        }
    }

    /// Set an explicit fallback chain (#425, Django parity). When
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

    /// Look up `key` in `locale`'s catalog, walking the fallback
    /// chain in order: exact locale → base language → explicit
    /// `fallback_chain` (#425) → default locale → `key` itself.
    ///
    /// `params` substitutes `{name}` placeholders in the result.
    #[must_use]
    pub fn translate(&self, locale: &str, key: &str, params: &[(&str, &str)]) -> String {
        let cats = self.catalogs.read().expect("translator poisoned");
        let req = Locale::new(locale);

        // Walk the lookup chain in priority order.
        let mut template: Option<String> = cats
            .get(&req)
            .and_then(|c| c.get(key))
            .or_else(|| {
                cats.get(&Locale::new(req.base_language()))
                    .and_then(|c| c.get(key))
            })
            .cloned();

        if template.is_none() {
            for fb in &self.fallback_chain {
                if let Some(v) = cats.get(fb).and_then(|c| c.get(key)) {
                    template = Some(v.clone());
                    break;
                }
            }
        }

        let template = template
            .or_else(|| {
                cats.get(&self.default_locale)
                    .and_then(|c| c.get(key))
                    .cloned()
            })
            .unwrap_or_else(|| key.to_owned());

        substitute(&template, params)
    }

    /// `true` when a catalog is registered for `locale` (or its base language).
    #[must_use]
    pub fn has_locale(&self, locale: &str) -> bool {
        let cats = self.catalogs.read().expect("translator poisoned");
        let req = Locale::new(locale);
        cats.contains_key(&req) || cats.contains_key(&Locale::new(req.base_language()))
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

    /// Load every `*.json` file in `dir` as a locale catalog. The file
    /// stem becomes the locale identifier (e.g. `en.json` → `Locale::new("en")`).
    ///
    /// Each file must contain a flat object of string→string entries.
    ///
    /// # Errors
    /// [`I18nError::Io`] when the dir can't be read or a file is unreadable.
    /// [`I18nError::Parse`] when a JSON file is malformed.
    pub fn from_directory(dir: &Path, default_locale: Locale) -> Result<Self, I18nError> {
        let t = Translator::new(default_locale);
        let entries = std::fs::read_dir(dir).map_err(|e| I18nError::Io(e.to_string()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = std::fs::read_to_string(&path).map_err(|e| I18nError::Io(e.to_string()))?;
            let catalog: HashMap<String, String> =
                serde_json::from_str(&raw).map_err(|e| I18nError::Parse {
                    file: path.display().to_string(),
                    detail: e.to_string(),
                })?;
            t.insert_locale(Locale::new(stem), catalog);
        }
        Ok(t)
    }

    /// Build a `Translator` from a [`crate::config::sections::I18nSettings`]
    /// — Django-shape `LANGUAGE_CODE` / `LANGUAGES` / `LOCALE_PATHS`
    /// (#403). Reads each `locale_paths` entry as a directory of
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
        // catalogs whose stem matches one of the entries.
        let allowlist: Option<std::collections::HashSet<String>> = if settings.languages.is_empty()
        {
            None
        } else {
            Some(settings.languages.iter().cloned().collect())
        };

        for raw_path in &settings.locale_paths {
            let path = std::path::Path::new(raw_path);
            let entries = std::fs::read_dir(path)
                .map_err(|e| I18nError::Io(format!("{}: {e}", path.display())))?;
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Some(allow) = &allowlist {
                    if !allow.contains(stem) {
                        continue;
                    }
                }
                let raw = std::fs::read_to_string(&p).map_err(|e| I18nError::Io(e.to_string()))?;
                let catalog: HashMap<String, String> =
                    serde_json::from_str(&raw).map_err(|e| I18nError::Parse {
                        file: p.display().to_string(),
                        detail: e.to_string(),
                    })?;
                t.insert_locale(Locale::new(stem), catalog);
            }
        }

        if !settings.fallback_chain.is_empty() {
            let chain: Vec<&str> = settings.fallback_chain.iter().map(String::as_str).collect();
            t = t.with_fallback_chain(&chain);
        }

        Ok(t)
    }

    /// Django/gettext-shape alias for [`Self::translate`] — issue #422.
    /// Mirrors Django's `gettext(message)`: look up the key in the
    /// supplied locale and return the translated string (or the
    /// key itself when no translation is found). No parameter
    /// substitution — Django's `gettext` is the raw lookup.
    ///
    /// For interpolation use [`Self::translate`] (rustango shape)
    /// or [`Self::gettext_fmt`] (gettext shape that accepts a
    /// placeholder map).
    #[must_use]
    pub fn gettext(&self, locale: &str, key: &str) -> String {
        self.translate(locale, key, &[])
    }

    /// gettext-shape lookup with placeholder substitution. Same
    /// substitution rules as [`Self::translate`] (`{name}` →
    /// `params["name"]`). Provided so projects porting from Django
    /// keep their muscle memory: `gettext_fmt(locale, "Hi, {name}",
    /// &[("name", &user.name)])`.
    #[must_use]
    pub fn gettext_fmt(&self, locale: &str, key: &str, params: &[(&str, &str)]) -> String {
        self.translate(locale, key, params)
    }

    /// Django/gettext-shape `pgettext(context, message)` — context-
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

    /// Django/gettext-shape `ngettext(singular, plural, count)` —
    /// pluralization. Returns `singular` when `count == 1`,
    /// `plural` otherwise. Issue #422.
    ///
    /// Catalog format mirrors gettext's `msgid_plural` convention:
    /// register two keys, one for each form. The fallback chain is
    /// the same as [`Self::translate`]; missing entries fall back
    /// to the supplied source strings.
    ///
    /// This implements the **English** plural rule (`n == 1` →
    /// singular; everything else → plural). Languages with more
    /// complex plural categories (Russian's three forms, Arabic's
    /// six, etc.) need a CLDR plural-rules resolver — out of scope
    /// for v1; track in #426 / future-backlog.
    ///
    /// `count` is bound to the `{count}` placeholder automatically
    /// so templates can read `"You have {count} unread messages"`
    /// without the caller threading it in.
    #[must_use]
    pub fn ngettext(&self, locale: &str, singular: &str, plural: &str, count: i64) -> String {
        let key = if count == 1 { singular } else { plural };
        let count_str = count.to_string();
        self.translate(locale, key, &[("count", &count_str)])
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
        let key = if count == 1 { singular } else { plural };
        let count_str = count.to_string();
        let mut all: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 1);
        all.push(("count", &count_str));
        all.extend_from_slice(params);
        self.translate(locale, key, &all)
    }
}

/// Substitute `{name}` placeholders in `template` with values from `params`.
fn substitute(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (name, value) in params {
        let placeholder = format!("{{{name}}}");
        out = out.replace(&placeholder, value);
    }
    out
}

// ------------------------------------------------------------------ Accept-Language negotiation

/// Pick the best-matching language from `Accept-Language`.
///
/// `accept_language` is the raw header value (e.g. `"fr-FR,fr;q=0.9,en;q=0.8"`).
/// `available` is the list of locales the app supports.
///
/// Returns the best match or `None` if no acceptable language is supported.
#[must_use]
pub fn negotiate_language<S: AsRef<str>>(accept_language: &str, available: &[S]) -> Option<String> {
    let mut prefs = parse_accept_language(accept_language);
    // Sort by quality desc
    prefs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let avail_lower: Vec<String> = available
        .iter()
        .map(|s| s.as_ref().to_lowercase())
        .collect();

    for (lang, _q) in prefs {
        let lang_lower = lang.to_lowercase();
        // Exact match
        if let Some(matched) = avail_lower.iter().find(|a| **a == lang_lower) {
            return Some(matched.clone());
        }
        // Base-language match
        let base = lang_lower.split('-').next().unwrap_or(&lang_lower);
        if let Some(matched) = avail_lower
            .iter()
            .find(|a| **a == base || a.starts_with(&format!("{base}-")))
        {
            return Some(matched.clone());
        }
    }
    None
}

fn parse_accept_language(header: &str) -> Vec<(String, f32)> {
    header
        .split(',')
        .filter_map(|raw| {
            let mut parts = raw.split(';').map(str::trim);
            let lang = parts.next()?.to_owned();
            if lang.is_empty() {
                return None;
            }
            let mut q = 1.0;
            for kv in parts {
                if let Some(rest) = kv.strip_prefix("q=") {
                    if let Ok(parsed) = rest.parse::<f32>() {
                        q = parsed;
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

    // ============================================================ #422
    //
    // Django/gettext-shape aliases: `gettext`, `pgettext` (context
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
        // Missing keys fall through to the literal source string —
        // matches Django's behavior where untranslated msgids
        // surface unchanged.
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
        // Unknown context → fall through to the bare-key entry
        // (matches Django's pgettext fallback).
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
}
