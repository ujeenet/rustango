# `rustango-orm` Extraction — Inventory

Issue [#141](https://github.com/ujeenet/rustango/issues/141). Authoritative file list + public-API surface for the standalone ORM crate. Locks the contract so downstream slices in the [#149 epic](https://github.com/ujeenet/rustango/issues/149) can move files in parallel without re-relitigating scope.

## Modules that move to `rustango-orm`

| Module | Files | LOC | Notes |
|---|---|---|---|
| [`core/`](../crates/rustango/src/core) | 16 | 7,252 | dialect-neutral IR, typed expressions, schema metadata |
| [`sql/`](../crates/rustango/src/sql) | 26 | 15,266 | writers + executor + dialect-specific (Postgres / MySql / Sqlite gated) |
| [`query/`](../crates/rustango/src/query) | 2 | 3,400 | `QuerySet` builder + `Q` expression |
| [`migrate/`](../crates/rustango/src/migrate) | 13 | 15,961 | migration runner + scaffolder + diff + manage CLI verbs |
| [`soft_delete.rs`](../crates/rustango/src/soft_delete.rs) | 1 | 457 | `active_filter` / `compose_with_active` / `trashed_filter` |
| [`contenttypes.rs`](../crates/rustango/src/contenttypes.rs) | 1 | 1,034 | `ContentType` model + lookup cache |

**Total: 59 files, ~43k LOC.**

Plus the four `macro_rules` definitions from [`lib.rs:107-305`](../crates/rustango/src/lib.rs):
- `__impl_my_*!`
- `__impl_sqlite_*!`
- (×2 for the from-row + load-related shapes)

## Public API surface of `rustango-orm` (post-move)

The current `crate::core::*` / `crate::sql::*` / `crate::query::*` / `crate::migrate::*` public re-exports become `rustango-orm::{core,sql,query,migrate}::*` after the move. No symbol-level renaming. The `rustango` crate keeps every public symbol bit-identical via `pub use rustango_orm::{...};` re-exports at the same paths.

### `rustango-orm::core` (16 files, 7,252 LOC)

```rust
pub mod aggregates;          // AggregateExpr, count/sum/avg/min/max/stddev/var
pub mod case;                // CaseBuilder, case(), value()
pub mod fts;                 // PG full-text-search constructors
pub mod funcs;               // ScalarFn helpers (now() / extract_year / random / …)
pub mod joins;               // Join, JoinKind
pub mod subquery;            // Subquery / Exists / OuterRef
pub mod window;              // WindowFn / WindowFrame / OVER clause

pub use case::{case, value, CaseBuilder};
pub use column::{Column, TypedAssignment, TypedExpr, TypedFieldList, TypedFilter};
pub use error::QueryError;
pub use expr::{BinOp, CaseBranch, Expr, JsonPathStep, ScalarFn, F};
pub use field_type::FieldType;
pub use query::{
    AggregateExpr, AggregateQuery, AggregateRow, Assignment, ColumnFilter,
    DeleteQuery, Filter, InsertQuery, NullsOrder, Op, OrderClause, OrderItem,
    OuterRef, PendingFilter, Relation, ScalarFnArg, SelectQuery, SqlError,
    UpdateQuery, WhereExpr, /* … */
};
pub use schema::{
    ColumnSchema, FieldSchema, FkPkKind, ModelEntry, ModelSchema, /* … */
};
pub use validate::validate_value;
pub use value::SqlValue;
pub use window::{FrameBoundary, FrameKind, WindowExpr, WindowFn, WindowFrame};
pub use inventory;
pub fn version() -> &'static str;
```

### `rustango-orm::sql` (26 files, 15,266 LOC)

```rust
pub mod m2m;                // M2MManager — already public
pub mod model_shortcuts;    // doc(hidden) helper-fn module backing macro emit

pub use auto::Auto;
pub use backend::{apply_auto_pk, try_get_returning, AssignAutoPkPool, /* … */};
pub use compiled::CompiledStatement;
pub use dialect::Dialect;
pub use error::{is_mysql_dup_index_error, ExecError, SqlError};
pub use executor::{
    atomic, bulk_insert_pool, bulk_update_pool, count_rows_pool, delete_pool,
    delete_tx, explain_pool, fetch_aggregate_pool, fetch_dates_pool,
    fetch_datetimes_pool, fetch_paginated_pool, fetch_with_prefetch_filtered,
    fetch_with_prefetch_pool, get_or_create, insert_pool, insert_returning_pool,
    insert_returning_tx, insert_tx, on_commit, on_commit_pending,
    raw_execute_pool, raw_execute_tx, raw_query_pool, raw_query_tx,
    run_ddl_idempotent, select_one_row_as_json, select_one_row_pool,
    select_rows_as_json, select_rows_pool, select_rows_pool_with_related,
    select_rows_tx_with_related, transaction_pool, update_or_create,
    update_pool, update_tx,
    CounterPool, ExistsPool, ExplainFormat, ExplainOptions, FetcherPool,
    FetcherTx, FkPkAccess, HasPkValue, InsertReturningPool, LoadRelated,
    MaybeMyFromRow, MaybeMyLoadRelated, MaybeMyScalar, MaybePgFromRow,
    MaybePgScalar, MaybeSqliteFromRow, MaybeSqliteLoadRelated, MaybeSqliteScalar,
    Page, PoolTx, UpdaterPool,
};
#[cfg(feature = "mysql")]  pub use executor::{row_to_json_my, LoadRelatedMy};
#[cfg(feature = "sqlite")] pub use executor::{row_to_json_sqlite, LoadRelatedSqlite};
#[cfg(feature = "postgres")] pub use executor::row_to_json;
pub use foreign_key::ForeignKey;
pub use m2m::M2MManager;
#[cfg(feature = "mysql")]    pub use mysql::MySql;
pub use pool::{Pool, PoolError};
pub use postgres::Postgres;
#[cfg(feature = "sqlite")]   pub use sqlite::Sqlite;
#[doc(hidden)] pub use sqlx;

#[cfg(feature = "postgres")]
#[doc(hidden)]
pub mod __macro_internals {
    pub use super::executor::{
        annotate_count_children, annotate_count_children_on, bulk_insert_on,
        delete_on, fetch_aggregate_on, fetch_with_prefetch, insert_on,
        insert_returning_on, raw_query_on, select_one_row_on, select_rows_on,
        update_on,
    };
}
```

### `rustango-orm::query` (2 files, 3,400 LOC)

```rust
pub use q::Q;
// + the QuerySet<T> / UpdateBuilder<T> / AggregateBuilder<T> / DatesQuerySet /
//   ValuesFlatQuerySet / ChunkedIter types from mod.rs (all already pub).
```

### `rustango-orm::migrate` (13 files, 15,961 LOC)

The CLI verb surface (`manage migrate` / `make_migrations` / `inspectdb` / …) plus the runner + diff + scaffolder. Inherits the current `crate::migrate::*` public surface; no public-API delta.

### `rustango-orm::soft_delete` / `rustango-orm::contenttypes`

Each is a single file; the existing module-level `pub` surface (`active_filter`, `compose_with_active`, `ContentType`, `ensure_seeded`, `get_for_model`, `for_model`, `all_ordered`, `by_id`, `by_natural_key`, etc.) becomes the standalone-crate surface verbatim.

## External `crate::` references the ORM makes today

Three sites in the modules that move reach OUTSIDE the ORM. Each must be resolved before the physical move (#144):

| Site | Reference | Disposition for `rustango-orm` |
|---|---|---|
| [`sql/pool.rs:59`](../crates/rustango/src/sql/pool.rs#L59) | `crate::env::{database_url_from_env, EnvError}` | **Inline.** The `database_url_from_env` helper is ~30 LOC of `std::env::var` + parse; move it into `sql::pool::env_helpers` (or just hand-roll the `std::env::var("DATABASE_URL")` two-liner in callers). `EnvError` is a thin wrapper around the parse error; collapse into `PoolError` variant or move alongside. |
| [`migrate/manage.rs:3912`](../crates/rustango/src/migrate/manage.rs#L3912) | `crate::config::Settings` (cfg-gated) | **Feature gap.** Settings lives in `rustango` proper. Either (a) leave the `migrate-tenant-storage` verb behind a `manage-with-settings` feature flag that's only enabled by the full `rustango` crate, or (b) thread the relevant fields through a `MigrateConfig { … }` struct that the verb constructs from Settings on the `rustango`-side. Option (b) preferred — `rustango-orm` stays settings-agnostic. |
| [`migrate/manage.rs:1533`](../crates/rustango/src/migrate/manage.rs#L1533) | `use crate::models::{model}` inside a scaffolder **template string** | **Leave as-is.** This isn't real Rust code that compiles in the rustango-macros build — it's a string interpolation that ends up in the user-generated app's source. The string `crate::models::{model}` is what the scaffolded app uses to import its own models; the scaffolder doesn't care which crate provides `Model`. No change needed. |

Additional refs that ONLY appear in unit-test modules (inside `#[cfg(test)] mod tests { ... }`) — not real public-API or external-crate dependencies:

- [`migrate/runner.rs:256/314/409`](../crates/rustango/src/migrate/runner.rs#L256) — `use crate::signals::migrate::{pre_migrate, post_migrate, …}` in three test functions. The tests assert the migrate runner fires the `pre_migrate` / `post_migrate` signals correctly. Signals live in `crate::signals` (not the ORM). Two paths:
  - Move the `signals::migrate::{pre_migrate, post_migrate, MigrationCtx}` channel types alongside `migrate/` into `rustango-orm` (they're meaningful only to the migrate flow).
  - Leave signals in `rustango` and drop these three test functions from `rustango-orm` (they get re-instated in the full `rustango` crate's integration tests).

Decision recommendation: **move the migrate signal channel into `rustango-orm`** so the runner stays testable standalone. The signals broadcast surface itself stays in `rustango`.

## Modules that STAY in `rustango`

Confirmed by reverse-dep grep — none of these are imported BY the ORM. They depend ON the ORM, not the other way around (counts = `use crate::{core,sql,query,migrate}::*` ref counts inside each module):

| Module | Imports from ORM | Stays in rustango |
|---|---|---|
| `admin/`     | 23 refs | ✓ |
| `tenancy/`   | 85 refs | ✓ |
| `viewset/`   | 12 refs | ✓ |
| `serializer/`|  2 refs | ✓ |
| `jobs/`      |  5 refs | ✓ |
| `oauth2/`    |  0 refs | ✓ |
| `auth/`      |  0 refs | ✓ |
| `signals/`   |  1 ref  | ✓ (modulo the migrate-channel carve-out noted above) |
| `cache/`     |  2 refs | ✓ |
| `email/`     |  0 refs | ✓ |
| `i18n/`      |  0 refs | ✓ |
| `media/`     |  3 refs | ✓ |
| `storage/`   |  0 refs | ✓ |
| `template/`  |  0 refs | ✓ |
| `forms/`     |  4 refs | ✓ |
| `webhook/`   |  0 refs | ✓ |

**Direction is clean:** `rustango → rustango-orm` only; never the reverse. The framework can ship as `pub use rustango_orm::{core, sql, query, migrate, soft_delete, contenttypes};` re-exports plus all the framework-only modules.

## Sign-off

- [x] **Module list:** 4 module-dirs + 2 files + 4 `macro_rules` → `rustango-orm`.
- [x] **Public surface:** every existing `pub use rustango::{core,sql,query,migrate}::*` re-export listed above; `rustango-orm` mirrors at `pub use rustango_orm::{core,sql,query,migrate}::*`.
- [x] **External refs:** 3 sites enumerated, dispositions noted (inline / feature-gap / leave-as-is); 1 test-only signal-channel carve-out flagged.
- [x] **Reverse direction:** no `crate::{admin,tenancy,viewset,…}` reference INSIDE the moved modules (verified by grep).
- [x] **`inventory` re-export:** `core::inventory` is part of the move — every `#[derive(Model)]` registers itself with the `inventory` crate, and downstream apps iterate via `inventory::iter::<ModelEntry>()`.

Closes [#141](https://github.com/ujeenet/rustango/issues/141). Unlocks downstream slices #144 (physical move), #145 (scaffolder templates), #147 (smoke tests).
