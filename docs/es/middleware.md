# Middleware

El middleware es código que se ejecuta **alrededor** de cada petición — antes de
que tu handler la vea y después de que produce una respuesta. Ahí es donde viven
las preocupaciones transversales: logging, limitación de tasa, cabeceras de
seguridad, CSRF, resolución de locale y zona horaria. **Rustango** incluye un
catálogo amplio de middleware listo para usar y hace que escribir el tuyo propio
sea cuestión de unas pocas líneas. Si vienes de Django, esto es la lista
`MIDDLEWARE`; de Express, es `app.use()`; de Laravel, es el kernel HTTP — la
misma idea, adjunta a tu router.

[![Middleware in Rustango: a request flows down through a stack of tower layers (request-id, locale, security headers, CSRF) into the handler and back up through the response side of each layer](../img/middleware.png)](../img/middleware.png)

> **Fuente:** el middleware está construido sobre
> [`tower::Layer`](https://docs.rs/tower) + `axum::middleware::from_fn`. Los
> componentes integrados se reparten entre
> `rustango::{access_log, rate_limit, cors, security_headers, request_id, …}`,
> más `rustango::forms::csrf` (la feature `csrf`) y `rustango::i18n`
> (`middleware::LocaleMiddleware`, `timezone`). La mayoría están activos por
> defecto.
>
> **Versión ejecutable:** cada fragmento a continuación está copiado del ejemplo
> probado en
> `crates/rustango/examples/getting_started_blog/tests/middleware.rs`
> (locale, zona horaria, cabeceras de seguridad, CSRF — todos sin base de datos).
> Ejecútalo con `cargo test -p getting_started_blog --test middleware`.

> **¿Un término nuevo aquí?** *Middleware*, *layer*, *CSRF*, *locale* — el
> [glosario](glossary.md) los explica en lenguaje sencillo.

## Tabla de contenidos

- [Cómo funciona el middleware en Rustango](#how-middleware-works-in-rustango)
- [El orden importa](#ordering-matters)
- [El catálogo integrado](#the-built-in-catalog)
- [Middleware consciente del locale](#locale-aware-middleware)
- [Middleware consciente de la zona horaria](#timezone-aware-middleware)
- [Cabeceras de seguridad](#security-headers)
- [Protección CSRF](#csrf-protection)
- [Escribir tu propio middleware](#writing-your-own-middleware)
- [Véase también](#see-also)

---

## Cómo funciona el middleware en Rustango

Hay dos formas, y usarás ambas:

1. **Un `tower::Layer`** — una struct de middleware reutilizable y configurable.
   Cada componente integrado es uno (`SecurityHeadersLayer`, `RateLimitLayer`,
   …). Adjuntas un layer con el `.layer(...)` de axum, o — para la mayoría de
   los integrados — con un **one-liner de trait de extensión** que se lee mejor:

   ```rust
   use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt};

   // These two are equivalent; the second is the ergonomic form.
   let app = router.layer(SecurityHeadersLayer::strict());
   let app = router.security_headers(SecurityHeadersLayer::strict());
   ```

   Cada módulo integrado exporta un trait `…RouterExt` (`SecurityHeadersRouterExt`,
   `RateLimitRouterExt`, …). Tráelo al scope y obtienes un método
   `.security_headers()` / `.rate_limit()` / `.cors()` directamente sobre
   `Router`. Esa es la forma idiomática de cablear una pila:

   ```rust
   let app = router
       .request_id(RequestIdLayer::default())
       .access_log(AccessLogLayer::default())
       .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
       .cors(CorsLayer::new().allow_origins(vec!["https://app.example.com"]))
       .security_headers(SecurityHeadersLayer::strict());
   ```

2. **Una función mediante `axum::middleware::from_fn`** — la forma más rápida de
   escribir uno puntual. Recibes la `Request` y un `Next`; llamas a
   `next.run(req)` para continuar, y puedes hacer trabajo a ambos lados de esa
   llamada. El [ejemplo de zona horaria](#timezone-aware-middleware) más abajo es
   exactamente esto.

Ambos se componen libremente — un middleware `from_fn` *es* un layer, así que se
apila con los integrados en la misma cadena `.layer(...)`.

---

## El orden importa

Un layer envuelve todo lo añadido **antes** que él, así que el **último** layer
que adjuntas es el **más externo** — se ejecuta primero a la entrada y último a
la salida. Imagina la pila como una cebolla; la petición viaja hacia abajo hasta
el handler y la respuesta viaja de vuelta hacia arriba:

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

Consecuencias prácticas:

- **Coloca `request_id` / `access_log` cerca del fondo** (añadidos primero) para
  que el id de correlación y el span de log cubran todo lo que se ejecuta después
  de ellos.
- **Coloca las guardas de rechazo barato cerca de la cima** (añadidas último) —
  `allowed_hosts`, `rate_limit`, `body_limit` — para que una petición bloqueada
  se descarte antes de alcanzar los layers internos costosos.
- **`security_headers` en el nivel más externo** para que las cabeceras se
  estampen en *cada* respuesta, incluidas las cortocircuitadas por un layer
  interno (un 429, un 403).

---

## El catálogo integrado

Cada entrada es un `tower::Layer` con un one-liner `…RouterExt` correspondiente,
salvo que se indique lo contrario. Trae el trait `…RouterExt` del módulo al scope
para obtener el método.

| Preocupación | Layer | Cablealo con |
| --- | --- | --- |
| **Seguridad y acceso** | | |
| Cabeceras de seguridad (HSTS, XFO, CSP…) | `SecurityHeadersLayer` | `.security_headers(..)` |
| CSRF (cookie double-submit) | `CsrfLayer` | `.layer(csrf::layer())` |
| CORS | `CorsLayer` | `.cors(..)` |
| Lista de hosts permitidos | `AllowedHostsLayer` | `.allowed_hosts(..)` |
| Permitir/bloquear IP | `IpFilterLayer` | `.ip_filter(..)` |
| Forzar HTTPS | `SslRedirectLayer` | `.ssl_redirect(..)` |
| Nonce CSP por petición | `CspNonceLayer` | `.csp_nonce(..)` |
| Firma de petición HMAC | `HmacAuthLayer` | `.layer(..)` |
| **Tráfico y resiliencia** | | |
| Límite de tasa (por IP/global) | `RateLimitLayer` | `.rate_limit(..)` |
| Límite de tasa distribuido (respaldado por caché) | `CacheRateLimitLayer` | `.cache_rate_limit(..)` |
| Timeout de petición | `RequestTimeoutLayer` | `.request_timeout(..)` |
| Tamaño máximo de cuerpo | `BodyLimitLayer` | `.body_limit(..)` |
| Modo mantenimiento (503) | `MaintenanceLayer` | `.maintenance(..)` |
| Claves de idempotencia | `IdempotencyLayer` | `.idempotency(..)` |
| Restringir métodos HTTP | `MethodRestrictLayer` | `.require_get()` / `.require_post()` / `.require_safe()` |
| **Observabilidad** | | |
| Id de petición (`X-Request-Id`) | `RequestIdLayer` | `.request_id(..)` |
| Log de acceso (PII redactada) | `AccessLogLayer` | `.access_log(..)` |
| Spans de `tracing` | `TracingLayer` | `.layer(..)` |
| Cabecera `Server-Timing` | `ServerTimingLayer` | `.server_timing(..)` |
| IP real del cliente (tras proxies) | `RealIpLayer` | `.real_ip(..)` |
| **Contenido y respuesta** | | |
| Compresión gzip/br | `CompressionLayer` | `.compression(..)` |
| ETag / GET condicional | `EtagLayer` | `.etag(..)` |
| Redirección de barra final | `TrailingSlashLayer` | `.trailing_slash(..)` |
| Caché de página | `CachePageLayer` | `.layer(..)` |
| **Localización** | | |
| Negociación de locale | `LocaleMiddleware` | `.layer(..)` (+ extractor `ActiveLocale`) |
| Zona horaria activa | *(componer con `from_fn`)* | ver [más abajo](#timezone-aware-middleware) |
| **Desarrollo** | | |
| Recarga en vivo | `LiveReloadLayer` | `.livereload(..)` |
| Panel de depuración | `DebugPanelLayer` | `.debug_panel(..)` |

Las siguientes secciones recorren en detalle las que pidió la petición.

---

## Middleware consciente del locale

`LocaleMiddleware` resuelve un locale por petición y lo inyecta en la petición
para que cualquier handler pueda leerlo. El orden de selección es el de Django:
**cookie → `Accept-Language` → valor por defecto**. El primer locale que listes
es el valor por defecto salvo que lo sobrescribas.

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

Los handlers leen el resultado con el extractor `ActiveLocale`. Es `Infallible`
— si no se ejecutó ningún middleware, recae en `"en"` — y lleva helpers RTL para
que las plantillas puedan ramificar según la bidireccionalidad:

```rust
loc.0            // the picked locale, e.g. "fr"
loc.direction()  // "ltr" / "rtl" — feed straight into <html dir="…">
loc.is_rtl()     // true for ar, he, fa, …
```

El nombre de la cookie es por defecto `django_language` (compatible con Django);
cámbialo con `.cookie_name("…")`, o pasa `None` para deshabilitar por completo la
búsqueda por cookie. El orden de precedencia de resolución, verificado de
extremo a extremo:

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

Para locales en prefijo de URL (`/en/about`, `/fr/about`) en lugar de la
negociación por cabecera, monta un sub-router por locale con
`Router::nest("/fr", …)` e inyecta el locale ahí.

---

## Middleware consciente de la zona horaria

Deliberadamente **no hay layer de zona horaria**. En su lugar, el framework te da
un offset activo task-local (`rustango::i18n::timezone`) y un decodificador de
cabecera/cookie, y compones un middleware de una línea que lo activa — el ejemplo
canónico de «escribe el tuyo». Esto refleja el `USE_TZ=True` de Django: almacena
en UTC, renderiza en el reloj local del usuario.

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

`with_offset` instala el offset en un **`tokio::task_local`**, de modo que
sobrevive a los puntos `.await` y a los saltos de hilo dentro de la petición — a
diferencia de un thread-local, no se desvanece cuando tokio reprograma la tarea.
Dentro del scope:

- `current_offset()` devuelve el `FixedOffset` activo (UTC fuera de cualquier
  scope),
- `localtime(utc_dt)` convierte un `DateTime` UTC almacenado a él,
- el filtro Tera `{{ ts | localtime }}` (registrado con
  `timezone::register_filters`) lo renderiza automáticamente.

`from_request_headers` acepta primero la cookie `tz_offset`, luego una cabecera
`Time-Zone:` (o `X-Timezone:`). Los formatos aceptados son flexibles —
`"+05:30"`, `"+0530"`, `"Z"`/`"UTC"`, o **minutos enteros con signo** (`"330"`,
`"-300"`), la forma que produce `Date.getTimezoneOffset()` de JS. La cookie la
suele fijar un pequeño fragmento al cargar la página:

```html
<script>
  // getTimezoneOffset() is positive west-of-UTC, so flip the sign.
  const minutes = -new Date().getTimezoneOffset();
  document.cookie = `tz_offset=${minutes};path=/;max-age=31536000`;
</script>
```

Comportamiento, verificado:

```rust
// Cookie: tz_offset=330  (UTC+05:30)   → current_offset() = +19800s
// Header: Time-Zone: -300 (UTC-05:00)  → current_offset() = -18000s
// nothing sent                          → current_offset() = 0 (UTC)
```

> **Nota — `FixedOffset`, no IANA.** Esto modela la zona horaria activa como un
> offset fijo, que es todo lo que necesita un *único instante en el tiempo* y
> evita la base de datos `chrono-tz` de unos 3 MB. **No** maneja las transiciones
> de horario de verano. Para soporte IANA completo, parsea el `Tz` del usuario
> con `chrono-tz` en tiempo de petición y pasa
> `tz.offset_from_utc_datetime(...)` a `with_offset` — la forma del middleware de
> arriba no cambia.

---

## Cabeceras de seguridad

`SecurityHeadersLayer` estampa las cabeceras estándar de endurecimiento del
navegador — HSTS, `X-Frame-Options`, `X-Content-Type-Options`, Referrer-Policy y
una Content-Security-Policy opcional — en cada respuesta, en una línea.

```rust
use rustango::security_headers::{CspBuilder, SecurityHeadersLayer, SecurityHeadersRouterExt};

let app = Router::new()
    .route("/", get(|| async { "ok" }))
    .security_headers(SecurityHeadersLayer::strict().csp(CspBuilder::strict_starter().build()));
```

Eso endurece cada respuesta:

```rust
// strict-transport-security: max-age=31536000; includeSubDomains; preload
// x-content-type-options:     nosniff
// x-frame-options:            DENY
// content-security-policy:    <built from CspBuilder::strict_starter()>
```

Tres presets cubren los casos comunes:

| Preset | Usar para | HSTS | `X-Frame-Options` |
| --- | --- | --- | --- |
| `strict()` | producción | 1 año + preload | `DENY` |
| `relaxed()` | páginas embebibles | 1 año | `SAMEORIGIN` |
| `dev()` | dev local | *(omitido)* | — |

Usa `dev()` en local — de lo contrario HSTS fijaría tu navegador a HTTPS durante
un año en `localhost`. Construye una CSP de forma fluida con `CspBuilder`
(`.default_src(..)`, `.script_src(..)`, …, `.build()`), y despliégala de forma
segura con `.csp_report_only(true)` + `.csp_report_uri("/csp-report")` antes de
aplicarla.

---

## Protección CSRF

CSRF (cross-site request forgery) es cuando otro sitio engaña al navegador de un
usuario autenticado para que envíe a tu aplicación. **Rustango** se defiende con
el patrón **double-submit-cookie**, en `rustango::forms::csrf` (tras la feature
`csrf`, activada automáticamente por `admin`).

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/form", get(|| async { "render form" }))
    .route("/submit", post(|| async { "accepted" }))
    .layer(csrf::layer());
```

Un método seguro (GET/HEAD/OPTIONS) emite una cookie `rustango_csrf`. Un método
no seguro (POST/PUT/PATCH/DELETE) debe devolver el valor de esa cookie, ya sea en
la cabecera `X-CSRF-Token` o en el campo de formulario `_csrf`; una discrepancia
es `403 Forbidden`:

```rust
// GET /form                            → 200 + Set-Cookie: rustango_csrf=…
//
// POST /submit  (no token)             → 403 Forbidden
//
// POST /submit  with both:
//   Cookie:        rustango_csrf=<t>
//   X-CSRF-Token:  <t>                 → 200 OK   (double-submit matches)
```

En plantillas Tera, `{{ csrf_token }}` da el token en bruto y `{{ csrf_input }}`
un `<input name="_csrf">` oculto listo para usar — coloca uno en cada
formulario. Sobrescribe los nombres de cookie/cabecera o el flag `Secure` con
`csrf::with_config(CsrfConfig)`; para configuraciones SPA, añade
`.with_trusted_origins([...])` para habilitar la comprobación de defensa en
profundidad de la cabecera Origin además del token. Para endpoints colectores
append-only alcanzados vía `navigator.sendBeacon` (p. ej. analítica) que no
pueden enviar una cabecera de token, `CsrfConfig::exempt_prefix("/path")` omite la
aplicación para un prefijo de ruta estrecho. El auto-admin habilita CSRF en cada
mutación sin opción de desactivarlo.

---

## Escribir tu propio middleware

Ya viste la forma rápida — el [middleware de zona
horaria](#timezone-aware-middleware) es un ejemplo `from_fn` completo. Recurre a
`from_fn` siempre que la lógica sea específica de la aplicación y no necesites
configurarla:

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

Recurre a un **`tower::Layer`** completo cuando el middleware es reutilizable y
configurable — cuando lo distribuirías como parte de una biblioteca. El patrón es
una pequeña struct `Layer` que construye un `Service`, más (por convención) un
trait de extensión `…RouterExt` para el one-liner. `rustango::request_id` es la
referencia completa más pequeña para copiar:

- `RequestIdLayer` — el layer configurable (`::default()`, `.always_generate()`),
- `RequestIdService<S>` — envuelve el servicio interno; lee/fija la cabecera
  `X-Request-Id` dentro de `call`,
- `RequestId` — un extractor `FromRequestParts` para que los handlers puedan leer
  el id,
- `RequestIdRouterExt` — proporciona `Router::request_id(layer)`.

`LocaleMiddleware` (léelo en `crates/rustango/src/i18n/middleware.rs`) es un
segundo ejemplo trabajado — las mismas cuatro piezas, más un método puro
`.pick(&req)` que se puede testear unitariamente sin levantar un servidor.
Modela los nuevos layers sobre uno u otro.

---

## Véase también

- [Guía de seguridad](security.md) — la pila completa de defensa en profundidad y
  una checklist para el momento del despliegue (`manage check --deploy`).
- [Autenticación](auth-sessions.md) — los backends de auth y el middleware
  `CurrentUser` se apoyan en el mismo mecanismo de layer.
- [URLs y enrutamiento](urls.md) — dónde se adjuntan los layers a los routers y
  sub-routers.
