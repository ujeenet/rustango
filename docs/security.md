# Security guide

A walkthrough of every security feature rustango ships and how to compose them. Pair with `manage check --deploy` for an automated audit.

## Table of contents

- [Defense-in-depth checklist](#defense-in-depth-checklist)
- [Security headers](#security-headers)
- [CORS](#cors)
- [Rate limiting](#rate-limiting)
- [IP allowlist / blocklist](#ip-allowlist--blocklist)
- [CSRF](#csrf)
- [XSS defense](#xss-defense)
- [SQL injection](#sql-injection)
- [Authentication](#authentication)
- [Password hashing + strength](#password-hashing--strength)
- [JWT lifecycle](#jwt-lifecycle)
- [API keys](#api-keys)
- [TOTP / 2FA](#totp--2fa)
- [Signed URLs (magic links)](#signed-urls-magic-links)
- [Webhook signatures](#webhook-signatures)
- [Access log PII redaction](#access-log-pii-redaction)
- [Request ID for log correlation](#request-id-for-log-correlation)
- [Secrets management](#secrets-management)
- [Pre-deploy audit](#pre-deploy-audit)

---

## Defense-in-depth checklist

A production rustango app should layer these (most are one line each):

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

## Security headers

`SecurityHeadersLayer` sets the canonical HTTP response headers Django ships by default and Rocket auto-attaches via `Shield`.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};

let app = router.security_headers(SecurityHeadersLayer::strict());
```

### Presets

| Preset | When to use |
|---|---|
| `strict()` | Production: HSTS preload + XFO=DENY + nosniff + Referrer-Policy=no-referrer + COOP=same-origin + Permissions-Policy locked down |
| `relaxed()` | Embeddable in iframes: SAMEORIGIN + 1y HSTS |
| `dev()` | Local: nosniff only (no HSTS to avoid locking localhost into HTTPS forever) |
| `empty()` | Build up from scratch |

### Custom CSP

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

### Staged CSP rollout

Use `csp_report_only` to monitor without breaking production:

```rust
let layer = SecurityHeadersLayer::strict()
    .csp(CspBuilder::strict_starter().build())
    .csp_report_only(true);                       // sends Content-Security-Policy-Report-Only
```

After verifying no console errors → flip `csp_report_only(false)` to enforce.

---

## CORS

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

**Security note:** never combine `allow_credentials(true)` with `allow_any_origin()` — the browser will reject the response. With credentials, you MUST list explicit origins.

---

## Rate limiting

```rust
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};

// Per-IP
router.rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)));

// Per-API-key (header)
router.rate_limit(RateLimitLayer::per_header("authorization", 1000, Duration::from_secs(3600)));

// Global ceiling for an expensive endpoint
router.rate_limit(RateLimitLayer::global(10, Duration::from_secs(1)));
```

When exhausted: `429 Too Many Requests` with `Retry-After` header. Every successful response includes `X-RateLimit-Limit` + `X-RateLimit-Remaining`.

**Process-local** — for distributed enforcement across replicas, pair with a Redis-backed counter (separate effort; not yet shipped).

---

## IP allowlist / blocklist

Restrict admin / internal routes to known networks:

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

**IPv4 + IPv6 supported.** Cross-family safe (an IPv4 CIDR doesn't match IPv6 addresses). Returns `403 Forbidden` on rejection.

**Important:** rustango reads the client IP from `ConnectInfo<SocketAddr>`. Mount with:

```rust
axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
```

If your reverse proxy forwards `X-Forwarded-For`, configure it to set `RemoteAddr` — `ip_filter` reads the connecting peer, not headers.

---

## CSRF

CSRF middleware is in `rustango::forms::csrf` (behind the `csrf` feature, implied by `admin`):

```rust
use rustango::forms::csrf::CsrfLayer;

let app = Router::new()
    .route("/contact", get(form).post(submit))
    .layer(CsrfLayer::default());
```

Renders `{% csrf_token %}` in templates expand into a hidden form input. Double-submit cookie pattern — token in form must match cookie.

The auto-admin enables CSRF on every mutation by default (no opt-out).

---

## XSS defense

Two layers:

**1. Tera template auto-escape** — every `{{ var }}` is HTML-escaped by default. Use `{{ var | safe }}` to opt out (rare, dangerous).

**2. Manual escape helper** — for non-template HTML construction:

```rust
use rustango::text::html_escape;

let safe = html_escape(user_input);
// "<script>" → "&lt;script&gt;"
// "a & b" → "a &amp; b"
```

Replaces `&`, `<`, `>`, `"`, `'`. Suitable for HTML element content + double-quoted attributes.

For CSP-based XSS defense, see [Security headers](#security-headers).

---

## SQL injection

The ORM uses sqlx parameter binding throughout — every value goes through `$1, $2, ...` placeholders. **Direct concatenation of user input into SQL is impossible at the ORM API surface.**

Two places to be careful:

**1. Identifier interpolation in `bulk_actions` + `fixtures`:**

```rust
use rustango::bulk_actions::validate_ident;

validate_ident("user-supplied-table")?;   // rejects ;, ", \n, control chars
```

The framework calls this internally for any user-supplied table/column names.

**2. Raw SQL escape hatch:**

```rust
sqlx::query("SELECT * FROM ? WHERE id = $1")    // ❌ NEVER interpolate identifiers
    .bind(1).fetch_all(&pool).await?;

let table = validate_ident(user_table)?;        // ✅ validate first
let sql = format!("SELECT * FROM \"{table}\" WHERE id = $1");
sqlx::query(&sql).bind(1).fetch_all(&pool).await?;
```

---

## Authentication

### Three pluggable backends

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

The middleware tries each backend in order. First success wins; first hard error short-circuits.

### Custom backend

```rust
use rustango::tenancy::auth_backends::{AuthBackend, AuthUser, AuthError};
use async_trait::async_trait;

pub struct OAuthBackend { /* ... */ }

#[async_trait]
impl AuthBackend for OAuthBackend {
    async fn authenticate(&self, parts: &Parts, pool: &PgPool)
        -> Result<Option<AuthUser>, AuthError>
    {
        // Read your custom header, validate, look up the user
        Ok(None)  // means: not my request, try next backend
    }
}
```

---

## Password hashing + strength

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

Uses **argon2id** (default since OWASP 2023 — better than bcrypt against GPU attacks). PHC-string format is forward-compatible with parameter changes.

**Strength check** is intentionally minimal — pair with HIBP / pwned-passwords API for serious deployments.

---

## JWT lifecycle

```rust
use rustango::tenancy::jwt_lifecycle::{JwtLifecycle, JwtTokenPair, JwtIssueError};
use serde_json::json;

let jwt = JwtLifecycle::new(secret)
    .with_access_ttl(900)                         // 15 min
    .with_refresh_ttl(7 * 86400);                 // 7 days
```

### Issue with custom claims (no DB lookup on verify)

```rust
let pair = jwt.issue_pair_with(user_id, json!({
    "roles":  ["admin", "editor"],
    "tenant": "acme",
    "scope":  "read:posts write:posts",
    "email":  "alice@example.com",
}).as_object().unwrap().clone())?;
```

Reserved claim names (`sub`, `exp`, `jti`, `typ`) cannot appear in custom — returns `JwtIssueError::ReservedClaim`.

### Verify

```rust
let claims = jwt.verify_access(&access_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
let tenant: String = claims.get_custom("tenant").unwrap_or_default();
```

### Refresh — preserves custom claims

```rust
let new_pair = jwt.refresh(&refresh_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;
// new_pair has the same roles + scope + tenant
```

The old refresh token's JTI is added to the blacklist, preventing replay.

### Refresh with re-evaluated permissions

```rust
let new_pair = jwt.refresh_with(&refresh_token, json!({
    "roles": ["viewer"],     // role was demoted since login
    "tenant": "acme",
}).as_object().unwrap().clone())?;
```

### Revoke

```rust
jwt.revoke(&access_token);     // adds JTI to blacklist
jwt.revoke(&refresh_token);
```

In-memory blacklist auto-prunes expired entries. For multi-process deployments, swap to a cache-backed implementation (separate effort).

---

## API keys

For API-key authentication that doesn't require user-password sessions:

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

**Wire-compatible with the multi-tenancy `ApiKeyBackend`** — both use the same `{prefix}.{secret}` format with argon2id-hashed secrets.

---

## TOTP / 2FA

RFC 6238 time-based one-time passwords:

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

Compatible with Google Authenticator, Authy, 1Password, Bitwarden, etc.

**Recovery codes** (one-time backup codes) — not yet shipped; common pattern is to store 8–10 hashed codes per user and burn one when used.

---

## Signed URLs (magic links)

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

Common uses:
- Magic-link login (email a one-time URL instead of asking for password)
- Password reset confirmation
- Time-limited file downloads
- "Click here to verify your email" links
- Unsubscribe links (no auth needed; URL itself proves intent)

Canonical query-param sorting prevents reorder forgery (e.g. attacker can't move `?expires=...` around to change the signature input).

---

## Webhook signatures

For inbound webhooks (Stripe, GitHub, Slack, custom):

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

| Format | Used by |
|---|---|
| `HexSha256WithPrefix` | GitHub (`sha256=<hex>`) |
| `HexSha256` | Slack, raw HMAC providers |
| `Base64Sha256` | Stripe, AWS SNS |

Comparison is constant-time (defends against timing attacks).

---

## Access log PII redaction

`AccessLogLayer` redacts known credential params from logged URLs:

```rust
let app = router.access_log(AccessLogLayer::default());
```

By default, values for these query params get replaced with `[redacted]`:
`password`, `passwd`, `token`, `secret`, `api_key`, `apikey`, `access_token`, `refresh_token`, `signature`, `auth`.

Add to the list:

```rust
let layer = AccessLogLayer::default()
    .redact_additional("session_id")
    .redact_additional("private_key");
```

Replace the entire list:

```rust
let layer = AccessLogLayer::default()
    .redact(vec!["only_this".into()]);
```

Note: this redacts **query strings** only. For request bodies / headers, use `tracing::Subscriber` filtering at the source.

---

## Request ID for log correlation

```rust
let app = router.request_id(RequestIdLayer::default());
```

Honors inbound `X-Request-Id` (chained services) or generates a fresh 22-char base64 ID. Inbound IDs are validated — control chars / newlines / null bytes / >128 chars rejected (header-injection defense).

In handlers:

```rust
async fn handler(id: rustango::request_id::RequestId) -> String {
    tracing::info!(req_id = %id.0, "handling /me");
    format!("req {}", id.0)
}
```

For zero-trust environments where you don't trust upstream IDs:

```rust
router.request_id(RequestIdLayer::always_generate());
```

---

## Secrets management

Don't hard-code secrets — read via the `Secrets` trait:

```rust
use rustango::secrets::{Secrets, EnvSecrets, BoxedSecrets};
use std::sync::Arc;

// Env vars (with optional prefix)
let secrets: BoxedSecrets = Arc::new(EnvSecrets::with_prefix("MYAPP_"));

let db_pwd = secrets.require("DB_PASSWORD").await?;       // reads MYAPP_DB_PASSWORD
let redis_url = secrets.get("REDIS_URL").await?;          // None when unset
```

For Vault / AWS Secrets Manager / GCP Secret Manager, implement the trait yourself (one async method).

---

## Pre-deploy audit

```bash
manage check --deploy
```

Checks:
- ✅ DB reachable
- ✅ Models registered in inventory
- ✅ Pending migrations applied
- ✅ `RUSTANGO_ENV` is `prod` or `production`
- ✅ `SECRET_KEY` set and ≥ 32 bytes
- ✅ `DATABASE_URL` set

Returns non-zero exit on any **error-level** finding (good for CI gates). Warnings don't trigger failure but show in output.

For checks beyond what ships, extend with custom code in your `manage` binary before forwarding to `manage::run`.

---

## What's NOT yet shipped

The framework comparison roadmap (memory:`framework-comparison-2026-05-02.md`) flags these as future work:

- **OAuth2 social login** (Google, GitHub, etc.) — Tier 2
- **Account lockout after N failed logins** (per-account, not just per-IP)
- **Password reset + email verification end-to-end flow** (helpers exist; no built-in token+email+view+validate cycle)
- **CSP report endpoint** for `report-uri` directive
- **Distributed rate limiting** via cache layer
- **PII redaction of request body / headers** in access_log

Until these ship, glue them yourself using the primitives above (`signed_url::sign` for password reset tokens, `cache::set` for account lockout counters, etc.).
