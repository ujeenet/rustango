# SSO (OpenID Connect / social login)

Sign in with an external identity provider — Google, Microsoft / Azure
AD, GitHub, GitLab, Discord, or **any OpenID Connect provider** (Okta,
Auth0, Keycloak, …) — instead of a local password.

You can configure **multiple providers**, each managed from the admin UI
as a row (no config file, no rebuild). A provider's endpoints are
auto-discovered from its OIDC issuer URL at login; social providers use
built-in presets.

SSO is **link-to-existing** for the admin: the verified email the IdP
returns must match an existing admin user. It authenticates the person;
it never creates accounts and never grants access on its own. An unknown
or unverified email is refused. (The member flow, below, may opt into
auto-provisioning.)

> **Source:** the admin-independent core `rustango::sso` (`SsoProvider`,
> `build_provider`, `verified_email`, `ResolvedSso`, `SsoError`), the
> bare-admin wiring `rustango::admin::sso`, per-tenant/console SSO
> `rustango::tenancy::sso` (`SharedSsoProvider`), and member SSO
> `rustango::tenancy::member_auth`.

## Features & who uses them

Since **0.49** the SSO core is its own feature, independent of the
auto-admin, so an end-user (member) login can build without pulling in
`crate::admin`:

| Feature | Pulls in | Gives you |
|---|---|---|
| `sso` | `oauth2`, `casts` | The admin-independent core: `rustango::sso` — the OIDC / social OAuth handshake, the DB-backed `SsoProvider` model (secret encrypted at rest via `casts`), and the member flow (`tenancy::member_auth`, with `tenancy`). |
| `admin-sso` | `admin`, `sso` | The above **plus** the bare-admin login wiring (`rustango::admin::sso`) — SSO buttons on the admin login page that mint the admin session. |

```toml
[dependencies]
# Admin login with SSO:
rustango = { version = "0.51", features = ["admin-sso"] }
# Member (end-user) SSO without the auto-admin:
rustango = { version = "0.51", features = ["tenancy", "sso"] }
```

`admin::sso_provider` and the historical `admin::sso::*` core paths are
now **re-export shims** over `sso::provider` / `sso::*`, so existing
`crate::admin::sso::{build_provider, ResolvedSso, …}` and
`crate::admin::sso_provider::SsoProvider` imports keep resolving
unchanged (the table name `rustango_sso_providers` and every field are
untouched — migrations are unaffected).

The email a user is linked on is the `email` column. On the tenant
`User` model it is gated on the **`sso`** feature (moved off `admin-sso`
in 0.49, so member-SSO-only builds still get the column); the bare
`AdminUser.email` remains behind `admin-sso`. Enabling or disabling the
feature emits an `AddColumn` / `DropColumn` migration for that column.

## How it works

1. The login page shows one **"Sign in with &lt;provider&gt;"** button per
   enabled provider.
2. Clicking one (`GET <login>/sso/<slug>`) redirects to the IdP with a
   signed, short-lived flow cookie (PKCE + CSRF `state`).
3. The IdP sends the user back to `<login>/sso/<slug>/callback`.
4. rustango verifies the flow, exchanges the code, reads `/userinfo`,
   and requires **`email_verified`**.
5. It looks up an admin user by that email. If one exists and is active,
   it mints the **same signed-cookie session** a password login
   produces, bound to that user — so every existing gate (superuser /
   permissions, live password-change invalidation) still applies.
6. No match → the user is bounced back to the login page with a generic
   error (details go to the server log, never the browser).

The client **secret is encrypted at rest** — the `client_secret` column
is an [`EncryptedString`](#secret-storage) cast, decrypted in-memory only
at login time.

## Providers are rows, managed in the admin

Each provider is a `SsoProvider` row. It shows up as an ordinary
admin model — add/edit/enable from the admin UI, no redeploy. Fields:

| Field | Meaning |
|---|---|
| `slug` | Stable route key + button id (`<login>/sso/<slug>`). Unique. |
| `label` | Button text, e.g. "Sign in with Google". |
| `kind` | A preset — `google` / `microsoft` / `github` / `gitlab` / `discord` — or `oidc` for a generic OpenID Connect provider. |
| `issuer_url` | OIDC discovery base URL (for `kind = "oidc"`); rustango fetches `{issuer}/.well-known/openid-configuration`. Unused for presets. |
| `client_id` | The OAuth client id from the IdP. |
| `client_secret` | The OAuth client secret, **encrypted at rest** (never plaintext in the DB). |
| `enabled` | Whether the button shows on the login page. |
| `sort_order` | Button ordering (ascending). |
| `scopes` | Optional space-separated scope override (default `openid email profile`). |

To add a provider: enter the `client_id` + `client_secret`, pick a
`kind` (or `oidc` + an `issuer_url`), and save. The endpoints are
discovered at login — no per-provider endpoint wiring.

## Where each surface manages providers

- **Single-tenant / standalone admin** (`crate::admin`): `SsoProvider`
  rows are a plain global table, managed from the bare admin. Requires
  `Builder::with_session_auth` (SSO mints the same session).
- **Tenant admin** (multi-tenancy): each tenant manages its **own**
  `SsoProvider` rows from its admin — granular, self-service, isolated
  per tenant.
- **Operator console** (multi-tenancy): an operator defines a
  **`SharedSsoProvider`** once and it's offered to **every** tenant
  (a company-wide Google, say). Managed from the console's *Shared SSO*
  panel.

On a tenant's login page the two sets merge, and on a slug clash the
**tenant's own provider wins** over the shared one — so a tenant can
override a shared provider for itself.

The callback URL is derived per request from the host + slug
(`https://<host><login>/sso/<slug>/callback`), so register that with the
IdP. Link a user by setting the `email` column on their
`rustango_users` (tenant) / `rustango_admin_users` (bare) row to the
address the IdP returns.

## Member (end-user) SSO

The surfaces above sign people in to an **admin**. `tenancy::member_auth`
is the member-facing analogue: it logs an end-user into a tenant's own
user pool (`rustango_users`) and mints a **member session**, so a gym
member / SaaS customer can "Sign in with Google" without touching the
admin. It reuses the exact same `rustango::sso` core and the tenant's own
`SsoProvider` rows — only the session it mints differs, which is why it
lives behind the `sso` feature (not `admin-sso`) and needs no auto-admin.

Mount `member_sso_router` into a `tenancy::server::Builder` stack (it
reads the resolved `Arc<TenantContext>` the builder injects):

```rust
use rustango::tenancy::member_auth::{member_sso_router, MemberAuthConfig};

let members = member_sso_router(MemberAuthConfig {
    login_base:     "/auth".into(),   // buttons link to /auth/sso/<slug>
    landing_url:    "/".into(),       // post-login destination (honors a same-origin ?next)
    auto_provision: true,             // create a user from a verified email on first sign-in
    session_ttl:    7 * 24 * 60 * 60, // 7 days
    ..Default::default()
});
```

It mounts two per-slug routes off `login_base`:

- `GET {login_base}/sso/{slug}` — begin the handshake, redirect to the IdP.
- `GET {login_base}/sso/{slug}/callback` — complete it, find-or-provision
  the member, mint the session cookie.

Differences from the admin flow:

- **Auto-provisioning.** With `auto_provision = true` (the default), a
  verified IdP email with no matching `rustango_users` row **creates**
  one — username from the email local-part (deduped on a clash), a real
  but unusable random password hash (SSO users can't password-login).
  Set it to `false` for admin-style link-to-existing (unknown email
  refused).
- **Its own session cookie.** The member cookie
  (`rustango_member_session`) is **domain-separated** from the tenant /
  admin session cookies: the signed message carries a per-domain tag and
  an audience claim, so a member cookie can never validate as a
  tenant/admin cookie (or vice-versa) even though both are signed with
  `RUSTANGO_SESSION_SECRET`. It is slug-bound (a cookie minted for `acme`
  never authenticates on `globex`) and invalidated by a password rotation
  (parity with the admin session).

Read the current member in a handler with the **`CurrentMember`**
extractor — the member analogue of `SessionUser`. It's infallible
(`None` for anonymous / expired / rotated-out / cross-tenant sessions),
so it composes with public routes:

```rust
use rustango::tenancy::member_auth::CurrentMember;

async fn dashboard(CurrentMember(member): CurrentMember) -> impl axum::response::IntoResponse {
    match member {
        Some(user) => format!("Hi, {}", user.username),
        None => "Please sign in".to_owned(),
    }
}
```

> **v1 scope.** Member SSO resolves providers from the tenant's own
> `SsoProvider` rows only — the registry-wide `SharedSsoProvider` merge
> and a custom `provision` hook are follow-ups.

## Secret storage

`client_secret` is stored **encrypted at rest** with XChaCha20-Poly1305
(AEAD), the key derived from the **`RUSTANGO_SECRET_KEY`** environment
variable. It's decrypted in memory only at login, to authenticate to the
IdP's token endpoint. So a leaked DB dump never exposes the secret, and
each tenant keeps its own secret with no per-provider env var.

> Set `RUSTANGO_SECRET_KEY` in the deployment (any length; it's SHA-256'd
> to a 32-byte key). Without it, saving or using a provider fails fast —
> the same posture as a missing database URL.

## Providers (presets)

Built-in presets: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. For anything else, use `kind = "oidc"` with an `issuer_url` —
rustango runs OpenID Connect discovery to find the endpoints. (Sign in
with Apple isn't a preset; it needs id_token/JWKS verification.)

## Security notes

- **Verified email only** — unverified IdP emails are rejected.
- **No auto-provisioning** — an unknown email can't get in; create the
  admin user (and set its `email`) first.
- **Secrets encrypted at rest** (`RUSTANGO_SECRET_KEY`), decrypted only
  in memory at login; edit forms mask the stored secret.
- The flow cookie is short-lived (10 min), `HttpOnly`, `SameSite=Lax`,
  and `Secure` on HTTPS; the handshake carries PKCE + a signed `state`.
- SSO sessions are the ordinary admin session — rotating or deactivating
  the linked user invalidates them through the existing live gate.
- Trust model is `/userinfo` over TLS (the id_token isn't independently
  verified); front the admin with HTTPS.

## See also

- [Security guide](security.md) · [Authentication](auth-flows.md)
