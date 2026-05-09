# rustango Cookbook

A feature-by-feature tour of `rustango`. Every section follows the
same shape:

```text
N.M Feature name
────────────────
What:         one-line summary
When:         trigger / use case
API:          file:line citation in rustango
Recipe:       minimal code from cookbook_blog
Verified by:  the test that exercises the recipe
```

The blog app under [src/](src/) is the source of truth — every recipe
quotes from a real, compiling, test-covered file.

## Table of contents

1. [Project shape & manage commands](#chapter-1--project-shape--manage-commands)
2. [Models & schema](#chapter-2--models--schema)
3. [ORM](#chapter-3--orm)
4. [Migrations](#chapter-4--migrations)
5. [Multi-tenancy](#chapter-5--multi-tenancy)
6. [Auth + permissions](#chapter-6--auth--permissions)
7. [Forms + serializer](#chapter-7--forms--serializer)
8. [Admin](#chapter-8--admin)
9. [ViewSets / DRF / OpenAPI](#chapter-9--viewsets--drf--openapi)
10. [Templates + static](#chapter-10--templates--static)
11. [Async / IO / extensions](#chapter-11--async--io--extensions)
12. [Bi-dialect + cross-cutting](#chapter-12--bi-dialect--cross-cutting)
13. [SQLite backend (v0.27 / v0.28)](#chapter-13--sqlite-backend-v027--v028)

---

## Chapter 1 — Project shape & manage commands

### 1.1 `cargo rustango startproject` / `manage startapp`

**What**: Scaffolder that emits the canonical Django-shape project layout.

**When**: Brand-new project, or adding a new sub-app to an existing one.

**API**: [`cargo-rustango`](../../../cargo-rustango/src/main.rs) for `startproject`; [`manage::startapp`](../../src/manage/scaffold.rs) for `startapp`.

**Recipe**: this very project was scaffolded by hand to match the layout `cargo rustango new --template tenant` produces. v0.16's unified `Cli::new()` dispatcher means there is no `src/bin/manage.rs` and no second binary — `cargo run` is `runserver`, `cargo run -- <verb>` is everything else.

**Polished output (v0.28.3, #63)**: `manage startapp <name>` ships:

- A **singularized starter model** — `startapp posts` produces `pub struct Post` on table `"post"`. Conservative trailing-`s` strip on names of length ≥ 5 (`comments → comment`, `users → user`); `news` / `address` / `bus` / short names stay untouched. Rename the struct or table literal freely.
- An `admin(...)` config block (`list_display = "name, active, created_at"`, `search_fields = "name"`, `ordering = "-created_at"`) so the list view is usable out of the box.
- A `created_at: DateTime<Utc>` field with `#[rustango(auto_now_add)]` — Django convention.
- A `starter_model_registered_in_inventory` smoke test in `tests.rs` asserting the model lands in `inventory::iter::<ModelEntry>` (the canonical signal that the auto-admin will pick it up).
- Doc comments calling out that `permissions = true` is the default and the four CRUD codenames (`{table}.add`, `.change`, `.delete`, `.view`) are auto-seeded by `auto_create_permissions` during the next `migrate`.

```text
cookbook_blog/
├── Cargo.toml                  -- one [package], one binary
├── src/
│   ├── main.rs                 // rustango::main + Cli::new().tenancy().api(...).run()
│   ├── settings.rs             // config/{default,test}.toml loader
│   └── apps/
│       ├── tenants/{models.rs, urls.rs, views.rs, admin.rs, mod.rs}
│       ├── auth/...
│       ├── blog/...
│       └── ...
├── migrations/0001_*.json
├── config/{default,test}.toml
└── tests/cookbook_chapter*.rs
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::layout_matches_django_shape`

---

### 1.2 `cargo run -- migrate` / `migrate <target>` / `downgrade [N]`

**What**: Apply / rewind / point-target migrations against a `Pool`.

**When**: Boot, deploy, local dev — anywhere schema needs to track code.

**API**: [`manage::Cli`](../../src/manage.rs) wraps [`migrate::runner`](../../src/migrate/runner.rs).

**Recipe** ([src/main.rs](src/main.rs)):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .tenancy()
        .api(apps::api())
        .run().await
}
```

* `cargo run -- migrate` applies pending; `cargo run -- migrate <target>` walks forward or back to a named target (`zero` unapplies everything); `cargo run -- downgrade [N]` rolls back N steps (default 1).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_help_works_without_database_url`

---

### 1.3 `cargo run -- makemigrations [name] [--empty] [--dry-run]`

**What**: Diff the current model registry against the latest snapshot and emit a new migration JSON.

**When**: After a model change.

**API**: [`migrate::make`](../../src/migrate/make.rs)

**Recipe**: forwarded by `Cli::run()` to `migrate::manage::run`. `cargo run -- makemigrations [<name>]`. `--dry-run` prints the planned ops without writing; `--empty <name>` emits a stub for hand-written `Operation::Data` work (e.g. `RenameTable`).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_makemigrations_verb`

---

### 1.4 `cargo run -- check`

**What**: Static configuration sanity check — model registry + settings + migration ledger.

**When**: CI gate before running tests.

**API**: [`migrate::manage::run`](../../src/migrate/manage.rs) `check` arm.

**Recipe**: `cargo run -- check` — runs the bundled checks (system warnings, unapplied migrations).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_check_verb`

---

### 1.5 `cargo run` (no args) — runserver via `Cli`

**What**: Default verb. Opens the pool, applies migrations, mounts the user's API router, serves on `RUSTANGO_BIND` (default `0.0.0.0:8080`). Tenancy variant defers to [`server::Builder`](../../src/server/mod.rs) which wires the apex/subdomain host split + operator console.

**When**: Always — replaces hand-rolled axum wiring AND the second `manage` binary projects used to write.

**API**: [`manage::Cli::run`](../../src/manage.rs)

**Recipe** ([src/main.rs](src/main.rs)) — same one-liner as §1.2.

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_no_args_dispatches_to_runserver`

---

### 1.6 `cargo run -- create-operator` / `create-user` (tenancy)

**What**: Bootstrap an Operator (registry-side admin) or tenant User. Argon2-hashes the password and inserts into the right table.

**When**: First boot of a fresh database; new tenant onboarding.

**API**: [`tenancy::manage::run`](../../src/tenancy/manage/mod.rs) `create-operator` / `create-user` arms (forwarded by `Cli::tenancy().run()`).

**Recipe**: `cargo run -- create-operator admin --password letmein` then `cargo run -- create-user acme alice --password hunter2 --superuser`.

**First-user auto-superuser (v0.27.6)**: when `create-user <slug> <name>` runs and `rustango_users` for that tenant is empty, the new row is forced `is_superuser = true` regardless of `--superuser`. This avoids the cold-start trap where the first onboarded user lands on an admin index with an empty sidebar (no perms granted, no role assignments yet). A `note: auto-promoted because first user of tenant` line is emitted to stderr so onboarding scripts can detect the promotion.

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_create_operator_verb`

---

### 1.6b Recovery + setup CLI verbs (v0.27.6+)

**What**: Six verbs the v0.16 unified `Cli` dispatches into `tenancy::manage::run` for password / superuser / pool maintenance.

| Verb | Purpose |
|---|---|
| `create-superuser <slug> <username> --password <pw>` | Create user + force `is_superuser = true`. Sugar for `create-user <slug> <username> --password <pw> --superuser`. |
| `set-superuser <slug> <username> [--off]` | Flip an existing user's `is_superuser` flag. `--off` revokes. No password change. |
| `reset-password <slug> <username> --password <pw>` | Argon2-rehash + write to `rustango_users.password_hash`. |
| `reset-operator-password <username> --password <pw>` | Same, but for the registry-side `rustango_operators` table. |
| `migrate --fake <name>` | Insert a row into `__rustango_migrations__` without running the SQL. Drift-recovery for environments where the schema is already at that revision. |
| `prewarm-pools` | Iterate every active database-mode tenant in `rustango_orgs`, build its `PgPool` once, cache it. Optional warm-up that pays the TCP/TLS/auth cost up-front instead of on the first request. Bounded by [`TenantPoolsConfig::max_cached_database_pools`]. |

**API**: [`tenancy::manage::users::create_superuser_cmd`](../../src/tenancy/manage/users.rs) / `set_superuser_cmd` / `reset_password_cmd` / `reset_operator_password_cmd`; [`tenancy::manage::migrations::fake_apply_to_registry`](../../src/tenancy/manage/migrations.rs); [`TenantPools::prewarm_database_tenants`](../../src/tenancy/pools.rs).

**Recipe**:

```sh
# Recover a forgotten password without touching the DB:
cargo run -- reset-password acme alice --password rotated-password

# Promote without re-creating:
cargo run -- set-superuser acme alice

# Mark a manually-applied migration as ledgered (drift recovery):
cargo run -- migrate --fake 0007_add_audit_log

# Warm pools at boot (pair with TenantPoolsConfig { prewarm_active_tenants: true }):
cargo run -- prewarm-pools
```

**Verified by**: framework unit tests in `tenancy::manage::users::tests`; pool tests in `tests/pools_live.rs`.

---

### 1.6c Dev-iteration verbs (v0.29 — #82, #84a, #61, #84b)

**What**: Four verbs that close the dev-loop friction surfaced by
the 2026-05 batch. None of them touch applied rows; each is safe to
run unattended and idempotent or refuse-on-conflict.

| Verb | Purpose |
|---|---|
| `make:api_routes <app> [--tenant]` | Scaffold `src/<app>/api_routes.rs` — the per-app composer that `.merge(...)`-es every viewset's router into a single `Router<()>`. `--tenant` emits the no-arg shape (each viewset resolves its own per-request connection); default emits the `pool: PgPool` shape. Refuses to overwrite existing files. |
| `forget-pending <name>` | Delete a single un-applied migration JSON so the next `makemigrations` regenerates against current models. Accepts exact name or unique substring; refuses if the named migration is already in the ledger. |
| `migrate --squash` | Delete every pending JSON and re-run `makemigrations` to produce a single fresh diff. Dev-iteration escape hatch when an evolving model produces a migration the validator rejects (e.g. `AddColumn NOT NULL no default`). Refuses with zero pending or only one pending (`forget-pending` is the right verb for the single-file case). |
| `seed-permissions [--slug <s>]` | Re-run `auto_create_permissions` against one (`--slug`) or every active tenant. Idempotent — `UNIQUE (content_type_id, codename)` makes re-running on a populated catalog a no-op. Useful after adding `#[rustango(permissions)]` to a model without a fresh migrate cycle. |

**API**:
[`migrate::manage::make_api_routes_cmd`](../../src/migrate/manage.rs),
[`migrate::manage::forget_pending_cmd`](../../src/migrate/manage.rs),
[`migrate::manage::migrate_squash`](../../src/migrate/manage.rs),
[`tenancy::manage::roles::seed_permissions_cmd`](../../src/tenancy/manage/roles.rs).

**Recipe**:

```sh
# Drop a fresh per-app api_routes.rs + start adding viewsets:
cargo run -- startapp regions
cargo run -- make:api_routes regions --tenant
cargo run -- make:viewset CountryViewSet --model Country --tenant
# Then in src/regions/api_routes.rs uncomment / add:
#   .merge(super::viewsets::country::viewset().tenant_router("/api/countries"))

# Got an `AddColumn NOT NULL no default` rejection on a fresh table?
cargo run -- migrate --squash
# (deletes pending JSONs, regenerates one fresh diff via makemigrations)

# Or surgical: drop one named pending JSON and re-diff:
cargo run -- forget-pending 0003_auto_20260509
cargo run -- makemigrations

# Add `#[rustango(permissions)]` to an existing model without a
# fresh migrate cycle:
cargo run -- seed-permissions             # every active tenant
cargo run -- seed-permissions --slug acme # one tenant only
```

**Verified by**: scaffold/template tests in `crates/rustango/src/migrate/manage.rs::gen_tests`; `forget-pending` end-to-end via the validator-rejection recovery flow.

---

### 1.7 `embed_migrations!` macro

**What**: Compile-time embed of the `migrations/` JSON files as `&'static [Migration]` so binaries ship with no filesystem dependency.

**When**: Distributing a single static binary that owns its schema.

**API**: [`rustango_macros::embed_migrations!`](../../../rustango-macros/src/lib.rs)

**Recipe** ([src/main.rs](src/main.rs)):

```rust
const EMBEDDED: &[rustango::migrate::Migration] =
    rustango::embed_migrations!("migrations");
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::embedded_migrations_are_nonempty`

---

### 1.8 Settings layering (`default.toml` → `<env>_settings.toml` → env vars)

**What**: Tiered TOML config loader (#87, v0.29). Three layers, last writer wins:

1. `config/default.toml` — required. Shared knobs across every environment.
2. `config/<RUSTANGO_ENV>_settings.toml` — tier overlay (`dev_settings.toml`,
   `staging_settings.toml`, `prod_settings.toml`). The legacy `<env>.toml`
   shape (pre-v0.29) still loads when no `_settings` variant exists; the
   `_settings` form wins when both are present.
3. `RUSTANGO__SECTION__KEY=value` env vars — final override. Double
   underscore is the path separator (`RUSTANGO__DATABASE__URL` overrides
   `[database] url`).

**When**: Per-environment differences (dev/staging/prod) without code changes,
or when secrets need to come from a secrets manager rather than version control.

**API**: [`config::Settings::load_from_env`](../../src/config/mod.rs),
[`Settings::load`](../../src/config/mod.rs),
[`Settings::current_env_tier`](../../src/config/mod.rs),
[`Settings::detected_features`](../../src/config/sections.rs).

**Recipe**:

```rust
// Reads RUSTANGO_ENV (defaults to "dev"), runs the layered load:
let cfg = rustango::config::Settings::load_from_env()?;

// Or explicit tier:
let cfg = rustango::config::Settings::load("prod")?;

// What tier did we land on?
let tier = rustango::config::Settings::current_env_tier();

// Compile-time feature reflection (telemetry, version pages):
let feats = rustango::config::Settings::detected_features();
// → ["postgres", "tenancy", "admin", "manage", "config", ...]
```

**Wiring into `Cli` (v0.29)**:

```rust
// One-liner that loads via load_from_env() + applies. Falls back
// to Cli defaults (with a tracing::warn) if config files are missing,
// so projects that don't use the layered loader still build cleanly.
rustango::manage::Cli::new()
    .with_settings_from_env()
    .api(urls::api())
    .run().await

// Or explicit Settings handle (when you also want to read other sections):
let cfg = rustango::config::Settings::load_from_env()?;
my_setup(&cfg);
rustango::manage::Cli::new()
    .with_settings(&cfg)
    .api(urls::api())
    .run().await
```

Today `with_settings` consumes:

- **`Settings.server.bind`** — bind address. Resolution: explicit
  `.bind(...)` after `.with_settings(...)` → `RUSTANGO_BIND` env →
  `Settings.server.bind` → hardcoded `0.0.0.0:8080`.
- **`Settings.routes`** (tenancy projects) — pick the preset
  (`legacy_preset = true` → `RouteConfig::legacy()`, otherwise the
  friendly `default()`), then layer per-field overrides
  (`login_url`, `admin_url`, …) on top. An explicit `.routes(rc)`
  call BEFORE `.with_settings(...)` is preserved as the base, so
  TOML overrides layer on top of any code-side construction.

Other sections expose typed entry points so any subsystem can
consume the relevant slice without depending on the whole struct:

```rust
let cfg = rustango::config::Settings::load_from_env()?;

// auth_routes — access_ttl_secs / refresh_ttl_secs
let auth = rustango::tenancy::auth_routes::Config::default()
    .with_jwt_settings(&cfg.auth.jwt);
api.merge(rustango::tenancy::auth_routes::jwt_router(auth));

// security_headers — preset + csp + hsts override
let sec = rustango::security_headers::SecurityHeadersLayer::from_settings(&cfg.security);
let app = app.layer(sec.into_layer());

// CORS — empty list = skip, "*" = permissive, otherwise allowlist
if let Some(cors) = rustango::cors::CorsLayer::from_settings(&cfg.security) {
    let app = app.layer(cors.into_layer());
}

// access_log — extends the redact list with project additions
let log = rustango::access_log::AccessLogLayer::default()
    .with_audit_settings(&cfg.audit);   // redact_query_params extras
let app = app.access_log(log);

// body_limit — opt-in (returns None when max_body_bytes is unset)
if let Some(layer) = rustango::body_limit::BodyLimitLayer::from_settings(&cfg.server) {
    let app = app.body_limit(layer);
}

// cache backend selection — "memory" / "redis" / "null" / unset
let cache: rustango::cache::BoxedCache = rustango::cache::from_settings(&cfg.cache);

// mailer backend selection — "console" / "memory" / "null" / "smtp"
let mailer: rustango::email::BoxedMailer = rustango::email::from_settings(&cfg.mail);
```

The operator console **automatically** picks up `[brand]` from the
loaded settings at boot — no wiring call needed. Resolution
priority: defaults → `Settings.brand.*` (TOML) → `RUSTANGO_OPERATOR_*`
env vars (which still win for deploy-time overrides). Empty strings
in TOML skip (so `name = ""` falls through to the default rather
than rendering as a blank brand name); invalid hex / theme_mode
values are dropped.

Future fields land here as the wiring catches up — every
`Settings` field is `Option`-typed (missing keys fall through,
don't reset).

**Sections** (every field is `Option<T>` with sensible defaults):

```toml
[database]              # url, pool_min_size, pool_max_size
[admin]                 # allowed_tables, read_only_tables
[server]                # bind, request_timeout_secs, max_body_bytes
[auth]                  # argon2 cost, lockout threshold/duration
[auth.jwt]              # access_ttl_secs, refresh_ttl_secs, issuer, audience
[brand]                 # name, tagline, logo_url, primary_color, theme_mode
[security]              # headers_preset, csp, hsts_max_age_secs, cors_allowed_origins
[routes]                # legacy_preset + per-field URL prefix overrides
[audit]                 # retention_days, redact_query_params
[tenancy]               # apex_domain
[cache]                 # backend, redis_url
[jobs]                  # backend, concurrency
[mail]                  # backend, smtp_host, from_address
```

The scaffolder writes all four files (`default.toml` + the three tier
overlays) when you run `cargo rustango new <name>`. A fresh
`cargo run` works without env vars (tier defaults to `dev`).

**Deploy audit**: `cargo run -- check --deploy` flags dev-defaults left
in the prod tier — `headers_preset = "dev"`, `hsts_max_age_secs = 0`,
`argon2_memory_kib < 19456`, `access_ttl_secs > 3600`, loopback bind, etc.

**Verified by**: `tests/cookbook_chapter01_manage.rs::settings_layer_resolves_env_overrides`

---

### 1.9 `rustango::main` macro

**What**: Tokio runtime boot + tracing-subscriber wire-up in one attribute.

**When**: Every `main`.

**API**: [`rustango::main`](../../../rustango-macros/src/lib.rs)

**Recipe** ([src/main.rs](src/main.rs)):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> { ... }
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::main_macro_compiles_and_boots`

---

### 1.10 Welcome page

**What**: Default `GET /` landing page when no route is registered.

**When**: First boot of a brand-new project — confirms the server is alive.

**API**: [`welcome`](../../src/welcome.rs)

**Recipe**: handled by `Builder::serve` automatically when no `/` route is mounted; replaced by the first user-defined `Router::route("/", ...)`.

**Verified by**: `tests/cookbook_chapter01_manage.rs::welcome_page_renders_on_fresh_router`

---

## Chapter 2 — Models & schema

Models live in [src/apps/blog/models.rs](src/apps/blog/models.rs).
Live tests against docker PG in
[tests/cookbook_chapter02_models.rs](tests/cookbook_chapter02_models.rs).
Run with `DATABASE_URL=... cargo test --test cookbook_chapter02_models -- --test-threads=1`.

### 2.11 / 2.12 `#[derive(Model)]` + `Auto<i64>` / `Auto<i32>`

**What**: Derive macro registers the struct with the global inventory and emits `objects()` / typed save / FromRow impls. `Auto<T>` PKs translate to `BIGSERIAL` (i64) / `SERIAL` (i32); the macro skips them on INSERT and assigns the returning value.

**API**: [`rustango::Model`](../../src/lib.rs#L610) (re-exported from [`rustango_macros`](../../../rustango-macros/src/lib.rs)); [`rustango::sql::Auto`](../../src/sql/auto.rs).

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    // ...
}
```

**Verified by**: `tests/cookbook_chapter02_models.rs::save_assigns_auto_pk`

---

### 2.13 `Option<T>` → nullable column

**What**: Wrap any field type in `Option<T>` and the column becomes `NULL`-able; `None` round-trips as SQL `NULL`.

**Recipe**: `pub bio: Option<String>` on `Author`.

**Verified by**: `option_field_round_trips_null`

---

### 2.14 `#[rustango(default = "...")]` + 2.29 `auto_now_add`

**What**: `default = "expr"` emits `DEFAULT <expr>` in DDL. The mixin `auto_now_add` is sugar for "wrap in `Auto<T>` + DB DEFAULT NOW()" so the column auto-fills on INSERT and the macro skips it.

**Recipe**: `#[rustango(auto_now_add)] pub joined_at: Auto<chrono::DateTime<chrono::Utc>>`.

**Verified by**: `auto_now_add_assigns_at_insert`

---

### 2.15 `#[rustango(unique)]`

**What**: Per-column UNIQUE constraint. Duplicate inserts fail with a SQL unique-violation error.

**Recipe**: `#[rustango(unique, max_length = 200)] pub email: String`.

**Verified by**: `unique_constraint_rejects_duplicates`

---

### 2.16 `#[rustango(min = N, max = M)]` → CHECK + client validation

**What**: Defense in depth — the macro adds a CHECK constraint to DDL **and** a client-side range validator. `save()` rejects out-of-range values before the round-trip with `ExecError::OutOfRange`.

**Recipe**: `#[rustango(min = 1, max = 5)] pub score: i64`.

**Verified by**: `min_max_check_rejects_out_of_range`

---

### 2.17 `#[rustango(max_length = N)]`

**What**: String columns become `VARCHAR(N)` instead of `TEXT`. Without it, plain `String` is `TEXT`.

**Recipe**: `#[rustango(max_length = 80)] pub name: String`.

**Verified by**: implicit (every `cookbook_*` table uses VARCHAR for max_length fields).

---

### 2.18 `#[rustango(index)]` (field-level)

**What**: Single-column index on the field. `index(unique)` for unique-indexes, `index(name = "...")` to override the auto name.

**Recipe**: `#[rustango(fk = "cookbook_author", index)] pub author_id: i64`.

**Verified by**: `fk_column_round_trips`

---

### 2.18b `#[rustango(unique_together = "col1, col2")]` — composite UNIQUE

**What**: Container-level Django-shape `unique_together`. Emits `CREATE UNIQUE INDEX <table>_<col1>_<col2>_uq ON <table> (col1, col2)` so the DB rejects duplicate pairs even though neither column on its own is unique. Sister attr `index_together = "..."` for non-unique composite indexes. Both auto-derive the index name from the column list (override pending — see [v0.19 roadmap](../../../../../.claude/projects/-Users-ievgeniisvyryd-projects-rustango/memory/v019-unique-together.md)).

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_membership",
    unique_together = "org_id, user_id",
)]
pub struct Membership {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub org_id: i64,
    pub user_id: i64,
    pub role: String,
}
```

**Verified by**: `unique_together_emits_composite_unique_index_in_schema`,
`unique_together_rejects_duplicate_pair`

**Caveat**: today the duplicate surfaces as the raw Postgres
`duplicate key value violates unique constraint "..."` message. A
DRF-style `UniqueTogetherValidator` that pre-checks at form
validation time and emits friendly per-field errors is tracked as
v0.19.1.

**Also during this slice** — the legacy container-level
`#[rustango(index = "col1, col2", unique, name = "...")]` syntax was
found unparseable (the trailing-flag block didn't compose under the
syn `parse_nested_meta` API). Removed the broken trailing-flag block;
`index = "..."` is now bare-only (composite, non-unique).

---

### 2.20 `#[rustango(fk = "table")]` — basic foreign key

**What**: Adds a `BIGINT` FK column and a `REFERENCES <table>(id)` constraint. The `on = "..."` sub-attr overrides the target column name. See also Chapter 17 `fk = "self"` for tree shapes.

**Recipe**: `#[rustango(fk = "cookbook_author", index)] pub author_id: i64`.

**Verified by**: `fk_column_round_trips`

---

### 2.26 `serde_json::Value` → JSONB

**What**: Field of type `serde_json::Value` becomes a `JSONB` column. Nested structures round-trip without manual encoding.

**Recipe**: `pub metadata: serde_json::Value`.

**Verified by**: `jsonb_field_round_trips_structured_data`

---

### 2.28 `chrono::DateTime<Utc>` / `Option<DateTime>` → TIMESTAMPTZ

**What**: Maps to `TIMESTAMPTZ`. `Option<DateTime>` is nullable.

**Recipe**: `pub published_at: Option<chrono::DateTime<chrono::Utc>>`.

**Verified by**: `datetime_option_round_trips`

---

### 2.21 `#[rustango(o2o = "table")]` — one-to-one (UNIQUE FK)

**What**: Same shape as `fk` but enforces a UNIQUE constraint so the relation is 1:1. Duplicate inserts into the FK column fail.

**Recipe**: `#[rustango(o2o = "cookbook_author")] pub author_id: i64` on `AuthorProfile`.

**Verified by**: `o2o_unique_fk_rejects_duplicate`

---

### 2.22 `#[rustango(m2m(name, to, through, src, dst))]` — M2M through

**What**: Container-level attribute that emits a junction-table accessor `<name>_m2m()`. The macro doesn't auto-create the through table; you create it by adding a regular junction model + migration. Reads/writes go through the junction table directly.

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[rustango(
    table = "cookbook_post",
    m2m(name = "tags", to = "cookbook_tag",
        through = "cookbook_post_tag",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post { ... }
```

**Verified by**: `m2m_through_junction_table_round_trips`

---

### 2.27 `Auto<uuid::Uuid>` + `auto_uuid` — UUID PKs

**What**: `#[rustango(auto_uuid)]` is sugar for `primary_key + auto + DEFAULT gen_random_uuid()`. Postgres' `pgcrypto` extension supplies the v4. Macro skips the column on INSERT; the returning value lands in `Auto<Uuid>`.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(table = "cookbook_session")]
pub struct Session {
    #[rustango(auto_uuid)]
    pub id: Auto<Uuid>,
    pub user_token: String,
}
```

**Verified by**: `auto_uuid_assigns_server_side_uuid`

---

### 2.30 `#[rustango(soft_delete)]` — tombstone deletes

**What**: Mark an `Option<DateTime<Utc>>` field as the soft-delete tombstone. Currently captured in the model's `SCHEMA.soft_delete_column` so the ORM can layer the alive-when-NULL filter and override `delete()` to UPDATE the tombstone instead of `DELETE FROM`.

**Recipe**:

```rust
#[derive(Model)]
pub struct ArchiveNote {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub note: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}
```

**Verified by**: `soft_delete_column_round_trips_and_deleted_at_defaults_null`

---

### 2.19 `#[rustango(check(name, expr))]` — table-level CHECK

**What**: Container-level CHECK constraint with a chosen name and a raw SQL boolean expression. Rejects inserts that violate the predicate at the DB.

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_inventory_item",
    check(name = "cookbook_inventory_item_qty_chk",
          expr = "qty >= 0 AND price_cents > 0"),
)]
pub struct InventoryItem { ... }
```

**Verified by**: `table_level_check_rejects_invalid_row`

---

### 2.23 `#[rustango(fk_composite(name, to, from, on))]` — composite FK

**What**: Multi-column foreign key. The `from = (...)` columns on this model reference the `on = (...)` columns on the target table. The DDL emits a single `CONSTRAINT … FOREIGN KEY (a, b) REFERENCES tgt (x, y)` so the DB rejects unmatched pairs.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_pair_link",
    fk_composite(
        name = "pair_target_fk",
        to = "cookbook_pair_target",
        from = ("left_ref", "right_ref"),
        on = ("a_id", "b_id"),
    ),
)]
pub struct PairLink { ... }
```

**Verified by**: `fk_composite_rejects_unmatched_pair`

---

### 2.24 / 2.25 `#[rustango(generic_fk(name, ct_column, pk_column))]` + ContentType lookup

**What**: Generic foreign key — pairs a `target_content_type_id BIGINT` column with a `target_object_pk BIGINT` column. The framework knows the pair logically points at "any registered model's row." `Model::SCHEMA.generic_relations` exposes the metadata; admin uses it to render clickable links via `render_generic_fk_link`.

`ContentType::for_model::<T>()` looks up the ContentType row for any registered model after `ensure_seeded(&pool)` populates the registry.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_activity",
    generic_fk(
        name = "target",
        ct_column = "target_content_type_id",
        pk_column = "target_object_pk",
    ),
)]
pub struct Activity { ... }
```

```rust
ensure_seeded(&pool).await?;
let ct = ContentType::for_model::<Author>(&pool).await?.unwrap();
let mut act = Activity {
    target_content_type_id: ct.id_value()?,
    target_object_pk: author_id,
    action: "viewed".into(), ..
};
act.save(&pool).await?;
```

**Verified by**: `generic_fk_schema_and_content_type_lookup`

## Chapter 3 — ORM

13 live recipes against the Author / Post fixture from Chapter 2.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter03_orm -- --test-threads=1`.

* §3.31 `Post::objects().filter("published", Op::Eq, true).fetch(&pool)` →
  `filter_eq_fetch_returns_matching_rows`
* §3.34 `Op::Gt` / `Op::Lt` / `Op::ILike` / `Op::In` / `Op::Between` /
  `Op::IsNull` — six tests covering the full Op surface.
* §3.35 `.order_by(&[("col", desc)])` (`true = DESC`, `false = ASC`) →
  `order_by_view_count_desc`
* §3.36 `.limit(N).offset(M)` for pagination → `limit_offset_paginates`
* §3.37 `.aggregate().annotate("alias", AggregateExpr::Count|Sum|Avg|...)`
  + `fetch_aggregate(&q, &pool)` → `Vec<HashMap<String, SqlValue>>`
  → `aggregate_count_and_sum`
* §3.42 `model.save(&pool)` does INSERT (PK Unset) or UPDATE (PK Set) →
  `save_inserts_then_updates_in_place`
* §3.46 raw `sqlx::query_scalar / query_as` for SQL the QuerySet
  doesn't cover → `raw_sql_escape_via_sqlx`
* §3.47 manual `pool.begin() ... tx.rollback()` — atomic rollback on
  UNIQUE violation → `manual_transaction_rolls_back_on_error`
* §3.48 PG JSONB `@>` containment operator on the `metadata` column →
  `json_operator_on_jsonb_column`

**Framework bug fixed during this slice**: `SUM(BIGINT)` returns
PostgreSQL `NUMERIC` and `AVG(BIGINT)` returns `NUMERIC` — the
aggregate row decoder only tries `i64`/`i32`/`f64`/`bool`/`String`,
so the result silently came back as `SqlValue::Null`. Added
`Dialect::cast_aggregate_to_int` / `cast_aggregate_to_float` (PG
emits `::bigint` / `::double precision`; MySQL emits `CAST(.. AS
SIGNED)` / `CAST(.. AS DOUBLE)`). Aggregate writer wraps SUM/AVG via
the new methods.

*Sub-sections 3.32 (get/get_on/fetch_on), 3.33 (OR-nested),
3.38 (annotate), 3.39 (prefetch FK), 3.40 (prefetch_soft),
3.41 (prefetch_generic), 3.43 (bulk_insert), 3.44 (bulk UPDATE),
3.45 (WhereExpr::Not), 3.49 (_pool executor variants) queued for
Slice 3b.*

## Chapter 4 — Migrations

5 live recipes against docker PG verifying the migration lifecycle.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter04_migrations -- --test-threads=1`.

* §4.51 / 4.53 / 4.54 / 4.61 — `migrate(&pool, &dir)` applies pending,
  `unapply(&pool, &dir, name)` rolls back. Verifies schema catalog
  before + after. → `apply_then_unapply_round_trip`
* §4.55 / 4.57 / 4.58 — `Migration { name, prev, scope, atomic, snapshot,
  forward: Vec<Operation> }` with `Operation::Schema(SchemaChange)` +
  `Operation::Data(DataOp { sql, reverse_sql, reversible })`.
  Externally-tagged JSON: `{"schema": {"CreateTable": "..."}}` /
  `{"data": {"sql": "...", "reverse_sql": "...", "reversible": true}}`.
  → `migration_serde_round_trips_schema_and_data_ops`
* §4.56 — `embed_migrations!("migrations")` baked into the binary at
  compile time; `EMBEDDED.iter()` enumerates the migrations the
  current cookbook ships with. → `embedded_migrations_const_is_loaded`
* §4.59 — `AlterColumnType` / `AlterColumnNullable` /
  `AlterColumnDefault` (and friends) round-trip via the same
  externally-tagged JSON. → `alter_column_ops_serialize_with_external_tag`
* §4.64 — `AddCompositeFk { table, name, to, from, on }` /
  `DropCompositeFk { table, name }` (v0.15-F.5b) round-trip.
  → `composite_fk_ops_serialize_round_trip`

*Sub-sections 4.51b (make_migrations from inventory diff),
4.52 (per-app), 4.60 (rename), 4.65 (per-app ledger) queued for
Slice 4b.*

## Chapter 5 — Multi-tenancy

One comprehensive live test that provisions two tenants in schema
mode, then exercises every resolver against the seeded registry.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter05_tenancy -- --test-threads=1`.

* §5.66 `SubdomainResolver::new(apex)` — extracts `acme` from
  `acme.cookbook-test.local` then DB-loads the matching `Org`.
* §5.68 `HeaderResolver::default()` — reads `X-Org` header slug.
* §5.70 `ChainResolver::new().push(...).push(...)` — first hit wins
  (Subdomain → Header fallback when no subdomain present).
* §5.71 schema-per-tenant — `create_tenant_if_missing(...)` creates a
  PG schema named after the slug + INSERTs the `Org` row + applies
  tenant-scoped migrations.
* §5.73 `TenantPools::pool_for_org(&org)` — returns a tenant-scoped
  pool whose `search_path` lands on the tenant's schema.
* §5.74 `tenancy::migrate_registry / migrate_tenants` — registry-
  scoped vs tenant-scoped migration passes.

→ `provision_two_tenants_then_resolve_and_lazy_pool`

The tenant Builder + apex/subdomain HTTP host split is exercised
implicitly by the `Cli::new().tenancy().run()` runserver path —
covered by Chapter 1 §1.5 and the framework's own `tenant_admin_live`
test.

* §5.77 **Extras on tenant users** — three escalating options when the
  framework's seven `rustango_users` columns aren't enough:
  1. **JSONB `data` column** — stuff sparse attributes into
     `user.data["display_name"]`. Zero migration, no override. Right
     answer for preferences / onboarding flags.
  2. **Sibling `UserProfile` model with FK** — typed, indexable extras
     without touching `rustango_users`. Define a regular
     `#[derive(Model)]` with `#[rustango(fk = "rustango_users")]`,
     then `cargo run -- makemigrations && cargo run -- migrate`. Works
     on any project, including ones already in production.
  3. **`Cli::user_model::<AppUser>()`** — extras inline on
     `rustango_users` itself (greenfield only).
     `impl rustango::tenancy::TenantUserModel for AppUser {}` on a
     `#[derive(Model)] #[rustango(table = "rustango_users")]` struct
     that mirrors all seven required columns plus your extras, then
     chain `.user_model::<AppUser>()` on `Cli` (or `Builder`).
     `init-tenancy` and `Builder::migrate` then write the bootstrap
     migration with your `CREATE TABLE rustango_users` columns.
     Caveats:
     - Idempotent: only takes effect when the bootstrap JSON
       doesn't already exist. On a `cargo rustango new --template
       tenant` project you must `rm migrations/0001_rustango_*.json`
       before `cargo run -- init-tenancy`.
     - Both framework `User` and `AppUser` register in inventory —
       subsequent `makemigrations` may emit redundant ops touching
       `rustango_users`; review the JSON.
     - Validation (`validate_tenant_user_schema`) panics at
       `init-tenancy` time on wrong table name or a missing required
       column.
  → option 3 covered by `tenancy::bootstrap::tests::user_model_override_*`
    and `tenancy::auth::tests::validate_*` in the framework crate.
  → see [docs/manage.md "Custom user model"](../../../../docs/manage.md#custom-user-model-extra-columns-on-rustango_users)
    for the full step-by-step recipe.

*Sub-sections 5.67 (PathPrefixResolver), 5.69 (PortResolver),
5.72 (database-per-tenant via `--database-url`), 5.75 (per-tenant
auth: Operator vs User scoping), 5.76 (org bootstrap migration
templates) queued for Slice 5b.*

### 5.78 `TenantPoolsConfig` — pool tuning (v0.27.7)

**What**: Knobs on the database-mode pool builder. Pre-0.27.7 every tenant pool was `PgPoolOptions::new().max_connections(N)` and nothing else, leaving sqlx defaults to drive timeouts / lifetimes. Apps hitting slow upstreams (vault-resolved DSNs, distant databases) had no way to tune them without bypassing `TenantPools` entirely.

**When**: Production tenants that get regular traffic and want sub-second first-request latency; deployments behind PG load balancers with `idle_in_transaction_session_timeout`; clouds with rotating IAM credentials.

**API**: [`tenancy::pools::TenantPoolsConfig`](../../src/tenancy/pools.rs).

**Recipe**:

```rust
use rustango::tenancy::TenantPoolsConfig;
use std::time::Duration;

let cfg = TenantPoolsConfig {
    max_cached_database_pools: 64,
    database_pool_max_connections: 8,
    database_pool_min_connections: 1,            // keep one warm
    database_pool_acquire_timeout: Duration::from_secs(10),
    database_pool_idle_timeout: Some(Duration::from_secs(10 * 60)),
    database_pool_max_lifetime: Some(Duration::from_secs(30 * 60)),
    prewarm_active_tenants: true,                // build all on boot
};

let pools = TenantPools::with_config(registry_pool, cfg);
```

Defaults preserve pre-0.27.7 behavior: `min_connections = 0`, `prewarm_active_tenants = false`. The `prewarm-pools` manage verb (§1.6b) runs the same warm-up loop one-shot.

**Verified by**: `tests/pools_live.rs` + `pools::tests::*` unit tests.

---

### 5.79 `RouteConfig` — configurable URL prefixes (v0.28.0)

**What**: One struct that drives every framework-mounted URL prefix on the tenant admin (login, logout, admin, audit, static, brand). Defaults match the legacy `__login` / `__admin` / `__static__` / `__brand__` paths so v0.27 → v0.28 is a no-op upgrade. `RouteConfig::friendly()` flips them all to underscore-free shapes (`/login`, `/admin`, `/audit`, `/_static`, `/_brand`) for projects that prefer Django-style URLs.

**When**: Apps that want public-facing tenant admins on clean paths instead of the framework's `__`-prefixed defaults; or apps hosting a tenant admin alongside their own routes that already use `/admin/...`.

**API**: [`tenancy::routes::RouteConfig`](../../src/tenancy/routes.rs); [`server::Builder::routes`](../../src/server/builder.rs).

**Recipe**:

```rust
use rustango::tenancy::RouteConfig;

let routes = RouteConfig::friendly();   // /login, /admin, /audit, /_static, /_brand
// or pick individually:
// let routes = RouteConfig {
//     login_url: "/sign-in".into(),
//     admin_url: "/control".into(),
//     ..RouteConfig::default()
// };

rustango::server::Builder::new(api_router)
    .routes(routes)
    .serve()
    .await?;
```

Also exposes session TTLs (`tenant_session_ttl`, `operator_session_ttl`, `impersonation_ttl`) and the basic-auth realm string. The full URL builder `audit_full_url()` joins admin + audit prefixes for callers (`/admin/audit` for friendly, `/__admin/__audit` for default).

**Verified by**: `routes::tests::*` (4 unit tests covering defaults, friendly preset, joined audit URL, TTL defaults).

---

### 5.80 Operator-as-superuser tenant impersonation (v0.27.8)

**What**: From the operator console org-edit page (`/orgs/{slug}/edit`), an operator can click "Open admin as superuser →" to get an HMAC-signed cross-domain cookie that logs them into that tenant's admin with implicit superuser rights. No password reset, no shadow account. The tenant admin renders a sticky warning banner ("You are impersonating tenant `acme` as operator `admin` — [End impersonation]") on every page so the privileged context is visible at all times.

**When**: Customer support — operator needs to reproduce a tenant-side bug; admin maintenance — fix a malformed model row in a tenant DB; onboarding — sanity-check a freshly-provisioned tenant before handing it over.

**API**: [`tenancy::tenant_console::TenantSessionPayload::impersonation`](../../src/tenancy/tenant_console.rs); [`operator_console::router_with_impersonation`](../../src/tenancy/operator_console/mod.rs); [`admin::Builder::impersonated_by`](../../src/admin/urls.rs).

**Recipe**: enabled automatically by `server::Builder` when both an operator session secret and a tenant session secret are configured. The operator-side form posts to `/orgs/{slug}/impersonate`; the response sets a slug-pinned tenant cookie with TTL `RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS` (default `3600`). The cookie payload carries an `imp` field (operator user id) — distinguishable from native tenant sessions and audit-logged into `rustango_audit_log` on issue. Clicking "End impersonation" in the banner clears the cookie.

```env
# Optional — override the 1h impersonation cookie TTL:
RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS=900   # 15 min
```

**Verified by**: framework unit tests in `tenant_console::tests` (5 tests covering the `imp` claim round-trip + `is_impersonation()` accessor + TTL defaults).

---

### 5.81 Registry-scope filter on tenant admin (v0.27.7)

**What**: A `tenant_mode()` Builder flag on `admin::Builder` that filters out registry-scoped models (Org, Operator, Permission registry, etc.) from the tenant-side admin sidebar and request resolver. Without it, a tenant superuser would see — and could route to — `Org` / `Operator` rows that live in the registry DB; the request would actually resolve those rows out of the tenant pool's `search_path` fallback, leaking cross-tenant data.

**When**: Always — `server::Builder` sets it for you. Only call manually if you're hand-rolling the inner admin router (e.g. mounting the admin alongside an unusual host shape).

**API**: [`admin::Builder::tenant_mode`](../../src/admin/urls.rs); [`admin::AppState::scope_visible`](../../src/admin/urls.rs); [`ModelScope::Registry` / `Tenant`](../../src/schema/mod.rs).

**Recipe**: opt-out only — most apps don't touch this. The check fires both on inventory-walk (sidebar enumeration) and on URL resolution (`lookup_model`), so a hand-typed `/__admin/rustango_orgs` URL on the tenant side returns 404 instead of leaking registry rows.

**Verified by**: `admin::urls::tests::*` (6 unit tests covering scope filter + `admin_prefix` Builder variants).

---

### 5.82 `admin_prefix` template variable (v0.27.9)

**What**: Every admin Tera template gets `{{ admin_prefix }}` injected (default `/__admin`) so links inside `_sidebar.html` / `index.html` / `form.html` / `detail.html` / `list.html` / `audit_log.html` follow the admin URL chosen by `RouteConfig`. Pre-0.27.9 the templates had hardcoded `/__admin/...` strings that would 404 if the admin was mounted under a different prefix.

**When**: Anyone using `RouteConfig::friendly()` or any custom `admin_url`. The framework keeps `/__admin` as the default so apps that don't override `RouteConfig` see no behavior change.

**API**: [`admin::Builder::admin_prefix`](../../src/admin/urls.rs); [`admin::helpers::chrome_context`](../../src/admin/helpers.rs).

**Recipe**: handled automatically when `server::Builder::routes(...)` flows the prefix through to the inner admin Builder. Custom templates can read `{{ admin_prefix }}/<slug>` directly.

**Verified by**: `admin::urls::tests::*` admin_prefix variants.

---

### 5.83 Users / roles / permissions admin pages (v0.28.1)

**What**: Five framework auth + RBAC tables exposed in the tenant admin: `rustango_users` (already had admin config), `rustango_roles` (already), `rustango_role_permissions`, `rustango_user_roles`, `rustango_user_permissions`. The three junction models picked up `admin(...)` config in v0.28.1 so list pages show useful columns instead of every field raw.

**When**: Operators want to inspect or edit role memberships, role-level codename grants, and per-user overrides without reaching for SQL or the `assign_role` / `grant_role_perm` / `set_user_perm` Rust APIs.

**API**: [`tenancy::permissions`](../../src/tenancy/permissions.rs) — Models `Role`, `RolePermission`, `UserRole`, `UserPermission` plus the `User` model from [`tenancy::auth`](../../src/tenancy/auth.rs).

Plus a **Roles & permissions panel** rendered on the user detail page (`/{admin_url}/rustango_users/{id}`):

- Lists each assigned role with a link to its detail page.
- Lists the user's effective codenames — union of role grants + direct grants minus explicit denials. Computed by the same SQL the runtime [`has_perm`](../../src/tenancy/permissions.rs) check uses, so what you see is what `has_perm` enforces.
- Quick links to the four manage-able junction tables for inline editing of memberships, role-level grants, and per-user overrides.
- Hides itself silently when the permission tables haven't been seeded — same posture as the audit-trail panel.

```sh
# Bootstrap the perm tables on a fresh tenant (idempotent):
cargo run -- create-user acme alice --password hunter2

# Then visit the user detail page; the panel is automatically there.
# Edit role memberships at /__admin/rustango_user_roles
# Edit role-level grants at /__admin/rustango_role_permissions
```

**Verified by**: `tests/admin_user_roles_panel_live.rs::user_detail_page_renders_roles_and_effective_perms` (provisions a user with one role granting two codenames, one direct grant, one explicit denial; asserts the panel renders the role + effective grants and that the denial suppresses the role-granted codename); plus `tenancy::permissions::admin_config_tests` (asserts every junction model carries `admin(...)` and stays in `ModelScope::Tenant`).

**Out of scope (v0.29 follow-ups)**: inline assign/revoke buttons on the user detail panel (currently read-only — manage via junction tables); `rustango_permissions` catalog as an admin page (it has no Rust `Model` today; adding one would diff against existing tenants' bootstrap snapshots — needs a schema-aware migration).

---

### 5.84 Self-serve change-password page + `--generate` (v0.28.2)

**What**: A self-serve change-password flow on the tenant admin (`/__change-password`) — the user enters their current password plus a new one and the framework verifies + rotates without operator involvement. Plus a `change-password` / `change-operator-password` CLI counterpart and a `--generate` flag on every password verb.

**When**: Whenever the user remembers their current password (rotation, periodic refresh, switching from a generated bootstrap password). Operator-driven recovery for locked-out users still uses `reset-password` / `reset-operator-password`.

**API**:
- [`tenancy::routes::RouteConfig::change_password_url`](../../src/tenancy/routes.rs) (default `/__change-password`; `friendly()` → `/change-password`).
- [`admin::Builder::change_password_url`](../../src/admin/urls.rs) — surfaces the link in the standalone admin sidebar; tenant admin Builder threads it through automatically.
- [`tenancy::password::generate(length)`](../../src/tenancy/password.rs) — `OsRng`-backed generator over a 58-character unambiguous alphabet (no `0/O`, `1/l/I`).

**Recipe** (tenant admin):

The form is auto-mounted when `TenantAdminBuilder::with_session(secret)` is wired. The "Change password" link appears in the admin sidebar; the page lives outside the admin URL prefix so it stays a distinct namespace from per-table admin routes.

**Recipe** (CLI):

```sh
# Symmetric — current password verified before rotating:
cargo run -- change-password acme alice
cargo run -- change-operator-password admin

# Operator-driven recovery (no current pw needed):
cargo run -- reset-password acme alice
cargo run -- reset-operator-password admin

# Generate a secure random password — printed once, stored hashed:
cargo run -- create-superuser acme alice --generate
cargo run -- reset-password acme alice --generate
```

`--password` and `--generate` are mutually exclusive on every verb that accepts both.

**Verified by**:
- 3 unit tests in `tenancy::password::tests` (generator length / charset / hash round-trip / uniqueness).
- 3 live tests in `tests/manage_change_password_live.rs` (CLI round-trip, `--generate` prints + verifies, mutually-exclusive flags rejected).
- 4 live tests in `tests/admin_change_password_ui_live.rs` (anonymous → 303 to login; authenticated GET renders form; POST with correct current rotates the hash; POST with wrong current shows error and leaves hash unchanged).

**Out of scope (v0.29 follow-ups)**: operator-driven password reset on a tenant user via the operator console UI (the `reset-password` CLI verb already covers this path; UI sugar deferred); password strength enforcement at the form layer (the `passwords::strength_score` helper exists but isn't wired in).

**Shipped in v0.28.4**: `password_changed_at` column on `User` / `Operator` is stamped to `NOW()` on every password rotation path. The session payload now carries `iat` (issued-at); `validate_session` and `require_session` reject cookies whose `iat` is strictly less than `password_changed_at.timestamp()`. Pre-0.28.4 cookies stay parseable (`#[serde(default)]` on `iat` decodes as `0`) — they're invalidated by any future password change, which is the intended security posture. Verified by `tenant_console::tests::new_payload_stamps_iat_at_construction_time`, `tenant_console::tests::legacy_pre_v0_28_4_cookie_decodes_with_iat_zero`, and the live test `admin_change_password_ui_live::session_minted_before_password_rotation_is_rejected`.

## Chapter 6 — Auth + permissions

7 live tests on the password / API-key / JWT / permission primitives.
No DB needed — pure crypto. Run with `cargo test --test cookbook_chapter06_auth`.

* §6.83 `passwords::hash` (Argon2 with random salt) +
  `passwords::verify` round-trip + `strength_score` issue list.
  → `passwords_hash_and_verify_round_trip`,
  `passwords_strength_score_flags_weak`
* §6.84 `api_keys::generate_key()` → `(token, prefix, hash)`.
  `split_token(token)` → `(prefix, secret)`. `verify_key(secret, hash)`.
  → `api_keys_issue_then_verify`
* §6.85 `jwt::Claims::new(sub).issuer(...).audience(...).ttl(...)` →
  `encode(&claims, secret)` → token. `decode(&token, secret)` rejects
  wrong-secret + tampered tokens; `decode_at(&token, secret, now)`
  rejects expired tokens.
  → `jwt_round_trips_with_ttl_and_subject`,
  `jwt_rejects_wrong_secret_and_tampered_token`,
  `jwt_decode_at_rejects_expired_token`
* §6.78 / 6.79 `permissions::codename_for::<T>("view"|"add"|"change"|"delete")`
  → Django-shape `{app}.{action}_{model}` strings.
  → `permission_codename_for_model_resolves_app_action_model`

*Sub-sections 6.77 (User/Role/Permission models — registry-side, see
framework's tenant_auth_live), 6.80 (ViewSet typed perms),
6.81 (auth backends), 6.82 (auth middleware), 6.86 (sessions),
6.87 (CSRF), 6.88 (HMAC-auth), 6.89 (TOTP), 6.90 (OAuth2),
6.91 (auth_flows), 6.92 (signed URLs) queued for Slice 6b.*

## Chapter 7 — Forms + serializer

6 tests covering `ModelFormFor<T>` parse/from_json/error aggregation/
bound-validation/null handling/insert-query emission. No DB needed.
Run with `cargo test --test cookbook_chapter07_forms`.

* §7.95 `ModelFormFor::<T>::parse(&HashMap<String,String>)` — form-
  encoded payload → `(columns, values)` with per-field bound
  validation. Auto<T> PK and `auto_now_add` columns are skipped
  (DB fills them). → `modelform_parses_form_encoded_into_typed_values`
* §7.95 missing required fields aggregate into `FormErrors`, one
  entry per field. → `modelform_missing_required_fields_aggregate_errors`
* §7.96 `ModelFormFor::<T>::from_json(&serde_json::Value)` — JSON
  request body. Null values on `Option<T>` write explicit
  `SqlValue::Null`. → `modelform_from_json_parses_object`,
  `modelform_from_json_null_writes_explicit_null`
* §7.98 `min`/`max` bounds run at parse time. Out-of-range values
  land in `FormErrors` keyed by field. →
  `modelform_bound_violation_lands_in_form_errors`
* §7.99 `into_insert_query()` emits an `InsertQuery` against the
  model's table. → `modelform_into_insert_query_targets_model_table`

**Framework bug fixed during this slice**: `ModelFormFor::parse`
required EVERY field including `auto_now_add` columns (e.g.
`joined_at: Auto<DateTime<Utc>>`), which the macro skips on INSERT
because `DEFAULT NOW()` fills them. Fix: skip every `auto = true`
field (was: only `auto && primary_key`).

### 7.99b `#[derive(Serializer)]` — DRF-shape JSON façade

6 tests in `tests/cookbook_chapter07b_serializer.rs` exercise the
serializer derive against the cookbook's `Author` model. Run with
`cargo test --test cookbook_chapter07b_serializer`.

* `from_model` + `to_value` round-trip — every field maps from the
  model and lands in the JSON output. → `serializer_from_model_then_to_value_round_trip`
* `#[serializer(read_only)]` — excluded from `writable_fields()`,
  still appears in JSON output. → `read_only_field_omitted_from_writable_fields`
* `#[serializer(write_only)]` — excluded from JSON output, accepted
  on input. → `write_only_field_excluded_from_json_output`
* `#[serializer(source = "x")]` — renames the JSON key to a
  different model field. → `source_attribute_renames_json_key`
* `#[serializer(skip)]` — uses `Default::default()`, leaves the field
  in JSON but excludes it from `writable_fields()`. →
  `skip_field_uses_default_and_appears_in_json_unchanged`
* `many_to_value` — batches a `Vec<Model>` into a JSON array. →
  `many_to_value_batches_into_json_array`

**Framework fix during this slice**: `Auto<T>` was missing an
`OpenApiSchema` impl, so `#[derive(Serializer)]` failed to build with
the `openapi` feature on for any model with `Auto<T>` fields. Added
`impl<T: OpenApiSchema> OpenApiSchema for Auto<T>` (forwards to T's
schema).

**Gaps tracked in [v0.18 DRF parity roadmap](../../../../../.claude/projects/-Users-ievgeniisvyryd-projects-rustango/memory/v018-drf-parity.md)**:

1. **Auto-nested FK** — `#[serializer(nested = AuthorSerializer)]`
   that lazy-loads + nests the parent. Today: manual.
2. **`SerializerMethodField`** — `#[serializer(method = "fn_name")]`
   computed fields. Today: manual after `from_model`.
3. **ViewSet ↔ Serializer wiring** —
   `ViewSet::for_model(...).serializer::<T>()`. Today: ViewSet
   serializes the bare model.
4. **M2M many-relations** —
   `#[serializer(many = TagSerializer, source = "tags")]`. Today: no
   automatic M2M traversal in serializers.
5. **Per-field validators chain** —
   `#[serializer(validate = "fn_name")]` per-field validator
   callable. Today: only model-level `min`/`max`/`max_length`.

*Sub-sections 7.93 (raw `Form` derive), 7.94 (admin's hand-rolled
ModelForm engine), 7.97 (parse_form_value per type), 7.98b (custom
validators) queued for Slice 7c.*

## Chapter 8 — Admin

Two parts: in-process router smoke + a real-browser playwright session.

### Part A — in-process smoke (no socket, no browser)

`tests/cookbook_chapter08_admin.rs` boots `admin::Builder::new(pool)
.build()` and hits routes via `tower::ServiceExt::oneshot`. 2 tests:

* §8.100 / 8.101 `admin_builder_serves_list_page_for_registered_model`
  — `GET /cookbook_author` returns 200 with the table name in the body.
* §8.103 `admin_create_form_renders_input_for_each_writable_field` —
  `GET /cookbook_author/new` returns 200; HTML contains `name="name"`,
  `name="email"`, `name="bio"`; does NOT contain `name="id"` (Auto<i64>
  PK is server-assigned and hidden from the create form).

Run: `DATABASE_URL=... cargo test --test cookbook_chapter08_admin -- --test-threads=1`.

### Part B/C — real-binary HTTP loop (Chapter 8b)

`tests/cookbook_chapter08b_browser_forms.rs` boots the actual
`cookbook_blog` binary against an isolated DB and drives the full
admin form + ViewSet flow over HTTP — the closest a Rust integration
test comes to a real browser session without playwright in the loop.
1 test:

* `admin_form_creates_then_viewset_isolates_per_tenant`:
  - migrate registry → create operator → create acme + globex →
    create alice/tenantpw on acme.
  - spawn `cookbook_blog` on `127.0.0.1:8867`.
  - `POST /__login` as alice (acme tenant).
  - `POST /__admin/cookbook_author` with `name=ada lovelace` etc.
  - `GET /api/authors` (acme) → returns `[{id:1, name:"ada lovelace"}]`.
  - `GET /api/authors` (globex) → returns `[]`.
  - `GET /api/authors` (apex `localhost`) → `404` (no tenant route).

Run: `DATABASE_URL=... cargo test --test cookbook_chapter08b_browser_forms -- --test-threads=1`.

**Framework bug fixed during this slice**: `forms::collect_values`
(used by the admin's hand-rolled create handler) demanded every
non-PK auto field — same shape as the Chapter 7 ModelFormFor bug.
Posting an Author through the admin returned "required field
`joined_at` was missing from the form" because `auto_now_add` columns
should be skipped server-side. Added `field.auto` to the auto-skip
filter alongside the explicit `skip` list.

### Part D — real-browser session (playwright MCP)

Reproducible by hand:

```sh
# Terminal 1 — fresh DB + boot
docker exec shop-postgres-1 psql -U rustango -c "CREATE DATABASE cookbook_browser_dev"
DATABASE_URL=postgres://rustango:rustango@localhost:5432/cookbook_browser_dev \
RUSTANGO_APEX_DOMAIN=localhost \
RUSTANGO_BIND=127.0.0.1:8765 \
RUSTANGO_SESSION_SECRET=cookbook-test-32bytes-cookbook-test-32bytes \
cargo run -- init-tenancy
# (then `migrate`, `create-operator admin --password letmein`,
#  `create-tenant acme --display-name "Acme Inc" --host-pattern acme.localhost`,
#  `create-user acme alice --password tenantpw --superuser`,
#  finally `cargo run`)
```

Verified browser-side via playwright MCP:

* §8.0 `http://localhost:8765/login` — operator login form renders
  with username/password/Sign in. Logging in as `admin / letmein`
  lands on the operator console (sidebar nav: Home, Operators,
  Organizations).
* §8.0 `http://acme.localhost:8765/__login` — tenant login form
  renders titled "Sign in to **acme**". Logging in as `alice /
  tenantpw` lands on the tenant admin index showing every registered
  model split by app group: `apps` (the cookbook's blog/auth/etc.
  models), `contenttypes` (rustango_content_types), `tenancy`
  (rustango_users + friends).

**Surfaced gap**: tenant admin returns a JSON 500 (`relation
"cookbook_author" does not exist`) when a tenant-scoped model's table
isn't materialized in the tenant's schema. The cookbook's models live
in inventory but no `make-migrations` has been run for them, so the
admin shows them in the index then errors on browse. A friendlier
"table not yet migrated — run `migrate-tenants`" message would close
this UX gap. Tracked in Gaps section below.

*Sub-sections 8.102 (detail view), 8.104 (FK display widget),
8.105 (FK search widget), 8.106 (M2M widget), 8.107 (JSONB editor),
8.108 (generic-FK link rendering), 8.109 (basic-auth wrap),
8.110 (custom actions), 8.111 (inline editing) queued for Slice 8b
which would need a tenant-scoped cookbook migration applied.*

## Chapter 9 — ViewSets / DRF / OpenAPI

4 live tests against `ViewSet::for_model(...).router(...)` mounted
in-process. Run with
`DATABASE_URL=... cargo test --test cookbook_chapter09_viewsets -- --test-threads=1`.

* §9.112 / 9.113 — `GET /authors` returns paginated list payload;
  `POST /authors` with JSON body creates a row and returns the
  serialized object with the assigned id.
  → `viewset_list_create_round_trip`
* §9.113 — `GET /authors/{id}` returns the single object;
  `PUT /authors/{id}` updates it; `DELETE /authors/{id}` removes it
  and a subsequent GET 404s. → `viewset_retrieve_returns_single_object_by_pk`,
  `viewset_update_then_destroy`
* §9.115 — `?name=bob` query param narrows the list to matching rows
  when `filter_fields(&["name", ...])` is configured.
  → `viewset_filter_query_param_narrows_list`

The ViewSet builder also exposes `.search_fields` (?search=…),
`.ordering` (default ORDER BY), `.page_size`, `.cursor_pagination`,
`.pagination(PaginationStyle)`, and `.permissions_for_model::<T>()`
(see Chapter 6 §6.80 for the typed-perm shortcut). All exercised
live in rustango's own viewset / order_by_annotate_live tests.

*Sub-sections 9.114 (full pagination — count + next + prev),
9.116b (typed permissions), 9.117 (OpenAPI auto-derive),
9.118 (response shaping via `.fields(&[...])`) queued for Slice 9b.*

## Chapter 10 — Templates + static

3 tests on the Tera template surface that the admin (Chapter 8) +
operator console use. No DB needed.
Run with `cargo test --test cookbook_chapter10_templates`.

* §10.119 `tera::Tera::default() + add_raw_template + render(name, ctx)`
  → `tera_template_renders_with_context`
* §10.119 `t.autoescape_on(vec!["html"])` — HTML special chars in
  context get escaped. → `tera_template_autoescapes_html`
* §10.122 `{% extends %} + {% block %}` template inheritance — child
  blocks override parent fallbacks. → `tera_extends_inherits_blocks_from_base`

The `render_generic_fk_link` helper (§10.121) is exercised live in
Chapter 2's `generic_fk_schema_and_content_type_lookup`.

*Sub-section 10.120 (Tera rendering from view handlers) and 10.123
(static-file serving) queued for Slice 10b.*

## Chapter 11 — Async / IO / extensions

7 tests on the most-used extension surfaces. No DB / network needed.
Run with `cargo test --test cookbook_chapter11_extensions`.

* §11.135 `cache::get_or_set(&dyn Cache, key, factory, ttl)` —
  loader runs once; subsequent reads hit the cached value.
  → `cache_get_or_set_memoizes_loader`,
  `cache_set_get_json_round_trips`
* §11.128 `webhook::sign(format, secret, body)` +
  `verify_signature(...)` round-trip. `HexSha256` produces a raw hex
  digest; `HexSha256WithPrefix` produces GitHub's `sha256=<hex>`
  shape; `Base64Sha256` for Stripe-style headers.
  → `webhook_sign_then_verify_round_trip`,
  `webhook_github_prefix_format`
* §11.92 `signed_url::sign[_at](url, secret, ttl|None)` +
  `verify[_at](signed, secret[, now_secs])` for one-time / time-bounded
  URLs. → `signed_url_sign_then_verify_at_respects_expiry`,
  `signed_url_no_expiry_always_verifies`
* §11.126 `Scheduler::new().every(name, period, job).start()` runs
  jobs at the given period; `Handle::shutdown()` stops further fires.
  → `scheduler_every_fires_periodic_job`

*Sub-sections 11.124 (WS hub), 11.125 (SSE), 11.127 (jobs queue),
11.129 (http_client retry/UA), 11.130 (email backends),
11.131 (storage filesystem), 11.132 (storage S3), 11.133 (media uploads),
11.134 (notifications), 11.137 (signals — wire receivers to model
save/delete events), 11.138 (compression middleware), 11.139 (CSP nonce)
queued for Slice 11b. Several have framework-level live tests under
rustango/tests already.*

## Chapter 12 — Bi-dialect + cross-cutting

2 live tests against a docker MySQL 8.0 container, exercising the
same cookbook model that PG tests use through the dialect-agnostic
`Pool` + `save_pool` / `fetch_pool` ORM API. Run with:

```sh
docker run -d --name rustango-mysql \
  -e MYSQL_ROOT_PASSWORD=rustango -e MYSQL_DATABASE=cookbook_blog_my \
  -e MYSQL_USER=rustango -e MYSQL_PASSWORD=rustango \
  -p 3406:3306 mysql:8.0

MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/cookbook_blog_my \
  cargo test --test cookbook_chapter12_bidialect
```

* §12.140 / §12.141 — `Rating` model save + fetch + multi-row decode
  via `Pool::Mysql` and `save_pool` / `fetch_pool`. Exercises
  AUTO_INCREMENT for `Auto<i64>`, BIGINT round-trip, and the
  Backend trait dispatch the v0.17.1 macro refactor enabled.
  → `cookbook_rating_round_trips_against_mysql`,
  `mysql_decodes_multi_row_select`

**Surfaced limitation**: the cookbook's `Author` model can't run on
MySQL because it has two `Auto<T>` columns (`id: Auto<i64>` PK +
`joined_at: Auto<DateTime>` from `auto_now_add`). MySQL only supports
single-column RETURNING via `LAST_INSERT_ID()`, so `save_pool` errors
with `OperatorNotSupportedInDialect { op: "multi-column RETURNING",
dialect: "mysql" }`. Not a bug — a known dialect divergence the
framework surfaces as a clear runtime error. Workaround: drop the
`auto_now_add` mixin on MySQL-targeted models, or split the timestamp
column off into a trigger-managed shape.

*Sub-sections 12.142 (connection-pool tuning), 12.143 (tracing
subscriber + structured logs), 12.144 (manage check warnings),
12.145 (health endpoint), 12.146 (graceful shutdown), 12.147 (test
client utilities), 12.148 (inventory mechanism), 12.149 (macro
hygiene), 12.150 (project-shape conventions) queued for Slice 12b.*

---

## Chapter 13 — SQLite backend (v0.27 / v0.28)

v0.27 lights up SQLite as a third dialect alongside Postgres and
MySQL. Same `Pool` enum, same `_pool` ORM surface — the macro now
emits `FromRow<SqliteRow>` + `LoadRelatedSqlite` + a SQLite arm in
`AssignAutoPkPool` so every existing model with `Auto<T>` PK or
`ForeignKey<T>` works against `Pool::Sqlite` without recompilation
of the model itself, only a flip of the rustango feature set.

Cookbook-grade smoke test for the dialect lives at
[crates/rustango/examples/sqlite_orm_demo.rs](../sqlite_orm_demo.rs)
— a single-file, runnable example that bootstraps an in-memory DB
and exercises 12 ORM features end-to-end. No docker, no env vars,
no setup:

```sh
PATH="$HOME/.cargo/bin:$PATH" \
  cargo run -p rustango --example sqlite_orm_demo --features sqlite
```

### 13.151 `Pool::connect("sqlite::memory:")` — opening an in-memory pool

What:        Construct a `Pool::Sqlite` from a sqlite URL.
When:        Anywhere a `Pool` works — tests, dev bootstrap, CLI
             tools, embedded systems where shipping a sqlx
             postgres binary is awkward.
API:         [`crates/rustango/src/sql/pool.rs`](../../src/sql/pool.rs)
Recipe:
```rust,ignore
let pool = rustango::sql::Pool::connect("sqlite::memory:").await?;
assert_eq!(pool.backend_name(), "sqlite");
```
Accepted URL forms (sqlx-sqlite):
- `sqlite::memory:` — anonymous in-memory DB
- `sqlite:./relative.db` / `sqlite:///abs/path.db` — file-backed
- `sqlite:?mode=memory&cache=shared` — query-string options
Verified by:  [`tests/sqlite_live.rs`](../../tests/sqlite_live.rs)
              `pool_connect_sqlite_in_memory`

### 13.152 Auto<T> PK round-trip via `INSERT … RETURNING`

What:        SQLite ≥ 3.35 supports `INSERT … RETURNING <cols>` with
             the same shape as Postgres. The macro emits
             `__rustango_assign_from_sqlite_row` mirroring the PG
             arm — `insert_pool` populates every `Auto<T>` field
             from the returned row in one round trip.
API:         [`crates/rustango/src/sql/backend.rs`](../../src/sql/backend.rs)
Recipe:
```rust,ignore
let mut alice = Author { id: Auto::Unset, name: "Alice".into(), age: 32 };
alice.insert_pool(&pool).await?;
assert!(alice.id.is_set());  // populated from RETURNING
```
Verified by:  [`tests/sqlite_live.rs`](../../tests/sqlite_live.rs)
              `auto_pk_insert_pool_round_trips`

### 13.153 Bi-dialect `_pool` ORM API on SQLite

Every `_pool` executor function has a SQLite arm now. The
[`sqlite_orm_demo`](../sqlite_orm_demo.rs) example exercises:

| Feature                       | API                                          |
|-------------------------------|----------------------------------------------|
| `INSERT … RETURNING`          | `Model::insert_pool`                         |
| `UPDATE` single row           | `Model::save_pool`                           |
| `DELETE` single row           | `Model::delete_pool`                         |
| `SELECT *`                    | `QuerySet::fetch_pool` (`FetcherPool`)       |
| `SELECT COUNT(*)`             | `QuerySet::count_pool` (`CounterPool`)       |
| `WHERE col <op> v`            | `QuerySet::filter` (Eq/Gt/In/Like/ILike/Between) |
| `ORDER BY` / `LIMIT`/ `OFFSET`| `QuerySet::order_by/limit/offset`            |
| `INSERT …, …, …` batch        | `bulk_insert_pool(&pool, &BulkInsertQuery)`  |
| FK join (`select_related`)    | `QuerySet::select_related` (`LoadRelatedSqlite`) |
| Parents + children prefetch   | `fetch_with_prefetch_pool`                   |
| `BEGIN` / `COMMIT`            | `transaction_pool` → `PoolTx::Sqlite(tx)`    |
| `GROUP BY` + `MIN/MAX/AVG/SUM/COUNT` | `QuerySet::aggregate().compile()` + `fetch_aggregate_pool` |
| Raw SQL (typed)               | `raw_query_pool::<T>(sql, binds, &pool)`     |
| Raw SQL (rows affected)       | `raw_execute_pool(&pool, sql, binds)`        |

### 13.154 ILIKE → `LOWER(col) LIKE LOWER(?)` translation

What:        SQLite has no native `ILIKE`. The dialect rewrites
             `Op::ILike` to `LOWER(col) LIKE LOWER(?)` so the same
             cookbook recipes that ship Postgres-flavored
             case-insensitive matching work unchanged on SQLite.
API:         [`crates/rustango/src/sql/sqlite.rs`](../../src/sql/sqlite.rs)
Recipe:
```rust,ignore
Post::objects()
    .filter("title", Op::ILike, "%hello%")
    .fetch_pool(&pool).await?;  // emits: WHERE LOWER(title) LIKE LOWER(?)
```

### 13.155 SQLite-specific gotchas the demo had to dance around

Three frictions surface when running on SQLite:

1. **No `ALTER TABLE … ADD CONSTRAINT FOREIGN KEY`.** SQLite only
   accepts FK constraints declared inline at CREATE TABLE time.
   `ddl::create_constraints_sql_with_dialect` emits ALTER-style
   SQL that PG/MySQL accept; for SQLite the demo skips this loop.
   FK referential integrity is enforced anyway by manual ordering
   (insert parents before children) when `PRAGMA foreign_keys = ON`
   is off (sqlx-sqlite default). Tracked for v0.28.

2. **`sqlite_*` table names are reserved.** SQLite treats any
   identifier starting with `sqlite_` as internal-use only. The
   first attempt at the live test used `sqlite_live_users` and
   hit `object name reserved for internal use`. Stick to neutral
   prefixes (`live_users_sqlite`, `demo_…`).

3. **No advisory-lock primitive.** PG has `pg_advisory_lock`,
   MySQL has `GET_LOCK`, SQLite has nothing comparable. The
   migrate runner's `with_migrate_lock_pool` is a no-op on SQLite
   — adequate because SQLite's single-writer file-lock semantics
   already serialize concurrent migrations.

### 13.156 In-memory SQLite as a unit-test database

What:        Anonymous `sqlite::memory:` is the fastest way to spin
             up a real DB inside a `#[tokio::test]` — sub-millisecond
             pool open, no docker, no port conflicts, parallel
             tests get isolated DBs by default. Pin
             `max_connections = 1` if you want one DB per pool
             (default sqlx behavior would open multiple anonymous
             DBs as connections come up).
API:         `sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:")`
Recipe (cookbook test pattern):
```rust,ignore
async fn fresh_pool() -> Pool {
    let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:").await.unwrap();
    let pool: Pool = sqlite.into();
    let dialect = pool.dialect();
    let sql = ddl::create_table_sql_with_dialect(dialect, MyModel::SCHEMA);
    raw_execute_pool(&pool, &sql, vec![]).await.unwrap();
    pool
}
```
Verified by:  [`tests/sqlite_live.rs`](../../tests/sqlite_live.rs)
              — 5 live tests using exactly this harness.

### 13.157a `AppBuilder::from_env()` — bootstrap the app on SQLite

What:        Single-pool bi-dialect builder. Reads `DATABASE_URL`,
             constructs a `Pool` (sqlite / postgres / mysql), runs
             your model schemas as `CREATE TABLE IF NOT EXISTS`,
             mounts an axum router, serves. The Django-style
             multi-tenant `Builder` is `PgPool`-bound; this is the
             non-tenancy alternative.
When:        You want the rustango framework to bootstrap your app
             on SQLite (or any backend) without rolling your own
             axum + pool wiring.
API:         [`crates/rustango/src/server/app.rs`](../../src/server/app.rs)
             — gated on the `runserver` feature (in defaults).
Recipe (full app, runs and serves):
```rust,ignore
use std::sync::Arc;
use axum::{routing::get, Extension, Json, Router};
use rustango::core::Model as _;
use rustango::server::AppBuilder;
use rustango::sql::{Auto, FetcherPool, Pool};
use rustango::Model;

#[derive(Model, Debug, Clone, serde::Serialize)]
#[rustango(table = "demo_user")]
pub struct User {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 80)] pub name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    AppBuilder::from_env().await?       // reads DATABASE_URL
        .bootstrap(&[User::SCHEMA]).await?  // CREATE TABLE IF NOT EXISTS
        .api(Router::new().route("/users", get(list_users)))
        .serve("0.0.0.0:8080").await
}

async fn list_users(Extension(pool): Extension<Arc<Pool>>) -> Json<Vec<User>> {
    Json(User::objects().fetch_pool(&pool).await.unwrap())
}
```
Run:
```sh
DATABASE_URL='sqlite:./var/app.db?mode=rwc' \
  cargo run --features sqlite,runserver
# Switch backends without touching code:
DATABASE_URL='postgres://…' cargo run --features postgres,runserver
DATABASE_URL='mysql://…'    cargo run --features mysql,runserver
```
Verified by:  [`examples/sqlite_app_demo.rs`](../sqlite_app_demo.rs) +
              `cargo test --features tenancy,sqlite --lib server::app`

What `AppBuilder` does NOT include (compared to the multi-tenant
`Builder`): operator console, tenant admin, tenant resolver chain,
session middleware. Those all sit on top of `TenantPools` which is
still PG-bound. For a SQLite-backed multi-tenant app you'd
combine `AppBuilder` with the per-tenant pool registry sketch from
the cookbook discussion ("How do I use SQLite for tenants").

The pool is injected as `Extension<Arc<Pool>>` into every request,
so handlers extract it directly — no per-app `with_state(...)`
ceremony.

### 13.157b `AppBuilder::from_pool` — inject any `Pool`

When `from_env` is too rigid (custom `SqlitePoolOptions`,
`max_connections(1)` for in-memory tests, dependency injection in
tests):

```rust,ignore
let sqlx_pool = sqlx::sqlite::SqlitePoolOptions::new()
    .max_connections(1).connect("sqlite::memory:").await?;
let pool: rustango::sql::Pool = sqlx_pool.into();
let app = AppBuilder::from_pool(pool).bootstrap(&[…]).await?;
```

### 13.157 Building rustango with the `sqlite` feature

Add `features = ["sqlite"]` to your `Cargo.toml` rustango dep, or
combine with the existing dialect features:

```toml
[dependencies]
rustango = { version = "0.28", features = ["sqlite"] }
# or both at once:
rustango = { version = "0.28", features = ["postgres", "sqlite"] }
```

The macro emits per-backend trait impls only when the feature is
on — `MaybeSqliteFromRow` / `MaybeSqliteLoadRelated` are blanket-
implemented for every `T` when `sqlite` is off, so existing
PG-only consumers compile unchanged. Verified by the macro
hygiene regression test:
[`tests/macro_no_backend_cfg.rs`](../../tests/macro_no_backend_cfg.rs).

---

## Gaps surfaced while writing this cookbook

*(populated as we discover them per chapter)*

- **Chapter 13 / SQLite (v0.27, 2026-05-07):** the
  `ddl::create_constraints_sql_with_dialect` emitter still produces
  `ALTER TABLE … ADD CONSTRAINT FOREIGN KEY` SQL, which SQLite's
  parser rejects (FK constraints must be inline at CREATE TABLE).
  The `sqlite_orm_demo` example skips that loop on SQLite as a
  workaround. Real fix: refactor the emitter to put FK constraints
  in the inline column list when the dialect doesn't support
  ALTER-style FK addition. Tracked for v0.28.
- **Chapter 13 / SQLite (v0.27):** `apply_all_pool` walks the full
  `inventory::registered_models()` list, which on a default build
  includes framework models (Org, Operator, Job, etc.) whose DDL
  emits Postgres-shape SQL — those CREATE TABLE statements fail
  on SQLite. Workaround used in the demo: emit DDL manually for
  just the test models. A `Dialect::supports_model(&ModelSchema)`
  filter (or per-model dialect-compat flag) would let
  `apply_all_pool` skip incompatible models cleanly.
