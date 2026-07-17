# Admin SSO (OpenID Connect / social login)

Sign in to the rustango admin with an external identity provider —
Google, Microsoft / Azure AD, GitHub, GitLab, Discord, or **any OpenID
Connect provider** (Okta, Auth0, Keycloak, …) — instead of a local
password. Enable it with the `admin-sso` cargo feature.

SSO is **link-to-existing**: the verified email the IdP returns must
match an existing admin user. SSO authenticates the person; it never
creates accounts and never grants access on its own. An unknown or
unverified email is refused.

```toml
[dependencies]
rustango = { version = "0.46", features = ["admin-sso"] }
```

## How it works

1. The login page shows a **"Sign in with &lt;provider&gt;"** button.
2. Clicking it (`GET <login>/sso`) redirects to the IdP with a signed,
   short-lived flow cookie (PKCE + CSRF `state`).
3. The IdP sends the user back to `<login>/sso/callback`.
4. rustango verifies the flow, exchanges the code, reads `/userinfo`,
   and requires **`email_verified`**.
5. It looks up an admin user by that email. If one exists and is active,
   it mints the **same signed-cookie session** a password login
   produces, bound to that user — so every existing gate (superuser /
   permissions, live password-change invalidation) still applies.
6. No match → the user is bounced back to the login page with a generic
   error (details go to the server log, never the browser).

The client **secret is never stored in plaintext** — it's a reference
(`env://…`) resolved at login time, the same way `Org.database_url` is.

## Single-tenant (no multi-tenancy)

Configure one global provider. Set the email column on the admin user
you want to allow, then enable SSO.

### Via code

```rust
use rustango::admin::{Builder, sso::BareSsoConfig};

let admin = Builder::new(pool)
    .with_session_auth(secret)          // SSO reuses this session
    .with_sso(BareSsoConfig {
        provider: "google".into(),      // or microsoft/github/gitlab/discord/oidc
        issuer_url: None,               // required only for provider = "oidc"
        client_id: std::env::var("SSO_CLIENT_ID")?,
        client_secret: std::env::var("SSO_CLIENT_SECRET")?,
        redirect_uri: "https://admin.example.com/login/sso/callback".into(),
    })
    .build();
```

### Via config

```toml
[sso]
enabled = true
provider = "oidc"                       # generic OpenID Connect
issuer_url = "https://id.example.com"   # discovery: {issuer}/.well-known/openid-configuration
client_id = "rustango-admin"
# client_secret comes from RUSTANGO__SSO__CLIENT_SECRET (env overlay), not TOML
redirect_uri = "https://admin.example.com/login/sso/callback"
```

`Builder::from_settings(pool, &settings)` wires this automatically.
Incomplete config (missing provider / client_id / redirect_uri) logs a
warning and leaves SSO off rather than half-wiring a broken button.

Link an operator to their IdP identity by setting the `email` column on
their `rustango_admin_users` row to the address the IdP returns.

## Multi-tenant — per-tenant IdP

Each tenant brings its own identity provider. The config lives on the
`Org` row, editable in the operator console's org-edit form:

| Column | Meaning |
|---|---|
| `sso_enabled` | Turns the button on for this tenant. |
| `sso_provider` | `google` / `microsoft` / `github` / `gitlab` / `discord` / `oidc`. |
| `sso_issuer_url` | OIDC discovery base URL (for `provider = "oidc"`). |
| `sso_client_id` | The tenant's OAuth client id. |
| `sso_secret_ref` | **Reference** to the client secret — e.g. `env://ACME_SSO_SECRET` — resolved by the tenancy `SecretsResolver`. Never the raw secret. |

The callback URL is derived per tenant from the request host
(`https://<tenant-host><login>/sso/callback`), so register that with
each tenant's IdP. Link a tenant user by setting the `email` column on
their `rustango_users` row.

## Providers

Built-in presets: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. For anything else, use `provider = "oidc"` with an
`issuer_url` — rustango runs OpenID Connect discovery to find the
endpoints. (Sign in with Apple isn't a preset; it needs id_token/JWKS
verification.)

## Security notes

- **Verified email only** — unverified IdP emails are rejected.
- **No auto-provisioning** — an unknown email can't get in; create the
  admin user (and set its `email`) first.
- **Secrets are references**, resolved at runtime; the operator console
  masks `sso_secret_ref` like it masks `database_url`.
- The flow cookie is short-lived (10 min), `HttpOnly`, `SameSite=Lax`,
  and `Secure` on HTTPS; the handshake carries PKCE + a signed `state`.
- SSO sessions are the ordinary admin session — rotating or deactivating
  the linked user invalidates them through the existing live gate.
- Trust model is `/userinfo` over TLS (the id_token isn't independently
  verified); front the admin with HTTPS.

## See also

- [Security guide](security.md) · [Authentication](auth-flows.md)
