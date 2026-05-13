# ORM cookbook

Patterns for the rustango ORM beyond the basics. Most examples assume you already have a `Post` model from `Getting Started`.

## Table of contents

- [Querying](#querying)
- [Aggregations](#aggregations)
- [Joins + select_related](#joins--select_related)
- [Bulk operations](#bulk-operations)
- [Upsert (ON CONFLICT)](#upsert-on-conflict)
- [Transactions](#transactions)
- [Many-to-many](#many-to-many)
- [JSON / JSONB](#json--jsonb)
- [Soft delete](#soft-delete)
- [Audit trail](#audit-trail)
- [Custom SQL escape hatch](#custom-sql-escape-hatch)
- [Lazy FK loading](#lazy-fk-loading)
- [QuerySet vs string-keyed filters](#queryset-vs-string-keyed-filters)
- [Tenant-scoped queries](#tenant-scoped-queries)
- [Performance tips](#performance-tips)

---

## Querying

```rust
use rustango::core::Column as _;
use rustango::sql::Fetcher as _;

// Simplest — fetch all
let posts = Post::objects().fetch(&pool).await?;

// Single equality filter
let drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .fetch(&pool).await?;

// Chained filters (AND)
let recent_drafts = Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::author_id.eq(42))
    .where_(Post::deleted_at.is_null())
    .order_by(Post::created_at, true)        // true = DESC
    .limit(20)
    .fetch(&pool).await?;

// String-keyed filter (validated at compile of the queryset)
let by_id = Post::objects()
    .filter("id", Op::Eq, SqlValue::I64(42))
    .fetch(&pool).await?;

// OR / nested
use rustango::core::WhereExpr;
let qs = Post::objects().where_expr(WhereExpr::Or(vec![
    Post::status.eq("draft").into(),
    Post::status.eq("review").into(),
]));
```

### Lookup operators

```rust
Post::objects().where_(Post::view_count.gt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.gte(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lt(100)).fetch(&pool).await?;
Post::objects().where_(Post::view_count.lte(100)).fetch(&pool).await?;
Post::objects().where_(Post::status.ne("archived")).fetch(&pool).await?;
Post::objects().where_(Post::id.in_(&[1, 2, 3])).fetch(&pool).await?;
Post::objects().where_(Post::status.not_in(&["draft", "deleted"])).fetch(&pool).await?;
Post::objects().where_(Post::title.like("%draft%")).fetch(&pool).await?;          // case-sensitive contains
Post::objects().where_(Post::title.ilike("%draft%")).fetch(&pool).await?;         // case-insensitive contains
Post::objects().where_(Post::title.ilike("Hello%")).fetch(&pool).await?;          // case-insensitive starts-with
Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
Post::objects().where_(Post::published_at.between(start, end)).fetch(&pool).await?;
```

### Pagination

```rust
// Page-number
let page = Post::objects().page(2, 50).fetch(&pool).await?;

// Cursor (manual — no auto-next-token from QuerySet)
let next = Post::objects()
    .where_(Post::id.gt(last_id))
    .order_by(Post::id, false)
    .limit(50)
    .fetch(&pool).await?;
```

For HTTP-side cursor pagination, use `ViewSet::cursor_pagination("id")` instead.

---

## Aggregations

```rust
use rustango::sql::Counter;

// COUNT
let n = Post::objects()
    .where_(Post::status.eq("published"))
    .count(&pool).await?;

// SUM / AVG / MIN / MAX
let total_views = Post::objects().sum::<i64>(Post::view_count, &pool).await?;
let avg_views = Post::objects().avg(Post::view_count, &pool).await?;
let max_views = Post::objects().max::<i64>(Post::view_count, &pool).await?;

// Annotate (per-row aggregation)
use rustango::core::AggregateExpr;
let counts = Author::objects()
    .annotate("post_count", AggregateExpr::CountChildren("posts", "author_id"))
    .fetch(&pool).await?;
// each Author gets a `post_count` extra field accessible via the annotated row
```

---

## Joins + select_related

Pre-load FK targets in one query (avoids N+1):

```rust
let posts = Post::objects()
    .select_related("author")              // JOIN posts.author -> authors.id
    .fetch(&pool).await?;

for post in &posts {
    let author = post.author.value().unwrap();   // already loaded, no DB round-trip
    println!("{} by {}", post.title, author.name);
}
```

`select_related` resolves FK fields at compile-of-queryset time. The `ForeignKey<T>` field on the parent goes from `Unloaded(pk)` to `Loaded(pk, T)`.

For reverse FKs (parent.children), use the macro-generated `_set` method:

```rust
let author_posts = author.post_set(&pool).await?;
```

---

## Bulk operations

```rust
// Bulk INSERT
Post::bulk_insert_on(&pool, vec![p1, p2, p3]).await?;

// Bulk UPDATE — applies the same set to every matched row
use rustango::sql::Updater as _;
Post::objects()
    .where_(Post::status.eq("draft"))
    .where_(Post::created_at.lt(thirty_days_ago))
    .update()
    .set(Post::status, "archived")
    .execute_on(&pool).await?;

// Bulk DELETE
use rustango::sql::Deleter as _;
Post::objects()
    .where_(Post::deleted_at.is_not_null())
    .delete_on(&pool).await?;
```

---

## Upsert (ON CONFLICT)

```rust
// Upsert by `external_id` — INSERT, or UPDATE if external_id collides
post.upsert_on(&pool, &["external_id"]).await?;

// Upsert by composite key
post.upsert_on(&pool, &["author_id", "slug"]).await?;
```

The macro generates `ON CONFLICT (col1, col2) DO UPDATE SET ...` with every non-PK, non-`auto_now_add` column.

---

## Transactions

```rust
use rustango::sql::transaction;

transaction(&pool, |conn| async move {
    let mut a = Account::objects().get_on(&mut *conn, 1).await?;
    let mut b = Account::objects().get_on(&mut *conn, 2).await?;
    a.balance -= 100;
    b.balance += 100;
    a.save_on(&mut *conn).await?;
    b.save_on(&mut *conn).await?;
    Ok::<(), rustango::sql::ExecError>(())
}).await?;
```

If the closure returns `Err`, the transaction rolls back. Nested `transaction` calls reuse the outer one (savepoint-style).

---

## Many-to-many

Declare on the model:

```rust
#[rustango(
    table = "posts",
    m2m(name = "tags", to = "tags", through = "post_tags",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post { ... }
```

Use the auto-generated accessor:

```rust
let tag_ids: Vec<i64> = post.tags_m2m().all(&pool).await?;
post.tags_m2m().add(42, &pool).await?;
post.tags_m2m().remove(42, &pool).await?;
post.tags_m2m().set(&[1, 2, 3], &pool).await?;        // replace all
post.tags_m2m().clear(&pool).await?;
let has = post.tags_m2m().contains(42, &pool).await?;
```

Junction table (`post_tags`) is auto-created by `make_migrations` with composite PK + two FKs `ON DELETE CASCADE`. Currently the junction has only the two FK columns — for extra columns (added_by, order, created_at) you'll define a separate Model and traverse manually until "custom through model" ships.

---

## JSON / JSONB

Declare the field as `serde_json::Value`:

```rust
#[derive(Model)]
pub struct Event {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(default = r#"'{}'::jsonb"#)]
    pub data: serde_json::Value,
}
```

Query JSON contents:

```rust
use rustango::core::Op;

let with_email = Event::objects()
    .where_(Event::data.json_contains(serde_json::json!({"email_set": true})))
    .fetch(&pool).await?;

// Path extract
let typed = Event::objects()
    .where_(Event::data.filter("$.type", Op::Eq, "user.created"))
    .fetch(&pool).await?;
```

Read/write Rust types via `serde_json::from_value` / `to_value`.

---

## Soft delete

Mark a column with `#[rustango(soft_delete)]`:

```rust
#[derive(Model)]
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub title: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<DateTime<Utc>>,
}
```

Use:

```rust
post.soft_delete_on(&pool).await?;     // sets deleted_at = NOW()
post.restore_on(&pool).await?;          // sets deleted_at = NULL

// Default queries DO include soft-deleted rows. Filter explicitly:
let live = Post::objects().where_(Post::deleted_at.is_null()).fetch(&pool).await?;
```

The admin's "Delete" button auto-routes to `soft_delete_on` for any model that has the column. The auto-filter (default exclusion) is on the v0.21 roadmap.

---

## Audit trail

Annotate the model:

```rust
#[derive(Model)]
#[rustango(audit(track = "title, body, status"))]
pub struct Post { ... }
```

Every save/delete writes a row to `rustango_audit_log` with a `before / after` JSONB diff for the listed fields. Set the source per-request:

```rust
use rustango::audit::{with_source, AuditSource};

with_source(
    AuditSource::User { id: user_id.to_string() },
    async {
        post.save_on(&pool).await
    },
).await?;
```

The admin's per-row history panel reads from this table; the cross-model feed is at `/__audit`.

Cleanup:

```rust
rustango::audit::cleanup_older_than(&pool, 90).await?;       // delete > 90 days
rustango::audit::cleanup_keep_last_n(&pool, 50).await?;      // keep most recent 50/row

// CLI
manage audit-cleanup --days 90
manage audit-cleanup --keep-last 50 --tenant acme
```

---

## Custom SQL escape hatch

For things the QuerySet doesn't express:

```rust
use rustango::sql::sqlx;

// Raw query → typed rows
let rows = sqlx::query_as::<_, (i64, String)>("SELECT id, title FROM posts WHERE views > $1 ORDER BY views DESC")
    .bind(1000)
    .fetch_all(&pool)
    .await?;

// Raw with model decoding
let posts: Vec<Post> = sqlx::query_as::<_, Post>("SELECT * FROM posts WHERE complicated_condition")
    .fetch_all(&pool)
    .await?;

// Raw without rows (DDL / DML)
sqlx::query("REINDEX TABLE posts").execute(&pool).await?;
```

For programmatic raw SQL within the rustango query layer:

```rust
use rustango::sql::raw_query;

let count = raw_query::<i64>(&pool, "SELECT COUNT(*) FROM posts WHERE complicated").await?;
```

---

## Lazy FK loading

```rust
let post = Post::objects().get(&pool, 1).await?;

// FK starts Unloaded — just the PK
match &post.author {
    ForeignKey::Unloaded(pk) => println!("author id = {pk}"),
    ForeignKey::Loaded(pk, author) => println!("author = {}", author.name),
}

// Force-load
let author = post.author.get(&pool).await?;          // fetches if Unloaded
```

Use `select_related("author")` on the queryset to pre-load a batch.

---

## QuerySet vs string-keyed filters

Three syntaxes — pick by context:

```rust
// 1. HTTP query string (set via ViewSet filter_fields)
//    GET /api/posts?author_id=42&status__ne=archived

// 2. String-keyed (validated at compile of the queryset; runtime field lookup)
Post::objects().filter("author_id", Op::Eq, SqlValue::I64(42));

// 3. Typed columns (compile-time field check; preferred in app code)
Post::objects().where_(Post::author_id.eq(42));
```

**Convention:** typed in app code, string-keyed in admin / generic CRUD code, HTTP query for the public API surface.

---

## Tenant-scoped queries

Multi-tenant projects acquire a per-request connection from `TenantPools`:

```rust
use rustango::extractors::Tenant;

async fn handler(mut t: Tenant) -> Result<...> {
    let mut conn = t.acquire().await?;
    let posts = Post::objects().fetch_on(&mut *conn).await?;
    Ok(...)
}
```

`fetch_on` works with any `sqlx::Executor`; `fetch` is sugar for `fetch_on(&pool)`.

---

## Performance tips

- **Always use indexes for `WHERE` and `ORDER BY` columns.** Declare via `#[rustango(index)]` so they're in migrations.
- **`select_related` for FK display in lists** — eliminates N+1 in admin/list views.
- **`page` instead of `fetch().drain()`** — never load entire tables.
- **Cursor pagination for huge tables** — skips `COUNT(*)` per page.
- **`bulk_insert_on` for batches** — single round trip vs N.
- **`upsert_on` for idempotent imports** — `ON CONFLICT` is faster than SELECT-then-INSERT.
- **`transaction` for related writes** — reduces commit overhead and keeps consistency.
- **Cache hot reads** with `cache::get_or_set` — invalidate on `connect_post_save<T>(...)` signal handler.
