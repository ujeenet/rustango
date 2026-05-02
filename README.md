# rustango

**A Django-shaped, batteries-included web framework for Rust.**

ORM with auto-migrations, multi-tenancy, auto-admin, JWT lifecycle, signals, caching, file storage, email, scheduled tasks, RFC 6238 TOTP, signed URLs, security headers, rate limiting, CORS, ETag, content negotiation, i18n, fixtures, test client — all shipped, all opt-out via cargo features.

```toml
[dependencies]
rustango = "0.20"
```

---

## Table of contents

- [Quick start](#quick-start)
- [Project layout](#project-layout)
- [`manage` CLI reference](#manage-cli-reference)
- [ORM cookbook](#orm-cookbook)
- [Migrations](#migrations)
- [Auto-admin](#auto-admin)
- [APIs (ViewSet + Serializer + JWT)](#apis-viewset--serializer--jwt)
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
cargo run --bin manage -- migrate           # apply bootstrap migrations
cargo run                                   # http://localhost:8080
```

You should see the **welcome page** confirming rustango is wired up. Replace `welcome_router()` in `src/main.rs` once you mount your own `/` route.

### 4. Add an app + model

```bash
cargo run --bin manage -- startapp blog     # scaffolds src/blog/
```

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
cargo run --bin manage -- makemigrations    # generates migration JSON
cargo run --bin manage -- migrate           # applies it
```

### 5. Generate a viewset + serializer

```bash
cargo run --bin manage -- make:viewset PostViewSet --model Post
cargo run --bin manage -- make:serializer PostSerializer --model Post
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
├── migrations/                 # JSON files written by `manage makemigrations`
├── src/
│   ├── main.rs                 # HTTP entry point — Builder chain
│   ├── lib.rs                  # `pub mod` registry of every app module
│   ├── urls.rs                 # top-level Router composition
│   ├── models.rs               # OR per-app: src/blog/models.rs
│   ├── views.rs
│   ├── urls.rs
│   └── bin/manage.rs           # CLI dispatcher (re-exports user models)
└── tests/                      # integration tests using test_client
```

Per-app structure (created by `manage startapp <name>`):

```
src/blog/
  ├── mod.rs
  ├── models.rs                 # #[derive(Model)] structs
  ├── views.rs                  # axum handlers
  └── urls.rs                   # router for this app
```

---

## `manage` CLI reference

Every command has `--help`. Exit code is non-zero on validation/IO/system-check errors.

### Migrations

```bash
manage makemigrations [name]                   # diff registry → next JSON
manage makemigrations --app <app> [name]       # per-app migration dir
manage makemigrations --empty <name>           # scaffold for hand-authored data ops
manage migrate                                  # apply every pending
manage migrate <target>                         # forward or back to <target>; `zero` wipes
manage migrate --dry-run                        # print SQL without writing
manage downgrade [N]                            # step back N (default 1)
manage showmigrations | status                  # [X] applied / [ ] pending list
```

### Data migrations

```bash
manage add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

manage add-data-op --to 0003_add_slug --sql "UPDATE posts SET slug = id::text"
```

### Scaffolders

```bash
manage startapp <name> [--with-manage-bin]
manage make:viewset PostViewSet --model Post
manage make:serializer PostSerializer --model Post
manage make:form ContactForm
manage make:job EmailDigestJob
manage make:notification WelcomeEmail
manage make:middleware AuditLog
manage make:test post_smoke
```

### System

```bash
manage about                # version, model count, registered apps, DB connectivity
manage check                # pending migrations, missing models, DB reachable
manage check --deploy       # + SECRET_KEY length, RUSTANGO_ENV, DATABASE_URL
manage docs                 # opens https://docs.rs/rustango in browser
manage version | --version  # framework version
```

### Tenancy (only with `tenancy` feature)

```bash
manage create-tenant acme --display-name "ACME Corp"
manage create-operator admin --password letmein
manage create-user acme alice --password hunter2 --superuser
manage list-tenants
manage audit-cleanup --days 90
manage audit-cleanup --keep-last 50 --tenant acme
```

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

---

## Migrations

```bash
manage makemigrations              # diff inventory ↔ snapshot, emit JSON
manage migrate                     # apply pending
manage migrate --dry-run           # print SQL only
manage migrate <target>            # forward or back to specific name
manage downgrade 2                 # step back 2 migrations
manage showmigrations              # status
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
| Renames | NOT auto-detected | use `manage makemigrations --empty` and edit JSON |

### Hand-authored data migrations

```bash
# Quick path — one-liner
manage add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Or append to an existing migration
manage add-data-op --to 0003_add_slug --sql "UPDATE posts SET slug = id::text"

# Or scaffold an empty file and edit manually
manage makemigrations --empty seed_initial_categories
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

Generate a secret: `openssl rand -base64 32`.

### Tenant resolver chain

Auto-resolves `Tenant` from request via, in order:
1. Subdomain (`acme.myapp.com`)
2. URL path prefix (`/t/acme/...`)
3. Custom header (`X-Tenant-Slug: acme`)
4. Port (rare; for testing)

### Storage modes

- **Schema mode** — one Postgres schema per tenant, single database
- **Database mode** — full database per tenant; `TenantPools` lazily opens connections

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

### CORS presets

```rust
CorsLayer::permissive()                          // dev: any origin, common methods
CorsLayer::new().allow_origins(vec!["..."])      // prod: explicit allowlist
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

```rust
use rustango::storage::{Storage, BoxedStorage, LocalStorage, InMemoryStorage};

let storage: BoxedStorage = Arc::new(
    LocalStorage::new("./uploads".into())
        .with_base_url("https://cdn.example.com/uploads")
);

storage.save("avatars/alice.png", &png_bytes).await?;
let bytes = storage.load("avatars/alice.png").await?;
let url = storage.url("avatars/alice.png");          // → https://cdn.../avatars/alice.png
```

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
rustango = "0.20"

# Multi-tenant
rustango = { version = "0.20", features = ["tenancy"] }

# With Redis cache
rustango = { version = "0.20", features = ["cache-redis"] }

# Bare ORM only (no admin, no forms, no email, no storage)
rustango = { version = "0.20", default-features = false, features = ["postgres"] }
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

---

## Production checklist

Run before deploy:

```bash
cargo run --bin manage -- check --deploy
```

Audits:
- ✅ DEBUG-style env (`RUSTANGO_ENV` is `prod` or `production`)
- ✅ `SECRET_KEY` set and ≥ 32 bytes
- ✅ `DATABASE_URL` set
- ✅ Pending migrations applied
- ✅ Models registered in inventory

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
