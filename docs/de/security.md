# Sicherheitsleitfaden

Dieser Leitfaden behandelt jede Sicherheitsfunktion, die **Rustango** mitbringt, und wie man sie kombiniert. Wenn du von Django, Laravel oder Rails kommst, werden dir die meisten davon vertraut vorkommen — die Namen unterscheiden sich, aber die Ideen sind dieselben. Jede der folgenden Funktionen benötigt in der Regel eine Zeile Setup. Wenn du bereit bist, in Produktion zu gehen, führe `manage check --deploy` für ein automatisiertes Audit aus.

[![Der gehärtete Middleware-Stack in einer Kette verdrahtet: Request-IDs, Zugriffsprotokollierung, Rate Limiting, CORS und Security-Header](img/security.png)](img/security.png)

## Inhaltsverzeichnis

- [Die Defense-in-Depth-Checkliste](#the-defense-in-depth-checklist)
- [Security-Header setzen](#setting-security-headers)
- [Cross-Origin-Anfragen erlauben (CORS)](#allowing-cross-origin-requests-cors)
- [Anfragen per Rate Limiting drosseln](#rate-limiting-requests)
- [IPs erlauben oder blockieren](#allowing-or-blocking-ips)
- [Schutz vor CSRF](#protecting-against-csrf)
- [XSS verhindern](#preventing-xss)
- [SQL-Injection verhindern](#preventing-sql-injection)
- [Benutzer authentifizieren](#authenticating-users)
- [Passwörter hashen und prüfen](#hashing-and-checking-passwords)
- [JWTs ausstellen und erneuern](#issuing-and-refreshing-jwts)
- [Mit API-Keys authentifizieren](#authenticating-with-api-keys)
- [Zwei-Faktor-Authentifizierung hinzufügen (TOTP)](#adding-two-factor-auth-totp)
- [Signierte URLs versenden (Magic Links)](#sending-signed-urls-magic-links)
- [Eingehende Webhooks verifizieren](#verifying-incoming-webhooks)
- [Secrets aus deinen Logs heraushalten](#keeping-secrets-out-of-your-logs)
- [Anfragen über Services hinweg nachverfolgen](#tracing-requests-across-services)
- [Secrets verwalten](#managing-secrets)
- [Vor dem Deploy auditieren](#auditing-before-you-deploy)

---

## Die Defense-in-Depth-Checkliste

Gute Sicherheit entsteht aus vielen kleinen Schichten, nicht aus einer großen Mauer. Eine produktive **Rustango**-App sollte die folgenden Schichten stapeln — die meisten davon sind jeweils eine Zeile. Der Rest dieses Leitfadens erklärt jede einzelne.

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

## Security-Header setzen

Security-Header teilen dem Browser mit, wie er deine Benutzer schützen soll (Clickjacking blockieren, HTTPS erzwingen, Content-Type-Sniffing unterbinden). `SecurityHeadersLayer` setzt das Standardset mit einer Zeile — dieselben Header, die Django standardmäßig mitbringt. (Ein „Layer" ist **Rustango**s Begriff für Middleware; du hängst sie an deinen Router an.)

> Vertiefung: [Middleware](middleware.md) behandelt, wie Layer funktionieren, ihre Reihenfolge, den vollständigen eingebauten Katalog und das Schreiben eigener Layer (locale-, zeitzonenbewusst, Header, CSRF).

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};

let app = router.security_headers(SecurityHeadersLayer::strict());
```

### Presets

| Preset | Wann verwenden |
|---|---|
| `strict()` | Produktion: HSTS preload + XFO=DENY + nosniff + Referrer-Policy=no-referrer + COOP=same-origin + Permissions-Policy gesperrt |
| `relaxed()` | Einbettbar in iframes: SAMEORIGIN + 1 Jahr HSTS |
| `dev()` | Lokal: nur nosniff (kein HSTS, um localhost nicht für immer in HTTPS zu sperren) |
| `empty()` | Von Grund auf aufbauen |

### Benutzerdefinierte CSP

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

### Gestaffelter CSP-Rollout

Nutze `csp_report_only`, um zu überwachen, ohne die Produktion zu brechen:

```rust
let layer = SecurityHeadersLayer::strict()
    .csp(CspBuilder::strict_starter().build())
    .csp_report_only(true);                       // sends Content-Security-Policy-Report-Only
```

Nachdem du verifiziert hast, dass keine Konsolenfehler auftreten → schalte `csp_report_only(false)`, um zu erzwingen.

---

## Cross-Origin-Anfragen erlauben (CORS)

CORS steuert, welche anderen Websites deine API aus einem Browser heraus aufrufen dürfen. Standardmäßig blockieren Browser diese Aufrufe; du lässt bestimmte Origins gezielt wieder zu. Liste in Produktion deine echten Frontend-Domains auf und öffne es niemals für alle.

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

**Sicherheitshinweis:** Kombiniere niemals `allow_credentials(true)` mit `allow_any_origin()` — der Browser wird die Antwort ablehnen. Mit Credentials MUSST du explizite Origins auflisten.

---

## Anfragen per Rate Limiting drosseln

Rate Limiting begrenzt, wie viele Anfragen ein Client in einem Zeitfenster stellen kann. Nutze es, um Brute-Force-Logins, Scraping und Missbrauch auszubremsen. Du kannst pro IP, pro API-Key oder global für einen teuren Endpunkt begrenzen.

```rust
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};

// Per-IP
router.rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)));

// Per-API-key (header)
router.rate_limit(RateLimitLayer::per_header("authorization", 1000, Duration::from_secs(3600)));

// Global ceiling for an expensive endpoint
router.rate_limit(RateLimitLayer::global(10, Duration::from_secs(1)));
```

Bei Erschöpfung: `429 Too Many Requests` mit `Retry-After`-Header. Jede erfolgreiche Antwort enthält `X-RateLimit-Limit` + `X-RateLimit-Remaining`.

> **Hinter einem Reverse Proxy: kombiniere `per_ip` mit `real_ip`.** `RateLimitLayer::per_ip` schlüsselt auf den verbindenden Socket (`ConnectInfo`), was hinter einem Proxy die IP des *Proxys* ist — also teilen sich alle Clients einen Bucket und das Limit ist nutzlos. Setze `real_ip::RealIpLayer` (liest `X-Forwarded-For` / `X-Real-IP`) davor, damit die echte Client-IP verwendet wird.

`RateLimitLayer` ist **prozesslokal** — es zählt Anfragen nur innerhalb einer laufenden Instanz, was in Ordnung ist, wenn du eine einzelne Instanz betreibst. Wenn du mehrere Instanzen (Replicas) hinter einem Load Balancer betreibst, würde jede ihre eigene Zählung führen, sodass sich das reale Limit vervielfacht. Um eine Zählung über alle Replicas hinweg zu teilen, nutze `rate_limit_cache::CacheRateLimitLayer`, das an eine beliebige `cache::Cache`-Implementierung delegiert (kombiniere es mit `cache::RedisCache` für einen gemeinsamen Zähler, der atomar per Redis `INCRBY` inkrementiert wird):

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

## IPs erlauben oder blockieren

Sperre sensible Routen (wie dein Admin) auf eine bekannte Menge von IP-Adressen ein oder blockiere bestimmte Missbraucher. Eine Allowlist erlaubt nur die aufgeführten Netzwerke; eine Blocklist verweigert die aufgeführten.

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

**IPv4 + IPv6 werden unterstützt.** Familienübergreifend sicher (ein IPv4-CIDR matcht keine IPv6-Adressen). Gibt bei Ablehnung `403 Forbidden` zurück.

**Wichtig:** **Rustango** liest die Client-IP aus `ConnectInfo<SocketAddr>`. Mounte mit:

```rust
axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

Wenn dein Reverse Proxy `X-Forwarded-For` weiterleitet, konfiguriere ihn so, dass er `RemoteAddr` setzt — `ip_filter` liest den verbindenden Peer, nicht die Header.

---

## Schutz vor CSRF

> **CSRF und Token-APIs.** CSRF schützt **Cookie-authentifizierte** Anfragen. Eine
> reine Token- / Bearer- / [JWT](auth-jwt.md)-API ist kein CSRF-Ziel — wende
> den CSRF-Layer nicht darauf an (sonst gibst du legitimen Clients ein 403). Für eine SPA, die
> *tatsächlich* Cookies verwendet, lies das `rustango_csrf`-Cookie und gib es im `X-CSRF-Token`-
> Header zurück.

CSRF (Cross-Site Request Forgery) liegt vor, wenn eine andere Website den Browser eines eingeloggten Benutzers dazu bringt, eine Anfrage an deine App zu senden. Die Verteidigung ist ein geheimes Token in jedem Formular, genau wie Djangos `{% csrf_token %}`. Die CSRF-Middleware liegt in `rustango::forms::csrf` (hinter dem `csrf`-Feature, das durch das `admin`-Feature automatisch aktiviert wird):

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/contact", get(form).post(submit))
    .layer(csrf::layer());
```

`csrf::layer()` baut den Layer mit sinnvollen Standardwerten; `csrf::with_config(CsrfConfig)` erlaubt dir, die Cookie-/Header-Namen und das `Secure`-Flag zu überschreiben. In Templates gibt `{{ csrf_token }}` das rohe Token und `{{ csrf_input }}` ein fertiges verstecktes `<input>` — platziere eines in jedem Formular. Es verwendet das Double-Submit-Cookie-Muster: bei unsicheren Methoden (POST, PUT, PATCH, DELETE) prüft der Layer den `X-CSRF-Token`-Header (oder das `_csrf`-Formularfeld) gegen das `rustango_csrf`-Cookie; eine Nichtübereinstimmung gibt `403 Forbidden` zurück.

**Collector-Endpunkte ausnehmen.** `CsrfConfig::exempt_prefix("/path")` (wiederholbar) überspringt die CSRF-Durchsetzung für unsichere Methoden bei Anfragen, deren Pfad mit dem angegebenen Präfix beginnt. Das ist für Append-only-, zustandslose Endpunkte gedacht, die per `navigator.sendBeacon` angesprochen werden — z. B. ein Analytics-Collector — die keinen `X-CSRF-Token`-Header setzen können und, wenn die Seite aus einem CDN-Cache ausgeliefert wird, der `Set-Cookie` entfernt, möglicherweise gar kein CSRF-Cookie mitführen. Halte Präfixe eng und nimm niemals etwas aus, das Auth-Zustand liest oder schreibt.

Der Auto-Admin aktiviert CSRF standardmäßig bei jeder Mutation, und es gibt keine Möglichkeit, sich davon abzumelden.

---

## XSS verhindern

XSS (Cross-Site Scripting) tritt auf, wenn Benutzereingaben als HTML gerendert werden und als Code im Browser einer anderen Person laufen. Die Lösung ist, jede Benutzereingabe zu escapen, bevor sie die Seite erreicht. **Rustango** löst das auf zwei Wegen:

**1. Tera-Template-Auto-Escape** — Tera ist **Rustango**s Template-Engine (wie Django-Templates oder Blade). Jedes `{{ var }}` wird automatisch HTML-escapt. Verwende `{{ var | safe }}`, um dich abzumelden — selten und gefährlich, also tue das nur für HTML, dem du vollständig vertraust.

**2. Manueller Escape-Helper** — für den Fall, dass du HTML in Rust-Code statt in einem Template baust:

```rust
use rustango::text::html_escape;

let safe = html_escape(user_input);
// "<script>" → "&lt;script&gt;"
// "a & b" → "a &amp; b"
```

Ersetzt `&`, `<`, `>`, `"`, `'`. Geeignet für HTML-Elementinhalte + doppelt-quotierte Attribute.

Für CSP-basierte XSS-Verteidigung siehe [Security-Header setzen](#setting-security-headers).

---

## SQL-Injection verhindern

SQL-Injection tritt auf, wenn Benutzereingaben direkt in einen SQL-String eingefügt werden und die Query verändern. **Rustango**s ORM verhindert das durch Design: es verwendet überall sqlx-Parameter-Binding — jeder Wert wird als `$1, $2, ...`-Platzhalter gesendet, niemals in den Query-Text eingeklebt. (sqlx ist die zugrunde liegende Datenbankbibliothek.) **Du kannst über die ORM-API nicht versehentlich Benutzereingaben in SQL konkatenieren.**

Es gibt zwei Stellen, bei denen du weiterhin vorsichtig sein solltest:

**1. Tabellen- und Spaltennamen in `bulk_actions`:**

Platzhalter schützen Werte, können aber keinen Bezeichner (einen Tabellen- oder Spaltennamen) tragen, also benötigen diese ihren eigenen Schutz. Der Bulk-Action-Runner klebt niemals rohe, benutzergelieferte Tabellen-/Spaltennamen in SQL. Intern validiert er jeden Bezeichner (ein crate-privates
`validate_ident`, das `"`, `` ` ``, `\0`, `\n`, `\r`, `\`, `;`,
Leerzeichen und Steuerzeichen ablehnt) und quotet ihn dann per `dialect.quote_ident()`,
bevor die Anweisung gebaut wird. Dieser Schutz läuft für dich — du rufst ihn nicht
selbst auf.

**2. Der Raw-SQL-Notausgang:**

Wenn du auf rohes `sqlx` heruntergehst, binde jeden *Wert* als Parameter und
füge niemals einen *Bezeichner* (Tabellen- oder Spaltenname), der aus
Benutzereingaben stammte, in den Query-String ein — Platzhalter können keine Bezeichner tragen:

```rust
// ✅ values go through bound placeholders
sqlx::query("SELECT * FROM posts WHERE id = $1")
    .bind(1).fetch_all(&pool).await?;

// ❌ NEVER interpolate a user-supplied identifier into the SQL string
let sql = format!("SELECT * FROM \"{user_table}\" WHERE id = $1");
sqlx::query(&sql).bind(1).fetch_all(&pool).await?;
```

---

## Benutzer authentifizieren

Authentifizierung ist die Art, wie du bestätigst, wer eine Anfrage stellt. **Rustango** bringt drei fertige Backends mit (Basic Auth, API-Keys und JWTs) und lässt dich eigene schreiben — ganz ähnlich wie Djangos Authentifizierungs-Backends. Du hängst sie an Routen an, und Anfragen ohne ein erkanntes Credential erhalten ein `401`.

> **Admin-SSO.** Um Betreibern zu erlauben, sich mit einem externen IdP (Google, Microsoft/Azure AD, GitHub oder einem beliebigen OpenID-Connect-Provider) statt mit einem Passwort im Admin anzumelden, aktiviere das `admin-sso`-Feature — siehe den [SSO-Leitfaden](sso.md). Provider werden **im Admin-UI als Zeilen verwaltet** (mehrere pro Surface; pro Tenant oder ein gemeinsames Set über Tenants hinweg), wobei das Client-Secret **verschlüsselt gespeichert** wird. Es ist Link-to-existing (die verifizierte IdP-E-Mail muss mit einem Admin-Benutzer übereinstimmen; kein Auto-Provisioning) und verwendet die bestehende Session wieder.

### Drei fertige Backends

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

Die Middleware probiert jedes Backend der Reihe nach. Das erste, das erfolgreich ist, gewinnt; das erste, das einen harten Fehler zurückgibt, stoppt die Kette.

### Ein eigenes Backend schreiben

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

## Passwörter hashen und prüfen

Speichere Passwörter niemals als Klartext. Hashe sie bei der Registrierung mit `hash` und vergleiche beim Login den Versuch mit `verify`. Nutze `strength_score`, um schwache Passwörter abzulehnen, bevor du sie überhaupt hashst.

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

Das Hashing verwendet **argon2id** (der von OWASP empfohlene Standard seit 2023 — es widersteht GPU-Cracking besser als bcrypt). Hashes werden im PHC-String-Format gespeichert, sodass du Parameter später ändern kannst, ohne alte Hashes zu brechen.

Die eingebaute **Stärkeprüfung** ist bewusst minimal. Für ernsthafte Deployments prüfe Passwörter zusätzlich gegen die HIBP- / pwned-passwords-API, um bekanntermaßen geleakte abzulehnen.

> **Vertiefung:** [Passwörter](auth-passwords.md) — argon2id-Interna, automatisches Salting, timing-sichere Logins (`verify_dummy`) und wo der Hash liegt. Sobald authentifiziert, übergib an eine [Session](auth-sessions.md) (Browser) oder ein [JWT](auth-jwt.md) (API).

---

## JWTs ausstellen und erneuern

Ein JWT (JSON Web Token) ist ein signiertes Token, das ein Client sendet, anstatt sich jedes Mal einzuloggen. `JwtLifecycle` handhabt den gesamten Ablauf: ein kurzlebiges Access-Token plus ein länger lebendes Refresh-Token, mit eingebautem Verify, Refresh und Revoke.

```rust
use rustango::tenancy::jwt_lifecycle::{JwtLifecycle, JwtTokenPair, JwtIssueError};
use serde_json::json;

let jwt = JwtLifecycle::new(secret)
    .with_access_ttl(900)                         // 15 min
    .with_refresh_ttl(7 * 86400);                 // 7 days
```

### Ein Token mit benutzerdefinierten Claims ausstellen (kein DB-Lookup beim Verify)

```rust
let pair = jwt.issue_pair_with(user_id, json!({
    "roles":  ["admin", "editor"],
    "tenant": "acme",
    "scope":  "read:posts write:posts",
    "email":  "alice@example.com",
}).as_object().unwrap().clone())?;
```

„Claims" sind die Schlüssel/Wert-Fakten, die du in das Token einbettest (Rollen, Tenant und so weiter); der Client sendet sie zurück und du liest sie, ohne die Datenbank anzusprechen. Reservierte Claim-Namen (`sub`, `exp`, `jti`, `typ`) werden vom Framework verwendet und dürfen nicht in deinen benutzerdefinierten Claims auftauchen — der Versuch gibt `JwtIssueError::ReservedClaim` zurück.

### Ein Token verifizieren

```rust
let claims = jwt.verify_access(&access_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
let tenant: String = claims.get_custom("tenant").unwrap_or_default();
```

### Ein Token erneuern (behält deine benutzerdefinierten Claims)

```rust
let new_pair = jwt.refresh(&refresh_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;
// new_pair has the same roles + scope + tenant
```

Die JTI des alten Refresh-Tokens (seine eindeutige ID) wird zu einer Blacklist hinzugefügt, sodass es nicht wiederverwendet werden kann — das blockiert Replay-Angriffe, bei denen ein gestohlenes altes Token erneut gesendet wird.

### Erneuern mit neu geprüften Berechtigungen

```rust
// returns Result<Option<JwtTokenPair>, JwtIssueError>:
// Err on a reserved-claim collision, Ok(None) on an invalid/expired refresh.
let new_pair = jwt.refresh_with(&refresh_token, json!({
    "roles": ["viewer"],     // role was demoted since login
    "tenant": "acme",
}).as_object().unwrap().clone())?
    .ok_or(StatusCode::UNAUTHORIZED)?;
```

### Ein Token widerrufen

```rust
jwt.revoke(&access_token);     // adds JTI to blacklist
jwt.revoke(&refresh_token);
```

Die standardmäßige In-Memory-Blacklist (`InMemoryJtiStore`) räumt abgelaufene Einträge von selbst aus, aber sie lebt in einem Prozess und vergisst bei jedem Neustart jeden Widerruf. Für Multi-Prozess-Deployments installiere einen gemeinsamen, dauerhaften Store über `JwtLifecycle::new(secret).with_jti_store(store)` — `store` ist ein beliebiger `Arc<dyn JtiStore>` (zum Beispiel ein Redis- oder DB-gestützter). Ohne einen gemeinsamen Store kann ein Token, das du auf einer Replica widerrufst, auf einer anderen weiterhin abgespielt werden, bis es abläuft.

> **Vertiefung:** [JWT-Auth-API](auth-jwt-api.md) — der eingebaute `/api/auth/login|refresh|logout|me`-Router, gleitendes Refresh und der JTI-Widerrufs-Store. Für ein einzelnes selbstverwaltetes Token siehe [JWT (standalone)](auth-jwt.md). Um Browser-/API-Routen per Login zu gaten, siehe [Access-Decorators](auth-decorators.md).

---

## Mit API-Keys authentifizieren

API-Keys erlauben Skripten und Services die Authentifizierung ohne Benutzername, Passwort oder Session — praktisch für Machine-to-Machine-Zugriff. Du generierst einen Key, zeigst ihn dem Benutzer einmal und speicherst nur seinen Präfix und Hash.

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

Diese Keys funktionieren direkt mit dem Multi-Tenancy-`ApiKeyBackend` — beide verwenden dasselbe `{prefix}.{secret}`-Format mit argon2id-gehashten Secrets, sodass ein hier ausgestellter Key dort ohne zusätzliche Arbeit authentifiziert.

---

## Zwei-Faktor-Authentifizierung hinzufügen (TOTP)

TOTP ist der 6-stellige Code aus einer Authenticator-App — ein zweiter Faktor zusätzlich zum Passwort. Du generierst pro Benutzer ein Secret, zeigst es als QR-Code zum Einrichten an und verifizierst dann bei jedem Login den eingetippten Code. Es folgt dem RFC-6238-Standard.

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

Funktioniert mit Google Authenticator, Authy, 1Password, Bitwarden und anderen Standard-Authenticator-Apps.

**Recovery-Codes** (einmalige Backup-Codes für den Fall, dass ein Benutzer sein Telefon verliert) werden noch nicht mitgeliefert. Das gängige Muster ist, 8–10 gehashte Codes pro Benutzer zu speichern und einen bei jeder Verwendung zu verbrauchen.

---

## Signierte URLs versenden (Magic Links)

Eine signierte URL trägt eine manipulationssichere Signatur, sodass du ihr ohne Session vertrauen kannst — perfekt für Magic-Link-Logins, Passwort-Resets und zeitlich begrenzte Download-Links. Du `sign`st eine URL (optional mit einem Ablaufdatum) und `verify`st sie, wenn der Benutzer klickt. (Laravel nennt diese „signed routes".)

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

Häufige Einsatzzwecke:
- Magic-Link-Login (eine einmalige URL per E-Mail versenden, statt nach einem Passwort zu fragen)
- Passwort-Reset-Bestätigung
- Zeitlich begrenzte Datei-Downloads
- „Hier klicken, um deine E-Mail zu verifizieren"-Links
- Abmelde-Links (keine Auth nötig; die URL selbst beweist die Absicht)

Vor dem Signieren werden die Query-Parameter in eine feste Reihenfolge sortiert, sodass ein Angreifer sie nicht umordnen kann (zum Beispiel durch Verschieben von `?expires=...`), um zu ändern, was die Signatur abdeckt, und einen gültig aussehenden Link zu fälschen.

> Vertiefung: [Account-Flows](auth-flows.md) baut Passwort-Reset, E-Mail-Verifizierung und Magic-Link-Login auf diesem Signed-URL-Primitive auf.

---

## Eingehende Webhooks verifizieren

Ein Webhook ist ein HTTP-Callback, den ein anderer Service dir sendet (eine Zahlung war erfolgreich, ein Push ist passiert). Prüfe immer seine Signatur, damit du weißt, dass er wirklich von diesem Service kam und der Body nicht verändert wurde. `verify_signature` handhabt die gängigen Formate.

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

| Format | Verwendet von |
|---|---|
| `HexSha256WithPrefix` | GitHub (`sha256=<hex>`) |
| `HexSha256` | Slack, rohe HMAC-Provider |
| `Base64Sha256` | Stripe, AWS SNS |

Der Signaturvergleich ist konstant-zeitlich, das heißt, er dauert immer gleich lang, egal ob die Vermutung richtig oder falsch ist. Das stoppt Timing-Angriffe, bei denen ein Angreifer winzige Antwortzeit-Unterschiede misst, um das Secret Zeichen für Zeichen zu erraten.

---

## Secrets aus deinen Logs heraushalten

Geloggte URLs können Passwörter und Tokens leaken, die in Query-Strings auftauchen. `AccessLogLayer` maskiert automatisch bekannte Credential-Parameter, bevor die Log-Zeile geschrieben wird (PII = personenbezogene Daten).

```rust
let app = router.access_log(AccessLogLayer::default());
```

Standardmäßig werden Werte für diese Query-Parameter durch `[redacted]` ersetzt:
`password`, `passwd`, `token`, `secret`, `api_key`, `apikey`, `access_token`, `refresh_token`, `signature`, `auth`.

Zur Liste hinzufügen:

```rust
let layer = AccessLogLayer::default()
    .redact_additional("session_id")
    .redact_additional("private_key");
```

Die gesamte Liste ersetzen:

```rust
let layer = AccessLogLayer::default()
    .redact(vec!["only_this".into()]);
```

Hinweis: Dies redaktiert nur **Query-Strings**. Um Request-Bodies oder Header zu bereinigen, filtere an der Quelle mit `tracing::Subscriber`.

---

## Anfragen über Services hinweg nachverfolgen

Eine Request-ID versieht jede Log-Zeile für eine Anfrage mit derselben ID, sodass du dieser Anfrage über Handler hinweg — und über Services hinweg — folgen kannst. `RequestIdLayer` fügt jeder Anfrage eine hinzu.

```rust
let app = router.request_id(RequestIdLayer::default());
```

Es verwendet eine eingehende `X-Request-Id` wieder (sodass verkettete Services eine ID teilen) oder generiert eine frische 22-Zeichen-Base64-ID. Eingehende IDs werden zuerst validiert — alles mit Steuerzeichen, Zeilenumbrüchen, Null-Bytes oder länger als 128 Zeichen wird abgelehnt, was Header-Injection-Angriffe blockiert.

In Handlern:

```rust
async fn handler(id: rustango::request_id::RequestId) -> String {
    tracing::info!(req_id = %id.0, "handling /me");
    format!("req {}", id.0)
}
```

In einem Zero-Trust-Setup, in dem du IDs, die von Upstream-Services gesendet werden, nicht vertraust, erstelle immer deine eigene:

```rust
router.request_id(RequestIdLayer::always_generate());
```

---

## Secrets verwalten

Hardcode Secrets (Datenbank-Passwörter, API-Keys) niemals in deinem Quellcode. Lies sie zur Laufzeit über das `Secrets`-Trait, das darüber abstrahiert, wo sie tatsächlich liegen. (Ein „Trait" ist Rusts Version eines Interface.) Das eingebaute `EnvSecrets` liest aus Umgebungsvariablen.

```rust
use rustango::secrets::{Secrets, EnvSecrets, BoxedSecrets};
use std::sync::Arc;

// Env vars (with optional prefix)
let secrets: BoxedSecrets = Arc::new(EnvSecrets::with_prefix("MYAPP_"));

let db_pwd = secrets.require("DB_PASSWORD").await?;       // reads MYAPP_DB_PASSWORD
let redis_url = secrets.get("REDIS_URL").await?;          // None when unset
```

Um Secrets aus Vault, AWS Secrets Manager oder GCP Secret Manager zu ziehen, implementiere das Trait selbst — es ist nur eine async Methode.

---

## Vor dem Deploy auditieren

Bevor du in Produktion gehst, führe das eingebaute Audit aus. Es fängt häufige Fehlkonfigurationen ab — wie ein fehlendes oder zu kurzes Signing-Secret — die du nicht in der Produktion entdecken möchtest.

```bash
manage check --deploy
```

Immer aktive Prüfungen (laufen mit oder ohne `--deploy`):
- ✅ Modelle im Inventory registriert
- ✅ DB erreichbar (`SELECT 1`)
- ✅ Migrationen auf der Platte für registrierte Modelle

Zusätzliche `--deploy`-Prüfungen (Produktionshärtung):
- ✅ `RUSTANGO_ENV` ist `prod` oder `production`
- ✅ `RUSTANGO_SESSION_SECRET` gesetzt und ≥ 32 Bytes (der HMAC-Schlüssel für Cookie- + JWT-Signierung — `SECRET_KEY` wird vom Framework **nicht** gelesen), ohne verbliebenen Scaffolder-Platzhalter
- ✅ `DATABASE_URL` gesetzt (und warnt, wenn es auf localhost zeigt)
- ⚠️ `RUSTANGO_APEX_DOMAIN` / `RUSTANGO_BIND` Plausibilitätswarnungen für Tenancy + Nicht-Loopback-Binding

Gibt bei jedem Befund auf **Error-Level** einen Exit-Code ungleich null zurück (gut für CI-Gates). Warnungen lösen kein Fehlschlagen aus, erscheinen aber in der Ausgabe.

Für Prüfungen jenseits des Mitgelieferten erweitere mit benutzerdefiniertem Code in deinem `manage`-Binary, bevor du an `manage::run` weiterleitest.

---

## Was NOCH nicht mitgeliefert wird

Ein paar End-to-End-Flows müssen noch aus den untenstehenden Primitiven zusammengeklebt werden:

- **Passwort-Reset + E-Mail-Verifizierung** — die Token-Issue-/Verify-Helper existieren (`auth_flows::confirm_password_reset_pool`, plus E-Mail-Verifizierungs-Round-Trips) und die E-Mail-Pipeline wird separat mitgeliefert, aber es gibt keinen einzelnen vorgefertigten View- + E-Mail- + Validate-Zyklus, der für dich verdrahtet ist.
- **PII-Redaktion von Request-Body / Headern** in `access_log` — feldbasierte Log-Redaktion existiert (siehe [Secrets aus deinen Logs heraushalten](#keeping-secrets-out-of-your-logs)), aber keine automatische Bereinigung des Request-Bodys oder der Header.

Bereits mitgeliefert (greife nicht zu einem Workaround):

- **OAuth2 / OIDC Social Login** — `oauth2::providers` bringt Google-, GitHub-, Microsoft-, GitLab- und Discord-Helper mit (plus `OAuth2Provider::from_discovery` für jeden OIDC-Provider), und `oauth2::router::oauth2_router` mountet die Login- + Callback-Routen, erstellt den Benutzerdatensatz und setzt das Session-Cookie.
- **Sperre pro Account** — `rustango::account_lockout::Lockout` (cache-gestützt; `is_locked` / `record_failure` / `clear`, konfigurierbare `max_attempts` + `lockout_duration`).
- **CSP-Report-Endpunkt** — `security_headers::csp_report_router(path)` + `SecurityHeadersLayer::csp_report_uri(uri)`.
- **Verteiltes Rate Limiting** — `rate_limit_cache::CacheRateLimitLayer` (siehe [Anfragen per Rate Limiting drosseln](#rate-limiting-requests)).

Bis die nicht mitgelieferten Flows landen, klebe sie aus den obigen Primitiven zusammen (`signed_url::sign` für Passwort-Reset-Tokens, die E-Mail-Pipeline für die Zustellung usw.).

---

## Siehe auch

- [Middleware](middleware.md) — wie die Sicherheits-Layer angehängt werden, ihre Reihenfolge und der vollständige Katalog.
- [Authentifizierung](auth-passwords.md) — Passwörter, Sessions, JWTs, API-Keys, HMAC, Auth-Backends und Account-Flows, jeweils in der Tiefe.
- [API-Konventionen](api-conventions.md) — die Fehlertyp- und Rückgabetyp-Regeln hinter diesen APIs.
