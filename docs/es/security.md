# Guía de seguridad

Esta guía cubre todas las funciones de seguridad que incluye **Rustango** y cómo combinarlas. Si vienes de Django, Laravel o Rails, la mayoría te resultarán familiares — los nombres difieren, pero las ideas son las mismas. Cada función de abajo suele ser una sola línea de configuración. Cuando estés listo para desplegar, ejecuta `manage check --deploy` para una auditoría automatizada.

[![La pila de middleware endurecida conectada en una sola cadena: identificadores de petición, registro de accesos, limitación de tasa, CORS y cabeceras de seguridad](img/security.png)](img/security.png)

## Tabla de contenidos

- [La lista de comprobación de defensa en profundidad](#the-defense-in-depth-checklist)
- [Establecer cabeceras de seguridad](#setting-security-headers)
- [Permitir peticiones de origen cruzado (CORS)](#allowing-cross-origin-requests-cors)
- [Limitar la tasa de peticiones](#rate-limiting-requests)
- [Permitir o bloquear IPs](#allowing-or-blocking-ips)
- [Protección contra CSRF](#protecting-against-csrf)
- [Prevenir XSS](#preventing-xss)
- [Prevenir la inyección SQL](#preventing-sql-injection)
- [Autenticar usuarios](#authenticating-users)
- [Hashear y verificar contraseñas](#hashing-and-checking-passwords)
- [Emitir y refrescar JWTs](#issuing-and-refreshing-jwts)
- [Autenticar con claves de API](#authenticating-with-api-keys)
- [Añadir autenticación de dos factores (TOTP)](#adding-two-factor-auth-totp)
- [Enviar URLs firmadas (enlaces mágicos)](#sending-signed-urls-magic-links)
- [Verificar webhooks entrantes](#verifying-incoming-webhooks)
- [Mantener los secretos fuera de tus registros](#keeping-secrets-out-of-your-logs)
- [Rastrear peticiones a través de servicios](#tracing-requests-across-services)
- [Gestionar secretos](#managing-secrets)
- [Auditar antes de desplegar](#auditing-before-you-deploy)

---

## La lista de comprobación de defensa en profundidad

La buena seguridad surge de muchas capas pequeñas, no de un único gran muro. Una aplicación **Rustango** en producción debería apilar las capas de abajo — la mayoría son una línea cada una. El resto de esta guía explica cada una.

```rust
let app = Router::new()
    .route("/...", ...)
    .merge(health::health_router(pool.clone()))         // /health, /ready
    .request_id(RequestIdLayer::default())              // log correlation
    .access_log(AccessLogLayer::default())              // PII-redacted by default
    .rate_limit(RateLimitLayer::per_ip(60, ONE_MIN))    // 60 req/min/IP
    .cors(CorsLayer::new().allow_origins(vec!["..."])) // explicit allowlist
    .security_headers(                                   // HSTS + XFO + nosniff + Referrer + CSP
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    );
// Plus: TLS termination at reverse proxy.
// Plus: argon2 for passwords, JWT lifecycle for tokens, TOTP for MFA.
```

---

## Establecer cabeceras de seguridad

Las cabeceras de seguridad le indican al navegador cómo proteger a tus usuarios (bloquear el clickjacking, forzar HTTPS, detener el sniffing del tipo de contenido). `SecurityHeadersLayer` establece el conjunto estándar con una sola línea — las mismas cabeceras que Django incluye por defecto. (Una "capa" es el término de **Rustango** para el middleware; la adjuntas a tu router.)

> Análisis en profundidad: [Middleware](middleware.md) cubre cómo funcionan las capas, el orden, el catálogo completo de las incorporadas, y cómo escribir la tuya propia (locale, zona horaria, cabeceras, CSRF).

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};

let app = router.security_headers(SecurityHeadersLayer::strict());
```

### Preajustes

| Preajuste | Cuándo usarlo |
|---|---|
| `strict()` | Producción: HSTS preload + XFO=DENY + nosniff + Referrer-Policy=no-referrer + COOP=same-origin + Permissions-Policy restringida |
| `relaxed()` | Incrustable en iframes: SAMEORIGIN + HSTS de 1 año |
| `dev()` | Local: solo nosniff (sin HSTS para evitar bloquear localhost en HTTPS para siempre) |
| `empty()` | Construir desde cero |

### CSP personalizada

```rust
let csp = CspBuilder::new()
    .default_src(&["'self'"])
    .script_src(&["'self'", "https://cdn.example.com"])
    .style_src(&["'self'", "'unsafe-inline'"])    // for inline <style>
    .img_src(&["'self'", "data:", "https:"])
    .font_src(&["'self'", "data:"])
    .connect_src(&["'self'", "wss://realtime.example.com"])
    .frame_ancestors(&["'none'"])                 // disallow embedding (clickjacking)
    .object_src(&["'none'"])
    .directive("base-uri", &["'self'"])
    .build();

let layer = SecurityHeadersLayer::strict().csp(csp);
```

### Despliegue gradual de CSP

Usa `csp_report_only` para monitorizar sin romper producción:

```rust
let layer = SecurityHeadersLayer::strict()
    .csp(CspBuilder::strict_starter().build())
    .csp_report_only(true);                       // sends Content-Security-Policy-Report-Only
```

Tras verificar que no hay errores de consola → cambia a `csp_report_only(false)` para aplicarla.

---

## Permitir peticiones de origen cruzado (CORS)

CORS controla qué otros sitios web tienen permiso para llamar a tu API desde un navegador. Por defecto los navegadores bloquean estas llamadas; tú vuelves a habilitar orígenes específicos. Enumera tus dominios front-end reales en producción y nunca lo abras a todo el mundo.

```rust
use rustango::cors::{CorsLayer, CorsRouterExt};

// Production
let layer = CorsLayer::new()
    .allow_origins(vec!["https://app.example.com", "https://admin.example.com"])
    .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"])
    .allow_headers(vec!["content-type", "authorization"])
    .allow_credentials(true)
    .max_age(Duration::from_secs(3600));

// Dev only
let layer = CorsLayer::permissive();              // any origin, common methods
```

**Nota de seguridad:** nunca combines `allow_credentials(true)` con `allow_any_origin()` — el navegador rechazará la respuesta. Con credenciales, DEBES enumerar orígenes explícitos.

---

## Limitar la tasa de peticiones

La limitación de tasa restringe cuántas peticiones puede hacer un cliente en una ventana de tiempo. Úsala para frenar los inicios de sesión por fuerza bruta, el scraping y el abuso. Puedes limitar por IP, por clave de API o globalmente para un endpoint costoso.

```rust
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};

// Per-IP
router.rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)));

// Per-API-key (header)
router.rate_limit(RateLimitLayer::per_header("authorization", 1000, Duration::from_secs(3600)));

// Global ceiling for an expensive endpoint
router.rate_limit(RateLimitLayer::global(10, Duration::from_secs(1)));
```

Cuando se agota: `429 Too Many Requests` con la cabecera `Retry-After`. Cada respuesta exitosa incluye `X-RateLimit-Limit` + `X-RateLimit-Remaining`.

> **Detrás de un proxy inverso, empareja `per_ip` con `real_ip`.** `RateLimitLayer::per_ip` se basa en el socket de conexión (`ConnectInfo`), que detrás de un proxy es la IP del *proxy* — de modo que todos los clientes comparten un único cubo y el límite resulta inútil. Coloca `real_ip::RealIpLayer` (lee `X-Forwarded-For` / `X-Real-IP`) delante de él para que se use la IP real del cliente.

`RateLimitLayer` es **local al proceso** — cuenta las peticiones solo dentro de una instancia en ejecución, lo cual está bien si ejecutas una sola instancia. Si ejecutas varias instancias (réplicas) detrás de un balanceador de carga, cada una mantendría su propio recuento, de modo que el límite real se multiplica. Para compartir un único recuento entre todas las réplicas, usa `rate_limit_cache::CacheRateLimitLayer`, que delega en cualquier implementación de `cache::Cache` (empareja con `cache::RedisCache` para un contador compartido incrementado atómicamente por el `INCRBY` de Redis):

```rust
use rustango::cache::RedisCache;
use rustango::rate_limit::KeyBy;
use rustango::rate_limit_cache::{CacheRateLimitLayer, CacheRateLimitRouterExt};

let cache: rustango::cache::BoxedCache =
    std::sync::Arc::new(RedisCache::new("redis://localhost").await?);

let app = axum::Router::new()
    .route("/api/login", axum::routing::post(login))
    .cache_rate_limit(
        CacheRateLimitLayer::new(cache, 5, Duration::from_secs(60))
            .key_by(KeyBy::Ip)
            .key_prefix("login"),
    );
```

---

## Permitir o bloquear IPs

Restringe las rutas sensibles (como tu admin) a un conjunto conocido de direcciones IP, o bloquea a abusadores concretos. Una lista de permitidos autoriza solo las redes enumeradas; una lista de bloqueo deniega las enumeradas.

```rust
use rustango::ip_filter::{IpFilterLayer, IpFilterRouterExt};

// Internal admin only
let admin_router = admin_router
    .ip_filter(IpFilterLayer::allow_only(vec![
        "10.0.0.0/8",
        "192.168.0.0/16",
        "203.0.113.0/24",
    ])?);

// Block known abusers
let public_router = public_router
    .ip_filter(IpFilterLayer::block(vec!["203.0.113.42"])?);
```

**IPv4 + IPv6 soportados.** Seguro entre familias (un CIDR IPv4 no coincide con direcciones IPv6). Devuelve `403 Forbidden` al rechazar.

**Importante:** **Rustango** lee la IP del cliente desde `ConnectInfo<SocketAddr>`. Monta con:

```rust
axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

Si tu proxy inverso reenvía `X-Forwarded-For`, configúralo para que establezca `RemoteAddr` — `ip_filter` lee el par de conexión, no las cabeceras.

---

## Protección contra CSRF

> **CSRF y las APIs de token.** CSRF protege las peticiones **autenticadas por
> cookie**. Una API puramente de token / Bearer / [JWT](auth-jwt.md) no es un
> objetivo de CSRF — no le apliques la capa CSRF (o darás un 403 a clientes
> legítimos). Para una SPA que *sí* usa cookies, lee la cookie `rustango_csrf`
> y reenvíala en la cabecera `X-CSRF-Token`.

CSRF (falsificación de petición en sitios cruzados) es cuando otro sitio engaña al navegador de un usuario que ha iniciado sesión para que envíe una petición a tu aplicación. La defensa es un token secreto en cada formulario, igual que el `{% csrf_token %}` de Django. El middleware CSRF vive en `rustango::forms::csrf` (detrás de la función `csrf`, que se activa automáticamente con la función `admin`):

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/contact", get(form).post(submit))
    .layer(csrf::layer());
```

`csrf::layer()` construye la capa con valores predeterminados sensatos; `csrf::with_config(CsrfConfig)` te permite sobrescribir los nombres de la cookie/cabecera y el flag `Secure`. En las plantillas, `{{ csrf_token }}` te da el token en bruto y `{{ csrf_input }}` te da un `<input>` oculto listo para usar — coloca uno dentro de cada formulario. Usa el patrón de cookie de doble envío: en los métodos inseguros (POST, PUT, PATCH, DELETE) la capa comprueba la cabecera `X-CSRF-Token` (o el campo de formulario `_csrf`) contra la cookie `rustango_csrf`; una discrepancia devuelve `403 Forbidden`.

**Eximir endpoints recolectores.** `CsrfConfig::exempt_prefix("/path")` (repetible) omite la aplicación de CSRF para los métodos inseguros en peticiones cuya ruta comienza con el prefijo dado. Esto es para endpoints de solo anexado, sin estado de autenticación, alcanzados vía `navigator.sendBeacon` — por ejemplo, un recolector de analíticas — que no pueden establecer una cabecera `X-CSRF-Token` y, cuando la página se sirve desde una caché de CDN que elimina `Set-Cookie`, puede que no lleven ninguna cookie CSRF en absoluto. Mantén los prefijos estrechos y nunca eximas nada que lea o escriba estado de autenticación.

El auto-admin habilita CSRF en cada mutación por defecto, y no hay forma de desactivarlo.

---

## Prevenir XSS

XSS (scripting entre sitios) ocurre cuando la entrada del usuario se renderiza como HTML y se ejecuta como código en el navegador de otra persona. La solución es escapar cualquier entrada del usuario antes de que llegue a la página. **Rustango** maneja esto de dos maneras:

**1. Auto-escape de plantillas Tera** — Tera es el motor de plantillas de **Rustango** (como las plantillas de Django o Blade). Cada `{{ var }}` se escapa como HTML automáticamente. Usa `{{ var | safe }}` para desactivarlo — raro, y peligroso, así que hazlo solo con HTML en el que confíes plenamente.

**2. Ayudante de escape manual** — para cuando construyes HTML en código Rust en lugar de en una plantilla:

```rust
use rustango::text::html_escape;

let safe = html_escape(user_input);
// "<script>" → "&lt;script&gt;"
// "a & b" → "a &amp; b"
```

Reemplaza `&`, `<`, `>`, `"`, `'`. Adecuado para el contenido de elementos HTML + atributos entre comillas dobles.

Para la defensa XSS basada en CSP, consulta [Establecer cabeceras de seguridad](#setting-security-headers).

---

## Prevenir la inyección SQL

La inyección SQL ocurre cuando la entrada del usuario se pega directamente en una cadena SQL y cambia la consulta. El ORM de **Rustango** detiene esto por diseño: usa el enlace de parámetros de sqlx en todas partes — cada valor se envía como un marcador de posición `$1, $2, ...`, nunca pegado en el texto de la consulta. (sqlx es la biblioteca de base de datos subyacente.) **No puedes concatenar accidentalmente la entrada del usuario en SQL a través de la API del ORM.**

Hay dos puntos en los que hay que tener cuidado:

**1. Nombres de tabla y columna en `bulk_actions`:**

Los marcadores de posición protegen los valores, pero no pueden llevar un identificador (un nombre de tabla o columna), así que esos necesitan su propia protección. El ejecutor de acciones masivas nunca pega nombres de tabla/columna suministrados por el usuario en bruto dentro del SQL. Internamente valida cada identificador (un `validate_ident` privado al crate que rechaza `"`, `` ` ``, `\0`, `\n`, `\r`, `\`, `;`, espacios y caracteres de control) y luego lo entrecomilla mediante `dialect.quote_ident()` antes de construir la sentencia. Esta protección se ejecuta por ti — no la llamas tú mismo.

**2. La salida de emergencia de SQL en bruto:**

Si desciendes a `sqlx` en bruto, enlaza cada *valor* como un parámetro y nunca pegues un *identificador* (nombre de tabla o columna) que provenga de la entrada del usuario en la cadena de la consulta — los marcadores de posición no pueden llevar identificadores:

```rust
// ✅ values go through bound placeholders
sqlx::query("SELECT * FROM posts WHERE id = $1")
    .bind(1).fetch_all(&pool).await?;

// ❌ NEVER interpolate a user-supplied identifier into the SQL string
let sql = format!("SELECT * FROM \"{user_table}\" WHERE id = $1");
sqlx::query(&sql).bind(1).fetch_all(&pool).await?;
```

---

## Autenticar usuarios

La autenticación es cómo confirmas quién está haciendo una petición. **Rustango** incluye tres backends listos para usar (autenticación Basic, claves de API y JWTs) y te permite escribir el tuyo propio — muy parecido a los backends de autenticación de Django. Los adjuntas a las rutas, y las peticiones sin una credencial reconocida obtienen un `401`.

> **SSO del admin.** Para permitir que los operadores inicien sesión en el admin con
> un IdP externo (Google, Microsoft/Azure AD, GitHub, o cualquier proveedor
> OpenID Connect) en lugar de una contraseña, habilita la función `admin-sso` —
> consulta la [guía de SSO](sso.md). Los proveedores se **gestionan desde la UI
> del admin como filas** (varios por superficie; por inquilino, o un conjunto
> compartido entre inquilinos), con el secreto de cliente **cifrado en reposo**.
> Es enlace-a-existente (el email verificado del IdP debe coincidir con un usuario
> del admin; sin auto-aprovisionamiento) y reutiliza la sesión existente.

### Tres backends listos para usar

```rust
use rustango::tenancy::auth_backends::{ModelBackend, ApiKeyBackend, JwtBackend};
use rustango::tenancy::middleware::{RouterAuthExt, CurrentUser};
use std::sync::Arc;

let backends = vec![
    Arc::new(ModelBackend) as _,                   // Authorization: Basic <b64>
    Arc::new(ApiKeyBackend) as _,                   // Authorization: Bearer <prefix>.<secret>
    Arc::new(JwtBackend::new(secret)) as _,         // Authorization: Bearer <jwt>
];

let app = Router::new()
    .route("/me", get(profile))
    .require_auth(backends.clone(), pool.clone())   // 401 if no backend recognizes
    .route("/posts/new", post(create_post))
    .require_perm("post.add", pool.clone())         // gate by codename
    .require_auth(backends, pool);
```

El middleware prueba cada backend en orden. El primero que tiene éxito gana; el primero que devuelve un error duro detiene la cadena.

### Escribir tu propio backend

```rust
use rustango::tenancy::auth_backends::{AuthBackend, AuthUser, AuthError};
use async_trait::async_trait;

pub struct OAuthBackend { /* ... */ }

#[async_trait]
impl AuthBackend for OAuthBackend {
    async fn authenticate(&self, parts: &Parts, pool: &rustango::sql::Pool)
        -> Result<Option<AuthUser>, AuthError>
    {
        // Read your custom header, validate, look up the user
        Ok(None)  // means: not my request, try next backend
    }
}
```

---

## Hashear y verificar contraseñas

Nunca almacenes las contraseñas como texto plano. Hashéalas al registrarse con `hash`, y al iniciar sesión compara el intento con `verify`. Usa `strength_score` para rechazar contraseñas débiles antes siquiera de hashearlas.

```rust
use rustango::passwords::{hash, verify, strength_score, StrengthIssue};

// Signup
let issues = strength_score(&new_password);
if !issues.is_empty() {
    return Err(format!("password too weak: {issues:?}"));
}
let hashed = hash(&new_password)?;

// Login
let ok = verify(&attempted, &user.password_hash)?;
```

El hasheo usa **argon2id** (el valor predeterminado recomendado por OWASP desde 2023 — resiste mejor el crackeo por GPU que bcrypt). Los hashes se almacenan en formato de cadena PHC, así que puedes cambiar los parámetros más tarde sin romper los hashes antiguos.

La **comprobación de fortaleza** incorporada es intencionadamente mínima. Para despliegues serios, comprueba también las contraseñas contra la API HIBP / pwned-passwords para rechazar las que se sabe que se han filtrado.

> **Análisis en profundidad:** [Contraseñas](auth-passwords.md) — internos de argon2id, salado automático, inicios de sesión seguros ante el tiempo (`verify_dummy`), y dónde vive el hash. Una vez autenticado, delega en una [Sesión](auth-sessions.md) (navegador) o un [JWT](auth-jwt.md) (API).

---

## Emitir y refrescar JWTs

Un JWT (JSON Web Token) es un token firmado que un cliente envía en lugar de iniciar sesión cada vez. `JwtLifecycle` maneja todo el flujo: un token de acceso de corta duración más un token de refresco de mayor duración, con verificación, refresco y revocación incorporados.

```rust
use rustango::tenancy::jwt_lifecycle::{JwtLifecycle, JwtTokenPair, JwtIssueError};
use serde_json::json;

let jwt = JwtLifecycle::new(secret)
    .with_access_ttl(900)                         // 15 min
    .with_refresh_ttl(7 * 86400);                 // 7 days
```

### Emitir un token con claims personalizados (sin consulta a la BD al verificar)

```rust
let pair = jwt.issue_pair_with(user_id, json!({
    "roles":  ["admin", "editor"],
    "tenant": "acme",
    "scope":  "read:posts write:posts",
    "email":  "alice@example.com",
}).as_object().unwrap().clone())?;
```

Los "claims" son los hechos clave/valor que incrustas en el token (roles, inquilino, etc.); el cliente los devuelve y tú los lees sin tocar la base de datos. Los nombres de claim reservados (`sub`, `exp`, `jti`, `typ`) son usados por el framework y no pueden aparecer en tus claims personalizados — intentarlo devuelve `JwtIssueError::ReservedClaim`.

### Verificar un token

```rust
let claims = jwt.verify_access(&access_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
let tenant: String = claims.get_custom("tenant").unwrap_or_default();
```

### Refrescar un token (mantiene tus claims personalizados)

```rust
let new_pair = jwt.refresh(&refresh_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;
// new_pair has the same roles + scope + tenant
```

El JTI (su ID único) del antiguo token de refresco se añade a una lista negra para que no pueda reutilizarse — esto bloquea los ataques de repetición donde se reenvía un token antiguo robado.

### Refrescar con permisos revisados de nuevo

```rust
// returns Result<Option<JwtTokenPair>, JwtIssueError>:
// Err on a reserved-claim collision, Ok(None) on an invalid/expired refresh.
let new_pair = jwt.refresh_with(&refresh_token, json!({
    "roles": ["viewer"],     // role was demoted since login
    "tenant": "acme",
}).as_object().unwrap().clone())?
    .ok_or(StatusCode::UNAUTHORIZED)?;
```

### Revocar un token

```rust
jwt.revoke(&access_token);     // adds JTI to blacklist
jwt.revoke(&refresh_token);
```

La lista negra en memoria por defecto (`InMemoryJtiStore`) limpia por sí sola las entradas caducadas, pero vive en un único proceso y olvida cada revocación al reiniciar. Para despliegues multiproceso, instala un almacén compartido y duradero vía `JwtLifecycle::new(secret).with_jti_store(store)` — `store` es cualquier `Arc<dyn JtiStore>` (por ejemplo uno respaldado por Redis o por BD). Sin un almacén compartido, un token que revocas en una réplica todavía puede repetirse en otra hasta que caduque.

> **Análisis en profundidad:** [API de auth JWT](auth-jwt-api.md) — el router incorporado `/api/auth/login|refresh|logout|me`, el refresco deslizante, y el almacén de revocación de JTI. Para un único token autogestionado, consulta [JWT (independiente)](auth-jwt.md). Para restringir rutas de navegador/API por inicio de sesión, consulta los [decoradores de acceso](auth-decorators.md).

---

## Autenticar con claves de API

Las claves de API permiten que scripts y servicios se autentiquen sin nombre de usuario, contraseña o sesión — útil para el acceso máquina a máquina. Generas una clave, se la muestras al usuario una vez, y almacenas solo su prefijo y su hash.

```rust
use rustango::api_keys::{generate_key, verify_key, split_token};

// Issuance
let (full_token, prefix, hash) = generate_key()?;
// Format: {8-char hex prefix}.{32-char hex secret}
// Show full_token to the user once. Store prefix + hash in your DB.

// Verification on each request
let (prefix, secret) = split_token(&inbound_header)
    .ok_or(StatusCode::UNAUTHORIZED)?;
let row = lookup_by_prefix(prefix).await?;
if !verify_key(secret, &row.hash)? {
    return Err(StatusCode::UNAUTHORIZED);
}
```

Estas claves funcionan directamente con el `ApiKeyBackend` de multi-tenencia — ambos usan el mismo formato `{prefix}.{secret}` con secretos hasheados con argon2id, así que una clave emitida aquí autentica allí sin trabajo adicional.

---

## Añadir autenticación de dos factores (TOTP)

TOTP es el código de 6 dígitos de una aplicación autenticadora — un segundo factor sobre la contraseña. Generas un secreto por usuario, lo muestras como un código QR para inscribirse, y luego verificas el código que teclean en cada inicio de sesión. Sigue el estándar RFC 6238.

```rust
use rustango::totp::{TotpSecret, otpauth_url, verify};

// Enrollment
let secret = TotpSecret::generate();                           // 20 random bytes
user.totp_secret = secret.to_base32();                          // store in DB
let qr_url = otpauth_url("MyApp", &user.email, &secret);        // encode as QR

// Verification on login
let secret = TotpSecret::from_base32(&user.totp_secret).unwrap();
if !verify(&secret, &user_supplied_code, 30, 6, 1) {            // 6 digits, ±30s drift
    return Err("bad TOTP code");
}
```

Funciona con Google Authenticator, Authy, 1Password, Bitwarden y otras aplicaciones autenticadoras estándar.

**Códigos de recuperación** (códigos de respaldo de un solo uso para cuando un usuario pierde su teléfono) todavía no se incluyen. El patrón común es almacenar de 8 a 10 códigos hasheados por usuario y consumir uno cada vez que se usa.

---

## Enviar URLs firmadas (enlaces mágicos)

Una URL firmada lleva una firma a prueba de manipulaciones, de modo que puedes confiar en ella sin una sesión — perfecta para inicios de sesión con enlace mágico, restablecimientos de contraseña y enlaces de descarga de tiempo limitado. Firmas una URL con `sign` (opcionalmente con una caducidad) y la verificas con `verify` cuando el usuario hace clic. (Laravel las llama "rutas firmadas".)

```rust
use rustango::signed_url::{sign, verify, SignedUrlError};
use std::time::Duration;

// Issue a 1-hour magic-link login URL
let url = sign(
    "https://app.example.com/auth/login?email=alice@x.com",
    secret,
    Some(Duration::from_secs(3600)),
);
// Send via email...

// On the callback handler
match verify(&incoming_url, secret) {
    Ok(()) => { /* identity confirmed */ }
    Err(SignedUrlError::Expired) => { /* prompt to request a new link */ }
    Err(SignedUrlError::InvalidSignature) => { /* tampered — log + 401 */ }
    Err(_) => { /* malformed */ }
}
```

Usos comunes:
- Inicio de sesión con enlace mágico (envía por email una URL de un solo uso en lugar de pedir la contraseña)
- Confirmación de restablecimiento de contraseña
- Descargas de archivos de tiempo limitado
- Enlaces de "Haz clic aquí para verificar tu email"
- Enlaces de baja de suscripción (sin necesidad de auth; la propia URL prueba la intención)

Antes de firmar, los parámetros de consulta se ordenan en un orden fijo, así que un atacante no puede reordenarlos (por ejemplo moviendo `?expires=...`) para cambiar lo que cubre la firma y falsificar un enlace de aspecto válido.

> Análisis en profundidad: [Flujos de cuenta](auth-flows.md) construye el restablecimiento de contraseña, la verificación de email y el inicio de sesión con enlace mágico sobre esta primitiva de URL firmada.

---

## Verificar webhooks entrantes

Un webhook es una llamada de retorno HTTP que otro servicio te envía (un pago tuvo éxito, ocurrió un push). Comprueba siempre su firma para saber que realmente vino de ese servicio y que el cuerpo no fue modificado. `verify_signature` maneja los formatos comunes.

```rust
use rustango::webhook::{verify_signature, SignatureFormat};

async fn handle_stripe_webhook(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let signature = headers.get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !verify_signature(SignatureFormat::HexSha256, secret, &body, signature) {
        return StatusCode::UNAUTHORIZED;
    }
    // ... process the verified payload
    StatusCode::OK
}
```

| Formato | Usado por |
|---|---|
| `HexSha256WithPrefix` | GitHub (`sha256=<hex>`) |
| `HexSha256` | Slack, proveedores HMAC en bruto |
| `Base64Sha256` | Stripe, AWS SNS |

La comparación de la firma es de tiempo constante, lo que significa que siempre tarda la misma cantidad de tiempo tanto si la conjetura es correcta como si es incorrecta. Eso detiene los ataques de temporización, donde un atacante mide diminutas diferencias en el tiempo de respuesta para adivinar el secreto un carácter a la vez.

---

## Mantener los secretos fuera de tus registros

Las URLs registradas pueden filtrar contraseñas y tokens que aparecen en las cadenas de consulta. `AccessLogLayer` enmascara automáticamente los parámetros de credenciales conocidos antes de escribir la línea de registro (PII = información de identificación personal).

```rust
let app = router.access_log(AccessLogLayer::default());
```

Por defecto, los valores de estos parámetros de consulta se reemplazan por `[redacted]`:
`password`, `passwd`, `token`, `secret`, `api_key`, `apikey`, `access_token`, `refresh_token`, `signature`, `auth`.

Añadir a la lista:

```rust
let layer = AccessLogLayer::default()
    .redact_additional("session_id")
    .redact_additional("private_key");
```

Reemplazar la lista completa:

```rust
let layer = AccessLogLayer::default()
    .redact(vec!["only_this".into()]);
```

Nota: esto solo redacta las **cadenas de consulta**. Para depurar los cuerpos o las cabeceras de las peticiones, filtra en el origen con `tracing::Subscriber`.

---

## Rastrear peticiones a través de servicios

Un ID de petición etiqueta cada línea de registro de una petición con el mismo ID, de modo que puedes seguir esa petición a través de los manejadores — y a través de los servicios. `RequestIdLayer` añade uno a cada petición.

```rust
let app = router.request_id(RequestIdLayer::default());
```

Reutiliza un `X-Request-Id` entrante (para que los servicios encadenados compartan un ID) o genera un ID base64 de 22 caracteres nuevo. Los IDs entrantes se validan primero — cualquiera con caracteres de control, saltos de línea, bytes nulos, o de más de 128 caracteres se rechaza, lo que bloquea los ataques de inyección de cabeceras.

En los manejadores:

```rust
async fn handler(id: rustango::request_id::RequestId) -> String {
    tracing::info!(req_id = %id.0, "handling /me");
    format!("req {}", id.0)
}
```

En una configuración de confianza cero, donde no confías en los IDs enviados por los servicios upstream, genera siempre el tuyo propio:

```rust
router.request_id(RequestIdLayer::always_generate());
```

---

## Gestionar secretos

Nunca codifiques secretos (contraseñas de base de datos, claves de API) directamente en tu código fuente. Léelos en tiempo de ejecución a través del trait `Secrets`, que abstrae dónde viven realmente. (Un "trait" es la versión de Rust de una interfaz.) El `EnvSecrets` incorporado lee de las variables de entorno.

```rust
use rustango::secrets::{Secrets, EnvSecrets, BoxedSecrets};
use std::sync::Arc;

// Env vars (with optional prefix)
let secrets: BoxedSecrets = Arc::new(EnvSecrets::with_prefix("MYAPP_"));

let db_pwd = secrets.require("DB_PASSWORD").await?;       // reads MYAPP_DB_PASSWORD
let redis_url = secrets.get("REDIS_URL").await?;          // None when unset
```

Para obtener secretos de Vault, AWS Secrets Manager o GCP Secret Manager, implementa el trait tú mismo — es solo un método asíncrono.

---

## Auditar antes de desplegar

Antes de ir a producción, ejecuta la auditoría incorporada. Detecta configuraciones erróneas comunes — como un secreto de firma ausente o demasiado corto — que no quieres descubrir en producción.

```bash
manage check --deploy
```

Comprobaciones siempre activas (se ejecutan con o sin `--deploy`):
- ✅ Modelos registrados en el inventario
- ✅ BD accesible (`SELECT 1`)
- ✅ Migraciones en disco para los modelos registrados

Comprobaciones adicionales de `--deploy` (endurecimiento para producción):
- ✅ `RUSTANGO_ENV` es `prod` o `production`
- ✅ `RUSTANGO_SESSION_SECRET` establecida y ≥ 32 bytes (la clave HMAC para la firma de cookies + JWT — `SECRET_KEY` **no** la lee el framework), sin ningún marcador de posición del scaffolder dejado dentro
- ✅ `DATABASE_URL` establecida (y advierte si apunta a localhost)
- ⚠️ Advertencias de coherencia de `RUSTANGO_APEX_DOMAIN` / `RUSTANGO_BIND` para la tenencia + el enlace no de loopback

Devuelve un código de salida distinto de cero ante cualquier hallazgo de **nivel de error** (bueno para las puertas de CI). Las advertencias no provocan un fallo pero aparecen en la salida.

Para comprobaciones más allá de las que se incluyen, extiende con código personalizado en tu binario `manage` antes de reenviar a `manage::run`.

---

## Lo que TODAVÍA NO se incluye

Un par de flujos de extremo a extremo todavía necesitan ensamblarse a partir de las primitivas de abajo:

- **Restablecimiento de contraseña + verificación de email** — los ayudantes de emisión/verificación de tokens existen (`auth_flows::confirm_password_reset_pool`, además de los viajes de ida y vuelta de verificación de email) y la canalización de email se incluye por separado, pero no hay un único ciclo prefabricado de vista + email + validación conectado para ti.
- **Redacción de PII del cuerpo / las cabeceras de la petición** en `access_log` — existe la redacción de registros a nivel de campo (consulta [Mantener los secretos fuera de tus registros](#keeping-secrets-out-of-your-logs)), pero no la depuración automática del cuerpo o las cabeceras de la petición.

Ya incluido (no recurras a un apaño):

- **Inicio de sesión social OAuth2 / OIDC** — `oauth2::providers` incluye ayudantes para Google, GitHub, Microsoft, GitLab y Discord (además de `OAuth2Provider::from_discovery` para cualquier proveedor OIDC), y `oauth2::router::oauth2_router` monta las rutas de inicio de sesión + callback, crea el registro de usuario y establece la cookie de sesión.
- **Bloqueo por cuenta** — `rustango::account_lockout::Lockout` (respaldado por caché; `is_locked` / `record_failure` / `clear`, con `max_attempts` + `lockout_duration` configurables).
- **Endpoint de informe de CSP** — `security_headers::csp_report_router(path)` + `SecurityHeadersLayer::csp_report_uri(uri)`.
- **Limitación de tasa distribuida** — `rate_limit_cache::CacheRateLimitLayer` (consulta [Limitar la tasa de peticiones](#rate-limiting-requests)).

Hasta que aterricen los flujos no incluidos, ensámblalos a partir de las primitivas de arriba (`signed_url::sign` para los tokens de restablecimiento de contraseña, la canalización de email para la entrega, etc.).

---

## Véase también

- [Middleware](middleware.md) — cómo se adjuntan las capas de seguridad, su orden, y el catálogo completo.
- [Autenticación](auth-passwords.md) — contraseñas, sesiones, JWTs, claves de API, HMAC, backends de auth, y flujos de cuenta, cada uno en profundidad.
- [Convenciones de API](api-conventions.md) — las reglas de tipo de error y tipo de retorno detrás de estas APIs.
