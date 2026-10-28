# Sicherheitsleitfaden

Dieser Leitfaden behandelt jede Sicherheitsfunktion, die **Rustango** mitbringt, und wie man sie kombiniert. Wenn du schon einmal ein Web-Framework benutzt hast, werden dir die meisten davon vertraut vorkommen — die Namen unterscheiden sich, aber die Ideen sind dieselben. Jede der folgenden Funktionen benötigt in der Regel eine Zeile Setup. Wenn du bereit bist, in Produktion zu gehen, führe `manage check --deploy` für ein automatisiertes Audit aus.

[![Der gehärtete Middleware-Stack in einer Kette verdrahtet: Request-IDs, Zugriffsprotokollierung, Rate Limiting, CORS und Security-Header](../img/security.png)](../img/security.png)

## Inhaltsverzeichnis

- [Die Defense-in-Depth-Checkliste](#die-defense-in-depth-checkliste)
- [Security-Header setzen](#security-header-setzen)
- [Cross-Origin-Anfragen erlauben (CORS)](#cross-origin-anfragen-erlauben-cors)
- [Anfragen per Rate Limiting drosseln](#anfragen-per-rate-limiting-drosseln)
- [IPs erlauben oder blockieren](#ips-erlauben-oder-blockieren)
- [Schutz vor CSRF](#schutz-vor-csrf)
- [XSS verhindern](#xss-verhindern)
- [SQL-Injection verhindern](#sql-injection-verhindern)
- [Benutzer authentifizieren](#benutzer-authentifizieren)
- [Passwörter hashen und prüfen](#passwörter-hashen-und-prüfen)
- [JWTs ausstellen und erneuern](#jwts-ausstellen-und-erneuern)
- [Mit API-Keys authentifizieren](#mit-api-keys-authentifizieren)
- [Zwei-Faktor-Authentifizierung hinzufügen (TOTP)](#zwei-faktor-authentifizierung-hinzufügen-totp)
- [Signierte URLs versenden (Magic Links)](#signierte-urls-versenden-magic-links)
- [Eingehende Webhooks verifizieren](#eingehende-webhooks-verifizieren)
- [Secrets aus deinen Logs heraushalten](#secrets-aus-deinen-logs-heraushalten)
- [Anfragen über Services hinweg nachverfolgen](#anfragen-über-services-hinweg-nachverfolgen)
- [Secrets verwalten](#secrets-verwalten)
- [Vor dem Deploy auditieren](#vor-dem-deploy-auditieren)

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

Security-Header teilen dem Browser mit, wie er deine Benutzer schützen soll (Clickjacking blockieren, HTTPS erzwingen, Content-Type-Sniffing unterbinden). `SecurityHeadersLayer` setzt das Standardset mit einer Zeile — die gängigen Schutz-Header, ohne dass du sie einzeln benennen musst. (Ein „Layer" ist **Rustango**s Begriff für Middleware; du hängst sie an deinen Router an.)

> Vertiefung: [Middleware](middleware.md) behandelt, wie Layer funktionieren, ihre Reihenfolge, den vollständigen eingebauten Katalog und das Schreiben eigener Layer (locale-, zeitzonenbewusst, Header, CSRF).

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};

let app = router.security_headers(SecurityHeadersLayer::strict());
```

### Presets

| Preset | Wann verwenden |
|---|---|
| `strict()` | Produktion: HSTS preload + XFO=DENY + nosniff + Referrer-Policy=same-origin + COOP=same-origin + Permissions-Policy gesperrt |
| `relaxed()` | Einbettbar in iframes: SAMEORIGIN + 1 Jahr HSTS |
| `dev()` | Lokal: nur nosniff (kein HSTS, um localhost nicht für immer in HTTPS zu sperren) |
| `empty()` | Von Grund auf aufbauen |

`SecurityHeadersLayer::default()` ist `strict()`. `from_preset(name)` wählt ein Preset per Name (`"strict"`, `"relaxed"`, `"dev"`, `"none"` oder `"empty"`); ein unbekannter Name ergibt `strict()`, nie gar keine Header. `from_settings(&settings.security)` liest `[security] headers_preset`, `csp` und `hsts_max_age_secs` (`0` schaltet HSTS ab). Ein Header, den der Handler schon gesetzt hat, bleibt.

Zwei weitere Layer gehören hierher. `host_validation::AllowedHostsLayer::new(["example.com", ".example.com"])` beantwortet eine Anfrage, deren `Host` nicht in der Liste steht, mit `400` (`.example.com` deckt auch die Subdomains ab). `ssl_redirect::SslRedirectLayer::new()` beantwortet reines HTTP mit einem `301` auf HTTPS; hinter einem Proxy, der TLS terminiert, ergänze `.proxy_ssl_header("X-Forwarded-Proto", "https")`, sonst leitet er im Kreis um.

### Benutzerdefinierte CSP

```rust
let csp = CspBuilder::new()
    .default_src(&["'self'"])
    // The placeholder becomes a per-request nonce only under CspNonceLayer (below).
    .script_src(&["'self'", "'nonce-__RUSTANGO_NONCE__'", "https://cdn.example.com"])
    .style_src(&["'self'", "'nonce-__RUSTANGO_NONCE__'"])
    .img_src(&["'self'", "data:", "https:"])
    .font_src(&["'self'", "data:"])
    .connect_src(&["'self'", "wss://realtime.example.com"])
    .frame_ancestors(&["'none'"])                 // disallow embedding (clickjacking)
    .object_src(&["'none'"])
    .directive("base-uri", &["'self'"])
    .build();

let layer = SecurityHeadersLayer::strict().csp(csp);
```

`'nonce-__RUSTANGO_NONCE__'` (die Konstante `csp_nonce::CSP_NONCE_PLACEHOLDER`) ist nur ein Platzhalter. Mounte `csp_nonce::CspNonceLayer::default()` mit `.csp_nonce(...)` **nach** `.security_headers(...)`, damit er außen läuft: er erzeugt pro Anfrage eine zufällige Nonce, legt sie als `Extension<Nonce>` in die Anfrage und ersetzt den Platzhalter im CSP-Header. Ohne ihn bekommt der Browser den wörtlichen Platzhalter und blockiert jedes Inline-Script und jeden Inline-Style.

Die eingebauten [Fehlerseiten](html-views.md#fehlerseiten) und die Admin-Templates setzen dieselbe Nonce auf ihre Inline-Tags, sie bestehen diese Policy also.

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

`CorsLayer::new()` erlaubt keinen Origin, bis du welche hinzufügst. `CorsLayer::from_settings(&settings.security)` liest `[security] cors_allowed_origins`: eine leere Liste ergibt `None` (nichts mounten), `"*"` ergibt `permissive()`, und benannte Origins ergeben eine Allowlist mit den üblichen Methoden (inklusive QUERY) und den Request-Headern `Content-Type`, `Authorization`, `X-Date`, `Idempotency-Key`, `If-Match`, `If-None-Match`. Beide geben `Retry-After`, `Idempotent-Replayed`, `ETag`, `Location`, `Link` für Skripte frei. `cors_allowed_headers` und `cors_expose_headers` ergänzen diese Listen, und `cors_allow_credentials = true` sendet `Access-Control-Allow-Credentials` mit benannten Origins; mit `"*"` wird es mit einem geloggten Fehler abgelehnt, und ein `"*"`-Header-Eintrag entfällt.

**Sicherheitshinweis:** Mit Credentials MUSST du explizite Origins auflisten. `allow_credentials(true)` zusammen mit `allow_any_origin()` ergibt kein „credentialed Wildcard-CORS" — das gibt es nicht, und wer es verlangt, bekommt keine der beiden Hälften.

Rustango antwortet darauf mit `Access-Control-Allow-Origin: *` und **ohne** `Access-Control-Allow-Credentials`-Header, sodass der Browser die Anfrage mit Credentials blockiert. Ältere Versionen spiegelten stattdessen die anfragende Origin zurückgespiegelt — und genau das akzeptieren Browser zusammen mit Credentials. Diese Kombination war ein funktionierendes Loch, lesbar von jeder Seite, die der Nutzer besuchte, und diese Seite beschrieb es als etwas, das der Browser ablehnen würde.

Nutze `.allow_origins([...])` mit den Origins, die du tatsächlich bedienst. Dieser Pfad ist unverändert und sendet weiterhin `Access-Control-Allow-Credentials: true`.

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

Bei Erschöpfung: `429 Too Many Requests` mit `Retry-After`-Header. Jede erfolgreiche Antwort enthält `X-RateLimit-Limit` + `X-RateLimit-Remaining`. Der Layer ist ein Token-Bucket: `capacity` Anfragen, aufgefüllt über `refill_period`. Er hält höchstens 100.000 Buckets; ändere das mit `.max_buckets(n)`.

> **Hinter einem Reverse Proxy: benenne deine Proxys, sonst limitiert `per_ip` nichts Sinnvolles**. `RateLimitLayer::per_ip` schlüsselt auf den verbindenden Socket, was hinter einem Proxy die Adresse des *Proxys* ist — also teilen sich alle Clients einen Bucket. Ein einziger lauter Client limitiert dann alle anderen, und kein einzelner Angreifer wird je limitiert.
>
> Mounte `RealIpLayer` **und sag ihm, welchen Hops er glauben soll**:
>
> ```rust
> use rustango::real_ip::{HeaderStrategy, RealIpLayer, RealIpRouterExt};
>
> app.rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
>    .real_ip(
>        RealIpLayer::new(HeaderStrategy::XForwardedFor)
>            .trust_proxies(["10.0.0.0/8"])?,   // dein Ingress, nicht das Internet
>    );
> ```
>
> **Die Reihenfolge ist entscheidend und scheitert lautlos.** Layer werden von außen nach innen zuletzt angewandt, `real_ip` muss also *nach* `rate_limit` hinzugefügt werden, um *davor* zu laufen. Andersherum sieht der Limiter keine aufgelöste Adresse und schlüsselt still wieder auf den Socket — er warnt einmal, wenn ein Forwarding-Header ohne aufgelöste Adresse eintrifft.
>
> **`trust_proxies` ist keine optionale Dekoration.** `X-Forwarded-For` setzt, wer ihn sendet. Ohne deklarierte Proxy-Liste ist der Header nur eine Behauptung, also ignoriert ihn der Limiter vollständig und schlüsselt auf den Socket — absichtlich. Täte er es nicht, könnte jeder Client durch Variieren eines Headers pro Anfrage einen frischen Bucket erzeugen und aus „das Limit ist zu grob" würde „es gibt kein Limit". `RealIp` (die Behauptung) ist für Logging in Ordnung; nur `TrustedRealIp`, das erscheint, wenn der Peer zu `trust_proxies` passt, schlüsselt einen Limiter.
>
> **Eine Regel für jeden IP-Leser.** Rate Limits, ViewSet-Throttles, Login-Limits, Auth-Signale und das Access-Log nutzen dieselbe Adresse: `TrustedRealIp` (`X-Forwarded-For` von rechts nach links lesen, den ersten Hop nehmen, der nicht in `trust_proxies` steht), sonst den Socket. Keiner liest den linken Hop: den schreibt der Client.
>
> Benenne die Adressen, von denen dein Ingress tatsächlich verbindet. Einen weiteren Bereich zu vertrauen, reicht den Bypass an jeden darin weiter.

### Eingebaute Login-Limits

Jeder eingebaute Passwort-Login (Admin, Operator-Konsole, Tenant-Admin, JWT `/auth/login`) läuft vor dem Benutzer-Lookup durch `rustango::login_throttle`. Er prüft der Reihe nach eine Sperre pro Benutzername (5 Fehlversuche, 15 Min.), die unbekannte Benutzernamen genauso zählt wie echte, ein Limit pro IP (20 fehlgeschlagene Logins/Min.; IPv6 zählt pro /64) und ein globales Limit pro Login (je 600 fehlgeschlagene Logins/Min. für den Admin, die Operator-Konsole und jeden Tenant). Ein erfolgreicher Login zählt nicht gegen die Limits. Ein Benutzername, der sich in den letzten 30 Tagen von einer IP angemeldet hat, überspringt von dieser IP ein ausgeschöpftes globales Limit, sodass ein Spray von vielen IPs nicht jeden Benutzer aussperren kann; sein IP-Limit und seine Benutzernamen-Sperre gelten weiter. Sobald die Benutzerzeile gefunden ist, folgt die Sperre auch dem gespeicherten Benutzernamen, sodass Schreibweisen, die die Datenbank als gleich behandelt (MySQL setzt `alicé` mit `alice` gleich), eine Sperre teilen. HTTP Basic und API-Keys haben ihren eigenen Bereich und zählen nur Fehlversuche. Ein abgelehnter Login bekommt `429` mit `Retry-After`, und dieselbe Antwort, ob das Konto existiert oder nicht.

Hinter einem Reverse Proxy mountest du `RealIpLayer` mit deinen Proxys wie oben gezeigt; eine `Cli`-App ruft stattdessen `Cli::with_trusted_proxies(["127.0.0.1/32"])?` auf. Ohne das hat jeder Client die Adresse des Proxys und teilt sich einen IP-Bucket; eine Warnung wird einmal geloggt, wenn Forwarding-Header eintreffen.

Passwort-Hashing führt höchstens einen Job pro CPU aus, geteilt von jedem Login; HTTP Basic, API-Keys und Agent-Secrets dürfen höchstens die Hälfte der Slots belegen. Ein Login, der länger als `hash_wait_ms` (5 s) auf einen Slot wartet, bekommt `503` mit `Retry-After`, für bekannte und unbekannte Benutzer gleichermaßen.

Feinjustiert wird unter `[auth]`: `login_ip_limit`, `login_ip_window_secs`, `login_global_limit`, `login_global_window_secs`, `lockout_threshold`, `lockout_duration_secs`, `hash_wait_ms`. Ein im Code gesetzter Wert (`login_throttle::configure_shared`, `account_lockout::configure_shared`, `passwords::configure_hash_wait`) gewinnt über diese Schlüssel. Die IP- und globalen Buckets gelten pro Prozess, außer du installierst `login_throttle::configure_shared(LoginThrottle::with_cache(limits, cache))`, das sie in festen Fenstern in einem geteilten Cache zählt (bei einem Cache-Fehler fallen beide auf den Prozess zurück; ein Burst über eine Fenstergrenze kann das Doppelte eines Limits erreichen; Apps, die sich einen Cache teilen, teilen die Zähler, außer jede nutzt ihren eigenen `ScopedCache`); der erste Login loggt eine Warnung, solange sie pro Prozess gelten. Die Benutzernamen-Sperre nutzt `account_lockout::shared()`, das du über `account_lockout::configure_shared` mit einem geteilten Cache unterlegen kannst. Fehlversuche zählen in einem festen Fenster (standardmäßig 1 Stunde), das mit dem ersten Fehlversuch beginnt, ein Angreifer bekommt also `lockout_threshold - 1` Versuche pro Fenster ohne Sperre; die IP- und globalen Limits begrenzen den Rest.

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

Er zählt in festen Fenstern, also kann ein Burst über eine Fenstergrenze das Doppelte des Limits erreichen. Ein Cache-Fehler lässt die Anfrage durch (fail open) und loggt eine Warnung.

> **Nach einem geheimen Header zu keyen ist teilbar-sicher.** Bei `key_by(KeyBy::Header("authorization"))` oder `"x-api-key"` **hasht** der cache-gestützte Limiter den Header-Wert, bevor er ihn als Schlüssel verwendet — ein geteiltes Redis speichert also nie eine gültige Anmeldeinformation dort, wo jemand mit Schlüssel-Lesezugriff sie abgreifen könnte. `KeyBy::Ip` braucht `ConnectInfo` (oder ein vorgeschaltetes `RealIpLayer` mit `trust_proxies`); fehlt das, warnt der Limiter einmalig und fällt auf einen einzigen geteilten Bucket zurück.


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

Eine Anfrage ohne `ConnectInfo` wird von `allow_only` abgelehnt und von `block` durchgelassen; ändere das mit `.allow_no_ip(bool)`. Hinter einem Reverse Proxy ist der Peer der Proxy. Rufe `.behind_trusted_proxy()` auf, um stattdessen die Client-IP zu prüfen, die ein `RealIpLayer` mit `trust_proxies` aufgelöst hat. Der Filter muss innerhalb dieses `RealIpLayer` sitzen (füge `.real_ip(...)` nach `.ip_filter(...)` hinzu), sonst prüft er die Proxy-Adresse und eine Blocklist lässt jeden durch. `ip_filter` liest selbst nie einen Forwarding-Header.

---

## Schutz vor CSRF

> **CSRF und Token-APIs.** CSRF schützt **Cookie-authentifizierte** Anfragen. Eine
> reine Token- / Bearer- / [JWT](auth-jwt.md)-API ist kein CSRF-Ziel — wende
> den CSRF-Layer nicht darauf an (sonst gibst du legitimen Clients ein 403). Für eine SPA, die
> *tatsächlich* Cookies verwendet, lies das `rustango_csrf`-Cookie und gib es im `X-CSRF-Token`-
> Header zurück.

CSRF (Cross-Site Request Forgery) liegt vor, wenn eine andere Website den Browser eines eingeloggten Benutzers dazu bringt, eine Anfrage an deine App zu senden. Die Verteidigung ist ein geheimes Token in jedem Formular, das der Server bei jedem Schreibvorgang gegenprüft. Die CSRF-Middleware liegt in `rustango::forms::csrf` (hinter dem `csrf`-Feature, das durch das `admin`-Feature automatisch aktiviert wird):

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/contact", get(form).post(submit))
    .layer(csrf::layer());
```

`csrf::layer()` nimmt das `Secure`-Flag des Cookies aus `session::secure_cookies()`: der Boot-Policy (`[security] secure_cookies`, die der `manage`-Pfad standardmäßig auf `true` setzt), sonst `Secure` auf dem Prod-Tier. Die `template_views`-CBVs setzen ihr Cookie über `forms::csrf::ensure_token`, das derselben Policy folgt. `csrf::with_config(CsrfConfig)` nutzt stattdessen `CsrfConfig::secure` (Standard `true`); auf `http://localhost` rufe `CsrfConfig::default().allow_insecure_for_dev()` auf, sonst verwirft der Browser das Cookie und jedes POST bekommt `403`. `with_config` setzt auch die Cookie-/Header-Namen und `trusted_origins` — zusätzliche Origins neben dem eigenen Host der Anfrage (`.trust_origin("https://app.example.com")`, `*.example.com`-Wildcards erlaubt). Die Origin-Prüfung läuft auch bei leerer Liste: ein fremder `Origin` bekommt `403`, über TLS auch ein POST ohne `Origin`. In Templates gibt `{{ csrf_token }}` das rohe Token und `{{ csrf_input }}` ein fertiges verstecktes `<input>` — schreib es als `{{ csrf_input | safe }}`, denn Tera escapt `.html`-Templates automatisch: ohne den Filter rendert die Seite ein sichtbares literales `<input …>`, das Formular trägt kein `_csrf`-Feld und jedes POST endet in 403. Beide Variablen liegen nur für die `template_views`-CBVs im Context oder nachdem du `forms::csrf::stamp_into_context` selbst aufgerufen hast — ein handgeschriebener Handler hat keine davon. Es verwendet das Double-Submit-Cookie-Muster. GET, HEAD, OPTIONS und TRACE sind sicher, ebenso QUERY, solange `require_csrf_on_query` nicht gesetzt ist. Bei jeder anderen Methode prüft der Layer den `X-CSRF-Token`-Header (oder das `_csrf`-Formularfeld) gegen das `rustango_csrf`-Cookie; eine Nichtübereinstimmung gibt `403 Forbidden` zurück.

**Collector-Endpunkte ausnehmen.** `CsrfConfig::exempt_prefix("/path")` (wiederholbar) überspringt die CSRF-Durchsetzung für unsichere Methoden bei Anfragen, deren Pfad mit dem angegebenen Präfix beginnt. Das ist für Append-only-, zustandslose Endpunkte gedacht, die per `navigator.sendBeacon` angesprochen werden — z. B. ein Analytics-Collector — die keinen `X-CSRF-Token`-Header setzen können und, wenn die Seite aus einem CDN-Cache ausgeliefert wird, der `Set-Cookie` entfernt, möglicherweise gar kein CSRF-Cookie mitführen. Halte Präfixe eng und nimm niemals etwas aus, das Auth-Zustand liest oder schreibt.

**Der Auto-Admin.** Sobald du `.with_session_auth(...)` aufrufst, wird CSRF automatisch auf jede Admin-Mutation gelegt — anlegen, ändern, löschen, Bulk-Aktionen, Audit-Cleanup — und jedes Admin-Formular rendert seinen Token selbst. Es gibt nichts zu verdrahten und keine Möglichkeit, sich davon abzumelden.

Die Bedingung ist Absicht, kein Zufall: CSRF schützt Credentials, die der Browser von sich aus mitschickt, also ist es das Session-Cookie, das es überhaupt sinnvoll macht. Ein Admin ohne `with_session_auth` hat kein von rustango verwaltetes Credential, mit dem sich fälschen ließe, und seine Mutationen sind für jeden direkt erreichbar, der die Route erreicht — das ist eine Authentifizierungslücke, keine CSRF-Lücke, und CSRF würde sie nicht verkleinern. Das gilt nur, solange davor kein vom Browser mitgeschicktes Credential steht. HTTP Basic (`protect_with_basic_auth`) oder eine eigene Cookie-Auth ist eines, also reiten Cross-Site-POSTs darauf — und `csrf::layer()` selbst einzuhängen hilft nicht, weil der Admin Formular-Tokens nur unter `with_session_auth` rendert, sodass jedes POST mit 403 endet. Nimm `with_session_auth` für einen Admin im Browser.

Ältere Fassungen dieses Absatzes behaupteten, der Schutz sei bedingungslos — geschützt war allein `POST /login`. Jede andere Admin-Mutation akzeptierte ein Cross-Site-POST auf der Session des Administrators, Audit-Cleanup eingeschlossen, sodass dieselbe Anfrageklasse ihre eigene Spur löschen konnte.

---

## XSS verhindern

XSS (Cross-Site Scripting) tritt auf, wenn Benutzereingaben als HTML gerendert werden und als Code im Browser einer anderen Person laufen. Die Lösung ist, jede Benutzereingabe zu escapen, bevor sie die Seite erreicht. **Rustango** löst das auf zwei Wegen:

**1. Tera-Template-Auto-Escape** — Tera ist **Rustango**s Template-Engine. Jedes `{{ var }}` wird in jedem Template, das das Framework baut, automatisch HTML-escapt, egal welche Endung. Mit `template_extensions::html_tera()` / `html_tera_from_glob(glob)` bekommt deine eigene Engine dasselbe; ein nacktes `Tera::new` escapt nur `.html`, `.htm` und `.xml`. Nur die Klartext-Teile von E-Mails (`EmailRenderer`-Betreff und `.txt`-Body) werden roh gerendert. Verwende `{{ var | safe }}`, um dich abzumelden — selten und gefährlich, also tue das nur für HTML, dem du vollständig vertraust.

**2. Manueller Escape-Helper** — für den Fall, dass du HTML in Rust-Code statt in einem Template baust:

```rust
use rustango::text::html_escape;

let safe = html_escape(user_input);
// "<script>" → "&lt;script&gt;"
// "a & b" → "a &amp; b"
```

Ersetzt `&`, `<`, `>`, `"`, `'`. Geeignet für HTML-Elementinhalte + doppelt-quotierte Attribute.

Für CSP-basierte XSS-Verteidigung siehe [Security-Header setzen](#security-header-setzen).

---

## SQL-Injection verhindern

SQL-Injection tritt auf, wenn Benutzereingaben direkt in einen SQL-String eingefügt werden und die Query verändern. **Rustango**s ORM verhindert das durch Design: es verwendet überall sqlx-Parameter-Binding — jeder Wert wird als gebundener Platzhalter gesendet (`$1, $2, ...` auf PostgreSQL, `?` auf MySQL und SQLite), niemals in den Query-Text eingeklebt. (sqlx ist die zugrunde liegende Datenbankbibliothek.) **Du kannst über die ORM-API nicht versehentlich Benutzereingaben in SQL konkatenieren.**

Es gibt zwei Stellen, bei denen du weiterhin vorsichtig sein solltest:

**1. Tabellen- und Spaltennamen in `bulk_actions`:**

Platzhalter schützen Werte, können aber keinen Bezeichner (einen Tabellen- oder Spaltennamen) tragen, also benötigen diese ihren eigenen Schutz. Bulk Actions schreiben über das ORM, also erreicht kein roher Name das SQL. Eine eingebaute Aktion, die eine Spalte setzt, sucht sie im Modell-Schema; ein Name, den das Modell nicht hat, scheitert mit `BulkActionError::InvalidIdent`. Die gewählten Schlüssel sind ein `PkSet`, typisiert nach dem Primärschlüssel des Modells: ein Schlüssel vom falschen Typ scheitert mit `InvalidPk`, und ein Set hält höchstens 10.000 Schlüssel. Dieser Schutz läuft für dich — du rufst ihn nicht selbst auf.

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

Authentifizierung ist die Art, wie du bestätigst, wer eine Anfrage stellt. **Rustango** bringt drei fertige Backends mit (Basic Auth, API-Keys und JWTs) und lässt dich eigene schreiben, indem du ein einziges Trait implementierst. Du hängst sie an Routen an, und Anfragen ohne ein erkanntes Credential erhalten ein `401`.

> **Admin-SSO.** Um Betreibern zu erlauben, sich mit einem externen IdP (Google, Microsoft/Azure AD, GitHub oder einem beliebigen OpenID-Connect-Provider) statt mit einem Passwort im Admin anzumelden, aktiviere das `admin-sso`-Feature — siehe den [SSO-Leitfaden](sso.md). Provider werden **im Admin-UI als Zeilen verwaltet** (mehrere pro Surface; pro Tenant oder ein gemeinsames Set über Tenants hinweg), wobei das Client-Secret **verschlüsselt gespeichert** wird. Es meldet das Konto an, das mit dem IdP-Subject verknüpft ist (Verknüpfen per E-Mail ist pro Provider optional und gilt nie für Superuser oder Staff; kein Auto-Provisioning) und verwendet die bestehende Session wieder.

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

// `require_auth` / `require_perm` are `Router::layer` calls: each wraps the
// routes registered BEFORE it. Chaining them on one router would put /me
// behind post.add. Gate the permission on an inner sub-router instead.
let posts = Router::new()
    .route("/posts/new", post(create_post))
    .require_perm("post.add");     // inner: needs the codename

let app = Router::new()
    .route("/me", get(profile))
    .merge(posts)
    .require_auth(backends);       // outer: resolves the user first
```

Die Middleware probiert jedes Backend der Reihe nach. Das erste, das erfolgreich ist, gewinnt; das erste, das einen harten Fehler zurückgibt, stoppt die Kette. `require_perm` antwortet `401` ohne Benutzer, `403` ohne den Codename und `503`, wenn das Lesen der Berechtigung scheitert. `optional_auth(backends)` löst den Benutzer genauso auf, lässt anonyme Anfragen aber durch. Alle drei brauchen den Tenancy-Layer außen herum; ohne `TenantContext` scheitern sie sicher mit `500`.

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

`hash` und `verify` blockieren den Thread für den ganzen Hash. In async-Code nutze `hash_async` / `verify_async`, die auf dem begrenzten Hashing-Pool laufen und `PasswordError::Busy` zurückgeben, wenn nicht rechtzeitig ein Slot frei wird. Nach einem erfolgreichen Login sagt dir `needs_rehash(stored)`, dass der Hash unter den heutigen Kosten liegt; `rehash_async` liefert den neuen.

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

Das sind auch die Standardwerte. `JwtLifecycle::new` bricht mit einem Panic ab, wenn `secret` kürzer als 32 Bytes ist, denn mit einem kurzen Schlüssel kann jeder Tokens prägen. `issue_pair(user_id)` stellt ein Paar ohne eigene Claims aus, `issue_access_with` ein einzelnes Access-Token, und `verify_refresh` prüft ein Refresh-Token.

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
let claims = jwt.verify_access(&access_token).await
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
let tenant: String = claims.get_custom("tenant").unwrap_or_default();
```

### Ein Token erneuern (behält deine benutzerdefinierten Claims)

```rust
let new_pair = jwt.refresh(&refresh_token).await
    .ok_or(StatusCode::UNAUTHORIZED)?;
// new_pair has the same roles + scope + tenant
```

Die JTI des alten Refresh-Tokens (seine eindeutige ID) wird zu einer Blacklist hinzugefügt, sodass es nicht wiederverwendet werden kann — das blockiert Replay-Angriffe, bei denen ein gestohlenes altes Token erneut gesendet wird. Ein Refresh-Token ist einmalig: von zwei Refreshes desselben Tokens, auch gleichzeitig, bekommt der zweite `None`.

### Erneuern mit neu geprüften Berechtigungen

```rust
// returns Result<Option<JwtTokenPair>, JwtIssueError>:
// Err on a reserved-claim collision, Ok(None) on an invalid/expired refresh.
let new_pair = jwt.refresh_with(&refresh_token, json!({
    "roles": ["viewer"],     // role was demoted since login
    "tenant": "acme",
}).as_object().unwrap().clone()).await?
    .ok_or(StatusCode::UNAUTHORIZED)?;
```

### Ein Token widerrufen

```rust
jwt.revoke(&access_token).await;     // adds JTI to blacklist; false if the token isn't ours
jwt.revoke(&refresh_token).await;
```

Die standardmäßige In-Memory-Blacklist (`InMemoryJtiStore`) räumt abgelaufene Einträge von selbst aus, aber sie lebt in einem Prozess und vergisst bei jedem Neustart jeden Widerruf. Für Multi-Prozess-Deployments installiere einen gemeinsamen, dauerhaften Store über `JwtLifecycle::new(secret).with_jti_store(store)` — `store` ist ein beliebiger `Arc<dyn JtiStore>`. Nur `InMemoryJtiStore` wird mitgeliefert, den Redis- oder DB-gestützten schreibst du also selbst. Ohne einen gemeinsamen Store kann ein Token, das du auf einer Replica widerrufst, auf einer anderen weiterhin abgespielt werden, bis es abläuft. Seit v0.52 sind all diese Aufrufe `async` — `verify_*`, `refresh*` und `revoke` warten auf den Store, sodass ein dauerhafter Store ein einzelner bedingter Schreibvorgang sein kann statt eines letztlich konsistenten Caches mit Hintergrund-Flusher.

> **Vertiefung:** [JWT-Auth-API](auth-jwt-api.md) — der eingebaute `/api/auth/login|refresh|logout|me`-Router, gleitendes Refresh und der JTI-Widerrufs-Store. Für ein einzelnes selbstverwaltetes Token siehe [JWT (standalone)](auth-jwt.md). Um Browser-/API-Routen per Login zu gaten, siehe [Access-Decorators](auth-decorators.md).

---

## Mit API-Keys authentifizieren

API-Keys erlauben Skripten und Services die Authentifizierung ohne Benutzername, Passwort oder Session — praktisch für Machine-to-Machine-Zugriff. Du generierst einen Key, zeigst ihn dem Benutzer einmal und speicherst nur seinen Präfix und Hash.

```rust
use rustango::api_keys::{generate_key_async, verify_key_async, split_token};

// Issuance
let (full_token, prefix, hash) = generate_key_async().await?;
// Format: {8-char hex prefix}.{32-char hex secret}
// Show full_token to the user once. Store prefix + hash in your DB.

// Verification on each request
let (prefix, secret) = split_token(&inbound_header)
    .ok_or(StatusCode::UNAUTHORIZED)?;
let row = lookup_by_prefix(prefix).await?;
if !verify_key_async(secret, &row.hash).await? {
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

`verify` akzeptiert denselben Code erneut, bis er abläuft. Für Einmal-Codes rufen Sie stattdessen `matched_step` auf, speichern den zurückgegebenen Schritt und akzeptieren einen Code nur, wenn sein Schritt später ist als der gespeicherte. Der eingebaute Admin-Login macht das so.

Mit dem Feature `totp` hat auch der Bare-Admin eine Zwei-Faktor-Anmeldung. Jeder Benutzer registriert sich unter `<admin>/account/totp`, und ein bestätigtes Gerät wird bei jedem Login abgefragt, SSO eingeschlossen. Zum Abschalten ist ein aktueller Code nötig; bei verlorenem Gerät `manage reset-admin-totp <username>` ausführen (`reset-operator-totp` für einen Operator). Registrieren, Abschalten und falsche Codes schreiben Audit-Zeilen.

Die Operator-Konsole hat denselben zweiten Faktor, freiwillig pro Operator oder Pflicht für alle; siehe [Zwei-Faktor-Anmeldung](operator-console.md#zwei-faktor-anmeldung).

**Recovery-Codes** (einmalige Backup-Codes für den Fall, dass ein Benutzer sein Telefon verliert) werden noch nicht mitgeliefert. Das gängige Muster ist, 8–10 gehashte Codes pro Benutzer zu speichern und einen bei jeder Verwendung zu verbrauchen.

---

## Signierte URLs versenden (Magic Links)

Eine signierte URL trägt eine manipulationssichere Signatur, sodass du ihr ohne Session vertrauen kannst — perfekt für Magic-Link-Logins, Passwort-Resets und zeitlich begrenzte Download-Links. Du `sign`st eine URL (optional mit einem Ablaufdatum) und `verify`st sie, wenn der Benutzer klickt.

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

Die Signatur ist HMAC-SHA256 über Schema, Host, Pfad und die Query, vorher sortiert, sodass die Reihenfolge der Parameter keine Rolle spielt. Jede Änderung daran, `expires` eingeschlossen, macht sie ungültig.

**Diese Links sind nicht einmalig.** `verify` prüft nur Signatur und Ablauf, ein durchgesickerter Link (ein Referer-Header, der Browserverlauf, eine weitergeleitete E-Mail) funktioniert also bis zum Ablauf. Für einen Link, der jemanden anmeldet oder Daten ändert, nutze die `verify_single_use`-Varianten in `auth_flows` oder lösche den Datensatz nach dem ersten Klick.

> Vertiefung: [Account-Flows](auth-flows.md) baut Passwort-Reset, E-Mail-Verifizierung und Magic-Link-Login auf diesem Signed-URL-Primitive auf.

---

## Eingehende Webhooks verifizieren

Ein Webhook ist ein HTTP-Callback, den ein anderer Service dir sendet (eine Zahlung war erfolgreich, ein Push ist passiert). Prüfe immer seine Signatur, damit du weißt, dass er wirklich von diesem Service kam und der Body nicht verändert wurde. `verify_signature` handhabt die gängigen Formate.

Stripe und Slack signieren nicht nur den Body: sie signieren einen Zeitstempel zusammen mit ihm, und dieser Zeitstempel verhindert Replays. Baue diesen String selbst und prüfe den Zeitstempel, wie unten. Stripe signiert `"{t}.{body}"` mit deinem ganzen `whsec_…`-Secret als Schlüssel; Slack signiert `"v0:{ts}:{body}"` und sendet `v0=<hex>`.

```rust
use axum::{body::Bytes, http::HeaderMap, http::StatusCode};
use rustango::webhook::{verify_signature, SignatureFormat};

/// Stripe sends `Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>]`
/// and signs `"{t}.{body}"`, not the body alone.
fn verify_stripe(secret: &[u8], header: &str, body: &[u8], now: i64, tolerance: i64) -> bool {
    let mut ts = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", t)) => ts = Some(t),
            Some(("v1", sig)) => sigs.push(sig),
            _ => {}
        }
    }
    let Some(ts) = ts else { return false };
    let Ok(t) = ts.parse::<i64>() else {
        return false;
    };
    if (now - t).abs() > tolerance {
        return false; // stale or future-dated: a replay
    }
    let mut signed = format!("{ts}.").into_bytes();
    signed.extend_from_slice(body);
    sigs.iter()
        .any(|sig| verify_signature(SignatureFormat::HexSha256, secret, &signed, sig))
}

async fn handle_stripe_webhook(headers: HeaderMap, body: Bytes) -> StatusCode {
    let Ok(secret) = std::env::var("STRIPE_WEBHOOK_SECRET") else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    let header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if !verify_stripe(secret.as_bytes(), header, &body, now, 300) {
        return StatusCode::UNAUTHORIZED;
    }
    // ... process the verified payload
    StatusCode::OK
}
```

| Format | Signiert | Verwendet von |
|---|---|---|
| `HexSha256WithPrefix` | Body, `sha256=<hex>` | GitHub (`X-Hub-Signature-256`) |
| `HexSha256` | die übergebenen Bytes, rohes Hex | Stripe `v1=` / Slack `v0=` über die Strings oben |
| `Base64Sha256` | Body, Standard-Base64 | Shopify (`X-Shopify-Hmac-Sha256`) |

### Webhooks aus `webhook_delivery`

Rustangos eigene Zustellungen tragen `X-Webhook-Signature-V2`, signiert über Id, Event, Zeitstempel und Body. Prüfe sie mit `verify_delivery` und verwirf dann eine `X-Webhook-Id`, die du innerhalb von `max_age` schon gesehen hast. Wähle ein `max_age`, das länger ist, als Jobs in der Warteschlange stehen können.

```rust
use axum::{body::Bytes, http::HeaderMap, http::StatusCode};
use rustango::webhook::{verify_delivery, DeliveryHeaders, SignatureFormat};

fn check(secret: &[u8], headers: &HeaderMap, body: &Bytes) -> StatusCode {
    let h = |name| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let signed = DeliveryHeaders::new(
        h("x-webhook-id"),
        h("x-webhook-event"),
        h("x-webhook-timestamp"),
        h("x-webhook-signature-v2"),
    );
    let max_age = std::time::Duration::from_secs(600);
    if !verify_delivery(SignatureFormat::HexSha256WithPrefix, secret, &signed, body, max_age) {
        return StatusCode::UNAUTHORIZED;
    }
    // ... drop a repeated id, then process
    StatusCode::OK
}
```

Die ältere `X-Webhook-Signature` deckt nur den Body ab und wird weiter gesendet, solange Empfänger auf v2 umstellen.

Eine Zustellung, die mit 408, 429 oder 5xx beantwortet wird oder beim Transport scheitert, wird wiederholt: insgesamt 8 Läufe, mit 1s Wartezeit, die sich jedes Mal verdoppelt. `WebhookSubscription::max_attempts(n)` und `retry_backoff(base)` ändern beides für ein Abonnement. Ein `Retry-After` auf einer 429 oder 503, in Sekunden oder als HTTP-Datum, lässt den nächsten Versuch mindestens so lange warten, höchstens eine Stunde. Andere 4xx-Antworten gehen sofort in den Dead-Letter; `retry_status_codes([...])` fügt Codes zum Wiederholen hinzu. Wiederholungen nutzen den ersten Zeitstempel, also muss das `max_age` des Empfängers das ganze Wiederholungsfenster abdecken.

Der Signaturvergleich ist konstant-zeitlich, das heißt, er dauert immer gleich lang, egal ob die Vermutung richtig oder falsch ist. Das stoppt Timing-Angriffe, bei denen ein Angreifer winzige Antwortzeit-Unterschiede misst, um das Secret Zeichen für Zeichen zu erraten.

Ausgehende Aufrufe an URLs, die die Konfiguration setzen kann (Webhook-Zustellung, Slack `webhook_callback`, OAuth2/OIDC-Discovery, Token und Userinfo), lehnen Loopback-, private, Link-Local- und Metadaten-Adressen ab und folgen nie Redirects. Um einen IdP oder einen Slack-Hook im eigenen Netz zu erreichen, trage ihn in `RUSTANGO_OUTBOUND_ALLOW` ein (kommagetrennte Hosts und CIDRs, z. B. `10.0.5.0/24,idp.internal`); alles andere bleibt abgelehnt. Ein Host-Eintrag vertraut allem, wozu dieser Name auflöst, trage also nur Namen ein, die du kontrollierst. Die Liste gilt nie für Webhook-Zustellung: Ein Abonnement schaltet das mit `allow_private_targets(true)` frei. Sie gilt auch nicht für die eigenen SSO-Provider-Zeilen eines Tenants, die ein Tenant-Admin setzt: Diese lesen `RUSTANGO_TENANT_OUTBOUND_ALLOW` (standardmäßig leer, also nur öffentliche Adressen).

Diese Aufrufe ignorieren `HTTPS_PROXY`/`NO_PROXY`. Um sie über einen Egress-Proxy zu schicken, setze `RUSTANGO_OUTBOUND_PROXY` (z. B. `http://proxy.internal:3128`); der Proxy selbst darf privat sein. Die App löst jedes Ziel trotzdem zuerst auf und prüft es, braucht also lokales DNS dafür; ein fehlgeschlagener Lookup lehnt den Aufruf ab. Der Proxy löst den Namen erneut auf, muss also ebenfalls private und Metadaten-Ziele ablehnen: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8, 169.254.0.0/16 (inkl. 169.254.169.254, 169.254.170.2, 169.254.170.23), 100.64.0.0/10 (inkl. 100.100.100.200), ::1, fc00::/7 (inkl. fd00:ec2::254, fd00:ec2::23) und fe80::/10. Ohne Proxy werden Adressen beim Verbindungsaufbau erneut geprüft.

---

## Secrets aus deinen Logs heraushalten

Geloggte URLs können Passwörter und Tokens leaken, die in Query-Strings auftauchen. `AccessLogLayer` maskiert automatisch bekannte Credential-Parameter, bevor die Log-Zeile geschrieben wird (PII = personenbezogene Daten).

```rust
let app = router.access_log(AccessLogLayer::default());
```

Standardmäßig wird ein Query-Parameter durch `[redacted]` ersetzt, wenn sein Name ein geheimes Wort enthält, wortweise verglichen, sodass `api_key`, `reset_token` und `X-Api-Key` alle zählen. Zu den Wörtern gehören `password`, `pass`, `pwd`, `token`, `jwt`, `secret`, `key`, `signature`, `auth`, `credential`, `cookie`, `session` und `otp`, dazu die OAuth-Namen `code`, `state` und `assertion`.

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

`token`, `signature` und `code` bleiben auch nach `.redact(...)` maskiert, weil das Framework Zugangsdaten hineinlegt.

Hinweis: Dies redaktiert nur **Query-Strings**. Um Request-Bodies oder Header zu bereinigen, filtere an der Quelle mit `tracing::Subscriber`.

---

## Anfragen über Services hinweg nachverfolgen

Eine Request-ID versieht jede Log-Zeile für eine Anfrage mit derselben ID, sodass du dieser Anfrage über Handler hinweg — und über Services hinweg — folgen kannst. `RequestIdLayer` fügt jeder Anfrage eine hinzu.

```rust
let app = router.request_id(RequestIdLayer::default());
```

Es verwendet eine eingehende `X-Request-Id` nur von einem Proxy wieder, der in `RealIpLayer::trust_proxies` (oder `Cli::with_trusted_proxies`) steht, sodass verkettete Services eine ID teilen, und generiert sonst eine frische 22-Zeichen-Base64-ID. Eingehende IDs dürfen höchstens 128 sichtbare ASCII-Zeichen ohne Anführungszeichen oder Backslash haben, was Header- und Log-Injection blockiert.

In Handlern:

```rust
async fn handler(id: rustango::request_id::RequestId) -> String {
    tracing::info!(req_id = %id.0, "handling /me");
    format!("req {}", id.0)
}
```

Um die ID auf jede Log-Zeile der Anfrage zu bringen, mounte auch `tracing_layer::TracingLayer::new()`: sein Request-Span hat ein Feld `request_id`, das der Layer füllt. Ohne ihn landet die ID nur im Response-Header.

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

Um Secrets aus Vault, AWS Secrets Manager oder GCP Secret Manager zu ziehen, implementiere das Trait selbst — es ist nur eine async Methode, `get`. In Tests nutze `InMemorySecrets::new().with(&[("DB_PASSWORD", "x")])`.

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
- ✅ `RUSTANGO_SESSION_SECRET` gesetzt und **dekodiert** ≥ 32 Bytes (der HMAC-Schlüssel für Cookie- + JWT-Signierung — `SECRET_KEY` wird vom Framework **nicht** gelesen), ohne verbliebenen Scaffolder-Platzhalter. Es ist base64: `openssl rand -base64 32` liefert 44 Zeichen. Ein Secret mit 32 *Zeichen* hat nur 24 Bytes und wird abgelehnt — von `check --deploy` und der Laufzeit gleichermaßen; genau darum geht es, denn die Prüfung maß früher die kodierte Zeichenkette und ließ einen Wert durch, den die App dann ablehnte
- ✅ Eine Datenbank ist konfiguriert: `DATABASE_URL` oder `DB_USER` + `DB_NAME` (und warnt, wenn sie auf localhost zeigt)
- ⚠️ `RUSTANGO_APEX_DOMAIN` / `RUSTANGO_BIND` Plausibilitätswarnungen für Tenancy + Nicht-Loopback-Binding
- ⚠️ Einstellungen mit Dev-Werten im Prod-Tier: `[security] headers_preset`, `secure_cookies = false`, `hsts_max_age_secs = 0`, leeres oder `"*"` `allowed_hosts`, `"*"` in `cors_allowed_origins`, leeres `csrf_trusted_origins`, kein `secure_ssl_redirect`; `[server] request_timeout_secs` und `max_body_bytes` nicht gesetzt; ein niedriges `[auth] argon2_memory_kib`; ein `[auth.jwt] access_ttl_secs` über einer Stunde; `[mail] backend` nicht gesetzt oder `console`
- ⚠️ Ein Admin-Router ohne Authentifizierung, ein Login-Sperr-Store pro Prozess, Kollation und Zeitzone der Datenbank, Modelle, deren `required_db_vendor` oder `required_db_features` der Datenbank fehlen, und SSO-Anbieter, die jeden Benutzer abweisen würden

`RUSTANGO_SECRET_KEY` ist ein anderer Schlüssel: er verschlüsselt gespeicherte Secrets wie SSO-Client-Secrets. Ist die Tenant-Provisionierung eingehängt, warnt `check --deploy`, wenn er fehlt oder kürzer als 32 Bytes ist.

Gibt bei jedem Befund auf **Error-Level** einen Exit-Code ungleich null zurück (gut für CI-Gates). Warnungen lösen kein Fehlschlagen aus, erscheinen aber in der Ausgabe.

Für Prüfungen jenseits des Mitgelieferten erweitere mit benutzerdefiniertem Code in deinem `manage`-Binary, bevor du an `manage::run` weiterleitest.

---

## Was NOCH nicht mitgeliefert wird

Ein paar End-to-End-Flows müssen noch aus den untenstehenden Primitiven zusammengeklebt werden:

- **Passwort-Reset + E-Mail-Verifizierung** — die Token-Issue-/Verify-Helper existieren (`auth_flows::confirm_password_reset_pool`, plus E-Mail-Verifizierungs-Round-Trips) und die E-Mail-Pipeline wird separat mitgeliefert, aber es gibt keinen einzelnen vorgefertigten View- + E-Mail- + Validate-Zyklus, der für dich verdrahtet ist.
- **PII-Redaktion von Request-Body / Headern** in `access_log` — feldbasierte Log-Redaktion existiert (siehe [Secrets aus deinen Logs heraushalten](#secrets-aus-deinen-logs-heraushalten)), aber keine automatische Bereinigung des Request-Bodys oder der Header.

Bereits mitgeliefert (greife nicht zu einem Workaround):

- **OAuth2 / OIDC Social Login** — `oauth2::providers` bringt Google-, GitHub-, Microsoft-, GitLab- und Discord-Helper mit (plus `OAuth2Provider::from_discovery` für jeden OIDC-Provider), und `oauth2::router::oauth2_router` mountet die Login- + Callback-Routen; Ihr `OnAuthSuccess`-Hook findet den Benutzer über `AuthSuccess::identity_key()` und setzt das Session-Cookie.
- **Sperre pro Account** — `rustango::account_lockout::Lockout` (cache-gestützt; `is_locked` / `record_failure` / `clear`, konfigurierbare `max_attempts` + `lockout_duration`).
- **CSP-Report-Endpunkt** — `security_headers::csp_report_router(path)` + `SecurityHeadersLayer::csp_report_uri(uri)`.
- **Verteiltes Rate Limiting** — `rate_limit_cache::CacheRateLimitLayer` (siehe [Anfragen per Rate Limiting drosseln](#anfragen-per-rate-limiting-drosseln)).

Bis die nicht mitgelieferten Flows landen, klebe sie aus den obigen Primitiven zusammen (`signed_url::sign` für Passwort-Reset-Tokens, die E-Mail-Pipeline für die Zustellung usw.).

---

## Siehe auch

- [Middleware](middleware.md) — wie die Sicherheits-Layer angehängt werden, ihre Reihenfolge und der vollständige Katalog.
- [Authentifizierung](auth-passwords.md) — Passwörter, Sessions, JWTs, API-Keys, HMAC, Auth-Backends und Account-Flows, jeweils in der Tiefe.
- [API-Konventionen](api-conventions.md) — die Fehlertyp- und Rückgabetyp-Regeln hinter diesen APIs.
