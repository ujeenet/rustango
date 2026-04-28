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
use rustango::{Model, admin, migrate};
use rustango::core::Column as _;
use rustango::sql::{Fetcher, sqlx::PgPool};

#[derive(Model, Debug, Clone)]
#[rustango(table = "user", display = "username")]
struct User {
    #[rustango(primary_key)]
    id: i64,
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

```rust
use rustango::migrate;

// Bootstrap a fresh schema.
migrate::apply_all(&pool).await?;

// v0.2: snapshot + diff.
let prev: migrate::SchemaSnapshot =
    serde_json::from_str(&std::fs::read_to_string("migrations/0001.json")?)?;
let current = migrate::SchemaSnapshot::from_registry();
let changes = migrate::detect_changes(&prev, &current);
let ddl = migrate::render_changes(&changes, &current)?;
for stmt in ddl {
    sqlx::query(&stmt).execute(&pool).await?;
}
```

What the diff covers in v0.2: new/dropped tables, new/dropped columns,
FK constraints. Type changes, constraint changes, and renames are
deliberately deferred — renames in particular need explicit `Rename`
operations à la Django (snapshot diffs can't tell rename from drop+add).

## Status

This is a hobbyist project. The shape is novel for Rust ORMs (registry-
driven admin, Django-style API), the test count is high (~200 unit +
live integration), and the demo works in a real browser. It is
**not** production-ready: there is no per-user auth, no SQLite/MySQL,
no streaming queries, and no benchmarks against the mature alternatives.
For real workloads today, use [Diesel](https://diesel.rs) or
[SeaORM](https://www.sea-ql.org/SeaORM/).

If you want a Django-shaped admin in Rust, this is the only thing that
exists.

## License

MIT OR Apache-2.0.
