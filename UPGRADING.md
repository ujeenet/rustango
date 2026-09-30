# Upgrading rustango

For application developers moving an app between rustango versions.
Maintainers cutting a release want [RELEASING.md](RELEASING.md) instead.

rustango is `0.x`. Every minor bump is allowed to break something, and
several have. What follows is a method that survives that, and then the
per-version notes.

The method came out of a real 0.52.1 → 0.57.6 upgrade of a production
app. That upgrade needed **one line** of application code — not because
the range was gentle (it carries six breaking changes) but because none
of them touched API that app used. The work was establishing that, and
the establishing is the reusable part.

---

## The flow

1. **Find out whether the target is published.**

   ```bash
   curl -s https://crates.io/api/v1/crates/rustango/versions \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["versions"][0]["num"])'
   ```

   If the version is absent it is a git pin, and you need a commit:

   ```bash
   git -C …/rustango rev-parse origin/release/vX.Y.Z
   ```

   Pin a **rev**, not a branch. A release branch can be force-pushed
   while its PR is open, and a `branch = …` dependency would silently
   rebuild against different code. Every entry for the package —
   including `[dev-dependencies]`, which is where `testkit` usually goes
   — carries the same rev; cargo requires one source per package.

   When the version is published, both entries go back to
   `version = "X.Y.Z"` and the `git`/`rev` keys come out.

2. **Check your feature list still exists.** A renamed or removed
   feature fails to resolve with a message that never names the version
   boundary, so check before you build: pull
   `crates/rustango/Cargo.toml` at the target ref and diff the
   `[features]` keys against the ones you ask for.

3. **Read [CHANGELOG.md](CHANGELOG.md) across the whole range**, not
   just the target. Do not grep for "breaking" alone — the phrasing that
   matters is also "you must", "no longer", and "can stop a working app
   booting".

4. **For each breaking change, grep your tree for the API it names, and
   write the verdict down.** "Not used — 0 references" is a finding and
   deserves recording; next time you will not have to re-derive it.

5. **Build, clippy, test — in that order**, because the first two are
   fast. Run the suite with the feature set production runs, not the
   default one. `--no-default-features --features sqlite` and
   `--all-features` can disagree.

6. **Boot it.** A framework upgrade can compile and still refuse to
   start — the session-secret rule below does exactly that. Run
   `migrate`, then `runserver`, and *read the log* rather than only the
   exit code. Warnings here are the point.

7. **Look for a generated system migration.** `migrate` writes one when
   the framework's own tables change. It is untracked until you commit
   it, nothing prompts you, and it has to reach production. See below.

8. **Check the production environment against the new rules _before_
   deploying**, not after.

---

## Things that bite regardless of version

### The session secret can stop a running app booting

Every reader of `RUSTANGO_SESSION_SECRET` goes through
`SessionSecret::from_b64`, which requires **valid base64 decoding to ≥32
bytes**. A passphrase or a hex string used to sign fine and now fails at
startup. Note that a 32-*character* base64 string is only 24 bytes.

Check the target box without printing the secret:

```bash
ssh <host> 'systemctl show <unit> -p Environment' \
  | tr ' ' '\n' | grep -o 'RUSTANGO_SESSION_SECRET=.*' | cut -d= -f2- \
  | python3 -c 'import base64,sys; v=sys.stdin.read().strip().strip("\"");
raw=base64.b64decode(v, validate=True); print(len(v), "chars ->", len(raw), "bytes")'
```

You want `44 chars -> 32 bytes`. If it raises, or the count is under 32,
regenerate with `openssl rand -base64 32` **before** deploying —
rotating the secret signs everyone out, so do that deliberately rather
than discovering it as a boot failure.

`SessionSecret::from_bytes` enforces the same floor and **panics** below
it. That only affects apps constructing a secret from a custom source
(Vault, KMS, a secrets manager); apps that only *consume*
`ctx.session_secret` are unaffected.

### A generated system migration has to reach production

`migrate` generates and applies a **system** chain in
`system/migrations/` (ledger `__rustango_system_migrations__`) owning
every `rustango_*` table. When a release adds framework tables, the
first `migrate` on the new version emits a new file there.

Two consequences:

1. **Commit the generated file.** Otherwise the repo and the running
   registry disagree about what the chain is.
2. **Make sure `system/` is in your deploy.** A deploy that rsyncs
   `templates/ migrations/ config/ locales/ assets/` does *not* carry
   `system/`. Either add it — deterministic, and production applies the
   same file development did — or let `migrate` regenerate it on the
   box. Regeneration works, but then production writes its own copy of a
   file the repo also has, and the two agree only for as long as
   generation stays deterministic.

Your tenant-scoped chain under `migrations/` is unaffected by framework
upgrades.

### Open a pool after applying settings, not before

0.57.x warns when a pool is constructed before `[database]` settings are
applied, because such a pool silently runs on environment defaults:

```
1 database pool(s) were opened before settings were applied and are running on
environment defaults — move `.with_settings(…)` ahead of any pool construction
```

If a pool is built early — a translation-overrides pool, a health probe
— install the tuning before it:

```rust
if let Ok(s) = rustango::config::Settings::load_from_env() {
    let _ = rustango::sql::configure_pools(s.database.pool_tuning());
}
```

`configure_pools` is first-call-wins, so the builder's own later call is
a no-op for tuning and everything else `with_settings` does is
untouched.

---

## Unreleased

### `ApiKeyError` and `HasherError` gain `Busy`

Both are now `#[non_exhaustive]`; add a `_` arm. From async code use
`api_keys::{generate_key,hash_secret,verify_key}_async` and `PasswordHasherChain::{hash,verify}_async`.

### HMAC signing takes the host

`sign_request` and `sign_now` take a `host` argument after `method`, and every signature
changes. Services sharing a key, or behind a proxy that rewrites `Host`, set
`HmacAuthLayer::host`; it panics on an empty or invalid host.

## 0.59.6

### Outbound calls ignore `HTTPS_PROXY`

Webhook deliveries with `allow_private_targets(true)` no longer read `HTTP(S)_PROXY`,
like every other checked call. Set `RUSTANGO_OUTBOUND_PROXY` instead.

### Two tenant contexts on one request

One is used for everything, picked by a fixed type order, not mount order: `TenantContext`
Postgres, SQLite, MySQL, then `DatabaseTenantContext` in the same order. An extractor for
another backend now gets `MissingContext` instead of resolving from its own context.

### `HmacAuthLayer::nonce_store` uses `Cache::add`

A custom cache used as the nonce store should override `add` atomically; the default
is still `exists` then `set`.

## 0.59.5

### `Cache::stores_nothing`

New provided method, `true` only on `NullCache`. A wrapper cache should forward it,
like `is_process_local`, or a lockout behind it is not flagged.

### Wrong enrollment codes count toward the admin lockout

Failed TOTP confirms on `/account/totp` add to the same per-user lock as failed logins.

### FileCache keeps lock files in its directory

`FileCache` now creates up to 256 `.lock-XX` files next to its entries. `clear`
leaves them; don't count directory files as entries. A write that waits over 5 s
for a lock now fails. Where the filesystem has no file locks, writes run unlocked
after one `rustango::cache` warning.

### OAuth2 responses are capped at 1 MiB

An IdP discovery, token or userinfo body over 1 MiB is now an error.

### Cloud-metadata addresses are always refused

SSO and Slack calls refuse cloud-metadata addresses even when `RUSTANGO_OUTBOUND_ALLOW`
names the host or a CIDR that covers them.

### ViewSet create writes audit rows

On an audited model, ViewSet `POST` (single and bulk) now writes a `create` audit
row per row. A single create on such a model now runs in a transaction.
If the audit row can't be written, or the generated PK can't be read back, the
create rolls back and answers `500`. A ViewSet write failure that is not a database
rejection (bulk create included) is now a logged `500` with an opaque body, not `400`.

### `BulkAction` takes a `PkSet` (breaking)

`run(&self, pks: &PkSet, pool)` replaces `run(&self, table, &[i64], pool)`. Build
keys with `PkSet::new(M::SCHEMA, ids)` or `PkSet::parse(M::SCHEMA, raw)`; a key
of the wrong type is `BulkActionError::InvalidPk` (new variant). `restore_selected`
now counts only deleted rows on every model.
More than `PkSet::MAX_KEYS` (10 000) keys is also `InvalidPk`; split larger selections.

### Unauthenticated MCP routers answer `GET` with `405`

`mcp::router` and `mcp::tenant_router` have no SSE stream now; it was always a `500`.
Use an authed router for notifications.

### `AccessLogLayer::trust_proxy_headers` is now `use_real_ip`

Rename the field and the setter call. The old setter still works but warns.

## 0.59.4

### One access-log line per operator-console request

Behind `server::Builder` with observability, console requests log once, from your
configured layer; `next` is now redacted in all its lines and spans.

### `pluck_pairs` takes `FlatScalar` (breaking)

`K` and `V` must be flat scalars, as for `pluck`: `i8`–`i64`, floats, `bool`,
`String`, `Vec<u8>`, `Uuid`, `serde_json::Value`, `sqlx::types::Json<T>`, chrono
types, `u8`–`u64` without `postgres` and `Decimal` without `sqlite`. Use
`Option<T>` for a nullable column; a bare `T` now errors on NULL on SQLite too.

### Schema-driven writes on audited models are audited

ViewSet, template-view, `soft_delete` and `bulk_actions` writes on an audited
model now lock the rows and write audit rows in one transaction. Soft delete
and restore are recorded as `soft_delete` and `restore`, as on the typed path.
`soft_delete::restore` now returns 0 for a row that is not deleted, and the
audited `restore_selected` counts only deleted rows.

### Custom admin actions run object-permission hooks

**Breaking:** a `register_action` action now gets `403` when your `change`
hook, or a hook registered under the action's name, refuses any selected row.

## 0.59.3

### Flat projections take `FlatScalar` (breaking)

`pluck::<U>`, `pks::<U>`, `value::<U>` and `values_list_flat(..).fetch::<U>`
accept built-in scalars (integers, floats, `bool`, `String`, `Vec<u8>`,
`Uuid`, JSON, chrono types) and `Option` of them. Pluck the inner type
and wrap it for a newtype; use `Option<T>` for a nullable column, which
now errors into a bare `T` on SQLite too.

### Warnings for per-process login state

The first login logs a warning, and `check --deploy` adds a note, while
account lockout uses an in-memory or file cache; on more than one replica
install a shared cache with `account_lockout::configure_shared(Lockout::new(cache))`. For JWT logout,
set `auth_routes::Config::jti_store`. A custom `Cache` that keeps data in
process memory should override `is_process_local` to return `true`.

### Pruning audited models

`prune_all` on an audited model now reads and locks the rows and writes one
audit row each, in one transaction; a model without a primary key errors.

### Admin bulk actions run object-permission hooks

A bulk `delete_selected` / `restore_selected` that includes a row your
`register_admin_object_permission!` hook refuses now gets `403` and writes nothing.

### Idempotency: concurrent retries get 409

A request whose `Idempotency-Key` is still running gets `409` with
`Retry-After: 1`; clients should retry. Handlers that run over 60 s need
`IdempotencyLayer::lock_ttl`. A broken response stream now answers `500`.
The marker is renewed every `lock_ttl / 2`, so a long handler keeps its key.
## 0.59.2

### Feature graph (#1739)

`config` now enables `signals`. The request middleware (`cors`, `body_limit`, `security_headers`, …) now comes with `manage` or `admin`.
## 0.59.1

### Tenant routers for a non-default backend

With several backends compiled in, the default `Tenant` is Postgres. Mount
the `*_for::<sqlx::Sqlite>` (or `MySql`) variant for a SQLite or MySQL
`TenantContext`: `mcp::tenant_router_authed_for`,
`mcp::secure_tenant_router_from_settings_for`,
`member_auth::member_sso_router_for`.

## 0.59.0

### Bulk writes on audited models write audit rows

On audited models the bulk shortcuts now lock and read the affected rows
first and write one audit row each (#1747); expect one extra SELECT per
500 rows. `audit::emit_many` now takes an `Acquire` (`&PgPool`,
`&mut PgConnection`) instead of any `Executor`, so it can split large batches.
Audited non-`Auto` `bulk_insert_on` now takes `&mut PgConnection`, like
the other audited `_on` methods. On audited models these now return
`ExecError::AuditUnsupported`: `bulk_upsert_pool`,
`bulk_insert_or_ignore_pool`, `QuerySet::delete_on` / `execute_on`, and
a bulk update that sets the primary key. `ExecError` gains that variant.

### Outbound calls refuse private addresses

Slack `webhook_callback` and OAuth2/OIDC calls now refuse loopback,
private and metadata targets. For an IdP on your own network list it in
`RUSTANGO_OUTBOUND_ALLOW=10.0.5.0/24,idp.internal` (hosts and CIDRs).
Webhook delivery ignores that list; use `allow_private_targets(true)`.

`OAuth2Provider::http` is removed: set a root CA or mTLS identity with
`with_client_config(|b| ...)` or `from_discovery_with`. Build providers
with `new` or a preset; struct literals no longer compile. These calls
connect directly and cannot use an egress proxy.

### Templates escape whatever their name

Templates built with `html_tera*` and `EmailRenderer` HTML bodies now
escape `.tera`, `.j2` and suffix-less templates too; output that relied on
raw values needs `| safe`. `EmailRenderer::tera_mut()` is replaced by
`configure(|tera| ...)`, which changes both engines; `tera()` returns the
HTML engine.

### TOTP re-enroll asks for a current code

`POST /account/totp` with `reset=1` now needs `totp_code` from the current device (#1776).
Custom `totp_enroll.html` overrides must add that field to the re-enroll form.

### An admin TOTP re-enroll keeps the old device until confirmed

`rustango_admin_totp` gains a nullable `pending_secret_base32` column
(#1756); `totp_store::ensure_table` adds it. `AdminTotp` literals need
the new field. `start_enrollment` on a confirmed device no longer drops it;
`confirm` promotes the pending secret, and a successful `redeem_code` clears it.

## 0.58.1

### Forwarded IPs need `RealIpLayer::trust_proxies`

The ViewSet throttle, auth signals and `AccessLogLayer::trust_proxy_headers`
now read only `TrustedRealIp` (else the socket). Behind a proxy, mount
`RealIpLayer` with `.trust_proxies([...])`, or every client shares one
ViewSet bucket. With `server::Builder`, pass it to `.real_ip(layer)`.
Replace `meta_from_headers(&h, p)` with
`meta_from_parts(&extensions, &h, p)`; it needs the `admin` feature, and
without it you build `AuthRequestMeta` yourself.

### `verify_raw_agent_credential` returns a `Result`

It is now `Result<Option<McpAgent>, AgentError>`: `Ok(None)` is a
refused key, `Err(AgentError::Tenancy(TenancyError::Busy))` means 503.

### SQLite NULLs are `null` in JSON and `values_dict`

Code that read `0` / `false` / `""` for a NULL SQLite cell now gets `null` / `SqlValue::Null`.

### Admin password widget and derived-field hooks

A `formfield_overrides = "col: password"` column is now never echoed: forms
render it empty, list and detail show only "set", and an empty edit keeps the
stored value. `admin::derived_fields::DeriveFn` is now async and fallible:
return `Box::pin(async move { …; Ok(()) })`; an `Err` is shown on the form.

### Re-save SSO provider secrets created in the admin

Before this release the admin stored an SSO provider's `client_secret` in
plaintext, which no longer decrypts. Re-save each admin-created provider's
secret after upgrading.

### MySQL cache keys compare exactly

`DatabaseCache` now creates `cache_key` as `VARBINARY(255)` on MySQL (#1757).
`ensure_table` does not change an existing table; run once per cache table:
`ALTER TABLE rustango_cache MODIFY cache_key VARBINARY(255) NOT NULL;`

## 0.58.0

### Bare admin logout needs a CSRF token

A custom form posting to the bare admin `/logout` must send `_csrf`
(or `X-CSRF-Token`); without it the POST gets 403.

### ViewSet and template views hide scoped-out rows

A model with a `global_scope` served through `ViewSet` or the template
views now hides the scoped-out rows there too, and a PK request for one
is a 404 (#1746). An endpoint that must reach them should use
`Model::objects().without_global_scopes()` in its own handler. A
ViewSet PUT/PATCH that moves its row out of a scope or filter backend
answers `204 No Content`; a create whose row lands outside answers
`201` with no body (`null` at that index in a bulk create).

### SSO signs in by link, not by email

Existing SSO users are refused until they are linked. Run
`makemigrations` + `migrate`: it adds `allow_email_link` to
`rustango_sso_providers` / `rustango_shared_sso_providers` and creates
`rustango_sso_links` (until then email linking reads as off and SSO is
refused). Then either turn on `allow_email_link` (a normal user is linked
on the next login; for a shared provider it applies to every tenant), or
have a superuser add an `SsoLink` row. Superusers, staff and every
bare-admin account need the row: `provider_source` `tenant`/`shared`/
`admin`, `provider_id` the provider row id, `issuer` `kind` or
`kind|issuer_url` without a trailing slash, `subject`, `user_id`; the
admin computes `key_sha256`. The `sso refused` log
line carries `provider_id`, `issuer` and `subject`. Only superusers can
now add, change or delete `SsoProvider` and `SsoLink` rows in the admin.
A read-only operator console can no longer change shared providers.
(Pre-release soak databases built from an earlier 0.58.0 draft have a
`subject_sha256` column instead of `key_sha256`: drop and re-migrate
`rustango_sso_links`. Released versions never had it.)
`find_or_provision_member(pool, email, profile, auto)` is now
`(pool, &ProviderKey, allow_email_link, profile, auto)` and returns
`MemberSignIn`: map `NotLinked` (an existing account, not linkable by
email) apart from `NoAccount`, or every existing member looks "closed".

### Bounded update/delete; `atomic()` hands out a lockable `AtomicTx`

`update()` / `delete()` now honour `limit`, `offset` and `order_by`
(#1666); a queryset that relied on them being ignored now touches fewer
rows. With a composite or missing PK, or after `union()`, a bounded one
returns `QueryError::BoundedDmlUnsupported`. The `atomic` closure now
gets `&AtomicTx`, not `&mut PoolTx`: write `insert_tx(&mut *tx.lock().await?, &q)`.
A nested `atomic(&pool, …)` on the same pool is now a savepoint on the
outer transaction, and its `on_commit` callbacks wait for the outermost
commit. Drop the `TxGuard` before nesting, or get `ExecError::NestedAtomic`.

Nested writes that used to survive an outer rollback (an audit row, say)
are now rolled back with it, silently. For an independent commit, use a
different pool or `tokio::spawn`. Nesting is per pool object: pass the
request's pool down instead of looking it up again. On MySQL before
8.0.21 and MariaDB before 11.1 a bounded update/delete may scan the whole
table. On MySQL, DDL / `TRUNCATE` / `LOCK TABLES` inside `atomic` commit
implicitly: `atomic` returns `ExecError::AtomicEndedEarly` with writes
already committed, so a retry can write twice. On MySQL and SQLite a
failed statement undoes only itself; if the closure ignores it, the rest
commits (PG aborts the whole transaction).

### Admin TOTP codes are single use

`rustango_admin_totp` gains a nullable `last_used_step` column (#1672).
`totp_store::ensure_table`, or the first code accepted after the upgrade,
adds it to an existing table. `AdminTotp` literals need the new field. A
code that already signed in is refused, so users wait for the next one.
`Lockout::counter_ttl` is now a fixed window from the first failure. The
lockout cache keys changed, so failure counts in progress at the
upgrade start again from zero; active locks are kept.

### Page cache keys include the tenant

Page cache keys now include the tenant (#1674), so cached pages miss
once after upgrade.

### The page cache stops caching outside the tenancy layer

Under `tenancy`, a `CachePageLayer` that cannot see the tenant context
no longer caches. Mount it on a router passed to the server builder,
or add `.tenant_agnostic(true)` for routes that are the same for every
tenant. A CDN in front must vary on the tenant header itself.

### Long database cache keys change stored form

`DatabaseCache` keys over 255 bytes, or ending in `#` plus 64 hex, are
now stored hashed, so those entries miss once. Run `cache.clear()`
after upgrading to drop the old rows.

### The trusted client IP is the rightmost untrusted hop

Behind `trust_proxies`, `TrustedRealIp` and `RealIp` are now the
rightmost hop that is not a trusted proxy (#1673). List every proxy hop
(CDN egress, load balancer, nginx) in `trust_proxies`, or the client IP
will be one of your proxies. If your proxy sets `X-Real-IP`,
`CF-Connecting-IP` or `Forwarded` instead of appending to XFF, name
that strategy; `Auto` behind a trusted proxy reads only XFF.
`BodyLimitLayer` now also limits bodies without `Content-Length`; over
the limit a body extractor answers axum's plain 413.

### Model shortcuts respect global scopes

`Model::sum/avg/min/max/destroy/delete_where` now apply global scopes
(#1675); to act on every row use `Model::objects().without_global_scopes()`.
`delete_where` type-checks its value like `update_where`
(`QueryError::TypeMismatch`). `audit::insert_one_with_audit` takes
`(pool, &query, &mut model, |m| entry)` and sets the PK on `model`. The
hidden `Model::__aggregate_one_pool` and
`sql::model_shortcuts::aggregate_one_pool` are removed.

### Logins are rate limited; lockout keys on the username

A locked or throttled login answers `429` with `Retry-After` (#1609);
it used to re-render the form. Lockout counts the submitted username,
not the user id. Behind a reverse proxy set `RealIpLayer::trust_proxies`,
or every client shares one per-IP bucket (20 a minute). The `admin`
feature now enables `cache`. `passwords::verify_dummy_async` and
`tenancy::password::verify_dummy_async` return `Result`; handle `Busy`
as on the known-user path. New variants: `PasswordError::Busy`,
`TenancyError::Busy`, `tenancy::auth_backends::AuthError::Refused`.
`LoginThrottle::begin` takes a `&LoginScope`; `LoginThrottle::account`
is gone (use `LoginScope::TenantBasic`); call
`LoginAttempt::resolve(stored_username)` after the user lookup.
`login_throttle::configure_shared`, `account_lockout::configure_shared`
and `passwords::configure_hash_wait` now win over `[auth]` values in
either order. `login_ip_limit` and `login_global_limit` count failed
logins only; the global limit applies per scope. `ModelBackend` and
`ApiKeyBackend` refuse without a `TenantSlug`.

### Sessions carry a password fingerprint

Bare-admin, tenant, member and operator sessions sign out once after
the upgrade (#1338). `TenantSessionPayload::new`,
`tenancy::session::SessionPayload::new` and `MemberSessionPayload::new`
take a `PasswordFingerprint`
(`PasswordFingerprint::of(&secret, &user.password_hash)`);
`HandoffPayload::new` and `TenantSessionPayload::impersonation` take the
operator's. `member_auth::mint_cookie` takes `&User` instead of `uid`
and `password_hash`. `TestClient::force_login_tenant_user` and
`force_login_operator` take `&User` / `&Operator`. `SessionPayload` is
no longer `Copy`.

### Idempotency keys stored before the upgrade are not replayed

The key format changed (#1668). Add your auth layer after
`.idempotency(..)` so the caller is resolved first. A keyed request
with a body over `body_cap` (4 MiB by default) now gets `413`, with or
without `Content-Length`; raise `body_cap` for large uploads.

### `template_views` POSTs need the CSRF token; `urlize` escapes

`template_views` POST routes return 403 without a token (#1669): put
`{{ csrf_input | safe }}` in every form. The feature now enables `csrf`.
`urlize` escapes its input, so stop passing it pre-escaped text or `&`
shows as `&amp;amp;`.

### Webhooks to private addresses are dead-lettered

Receivers on localhost, private or link-local addresses (tests,
intranet) now fail unless the subscription calls
`.allow_private_targets(true)` (#1670). Delivery ignores
`HTTP(S)_PROXY`. `WebhookEvent` gained a public field,
`allow_private_targets`; a hand-built `WebhookEvent { .. }` must set it.

### Admin inline helpers are removed

`admin::inlines::apply_post` and `apply_post_generic` wrote child rows
with no permission check (#1667). The admin's parent update applies
inlines itself; there is no public replacement.

### Check schema-mode tenants for FKs into `public`

Tenants created before #1645 may have a framework FK into `public`. This
lists them (your own cross-schema FKs are left out):

```sql
SELECT n.nspname, s.relname, c.conname
FROM pg_constraint c
JOIN pg_class s ON s.oid = c.conrelid
JOIN pg_namespace n ON n.oid = s.relnamespace
JOIN pg_class t ON t.oid = c.confrelid
JOIN pg_namespace tn ON tn.oid = t.relnamespace
WHERE c.contype = 'f' AND n.nspname <> 'public' AND tn.nspname = 'public'
  AND s.relname IN ('rustango_api_keys', 'rustango_role_permissions',
                    'rustango_user_roles', 'rustango_user_permissions');
```

Drop each one (`ALTER TABLE "<schema>"."<table>" DROP CONSTRAINT "<name>"`).
For the three permission tables, `manage seed-permissions --slug <slug>`
then re-creates them inside the tenant. For `rustango_api_keys`, re-add it
by hand:

```sql
ALTER TABLE "<schema>"."rustango_api_keys"
  ADD CONSTRAINT "rustango_api_keys_user_id_fkey" FOREIGN KEY ("user_id")
  REFERENCES "<schema>"."rustango_users" ("id") ON DELETE CASCADE;
```

### Tenant admin and operator console POSTs need the CSRF token

Every POST to the tenant admin (#1713) and the operator console (#1710)
now needs the `rustango_csrf` cookie echoed as `_csrf` (form) or
`X-CSRF-Token` (header), and a same-host `Origin`. Browsers get this
from the rendered forms. A script or test that posts directly gets
`403`: GET a page first to receive the cookie, then send it back.

### Tenancy: `allowed_hosts` and the HTTPS redirect now cover `/health`

Under `Cli::tenancy()` they now wrap every route (#1700). A load
balancer that probes `/health` by IP needs that host in
`allowed_hosts`, and `/health` in `secure_redirect_exempt` if it
probes over plain HTTP. The single-tenant server already worked so.

### Login returns 403 behind a proxy that rewrites Host

The admin and tenant logins now require `Origin` to match `Host`
(#1695); `csrf_trusted_origins` does not apply. Forward the original
`Host` from the proxy. A custom handler calling `verify_form_token`
gets the same check.

### `strict` headers preset: `Referrer-Policy: same-origin`

Was `no-referrer`, which makes browsers send `Origin: null` on every
POST, so forms were refused (#1695). If you need `no-referrer`, set it
per response on pages without forms.

### `shortcuts::redirect_to_login` is removed

Its arguments ran the other way round from the one that stays (#1663):

```rust
// before: shortcuts::redirect_to_login(next, "/login")
rustango::auth_decorators::redirect_to_login("/login", "next", next)
```

### Query IR structs are `#[non_exhaustive]`

`Filter`, `Assignment`, `SelectQuery`, `InsertQuery`, `BulkInsertQuery`,
`UpdateQuery`, `BulkUpdateQuery`, `DeleteQuery`, `CountQuery` and
`AggregateQuery` (#1661). A struct literal outside the crate, including
`..SelectQuery::new(m)`, now fails with `E0639`; use `X::new(..)`:

```rust
let f = Filter::new("status", Op::Eq, "draft");
let q = InsertQuery::new(Post::SCHEMA, cols, vals).returning(vec!["id"]);
let mut s = SelectQuery::new(Post::SCHEMA).where_clause(f.into());
s.limit = Some(10);
```

A destructuring pattern needs `..` (`let Filter { column, .. } = f`), or
it fails with `E0638`. Reading and assigning fields still works. Code
that only uses the `QuerySet` API or `#[derive(Model)]` is unaffected.
`OrderClause`, `Join` and the other clause structs are not changed yet.

### Schema structs are `#[non_exhaustive]`

`FieldSchema`, `ModelSchema`, `AdminConfig`, `IndexSchema`,
`GlobalScope`, `Fieldset`, `PrepopulatedField`, the relation and
constraint structs, and `Relation::Fk` / `Relation::O2O` (#1661). A hand-built schema now fails with `E0639`. Start from `new`
(or `AdminConfig::DEFAULT`) and assign fields:

```rust
const ID: FieldSchema = {
    let mut f = FieldSchema::new("id", "id", FieldType::I64);
    f.primary_key = true;
    f
};
let rel = Relation::fk("user", "id");
```

A pattern on a variant needs `..` (`Relation::Fk { to, on, .. }`), or
it fails with `E0638`.
`SqlError` and `ExecError` are also `#[non_exhaustive]`; add a `_ =>`
arm. `#[derive(Model)]` users are unaffected.

### `migrate` refuses a callback migration without `"atomic": false`

Any migration file with a `{"callback": …}` op now needs
`"atomic": false`, including files already applied (#1626). Add it;
the ledger stores only names, so editing an applied file is safe.
Embedded migrations need a rebuild.

`atomic: false` means a failed callback does not roll back the schema
op before it. To keep that, split the file: the schema op in one
migration, the callback alone in the next.

### `rustango::core` enums are `#[non_exhaustive]`

`SqlValue`, `FieldType`, `Op`, `WhereExpr`, `Expr`, `Relation`,
`OnDeleteAction`, `QueryError` and the other enums in `rustango::core`
(#1661). A match without a `_ =>` arm now fails with
`error[E0004]: non-exhaustive patterns`; add the arm. `Weight` and
`NullsOrder` stay exhaustive.
### Every framework JSON error is now an `ApiError` body

Only affects clients that parse error bodies (#1193). The shape is

```json
{"error": "not_found", "message": "not found", "status": 404}
```

`details` appears only when there is something in it.

| Was | Now |
|---|---|
| ViewSet `{"error": "<sentence>"}` | sentence in `message`; `error` is a code |
| Serializer `400` `{"title": [...]}` | **`422`**, `details.title`, `error: "validation_failed"` |
| Admin `{"error": "form", "detail": …}` | `400` `bad_request`, reason in `message` |
| Tenant / `Principal` rejections, plain text | JSON, same shape |
| `auth_routes` / `require_bearer` handler errors, plain text | JSON, same shape (#1684); axum's own body-parse rejections are unchanged |
| `limit_bytes`, `retry_after`, admin `table` / `pk` | under `details` |
| 5xx carrying the driver message | generic `message` unless `RUSTANGO_DISCLOSE_ERRORS`; cause logged at `rustango::error` |
| ViewSet create/update constraint `400` with driver text | `400`, generic `message` |

A `MaintenanceLayer` with a custom `.body(…)` is unchanged.

### `jwt_router` is gone: build one `JwtAuth`

The JWT config no longer lives in a process global (#1190).
`jwt_router(cfg)` becomes `JwtAuth::new(cfg).router()`:

```rust
let auth = JwtAuth::new(Config::default());
api.layer(middleware::from_fn_with_state(auth.clone(), require_bearer))
    .merge(auth.router())
```

`auth_routes::verify_for_tenant(token, slug)` is now
`auth.verify_for_tenant(token, slug)`. Use the **same** `JwtAuth` for the
router and the middleware, or a logout will not revoke for the middleware.

### `MigrateError` is now `#[non_exhaustive]`

Only affects code that **matches exhaustively** on it. Add a `_ =>` arm:

```rust
match err {
    MigrateError::Driver(e) => …,
    MigrateError::Io(e) => …,
    _ => …,            // <- add this
}
```

Anything that only propagates the error, or formats it with `{}` / `?`,
is unaffected — which is most code.

The marker went on together with a new variant, `PartiallyApplied`, so
that this is **one** break rather than two: every future variant is now
additive. See #1513, which wants the same treatment across the other
public error enums.

`PartiallyApplied` is raised when a migration fails on MySQL *after*
committing DDL. MySQL commits DDL immediately, so the transaction around
an `atomic: true` migration cannot undo it — the schema moves and the
ledger row is never written, and re-running then fails differently
because the work is already done. The error now names how much committed
and the way out (`manage migrate --fake <name>` once the schema
matches), which was previously folklore. A failure with no committed DDL
still surfaces as `Driver`, unchanged, because that one rolled back
cleanly and should simply be re-run.

---

## 0.57.7

> Published 2026-09-18.

The security pass. One change can break a working deployment, and it
does so at runtime rather than at build time — read the first row even
if you skip the rest.

### Breaking, and how to tell whether it reaches you

| Change | How to check |
|---|---|
| **`media_router` is deprecated and now refuses every request with `403`.** Build the router with `media_router_with(manager, authorizer)` and supply a `MediaAuthorizer`. | Grep for `media_router(`. You get a **deprecation warning, not an error** — `cargo build` still succeeds, so a noisy build carries this to production, where the symptom is every media route answering 403. This is deliberate: the constructor used to mount 16 routes that took no authentication, authorization or tenant extractor at all, so the alternative was leaving an open bucket open. |

**The shortest fix, with the `tenancy` feature on**, is the shipped
policy — reach for it before writing a trait impl:

```rust
use rustango::media::router::{media_router_with, MediaPerms};

let app = axum::Router::new()
    .nest("/media", media_router_with(manager, MediaPerms::new(pool)));
```

`MediaPerms` checks the `{table}.{action}` permission codenames the
admin already uses: `rustango_media.view` to read a row or a listing,
`rustango_media.add` for an upload ticket,
`rustango_media_collections.add` / `rustango_media_tags.add` for the two
creates, `rustango_media.change` / `.delete` for the media row. Deleting
a collection needs **both** `rustango_media_collections.delete` and
`rustango_media.change`, because that route re-parents every media row
underneath it. Superusers skip the check.

Mount it inside `require_auth` — that middleware is what injects the
`AuthenticatedUser` it reads. Without it every request is a `401`, which
is the symptom naming its own cause.

Not `optional_auth`: it compiles and then 401s anyway, because
`MediaPerms` has no anonymous path. If what you wanted was a **public
page** showing an uploaded image, this router is the wrong tool
entirely — render `manager.public_url(id).await?` from your own route.
`docs/files.md` has both delivery models.

Reading a collection's **contents** needs
`rustango_media_collections.view` *and* `rustango_media.view`, because
that route answers media rows with a presigned download URL each — it
is a media read that happens to be addressed by collection id.

Three things to know before you rely on it.

The codenames are seeded by `auto_create_permissions`, which runs during
provisioning and on migrate; an app upgrading into this can re-seed
without a migrate cycle with the `seed-permissions` manage command.

It is **table-level, not row-level**: `rustango_media.view` grants
reading *any* media row by id, so a multi-tenant deployment still scopes
rows in its own `MediaAuthorizer`. `MediaPerms` is the floor.

And **every registered disk is writable until you say otherwise**.
`disk` is caller-supplied on `POST /uploads/begin` and the
`StorageRegistry` is process-wide, so pool-per-tenant isolates the
database and *not* the object store — a bare `rustango_media.add` grant
mints a presigned `PUT` into any bucket the process knows about. A
codename cannot express "this disk", so the allow-list is a builder:

```rust
MediaPerms::new(pool).allow_disks(["user-uploads"])
```

Set it on any deployment with more than one disk. Prefixes *within* a
disk are still not expressible — for "your own prefix on a shared
bucket", implement `MediaAuthorizer`, which is handed `key_prefix`.

Write the trait impl when you need per-row decisions:

```rust
// before (0.57.6) — served anyone who could reach it
let app = axum::Router::new()
    .nest("/media", media_router(manager));

// after (0.57.7)
use rustango::media::router::{
    media_router_with, MediaAction, MediaAuthorizer, MediaDecision, MediaTarget,
};

struct MyPolicy;

#[rustango::media::async_trait]
impl MediaAuthorizer for MyPolicy {
    async fn authorize(
        &self,
        parts: &axum::http::request::Parts,
        action: MediaAction,
    ) -> MediaDecision {
        // No identity at all is 401, not 403.
        let Some(user) = current_user(parts) else {
            return MediaDecision::Unauthenticated;
        };
        let allowed = match action {
            MediaAction::Read(MediaTarget::Media(id)) => user.may_read_media(id).await,
            // Listings enumerate the whole library; NewUpload mints a
            // presigned PUT for a caller-chosen disk and key prefix.
            // Both are explicit decisions, not defaults.
            MediaAction::Read(MediaTarget::Listing) => user.may_browse_library(),
            MediaAction::Add(MediaTarget::NewUpload { disk, key_prefix, .. }) => {
                user.is_trusted_uploader()
                    && disk == "user-uploads"
                    && key_prefix.starts_with(&user.prefix())
            }
            MediaAction::Add(MediaTarget::NewCollection { .. }) => user.is_editor(),
            MediaAction::Add(MediaTarget::NewTag { .. }) => user.is_editor(),
            _ => false,
        };
        allowed.into()
    }
}

let app = axum::Router::new()
    .nest("/media", media_router_with(manager, MyPolicy));
```

End on `_ => false`. `MediaAction` and `MediaTarget` are
`#[non_exhaustive]`, so a route added later reaches your policy as a
variant you have not written an arm for — and should arrive denied.

`authorize` returns `MediaDecision`, not `bool`. `false.into()` is
`Forbidden`, which is exactly the old behaviour, so a policy that
already computes a boolean only needs `.into()`. Return
`MediaDecision::Unauthenticated` where there is **no principal at all**
— the client is then told `401` so a token client refreshes rather than
treating the refusal as final. A signed-in user who lacks the permission
stays `Forbidden`.

`NewUpload` carries what the caller **asked for** — `disk`,
`key_prefix`, `collection_id`, `uploaded_by_id` — read out of the
request body before the handler runs, so the grant can be "this disk,
under your own prefix" rather than "anywhere in any bucket, attributed
to anyone". They are unvalidated caller input, not facts; a body that
does not parse arrives as empty strings and `None` rather than a `400`,
because authorization is decided before validation is. Match it with a
trailing `..` (`MediaTarget::NewUpload { disk, .. }`) — the variant
stays `#[non_exhaustive]` so more of the body can be surfaced later
without breaking your policy.

The gate buffers that one body, capped at 16 KiB; nothing legitimate
sends an upload-ticket JSON larger than that, and a request that does is
refused. No other route's body is read.

`NewCollection` and `NewTag` are empty struct variants of the same
shape, for `POST /collections` and `POST /tags`. Those two used to
arrive as the same `Add(Listing)`, so "may label things" also granted
"may create folders" — and collections nest, so it granted a foothold
under someone else's tree. `Listing` now means a read.

`DELETE /collections/{id}` arrives as
`Delete(MediaTarget::CollectionSubtree(id))`, **not**
`Delete(MediaTarget::Collection(id))` — so an arm written for the
latter does not grant it, and the route answers 403 until you add the
subtree arm. That is the intended reading: the route soft-deletes every
descendant collection and orphans the media at every level, and the
nesting is not the deleting caller's to control, since `POST
/collections` takes `parent_id` in the body. Grant it where a caller
owning the root may take the whole tree, and keep it on `_ => false`
where they may not.

The router needs the **`admin`** feature as well as `media`, and
`media_router` is **removed in 0.59.0** — the deprecation is not
open-ended.

### Not breaking, but your clients will notice

- **`DELETE /collections/{id}` now takes the whole subtree.** In 0.57.6
  it orphaned the media in that one collection and soft-deleted that one
  row; children were left pointing at a deleted parent, which made
  `collection_path` on the subtree a permanent error. Fixing that made
  the route recursive. If anything in your app deletes a collection that
  has children, its blast radius changed — check that before upgrading,
  not after.
- **`GET /collections/{id}/contents` caps at 100 rows.** It was
  unbounded. `?limit=` is clamped to `1..=1000`, so a client that used
  to receive a whole large collection in one response now receives a
  page, with nothing in the body saying there is more. Page with
  `?limit=` and `?offset=`.
- **`popular_tags` no longer counts soft-deleted media.** `GET
  /tags/popular` and `GET /tags` both serve it, so their numbers drop on
  upgrade. The new numbers are the correct ones — the old ones
  contradicted `GET /tags/{slug}/media` — but a dashboard tracking them
  will show a step change.

### Not breaking, worth knowing

- `url_codec::percent_decode_path` is new: `%XX` only, `+` left
  literal, for comparing a path segment against what a router decoded.
  `url_decode` keeps form semantics (`+` → space) and is unchanged for
  that use.
- Both decoders stopped treating a **signed** hex pair as an escape.
  `%+5` used to decode to byte `0x05`, because `u8::from_str_radix`
  accepts a leading sign. Only affects malformed input.
- `MediaManager::purge` now **fails** when the storage object cannot be
  deleted, instead of deleting the row and returning `Ok(())`. If your
  scheduled `purge_orphans` starts returning an error, it is reporting a
  storage failure it was previously hiding — check the `warn` lines,
  which name the disk and key. The rows it could not purge stay
  soft-deleted and are retried on the next sweep; the rest of the sweep
  still runs. A disk missing from the `StorageRegistry` is now
  `MediaError::UnknownDisk` rather than a silent skip.
- `rustango::storage::async_trait` is re-exported, so implementing the
  public `Storage` trait no longer needs `async-trait` in your own
  `Cargo.toml`. The `media::async_trait` re-export is unchanged; it sits
  behind the `admin` feature, which a crate implementing only `Storage`
  may not have on.

### `on_delete` now reaches the database — on **new** databases only

Every `#[rustango(fk = "…", on_delete = "…")]` was being discarded when
a schema snapshot was built, and system migrations render *from*
snapshots, so the clause reached no database at all. A declared
`cascade` arrived as `NO ACTION`, which does not merely fail to cascade
— it makes the parent delete a hard refusal (`ERROR 1451` on MySQL).

**What you need to know about upgrading:**

| | |
|---|---|
| A **new** database, migrated from nothing | gets the correct `ON DELETE`. Nothing to do. |
| An **existing** database | keeps the constraints it already has. `migrate` reports `nothing to migrate` and writes no file — correctly, because a changed `on_delete` is not a schema operation this release can emit. |

So `migrate` exiting `0` after the upgrade does **not** mean your
constraints were corrected. If you rely on a declared `cascade` — and
you may not have noticed you did, because it has never worked — the
constraint has to be rewritten by hand:

```sql
-- PostgreSQL / MySQL. Check first:
--   PG:    SELECT conname, confdeltype FROM pg_constraint WHERE contype='f';
--          'a' = NO ACTION, 'c' = CASCADE, 'n' = SET NULL
--   MySQL: SELECT constraint_name, delete_rule
--            FROM information_schema.referential_constraints;
ALTER TABLE child DROP CONSTRAINT child_parent_id_fkey;
ALTER TABLE child ADD CONSTRAINT child_parent_id_fkey
  FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE;
```

SQLite has no `ALTER TABLE … DROP CONSTRAINT`, so correcting one there
means rebuilding the table.

This affects apps that never touched media: **eleven framework foreign
keys declare `cascade`, ten of them in `tenancy`** (roles, permissions,
agent skills), and `fold_in_framework_tables` puts them in every
project's snapshot.

### `ON DELETE SET NULL` can now fail your deploy on MySQL

Because the clause is finally emitted, a model declaring
`on_delete = "set_null"` on a **non-nullable** column now produces DDL
MySQL rejects:

> **`ERROR 1830 (HY000): Column 'x' cannot be NOT NULL: needed in a
> foreign key constraint 'y' SET NULL`**

Make the column `Option<…>`, or change the action. PostgreSQL and SQLite
accept the DDL and fail at delete time instead, which is worse — so this
is the loud one.

## 0.57.6

> Published. `rustango = "0.57.6"` resolves.

### Breaking, and how to tell whether it reaches you

| Change | How to check |
|---|---|
| **The access log's field names are now OpenTelemetry conventions.** `method` → `http.request.method`, `path` → `url.path`, `status` → `http.response.status_code`, `ip` → `client.address`. `url.path` is the path alone; the query moved to `url.query`. | Grep your dashboards, alerts and collector mappings — not your code. This breaks **log consumers**, and nothing in your build will tell you. |
| **`duration_ms` is `f64` microseconds, not `u64` milliseconds.** | Elasticsearch/OpenSearch rejects a document whose field type conflicts with an established mapping. Update the mapping before the first line lands. |
| **`LoggingSettings`, `Format` and `Color` are `#[non_exhaustive]`.** | Struct literals stop compiling — **and so does `..Default::default()`**, which `#[non_exhaustive]` also forbids across crates. Build the default and assign: `let mut s = LoggingSettings::default(); s.level = …;` |
| **`Dialect` gained `drop_check_constraint_sql` / `drop_foreign_key_sql`.** | Only affects a downstream `impl Dialect`. Defaults `unimplemented!` rather than falling through to PostgreSQL's form, so you get a loud panic naming the method rather than invalid SQL. |
| **`RUSTANGO_SESSION_SECRET` must be base64 ≥32 bytes** (#1396). | See above. This one can stop a booting app. |
| Admin mutations require a CSRF token. | Only if you mount the rustango admin *and* have a `Method::POST` admin view or custom admin template. |
| `JwtBackend` needs `with_jti_store` for logout revocation. | Only if you use `jwt_router`. Session-signed API auth is unaffected. |
| `allow_any_origin()` + credentials no longer reflects the origin. | Only if you build CORS through that helper. |
| `db_dump_cmd` no longer takes a writer. | Rare; grep for it. |

### Behaviour changes that cost nothing to adopt

- **The request span and `X-Request-Id` now mount by default** on every
  serving path. Expect a new response header and span context on log
  lines. `[logging] access_log = false` turns off the *log line* only —
  the span and request id stay, because a service logging at the edge
  still wants trace context.
- **Query-string credentials are redacted** from both the access-log
  event and the span, using your configured `[audit]
  redact_query_params` plus defaults that now include the OAuth2/OIDC
  names (`code`, `client_secret`, `id_token`, `code_verifier`, `state`).
- **`confirm_password_reset_pool` stamps `password_changed_at`** and
  applies the real password policy. Only matters if you call it.
- **Migration DDL for `RENAME TO` / `RENAME COLUMN` / `DROP CONSTRAINT`
  is centralised on the dialect.** `quote_ident`'s default is unchanged,
  so PostgreSQL and SQLite output is byte-identical; MySQL is fixed
  (it previously received `"`-quoted identifiers and answered
  `ERROR 1064`).

### Worth adopting

`testkit::matrix` — `tri_dialect_test!`, `fresh_table::<M>`,
`Backend::pool()`, `by_dialect!`. It costs one line
(`rustango = { features = ["testkit"] }` as a dev-dependency) and
replaces hand-rolled per-dialect harnesses.

It is worth the change specifically if your suite has a test that gates
on a bespoke `MY_APP_TEST_PG_URL`-style variable and prints `skip:` when
it is unset. That shape reports `ok` while testing nothing.
`Backend::pool()` owns the policy in one place: **unset skips,
set-but-unreachable panics, set-to-the-wrong-engine panics.**

---

## Recording your own upgrade

The table in the version section is the valuable artefact, and it is
cheap to produce while you are already looking. Write the verdict for
every breaking change, including the ones that did not apply — "not
used, 0 references" is a finding. The next person upgrading past that
version starts from your table instead of re-deriving it.
