//! Django-shape `LocaleMiddleware` — tower layer that picks the
//! active locale per request and injects it into request extensions.
//!
//! Issue #406. Mirrors Django's
//! [`django.middleware.locale.LocaleMiddleware`](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.locale.LocaleMiddleware)
//! pick order, excluding URL prefix (see note below):
//!
//! 1. Session / cookie (configurable cookie name; default `django_language`).
//! 2. `Accept-Language` header via [`super::negotiate_language`].
//! 3. The configured default locale.
//!
//! ## Quick start
//!
//! ```ignore
//! use rustango::i18n::middleware::{LocaleMiddleware, ActiveLocale};
//!
//! let layer = LocaleMiddleware::new(&["en", "fr", "es"])
//!     .default("en")
//!     .cookie_name("django_language");
//!
//! let app = axum::Router::new()
//!     .route("/", get(handler))
//!     .layer(layer);
//!
//! async fn handler(active: ActiveLocale) -> String {
//!     format!("you speak {}", active.0)
//! }
//! ```
//!
//! ## URL-prefix locale (`/en/foo` / `/es/foo`)
//!
//! axum's `Router::layer` runs **after** path matching, so a layer
//! can't strip `/en` before routing. Use `Router::nest` per locale
//! instead — composes with `LocaleMiddleware` cleanly:
//!
//! ```ignore
//! use axum::Router;
//! use rustango::i18n::middleware::LocaleMiddleware;
//!
//! fn locale_router(lang: &str) -> Router {
//!     Router::new()
//!         .route("/posts", get(list_posts))
//!         // Hard-code the locale for routes under this nest:
//!         .layer(LocaleMiddleware::new(&[lang]).default(lang))
//! }
//!
//! let app = Router::new()
//!     .nest("/en", locale_router("en"))
//!     .nest("/fr", locale_router("fr"));
//! ```
//!
//! This is the Django-idiomatic pattern for issue #424 too — Django
//! generates locale-prefixed routes via `i18n_patterns()`. Composing
//! `Router::nest` per locale is the axum-shaped equivalent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{Request, Response};

use super::negotiate_language;

const DEFAULT_COOKIE: &str = "django_language";

/// The locale picked for the current request. Available as an
/// extractor on handler signatures; populated by
/// [`LocaleMiddleware`]. Falls back to `"en"` when no middleware ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLocale(pub String);

impl ActiveLocale {
    /// Read the active locale from request extensions, if any was
    /// installed by [`LocaleMiddleware`].
    pub fn from_extensions(ext: &axum::http::Extensions) -> Option<Self> {
        ext.get::<Self>().cloned()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ActiveLocale {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ActiveLocale>()
            .cloned()
            .unwrap_or_else(|| ActiveLocale("en".into())))
    }
}

#[derive(Clone)]
struct LocaleConfig {
    /// Lowercase locale identifiers the app supports.
    available: Vec<String>,
    /// Fallback locale when nothing matches. Always lowercased.
    default: String,
    /// Cookie name to read; `None` to disable cookie lookup.
    cookie_name: Option<String>,
}

/// Tower layer that resolves an [`ActiveLocale`] per request.
#[derive(Clone)]
pub struct LocaleMiddleware {
    config: Arc<LocaleConfig>,
}

impl LocaleMiddleware {
    /// Build a middleware that picks among `available` locales. The
    /// first entry becomes the default; override with
    /// [`Self::default`].
    #[must_use]
    pub fn new(available: &[&str]) -> Self {
        let avail: Vec<String> = available.iter().map(|s| s.to_lowercase()).collect();
        let default = avail.first().cloned().unwrap_or_else(|| "en".into());
        Self {
            config: Arc::new(LocaleConfig {
                available: avail,
                default,
                cookie_name: Some(DEFAULT_COOKIE.into()),
            }),
        }
    }

    /// Override the fallback locale.
    #[must_use]
    pub fn default(mut self, locale: &str) -> Self {
        Arc::make_mut(&mut self.config).default = locale.to_lowercase();
        self
    }

    /// Set the cookie name to read for a sticky locale preference.
    /// Pass `None` to disable cookie lookup.
    #[must_use]
    pub fn cookie_name(mut self, name: impl Into<Option<String>>) -> Self {
        Arc::make_mut(&mut self.config).cookie_name = name.into();
        self
    }

    /// Run the pick algorithm against a request. Pure — exposed for
    /// unit tests + direct use from custom middleware compositions.
    pub fn pick(&self, req: &Request<Body>) -> String {
        let cfg = &self.config;

        // 1. Cookie
        if let Some(name) = cfg.cookie_name.as_deref() {
            if let Some(value) = cookie_value(req.headers(), name) {
                let lower = value.to_lowercase();
                if cfg.available.iter().any(|a| *a == lower) {
                    return lower;
                }
            }
        }

        // 2. Accept-Language
        if let Some(al) = req.headers().get(axum::http::header::ACCEPT_LANGUAGE) {
            if let Ok(value) = al.to_str() {
                if let Some(matched) = negotiate_language(value, &cfg.available) {
                    return matched;
                }
            }
        }

        // 3. Default fallback
        cfg.default.clone()
    }
}

fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(axum::http::header::COOKIE) {
        let raw = match h.to_str() {
            Ok(s) => s,
            Err(_) => continue,
        };
        for pair in raw.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                if k == name {
                    return Some(v.to_owned());
                }
            }
        }
    }
    None
}

impl<S> tower::Layer<S> for LocaleMiddleware {
    type Service = LocaleService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LocaleService {
            inner,
            middleware: self.clone(),
        }
    }
}

#[derive(Clone)]
pub struct LocaleService<S> {
    inner: S,
    middleware: LocaleMiddleware,
}

impl<S> tower::Service<Request<Body>> for LocaleService<S>
where
    S: tower::Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let picked = self.middleware.pick(&req);
        req.extensions_mut().insert(ActiveLocale(picked));
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(uri: &str, accept_language: Option<&str>, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(al) = accept_language {
            b = b.header(axum::http::header::ACCEPT_LANGUAGE, al);
        }
        if let Some(c) = cookie {
            b = b.header(axum::http::header::COOKIE, c);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn cookie_beats_accept_language() {
        let mw = LocaleMiddleware::new(&["en", "fr"]).default("en");
        let r = req("/", Some("en"), Some("django_language=fr"));
        assert_eq!(mw.pick(&r), "fr");
    }

    #[test]
    fn accept_language_used_when_no_cookie() {
        let mw = LocaleMiddleware::new(&["en", "fr"]).default("en");
        let r = req("/", Some("fr-FR,fr;q=0.9"), None);
        assert_eq!(mw.pick(&r), "fr");
    }

    #[test]
    fn default_when_nothing_matches() {
        let mw = LocaleMiddleware::new(&["en", "fr"]).default("en");
        let r = req("/", Some("ja"), None);
        assert_eq!(mw.pick(&r), "en");
    }

    #[test]
    fn unknown_cookie_value_falls_through() {
        // Cookie set to a non-available locale — middleware should
        // ignore it and fall through to Accept-Language.
        let mw = LocaleMiddleware::new(&["en", "fr"]).default("en");
        let r = req("/", Some("fr"), Some("django_language=de"));
        assert_eq!(mw.pick(&r), "fr");
    }

    #[test]
    fn disabled_cookie_skips_cookie_step() {
        let mw = LocaleMiddleware::new(&["en", "fr"])
            .default("en")
            .cookie_name(None);
        // Cookie says fr but cookie lookup is disabled — Accept-Language
        // points at en, so en should win.
        let r = req("/", Some("en"), Some("django_language=fr"));
        assert_eq!(mw.pick(&r), "en");
    }

    #[test]
    fn case_insensitive_locale_lookup() {
        let mw = LocaleMiddleware::new(&["en", "fr"]).default("en");
        let r = req("/", None, Some("django_language=FR"));
        assert_eq!(mw.pick(&r), "fr");
    }
}
