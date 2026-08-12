# Middleware

Middleware ist Code, der **rund um** jeden Request läuft — bevor ihn dein
Handler sieht und nachdem er eine Response erzeugt hat. Hier leben die
Querschnittsthemen: Logging, Rate Limiting, Security-Header, CSRF, Auflösung von
Locale und Zeitzone. **Rustango** liefert einen umfangreichen Katalog
gebrauchsfertiger Middleware und macht das Schreiben eigener zu einer Sache
weniger Zeilen. Wenn du von Django kommst, ist das die `MIDDLEWARE`-Liste; von
Express `app.use()`; von Laravel der HTTP-Kernel — dieselbe Idee, an deinen
Router angehängt.

[![Middleware in Rustango: a request flows down through a stack of tower layers (request-id, locale, security headers, CSRF) into the handler and back up through the response side of each layer](img/middleware.png)](img/middleware.png)

> **Quelle:** Middleware baut auf [`tower::Layer`](https://docs.rs/tower) +
> `axum::middleware::from_fn` auf. Die eingebauten Bausteine verteilen sich über
> `rustango::{access_log, rate_limit, cors, security_headers, request_id, …}`,
> plus `rustango::forms::csrf` (die `csrf`-Feature) und `rustango::i18n`
> (`middleware::LocaleMiddleware`, `timezone`). Die meisten sind standardmäßig
> aktiv.
>
> **Ausführbare Version:** jeder Codeausschnitt unten ist aus dem getesteten
> Beispiel unter
> `crates/rustango/examples/getting_started_blog/tests/middleware.rs`
> kopiert (Locale, Zeitzone, Security-Header, CSRF — alle ohne Datenbank).
> Führe es mit `cargo test -p getting_started_blog --test middleware` aus.

> **Ein Begriff hier neu für dich?** *Middleware*, *Layer*, *CSRF*, *Locale* —
> das [Glossar](glossary.md) erklärt sie in einfachen Worten.

## Inhaltsverzeichnis

- [Wie Middleware in Rustango funktioniert](#how-middleware-works-in-rustango)
- [Die Reihenfolge zählt](#ordering-matters)
- [Der eingebaute Katalog](#the-built-in-catalog)
- [Locale-bewusste Middleware](#locale-aware-middleware)
- [Zeitzonen-bewusste Middleware](#timezone-aware-middleware)
- [Security-Header](#security-headers)
- [CSRF-Schutz](#csrf-protection)
- [Eigene Middleware schreiben](#writing-your-own-middleware)
- [Siehe auch](#see-also)

---

## Wie Middleware in Rustango funktioniert

Es gibt zwei Formen, und du wirst beide verwenden:

1. **Ein `tower::Layer`** — ein wiederverwendbares, konfigurierbares
   Middleware-Struct. Jeder eingebaute Baustein ist einer
   (`SecurityHeadersLayer`, `RateLimitLayer`, …). Du hängst einen Layer mit
   axums `.layer(...)` an, oder — für die meisten Bausteine — mit einem
   **Einzeiler über einen Extension-Trait**, der sich besser liest:

   ```rust
   use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt};

   // These two are equivalent; the second is the ergonomic form.
   let app = router.layer(SecurityHeadersLayer::strict());
   let app = router.security_headers(SecurityHeadersLayer::strict());
   ```

   Jedes eingebaute Modul exportiert einen `…RouterExt`-Trait
   (`SecurityHeadersRouterExt`, `RateLimitRouterExt`, …). Bring ihn in den
   Scope und du erhältst eine Methode `.security_headers()` / `.rate_limit()` /
   `.cors()` direkt auf `Router`. Das ist der idiomatische Weg, einen Stack zu
   verdrahten:

   ```rust
   let app = router
       .request_id(RequestIdLayer::default())
       .access_log(AccessLogLayer::default())
       .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
       .cors(CorsLayer::new().allow_origins(vec!["https://app.example.com"]))
       .security_headers(SecurityHeadersLayer::strict());
   ```

2. **Eine Funktion über `axum::middleware::from_fn`** — der schnellste Weg, eine
   einmalige zu schreiben. Du erhältst den `Request` und ein `Next`; du rufst
   `next.run(req)` auf, um fortzufahren, und du kannst auf beiden Seiten dieses
   Aufrufs arbeiten. Das [Zeitzonen-Beispiel](#timezone-aware-middleware) unten
   ist genau das.

Beide lassen sich frei komponieren — eine `from_fn`-Middleware *ist* ein Layer,
sie stapelt sich also mit den eingebauten Bausteinen in derselben
`.layer(...)`-Kette.

---

## Die Reihenfolge zählt

Ein Layer umschließt alles, was **vor** ihm hinzugefügt wurde, daher ist der
**zuletzt** angehängte Layer der **äußerste** — er läuft auf dem Weg hinein
zuerst und auf dem Weg hinaus zuletzt. Stell dir den Stack als Zwiebel vor; der
Request wandert hinab zum Handler und die Response wandert wieder hinauf:

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

Praktische Konsequenzen:

- **Platziere `request_id` / `access_log` nahe am Boden** (zuerst hinzugefügt),
  damit die Korrelations-ID und der Log-Span alles abdecken, was nach ihnen
  läuft.
- **Platziere abweisungsgünstige Guards nahe der Spitze** (zuletzt hinzugefügt)
  — `allowed_hosts`, `rate_limit`, `body_limit` — damit ein blockierter Request
  verworfen wird, bevor er die teuren inneren Layer erreicht.
- **`security_headers` am äußersten**, damit die Header auf *jede* Response
  gestempelt werden, auch auf solche, die von einem inneren Layer
  kurzgeschlossen werden (ein 429, ein 403).

---

## Der eingebaute Katalog

Jeder Eintrag ist ein `tower::Layer` mit einem passenden
`…RouterExt`-Einzeiler, sofern nicht anders vermerkt. Bring den
`…RouterExt`-Trait des Moduls in den Scope, um die Methode zu erhalten.

| Thema | Layer | Verdrahten mit |
| --- | --- | --- |
| **Sicherheit & Zugriff** | | |
| Security-Header (HSTS, XFO, CSP…) | `SecurityHeadersLayer` | `.security_headers(..)` |
| CSRF (Double-Submit-Cookie) | `CsrfLayer` | `.layer(csrf::layer())` |
| CORS | `CorsLayer` | `.cors(..)` |
| Host-Allowlist | `AllowedHostsLayer` | `.allowed_hosts(..)` |
| IP zulassen/blockieren | `IpFilterLayer` | `.ip_filter(..)` |
| HTTPS erzwingen | `SslRedirectLayer` | `.ssl_redirect(..)` |
| CSP-Nonce pro Request | `CspNonceLayer` | `.csp_nonce(..)` |
| HMAC-Request-Signierung | `HmacAuthLayer` | `.layer(..)` |
| **Traffic & Resilienz** | | |
| Rate Limit (pro IP/global) | `RateLimitLayer` | `.rate_limit(..)` |
| Verteiltes Rate Limit (Cache-gestützt) | `CacheRateLimitLayer` | `.cache_rate_limit(..)` |
| Request-Timeout | `RequestTimeoutLayer` | `.request_timeout(..)` |
| Maximale Body-Größe | `BodyLimitLayer` | `.body_limit(..)` |
| Wartungsmodus (503) | `MaintenanceLayer` | `.maintenance(..)` |
| Idempotenz-Schlüssel | `IdempotencyLayer` | `.idempotency(..)` |
| HTTP-Methoden einschränken | `MethodRestrictLayer` | `.require_get()` / `.require_post()` / `.require_safe()` |
| **Observability** | | |
| Request-ID (`X-Request-Id`) | `RequestIdLayer` | `.request_id(..)` |
| Access-Log (PII-redigiert) | `AccessLogLayer` | `.access_log(..)` |
| `tracing`-Spans | `TracingLayer` | `.layer(..)` |
| `Server-Timing`-Header | `ServerTimingLayer` | `.server_timing(..)` |
| Echte Client-IP (hinter Proxys) | `RealIpLayer` | `.real_ip(..)` |
| **Inhalt & Response** | | |
| gzip/br-Kompression | `CompressionLayer` | `.compression(..)` |
| ETag / bedingtes GET | `EtagLayer` | `.etag(..)` |
| Trailing-Slash-Redirect | `TrailingSlashLayer` | `.trailing_slash(..)` |
| Page-Caching | `CachePageLayer` | `.layer(..)` |
| **Lokalisierung** | | |
| Locale-Aushandlung | `LocaleMiddleware` | `.layer(..)` (+ `ActiveLocale`-Extractor) |
| Aktive Zeitzone | *(mit `from_fn` komponieren)* | siehe [unten](#timezone-aware-middleware) |
| **Entwicklung** | | |
| Live-Reload | `LiveReloadLayer` | `.livereload(..)` |
| Debug-Panel | `DebugPanelLayer` | `.debug_panel(..)` |

Die nächsten Abschnitte gehen die im Detail durch, nach denen der Request
gefragt hat.

---

## Locale-bewusste Middleware

`LocaleMiddleware` löst eine Locale pro Request auf und injiziert sie in den
Request, damit jeder Handler sie lesen kann. Die Auswahlreihenfolge ist die von
Django: **Cookie → `Accept-Language` → Standard**. Die erste Locale, die du
auflistest, ist der Standard, sofern du ihn nicht überschreibst.

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

Handler lesen das Ergebnis mit dem `ActiveLocale`-Extractor. Er ist
`Infallible` — wenn keine Middleware lief, fällt er auf `"en"` zurück — und er
trägt RTL-Helfer, damit Templates nach Bidirektionalität verzweigen können:

```rust
loc.0            // the picked locale, e.g. "fr"
loc.direction()  // "ltr" / "rtl" — feed straight into <html dir="…">
loc.is_rtl()     // true for ar, he, fa, …
```

Der Cookie-Name ist standardmäßig `django_language` (Django-kompatibel); ändere
ihn mit `.cookie_name("…")`, oder übergib `None`, um das Cookie-Lookup ganz zu
deaktivieren. Die Auflösungsreihenfolge, durchgängig verifiziert:

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

Für URL-Präfix-Locales (`/en/about`, `/fr/about`) statt Header-Aushandlung
montiere einen Sub-Router pro Locale mit `Router::nest("/fr", …)` und injiziere
die Locale dort.

---

## Zeitzonen-bewusste Middleware

Es gibt bewusst **keinen Zeitzonen-Layer**. Stattdessen gibt dir das Framework
einen task-lokalen aktiven Offset (`rustango::i18n::timezone`) und einen
Header-/Cookie-Decoder, und du komponierst eine einzeilige Middleware, die ihn
aktiviert — das kanonische „Schreib deine eigene"-Beispiel. Das spiegelt Djangos
`USE_TZ=True`: speichere in UTC, rendere in der lokalen Uhr des Benutzers.

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

`with_offset` installiert den Offset in einem **`tokio::task_local`**, sodass er
`.await`-Punkte und Thread-Wechsel innerhalb des Requests überlebt — anders als
ein Thread-Local verschwindet er nicht, wenn tokio den Task neu einplant.
Innerhalb des Scopes:

- `current_offset()` liefert den aktiven `FixedOffset` (UTC außerhalb jedes
  Scopes),
- `localtime(utc_dt)` konvertiert ein gespeichertes UTC-`DateTime` dorthin,
- der `{{ ts | localtime }}` Tera-Filter (registriert mit
  `timezone::register_filters`) rendert automatisch darin.

`from_request_headers` akzeptiert zuerst das `tz_offset`-Cookie, dann einen
`Time-Zone:`- (oder `X-Timezone:`-)Header. Die akzeptierten Formate sind
flexibel — `"+05:30"`, `"+0530"`, `"Z"`/`"UTC"`, oder **vorzeichenbehaftete
ganze Minuten** (`"330"`, `"-300"`), die Form, die JS' `Date.getTimezoneOffset()`
erzeugt. Das Cookie wird typischerweise von einem winzigen Snippet beim Laden der
Seite gesetzt:

```html
<script>
  // getTimezoneOffset() is positive west-of-UTC, so flip the sign.
  const minutes = -new Date().getTimezoneOffset();
  document.cookie = `tz_offset=${minutes};path=/;max-age=31536000`;
</script>
```

Verhalten, verifiziert:

```rust
// Cookie: tz_offset=330  (UTC+05:30)   → current_offset() = +19800s
// Header: Time-Zone: -300 (UTC-05:00)  → current_offset() = -18000s
// nothing sent                          → current_offset() = 0 (UTC)
```

> **Hinweis — `FixedOffset`, nicht IANA.** Das modelliert die aktive Zeitzone
> als festen Offset, was alles ist, was ein *einzelner Zeitpunkt* braucht, und
> vermeidet die rund 3 MB große `chrono-tz`-Datenbank. Es behandelt **keine**
> Sommerzeit-Übergänge. Für vollständige IANA-Unterstützung parse die `Tz` des
> Benutzers zur Request-Zeit mit `chrono-tz` und übergib
> `tz.offset_from_utc_datetime(...)` an `with_offset` — die Middleware-Form oben
> bleibt unverändert.

---

## Security-Header

`SecurityHeadersLayer` stempelt die standardmäßigen Browser-Härtungs-Header —
HSTS, `X-Frame-Options`, `X-Content-Type-Options`, Referrer-Policy und eine
optionale Content-Security-Policy — auf jede Response, in einer Zeile.

```rust
use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};

let app = Router::new()
    .route("/", get(|| async { "ok" }))
    .security_headers(SecurityHeadersLayer::strict().csp(CspBuilder::strict_starter().build()));
```

Das härtet jede Response:

```rust
// strict-transport-security: max-age=31536000; includeSubDomains; preload
// x-content-type-options:     nosniff
// x-frame-options:            DENY
// content-security-policy:    <built from CspBuilder::strict_starter()>
```

Drei Presets decken die häufigen Fälle ab:

| Preset | Verwenden für | HSTS | `X-Frame-Options` |
| --- | --- | --- | --- |
| `strict()` | Produktion | 1 Jahr + preload | `DENY` |
| `relaxed()` | einbettbare Seiten | 1 Jahr | `SAMEORIGIN` |
| `dev()` | lokale Entwicklung | *(weggelassen)* | — |

Verwende lokal `dev()` — HSTS würde deinen Browser sonst ein Jahr lang auf
`localhost` an HTTPS binden. Baue eine CSP flüssig mit `CspBuilder`
(`.default_src(..)`, `.script_src(..)`, …, `.build()`), und rolle sie sicher mit
`.csp_report_only(true)` + `.csp_report_uri("/csp-report")` aus, bevor du sie
durchsetzt.

---

## CSRF-Schutz

CSRF (Cross-Site Request Forgery) ist, wenn eine andere Website den Browser eines
angemeldeten Benutzers dazu bringt, an deine App zu senden. **Rustango**
verteidigt sich mit dem **Double-Submit-Cookie**-Muster, in
`rustango::forms::csrf` (hinter der `csrf`-Feature, die automatisch von `admin`
eingeschaltet wird).

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/form", get(|| async { "render form" }))
    .route("/submit", post(|| async { "accepted" }))
    .layer(csrf::layer());
```

Eine sichere Methode (GET/HEAD/OPTIONS) prägt ein `rustango_csrf`-Cookie. Eine
unsichere Methode (POST/PUT/PATCH/DELETE) muss den Wert dieses Cookies
zurückgeben, entweder im `X-CSRF-Token`-Header oder im `_csrf`-Formularfeld; eine
Abweichung ergibt `403 Forbidden`:

```rust
// GET /form                            → 200 + Set-Cookie: rustango_csrf=…
//
// POST /submit  (no token)             → 403 Forbidden
//
// POST /submit  with both:
//   Cookie:        rustango_csrf=<t>
//   X-CSRF-Token:  <t>                 → 200 OK   (double-submit matches)
```

In Tera-Templates liefert `{{ csrf_token }}` das rohe Token und
`{{ csrf_input }}` ein fertiges verstecktes `<input name="_csrf">` — leg eines in
jedes Formular. Überschreibe die Cookie-/Header-Namen oder das `Secure`-Flag mit
`csrf::with_config(CsrfConfig)`; für SPA-Setups füge
`.with_trusted_origins([...])` hinzu, um die Defense-in-Depth-Prüfung des
Origin-Headers zusätzlich zum Token zu aktivieren. Für Append-only-Collector-
Endpoints, die über `navigator.sendBeacon` (z. B. Analytics) angesprochen werden
und keinen Token-Header senden können, überspringt
`CsrfConfig::exempt_prefix("/path")` die Durchsetzung für ein schmales
Pfad-Präfix. Der Auto-Admin aktiviert CSRF bei jeder Mutation ohne Opt-out.

---

## Eigene Middleware schreiben

Die schnelle Form hast du schon gesehen — die [Zeitzonen-
Middleware](#timezone-aware-middleware) ist ein vollständiges
`from_fn`-Beispiel. Greife zu `from_fn`, wann immer die Logik anwendungs-
spezifisch ist und du sie nicht konfigurieren musst:

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

Greife zu einem vollständigen **`tower::Layer`**, wenn die Middleware
wiederverwendbar und konfigurierbar ist — wenn du sie als Teil einer Bibliothek
ausliefern würdest. Das Muster ist ein kleines `Layer`-Struct, das einen
`Service` baut, plus (per Konvention) einen `…RouterExt`-Extension-Trait für den
Einzeiler. `rustango::request_id` ist die kleinste vollständige Referenz zum
Kopieren:

- `RequestIdLayer` — der konfigurierbare Layer (`::default()`,
  `.always_generate()`),
- `RequestIdService<S>` — umschließt den inneren Service; liest/setzt den
  `X-Request-Id`-Header innerhalb von `call`,
- `RequestId` — ein `FromRequestParts`-Extractor, damit Handler die ID lesen
  können,
- `RequestIdRouterExt` — liefert `Router::request_id(layer)`.

`LocaleMiddleware` (nachzulesen in `crates/rustango/src/i18n/middleware.rs`) ist
ein zweites ausgearbeitetes Beispiel — dieselben vier Teile, plus eine reine
`.pick(&req)`-Methode, die sich unit-testen lässt, ohne einen Server
hochzufahren. Modelliere neue Layer nach dem einen oder dem anderen.

---

## Siehe auch

- [Security-Leitfaden](security.md) — der vollständige Defense-in-Depth-Stack
  und eine Checkliste für den Deploy-Zeitpunkt (`manage check --deploy`).
- [Authentifizierung](auth-sessions.md) — die Auth-Backends und die
  `CurrentUser`-Middleware bauen auf demselben Layer-Mechanismus auf.
- [URLs & Routing](urls.md) — wo Layer an Router und Sub-Router angehängt
  werden.
