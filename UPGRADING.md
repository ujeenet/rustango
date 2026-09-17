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

## 0.57.7

> **Not yet published.** Lives on `release/v0.57.7`. Pin a rev until it
> lands.

The security pass. One change can break a working deployment, and it
does so at runtime rather than at build time — read the first row even
if you skip the rest.

### Breaking, and how to tell whether it reaches you

| Change | How to check |
|---|---|
| **`media_router` is deprecated and now refuses every request with `403`.** Build the router with `media_router_with(manager, authorizer)` and supply a `MediaAuthorizer`. | Grep for `media_router(`. You get a **deprecation warning, not an error** — `cargo build` still succeeds, so a noisy build carries this to production, where the symptom is every media route answering 403. This is deliberate: the constructor used to mount 16 routes that took no authentication, authorization or tenant extractor at all, so the alternative was leaving an open bucket open. |

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
            MediaAction::Add(MediaTarget::NewUpload { .. }) => user.is_trusted_uploader(),
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

Match `NewUpload` as `MediaTarget::NewUpload { .. }`, with the braces.
It is an empty struct variant so that the requested `disk` and
`key_prefix` can be added to it later without breaking your policy.
`NewCollection` and `NewTag` are the same shape, for `POST /collections`
and `POST /tags`. Those two used to arrive as the same `Add(Listing)`,
so "may label things" also granted "may create folders" — and
collections nest, so it granted a foothold under someone else's tree.
`Listing` now means a read.

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

### Not breaking, worth knowing

- `url_codec::percent_decode_path` is new: `%XX` only, `+` left
  literal, for comparing a path segment against what a router decoded.
  `url_decode` keeps form semantics (`+` → space) and is unchanged for
  that use.
- Both decoders stopped treating a **signed** hex pair as an escape.
  `%+5` used to decode to byte `0x05`, because `u8::from_str_radix`
  accepts a leading sign. Only affects malformed input.

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
