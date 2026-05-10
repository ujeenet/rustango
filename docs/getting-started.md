# Getting Started — building a blog with rustango

This walkthrough takes you from an empty directory to a deployed blog with posts, an admin UI, a JSON API, JWT authentication, and tests. End-to-end.

> **Time investment:** ~45 minutes for the full tour, ~10 minutes if you just want to see it running.

---

## Prerequisites

| Tool | Why | Install |
|---|---|---|
| Rust 1.88+ | Compiler | <https://rustup.rs> |
| Docker | Local Postgres | <https://docker.com> |
| `psql` (optional) | Inspect DB | `brew install libpq` / `apt install postgresql-client` |

Verify:

```bash
rustc --version    # should print 1.88+
docker --version   # any recent version
```

---

## Step 1 — Install the scaffolder

```bash
cargo install cargo-rustango
```

This installs the `cargo rustango ...` subcommand globally. Verify:

```bash
cargo rustango --help
```

---

## Step 2 — Create the project

```bash
cd ~/projects                                 # wherever you keep code
cargo rustango new myblog                     # default = fullstack template
cd myblog
```

What you got:

```
myblog/
├── Cargo.toml                  # rustango + axum + sqlx + tokio
├── .env.example                # template for DATABASE_URL etc.
├── .gitignore
├── docker-compose.yml          # Postgres in a container
├── README.md                   # project-specific
├── migrations/                 # empty — `manage makemigrations` populates
└── src/
    ├── main.rs                 # HTTP entry point
    ├── models.rs               # placeholder
    ├── views.rs                # placeholder
    ├── urls.rs                 # router composition
    └── bin/
        └── manage.rs           # CLI dispatcher
```

Open `Cargo.toml`. Confirm `rustango = "0.20"` is there.

---

## Step 3 — Configure environment

```bash
cp .env.example .env
```

Open `.env` in your editor:

```bash
DATABASE_URL=postgres://postgres:postgres@localhost:5433/myblog
SECRET_KEY=please-replace-with-32-or-more-random-bytes
RUSTANGO_ENV=local
RUST_LOG=info,sqlx=warn
```

Generate a real secret:

```bash
openssl rand -base64 32     # paste output as SECRET_KEY value
```

---

## Step 4 — Start Postgres

```bash
docker compose up -d
```

Verify it's running:

```bash
docker compose ps
psql "$DATABASE_URL" -c "SELECT version();"   # should print Postgres version
```

---

## Step 5 — Run the bootstrap migrations

```bash
cargo run -- migrate
```

First compile takes ~2 minutes. After: a few empty migration JSON files get applied (the framework's audit/permissions tables).

Verify:

```bash
cargo run -- showmigrations
```

You should see `[X]` next to each entry.

---

## Step 6 — First boot

```bash
cargo run
```

Output:

```
listening on http://0.0.0.0:8080
```

Open <http://localhost:8080> in your browser. You'll see the **welcome page** — green check mark, framework version, next-steps list. This confirms rustango is wired up.

Press Ctrl-C to stop.

---

## Step 7 — Scaffold an app

A "blog" app holds the Post model + its routes + its templates.

```bash
cargo run -- startapp blog
```

What got written:

```
src/blog/
├── mod.rs
├── models.rs              # empty placeholder
├── views.rs               # axum handlers
└── urls.rs                # blog-specific routes
```

Make rustango aware of the new module — open `src/lib.rs`:

```rust
pub mod urls;
pub mod blog;             // ← add this line
```

---

## Step 8 — Define a model

Open `src/blog/models.rs`:

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

---

## Step 9 — Generate + apply the migration

```bash
cargo run -- makemigrations
```

Output:

```
wrote ./migrations/0004_initial.json
```

Look at it — it's a JSON file with a `CreateTable` op + the full schema snapshot.

Apply it:

```bash
cargo run -- migrate
```

Verify the table exists:

```bash
psql "$DATABASE_URL" -c "\d posts"
```

---

## Step 10 — Try the ORM in a script

Edit `src/main.rs` to add a one-shot test before serving:

```rust
use myblog::blog::models::Post;
use rustango::{Auto, Model};
use rustango::sql::Fetcher as _;

#[tokio::main]
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
    p.save_on(&pool).await?;
    println!("created post id = {}", p.id.get().copied().unwrap());

    // READ
    let posts = Post::objects().fetch(&pool).await?;
    for post in &posts {
        println!("- {}", post.title);
    }

    Ok(())
}
```

Run it:

```bash
cargo run
```

You should see your post id + the readback. Comment out the test code when done.

---

## Step 11 — Wire up the auto-admin

Restore `src/main.rs` to a server-shape. The simplest is the welcome-page version that ships with the scaffolder. To add the admin, change `src/main.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let admin = rustango::admin::Builder::new(pool.clone())
        .title("Myblog Admin")
        .build();

    let app = axum::Router::new()
        .merge(rustango::welcome::welcome_router())
        .nest("/admin", admin);

    println!("listening on http://0.0.0.0:8080");
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

Run it:

```bash
cargo run
```

Open <http://localhost:8080/admin/>. You should see the admin home with a `posts` link. Click it — see your draft post in the list. Click it — edit form. Save. Audit-trail tab shows your write.

---

## Step 12 — Build the JSON API

### 12a. Generate the viewset

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

Add `pub mod post_view_set;` to `src/lib.rs`.

### 12b. Mount it

Edit `src/urls.rs`:

```rust
use axum::Router;
use rustango::sql::sqlx::PgPool;

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .merge(crate::post_view_set::PostViewSet::router("/api/posts", pool))
}
```

Update `src/main.rs` to include it:

```rust
let app = axum::Router::new()
    .merge(rustango::welcome::welcome_router())
    .nest("/admin", admin)
    .merge(myblog::urls::router(pool.clone()));
```

### 12c. Try it

```bash
cargo run
```

In another terminal:

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

## Step 13 — Add a Serializer

The default ViewSet response wraps every model field. Use a Serializer to control shape, hide internal fields, or rename for the API:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Edit `src/post_serializer.rs`:

```rust
use rustango::Serializer;
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: i64,
    pub title: String,

    #[serializer(source = "body")]                      // rename in API
    pub content: String,

    #[serializer(read_only)]                            // include in GET, ignore in POST/PUT
    pub published_at: chrono::DateTime<chrono::Utc>,
}
```

(Wiring this into the ViewSet response shape is on the v0.21 roadmap; for now, use the serializer manually in custom handlers.)

---

## Step 14 — Add JWT authentication

### 14a. Generate access + refresh tokens on login

```rust
use rustango::tenancy::jwt_lifecycle::JwtLifecycle;
use serde_json::json;

let jwt = JwtLifecycle::new(secret_key.clone()).with_access_ttl(900);

let pair = jwt.issue_pair_with(user_id, json!({
    "roles": ["editor"],
}).as_object().unwrap().clone()).unwrap();

// Send pair.access to the client; store pair.refresh in HttpOnly cookie.
```

### 14b. Verify on each authenticated request

```rust
let claims = jwt
    .verify_access(&access_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;

let roles: Vec<String> = claims.get_custom("roles").unwrap_or_default();
```

### 14c. Refresh

```rust
let new_pair = jwt
    .refresh(&refresh_token)
    .ok_or(StatusCode::UNAUTHORIZED)?;
// Roles + scope from the original login are preserved automatically.
```

---

## Step 15 — Add security middleware

```rust
use rustango::security_headers::{SecurityHeadersLayer, SecurityHeadersRouterExt, CspBuilder};
use rustango::cors::{CorsLayer, CorsRouterExt};
use rustango::rate_limit::{RateLimitLayer, RateLimitRouterExt};
use rustango::access_log::{AccessLogLayer, AccessLogRouterExt};
use rustango::request_id::{RequestIdLayer, RequestIdRouterExt};
use rustango::health::health_router;
use std::time::Duration;

let app = axum::Router::new()
    .merge(rustango::welcome::welcome_router())
    .nest("/admin", admin)
    .merge(myblog::urls::router(pool.clone()))
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

---

## Step 16 — Write tests

```bash
cargo run -- make:test post_smoke
```

Edit `tests/post_smoke.rs`:

```rust
use rustango::test_client::TestClient;
use myblog::urls;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    urls::router(pool)
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
    assert_eq!(response.status, 200);
    let v: serde_json::Value = response.json();
    assert_eq!(v["title"], "Test");
}
```

Run:

```bash
cargo test --test post_smoke
```

---

## Step 17 — System check

Before deploying:

```bash
cargo run -- check --deploy
```

Output (with sensible env):

```
running rustango system check (deploy mode)...
  [info]    1 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    SECRET_KEY length OK
all checks passed
```

If any are warnings/errors, fix them before pushing to prod.

---

## Step 18 — Production deploy

The deploy itself is platform-specific (Fly, Railway, Kubernetes, bare ECS, etc.). The framework-side checklist:

```bash
# 1. Set production env
export RUSTANGO_ENV=prod
export DATABASE_URL=postgres://prod-host/myblog
export SECRET_KEY=$(openssl rand -base64 32)

# 2. Run migrations
cargo run --release --bin manage -- migrate

# 3. Audit
cargo run --release --bin manage -- check --deploy

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

## Where to next

| Topic | Doc |
|---|---|
| Every `manage` subcommand | [`docs/manage.md`](manage.md) |
| ORM cookbook (advanced filters, aggregations, M2M, soft delete) | [`docs/orm.md`](orm.md) (coming soon) |
| Security features in depth | [`docs/security.md`](security.md) (coming soon) |
| Multi-tenancy | [README — Multi-tenancy section](../README.md#multi-tenancy) |
| API docs | <https://docs.rs/rustango> |

If you hit something that doesn't work or is unclear, open an issue.
