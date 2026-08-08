# ViewSets — CRUD REST APIs

A ViewSet turns a model into a full REST resource — endpoints to **list,
create, read, update and delete** records — from one declaration. (It's
**Rustango**'s equivalent of a Django REST Framework `ModelViewSet` or a Laravel
API resource controller, if you've used those.)

> **New to REST APIs?** This guide assumes you know what an *endpoint*, an *HTTP
> verb* (GET / POST / …) and a *JSON request and response* are. If any of those
> are fuzzy, the [glossary](glossary.md#web-api-basics) is a five-minute primer —
> read it first, then come back here.

Pair a ViewSet with a [serializer](serializers.md) — the piece that shapes your
JSON — and it guards **both directions** at once: the serializer formats every
**response** (rename, hide, compute or nest fields) *and* governs every
**request** (it validates incoming data and silently ignores fields a client
shouldn't be allowed to set). Rejected input comes back in the familiar DRF
shape — a JSON object keyed by field name. It all works the same on PostgreSQL,
MySQL and SQLite.

This guide is tutorial-first: we **build a complete REST blog API** end to end —
scaffolding, models, a serializer, the ViewSet, all six CRUD endpoints, input
validation, filtering/search/pagination, and tests — then the rest of the page
is a reference for every knob.

[![A Rustango ViewSet wired to a serializer: one #[viewset(serializer = …)] block gives typed JSON output and validated input across the six CRUD routes](img/viewsets.png)](img/viewsets.png)

> **Source:** `rustango::viewset` (`ViewSet`, `#[derive(ViewSet)]`, the
> `#[viewset(...)]` options + the `for_model` builder) — always compiled.
>
> **Runnable version:** the blog built here mirrors the tested, compilable
> [`getting_started_blog`](../crates/rustango/examples/getting_started_blog)
> example (its `Post` / `PostSerializer` / `PostViewSet`), and every behavior is
> pinned by the framework's own live tests — `crates/rustango/tests/viewset_*.rs`
> (notably `viewset_serializer_render_sqlite_live` and
> `viewset_serializer_input_sqlite_live`).

---

## Table of contents
- [API views vs HTML views](#api-views-vs-html-views) — JSON for clients, or HTML pages?
- [Build a REST blog API](#build-a-rest-blog-api) — the full walkthrough
- [The serializer marriage: input + output](#the-serializer-marriage-input--output)
- [The two ways to define a ViewSet](#the-two-ways-to-define-a-viewset)
- [The CRUD endpoints](#the-crud-endpoints) · [Choosing which to expose](#choosing-which-operations-to-expose)
- [`#[viewset(...)]` reference](#viewset-attribute-reference) · [Builder reference](#builder-reference)
- [Filtering, search & ordering](#filtering-search-and-ordering) · [Pagination](#pagination)
- [Validation](#validation) · [Permissions & throttling](#permissions-and-throttling) · [Custom actions](#custom-actions-beyond-crud)
- [Mounting](#mounting) · [Backends](#backend-support)

---

## API views vs HTML views

Before the tutorial, one fork in the road. **Rustango** has two ways to turn a
model into endpoints, and a ViewSet is one of them:

- A **ViewSet** (this guide) is an **API view** — it speaks **JSON**, for
  frontend frameworks, mobile apps, and other services.
- A **template view** ([HTML views](html-views.md)) is an **HTML view** — it
  renders **server-side pages** through Tera, for browsers and server-rendered
  sites.

Same model underneath; what differs is what comes out and who's calling.

| | **API view** — ViewSet (here) | **HTML view** — [template views](html-views.md) |
|---|---|---|
| Module | `rustango::viewset` | `rustango::template_views` |
| Sends back | **JSON data** | a **server-rendered HTML page** |
| Built for | SPAs, mobile, other services | browsers, server-rendered sites, admin-style CRUD |
| A "create" | `POST` JSON → `201` + the object | `POST` a form → `303` redirect (Post/Redirect/Get) |
| On bad input | `400` + a field-keyed JSON error map | re-render the form with the errors shown |
| A "list" is | a paginated JSON envelope | a loop over rows in your template |
| Usually authed by | tokens / JWT / API keys | session cookies |
| Django analogue | DRF `ModelViewSet` | generic class-based views |

Pick per resource — and you can mount **both on the same model** (a public JSON
API *and* internal CRUD pages). The rest of this guide is the JSON/API side; for
the HTML side see [HTML views — server-rendered pages](html-views.md).

---

## Build a REST blog API

We'll build a blog with two models — `Author` and `Post` — and expose `Post` as
a REST resource at `/api/posts` whose JSON shape and validation are driven by a
serializer. By the end you can `curl` every CRUD verb and watch the serializer
shape output and reject bad input.

This walkthrough assumes a project created with `cargo rustango new myblog`
(see [Getting Started](getting-started.md) for project setup and the database).
Every step is a real command or file.

### Step 1 — Create the blog app

Apps are self-contained feature modules (Django's `startapp`):

```bash
cargo run -- startapp blog
```

That writes `src/blog/{mod,models,views,urls,tests}.rs` and wires the module
into `main.rs` + the `urls::api()` aggregator.

### Step 2 — Define the models

`src/blog/models.rs` — an `Author` and a `Post` (a foreign key links them):

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "authors", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 120)]
    pub name: String,
    #[rustango(max_length = 200)]
    pub email: String,
}

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title", index("status, published_at"))]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(max_length = 20, default = "'draft'")]
    pub status: String,                       // draft | published | archived

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub published_at: Auto<DateTime<Utc>>,
}
```

### Step 3 — Migrate

Generate and apply the migration (same as `makemigrations` + `migrate`):

```bash
cargo run -- makemigrations
cargo run -- migrate
```

### Step 4 — Scaffold the serializer

The serializer is what makes this a *DRF* API — it defines the request/response
contract. Generate the skeleton:

```bash
cargo run -- make:serializer PostSerializer --model Post
```

Then fill it in. This one exercises the whole input+output surface — a rename,
a computed read-only field, a read-only server field, and a field validator:

```rust
// src/blog/post_serializer.rs
use rustango::{Auto, Serializer};
use chrono::{DateTime, Utc};
use crate::blog::models::Post;

#[derive(Serializer, serde::Deserialize, Default)]
#[serializer(model = Post)]
pub struct PostSerializer {
    pub id: Auto<i64>,

    #[serializer(validate = "title_min_3")]   // input: reject titles < 3 chars
    pub title: String,

    #[serializer(source = "body")]            // JSON key `content`, column `body`
    pub content: String,

    pub status: String,
    pub author_id: i64,

    #[serializer(method = "summary")]         // output: computed, never written
    pub summary: String,

    #[serializer(read_only)]                  // output: shown, ignored on write
    pub published_at: Auto<DateTime<Utc>>,
}

impl PostSerializer {
    fn title_min_3(t: &String) -> Result<(), String> {
        if t.chars().count() < 3 {
            Err("title must be at least 3 characters".into())
        } else {
            Ok(())
        }
    }
    fn summary(p: &Post) -> String {
        p.body.chars().take(80).collect::<String>()
    }
}
```

Register the module — add `pub mod post_serializer;` to `src/blog/mod.rs`.

Note we only wrote one validator (`title_min_3`); the fields also **inherit the
model's constraints** automatically — `title` is length-checked against the
model's `max_length = 200`, and a `choices`/`min`/`max` column would be checked
too, all returning friendly `400`s on write. Add `max_length` / `min_length` /
`min` / `max` serializer attributes to override a field's bound. (See the
[serializers guide](serializers.md#validation) for the full validation story.)

### Step 5 — Scaffold the ViewSet and wire the serializer

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Edit it to declare the resource and **wire the serializer with the `serializer`
attribute** — that one line turns on serializer-driven output *and* input:

```rust
// src/blog/post_view_set.rs
use rustango::ViewSet;

#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;
```

Add `pub mod post_view_set;` to `src/blog/mod.rs`.

> With a serializer wired you don't need `fields = "..."` — the serializer is
> the projection. Use `fields` only when you want the default (non-serializer)
> field projection instead.

### Step 6 — Mount the routes

In a single-tenant project, nest the ViewSet's router under a path, passing the
pool:

```rust
// src/blog/urls.rs (or your urls::api aggregator)
use axum::Router;
use rustango::sql::sqlx::PgPool;
use crate::blog::post_view_set::PostViewSet;

pub fn api(pool: PgPool) -> Router {
    Router::new()
        .merge(PostViewSet::router("/api/posts", pool))
}
```

`make:api_routes blog` scaffolds exactly this aggregator if you'd rather
generate it. Wire `blog::urls::api(pool)` into your top-level `urls.rs`.

### Step 7 — Run it and exercise every endpoint

```bash
cargo run            # listening on http://0.0.0.0:8080
```

**Create** (`POST`). The serializer validates first, then writes only the
fields it accepts:

```bash
# happy path — note `content` (the renamed `body`) on the way in
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Hello Rustango","content":"First post body.","status":"published","author_id":1}'
```
```json
{
  "id": 1,
  "title": "Hello Rustango",
  "content": "First post body.",
  "status": "published",
  "author_id": 1,
  "summary": "First post body.",
  "published_at": "2026-01-02T12:00:00Z"
}
```
The response is the **serializer's** shape: `body` came back as `content`, the
computed `summary` appeared, and `published_at` (read-only, server-set) is
present.

**Validation rejects bad input** with a DRF-shape `400` — field-keyed arrays of
messages:

```bash
curl -i -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"hi","content":"x","author_id":1}'
# HTTP/1.1 400 Bad Request
# {"title":["title must be at least 3 characters"]}
```

**Read-only / computed fields a client posts are ignored** — they can't inject
`published_at` or `summary`:

```bash
curl -X POST localhost:8080/api/posts \
  -H 'content-type: application/json' \
  -d '{"title":"Sneaky","content":"x","author_id":1,"published_at":"1999-01-01T00:00:00Z","summary":"hax"}'
# → published_at is the server value, not 1999; summary is recomputed from body.
```

**List** (`GET`) — paginated, each row in the serializer's shape:

```bash
curl localhost:8080/api/posts
```
```json
{ "count": 1, "page": 1, "page_size": 20, "last_page": 1, "results": [ { "id": 1, "title": "Hello Rustango", … } ] }
```

**Retrieve / update / partial-update / delete:**

```bash
curl localhost:8080/api/posts/1                       # retrieve  → 200
curl -X PUT   localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Edited","content":"new body","status":"published","author_id":1}'   # full update → 200
curl -X PATCH localhost:8080/api/posts/1 -H 'content-type: application/json' \
     -d '{"title":"Just the title"}'                   # partial update → 200 (other fields untouched)
curl -X DELETE localhost:8080/api/posts/1              # destroy → 204
```

`PATCH` validation runs on what you send; read-only fields stay at their server
value even if posted.

### Step 8 — Filter, search, order, paginate

All on the list endpoint, no extra code (you declared the fields in Step 5):

```bash
curl 'localhost:8080/api/posts?status=published&author_id=1'      # filter
curl 'localhost:8080/api/posts?status__in=published,archived'     # lookup
curl 'localhost:8080/api/posts?search=rustango'                   # search title+body
curl 'localhost:8080/api/posts?ordering=title'                    # sort (asc)
curl 'localhost:8080/api/posts?page=2&page_size=10'               # paginate
```

### Step 9 — Test it

The framework ships an in-process test client — assert on real HTTP responses
without booting a server:

```rust
// tests/post_api.rs
use rustango::test_client::TestClient;
use myblog::blog::post_view_set::PostViewSet;
use rustango::sql::sqlx::PgPool;
use serde_json::json;

async fn app() -> axum::Router {
    let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
    PostViewSet::router("/api/posts", pool)
}

#[tokio::test]
async fn rejects_short_title() {
    let client = TestClient::new(app().await);
    let res = client.post("/api/posts")
        .json(&json!({"title":"hi","content":"x","author_id":1}))
        .send().await;
    assert_eq!(res.status, 400);
    assert!(res.json_value()["title"].is_array());   // DRF field-error shape
}

#[tokio::test]
async fn create_then_list() {
    let client = TestClient::new(app().await);
    let created = client.post("/api/posts")
        .json(&json!({"title":"Hello","content":"b","status":"published","author_id":1}))
        .send().await;
    assert_eq!(created.status, 201);
    let list = client.get("/api/posts").send().await;
    assert!(list.json_value()["results"].is_array());
}
```

```bash
cargo test --test post_api
```

That's a complete, validated REST resource. The rest of this page is the
reference behind each step.

---

## The serializer marriage: input + output

Wiring a serializer (via `serializer = …` on the derive, or `.serializer::<S>()`
on the builder) changes **both** directions. It works on PostgreSQL, MySQL and
SQLite alike.

### Output — responses render through the serializer

`list`, `retrieve`, `create` and `update` responses are produced by
`S::from_model(&row)`, so the serializer's overrides shape the JSON:

| Serializer field | Effect on the response |
|---|---|
| `#[serializer(source = "body")]` | column `body` is emitted under the field's name (e.g. `content`) |
| `#[serializer(method = "fn")]` | a computed field appears (from `Self::fn(&model)`) |
| `#[serializer(read_only)]` | included in output |
| `#[serializer(write_only)]` | **omitted** from output |

> **`nested` / `many` caveat.** Nested and collection serializer fields render
> only when the related rows were loaded (via `select_related` / an eager
> fetch); otherwise they fall back to their default. The auto ViewSet list
> query loads the base row — wire relations explicitly if a nested field must
> be populated.

### Input — requests are validated and filtered

On `create` and `update`, when a serializer is registered:

1. **Validation runs.** The serializer's `validate()` — every per-field
   `#[serializer(validate = "fn")]` plus the container-level cross-field
   `validate` — runs against the JSON body. On failure the request is rejected
   `400 Bad Request` with the DRF error shape: a JSON object keyed by field name
   with arrays of messages, e.g. `{"title":["title must be at least 3 characters"]}`.
2. **Writable-field filtering.** Only the serializer's writable fields are
   persisted; `read_only` and `method`/computed fields a client posts are
   **ignored** (not written), and `source` renames are resolved to the model
   column. So a client can't set a server-controlled field by including it in
   the body.

> **Form-urlencoded bodies** (vs JSON) skip `validate()` — there's no typed
> value to validate — but still get writable-field filtering.

Under the hood this is the `ModelSerializer` trait's `validate()`,
`writable_source_fields()` and `from_writable_json()` methods, all generated by
`#[derive(Serializer)]`. See the [serializers guide](serializers.md) for how to
write the validators.

---

## The two ways to define a ViewSet

Both produce an `axum::Router` of the same CRUD routes.

**1. The derive macro** — declarative, single-tenant; wire a serializer with
`serializer = …`:

```rust
#[derive(ViewSet)]
#[viewset(
    model         = Post,
    serializer    = crate::blog::post_serializer::PostSerializer,
    filter_fields = "author_id, status",
    search_fields = "title, body",
    ordering      = "-published_at",
    page_size     = 20,
)]
pub struct PostViewSet;

let router = PostViewSet::router("/api/posts", pool);
```

**2. The builder** — `ViewSet::for_model(...)`, programmatic, tri-dialect
(PostgreSQL / SQLite / MySQL) and tenancy-aware; wire a serializer with
`.serializer::<S>()`:

```rust
use rustango::viewset::ViewSet;
use rustango::core::Model as _;

let router = ViewSet::for_model(Post::SCHEMA)
    .serializer::<PostSerializer>()
    .filter_fields(&["author_id", "status"])
    .search_fields(&["title", "body"])
    .ordering(&[("published_at", true)])    // true = DESC
    .page_size(20)
    .router_pool("/api/posts", pool);       // tri-dialect Pool
```

Reach for the builder when you need SQLite/MySQL, multi-tenancy, a runtime-built
config, or the extras (throttling, custom filter backends, cursor pagination).

---

## The CRUD endpoints

Mounting at `/api/posts` wires all six REST operations:

| Verb | Path | Action | Success | Body |
|---|---|---|---|---|
| `GET` | `/api/posts` | **list** | 200 | paginated envelope (see [Pagination](#pagination)) |
| `POST` | `/api/posts` | **create** | 201 | the created object — *or an array, for bulk create* |
| `GET` | `/api/posts/{pk}` | **retrieve** | 200 | the object |
| `PUT` | `/api/posts/{pk}` | **update** (full) | 200 | the updated object |
| `PATCH` | `/api/posts/{pk}` | **partial update** | 200 | the updated object (only supplied fields change) |
| `DELETE` | `/api/posts/{pk}` | **destroy** | 204 | empty |

A trailing slash on the mount prefix is optional. Only these six verbs are
wired — no automatic `HEAD`/`OPTIONS`. **Bulk create** is free: `POST` a JSON
*array* and every element is inserted in order, validated atomically (one bad
element rejects the whole batch).

---

## Choosing which operations to expose

For a **read-only** resource (list + retrieve only), add `read_only`:

```rust
#[viewset(model = Post, read_only)]            // macro
ViewSet::for_model(Post::SCHEMA).read_only()   // builder
```

There's no per-verb toggle beyond read-only. For "everything except delete",
mount the ViewSet and override the one route with your own handler (see
[Custom actions](#custom-actions-beyond-crud)).

---

## `#[viewset(...)]` attribute reference

| Key | Example | Default | What it does |
|---|---|---|---|
| `model` | `model = Post` | **required** | The model the resource is built over. |
| `serializer` | `serializer = path::To::S` | none | Wire a serializer for typed **output + input** (see [above](#the-serializer-marriage-input--output)). |
| `fields` | `"id, title, body"` | all scalar fields | Whitelist for the default (non-serializer) projection + writable fields. |
| `filter_fields` | `"author_id, status"` | none | Fields filterable via `?field=value` (+ lookups). |
| `search_fields` | `"title, body"` | none | Fields the `?search=` box matches (case-insensitive OR). |
| `ordering` | `"-published_at, id"` | none | Default sort (`-` = DESC). |
| `page_size` | `20` | 20 | Rows per page (client `?page_size=` capped at 1000). |
| `read_only` | *(flag)* | off | Expose GET (list + retrieve) only. |
| `permissions(...)` | `permissions(create = "post.add")` | none | Per-action permission codenames. |

---

## Builder reference

Every method on `ViewSet::for_model(SCHEMA)` (each returns `Self`):

| Method | Purpose |
|---|---|
| `serializer::<S>()` | Wire a serializer for typed output + input (tri-dialect). |
| `fields(&["…"])` | Default-projection + writable field whitelist (when no serializer). |
| `filter_fields(&["…"])` | Enable `?field=value` filtering. |
| `search_fields(&["…"])` | Enable `?search=`. |
| `ordering(&[("field", desc)])` | Default sort order. |
| `ordering_fields(&["…"])` | Whitelist which fields `?ordering=` may use. |
| `page_size(n)` | Default page size (≤ 1000). |
| `read_only()` | GET-only. |
| `permissions(ViewSetPerms{…})` / `permissions_for_model::<T>()` | Per-action codename gates (the latter on tenancy). |
| `cursor_pagination("id")` / `cursor_pagination_desc("id")` | Keyset pagination (skips `COUNT(*)`). |
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

**Filtering** — each `filter_fields` entry accepts `?field=value` (exact) plus
Django-style lookups via a `__suffix`:

```
?status=published
?author_id__in=1,2,3
?published_at__gte=2026-01-01
?title__icontains=rust
?body__isnull=false
```

Supported lookups: `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `not_in`, `contains`,
`icontains`, `startswith`, `istartswith`, `endswith`, `iendswith`, `isnull`
(no suffix = exact). Fields not in `filter_fields` are ignored.

**Search** — `?search=term` matches `search_fields` with a case-insensitive OR.

**Ordering** — `?ordering=field,-other` (`-` = DESC). Any field is sortable
unless you set `.ordering_fields([...])` to restrict it. Without a param, the
`ordering` default applies. They all compose.

---

## Pagination

> **Pitfall — paginate on a deterministic order.** Page-number and
> limit/offset pagination assume a stable sort; ordering on a non-unique column
> (or none) lets rows shift between pages — duplicated or skipped. Always add a
> unique tiebreaker, e.g. `ordering = "-published_at, id"`. (Both also run
> `COUNT(*)` per call; cursor pagination skips it for large tables.)

Three styles; page-number is the default. The list envelope differs per style:

**Page-number** (default) — `?page=2&page_size=20`:

```json
{ "count": 137, "page": 2, "page_size": 20, "last_page": 7, "results": [ … ] }
```

**Cursor** — `.cursor_pagination("id")` (or `_desc`); skips `COUNT(*)`, ideal
for very large tables. `?cursor=<token>&page_size=20`:

```json
{ "page_size": 20, "next": "<opaque-cursor-or-null>", "results": [ … ] }
```

**Limit/offset** — `.limit_offset_pagination()`. `?limit=20&offset=40`:

```json
{ "count": 137, "limit": 20, "offset": 40, "results": [ … ] }
```

`page_size` / `limit` are clamped to 1000.

---

## Validation

With a **serializer wired**, the create/update path runs the serializer's
validators and returns DRF-shape `400`s — the recommended way to validate (see
[the marriage](#the-serializer-marriage-input--output) and the
[serializers guide](serializers.md#validation)). Three layers run:

- **Declarative constraints** — `max_length` / `min_length` / `min` / `max`, and
  by default the field **inherits the model's** `max_length` / `min` / `max` /
  `choices`. So a `#[rustango(max_length = 200)]` column is length-checked on the
  API with no extra config (DRF `ModelSerializer` behaviour), turning would-be
  DB-constraint `500`s into friendly `400`s like
  `{"title":["Ensure this value has at most 200 characters."]}`.
- **Per-field** `validate = "fn"` and a **cross-field** `validate` hook — your
  custom rules (formats, cross-field, business logic).

Independently of a serializer, the write path always enforces the **schema**:

- **Types are coerced and checked** — a bad `i64` / `DateTime` / `Uuid` / `bool`
  value is a `400` naming the field.
- **Required / NOT NULL** — a missing non-nullable field (or empty string for a
  non-nullable `String`) is a `400`; nullable fields accept empty → `NULL`.
- **Database constraints** — unique, foreign keys and check constraints surface
  as a `400` on INSERT/UPDATE.

So even without a serializer you get type + required + DB-constraint validation;
wire a serializer to get declarative length/range/choice checks (auto-inherited)
plus your own per-field and cross-field rules.

---

## Permissions and throttling

> **A ViewSet is public by default.** Mounting one exposes all six CRUD verbs
> to anyone — there is no built-in authentication. Gate it with `permissions(...)`
> (below), put it behind the [auth middleware](auth-backends.md) (`require_auth`),
> or both, before exposing writes.

**Permissions** gate each action on codenames (OR within an action):

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

An empty action list = no check. Enforcement reads an authenticated user from
the request (the `tenancy` auth integration); superusers bypass, a missing user
is denied. `.permissions_for_model::<Post>()` auto-fills the standard
`post.view`/`add`/`change`/`delete` codenames.

**Throttling** applies fixed-window per-client limits, per action:

```rust
ViewSet::for_model(Post::SCHEMA)
    .throttle_all(60, 60)              // 60 requests / 60s per client, every action
    .router_pool("/api/posts", pool);
```

Over-limit → `429 Too Many Requests` + `Retry-After`. Counters are per-process;
the client key is the connection IP (or `X-Forwarded-For` / `X-Real-IP`).

---

## Custom actions beyond CRUD

There's no DRF `@action` decorator — the ViewSet is strictly the six CRUD
routes. For extra endpoints, mount your own handlers alongside the ViewSet:

```rust
use axum::{Router, routing::{get, post}};

let api = Router::new()
    .merge(ViewSet::for_model(Post::SCHEMA).router_pool("/api/posts", pool.clone()))
    .route("/api/posts/stats", get(post_stats))
    .route("/api/posts/bulk_archive", post(bulk_archive));
```

For extra `WHERE` logic, `.filter_backend(…)` contributes predicates without a
separate route.

### Scoping rows to the authenticated principal

A backend runs on **every** action — `list`, `retrieve`, `update`, `destroy` —
so it behaves like DRF's `get_queryset()`. A row the backend excludes is a
**404** on the item routes, not a 403: a 403 would confirm the id exists.

The principal lives in the request extensions, not the query string, so
implement the trait and override `filter_with`, which receives the request
`Parts`:

```rust
use axum::http::request::Parts;
use rustango::viewset::ViewSetFilter;

struct OwnerFilter;

impl ViewSetFilter for OwnerFilter {
    // No principal in hand — fail closed. Returning no predicates here would
    // widen the query to every row in the table.
    fn filter(&self, _p: &HashMap<String, String>, schema: &'static ModelSchema) -> Vec<WhereExpr> {
        deny_all(schema)
    }

    fn filter_with(
        &self,
        parts: &Parts,
        _p: &HashMap<String, String>,
        schema: &'static ModelSchema,
    ) -> Vec<WhereExpr> {
        let Some(user) = parts.extensions.get::<AuthenticatedUser>() else {
            return deny_all(schema);
        };
        vec![WhereExpr::Predicate(Filter {
            column: schema.field("owner_id").expect("owner_id").column,
            op: Op::Eq,
            value: SqlValue::from(user.id),
        })]
    }
}

ViewSet::for_model(Note::SCHEMA)
    .filter_backend(OwnerFilter)
    .tenant_router("/api/notes")
```

`filter_with` defaults to `filter`, so a backend that does not need the request
— including the plain closure form — implements only `filter` as before.

---

## Mounting

Compose the ViewSet's router into your app. Single-tenant, static pool:

```rust
let api = urls::api()
    .merge(PostViewSet::router("/api/posts", pool.clone()))                          // macro
    .merge(ViewSet::for_model(Author::SCHEMA).router_pool("/api/authors", pool.clone())); // builder
```

Multi-tenant (no pool captured — each request resolves its tenant connection):

```rust
let api = urls::api()
    .merge(ViewSet::for_model(Post::SCHEMA).tenant_router("/api/posts"));
```

`make:api_routes <app>` generates a per-app `api()` that gathers these
`.merge(...)` lines; wire it into your top-level `urls.rs`.

---

## Backend support

- **Builder + `router_pool` / `tenant_router`** is **tri-dialect** — PostgreSQL,
  SQLite and MySQL — and is the recommended path.
- **The derive macro's `router(prefix, PgPool)`** captures a `PgPool` (PostgreSQL).
- **Serializer input + output** now works on **all three backends** (the
  per-row render is tri-dialect; the old PG-only gate is gone).
- Filtering, search, ordering, the three pagination modes, permissions,
  throttling and bulk-create all work across the supported backends on the
  builder path.

---

## Try it

The end-to-end flow above mirrors the compilable `getting_started_blog` example
(Steps 12–13 of the [getting-started guide](getting-started.md)). The
framework's own live tests under `crates/rustango/tests/viewset_*.rs` are the
most complete runnable reference — including the serializer input/output tests.
They run on in-memory SQLite but need the matching feature flags, e.g.:

```bash
cd crates/rustango
cargo test --features sqlite,tenancy --test viewset_serializer_render_sqlite_live
cargo test --features sqlite,tenancy --test viewset_serializer_input_sqlite_live
cargo test --features sqlite,tenancy --test viewset_sqlite_live
```

---

## See also

- [Serializers](serializers.md) — shape the JSON a ViewSet sends and validates.
- [HTML views](html-views.md) — the server-rendered counterpart to this JSON API.
- [OpenAPI](openapi.md) — generate a spec + Swagger UI from your ViewSets.
- [URLs & routing](urls.md) — compose ViewSet routers into your app.
