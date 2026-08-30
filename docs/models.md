# Models

A model is a Rust struct that maps to a database table. Add `#[derive(Model)]`,
annotate the fields, and **Rustango** generates the schema, a type-safe query
entry point, and `save`/`find`/`delete` methods — Django's models or Laravel's
Eloquent, with the compiler checking your columns. This is the **declaration**
reference: every field type, every primary-key option, and every
`#[rustango(...)]` attribute. For *querying* models once they're declared, see
the [ORM cookbook](orm.md).

[![Models in Rustango: a #[derive(Model)] struct maps Rust field types to per-dialect columns, the primary key can be an auto-increment Auto<i64> or a custom application-assigned key, and the derive generates SCHEMA + objects() + save/find](img/models.png)](img/models.png)

> **New to a term here?** *model*, *primary key*, *foreign key*, *migration*,
> *nullable* — see the [glossary](glossary.md).

> **Source:** `rustango::Model` (`#[derive(Model)]`), `rustango::core`
> (`Model` trait, `ModelSchema`, `FieldType`, `Auto`, `ForeignKey`), and the
> dialect type mappings in `rustango::sql::{dialect, mysql, sqlite}` — always
> compiled (pick a backend feature: `postgres` / `mysql` / `sqlite`).
>
> **Runnable version:** the field-type round-trips, custom-PK, and SCHEMA
> snippets are copied from
> [`models_doc.rs`](https://github.com/ujeenet/rustango/blob/main/crates/rustango/tests/models_doc.rs)
> (`cargo test -p rustango --features sqlite --test models_doc`).

## Table of contents

- [Anatomy of a model](#anatomy-of-a-model)
- [Field types](#field-types) · [PostgreSQL-only types](#postgresql-only-types)
- [Primary keys](#primary-keys) — [custom PKs](#custom-primary-keys) · [composite](#composite-primary-keys)
- [Relationships](#relationships)
- [Common field attributes](#common-field-attributes)
- [Indexes & constraints](#indexes-and-constraints)
- [Common model attributes](#common-model-attributes)
- [The generated API](#the-generated-api) — [save vs insert](#save-vs-insert)
- [Full attribute reference](#full-attribute-reference)
- [See also](#see-also)

---

## Anatomy of a model

```rust
use rustango::{Auto, Model};
use chrono::{DateTime, Utc};

#[derive(Model, Clone, Debug)]
#[rustango(table = "posts", display = "title")]   // model-level attributes
pub struct Post {
    #[rustango(primary_key)]
    pub id: Auto<i64>,                            // field-level attributes

    #[rustango(max_length = 200)]
    pub title: String,

    pub body: String,

    #[rustango(fk = "authors", on = "id")]
    pub author_id: i64,

    #[rustango(auto_now_add)]
    pub created_at: Auto<DateTime<Utc>>,
}
```

From that one declaration the derive generates:

- the **schema** (`Post::SCHEMA` — table name, columns, types, the PK) that
  migrations and the admin read;
- a query entry point, **`Post::objects()`**, returning a `QuerySet<Post>`;
- **typed field constants** (`Post::title`, `Post::author_id`) for compile-checked
  filters — `Post::objects().where_(Post::author_id.eq(42))`;
- row methods — **`save`**, **`find`**, **`delete`**, and more (see
  [the generated API](#the-generated-api)).

The table name defaults to the model name if you omit `table`; column names
default to the snake-cased field name unless you set `column`.

---

## Field types

The field's Rust type determines its database column type. Rustango maps each
type per dialect, so the same model works on PostgreSQL, MySQL, and SQLite:

| Rust type | PostgreSQL | MySQL | SQLite |
|---|---|---|---|
| `i16` | `SMALLINT` | `SMALLINT` | `INTEGER` |
| `i32` | `INTEGER` | `INT` | `INTEGER` |
| `i64` | `BIGINT` | `BIGINT` | `INTEGER` |
| `f32` | `REAL` | `FLOAT` | `REAL` |
| `f64` | `DOUBLE PRECISION` | `DOUBLE` | `REAL` |
| `bool` | `BOOLEAN` | `TINYINT(1)` | `INTEGER` (0/1) |
| `String` | `TEXT` | `TEXT` | `TEXT` |
| `String` + `max_length = N` | `VARCHAR(N)` | `VARCHAR(N)` | `TEXT` |
| `chrono::DateTime<Utc>` | `TIMESTAMPTZ` | `DATETIME(6)` | `TEXT` (ISO-8601) |
| `chrono::NaiveDate` | `DATE` | `DATE` | `TEXT` |
| `chrono::NaiveTime` | `TIME` | `TIME(6)` | `TEXT` |
| `uuid::Uuid` | `UUID` | `CHAR(36)` | `TEXT` |
| `serde_json::Value` | `JSONB` | `JSON` | `TEXT` |
| `rust_decimal::Decimal` | `NUMERIC` | `DECIMAL(38,10)` | `NUMERIC` |
| `Vec<u8>` | `BYTEA` | `LONGBLOB` | `BLOB` |
| `Option<T>` | `T NULL` | `T NULL` | `T` (nullable) |

`Option<T>` is how you make a column **nullable** — a non-`Option` field is
`NOT NULL`. All of these round-trip through `save` → `find`, verified end to
end:

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "gadget")]
pub struct Gadget {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 100)]
    pub name: String,
    pub qty: i64,
    pub active: bool,
    pub note: Option<String>,        // nullable
    pub made_at: DateTime<Utc>,
    pub meta: serde_json::Value,     // JSON
}
```

> **Decimal precision.** PostgreSQL `NUMERIC` is arbitrary-precision; MySQL uses
> `DECIMAL(38,10)` (38 digits, 10 fractional — the widest portable fit); SQLite
> uses `NUMERIC` affinity. Use `rust_decimal::Decimal` for money, never `f64`.

### PostgreSQL-only types

These map to native PostgreSQL column types and have **no MySQL/SQLite
equivalent** — the migration writer emits `TEXT` there to stay valid, but
reading/writing them errors at runtime on those backends. Use them only in
PostgreSQL deployments:

| Rust type | PostgreSQL | Notes |
|---|---|---|
| `Array<T>` | `text[]` / `integer[]` / `bigint[]` | native arrays |
| `Range<T>` | `int4range` / `int8range` / `numrange` / `daterange` / `tstzrange` | range types |
| `HStore` | `hstore` | flat string→string map (needs the extension) |
| `Vector` + `#[rustango(vector(dims = N))]` | `vector(N)` | pgvector embeddings |
| `Point` + `#[rustango(geometry(srid = N))]` | `geometry(Point, N)` | PostGIS |

---

## Primary keys

Every model needs a primary key. Mark one field `#[rustango(primary_key)]`; if
you mark none, the schema looks for a column named `id`.

The **default and most common** PK is an auto-incrementing 64-bit integer,
declared as `Auto<i64>`:

```rust
#[rustango(primary_key)]
pub id: Auto<i64>,
```

**`Auto<T>` semantics.** An `Auto<T>` field is either `Unset` (the value the DB
will assign) or `Set(v)`. On insert, an `Unset` PK is omitted from the column
list so the database generates it, then the value is read back (`RETURNING` on
PostgreSQL/SQLite, `LAST_INSERT_ID()` on MySQL) and stored on your struct:

```rust
let mut g = Gadget { id: Auto::default(), /* … */ };   // Unset
g.save_pool(&pool).await?;                              // DB assigns the id
let new_id = g.id.get().copied().unwrap();             // now populated
```

Supported `Auto<T>` inner types are `i32`, `i64`, and `Uuid`.

### Custom primary keys

The PK doesn't have to be an auto-increment integer. Any type that maps to a
column can be the PK; you assign the value yourself:

| PK declaration | Column type | Who assigns it |
|---|---|---|
| `Auto<i64>` *(default)* | `BIGSERIAL` / `BIGINT AUTO_INCREMENT` / `INTEGER … AUTOINCREMENT` | database |
| `Auto<i32>` | `SERIAL` / `INT AUTO_INCREMENT` / `INTEGER …` | database |
| `Auto<Uuid>` + `auto_uuid` | `UUID` | Rust-side (`uuid v4`) |
| `Auto<Uuid>` + `default_uuid_v7` | `UUID` | Rust-side (sortable `uuid v7`) |
| `String` + `primary_key` | `VARCHAR(N)` / `TEXT` | you (application) |
| `Uuid` + `primary_key` | `UUID` / `CHAR(36)` / `TEXT` | you (application) |
| `i64` / `i32` + `primary_key` | `BIGINT` / `INTEGER` | you (application) |

A **natural string key** (e.g. a coupon code) — note you provide the value and
insert with [`insert_pool`](#save-vs-insert), since there's no `Auto::Unset` for
`save` to detect:

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "coupon")]
pub struct Coupon {
    #[rustango(primary_key, max_length = 32)]
    pub code: String,        // you assign this
    pub discount: i64,
}

let c = Coupon { code: "SAVE10".into(), discount: 10 };
c.insert_pool(&pool).await?;                       // explicit INSERT
let back = Coupon::find_or_fail("SAVE10".to_string(), &pool).await?;   // look up by the string PK
```

**Rename the PK column** with `column` (the Rust field stays `number`, the SQL
column is `account_no`):

```rust
#[rustango(primary_key, column = "account_no")]
pub number: i64,
```

```rust
// Introspect it via the schema:
let pk = Account::SCHEMA.primary_key().unwrap();
assert_eq!(pk.name, "number");        // Rust field
assert_eq!(pk.column, "account_no");  // SQL column
```

**UUID primary keys** generate the value Rust-side: `auto_uuid` gives a random
v4, `default_uuid_v7` a time-sortable v7 (better for index locality):

```rust
#[rustango(primary_key, auto_uuid)]
pub id: Auto<uuid::Uuid>,
```

### Composite primary keys

Native multi-column primary keys are **not supported** — exactly one field may
be `primary_key`. The shipped pattern is a surrogate `Auto<i64>` PK plus a
declared composite-unique constraint, which is index-equivalent:

```rust
#[derive(Model)]
#[rustango(table = "line_item", unique_together = "invoice_id, line_no")]
pub struct LineItem {
    #[rustango(primary_key)]
    pub id: Auto<i64>,        // surrogate PK
    pub invoice_id: i64,
    pub line_no: i32,         // (invoice_id, line_no) is unique together
}
```

Look rows up with `.where_(LineItem::invoice_id.eq(..)).where_(LineItem::line_no.eq(..))`.

---

## Relationships

A foreign key is an `_id` column plus an optional typed accessor:

```rust
// Plain FK column — store the parent's id:
#[rustango(fk = "authors", on = "id")]
pub author_id: i64,

// Typed FK — lazy-loads the parent on demand:
pub author: ForeignKey<Author>,
```

`ForeignKey<T>` defaults its key type to `i64`; if the parent's PK is a
different type, name it: `ForeignKey<User, String>`. One-to-one uses
`#[rustango(o2o)]`; many-to-many is a separate table — see
[ORM cookbook → Many-to-many](orm.md#many-to-many). Eager-load related rows with
`select_related` (also in the ORM guide).

---

## Common field attributes

`#[rustango(...)]` on a field. The ones you'll use constantly:

| Attribute | Example | Effect |
|---|---|---|
| `primary_key` | `#[rustango(primary_key)]` | marks the PK |
| `max_length = N` | `#[rustango(max_length = 200)]` | `VARCHAR(N)` + write-time length check |
| `default = "…"` | `#[rustango(default = "'draft'")]` | DB column default (SQL literal) |
| `unique` | `#[rustango(unique)]` | unique constraint on the column |
| `choices = "…"` | `#[rustango(choices = "draft:Draft, published:Published")]` | enumerated values (`value:Label`); admin `<select>` + validation |
| `auto_now_add` | `#[rustango(auto_now_add)]` | set to now on **insert** (on an `Auto<DateTime<Utc>>`) |
| `auto_now` | `#[rustango(auto_now)]` | set to now on **every save** |
| `column = "…"` | `#[rustango(column = "account_no")]` | rename the SQL column |
| `null` / `Option<T>` | `pub note: Option<String>` | nullable column |
| `min` / `max` | `#[rustango(min = 0, max = 100)]` | write-time range validation |
| `blank` / `editable` | `#[rustango(editable = false)]` | form/admin behavior |
| `db_comment = "…"` | `#[rustango(db_comment = "cents")]` | column COMMENT |

`choices`, `default`, `auto_now_add`, and soft-delete together (all verified):

```rust
#[rustango(max_length = 20, default = "'draft'", choices = "draft:Draft, published:Published")]
pub status: String,
#[rustango(auto_now_add)]
pub created_at: Auto<DateTime<Utc>>,
#[rustango(soft_delete)]
pub deleted_at: Option<DateTime<Utc>>,
```

---

## Indexes and constraints

Declared on the **model**:

```rust
#[rustango(
    table = "posts",
    index("status, published_at"),                 // composite btree index
    unique_together = "author_id, slug",           // multi-column unique
    check(name = "qty_nonneg", expr = "qty >= 0"), // CHECK constraint
)]
```

- **`index(...)`** — a btree index by default; choose a method for PostgreSQL
  with `index(columns = "body", method = "gin")` (also `gist`, `brin`, `hash`,
  `bloom`, `spgist`).
- **`unique_together` / `index_together`** — multi-column unique / non-unique.
- **Partial indexes** — `unique_when(...)` / `index_when(...)` add a `WHERE`
  condition.
- **`check(name, expr)`** — a CHECK constraint; **`exclude(...)`** is a
  PostgreSQL EXCLUDE constraint.

---

## Common model attributes

`#[rustango(...)]` on the struct:

| Attribute | Example | Effect |
|---|---|---|
| `table = "…"` | `table = "posts"` | table name (defaults to the struct name) |
| `display = "…"` | `display = "title"` | the field shown when a row is referenced (FK labels, admin) |
| `app = "…"` | `app = "blog"` | groups the model under an app |
| `default_order = "…"` | `default_order = "-created_at"` | default sort for queries |
| `default_permissions` | `default_permissions = "add, change"` | which auto-permissions to create |
| `soft_delete` *(field)* | `#[rustango(soft_delete)] deleted_at: Option<…>` | enable soft delete (mark, don't remove) |
| `audit(track = "…")` | `audit(track = "title, status")` | record per-row change history |
| `scope = "…"` | `scope = "tenant"` | multi-tenancy scope (registry vs tenant) |
| `admin(...)` | `admin(list_display = "…")` | admin UI config — see [the admin](admin.md) |

---

## The generated API

`#[derive(Model)]` implements the `Model` trait (`Post::SCHEMA`) and generates:

- **`Post::objects()`** (alias `Post::query()`) → a `QuerySet<Post>` to filter,
  order, and fetch (the [ORM cookbook](orm.md) covers the query API).
- **Typed field constants** — `Post::title`, `Post::author_id` — used in
  `.where_(Post::author_id.eq(42))` for compile-checked filters.
- **Finders** — `find(pk, &pool)` → `Option<Self>`; `find_or_fail(pk, &pool)` →
  `Self` (errors if absent); `find_many(pks, &pool)`; `find_or_insert(...)`.
- **Writers** — `save`/`save_pool`, `save_partial(&["title"], &pool)` (update
  only some columns), `insert_pool` (explicit insert), `delete`.
- **Soft delete** (when enabled) — `soft_delete`, `restore`, `force_delete`;
  `QuerySet::active()` / `with_trashed()` / `only_trashed()`.

### save vs insert

This trips people up, so it's worth stating plainly:

| Method | Behaviour |
|---|---|
| `save_pool(&mut self, &pool)` | **INSERT** if the `Auto` PK is `Unset`, otherwise **UPDATE** |
| `insert_pool(&self, &pool)` | always **INSERT** |

For the default `Auto<i64>` PK, `save_pool` does the right thing automatically.
For an **application-assigned PK** (a `String`/`Uuid` you set yourself), there's
no `Unset` state — so `save_pool` would UPDATE a (possibly non-existent) row.
Use **`insert_pool`** to insert a brand-new row with a custom PK (verified in the
backing test).

---

## Full attribute reference

Every `#[rustango(...)]` key the derive accepts. The common ones are covered
above; this is the complete list, including advanced/PostgreSQL-specific ones.

### Model-level (on the struct)

| Attribute | Value | Effect |
|---|---|---|
| `table` | `"name"` | table name |
| `display` | `"field"` | human label for a row |
| `app` | `"name"` | app grouping |
| `default_order` | `"-field"` | default sort |
| `default_permissions` | `"add, change, delete, view"` | auto-permissions to create |
| `default_related_name` | `"posts"` | reverse-accessor name on the parent |
| `base_manager_name` | `"all_objects"` | name of the base (unfiltered) manager |
| `manager(ext = "Trait")` | trait path | generate a custom manager extension trait |
| `manager_fn` | `"published"` | add a manager accessor beyond `objects()` |
| `get_latest_by` | `"created_at"` | default column for `latest()`/`earliest()` |
| `order_with_respect_to` | `"parent"` | Django ordered-relative-to-parent |
| `index(...)` | `columns`, `method`, `name` | secondary index (btree/gin/gist/brin/hash/bloom/spgist) |
| `unique_together` | `"a, b"` | composite unique constraint |
| `index_together` | `"a, b"` | composite non-unique index |
| `unique_when(...)` / `index_when(...)` | columns + `condition` | partial (conditional) index |
| `check(...)` | `name`, `expr` | CHECK constraint |
| `exclude(...)` | operator spec | PostgreSQL EXCLUDE constraint |
| `audit(track = "…")` | field list | per-row change history |
| `scope` | `"tenant"` / `"registry"` | multi-tenancy scope |
| `proxy` | flag | proxy model (shares another's table) |
| `global_scope(name, apply = fn)` | name + fn | filter auto-applied to all queries |
| `through(...)` | relation spec | custom through-relation accessor |
| `reverse_has(...)` / `generic_has(...)` | relation spec | reverse has-many / reverse generic-FK accessor |
| `required_db_features` / `required_db_vendor` | list / vendor | deployment validation constraints |
| `db_table_comment` | `"…"` | table COMMENT |
| `admin(...)` | admin opts | admin UI config (see [admin.md](admin.md)) |

### Field-level (on a field)

| Attribute | Value | Effect |
|---|---|---|
| `primary_key` | flag | marks the PK |
| `column` | `"name"` | rename the SQL column |
| `max_length` | `N` | `VARCHAR(N)` + length validation |
| `default` | `"sql literal"` | column DEFAULT |
| `null` | flag | nullable (or use `Option<T>`) |
| `unique` | flag | unique constraint |
| `choices` | `"v:Label, …"` | enumerated values |
| `min` / `max` | number | range validation |
| `blank` | flag | allow empty in forms/admin |
| `editable` | `true`/`false` | form/admin editability |
| `auto_now` | flag | set to now on every save |
| `auto_now_add` | flag | set to now on insert |
| `auto_uuid` | flag | Rust-side UUID v4 (on `Auto<Uuid>`) |
| `default_uuid_v7` | flag | Rust-side sortable UUID v7 |
| `fk` + `on` | `"table"`, `"col"` | foreign key column |
| `cascade` | flag | `ON DELETE CASCADE` |
| `o2o` | flag | one-to-one relation |
| `fk_composite(...)` / `generic_fk(...)` | spec | composite FK / generic (content-type) FK |
| `generated_as` | `"expr"` | DB-computed (generated) column |
| `citext` | flag | case-insensitive text (PostgreSQL CITEXT) |
| `vector(dims = N)` | `N` | pgvector dimension |
| `geometry(srid = N)` | `N` | PostGIS spatial reference id |
| `db_comment` | `"…"` | column COMMENT |

---

## See also

- [ORM cookbook](orm.md) — querying, filters, aggregations, joins, transactions
  (what to do with a model once it's declared).
- [Serializers](serializers.md) — shape a model into JSON for an API.
- [The admin](admin.md) — the `admin(...)` block and the generated UI.
- [Scaffolding](scaffolding.md) · [`manage` CLI](manage.md) — generate a model
  and its migration.
