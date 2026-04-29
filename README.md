# rustango

**A Django-shaped ORM for Rust, with a registry-driven CRUD admin.**

`#[derive(Model)]` on a struct gets you:

- A typed `QuerySet<T>` — `User::objects().where_(User::name.eq("alice")).fetch(&pool).await?`
- Migrations from your code — `migrate::apply_all(&pool).await?` (and v0.2 schema-snapshot diffs)
- A working CRUD HTTP admin — `axum::serve(listener, admin::router(pool)).await?`

Zero per-model wiring. The admin walks an `inventory` registry that every
derive populates, so a brand-new struct gets a browseable list/detail/edit/
delete page the moment it compiles.

```rust
use rustango::{Auto, Model, admin, migrate};
use rustango::core::Column as _;
use rustango::sql::{Fetcher, sqlx::PgPool};

#[derive(Model, Debug, Clone)]
#[rustango(table = "user", display = "username")]
struct User {
    #[rustango(primary_key)]
    id: Auto<i64>,                  // BIGSERIAL — sequence assigns the PK
    #[rustango(max_length = 32)]
    username: String,
    #[rustango(min = 0, max = 150)]
    age: i32,
    is_active: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    migrate::apply_all(&pool).await?;

    // Insert with a server-assigned id — `id` is populated in place.
    let mut alice = User {
        id: Auto::default(),
        username: "alice".into(),
        age: 30,
        is_active: true,
    };
    alice.insert(&pool).await?;
    println!("alice got id {}", alice.id.get().unwrap());

    // Typed query.
    let actives: Vec<User> = User::objects()
        .where_(User::is_active.eq(true))
        .where_(User::age.gte(18))
        .fetch(&pool).await?;

    // Build the admin and serve it.
    let app = admin::Builder::new(pool)
        .read_only(["audit_log"])
        .build();
    let app = admin::protect_with_basic_auth(app, "admin", "secret");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

## What's distinct

- **Zero-wiring auto-admin.** Every `#[derive(Model)]` is admin-visible the moment it compiles — no per-model registration. (Cot needs four opt-in steps; Loco has [no admin yet](https://github.com/loco-rs/loco/issues/819).)
- **Migrations as data.** JSON files with full per-step `SchemaSnapshot`, mechanically diff-able, language-agnostic. `embed_migrations!` validates the chain at **compile time** — a broken `prev` reference fails `cargo build`, not runtime.
- **`manage migrate --dry-run`** prints every DDL/DML the next migrate would run, no side effects.
- **`DataOp` interleaved with `SchemaChange`** in one migration — Django's "add nullable + backfill + set NOT NULL" recipe lives in one file.
- **Multi-tenancy without a `DATABASES` dict** (v0.5) — adding a tenant is `INSERT INTO rustango_orgs (...)`, no restart, no config edit, no redeploy. See below.
- **Per-tenant auth + `is_superuser` gating out of the box** (v0.6) — superusers get read/write admin, non-superusers see read-only views, anon traffic redirects to `/__login`. Same HMAC-SHA256 session for the operator console at the apex.
- Postgres-only by design. Single dev hobby project; for multi-DB ORMs use [Diesel](https://diesel.rs) or [SeaORM](https://www.sea-ql.org).

## Multi-tenancy (v0.5, opt-in via `rustango-tenancy`)

> **The Django footgun fix.** Django's `DATABASES` dict in `settings.py` requires every database to be declared at boot — adding a tenant means edit + restart + redeploy. `rustango-tenancy` makes tenants first-class **rows in a Postgres table**, resolved per-request from an `OrgResolver` chain.

```toml
# Cargo.toml
[dependencies]
rustango = "0.6"
rustango-tenancy = "0.6"   # opt-in
```

> The fastest path: drop a 5-line `src/bin/manage.rs` and run
> `cargo run --bin manage -- run-server`. That ships the recommended
> wiring (operator console at the apex, tenant admin at every
> subdomain, host-based dispatch, signal-driven shutdown) with one
> command. The snippet below is the manual form for projects that
> need a custom router shape.

```rust
use std::sync::Arc;
use rustango::sql::sqlx::PgPool;
use rustango_tenancy::{
    admin::TenantAdminBuilder,
    operator_console::{self, SessionSecret},
    ChainResolver, TenantPools,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry_url = std::env::var("DATABASE_URL")?;
    let registry = PgPool::connect(&registry_url).await?;

    // Lazy tenant pool registry. Schema-mode tenants share `registry`;
    // database-mode tenants get a dedicated pool, lazy-built and cached.
    let pools = Arc::new(TenantPools::new(registry.clone()));

    // Subdomain-first by design: `acme.app.example.com` → tenant `acme`.
    // X-Org header is the API fallback. Path-prefix is opt-in (drop it
    // in if you can't get wildcard DNS / TLS).
    let resolver = ChainResolver::standard("app.example.com");

    // One signing key for both consoles (distinct cookie names keep
    // them isolated). Reads RUSTANGO_SESSION_SECRET (base64, ≥32 bytes)
    // or falls back to a random key with a tracing::warn.
    let secret = SessionSecret::from_env_or_random();

    // Tenant admin under each subdomain — with per-tenant auth.
    let tenant = TenantAdminBuilder::new(pools.clone(), registry_url, resolver)
        .read_only(["audit_log"])
        .with_session(secret.clone())
        .build();

    // Operator console at the apex: form-based login, sidebar layout,
    // read-only operator + org views.
    let operator = operator_console::router(registry.clone(), secret);

    // Host-based dispatch: apex (no subdomain) → operator, anything
    // else → tenant admin. This sidesteps axum's nest-vs-absolute-href
    // gotcha and matches the production routing story.
    let app = axum::Router::new().fallback_service(tower::service_fn({
        let operator = operator.clone();
        let tenants = tenant.clone();
        let apex = "app.example.com".to_owned();
        move |req: axum::http::Request<axum::body::Body>| {
            let mut operator = operator.clone();
            let mut tenants = tenants.clone();
            let apex = apex.clone();
            async move {
                use tower::ServiceExt as _;
                let host = req
                    .headers()
                    .get(axum::http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(':').next().unwrap_or(s).to_owned())
                    .unwrap_or_default();
                let resp = if host == apex {
                    operator.as_service().oneshot(req).await
                } else {
                    tenants.as_service().oneshot(req).await
                };
                resp.map_err(|e| -> std::convert::Infallible {
                    panic!("axum router service is Infallible: {e}")
                })
            }
        }
    }));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

**Provision tenants from the `manage` runner**:

> The snippets below assume **your own project** has a `src/bin/manage.rs`
> (5-line wrapper around `rustango_tenancy::manage::run` — see
> `crates/rustango-tenancy/examples/tenancy_manage.rs`). To exercise the
> CLI **in this repo without writing your own binary**, swap
> `cargo run --bin manage --` for
> `cargo run --example tenancy_manage -p rustango-tenancy --`.

```sh
# First-run bootstrap. Writes packaged migrations into ./migrations
# and applies the registry-scoped one (creates rustango_orgs +
# rustango_operators with UNIQUE constraints). Idempotent — re-runs
# leave existing files alone.
cargo run --bin manage -- init-tenancy
cargo run --bin manage -- migrate

# Hand out a slug + storage mode. Without --no-migrate, the packaged
# tenant bootstrap runs against the new schema so rustango_users
# exists from the start.
RUSTANGO_APEX_DOMAIN=app.example.com cargo run --bin manage -- \
    create-tenant acme --mode schema

# Or a fully isolated database tenant pointing at a separate DB.
cargo run --bin manage -- \
    create-tenant globex --mode database \
    --database-url postgres://...:5432/globex_data

cargo run --bin manage -- list-tenants
cargo run --bin manage -- migrate-tenants
cargo run --bin manage -- create-operator admin --password ...
cargo run --bin manage -- create-user acme alice --password ... --superuser

# Soft-delete a tenant (data preserved, sets active=false).
cargo run --bin manage -- drop-tenant acme --confirm acme

# Hard-delete (UNRECOVERABLE — drops the schema CASCADE).
# Database-mode tenants additionally require --purge-database.
cargo run --bin manage -- purge-tenant acme --confirm acme

# Boot the operator console + tenant admin (Ctrl-C to stop):
cargo run --bin manage -- run-server
```

**Interactive prompts.** Required positional args and `--password` flags
are prompted for when stdin is a TTY. So `cargo run … -- create-tenant`
asks `Tenant slug:`, `create-operator alice` asks for password with
hidden echo, and `drop-tenant acme` asks you to retype `acme` to confirm
(skip `--confirm`).

**`.env` auto-load.** The example binary loads `./.env` (or any ancestor)
on startup — drop your `DATABASE_URL`, `RUSTANGO_APEX_DOMAIN`, and
`RUSTANGO_SESSION_SECRET` there once and you don't need to re-export them
each session.

**Two storage modes per tenant, choose per row:**

- `schema` — tenant data lives in a Postgres schema in the registry DB. Cheap (one connection budget for all tenants); good for many small tenants.
- `database` — tenant data lives in a fully separate Postgres database (different host is fine). Strong isolation, per-tenant pool. The connection URL goes through a pluggable `SecretsResolver` — `env://VAR_NAME` works out of the box; HashiCorp Vault / AWS Secrets Manager / Azure Key Vault adapters land as separate crates implementing the trait.

**Hard wall between identity domains.** Operators (`rustango_operators`, registry) and per-tenant Users (`rustango_users` in the tenant's storage) are strictly separate. Argon2id-hashed passwords. An operator's credentials never authenticate against a tenant; a tenant superuser's credentials never reach `/operator`. Browser cookie isolation by subdomain plus the in-code wall gives defense in depth.

**Per-tenant auth + `is_superuser` gating** (v0.6). Opt in via
`TenantAdminBuilder::with_session(SessionSecret)` and the tenant admin
gets HMAC-SHA256 session cookies + a login form at `/__login`. Anon
traffic to a tenant URL → `303 → /__login?next=<path>`. Authenticated
**superusers** see full read/write admin; **non-superusers** see a
read-only admin (list/detail render, mutating routes 403,
write-buttons hidden). The cookie's `slug` field binds it to one
tenant — a cookie minted at `acme.localhost` can't authenticate at
`globex.localhost`. The same `SessionSecret` signs both consoles
(different cookie names + payload shapes keep them isolated), so a
single `RUSTANGO_SESSION_SECRET` env var covers everything.

```rust
use rustango_tenancy::{
    admin::TenantAdminBuilder,
    operator_console::SessionSecret,
};

let secret = SessionSecret::from_env_or_random();
let tenant = TenantAdminBuilder::new(pools, registry_url, resolver)
    .with_session(secret)            // ← anon redirect, non-su read-only
    .build();
```

The operator UI at the apex ships its own login form via
`rustango_tenancy::operator_console::router(registry, secret)` —
form-based, sidebar layout, read-only `/operators` and `/orgs` views,
embedded `rustango.png` brand asset. Mutations stay on the CLI so
side-effects (CREATE SCHEMA, migrations) happen atomically.

**Subdomain-first routing**, with `*.localhost` for dev:

```
acme.app.example.com/admin/...   → ACME's admin (production)
acme.localhost:8080/admin/...    → ACME's admin (dev — Chrome/Firefox/Safari
                                    resolve `*.localhost` to 127.0.0.1
                                    automatically; no /etc/hosts edits)
app.example.com/operator/...     → operator UI (no subdomain → no tenant)
```

## Try the demo

```sh
docker compose up -d                       # local Postgres
cargo run --example admin_demo
```

Open <http://127.0.0.1:8080/>, login `admin` / `secret`. Walk through:

- `User` → list view with search box and per-field filters
- click into a row → detail with edit / delete (delete confirms)
- `Post` rows render `author` as a clickable link to the user (FK display)
- `AuditLog` is mounted read-only — visible, no edit / delete buttons,
  direct POST returns 403

If `cargo` complains *"rustc 1.86.0 is not supported"* a Homebrew `rust`
install is shadowing rustup's 1.88. Run `PATH="$HOME/.cargo/bin:$PATH"
cargo run --example admin_demo` instead.

## What's in the box

| crate              | role                                                                       |
| ------------------ | -------------------------------------------------------------------------- |
| `rustango`         | facade — re-exports the others; what users depend on                        |
| `rustango-core`    | schema, query IR, value types, validation, error types — dep-light, no async |
| `rustango-macros`  | `#[derive(Model)]` — emits Model impl, `objects()`, typed columns, FromRow, insert/delete |
| `rustango-query`   | `QuerySet<T>` with `filter` / `where_` / `update` / `compile` / `limit` / `offset` |
| `rustango-sql`     | Postgres dialect writer (SELECT/INSERT/UPDATE/DELETE/COUNT, LEFT JOIN), executor traits |
| `rustango-migrate` | `apply_all` for fresh DBs, `SchemaSnapshot` + diff for evolving schemas    |
| `rustango-admin`   | axum Router that walks the registry → list / detail / CRUD forms / search / pagination / basic auth / Tera templates |

## Field attributes

```rust
#[derive(Model)]
#[rustango(table = "user", display = "username")]   // override table; pick which field FK references render
struct User {
    #[rustango(primary_key)]                         id: i64,
    #[rustango(column = "user_name")]                name: String,
    #[rustango(max_length = 32)]                     username: String,    // → VARCHAR(32) + form maxlength
    #[rustango(min = 0, max = 150)]                  age: i32,            // → CHECK + form min/max
    #[rustango(fk = "user", on = "id")]              author_id: i64,      // → FOREIGN KEY + admin link rendering
    is_active: bool,
                                                     email: Option<String>, // nullable
}
```

## Query API

Two filter shapes, same builder. Mix freely; multiple predicates `AND`
together (no `.or(...)` yet).

**Typed — `where_`.** The derive emits a `Column` per field; typos and
wrong types fail at compile time.

```rust
use rustango::core::Column as _;   // brings .eq / .gt / .like / .is_in into scope

let actives: Vec<User> = User::objects()
    .where_(User::is_active.eq(true))      // = $1
    .where_(User::age.gte(18))             // >= $1
    .where_(User::age.lt(65))              // <  $1
    .where_(User::name.like("ali%"))       // LIKE $1
    .where_(User::id.is_in([1, 2, 3]))     // IN ($1, $2, $3)
    .limit(20)
    .fetch(&pool).await?;
```

**String-keyed — `filter` / `eq`.** Field name is a string, validated at
`compile`-time against the schema. Use this when the column is dynamic
(e.g. admin search params); prefer `where_` everywhere else.

```rust
use rustango::core::Op;

let actives: Vec<User> = User::objects()
    .eq("is_active", true)                 // sugar for filter(_, Op::Eq, _)
    .filter("age", Op::Gte, 18_i32)
    .filter("name", Op::Like, "ali%")
    .fetch(&pool).await?;
```

Wrong field → `QueryError::UnknownField`; wrong value type →
`QueryError::TypeMismatch`. Available `Op`s: `Eq`, `Ne`, `Lt`, `Lte`,
`Gt`, `Gte`, `Like`, `In`.

**Bulk update / delete / count / per-instance.**

```rust
// Bulk update.
let n = User::objects()
    .where_(User::age.lt(13))
    .update()
    .set_typed(User::is_active.set(false))
    .execute(&pool).await?;

// Bulk delete.
let n = User::objects()
    .eq("id", 99_i64)
    .delete(&pool).await?;

// Count.
use rustango::sql::Counter;
let n = User::objects().eq("is_active", true).count(&pool).await?;

// Per-instance.
User { id: 1, username: "alice".into(), age: 30, is_active: true }
    .insert(&pool).await?;
user.delete(&pool).await?;
```

## Admin

`admin::router(pool)` returns a stock `axum::Router`. `admin::Builder` is
the configurable form:

```rust
let app = admin::Builder::new(pool)
    .show_only(["user", "post", "audit_log"])  // allowlist; missing → 404
    .read_only(["audit_log"])                  // visible, no edit/create/delete
    .build();
let app = admin::protect_with_basic_auth(app, "admin", "secret");
let app = axum::Router::new().nest("/admin", app);  // mount under any prefix
```

The list view supports `?q=foo` (case-insensitive substring across String
fields with a `max_length`), `?<field>=<value>` per-field filters, and
`?page=N` pagination at 50 rows per page. Pager links carry search and
filter state forward.

HTML is rendered through Tera templates bundled at compile time
(`crates/rustango-admin/templates/`). User-supplied strings are
auto-escaped.

## Migrations

The high-level UX is Django-shaped: drop a tiny `src/bin/manage.rs`
into your project, then run `cargo run --bin manage -- <subcommand>`.

```rust
// src/bin/manage.rs
use rustango::sql::sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use my_app::models::*;   // pulls user models into this binary so
                             // `inventory` registers them
    let pool = PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let dir: &std::path::Path = "./migrations".as_ref();
    rustango::migrate::manage::run(&pool, dir, std::env::args().skip(1)).await?;
    Ok(())
}
```

Subcommands:

| command | what it does |
|---------|--------------|
| `makemigrations [name]`      | Diff registry against latest snapshot, write next file. |
| `makemigrations --empty <name>` | Empty scaffold for hand-authored data migrations. |
| `migrate`                    | Apply every pending migration in order. |
| `migrate <target>`           | Forward or back to `<target>`. `zero` wipes everything. |
| `downgrade [N]`              | Step back N applied migrations (default 1). |
| `showmigrations`             | List applied / pending migrations. |

Migration files live in `./migrations/*.json`, committed to git.
Each file carries the full schema snapshot at that point plus an
ordered list of operations — schema changes (`AddColumn`,
`CreateTable`, …) and data ops (raw SQL with optional `reverse_sql`)
interleaved, so the canonical "add nullable → backfill → set NOT NULL"
recipe lives in one file.

The library API is also available directly without the dispatcher:

```rust
use rustango::migrate;

// First-run bootstrap from the registry (no migration files involved).
migrate::apply_all(&pool).await?;

// File-driven flow.
let dir: &std::path::Path = "./migrations".as_ref();
migrate::make_migrations(dir, None)?;          // diff + write next file
migrate::migrate(&pool, dir).await?;           // apply all pending
migrate::migrate_to(&pool, dir, "0003_initial_data").await?;  // jump to a target
migrate::downgrade(&pool, dir, 1).await?;      // roll back one step
migrate::unapply(&pool, dir, "0042_oops").await?;  // roll back a specific one
```

What's covered: new/dropped tables, new/dropped columns, FK
constraints, the `default` attribute (so `ADD COLUMN ... NOT NULL
DEFAULT '…'` works), forward + reverse, persistent tracking via
`__rustango_migrations__`, and per-migration `atomic: false` opt-out
for things like `CREATE INDEX CONCURRENTLY`.

For deployments where shipping a `migrations/` folder alongside the
binary is awkward (Docker images, single-binary distribution),
`embed_migrations!` bakes the JSON files in at compile time:

```rust
const EMBEDDED: &[(&str, &str)] = rustango::embed_migrations!("./migrations");

// At runtime — same shape as `migrate(pool, dir)`, no filesystem access.
rustango::migrate::migrate_embedded(&pool, EMBEDDED).await?;
```

What's deferred: type / constraint changes and renames (need
explicit `Rename`/`AlterField` operations à la Django — snapshot
diffs can't tell a rename from a drop+add).

## Status

This is a hobbyist project. The shape is novel for Rust ORMs (registry-
driven admin, Django-style API), the test count is high (~250 unit +
live integration including the v0.6 multi-tenancy + auth paths), and
the demo works in a real browser. v0.6 closed the multi-tenancy gaps
(form login on both consoles, packaged bootstrap migrations,
scope-aware `migrate`, `purge-tenant`, `is_superuser` gating). It is
still **not** production-ready: no SQLite/MySQL, no streaming queries,
no benchmarks against the mature alternatives, and session revocation
is whole-secret rotation only. For real workloads today, use
[Diesel](https://diesel.rs) or [SeaORM](https://www.sea-ql.org/SeaORM/).

If you want a Django-shaped admin in Rust, this is the only thing that
exists.

## License

MIT OR Apache-2.0.
