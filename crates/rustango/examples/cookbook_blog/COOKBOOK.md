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

### 2.18b `#[rustango(unique_together = "col1, col2")]` — composite UNIQUE

**What**: Container-level Django-shape `unique_together`. Emits `CREATE UNIQUE INDEX <table>_<col1>_<col2>_uq ON <table> (col1, col2)` so the DB rejects duplicate pairs even though neither column on its own is unique. Sister attr `index_together = "..."` for non-unique composite indexes. Both auto-derive the index name from the column list (override pending — see [v0.19 roadmap](../../../../../.claude/projects/-Users-ievgeniisvyryd-projects-rustango/memory/v019-unique-together.md)).

**Recipe** ([models.rs](src/apps/blog/models.rs)):

```rust
#[derive(Model)]
#[rustango(
    table = "cookbook_membership",
    unique_together = "org_id, user_id",
)]
pub struct Membership {
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    pub org_id: i64,
    pub user_id: i64,
    pub role: String,
}
```

**Verified by**: `unique_together_emits_composite_unique_index_in_schema`,
`unique_together_rejects_duplicate_pair`

**Caveat**: today the duplicate surfaces as the raw Postgres
`duplicate key value violates unique constraint "..."` message. A
DRF-style `UniqueTogetherValidator` that pre-checks at form
validation time and emits friendly per-field errors is tracked as
v0.19.1.

**Also during this slice** — the legacy container-level
`#[rustango(index = "col1, col2", unique, name = "...")]` syntax was
found unparseable (the trailing-flag block didn't compose under the
syn `parse_nested_meta` API). Removed the broken trailing-flag block;
`index = "..."` is now bare-only (composite, non-unique).

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

5 live recipes against docker PG verifying the migration lifecycle.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter04_migrations -- --test-threads=1`.

* §4.51 / 4.53 / 4.54 / 4.61 — `migrate(&pool, &dir)` applies pending,
  `unapply(&pool, &dir, name)` rolls back. Verifies schema catalog
  before + after. → `apply_then_unapply_round_trip`
* §4.55 / 4.57 / 4.58 — `Migration { name, prev, scope, atomic, snapshot,
  forward: Vec<Operation> }` with `Operation::Schema(SchemaChange)` +
  `Operation::Data(DataOp { sql, reverse_sql, reversible })`.
  Externally-tagged JSON: `{"schema": {"CreateTable": "..."}}` /
  `{"data": {"sql": "...", "reverse_sql": "...", "reversible": true}}`.
  → `migration_serde_round_trips_schema_and_data_ops`
* §4.56 — `embed_migrations!("migrations")` baked into the binary at
  compile time; `EMBEDDED.iter()` enumerates the migrations the
  current cookbook ships with. → `embedded_migrations_const_is_loaded`
* §4.59 — `AlterColumnType` / `AlterColumnNullable` /
  `AlterColumnDefault` (and friends) round-trip via the same
  externally-tagged JSON. → `alter_column_ops_serialize_with_external_tag`
* §4.64 — `AddCompositeFk { table, name, to, from, on }` /
  `DropCompositeFk { table, name }` (v0.15-F.5b) round-trip.
  → `composite_fk_ops_serialize_round_trip`

*Sub-sections 4.51b (make_migrations from inventory diff),
4.52 (per-app), 4.60 (rename), 4.65 (per-app ledger) queued for
Slice 4b.*

## Chapter 5 — Multi-tenancy

One comprehensive live test that provisions two tenants in schema
mode, then exercises every resolver against the seeded registry.
Run with `DATABASE_URL=... cargo test --test cookbook_chapter05_tenancy -- --test-threads=1`.

* §5.66 `SubdomainResolver::new(apex)` — extracts `acme` from
  `acme.cookbook-test.local` then DB-loads the matching `Org`.
* §5.68 `HeaderResolver::default()` — reads `X-Org` header slug.
* §5.70 `ChainResolver::new().push(...).push(...)` — first hit wins
  (Subdomain → Header fallback when no subdomain present).
* §5.71 schema-per-tenant — `create_tenant_if_missing(...)` creates a
  PG schema named after the slug + INSERTs the `Org` row + applies
  tenant-scoped migrations.
* §5.73 `TenantPools::pool_for_org(&org)` — returns a tenant-scoped
  pool whose `search_path` lands on the tenant's schema.
* §5.74 `tenancy::migrate_registry / migrate_tenants` — registry-
  scoped vs tenant-scoped migration passes.

→ `provision_two_tenants_then_resolve_and_lazy_pool`

The tenant Builder + apex/subdomain HTTP host split is exercised
implicitly by the `Cli::new().tenancy().run()` runserver path —
covered by Chapter 1 §1.5 and the framework's own `tenant_admin_live`
test.

*Sub-sections 5.67 (PathPrefixResolver), 5.69 (PortResolver),
5.72 (database-per-tenant via `--database-url`), 5.75 (per-tenant
auth: Operator vs User scoping), 5.76 (org bootstrap migration
templates) queued for Slice 5b.*

## Chapter 6 — Auth + permissions

7 live tests on the password / API-key / JWT / permission primitives.
No DB needed — pure crypto. Run with `cargo test --test cookbook_chapter06_auth`.

* §6.83 `passwords::hash` (Argon2 with random salt) +
  `passwords::verify` round-trip + `strength_score` issue list.
  → `passwords_hash_and_verify_round_trip`,
  `passwords_strength_score_flags_weak`
* §6.84 `api_keys::generate_key()` → `(token, prefix, hash)`.
  `split_token(token)` → `(prefix, secret)`. `verify_key(secret, hash)`.
  → `api_keys_issue_then_verify`
* §6.85 `jwt::Claims::new(sub).issuer(...).audience(...).ttl(...)` →
  `encode(&claims, secret)` → token. `decode(&token, secret)` rejects
  wrong-secret + tampered tokens; `decode_at(&token, secret, now)`
  rejects expired tokens.
  → `jwt_round_trips_with_ttl_and_subject`,
  `jwt_rejects_wrong_secret_and_tampered_token`,
  `jwt_decode_at_rejects_expired_token`
* §6.78 / 6.79 `permissions::codename_for::<T>("view"|"add"|"change"|"delete")`
  → Django-shape `{app}.{action}_{model}` strings.
  → `permission_codename_for_model_resolves_app_action_model`

*Sub-sections 6.77 (User/Role/Permission models — registry-side, see
framework's tenant_auth_live), 6.80 (ViewSet typed perms),
6.81 (auth backends), 6.82 (auth middleware), 6.86 (sessions),
6.87 (CSRF), 6.88 (HMAC-auth), 6.89 (TOTP), 6.90 (OAuth2),
6.91 (auth_flows), 6.92 (signed URLs) queued for Slice 6b.*

## Chapter 7 — Forms + serializer

6 tests covering `ModelFormFor<T>` parse/from_json/error aggregation/
bound-validation/null handling/insert-query emission. No DB needed.
Run with `cargo test --test cookbook_chapter07_forms`.

* §7.95 `ModelFormFor::<T>::parse(&HashMap<String,String>)` — form-
  encoded payload → `(columns, values)` with per-field bound
  validation. Auto<T> PK and `auto_now_add` columns are skipped
  (DB fills them). → `modelform_parses_form_encoded_into_typed_values`
* §7.95 missing required fields aggregate into `FormErrors`, one
  entry per field. → `modelform_missing_required_fields_aggregate_errors`
* §7.96 `ModelFormFor::<T>::from_json(&serde_json::Value)` — JSON
  request body. Null values on `Option<T>` write explicit
  `SqlValue::Null`. → `modelform_from_json_parses_object`,
  `modelform_from_json_null_writes_explicit_null`
* §7.98 `min`/`max` bounds run at parse time. Out-of-range values
  land in `FormErrors` keyed by field. →
  `modelform_bound_violation_lands_in_form_errors`
* §7.99 `into_insert_query()` emits an `InsertQuery` against the
  model's table. → `modelform_into_insert_query_targets_model_table`

**Framework bug fixed during this slice**: `ModelFormFor::parse`
required EVERY field including `auto_now_add` columns (e.g.
`joined_at: Auto<DateTime<Utc>>`), which the macro skips on INSERT
because `DEFAULT NOW()` fills them. Fix: skip every `auto = true`
field (was: only `auto && primary_key`).

### 7.99b `#[derive(Serializer)]` — DRF-shape JSON façade

6 tests in `tests/cookbook_chapter07b_serializer.rs` exercise the
serializer derive against the cookbook's `Author` model. Run with
`cargo test --test cookbook_chapter07b_serializer`.

* `from_model` + `to_value` round-trip — every field maps from the
  model and lands in the JSON output. → `serializer_from_model_then_to_value_round_trip`
* `#[serializer(read_only)]` — excluded from `writable_fields()`,
  still appears in JSON output. → `read_only_field_omitted_from_writable_fields`
* `#[serializer(write_only)]` — excluded from JSON output, accepted
  on input. → `write_only_field_excluded_from_json_output`
* `#[serializer(source = "x")]` — renames the JSON key to a
  different model field. → `source_attribute_renames_json_key`
* `#[serializer(skip)]` — uses `Default::default()`, leaves the field
  in JSON but excludes it from `writable_fields()`. →
  `skip_field_uses_default_and_appears_in_json_unchanged`
* `many_to_value` — batches a `Vec<Model>` into a JSON array. →
  `many_to_value_batches_into_json_array`

**Framework fix during this slice**: `Auto<T>` was missing an
`OpenApiSchema` impl, so `#[derive(Serializer)]` failed to build with
the `openapi` feature on for any model with `Auto<T>` fields. Added
`impl<T: OpenApiSchema> OpenApiSchema for Auto<T>` (forwards to T's
schema).

**Gaps tracked in [v0.18 DRF parity roadmap](../../../../../.claude/projects/-Users-ievgeniisvyryd-projects-rustango/memory/v018-drf-parity.md)**:

1. **Auto-nested FK** — `#[serializer(nested = AuthorSerializer)]`
   that lazy-loads + nests the parent. Today: manual.
2. **`SerializerMethodField`** — `#[serializer(method = "fn_name")]`
   computed fields. Today: manual after `from_model`.
3. **ViewSet ↔ Serializer wiring** —
   `ViewSet::for_model(...).serializer::<T>()`. Today: ViewSet
   serializes the bare model.
4. **M2M many-relations** —
   `#[serializer(many = TagSerializer, source = "tags")]`. Today: no
   automatic M2M traversal in serializers.
5. **Per-field validators chain** —
   `#[serializer(validate = "fn_name")]` per-field validator
   callable. Today: only model-level `min`/`max`/`max_length`.

*Sub-sections 7.93 (raw `Form` derive), 7.94 (admin's hand-rolled
ModelForm engine), 7.97 (parse_form_value per type), 7.98b (custom
validators) queued for Slice 7c.*

## Chapter 8 — Admin

Two parts: in-process router smoke + a real-browser playwright session.

### Part A — in-process smoke (no socket, no browser)

`tests/cookbook_chapter08_admin.rs` boots `admin::Builder::new(pool)
.build()` and hits routes via `tower::ServiceExt::oneshot`. 2 tests:

* §8.100 / 8.101 `admin_builder_serves_list_page_for_registered_model`
  — `GET /cookbook_author` returns 200 with the table name in the body.
* §8.103 `admin_create_form_renders_input_for_each_writable_field` —
  `GET /cookbook_author/new` returns 200; HTML contains `name="name"`,
  `name="email"`, `name="bio"`; does NOT contain `name="id"` (Auto<i64>
  PK is server-assigned and hidden from the create form).

Run: `DATABASE_URL=... cargo test --test cookbook_chapter08_admin -- --test-threads=1`.

### Part B/C — real-binary HTTP loop (Chapter 8b)

`tests/cookbook_chapter08b_browser_forms.rs` boots the actual
`cookbook_blog` binary against an isolated DB and drives the full
admin form + ViewSet flow over HTTP — the closest a Rust integration
test comes to a real browser session without playwright in the loop.
1 test:

* `admin_form_creates_then_viewset_isolates_per_tenant`:
  - migrate registry → create operator → create acme + globex →
    create alice/tenantpw on acme.
  - spawn `cookbook_blog` on `127.0.0.1:8867`.
  - `POST /__login` as alice (acme tenant).
  - `POST /__admin/cookbook_author` with `name=ada lovelace` etc.
  - `GET /api/authors` (acme) → returns `[{id:1, name:"ada lovelace"}]`.
  - `GET /api/authors` (globex) → returns `[]`.
  - `GET /api/authors` (apex `localhost`) → `404` (no tenant route).

Run: `DATABASE_URL=... cargo test --test cookbook_chapter08b_browser_forms -- --test-threads=1`.

**Framework bug fixed during this slice**: `forms::collect_values`
(used by the admin's hand-rolled create handler) demanded every
non-PK auto field — same shape as the Chapter 7 ModelFormFor bug.
Posting an Author through the admin returned "required field
`joined_at` was missing from the form" because `auto_now_add` columns
should be skipped server-side. Added `field.auto` to the auto-skip
filter alongside the explicit `skip` list.

### Part D — real-browser session (playwright MCP)

Reproducible by hand:

```sh
# Terminal 1 — fresh DB + boot
docker exec shop-postgres-1 psql -U rustango -c "CREATE DATABASE cookbook_browser_dev"
DATABASE_URL=postgres://rustango:rustango@localhost:5432/cookbook_browser_dev \
RUSTANGO_APEX_DOMAIN=localhost \
RUSTANGO_BIND=127.0.0.1:8765 \
RUSTANGO_SESSION_SECRET=cookbook-test-32bytes-cookbook-test-32bytes \
cargo run -- init-tenancy
# (then `migrate`, `create-operator admin --password letmein`,
#  `create-tenant acme --display-name "Acme Inc" --host-pattern acme.localhost`,
#  `create-user acme alice --password tenantpw --superuser`,
#  finally `cargo run`)
```

Verified browser-side via playwright MCP:

* §8.0 `http://localhost:8765/login` — operator login form renders
  with username/password/Sign in. Logging in as `admin / letmein`
  lands on the operator console (sidebar nav: Home, Operators,
  Organizations).
* §8.0 `http://acme.localhost:8765/__login` — tenant login form
  renders titled "Sign in to **acme**". Logging in as `alice /
  tenantpw` lands on the tenant admin index showing every registered
  model split by app group: `apps` (the cookbook's blog/auth/etc.
  models), `contenttypes` (rustango_content_types), `tenancy`
  (rustango_users + friends).

**Surfaced gap**: tenant admin returns a JSON 500 (`relation
"cookbook_author" does not exist`) when a tenant-scoped model's table
isn't materialized in the tenant's schema. The cookbook's models live
in inventory but no `make-migrations` has been run for them, so the
admin shows them in the index then errors on browse. A friendlier
"table not yet migrated — run `migrate-tenants`" message would close
this UX gap. Tracked in Gaps section below.

*Sub-sections 8.102 (detail view), 8.104 (FK display widget),
8.105 (FK search widget), 8.106 (M2M widget), 8.107 (JSONB editor),
8.108 (generic-FK link rendering), 8.109 (basic-auth wrap),
8.110 (custom actions), 8.111 (inline editing) queued for Slice 8b
which would need a tenant-scoped cookbook migration applied.*

## Chapter 9 — ViewSets / DRF / OpenAPI

4 live tests against `ViewSet::for_model(...).router(...)` mounted
in-process. Run with
`DATABASE_URL=... cargo test --test cookbook_chapter09_viewsets -- --test-threads=1`.

* §9.112 / 9.113 — `GET /authors` returns paginated list payload;
  `POST /authors` with JSON body creates a row and returns the
  serialized object with the assigned id.
  → `viewset_list_create_round_trip`
* §9.113 — `GET /authors/{id}` returns the single object;
  `PUT /authors/{id}` updates it; `DELETE /authors/{id}` removes it
  and a subsequent GET 404s. → `viewset_retrieve_returns_single_object_by_pk`,
  `viewset_update_then_destroy`
* §9.115 — `?name=bob` query param narrows the list to matching rows
  when `filter_fields(&["name", ...])` is configured.
  → `viewset_filter_query_param_narrows_list`

The ViewSet builder also exposes `.search_fields` (?search=…),
`.ordering` (default ORDER BY), `.page_size`, `.cursor_pagination`,
`.pagination(PaginationStyle)`, and `.permissions_for_model::<T>()`
(see Chapter 6 §6.80 for the typed-perm shortcut). All exercised
live in rustango's own viewset / order_by_annotate_live tests.

*Sub-sections 9.114 (full pagination — count + next + prev),
9.116b (typed permissions), 9.117 (OpenAPI auto-derive),
9.118 (response shaping via `.fields(&[...])`) queued for Slice 9b.*

## Chapter 10 — Templates + static

3 tests on the Tera template surface that the admin (Chapter 8) +
operator console use. No DB needed.
Run with `cargo test --test cookbook_chapter10_templates`.

* §10.119 `tera::Tera::default() + add_raw_template + render(name, ctx)`
  → `tera_template_renders_with_context`
* §10.119 `t.autoescape_on(vec!["html"])` — HTML special chars in
  context get escaped. → `tera_template_autoescapes_html`
* §10.122 `{% extends %} + {% block %}` template inheritance — child
  blocks override parent fallbacks. → `tera_extends_inherits_blocks_from_base`

The `render_generic_fk_link` helper (§10.121) is exercised live in
Chapter 2's `generic_fk_schema_and_content_type_lookup`.

*Sub-section 10.120 (Tera rendering from view handlers) and 10.123
(static-file serving) queued for Slice 10b.*

## Chapter 11 — Async / IO / extensions

7 tests on the most-used extension surfaces. No DB / network needed.
Run with `cargo test --test cookbook_chapter11_extensions`.

* §11.135 `cache::get_or_set(&dyn Cache, key, factory, ttl)` —
  loader runs once; subsequent reads hit the cached value.
  → `cache_get_or_set_memoizes_loader`,
  `cache_set_get_json_round_trips`
* §11.128 `webhook::sign(format, secret, body)` +
  `verify_signature(...)` round-trip. `HexSha256` produces a raw hex
  digest; `HexSha256WithPrefix` produces GitHub's `sha256=<hex>`
  shape; `Base64Sha256` for Stripe-style headers.
  → `webhook_sign_then_verify_round_trip`,
  `webhook_github_prefix_format`
* §11.92 `signed_url::sign[_at](url, secret, ttl|None)` +
  `verify[_at](signed, secret[, now_secs])` for one-time / time-bounded
  URLs. → `signed_url_sign_then_verify_at_respects_expiry`,
  `signed_url_no_expiry_always_verifies`
* §11.126 `Scheduler::new().every(name, period, job).start()` runs
  jobs at the given period; `Handle::shutdown()` stops further fires.
  → `scheduler_every_fires_periodic_job`

*Sub-sections 11.124 (WS hub), 11.125 (SSE), 11.127 (jobs queue),
11.129 (http_client retry/UA), 11.130 (email backends),
11.131 (storage filesystem), 11.132 (storage S3), 11.133 (media uploads),
11.134 (notifications), 11.137 (signals — wire receivers to model
save/delete events), 11.138 (compression middleware), 11.139 (CSP nonce)
queued for Slice 11b. Several have framework-level live tests under
rustango/tests already.*

## Chapter 12 — Bi-dialect + cross-cutting

2 live tests against a docker MySQL 8.0 container, exercising the
same cookbook model that PG tests use through the dialect-agnostic
`Pool` + `save_pool` / `fetch_pool` ORM API. Run with:

```sh
docker run -d --name rustango-mysql \
  -e MYSQL_ROOT_PASSWORD=rustango -e MYSQL_DATABASE=cookbook_blog_my \
  -e MYSQL_USER=rustango -e MYSQL_PASSWORD=rustango \
  -p 3406:3306 mysql:8.0

MYSQL_TEST_URL=mysql://rustango:rustango@127.0.0.1:3406/cookbook_blog_my \
  cargo test --test cookbook_chapter12_bidialect
```

* §12.140 / §12.141 — `Rating` model save + fetch + multi-row decode
  via `Pool::Mysql` and `save_pool` / `fetch_pool`. Exercises
  AUTO_INCREMENT for `Auto<i64>`, BIGINT round-trip, and the
  Backend trait dispatch the v0.17.1 macro refactor enabled.
  → `cookbook_rating_round_trips_against_mysql`,
  `mysql_decodes_multi_row_select`

**Surfaced limitation**: the cookbook's `Author` model can't run on
MySQL because it has two `Auto<T>` columns (`id: Auto<i64>` PK +
`joined_at: Auto<DateTime>` from `auto_now_add`). MySQL only supports
single-column RETURNING via `LAST_INSERT_ID()`, so `save_pool` errors
with `OperatorNotSupportedInDialect { op: "multi-column RETURNING",
dialect: "mysql" }`. Not a bug — a known dialect divergence the
framework surfaces as a clear runtime error. Workaround: drop the
`auto_now_add` mixin on MySQL-targeted models, or split the timestamp
column off into a trigger-managed shape.

*Sub-sections 12.142 (connection-pool tuning), 12.143 (tracing
subscriber + structured logs), 12.144 (manage check warnings),
12.145 (health endpoint), 12.146 (graceful shutdown), 12.147 (test
client utilities), 12.148 (inventory mechanism), 12.149 (macro
hygiene), 12.150 (project-shape conventions) queued for Slice 12b.*

---

## Gaps surfaced while writing this cookbook

*(populated as we discover them per chapter)*
