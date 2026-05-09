# rustango

**A Django-shaped, batteries-included web framework for Rust.**

Bi-dialect ORM (Postgres / MySQL / **SQLite**) with auto-migrations, multi-tenancy, auto-admin (token-driven theme system + dark mode + per-tenant branding via pluggable `Storage` trait — S3/R2/B2/MinIO/Local), sessions + JWT + OAuth2/OIDC + HMAC auth, signals, caching, first-class media (Postgres rows + S3/R2/B2/MinIO + presigned uploads + collections + tags), email pipeline (renderer + jobs + Mailable), background jobs (in-mem + Postgres), webhook delivery, OpenAPI 3.1 auto-derive from serializers + viewsets, JSON:API + RFC 7807 Problem Details, scheduled tasks, RFC 6238 TOTP, signed URLs, Prometheus metrics, OTel-shape tracing, distributed locks + rate limits + feature flags, every standard middleware (CSRF, CSP nonce, gzip/deflate, body limit, real-IP, idempotency, maintenance, trailing slash, static files, method override, server-timing, …) — all shipped, all opt-out via cargo features.

```toml
[dependencies]
# Postgres (default)
rustango = "0.29"

# SQLite — file-backed or in-memory, full bi-dialect ORM
rustango = { version = "0.29", features = ["sqlite"] }

# Multiple backends in one binary
rustango = { version = "0.29", features = ["postgres", "sqlite"] }
```

### Spin up an app on SQLite in 30 lines

```rust
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
    AppBuilder::from_env().await?            // reads DATABASE_URL
        .bootstrap(&[User::SCHEMA]).await?   // CREATE TABLE IF NOT EXISTS
        .api(Router::new().route("/users", get(list)))
        .serve("0.0.0.0:8080").await
}

async fn list(Extension(pool): Extension<Arc<Pool>>) -> Json<Vec<User>> {
    Json(User::objects().fetch_pool(&pool).await.unwrap())
}
```

```sh
DATABASE_URL='sqlite:./var/app.db?mode=rwc' \
  cargo run --features sqlite,runserver
```

Same code unchanged with `DATABASE_URL=postgres://…` boots on Postgres.

> **Multi-tenant on SQLite?** The `tenancy` module's `TenantPools` /
> `Builder` is still PG-only — pending refactor in v0.28. For SQLite
> tenants today, see Cookbook chapter 13 for the per-tenant `Pool`
> registry shape.

---

## Table of contents

- [Quick start](#quick-start)
- [Project layout](#project-layout)
- [`manage` CLI reference](#manage-cli-reference)
- [Configuration](#configuration)
- [ORM cookbook](#orm-cookbook)
- [Migrations](#migrations)
- [Auto-admin](#auto-admin)
- [APIs (ViewSet + Serializer + JWT)](#apis-viewset--serializer--jwt)
- [HTML views (Django-shape CBVs)](#html-views-django-shape-cbvs)
- [Forms](#forms)
- [Multi-tenancy](#multi-tenancy)
- [Authentication & permissions](#authentication--permissions)
- [Security middleware](#security-middleware)
- [Caching](#caching)
- [Email + storage + scheduling](#email--storage--scheduling)
- [Signals](#signals)
- [i18n](#i18n)
- [Testing](#testing)
- [Feature flags](#feature-flags)
- [Contributing — git hooks](#contributing--git-hooks)
- [Production checklist](#production-checklist)

---

## Quick start

### 1. Install the scaffolder

```bash
cargo install cargo-rustango
```

### 2. Create a project

```bash
cargo rustango new myblog                   # default: ORM + admin
cargo rustango new myapi --template api     # JSON-only, no admin
cargo rustango new shop --template tenant   # multi-tenancy + operator console
```

### 3. First-time setup

```bash
cd myblog
cp .env.example .env                        # edit DATABASE_URL
docker compose up -d                        # starts Postgres
cargo run -- migrate                        # apply bootstrap migrations
cargo run                                   # http://localhost:8080
```

**Autoreload during development** — recompiles + restarts on every file save:

```bash
cargo install cargo-watch
cargo watch -x run

# Or with bacon (faster, nicer UI):
cargo install bacon
bacon run
```

You should see the scaffolded landing page (`src/views.rs::index`) at `/` confirming rustango is wired up. Replace it with your own handler — or mount a real router under `/` — when you're ready.

### 4. Add an app + model

```bash
cargo run -- startapp blog     # scaffolds src/blog/
```

Since v0.28.3 the scaffolder ships a singularized starter model
(`startapp posts` → `pub struct Post` on table `"post"`), an
`admin(...)` config block, a `created_at` timestamp, and a smoke
test that asserts the model registered itself in `inventory`.
Rename the struct or table literal freely once it doesn't fit.

Edit `src/blog/models.rs`:

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone)]
#[rustango(
    table = "posts",
    display = "title",
    admin(list_display = "id, title, published_at", search_fields = "title, body"),
    audit(track = "title, body"),
    index("published_at, author_id"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 200)]
    pub title: String,
    pub body: String,
    pub author_id: i64,
    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,
}
```

```bash
cargo run -- makemigrations                 # generates migration JSON
cargo run -- migrate                        # applies it
```

### 5. Generate a viewset + serializer

```bash
cargo run -- make:viewset PostViewSet --model Post
cargo run -- make:serializer PostSerializer --model Post
```

Edit the generated files to fill in field lists, then mount in `src/urls.rs`.

---

## Project layout

A scaffolded project looks like:

```
myblog/
├── Cargo.toml                  # rustango + axum + tokio + sqlx
├── .env / .env.example         # DATABASE_URL, SECRET_KEY, RUSTANGO_ENV
├── docker-compose.yml          # Postgres in a container for local dev
├── README.md
├── config/                     # tiered settings (#87, v0.29)
│   ├── default.toml            # shared knobs, every section commented
│   ├── dev_settings.toml       # local Postgres URL, relaxed headers, `(dev)` tagline
│   ├── staging_settings.toml   # 0.0.0.0 bind, strict headers, 30d retention
│   └── prod_settings.toml      # pool 5-50, strict headers, 365d retention
├── migrations/                 # JSON files written by `cargo run -- makemigrations`
└── src/
    ├── main.rs                 # one binary — HTTP server + manage CLI; declares `mod`s here
    ├── urls.rs                 # top-level Router composition
    ├── models.rs               # OR per-app: src/blog/models.rs
    └── views.rs
```

The runtime picks a tier from `RUSTANGO_ENV` (defaults to `dev`); see
[Configuration](#configuration) below.

Since v0.16 the `manage` CLI is dispatched from `main.rs` via
`rustango::manage::Cli` — running `cargo run` with no args starts
the HTTP server, and `cargo run -- <verb>` dispatches a CLI command.
There is no longer a separate `src/bin/manage.rs`, and the scaffold
is binary-only (no `src/lib.rs`) — apps are declared as `mod blog;`
inside `src/main.rs`. The scaffolded `main.rs` is just:

```rust
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

The `tenant` template additionally calls `.tenancy()` and
`.migrations_dir(...)` on the `Cli` builder.

Per-app structure (created by `cargo run -- startapp <name>`):

```
src/blog/
  ├── mod.rs
  ├── models.rs                 # #[derive(Model)] structs
  ├── views.rs                  # axum handlers
  └── urls.rs                   # router for this app
```

---

## `manage` CLI reference

Since v0.16 there is no separate `manage` binary — the verbs are
dispatched by the project's own binary via `rustango::manage::Cli`,
so the canonical invocation is `cargo run -- <verb>`. Every command
has `--help`. Exit code is non-zero on validation / IO / system-check
errors.

### Migrations

```bash
cargo run -- makemigrations [name]                   # diff registry → next JSON
cargo run -- makemigrations --app <app> [name]       # per-app migration dir
cargo run -- makemigrations --empty <name>           # scaffold for hand-authored data ops
cargo run -- migrate                                  # apply every pending
cargo run -- migrate <target>                         # forward or back to <target>; `zero` wipes
cargo run -- migrate --dry-run                        # print SQL without writing
cargo run -- migrate --squash                         # delete every pending JSON + regenerate
                                                      # one fresh diff (dev-iteration escape
                                                      # hatch — refuses to touch applied rows)
cargo run -- forget-pending <name>                    # rm one un-applied JSON; substring match
cargo run -- downgrade [N]                            # step back N (default 1)
cargo run -- showmigrations | status                  # [X] applied / [ ] pending list
```

### Data migrations

```bash
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

cargo run -- add-data-op --to 0003_add_slug --sql "UPDATE posts SET slug = id::text"
```

### Scaffolders

```bash
cargo run -- startapp <name> [--with-manage-bin]
cargo run -- make:viewset PostViewSet --model Post [--tenant]
cargo run -- make:api_routes <app> [--tenant]   # composer that .merges() per-model viewsets
cargo run -- make:serializer PostSerializer --model Post
cargo run -- make:form ContactForm
cargo run -- make:job EmailDigestJob
cargo run -- make:notification WelcomeEmail
cargo run -- make:middleware AuditLog
cargo run -- make:test PostSmoke               # PascalCase — file is post_smoke.rs
```

### System

```bash
cargo run -- about                # version, model count, registered apps, DB connectivity
cargo run -- check                # pending migrations, missing models, DB reachable
cargo run -- check --deploy       # + RUSTANGO_SESSION_SECRET length, RUSTANGO_ENV,
                                  #   DATABASE_URL, RUSTANGO_APEX_DOMAIN, RUSTANGO_BIND
                                  #   PLUS Settings audit (#87): flags dev-defaults left
                                  #   in prod tier — headers_preset=dev, hsts_max_age=0,
                                  #   argon2 below OWASP floor, JWT TTL > 1h, loopback bind
cargo run -- docs                 # opens https://docs.rs/rustango in browser
cargo run -- version | --version  # framework version
```

### Tenancy (only with `tenancy` feature)

```bash
cargo run -- create-tenant acme --display-name "ACME Corp"
cargo run -- create-operator admin --password letmein
cargo run -- create-user acme alice --password hunter2 --superuser
cargo run -- list-tenants
cargo run -- audit-cleanup --days 90
cargo run -- audit-cleanup --keep-last 50 --tenant acme

# Re-seed the rustango_permissions catalog after adding
# `#[rustango(permissions)]` to a model — without --slug walks every
# active tenant; idempotent (UNIQUE on (content_type_id, codename)).
cargo run -- seed-permissions [--slug acme]

# Move a populated tenant between schema and database storage modes
# (pg_dump → psql pipe, Org row update, pool eviction, smoke check):
cargo run -- migrate-tenant-storage acme --to database \
    --database-url "postgres://acme:secret@db.example.com/acme" --dry-run
cargo run -- migrate-tenant-storage acme --to database \
    --database-url "postgres://acme:secret@db.example.com/acme"
```

---

## Configuration

Tiered TOML settings (since v0.29, [#87](https://github.com/anthropics/claude-code/issues/87))
— a fresh project ships four config files; the runtime picks the
right tier from `RUSTANGO_ENV`.

```
config/
├── default.toml             # shared defaults; every section commented + documented
├── dev_settings.toml        # local Postgres URL, headers_preset = "dev",
│                            #   hsts_max_age_secs = 0, tagline "(dev)"
├── staging_settings.toml    # bind 0.0.0.0, strict headers, retention 30d
└── prod_settings.toml       # pool 5-50, strict headers, retention 365d
```

### Loader pipeline

1. `config/default.toml` — required, shared defaults.
2. `config/<RUSTANGO_ENV>_settings.toml` — tier overlay (the legacy
   `<env>.toml` shape still loads when no `_settings` variant exists).
3. `RUSTANGO__SECTION__KEY=value` env vars — final override.

```rust
// Reads RUSTANGO_ENV (defaults to "dev"), runs the layered load.
let cfg = rustango::config::Settings::load_from_env()?;

// Or pick the tier explicitly:
let cfg = rustango::config::Settings::load("prod")?;

// Resolved tier (useful for telemetry / version pages):
let tier = rustango::config::Settings::current_env_tier();

// One-liner wiring — at runserver time this picks up bind address,
// RouteConfig, AND auto-applies the security_headers + CORS +
// access_log + body_limit layers built from [security] / [audit] /
// [server] settings. Falls back to Cli defaults silently if config
// files are missing (with a tracing::warn).
rustango::manage::Cli::new()
    .api(urls::api())
    .with_settings_from_env()
    .with_health()                              // /health + /ready endpoints
    .with_static("/static", "./assets")         // CSS, JS, images
    .run().await
```

Apps using only TOML-side routes config no longer need to call
`.routes(RouteConfig::legacy())` from code — set
`[routes] legacy_preset = true` in `prod_settings.toml` and
`with_settings_from_env()` picks it up.

Layer order at runserver time (innermost → outermost):
`body_limit → access_log → CORS → security_headers → handler`.
This matches the canonical recommendation in the production
checklist below — and you don't have to wire any of it manually.

### Sections

```toml
# config/default.toml
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

Every field is `Option<T>` with sensible defaults documented in
[`config::sections`](crates/rustango/src/config/sections.rs) — missing
keys fall through to `Default::default()`, so your TOML stays
forward-compatible.

### Compile-time feature reflection

```rust
let feats = rustango::config::Settings::detected_features();
// → ["postgres", "tenancy", "admin", "manage", "config", ...]
```

Useful on `/about` pages, in deployment audits, or as a sanity check
that the prod binary was built with every feature its TOML
references.

### Deploy audit

`cargo run -- check --deploy` flags dev-defaults that survived a
promotion to the prod tier:

- `[security] headers_preset = "dev"` or `"none"` → warning
- `[security] hsts_max_age_secs = 0` → warning
- `[auth] argon2_memory_kib < 19456` → warning (OWASP 2024 floor)
- `[auth.jwt] access_ttl_secs > 3600` → warning (use refresh flow)
- `[server] bind = 127.0.0.1:*` / `localhost:*` → warning
- `[audit] retention_days` unset → info ("log grows forever")
- `[routes] legacy_preset = true` → info (deliberate v0.28 shape)

The audit is a no-op on dev/staging tiers — operators only want
verbose feedback when something promoted to prod was incorrectly
relaxed.

---

## ORM cookbook

### Model declaration

```rust
use rustango::{Auto, Model};
use rustango::sql::ForeignKey;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Model, Clone)]
#[rustango(
    table = "posts",
    display = "title",                            // shown when post is FK target
    admin(
        list_display = "id, title, published_at",
        search_fields = "title, body",
        list_filter = "author_id",
        ordering = "-published_at",
        list_per_page = 50,
    ),
    audit(track = "title, body, status"),         // per-field before/after diff
    permissions,                                  // auto-create CRUD codenames
    index("published_at, author_id"),             // composite index
    check(name = "valid_status",
          expr = "status IN ('draft', 'published')"),
    m2m(name = "tags", to = "tags", through = "post_tags",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200, index)]
    pub title: String,

    pub body: String,
    pub status: String,

    pub author: ForeignKey<Author>,                // typed FK, lazy-loadable

    #[rustango(auto_now_add)]                      // NOW() on INSERT, immutable
    pub created_at: Auto<DateTime<Utc>>,
    #[rustango(auto_now)]                          // NOW() on every save
    pub updated_at: Auto<DateTime<Utc>>,
    #[rustango(soft_delete)]                       // stamp instead of hard-DELETE
    pub deleted_at: Option<DateTime<Utc>>,
    #[rustango(auto_uuid)]                         // server-generated UUIDv4
    pub external_id: Auto<Uuid>,

    #[rustango(default = r#"'{}'::jsonb"#)]
    pub data: serde_json::Value,
}
```

### Field types

| Rust type | Postgres | MySQL | Notes |
|---|---|---|---|
| `i16` | `SMALLINT` | `SMALLINT` | 2-byte signed, `-32768..=32767`. Smallest portable integer. |
| `i32` | `INTEGER` | `INT` | 4-byte signed. |
| `i64` | `BIGINT` | `BIGINT` | 8-byte signed. Default PK width. |
| `f32` | `REAL` | `FLOAT` | |
| `f64` | `DOUBLE PRECISION` | `DOUBLE` | |
| `bool` | `BOOLEAN` | `TINYINT(1)` | |
| `String` | `TEXT` / `VARCHAR(N)` | `TEXT` / `VARCHAR(N)` | `TEXT` unless `max_length = N` is set. |
| `chrono::DateTime<Utc>` | `TIMESTAMPTZ` | `DATETIME(6)` | |
| `chrono::NaiveDate` | `DATE` | `DATE` | |
| `uuid::Uuid` | `UUID` | `CHAR(36)` | |
| `serde_json::Value` | `JSONB` | `JSON` | |

`i8`, `u8`/`u16`/`u32`/`u64` are intentionally not supported — Postgres has no native 1-byte signed integer and no unsigned integers, so cross-dialect storage would diverge. Use `i16` and bounded `min`/`max` attributes instead. `Auto<i16>` is also rejected (SMALLSERIAL exhausts at 32k); use `Auto<i32>` or `Auto<i64>` for auto-incrementing PKs.

### Nullable fields

Wrap any scalar in `Option<T>` to make the column `NULL`-able. `None` round-trips through fetch + insert + update; `parse_form_value` and the admin's edit form both treat empty input as `NULL`.

```rust
#[derive(Model, Clone)]
pub struct Article {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    pub title: String,                          // NOT NULL
    pub subtitle: Option<String>,               // NULLABLE
    pub priority: Option<i16>,                  // NULLABLE SMALLINT

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,      // soft delete requires nullable timestamp
}
```

`Auto<Option<T>>` and `Option<Auto<T>>` are both rejected — `Auto<T>` columns are server-assigned and cannot be `NULL`.

### Primary keys

Any scalar field marked `#[rustango(primary_key)]` becomes the PK. Three common shapes:

```rust
// Auto-incrementing i64 PK — the default for most tables.
#[rustango(primary_key)] pub id: Auto<i64>,

// Auto-incrementing i32 PK — saves 4 bytes per row when 2B is enough.
#[rustango(primary_key)] pub id: Auto<i32>,

// Server-generated UUID PK — Postgres `gen_random_uuid()`.
#[rustango(primary_key, auto_uuid)] pub id: Auto<Uuid>,

// Caller-supplied String PK — common for slug-shaped natural keys.
#[rustango(primary_key, max_length = 64)] pub slug: String,
```

`Auto<T>` is supported on `i32`, `i64`, `Uuid`, and `DateTime<Utc>` (the last for `auto_now_add` defaults). Other PK types (i16, plain integers, String) require the caller to supply the value on insert.

### Foreign keys

`ForeignKey<T, K = i64>` is the typed, lazy-loadable wrapper. `T` is the parent model; `K` is the parent's primary-key type (defaults to `i64`).

```rust
use rustango::sql::ForeignKey;

// Default — parent PK is i64 (or Auto<i64>). Stored as BIGINT.
pub author: ForeignKey<Author>,

// Parent PK is uuid::Uuid. Stored as UUID.
pub region: ForeignKey<Region, uuid::Uuid>,

// Parent PK is String. Stored as VARCHAR(N) — provide max_length on the field.
#[rustango(max_length = 36, on = "user_uuid")]
pub user: ForeignKey<User, String>,

// Nullable FK — column allows NULL, the field stores Option<ForeignKey<…>>.
pub editor: Option<ForeignKey<User>>,
pub region: Option<ForeignKey<Region, uuid::Uuid>>,

// Self-referential FK — for trees / hierarchies. The macro substitutes the
// containing table name, sidestepping the type-name self-reference cycle.
#[rustango(fk = "self")]
pub parent_id: Option<i64>,
```

Reading the parent on demand:

```rust
let mut book = Book::objects().where_(Book::id.eq(42)).fetch_one(&pool).await?;
let author: &Author = book.author.get(&pool).await?;     // fires one SELECT, caches the result
println!("{}", author.name);
let cached = book.author.get(&pool).await?;              // no SQL — returns the cached parent
```

For one-shot eager loading:

```rust
let books = Book::objects()
    .select_related(&[Book::author])                     // single LEFT JOIN
    .fetch(&pool).await?;                                // every book.author is already Loaded
```

`prefetch_related` (the bulk N+1 killer for `<parent>.<child>_set`) currently requires `i64` parent PKs; non-i64 FK PKs work for everything else but skip the prefetch grouper — tracked as P10 in the ORM improvement plan.

#### Raw FK attribute (bypass `ForeignKey<T>`)

When you want a foreign-key constraint on a plain typed field — no lazy-load, no wrapper — use `#[rustango(fk = "<table>")]`:

```rust
pub struct Comment {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(fk = "users", on = "id")]                 // FK constraint, plain i64
    pub user_id: i64,
    pub body: String,
}
```

This is the v0.1 form — emits the same SQL constraint, no Rust-side resolver. Use it for hot-path columns where you don't want to opt into ForeignKey's lazy-load machinery, or when migrating from a legacy schema.

#### Composite (multi-column) FK

Container attribute, declared once per relation:

```rust
#[derive(Model)]
#[rustango(
    table = "audit_log",
    fk_composite(name = "target",
                 to = "rustango_audit_log",
                 from = ("entity_table", "entity_pk"),
                 on   = ("table_name",   "row_pk")),
)]
pub struct AuditLog {
    #[rustango(primary_key)] pub id: Auto<i64>,
    pub entity_table: String,
    pub entity_pk: i64,
    pub action: String,
}
```

### Field attributes

Attribute on the field carries column-level options:

| Attribute | Effect |
|---|---|
| `#[rustango(primary_key)]` | Marks the PK. Exactly one per model. |
| `#[rustango(column = "name")]` | Override the SQL column name. Default = field name. |
| `#[rustango(max_length = N)]` | `VARCHAR(N)` (String) / range check (integer). |
| `#[rustango(min = N, max = N)]` | Integer range check enforced at parse + insert/update. |
| `#[rustango(default = "expr")]` | Raw SQL fragment for the column's `DEFAULT`. Quote literals yourself. |
| `#[rustango(unique)]` | Adds `UNIQUE` to the column DDL inline. |
| `#[rustango(index)]` | Single-column `CREATE INDEX`. |
| `#[rustango(auto_now_add)]` | DB sets `now()` on INSERT; field becomes immutable on subsequent saves. Pair with `Auto<DateTime<Utc>>`. |
| `#[rustango(auto_now)]` | Same as above + bound to `now()` on every UPDATE too. |
| `#[rustango(auto_uuid)]` | DB sets `gen_random_uuid()` on INSERT. Pair with `Auto<Uuid>`. |
| `#[rustango(soft_delete)]` | Routes admin DELETE through `UPDATE … = NOW()`. Requires `Option<DateTime<Utc>>`. |
| `#[rustango(generated_as = "EXPR")]` | DB-computed column — emits `GENERATED ALWAYS AS (EXPR) STORED`. The macro skips the column from every INSERT and UPDATE; Postgres recomputes the value from `EXPR`. Read-back via `FromRow` works as for any other column. |
| `#[rustango(fk = "table")]` | Raw foreign-key constraint on a plain field. |
| `#[rustango(on = "column")]` | Override the FK target column (default `"id"`). |

### Container attributes

Attributes on the struct itself, all wrapped in `#[rustango(...)]`:

| Attribute | Effect |
|---|---|
| `table = "name"` | Override the SQL table name. Default = snake-case of struct name. |
| `display = "field"` | Field rendered when this model is the target of an FK in admin / OpenAPI. |
| `app = "label"` | Django-shape app label. Default = inferred from module path. |
| `admin(...)` | Auto-admin config: `list_display`, `search_fields`, `list_filter`, `ordering`, `list_per_page`, `readonly_fields`. |
| `audit(track = "f1, f2")` | Per-write before/after diff captured for these fields. Empty list = all scalar fields. |
| `permissions` | Auto-seed the four CRUD codenames (`add`, `change`, `delete`, `view`). |
| `index("a, b")` / `unique_together = "a, b"` | Composite (multi-column) index / unique constraint. The first declared `unique_together` doubles as the `ON CONFLICT` target for the macro-generated `Model::upsert()` — useful for surrogate-PK + composite-UNIQUE shapes (junction rows, membership tables) where conflicting on a `BIGSERIAL` PK would never fire. |
| `unique_together(columns = "a, b", name = "my_idx")` | Same with an explicit constraint name override. |
| `check(name = "n", expr = "raw SQL")` | Table-level `CHECK` constraint. |
| `m2m(name = "...", to = "...", through = "...", src = "...", dst = "...")` | Many-to-many relation through a junction table. |
| `fk_composite(name = "...", to = "...", from = (...), on = (...))` | Multi-column FK (see above). |

### Querying

```rust
use rustango::core::Column as _;
use rustango::sql::Fetcher as _;

// Filter + order
let recent = Post::objects()
    .where_(Post::status.eq("published"))
    .where_(Post::deleted_at.is_null())
    .order_by(Post::published_at, true)            // true = DESC
    .limit(20)
    .fetch(&pool).await?;

// Pagination
let page = Post::objects().page(2, 50).fetch(&pool).await?;

// Aggregation
let count = Post::objects().where_(Post::author_id.eq(42)).count(&pool).await?;
let avg = Post::objects().avg(Post::view_count, &pool).await?;

// IN / NOT IN
let some = Post::objects()
    .where_(Post::id.in_(&[1, 2, 3]))
    .fetch(&pool).await?;

// Pattern lookups
let drafts = Post::objects()
    .where_(Post::title.icontains("draft"))       // ILIKE %draft%
    .fetch(&pool).await?;

// Pre-load FKs (no N+1)
let with_authors = Post::objects()
    .select_related("author")
    .fetch(&pool).await?;
```

### Mutations

```rust
// Insert
let mut p = Post {
    id: Auto::default(),
    title: "Hello".into(),
    body: "World".into(),
    status: "draft".into(),
    // ... other fields
};
p.save_on(&pool).await?;
println!("inserted id = {}", p.id.get().copied().unwrap_or(0));

// Update — same `save_on()`, dispatched by Auto<T> PK state
p.title = "Hello world".into();
p.save_on(&pool).await?;

// Bulk insert
Post::bulk_insert_on(&pool, vec![p1, p2, p3]).await?;

// Bulk update
Post::objects()
    .where_(Post::status.eq("draft"))
    .update()
    .set(Post::status, "published")
    .execute_on(&pool).await?;

// Upsert (ON CONFLICT)
post.upsert_on(&pool, &["external_id"]).await?;

// Delete (soft if model has #[rustango(soft_delete)])
p.soft_delete_on(&pool).await?;
p.restore_on(&pool).await?;
```

### Transactions

```rust
rustango::sql::transaction(&pool, |conn| async move {
    p1.save_on(&mut *conn).await?;
    p2.save_on(&mut *conn).await?;
    Ok(())
}).await?;
```

### EXPLAIN — query planner output for any queryset

```rust
use rustango::sql::{ExplainFormat, ExplainOptions};

// Plain EXPLAIN — safe (no execution).
let plan = Post::objects()
    .where_(Post::author_id.eq(7_i64))
    .explain(&pool).await?;
for line in plan { println!("{line}"); }

// EXPLAIN (ANALYZE, BUFFERS) — actually runs the query.
let plan = Post::objects()
    .where_(Post::status.eq("published"))
    .explain_on(&pool, ExplainOptions {
        analyze: true,
        buffers: true,
        ..Default::default()
    })
    .await?;

// EXPLAIN (FORMAT JSON) — parseable payload.
let plan = Post::objects()
    .explain_on(&pool, ExplainOptions {
        format: ExplainFormat::Json,
        ..Default::default()
    })
    .await?;
let parsed: serde_json::Value = serde_json::from_str(&plan.join("\n"))?;
```

### Bi-dialect (Postgres + MySQL) via `&Pool`

The classic API takes `&PgPool` (Postgres-only). The v0.23.0 series
adds a parallel `&Pool` API that targets either backend; pick MySQL
8.0+ or Postgres at runtime via the connection URL.

```toml
# Cargo.toml — opt in to MySQL alongside the default postgres feature
rustango = { version = "0.29", features = ["mysql"] }
```

```rust
use rustango::sql::{Pool, FetcherPool, CounterPool};

// Connect from URL (postgres:// or mysql://) or from split env vars
// (DB_DRIVER + DB_HOST + DB_PORT + DB_USER + DB_PASSWORD + DB_NAME +
// DB_PARAMS — passwords are auto percent-encoded).
let pool = Pool::connect_from_env().await?;

// Schema bootstrap (CREATE TABLE per registered model — dialect-aware
// type names, BIGINT AUTO_INCREMENT vs BIGSERIAL, JSON vs JSONB, etc.).
rustango::migrate::apply_all_pool(&pool).await?;

// Or run the file-based ledger runner — full Django-shape lifecycle.
rustango::migrate::migrate_pool(&pool, dir).await?;
rustango::migrate::migrate_to_pool(&pool, dir, "0005_x").await?;
rustango::migrate::downgrade_pool(&pool, dir, 1).await?;
rustango::migrate::unapply_pool(&pool, dir, "0005_x").await?;

// Macro-emitted CRUD against either backend (every #[derive(Model)] type).
let mut user = User { id: Auto::Unset, name: "alice".into(), .. };
user.insert_pool(&pool).await?;        // Auto<i64> populated via
                                       // RETURNING (PG) / LAST_INSERT_ID() (MySQL)
user.name = "Alice".into();
user.save_pool(&pool).await?;          // INSERT-or-UPDATE; audited models
                                       // emit a transactional diff audit row
user.delete_pool(&pool).await?;        // DELETE (transactional with audit
                                       // emit when the model is audited)

// QuerySet read path — single-table, select_related joins, prefetch,
// pagination, and aggregates all bi-dialect.
let posts: Vec<Post> = Post::objects()
    .filter(Post::is_published.eq(true))
    .select_related(&[Post::author])   // joins decoded automatically
    .order_by(&[Post::created_at.desc()])
    .limit(20)
    .fetch_pool(&pool).await?;
let n: i64 = User::objects().count_pool(&pool).await?;
let page = rustango::sql::fetch_paginated_pool(
    Post::objects().limit(20).offset(40),
    &pool,
).await?;
let with_kids: Vec<(User, Vec<Post>)> =
    rustango::sql::fetch_with_prefetch_pool::<User, Post>(
        User::objects(),
        "user_id",
        &pool,
    ).await?;

// Cross-table atomicity — open a backend-tagged transaction.
let mut tx = rustango::sql::transaction_pool(&pool).await?;
match &mut tx {
    rustango::sql::PoolTx::Postgres(t) => { /* $1 placeholders */ }
    rustango::sql::PoolTx::Mysql(t)    => { /* ?  placeholders */ }
}
tx.commit().await?;
```

**Operator translations:** `ILIKE` → `LOWER(col) LIKE LOWER(?)`,
`IS DISTINCT FROM` → `NOT (col <=> ?)`, JSONB `@>` → `JSON_CONTAINS`,
JSONB `?`/`?|`/`?&` → `JSON_CONTAINS_PATH(col, 'one'|'all', CONCAT('$.', ?))`,
`UPDATE … FROM (VALUES …)` → `UPDATE … INNER JOIN (VALUES ROW(?, ?), …)`.
`ON CONFLICT DO UPDATE SET col = EXCLUDED.col` → `ON DUPLICATE KEY UPDATE
col = VALUES(col)` (with `target: vec![]`; `MySQL`'s upsert can't take a
target column list).

**MySQL caveats:**
- requires MySQL 8.0+ (window functions for `fetch_paginated_pool`,
  `JSON` column type, `VALUES ROW(…)` syntax for `bulk_update_pool`)
- `fetch_paginated_pool` uses `COUNT(*) OVER ()` — needs 8.0
- `LAST_INSERT_ID()` reports one auto-assigned column per connection,
  so models with multiple `Auto<T>` PKs error at runtime on MySQL
  (Postgres `RETURNING` is unaffected)

**Migration story:** the `&PgPool` API stays exactly as it was — every
existing app keeps working unchanged on upgrade. Adopt `&Pool` at
your own pace (or never, if you only target Postgres).

### Many-to-many

```rust
// All declared via #[rustango(m2m(...))] — junction table auto-created
let tag_ids = post.tags_m2m().all(&pool).await?;
post.tags_m2m().add(42, &pool).await?;
post.tags_m2m().remove(42, &pool).await?;
post.tags_m2m().set(&[1, 2, 3], &pool).await?;     // replace all
post.tags_m2m().clear(&pool).await?;
let has = post.tags_m2m().contains(42, &pool).await?;
```

### ContentTypes — generic relations + composite-key FKs + soft-FK prefetch

Django-shape framework for "any registered model" pointers. Lets one
table point at *any* other model via `(content_type_id, object_pk)`
(comments-on-anything, audit log targets, activity streams, tag
generic relations) and lets a single FK constraint span multiple
columns. Sub-slices F.1 / F.2 / F.3 of v0.15.0.

#### Bootstrap

```rust
// Registers one row per #[derive(Model)] type in `rustango_content_types`.
// Idempotent — re-runs on a populated DB return Ok(0). Run once at startup
// after `migrate(&pool, dir).await?`.
let inserted = rustango::contenttypes::ensure_seeded(&pool).await?;
```

#### Lookups

```rust
use rustango::contenttypes::ContentType;

// By Rust type
let ct = ContentType::for_model::<Post>(&pool).await?;       // Option<ContentType>

// By natural key (parsed permission codenames, admin URLs, etc.)
let ct = ContentType::by_natural_key(&pool, "blog", "post").await?;

// By id (FK joins from audit log / permissions / generic FK rows)
let ct = ContentType::by_id(&pool, 7).await?;

// Full listing for admin sidebars / API
let all_cts = ContentType::all(&pool).await?;                 // ordered (app, model)
```

#### Composite-key foreign keys (F.2)

Multi-column FKs declared on the model, not the field. Each
participating column keeps its plain Rust type — the FK metadata
records which columns participate and where they reference.

```rust
#[derive(Model)]
#[rustango(
    table = "audit_log",
    fk_composite(
        name = "target",
        to   = "ct_live_pair",
        from = ("entity_table", "entity_pk"),
        on   = ("table_name",   "row_pk"),
    ),
)]
pub struct AuditLog {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub entity_table: String,
    pub entity_pk: i64,
}

// Emits on migrate / apply_all:
//   ALTER TABLE "audit_log" ADD CONSTRAINT "audit_log_target_fkey"
//     FOREIGN KEY ("entity_table", "entity_pk")
//     REFERENCES "ct_live_pair" ("table_name", "row_pk");
```

Both Postgres and MySQL emit the standard composite FK syntax —
only identifier quoting differs. Single-column FKs continue to use
the existing per-field `Relation::Fk` machinery; composite FKs sit
on `ModelSchema.composite_relations` so the existing single-FK
machinery (admin display, snapshot diff) stays untouched.

#### GenericForeignKey + soft-FK prefetch (F.3)

```rust
use rustango::contenttypes::{GenericForeignKey, prefetch_soft, prefetch_generic};

// `GenericForeignKey` — a runtime pointer at any registered model's row.
let gfk = GenericForeignKey::for_target::<Post>(&pool, post_id).await?;
// gfk.content_type_id  ← Post's ContentType row id
// gfk.object_pk        ← post_id
```

**Soft-FK prefetch** — for integer columns that conceptually point at
another model's PK without a declared `Relation::Fk` (audit log
`entity_pk`, denormalized snapshots, optional cross-app refs):

```rust
let parent_pks: Vec<i64> = posts.iter().map(|p| p.id.get().copied().unwrap()).collect();

// One batched SELECT + group-by-extractor → HashMap<i64, Vec<C>>
let by_post = prefetch_soft::<Comment, _>(
    &pool,
    &parent_pks,
    "post_id",            // soft-FK column on Comment
    |c| c.post_id,        // extractor: how to read the value off &Comment
).await?;

for post in &posts {
    let pk = post.id.get().copied().unwrap();
    let comments = by_post.get(&pk).map(Vec::as_slice).unwrap_or(&[]);
    // ...
}
```

**Generic-FK prefetch** — for `(content_type_id, object_pk)` pointers
that vary their target type per row. Single-target-type variant —
caller picks the concrete `T: Model` to hydrate; pairs whose
`content_type_id` doesn't match are filtered out:

```rust
let pairs: Vec<(i64, i64)> = audit_rows.iter()
    .map(|a| (a.target.content_type_id, a.target.object_pk))
    .collect();

let posts: HashMap<(i64, i64), Post> =
    prefetch_generic::<Post>(&pool, &pairs).await?;

for row in &audit_rows {
    if let Some(post) = posts.get(&(row.target.content_type_id, row.target.object_pk)) {
        println!("{} → {}", row.id.get().copied().unwrap(), post.title);
    }
}
```

Both prefetch helpers short-circuit on empty input (no DB round trip)
and use a single batched SELECT for the actual fetch.

**What this unblocks:**
- **Permissions (Option G, v0.16.0)** — `permission.content_type_id`
  is a real FK to `rustango_content_types.id` instead of a
  hard-coded `app.action_model` string that breaks when two apps
  register the same model name.
- **Audit history admin panels** — `User.history.all()`-style
  queries become composite-FK joins instead of raw SQL.
- **Comments / tags / generic FK** — one `Comment` model points
  at any `Post` / `Photo` / `Article` via `(content_type_id,
  object_pk)`, queried + admin-rendered in one shape.
- **Activity stream / "recently changed" feeds** — the target of
  each entry hydrates in one batched `prefetch_generic` call per
  target type, no N+1.

**Deferred (follow-up slice):**
- Boxed-trait dynamic decoder registry → `prefetch_generic_dyn`
  for mixed-target hydration in one query.
- Admin renderer for `GenericForeignKey` columns (clickable target
  links in list/detail).
- `composite_relations` snapshot/diff support in `make_migrations`.

---

## Migrations

```bash
cargo run -- makemigrations              # diff inventory ↔ snapshot, emit JSON
cargo run -- migrate                     # apply pending
cargo run -- migrate --dry-run           # print SQL only
cargo run -- migrate <target>            # forward or back to specific name
cargo run -- downgrade 2                 # step back 2 migrations
cargo run -- showmigrations              # status
```

Migration files are JSON in `migrations/`, lex-sorted by name. They:

- **Embed the full schema snapshot** — any one file is a self-contained starting state
- **Include both schema ops AND data ops** in the `forward` list, in any order
- **Are invertible** when each op carries `reverse_sql` (or has a natural inverse)
- **Run atomically per file** by default — partial progress recoverable

### Auto-detected schema changes

| Change | Op generated | Notes |
|---|---|---|
| New struct | `CreateTable` | + deferred FK constraints |
| Removed struct | `DropTable` | CASCADE |
| New field | `AddColumn` | rejects NOT NULL without `default` |
| Removed field | `DropColumn` | |
| Type changed | `AlterColumnType` | with `USING ::pg_type` cast |
| Nullable flipped | `AlterColumnNullable` | |
| Default changed | `AlterColumnDefault` | |
| `max_length` changed | `AlterColumnMaxLength` | VARCHAR(N) ↔ TEXT |
| `unique` toggled | `AlterColumnUnique` | |
| New `#[rustango(index)]` / composite index | `CreateIndex` | unique flag respected |
| New `#[rustango(check(...))]` | `AddCheckConstraint` | |
| New M2M relation | `CreateM2MTable` | composite PK + 2 FKs ON DELETE CASCADE |
| Renames | NOT auto-detected | use `cargo run -- makemigrations --empty` and edit JSON |

### Hand-authored data migrations

```bash
# Quick path — one-liner
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Or append to an existing migration
cargo run -- add-data-op --to 0003_add_slug --sql "UPDATE posts SET slug = id::text"

# Or scaffold an empty file and edit manually
cargo run -- makemigrations --empty seed_initial_categories
```

---

## Auto-admin

Mount once and every `#[derive(Model)]` is fully editable:

```rust
let app = rustango::admin::Builder::new(pool.clone())
    .title("My App Admin")
    .show_only(["post", "author", "tag"])
    .read_only(["audit_log"])
    .build();
```

Lives at `/__admin/`. Per-model customization via `#[rustango(admin(...))]`:

| Knob | Effect |
|---|---|
| `list_display = "f1, f2, f3"` | Columns on list view (FKs render display-name) |
| `search_fields = "f1, f2"` | `?q=...` search box |
| `list_filter = "fk_field, status"` | Right-rail facet filters with counts |
| `ordering = "field, -other"` | Default sort (`-` prefix = DESC) |
| `list_per_page = 50` | Pagination size |
| `readonly_fields = "created_at"` | Shown but not editable |
| `fieldsets = "Group A: f1, f2 \| Group B: f3"` | Form layout |
| `actions = "delete_selected, my_action"` | Bulk actions |

### Bulk actions

```rust
use rustango::bulk_actions::{BulkActionRegistry, BulkDeleteAction, BulkSoftDeleteAction};
use std::sync::Arc;

let registry = BulkActionRegistry::new()
    .register(Arc::new(BulkDeleteAction))
    .register(Arc::new(BulkSoftDeleteAction { column: "deleted_at" }));

// In a handler:
let result = registry.run("delete_selected", "posts", &[1, 2, 3], &pool).await?;
println!("affected {} rows", result.affected);
```

### Theming + per-tenant branding (v0.26)

Both admin surfaces — the per-tenant Django-shape admin and the
operator console — share a CSS-variable token vocabulary
(`--color-bg`, `--color-fg`, `--color-accent`, `--space-*`,
`--font-*`, `--radius-*`, `--shadow-*`). Total customization without
rewriting HTML, just CSS-variable overrides. Light by default,
`[data-theme="dark"]` override, `prefers-color-scheme` auto-switch.
A fixed-position theme toggle button (auto → light → dark) persists
to `localStorage` with a no-flash inline `<head>` script.

**Per-tenant branding** rides the framework's existing `Storage`
trait — wire any backend (LocalStorage, S3, R2, B2, MinIO, custom)
through `TenantAdminBuilder::brand_storage(...)` /
`operator_console::router_with_brand_storage(...)`. When the backend
exposes URLs (`Storage::url(key)`), rendered `<img src>` tags point
straight at the origin or CDN — the `/__brand__/{slug}/{filename}`
fallback handler is only mounted for backends that return `None`.

`Org` carries six brand columns: `brand_name`, `brand_tagline`,
`logo_path`, `favicon_path`, `primary_color`, `theme_mode`. Edit live
through the existing operator-console org-edit form; logo / favicon
upload via the same form's multipart sub-form. `primary_color`
drives a derived `--color-accent` / `--color-accent-hover` /
`--color-accent-bg-soft` triple via `branding::build_brand_css(&org)`,
safelisted to hex-only values (no raw CSS from operator input).

Operator-console branding is env-driven for the global UI:

```bash
RUSTANGO_OPERATOR_BRAND_NAME="Acme Operations"
RUSTANGO_OPERATOR_TAGLINE="Internal admin"
RUSTANGO_OPERATOR_LOGO_URL="https://cdn.example.com/acme-ops.png"
RUSTANGO_OPERATOR_PRIMARY_COLOR="#2c5fb0"
RUSTANGO_OPERATOR_THEME_MODE="auto"
```

### Session invalidation on password rotation (v0.28.4)

Sessions minted before a password change are now invalidated on
the next request instead of remaining valid until their TTL
expires. Both consoles (tenant admin + operator console) check the
cookie's `iat` (issued-at, stamped at mint time) against the
account's `password_changed_at` column on every authenticated
request — sessions older than the rotation get bounced to login.

The lookup is folded into the existing per-request `is_superuser`
/ `active` query, so there's no extra round-trip. Existing
deployments pick up the new column on the next `cargo run --
migrate` (idempotent `ALTER TABLE … ADD COLUMN IF NOT EXISTS`).
Cookies minted by pre-0.28.4 servers stay parseable
(`#[serde(default)]` on the new field) — their `iat` decodes as
`0` so they're invalidated by any future password change.

### Self-serve change-password page + CLI ergonomics (v0.28.2)

Tenant users can change their own password without an operator
in the loop. The admin sidebar shows a **Change password** link
when the tenant admin is wired with a session secret; clicking
it lands on `/__change-password` (URL configurable via
`RouteConfig::change_password_url`, defaults to
`/change-password` under `friendly()`). The form takes the
current password (verified server-side), a new password, and a
confirmation; on success it stores the new Argon2id hash and
redirects with a "Password updated" banner.

Two new CLI verbs cover the symmetric flow:

```sh
# Self-serve symmetric: requires the current password.
cargo run -- change-password acme alice
cargo run -- change-operator-password admin

# Operator-driven recovery (no current pw needed):
cargo run -- reset-password acme alice
cargo run -- reset-operator-password admin
```

Every password verb (`create-operator`, `create-user`,
`reset-password`, `reset-operator-password`, `change-password`,
`change-operator-password`) accepts a `--generate` flag that
emits a 20-character secure random password from a 58-char
unambiguous alphabet (no `0/O`, `1/l/I`):

```sh
cargo run -- create-superuser acme alice --generate
# created user `alice` in tenant `acme` (id 1, superuser=true)
#   generated password: kT3nx9pZQRgwYjvFmCdh
#   store this safely — it won't be shown again
```

`--password` and `--generate` are mutually exclusive. Sessions
issued before a password change currently remain valid until
they expire — `password_changed_at` cookie invalidation is on
the v0.29 roadmap.

### Users / roles / permissions admin (v0.28.1)

Every framework auth + RBAC table is admin-visible in a tenant
admin: `rustango_users`, `rustango_roles`, `rustango_role_permissions`,
`rustango_user_roles`, `rustango_user_permissions`. The four junction
models carry `admin(...)` config so list views render
`role_id, codename`, `user_id, role_id`, and
`user_id, codename, granted` with sensible ordering.

Visiting a user's detail page (`/{admin_url}/rustango_users/{id}`)
renders a **Roles & permissions panel** showing the user's assigned
roles (linked through to each role's detail page) and their
**effective codenames** — the union of role grants + direct grants
minus explicit denials, computed by the same SQL the runtime
`has_perm` check uses. Quick links beneath the panel jump to the
four manage-able junction tables for inline editing. The panel is
read-only at the user level: assign / revoke flows go through the
junction-table admin pages.

When the permission tables haven't been seeded
(`tenancy::ensure_permission_tables(&pool)` not called), the panel
hides itself silently — same posture as the audit-trail panel.

---

## APIs (ViewSet + Serializer + JWT)

### `#[derive(ViewSet)]` — full CRUD in 5 lines

```rust
use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
    permissions(
        list     = "post.view",
        retrieve = "post.view",
        create   = "post.add",
        update   = "post.change",
        destroy  = "post.delete",
    ),
)]
pub struct PostViewSet;

// Mount:
let app = Router::new()
    .merge(PostViewSet::router("/api/posts", pool.clone()));
```

### Endpoints

| Method | Path | Action |
|---|---|---|
| `GET` | `/api/posts` | List (page-number or cursor pagination) |
| `POST` | `/api/posts` | Create |
| `GET` | `/api/posts/{pk}` | Retrieve |
| `PUT` | `/api/posts/{pk}` | Update |
| `PATCH` | `/api/posts/{pk}` | Partial update |
| `DELETE` | `/api/posts/{pk}` | Delete (soft when model carries `#[rustango(soft_delete)]`) |

### Query params (list endpoint)

```
?page=2&page_size=50                            # page-number pagination
?cursor=eyJpZCI6MTAwfQ&page_size=50             # cursor pagination (opt-in)
?search=rust&ordering=-published_at             # search + sort
?author_id=42                                    # exact filter
?published_at__gte=2026-01-01                    # Django-style lookup operators
?title__icontains=draft                          # gt, gte, lt, lte, ne, in, not_in,
                                                 # contains, icontains, startswith,
                                                 # istartswith, endswith, iendswith, isnull
```

### Serializers

```rust
use rustango::Serializer;
use rustango::serializer::ModelSerializer;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: i64,
    pub title: String,

    #[serializer(read_only)]
    pub created_at: chrono::DateTime<chrono::Utc>,

    #[serializer(write_only)]
    pub draft_password: String,

    #[serializer(source = "body")]                // read from model.body
    pub content: String,

    #[serializer(skip)]                            // user sets manually
    pub tag_ids: Vec<i64>,
}

// Use:
let s = PostSerializer::from_model(&post);
let json = s.to_value();
let array = PostSerializer::many_to_value(&posts);
```

### JWT lifecycle

```rust
use rustango::tenancy::jwt_lifecycle::{JwtLifecycle, JwtTokenPair};
use serde_json::json;

let jwt = JwtLifecycle::new(secret).with_access_ttl(900).with_refresh_ttl(7 * 86400);

// Login: embed roles + scope in the token
let pair = jwt.issue_pair_with(user_id, json!({
    "roles":  ["admin", "editor"],
    "tenant": "acme",
    "scope":  "read:posts write:posts",
}).as_object().unwrap().clone())?;

// Authenticated request — no DB lookup needed:
let claims = jwt.verify_access(&access_token).ok_or(StatusCode::UNAUTHORIZED)?;
let roles: Vec<String> = claims.get_custom("roles").unwrap();

// Refresh — preserves custom claims:
let new_pair = jwt.refresh(&refresh_token).ok_or(StatusCode::UNAUTHORIZED)?;

// Refresh with re-evaluated permissions:
let downgraded = jwt.refresh_with(&refresh_token, json!({"roles": ["viewer"]})...)?;

// Logout:
jwt.revoke(&access_token);
jwt.revoke(&refresh_token);
```

Reserved-claim defense: `sub`, `exp`, `jti`, `typ` cannot appear in `custom` (returns `JwtIssueError::ReservedClaim`).

For tenancy projects there's a one-liner that mounts the four-route
JWT auth surface (`/api/auth/login` / `/refresh` / `/logout` / `/me`):

```rust
use rustango::tenancy::auth_routes;

// Pick up `[auth.jwt] access_ttl_secs / refresh_ttl_secs` from the
// loaded Settings; falls through to defaults (15min / 7d) when unset.
let cfg = rustango::config::Settings::load_from_env()?;
let auth = auth_routes::Config::default().with_jwt_settings(&cfg.auth.jwt);
let api = my_app::api().merge(auth_routes::jwt_router(auth));
```

The endpoints are tenant-aware via the `Tenant` extractor, so the
JWT's `tenant` claim is matched against the resolved subdomain — a
token minted on `acme.example.com` is rejected on `globex.example.com`.

---

## HTML views (Django-shape CBVs)

`rustango::template_views` is the HTML-side sibling of `viewset` —
generic class-based views that build a Tera-rendered axum `Router`
over any `#[derive(Model)]` schema. The full Django-shape CRUD
surface ships: `ListView`, `DetailView`, `CreateView`, `UpdateView`,
`DeleteView`.

```rust
use rustango::template_views::{ListView, DetailView, CreateView, UpdateView, DeleteView};
use std::sync::Arc;
use tera::Tera;

let tera = Arc::new(/* …Tera with posts_list / posts_detail / posts_form / posts_confirm_delete… */);

let app = axum::Router::new()
    .merge(ListView::for_model(Post::SCHEMA)
        .page_size(20)
        .order_by("created_at", true)        // DESC
        .router("/posts", tera.clone(), pool.clone()))
    .merge(DetailView::for_model(Post::SCHEMA)
        .router("/posts", tera.clone(), pool.clone()))
    .merge(CreateView::for_model(Post::SCHEMA)
        .success_url("/posts")
        .router("/posts", tera.clone(), pool.clone()))
    .merge(UpdateView::for_model(Post::SCHEMA)
        .success_url("/posts")
        .router("/posts", tera.clone(), pool.clone()))
    .merge(DeleteView::for_model(Post::SCHEMA)
        .success_url("/posts")
        .router("/posts", tera.clone(), pool.clone()));
```

| view | URL | template default | context |
|------|-----|------------------|---------|
| `ListView` | `GET <prefix>` | `<table>_list.html` | `object_list`, `page`, `page_size`, `total`, `total_pages`, `has_next`, `has_prev` |
| `DetailView` | `GET <prefix>/{pk}` | `<table>_detail.html` | `object` |
| `CreateView` | `GET`/`POST <prefix>/new` | `<table>_form.html` | `form: { fields, errors }`, `is_create=true` |
| `UpdateView` | `GET`/`POST <prefix>/{pk}/edit` | `<table>_form.html` | `form: { fields, errors }`, `object`, `pk`, `is_update=true` |
| `DeleteView` | `GET`/`POST <prefix>/{pk}/delete` | `<table>_confirm_delete.html` | `object` (GET only) |

CreateView/UpdateView/DeleteView are all two-step: GET renders a
form/confirmation page, POST mutates and 303s to `success_url`.
Form views auto-skip the PK and `Auto<T>` columns from the rendered
field set, parse `application/x-www-form-urlencoded`, coerce values
to the field's declared SQL type, and re-render with errors + a 422
status on validation failure (preserving what the user typed).

CSRF protection is the project's responsibility — mount under a
CSRF-protected scope when the POSTs are reachable from a browser.

**Tenancy projects** swap `.router(prefix, tera, pool)` for
`.tenant_router(prefix, tera)` — each request resolves its own
connection via the `Tenant` extractor instead of capturing a single
pool at mount time. Same Tera context shape, same builder knobs.

Single-tenant only today (captures a `PgPool` at mount time, like
the original `viewset::ViewSet::router`). The `tenant_router`
variant lands once the per-tenant pattern matches `viewset::tenant_router`.

Behind the `template_views` Cargo feature (default-on).

---

## Forms

```rust
use rustango::forms::{Form, FormErrors, ModelForm};
use rustango::Form as DeriveForm;

// Typed form via derive
#[derive(DeriveForm)]
pub struct ContactForm {
    #[form(min_length = 1, max_length = 200)]
    pub name: String,
    #[form(required = false)]
    pub message: Option<String>,
}

// In a handler:
match ContactForm::parse(&form_data) {
    Ok(form) => { /* form.name, form.message */ }
    Err(errors) => { /* errors.fields() per-field map */ }
}

// Or schema-driven for any model
let form = ModelForm::new(Post::SCHEMA, form_data);
match form.save(&pool).await {
    Ok(pk) => redirect_to_detail(pk),
    Err(ModelFormError::Validation(errors)) => render_with_errors(errors),
    Err(ModelFormError::Database(e)) => server_error(e),
}
```

---

## Multi-tenancy

```rust
use rustango::server::Builder;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Builder::from_env().await?
        .admin_title("My SaaS Admin")
        .migrate(".")
        .await?
        .api(my_app::urls::router())
        .seed_with(seed)
        .await?
        .serve("0.0.0.0:8080")
        .await
}
```

| Env var | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | — | Registry Postgres (orgs, operators, users) |
| `RUSTANGO_APEX_DOMAIN` | `localhost` | Subdomain root → `<slug>.<apex>` |
| `RUSTANGO_BIND` | `0.0.0.0:8080` | Bind address |
| `RUSTANGO_SESSION_SECRET` | random (warns) | Base64-encoded 32-byte HMAC key |
| `RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS` | `3600` | "Open admin as superuser →" cookie lifetime (#78) |

Generate a secret: `openssl rand -base64 32`.

### Tenant pool tuning (v0.27.7+)

Database-mode tenants get one cached `PgPool` each. Defaults
preserve pre-0.27.7 behavior; override via `TenantPoolsConfig`:

```rust
use rustango::tenancy::{TenantPools, TenantPoolsConfig};

let pools = TenantPools::new(registry).config(TenantPoolsConfig {
    database_pool_min_connections: 1,                        // keep 1 conn warm
    database_pool_acquire_timeout: Duration::from_secs(10),
    database_pool_max_lifetime: Some(Duration::from_secs(60 * 60)),
    prewarm_active_tenants: true,                            // build pools at boot
    ..Default::default()
});
```

CLI: `cargo run -- prewarm-pools` for an explicit ops trigger
after credential rotation.

### Configurable URL prefixes (v0.28.0)

Default tenant admin paths use `__` prefixes (`/__login`,
`/__admin`, `/__audit`, …). Apps that have reserved their root
namespace cleanly can flip to friendly URLs:

```rust
use rustango::tenancy::RouteConfig;

Builder::from_env().await?
    .routes(RouteConfig::friendly())     // /login, /admin, /audit, …
    // or build custom: RouteConfig { login_url: "/sign-in".into(), .. Default::default() }
    .api(my_app::urls::router())
    .serve("0.0.0.0:8080")
    .await
```

Defaults preserve pre-0.28 paths so upgrading is a no-op until
apps explicitly call `.routes(...)`.

### Operator-as-superuser tenant impersonation (v0.27.8+)

Operators logged into the apex console (`/orgs/<slug>/edit`)
get an **"Open admin as superuser →"** button. Click → the
operator console mints a tenant-bound, slug-pinned, signed
cookie (1h TTL by default) and redirects to
`<slug>.<apex>/__admin/`. The tenant admin recognizes the
cookie as superuser; an unmissable banner reminds the operator
they're impersonating; every audited write tags
`source = operator:<id>:impersonating` so post-hoc forensics
can pinpoint operator-driven changes. **End impersonation**
button clears the cookie + redirects back to the operator
console.

### Recovery CLI verbs

```sh
cargo run -- create-superuser <slug> <username> --password <p>
cargo run -- set-superuser <slug> <username> [--on|--off]
cargo run -- reset-password <slug> <username> --password <new>
cargo run -- reset-operator-password <username> --password <new>
cargo run -- migrate --fake <name>      # backfill ledger without running SQL
cargo run -- prewarm-pools              # warm every active database-mode pool
```

First user of a tenant is **auto-promoted to superuser** even
without `--superuser` so an onboarding script that forgets the
flag still produces a tenant with at least one functional admin.

### Tenant resolver chain

Auto-resolves `Tenant` from request via, in order:
1. Subdomain (`acme.myapp.com`)
2. URL path prefix (`/t/acme/...`)
3. Custom header (`X-Tenant-Slug: acme`)
4. Port (rare; for testing)

### Storage modes

- **Schema mode** — one Postgres schema per tenant, single database
- **Database mode** — full database per tenant; `TenantPools` lazily opens connections

Flip a populated tenant between modes with `cargo run --
migrate-tenant-storage <slug> --to schema|database` (since v0.26).
The verb runs `pg_dump` → `psql`, updates the `Org` row in a single
transaction, evicts the cached pool, and smoke-checks the new
location with `SELECT 1 FROM rustango_users LIMIT 1`. `--dry-run`
short-circuits before any pg_dump call to preview the move.

### Programmatic provisioning

```rust
use rustango::tenancy::manage::api::*;

let org = create_tenant_if_missing(&pools, &registry_url, "migrations", "acme",
    CreateTenantOpts {
        mode: StorageMode::Schema,
        display_name: Some("ACME Corp".into()),
        ..Default::default()
    },
).await?;

create_operator_if_missing(&pools, "admin", "letmein").await?;
create_user_if_missing(&pools, "acme", "alice", "hunter2", true).await?;
```

### Extra fields on tenant users

The framework's tenant `User` is fixed at seven columns: `id`,
`username`, `password_hash`, `is_superuser`, `active`, `created_at`,
plus `data: serde_json::Value` (JSONB) for ad-hoc per-user metadata.
Three escalating options when you want more:

**1. Stuff it in `data` (zero-cost).** No schema change, no override —
read/write `user.data["display_name"]`. Right answer for sparse,
non-indexed attributes (preferences, onboarding flags, app-specific
settings).

**2. Sibling profile model with FK (works on any project).** When you
want typed, indexable extras without touching `rustango_users`:

```rust
#[derive(rustango::Model)]
pub struct UserProfile {
    #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "rustango_users")] pub user_id: i64,
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
```

Run `cargo run -- makemigrations && cargo run -- migrate`. One JOIN
per access; no risk of conflicting with framework auth.

**3. Custom user model (greenfield only).** Extras go inline on
`rustango_users` itself via the `TenantUserModel` trait. The override
is read by `manage init-tenancy` and `Builder::migrate` to write the
bootstrap migration's `CREATE TABLE` with your extra columns:

```rust
use rustango::sql::Auto;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)] pub username: String,
    #[rustango(max_length = 255)] pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")] pub data: serde_json::Value,
    // extras —
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
impl rustango::tenancy::TenantUserModel for AppUser {}

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .api(my_app::urls::router())
        .tenancy()
        .user_model::<AppUser>()
        .run().await
}
```

Then:

```bash
# If you used `cargo rustango new --template tenant`, delete the
# scaffolder-written bootstrap JSONs — `init-tenancy` is idempotent
# and won't replace them otherwise:
rm migrations/0001_rustango_registry_initial.json
rm migrations/0001_rustango_tenant_initial.json

cargo run -- init-tenancy        # writes 0001_*.json from AppUser's schema
cargo run -- migrate
```

Constraints:
- Your model must declare every framework column verbatim (`id`,
  `username`, `password_hash`, `is_superuser`, `active`, `created_at`,
  `data`); `validate_tenant_user_schema` panics with a clear message at
  `init-tenancy` time otherwise. Extras must be `NULL`-able or carry
  `default = "…"`.
- Both the framework's `User` and your `AppUser` register in the model
  inventory (same `table`). Subsequent `makemigrations` runs may emit
  redundant ops touching `rustango_users` — review the JSON before
  applying. This is why option 3 is greenfield-only; option 2 sidesteps
  it.
- The framework's auth and admin paths still read the seven core
  columns by name — extras are accessible via
  `AppUser::objects().fetch(...)`.

`Builder::user_model::<AppUser>()` is the equivalent setter for code
that constructs the server `Builder` directly. Full reference in
[docs/manage.md](docs/manage.md#custom-user-model-extra-columns-on-rustango_users).
Runnable demo:
[`crates/rustango/examples/tenant_user_extension/`](crates/rustango/examples/tenant_user_extension/).

---

## Authentication & permissions

### Auth backends (pluggable)

```rust
use rustango::tenancy::auth_backends::{ModelBackend, ApiKeyBackend, JwtBackend};
use rustango::tenancy::middleware::{RouterAuthExt, CurrentUser};
use std::sync::Arc;

let backends = vec![
    Arc::new(ModelBackend) as _,                  // username + password (Basic Auth)
    Arc::new(ApiKeyBackend) as _,                  // Bearer <prefix>.<secret>
    Arc::new(JwtBackend::new(secret)) as _,        // Bearer <jwt>
];

let app = Router::new()
    .route("/me", get(profile))
    .require_auth(backends.clone(), pool.clone())  // 401 if no backend authenticates
    .route("/posts/new", post(create_post))
    .require_perm("post.add", pool.clone())        // gate by codename
    .require_auth(backends, pool);

async fn profile(CurrentUser(u): CurrentUser) -> impl IntoResponse {
    match u {
        Some(user) => format!("hello {}", user.username).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}
```

### Typed permission helpers (v0.16.0)

Permissions today use `{table}.{action}` string codenames
(`"post.change"`, `"comment.delete"`, …). The
`rustango::permissions` facade adds typed convenience over the
existing engine so callers reach for permissions by their
`T: Model` type instead of hand-typing the codename:

```rust
use rustango::permissions;

// Old (still supported):
rustango::tenancy::permissions::has_perm(uid, "post.change", &pool).await?;

// New, typed:
permissions::has_perm_for_model::<Post>(uid, "change", &pool).await?;

// Bulk helpers exist for grant/revoke/set_user_perm/clear_user_perm too:
permissions::grant_role_perm_for_model::<Post>(editor_role, "change", &pool).await?;
permissions::set_user_perm_for_model::<Post>(uid, "delete", false, &pool).await?;

// Build the four standard codenames for a model:
let [add, change, delete, view] = permissions::model_codenames_for::<Post>();
```

The full underlying engine (`Role`, `RolePermission`, `UserRole`,
`UserPermission`, `has_perm` / `has_any_perm` / `has_all_perms`,
`assign_role` / `grant_role_perm` / `auto_create_permissions`)
lives at [`rustango::tenancy::permissions`] — re-exported from
the top-level `rustango::permissions` for the conceptually-cleaner
path. Requires the `tenancy` feature (the underlying tables live
in the tenancy bootstrap migration).

### TOTP / 2FA

```rust
use rustango::totp::{TotpSecret, otpauth_url, generate, verify};

// Enrollment:
let secret = TotpSecret::generate();
user.totp_secret = secret.to_base32();
let qr_url = otpauth_url("MyApp", &user.email, &secret);   // encode as QR

// Verification on login:
if !verify(&secret, &user_supplied_code, 30, 6, 1) {
    return Err("bad TOTP code");
}
```

### API keys

```rust
use rustango::api_keys::{generate_key, verify_key, split_token};

// Issue:
let (full_token, prefix, hash) = generate_key()?;
// Show full_token to user once. Store prefix + hash in your DB.

// Verify on request:
let (prefix, secret) = split_token(&inbound).ok_or(BadRequest)?;
let row = db_lookup_by_prefix(prefix).await?;
if verify_key(secret, &row.hash)? { /* ok */ }
```

### Signed URLs (magic links / file downloads)

```rust
use rustango::signed_url::{sign, verify};
use std::time::Duration;

// Issue:
let url = sign(
    "https://app.example.com/login?email=alice@x.com",
    secret,
    Some(Duration::from_secs(3600)),
);

// Verify on callback:
match verify(&incoming_url, secret) {
    Ok(()) => { /* identity confirmed */ }
    Err(e) => { /* expired or tampered */ }
}
```

---

## Security middleware

All optional, all chainable on any axum Router.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::ip_filter::{IpFilterLayer, IpFilterRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::etag::{EtagLayer, EtagRouterExt};
use std::time::Duration;

let app = Router::new()
    .route("/api/posts", get(list_posts).post(create_post))
    .security_headers(                                          // HSTS + XFO + CSP + Permissions-Policy
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    )
    .cors(                                                      // CORS allowlist
        CorsLayer::new()
            .allow_origins(vec!["https://app.example.com"])
            .allow_methods(vec!["GET", "POST", "PUT", "DELETE"])
            .allow_credentials(true),
    )
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))   // 60 req/min/IP
    .ip_filter(                                                 // optional allowlist
        IpFilterLayer::block(vec!["203.0.113.42"]).unwrap(),
    )
    .request_id(RequestIdLayer::default())                      // X-Request-Id for log correlation
    .access_log(AccessLogLayer::default())                      // tracing::info per request
    .etag(EtagLayer::default());                                // 304 Not Modified
```

### Security headers presets

| Preset | When to use |
|---|---|
| `SecurityHeadersLayer::strict()` | Production: HSTS preload + XFO=DENY + nosniff + Referrer-Policy=no-referrer |
| `SecurityHeadersLayer::relaxed()` | Embeddable apps: SAMEORIGIN + 1y HSTS |
| `SecurityHeadersLayer::dev()` | Local: nosniff only (no HSTS lockout) |
| `SecurityHeadersLayer::empty()` | Build up from scratch |

`SecurityHeadersLayer::from_settings(&Settings.security)` builds the
layer from TOML — picks the preset by name (`headers_preset =
"strict" | "relaxed" | "dev" | "none"`), then layers per-field
overrides on top (`csp`, `hsts_max_age_secs`):

```rust
let cfg = rustango::config::Settings::load_from_env()?;
app.layer(SecurityHeadersLayer::from_settings(&cfg.security).into_layer())
```

Unknown preset names fail-safe to `strict()` — a typo in the TOML
shouldn't silently strip protection. (`manage check --deploy`
warns separately when the resolved preset is `dev` / `none` in the
prod tier.)

### CORS presets

```rust
CorsLayer::permissive()                          // dev: any origin, common methods
CorsLayer::new().allow_origins(vec!["..."])      // prod: explicit allowlist
```

`CorsLayer::from_settings(&Settings.security)` builds the layer
from `[security] cors_allowed_origins` (#87 wiring). Returns
`None` when the list is empty so callers skip mounting altogether
(different from "allow zero origins" which would 403 every preflight).
`["*"]` maps to `permissive()`; specific origins build an allowlist
with sensible default methods + headers + 1h preflight cache:

```rust
let cfg = rustango::config::Settings::load_from_env()?;
if let Some(layer) = CorsLayer::from_settings(&cfg.security) {
    app = app.layer(layer.into_layer());
}
```

---

## Caching

```rust
use rustango::cache::{Cache, BoxedCache, InMemoryCache, get_json, set_json, get_or_set};
use std::sync::Arc;
use std::time::Duration;

// Build a shared cache
let cache: BoxedCache = Arc::new(
    InMemoryCache::with_default_ttl(Duration::from_secs(300))
);

// Raw strings
cache.set("greeting", "hello", Some(Duration::from_secs(60))).await?;
let val: Option<String> = cache.get("greeting").await?;

// Typed JSON helpers
set_json(&*cache, "user:1", &user, None).await?;
let user: Option<User> = get_json(&*cache, "user:1").await?;

// Fetch-or-compute pattern
let posts: Vec<Post> = get_or_set(
    &*cache,
    "posts:recent",
    || async { Post::objects().fetch(&pool).await.unwrap() },
    Some(Duration::from_secs(60)),
).await?;
```

### Redis backend (`cache-redis` feature)

```rust
use rustango::cache::redis_backend::RedisCache;

let cache: BoxedCache = Arc::new(
    RedisCache::new("redis://127.0.0.1/").await?
);
```

---

## Email + storage + scheduling

### Email

```rust
use rustango::email::{Mailer, Email, ConsoleMailer, InMemoryMailer, NullMailer};

let mailer: Arc<dyn Mailer> = Arc::new(ConsoleMailer);   // dev: prints to stdout

let email = Email::new()
    .to("user@example.com")
    .from("noreply@app.example.com")
    .reply_to("support@app.example.com")
    .subject("Welcome")
    .body("Plain text version")
    .html_body("<h1>Welcome</h1>");
mailer.send(&email).await?;
```

### File storage

The framework ships three layers, each useful on its own:

1. **`Storage` trait + backends** — write/read/delete/url + presign.
2. **`StorageRegistry`** — named "disks" (Laravel-style) with optional CDN prefixes. Pick the right backend per call site by name.
3. **`Media` model + `MediaManager`** — first-class Postgres-backed file references with direct browser uploads, soft delete, and orphan sweeps.

#### Storage backends

```rust
use rustango::storage::{Storage, BoxedStorage, LocalStorage};
use rustango::storage::s3::{S3Storage, S3Config};
use std::sync::Arc;

// Local disk:
let local: BoxedStorage = Arc::new(
    LocalStorage::new("./uploads".into())
        .with_base_url("https://cdn.example.com/uploads")
);
local.save("avatars/alice.png", &png_bytes).await?;

// AWS S3 (or Cloudflare R2 / Backblaze B2 / MinIO — same struct):
let s3: BoxedStorage = Arc::new(S3Storage::new(S3Config {
    bucket: "my-bucket".into(),
    region: "us-east-1".into(),
    endpoint: None,                    // Some("https://...") for R2/B2/MinIO
    access_key_id: env::var("AWS_ACCESS_KEY_ID")?,
    secret_access_key: env::var("AWS_SECRET_ACCESS_KEY")?,
    path_style: false,                 // true for R2/MinIO
}));

// Trait methods are identical across backends — swap in config.
s3.save("avatars/alice.png", &png_bytes).await?;
let bytes = s3.load("avatars/alice.png").await?;
let public_url = s3.url("avatars/alice.png");
```

The S3 backend is hand-rolled SigV4 over `reqwest` — no `aws-sdk-s3` dependency. Behind the `storage-s3` feature flag (default-on).

#### Presigned URLs (private files + direct browser uploads)

```rust
use std::time::Duration;

// Time-limited GET: paste into <img src=...> or <a href=...>
let download_url = s3
    .presigned_get_url("invoices/2026.pdf", Duration::from_secs(3600))
    .await
    .expect("S3 backend signs");

// Time-limited PUT: browser uploads directly, server never proxies the body.
// Content-Type binding — browser MUST send a matching header (S3 enforces).
let upload_url = s3
    .presigned_put_url("uploads/x.png", Duration::from_secs(300), Some("image/png"))
    .await
    .unwrap();
```

`LocalStorage` and `InMemoryStorage` return `None` from these methods (they can't sign). `S3Storage` (and any S3-compatible API) does. AWS caps presign expiry at 7 days; we clamp.

#### `StorageRegistry` — named disks

```rust
use rustango::storage::StorageRegistry;

let registry = StorageRegistry::new()
    .set("avatars", Arc::new(s3))
    .cdn("avatars", "https://cdn.example.com/avatars")
    .set("docs",    Arc::new(docs_s3))
    .set("cache",   Arc::new(local))
    .with_default("avatars");

let s = registry.disk("avatars").unwrap();
let url = registry.cdn_url("avatars", "alice.png");
//        → "https://cdn.example.com/avatars/alice.png"

let internal = registry.origin_url("avatars", "alice.png");
//        → bypasses CDN — for internal admin tools
```

#### First-class `Media` model

`Media` is a Postgres-backed row. User models reference it via a normal integer FK (`Option<ForeignKey<Media>>`) — all metadata (disk, key, MIME, size, filename, free-form JSONB) lives on the `Media` row, so deletes are atomic and one file can be referenced by N parents without duplication.

```rust
use rustango::media::{Media, MediaManager, SaveOpts, UploadIntent};

// Once at startup:
Media::ensure_table(&pool).await?;
let manager = MediaManager::new(pool.clone(), registry);

// Server-side save: writes to S3 + inserts a Media row in one call.
let m = manager.save_bytes(SaveOpts {
    disk: "avatars".into(),
    key_prefix: "users/".into(),
    bytes: png_bytes.clone(),
    mime: "image/png".into(),
    original_filename: "alice.png".into(),
    uploaded_by_id: Some(user.id),
    metadata: serde_json::json!({"alt": "alice headshot"}),
}).await?;

// CDN-aware URL (falls back to backend URL when no CDN configured):
let url = manager.url(&m).expect("url");

// Time-limited download link for private files:
let dl = manager.presigned_get(&m, Duration::from_secs(3600)).await;
```

#### Direct browser uploads (no proxying through your server)

Two-step flow: server issues a presigned PUT URL, browser uploads to S3 directly, server confirms. Big files don't tie up handler bandwidth; you keep server-side gating on size/MIME via the pre-creation step.

```rust
// 1. Server: issue a presigned upload ticket.
let ticket = manager.begin_upload(UploadIntent {
    disk: "avatars".into(),
    key_prefix: "uploads/".into(),
    mime: "image/png".into(),
    original_filename: "selfie.png".into(),
    size_bytes: 12_345,
    uploaded_by_id: Some(user.id),
    ttl: Duration::from_secs(300),
}).await?;
// ticket.media_id    -> the Pending Media row id
// ticket.upload_url  -> hand to the browser
// ticket.expires_at  -> show client a deadline

// 2. Browser:
//    fetch(ticket.upload_url, {
//        method: 'PUT',
//        headers: { 'Content-Type': 'image/png' },
//        body: file
//    })

// 3. Server: confirm the object landed; flips Pending → Ready
//    (or → Failed if the browser abandoned).
let m = manager.finalize_upload(ticket.media_id).await?;
assert!(m.is_ready());
```

#### Lifecycle + cleanup

```rust
// Soft delete: marks deleted_at = NOW() but preserves the storage object.
manager.delete(&m).await?;

// Hard purge: removes both the row AND the storage object. Typical
// "clean up after soft delete grace period" pattern.
manager.purge(&m).await?;

// Background sweeps — wire to rustango::scheduler:
manager.purge_orphans(Duration::from_secs(7 * 86400)).await?;  // 7-day grace
manager.purge_pending(Duration::from_secs(86400)).await?;      // abandoned uploads
```

The `MediaStatus` enum (`Pending` / `Ready` / `Failed`) is stored as TEXT so admins can filter / order without bespoke type handling. Soft-deleted rows are excluded from `manager.get(...)` by default; `manager.get_including_deleted(...)` brings them back for restore flows.

#### Collections (folders) + tags

`MediaCollection` is a hierarchical "where the file lives" folder — one Media row belongs to at most one collection; collections nest via `parent_id`. `MediaTag` is a flat M2M label — one Media has any number of tags. Both are first-class Postgres tables, so the auto-admin lists/filters/searches them with no extra wiring.

```rust
use rustango::media::ensure_all_tables;

// Bootstrap rustango_media + rustango_media_collections +
// rustango_media_tags + rustango_media_tag_links in one call.
ensure_all_tables(&pool).await?;

// Folders (hierarchical).
let products = manager.create_collection("Products", "products", None, "").await?;
let cid = match products.id { rustango::sql::Auto::Set(v) => v, _ => unreachable!() };
let launch = manager.create_collection("2026 Launch", "2026-launch", Some(cid), "").await?;

// Drop a file into a folder at save-time.
let m = manager.save_bytes(SaveOpts {
    disk: "avatars".into(),
    key_prefix: "products".into(),
    bytes: png,
    mime: "image/png".into(),
    original_filename: "hero.png".into(),
    uploaded_by_id: Some(user.id),
    collection_id: launch.id.into(),     // folder
    metadata: serde_json::json!({}),
}).await?;

// Move it later.
let mid = match m.id { rustango::sql::Auto::Set(v) => v, _ => unreachable!() };
manager.move_to_collection(mid, Some(cid)).await?;

// Walk the folder path.
let path = manager.collection_path(cid).await?;          // "products"

// List media in a folder, optionally recursive.
let in_folder = manager.list_in_collection(cid, true).await?;

// Tags (M2M, free-form labels).
manager.tag(mid, &["featured", "approved", "homepage-hero"]).await?;
manager.untag(mid, "homepage-hero").await?;

// Replace the entire tag set:
manager.set_tags(mid, &["featured", "draft"]).await?;

// Find media by tag, paginated.
let featured = manager.list_with_tag("featured", 50, 0).await?;

// Top tags by usage:
let popular = manager.popular_tags(10).await?;           // Vec<(MediaTag, i64)>
```

Deleting a collection orphans its Media (sets `collection_id = NULL`) — the rows + storage objects survive, just lose their folder. Deleting a tag cascades the junction rows away (tags are cheap to recreate).

#### REST router

The `media_router` exposes the manager surface as JSON endpoints — drop it under any prefix you like:

```rust
use rustango::media::router::media_router;

let app = axum::Router::new()
    .nest("/media", media_router(manager));
```

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/uploads/begin` | Issue a presigned PUT ticket for browser upload |
| `POST` | `/uploads/{id}/finalize` | Confirm storage object landed → flips `pending → ready` |
| `GET` | `/media/{id}` | Single Media row with `url`, `presigned_url`, `tags` |
| `DELETE` | `/media/{id}` | Soft-delete (storage preserved) |
| `POST` | `/media/{id}/move` | Move to another collection: `{collection_id?}` |
| `POST` | `/media/{id}/tags` | Replace tag set: `{slugs: [...]}` |
| `DELETE` | `/media/{id}/tags/{slug}` | Remove a single tag |
| `POST` | `/collections` | Create folder: `{name, slug, parent_id?, description?}` |
| `GET` | `/collections` | List all (non-deleted) folders |
| `GET` | `/collections/{id}` | Single folder |
| `DELETE` | `/collections/{id}` | Soft-delete (Media inside orphaned, NOT deleted) |
| `GET` | `/collections/{id}/contents` | Media in folder. `?recursive=1` includes sub-folders |
| `POST` | `/tags` | Create / upsert tag: `{slug}` |
| `GET` | `/tags` | All tags |
| `GET` | `/tags/popular` | Top tags by use count. `?limit=N` |
| `GET` | `/tags/{slug}/media` | Media carrying the tag. `?limit=N&offset=N` |

`MediaError` implements `IntoResponse` so unknown disks return `400`, storage transport errors `502`, and "not found" errors `404`.

Behind the `media` feature flag (default-on, implies `storage` + `postgres`); `media_router` additionally needs the `admin` feature for axum.

### Scheduled tasks

```rust
use rustango::scheduler::Scheduler;
use std::time::Duration;

let s = Scheduler::new();

s.every("cleanup_sessions", Duration::from_secs(300), || async {
    cleanup_expired().await.ok();
});

s.every("rotate_logs", Duration::from_secs(86_400), || async {
    rotate().await.ok();
});

let handle = s.start();   // each task runs in its own tokio task with panic isolation
// ... app runs ...
handle.shutdown().await;
```

---

## Signals

```rust
use rustango::signals::{connect_post_save, send_post_save, PostSaveContext};

// Register at startup:
connect_post_save::<Post, _, _>(|post, ctx| async move {
    if ctx.created {
        tracing::info!("New post #{}", post.id.get().copied().unwrap_or(0));
    }
});

// Fire after save (until macro auto-fires it for you):
post.save_on(&pool).await?;
send_post_save(&post, PostSaveContext { created: true }).await;
```

Available: `connect_pre_save`, `connect_post_save`, `connect_pre_delete`, `connect_post_delete`. Disconnect via the returned `ReceiverId`.

---

## i18n

```rust
use rustango::i18n::{Translator, Locale, negotiate_language};

// Load catalogs from disk
let t = Translator::from_directory("./locales".as_ref(), Locale::new("en"))?;

// Or build manually
let t = Translator::new(Locale::new("en"))
    .add_locale(Locale::new("en"), HashMap::from([
        ("welcome".into(), "Welcome, {name}!".into()),
    ]))
    .add_locale(Locale::new("fr"), HashMap::from([
        ("welcome".into(), "Bienvenue, {name} !".into()),
    ]));

// Translate
let s = t.translate("fr-FR", "welcome", &[("name", "Alice")]);
// → "Bienvenue, Alice !"  (fr-FR falls back to fr)

// Pick from Accept-Language
let lang = negotiate_language(
    "fr-FR,fr;q=0.9,en;q=0.8",
    &t.locales(),
);
```

---

## Testing

### Test client

```rust
use rustango::test_client::TestClient;

#[tokio::test]
async fn create_post_returns_201() {
    let app = build_app().await;
    let client = TestClient::new(app);

    let response = client.post("/api/posts")
        .header("authorization", "Bearer eyJ...")
        .json(&serde_json::json!({"title": "Hi"}))
        .send().await;

    assert_eq!(response.status, 201);
    let post: serde_json::Value = response.json();
    assert_eq!(post["title"], "Hi");
}
```

### Fixtures

```rust
use rustango::fixtures::{Fixture, load_all};

let users = Fixture::new("users").from_file("fixtures/users.json")?;
let posts = Fixture::new("posts").from_file("fixtures/posts.json")?;

load_all(&[
    ("rustango_users", &users),         // load parents first
    ("posts", &posts),
], &pool).await?;
```

`fixtures/users.json`:

```json
[
    {"username": "alice", "email": "a@x.com"},
    {"username": "bob",   "email": "b@x.com"}
]
```

---

## Feature flags

The default features cover everything most apps need. Trim them when shipping a slim binary:

```toml
# Default — everything except tenancy + cache-redis
rustango = "0.29"

# Multi-tenant
rustango = { version = "0.29", features = ["tenancy"] }

# With Redis cache
rustango = { version = "0.29", features = ["cache-redis"] }

# Bare ORM only (no admin, no forms, no email, no storage)
rustango = { version = "0.29", default-features = false, features = ["postgres"] }
```

| Feature | What it adds | On by default? |
|---|---|---|
| `postgres` | sqlx + Postgres driver | yes |
| `admin` | `rustango::admin` HTTP layer (axum, Tera) | yes |
| `config` | layered TOML config + env overrides | yes |
| `forms` | `rustango::forms` parsers + ModelForm | yes |
| `serializer` | `#[derive(Serializer)]` | yes |
| `cache` | `Cache` trait + InMemoryCache + NullCache | yes |
| `cache-redis` | + RedisCache | no |
| `signals` | pre/post save/delete dispatcher | yes |
| `email` | `Mailer` trait + console/in-memory/null | yes |
| `storage` | `Storage` trait + Local/InMemory | yes |
| `scheduler` | in-process cron-shape scheduler | yes |
| `secrets` | `Secrets` trait + Env/InMemory | yes |
| `totp` | RFC 6238 2FA | yes |
| `webhook` | inbound HMAC signature verification | yes |
| `api_keys` | `{prefix}.{secret}` argon2 keys | yes |
| `passwords` | argon2 hash + strength check | yes |
| `signed_url` | HMAC-SHA256 signed URLs | yes |
| `tenancy` | multi-tenancy + operator console + permissions | no |
| `csrf` | CSRF middleware (depends on `forms`) | implied by admin |
| `template_views` | Django-shape CBVs (`ListView`, `DetailView`, …) | yes |

---

## Contributing — git hooks

Since v0.26 the rustango repo ships in-repo git hooks that catch
formatting + obvious regressions before they hit CI. One-line setup
per clone:

```bash
bin/install-hooks.sh
```

This sets `git config core.hooksPath .githooks`, after which:

- **pre-commit** (fast, blocking) — `cargo fmt --check` on staged
  Rust files, secret-shape scan (`.env`/`*.pem` files, AWS / GitHub
  / Stripe / Slack token prefixes anywhere in the diff), debris
  check (`dbg!`, `todo!()`, `unimplemented!()` in `src/`/`tests/`).
- **pre-push** (slower, blocking) — `cargo check --workspace
  --all-features`, scoped clippy on the rustango lib, lib tests.

Per-step env-var overrides documented at the top of each script.
Optional tools the hooks pick up when installed:
`cargo install typos-cli` (then `PRECOMMIT_TYPOS=1`),
`cargo install cargo-deny` (then `PREPUSH_DENY=1`).
`git commit --no-verify` / `git push --no-verify` bypass everything
when needed.

---

## Production checklist

Run before deploy:

```bash
cargo run -- check --deploy
```

Audits the env + the loaded `Settings`:
- ✅ DEBUG-style env (`RUSTANGO_ENV` is `prod` or `production`)
- ✅ `RUSTANGO_SESSION_SECRET` set and ≥ 32 bytes (no `change-me` placeholder)
- ✅ `DATABASE_URL` set, not pointing at localhost in prod
- ✅ `RUSTANGO_APEX_DOMAIN` set to a non-`localhost` value (tenancy projects)
- ✅ `RUSTANGO_BIND` not loopback-only
- ✅ Pending migrations applied
- ✅ Models registered in inventory
- ✅ `[security] headers_preset` is not `"dev"` / `"none"` in prod tier
- ✅ `[security] hsts_max_age_secs` is not `0` in prod tier
- ✅ `[auth] argon2_memory_kib` ≥ 19456 (OWASP 2024 floor)
- ✅ `[auth.jwt] access_ttl_secs` ≤ 3600 (use the refresh flow for longer sessions)
- ✅ `[server] bind` is not loopback in prod tier

Then verify your stack has:

| Layer | Required | Tool in rustango |
|---|---|---|
| HTTPS termination | yes | (reverse proxy — nginx / cloudflare / aws ALB) |
| Security headers | yes | `SecurityHeadersLayer::strict()` |
| Rate limiting | yes | `RateLimitLayer::per_ip(...)` |
| Access logging | yes | `AccessLogLayer::default()` (PII-redacted by default) |
| Health endpoints | yes | `health::health_router(pool)` → `/health`, `/ready` |
| Request IDs | recommended | `RequestIdLayer::default()` |
| CORS allowlist | if you have a JS frontend | `CorsLayer::new().allow_origins(...)` |
| ETag caching | optional | `EtagLayer::default()` |
| Backups | yes | external — `pg_dump` |

---

## Comparison

| | rustango | Django | Laravel | Rocket | Cot |
|---|:-:|:-:|:-:|:-:|:-:|
| ORM | ✅ | ✅ | ✅ | ❌ | ✅ |
| Auto-migrations | ✅ | ✅ | ✅ | ❌ | ✅ |
| Auto-admin | ✅ | ✅ | ⚠️ Filament | ❌ | ✅ |
| Multi-tenancy | ✅ | ⚠️ ext | ⚠️ ext | ❌ | ❌ |
| JWT lifecycle (refresh + blacklist + custom claims) | ✅ | ⚠️ ext | ⚠️ Sanctum/Passport | ❌ | ❌ |
| TOTP / 2FA | ✅ | ⚠️ ext | ✅ Fortify | ❌ | ❌ |
| Signals | ✅ | ✅ | ✅ Events | ❌ | ❌ |
| Cache backends | ✅ | ✅ | ✅ | ❌ | ⚠️ optional |
| Email backends | ✅ | ✅ | ✅ | ❌ | ❌ |
| File storage | ✅ | ⚠️ ext | ✅ Flysystem | ❌ | ❌ |
| Scheduled tasks | ✅ | ⚠️ Celery beat | ✅ | ❌ | ❌ |
| Security headers | ✅ | ✅ | ⚠️ middleware | ✅ Shield | ❌ |
| Test client | ✅ | ✅ | ✅ | ✅ Client | ✅ |
| Project scaffolder | ✅ `cargo rustango new` | ✅ `startproject` | ✅ Laravel installer | ❌ | ✅ `cot new` |
| File generators | ✅ `make:*` | ⚠️ ext | ✅ artisan | ❌ | ❌ |

✅ shipped · ⚠️ partial / via extension · ❌ not shipped

---

## Documentation

- **API docs**: <https://docs.rs/rustango>
- **Tutorial**: see `docs/getting-started.md`
- **CHANGELOG**: see `CHANGELOG.md`
- **Source**: <https://github.com/cot-rs/rustango>

---

## License

MIT OR Apache-2.0
