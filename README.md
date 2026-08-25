<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/ujeenet/rustango/main/docs/rustango_dark.png">
    <img src="https://raw.githubusercontent.com/ujeenet/rustango/main/docs/rustango_light.png" alt="Rustango — the Rust framework with Django spirit" width="640">
  </picture>
</p>

# Rustango

**A Django-shaped, batteries-included web framework for Rust.**

Rustango gives you the productivity of Django or Laravel with the speed and type-safety of Rust: a tri-dialect ORM, auto-migrations, an auto-generated admin, multi-tenancy, first-class auth, and every standard middleware — all shipped, all opt-out via cargo features, and all working on **Postgres, MySQL, and SQLite** out of the box.

📚 **Docs:** [rustango.com](https://rustango.com) · [in-repo guides](docs/) · [API reference](https://docs.rs/rustango)
🍳 **Cookbook:** [`cookbook_blog/COOKBOOK.md`](crates/rustango/examples/cookbook_blog/COOKBOOK.md) — a runnable, test-backed recipe for every feature below.

---

## Install

```toml
[dependencies]
# Postgres (default)
rustango = "0.52"

# SQLite — file-backed or in-memory
rustango = { version = "0.52", default-features = false, features = ["sqlite", "tenancy", "admin", "manage"] }

# MySQL 8+
rustango = { version = "0.52", default-features = false, features = ["mysql", "tenancy", "admin", "manage"] }
```

Every capability is a cargo feature you can turn off. Renaming the dep works too — `#[derive(Model)]` resolves the crate root via `proc-macro-crate`, so `orm = { package = "rustango", version = "0.52" }` needs no extra wiring.

## An app on SQLite in 30 lines

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
DATABASE_URL='sqlite:./var/app.db?mode=rwc' cargo run --features sqlite,runserver
```

The **same code** boots on Postgres with `DATABASE_URL=postgres://…` or MySQL with `DATABASE_URL=mysql://…` — no changes. Every SQLite connection turns on sensible defaults automatically (`PRAGMA foreign_keys = ON`, `journal_mode = WAL` for file-backed DBs, `busy_timeout = 5s`).

## Why Rustango

- **One ORM, three backends.** Models, queries, migrations, relations, and aggregates emit correct SQL for Postgres, MySQL 8+, and SQLite from the same code.
- **Batteries included.** Auth (sessions + JWT + OAuth2/OIDC + HMAC + API keys + TOTP), an auto-admin, multi-tenancy, caching, background jobs, email, file storage, signals, i18n, an MCP server, and OpenAPI — not add-ons, in the box.
- **Django ergonomics.** A project scaffolder (`cargo rustango new`), `make:*` generators, `manage` CLI, `#[derive(Model)]` / `#[derive(ViewSet)]` / `#[derive(Serializer)]`, and admin config blocks that feel familiar coming from Django, DRF, or Laravel.
- **Opt-out, not opt-in.** Everything is a cargo feature. A JSON-only API binary compiles out the admin, templates, and tenancy entirely.

---

## Table of contents

- [Quick start](#quick-start)
- [The ORM](#the-orm)
- [Migrations](#migrations)
- [Auto-admin](#auto-admin)
- [APIs — ViewSets, Serializers, JWT, OpenAPI](#apis--viewsets-serializers-jwt-openapi)
- [HTML views & forms](#html-views--forms)
- [Multi-tenancy](#multi-tenancy)
- [Authentication & permissions](#authentication--permissions)
- [Security middleware](#security-middleware)
- [Caching, email, storage, jobs](#caching-email-storage-jobs)
- [Signals, i18n, MCP](#signals-i18n-mcp)
- [The `manage` CLI](#the-manage-cli)
- [Configuration](#configuration)
- [Testing](#testing)
- [Comparison](#comparison)
- [Documentation](#documentation)

---

## Quick start

```bash
cargo install cargo-rustango

cargo rustango new myblog                   # default: ORM + admin
cargo rustango new myapi --template api      # JSON-only, no admin
cargo rustango new shop --template tenant    # multi-tenancy + operator console
```

```bash
cd myblog
cp .env.example .env                         # edit DATABASE_URL
docker compose up -d                         # starts Postgres
cargo run -- migrate                         # generate + apply migrations
cargo run                                    # http://localhost:8080
```

For autoreload during development: `cargo watch -x run` (or [`bacon`](https://github.com/dtolnay/bacon) `run`).

Add an app and a model:

```bash
cargo run -- startapp blog                   # scaffolds src/blog/ with a starter model + admin block + smoke test
```

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
cargo run -- makemigrations                  # generates migration JSON from the model diff
cargo run -- migrate                         # applies it
cargo run -- make:viewset PostViewSet --model Post
cargo run -- make:serializer PostSerializer --model Post
```

Full walkthrough: [getting started](docs/getting-started.md) · [scaffolding](docs/scaffolding.md).

---

## The ORM

`#[derive(Model)]` registers a struct in a global inventory and emits typed query, save, and `FromRow` code. The query builder is Django-shape (`.filter()`, `.exclude()`, `.order_by()`, `.annotate()`, `.select_related()`, `.prefetch_related()`), and the **same code runs on all three backends** through the `Pool` enum.

```rust
// Filter, order, paginate
let recent = Post::objects()
    .filter("published_at", Op::Lt, Utc::now())
    .exclude("status", Op::Eq, "draft")
    .order_by(&[("published_at", true)])   // true = DESC
    .limit(20)
    .fetch_pool(&pool).await?;

// Aggregate (scalar): .values(&[]) → one row, no GROUP BY
let stats = Post::objects()
    .values(&[])
    .annotate("total", AggregateExpr::Count(None))
    .annotate("avg_views", AggregateExpr::Avg("view_count"))
    .compile()?;
```

Supported: every field type (ints, floats, `String`, `bool`, `DateTime`/`Date`, `Uuid`, `Json`, `Decimal`, plus PG-only `Array`/`Range`/`HStore`/`Vector`/`Geometry`), nullable `Option<T>`, `Auto<T>` primary keys, `ForeignKey<T>` / one-to-one / many-to-many, generic FKs + composite-key FKs (ContentTypes), soft-delete, `unique_together` / `index_together`, container-level default scopes, subquery/`EXISTS` filters, bulk insert/update, transactions, and raw SQL escape hatches. `EXPLAIN` works on any queryset.

📖 [ORM guide](docs/orm.md) · [models](docs/models.md) · [runnable ORM recipes](crates/rustango/examples/cookbook_blog/COOKBOOK.md)

## Migrations

`makemigrations` diffs your models against the last migration snapshot and emits JSON operations; `migrate` applies pending ones and can `unapply` to roll back. Schema changes (create/alter/drop tables, columns, indexes, constraints, composite FKs) are auto-detected; data migrations are hand-authored with `sql` + `reverse_sql`. `embed_migrations!("migrations")` bakes them into the binary.

```bash
cargo run -- makemigrations
cargo run -- migrate
cargo run -- migrate --unapply <name>
```

📖 [Adopt an existing schema](docs/manage.md) with `manage inspectdb` — it emits `#[derive(Model)]` source for every table.

## Auto-admin

An `admin(...)` block on a model gives you a full CRUD admin — `list_display`, `list_filter`, `search_fields`, `date_hierarchy`, `fieldsets`, ordering, pagination, and bulk actions — with zero hand-written views:

<p align="center">
  <img src="https://raw.githubusercontent.com/ujeenet/rustango/main/docs/img/admin.png" alt="The auto-admin: a Posts list with filter facets, search, bulk actions, and pagination — all from one admin(...) block" width="760">
</p>

Also included: a token-driven **theme system** with dark mode and per-tenant branding (logo/colors via the pluggable `Storage` trait — S3/R2/B2/MinIO/local), inline child editing (`register_admin_inline!`), a per-write **audit trail** with JSON diffs, a users/roles/permissions RBAC surface, a self-serve change-password page, and session invalidation on password rotation.

📖 [Admin guide](docs/admin.md)

## APIs — ViewSets, Serializers, JWT, OpenAPI

`#[derive(ViewSet)]` gives you full REST CRUD — list (page or cursor pagination), retrieve, create (incl. DRF-style bulk create), update, partial update, destroy (soft when the model opts in) — with per-action permission gates:

```rust
#[derive(ViewSet)]
#[viewset(
    model = Post,
    fields = "id, title, body, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering = "-published_at",
    page_size = 20,
    permissions(list = "post.view", create = "post.add", update = "post.change", destroy = "post.delete"),
)]
pub struct PostViewSet;

let app = Router::new().merge(PostViewSet::router("/api/posts", pool.clone()));
```

`#[derive(Serializer)]` is a DRF-shape JSON façade (read-only / write-only / renamed / computed `method` fields, per-field `validate`, nested FK serialization, and `many` collections). JWT ships a full lifecycle (issue with custom claims, verify without a DB hit, refresh, re-check permissions, revoke/blacklist). OpenAPI 3.1 auto-derives from your serializers + viewsets, and responses follow JSON:API + RFC 7807 Problem Details. The HTTP `QUERY` method (RFC 10008) is supported for body-carrying reads.

📖 [ViewSets](docs/viewsets.md) · [serializers](docs/serializers.md) · [JWT](docs/auth-jwt-api.md) · [OpenAPI](docs/openapi.md) · [QUERY method](docs/query-method.md)

## HTML views & forms

Django-shape class-based views (`ListView`, `DetailView`, `CreateView`, `UpdateView`, `DeleteView`) render Tera templates with pagination, filters, bulk actions, FK-display, and business-validation hooks. `ModelForm`-style forms parse and validate against a model (auto-skipping DB-populated fields), aggregate per-field errors, and emit an insert query. CSRF auto-mounts for form-driven views.

📖 [HTML views](docs/html-views.md)

## Multi-tenancy

`Cli::tenancy().run()` boots an operator console, a per-tenant admin, and host-based request dispatch on **any backend**. Tenants resolve from subdomain, header, path, or port via a resolver chain; each `Org` picks its isolation strategy:

| Mode | Backends | What it does | When |
|---|---|---|---|
| **`database`** (default) | PG, MySQL, SQLite | A dedicated database (or SQLite file) per tenant, one cached pool each. | Enterprise B2B, compliance, sharding — anything on MySQL/SQLite. |
| **`schema`** | **Postgres only** | All tenants share one DB, one per PG schema, one shared pool with `SET search_path` per request. | High-N SaaS on PG (500+ small tenants) where connection counts bite. |

Database-mode is the default and works identically everywhere; schema-mode is a Postgres-only pool optimization. Set `schema` on MySQL/SQLite and the framework returns a clear error pointing you back to database-mode.

📖 Runnable walkthrough: [cookbook Ch. 5 — Multi-tenancy](crates/rustango/examples/cookbook_blog/COOKBOOK.md#chapter-5--multi-tenancy)

## Authentication & permissions

Pluggable auth backends (model / API-key / JWT — first to recognize the credential wins), argon2id password hashing, typed permission helpers (codename-based, superuser bypass), sessions, TOTP/2FA, API keys, and signed URLs (magic links / time-bounded file downloads).

📖 [passwords](docs/auth-passwords.md) · [sessions](docs/auth-sessions.md) · [backends](docs/auth-backends.md) · [API keys](docs/auth-api-keys.md) · [decorators](docs/auth-decorators.md) · [flows](docs/auth-flows.md)

## Security middleware

One hardened middleware chain: request IDs, access logging, rate limiting (in-process or distributed via cache), CORS presets, security-header presets + custom/staged CSP, CSP report endpoint, IP allow/block, CSRF, and per-account lockout. `manage check --deploy` runs an automated pre-ship audit.

📖 [Security guide](docs/security.md) · [middleware catalog](docs/middleware.md)

## Caching, email, storage, jobs

- **Caching** — in-memory / Redis backends, `get_or_set` memoization, and per-view response caching (`CachePageLayer`). [caching](docs/caching.md)
- **Email** — a renderer + `Mailable` + job-backed delivery pipeline with pluggable backends. [email](docs/email.md)
- **Storage & media** — pluggable `Storage` (S3/R2/B2/MinIO/local), `Media` rows, presigned uploads, collections, and tags. [files](docs/files.md)
- **Background jobs** — in-memory or DB queue (`FOR UPDATE SKIP LOCKED` on PG/MySQL 8+, transaction-bounded `UPDATE … RETURNING` on SQLite) plus scheduled tasks. [jobs](docs/jobs.md)

## Signals, i18n, MCP

- **Signals** — model lifecycle (`pre_save` / `post_save` / `pre_delete` / `post_delete`) and request lifecycle (`request_started` / `request_finished` / `got_request_exception`).
- **i18n** — `Translator` is Django's `gettext` family in Rust: per-locale catalogs, base-language fallback, `{name}` placeholders, CLDR pluralization, plus a DB-override layer and live admin translation editor. [i18n](docs/i18n.md)
- **MCP server** — the `mcp` feature turns an app into a Model Context Protocol server: AI agents authenticate as tenant-scoped identities and call your framework-exposed tools over JSON-RPC 2.0. [mcp](docs/mcp.md)

## The `manage` CLI

`cargo run -- <cmd>` — Django's `manage.py` in Rust. Migrations (`makemigrations` / `migrate` / `inspectdb`), scaffolders (`startapp` / `make:viewset` / `make:serializer`), system commands (`check` / `check --deploy` / `shell`), and — with the `tenancy` feature — operator/tenant/superuser provisioning and recovery verbs.

📖 [manage reference](docs/manage.md)

## Configuration

Layered config: a `<env>_settings.toml` pipeline (base → env → local → environment variables), typed sections, compile-time feature reflection, and a deploy audit. Everything has a sensible default; override only what you need.

## Testing

A `TestClient` drives the router as a tower service (no socket), a `RequestFactory` builds requests, and fixtures seed data. The cookbook's ~150 tests run against live Postgres, MySQL, and SQLite.

📖 [testing](docs/testing.md)

---

## Comparison

| | Rustango | Django | Laravel | Rocket | Cot |
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
| Project scaffolder | ✅ `cargo rustango new` | ✅ `startproject` | ✅ installer | ❌ | ✅ `cot new` |
| File generators | ✅ `make:*` | ⚠️ ext | ✅ artisan | ❌ | ❌ |

✅ shipped · ⚠️ partial / via extension · ❌ not shipped

---

## Documentation

- **Guides & tutorials**: <https://rustango.com>
- **Runnable cookbook**: [`cookbook_blog/COOKBOOK.md`](crates/rustango/examples/cookbook_blog/COOKBOOK.md) — a test-backed recipe for every feature, on all three backends.
- **In-repo guides** ([`docs/`](docs/)): [getting started](docs/getting-started.md) · [models](docs/models.md) · [ORM](docs/orm.md) · [migrations & CLI](docs/manage.md) · [admin](docs/admin.md) · [viewsets](docs/viewsets.md) · [serializers](docs/serializers.md) · [auth](docs/auth-flows.md) · [security](docs/security.md) · [middleware](docs/middleware.md) · [caching](docs/caching.md) · [email](docs/email.md) · [files](docs/files.md) · [jobs](docs/jobs.md) · [i18n](docs/i18n.md) · [MCP](docs/mcp.md) · [testing](docs/testing.md) · [glossary](docs/glossary.md)
- **API reference**: <https://docs.rs/rustango>
- **Changelog**: [`CHANGELOG.md`](CHANGELOG.md)

## Contributing

Git hooks (fmt + secret/debris scan on pre-commit; `cargo check --all-features` + clippy + lib tests on pre-push) install with `bin/install-hooks.sh` (sets `git config core.hooksPath .githooks`). Please run `cargo fmt` and `cargo clippy --all-targets` before opening a PR.

## License

MIT OR Apache-2.0
