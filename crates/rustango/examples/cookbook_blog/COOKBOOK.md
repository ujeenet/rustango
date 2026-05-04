# rustango Cookbook

A feature-by-feature tour of `rustango`. Every section follows the
same shape:

```text
N.M Feature name
────────────────
What:         one-line summary
When:         trigger / use case
API:          file:line citation in rustango
Recipe:       minimal code from cookbook_blog
Verified by:  the test that exercises the recipe
```

The blog app under [src/](src/) is the source of truth — every recipe
quotes from a real, compiling, test-covered file.

## Table of contents

1. [Project shape & manage commands](#chapter-1--project-shape--manage-commands)
2. [Models & schema](#chapter-2--models--schema)
3. [ORM](#chapter-3--orm)
4. [Migrations](#chapter-4--migrations)
5. [Multi-tenancy](#chapter-5--multi-tenancy)
6. [Auth + permissions](#chapter-6--auth--permissions)
7. [Forms + serializer](#chapter-7--forms--serializer)
8. [Admin](#chapter-8--admin)
9. [ViewSets / DRF / OpenAPI](#chapter-9--viewsets--drf--openapi)
10. [Templates + static](#chapter-10--templates--static)
11. [Async / IO / extensions](#chapter-11--async--io--extensions)
12. [Bi-dialect + cross-cutting](#chapter-12--bi-dialect--cross-cutting)

---

## Chapter 1 — Project shape & manage commands

### 1.1 `cargo rustango startproject` / `manage startapp`

**What**: Scaffolder that emits the canonical Django-shape project layout.

**When**: Brand-new project, or adding a new sub-app to an existing one.

**API**: [`cargo-rustango`](../../../cargo-rustango/src/main.rs) for `startproject`; [`manage::startapp`](../../src/manage/scaffold.rs) for `startapp`.

**Recipe**: this very project was scaffolded by hand to match the layout `cargo rustango new --template tenant` produces. v0.16's unified `Cli::new()` dispatcher means there is no `src/bin/manage.rs` and no second binary — `cargo run` is `runserver`, `cargo run -- <verb>` is everything else.

```text
cookbook_blog/
├── Cargo.toml                  -- one [package], one binary
├── src/
│   ├── main.rs                 // rustango::main + Cli::new().tenancy().api(...).run()
│   ├── settings.rs             // config/{default,test}.toml loader
│   └── apps/
│       ├── tenants/{models.rs, urls.rs, views.rs, admin.rs, mod.rs}
│       ├── auth/...
│       ├── blog/...
│       └── ...
├── migrations/0001_*.json
├── config/{default,test}.toml
└── tests/cookbook_chapter*.rs
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::layout_matches_django_shape`

---

### 1.2 `cargo run -- migrate` / `migrate <target>` / `downgrade [N]`

**What**: Apply / rewind / point-target migrations against a `Pool`.

**When**: Boot, deploy, local dev — anywhere schema needs to track code.

**API**: [`manage::Cli`](../../src/manage.rs) wraps [`migrate::runner`](../../src/migrate/runner.rs).

**Recipe** ([src/main.rs](src/main.rs)):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .tenancy()
        .api(apps::api())
        .run().await
}
```

* `cargo run -- migrate` applies pending; `cargo run -- migrate <target>` walks forward or back to a named target (`zero` unapplies everything); `cargo run -- downgrade [N]` rolls back N steps (default 1).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_help_works_without_database_url`

---

### 1.3 `cargo run -- makemigrations [name] [--empty] [--dry-run]`

**What**: Diff the current model registry against the latest snapshot and emit a new migration JSON.

**When**: After a model change.

**API**: [`migrate::make`](../../src/migrate/make.rs)

**Recipe**: forwarded by `Cli::run()` to `migrate::manage::run`. `cargo run -- makemigrations [<name>]`. `--dry-run` prints the planned ops without writing; `--empty <name>` emits a stub for hand-written `Operation::Data` work (e.g. `RenameTable`).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_makemigrations_verb`

---

### 1.4 `cargo run -- check`

**What**: Static configuration sanity check — model registry + settings + migration ledger.

**When**: CI gate before running tests.

**API**: [`migrate::manage::run`](../../src/migrate/manage.rs) `check` arm.

**Recipe**: `cargo run -- check` — runs the bundled checks (system warnings, unapplied migrations).

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_check_verb`

---

### 1.5 `cargo run` (no args) — runserver via `Cli`

**What**: Default verb. Opens the pool, applies migrations, mounts the user's API router, serves on `RUSTANGO_BIND` (default `0.0.0.0:8080`). Tenancy variant defers to [`server::Builder`](../../src/server/mod.rs) which wires the apex/subdomain host split + operator console.

**When**: Always — replaces hand-rolled axum wiring AND the second `manage` binary projects used to write.

**API**: [`manage::Cli::run`](../../src/manage.rs)

**Recipe** ([src/main.rs](src/main.rs)) — same one-liner as §1.2.

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_no_args_dispatches_to_runserver`

---

### 1.6 `cargo run -- create-operator` / `create-user` (tenancy)

**What**: Bootstrap an Operator (registry-side admin) or tenant User. Argon2-hashes the password and inserts into the right table.

**When**: First boot of a fresh database; new tenant onboarding.

**API**: [`tenancy::manage::run`](../../src/tenancy/manage/mod.rs) `create-operator` / `create-user` arms (forwarded by `Cli::tenancy().run()`).

**Recipe**: `cargo run -- create-operator admin --password letmein` then `cargo run -- create-user acme alice --password hunter2 --superuser`. (Single-tenant `createsuperuser` not yet wired.)

**Verified by**: `tests/cookbook_chapter01_manage.rs::cli_dispatcher_recognises_create_operator_verb`

---

### 1.7 `embed_migrations!` macro

**What**: Compile-time embed of the `migrations/` JSON files as `&'static [Migration]` so binaries ship with no filesystem dependency.

**When**: Distributing a single static binary that owns its schema.

**API**: [`rustango_macros::embed_migrations!`](../../../rustango-macros/src/lib.rs)

**Recipe** ([src/main.rs](src/main.rs)):

```rust
const EMBEDDED: &[rustango::migrate::Migration] =
    rustango::embed_migrations!("migrations");
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::embedded_migrations_are_nonempty`

---

### 1.8 Settings layering (`config/default.toml` → `config/{env}.toml` → env)

**What**: Three-tier config loader. Later layers shadow earlier; env vars take final precedence with `RUSTANGO__SECTION__KEY` syntax.

**When**: Per-environment differences (dev/test/prod) without code changes.

**API**: [`config::Settings::load`](../../src/config/mod.rs)

**Recipe** ([src/settings.rs](src/settings.rs)):

```rust
pub fn load() -> rustango::config::Settings {
    rustango::config::Settings::load("config", &std::env::var("RUSTANGO_ENV").unwrap_or_else(|_| "default".into()))
}
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::settings_layer_resolves_env_overrides`

---

### 1.9 `rustango::main` macro

**What**: Tokio runtime boot + tracing-subscriber wire-up in one attribute.

**When**: Every `main`.

**API**: [`rustango::main`](../../../rustango-macros/src/lib.rs)

**Recipe** ([src/main.rs](src/main.rs)):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> { ... }
```

**Verified by**: `tests/cookbook_chapter01_manage.rs::main_macro_compiles_and_boots`

---

### 1.10 Welcome page

**What**: Default `GET /` landing page when no route is registered.

**When**: First boot of a brand-new project — confirms the server is alive.

**API**: [`welcome`](../../src/welcome.rs)

**Recipe**: handled by `Builder::serve` automatically when no `/` route is mounted; replaced by the first user-defined `Router::route("/", ...)`.

**Verified by**: `tests/cookbook_chapter01_manage.rs::welcome_page_renders_on_fresh_router`

---

## Chapter 2 — Models & schema

Models live in [src/apps/blog/models.rs](src/apps/blog/models.rs).
Live tests against docker PG in
[tests/cookbook_chapter02_models.rs](tests/cookbook_chapter02_models.rs).
Run with `DATABASE_URL=... cargo test --test cookbook_chapter02_models -- --test-threads=1`.

### 2.11 / 2.12 `#[derive(Model)]` + `Auto<i64>` / `Auto<i32>`

**What**: Derive macro registers the struct with the global inventory and emits `objects()` / typed save / FromRow impls. `Auto<T>` PKs translate to `BIGSERIAL` (i64) / `SERIAL` (i32); the macro skips them on INSERT and assigns the returning value.

**API**: [`rustango::Model`](../../src/lib.rs#L610) (re-exported from [`rustango_macros`](../../../rustango-macros/src/lib.rs)); [`rustango::sql::Auto`](../../src/sql/auto.rs).

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model, Debug, Clone)]
#[rustango(table = "cookbook_author", display = "name")]
pub struct Author {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    #[rustango(max_length = 80)]
    pub name: String,
    // ...
}
```

**Verified by**: `tests/cookbook_chapter02_models.rs::save_assigns_auto_pk`

---

### 2.13 `Option<T>` → nullable column

**What**: Wrap any field type in `Option<T>` and the column becomes `NULL`-able; `None` round-trips as SQL `NULL`.

**Recipe**: `pub bio: Option<String>` on `Author`.

**Verified by**: `option_field_round_trips_null`

---

### 2.14 `#[rustango(default = "...")]` + 2.29 `auto_now_add`

**What**: `default = "expr"` emits `DEFAULT <expr>` in DDL. The mixin `auto_now_add` is sugar for "wrap in `Auto<T>` + DB DEFAULT NOW()" so the column auto-fills on INSERT and the macro skips it.

**Recipe**: `#[rustango(auto_now_add)] pub joined_at: Auto<chrono::DateTime<chrono::Utc>>`.

**Verified by**: `auto_now_add_assigns_at_insert`

---

### 2.15 `#[rustango(unique)]`

**What**: Per-column UNIQUE constraint. Duplicate inserts fail with a SQL unique-violation error.

**Recipe**: `#[rustango(unique, max_length = 200)] pub email: String`.

**Verified by**: `unique_constraint_rejects_duplicates`

---

### 2.16 `#[rustango(min = N, max = M)]` → CHECK + client validation

**What**: Defense in depth — the macro adds a CHECK constraint to DDL **and** a client-side range validator. `save()` rejects out-of-range values before the round-trip with `ExecError::OutOfRange`.

**Recipe**: `#[rustango(min = 1, max = 5)] pub score: i64`.

**Verified by**: `min_max_check_rejects_out_of_range`

---

### 2.17 `#[rustango(max_length = N)]`

**What**: String columns become `VARCHAR(N)` instead of `TEXT`. Without it, plain `String` is `TEXT`.

**Recipe**: `#[rustango(max_length = 80)] pub name: String`.

**Verified by**: implicit (every `cookbook_*` table uses VARCHAR for max_length fields).

---

### 2.18 `#[rustango(index)]` (field-level)

**What**: Single-column index on the field. `index(unique)` for unique-indexes, `index(name = "...")` to override the auto name.

**Recipe**: `#[rustango(fk = "cookbook_author", index)] pub author_id: i64`.

**Verified by**: `fk_column_round_trips`

---

### 2.20 `#[rustango(fk = "table")]` — basic foreign key

**What**: Adds a `BIGINT` FK column and a `REFERENCES <table>(id)` constraint. The `on = "..."` sub-attr overrides the target column name. See also Chapter 17 `fk = "self"` for tree shapes.

**Recipe**: `#[rustango(fk = "cookbook_author", index)] pub author_id: i64`.

**Verified by**: `fk_column_round_trips`

---

### 2.26 `serde_json::Value` → JSONB

**What**: Field of type `serde_json::Value` becomes a `JSONB` column. Nested structures round-trip without manual encoding.

**Recipe**: `pub metadata: serde_json::Value`.

**Verified by**: `jsonb_field_round_trips_structured_data`

---

### 2.28 `chrono::DateTime<Utc>` / `Option<DateTime>` → TIMESTAMPTZ

**What**: Maps to `TIMESTAMPTZ`. `Option<DateTime>` is nullable.

**Recipe**: `pub published_at: Option<chrono::DateTime<chrono::Utc>>`.

**Verified by**: `datetime_option_round_trips`

---

### 2.21 `#[rustango(o2o = "table")]` — one-to-one (UNIQUE FK)

**What**: Same shape as `fk` but enforces a UNIQUE constraint so the relation is 1:1. Duplicate inserts into the FK column fail.

**Recipe**: `#[rustango(o2o = "cookbook_author")] pub author_id: i64` on `AuthorProfile`.

**Verified by**: `o2o_unique_fk_rejects_duplicate`

---

### 2.22 `#[rustango(m2m(name, to, through, src, dst))]` — M2M through

**What**: Container-level attribute that emits a junction-table accessor `<name>_m2m()`. The macro doesn't auto-create the through table; you create it by adding a regular junction model + migration. Reads/writes go through the junction table directly.

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[rustango(
    table = "cookbook_post",
    m2m(name = "tags", to = "cookbook_tag",
        through = "cookbook_post_tag",
        src = "post_id", dst = "tag_id"),
)]
pub struct Post { ... }
```

**Verified by**: `m2m_through_junction_table_round_trips`

---

### 2.27 `Auto<uuid::Uuid>` + `auto_uuid` — UUID PKs

**What**: `#[rustango(auto_uuid)]` is sugar for `primary_key + auto + DEFAULT gen_random_uuid()`. Postgres' `pgcrypto` extension supplies the v4. Macro skips the column on INSERT; the returning value lands in `Auto<Uuid>`.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(table = "cookbook_session")]
pub struct Session {
    #[rustango(auto_uuid)]
    pub id: Auto<Uuid>,
    pub user_token: String,
}
```

**Verified by**: `auto_uuid_assigns_server_side_uuid`

---

### 2.30 `#[rustango(soft_delete)]` — tombstone deletes

**What**: Mark an `Option<DateTime<Utc>>` field as the soft-delete tombstone. Currently captured in the model's `SCHEMA.soft_delete_column` so the ORM can layer the alive-when-NULL filter and override `delete()` to UPDATE the tombstone instead of `DELETE FROM`.

**Recipe**:

```rust
#[derive(Model)]
pub struct ArchiveNote {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub note: String,
    #[rustango(soft_delete)]
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}
```

**Verified by**: `soft_delete_column_round_trips_and_deleted_at_defaults_null`

---

### 2.19 `#[rustango(check(name, expr))]` — table-level CHECK

**What**: Container-level CHECK constraint with a chosen name and a raw SQL boolean expression. Rejects inserts that violate the predicate at the DB.

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_inventory_item",
    check(name = "cookbook_inventory_item_qty_chk",
          expr = "qty >= 0 AND price_cents > 0"),
)]
pub struct InventoryItem { ... }
```

**Verified by**: `table_level_check_rejects_invalid_row`

---

### 2.23 `#[rustango(fk_composite(name, to, from, on))]` — composite FK

**What**: Multi-column foreign key. The `from = (...)` columns on this model reference the `on = (...)` columns on the target table. The DDL emits a single `CONSTRAINT … FOREIGN KEY (a, b) REFERENCES tgt (x, y)` so the DB rejects unmatched pairs.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_pair_link",
    fk_composite(
        name = "pair_target_fk",
        to = "cookbook_pair_target",
        from = ("left_ref", "right_ref"),
        on = ("a_id", "b_id"),
    ),
)]
pub struct PairLink { ... }
```

**Verified by**: `fk_composite_rejects_unmatched_pair`

---

### 2.24 / 2.25 `#[rustango(generic_fk(name, ct_column, pk_column))]` + ContentType lookup

**What**: Generic foreign key — pairs a `target_content_type_id BIGINT` column with a `target_object_pk BIGINT` column. The framework knows the pair logically points at "any registered model's row." `Model::SCHEMA.generic_relations` exposes the metadata; admin uses it to render clickable links via `render_generic_fk_link`.

`ContentType::for_model::<T>()` looks up the ContentType row for any registered model after `ensure_seeded(&pool)` populates the registry.

**Recipe**:

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_activity",
    generic_fk(
        name = "target",
        ct_column = "target_content_type_id",
        pk_column = "target_object_pk",
    ),
)]
pub struct Activity { ... }
```

```rust
ensure_seeded(&pool).await?;
let ct = ContentType::for_model::<Author>(&pool).await?.unwrap();
let mut act = Activity {
    target_content_type_id: ct.id_value()?,
    target_object_pk: author_id,
    action: "viewed".into(), ..
};
act.save(&pool).await?;
```

**Verified by**: `generic_fk_schema_and_content_type_lookup`

## Chapter 3 — ORM

13 live recipes against the Author / Post fixture from Chapter 2.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter03_orm -- --test-threads=1`.

* §3.31 `Post::objects().filter("published", Op::Eq, true).fetch(&pool)` →
  `filter_eq_fetch_returns_matching_rows`
* §3.34 `Op::Gt` / `Op::Lt` / `Op::ILike` / `Op::In` / `Op::Between` /
  `Op::IsNull` — six tests covering the full Op surface.
* §3.35 `.order_by(&[("col", desc)])` (`true = DESC`, `false = ASC`) →
  `order_by_view_count_desc`
* §3.36 `.limit(N).offset(M)` for pagination → `limit_offset_paginates`
* §3.37 `.aggregate().annotate("alias", AggregateExpr::Count|Sum|Avg|...)`
  + `fetch_aggregate(&q, &pool)` → `Vec<HashMap<String, SqlValue>>`
  → `aggregate_count_and_sum`
* §3.42 `model.save(&pool)` does INSERT (PK Unset) or UPDATE (PK Set) →
  `save_inserts_then_updates_in_place`
* §3.46 raw `sqlx::query_scalar / query_as` for SQL the QuerySet
  doesn't cover → `raw_sql_escape_via_sqlx`
* §3.47 manual `pool.begin() ... tx.rollback()` — atomic rollback on
  UNIQUE violation → `manual_transaction_rolls_back_on_error`
* §3.48 PG JSONB `@>` containment operator on the `metadata` column →
  `json_operator_on_jsonb_column`

**Framework bug fixed during this slice**: `SUM(BIGINT)` returns
PostgreSQL `NUMERIC` and `AVG(BIGINT)` returns `NUMERIC` — the
aggregate row decoder only tries `i64`/`i32`/`f64`/`bool`/`String`,
so the result silently came back as `SqlValue::Null`. Added
`Dialect::cast_aggregate_to_int` / `cast_aggregate_to_float` (PG
emits `::bigint` / `::double precision`; MySQL emits `CAST(.. AS
SIGNED)` / `CAST(.. AS DOUBLE)`). Aggregate writer wraps SUM/AVG via
the new methods.

*Sub-sections 3.32 (get/get_on/fetch_on), 3.33 (OR-nested),
3.38 (annotate), 3.39 (prefetch FK), 3.40 (prefetch_soft),
3.41 (prefetch_generic), 3.43 (bulk_insert), 3.44 (bulk UPDATE),
3.45 (WhereExpr::Not), 3.49 (_pool executor variants) queued for
Slice 3b.*

## Chapter 4 — Migrations

*(Slice 2)*

## Chapter 5 — Multi-tenancy

*(Slice 4)*

## Chapter 6 — Auth + permissions

*(Slice 5)*

## Chapter 7 — Forms + serializer

*(Slice 5)*

## Chapter 8 — Admin

*(Slice 6)*

## Chapter 9 — ViewSets / DRF / OpenAPI

*(Slice 6)*

## Chapter 10 — Templates + static

*(Slice 6)*

## Chapter 11 — Async / IO / extensions

*(Slice 7)*

## Chapter 12 — Bi-dialect + cross-cutting

*(Slice 3 + Slice 8)*

---

## Gaps surfaced while writing this cookbook

*(populated as we discover them per chapter)*
