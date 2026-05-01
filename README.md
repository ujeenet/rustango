# rustango

**A Django-shaped web framework for Rust — ORM, migrations, auto-admin, multi-tenancy, audit log.**

One `#[derive(Model)]` and you get a typed queryset, schema-diffing migrations, and a full CRUD admin — no per-model wiring required.

```toml
[dependencies]
rustango = { version = "0.14", features = ["tenancy"] }
```

---

## Feature overview

| Capability | What you write | What you get |
|---|---|---|
| ORM | `#[derive(Model)]` | `Model::objects().where_(...).fetch(&pool)` |
| Migrations | `makemigrations` CLI verb | JSON diff files, atomic apply, per-app ledger |
| Auto-admin | `admin::Builder::new(pool).build()` | Full CRUD UI at `/__admin/` — list, search, facets, create, edit, delete |
| Multi-tenancy | `TenantPools` + resolver | Schema-mode or database-mode tenants, per-tenant admin + auth |
| Field mixins | `#[rustango(soft_delete)]` | `auto_now_add`, `auto_now`, `soft_delete`, `auto_uuid` on `Auto<T>` fields |
| Audit log | `#[rustango(audit(track="..."))]` | Per-field before/after diff on every write — feed at `/__audit` |
| Server builder | `Builder::from_env()` | One-chain boot: migrate, seed, serve |
| Manage CLI | `cargo run --bin manage` | `create-tenant`, `create-operator`, `audit-cleanup`, `makemigrations`, … |

---

## Quick start

### 1 — Define a model

```rust
use rustango::{Auto, Model};
use rustango::sql::ForeignKey;
use chrono::{DateTime, Utc};

#[derive(Model, Debug, Clone)]
#[rustango(
    table = "post",
    display = "title",
    admin(
        list_display  = "title, author, published_at",
        search_fields = "title, body",
        list_filter   = "author",
        ordering      = "-published_at",
        actions       = "delete_selected, restore_selected",
        fieldsets     = "Content: title, body | Meta: author, published_at",
    ),
    audit(track = "title, body"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    #[rustango(max_length = 8000)]
    pub body: String,

    pub author: ForeignKey<Author>,

    #[rustango(auto_now_add)]           // set to NOW() on INSERT, never updated
    pub created_at: Auto<DateTime<Utc>>,

    #[rustango(auto_now)]               // updated to NOW() on every save
    pub updated_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]            // stamp instead of hard-DELETE
    pub deleted_at: Option<DateTime<Utc>>,
}
```

### 2 — ORM queries

```rust
use rustango::core::Column as _;
use rustango::sql::Fetcher;

// Fetch all live posts newest-first
let posts = Post::objects()
    .where_(Post::deleted_at.is_null())
    .order_by(Post::created_at, true)   // true = DESC
    .fetch(&pool)
    .await?;

// Lazy-load a FK
let author = post.author.get(&pool).await?;

// Create
let mut p = Post { id: Auto::default(), title: "Hello".into(), .. };
p.save_on(&mut conn).await?;

// Soft-delete / restore
p.soft_delete_on(&mut conn).await?;
p.restore_on(&mut conn).await?;
```

### 3 — Migrations

```bash
# Diff models → JSON migration file
cargo run --bin manage -- makemigrations

# Apply pending migrations
cargo run --bin manage -- migrate
```

Migration files are JSON committed alongside your code — no live database needed to inspect history.

### 4 — Auto-admin

```rust
use rustango::admin;

let app = admin::Builder::new(pool)
    .title("My App Admin")
    .show_only(["post", "author", "tag"])
    .read_only(["audit_log"])
    .build();

let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
axum::serve(listener, app).await?;
```

The admin lives at `/__admin/`. Every `#[rustango(admin(...))]` model gets:

- **List view** — paginated table, column ordering, search box
- **Facet rail** — `list_filter` fields become sidebar filters; FK fields render as `<select>` dropdowns showing the target's display name
- **Create / edit forms** — fieldsets, readonly fields, Auto-PK hidden on create
- **Soft-delete** — the delete button stamps `deleted_at` instead of hard-deleting when the model has `#[rustango(soft_delete)]`
- **Bulk actions** — `delete_selected` and `restore_selected` built-in; register custom handlers via `Builder::register_action`
- **Audit trail** — per-row history panel on the detail page; cross-model activity feed at `/__audit` with facets by operation, source, entity

---

## Multi-tenancy

Each tenant can live in its own Postgres schema (schema-mode) or a fully separate database (database-mode). Tenant routing is via subdomain by default (`acme.myapp.com`).

```rust
use rustango::server::Builder;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Builder::from_env().await?
        .admin_title("My SaaS Admin")
        .admin_show_only(["post", "author"])
        .migrate(".")
        .await?
        .api(urls::router())
        .seed_with(|pools, _registry, registry_url| async move {
            seed(pools, &registry_url).await
        })
        .await?
        .serve("0.0.0.0:8080")
        .await
}
```

`Builder::from_env()` reads:

| Env var | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | — | Registry Postgres (stores orgs, operators, users) |
| `RUSTANGO_APEX_DOMAIN` | `localhost` | Subdomain root — tenants resolve as `<slug>.<apex>` |
| `RUSTANGO_BIND` | `0.0.0.0:8080` | Bind address |
| `RUSTANGO_SESSION_SECRET` | random (warns) | Base64-encoded 32-byte HMAC key for session cookies |

Generate a valid secret: `openssl rand -base64 32`

### Provisioning tenants

```rust
use rustango::tenancy::manage::api::*;

// All calls are idempotent — safe in seed_with
let org = create_tenant_if_missing(
    &pools, &registry_url, migrations_dir, "acme",
    CreateTenantOpts {
        mode: StorageMode::Schema,
        display_name: Some("ACME Corp".into()),
        ..Default::default()
    },
).await?;

create_operator_if_missing(&pools, "admin", "letmein").await?;
create_user_if_missing(&pools, "acme", "alice", "hunter2", /* superuser */ true).await?;
```

### Manage CLI

```bash
# Migrations
cargo run --bin manage -- makemigrations
cargo run --bin manage -- migrate
cargo run --bin manage -- showmigrations

# Tenant / user management
cargo run --bin manage -- create-tenant acme --display-name "ACME Corp"
cargo run --bin manage -- create-operator admin --password letmein
cargo run --bin manage -- create-user acme alice --password hunter2 --superuser
cargo run --bin manage -- list-tenants

# Audit-log retention
cargo run --bin manage -- audit-cleanup --days 90
cargo run --bin manage -- audit-cleanup --keep-last 50 --tenant acme
```

---

## Admin customization reference

### `#[rustango(admin(...))]`

```rust
#[rustango(admin(
    list_display    = "field1, field2, field3",   // columns on list view
    search_fields   = "field1, field2",            // fields searched by ?q=
    list_filter     = "fk_field, bool_field",      // right-rail facets
    ordering        = "field, -other_field",        // default sort (- = DESC)
    list_per_page   = 50,                           // rows per page
    readonly_fields = "created_at, slug",           // shown but not editable
    fieldsets       = "Group A: f1, f2 | Group B: f3", // form layout
    actions         = "delete_selected, my_action", // bulk actions
))]
```

### Field mixins (`Auto<T>` fields)

| Attribute | Field type | Behaviour |
|---|---|---|
| `#[rustango(auto_now_add)]` | `Auto<DateTime<Utc>>` | Set to `NOW()` on INSERT; skip on UPDATE |
| `#[rustango(auto_now)]` | `Auto<DateTime<Utc>>` | Set to `NOW()` on every INSERT and UPDATE |
| `#[rustango(soft_delete)]` | `Option<DateTime<Utc>>` | `soft_delete_on()` stamps; `restore_on()` clears; admin routes to UPDATE |
| `#[rustango(auto_uuid)]` | `Auto<Uuid>` | Server-generated UUIDv4 on INSERT |

### Audit log

```rust
#[rustango(audit(track = "title, body, status"))]
pub struct Post { ... }
```

Every write through the ORM emits a row to `rustango_audit_log` with a typed before/after JSON diff. The audit source (system / user / custom) is set per-request via a task-local:

```rust
rustango::audit::with_source(
    AuditSource::User { id: user_id.to_string() },
    handler_future,
).await
```

Retention:

```rust
// Delete entries older than 90 days
rustango::audit::cleanup_older_than(&pool, 90).await?;

// Keep only the 50 most recent entries per row
rustango::audit::cleanup_keep_last_n(&pool, 50).await?;
```

---

## Feature flags

```toml
# Single-tenant (ORM + migrations + admin + audit)
rustango = { version = "0.14" }

# Multi-tenant (adds TenantPools, Builder, operator console, per-tenant auth)
rustango = { version = "0.14", features = ["tenancy"] }
```

---

## Framework comparison

| | rustango | Django |
|---|---|---|
| ORM | `Model::objects().where_(...)` | `Model.objects.filter(...)` |
| Migrations | JSON files, `makemigrations` CLI | Python files, `makemigrations` |
| Admin | `/__admin/` auto-CRUD | `/admin/` auto-CRUD |
| Multi-tenancy | Schema or DB per tenant, built-in | Third-party (`django-tenants`) |
| Audit log | Built-in, per-field diff | Third-party (`django-simple-history`) |
| Field mixins | `auto_now_add`, `soft_delete`, `auto_uuid`, … | `auto_now_add`, `auto_now`, … |
| Background tasks | BYO (`sqlxmq`, `apalis`) | Celery |

---

## Development

```bash
# Install watch runner
cargo install cargo-watch

# Watch mode — recompiles and restarts on every file save
cargo watch -x run

# Run the test suite (requires Postgres at DATABASE_URL)
cargo test --workspace --features tenancy -- --test-threads=1

# Build docs
cargo doc --no-deps --features tenancy --open
```

---

## Licence

MIT OR Apache-2.0
