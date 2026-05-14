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

### Date / time functions

Issue #3 adds the `Now` / `Extract*` / `Trunc*` family on the same `Expr` machinery. Use them for cohort queries, time-bucket aggregates, and "now()-default-at-write-time" patterns without dragging rows back to the app.

```rust
use rustango::core::funcs::{
    now, trunc_date, trunc_month,
    extract_year, extract_month, extract_weekday,
};
use rustango::core::F;

// 1. Stamp server-side current time on write.
Post::objects()
    .eq("id", id)
    .update()
    .set_expr("published_at", now())
    .execute(&pool).await?;

// 2. Extract year / month / weekday into denormalized indexable
// columns so cohort + day-of-week queries are cheap.
Signup::objects()
    .update()
    .set_expr("bucket_year", extract_year(F("created_at")))
    .set_expr("bucket_month", extract_month(F("created_at")))
    .set_expr("weekday", extract_weekday(F("created_at")))
    .execute(&pool).await?;

// 3. Filter on the stored bucket — typed integer comparison, uses
// the index, portable across all three dialects.
let friday_signups = Signup::objects()
    .where_(Signup::weekday.eq(5_i64))            // 5 = Friday (0=Sun)
    .fetch(&pool).await?;

// 4. For range filters where you'd be tempted to write
// `created_at >= trunc_year(now())` directly: don't. The function
// builders for `Trunc*` return text on MySQL/SQLite (see caveats
// below), so a column-vs-trunc comparison in WHERE only behaves
// well on PG. Compute the boundary in Rust instead and pass it as a
// typed literal — works the same on every backend and uses the
// index on `created_at`:
use chrono::{Datelike, TimeZone};
let this_year = chrono::Utc::now().year();
let year_start = chrono::Utc.with_ymd_and_hms(this_year, 1, 1, 0, 0, 0).unwrap();

let recent = Order::objects()
    .where_(Order::created_at.gte(year_start))
    .fetch(&pool).await?;

// 5. `Trunc*` shines on the *write* side. `trunc_date` is the
// one trunc-family builder with identical SQL on every dialect
// (`DATE(x)`) — handy for grouping by day without the type-divergence
// caveat the year/month variants carry.
Order::objects()
    .update()
    .set_expr("day_bucket", trunc_date(F("created_at")))     // DATE column on every backend
    .set_expr("month_bucket", trunc_month(F("created_at")))  // see caveat
    .execute(&pool).await?;
// `month_bucket` should be `TIMESTAMPTZ` on PG and `VARCHAR(10)` /
// `TEXT` on MySQL/SQLite — parse client-side when reading if you
// need a typed `chrono::NaiveDate`.
```

**Per-dialect emission:**

| Builder | PG | MySQL | SQLite |
|---|---|---|---|
| `now()` | `NOW()` | `NOW()` | `CURRENT_TIMESTAMP` |
| `extract_year(x)` | `CAST(EXTRACT(YEAR FROM x) AS INTEGER)` | `YEAR(x)` | `CAST(strftime('%Y', x) AS INTEGER)` |
| `extract_week(x)` ⚠ | `EXTRACT(WEEK FROM x)` — ISO 8601, range 1–53 | `WEEK(x)` — Sunday-start, range **0**–53 | `strftime('%W', x)` — Monday-start, range 00–53 |
| `extract_weekday(x)` | `CAST(EXTRACT(DOW FROM x) AS INTEGER)` | `(DAYOFWEEK(x) - 1)` | `CAST(strftime('%w', x) AS INTEGER)` |
| `extract_quarter(x)` | `EXTRACT(QUARTER FROM x)` | `QUARTER(x)` | **unsupported** — error |
| `trunc_date(x)` | `DATE(x)` | `DATE(x)` | `DATE(x)` |
| `trunc_year(x)` | `DATE_TRUNC('year', x)` → timestamp | `DATE_FORMAT(x, '%Y-01-01')` → **string** | `strftime('%Y-01-01', x)` → **string** |
| `trunc_month(x)` | `DATE_TRUNC('month', x)` → timestamp | `DATE_FORMAT(x, '%Y-%m-01')` → **string** | `strftime('%Y-%m-01', x)` → **string** |
| `trunc_day(x)` | `DATE_TRUNC('day', x)` → timestamp | `DATE(x)` → date | `date(x)` → text |

**Caveats specific to date/time:**

- **`trunc_year/month` return type diverges**: timestamp on PG, text on MySQL/SQLite. Cast on the app side when reading if you need a typed `chrono::NaiveDate` — or store the bucket as a plain integer (`extract_year` + `extract_month`) and reconstruct in code.
- **`extract_weekday` is normalized to 0 = Sunday** across all three dialects. MySQL's native `DAYOFWEEK()` returns 1=Sunday, so the writer subtracts 1.
- **⚠ `extract_week` is NOT portable.** PG returns ISO 8601 week numbers (Monday-start, range 1–53); MySQL's default `WEEK(x)` is Sunday-start with range **0**–53; SQLite's `strftime('%W')` is Monday-start with range 00–53. For 2024-01-01 (a Monday), the three backends return `1`, `0`, and `01` respectively. Single-backend code can use it freely; cross-dialect code should compute the week boundary as a typed `chrono::DateTime` in Rust and filter on the timestamp column instead.
- **`extract_quarter` on SQLite errors** with `OpNotSupportedInDialect` — SQLite has no native quarter token. Either gate the feature behind `cfg(not(sqlite))` or compute via `((extract_month - 1) / 3) + 1` in app code.
- **Time-zone handling**: PG `EXTRACT` operates in the column's timezone; MySQL `YEAR()` operates in the session timezone (`SET time_zone = ...`); SQLite has no real TZ support — treat everything as UTC. Use `TIMESTAMPTZ` on PG, `DATETIME` on MySQL with the session TZ set, ISO-8601 strings on SQLite.

### Conditional expressions — `case() / when() / value()`

Issue #4 adds Django-shape `CASE WHEN … THEN … ELSE … END` on the same `Expr` machinery. Use it for custom orderings, derived columns in `annotate`, computed defaults in `update`, and (combined with `Sum`) conditional aggregates.

```rust
use rustango::core::case::{case, value};
use rustango::core::{Column as _, F};
use rustango::core::funcs::lower;

// Custom ordering — published posts first, drafts last.
Post::objects()
    .update()
    .set_expr(
        "priority",
        case()
            .when(Post::status.eq("published"), 0_i64)
            .when(Post::status.eq("review"), 1_i64)
            .when(Post::status.eq("draft"), 2_i64)
            .default(99_i64),
    )
    .execute(&pool).await?;

let ordered = Post::objects()
    .order_by(&[("priority", false), ("id", false)])
    .fetch(&pool).await?;

// Computed default on update — drafts get a lowercased title for
// the label, everything else uses the title verbatim.
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(Post::status.eq("draft"), lower(F("title")))
            .default(F("title")),
    )
    .execute(&pool).await?;

// AND / OR composition in the WHEN predicate.
let viral = Post::status.eq("published").and(Post::views.gt(1_000_i64));
Post::objects()
    .update()
    .set_expr(
        "label",
        case()
            .when(viral, value("viral"))
            .when(Post::status.eq("published"), value("live"))
            .default(value("pending")),
    )
    .execute(&pool).await?;
```

**Builder shape:**

- `case()` — start a builder.
- `.when(condition, then)` — append a branch. `condition` is anything `Into<WhereExpr>` (typically `Column::eq()`, `.and()`, `.or()`); `then` is anything `Into<Expr>` (literal, `F()`, function call, nested `case()`).
- `.default(expr)` — set the optional `ELSE` branch. Omitting it produces a `CASE` that returns `NULL` for unmatched rows (SQL standard).
- `.build()` or `.into()` — finalize into an `Expr` for `set_expr` / `eq_expr` / `annotate`.
- `value(literal)` — Django-style sugar for `Expr::Literal(...)`. Optional — bare literals coerce via `Into<Expr>`, but `value("…")` reads explicitly as "this is a string literal, not a column ref".

**Tri-dialect emission:**

`CASE WHEN … THEN … [ELSE …] END` is SQL-92 standard — emitted identically across PG, MySQL, and SQLite. No dialect dispatch in the writer.

**Caveats:**

- **Empty branches**: `case().build()` with no `.when(...)` calls is rejected at emit time with `SqlError::EmptyCaseBranches`. SQL requires at least one `WHEN` clause. An empty `WHEN` condition (e.g. `WhereExpr::And(vec![])`) is rejected with `SqlError::EmptyCaseWhenCondition` for the same reason.
- **Type unification across branches**: every dialect picks a common type from the `THEN` and `ELSE` values. Mixing types (`THEN 1_i64` + `ELSE "string"`) can throw a runtime cast error or coerce surprisingly. Stick to one type per `CASE`.
- **Performance**: each row evaluates `WHEN` predicates in order until one matches (first-match-wins, per row). Cost grows with the number of branches and the cost of the predicates. For many fixed-string mappings, a join against a small lookup table can be cheaper and more readable.

### Subqueries — `exists() / not_exists() / in_subquery() / subquery() / outer_ref()`

Issue #5 adds Django-shape subquery primitives on the same `Expr` machinery. Four builders cover the bulk of "embed one query inside another":

| Builder | Shape | Use it for |
|---|---|---|
| `exists(qs)` | `EXISTS (SELECT … FROM …)` | "Authors who have at least one book" |
| `not_exists(qs)` | `NOT EXISTS (SELECT …)` | "Authors with no books" (anti-join) |
| `in_subquery(col, qs)` | `<col> IN (SELECT …)` | "Posts in any public category" |
| `not_in_subquery(col, qs)` | `<col> NOT IN (SELECT …)` | Inverse of the above |
| `subquery(qs)` | `(SELECT …)` as a scalar | Computed default in `set_expr` |
| `outer_ref(col)` | `"<outer_table>"."<col>"` | Reference outer row from inside any of the above |

```rust
use rustango::core::subquery::{exists, not_exists, in_subquery, outer_ref};
use rustango::core::{Column as _, WhereExpr};

// "Authors with no books" — the canonical anti-join. Build the inner
// queryset first so its compile() catches typos; embed via not_exists.
let no_books = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let orphans = Author::objects()
    .where_raw(not_exists(no_books))
    .fetch(&pool).await?;

// "Authors who have a published book of more than 100 pages" — the
// inner predicate combines a correlation (outer_ref) with literal
// filters in the same WHERE.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .where_(Book::status.eq("published"))
    .where_(Book::pages.gt(100_i64))
    .compile()?;
let long_writers = Author::objects()
    .where_raw(exists(inner))
    .fetch(&pool).await?;

// Compose EXISTS with an OR.
let inner = Book::objects()
    .where_(Book::author_id.eq_expr(outer_ref("id")))
    .compile()?;
let featured = Author::objects()
    .where_raw(WhereExpr::Or(vec![
        Author::name.eq("Carol").into(),
        exists(inner),
    ]))
    .fetch(&pool).await?;
```

**Nested correlation works.** OuterRef inside a doubly-nested subquery resolves to the *immediate* enclosing scope — the writer maintains a scope stack as it descends, so `EXISTS (Book WHERE id = outer.id AND EXISTS (Comment WHERE book_id = outer.id))` resolves the inner `outer.id` to `Book.id`, not to the outermost `Author.id`. Use `outer_ref(...)` twice if you really do need to reach two scopes up.

**Errors:**

- **`OuterRefOutsideSubquery`** — emitting `outer_ref("col")` at the top level (not inside any subquery wrapper) is a programming error. The writer raises this loudly with the column name so the call site is easy to find.

**Caveats:**

- **`IN (SELECT …)` projection narrowing**: PG strictly requires the inner SELECT to project exactly one column for the `<col> IN (…)` form. rustango doesn't ship `.values("col")`-style projection narrowing yet (issue #62), so the inner queryset always projects every model column — which makes `in_subquery` only work today against tables whose model has a single column. For the multi-column case, reach for `exists(inner.where_(<outer col>.eq_expr(outer_ref(...))))` — it has the same semantics and doesn't depend on the projection shape.
- **Scalar `subquery(...)` requires a one-column-one-row inner**: the SQL emitted is `SET col = (SELECT …)` — if the inner produces more than one row, the database errors at runtime. Constrain via `.limit(1)` and either narrow projection (once it lands) or design the inner around a uniqueness invariant.
- **Subquery compile-time validation lives on the inner queryset**: column typos surface at the inner `queryset.compile()?` call, not at the outer query's `compile()`. Build the inner first and propagate `?`.

### When to reach for a raw SQL escape hatch instead

The function set covers the common case. For features outside v1–v5 — `Cast`, full-text search, JSON path operators, hash functions, trig, window functions — see the [Custom SQL escape hatch](#custom-sql-escape-hatch) section below or wait on issues #6–#7 in the ORM Expression DSL epic, which extend the same `Expr` tree.

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

### Ad-hoc joins — `.join(Join { … })`

When the join you need isn't FK-driven (custom predicate, non-equi join, INNER instead of LEFT, self-join, joining on a non-PK column), reach for `QuerySet::join`. The `Join` struct accepts any [`WhereExpr`] as its `on` predicate, so `and()` / `or()` / `Not` / function calls / column-vs-column / literal filters all compose freely.

```rust
use rustango::core::joins::aliased;
use rustango::core::{Join, JoinKind, Op, WhereExpr};

// "Posts that have at least one APPROVED comment" — INNER JOIN with
// an extra predicate inside the ON. Posts with no approved comment
// drop out; LEFT JOIN would keep them.
Post::objects()
    .join(Join {
        target: Comment::SCHEMA,
        alias: "c",
        kind: JoinKind::Inner,
        on: WhereExpr::And(vec![
            // Column-on-column condition — both sides aliased.
            WhereExpr::ExprCompare {
                lhs: aliased("c", "post_id"),
                op: Op::Eq,
                rhs: aliased("post", "id"),
            },
            // Bare Filter — unqualified columns inside `on` resolve
            // to the joined alias ("c"), so this becomes
            // `"c"."is_approved" = $N`.
            Comment::is_approved.eq(true).into(),
        ]),
        project: vec![],
    })
    .fetch(&pool).await?;
```

**Column qualification rules inside `on`:**

- **Bare `Filter` / `ColumnFilter` columns + `F()` column refs** resolve to the joined alias (`<alias>` you passed). That's the natural reading because most of an ON predicate is about the joined table.
- **`aliased(alias, col)`** emits `"<alias>"."<col>"` explicitly — use this for cross-references back to the outer table (`aliased("<outer_table>", "<col>")`) or to a previously joined alias.
- **`WhereExpr::ExprCompare { lhs, op, rhs }`** is the right shape for column-vs-column comparisons across tables, since both sides take any `Expr`.

> ⚠️ **DANGEROUS PATTERN — typed filters from the OUTER model inside `on`.**
> `Post::status.eq("draft").into()` produces a `WhereExpr::Predicate(Filter { column: "status", ... })` and **drops the `Post` model tag** at the `Into<WhereExpr>` boundary. The auto-qualification rule above then misroutes that filter to the **joined alias**, not to `Post`. You get `"<joined_alias>"."status" = $N` — wrong table — and the compiler can't catch it. **Use [`joins::col_filter`] for predicates against any column whose table isn't the join's default alias:**
>
> ```rust
> use rustango::core::joins::{aliased, col_filter};
> use rustango::core::Op;
>
> // SAFE: explicit alias on the LHS.
> col_filter("post", "status", Op::Eq, "draft")
> ```
>
> Reserve bare typed filters (`Comment::is_approved.eq(true).into()`) for columns on the JOINED model only — never for outer-model columns.

**JoinKind tri-dialect support:**

| Kind | PG | MySQL | SQLite |
|---|---|---|---|
| `Inner` | ✓ | ✓ | ✓ |
| `Left` (default) | ✓ | ✓ | ✓ |
| `Right` | ✓ | ✓ | ✗ `SqlError::JoinKindNotSupported` |
| `Full` | ✓ | ✗ | ✗ |

`Right` is easy to work around — swap operands and use `Left`. `Full` on MySQL is usually emulated with `(LEFT JOIN) UNION (RIGHT JOIN)` if you really need it.

**Other emit-time errors:**

- **Empty `on` predicate** (`WhereExpr::And(vec![])` or no `ExprCompare`s) is rejected with `SqlError::EmptyJoinOnCondition`. SQL requires at least one boolean predicate inside `ON`; the auto-`true` shorthand from top-level WHERE doesn't apply here.

**`project` is currently dead data on ad-hoc joins.**

The `Join.project` field tells the writer to emit `<alias>"."<col>" AS "<alias>__<col>"` columns in the SELECT list. Today only `select_related` actually decodes those (via the FK-target's full-row decoder); ad-hoc joins emit the columns but the `Vec<MainModel>` decoder ignores them, so populating `project` on an ad-hoc join just adds bytes to the wire. Leave it as `vec![]` until projection-narrowing + tuple-decoding land.

**When to reach for ad-hoc joins:**

| Need | Tool |
|---|---|
| Pull related rows along with the main row | `select_related` (Django shape) |
| Filter main rows by a related-table predicate | `exists(...)` / `not_exists(...)` |
| Filter via INNER instead of LEFT, or with extra ON predicates | `.join(...)` |
| Self-join (e.g. `employee.manager_id = manager.id`) | `.join(...)` |
| Anti-join (rows in A with NO match in B) | `not_exists(...)` |

`select_related` stays the right tool when the join is "follow this FK and project all its columns." Ad-hoc joins are the escape hatch when you need: non-FK join key, INNER instead of LEFT, an extra predicate inside the ON, or a self-join.

[`joins::col_filter`]: https://docs.rs/rustango/latest/rustango/core/joins/fn.col_filter.html

[`WhereExpr`]: https://docs.rs/rustango/latest/rustango/core/enum.WhereExpr.html

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
