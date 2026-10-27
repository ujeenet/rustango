//! The error-pages layer itself; see [`super`] for the rules.

use std::sync::{Arc, OnceLock};

use axum::body::{Body, HttpBody as _};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::Router;
use tera::{Context, Tera};

use super::{error_words, KeepBody, Message};

/// Largest plain-text error body read for its message.
const MAX_MESSAGE_BYTES: usize = 2048;

/// Configuration of the error-pages layer. Mount it with
/// [`ErrorPagesRouterExt::error_pages`], or hand it to
/// `server::Builder::error_pages` / `AppBuilder::error_pages`.
#[derive(Clone)]
pub struct ErrorPagesLayer {
    templates: Option<Arc<Tera>>,
    htmx_target: Option<String>,
    htmx_swap: String,
    brand_name: Option<String>,
    brand_logo_url: Option<String>,
    brand_css: Option<String>,
    theme_mode: &'static str,
    /// Tenant context for resolving the signed-in user, when the layer
    /// sits outside the router that mounts it.
    #[cfg(feature = "tenancy")]
    pub(crate) user_context: axum::http::Extensions,
}

impl Default for ErrorPagesLayer {
    fn default() -> Self {
        Self {
            templates: None,
            htmx_target: Some("#flash".to_owned()),
            htmx_swap: "innerHTML show:window:top".to_owned(),
            brand_name: None,
            brand_logo_url: None,
            brand_css: None,
            theme_mode: "auto",
            #[cfg(feature = "tenancy")]
            user_context: axum::http::Extensions::new(),
        }
    }
}

impl std::fmt::Debug for ErrorPagesLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErrorPagesLayer")
            .field("templates", &self.templates.is_some())
            .field("htmx_target", &self.htmx_target)
            .field("htmx_swap", &self.htmx_swap)
            .field("brand_name", &self.brand_name)
            .finish_non_exhaustive()
    }
}

impl ErrorPagesLayer {
    /// Built-in pages only; htmx errors retargeted to `#flash`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `[errors]` and `[brand]` from settings; `None` unless
    /// `[errors] enabled = true`. A template dir that does not load is
    /// logged and the built-in pages are used.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn from_settings(settings: &crate::config::Settings) -> Option<Self> {
        let errors = &settings.errors;
        if errors.enabled != Some(true) {
            return None;
        }
        let mut layer = Self::new().brand(&settings.brand);
        if let Some(target) = &errors.htmx_target {
            layer = layer.htmx_target((!target.is_empty()).then_some(target.as_str()));
        }
        let dir = errors.templates.as_deref().unwrap_or("templates");
        let glob = format!("{}/**/*.html", dir.trim_end_matches('/'));
        if std::path::Path::new(dir).is_dir() {
            match crate::template_extensions::html_tera_from_glob(&glob) {
                Ok(tera) => layer = layer.templates(Arc::new(tera)),
                Err(e) => tracing::warn!(
                    target: "rustango::errors",
                    glob = %glob,
                    error = ?e,
                    "error-page templates did not load; using the built-in pages"
                ),
            }
        }
        Some(layer)
    }

    /// The app's templates, searched for the `errors/*.html` overrides.
    #[must_use]
    pub fn templates(mut self, tera: Arc<Tera>) -> Self {
        self.templates = Some(tera);
        self
    }

    /// CSS selector for `HX-Retarget` (default `#flash`); `None` sends no
    /// `HX-Retarget` / `HX-Reswap`.
    #[must_use]
    pub fn htmx_target(mut self, selector: Option<&str>) -> Self {
        self.htmx_target = selector.map(str::to_owned);
        self
    }

    /// `HX-Reswap` value (default `innerHTML show:window:top`).
    #[must_use]
    pub fn htmx_swap(mut self, swap: impl Into<String>) -> Self {
        self.htmx_swap = swap.into();
        self
    }

    /// Name, logo URL, accent colour and theme of the built-in page, from
    /// `[brand]`. The colour needs `tenancy`, whose brand code it reuses.
    #[cfg(feature = "config")]
    #[must_use]
    pub fn brand(mut self, brand: &crate::config::BrandSettings) -> Self {
        self.brand_name = brand.name.clone().filter(|n| !n.is_empty());
        self.brand_logo_url = brand.logo_url.clone().filter(|u| !u.is_empty());
        #[cfg(feature = "tenancy")]
        {
            self.brand_css =
                crate::tenancy::branding::build_op_brand_css(brand.primary_color.as_deref());
            if let Some(mode) = brand
                .theme_mode
                .as_deref()
                .and_then(crate::tenancy::branding::validate_theme_mode)
            {
                self.theme_mode = mode;
            }
        }
        self
    }
}

/// Adds `.error_pages(layer)` to a router.
pub trait ErrorPagesRouterExt {
    #[must_use]
    fn error_pages(self, layer: ErrorPagesLayer) -> Self;
}

impl<S: Clone + Send + Sync + 'static> ErrorPagesRouterExt for Router<S> {
    fn error_pages(self, layer: ErrorPagesLayer) -> Self {
        let cfg = Arc::new(layer);
        self.layer(axum::middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let cfg = cfg.clone();
                async move { handle(&cfg, req, next).await }
            },
        ))
    }
}

/// Who asked, read before the handler runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Asker {
    Browser,
    Htmx,
    Other,
}

/// What a page needs from the request: path, headers, URI and a few
/// `Arc`s, never the whole extension map. Kept only for HTML and htmx
/// requests. `csrf` is the fallback when no CSRF layer runs inside.
struct Kept {
    path: String,
    headers: HeaderMap,
    csrf: crate::forms::csrf::CsrfCookieSpec,
    nonce: String,
    #[cfg(feature = "tenancy")]
    uri: axum::http::Uri,
    #[cfg(feature = "tenancy")]
    tenant: axum::http::Extensions,
}

impl Kept {
    fn of(req: &Request<Body>, asker: Asker) -> Self {
        #[cfg(not(feature = "tenancy"))]
        let _ = asker;
        Self {
            path: req.uri().path().to_owned(),
            headers: req.headers().clone(),
            csrf: crate::forms::csrf::CsrfCookieSpec::active(req.extensions()),
            nonce: request_nonce(req.extensions()),
            #[cfg(feature = "tenancy")]
            uri: req.uri().clone(),
            #[cfg(feature = "tenancy")]
            tenant: {
                let mut tenant = axum::http::Extensions::new();
                if asker == Asker::Browser {
                    crate::extractors::MountedTenantContext::copy_all(
                        req.extensions(),
                        &mut tenant,
                    );
                }
                tenant
            },
        }
    }
}

async fn handle(cfg: &ErrorPagesLayer, req: Request<Body>, next: Next) -> Response {
    if req.method() == Method::HEAD {
        return next.run(req).await;
    }
    let asker = if req.headers().contains_key("hx-request") {
        Asker::Htmx
    } else if accepts_html(req.headers()) {
        Asker::Browser
    } else {
        Asker::Other
    };
    let kept = (asker != Asker::Other).then(|| Kept::of(&req, asker));

    let mut res = next.run(req).await;
    let status = res.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return res;
    }
    // Who asked picks the body, so caches must key on it, passthroughs too.
    add_vary(res.headers_mut());
    if res.extensions().get::<KeepBody>().is_some() {
        return res;
    }
    let content_type = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let plain = content_type.starts_with("text/plain");
    let text = content_type.is_empty() || plain || content_type.starts_with("text/html");
    let has_message = res.extensions().get::<Message>().is_some();
    let disclose = crate::error::disclose_server_errors();
    let shows_message = status.is_client_error() || disclose;

    let Some(kept) = kept else {
        // Other clients keep the body; an empty one gets the message.
        if !(text && has_message && shows_message && res.body().size_hint().exact() == Some(0)) {
            return res;
        }
        let (mut parts, _) = res.into_parts();
        let Some(Message(message)) = parts.extensions.remove::<Message>() else {
            unreachable!("checked above")
        };
        set_body_headers(&mut parts.headers, "text/plain; charset=utf-8");
        return Response::from_parts(parts, Body::from(message));
    };
    // A browser gets a page for JSON only when the handler gave words for it.
    if !(text || (content_type.contains("json") && has_message)) {
        return res;
    }

    let (mut parts, body) = res.into_parts();
    let (title, default_message) = error_words(status);
    let message = match parts.extensions.remove::<Message>() {
        Some(Message(m)) if shows_message => m,
        // axum's rejections and the CSRF check say what was wrong in plain text.
        None if plain && shows_message && !parts.headers.contains_key(header::CONTENT_ENCODING) => {
            axum::body::to_bytes(body, MAX_MESSAGE_BYTES)
                .await
                .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    let message = if message.is_empty() {
        default_message.to_owned()
    } else {
        capitalize(&message)
    };

    let mut ctx = Context::new();
    ctx.insert("code", &status.as_u16());
    ctx.insert("title", title);
    ctx.insert("message", &message);
    ctx.insert("path", &kept.path);
    ctx.insert("csp_nonce", &kept.nonce);
    ctx.insert("brand_name", &cfg.brand_name);
    ctx.insert("brand_logo_url", &cfg.brand_logo_url);
    ctx.insert("brand_css", &cfg.brand_css);
    ctx.insert("theme_mode", cfg.theme_mode);
    // The CSRF layer this response came through names the cookie; it ran
    // after `Kept::of`, so the request could not say.
    let csrf = crate::forms::csrf::CsrfCookieSpec::on_response(&parts.extensions)
        .unwrap_or_else(|| kept.csrf.clone());
    let response_headers = parts.headers.clone();
    let mut stamp = |ctx: &mut Context| {
        crate::forms::csrf::stamp_onto_response(&kept.headers, &response_headers, &csrf, ctx)
    };
    let (html, set_cookie) = if asker == Asker::Browser {
        insert_user(cfg, &kept, &mut ctx).await;
        let code = status.as_u16();
        let names = [
            format!("errors/{code}.html"),
            format!("errors/{}xx.html", code / 100),
            "errors/default.html".to_owned(),
        ];
        render(cfg, &names, "error.html", &ctx, &mut stamp)
    } else {
        insert_no_user(&mut ctx);
        if let Some(target) = cfg
            .htmx_target
            .as_deref()
            .and_then(|t| HeaderValue::from_str(t).ok())
        {
            parts.headers.insert("hx-retarget", target);
            if let Ok(swap) = HeaderValue::from_str(&cfg.htmx_swap) {
                parts.headers.insert("hx-reswap", swap);
            }
        }
        render(
            cfg,
            &["errors/partial.html".to_owned()],
            "partial.html",
            &ctx,
            &mut stamp,
        )
    };
    if let Some(c) = set_cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        parts.headers.append(header::SET_COOKIE, c);
    }
    let (content_type, body) = match html {
        Some(html) => ("text/html; charset=utf-8", html),
        None => ("text/plain; charset=utf-8", message),
    };
    set_body_headers(&mut parts.headers, content_type);
    Response::from_parts(parts, Body::from(body))
}

/// The old body's length, encoding, validators and range go with it.
fn set_body_headers(headers: &mut HeaderMap, content_type: &'static str) {
    for h in [
        header::CONTENT_LENGTH,
        header::CONTENT_ENCODING,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CONTENT_RANGE,
    ] {
        headers.remove(h);
    }
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
}

/// Add `Accept` and `HX-Request` to `Vary`, keeping the tokens already there.
fn add_vary(headers: &mut HeaderMap) {
    let present: Vec<String> = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    if present.iter().any(|t| t == "*") {
        return;
    }
    for name in ["Accept", "HX-Request"] {
        if !present.iter().any(|t| t.eq_ignore_ascii_case(name)) {
            headers.append(header::VARY, HeaderValue::from_static(name));
        }
    }
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|a| a.to_ascii_lowercase().contains("text/html"))
}

#[cfg(feature = "admin")]
fn request_nonce(ext: &axum::http::Extensions) -> String {
    ext.get::<crate::csp_nonce::Nonce>()
        .map(|n| n.value().to_owned())
        .unwrap_or_default()
}

#[cfg(not(feature = "admin"))]
fn request_nonce(_ext: &axum::http::Extensions) -> String {
    String::new()
}

fn insert_no_user(ctx: &mut Context) {
    ctx.insert("user", &None::<()>);
    ctx.insert("username", &None::<String>);
    ctx.insert("is_superuser", &false);
}

#[cfg(feature = "tenancy")]
async fn insert_user(cfg: &ErrorPagesLayer, kept: &Kept, ctx: &mut Context) {
    use axum::extract::FromRequestParts as _;
    let mut req = Request::new(());
    *req.headers_mut() = kept.headers.clone();
    *req.uri_mut() = kept.uri.clone();
    *req.extensions_mut() = kept.tenant.clone();
    if req.extensions().is_empty() {
        req.extensions_mut().extend(cfg.user_context.clone());
    }
    let (mut parts, ()) = req.into_parts();
    let Ok(crate::extractors::SessionUser(Some(user))) =
        crate::extractors::SessionUser::from_request_parts(&mut parts, &()).await
    else {
        return insert_no_user(ctx);
    };
    ctx.insert(
        "user",
        &serde_json::json!({ "username": user.username, "is_superuser": user.is_superuser }),
    );
    ctx.insert("username", &user.username);
    ctx.insert("is_superuser", &user.is_superuser);
}

#[cfg(not(feature = "tenancy"))]
async fn insert_user(_cfg: &ErrorPagesLayer, _kept: &Kept, ctx: &mut Context) {
    insert_no_user(ctx);
}

/// The first app template in `names` that exists, else `builtin`, and
/// the CSRF `Set-Cookie` it needs. `stamp` runs only for a template that
/// fails without `csrf_token` / `csrf_input`, so a page with no form sets
/// no cookie; the built-in pages have none. A failed app render falls back
/// to the built-in one; `None` only when that fails too.
fn render(
    cfg: &ErrorPagesLayer,
    names: &[String],
    builtin: &str,
    ctx: &Context,
    stamp: &mut dyn FnMut(&mut Context) -> Option<String>,
) -> (Option<String>, Option<String>) {
    if let Some(tera) = &cfg.templates {
        if let Some(name) = names
            .iter()
            .find(|n| tera.get_template_names().any(|t| t == n.as_str()))
        {
            if let Ok(html) = tera.render(name, ctx) {
                return (Some(html), None);
            }
            let mut with_csrf = ctx.clone();
            let set_cookie = stamp(&mut with_csrf);
            match tera.render(name, &with_csrf) {
                Ok(html) => return (Some(html), set_cookie),
                Err(e) => tracing::error!(
                    target: "rustango::errors",
                    template = %name,
                    error = ?e,
                    "error page failed to render; using the built-in page"
                ),
            }
        }
    }
    let html = builtin_tera()
        .render(builtin, ctx)
        .map_err(|e| {
            tracing::error!(target: "rustango::errors", error = ?e, "built-in error page failed to render");
        })
        .ok();
    (html, None)
}

fn builtin_tera() -> &'static Tera {
    static TERA: OnceLock<Tera> = OnceLock::new();
    TERA.get_or_init(|| {
        let mut tera = crate::template_extensions::html_tera();
        tera.add_raw_templates([
            (
                "_theme_tokens.html",
                include_str!("../styles/theme_tokens.html"),
            ),
            ("error.html", include_str!("error.html")),
            ("partial.html", include_str!("partial.html")),
        ])
        .expect("built-in error pages parse");
        tera
    })
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::routing::get;
    use serde_json::json;
    use tower::ServiceExt as _;

    use super::*;
    use crate::errors::{not_found, PageError};

    fn tera(templates: &[(&str, &str)]) -> Arc<Tera> {
        let mut tera = crate::template_extensions::html_tera();
        tera.add_raw_templates(templates.iter().copied()).unwrap();
        Arc::new(tera)
    }

    fn app(layer: ErrorPagesLayer) -> Router {
        let r = Router::new()
            .route(
                "/forbidden",
                get(|| async { PageError::forbidden("ask Bob") }),
            )
            .route(
                "/broken",
                get(|| async { PageError::server("db: password=hunter2") }),
            )
            .route(
                "/json",
                get(|| async { (StatusCode::BAD_REQUEST, axum::Json(json!({"error": "x"}))) }),
            )
            .route(
                "/kept",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        KeepBody,
                        axum::response::Html("<p>mine</p>"),
                    )
                }),
            )
            .route(
                "/body",
                axum::routing::post(|axum::Json(v): axum::Json<serde_json::Value>| async move {
                    v.to_string()
                }),
            )
            .route(
                "/xss",
                get(|| async { PageError::not_found("<script>alert(1)</script>") }),
            )
            .fallback(not_found);
        #[cfg(feature = "admin")]
        let r = r.route(
            "/admin-denied",
            get(|| async {
                crate::admin::AdminError::Forbidden {
                    table: "post".into(),
                    action: "change",
                }
            }),
        );
        r.error_pages(layer)
    }

    struct Got {
        status: StatusCode,
        body: String,
        headers: HeaderMap,
    }

    async fn call(app: Router, req: Request<Body>) -> Got {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        Got {
            status,
            body: String::from_utf8(body.to_vec()).unwrap(),
            headers,
        }
    }

    fn get_req(path: &str, accept: &str, htmx: bool) -> Request<Body> {
        let mut req = Request::get(path).header(header::ACCEPT, accept);
        if htmx {
            req = req.header("HX-Request", "true");
        }
        req.body(Body::empty()).unwrap()
    }

    /// The reference app's cases: page, htmx fragment, text, 404, hidden 5xx, JSON.
    #[tokio::test]
    async fn errors_are_pages_fragments_or_text_by_who_asks() {
        let app = app(ErrorPagesLayer::new());
        let page = call(app.clone(), get_req("/forbidden", "text/html", false)).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN);
        assert!(page.body.contains("<!doctype html>"));
        assert!(
            page.body.contains("Not allowed") && page.body.contains("Ask Bob"),
            "{}",
            page.body
        );

        let frag = call(app.clone(), get_req("/forbidden", "*/*", true)).await;
        assert!(!frag.body.contains("<!doctype") && frag.body.contains("Ask Bob"));
        assert_eq!(frag.headers.get("hx-retarget").unwrap(), "#flash");
        assert_eq!(
            frag.headers.get("hx-reswap").unwrap(),
            "innerHTML show:window:top"
        );

        let text = call(app.clone(), get_req("/forbidden", "*/*", false)).await;
        assert_eq!(text.body, "ask Bob", "other clients keep the body");

        let missing = call(app.clone(), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert!(missing.body.contains("Not found") && missing.body.contains("&#x2F;nowhere"));

        let broken = call(app.clone(), get_req("/broken", "text/html", false)).await;
        assert_eq!(broken.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(broken.body.contains("Something went wrong"));
        assert!(
            !broken.body.contains("hunter2"),
            "5xx cause stays in the log"
        );
        let broken = call(app.clone(), get_req("/broken", "*/*", false)).await;
        assert!(!broken.body.contains("hunter2"));

        let json = call(app, get_req("/json", "text/html", false)).await;
        assert_eq!(json.body, r#"{"error":"x"}"#);
    }

    #[tokio::test]
    async fn head_and_keep_body_pass_through() {
        let app = app(ErrorPagesLayer::new());
        let kept = call(app.clone(), get_req("/kept", "text/html", false)).await;
        assert_eq!(kept.body, "<p>mine</p>");
        let head = Request::head("/nowhere")
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap();
        let head = call(app, head).await;
        assert_eq!(head.status, StatusCode::NOT_FOUND);
        assert!(head.body.is_empty() && head.headers.get(header::CONTENT_TYPE).is_none());
    }

    /// axum's plain-text rejection is the message, first letter up.
    #[tokio::test]
    async fn a_rejection_shows_its_text() {
        let app = app(ErrorPagesLayer::new());
        let req = Request::post("/body")
            .header(header::ACCEPT, "text/html")
            .body(Body::from("{}"))
            .unwrap();
        let got = call(app, req).await;
        assert_eq!(got.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            got.body
                .contains("Expected request with `Content-Type: application&#x2F;json`"),
            "{}",
            got.body
        );
    }

    #[tokio::test]
    async fn the_method_not_allowed_is_dressed() {
        let req = Request::post("/forbidden")
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap();
        let got = call(app(ErrorPagesLayer::new()), req).await;
        assert_eq!(got.status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(got.body.contains("Not supported"));
    }

    #[tokio::test]
    async fn values_are_escaped() {
        let got = call(
            app(ErrorPagesLayer::new()),
            get_req("/xss", "text/html", false),
        )
        .await;
        assert!(!got.body.contains("<script>alert"), "{}", got.body);
        assert!(got.body.contains("&lt;script&gt;"));
    }

    /// A JSON error with words for a browser (the admin's denials) gets a
    /// page; an API client still gets the JSON.
    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn an_admin_denial_is_a_page_for_a_browser_only() {
        let app = app(ErrorPagesLayer::new());
        let page = call(app.clone(), get_req("/admin-denied", "text/html", false)).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN);
        assert!(page.body.contains("<!doctype html>") && page.body.contains("Ask an admin"));
        let api = call(app, get_req("/admin-denied", "application/json", false)).await;
        assert!(api.body.contains("permission denied") && !api.body.contains("<!doctype"));
    }

    #[tokio::test]
    async fn overrides_go_code_class_default() {
        let layer = ErrorPagesLayer::new().templates(tera(&[
            ("errors/404.html", "code-page {{ code }}"),
            ("errors/4xx.html", "class-page {{ code }}"),
            ("errors/default.html", "default-page {{ code }}"),
            ("errors/partial.html", "partial {{ message }}"),
        ]));
        let app = app(layer);
        let p = call(app.clone(), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(p.body, "code-page 404");
        let p = call(app.clone(), get_req("/forbidden", "text/html", false)).await;
        assert_eq!(p.body, "class-page 403");
        let p = call(app.clone(), get_req("/broken", "text/html", false)).await;
        assert_eq!(p.body, "default-page 500");
        let p = call(app, get_req("/forbidden", "*/*", true)).await;
        assert_eq!(p.body, "partial Ask Bob");

        let only_default =
            ErrorPagesLayer::new().templates(tera(&[("errors/default.html", "d {{ code }}")]));
        let p = call(
            tests::app(only_default),
            get_req("/nowhere", "text/html", false),
        )
        .await;
        assert_eq!(p.body, "d 404");
    }

    #[tokio::test]
    async fn a_failing_override_falls_back_to_the_built_in_page() {
        let layer =
            ErrorPagesLayer::new().templates(tera(&[("errors/404.html", "{{ nope.nope }}")]));
        let got = call(app(layer), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND);
        assert!(got.body.contains("<!doctype html>") && got.body.contains("Not found"));
    }

    #[tokio::test]
    async fn the_page_gets_csrf_path_and_user_keys() {
        let layer = ErrorPagesLayer::new().templates(tera(&[(
            "errors/default.html",
            "token={{ csrf_token }} path={{ path }} title={{ title }} user={% if user %}yes{% else %}anon{% endif %} su={{ is_superuser }}",
        )]));
        let got = call(app(layer), get_req("/nowhere", "text/html", false)).await;
        let token = got
            .body
            .split("token=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        assert!(token.len() > 10, "{}", got.body);
        assert!(
            got.body
                .contains("path=&#x2F;nowhere title=Not found user=anon su=false"),
            "{}",
            got.body
        );
        let cookie = got
            .headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            cookie.contains(token),
            "the stamped token rides on the cookie"
        );
    }

    #[cfg(feature = "_http_layers")]
    #[tokio::test]
    async fn framework_layers_inside_it_are_dressed() {
        use crate::body_limit::{BodyLimitLayer, BodyLimitRouterExt as _};
        use crate::request_timeout::{RequestTimeoutLayer, RequestTimeoutRouterExt as _};
        let app = Router::new()
            .route("/up", axum::routing::post(|| async { "ok" }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .request_timeout(RequestTimeoutLayer::new(std::time::Duration::from_millis(
                20,
            )))
            .body_limit(BodyLimitLayer::new(4))
            .error_pages(ErrorPagesLayer::new());
        let req = Request::post("/up")
            .header(header::ACCEPT, "text/html")
            .header(header::CONTENT_LENGTH, "100")
            .body(Body::from(vec![b'x'; 100]))
            .unwrap();
        let got = call(app.clone(), req).await;
        assert_eq!(got.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(got.body.contains("Too large"), "{}", got.body);
        let got = call(app, get_req("/slow", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::GATEWAY_TIMEOUT);
        assert!(got.body.contains("Took too long"));
    }

    #[cfg(feature = "_http_layers")]
    #[tokio::test]
    async fn a_rate_limit_429_is_dressed_and_keeps_retry_after() {
        use crate::rate_limit::{RateLimitLayer, RateLimitRouterExt as _};
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .rate_limit(RateLimitLayer::global(
                1,
                std::time::Duration::from_secs(60),
            ))
            .error_pages(ErrorPagesLayer::new());
        assert_eq!(
            call(app.clone(), get_req("/", "text/html", false))
                .await
                .status,
            StatusCode::OK
        );
        let got = call(app.clone(), get_req("/", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(got.body.contains("Slow down"), "{}", got.body);
        assert!(got.headers.contains_key(header::RETRY_AFTER));
        let api = call(app, get_req("/", "application/json", false)).await;
        assert!(api.body.starts_with('{'), "API clients keep the JSON");
    }

    #[cfg(any(feature = "manage", feature = "tenancy", feature = "runserver"))]
    #[tokio::test]
    async fn a_panic_inside_it_is_a_dressed_500() {
        let routes = Router::new().route(
            "/boom",
            get(|| async {
                if std::hint::black_box(true) {
                    panic!("boom with password=hunter2");
                }
                "unreachable"
            }),
        );
        let app = crate::panic_guard::catch_panics(routes).error_pages(ErrorPagesLayer::new());
        let got = call(app, get_req("/boom", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(got.body.contains("Something went wrong") && !got.body.contains("hunter2"));
    }

    #[cfg(feature = "admin")]
    #[tokio::test]
    async fn the_inline_style_carries_the_csp_nonce() {
        use crate::csp_nonce::{CspNonceLayer, CspNonceRouterExt as _};
        let app = app(ErrorPagesLayer::new()).csp_nonce(CspNonceLayer::default());
        let got = call(app, get_req("/nowhere", "text/html", false)).await;
        let nonce = got
            .body
            .split("<style nonce=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(nonce.len() >= 16, "{}", got.body);
    }

    #[cfg(feature = "config")]
    #[tokio::test]
    async fn settings_enable_it_and_name_the_template_root() {
        let mut s = crate::config::Settings::default();
        assert!(
            ErrorPagesLayer::from_settings(&s).is_none(),
            "off by default"
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("errors")).unwrap();
        std::fs::write(dir.path().join("errors/404.html"), "app 404 {{ code }}").unwrap();
        s.errors.enabled = Some(true);
        s.errors.templates = Some(dir.path().display().to_string());
        s.errors.htmx_target = Some(String::new());
        s.brand.name = Some("Acme".into());
        let layer = ErrorPagesLayer::from_settings(&s).unwrap();
        let got = call(app(layer.clone()), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(got.body, "app 404 404");
        let got = call(
            app(layer.clone()),
            get_req("/forbidden", "text/html", false),
        )
        .await;
        assert!(
            got.body.contains("Acme"),
            "the built-in page shows the brand"
        );
        let got = call(app(layer), get_req("/forbidden", "*/*", true)).await;
        assert!(got.headers.get("hx-retarget").is_none());
    }

    /// Without the layer nothing changes: the empty 404 stays empty.
    #[tokio::test]
    async fn without_the_layer_responses_are_unchanged() {
        let app = Router::new().fallback(not_found);
        let got = call(app, get_req("/nowhere", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND);
        assert!(got.body.is_empty());
    }

    fn set_cookies(h: &HeaderMap) -> Vec<String> {
        h.get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    }

    /// An app CSRF layer with its own cookie name, inside the error pages.
    fn csrf_app() -> Router {
        let csrf = crate::forms::csrf::with_config(crate::forms::csrf::CsrfConfig {
            cookie_name: "app_csrf".into(),
            secure: false,
            ..Default::default()
        });
        Router::new()
            .route("/form", axum::routing::post(|| async { "saved" }))
            .fallback(not_found)
            .layer(csrf)
            .error_pages(
                ErrorPagesLayer::new()
                    .templates(tera(&[("errors/default.html", "token={{ csrf_token }}")])),
            )
    }

    /// The page's token and the one cookie agree, under the app's name.
    fn one_app_cookie(got: &Got) {
        let cookies = set_cookies(&got.headers);
        assert_eq!(cookies.len(), 1, "{cookies:?}");
        let value = cookies[0]
            .strip_prefix("app_csrf=")
            .expect("app's cookie name");
        let value = value.split(';').next().unwrap();
        assert_eq!(got.body, format!("token={value}"));
    }

    #[tokio::test]
    async fn a_csrf_403_sets_one_cookie_under_the_apps_name() {
        let req = Request::post("/form")
            .header(header::ACCEPT, "text/html")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("a=b"))
            .unwrap();
        let got = call(csrf_app(), req).await;
        assert_eq!(got.status, StatusCode::FORBIDDEN);
        one_app_cookie(&got);
    }

    /// The CSRF layer seeds its cookie on the 404; the page reuses that token.
    #[tokio::test]
    async fn a_page_behind_the_csrf_layer_reuses_its_cookie() {
        let got = call(csrf_app(), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(got.status, StatusCode::NOT_FOUND);
        one_app_cookie(&got);
    }

    /// A first visit's built-in 404 has no form, so it sets no cookie.
    #[tokio::test]
    async fn the_built_in_page_sets_no_csrf_cookie() {
        let got = call(
            app(ErrorPagesLayer::new()),
            get_req("/nowhere", "text/html", false),
        )
        .await;
        assert!(set_cookies(&got.headers).is_empty(), "{:?}", got.headers);
        let partial = call(
            app(ErrorPagesLayer::new()),
            get_req("/nowhere", "*/*", true),
        )
        .await;
        assert!(set_cookies(&partial.headers).is_empty());
    }

    /// A template without a form sets no cookie either.
    #[tokio::test]
    async fn a_template_without_csrf_sets_no_cookie() {
        let layer =
            ErrorPagesLayer::new().templates(tera(&[("errors/default.html", "plain {{ code }}")]));
        let got = call(app(layer), get_req("/nowhere", "text/html", false)).await;
        assert_eq!(got.body, "plain 404");
        assert!(set_cookies(&got.headers).is_empty());
    }

    /// Every inspected error varies on Accept/HX-Request, passthroughs too,
    /// merged with the handler's own Vary.
    #[tokio::test]
    async fn errors_vary_on_accept_and_hx_request() {
        let app = Router::new()
            .route(
                "/json",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        [(header::VARY, "Origin")],
                        axum::Json(json!({"e": 1})),
                    )
                }),
            )
            .fallback(not_found)
            .error_pages(ErrorPagesLayer::new());
        for path in ["/json", "/nowhere"] {
            let got = call(app.clone(), get_req(path, "application/json", false)).await;
            let vary: Vec<String> = got
                .headers
                .get_all(header::VARY)
                .iter()
                .flat_map(|v| {
                    v.to_str()
                        .unwrap()
                        .split(',')
                        .map(|t| t.trim().to_ascii_lowercase())
                        .collect::<Vec<_>>()
                })
                .collect();
            for want in ["accept", "hx-request"] {
                assert_eq!(
                    vary.iter().filter(|t| *t == want).count(),
                    1,
                    "{path}: {vary:?}"
                );
            }
            if path == "/json" {
                assert!(vary.contains(&"origin".to_owned()), "{vary:?}");
            }
        }
    }

    /// A replaced body drops the validators and range of the old one.
    #[tokio::test]
    async fn a_replaced_body_drops_etag_last_modified_and_range() {
        let app = Router::new()
            .route(
                "/gone",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        [
                            (header::ETAG, "\"abc\""),
                            (header::LAST_MODIFIED, "Wed, 21 Oct 2015 07:28:00 GMT"),
                            (header::CONTENT_RANGE, "bytes 0-1/2"),
                        ],
                        "gone",
                    )
                }),
            )
            .error_pages(ErrorPagesLayer::new());
        let got = call(app, get_req("/gone", "text/html", false)).await;
        assert!(got.body.contains("<!doctype html>"));
        for h in [header::ETAG, header::LAST_MODIFIED, header::CONTENT_RANGE] {
            assert!(got.headers.get(&h).is_none(), "{h} kept");
        }
    }
}
