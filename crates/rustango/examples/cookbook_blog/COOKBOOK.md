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

---

## Chapter 1 — Project shape & manage commands

### 1.1 `cargo rustango startproject` / `manage startapp`

**What**: Scaffolder that emits the canonical Django-shape project layout.

**When**: Brand-new project, or adding a new sub-app to an existing one.

**API**: [`cargo-rustango`](../../../cargo-rustango/src/main.rs) for `startproject`; [`manage::startapp`](../../src/manage/scaffold.rs) for `startapp`.

**Recipe**: this very project was scaffolded by hand to match the layout `cargo rustango new --template tenant` produces. v0.16's unified `Cli::new()` dispatcher means there is no `src/bin/manage.rs` and no second binary — `cargo run` is `runserver`, `cargo run -- <verb>` is everything else.

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

**Recipe**: `cargo run -- create-operator admin --password letmein` then `cargo run -- create-user acme alice --password hunter2 --superuser`. (Single-tenant `createsuperuser` not yet wired.)

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_create_operator_verb`

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

### 1.8 Settings layering (`config/default.toml` → `config/{env}.toml` → env)

**What**: Three-tier config loader. Later layers shadow earlier; env vars take final precedence with `RUSTANGO__SECTION__KEY` syntax.

**When**: Per-environment differences (dev/test/prod) without code changes.

**API**: [`config::Settings::load`](../../src/config/mod.rs)

**Recipe** ([src/settings.rs](src/settings.rs)):

```rust
pub fn load() -> rustango::config::Settings {
    rustango::config::Settings::load("config", &std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "default".into()))
}
```

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

*(Slice 2)*

## Chapter 3 — ORM

*(Slice 3)*

## Chapter 4 — Migrations

*(Slice 2)*

## Chapter 5 — Multi-tenancy

*(Slice 4)*

## Chapter 6 — Auth + permissions

*(Slice 5)*

## Chapter 7 — Forms + serializer

*(Slice 5)*

## Chapter 8 — Admin

*(Slice 6)*

## Chapter 9 — ViewSets / DRF / OpenAPI

*(Slice 6)*

## Chapter 10 — Templates + static

*(Slice 6)*

## Chapter 11 — Async / IO / extensions

*(Slice 7)*

## Chapter 12 — Bi-dialect + cross-cutting

*(Slice 3 + Slice 8)*

---

## Gaps surfaced while writing this cookbook

*(populated as we discover them per chapter)*
