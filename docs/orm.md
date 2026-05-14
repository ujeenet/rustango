# ORM cookbook

Patterns for the rustango ORM beyond the basics. Most examples assume you already have a `Post` model from `Getting Started`.

## Table of contents

- [Querying](#querying)
- [F() expressions + database functions](#f-expressions--database-functions)
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

## F() expressions + database functions

The ORM Expression DSL — `F()` for column references and the `funcs::*` builders for scalar SQL functions — unlocks three patterns that pure value-based `.set()` / `.where_()` can't express:

### 1. Atomic updates (no read-modify-write race)

The classic counter bug — fetch row → mutate field → save — loses updates under concurrency. `F() + 1` collapses the round-trip into a single `UPDATE` statement so the database takes the row lock:

```rust
use rustango::core::F;

Post::objects()
    .eq("id", post_id)
    .update()
    .set_expr("view_count", F("view_count") + 1_i64)
    .execute(&pool).await?;
```

Tri-dialect: emits `views = ("views" + $1)` on PG, ``views = (`views` + ?)`` on MySQL, identical on SQLite. The arithmetic is parenthesized so nested operations stay unambiguous: `F("a") + F("b") * 2`.

Supported operators: `+ - * / %` plus `& | ^ << >>` (bitwise; XOR on SQLite emits a clear `OpNotSupportedInDialect` since SQLite has no XOR symbol).

### 2. Column-vs-column WHERE filters

`Reservation start_date < end_date` (sanity-check row invariant), `Inventory available > reserved` (find rows with capacity):

```rust
use rustango::core::Column as _;

// `start_date < end_date` for every selected row.
let valid = Reservation::objects()
    .where_(Reservation::start_date.lt_expr(F("end_date")))
    .fetch(&pool).await?;

// Combine with literal predicates.
let oversold = Inventory::objects()
    .where_(Inventory::available.lt_expr(F("reserved")))
    .where_(Inventory::active.eq(true))
    .fetch(&pool).await?;
```

The `*_expr` family — `eq_expr`, `ne_expr`, `lt_expr`, `lte_expr`, `gt_expr`, `gte_expr` — mirrors the literal `eq`, `ne`, … methods but takes any `impl Into<Expr>` on the right side: bare column refs (`F("col")`), arithmetic (`F("price") * 2`), or function results (next section).

### 3. Scalar functions — text, math, NULL handling

`rustango::core::funcs` ships builders for the most-used SQL function set. The 17 v1 functions:

| Group | Builders |
|---|---|
| **Text** | `lower`, `upper`, `length`, `trim`, `ltrim`, `rtrim`, `concat`, `substr`, `replace` |
| **Math** | `abs`, `ceil`, `floor`, `round` (1-arg) / `round_to` (2-arg precision) |
| **NULL** | `coalesce`, `greatest`, `least`, `nullif` |

```rust
use rustango::core::funcs::{lower, upper, concat, coalesce, trim, abs, round};
use rustango::core::F;

// Normalize on write.
User::objects()
    .eq("id", id)
    .update()
    .set_expr("email", lower(trim(F("email"))))
    .execute(&pool).await?;

// Build a derived column from two FKs + a literal.
User::objects()
    .update()
    .set_expr(
        "display_name",
        concat([F("first").into(), " ".into(), F("last").into()]),
    )
    .execute(&pool).await?;

// First non-NULL fallback.
User::objects()
    .update()
    .set_expr(
        "label",
        coalesce([F("nickname").into(), F("username").into(), "anonymous".into()]),
    )
    .execute(&pool).await?;

// Function on the WHERE rhs.
User::objects()
    .where_(User::email_norm.eq_expr(lower(F("email_norm"))))
    .fetch(&pool).await?;

// Functions compose freely — `abs(round(F("score") * 100))` is one Expr.
Player::objects()
    .update()
    .set_expr("score_int", abs(round(F("score") * 100_f64)))
    .execute(&pool).await?;
```

### Tri-dialect behavior

Most functions emit identical SQL across PG / MySQL / SQLite. The divergent shapes are handled per-dialect transparently:

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `concat([a, b])` | `CONCAT(a, b)` | `CONCAT(a, b)` | `(a \|\| b)` |
| `substr(s, 1, 3)` | `SUBSTRING(s FROM 1 FOR 3)` | `SUBSTRING(s, 1, 3)` | `SUBSTR(s, 1, 3)` |
| `greatest([a, b])` | `GREATEST(a, b)` | `GREATEST(a, b)` | `MAX(a, b)` scalar |
| `least([a, b])` | `LEAST(a, b)` | `LEAST(a, b)` | `MIN(a, b)` scalar |

### Variadic builders take `IntoIterator<Item = Expr>`

Rust arrays are homogeneous, so a heterogeneous mix of `F` + `&str` can't infer a common type. Call `.into()` once per element when passing an array literal:

```rust
concat([F("first").into(), " ".into(), F("last").into()])
//          ^^^^^^ each element lifted to Expr
```

Or build a `Vec<Expr>` and pass it directly — same shape, same result.

### Caveats

- **`length` byte-vs-char**: PG returns chars on `TEXT`/`VARCHAR`, MySQL returns **bytes** (use the framework's future `CharLength` builder or wrap in `CHAR_LENGTH` manually if you need cross-dialect char counts).
- **`round(x, n)` on PG**: PG's 2-arg form requires `numeric`, not `double`. Either pass an integer column or cast the float first; MySQL and SQLite accept either type.
- **`greatest([single_arg])` / `least([single_arg])` on SQLite**: not supported — SQLite's `MAX(x)` with one arg is the *aggregate*, not the scalar, form. The writer returns `OpNotSupportedInDialect`. PG and MySQL accept the single-arg form as a no-op returning `x`. Wrap with at least one literal to stay portable.
- **`substr` with negative start**: PG treats negative as "start from char position N" (effectively clamps to 0); MySQL and SQLite treat negative as "count from end". Avoid negative starts in portable code.

### When to reach for a raw SQL escape hatch instead

The function set covers the common case. For features outside v1 — `Cast`, date arithmetic (`Now`, `ExtractYear`, `TruncDate`), full-text search, JSON path operators, hash functions, trig, `Case/When`, `Subquery`/`Exists` — see the [Custom SQL escape hatch](#custom-sql-escape-hatch) section below or wait on issues #3–#7 in the ORM Expression DSL epic, which extend the same `Expr` tree.

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
