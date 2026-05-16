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

pub mod tera_tags;

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
    catalogs: RwLock<HashMap<Locale, HashMap<String, String>>>,
}

impl Translator {
    /// New translator with the given fallback locale.
    #[must_use]
    pub fn new(default_locale: Locale) -> Self {
        Self {
            default_locale,
            catalogs: RwLock::new(HashMap::new()),
        }
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

    /// Look up `key` in `locale`'s catalog, falling back to the default
    /// locale, and finally to `key` itself if nothing matches.
    ///
    /// `params` substitutes `{name}` placeholders in the result.
    #[must_use]
    pub fn translate(&self, locale: &str, key: &str, params: &[(&str, &str)]) -> String {
        let cats = self.catalogs.read().expect("translator poisoned");
        let req = Locale::new(locale);

        // Try exact locale, then base language, then default
        let template = cats
            .get(&req)
            .and_then(|c| c.get(key))
            .or_else(|| {
                cats.get(&Locale::new(req.base_language()))
                    .and_then(|c| c.get(key))
            })
            .or_else(|| cats.get(&self.default_locale).and_then(|c| c.get(key)))
            .cloned()
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
}
