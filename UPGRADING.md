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

### Passkeys in `public` on schema-mode tenants

`migrate-tenants` now creates `rustango_webauthn_credentials` in each tenant schema, which hides `public.rustango_webauthn_credentials`; passkeys stored there stop working (#2518). Rows are not copied, since user ids overlap across tenants. Before upgrading, move each tenant's rows by hand, e.g. `INSERT INTO "<schema>".rustango_webauthn_credentials SELECT * FROM public.rustango_webauthn_credentials WHERE user_id IN (<that tenant's user ids>)`, then delete them from `public`. `migrate-tenants` warns, naming the tenant, when it adds the table to a tenant that already had users while `public` holds rows.

### Impersonation links use https by default

Unset `RUSTANGO_TENANT_SCHEME` now means https when cookies are `Secure` (prod tier, or `[security] secure_cookies`), except on loopback hosts (#2425). A plain-http deploy on a real host must set `RUSTANGO_TENANT_SCHEME=http`.

### Password changes refuse a stale row

A password change or reset whose user changed password meanwhile now fails with "changed meanwhile; try again" instead of overwriting it (#2467).

### Auth links take a `LinkScope`

`PasswordReset`, `EmailVerification` and `MagicLink` `issue`/`verify`/`verify_single_use` take a `&LinkScope` as their first argument, and every `confirm_password_reset_*` takes a `LinkTarget` in place of the pool (#2472). In a tenant app pass the request's `&Tenant` to both (`LinkScope::from(&t)`, and `&t` to confirm); `LinkScope::audience` / `LinkTarget::audience(&pool, ..)` are for single-database apps. A link for another scope fails with the new `AuthFlowError::WrongScope`, and `AuthFlowError` is now `#[non_exhaustive]`: add a `_` arm to exhaustive matches. Links issued before the upgrade are refused.

### Admin: passkeys and registry tables

No admin serves `rustango_webauthn_credentials` any more; with `passkey`, `migrate` creates it on the single database or on each tenant, never the registry (#2364). With `tenancy` compiled in, a plain `admin::Builder` hides `Org`, `Operator` and the other registry-only tables; an admin you mount on a tenancy registry needs `.registry_mode()` to list them, and lists only registry tables (#2365).

### SSO: missing provider table

A missing `rustango_sso_providers` table now reads as no providers, so bare-admin `resolve_by_slug` returns `Ok(None)` instead of an error (#2366).

### Migrations re-add some FKs

On MySQL, dropping the index a composite FK uses now drops and re-adds that FK (#2326). On PG and MySQL, renaming an FK or M2M junction column re-adds its FK under the new column's name, which re-checks every row (#2307). FKs on columns renamed by 0.60.4 or older keep their old names: `migrate` finds them by column, but `sqlmigrate` prints the new name.

## 0.60.4

### Tenant admin: TOTP and translations

Run `migrate` once: it creates `rustango_translations` on the single database or the registry; a tenancy registry also gets `rustango_audit_log` and, with `totp`, `rustango_admin_totp`. `Translation` is now `scope = "registry"`: edit translations from a non-tenant admin, as a tenant admin no longer serves the editor or `export.json`. No admin lists `rustango_admin_totp` any more (#2360).

### `check --deploy`: SSO providers that refuse every user

New `[sso]` warnings name providers with no `SsoLink` rows that can't sign in any existing user (#2359). In a tenancy project `check --deploy` now opens every active tenant's pool to look, 8 at a time.

### Admin write errors

A bad inline row now refuses the edit and rolls back the parent UPDATE and all inline writes; the edit's `post_save` fires after that commit (#2339). Deleting a referenced row returns 409, after `pre_delete` signals but with no `post_delete` (#2340). Form errors no longer carry driver text (#2345); an unknown action is a 400 (#2346). `InlineApplyOutcome::failed` is deprecated: it is always 0.

### Scheduled jobs under a lock: use `once_per_period`

`with_lock` inside `Scheduler::every` still runs once per pod per period. Tick often and wrap the body: `scheduler.every(name, Duration::from_secs(60), …)` calling `lock.once_per_period(name, day, body)` (#2330).

### Job retries

`EmailJob` now makes 8 runs with 5s doubling backoff, about ten minutes (#2332). `MAX_ATTEMPTS = 0` now means one run (#2333). Per-queue mailers hold for in-memory queues only; with database queues use one mailer per jobs table (#2334, #2338).

### Admin audit feed: hook-scoped tables are superuser-only

Under `with_user_perms`, `/__audit` and the home "recent actions" skip tables with a queryset or `view` hook (#2342).

### `MediaPerms`: upload attribution and collection checks

`POST /uploads/begin` now answers 403 when `uploaded_by_id` is not the caller's id (superusers exempt), and a `collection_id` also needs `rustango_media_collections.view` (#2343). The same view is needed to move media into a collection and to create one under a `parent_id`. `required_codenames` returns the extra codename too, so policies built on it change the same way.

Custom `MediaAuthorizer`s: `POST /media/{id}/move` now arrives as `Change(MediaMove { id, collection_id, .. })`, not `Change(Media(id))`; a policy ending in `_ => false` refuses moves until it gets an arm. `NewCollection` carries `parent_id`.

### `values()` returns `Bool` and `Json` on MySQL and SQLite

A bool column used to come back as `SqlValue::I64`, a JSON column as `Null` (MySQL) or `String` (SQLite). Code matching `I64` / `String` for these columns must match `Bool` / `Json` now, as on Postgres. Aggregate aliases keep their own type (#2296).

### `db:restore --clean` needs `--yes`

Scripts must pass `--clean --yes`; without it the command asks on a terminal and errors otherwise. `--clean` takes only a non-empty regular file, and a tenancy project refuses it. Restores now run in one transaction, so a dump with its own `BEGIN`/`COMMIT` or non-transactional statements may need editing (#2283).

### `flush` on Postgres no longer cascades

If a table outside the filter references a flushed one, the flush now fails and clears nothing; add that model with `--model` or `--app` (#2285). An `ON DELETE CASCADE` link from such a table still empties it on MySQL and SQLite, but Postgres refuses.

### Tenancy `flush` needs `--tenant <slug>`

In a tenancy project plain `flush` now errors; use `flush --tenant <slug> --yes`. It clears only that tenant's tables; the registry is never flushed (#2284).

### Tenant PG pools set `application_name`

Database-mode tenant pools connect as `rustango-tenant:<org id>`, overriding one in the URL. A purge ends those sessions; any other session open on the database makes it fail (#2291).

### Tenant schemas are no longer shared

`create-tenant`, the console, the webhook and `migrate-tenant-storage` refuse a schema another tenant uses (#2290). Rows that already share one are not touched; `purge-tenant` now refuses to drop a shared schema, or one with a name provisioning would refuse (e.g. `public`, uppercase). Drop those by hand.

### A provisioning retry only resumes a never-activated tenant

Activating an org, editing an active one, or deactivating it drops its link to failed provision runs. A half-provisioned tenant activated by hand can no longer be resumed; a replay fails as "slug already exists" (#2292).

## 0.60.3

### `makemigrations` renames an M2M junction's column

Changing `src_col` / `dst_col` with the same `through` and tables now writes `RenameColumn` ops, so the rows stay. It was Drop + Create (#2245).

### Colliding FK names are refused

Two FKs whose `<table>_<column>_fkey` cuts to the same 63 bytes now fail at render, before the op that adds them, on PG and MySQL; rename a table or column. On MySQL, earlier ops of that migration have already committed (#2245).

A self-referencing M2M whose two columns both change is refused by `makemigrations`; rename one per migration.

### Custom `Cache` backends: override `touch`

`SessionStore::touch` now calls `Cache::touch`. The trait default is a get then a set, which can revive a session deleted in between; override it to extend only a live key (#2300).

### `[mcp] rate_limit_per_minute = 0` means unlimited

It used to refuse every request with 429 (#2299).

### i18n locale matching

`languages = ["pt-BR"]` now loads `pt_BR.json` (#2288), and `negotiate_language` picks `en` over `en-GB` for `en-US` (#2289). Check any test that pinned the old pick.

### `IpFilterLayer::behind_trusted_proxy` (opt-in)

The filter still checks the socket peer by default. Call `.behind_trusted_proxy()` to gate the client a `RealIpLayer::trust_proxies` layer resolved; then list client networks, not proxy ones (#2278).

### Purge pages with `CachePageLayer::invalidate`

Replace hand-built page-cache keys with `layer.invalidate([&PageKey::new(path, host).tenant(slug)])`; since 0.60 they miss the tenant and delete nothing (#2252).

### `RUSTANGO__LOGGING__FORMAT` now applies under `#[rustango::main]`

If it is set in the env or `./.env`, the default subscriber uses that format; before, it was ignored there (#2258).

### `Cli` behind a reverse proxy

Call `Cli::with_trusted_proxies(["127.0.0.1/32"])?` so logs and login limits key on the client, not the proxy (#2255).

### Mistyped env overrides now warn

A var with one `_` after `RUSTANGO` and `__` later is ignored, as before, but config loading now prints a warning. Rename it to `RUSTANGO__SECTION__KEY` (#2257).

### MCP 401 body is JSON

A missing or invalid agent token now gets a JSON-RPC error (code `-32001`), not plain text. Status and `WWW-Authenticate` are the same (#2259).

### Webhook headers are checked

`WebhookSubscription::header` with an invalid name or value makes `dispatch` return `JobError::Fatal` (#2236).

### MCP SSE streams close

The stream ends at the JWT's `exp` and within a minute of a revoke; clients should reconnect with a fresh token (#2237).

### `CachePageLayer` skips responses whose `Vary` is not in the key

Behind compression or `LocaleMiddleware`, pages stop being cached until you add the header, e.g. `.vary_on(["accept-encoding", "accept-language"])` (#2219).

### `S3Storage` default timeouts

10 s to connect; GET, HEAD and DELETE fail after 60 s without a reply or body chunk; an upload gets 60 s + size / 256 KiB/s. A client passed to `with_http` replaces all of them (#2220).

### `m2m_changed` skips no-op `add` / `remove`

A receiver that counted on a signal for a duplicate `add` or a missing `remove` no longer gets one (#2221).

### Password-reset links from before the upgrade are refused

`confirm_password_reset_pool` / `_single_use` need the issue time new links carry, and return `Expired` for older ones and for a link older than the last password change (#2248). Users request a new link. The `_into` forms are unchanged.

### Admin SSO with a TOTP device shows a code step

The callback now renders a code form posting to `{admin}/login/sso-totp` before the session is minted (#2249). With `totp` on, SSO also fails closed when the device table cannot be read.

### Member logout on a path-prefix tenant

Use `member_auth::logout_at(pool, &user, &org, full_request_path)`; `logout` clears `Path=/`, which leaves a prefix tenant's cookie in the browser (#2251).

### `Dialect::write_ilike_typed`

New provided method; the writers call it with the column's field type. A custom dialect that needs a cast before `ILIKE` overrides it (#2229).

### `Dialect::quote_literal`

New provided method for inline string literals. On MySQL a `\` in a comment or `string_agg` separator is now kept as written (#2232).

### `server::catch_panics` is public

Additive: `catch_panics(routes).layer(your_layer)` lets your layers see a panic 500 (#2168).

### `#[rustango::main]` reads `RUST_LOG` from `./.env`

With default logging it uses that value when the real `RUST_LOG` is unset. It sets no env vars; a bad `.env` is ignored (#2204).

### New `SchemaChange::AlterColumnComment`; `generated_as` changes are refused

makemigrations writes `AlterColumnComment` for a `db_comment` change and stops on a `generated_as` change; drop and re-add that column by hand. On PG a type change into a string now writes the field's whole type with no `USING` (#2239).

Binaries older than 0.60.3 cannot read a migrations directory that holds an `AlterColumnComment` file, so upgrade every checkout before pulling those migrations.

If an older `generated_as` edit was never migrated, makemigrations now refuses. Remove the field and run makemigrations (a DropColumn), add it back and run it again (an AddColumn), then migrate: the column comes back with the new expression.

### PG migrations create `citext` themselves

A migration, `apply_all_pool` and testkit table creation run `CREATE EXTENSION IF NOT EXISTS citext SCHEMA public` before a CITEXT column. It goes in `public` so every schema-mode tenant finds it. Where the role cannot create extensions, install it once by hand (#2240, #2269, #2271).

### A type change no longer writes a separate default op

makemigrations folds the new `DEFAULT` into `AlterColumnType`, so undoing it works on PG.

### `cursor_pagination` on a nullable column is logged, and will be refused in 0.61.0

It logs `tracing::error!` at build time (#2230), and will panic from 0.61.0 (#2265). Paginate on a NOT NULL column such as the primary key.

### ViewSet answers a bad filter with 400

An unknown `__lookup` or a value that does not parse (`?id=abc`, `?id__in=1,x`, `?flag__isnull=maybe`) now returns `400` instead of being ignored (#2227). With a filter backend registered, an unknown lookup is still passed to it. `iexact`/`contains`/... on a non-string field is a `400`.

### ViewSet skips empty filter values

`?field=` with an empty value no longer filters, on any field (#2226). It used to match `''` on a string field and nothing on a nullable one.

### Resolver caches are per registry

`invalidate_org_cache`, `invalidate_host_cache` and the testkit resolver resets still act on every registry in the process (#2077).

### Tenancy verbs reject stray arguments

`assign-role acme bob editor extra`, `set-operator-active alice bob --off` and similar used to ignore the extra word; they now fail. `set-superuser --on --off` fails instead of using the last flag (#1952).

`create-role --help` and `create-api-key --help` now return an error with the usage instead of printing it and exiting 0. A `--password` (or `--current`) value that starts with `--` is refused as a missing value. `create-operator`, `reset-operator-password`, `change-password`, `change-operator-password` and `create-api-key` now also take flags before the positionals.

### `[tenancy] apex_domain` now takes effect

If your config sets it, `Cli::with_settings` uses it when `RUSTANGO_APEX_DOMAIN` is unset (#1379).

### Admin search never covers a secret field

A `formfield_overrides = "x: password"` column is left out of `?q=` and autocomplete, even when `search_fields` names it (#2228). Only the `password` widget marks a secret: give `token` / `api_key` fields that widget too.

### A "view" object-permission hook now filters the admin list

Denied rows vanish from the list, autocomplete, FK facet names and FK cell names (#2267); pages may come up short, and totals, facet values and counts, date buckets and "has next" still see them. Use `register_admin_queryset!` to hide them everywhere (#2231). A password-widget field in `list_filter` gets no facet.

## 0.60.2

### `migrate-tenant-storage --to database` replaces the target's `public`

The target database's `public` must be empty, extensions included (the tenant's own are created there by the move, #2210), and droppable by the user: the database owner on PG 15+, else a superuser. Both are checked before anything moves. The new `public` is owned by that user, with `USAGE` granted to `PUBLIC` (#2189).

### `migrate-tenant-storage --to schema` needs PG 15+ with `CREATEDB`, or a superuser

It restores through a staging database (`rustango_stage_*`) on the registry server, which needs PG 13+ and, before PG 15, a superuser to rename `public`. The target schema must not exist yet. The extensions the tenant's objects use (`citext`, `pg_trgm`…) are created in the registry's `public` if it lacks them, but only trusted ones or those named with `--allow-extension <name>`; others, and a non-relocatable one (PostGIS), are refused up front. `--to database` applies the same rule on the target (#1864, #2210).

### The console connection probe returns JSON (#2144)

`POST <console>/orgs/test-connection` and `/orgs/{slug}/test-connection` now answer `{"status": "ok"|"bad", "message", "endpoint"?}` instead of an HTML fragment.

### Custom admin actions can require `delete` (#1818)

Register an action that deletes with `register_action_with_perm(.., ActionPerm::Delete, ..)`; `register_action` still checks `change`.

### Login limits warn when they count per process (#1809)

The first login logs a warning while the per-IP and global limits live in process memory; install `login_throttle::configure_shared(LoginThrottle::with_cache(limits, cache))` to share them.

### Error text no longer echoes secrets

`PoolError::UnsupportedScheme` holds just the scheme (empty if none), not the URL (#2172). `ConfigError::Shape` reads `` `section.key`: expected <type> `` and no longer quotes the value (#2159). Update any test that matched the old text.

### Admin CSRF cookie follows an outer `csrf::with_config` layer

The admin and tenant login forms now set and check that layer's cookie name, not always `rustango_csrf` (#2160).

### Provisioning runs store operator-safe failure text

New failed steps and runs store validation text or "Step failed (internal server error)"; the cause goes to the log (#2198). Rows already in `rustango_provisioning_events` and `rustango_provisioning_runs` keep their old text; clear them if they hold driver errors.

### Translations editor: an emptied cell deletes its override

The grid posts each shown value as a hidden `orig:<locale>:<key>` field; a cell shown non-empty and posted blank is deleted. A custom editor form must post those fields too, and can call `i18n::admin::apply_form` (#2091).

### SQLite: defaulted integer PKs are `BIGINT`

New tables and migrations create a non-`Auto` integer PK with a `default` as `BIGINT`, so the default applies. Existing tables keep the rowid column until rebuilt (#2137).

### ViewSet serializer PATCH runs in one transaction

The row is read with `FOR UPDATE` (SQLite: `BEGIN IMMEDIATE`), so a concurrent PATCH on the same row waits (#2010). A hand-written `ModelEntry` with an audited update runner but no `with_audited_update_record` keeps the old unlocked update, so its audit entry is still written.

### Host claims are transactional

Tenant edit and create now insert and delete a claim row in `rustango_org_hosts` inside their transaction (#2099).

### `GET /collections` returns at most 1000 rows

It used to return every collection. Page with `?limit=&offset=`; with no `?limit` it returns 1000 now and 100 from 0.61.0. `MediaManager::list_collections` is deprecated in favour of `list_collections_paged` (#1570).

### `GET /tags` is ordered by slug and paged

It used to return up to 1000 tags by usage; it now returns them by slug, with `?limit=&offset=` (default 1000 now, 100 from 0.61.0). `GET /tags/popular` still orders by usage (#1570).

### Wide recursive listings cap the offset

When a subtree has more collections than the backend's bind limit, `list_in_collection_paged` and `GET /collections/{id}/contents?recursive=true` refuse an `offset` above 10 000 with 400 (#1570).

### `tag` / `set_tags` take at most 1000 distinct slugs

More returns `MediaError::Other` (HTTP 400) and changes nothing (#1570).

### `#[rustango::main]` installs through `logging::Setup`

Same output as before. If a subscriber is already installed, it now warns on stderr instead of staying silent (#1493).

### MySQL batch upserts merge rows that collide in one statement

A `DoUpdate` batch with two rows on the same unique key now keeps the last one, as SQLite does; Postgres still rejects it (#2200).

## 0.60.1

### `seed-permissions` reports each failed tenant

It no longer stops at the first failure; it prints a `failed` line per broken tenant and ends with `N of M tenant(s) failed` (#2156).

### MySQL `PartiallyApplied` can report zero DDL

It is now also raised when only data operations committed, so `ddl_applied` can be 0, and by atomic unapply too (#2151).

Data operations before a DDL that fails to parse are now committed; they used to roll back. The error reports them.

### `bulk_insert_pool` validates rows

It and the `bulk_upsert_pool` / `bulk_insert_or_ignore_pool` model methods now return `ExecError::Query` for a row that breaks a field rule (`max_length`, `min`/`max`, `choices`, named validators) or names an unknown column (`UnknownField`), as `insert_pool` does (#2153).

### Render DDL outside the runner with `render_changes_between`

`render_changes_split_with_dialect` has no before-snapshot, so on MySQL it emits `DROP COLUMN` without the FK drop and fails (1828). Use `migrate::render_changes_between(changes, before, after, dialect)` (#2026).

### MySQL migrate lock name changed

The lock is now `rustango_migrate_<sha1 of DATABASE()>` (#1991). During a rolling upgrade an old and a new process on the same database do not exclude each other: upgrade them one at a time.

### `manager(ext = ...)` is gone

**Breaking:** drop the attribute and declare the trait yourself: `trait FooManagerExt: Sized { … }` plus `impl FooManagerExt for QuerySet<Foo>` (#2132).

### M2M writes validate against the through model

On a registered through model, `add` and `set` run its full `validate()`: `max_length`, `min`/`max`, `choices` and named validators, on every backend. A through model missing a manager column now gives `UnknownField` (#2136).

On MySQL, an `add` skipped as a duplicate sets the connection's `LAST_INSERT_ID()`, as other skipped inserts already did.

### Audited conflict bulk inserts run on PG and SQLite

On audited models `bulk_upsert_pool` and `bulk_insert_or_ignore_pool` no longer return `AuditUnsupported` on PG and SQLite; MySQL still does, except for an empty slice, which is now `Ok`. An audited model with no PK gets `MissingPrimaryKey` instead of `AuditUnsupported`. An audited `upsert` may now run two statements (#1795).

### Audited models have `save_partial`

`#[rustango(audit(...))]` models now get `save_partial` and `save_partial_typed`; an inherent method of the same name on such a model now clashes (#1744).

### One `through` table, one shape

Two `m2m` relations on one `through` table with different tables or columns (not just mirrored) are now a `MigrateError::Validation` in `makemigrations`, and a panic in the `SchemaSnapshot` builders (#2000). Give each its own `through`.

`M2MTableSnapshot` equality and order now ignore which end is the source, and new snapshots put the end that sorts first as the source. A mirrored pair no longer rebuilds the junction.

### `ConfigError::Parse` holds a `TomlSyntaxError`

**Breaking:** its `source` is now `config::TomlSyntaxError` (message and line only), not `toml::de::Error`. Read `message()` / `line_col()` (#2108).

### CSRF cookie `Secure` follows the layer

Under `csrf::with_config`, CBV and admin cookies take `Secure` from `CsrfConfig::secure`; plain-HTTP dev needs `allow_insecure_for_dev()`. Under a default `csrf::layer()` (and with no layer) they, and that layer's own cookie, follow the session `Secure` policy (#2117).

### ViewSet duplicate keys are 409

A ViewSet write that hits a unique or primary-key constraint now answers `409` (`"error": "conflict"`), not `400`. Clients that matched on 400 must accept 409 (#2075).

### `AppBuilder::serve` catches panics outside `api`

A handler panic is now a 500, but layers on the `api` router (headers, request id) do not see it (#2069).

### ViewSet throttles run after tenant resolution

Each tenant now has its own throttle budget; unknown tenants share one per client. `tenant_router` resolves the tenant through the mounted context auth uses, and takes no connection before the throttle (#2076).

### Hosts with a non-digit port are refused

`ALLOWED_HOSTS` (even `*`), tenant lookup, CSRF wildcards and `validate_url` now reject `host:port` where the port is not digits (#2043).

### `slugify` output for some non-ASCII input

`slugify` now folds via NFKD, so İ, Vietnamese letters and compat forms (`²`, `ﬁ`) keep a letter instead of being dropped. Stored slugs are not touched (#2092).

### Operator console

The console can be nested under a path prefix; its templates take `console_prefix` (#2007). Schema-mode and connect-check error texts changed (#1335).

### `tenancy::authenticate_*` return `PasswordVerified`

**Breaking:** `authenticate_user`, `authenticate_user_pool`, `authenticate_operator` and `authenticate_operator_pool` return `Option<PasswordVerified<_>>`. It derefs to the row; call `.complete(&pool)` (or `.complete_on(conn)`) after your second factor to store an upgraded hash (#2093).

### `AdminSession::impersonated_by`

**Breaking:** `AdminSession` has a new `impersonated_by` field; build it with `AdminSession::new`. In an impersonation `username` is empty: read `impersonated_by`. `actor()` returns an `AuditSource`; the i18n editor stores its token as `updated_by`, now `user:<id>` or `operator:<id>:impersonating` instead of a username or `operator:<name>`. Update any filter on it (#2110).

### `TenantSessionPayload::impersonation` takes a session id

**Breaking:** it takes a `sid` (the handoff `jti`), and the payload has a new `sid` field. Impersonation cookies from before this release are refused; open the tenant again from the console (#2038).

### `seal_flow` / `open_flow` take a `FlowScope`

**Breaking:** pass `FlowScope::new(purpose, tenant, provider)` (`""` tenant when single-tenant) at both ends. SSO logins in flight at deploy must restart (#1992).

### MCP agent tokens

**Breaking:** `issue_agent_token` takes the key's `secret_prefix`, `agent_token_still_valid_pool` takes it too, and `McpAgent` has a `secret_prefix` field. Agent JWTs minted before the upgrade are refused; clients re-mint (#1962).

### `MediaManager::pool()` returns `&sql::Pool`

**Breaking:** use `manager.pool().as_postgres()` where a `PgPool` was needed; it returns `Option<&PgPool>`, `None` on other backends (#2070).

### `create_collection` checks the parent

A missing or soft-deleted `parent` now returns `MediaError::Other("parent collection N is missing or deleted")` (400 over REST) instead of creating an orphan (#1573).

### MySQL refuses `on_delete = "set_default"`

InnoDB never enforced it: the parent delete failed with 1451. A migration that adds such an FK now fails to render on MySQL, as do `apply_all_pool` and testkit `create_tables_for`; pick another action (#1573).

### A `Host` with userinfo or a path is refused

`SslRedirectLayer` answers 400 and SSO login shows an error when `Host` is not `host[:port]` (#2173).

### `S3Storage` does not follow redirects

A 3xx from the endpoint is now an error. Point `S3Config` at the bucket's own region endpoint (#1780). A client passed to `with_http` keeps its own redirect policy; turn redirects off on it too.

### MCP JSON-RPC checks

A message with `"id": null` or a `jsonrpc` other than `"2.0"` is now an invalid request. HTTP Basic `client_id`/`client_secret` are form-urlencoded (RFC 6749), so a raw `%` or `+` in them must be encoded (#1963).

### `rustango_jobs.context`

`PgJobQueue::ensure_table_pool` adds a nullable `context` column (#1229). If you create the table in your own migration, add it there, then restart the workers:

- PostgreSQL: `ALTER TABLE rustango_jobs ADD COLUMN context JSONB;`
- MySQL: ``ALTER TABLE `rustango_jobs` ADD COLUMN `context` JSON;``
- SQLite: `ALTER TABLE rustango_jobs ADD COLUMN context TEXT;`

Without the column, or with another type, jobs run as `system`, as before. During a rolling deploy, 0.60.0 workers also run new rows as `system`.

### Tenant writes from a job

A job dispatched from the tenant admin records its user only on writes through the pool `tenancy::with_tenant` hands it for that tenant. Writes outside it, or through another pool inside it, record `system` (#2123).

## 0.60.0

### `verify_for_tenant` takes the `Tenant`

**Breaking:** call `auth.verify_for_tenant(token, &tenant)`. It refuses tokens not minted by `/login` or `/refresh`, and ended sessions (#2118).

### Strict CSP and the bundled admin

Put `'nonce-__RUSTANGO_NONCE__'` in `script-src` and `style-src` of `[security] csp` to run the admin without `'unsafe-inline'`. The `csp_nonce` module now also builds with `admin` (#1703).

### Basic-auth admins check CSRF

`protect_with_basic_auth` now refuses a POST without the `rustango_csrf` cookie and a matching `_csrf` field or `X-CSRF-Token` header. Scripts that post to it get 403 (#2131).

### Path-prefix tenant cookies use the prefix path

Tenant session, member session and SSO flow cookies set under a path prefix now carry `Path=/<prefix>`, not `Path=/` (#2098).

### `api::create_tenant` refuses what the CLI refuses

A bad slug or host, or a host, prefix or port another tenant uses, is now a `Validation` error (#2097).

### `user_model` validates the model

**Breaking:** `Cli::user_model` / `Builder::user_model` panic if the model lacks a required column. Add `password_changed_at` and `sessions_revoked_at` (`Option<DateTime<Utc>>`) to a custom user model (#1203).

### SQLite rebuilds tables for CHECK and composite FK changes

These ops no longer fail on SQLite (#2127); like other rebuilds, they refuse a table with a column the migration snapshot lacks. On PG, `render_changes_split_with_dialect` no longer emits the `DROP CONSTRAINT` of a UNIQUE drop: the runner finds the live name (#2133). `render_changes` still prints the usual name.

### `fresh_table` creates indexes

Test tables now carry the model's indexes (#2120): a test that inserted duplicate `unique_together` rows now gets a unique violation, and MySQL refuses an index over an unbounded `String`, as `migrate` does.

### `assert_num_queries` sees the PG `_on` reads

A block using `fetch_on`, `count_on` or another `_on` read now counts its queries (#1561); an expectation of 0 written around one fails.

### MySQL refuses a DB-default integer PK

An insert that leaves a non-`Auto` integer PK to its DB default fails with `GeneratedPkUnreadable` on MySQL (#1986). Set the PK or use `Auto<i64>`.

### Job queue `shutdown()` drains

`shutdown()` now waits up to `shutdown_grace` (default 5s) for running jobs; `InMemoryJobQueue` used to abort at once. Aborted jobs and parked retries stay queued for the next `start()` (#1255, #1677). An aborted job runs again from the start, so make jobs idempotent.

### `serve_until_drained(listener, app, drain)`

**Breaking:** it takes a `TcpListener` and an `axum::Router` instead of a serve closure, always adds `ConnectInfo<SocketAddr>`, and needs an axum-enabled feature such as `admin` (#1948). Other listeners or services now call `axum::serve` directly.

```rust
// before
serve_until_drained(|stop| axum::serve(listener, app).with_graceful_shutdown(stop), drain).await?;
// after
serve_until_drained(listener, app, drain).await?;
```

### `audit::save_one_with_diff` takes the BEFORE query

The macro-support function takes `&SelectQuery` (build it with `audit::before_image_query`) instead of a pk column, pk value and three column lists (#2061).

### `ModelForm::validate_unique_together` skips partial unique indexes

Like the serializer check; the database still enforces them (#2011).

### `auto_create_permissions(&PgPool)` seeds the reserved codenames

It now seeds `auth.access_admin`, the audit codenames and `extra_permissions`, like `auto_create_permissions_pool` (#2061).

### Background work keeps its caller's audit source

`InMemoryJobQueue` jobs and `Scheduler` ticks now run with the audit source and timezone of the scope they were dispatched or registered in, not `system` and UTC (#1229). A `run()` that re-enters `audit::with_source` overrides it. Reports counting `system` rows will drop.
A tenant admin user's id is recorded only on that tenant's writes (a `for_each_tenant` pass); elsewhere it is `system`. Tenant handlers that set a user source should use `audit::with_tenant_source`.

### OpenAPI `Schema` has no `nullable` field

**Breaking:** `Schema.nullable` is gone and `Schema.type_` is a `SchemaType`; call `.nullable()` instead (#1922). A new `any_of` field holds a nullable `$ref`. ViewSet request bodies are now inline schemas, not `$ref`s to the item schema.

### Re-creating a deleted media collection

`create_collection` hard-deletes a soft-deleted collection with the same slug (#1677).

### `AlterColumn*` no longer refused on MySQL and SQLite

A type, nullability, default, length or UNIQUE change now applies instead of failing with "not yet supported" (#1676); new migrations need no RunSQL workaround; leave applied ones as they are. MySQL relies on strict `sql_mode` to refuse a shrink that would truncate. SQLite keeps a value its new type cannot convert (column affinity), where PG and MySQL refuse it.

### `on_delete` changes are migrated

The first `migrate` after upgrading writes a system migration that fixes the framework's cascading FKs, and `makemigrations` emits `AlterFkOnDelete` for your own (#1557). The 0.57.7 catalog check and manual `ALTER` are no longer needed. On SQLite this, and every `DropColumn`, rebuilds the table, which fails if the table has a column the migration snapshot lacks. An atomic SQLite migration that rebuilds a table cannot also hold RunSQL; split it or set `atomic: false`.

**Breaking:** `SchemaChange` has a new `AlterFkOnDelete` variant, and `RenderedBatch` a new `rebuild` field; both are now `#[non_exhaustive]`, so match with `_` and build a batch from `Default`. On SQLite, `render_changes_split_with_dialect` returns no statements for `DropColumn` or `AlterFkOnDelete`: the work is in `rebuild`, which only the migrate runner can apply. Only single-column FKs are dropped by name before a `DropColumn` or an on_delete change.

## 0.59.19

### Bearer tokens need a login session

`require_bearer` and `/api/auth/me` refuse access tokens not minted by `/login` or `/refresh` (e.g. from `JwtAuth::lifecycle().issue_access_with`), and tokens after a logout or password change (#2086).

### `OnAuthSuccess` takes an `AuthSuccess`

**Breaking:** write `Arc::new(|login: AuthSuccess| Box::pin(async move { .. Ok(Redirect::to("/").into_response()) }))`. Find users by `login.identity_key()` (#1989).

### `WebhookEvent` has `body` and `signature`

**Breaking:** `signing_secret`, `signature_format` and `payload` are gone; sign with `webhook::sign` (#1852). Drain the webhook queue before upgrading: older queued jobs fail to decode.

### Provision webhook secrets are at least 32 bytes

**Breaking:** `WebhookConfig::secret` is a `WebhookSecret`; `WebhookConfig::new` panics on a shorter key (#1850).

### CBV CSRF follows the outer layer

With an app-wide `CsrfLayer` (e.g. `Cli::with_csrf_config`), template views use its cookie name and origins; forms posted with the old `rustango_csrf` cookie need a reload (#1722). A CBV defers to that layer only when it checked the token: on a path its `exempt_prefixes` skip, the CBV still requires one.

### `webhook::sign` returns a `Result`

**Breaking:** add `?` or `.expect(..)`; an empty key is `Err(EmptySigningKey)` (#1850).

### Template views and ViewSet writes respect soft delete

On a `#[rustango(soft_delete)]` model, `DeleteView` and `delete_selected` now stamp the column instead of deleting, the other template views 404 on deleted rows, and no form or ViewSet body can set the column (#2082, #2074). Use `soft_delete::restore` to undelete.

### Admin creates need the audit table

An admin create of a model with `audit(...)` now fails without `rustango_audit_log`, as edits do (#2101). `manage migrate` creates it.

### Admin `change_password_url` is a full path

`admin::Builder::change_password_url` and the `[routes] change_password_url` settings key are linked as given, no longer prefixed with the admin path (#2102).

### `sqlmigrate_one` takes a dialect (#2025)

**Breaking:** pass the target backend, e.g. `sqlmigrate_one(dir, name, pool.dialect())`; `manage sqlmigrate` and `migrate --dry-run` now render for the pool's backend.

### `Dialect::acquire_session_lock_sql` no longer waits (#2027)

It returns a try-lock (`pg_try_advisory_lock`, `GET_LOCK(?, 0)`) that yields whether it was taken; a custom dialect must follow suit.
`MigrateError` gains `LockTimeout`, returned only under `migrate::with_lock_timeout`.

### Schema-mode FK targets are schema-qualified (#1718)

Migrations on PostgreSQL pin `REFERENCES` to the session's schema, so a tenant FK to a table its schema lacks fails instead of binding to `public`. Registry-scoped models stay unqualified.

### Cache and derive behaviour

- `InMemoryCache::set_forever` entries are not evicted and do not count toward the budgets, up to `DEFAULT_MAX_PINNED_BYTES`/`_ENTRIES` (`with_max_pinned_bytes`/`_entries`); past them they are stored evictable.
- `DatabaseCache::ensure_table` now also creates the `expires` index (best effort); run it once on existing tables.
- `#[derive(Model)]`: `citext`, `vector(dims)` and `geometry(srid)` on a field of another type are now compile errors.
- A field `index` on a non-snake_case field now indexes its real column (`userName`, not `user_name`).
- `M2MManager::add` / `GenericM2MManager::add` on MySQL now return data errors (truncation, FK) that `INSERT IGNORE` hid.

## 0.59.18

### `with_rollback` hands the closure an `AtomicTx`

**Breaking:** write `insert_tx(&mut *tx.lock().await?, &q)` where you passed `tx` (#1761). A nested `atomic()` on the same pool is now a savepoint; drop the guard before it or a `bulk_insert_pool`, or they fail with `NestedAtomic`.

### Admin edits need the audit table

An admin edit of a model with `audit(...)` writes its audit row in the UPDATE's transaction, so a missing `rustango_audit_log` table now fails the edit (#2060). `manage migrate` creates it. Other models still log best-effort.

### Webhooks with private targets still refuse cloud metadata

`allow_private_targets` no longer reaches `100.100.100.200`, any `169.254.0.0/16` address or `fd00:ec2::/32` (#1821).

### `CreateView` on MySQL fails closed for some audited models

An audited model whose PK the database generates and is not an integer (a UUID default, say) cannot report the new PK on MySQL, so `CreateView` now fails instead of saving it unaudited (#1821).

### Schema-mode tenants cannot use `public`

Provisioning and `create_tenant` refuse a schema named `public` (#1868). Rename any such tenant's schema.

### Tenant hosts must be unique

Editing or provisioning a tenant with a host another tenant uses (base or extra) is refused (#1931). The console edit form now rejects a host with a port, a bad `path_prefix` or `port`. A path prefix or port another tenant uses is refused too.

The `<slug>.<RUSTANGO_APEX_DOMAIN>` default host is validated, so an apex with a port (`localhost:8080`) now fails provisioning. Set the apex without the port.

### Derived tenant URLs keep only TLS options

`tenant_url_on_registry_server` copies only the `sslmode`/`ssl-*` keys from the registry query; set any other option on the tenant URL itself.

### Mail config errors

`email::from_settings` returns `MailError::Config` for `backend = "file"` without `file_email_dir` and for unknown backends, instead of using the console (#1948). An SMTP 550–555 refusal is the new `MailError::Rejected`, which `is_retryable()` is false for; an auth failure (535) stays `Transport`.

### Impersonation username

An impersonation session's username is `operator:<username>`, not empty (#1939).

### Direct media uploads

**Breaking:** `Storage::presigned_put_url` takes a `content_length: Option<u64>`, and a backend that presigns PUTs
must implement the new `Storage::metadata`, or `finalize_upload` errors. `UploadTicket` gains `content_type`;
the browser must send that header and exactly `size_bytes` bytes.

**Breaking:** `Storage::presigned_put_url` takes `&PutConditions` instead of the type and length. Direct-upload PUTs
must also send `If-None-Match: *` (all of `UploadTicket.headers`); allow that header in the bucket's CORS rule.

`purge_pending` now also deletes old `Failed` rows, and the storage object of every row it purges.

`begin_upload` refuses a declared size over 100 MiB; raise it with `MediaManager::with_max_upload_bytes`.

Storage keys with an empty or `.` segment (`a//b`, `./a`, `a/`) are now `InvalidPath`.

### `save_uploads` keeps nothing on error

Any error now deletes the files the request already saved, not only `TooManyFiles`. Random key prefixes are UUIDs, not nanos.

## 0.59.17

### Number filters round halves up (#1896)

`floatformat`, `numberformat::format`, `format_number` and `format_currency` round half away from zero on the shortest decimal form, so `2.5` gives `3` (was `2`).

### Stricter email, nullable bools and `timesince` (#1897)

`validate_email` no longer trims: `" a@b.com"` fails, so trim before saving. A missing nullable `Option<bool>` form or JSON key saves `NULL`, not `false`.
`save` checks every column, so a stored row with a spaced email now fails its next save, even of another field. Trim stored data first: load the rows, set `row.email = row.email.trim().to_owned()`, `save`. `objects().update().set(..)` checks only the set columns.
`timesince(.., depth)` drops units after an empty one: a year and three days is `"1 year"`.

### `plural_category` can return `"zero"` and `"two"` (#1921)

Arabic, Hebrew and Slovenian now use those CLDR categories; add the forms to plural catalogs (a missing form falls back to `"other"`). `Locale::as_str` turns `_` into `-`.

### `slugify` folds accented Latin letters (#2048)

New slugs for accented titles change (`"Café"` → `"cafe"`, was `"caf"`); stored slugs are untouched. `unique_slug` falls back to `"untitled"`.

### SSO email verification and forwarded hosts (#1842)

**Breaking:** a GitHub login now also calls `/user/emails` (needs the `user:email` scope or
the app's email permission); a 403 or 404 there means no verified email, any other failure
fails the login. Facebook emails are never
verified, so email linking skips them. Behind a proxy, name it in `RealIpLayer::trust_proxies`
or member SSO builds `redirect_uri` from `Host`. Tenant SSO, admin SSO and the MCP discovery URLs
read `X-Forwarded-Proto` only from such a proxy too, else assume `https`. A proxy counts as trusted
only when it also sends the configured client-IP header (`X-Forwarded-For` by default).

### Passkey challenge and counter API (#1841)

**Breaking:** `seal_challenge` takes a `CeremonyPurpose`; `open_challenge` takes the same
purpose and a cache, and is async. `verify_authentication` returns `AuthenticationOutcome`
(`.sign_count`, `.user_verified`); `update_sign_count` returns `SignCountUpdate`: refuse the
login unless `.is_accepted()` (`Stale` is a clone or a lost race). Tokens sealed before
the upgrade no longer open. The `passkey` feature now enables `cache`.
A credential whose stored counter is non-zero and which now reports 0 (a reset or cloned
authenticator) fails with `CounterRegression`. The user removes that passkey and registers it again.

### `[auth] argon2_*` now apply (#1728)

New hashes use `argon2_memory_kib` / `argon2_iterations` / `argon2_parallelism` when set;
check them before deploying. Existing hashes keep verifying at their own cost.
Built-in logins store a new hash when the old one is weaker, which also ends that
user's other sessions. Until every user logs in once, login time differs between old-cost
and unknown accounts, which tells an attacker which accounts exist. Custom login code can
call `passwords::upgrade_stored_hash`.

### JWT refresh honours logout (#2036)

A cookie logout now also ends that user's JWT refresh chains; clients must log in again.

### `confirmed_secret_checked` errors on an undecodable secret (#1875)

It returned `Ok(None)` (no second factor); it now returns `Err`, also for a secret under 10 bytes
(`TotpSecret::from_base32` refuses those). `confirmed_secret` is deprecated: a read error looks like
no device, so use `confirmed_secret_checked`. `Debug` of `TotpSecret`,
`AdminTotp` and `Signer` no longer prints the secret.

### `runserver` auto-migrate and the registry run apply the system chain first (#2056)

`runserver` now applies the system chain like `manage migrate`. `migrate_registry` applies it before the project's registry migrations, not after.

### Framework tables a project's own migrations create get new columns (#2052)

`migrate` adds missing framework columns to them after the project chain; a NOT NULL column without a default on a table with rows fails until added by hand (an empty table is fine, #2066).

### A migrate inside a running migrate is an error (#2055)

A callback or observer that migrates while the outer run holds the lock now gets an error, not a hang. Migrate from `post_migrate` instead: it fires after the lock is released.

## 0.59.16

### Custom admin views need `change` for writes

**Breaking:** under `with_user_perms`, grant `{table}.change`, or declare `perm = "…"` on
`register_admin_view!`, for POST/PUT/PATCH/DELETE views. Without it only a declared `perm` is checked.
`AdminCustomView` gains a `perm` field; struct literals must set it.

### Admin list URL filters are allow-listed

`?<field>=` is ignored unless the field is in `list_filter` or `list_display`, an FK, or an inline's parent column (#2031). Add the field to `list_filter` to keep a bookmarked filter.

### `success_url` placeholders are percent-encoded

`{pk}` and `{column}` values in a `CreateView`/`UpdateView` `success_url` are encoded as one path
segment, so a `/` in the value becomes `%2F` (#1862).

### Admin inlines enforce `max_num` and use `INITIAL_FORMS`

A save that adds inline rows past `max_num` re-renders with an error; slots past `INITIAL_FORMS` are inserts (#1717).

### The ViewSet list follows the model's `default_order` (#2047)

With no `.ordering(..)` the list uses `default_order`, then the PK.

### ViewSet `DELETE` soft-deletes a `#[rustango(soft_delete)]` model (#1998)

It stamps the column; soft-deleted rows then read as `404` and leave the list.

### `CreateView` answers a duplicate unique value with `422` and the form (#2033)

A template must render `form.errors` (`__all__` for a row-level error) to show it.

### `list_params::parse_ordering` with an empty allow-list sorts on nothing (#1996)

Pass the sortable names explicitly; empty no longer means every field.
`ViewSet::ordering_fields(&[])` now disables `?ordering=` instead of allowing the rendered fields.

### ViewSet bulk create takes at most 1000 rows (#1999)

More is a `413` (raise with `max_bulk_create(n)`); each row spends one `create` throttle unit.
A bulk larger than the `create` throttle's `max` is a `413`; a throttled request no longer counts.

### ViewSet `QUERY` shares the `list` throttle with `GET` (#1997)

A client that sends both now spends one budget, not two.

### Handler panics are caught (#1541)

`Cli` and `server::Builder` turn a panic into a `text/plain` 500 with body `internal server error`, carrying CORS and the security headers; a panic hook you rely on still runs. A panic inside a streaming response body is not caught.

### `EtagLayer::default()` caps at 4 MiB; streams are not tagged (#1866)

The derived default had no cap. A body with no size hint (stream, SSE) or over the cap now passes through without an `ETag`.
`MethodOverrideLayer` answers `413` to a form over `body_limit` instead of forwarding an empty POST.

### CORS adds `Vary: Origin` more often (#1867)

Refused origins and any-origin mode now send it, so shared caches key on `Origin`.
A preflight that echoes the requested headers also varies on `Access-Control-Request-Headers` and `-Method`.

### `negotiate` honours `q=0` and specificity; flash cookies are byte-capped (#1957)

`negotiate` returns `None` for a type the client refused with `q=0`; a NaN or infinite q counts as the default. `messages::push` drops the oldest messages past `MAX_COOKIE_BYTES`, and truncates a newest message that alone is too long.
Use `WsHub::upgrade(ws)` instead of `ws.on_upgrade(.. ws_handler ..)` so `max_message_bytes` applies before buffering.

### Template fragment keys change (#1884)

`make_template_fragment_key` hashes length-prefixed parts; cached fragments miss once after upgrade.

### `TestClient` acts like a browser (#1958)

`logout(Some(path))` no longer clears the jar first; only the server's `Set-Cookie` removes cookies. Requests now carry `Host: testserver`, `Origin` on unsafe methods and a `127.0.0.1` `ConnectInfo`.
An empty `Set-Cookie` value is stored, not deleted; `TestResponse` has a new `header_map` field, so struct literals need it.
Redirects follow Fetch: 301/302 turn only `POST` into `GET` (PUT/DELETE repeat); `Origin` is `https://` when the test sends `X-Forwarded-Proto: https`.

### `truncate_tables` and `Fixture` loads are one transaction (#1959)

A failing table or row rolls back the whole call. Fixture keys for a model table must be its fields, and values are typed from them.
With two models on one table, the one whose fields cover the keys wins, then the project's own; a tie is an error.

### Page cache skips requests with no resolved tenant (#2045)

Apex or marketing routes that resolve no tenant are no longer cached. Set `CachePageLayer::tenant_agnostic(true)` on routes that are the same for everyone.

### `DynamicForm` enforces required checkboxes (#1895)

A `boolean` field marked `"required": true` (or `required: true` in Rust) must now be ticked. Checkboxes without it stay optional.

### `Settings.secret_key` removed; repeat soft delete returns 0 (#1929)

The field was never read; sessions use `RUSTANGO_SESSION_SECRET`. A key left in TOML is ignored.
`soft_delete` on a deleted row, and `restore` on a live one, now return `0`.

## 0.59.15

### The standard tenant chain drops the `X-Org` fallback (#1856)

A request is resolved by host only. To keep header routing, add `HeaderResolver::default().allow_only([...])` via `Builder::header_resolver`, `Cli::tenant_header` or `ChainResolver::push`.
`PortResolver` matches the `ListenerPort` extension that `Builder::serve` inserts; add it yourself if you serve another way.

### S3 with an `endpoint` and `path_style = false` puts the bucket in the host (#1904)

Requests go to `<bucket>.<endpoint-host>`; set `path_style = true` for MinIO-style URLs. `exists` now errors on a 403/503.

### `Storage` gains `save_with_content_type` (#1904)

A provided method; a `Storage` impl with its own method of that name must rename it.
`MediaManager::save_bytes` stores HTML, XML, SVG, JS and malformed MIMEs as `application/octet-stream`; the row keeps the declared type.

### Static files send an `ETag`; `EtagLayer` keeps an existing one (#1531)

`EtagLayer` no longer rehashes a response that already has an `ETag`, nor tags a `206`. A compressed response gets a weak ETag and no `Accept-Ranges`.

### M2M destination keys are `impl Into<SqlValue>` (#1950)

`add` / `remove` / `contains` take any key and `set` any `&[K]`; integers still bind as `i64`.
An empty `set(&[])` needs a type now: `set::<i64>(&[])`, or call `clear()`. `all_as::<K>()` takes a
`FlatScalar` key. A key the junction column can't hold is an `ExecError::Query(TypeMismatch)`.
**Breaking:** `M2mChangedContext::dst_pks` is `Vec<SqlValue>`. A bad URL PK in a template view is a 404.

### ViewSet `__in` lists cap at 1000 values (#1865)

A longer `?field__in=` / `?field__not_in=` list is a 400, as are lists summing past the dialect's bind
limit (less 1000). `InListTooLong` is an enum. Split larger lookups into several requests.

### `ListView` falls back to `default_order` (#2005)

A `ListView` with no `order_by` now sorts by the model's `default_order`, then the PK.

### `slugify` keeps non-ASCII-only text (#1919)

`slugify("Привет мир")` is `"привет-мир"`, not `""`. Mixed text still drops non-ASCII letters.

## 0.59.14

### `RustangoError` status changes (#1955)

DB errors inside `Auth`/`AuthFlow`/`BulkAction`, `Env`, `JwtIssue` and hashing errors are now `500`; `Busy` is `503`.
Their message is withheld unless `RUSTANGO_DISCLOSE_ERRORS` is set.

### `/ready` drops each check's `error` field (#1840)

Call `HealthRouter::show_errors()` to keep it on an endpoint only operators reach.

### `CompressionLayer` skips streams and `206` (#1954)

A body with no exact size hint (`Body::from_stream`) is now sent uncompressed instead of buffered.

### ViewSet form bodies are validated (#1993)

A form-urlencoded write that broke a serializer rule now gets the same `422` as JSON.

### ViewSet ignores a renamed field's model column on write (#1994)

With `#[serializer(source = "body")] content`, send `content`; a `body` key is now dropped.

### A UUID-default column added to a filled SQLite table is nullable (#1987)

SQLite can't add a `gen_random_uuid()` DEFAULT to a table with rows, so the column is backfilled and left nullable with no DEFAULT;
the ORM binds the value on insert. MySQL's DEFAULT `UUID()` gives v1 UUIDs, not v4.

### SQLite `now()` columns added to a filled table get a fixed default (#2017)

SQLite can't add a `now()` DEFAULT to a table with rows, so the column's DEFAULT is the time of the migration.
The ORM binds `auto_now_add` / `auto_now` on insert; raw `INSERT`s that omit the column get that fixed time.

### `migrate` fails when the system chain can't be generated (#2014)

`migrate`, `migrate-registry` and `migrate-tenants` now return the generation error instead of applying a stale
`system/migrations/` chain. A read-only image must ship an up-to-date `system/migrations/`.

### Tenant migrate verbs fail on a failed tenant (#1844)

`migrate-tenants`, the combined `migrate` and `migrate --fake --all-tenants` return an error (non-zero exit)
when any tenant failed, after printing the full report. Deploy scripts that relied on exit 0 now stop.

### Tenant admin change-password needs 8 characters (#1874)

A tenant user can no longer set a new password shorter than 8 characters (counted as characters, not bytes).

### Admin hides soft-deleted rows (#1918)

A `#[rustango(soft_delete)]` row no longer shows in the admin once deleted; its detail page is a 404.
Use the list's "Show deleted rows" link (`?trashed=1`) to see and restore them. `trashed` is now a reserved list param.
The trash list offers only `restore_selected`; a custom `list.html` posts `trashed=1` with the action to return there.

### Admin facet and date counts follow the filters (#2004)

Facet and date-strip counts now match the filtered list, not the whole table. The year strip lists at most
the newest 200 years (`MAX_YEAR_BUCKETS`). `values()`/`aggregate()` dicts on PG/MySQL return `SqlValue::Date`/`DateTime`
where they gave `Null`; code matching on `Null` or `String` for those cells must match the new variants.

### Admin pre-signals and per-row bulk-action signals fire (#1928)

Receivers on `admin_pre_save` / `admin_pre_delete` now run. `delete_selected` sends delete signals per row;
`restore_selected` and custom actions send `admin_pre_save`/`admin_post_save` with `change = true` per row.
A refused action, or `restore_selected` on a model without soft delete, sends none.

### Natural PKs are form input on create (#1725)

`CreateView` and `ModelForm::new` now render, require and insert a non-`Auto` PK field. A form that relied on it
being dropped must `.exclude` it. New: `core::WriteKind` and `FieldSchema::accepts_input` / `is_rust_side_uuid`.

### Logout ends a user's sessions on every device (#1855)

`User`, `Operator` and `AdminUser` gain a nullable `sessions_revoked_at`; run `migrate` (struct literals need the field).
Cookies are stateless, so logout ends all of that user's browser sessions, not only this one. Call
`member_auth::logout(pool, &user)` instead of `clear_cookie()` alone. An impersonation logout leaves the operator signed in.
`HandoffPayload` gains `iat` (struct literals need it). `authenticate_user` now reads through the ORM, so a tenant
`rustango_users` missing a column errors instead of filling a default.

## 0.59.13

### Admin `list.html` gets `hidden_params` (#1916)

A custom `list.html` should loop `hidden_params` (not `active_filters`) for the search form's
hidden inputs, so a search keeps custom filters and the date drill.

### Admin list falls back to `default_order`; lists end on the PK (#1917)

An admin list with no `admin(ordering)` now sorts by the model's `default_order` before the PK.
`ListView` and admin `ORDER BY` gain a trailing PK column when the sort does not include it.

### ViewSet `PATCH` validates over the stored row (#1995)

A serializer `PATCH` now loads the row once before the update. `ModelSerializer` gains a defaulted
`validate_patch`; a hand-written impl keeps the old body-only check unless it overrides it.

### Feature flags never expire by default (#1956)

Flag writes no longer carry a 1 hour TTL. To keep the old expiry, call `.ttl(Duration::from_secs(3600))`.
They go through the new defaulted `Cache::set_forever`; a custom cache that wraps another must forward it.

### `check --deploy` flags an ungated admin (#1627)

An app that builds `admin::router(pool)` or a `Builder` without `with_session_auth`
now gets an `[admin]` warning. Add the login, or ignore it if you gate the route yourself.
`tenant_mode()` alone no longer silences it.

### `admin::Builder::new` cookies follow the secure-cookie policy

They are `Secure` on the prod tier or when `[security].secure_cookies` is on, and under `manage` it
defaults to on. So a dev config without it gets `Secure` cookies and login fails over plain HTTP:
add `[security] secure_cookies = false` to `config/dev.toml`, or call `.secure_cookies(false)`.

### Commit and ship `system/migrations/` (#1988)

Without it, `migrate` regenerates the chain and checks the live schema instead of the ledger,
for the registry and every tenant. Commit it and add `COPY system /app/system` to an existing `Dockerfile`.

### `manage`-only builds log requests (#1514)

A build without `admin` (the `api` template) now sends `X-Request-Id` and writes
`rustango::access_log` lines. Turn the log off with `[logging] access_log = false`.

## 0.59.12

### Edited constraints now migrate (#1881)

**Breaking:** the next `makemigrations` picks up CHECK, EXCLUDE, composite FK and M2M edits it
ignored before. An edited M2M is dropped and recreated, so its rows are lost: copy them first.

### Shrinking `max_length` no longer truncates (#1878)

**Breaking:** on PostgreSQL the migration now fails when a value is longer than the new length.
Shorten those values first.

### Drop order in new migrations (#1879)

Only newly written files use it. An unapplied file that drops a column before its index,
or a parent table before its child, still fails on SQLite and MySQL: regenerate it.

### `AddColumn` adds the FK and UNIQUE (#1877)

**Breaking:** a migration that adds a `ForeignKey` or `unique` column now creates the constraint,
so it fails on rows that break it. Columns added by earlier migrations still lack it.
SQLite leaves out the FK of a column with a default (it refuses one on a table with rows) and warns.

### UNIQUE constraints are named (#1880)

`CREATE TABLE` writes `CONSTRAINT <table>_<column>_key UNIQUE (<column>)`; PG names do not change.
**Breaking** on MySQL: new tables name the unique index `<table>_<column>_key`, not after the column.
Two UNIQUE columns that map to one name (`a_b.c`, `a.b_c`) now fail to render: rename one.

### Integer division and PostgreSQL date lookups (#1900)

On MySQL, `F("n") / 2` over integers now truncates (7 / 2 = 3), as on PostgreSQL and SQLite.
On PostgreSQL, `__date`/`__hour`/… and `trunc_*` on a `DateTime` column use UTC even after `SET TIME ZONE`.

### MySQL `Decimal` columns are `DECIMAL(65, 28)` (#1899)

New tables get the wider type; existing `DECIMAL(38, 10)` columns keep rounding past 10 places.
Widen them with `ALTER TABLE t MODIFY c DECIMAL(65, 28)`. SQLite still keeps ~15 significant digits.
Read-back values carry 28 decimal places (`1.5000…`); call `Decimal::normalize()` before display.

### JSON comparisons match across backends (#1898)

SQLite `as_text` JSON paths now yield text: compare them to `'1'` / `'true'`, not to `1`.
SQLite still formats some values unlike PostgreSQL: `1.50` is `'1.5'`, `1e2` is `'100.0'`, big integers lose digits, objects have no spaces.
On MySQL, `as_text` of a JSON null is now SQL NULL, not `'null'`.

### Audit feed codenames are `rustango_audit_log.view_feed` / `.clean_feed` (#1979)

Grant these instead of `audit.view` / `audit.delete`. The old names are ignored once a model uses table `audit`.

### `InlineFormPanel` gains `more_rows_filter` (#1977)

A struct literal needs the new field. Panels past the formset cap show only the first rows.

### MySQL refuses an INSERT whose DB-default PK is not an integer (#1978)

`insert_returning_pool` returns `GeneratedPkUnreadable` before writing; it used to insert, then fail.
Submit the PK (or use `default_uuid_v7`) for such models on MySQL.

## 0.59.11

### Relation `SUM` decodes by column type (#1944)

`annotate_sum` over a relation's float column now reads back as `f64`, not `i64`.
Grouping a `union()` by a `.join()` column now fails with `QueryError::GroupByJoinUnreachable`.

### `upsert` targets the PK over a field `index(unique)` (#1935)

**Breaking:** a model whose only unique index is a field `index(unique)` or a `unique_when`
now upserts on the PK, so a new row with a taken value fails with a unique violation instead of
updating the existing row. To target the column, declare `unique_together = "col"`.

### `values()` returns `SqlValue::Uuid` for a Uuid column on MySQL (#1901)

**Breaking:** MySQL gave `SqlValue::String`; match on `SqlValue::Uuid` as on the other backends.

### `QuerySet::paginate` orders by PK when unordered (#1890)

A queryset with no `order_by` now pages in PK order instead of the database's scan order.

### `Dialect::write_conflict_clause` takes the model (#1887)

**Breaking:** a custom `Dialect` adds a `model: &ModelSchema` argument. On MySQL,
`insert_or_ignore` now returns `false` on a skip, and a skipped `DoNothing` through
`insert_returning_pool` is `RowNotFound`, as on PostgreSQL.

## 0.59.10

### Tenancy `migrate` verbs refuse unknown flags (breaking)

`migrate-registry` / `migrate-tenants` used to drop every flag and run the real apply;
now an unknown flag is an error. Use `migrate-tenants` for a tenant-scoped target.

### Scaffolder refuses keyword names

`make:*` and `cargo rustango new` now refuse names like `Type`, `std` or `crate`; the
code they generated for them did not compile.

### `dumpdata` / `loaddata` fail instead of losing rows (breaking)

`dumpdata` now errors on a model with an Array, Range, HStore, Vector or Geometry column;
leave it out with the new `--exclude app.Model`. `loaddata` exits non-zero if any row was skipped.

### Tenancy user and permission verbs refuse unknown flags (breaking)

`grant-perm`, `revoke-perm`, `create-user` and the host verbs now fail on a flag they don't
take. A password typed at the prompt is no longer trimmed: one set with a leading or
trailing space before now logs in without it.

### Shutdown drains for 20 s, then closes

`runserver` no longer waits forever for open connections after SIGTERM. Set
`[server] shutdown_timeout_secs` to change it, under your orchestrator's grace period.
`ServerSettings` gained that field, so a struct literal needs `..Default::default()`.
A webhook delivery whose earlier run failed now provisions again instead of returning
`duplicate: true`. `WebhookConfig` gained `stale_run_after`; build it with `WebhookConfig::new`.

### `email::from_settings` returns `Result` (breaking)

Add `?`. A `backend = "smtp"` that cannot be built used to fall back to `ConsoleMailer`;
it is now an error. Replace `smtp_tls = "tls"` with `"implicit"` (what it meant) or
`"starttls"`. Match `MailError` with a `_` arm. `SmtpMailer` refuses custom envelope headers
such as `Bcc` or `Subject`; set them on the `Email` fields.

### A broken config fails boot (breaking)

`Cli::run` now returns an error when `config/` exists but does not load, for example
`RUSTANGO__SECURITY__SECURE_SSL_REDIRECT=1` (use `true`). It used to warn and run without
allowed hosts, security headers or login limits. Fix the value the error names.

### Tenant moves reach every server

After `edit-tenant --database-url` or `migrate-tenant-storage`, running servers switch within
30 s without a restart. The CLI no longer claims it evicted their pools.

### Tenant purge deletes extra hosts

`purge-tenant` now deletes the tenant's `rustango_org_hosts` rows and sets `active = false`
before it drops anything, so a failed purge leaves an inactive tenant you can purge again.

## 0.59.9

### `DatabaseCache::incr` keeps the first TTL

**Breaking:** `incr` no longer moves the TTL on each call, and an `i64` overflow is an error.

### Change-password misses lock the account

**Breaking:** five wrong current passwords on a change-password form lock the account
like failed logins; the form and the login page answer 429 until the lock ends.

### Uploads: active types refused, `max_files`, `with_uploads`

**Breaking:** with no `allowed_extensions`, `save_uploads` refuses HTML, SVG, XML and JS
(`uploads::ACTIVE_EXTENSIONS`); list one to accept it. More than 20 files per request is
`UploadError::TooManyFiles` (raise with `.max_files(n)`). `UploadConfig` and `UploadError`
are `#[non_exhaustive]`: build configs with `UploadConfig::new(..)`, add a `_` match arm.
Mount upload directories with `with_uploads` instead of `with_static`.

### ViewSets with open write actions warn

No behaviour change: a ViewSet whose write actions have no codenames still serves them, but
logs a warning at mount. Add permissions, `.read_only()`, or `.allow_anonymous()` to silence it.

### `m2m_changed`: `src_pk` is a `SqlValue`

`M2mChangedContext::src_pk` changed from `i64` to `SqlValue`. Compare with
`SqlValue::I64(n)`, and log it with `?ctx.src_pk`.

### UPDATE validates field rules

Updates now fail with `QueryError::MaxLengthExceeded`, `OutOfRange`, `InvalidChoice` or
`ValidatorFailed` where they used to write. `ModelForm` returns these as field errors.

### Template views: typed form and filter values

Form errors for bad input now use the `FormError` text. A `ListView` filter value that
is empty or does not parse as its field type is ignored instead of matching nothing.

### Formsets: at most 1000 rows

`total_forms` / `parse_formset` return `FormSetError::TooManyForms` above
`formset::MAX_FORMS`. `FormSetError` is `#[non_exhaustive]`: add a `_ =>` arm to matches.

## 0.59.8

### Admin audit log is permission-gated

**Breaking:** grant `audit.view` (read) or `audit.delete` (cleanup) to non-superusers
who used the feed; `auto_create_permissions_pool` seeds both codenames.

### Tenant admin requests carry `AdminSession`

**Breaking:** tenant-admin non-superusers get 403 on translation edits. Custom views
reading `Extension<AdminSession>` now see the tenant user instead of nothing.

### Queryset hooks apply beyond the list

**Breaking:** a `register_admin_queryset!` hook now also limits by-pk pages, actions,
autocomplete, facets and inline child rows; rows it filters out are 404 there, and
skipped by actions.

### Hidden admin fields are not written

**Breaking:** an admin create now omits `editable = false` fields and fields outside
`fieldsets`, so a NOT NULL one needs a `default`, as `readonly_fields` already did.
A natural (non-auto) primary key left out of `fieldsets` can no longer be set on create.

### `count()` respects `limit`, `offset`, `distinct` and `union`

`qs.limit(10).count()` now returns at most 10. `CountQuery` and `AggregateQuery` gain a
public `source` field; build them with `new`, `CountQuery::from_select` or `AggregateQuery::over_select`.

### `Sum` over float and decimal columns

It now decodes as `f64` (float) or `Decimal` (decimal), not `i64`; `sum::<i64>` on such a column fails.

### `bulk_insert_pool` joins an outer `atomic()`

Inside `atomic()` on the same pool it now runs in that transaction, whatever its size.
Calling it while holding the block's `AtomicTx` guard returns `NestedAtomic`; drop the guard first.

### `QueryError::RelationPathTooDeep`

New variant: a relation span or `select_related` path longer than 6 hops is refused.

## 0.59.7

### `JwtBackend` tokens need a `tenant` claim (#1848)

**Breaking:** under `require_auth` a token from `JwtBackend::issue` (no tenant) is refused.
Mint with `issue_for_tenant(user_id, slug)` or the `JwtAuth` login instead.

### Single-use auth links need a storing cache (#1853)

**Breaking:** `verify_single_use` and `confirm_password_reset_single_use*` refuse every
link on a `NullCache`. A custom cache should override `add` atomically.

### JWT refresh: new `Config` field, old refresh tokens refused (#1854)

**Breaking:** `auth_routes::Config` gains `refresh_absolute_ttl_secs` (must be > 0, else
`JwtAuth::new` panics) and `refresh_reuse_grace_secs`; exhaustive literals need them or
`..Config::default()`. Every refresh token issued before the upgrade gets one 401, so all
users log in again once. `pwf`, `sat`, `fam` are router claims; a hook returning `kind` fails login.

### `PgJobQueue` counts attempts at pickup

`rustango_jobs.attempt` now includes the running attempt. Keep the
`reclaim_stuck_jobs_pool` threshold well above `heartbeat_interval` (10 s by default).

### ViewSet `fields()` limits writes (breaking)

A body key outside `fields()` is now ignored on create and update, so a required column
left out of `fields()` fails the insert. With `OwnedBy`, a client can no longer set the
owner, and an unauthenticated create or update is `403`. A custom `ViewSetFilter` that
scopes by owner should also implement `write_pins`.

### `?ordering=` with a serializer

Without `ordering_fields`, only the fields the serializer renders are sortable.
`readable_source_fields()` defaults to empty, so a ViewSet on a hand-written
`ModelSerializer` ignores `?ordering=` until the impl overrides it or the ViewSet
sets `ordering_fields`.

### `ApiKeyError` and `HasherError` gain `Busy`

Both are now `#[non_exhaustive]`; add a `_` arm. From async code use
`api_keys::{generate_key,hash_secret,verify_key}_async` and `PasswordHasherChain::{hash,verify}_async`.

### HMAC signing takes the host

`sign_request` and `sign_now` take a `host` argument after `method`, and every signature
changes. Services sharing a key, or behind a proxy that rewrites `Host`, set
`HmacAuthLayer::host`; it panics on an empty or invalid host.

### MySQL: `Uuid` is hyphenated text

The ORM now writes and reads a `Uuid` as the 36-character text its `CHAR(36)` column
holds. A hand-made `BINARY(16)` UUID column no longer works; make it `CHAR(36)`.
On MySQL, `ForeignKey<T, K>` and `Auto<T>` now decode through `FlatScalar`, so `K` / `T`
must be one of its types.

### MySQL: unbounded `String` is `LONGTEXT`

New tables get `LONGTEXT`; migrations do not change existing `TEXT` columns. To lift
the 64 KiB cap there, run `ALTER TABLE t MODIFY col LONGTEXT NOT NULL` for each
`DATA_TYPE = 'text'` column in `information_schema.COLUMNS`. `MODIFY` resets what it
omits: repeat the column's nullability, default and any `COLLATE`.

### MySQL: `check --deploy` warns on a `_ci` or `_bin` database collation

No schema change. A stock MySQL database (`utf8mb4_0900_ai_ci`) now gets a warning;
use `utf8mb4_0900_as_cs` to compare text like PostgreSQL and SQLite.

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
Then `manage seed-permissions --slug <slug>` re-creates them inside the
tenant, the `rustango_api_keys` one included since 0.60.1 (#1731).

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
