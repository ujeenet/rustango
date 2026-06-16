# ViewSets — CRUD REST APIs

A ViewSet turns a model into a full REST resource — list, create, retrieve,
update, partial-update and delete — from one declaration. It's **Rustango**'s
equivalent of a Django REST Framework `ModelViewSet` or a Laravel API resource
controller: you describe *what* to expose (fields, filters, search, ordering,
pagination, permissions) and the framework wires the routes, query-param
parsing, and JSON envelopes for you.

This guide covers scaffolding a ViewSet, the full set of CRUD endpoints you get,
how to restrict or extend them, and how to mount them into your app.

[![A Rustango ViewSet: one #[derive(ViewSet)] block declares the model, fields, filters, search, ordering and page size — and generates the six CRUD routes](/static/img/viewsets.png?v=1)](/static/img/viewsets.png?v=1)

> **Runnable version:** the ViewSet flow is part of the tested
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog)
> example (Step 12), and every behavior here is covered by the framework's own
> live tests — `crates/rustango/tests/viewset_*_live.rs`.

---

## Contents

- [Scaffold one](#scaffold-one) — `make:viewset` & `make:api_routes`
- [The two ways to define a ViewSet](#the-two-ways-to-define-a-viewset)
- [The CRUD endpoints you get](#the-crud-endpoints-you-get)
- [Choosing which operations to expose](#choosing-which-operations-to-expose)
- [`#[viewset(...)]` reference](#viewset-attribute-reference) · [Builder reference](#builder-reference)
- [Filtering, search & ordering](#filtering-search-and-ordering) · [Pagination](#pagination)
- [Permissions & throttling](#permissions-and-throttling) · [Custom actions](#custom-actions-beyond-crud)
- [Response shape](#response-shape-fields-and-serializers) · [Mounting](#mounting) · [Backends](#backend-support)

---

## Scaffold one

Two generators get you from nothing to a mounted API.

**`make:viewset`** writes a ViewSet for a model:

```bash
cargo run -- make:viewset PostViewSet --model Post
```

It detects whether your project uses the `tenancy` feature and emits the right
shape (pass `--tenant` / `--no-tenant` to force it). The default
(single-tenant) template is the derive form:

```rust
//! Auto-scaffolded by `manage make:viewset PostViewSet`.

use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model        = Post,
    fields       = "id, ",
    filter_fields = "",
    search_fields = "",
    page_size    = 20,
)]
pub struct PostViewSet;

// Mount in your urls.rs:
//
//   .merge(PostViewSet::router("/api/post", pool.clone()))
```

The `--tenant` template instead emits a builder-based `pub fn router()` using
`ViewSet::for_model(Post::SCHEMA).…tenant_router("/api/post")`, with every knob
present as a commented `// .filter_fields(...)` line to uncomment.

**`make:api_routes`** writes a per-app router that composes your viewsets:

```bash
cargo run -- make:api_routes blog
```

```rust
//! Auto-scaffolded by `manage make:api_routes blog`.

use axum::Router;
use rustango::sql::sqlx::PgPool;

pub fn api(pool: PgPool) -> Router<()> {
    let _pool = pool;
    Router::new()
        // .merge(super::viewsets::post::router("/api/post", _pool.clone()))
}
```

Add a `.merge(...)` line per resource, then wire the app's `api()` into your
top-level `urls.rs`.

---

## The two ways to define a ViewSet

Both produce an `axum::Router` of the same CRUD routes; pick by project shape.

**1. The derive macro** — fixed config, single-tenant:

```rust
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

let router = PostViewSet::router("/api/posts", pool);   // -> axum::Router
```

**2. The builder** — `ViewSet::for_model(...)`, programmatic, and the only form
that's tri-dialect (Postgres / SQLite / MySQL) and tenancy-aware:

```rust
use rustango::viewset::ViewSet;
use rustango::core::Model as _;

let router = ViewSet::for_model(Post::SCHEMA)
    .fields(&["id", "title", "body", "status", "author_id", "published_at"])
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .ordering(&[("published_at", true)])        // true = DESC
    .page_size(20)
    .router_pool("/api/posts", pool);           // tri-dialect Pool
```

The macro is the quick path for a fullstack/api project; reach for the builder
when you need SQLite/MySQL, multi-tenancy, a runtime-built config, or the extras
(throttling, custom filter backends, cursor pagination).

---

## The CRUD endpoints you get

Mounting at `/api/posts` wires all six REST operations:

| Verb | Path | Action | Success | Body |
|---|---|---|---|---|
| `GET` | `/api/posts` | **list** | 200 | paginated envelope (see [Pagination](#pagination)) |
| `POST` | `/api/posts` | **create** | 201 | the created object — *or an array, for bulk create* |
| `GET` | `/api/posts/{pk}` | **retrieve** | 200 | the object |
| `PUT` | `/api/posts/{pk}` | **update** (full) | 200 | the updated object |
| `PATCH` | `/api/posts/{pk}` | **partial update** | 200 | the updated object (only supplied fields change) |
| `DELETE` | `/api/posts/{pk}` | **destroy** | 204 | empty |

A trailing slash on the mount prefix is optional (`/api/posts` and
`/api/posts/` behave identically). Only these six verbs are wired — there's no
automatic `HEAD`/`OPTIONS`.

**Bulk create** comes for free: `POST` a JSON *array* and every element is
inserted in order, validated atomically (one bad element rejects the whole
batch). A single JSON object still creates one row.

---

## Choosing which operations to expose

To publish a **read-only** resource (list + retrieve only, no write verbs), add
`read_only`:

```rust
// macro
#[viewset(model = Post, read_only)]
pub struct PostViewSet;

// builder
ViewSet::for_model(Post::SCHEMA).read_only().router_pool("/api/posts", pool);
```

There's no per-verb toggle beyond read-only (no DRF-style `http_method_names`).
If you need, say, "everything except delete", mount the ViewSet and override the
one route with your own handler (see [Custom actions](#custom-actions-beyond-crud)),
or build that resource by hand — the ViewSet is the full-CRUD fast path.

---

## `#[viewset(...)]` attribute reference

| Key | Example | Default | What it does |
|---|---|---|---|
| `model` | `model = Post` | **required** | The model the resource is built over (`Post::SCHEMA`). |
| `fields` | `"id, title, body"` | all scalar fields | Whitelist: which columns appear in responses **and** are accepted on create/update. |
| `filter_fields` | `"author_id, status"` | none | Fields filterable via `?field=value` (+ lookups). |
| `search_fields` | `"title, body"` | none | Fields the `?search=` box matches (case-insensitive OR). |
| `ordering` | `"-published_at, id"` | none | Default sort (`-` = DESC). |
| `page_size` | `20` | 20 | Rows per page (client `?page_size=` capped at 1000). |
| `read_only` | *(flag)* | off | Expose GET (list + retrieve) only. |
| `permissions(...)` | `permissions(create = "post.add", destroy = "post.delete")` | none | Per-action permission codenames (see [below](#permissions-and-throttling)). |

---

## Builder reference

Every method on `ViewSet::for_model(SCHEMA)` (each returns `Self`):

| Method | Purpose |
|---|---|
| `fields(&["…"])` | Restrict response + writable fields. |
| `filter_fields(&["…"])` | Enable `?field=value` filtering. |
| `search_fields(&["…"])` | Enable `?search=`. |
| `ordering(&[("field", desc)])` | Default sort order. |
| `ordering_fields(&["…"])` | Whitelist which fields `?ordering=` may use (DRF parity). |
| `page_size(n)` | Default page size (≤ 1000). |
| `read_only()` | GET-only. |
| `permissions(ViewSetPerms{…})` | Per-action codename gates. |
| `permissions_for_model::<T>()` | *(tenancy)* auto-fill `view`/`add`/`change`/`delete` codenames. |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Keyset pagination on a field (skips `COUNT(*)`). |
| `limit_offset_pagination()` | `?limit=&offset=` windowing. |
| `pagination(PaginationStyle::…)` | Set the style explicitly. |
| `filter_backend(closure)` | Add custom `WHERE` predicates beyond `filter_fields`. |
| `throttle(…)` / `throttle_all(max, secs)` | Per-action fixed-window rate limits. |
| `router(prefix, pgpool)` | Mount (Postgres, static pool). |
| `router_pool(prefix, pool)` | Mount tri-dialect (PG / SQLite / MySQL). |
| `tenant_router(prefix)` | *(tenancy)* mount with per-request tenant resolution. |

---

## Filtering, search and ordering

All driven by query params on the **list** endpoint.

**Filtering** — each field named in `filter_fields` accepts `?field=value`
(exact) plus Django-style lookups via a `__suffix`:

```
?status=published
?author_id__in=1,2,3
?view_count__gte=100
?title__icontains=rust
?deleted_at__isnull=true
```

Supported lookups: `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`, `contains`,
`icontains`, `startswith`, `istartswith`, `endswith`, `iendswith`, `isnull`
(no suffix = exact). Fields not in `filter_fields` are ignored.

**Search** — `?search=term` matches the `search_fields` with a case-insensitive
OR (`ILIKE` on Postgres, normalised `LIKE` on SQLite/MySQL).

**Ordering** — `?ordering=field,-other` (prefix `-` for DESC). Any field is
sortable by default; call `.ordering_fields([...])` to restrict it to a
whitelist. Without a query param, the `ordering` default applies.

They compose: `?author_id=42&status__in=draft,published&search=async&ordering=-published_at`.

---

## Pagination

Three styles; page-number is the default. The list envelope differs per style:

**Page-number** (default) — `?page=2&page_size=20`:

```json
{ "count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ] }
```

**Cursor** — opt in with `.cursor_pagination("id")` (or `_desc`); skips the
`COUNT(*)`, ideal for very large tables. `?cursor=<token>&page_size=20`:

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/offset** — opt in with `.limit_offset_pagination()`. `?limit=20&offset=40`:

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` are clamped to 1000; negative pages/offsets are clamped to
their floor.

---

## Permissions and throttling

**Permissions** gate each action on a set of codenames (OR within an action):

```rust
use rustango::viewset::{ViewSet, ViewSetPerms};

ViewSet::for_model(Post::SCHEMA)
    .permissions(ViewSetPerms {
        list:     vec!["post.view".into()],
        retrieve: vec!["post.view".into()],
        create:   vec!["post.add".into()],
        update:   vec!["post.change".into()],
        destroy:  vec!["post.delete".into()],
    })
    .router_pool("/api/posts", pool);
```

An empty action list means "no check". Enforcement reads an authenticated user
from the request (the `tenancy` feature's auth integration); superusers bypass,
and a missing user is denied. In tenancy projects, `.permissions_for_model::<Post>()`
fills these in with the standard `post.view` / `add` / `change` / `delete`
codenames automatically.

**Throttling** applies fixed-window per-client rate limits, per action:

```rust
ViewSet::for_model(Post::SCHEMA)
    .throttle_all(60, 60)              // 60 requests / 60s across every action
    .router_pool("/api/posts", pool);
```

Over-limit requests get `429 Too Many Requests` with a `Retry-After` header.
Counters are per-process; the client key is the connection IP (or
`X-Forwarded-For` / `X-Real-IP`).

---

## Custom actions beyond CRUD

There's no DRF `@action` decorator — the ViewSet is strictly the six CRUD
routes. For extra endpoints (a `/stats`, a `/bulk_archive`), mount your own
handlers alongside the ViewSet router:

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

For per-list extra `WHERE` logic that filters can't express, `.filter_backend(…)`
lets you contribute predicates to the built-in list query without a separate
route.

---

## Validation on writes

The auto-generated create/update path validates the inbound body as it binds it
to the model's fields:

- **Types are coerced and checked.** A field declared `i64` / `DateTime` /
  `Uuid` / `bool` etc. must parse; a bad value (e.g. `"abc"` for an integer) is
  rejected with `400 Bad Request` naming the field.
- **Required / NOT NULL is enforced.** A missing non-nullable field — or an
  empty string for a non-nullable `String` (Django's "blank ≠ null") — is a
  `400`. Nullable fields accept empty → `NULL`.
- **Database constraints are enforced by the database.** Unique violations,
  `VARCHAR(n)` length (Postgres/MySQL), foreign keys and check constraints
  surface as a `400` when the INSERT/UPDATE fails.

What the auto path does **not** do is run **application-level validation** — it
does not call a [`Serializer`](serializers.md)'s `validate()`, per-field
validators, or any custom business rule, and there's no hook to inject one into
the generated create/update path. So format checks (email, regex), cross-field
rules, and `max_length` on SQLite are not enforced by the ViewSet itself.

When you need those, validate in a **custom handler** before saving — parse the
body into a serializer or a `ModelForm`, call `.validate()`, and `save(&pool)`
on success — and mount it alongside (or in place of) the ViewSet route.

---

## Response shape: fields and serializers

Today the **response shape is controlled by the ViewSet's `fields`** — the
columns you list are the columns projected into the JSON (and the fields
accepted on write). That's the supported way to slim or pin a payload:

```rust
#[viewset(model = Post, fields = "id, title, status, published_at")]
pub struct PostFeedViewSet;
```

There is a `.serializer::<S>()` builder method intended to render responses
through a [`Serializer`](serializers.md)'s `source` / `method` / `nested`
overrides. **Heads-up:** after the v0.38 tri-dialect refactor the list path
projects JSON directly (so it works on every backend), and the per-row
serializer rendering is **not currently applied** to ViewSet responses. So for
now, shape a ViewSet's output with `fields`, or map through a serializer in a
custom handler (`S::from_model(&row).to_value()`). Serializers remain fully
useful standalone — see the [serializers guide](serializers.md).

---

## Mounting

Compose the ViewSet's router into your app router. Single-tenant, static pool:

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))               // macro form
    .merge(ViewSet::for_model(Author::SCHEMA).router_pool("/api/authors", pool.clone())); // builder
```

Multi-tenant (no pool captured — each request resolves its tenant connection):

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA).tenant_router("/api/posts"));
```

`make:api_routes <app>` generates a per-app `api()` that gathers these
`.merge(...)` lines into one place; wire that into your top-level `urls.rs`.

---

## Backend support

- **Builder + `router_pool` / `tenant_router`** is **tri-dialect** — Postgres,
  SQLite and MySQL — and is the recommended path.
- **The derive macro's `router(prefix, PgPool)`** and the Postgres-typed
  `.router(...)` are Postgres-only (they capture a `PgPool`).
- All of filtering, search, ordering, the three pagination modes, permissions,
  throttling and bulk-create work across the supported backends on the builder
  path.
- The `.serializer::<S>()` response-shaping is the one ViewSet feature not
  currently wired (see [above](#response-shape-fields-and-serializers)).

---

## Try it

The end-to-end ViewSet flow — scaffold, mount, then `curl` the CRUD endpoints —
is Steps 12–13 of the [getting-started guide](getting-started.md), backed by the
compilable `getting_started_blog` example.

The framework's own live tests under `crates/rustango/tests/viewset_*.rs` are
the most complete, runnable reference for each feature (CRUD, the three
pagination modes, filtering/search/ordering, throttling, bulk create). They run
on an in-memory SQLite database — no external DB needed — but need the matching
feature flags switched on, e.g.:

```bash
cd crates/rustango
cargo test --features sqlite,tenancy --test viewset_sqlite_live
```
