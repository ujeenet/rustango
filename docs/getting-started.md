# Getting Started: build a blog with Rustango

This walkthrough takes you from an empty directory to a deployed blog: posts, an admin UI, a JSON API, JWT authentication, and tests. End to end. If you've used Django, Laravel, or Rails, most steps will feel familiar; we point out the parallels as we go.

> **Time:** ~45 minutes for the full tour, ~10 minutes if you just want to see it running.
>
> **Runnable version:** every step below is mirrored in a tested, compilable example at [`crates/rustango/examples/getting_started_blog`](../crates/rustango/examples/getting_started_blog). If a step ever looks off, diff against it.

---

## What you need first

| Tool | Why | Install |
|---|---|---|
| Rust 1.88+ | Compiler | <https://rustup.rs> |
| Docker | Local Postgres | <https://docker.com> |
| `psql` (optional) | Inspect DB | `brew install libpq` / `apt install postgresql-client` |

Check the versions you have installed:

```bash
rustc --version    # should print 1.88+
docker --version   # any recent version
```

---

## Step 1: Install the scaffolder

The scaffolder generates project and app skeletons for you, like `django-admin` or `rails new`.

```bash
cargo install cargo-rustango
```

This adds the `cargo rustango ...` subcommand globally. Confirm it's there:

```bash
cargo rustango --help
```

---

## Step 2: Create the project

This scaffolds a fresh project, the **Rustango** equivalent of `rails new` or `composer create-project`.

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

Here's what was generated:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── README.md                   # project-specific
├── config/                     # tiered settings (default + dev/staging/prod)
├── migrations/                 # empty — `cargo run -- makemigrations` populates
└── src/
    ├── main.rs                 # entry point: `Cli::new().api(urls::api()).run()`
    ├── models.rs               # every #[derive(Model)] lives here
    ├── views.rs                # axum request handlers
    └── urls.rs                 # `pub fn api()` route aggregator + `admin_router(pool)`
```

There is a single binary: `cargo run` boots the HTTP server, and every Django-style verb (`migrate`, `makemigrations`, `startapp`, `check`, …) flows through the same binary via `cargo run -- <verb>`. There's no separate `manage` binary.

`Cargo.toml` is the dependency manifest (like `composer.json` or a `Gemfile`). Open it and confirm `rustango` is listed under `[dependencies]`.

---

## Step 3: Set up your environment

Configuration lives in a `.env` file, just like in Django or Laravel. Copy the template:

```bash
cp .env.example .env
```

The generated `.env` is Docker-friendly out of the box. Because we'll run `cargo` on the host (not inside the dev container), change the database host from `postgres` to `localhost`:

```bash
DATABASE_URL=postgres://rustango:rustango@localhost:5432/myblog_dev
RUSTANGO_BIND=0.0.0.0:8080
RUSTANGO_APEX_DOMAIN=localhost
RUSTANGO_SESSION_SECRET=change-me-base64-encoded-32-bytes-or-more
```

The credentials, port, and database name (`myblog_dev`) already match the `docker-compose.yml` Postgres service, so you don't need to touch those.

`RUSTANGO_SESSION_SECRET` signs sessions and tokens, so don't ship the placeholder. Generate a real one and paste it in:

```bash
openssl rand -base64 32     # paste output as RUSTANGO_SESSION_SECRET value
```

---

## Step 4: Start Postgres

The project ships with a `docker-compose.yml` that runs Postgres in a container, so you don't install a database by hand. We'll run the app itself with `cargo` on the host, so start just the `postgres` service in the background (the compose file also defines an optional `rust` dev-container that would otherwise bind port 8080):

```bash
docker compose up -d postgres
```

Confirm it's running:

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Step 5: Run the built-in migrations

Migrations create your database tables, same idea as `php artisan migrate` or `rails db:migrate`. Run them once to set up the framework's own tables:

```bash
cargo run -- migrate
```

The first compile takes ~2 minutes (Rust builds everything from source). A fresh project ships no migration files yet, so you'll see `nothing to migrate (already up to date)` — `migrate` still sets up the framework's audit-log table so audited models work the moment you add them. You generate your first real migration in Step 9.

Check the migration state:

```bash
cargo run -- showmigrations
```

On a fresh project this prints `(no migrations in ./migrations)`. Once you create a model and run `makemigrations` (Step 9), each applied migration shows an `[X]` here.

---

## Step 6: First boot

Start the server to make sure everything is wired up.

```bash
cargo run
```

You'll see:

```
listening on http://0.0.0.0:8080
```

Open <http://localhost:8080> in your browser. The scaffold ships a simple root handler (`views::index`) that greets you with **Hello from rustango!** and a link to the admin — that confirms **Rustango** is running. (Projects that don't define their own `/` route get a built-in welcome page instead, via `Cli::with_welcome()`.)

Press Ctrl-C to stop.

---

## Step 7: Create an app

An "app" is a self-contained feature module, exactly like a Django app. Your blog app will hold the Post model, its routes, and its templates.

```bash
cargo run -- startapp blog
```

This writes:

```
src/blog/
├── mod.rs
├── models.rs              # a starter model named after the app (you'll replace it)
├── views.rs               # axum handlers
├── urls.rs                # blog-specific routes (pub fn api())
└── tests.rs               # in-process router + inventory smoke tests
```

`startapp` wires the new module in for you (similar to adding it to Django's `INSTALLED_APPS`): it declares `mod blog;` in `src/main.rs` and inserts a `.merge(crate::blog::urls::api())` line into the `api()` aggregator in `src/urls.rs`, so the blog's routes compose into the app automatically. No manual module registration needed.

---

## Step 8: Define a model

A model is a database table described as a Rust struct, like a Django model or an Eloquent/Active Record class. Open `src/blog/models.rs` and define your `Post`:

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(
    table = "posts",
    display = "title",
    admin(
        list_display  = "id, title, status, published_at",
        search_fields = "title, body",
        list_filter   = "status, author_id",
        ordering      = "-published_at",
    ),
    audit(track = "title, body, status"),
    index("status, published_at"),
)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                  // draft | published

    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,

    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

A few Rust things to note:

- `#[derive(Model, ...)]` is a **derive macro**: it auto-generates code for the struct, the way a class decorator or base class would in other frameworks. Deriving `Model` is what gives the struct its query methods.
- `Auto<i64>` marks a field the database fills in for you (an auto-incrementing `i64` integer), like an auto primary key.
- `Option<...>` means "this value may be absent." `Option<DateTime<Utc>>` is a timestamp that can be null, so `deleted_at` is empty until the row is soft-deleted.
- The `#[rustango(...)]` attributes configure each field (max length, defaults, indexes) and the `admin(...)` block sets up the admin UI columns and filters.

---

## Step 9: Create and apply the migration

Now turn that model into a real table. First, generate the migration from your model (like `makemigrations` in Django):

```bash
cargo run -- makemigrations
```

You'll see something like:

```
wrote ./migrations/0001_create_item_and_posts_and_rustango_admin_users_etc.json
    + CreateTable("item")
    + CreateTable("posts")
    + CreateTable("rustango_admin_users")
    + CreateTable("rustango_content_types")
    + CreateIndex { table: "posts", columns: ["status", "published_at"], ... }
```

This first migration creates your models — `posts`, plus the starter `item` model the scaffold shipped in `src/models.rs` — alongside the framework's own admin and content-type tables. Open the JSON if you like: it carries the operations plus a full schema snapshot.

Apply it to the database:

```bash
cargo run -- migrate
```

Confirm the table exists:

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Step 10: Try the ORM

Let's read and write rows from code. The ORM lets you work with database rows as Rust structs instead of raw SQL, like Django's ORM, Eloquent, or Active Record.

Temporarily edit `src/main.rs` to run a quick create-and-read test before booting the server. Replace the `Cli` body with an ad-hoc ORM smoke test (keep the scaffolder's `#[rustango::main]` and the `mod` declarations at the top of the file):

```rust
mod blog;
mod models;
mod urls;
mod views;

use crate::blog::models::Post;
use rustango::{Auto, Model};

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    // CREATE
    let mut p = Post {
        id: Auto::default(),
        title: "First post".into(),
        body: "Hello, world.".into(),
        status: "draft".into(),
        author_id: 1,
        published_at: Auto::default(),
        deleted_at: None,
    };
    p.save(&pool).await?;
    println!("created post id = {}", p.id.get().copied().unwrap());

    // READ
    let posts = Post::objects().fetch_on(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

What's happening here, in plain terms:

- `pool` is the shared database connection pool. You pass a reference to it (`&pool`) into query calls instead of opening a new connection each time.
- Database calls are asynchronous, so each one ends in `.await` — that pauses until the result comes back, then continues. The `?` after an `.await` says "if this errored, stop and return the error."
- `main` returns a `Result`, Rust's success-or-error type, which is why `?` and the closing `Ok(())` work.
- To save a row, call `.save(&pool)` on it. To read rows, build a query with `Post::objects()` and run it with `.fetch_on(&pool)` — the rough equivalent of Django's `Post.objects.all()`. (`.save(&pool)` / `.fetch_on(&pool)` take a `sqlx::PgPool`; the bare `.fetch(&pool)` variant takes a multi-backend `rustango::sql::Pool` instead — see the [ORM guide](orm.md).)

Run it:

```bash
cargo run
```

You should see your new post id and the rows read back. Restore `src/main.rs` to its scaffolded server shape once you've confirmed it works — the next step builds on that.

---

## Step 11: Turn on the auto-admin

**Rustango** ships a generated admin UI for your models, just like Django admin. The scaffolder already gave you an `admin_router(pool)` helper in `src/urls.rs` that builds the auto-admin from a pool — you just need to nest it under `/admin` and feed it into the `Cli`.

First, give the admin a title in `src/urls.rs`. The `admin_prefix` must match the path you'll nest it under in the next step (`/admin`) so the admin's own links and form actions resolve:

```rust
pub fn admin_router(pool: PgPool) -> Router {
    admin::Builder::new(pool)
        .title("Myblog Admin")
        .admin_prefix("/admin") // must match the `.nest("/admin", …)` below
        .build()
}
```

Then connect a pool in `src/main.rs` and nest the admin into the API router before handing it to the `Cli`. Keep the `mod blog;` line from Step 7 — that's what registers your `Post` model with the admin:

```rust
mod blog;
mod models;
mod urls;
mod views;

#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let api = urls::api().nest("/admin", urls::admin_router(pool));

    rustango::manage::Cli::new()
        .api(api)
        .with_health() // /health + /ready endpoints
        .run()
        .await
}
```

`Cli::new()...run()` is the same unified dispatcher the scaffolder generated — it still serves every `cargo run -- <verb>`; you've only enriched the router it serves at runserver time.

Run it:

```bash
cargo run
```

Open <http://localhost:8080/admin> (no trailing slash). You'll see the admin home with a `posts` link. Click it to see your draft post in the list, click the post to open its edit form, and save. The audit-trail tab records every write.

---

## Step 12: Build the JSON API

A ViewSet exposes a model as a REST API with list, create, retrieve, update, and delete endpoints, much like a Django REST Framework ViewSet or a Laravel API resource controller.

### 12a. Generate the ViewSet

Scaffold the file, then fill in which fields and behaviors to expose:

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Edit `src/post_view_set.rs`:

```rust
use rustango::ViewSet;
use crate::blog::models::Post;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    fields        = "id, title, body, status, author_id, published_at",
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Register the new module by adding `mod post_view_set;` alongside the other `mod` declarations at the top of `src/main.rs`.

### 12b. Mount the routes

Attach the ViewSet's routes to the app's router (the **Rustango** version of a `urls.py` or a `routes/api.php` file). The ViewSet router needs the database pool, so build it in `src/main.rs` where the pool lives and merge it into the `urls::api()` aggregator:

```rust
let api = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool));

rustango::manage::Cli::new()
    .api(api)
    .with_health()
    .run()
    .await
```

(`urls::api()` is the aggregator the scaffolder generated; `manage startapp` merges any sub-app's routes into it the same way.)

### 12c. Try the endpoints

Start the server:

```bash
cargo run
```

In another terminal, hit the API with `curl`:

```bash
curl http://localhost:8080/api/posts                                    # list
curl -X POST http://localhost:8080/api/posts \
     -H "content-type: application/json" \
     -d '{"title":"From API","body":"Yo","status":"published","author_id":1}'
curl http://localhost:8080/api/posts/1                                   # retrieve
curl "http://localhost:8080/api/posts?search=API&ordering=-id"            # search + sort
curl "http://localhost:8080/api/posts?status__ne=draft"                   # lookup operator
```

---

## Step 13: Shape the output with a Serializer

By default the ViewSet returns every model field. A Serializer lets you control the response shape: hide internal fields, rename them, or mark some read-only. It's the same role as a DRF serializer or a Laravel API resource.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Edit `src/post_serializer.rs`:

```rust
use rustango::{Auto, Serializer};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: Auto<chrono::DateTime<chrono::Utc>>,
}
```

Each serializer field's type mirrors the matching model field, so `id` and `published_at` keep their `Auto<…>` wrapper from the model (an `Auto<i64>` still serializes to a plain JSON integer). Then register the module by adding `mod post_serializer;` alongside the other `mod` declarations in `src/main.rs`.

(Auto-wiring this into the ViewSet response shape is on the roadmap. For now, use the serializer manually in custom handlers.)

---

## Step 14: Add JWT authentication

JWTs are signed tokens you hand a client after login and check on each request, a common pattern for API auth. **Rustango**'s `rustango::jwt` module issues and verifies them (HS256) and is on by default — no extra feature flag.

### 14a. Issue a token at login

Bake the user id (the token's "subject") and any custom claims, like roles, into a signed token, then hand it to the client:

```rust
use rustango::jwt::{encode, Claims};
use std::time::Duration;

// Derive the signing key from your session secret.
let secret = std::env::var("RUSTANGO_SESSION_SECRET")?.into_bytes();

let mut claims = Claims::new(user_id.to_string());   // subject = user id
claims.set("roles", vec!["editor"]);
let token = encode(&claims.ttl(Duration::from_secs(900)), &secret)?;

// Send `token` to the client (e.g. in the login response body).
```

### 14b. Verify the token on each request

Decode the token — this checks the signature and expiry — then read the claims back. If it's missing or invalid, reject the request as unauthorized:

```rust
use rustango::jwt::decode;

let claims = decode(&access_token, &secret)
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

let user_id = claims.subject().ok_or(StatusCode::UNAUTHORIZED)?;
let roles: Vec<String> = claims.get("roles").unwrap_or_default();
```

### 14c. Access + refresh lifecycle

`rustango::jwt` issues stateless single tokens. For the full pattern — short-lived **access** tokens, a long-lived **refresh** token in an HttpOnly cookie, rotation, and a JTI blacklist for revocation — enable the `tenancy` feature and use `rustango::tenancy::jwt_lifecycle::JwtLifecycle`, whose `issue_pair_with` / `verify_access` / `refresh` methods manage the pair for you.

---

## Step 15: Add security middleware

Middleware wraps every request to add cross-cutting behavior. Here you stack request IDs, access logging, rate limiting, CORS, and security headers in one chain. Each `.method(...)` adds one layer, similar to Django middleware or Laravel's middleware stack.

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::health::health_router;
use std::time::Duration;

let app = urls::api()
    .nest("/admin", urls::admin_router(pool.clone()))
    .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool.clone()))
    .merge(health_router(pool.clone()))                        // /health, /ready
    .request_id(RequestIdLayer::default())
    .access_log(AccessLogLayer::default())                      // PII-redacted
    .rate_limit(RateLimitLayer::per_ip(60, Duration::from_secs(60)))
    .cors(CorsLayer::new()
        .allow_origins(vec!["https://app.example.com"])
        .allow_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE"]))
    .security_headers(
        SecurityHeadersLayer::strict()
            .csp(CspBuilder::strict_starter().build()),
    );
```

Hand the finished `app` to the `Cli` exactly as before — `rustango::manage::Cli::new().api(app).with_welcome().run().await` — and every request now flows through the full middleware stack.

---

## Step 16: Write tests

**Rustango** includes a test client that drives your router in-process, so you can assert on real HTTP responses without starting a server, much like Django's test client or Laravel's HTTP tests. Scaffold a test file:

```bash
cargo run -- make:test PostSmoke      # generates tests/post_smoke.rs
```

The `make:*` generators take a PascalCase name; `PostSmoke` becomes the snake_case file `tests/post_smoke.rs`.

Edit `tests/post_smoke.rs`. Integration tests live in a separate crate, so they build the router under test directly from the ViewSet (the same `router(...)` call you mounted in Step 12b):

```rust
use rustango::test_client::TestClient;
use myblog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn list_posts_returns_200() {
    let client = TestClient::new(app().await);
    let response = client.get("/api/posts").send().await;
    assert_eq!(response.status, 200);
    let v = response.json_value();
    assert!(v["results"].is_array());
}

#[tokio::test]
async fn create_post_returns_the_new_object() {
    let client = TestClient::new(app().await);
    let response = client.post("/api/posts")
        .json(&json!({
            "title": "Test",
            "body":  "x",
            "status": "draft",
            "author_id": 1,
        }))
        .send().await;
    assert_eq!(response.status, 201);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
}
```

> **Heads-up:** integration tests in `tests/` can only `use myblog::…` if the crate exposes a library target. A fresh scaffold is binary-only (`src/main.rs`, no `src/lib.rs`), so add a one-line `src/lib.rs` that re-exports the modules you want to test — `pub mod models; pub mod post_view_set; pub mod urls;` — and keep the matching `mod …;` lines in `src/main.rs`. (If you'd rather not add a lib target, build the router fully inline in the test instead, the way `make:test` scaffolds its `app()`.)

Run the tests:

```bash
cargo test --test post_smoke
```

---

## Step 17: Run the system check

Before you deploy, run the built-in checker. It flags common misconfigurations (like a weak `RUSTANGO_SESSION_SECRET` or an unreachable database), similar to Django's `check --deploy`.

```bash
cargo run -- check --deploy
```

In your local dev environment you'll see something like:

```
running rustango system check (deploy mode)...
  [info]    6 models registered via inventory
  [info]    database reachable
  [info]    1 migration(s) on disk
  [info]    RUSTANGO_SESSION_SECRET length OK
  [info]    config tier resolved to `dev`
  [warning] RUSTANGO_ENV is unset — set to `prod` so config loaders pick the right tier
  [warning] DATABASE_URL points at localhost / 127.0.0.1 — verify this is intended in production
  [warning] RUSTANGO_APEX_DOMAIN is unset / `localhost` — set it for tenancy projects
```

(The exact model/migration counts depend on your project.) Those three warnings are the expected dev-environment ones. In a production setup — `RUSTANGO_ENV=prod`, a managed-database `DATABASE_URL`, an apex domain set — they clear and you'll see `all checks passed`. Fix any remaining warnings or errors before pushing to production.

---

## Step 18: Deploy to production

How you deploy depends on your platform (Fly, Railway, Kubernetes, bare ECS, and so on). The framework-side steps are the same everywhere; the `--release` flag builds an optimized binary:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export RUSTANGO_SESSION_SECRET=$(openssl rand -base64 32)

# 2. Run migrations
cargo run --release -- migrate

# 3. Audit
cargo run --release -- check --deploy

# 4. Build binary
cargo build --release

# 5. Run with a process supervisor (systemd / docker / k8s)
./target/release/myblog
```

Make sure your reverse proxy:
- Terminates HTTPS
- Forwards `X-Forwarded-For` for accurate IPs in `AccessLogLayer`
- Forwards `X-Forwarded-Host`, `X-Forwarded-Proto`
- Uses `axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())` so `ConnectInfo` is populated for rate limiting + IP filtering

---

## Where to go next

| Topic | Doc |
|---|---|
| Runnable version of this guide | [`examples/getting_started_blog`](../crates/rustango/examples/getting_started_blog) |
| Every `manage` subcommand | [`docs/manage.md`](manage.md) |
| ORM cookbook (advanced filters, aggregations, M2M, soft delete) | [`docs/orm.md`](orm.md) |
| API conventions (naming, builder patterns, feature gates) | [`docs/api-conventions.md`](api-conventions.md) |
| Security features in depth | [`docs/security.md`](security.md) |
| Django parity audit | [`docs/django-parity-audit-2026-05-21.md`](django-parity-audit-2026-05-21.md) |
| Multi-tenancy | [README — Multi-tenancy section](../README.md#multi-tenancy) |
| API docs | <https://docs.rs/rustango> |

If you hit something that doesn't work or is unclear, open an issue.
