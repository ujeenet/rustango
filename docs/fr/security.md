# Guide de sécurité

Ce guide couvre chaque fonctionnalité de sécurité fournie par **Rustango** et la manière de les combiner. Si vous venez de Django, Laravel ou Rails, la plupart vous sembleront familières — les noms diffèrent, mais les idées sont les mêmes. Chaque fonctionnalité ci-dessous se met généralement en place en une seule ligne. Quand vous êtes prêt à déployer, lancez `manage check --deploy` pour un audit automatisé.

[![La pile de middleware renforcée câblée dans une seule chaîne : identifiants de requête, journalisation des accès, limitation de débit, CORS et en-têtes de sécurité](img/security.png)](img/security.png)

## Table des matières

- [La checklist de défense en profondeur](#the-defense-in-depth-checklist)
- [Définir les en-têtes de sécurité](#setting-security-headers)
- [Autoriser les requêtes cross-origin (CORS)](#allowing-cross-origin-requests-cors)
- [Limiter le débit des requêtes](#rate-limiting-requests)
- [Autoriser ou bloquer des IP](#allowing-or-blocking-ips)
- [Se protéger contre le CSRF](#protecting-against-csrf)
- [Prévenir le XSS](#preventing-xss)
- [Prévenir l'injection SQL](#preventing-sql-injection)
- [Authentifier les utilisateurs](#authenticating-users)
- [Hacher et vérifier les mots de passe](#hashing-and-checking-passwords)
- [Émettre et rafraîchir des JWT](#issuing-and-refreshing-jwts)
- [S'authentifier avec des clés d'API](#authenticating-with-api-keys)
- [Ajouter l'authentification à deux facteurs (TOTP)](#adding-two-factor-auth-totp)
- [Envoyer des URL signées (liens magiques)](#sending-signed-urls-magic-links)
- [Vérifier les webhooks entrants](#verifying-incoming-webhooks)
- [Garder les secrets hors de vos journaux](#keeping-secrets-out-of-your-logs)
- [Tracer les requêtes entre services](#tracing-requests-across-services)
- [Gérer les secrets](#managing-secrets)
- [Auditer avant de déployer](#auditing-before-you-deploy)

---

## La checklist de défense en profondeur

Une bonne sécurité vient de nombreuses petites couches, pas d'un seul grand mur. Une application **Rustango** en production devrait empiler les couches ci-dessous — la plupart tiennent en une ligne chacune. Le reste de ce guide explique chacune d'elles.

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

## Définir les en-têtes de sécurité

Les en-têtes de sécurité indiquent au navigateur comment protéger vos utilisateurs (bloquer le clickjacking, forcer HTTPS, empêcher le sniffing de type de contenu). `SecurityHeadersLayer` définit l'ensemble standard en une ligne — les mêmes en-têtes que Django fournit par défaut. (Une « couche » est le terme de **Rustango** pour désigner un middleware ; vous l'attachez à votre routeur.)

> À approfondir : [Middleware](middleware.md) couvre le fonctionnement des couches, leur ordre, le catalogue intégré complet et la rédaction de vos propres couches (sensibles à la locale, au fuseau horaire, en-têtes, CSRF).

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};

let app = router.security_headers(SecurityHeadersLayer::strict());
```

### Presets

| Preset | Quand l'utiliser |
|---|---|
| `strict()` | Production : HSTS preload + XFO=DENY + nosniff + Referrer-Policy=no-referrer + COOP=same-origin + Permissions-Policy verrouillée |
| `relaxed()` | Intégrable dans des iframes : SAMEORIGIN + HSTS 1 an |
| `dev()` | Local : nosniff uniquement (pas de HSTS pour éviter de verrouiller localhost en HTTPS pour toujours) |
| `empty()` | Construire à partir de zéro |

### CSP personnalisé

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

### Déploiement progressif du CSP

Utilisez `csp_report_only` pour surveiller sans casser la production :

```rust
let layer = SecurityHeadersLayer::strict()
    .csp(CspBuilder::strict_starter().build())
    .csp_report_only(true);                       // sends Content-Security-Policy-Report-Only
```

Après avoir vérifié l'absence d'erreurs dans la console → basculez `csp_report_only(false)` pour appliquer.

---

## Autoriser les requêtes cross-origin (CORS)

Le CORS contrôle quels autres sites web sont autorisés à appeler votre API depuis un navigateur. Par défaut, les navigateurs bloquent ces appels ; vous réautorisez spécifiquement certaines origines. Listez vos vrais domaines front-end en production et ne l'ouvrez jamais à tout le monde.

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

**Note de sécurité :** ne combinez jamais `allow_credentials(true)` avec `allow_any_origin()` — le navigateur rejettera la réponse. Avec des identifiants, vous DEVEZ lister des origines explicites.

---

## Limiter le débit des requêtes

La limitation de débit plafonne le nombre de requêtes qu'un client peut effectuer dans une fenêtre de temps. Utilisez-la pour ralentir les connexions par force brute, le scraping et les abus. Vous pouvez limiter par IP, par clé d'API ou globalement pour un endpoint coûteux.

```rust
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};

// Per-IP
router.rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)));

// Per-API-key (header)
router.rate_limit(RateLimitLayer::per_header("authorization", 1000, Duration::from_secs(3600)));

// Global ceiling for an expensive endpoint
router.rate_limit(RateLimitLayer::global(10, Duration::from_secs(1)));
```

En cas d'épuisement : `429 Too Many Requests` avec l'en-tête `Retry-After`. Chaque réponse réussie inclut `X-RateLimit-Limit` + `X-RateLimit-Remaining`.

> **Derrière un reverse proxy, associez `per_ip` avec `real_ip`.** `RateLimitLayer::per_ip` s'indexe sur le socket de connexion (`ConnectInfo`), qui derrière un proxy est l'IP *du proxy* — donc tous les clients partagent un seul compartiment et la limite est inutile. Placez `real_ip::RealIpLayer` (qui lit `X-Forwarded-For` / `X-Real-IP`) avant lui pour que la vraie IP client soit utilisée.

`RateLimitLayer` est **local au processus** — il compte les requêtes uniquement au sein d'une instance en cours d'exécution, ce qui convient si vous exécutez une seule instance. Si vous exécutez plusieurs instances (répliques) derrière un load balancer, chacune tiendrait son propre compte, et la limite réelle se multiplierait. Pour partager un seul compte entre toutes les répliques, utilisez `rate_limit_cache::CacheRateLimitLayer`, qui délègue à n'importe quelle implémentation de `cache::Cache` (à associer avec `cache::RedisCache` pour un compteur partagé incrémenté de façon atomique par le `INCRBY` de Redis) :

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

## Autoriser ou bloquer des IP

Verrouillez les routes sensibles (comme votre admin) à un ensemble connu d'adresses IP, ou bloquez des abuseurs spécifiques. Une liste d'autorisation ne permet que les réseaux listés ; une liste de blocage refuse ceux qui sont listés.

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

**IPv4 + IPv6 pris en charge.** Sûr entre familles (un CIDR IPv4 ne correspond pas aux adresses IPv6). Renvoie `403 Forbidden` en cas de rejet.

**Important :** **Rustango** lit l'IP client depuis `ConnectInfo<SocketAddr>`. Montez avec :

```rust
axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

Si votre reverse proxy transmet `X-Forwarded-For`, configurez-le pour définir `RemoteAddr` — `ip_filter` lit le pair de connexion, pas les en-têtes.

---

## Se protéger contre le CSRF

> **CSRF et API à jeton.** Le CSRF protège les requêtes **authentifiées par cookie**. Une
> API purement à jeton / Bearer / [JWT](auth-jwt.md) n'est pas une cible CSRF — n'appliquez pas
> la couche CSRF sur celle-ci (sinon vous renverrez des 403 à des clients légitimes). Pour une SPA qui *utilise bien*
> des cookies, lisez le cookie `rustango_csrf` et renvoyez-le dans l'en-tête
> `X-CSRF-Token`.

Le CSRF (cross-site request forgery) survient lorsqu'un autre site trompe le navigateur d'un utilisateur connecté pour lui faire soumettre une requête à votre application. La défense est un jeton secret sur chaque formulaire, tout comme le `{% csrf_token %}` de Django. Le middleware CSRF vit dans `rustango::forms::csrf` (derrière la feature `csrf`, activée automatiquement par la feature `admin`) :

```rust
use rustango::forms::csrf;

let app = Router::new()
    .route("/contact", get(form).post(submit))
    .layer(csrf::layer());
```

`csrf::layer()` construit la couche avec des valeurs par défaut raisonnables ; `csrf::with_config(CsrfConfig)` vous permet de remplacer les noms de cookie/en-tête et le flag `Secure`. Dans les templates, `{{ csrf_token }}` vous donne le jeton brut et `{{ csrf_input }}` vous donne un `<input>` caché prêt à l'emploi — déposez-en un dans chaque formulaire. Il utilise le motif du cookie à double soumission : sur les méthodes non sûres (POST, PUT, PATCH, DELETE), la couche vérifie l'en-tête `X-CSRF-Token` (ou le champ de formulaire `_csrf`) par rapport au cookie `rustango_csrf` ; une discordance renvoie `403 Forbidden`.

**Exempter les endpoints collecteurs.** `CsrfConfig::exempt_prefix("/path")` (répétable) ignore l'application du CSRF pour les méthodes non sûres sur les requêtes dont le chemin commence par le préfixe donné. Ceci concerne les endpoints append-only, sans état d'authentification, atteints via `navigator.sendBeacon` — par exemple un collecteur d'analytics — qui ne peuvent pas définir un en-tête `X-CSRF-Token` et, lorsque la page est servie depuis un cache CDN qui supprime `Set-Cookie`, peuvent ne porter aucun cookie CSRF du tout. Gardez les préfixes étroits et n'exemptez jamais quoi que ce soit qui lit ou écrit un état d'authentification.

L'auto-admin active le CSRF sur chaque mutation par défaut, et il n'y a aucun moyen de s'y soustraire.

---

## Prévenir le XSS

Le XSS (cross-site scripting) survient lorsqu'une entrée utilisateur est rendue en HTML et s'exécute comme du code dans le navigateur de quelqu'un d'autre. La solution est d'échapper toute entrée utilisateur avant qu'elle n'atteigne la page. **Rustango** gère cela de deux façons :

**1. Auto-échappement des templates Tera** — Tera est le moteur de templates de **Rustango** (comme les templates Django ou Blade). Chaque `{{ var }}` est automatiquement échappé en HTML. Utilisez `{{ var | safe }}` pour vous en soustraire — rare, et dangereux, donc ne le faites que pour du HTML auquel vous faites entièrement confiance.

**2. Fonction d'échappement manuelle** — pour quand vous construisez du HTML dans du code Rust au lieu d'un template :

```rust
use rustango::text::html_escape;

let safe = html_escape(user_input);
// "<script>" → "&lt;script&gt;"
// "a & b" → "a &amp; b"
```

Remplace `&`, `<`, `>`, `"`, `'`. Convient pour le contenu d'éléments HTML + les attributs entre guillemets doubles.

Pour une défense XSS basée sur le CSP, voir [Définir les en-têtes de sécurité](#setting-security-headers).

---

## Prévenir l'injection SQL

L'injection SQL survient lorsqu'une entrée utilisateur est collée directement dans une chaîne SQL et modifie la requête. L'ORM de **Rustango** empêche cela par conception : il utilise partout le binding de paramètres de sqlx — chaque valeur est envoyée comme un placeholder `$1, $2, ...`, jamais collée dans le texte de la requête. (sqlx est la bibliothèque de base de données sous-jacente.) **Vous ne pouvez pas accidentellement concaténer une entrée utilisateur dans du SQL via l'API de l'ORM.**

Il y a deux endroits où rester vigilant :

**1. Les noms de table et de colonne dans `bulk_actions` :**

Les placeholders protègent les valeurs, mais ils ne peuvent pas porter un identifiant (un nom de table ou de colonne), donc ceux-ci ont besoin de leur propre garde-fou. Le runner d'actions groupées ne colle jamais de noms de table/colonne bruts fournis par l'utilisateur dans du SQL. En interne, il valide chaque identifiant (une fonction privée au crate,
`validate_ident`, qui rejette `"`, `` ` ``, `\0`, `\n`, `\r`, `\`, `;`,
les espaces et les caractères de contrôle) puis le met entre guillemets via `dialect.quote_ident()`
avant de construire l'instruction. Ce garde-fou s'exécute pour vous — vous ne l'appelez
pas vous-même.

**2. L'échappatoire du SQL brut :**

Si vous descendez au niveau de `sqlx` brut, liez chaque *valeur* comme un paramètre et
ne collez jamais un *identifiant* (nom de table ou de colonne) provenant d'une
entrée utilisateur dans la chaîne de requête — les placeholders ne peuvent pas porter d'identifiants :

```rust
// ✅ values go through bound placeholders
sqlx::query("SELECT * FROM posts WHERE id = $1")
    .bind(1).fetch_all(&pool).await?;

// ❌ NEVER interpolate a user-supplied identifier into the SQL string
let sql = format!("SELECT * FROM \"{user_table}\" WHERE id = $1");
sqlx::query(&sql).bind(1).fetch_all(&pool).await?;
```

---

## Authentifier les utilisateurs

L'authentification est la manière dont vous confirmez qui effectue une requête. **Rustango** fournit trois backends prêts à l'emploi (Basic auth, clés d'API et JWT) et vous laisse écrire les vôtres — un peu comme les backends d'authentification de Django. Vous les attachez aux routes, et les requêtes sans identifiant reconnu reçoivent un `401`.

> **SSO admin.** Pour permettre aux opérateurs de se connecter à l'admin avec un IdP externe (Google, Microsoft/Azure AD, GitHub, ou tout fournisseur OpenID Connect) au lieu d'un mot de passe, activez la feature `admin-sso` — voir le [guide SSO](sso.md). Les fournisseurs sont **gérés depuis l'interface admin sous forme de lignes** (plusieurs par surface ; par tenant, ou un ensemble partagé entre tenants), avec le secret client **chiffré au repos**. C'est un rattachement à l'existant (l'email vérifié de l'IdP doit correspondre à un utilisateur admin ; pas d'auto-provisionnement) et cela réutilise la session existante.

### Trois backends prêts à l'emploi

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

Le middleware essaie chaque backend dans l'ordre. Le premier qui réussit l'emporte ; le premier qui renvoie une erreur dure arrête la chaîne.

### Écrire votre propre backend

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

## Hacher et vérifier les mots de passe

Ne stockez jamais les mots de passe en clair. Hachez-les à l'inscription avec `hash`, et à la connexion comparez la tentative avec `verify`. Utilisez `strength_score` pour rejeter les mots de passe faibles avant même de les hacher.

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

Le hachage utilise **argon2id** (le défaut recommandé par l'OWASP depuis 2023 — il résiste mieux au cassage par GPU que bcrypt). Les hachages sont stockés au format PHC-string, de sorte que vous pouvez changer les paramètres plus tard sans casser les anciens hachages.

La **vérification de robustesse** intégrée est intentionnellement minimale. Pour les déploiements sérieux, vérifiez aussi les mots de passe par rapport à l'API HIBP / pwned-passwords afin de rejeter ceux connus comme divulgués.

> **À approfondir :** [Mots de passe](auth-passwords.md) — internals d'argon2id, salage automatique, connexions résistantes aux attaques temporelles (`verify_dummy`), et où vit le hachage. Une fois authentifié, passez la main à une [Session](auth-sessions.md) (navigateur) ou un [JWT](auth-jwt.md) (API).

---

## Émettre et rafraîchir des JWT

Un JWT (JSON Web Token) est un jeton signé qu'un client envoie au lieu de se connecter à chaque fois. `JwtLifecycle` gère tout le flux : un jeton d'accès à courte durée de vie plus un jeton de rafraîchissement à durée de vie plus longue, avec vérification, rafraîchissement et révocation intégrés.

```rust
use rustango::tenancy::jwt_lifecycle::{JwtLifecycle, JwtTokenPair, JwtIssueError};
use serde_json::json;

let jwt = JwtLifecycle::new(secret)
    .with_access_ttl(900)                         // 15 min
    .with_refresh_ttl(7 * 86400);                 // 7 days
```

### Émettre un jeton avec des claims personnalisés (pas de lookup DB à la vérification)

```rust
let pair = jwt.issue_pair_with(user_id, json!({
    "roles":  ["admin", "editor"],
    "tenant": "acme",
    "scope":  "read:posts write:posts",
    "email":  "alice@example.com",
}).as_object().unwrap().clone())?;
```

Les « claims » sont les faits clé/valeur que vous intégrez dans le jeton (rôles, tenant, etc.) ; le client les renvoie et vous les lisez sans toucher à la base de données. Les noms de claim réservés (`sub`, `exp`, `jti`, `typ`) sont utilisés par le framework et ne peuvent pas apparaître dans vos claims personnalisés — essayer renvoie `JwtIssueError::ReservedClaim`.

### Vérifier un jeton

```rust
let claims = jwt.verify_access(&access_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
let tenant: String = claims.get_custom("tenant").unwrap_or_default();
```

### Rafraîchir un jeton (conserve vos claims personnalisés)

```rust
let new_pair = jwt.refresh(&refresh_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;
// new_pair has the same roles + scope + tenant
```

Le JTI de l'ancien jeton de rafraîchissement (son ID unique) est ajouté à une liste noire pour qu'il ne puisse pas être réutilisé — cela bloque les attaques par rejeu où un ancien jeton volé est renvoyé.

### Rafraîchir avec re-vérification des permissions

```rust
// returns Result<Option<JwtTokenPair>, JwtIssueError>:
// Err on a reserved-claim collision, Ok(None) on an invalid/expired refresh.
let new_pair = jwt.refresh_with(&refresh_token, json!({
    "roles": ["viewer"],     // role was demoted since login
    "tenant": "acme",
}).as_object().unwrap().clone())?
    .ok_or(StatusCode::UNAUTHORIZED)?;
```

### Révoquer un jeton

```rust
jwt.revoke(&access_token);     // adds JTI to blacklist
jwt.revoke(&refresh_token);
```

La liste noire en mémoire par défaut (`InMemoryJtiStore`) nettoie d'elle-même les entrées expirées, mais elle vit dans un seul processus et oublie chaque révocation au redémarrage. Pour les déploiements multi-processus, installez un store partagé et durable via `JwtLifecycle::new(secret).with_jti_store(store)` — `store` est n'importe quel `Arc<dyn JtiStore>` (par exemple un store adossé à Redis ou à une base de données). Sans store partagé, un jeton que vous révoquez sur une réplique peut encore être rejoué sur une autre jusqu'à son expiration.

> **À approfondir :** [API d'authentification JWT](auth-jwt-api.md) — le routeur intégré `/api/auth/login|refresh|logout|me`, le rafraîchissement glissant, et le store de révocation JTI. Pour un seul jeton auto-géré, voir [JWT (autonome)](auth-jwt.md). Pour restreindre les routes navigateur/API selon la connexion, voir [décorateurs d'accès](auth-decorators.md).

---

## S'authentifier avec des clés d'API

Les clés d'API permettent aux scripts et aux services de s'authentifier sans nom d'utilisateur, mot de passe ni session — pratique pour l'accès machine à machine. Vous générez une clé, la montrez à l'utilisateur une seule fois, et ne stockez que son préfixe et son hachage.

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

Ces clés fonctionnent directement avec le `ApiKeyBackend` de la multi-tenancy — les deux utilisent le même format `{prefix}.{secret}` avec des secrets hachés en argon2id, de sorte qu'une clé émise ici s'authentifie là-bas sans travail supplémentaire.

---

## Ajouter l'authentification à deux facteurs (TOTP)

Le TOTP est le code à 6 chiffres d'une application d'authentification — un second facteur en plus du mot de passe. Vous générez un secret par utilisateur, le montrez sous forme de QR code pour l'enrôlement, puis vérifiez le code qu'il saisit à chaque connexion. Il suit la norme RFC 6238.

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

Fonctionne avec Google Authenticator, Authy, 1Password, Bitwarden et d'autres applications d'authentification standard.

**Codes de récupération** (codes de secours à usage unique pour quand un utilisateur perd son téléphone) pas encore fournis. Le motif courant est de stocker 8 à 10 codes hachés par utilisateur et d'en brûler un à chaque utilisation.

---

## Envoyer des URL signées (liens magiques)

Une URL signée porte une signature infalsifiable, de sorte que vous pouvez lui faire confiance sans session — parfait pour les connexions par lien magique, les réinitialisations de mot de passe et les liens de téléchargement à durée limitée. Vous `sign` une URL (optionnellement avec une expiration) et la `verify` quand l'utilisateur clique. (Laravel les appelle des « signed routes ».)

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

Usages courants :
- Connexion par lien magique (envoyer par email une URL à usage unique au lieu de demander un mot de passe)
- Confirmation de réinitialisation de mot de passe
- Téléchargements de fichiers à durée limitée
- Liens « Cliquez ici pour vérifier votre email »
- Liens de désinscription (aucune authentification nécessaire ; l'URL elle-même prouve l'intention)

Avant la signature, les paramètres de requête sont triés dans un ordre fixe, de sorte qu'un attaquant ne peut pas les réordonner (par exemple déplacer `?expires=...`) pour changer ce que couvre la signature et forger un lien d'apparence valide.

> À approfondir : [Flux de compte](auth-flows.md) construit la réinitialisation de mot de passe, la vérification d'email et la connexion par lien magique sur cette primitive d'URL signée.

---

## Vérifier les webhooks entrants

Un webhook est un rappel HTTP qu'un autre service vous envoie (un paiement a réussi, un push a eu lieu). Vérifiez toujours sa signature pour savoir qu'il provient réellement de ce service et que le corps n'a pas été modifié. `verify_signature` gère les formats courants.

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

| Format | Utilisé par |
|---|---|
| `HexSha256WithPrefix` | GitHub (`sha256=<hex>`) |
| `HexSha256` | Slack, fournisseurs HMAC bruts |
| `Base64Sha256` | Stripe, AWS SNS |

La comparaison de signature est à temps constant, ce qui signifie qu'elle prend toujours le même temps que la supposition soit correcte ou fausse. Cela stoppe les attaques temporelles, où un attaquant mesure de minuscules différences de temps de réponse pour deviner le secret un caractère à la fois.

---

## Garder les secrets hors de vos journaux

Les URL journalisées peuvent divulguer des mots de passe et des jetons qui apparaissent dans les query strings. `AccessLogLayer` masque automatiquement les paramètres d'identifiant connus avant d'écrire la ligne de journal (PII = informations personnellement identifiables).

```rust
let app = router.access_log(AccessLogLayer::default());
```

Par défaut, les valeurs de ces query params sont remplacées par `[redacted]` :
`password`, `passwd`, `token`, `secret`, `api_key`, `apikey`, `access_token`, `refresh_token`, `signature`, `auth`.

Ajouter à la liste :

```rust
let layer = AccessLogLayer::default()
    .redact_additional("session_id")
    .redact_additional("private_key");
```

Remplacer toute la liste :

```rust
let layer = AccessLogLayer::default()
    .redact(vec!["only_this".into()]);
```

Note : ceci masque uniquement les **query strings**. Pour nettoyer les corps de requête ou les en-têtes, filtrez à la source avec `tracing::Subscriber`.

---

## Tracer les requêtes entre services

Un identifiant de requête étiquette chaque ligne de journal d'une même requête avec le même ID, de sorte que vous pouvez suivre cette requête à travers les handlers — et à travers les services. `RequestIdLayer` en ajoute un à chaque requête.

```rust
let app = router.request_id(RequestIdLayer::default());
```

Il réutilise un `X-Request-Id` entrant (pour que les services chaînés partagent un seul ID) ou génère un nouvel ID base64 de 22 caractères. Les IDs entrants sont d'abord validés — tout ce qui contient des caractères de contrôle, des retours à la ligne, des octets nuls, ou dépasse 128 caractères est rejeté, ce qui bloque les attaques par injection d'en-tête.

Dans les handlers :

```rust
async fn handler(id: rustango::request_id::RequestId) -> String {
    tracing::info!(req_id = %id.0, "handling /me");
    format!("req {}", id.0)
}
```

Dans une configuration zero-trust, où vous ne faites pas confiance aux IDs envoyés par les services en amont, générez toujours le vôtre :

```rust
router.request_id(RequestIdLayer::always_generate());
```

---

## Gérer les secrets

Ne codez jamais en dur des secrets (mots de passe de base de données, clés d'API) dans votre source. Lisez-les à l'exécution via le trait `Secrets`, qui abstrait l'endroit où ils vivent réellement. (Un « trait » est la version Rust d'une interface.) Le `EnvSecrets` intégré lit depuis les variables d'environnement.

```rust
use rustango::secrets::{Secrets, EnvSecrets, BoxedSecrets};
use std::sync::Arc;

// Env vars (with optional prefix)
let secrets: BoxedSecrets = Arc::new(EnvSecrets::with_prefix("MYAPP_"));

let db_pwd = secrets.require("DB_PASSWORD").await?;       // reads MYAPP_DB_PASSWORD
let redis_url = secrets.get("REDIS_URL").await?;          // None when unset
```

Pour extraire des secrets depuis Vault, AWS Secrets Manager ou GCP Secret Manager, implémentez le trait vous-même — ce n'est qu'une seule méthode async.

---

## Auditer avant de déployer

Avant de passer en production, lancez l'audit intégré. Il attrape les mauvaises configurations courantes — comme un secret de signature manquant ou trop court — que vous ne voulez pas découvrir en production.

```bash
manage check --deploy
```

Vérifications toujours actives (exécutées avec ou sans `--deploy`) :
- ✅ Modèles enregistrés dans l'inventaire
- ✅ DB joignable (`SELECT 1`)
- ✅ Migrations sur disque pour les modèles enregistrés

Vérifications `--deploy` supplémentaires (durcissement pour la production) :
- ✅ `RUSTANGO_ENV` est `prod` ou `production`
- ✅ `RUSTANGO_SESSION_SECRET` défini et ≥ 32 octets (la clé HMAC pour la signature des cookies + JWT — `SECRET_KEY` n'est **pas** lu par le framework), sans placeholder du scaffolder laissé en place
- ✅ `DATABASE_URL` défini (et avertit s'il pointe vers localhost)
- ⚠️ Avertissements de cohérence `RUSTANGO_APEX_DOMAIN` / `RUSTANGO_BIND` pour la tenancy + le binding non-loopback

Renvoie un code de sortie non nul en cas de constat de **niveau erreur** (idéal pour les gates de CI). Les avertissements ne déclenchent pas d'échec mais s'affichent dans la sortie.

Pour des vérifications au-delà de ce qui est fourni, étendez avec du code personnalisé dans votre binaire `manage` avant de transmettre à `manage::run`.

---

## Ce qui n'est PAS encore fourni

Quelques flux de bout en bout ont encore besoin d'être assemblés à partir des primitives ci-dessous :

- **Réinitialisation de mot de passe + vérification d'email** — les helpers d'émission/vérification de jeton existent (`auth_flows::confirm_password_reset_pool`, plus les allers-retours de vérification d'email) et le pipeline d'email est fourni séparément, mais il n'y a pas de cycle unique préconstruit vue + email + validation câblé pour vous.
- **Masquage PII du corps de requête / des en-têtes** dans `access_log` — le masquage de journal au niveau des champs existe (voir [Garder les secrets hors de vos journaux](#keeping-secrets-out-of-your-logs)), mais pas le nettoyage automatique du corps de requête ou des en-têtes.

Déjà fournis (n'allez pas chercher un contournement) :

- **Connexion sociale OAuth2 / OIDC** — `oauth2::providers` fournit les helpers Google, GitHub, Microsoft, GitLab et Discord (plus `OAuth2Provider::from_discovery` pour tout fournisseur OIDC), et `oauth2::router::oauth2_router` monte les routes de login + callback, crée l'enregistrement utilisateur et définit le cookie de session.
- **Verrouillage par compte** — `rustango::account_lockout::Lockout` (adossé au cache ; `is_locked` / `record_failure` / `clear`, `max_attempts` + `lockout_duration` configurables).
- **Endpoint de rapport CSP** — `security_headers::csp_report_router(path)` + `SecurityHeadersLayer::csp_report_uri(uri)`.
- **Limitation de débit distribuée** — `rate_limit_cache::CacheRateLimitLayer` (voir [Limiter le débit des requêtes](#rate-limiting-requests)).

Jusqu'à ce que les flux non fournis arrivent, assemblez-les à partir des primitives ci-dessus (`signed_url::sign` pour les jetons de réinitialisation de mot de passe, le pipeline d'email pour la livraison, etc.).

---

## Voir aussi

- [Middleware](middleware.md) — comment les couches de sécurité s'attachent, leur ordre, et le catalogue complet.
- [Authentification](auth-passwords.md) — mots de passe, sessions, JWT, clés d'API, HMAC, backends d'authentification et flux de compte, chacun en profondeur.
- [Conventions d'API](api-conventions.md) — les règles de type d'erreur et de type de retour derrière ces API.
