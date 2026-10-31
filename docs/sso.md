# SSO (OpenID Connect / social login)

Sign in with an external identity provider — Google, Microsoft / Azure
AD, GitHub, GitLab, Discord, or **any OpenID Connect provider** (Okta,
Auth0, Keycloak, …) — instead of a local password.

You can configure **multiple providers**, each managed from the admin UI
as a row (no config file, no rebuild). A provider's endpoints are
auto-discovered from its OIDC issuer URL at login; social providers use
built-in presets.

SSO signs in the user **linked** to the IdP identity: one
`rustango_sso_links` row per `(provider, sub)`. The email the IdP sends is
never enough on its own. A first-time user is linked by verified email
only when the provider has **`allow_email_link`** on (default off), and
never when the account is a superuser or staff (tenant: holds any
permission; bare admin: every account). Those accounts are linked by an
admin, who adds the `SsoLink` row in the admin; the refusal log line names
the subject. (The member flow, below, can also create an account for an
unknown email; that is off by default.)

> **Source:** the admin-independent core `rustango::sso` (`SsoProvider`,
> `build_provider`, `verified_email`, `ResolvedSso`, `SsoError`), the link
> table `rustango::sso::link` (`SsoLink`, `ProviderKey`, `sign_in`), the
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
rustango = { version = "0.61", features = ["admin-sso"] }
# Member (end-user) SSO without the auto-admin:
rustango = { version = "0.61", features = ["tenancy", "sso"] }
```

`admin::sso_provider` and the historical `admin::sso::*` core paths are
now **re-export shims** over `sso::provider` / `sso::*`, so existing
`crate::admin::sso::{build_provider, ResolvedSso, …}` and
`crate::admin::sso_provider::SsoProvider` imports keep resolving
unchanged. Since 0.58 `rustango_sso_providers` has an `allow_email_link`
column and there is a `rustango_sso_links` table; `makemigrations`
emits both.

The email opt-in email linking matches is the `email` column. On the tenant
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
4. rustango verifies the flow, exchanges the code and reads `/userinfo`.
5. It looks up the link for this provider and the IdP `sub`. With no
   link, and `allow_email_link` on, a **verified** email that matches a
   non-privileged user creates the link.
6. If the linked user is active, it mints the **same signed-cookie
   session** a password login produces — so every existing gate
   (superuser / permissions, live password-change invalidation) still
   applies.
7. Otherwise the user is bounced back to the login page with a generic
   error (details go to the server log, never the browser).

The link table is a normal migrated model: in the tenant's storage for
tenant logins, and in the admin database for the bare admin. A link is
matched exactly (issuer and subject keyed by a SHA-256; the email ignores
ASCII case only),
whatever the database collation.

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
| `allow_email_link` | Link a first-time user by verified email (default off). Never links a superuser or staff account; ignored by the bare admin. |

Only a superuser can add, change or delete `SsoProvider` and `SsoLink`
rows in the admin; other staff can list them with the usual permissions.

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
  panel (`/sso-shared`, `admin-sso` feature; every operator can list,
  changes need an editable console), where *Allow email linking* toggles `allow_email_link` in place
  (the id and its links stay). The flag applies to **every** tenant.
  *Edit* (`/sso-shared/{id}/edit`) changes a provider in place (same id
  and slug; a blank secret keeps the saved one), and each row shows the
  callback URL to register. The issuer URL field shows only for `oidc`. A
  refused add or edit shows the form again (status 400) with the typed
  values and the error, never the secret.

On a tenant's login page the two sets merge, and on a slug clash the
**tenant's own provider wins** over the shared one — so a tenant can
override a shared provider for itself.

The callback URL is derived per request from the host + slug
(`https://<host><login>/sso/<slug>/callback`), so register that with the
IdP. The scheme is `https` unless a trusted proxy sends
`X-Forwarded-Proto`. A user is linked by opt-in email linking (non-privileged tenant
users), or by a superuser adding an `SsoLink` row: `provider_source`
(`tenant`, `shared`, `admin`, or `app` for a native sign-in), `provider_id` (the provider row id),
`issuer` (`kind`, or `kind|issuer_url` without a trailing slash),
`subject` and `user_id`. The admin computes
`key_sha256`. The refusal log line (`sso refused`) carries `provider_id`,
`issuer` and `subject`. Adding a row needs the admin's session auth
(`Builder::with_session_auth`, or the tenant admin's `with_session`);
without it nobody can add links.

A link created more than 5 minutes before the account it points at (an id
reused after a restore, say) logs a warning and still signs in.
A link whose user no longer exists is deleted at the next sign-in.
`rustango::sso::link::set_stale_links(StaleLinks::Drop)` deletes such a link
instead. Use it only when account `created_at` values are never imported or
backfilled.

With the `totp` feature, a bare-admin user with a confirmed TOTP device
must still enter a code after SSO. The admin shows the code form and
keeps the pending sign-in in a signed cookie for 5 minutes; the form
posts to `<login>/sso-totp`. The tenant admin has no TOTP step.

`manage check --deploy` warns about an enabled provider that would refuse
every existing user: no `SsoLink` rows, and email linking off or only
privileged users. In the bare admin that means any provider with no links.

## Member (end-user) SSO

The surfaces above sign people in to an **admin**. `tenancy::member_auth`
is the member-facing analogue: it logs an end-user into a tenant's own
user pool (`rustango_users`) and mints a **member session**, so a gym
member / SaaS customer can "Sign in with Google" without touching the
admin. It reuses the exact same `rustango::sso` core and the tenant's own
`SsoProvider` rows — only the session it mints differs, which is why it
lives behind the `sso` feature (not `admin-sso`) and needs no auto-admin.

Mount `member_sso_router` into a `rustango::server::Builder` stack (it
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

With several backends compiled in, `member_sso_router` is for the default
(Postgres) tenant type; use `member_sso_router_for::<sqlx::Sqlite>` (or `MySql`).

It mounts two per-slug routes off `login_base`:

- `GET {login_base}/sso/{slug}` — begin the handshake, redirect to the IdP.
- `GET {login_base}/sso/{slug}/callback` — complete it, find-or-provision
  the member, mint the session cookie.

A path-prefix tenant gets the same two routes under its prefix.

Differences from the admin flow:

- **Callback scheme.** The member `redirect_uri` takes its scheme from a
  trusted proxy's `X-Forwarded-Proto` (and its host from
  `X-Forwarded-Host`), else `http` on a loopback host
  (`localhost`, `*.localhost`, a loopback IP) and `https` everywhere else,
  so `localhost.example.com` gets `https`.
- **Auto-provisioning.** A provider's `signup_mode` ([Who may sign up](#who-may-sign-up)) wins; with none set, `auto_provision = true` (opt-in; the default is `false`) works as `open`: a
  verified IdP email with no matching `rustango_users` row **creates**
  one — username from the email local-part (deduped on a clash), a real
  but unusable random password hash (SSO users can't password-login) —
  and links it. An email that matches an existing account follows the
  `allow_email_link` rule above. Set it to `false` to refuse unknown
  emails. A native sign-in (an app that verified its own ID token) calls
  `find_or_provision_member` directly, with `ProviderKey::app(issuer)` as
  the key. Its `MemberSignIn` result is `Member(id)`, `NotLinked` (an
  account has the email but may not be linked by it), `NoAccount`,
  `Inactive`, `Unverified` or `Refused`.
- **Its own session cookie.** The member cookie
  (`rustango_member_session`) is **domain-separated** from the tenant /
  admin session cookies: the signed message carries a per-domain tag and
  an audience claim, so a member cookie can never validate as a
  tenant/admin cookie (or vice-versa) even though both are signed with
  `RUSTANGO_SESSION_SECRET`. It is slug-bound (a cookie minted for `acme`
  never authenticates on `globex`) and invalidated by a password rotation
  (parity with the admin session).
- **Logout.** `member_auth::logout(&pool, &user)` ends the member's
  sessions on every device and returns the `Set-Cookie` value that clears
  the cookie. On a path-prefix tenant use `logout_at`, which clears the
  cookie on the prefix path.

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

## Who may sign in

Each provider (and each shared provider in the operator console) has
sign-in rules. Every rule left empty admits anyone the IdP signs in.

| Field | Rule |
|---|---|
| `allowed_email_domains` | Only these domains: `client.org` matches only itself, `*.client.org` only its subdomains. Comma or newline separated. |
| `blocked_email_domains` | Never these domains, even if allowed above. Checked against any email, verified or not. |
| `allowed_idp_tenants` | Microsoft directory ids (`tid`). Read only from a verified id_token. |
| `allowed_hosted_domains` | Google Workspace domains (the `hd` claim). |
| `required_claims` | JSON `{"claim": ["value", ...]}`: the claim (a string or a list) must hold one of the values. `{"iss": [...]}` pins the issuer. A missing claim refuses. |
| `require_verified_email` | On by default: the allowlist accepts only an email the IdP verified. |

The admin and the operator console store lists in one canonical form and
refuse a value that does not parse. A value that still does not parse
(written past them) refuses every sign-in through the provider.

A refused sign-in goes to the server log and to the audit log
(`source = sso:<surface>`, `changes.rule` such as `domain_not_allowed`).
The browser gets one generic message; on the tenant login page it also
shows the tenant's `access_denied_message` (from `SsoTenantPolicy`),
escaped.

The member route (`member_sso_router`) resolves only the tenant's own
providers, so shared providers, and any tenant tightening of them, do not
apply there yet.

### id_token trust

Profile data comes from `/userinfo` over TLS. Claims only an id_token
carries (`tid`, `iss`, `xms_edov`) are used only after the id_token
passes: its signature against the issuer's JWKS (RS256 or ES256; the key
set is cached per its `max-age` and refetched at most once a minute for an
unknown key), `aud` holds the client id, `exp`/`nbf` with one minute of
leeway, the issuer (Microsoft: `https://login.microsoftonline.com/{tid}/v2.0`),
and the same `sub` as userinfo. Otherwise those claims are absent, and a
rule that needs them refuses. A Microsoft email counts as verified when
`xms_edov` is true, or when the provider pins `allowed_idp_tenants` and
the token's `tid` is one of them.

## Who may sign up

A provider can also create the account the first time someone signs in.
Sign-up runs only when no link and no account match the IdP's verified
email, and only after the sign-in rules above pass.

| Field | Meaning |
|---|---|
| `signup_mode` | `closed` (link only), `invite_only` (an `SsoInvite` for this email), `allowlist` (an invite for this email or its domain) or `open` (anyone the sign-in rules admit). Empty: closed on the tenant login; the member flow follows `MemberAuthConfig::auto_provision`. The bare admin never signs anyone up. |
| `default_roles` | Tenant roles a new user gets, e.g. `Viewer`. An invite with roles replaces them. |
| `role_map` | JSON `{"<group or app role>": "Editor"}` (or a list of roles). Read from the claims in `role_claims` (empty: `groups,roles`). |
| `sync_roles_on_login` | Re-apply `role_map` on every sign-in. Only roles named in `role_map` are added or removed; roles granted by hand stay. |
| `username_from` | `email_local` (default), `email`, or `preferred_username` (falls back to the email local part). |
| `username_clash` | `suffix` (default: `ann-x7k2q9`) or `refuse`. |
| `sync_profile` | Refresh the display name and email from the IdP on every sign-in (never for a superuser; an email another user has is kept). |
| `allow_email_link_staff` | With `allow_email_link`, also link a user who holds roles. A superuser is never linked by email. |

A new user is never a superuser. Roles go through the permission engine
(`UserRole`); a role name with no `Role` row is skipped and written to the
audit log (`sso_role_unknown`). The user, its link (`provisioned = true`),
its roles and the invite's use commit in one transaction.

**Invites** (`SsoInvite`, superuser-only in the tenant admin) target one
email (`ann@client.org`) or a domain (`@client.org`, or `*.client.org` for
its subdomains). They can name one provider, carry roles, expire
(`expires_at`) and limit uses (`max_uses`). A use is counted atomically:
two sign-ups racing for the last use make one account. `.../rustango_sso_invites/bulk`
adds many at once from a pasted list or a CSV column.

**Unlink and re-link.** On `SsoLink` rows, the admin actions *Unlink* and
*Re-link on next sign-in* (superuser-only). Re-link deletes the link and
leaves a one-use invite, good for 7 days, that links the same user again
the next time they sign in with their verified email, even if the IdP
gives them a new subject. It never re-links a superuser.

`check --deploy` does not report a provider whose sign-up is not closed
as refusing everyone, and warns about `signup_mode = open` with no
`allowed_email_domains`, `allowed_hosted_domains` or `allowed_idp_tenants`.

## Secret storage

`client_secret` is stored **encrypted at rest** with XChaCha20-Poly1305
(AEAD), the key derived from the **`RUSTANGO_SECRET_KEY`** environment
variable. It's decrypted in memory only at login, to authenticate to the
IdP's token endpoint. So a leaked DB dump never exposes the secret, and
each tenant keeps its own secret with no per-provider env var.

> Set `RUSTANGO_SECRET_KEY` in the deployment. It is SHA-256'd to a
> 32-byte key; use at least 32 random characters (with tenant
> provisioning mounted, `check --deploy` warns about a shorter key). Without it, saving or using a provider fails
> fast — the same posture as a missing database URL.

To rotate the key, set the new value in `RUSTANGO_SECRET_KEY` and the old
one in `RUSTANGO_SECRET_KEY_PREVIOUS`. Reads fall back to the old key. In
a multi-tenant deployment, `manage reseal-tenant-secrets --apply` then
re-seals the shared and tenant SSO secrets with the new key.

## Providers (presets)

Built-in presets: `google`, `microsoft` (Azure AD), `github`, `gitlab`,
`discord`. For anything else, use `kind = "oidc"` with an `issuer_url` —
rustango runs OpenID Connect discovery to find the endpoints, once per issuer
per hour. (Sign in
with Apple isn't a preset; it needs id_token/JWKS verification.)

## Security notes

- **Link first** — the `(provider, sub)` link decides; the email is used
  only by opt-in email linking, and only when verified.
- **No privileged email links** — superusers and staff are linked by a
  superuser only. A link made by email keeps working after the user is
  promoted; delete it if that is not wanted.
- **No auto-provisioning** for the admins — an unknown email can't get in.
- **Secrets encrypted at rest** (`RUSTANGO_SECRET_KEY`), decrypted only
  in memory at login; edit forms mask the stored secret.
- The flow cookie is short-lived (10 min), `HttpOnly`, `SameSite=Lax`,
  and `Secure` per configuration — **not** per request scheme. Precedence
  is `security.secure_cookies` (default `true` on the `manage` path, so
  it fails closed), falling back to "secure on the prod tier" from
  `RUSTANGO_ENV` when nothing is set. Nothing inspects whether the
  request actually arrived over HTTPS. The handshake carries PKCE + a
  signed `state`.
- SSO sessions are the ordinary admin session — rotating or deactivating
  the linked user invalidates them through the existing live gate.
- Trust model is `/userinfo` over TLS; id_token claims count only after
  JWKS verification (see [id_token trust](#id_token-trust)). Front the
  admin with HTTPS.

## See also

- [Security guide](security.md) · [Authentication](auth-flows.md)
