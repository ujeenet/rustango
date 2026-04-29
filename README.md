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
- **Day-2 ORM ergonomics** (v0.7) — `model.save(&pool)` insert-or-update via `Auto<T>` PK dispatch, `ForeignKey<T>` lazy-load (`post.author.get(&pool).await?`), OR / nested-expr `where_(A.or(B.and(C)))`, and per-app migration ledger naming (`migrate::Builder::new().ledger("__myapp__")`) so two rustango apps share one DB without colliding on the bookkeeping table.
- **Foundations + the missing 30%** (v0.8) — `cargo rustango new <name> --template api|fullstack|tenant` (Cargo-installable project scaffolder), `rustango::config::Settings` (layered TOML: `default.toml` → `{env}.toml` → `RUSTANGO__*` env-vars), `#[derive(Form)]` + CSRF middleware (`rustango::forms::csrf::layer()`), and a `Dialect` seam ready for SQLite + MySQL (lighting up in v0.10).
- Postgres-only by design today; SQLite + MySQL ride in via v0.10's `Dialect` impls. For multi-DB ORMs available right now, use [Diesel](https://diesel.rs) or [SeaORM](https://www.sea-ql.org).

## Multi-tenancy (v0.5, opt-in via the `tenancy` feature)

> **The Django footgun fix.** Django's `DATABASES` dict in `settings.py` requires every database to be declared at boot — adding a tenant means edit + restart + redeploy. The `tenancy` feature makes tenants first-class **rows in a Postgres table**, resolved per-request from an `OrgResolver` chain.

```toml
# Cargo.toml
[dependencies]
# v0.7+ ships as one crate with feature flags. Default features
# include `postgres + admin + config + forms`. Add `tenancy` for
# the multi-tenant resolver / pools / per-tenant auth pieces.
rustango = { version = "0.8", features = ["tenancy"] }
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
use rustango::tenancy::{
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
> (5-line wrapper around `rustango::tenancy::manage::run` — see
> `crates/rustango/examples/tenancy_manage.rs`). To exercise the
> CLI **in this repo without writing your own binary**, swap
> `cargo run --bin manage --` for
> `cargo run --example tenancy_manage --features tenancy --`.

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
use rustango::tenancy::{
    admin::TenantAdminBuilder,
    operator_console::SessionSecret,
};

let secret = SessionSecret::from_env_or_random();
let tenant = TenantAdminBuilder::new(pools, registry_url, resolver)
    .with_session(secret)            // ← anon redirect, non-su read-only
    .build();
```

The operator UI at the apex ships its own login form via
`rustango::tenancy::operator_console::router(registry, secret)` —
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

For a CLI-only walk through the v0.7 ergonomic additions (`save()`,
`ForeignKey<T>` lazy-load, OR / nested filters, per-app migration
ledger naming):

```sh
cargo run --example v07_ergonomics_demo
```

If `cargo` complains *"rustc 1.86.0 is not supported"* a Homebrew `rust`
install is shadowing rustup's 1.88. Run `PATH="$HOME/.cargo/bin:$PATH"
cargo run --example admin_demo` instead.

## Project layout

rustango projects follow Django's `models / views / urls` shape. The
recommended structure for a downstream binary:

```text
your-app/
├── Cargo.toml
└── src/
    ├── main.rs        — boots the binary, ties everything together
    ├── models.rs      — every #[derive(Model)] lives here; populates
    │                    the `inventory` registry the admin walks
    ├── views.rs       — request handlers (Django-style "views"); take
    │                    axum extractors, return Html / Json / Redirect
    └── urls.rs        — single `pub fn router(pool) -> Router` that
                         maps paths → handlers and nests the auto-admin
```

Every `#[derive(Model)]` you put in `models.rs` shows up in the auto-
admin without any extra wiring — adding a new model = one struct,
done. Custom HTTP endpoints (a JSON API, a published-posts feed, a
custom dashboard) live in `views.rs` and get wired in `urls.rs`
alongside `rustango::admin::router(pool).nest("/admin", …)`.

The runnable reference for this shape is bundled as a multi-file
example: [`crates/rustango/examples/project_layout/`](crates/rustango/examples/project_layout/).
Spin it up with:

```sh
cargo run --example project_layout
```

Then visit <http://127.0.0.1:8082/> for the landing page; it links to
the auto-admin (`/admin`), a couple of custom JSON views, and the
healthz probe.

The framework's own modules follow the same convention internally:

* [`rustango::admin`](crates/rustango/src/admin/) splits into
  `urls.rs` (the route table) + `views.rs` (handlers) +
  `templates.rs` + `helpers.rs` + `errors.rs`.
* [`rustango::tenancy::manage`](crates/rustango/src/tenancy/manage/)
  is a directory module with one file per command group:
  `tenants.rs`, `users.rs`, `migrations.rs`, `server.rs`, plus shared
  `args.rs`.

## What's in the box

v0.7+ ships as one library crate (`rustango`) plus the proc-macro
crate that Rust requires to live separately (`rustango-macros`). The
single `rustango` crate exposes its layers as feature-gated modules:

| module                | gated by feature | role |
| --------------------- | --------------- | ---- |
| `rustango::core`      | always on       | schema, query IR, value types, validation, error types — dep-light, no async |
| `rustango::query`     | always on       | `QuerySet<T>` with `filter` / `where_` / `update` / `compile` / `limit` / `offset` |
| `rustango::sql`       | always on       | dialect-pluggable SQL writer (Postgres ships in v0.8; SQLite + MySQL in v0.10), executor traits |
| `rustango::migrate`   | always on       | `apply_all` for fresh DBs, `SchemaSnapshot` + diff, `Builder` ledger naming, `scaffold::startapp` |
| `rustango::config`    | `config` *(default)* | layered TOML loader — `config/default.toml` → `config/{env}.toml` → `RUSTANGO__*` env-var overrides |
| `rustango::forms`     | `forms` *(default)* | form-payload parsers + `#[derive(Form)]` + per-field validators — shared by admin and user routes |
| `rustango::forms::csrf` | `csrf` *(default via `admin`)* | double-submit-cookie CSRF middleware (`csrf::layer()`) for axum |
| `rustango::admin`     | `admin` *(default)* | axum Router that walks the registry → list / detail / CRUD forms / search / pagination / basic auth / Tera templates |
| `rustango::tenancy`   | `tenancy`       | multi-tenant resolver chain, `TenantPools`, scoped migrations, per-tenant auth, operator console |
| `rustango_macros::*`  | always on       | `#[derive(Model)]`, `#[derive(Form)]`, `embed_migrations!` — re-exported from the facade as `rustango::Model` / `rustango::Form` etc. |
| `cargo-rustango` (binary) | separate crate | `cargo rustango new <name>` project scaffolder — three templates (api / fullstack / tenant) |

Drop `default-features` to get the bare ORM (`core` + `query` + `sql`
+ `migrate`) without axum/Tera; opt into `tenancy` for multi-tenant
projects. The full default set is `["postgres", "admin", "config",
"forms"]` (and `csrf` is pulled transitively via `admin`).

## Field attributes

```rust
use rustango::{Auto, Model};
use rustango::sql::ForeignKey;

#[derive(Model)]
#[rustango(table = "user", display = "username")]   // override table; pick which field FK references render
struct User {
    #[rustango(primary_key)]                         id: Auto<i64>,        // → BIGSERIAL; sequence assigns the PK
    #[rustango(column = "user_name")]                name: String,
    #[rustango(max_length = 32)]                     username: String,     // → VARCHAR(32) + form maxlength
    #[rustango(min = 0, max = 150)]                  age: i32,             // → CHECK + form min/max
    is_active: bool,
                                                     email: Option<String>, // nullable
}

#[derive(Model)]
struct Post {
    #[rustango(primary_key)] id: Auto<i64>,
    title: String,
    author: ForeignKey<User>,        // → BIGINT REFERENCES "user"("id"); lazy-load via .get(&pool)
    // Legacy form is still supported when you don't want the wrapper:
    //   #[rustango(fk = "user", on = "id")] author_id: i64,
}
```

## Query API

Two filter shapes, same builder. Mix freely. Successive predicates
`AND` together at the top level; `OR` and nested expressions are
contained inside a single `.where_()` argument (v0.7).

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
`Gt`, `Gte`, `Like`, `In`, `IsNull`.

**OR / nested expressions** (v0.7). Predicates compose with `.and()` /
`.or()` into a typed `WhereExpr`. The Postgres writer is precedence-
aware: nested composites are parenthesized so SQL grouping survives.

```rust
// (name = "alice" OR name = "bob") AND age >= 18
let candidates: Vec<User> = User::objects()
    .where_(User::name.eq("alice").or(User::name.eq("bob")))
    .where_(User::age.gte(18))
    .fetch(&pool).await?;
// → WHERE ("user_name" = $1 OR "user_name" = $2) AND "age" >= $3

// Nested: (age >= 40 AND active = false) OR name = "alice"
let mixed: Vec<User> = User::objects()
    .where_(
        User::age.gte(40)
            .and(User::is_active.eq(false))
            .or(User::name.eq("alice")),
    )
    .fetch(&pool).await?;
```

A bare `User::name.eq("alice")` flows into `.where_()` via `Into<TypedExpr<User>>`,
so existing single-predicate call sites are unchanged.

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

// Per-instance — explicit insert / delete.
let mut alice = User {
    id: Auto::default(),
    username: "alice".into(),
    age: 30,
    is_active: true,
};
alice.insert(&pool).await?;          // BIGSERIAL fills in `id`
user.delete(&pool).await?;

// Per-instance — `save()` (v0.7) dispatches on `Auto<T>` PK.
//   Auto::Unset → INSERT … RETURNING (populates the PK)
//   Auto::Set(_) → UPDATE … SET <every-non-pk-col> WHERE pk = ?
let mut bob = User {
    id: Auto::default(),
    username: "bob".into(),
    age: 41,
    is_active: true,
};
bob.save(&pool).await?;              // INSERT (PK was Unset)
bob.age = 42;
bob.save(&pool).await?;              // UPDATE (PK is now Set)

// Lazy-load a parent through a `ForeignKey<T>` field.
let mut post = Post::objects().eq("id", 1_i64).fetch(&pool).await?
    .into_iter().next().unwrap();
let author: &User = post.author.get(&pool).await?;   // resolves once, cached
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
(`crates/rustango/src/admin/templates/`). User-supplied strings are
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

**Per-app ledger naming** (v0.7). Two rustango apps in the same
Postgres database used to collide on the shared
`__rustango_migrations__` table. `migrate::Builder` carries an
opt-in ledger override:

```rust
use rustango::migrate;

let mine = migrate::Builder::new().ledger("__myapp_migrations__");
mine.migrate(&pool, dir).await?;          // applies pending migrations into the custom ledger
mine.applied_set(&pool).await?;
mine.downgrade(&pool, dir, 1).await?;
```

`Builder::default()` keeps the legacy default — every existing call
site (the `manage` CLI, `migrate::migrate(&pool, dir)`, tenancy's
`migrate_registry` / `migrate_tenants`) thunks through it without
edits. The ledger name is validated at config time
(`[A-Za-z_][A-Za-z0-9_]*`, ≤ 63 bytes); a quote-injection attempt
panics there, not deep in a SQL call.

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
driven admin, Django-style API), the test count is high (~270 unit +
live integration including the v0.6 multi-tenancy + auth paths and the
v0.7 ORM ergonomics), and the demo works in a real browser. v0.6
closed the multi-tenancy gaps (form login on both consoles, packaged
bootstrap migrations, scope-aware `migrate`, `purge-tenant`,
`is_superuser` gating). v0.7 closed the day-2 ORM gaps (`save()`
insert-or-update, `ForeignKey<T>` lazy-load, OR / nested filters,
per-app migration ledger naming). It is still **not**
production-ready: no SQLite/MySQL, no streaming queries, no
benchmarks against the mature alternatives, and session revocation is
whole-secret rotation only. For real workloads today, use
[Diesel](https://diesel.rs) or [SeaORM](https://www.sea-ql.org/SeaORM/).

If you want a Django-shaped admin in Rust, this is the only thing that
exists.

## License

MIT OR Apache-2.0.
