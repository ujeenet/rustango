# Middleware

Middleware is code that runs **around** every request — before your handler
sees it and after it produces a response. It's where cross-cutting concerns
live: logging, rate limiting, security headers, CSRF, locale and timezone
resolution. **Rustango** ships a deep catalog of ready-made middleware and
makes writing your own a few lines. If you come from Django, this is the
`MIDDLEWARE` list; from Express, it's `app.use()`; from Laravel, it's the HTTP
kernel — same idea, attached to your router.

[![Middleware in rustango: a request flows down through a stack of tower layers (request-id, locale, security headers, CSRF) into the handler and back up through the response side of each layer](img/middleware.png)](img/middleware.png)

> **Source:** middleware is built on [`tower::Layer`](https://docs.rs/tower) +
> `axum::middleware::from_fn`. Built-ins live across `rustango::{access_log,
> rate_limit, cors, security_headers, request_id, …}`, plus
> `rustango::forms::csrf` (the `csrf` feature) and `rustango::i18n`
> (`middleware::LocaleMiddleware`, `timezone`). Most are on by default.
>
> **Runnable version:** every snippet below is copied from the tested example
> at `crates/rustango/examples/getting_started_blog/tests/middleware.rs`
> (locale, timezone, security headers, CSRF — all DB-free). Run it with
> `cargo test -p getting_started_blog --test middleware`.

> **New to a term here?** *Middleware*, *layer*, *CSRF*, *locale* — the
> [glossary](glossary.md) explains them in plain language.

## Table of contents

- [How middleware works in Rustango](#how-middleware-works-in-rustango)
- [Ordering matters](#ordering-matters)
- [The built-in catalog](#the-built-in-catalog)
- [Locale-aware middleware](#locale-aware-middleware)
- [Timezone-aware middleware](#timezone-aware-middleware)
- [Security headers](#security-headers)
- [CSRF protection](#csrf-protection)
- [Writing your own middleware](#writing-your-own-middleware)
- [See also](#see-also)

---

## How middleware works in Rustango

There are two shapes, and you'll use both:

1. **A `tower::Layer`** — a reusable, configurable middleware struct. Every
   built-in is one (`SecurityHeadersLayer`, `RateLimitLayer`, …). You attach a
   layer with axum's `.layer(...)`, or — for most built-ins — with an
   **extension-trait one-liner** that reads better:

   ```rust
   use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt};

   // These two are equivalent; the second is the ergonomic form.
   let app = router.layer(SecurityHeadersLayer::strict());
   let app = router.security_headers(SecurityHeadersLayer::strict());
   ```

   Each built-in module exports a `…RouterExt` trait (`SecurityHeadersRouterExt`,
   `RateLimitRouterExt`, …). Bring it into scope and you get a `.security_headers()`
   / `.rate_limit()` / `.cors()` method directly on `Router`. That's the
   idiomatic way to wire a stack:

   ```rust
   let app = router
       .request_id(RequestIdLayer::default())
       .access_log(AccessLogLayer::default())
       .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
       .cors(CorsLayer::new().allow_origins(vec!["https://app.example.com"]))
       .security_headers(SecurityHeadersLayer::strict());
   ```

2. **A function via `axum::middleware::from_fn`** — the quickest way to write a
   one-off. You get the `Request` and a `Next`; you call `next.run(req)` to
   continue, and you can do work on either side of that call. The
   [timezone example](#timezone-aware-middleware) below is exactly this.

Both compose freely — a `from_fn` middleware *is* a layer, so it stacks with
the built-ins in the same `.layer(...)` chain.

---

## Ordering matters

A layer wraps everything added **before** it, so the **last** layer you attach
is the **outermost** — it runs first on the way in and last on the way out.
Picture the stack as an onion; the request travels down to the handler and the
response travels back up:

```text
            ┌──────────────────────────────────────┐
request  →  │ security_headers   (added last = top) │  → response
            │   ┌────────────────────────────────┐  │
            │   │ rate_limit                     │  │
            │   │   ┌──────────────────────────┐ │  │
            │   │   │ request_id (added first) │ │  │
            │   │   │      → handler →         │ │  │
            │   │   └──────────────────────────┘ │  │
            │   └────────────────────────────────┘  │
            └──────────────────────────────────────┘
```

Practical consequences:

- **Put `request_id` / `access_log` near the bottom** (added first) so the
  correlation id and the log span cover everything that runs after them.
- **Put rejection-cheap guards near the top** (added last) — `allowed_hosts`,
  `rate_limit`, `body_limit` — so a blocked request is dropped before it
  reaches the expensive inner layers.
- **`security_headers` outermost** so the headers are stamped on *every*
  response, including ones short-circuited by an inner layer (a 429, a 403).

---

## The built-in catalog

Every entry is a `tower::Layer` with a matching `…RouterExt` one-liner unless
noted. Bring the module's `…RouterExt` trait into scope to get the method.

| Concern | Layer | Wire it with |
| --- | --- | --- |
| **Security & access** | | |
| Security headers (HSTS, XFO, CSP…) | `SecurityHeadersLayer` | `.security_headers(..)` |
| CSRF (double-submit cookie) | `CsrfLayer` | `.layer(csrf::layer())` |
| CORS | `CorsLayer` | `.cors(..)` |
| Host allow-list | `AllowedHostsLayer` | `.allowed_hosts(..)` |
| IP allow/block | `IpFilterLayer` | `.ip_filter(..)` |
| Force HTTPS | `SslRedirectLayer` | `.ssl_redirect(..)` |
| CSP nonce per request | `CspNonceLayer` | `.csp_nonce(..)` |
| HMAC request signing | `HmacAuthLayer` | `.layer(..)` |
| **Traffic & resilience** | | |
| Rate limit (per-IP/global) | `RateLimitLayer` | `.rate_limit(..)` |
| Distributed rate limit (cache-backed) | `CacheRateLimitLayer` | `.cache_rate_limit(..)` |
| Request timeout | `RequestTimeoutLayer` | `.request_timeout(..)` |
| Max body size | `BodyLimitLayer` | `.body_limit(..)` |
| Maintenance mode (503) | `MaintenanceLayer` | `.maintenance(..)` |
| Idempotency keys | `IdempotencyLayer` | `.idempotency(..)` |
| Restrict HTTP methods | `MethodRestrictLayer` | `.require_get()` / `.require_post()` / `.require_safe()` |
| **Observability** | | |
| Request id (`X-Request-Id`) | `RequestIdLayer` | `.request_id(..)` |
| Access log (PII-redacted) | `AccessLogLayer` | `.access_log(..)` |
| `tracing` spans | `TracingLayer` | `.layer(..)` |
| `Server-Timing` header | `ServerTimingLayer` | `.server_timing(..)` |
| Real client IP (behind proxies) | `RealIpLayer` | `.real_ip(..)` |
| **Content & response** | | |
| gzip/br compression | `CompressionLayer` | `.compression(..)` |
| ETag / conditional GET | `EtagLayer` | `.etag(..)` |
| Trailing-slash redirect | `TrailingSlashLayer` | `.trailing_slash(..)` |
| Page caching | `CachePageLayer` | `.layer(..)` |
| **Localization** | | |
| Locale negotiation | `LocaleMiddleware` | `.layer(..)` (+ `ActiveLocale` extractor) |
| Active timezone | *(compose with `from_fn`)* | see [below](#timezone-aware-middleware) |
| **Development** | | |
| Live reload | `LiveReloadLayer` | `.livereload(..)` |
| Debug panel | `DebugPanelLayer` | `.debug_panel(..)` |

The next sections walk the ones the request asked for in detail.

---

## Locale-aware middleware

`LocaleMiddleware` resolves one locale per request and injects it into the
request so any handler can read it. The pick order is Django's: **cookie →
`Accept-Language` → default**. The first locale you list is the default unless
you override it.

```rust
use rustango::i18n::middleware::{ActiveLocale, LocaleMiddleware};

// First entry is the default; `.default("en")` is explicit here. `ar`
// (Arabic) is included so we can prove the RTL convenience too.
let layer = LocaleMiddleware::new(&["en", "fr", "ar"]).default("en");

let app = Router::new()
    .route(
        "/",
        // The extractor pulls the locale the layer chose for THIS request.
        get(|loc: ActiveLocale| async move { format!("{} {}", loc.0, loc.direction()) }),
    )
    .layer(layer);
```

Handlers read the result with the `ActiveLocale` extractor. It's
`Infallible` — if no middleware ran it falls back to `"en"` — and it carries
RTL helpers so templates can branch on bidirectionality:

```rust
loc.0            // the picked locale, e.g. "fr"
loc.direction()  // "ltr" / "rtl" — feed straight into <html dir="…">
loc.is_rtl()     // true for ar, he, fa, …
```

The cookie name defaults to `django_language` (Django-compatible); change it
with `.cookie_name("…")`, or pass `None` to disable cookie lookup entirely.
The resolution precedence, verified end-to-end:

```rust
// Cookie says fr, Accept-Language says ar → the cookie wins.
//   Cookie: django_language=fr ; Accept-Language: ar   → "fr ltr"
//
// No cookie → negotiate Accept-Language (ar is RTL).
//   Accept-Language: ar,en;q=0.5                        → "ar rtl"
//
// Neither cookie nor a supported language → the default locale.
//   (no headers)                                        → "en ltr"
```

For URL-prefix locales (`/en/about`, `/fr/about`) instead of header
negotiation, mount a sub-router per locale with `Router::nest("/fr", …)` and
inject the locale there.

---

## Timezone-aware middleware

There is deliberately **no timezone layer**. Instead the framework gives you a
task-local active offset (`rustango::i18n::timezone`) and a header/cookie
decoder, and you compose a one-line middleware that activates it — the
canonical "write your own" example. This mirrors Django's `USE_TZ=True`: store
UTC, render in the user's local clock.

```rust
use axum::middleware::{from_fn, Next};
use rustango::i18n::timezone::{current_offset, from_request_headers, with_offset};

/// Per-request middleware: decode the client's UTC offset from the
/// `tz_offset` cookie (or a `Time-Zone:` header), then run the rest of the
/// stack with that offset active so `current_offset()` / the `localtime`
/// Tera filter see it. Falls back to UTC when nothing parseable is sent.
async fn timezone_mw(req: Request<Body>, next: Next) -> Response {
    match from_request_headers(req.headers(), "tz_offset") {
        Some(offset) => with_offset(offset, next.run(req)).await,
        None => next.run(req).await,
    }
}

let app = Router::new()
    .route(
        "/now",
        // The handler runs inside the activated scope, so the task-local
        // offset is visible here (and inside any `.await` it makes).
        get(|| async { current_offset().local_minus_utc().to_string() }),
    )
    .layer(from_fn(timezone_mw));
```

`with_offset` installs the offset in a **`tokio::task_local`**, so it survives
`.await` points and thread hops within the request — unlike a thread-local, it
won't vanish when tokio reschedules the task. Inside the scope:

- `current_offset()` returns the active `FixedOffset` (UTC outside any scope),
- `localtime(utc_dt)` converts a stored UTC `DateTime` to it,
- the `{{ ts | localtime }}` Tera filter (register with
  `timezone::register_filters`) renders in it automatically.

`from_request_headers` accepts the `tz_offset` cookie first, then a
`Time-Zone:` (or `X-Timezone:`) header. The accepted formats are flexible —
`"+05:30"`, `"+0530"`, `"Z"`/`"UTC"`, or **signed integer minutes** (`"330"`,
`"-300"`), the shape JS's `Date.getTimezoneOffset()` produces. The cookie is
typically set by a tiny snippet on page load:

```html
<script>
  // getTimezoneOffset() is positive west-of-UTC, so flip the sign.
  const minutes = -new Date().getTimezoneOffset();
  document.cookie = `tz_offset=${minutes};path=/;max-age=31536000`;
</script>
```

Behavior, verified:

```rust
// Cookie: tz_offset=330  (UTC+05:30)   → current_offset() = +19800s
// Header: Time-Zone: -300 (UTC-05:00)  → current_offset() = -18000s
// nothing sent                          → current_offset() = 0 (UTC)
```

> **Note — `FixedOffset`, not IANA.** This models the active timezone as a
> fixed offset, which is everything a *single point in time* needs and avoids
> the ~3 MB `chrono-tz` database. It does **not** handle DST transitions. For
> full IANA support, parse the user's `Tz` with `chrono-tz` at request time and
> pass `tz.offset_from_utc_datetime(...)` into `with_offset` — the middleware
> shape above is unchanged.

---

## Security headers

`SecurityHeadersLayer` stamps the standard browser-hardening headers — HSTS,
`X-Frame-Options`, `X-Content-Type-Options`, Referrer-Policy, and an optional
Content-Security-Policy — onto every response, in one line.

```rust
use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};

let app = Router::new()
    .route("/", get(|| async { "ok" }))
    .security_headers(SecurityHeadersLayer::strict().csp(CspBuilder::strict_starter().build()));
```

That hardens every response:

```rust
// strict-transport-security: max-age=31536000; includeSubDomains; preload
// x-content-type-options:     nosniff
// x-frame-options:            DENY
// content-security-policy:    <built from CspBuilder::strict_starter()>
```

Three presets cover the common cases:

| Preset | Use for | HSTS | `X-Frame-Options` |
| --- | --- | --- | --- |
| `strict()` | production | 1 year + preload | `DENY` |
| `relaxed()` | embeddable pages | 1 year | `SAMEORIGIN` |
| `dev()` | local dev | *(omitted)* | — |

Use `dev()` locally — HSTS would otherwise pin your browser to HTTPS for a year
on `localhost`. Build a CSP fluently with `CspBuilder` (`.default_src(..)`,
`.script_src(..)`, …, `.build()`), and roll it out safely with
`.csp_report_only(true)` + `.csp_report_uri("/csp-report")` before enforcing.

---

## CSRF protection

CSRF (cross-site request forgery) is when another site tricks a logged-in
user's browser into submitting to your app. **Rustango** defends with the
**double-submit-cookie** pattern, in `rustango::forms::csrf` (behind the `csrf`
feature, turned on automatically by `admin`).

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/form", get(|| async { "render form" }))
    .route("/submit", post(|| async { "accepted" }))
    .layer(csrf::layer());
```

A safe method (GET/HEAD/OPTIONS) mints a `rustango_csrf` cookie. An unsafe
method (POST/PUT/PATCH/DELETE) must echo that cookie's value back, either in the
`X-CSRF-Token` header or the `_csrf` form field; a mismatch is `403 Forbidden`:

```rust
// GET /form                            → 200 + Set-Cookie: rustango_csrf=…
//
// POST /submit  (no token)             → 403 Forbidden
//
// POST /submit  with both:
//   Cookie:        rustango_csrf=<t>
//   X-CSRF-Token:  <t>                 → 200 OK   (double-submit matches)
```

In Tera templates, `{{ csrf_token }}` gives the raw token and `{{ csrf_input }}`
a ready-made hidden `<input name="_csrf">` — drop one in every form. Override
the cookie/header names or the `Secure` flag with `csrf::with_config(CsrfConfig)`;
for SPA setups, add `.with_trusted_origins([...])` to enable the Origin-header
defense-in-depth check on top of the token. The auto-admin enables CSRF on
every mutation with no opt-out.

---

## Writing your own middleware

You saw the quick form already — the [timezone middleware](#timezone-aware-middleware)
is a complete `from_fn` example. Use `from_fn` whenever the logic is app-specific
and you don't need to configure it:

```rust
use axum::middleware::{from_fn, Next};

async fn add_app_version(req: Request<Body>, next: Next) -> Response {
    let mut resp = next.run(req).await;          // run the rest of the stack
    resp.headers_mut()
        .insert("X-App-Version", HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
    resp                                          // …then touch the response
}

let app = router.layer(from_fn(add_app_version));
```

Reach for a full **`tower::Layer`** when the middleware is reusable and
configurable — when you'd ship it as part of a library. The pattern is a small
`Layer` struct that builds a `Service`, plus (by convention) a `…RouterExt`
extension trait for the one-liner. `rustango::request_id` is the smallest
complete reference to copy:

- `RequestIdLayer` — the configurable layer (`::default()`, `.always_generate()`),
- `RequestIdService<S>` — wraps the inner service; reads/sets the `X-Request-Id`
  header inside `call`,
- `RequestId` — a `FromRequestParts` extractor so handlers can read the id,
- `RequestIdRouterExt` — gives `Router::request_id(layer)`.

`LocaleMiddleware` (read in `crates/rustango/src/i18n/middleware.rs`) is a second
worked example — same four pieces, plus a pure `.pick(&req)` method that's unit-
testable without spinning up a server. Model new layers on either.

---

## See also

- [Security guide](security.md) — the full defense-in-depth stack and a
  deploy-time checklist (`manage check --deploy`).
- [Authentication](auth-sessions.md) — auth backends and the `CurrentUser`
  middleware build on the same layer mechanism.
- [URLs & routing](urls.md) — where layers attach to routers and sub-routers.
