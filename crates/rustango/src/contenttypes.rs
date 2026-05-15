//! Django-shape ContentType framework — sub-slice F.1 of v0.15.0.
//!
//! A `ContentType` row is a runtime handle to a registered model:
//! `(id, app_label, model_name, table)`. Lets framework features
//! (permissions, generic foreign keys, audit history, soft-FK
//! prefetch) reference any model by `(app_label, model_name)` or
//! by `content_type_id` without hard-coding the target table into
//! their schema.
//!
//! ## Lifecycle
//!
//! - The `ContentType` model itself ships as a `#[derive(Model)]`
//!   so it migrates / appears in the admin like any other table.
//! - [`ensure_seeded`] walks `inventory::iter::<ModelEntry>()` and
//!   inserts a row for every registered model that doesn't have one
//!   yet. Idempotent — re-running on a populated DB is a no-op.
//!   Wire into your bootstrap (e.g. inside `main()` after
//!   `migrate(&pool, dir).await?`).
//! - [`for_model`] resolves a model type to its `ContentType`
//!   row (cached per pool — repeated calls hit a process-wide
//!   `OnceCell`, not the DB).
//! - [`all`] / [`by_id`] / [`by_natural_key`] cover the lookup
//!   shapes the admin and audit views need.
//!
//! ## Why not infer at query time?
//!
//! The model registry (`inventory`) is process-local and Rust-typed
//! — but permissions, audit rows, generic foreign keys, and
//! cross-process integrations need a **stable database identifier**
//! the framework can hand to other systems. The ContentType row's
//! `id` (a `BIGSERIAL`) is that identifier. `(app_label, model_name)`
//! is the natural key for human-facing wiring; numeric `id` is the
//! foreign key everywhere else.

use crate::core::{inventory, Model as _, ModelEntry, SqlValue};
use crate::sql::{Auto, ExecError, FetcherPool as _};
// PG-typed helpers below pull in `PgPool` + `Fetcher`. Sqlite/MySQL-
// only builds get just the tri-dialect `*_pool` entry points.
#[cfg(feature = "postgres")]
use crate::sql::{sqlx::PgPool, Fetcher as _};
use crate::Model;

/// One row per registered model. The schema mirrors Django's
/// `django_content_types` table closely enough that any code reading
/// it (audit log front-ends, generic FKs, permissions) feels
/// instantly familiar.
///
/// `(app_label, model_name)` is a natural key — the migration
/// emits a `UNIQUE` constraint on the pair so duplicate inserts
/// from a racy bootstrap fail loudly instead of silently creating
/// two rows.
#[derive(Debug, Clone, Model)]
#[rustango(table = "rustango_content_types")]
pub struct ContentType {
    /// Auto-assigned primary key. Used as the foreign key everywhere
    /// the framework needs to point at "any model" (permissions,
    /// generic FKs, audit log targets in F.2 / F.3).
    #[rustango(primary_key)]
    pub id: Auto<i64>,
    /// Django-shape app label — `module_path!()`'s first segment
    /// after the crate root, or the explicit `#[rustango(app = "...")]`
    /// override from the model's container attr.
    #[rustango(max_length = 100)]
    pub app_label: String,
    /// Lowercase Rust-side model name. Conventionally matches the
    /// `#[derive(Model)]` struct ident lowercased (e.g. `User` →
    /// `user`).
    #[rustango(max_length = 100)]
    pub model_name: String,
    /// SQL table name (`#[rustango(table = "…")]` value or the
    /// auto-derived snake-case fallback). Carried alongside
    /// `model_name` so callers reading the audit log don't have to
    /// reconstruct it from the registry every time.
    #[rustango(max_length = 100)]
    pub table: String,
}

// v0.35 — PG-typed back-compat block. Sqlite/MySQL apps use the
// tri-dialect `*_pool` variants further down.
#[cfg(feature = "postgres")]
impl ContentType {
    /// Look up a `ContentType` row for a registered model type.
    ///
    /// Cheap-ish (DB round trip) — for hot paths consider
    /// [`for_model_cached`] which memoizes per-process. Returns
    /// `Ok(None)` when [`ensure_seeded`] hasn't been called yet for
    /// this model (the row doesn't exist in the DB).
    ///
    /// # Errors
    /// Driver / query failures from the underlying SELECT.
    pub async fn for_model<T: crate::core::Model>(
        pool: &PgPool,
    ) -> Result<Option<Self>, ExecError> {
        let entry = inventory::iter::<ModelEntry>
            .into_iter()
            .find(|e| e.schema.table == T::SCHEMA.table)
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            })?;
        let app = entry.resolved_app_label().unwrap_or("project");
        let name = T::SCHEMA.name.to_ascii_lowercase();
        Self::by_natural_key(pool, app, &name).await
    }

    /// Lookup by `(app_label, model_name)` — the natural key. Used
    /// when the caller has the strings (e.g. parsing
    /// `"app.action_model"` permission codenames) but not the Rust
    /// type.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_natural_key(
        pool: &PgPool,
        app_label: &str,
        model_name: &str,
    ) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter(
                "app_label",
                crate::core::Op::Eq,
                SqlValue::String(app_label.into()),
            )
            .filter(
                "model_name",
                crate::core::Op::Eq,
                SqlValue::String(model_name.into()),
            )
            .limit(1)
            .fetch(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Lookup by primary key. Used by FK joins (audit log target,
    /// permission scope, etc.).
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_id(pool: &PgPool, id: i64) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter("id", crate::core::Op::Eq, SqlValue::I64(id))
            .limit(1)
            .fetch(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// All registered ContentTypes, ordered by `(app_label, model_name)`
    /// for stable display in admin sidebars / API listings.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn all(pool: &PgPool) -> Result<Vec<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .order_by(&[("app_label", false), ("model_name", false)])
            .fetch(pool)
            .await?;
        Ok(rows)
    }
}

// ============================================================ tri-dialect lookups

/// Tri-dialect `ContentType` lookups — work against any backend the
/// [`crate::sql::Pool`] enum carries.
impl ContentType {
    /// Backend-agnostic counterpart of [`Self::by_natural_key`].
    /// Routes through [`FetcherPool::fetch_pool`] so the same call
    /// works against `Pool::Postgres` / `Pool::Mysql` / `Pool::Sqlite`.
    /// Prefer this in framework code so sqlite/mysql apps don't get
    /// silently locked out of the contenttype catalog.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_natural_key_pool(
        pool: &crate::sql::Pool,
        app_label: &str,
        model_name: &str,
    ) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter(
                "app_label",
                crate::core::Op::Eq,
                SqlValue::String(app_label.into()),
            )
            .filter(
                "model_name",
                crate::core::Op::Eq,
                SqlValue::String(model_name.into()),
            )
            .limit(1)
            .fetch_pool(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// v0.37 — backend-agnostic counterpart of [`Self::by_id`]. Used
    /// by the admin's generic-FK link renderer.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn by_id_pool(pool: &crate::sql::Pool, id: i64) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter("id", crate::core::Op::Eq, SqlValue::I64(id))
            .limit(1)
            .fetch_pool(pool)
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Tri-dialect counterpart of [`Self::for_model`] (v0.38).
    /// Resolves `T`'s registered ContentType row via the natural
    /// `(app_label, model_name)` key — same lookup logic, but the
    /// query goes through [`crate::sql::FetcherPool::fetch_pool`]
    /// so the call works on PG / SQLite / MySQL.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn for_model_pool<T: crate::core::Model>(
        pool: &crate::sql::Pool,
    ) -> Result<Option<Self>, ExecError> {
        let entry = inventory::iter::<ModelEntry>
            .into_iter()
            .find(|e| e.schema.table == T::SCHEMA.table)
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            })?;
        let app = entry.resolved_app_label().unwrap_or("project");
        let name = T::SCHEMA.name.to_ascii_lowercase();
        Self::by_natural_key_pool(pool, app, &name).await
    }

    /// Batch counterpart of [`Self::by_natural_key_pool`] — issue #35.
    /// Resolves a list of `(app_label, model_name)` pairs to their
    /// `ContentType` rows in a **single** DB round trip, returning a
    /// `HashMap` keyed by the natural pair (cloned strings). Pairs
    /// that don't have a row in the table are simply omitted from
    /// the map (no error — same shape Django's `get_for_models`
    /// gives back when a model isn't migrated yet).
    ///
    /// Implemented as `all_pool` + a Rust-side filter — the
    /// `rustango_content_types` table is O(dozens) of rows in
    /// realistic apps, so one un-filtered fetch is cheaper than N
    /// round trips OR a complex composite-key `WHERE`. If your
    /// catalog ever grows past ~1K rows this can be tightened to a
    /// per-pair OR-chain without touching the public API.
    ///
    /// ```ignore
    /// use rustango::contenttypes::ContentType;
    ///
    /// let cts = ContentType::for_models_pool(
    ///     &pool,
    ///     [("blog", "post"), ("blog", "author"), ("auth", "user")],
    /// ).await?;
    /// let post_ct = cts.get(&("blog".to_string(), "post".to_string()));
    /// ```
    ///
    /// # Errors
    /// As [`Self::all_pool`].
    pub async fn for_models_pool<I>(
        pool: &crate::sql::Pool,
        pairs: I,
    ) -> Result<std::collections::HashMap<(String, String), Self>, ExecError>
    where
        I: IntoIterator,
        I::Item: Into<(String, String)>,
    {
        let wanted: std::collections::HashSet<(String, String)> =
            pairs.into_iter().map(Into::into).collect();
        if wanted.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let all = Self::all_pool(pool).await?;
        let mut out =
            std::collections::HashMap::<(String, String), Self>::with_capacity(wanted.len());
        for ct in all {
            let key = (ct.app_label.clone(), ct.model_name.clone());
            if wanted.contains(&key) {
                out.insert(key, ct);
            }
        }
        Ok(out)
    }

    /// Tri-dialect counterpart of [`Self::all`] (v0.38) — every
    /// registered ContentType ordered by `(app_label, model_name)`,
    /// across any backend the [`crate::sql::Pool`] enum carries.
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn all_pool(pool: &crate::sql::Pool) -> Result<Vec<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .order_by(&[("app_label", false), ("model_name", false)])
            .fetch_pool(pool)
            .await?;
        Ok(rows)
    }
}

// ============================================================ #35 — in-process cache

/// Process-wide cache of `(app_label, model_name) → ContentType`.
/// Read-mostly: populated lazily by [`ContentType::for_model_cached_pool`]
/// / [`ContentType::by_natural_key_cached_pool`], invalidated wholesale
/// by [`clear_cache`]. The cache is keyed by the natural key, not by
/// Rust `TypeId`, so cached entries survive across tenants /
/// rebinds / hot-reloads — the bottleneck is the DB lookup, not the
/// model registry walk.
///
/// The cache stays small in practice: realistic apps have O(dozens)
/// of ContentTypes. We don't bother with an LRU — once populated,
/// every lookup is a `HashMap` hit, and `clear_cache()` covers the
/// "I just migrated, force a refetch" case.
fn cache() -> &'static std::sync::RwLock<std::collections::HashMap<(String, String), ContentType>> {
    static CACHE: std::sync::OnceLock<
        std::sync::RwLock<std::collections::HashMap<(String, String), ContentType>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Empty the per-process [`ContentType`] cache. Call after migrations
/// add new model rows OR after `ensure_seeded` runs on a brand-new
/// DB so the next lookup re-reads from the DB instead of returning
/// `None` (the negative result isn't cached, but a stale positive
/// entry could mask a re-seeded row's new id).
///
/// Issue #35 — matches Django's `ContentType.objects.clear_cache()`.
pub fn clear_cache() {
    let mut w = cache().write().unwrap_or_else(|e| e.into_inner());
    w.clear();
}

impl ContentType {
    /// Cached counterpart of [`Self::by_natural_key_pool`] — issue #35.
    /// First call for a given `(app_label, model_name)` hits the DB;
    /// subsequent calls return the cached row. `Ok(None)` results
    /// are **not** cached (so a follow-up `ensure_seeded` doesn't
    /// leave you with a stale negative).
    ///
    /// # Errors
    /// As [`Self::by_natural_key_pool`].
    pub async fn by_natural_key_cached_pool(
        pool: &crate::sql::Pool,
        app_label: &str,
        model_name: &str,
    ) -> Result<Option<Self>, ExecError> {
        let key = (app_label.to_owned(), model_name.to_owned());
        // Fast path — cache hit.
        if let Some(hit) = cache()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(Some(hit));
        }
        // Miss → DB lookup, populate on success.
        match Self::by_natural_key_pool(pool, app_label, model_name).await? {
            Some(ct) => {
                cache()
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, ct.clone());
                Ok(Some(ct))
            }
            None => Ok(None),
        }
    }

    /// Cached counterpart of [`Self::for_model_pool`] — issue #35.
    /// Resolves `T` to its natural key via the model registry, then
    /// delegates to [`Self::by_natural_key_cached_pool`].
    ///
    /// # Errors
    /// As [`Self::for_model_pool`].
    pub async fn for_model_cached_pool<T: crate::core::Model>(
        pool: &crate::sql::Pool,
    ) -> Result<Option<Self>, ExecError> {
        let entry = inventory::iter::<ModelEntry>
            .into_iter()
            .find(|e| e.schema.table == T::SCHEMA.table)
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            })?;
        let app = entry.resolved_app_label().unwrap_or("project");
        let name = T::SCHEMA.name.to_ascii_lowercase();
        Self::by_natural_key_cached_pool(pool, app, &name).await
    }
}

// ============================================================ #89 part B — fetch helpers

/// Look up a single row by ContentType + primary key, returning
/// it as a JSON object keyed by Rust field names. Sidesteps
/// Rust's compile-time-typed row decode (which would require
/// each consumer to know `T: Model` at the call site) by
/// driving the decode entirely from the runtime
/// `inventory::ModelEntry` for the ContentType's table.
///
/// Used by the admin's audit log (resolves `entity_table` +
/// `entity_pk` to the displayable target row), the generic-FK
/// link renderer, and any future "operator clicks a row from a
/// heterogeneous list" UI.
///
/// Returns `Ok(None)` when:
///   - No model is registered at `ct.table` (e.g. CT row points
///     at a table the binary no longer compiles in)
///   - No row exists at the given PK in that table
///
/// `pk` accepts any `Into<SqlValue>` so callers can pass
/// `i64` / `String` / `uuid::Uuid` / etc. — matches the
/// underlying schema's PK type.
///
/// # Errors
/// Driver / query failures from the SELECT.
#[cfg(feature = "postgres")]
pub async fn fetch_row_as_json(
    pool: &PgPool,
    ct: &ContentType,
    pk: impl Into<SqlValue>,
) -> Result<Option<serde_json::Value>, ExecError> {
    fetch_row_as_json_pool(&crate::sql::Pool::Postgres(pool.clone()), ct, pk).await
}

/// Tri-dialect counterpart of [`fetch_row_as_json`] (v0.38). Takes
/// the unified [`crate::sql::Pool`] enum and routes through
/// [`crate::sql::select_one_row_as_json_pool`] so the same body runs
/// against PG / SQLite / MySQL. Used by the admin's audit log + the
/// generic-FK link renderer on every backend.
///
/// # Errors
/// As [`fetch_row_as_json`].
pub async fn fetch_row_as_json_pool(
    pool: &crate::sql::Pool,
    ct: &ContentType,
    pk: impl Into<SqlValue>,
) -> Result<Option<serde_json::Value>, ExecError> {
    use crate::core::{Filter, Op, SelectQuery, WhereExpr};

    // Look up the schema for the CT's table from inventory.
    // Heterogeneous-by-design: a CT row may refer to a model
    // that's no longer compiled in (e.g. an old app got
    // dropped). Return None rather than erroring — the audit
    // log + generic FK consumers want graceful degradation.
    let entry = inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == ct.table.as_str());
    let Some(entry) = entry else {
        return Ok(None);
    };
    let pk_field = entry
        .schema
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: entry.schema.table,
        })?;

    let select_q = SelectQuery {
        model: entry.schema,
        where_clause: WhereExpr::Predicate(Filter {
            column: pk_field.column,
            op: Op::Eq,
            value: pk.into(),
        }),
        search: None,
        joins: vec![],
        order_by: vec![],
        limit: Some(1),
        offset: None,
    };
    let fields: Vec<&'static crate::core::FieldSchema> = entry.schema.scalar_fields().collect();
    crate::sql::select_one_row_as_json_pool(pool, &select_q, &fields).await
}

/// Stream every row of a given ContentType through `f`. Useful
/// for cross-table maintenance (e.g. expire audit rows whose
/// `entity_pk` no longer points at a live row in the target
/// table). Loads in batches of `batch_size` rows so memory
/// stays bounded for large tables.
///
/// `batch_size = 0` clamps to 1; very large values are accepted
/// but Postgres will allocate intermediate buffers proportional
/// to the page so default to 500-1000 for most use cases.
///
/// # Errors
/// Driver / query failures + any `Err` returned by `f` short-
/// circuits the iteration.
#[cfg(feature = "postgres")]
pub async fn for_each_row_of_ct<F>(
    pool: &PgPool,
    ct: &ContentType,
    batch_size: u32,
    f: F,
) -> Result<usize, ExecError>
where
    F: FnMut(serde_json::Value) -> Result<(), ExecError>,
{
    for_each_row_of_ct_pool(&crate::sql::Pool::Postgres(pool.clone()), ct, batch_size, f).await
}

/// Tri-dialect counterpart of [`for_each_row_of_ct`] (v0.38). Iterates
/// the rows in batches via [`crate::sql::select_rows_as_json_pool`] so
/// the same body runs across every backend the [`crate::sql::Pool`]
/// enum carries. `batch_size = 0` clamps to 1.
///
/// # Errors
/// As [`for_each_row_of_ct`].
pub async fn for_each_row_of_ct_pool<F>(
    pool: &crate::sql::Pool,
    ct: &ContentType,
    batch_size: u32,
    mut f: F,
) -> Result<usize, ExecError>
where
    F: FnMut(serde_json::Value) -> Result<(), ExecError>,
{
    use crate::core::{OrderClause, SelectQuery, WhereExpr};

    let entry = inventory::iter::<ModelEntry>
        .into_iter()
        .find(|e| e.schema.table == ct.table.as_str());
    let Some(entry) = entry else {
        return Ok(0);
    };
    let pk_field = entry
        .schema
        .primary_key()
        .ok_or(ExecError::MissingPrimaryKey {
            table: entry.schema.table,
        })?;
    let batch = batch_size.max(1) as i64;
    let fields: Vec<&'static crate::core::FieldSchema> = entry.schema.scalar_fields().collect();

    let mut visited = 0_usize;
    let mut offset = 0_i64;
    loop {
        let select_q = SelectQuery {
            model: entry.schema,
            where_clause: WhereExpr::And(vec![]),
            search: None,
            joins: vec![],
            order_by: vec![OrderClause {
                column: pk_field.column,
                desc: false,
            }],
            limit: Some(batch),
            offset: Some(offset),
        };
        let rows = crate::sql::select_rows_as_json_pool(pool, &select_q, &fields).await?;
        if rows.is_empty() {
            break;
        }
        let n = rows.len() as i64;
        for json in rows {
            f(json)?;
            visited += 1;
        }
        if n < batch {
            break;
        }
        offset += batch;
    }
    Ok(visited)
}

/// "Generic foreign key" pair — a runtime pointer at any registered
/// model's row, formed by `(content_type_id, object_pk)`. Sub-slice
/// F.3 of the v0.15.0 ContentType plan.
///
/// Models embed it as two plain columns plus this struct used at
/// the API surface — there's no dedicated SQL type for "generic
/// FK", just the convention that `content_type_id` references
/// `rustango_content_types.id` and `object_pk` is the target row's
/// primary key value. The framework hydrates targets via
/// [`prefetch_generic`].
///
/// # Why not a typed `T: Model` field?
///
/// The whole point of the generic shape is that the target type
/// isn't known at compile time — a single audit log row, comment,
/// activity-stream entry, or tag can point at any model. Typed FKs
/// (`ForeignKey<User>`) are the right choice when the target type
/// is fixed; `GenericForeignKey` is for the "could be anything"
/// case Django's `contenttypes` framework solves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenericForeignKey {
    /// FK to `rustango_content_types.id`. Identifies which model
    /// the `object_pk` lives in.
    pub content_type_id: i64,
    /// Primary key of the target row in the table named by the
    /// matching ContentType. Always `i64` — the framework's
    /// `Auto<T>` PK is `i64` and non-Auto PKs that ride this path
    /// must be i64-coercible.
    pub object_pk: i64,
}

impl GenericForeignKey {
    /// Construct a GFK from a ContentType row + a target PK. The
    /// natural shape when the caller already has the ContentType
    /// in hand (audit log writer, comment-create handler, etc.).
    #[must_use]
    pub const fn new(content_type_id: i64, object_pk: i64) -> Self {
        Self {
            content_type_id,
            object_pk,
        }
    }

    /// Construct a GFK pointing at a row of `T`. Looks up `T`'s
    /// ContentType via the cached registry (`for_model::<T>`) — one
    /// DB round-trip the first time, free thereafter once
    /// memoization lands.
    ///
    /// # Errors
    /// As [`ContentType::for_model`].
    #[cfg(feature = "postgres")]
    pub async fn for_target<T: crate::core::Model>(
        pool: &PgPool,
        object_pk: i64,
    ) -> Result<Self, ExecError> {
        Self::for_target_pool::<T>(&crate::sql::Pool::Postgres(pool.clone()), object_pk).await
    }

    /// Tri-dialect counterpart of [`Self::for_target`] (v0.38).
    /// Routes the ContentType lookup through
    /// [`ContentType::for_model_pool`] so the same call site works on
    /// PG / SQLite / MySQL.
    ///
    /// # Errors
    /// As [`Self::for_target`].
    pub async fn for_target_pool<T: crate::core::Model>(
        pool: &crate::sql::Pool,
        object_pk: i64,
    ) -> Result<Self, ExecError> {
        let ct = ContentType::for_model_pool::<T>(pool)
            .await?
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            })?;
        let id = ct
            .id
            .get()
            .copied()
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: ContentType::SCHEMA.table,
            })?;
        Ok(Self::new(id, object_pk))
    }
}

/// Render a `GenericForeignKey` value as a clickable HTML link
/// pointing at the target row's admin detail page. Used by the
/// admin list/detail views (sub-slice F.4) when a model declares
/// `#[rustango(generic_fk(...))]`.
///
/// Resolves the ContentType via [`ContentType::by_id`] to find the
/// target table name + a human label, builds a relative URL of the
/// form `/<target_table>/<object_pk>` (matches the auto-admin's
/// per-table route shape), and returns escaped HTML. Returns the
/// raw "(ct=…, pk=…)" text fallback if the ContentType lookup fails
/// (e.g. CT not yet seeded, table dropped, etc.) so the admin
/// degrades gracefully instead of crashing.
///
/// Output is HTML-safe: both `app_label.model_name` and the link
/// path are HTML-entity-escaped via the same routine the existing
/// per-FK renderer uses.
///
/// # Errors
/// Driver / SQL failures from the ContentType lookup. Caller can
/// `unwrap_or_else(|_| ...)` to a fallback rendering.
#[cfg(feature = "postgres")]
pub async fn render_generic_fk_link(
    pool: &PgPool,
    gfk: GenericForeignKey,
) -> Result<String, ExecError> {
    let escape = |s: &str| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
    };
    let ct = match ContentType::by_id(pool, gfk.content_type_id).await? {
        Some(c) => c,
        None => {
            // Stale or unseeded reference — show the raw pair so
            // the admin operator can spot it.
            return Ok(format!(
                "<em>(ct={}, pk={})</em>",
                gfk.content_type_id, gfk.object_pk
            ));
        }
    };
    let label = format!("{}.{}", ct.app_label, ct.model_name);
    let table_esc = escape(&ct.table);
    let label_esc = escape(&label);
    Ok(format!(
        r#"<a href="/{table}/{pk}">{label} #{pk}</a>"#,
        table = table_esc,
        pk = gfk.object_pk,
        label = label_esc,
    ))
}

/// v0.37 — backend-agnostic counterpart of [`render_generic_fk_link`].
/// Routes the ContentType lookup through [`ContentType::by_id_pool`]
/// so admin detail views render generic-FK links on any backend.
///
/// # Errors
/// As [`render_generic_fk_link`].
pub async fn render_generic_fk_link_pool(
    pool: &crate::sql::Pool,
    gfk: GenericForeignKey,
) -> Result<String, ExecError> {
    let escape = |s: &str| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
    };
    let ct = match ContentType::by_id_pool(pool, gfk.content_type_id).await? {
        Some(c) => c,
        None => {
            return Ok(format!(
                "<em>(ct={}, pk={})</em>",
                gfk.content_type_id, gfk.object_pk
            ));
        }
    };
    let label = format!("{}.{}", ct.app_label, ct.model_name);
    let table_esc = escape(&ct.table);
    let label_esc = escape(&label);
    Ok(format!(
        r#"<a href="/{table}/{pk}">{label} #{pk}</a>"#,
        table = table_esc,
        pk = gfk.object_pk,
        label = label_esc,
    ))
}

/// Soft-FK prefetch — fetch every row of `C` whose soft-FK column
/// matches one of `parent_pks`, then group the results by the
/// soft-FK value via the caller-supplied extractor closure. Returns
/// a `HashMap<i64, Vec<C>>` keyed on the soft-FK value, mirroring
/// the shape of [`crate::sql::fetch_with_prefetch`] but for
/// columns that aren't declared as a real `Relation::Fk` (no DDL FK
/// constraint, no macro-generated reverse helper).
///
/// "Soft FK" = an integer column that conceptually points at
/// another model's PK without a declared FK relation. Use cases:
/// optional cross-app references, migration-period references where
/// the constraint can't be enforced yet, audit-log `entity_pk` columns,
/// `denormalized_user_id` snapshots, etc.
///
/// Two SQL round trips total max (one for the prefetch + the parent
/// fetch the caller already did). Empty `parent_pks` short-circuits
/// to an empty map without a round trip.
///
/// ```ignore
/// // After fetching parents:
/// let parent_pks: Vec<i64> = posts.iter().map(|p| p.id.get().copied().unwrap()).collect();
/// let by_post: HashMap<i64, Vec<Comment>> = prefetch_soft::<Comment, _>(
///     &pool,
///     &parent_pks,
///     "post_id",        // the soft-FK column on the Comment table
///     |c| c.post_id,    // extractor: how to read the value off &Comment
/// ).await?;
/// for post in &posts {
///     let comments = by_post.get(&post.id.get().copied().unwrap())
///         .map(Vec::as_slice).unwrap_or(&[]);
///     // ...
/// }
/// ```
///
/// # Errors
/// Driver / SQL failures from the SELECT.
#[cfg(feature = "postgres")]
pub async fn prefetch_soft<C, F>(
    pool: &PgPool,
    parent_pks: &[i64],
    target_fk_column: &'static str,
    extract: F,
) -> Result<::std::collections::HashMap<i64, Vec<C>>, ExecError>
where
    C: crate::core::Model
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + Send
        + Unpin
        + 'static,
    F: Fn(&C) -> i64,
{
    if parent_pks.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    // Dedupe to keep the IN list compact — duplicate parent PKs
    // would otherwise pad the SQL string and bind list pointlessly.
    let mut keys: Vec<i64> = parent_pks.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let pk_values: Vec<crate::core::SqlValue> = keys
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter(
            target_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(pk_values),
        )
        .fetch(pool)
        .await?;
    let mut grouped: ::std::collections::HashMap<i64, Vec<C>> = ::std::collections::HashMap::new();
    for child in children {
        let key = extract(&child);
        grouped.entry(key).or_default().push(child);
    }
    Ok(grouped)
}

/// Generic-FK prefetch — given a list of `(content_type_id, object_pk)`
/// pairs (typically pulled off a parent set's `GenericForeignKey`
/// fields), batches one SQL per distinct ContentType to fetch the
/// targets, returns a `HashMap<(i64, i64), C>` keyed on the
/// (ct_id, pk) pair.
///
/// **Single-target-type variant** — caller supplies the concrete
/// `C: Model` they want hydrated. Filters out any pair whose
/// `content_type_id` doesn't match `C`'s ContentType (the framework
/// can't decode a `Photo` row into a `Comment`). For mixed-target
/// hydration (one query → many target types) you'd need the
/// boxed-trait dynamic decoder registry — that's a follow-up
/// (`prefetch_generic_dyn`) once the registry trait is in.
///
/// Two SQL round trips total (one ContentType lookup + one target
/// fetch). Empty input short-circuits to an empty map.
///
/// ```ignore
/// let pairs: Vec<(i64, i64)> = audit_rows.iter()
///     .map(|a| (a.target.content_type_id, a.target.object_pk))
///     .collect();
/// let posts: HashMap<(i64, i64), Post> =
///     prefetch_generic::<Post>(&pool, &pairs).await?;
/// ```
///
/// # Errors
/// As [`ContentType::for_model`] + driver / SQL failures from the
/// target SELECT.
#[cfg(feature = "postgres")]
pub async fn prefetch_generic<C>(
    pool: &PgPool,
    pairs: &[(i64, i64)],
) -> Result<::std::collections::HashMap<(i64, i64), C>, ExecError>
where
    C: crate::core::Model
        + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>
        + crate::sql::HasPkValue
        + Send
        + Unpin
        + 'static,
{
    if pairs.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    // Resolve C's ContentType once — every (ct_id, pk) pair whose
    // ct_id doesn't match drops out of the result map (caller's
    // expectation: typed-target prefetch).
    let target_ct =
        ContentType::for_model::<C>(pool)
            .await?
            .ok_or_else(|| ExecError::MissingPrimaryKey {
                table: C::SCHEMA.table,
            })?;
    let target_ct_id = target_ct
        .id
        .get()
        .copied()
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: ContentType::SCHEMA.table,
        })?;

    let mut wanted_pks: Vec<i64> = pairs
        .iter()
        .filter(|(ct, _)| *ct == target_ct_id)
        .map(|(_, pk)| *pk)
        .collect();
    if wanted_pks.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    wanted_pks.sort_unstable();
    wanted_pks.dedup();

    let pk_values: Vec<crate::core::SqlValue> = wanted_pks
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    let pk_field = C::SCHEMA
        .primary_key()
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: C::SCHEMA.table,
        })?;
    let rows: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter(
            pk_field.column,
            crate::core::Op::In,
            crate::core::SqlValue::List(pk_values),
        )
        .fetch(pool)
        .await?;

    let mut out: ::std::collections::HashMap<(i64, i64), C> =
        ::std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        // Pull the row's PK back out via the macro-generated
        // __rustango_pk_value path through HasPkValue.
        let pk_value = <C as crate::sql::HasPkValue>::__rustango_pk_value_impl(&row);
        if let crate::core::SqlValue::I64(pk) = pk_value {
            out.insert((target_ct_id, pk), row);
        }
    }
    Ok(out)
}

/// Tri-dialect counterpart of [`prefetch_soft`] (v0.38). Same shape
/// — one batched SELECT against `target_fk_column`, grouped by
/// parent PK — but routes through [`crate::sql::FetcherPool::fetch_pool`]
/// so the bound is `C: Model + MaybePgFromRow + MaybeMyFromRow +
/// MaybeSqliteFromRow + …` (all auto-implemented by `#[derive(Model)]`).
///
/// # Errors
/// As [`prefetch_soft`].
pub async fn prefetch_soft_pool<C, F>(
    pool: &crate::sql::Pool,
    parent_pks: &[i64],
    target_fk_column: &'static str,
    extract: F,
) -> Result<::std::collections::HashMap<i64, Vec<C>>, ExecError>
where
    C: crate::core::Model
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + Send
        + Unpin
        + 'static,
    F: Fn(&C) -> i64,
{
    use crate::sql::FetcherPool as _;
    if parent_pks.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    let mut keys: Vec<i64> = parent_pks.to_vec();
    keys.sort_unstable();
    keys.dedup();
    let pk_values: Vec<crate::core::SqlValue> = keys
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    let children: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter(
            target_fk_column,
            crate::core::Op::In,
            crate::core::SqlValue::List(pk_values),
        )
        .fetch_pool(pool)
        .await?;
    let mut grouped: ::std::collections::HashMap<i64, Vec<C>> = ::std::collections::HashMap::new();
    for child in children {
        let key = extract(&child);
        grouped.entry(key).or_default().push(child);
    }
    Ok(grouped)
}

/// Tri-dialect counterpart of [`prefetch_generic`] (v0.38). Routes the
/// ContentType lookup through [`ContentType::for_model_pool`] and the
/// target SELECT through [`crate::sql::FetcherPool::fetch_pool`] so
/// the same body works on PG / SQLite / MySQL.
///
/// # Errors
/// As [`prefetch_generic`].
pub async fn prefetch_generic_pool<C>(
    pool: &crate::sql::Pool,
    pairs: &[(i64, i64)],
) -> Result<::std::collections::HashMap<(i64, i64), C>, ExecError>
where
    C: crate::core::Model
        + crate::sql::MaybePgFromRow
        + crate::sql::MaybeMyFromRow
        + crate::sql::MaybeSqliteFromRow
        + crate::sql::LoadRelated
        + crate::sql::MaybeMyLoadRelated
        + crate::sql::MaybeSqliteLoadRelated
        + crate::sql::HasPkValue
        + Send
        + Unpin
        + 'static,
{
    use crate::sql::FetcherPool as _;
    if pairs.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    let target_ct = ContentType::for_model_pool::<C>(pool)
        .await?
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: C::SCHEMA.table,
        })?;
    let target_ct_id = target_ct
        .id
        .get()
        .copied()
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: ContentType::SCHEMA.table,
        })?;

    let mut wanted_pks: Vec<i64> = pairs
        .iter()
        .filter(|(ct, _)| *ct == target_ct_id)
        .map(|(_, pk)| *pk)
        .collect();
    if wanted_pks.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    wanted_pks.sort_unstable();
    wanted_pks.dedup();

    let pk_values: Vec<crate::core::SqlValue> = wanted_pks
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    let pk_field = C::SCHEMA
        .primary_key()
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: C::SCHEMA.table,
        })?;
    let rows: Vec<C> = crate::query::QuerySet::<C>::new()
        .filter(
            pk_field.column,
            crate::core::Op::In,
            crate::core::SqlValue::List(pk_values),
        )
        .fetch_pool(pool)
        .await?;

    let mut out: ::std::collections::HashMap<(i64, i64), C> =
        ::std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let pk_value = <C as crate::sql::HasPkValue>::__rustango_pk_value_impl(&row);
        if let crate::core::SqlValue::I64(pk) = pk_value {
            out.insert((target_ct_id, pk), row);
        }
    }
    Ok(out)
}

/// SQL that creates the `rustango_content_types` table + the
/// `(app_label, model_name)` UNIQUE index. Idempotent
/// (`IF NOT EXISTS`). Mounted as a runtime ensure-table so the
/// registry pool (whose bootstrap migration only creates
/// `rustango_orgs` + `rustango_operators`) gets the table without
/// a separate migration JSON. Tenant schemas already get this
/// table via the bootstrap migration's CreateTable op for every
/// registered model.
#[cfg(feature = "postgres")]
const CREATE_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS "rustango_content_types" (
    "id"          BIGSERIAL PRIMARY KEY,
    "app_label"   VARCHAR(100) NOT NULL,
    "model_name"  VARCHAR(100) NOT NULL,
    "table"       VARCHAR(100) NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS "rustango_content_types_natural_key"
    ON "rustango_content_types" ("app_label", "model_name");
"#;

// v0.34 — note: the legacy `CREATE_TABLE_SQL` Postgres constant above
// is kept for `ensure_table(&PgPool)` back-compat. The bi-dialect
// `ensure_table_pool` below no longer carries hand-rolled per-backend
// DDL constants — it routes through
// [`crate::migrate::ddl::create_table_if_not_exists_sql_with_dialect`]
// which already knows how to emit the table for any backend rustango
// supports. The UNIQUE INDEX on `(app_label, model_name)` is still
// hand-emitted because `crate::migrate::diff::render_changes` (the
// index renderer in the migration system) is currently Postgres-only;
// when index emission goes bi-dialect that block collapses too.

/// Ensure `rustango_content_types` exists in `pool`'s database /
/// schema. No-op when already present (`IF NOT EXISTS`). Used by
/// [`ensure_seeded`] internally + callable directly for any
/// registry-pool wiring that needs the table without a row walk.
///
/// Splits the statement on `;` because Postgres' simple-prepare
/// path rejects multiple statements in one prepared call —
/// each `CREATE TABLE` / `CREATE INDEX` runs as its own round-trip.
///
/// # Errors
/// Driver / SQL failures.
#[cfg(feature = "postgres")]
pub async fn ensure_table(pool: &PgPool) -> Result<(), crate::sql::sqlx::Error> {
    for stmt in CREATE_TABLE_SQL.split(';') {
        let trimmed = stmt.trim();
        if trimmed.is_empty() {
            continue;
        }
        crate::sql::sqlx::query(trimmed).execute(pool).await?;
    }
    Ok(())
}

/// Walk the inventory of registered models and INSERT a ContentType
/// row for every one missing. Idempotent.
///
/// The `ContentType` table itself (the one this function writes
/// into) is excluded from the walk — bootstrapping its own row
/// would be circular and meaningless. Callers don't need to
/// special-case it.
///
/// Run once after `migrate(&pool, dir).await?` at app startup, or
/// on demand from a `manage seed-content-types` verb (F.2 follow-up).
///
/// # Errors
/// Driver / query failures from the SELECT-or-INSERT loop.
#[cfg(feature = "postgres")]
pub async fn ensure_seeded(pool: &PgPool) -> Result<usize, ExecError> {
    // Idempotent table-create — ensures the registry-side CT
    // catalog has its physical home before we walk inventory.
    // Tenant schemas already get this from the bootstrap-migration
    // CreateTable op; the registry bootstrap doesn't include
    // `rustango_content_types`, so the seed call would otherwise
    // 42P01 on every fresh registry. Mirrors the
    // `audit::ensure_table(registry)` pattern.
    ensure_table(pool).await.map_err(ExecError::Driver)?;
    let mut inserted = 0_usize;
    for entry in inventory::iter::<ModelEntry> {
        let table = entry.schema.table;
        // Don't seed a row for the ContentType table itself — would
        // be circular and meaningless.
        if table == ContentType::SCHEMA.table {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("project").to_owned();
        let name = entry.schema.name.to_ascii_lowercase();
        // Probe natural key first; skip if already seeded.
        if ContentType::by_natural_key(pool, &app, &name)
            .await?
            .is_some()
        {
            continue;
        }
        let mut row = ContentType {
            id: Auto::Unset,
            app_label: app,
            model_name: name,
            table: table.to_owned(),
        };
        row.insert(pool).await?;
        inserted += 1;
    }
    Ok(inserted)
}

// ============================================================ v0.34 bi-dialect

/// Bootstrap the `rustango_content_types` table against any
/// rustango-supported backend. Dispatches the per-dialect DDL through
/// the [`crate::sql::Pool`] enum — Postgres / MySQL / SQLite variants
/// are picked at runtime from `pool.dialect().name()`.
///
/// Mirrors the [`crate::audit::ensure_table_pool`] pattern; this is
/// the abstraction the framework's registry-bootstrap path should
/// prefer over the PG-only [`ensure_table`].
///
/// # Errors
/// Driver / SQL failures from `CREATE TABLE IF NOT EXISTS`.
pub async fn ensure_table_pool(pool: &crate::sql::Pool) -> Result<(), crate::sql::sqlx::Error> {
    use crate::core::Model as _;
    let dialect = pool.dialect();
    // v0.34 — route the table DDL through the same emitter the
    // migration runner uses (`create_table_if_not_exists_sql_with_dialect`).
    // Type names, identifier quoting and `Auto<T>` serial spelling all
    // match what `make_migrations` would produce — keeps a single
    // source of truth for "what does ContentType look like on this
    // backend".
    let table_ddl = crate::migrate::ddl::create_table_if_not_exists_sql_with_dialect(
        dialect,
        &ContentType::SCHEMA,
    );
    exec_one(pool, &table_ddl).await?;

    // UNIQUE INDEX on the natural key. PG + SQLite support
    // `CREATE UNIQUE INDEX IF NOT EXISTS`; MySQL doesn't — we drop
    // the `IF NOT EXISTS` clause there and swallow the dup-index
    // error (1061) so re-runs stay idempotent. Hand-rolled until
    // `crate::migrate::diff::render_changes` goes bi-dialect; once
    // it does, this collapses to a `SchemaChange::CreateIndex` op.
    let table_q = dialect.quote_ident("rustango_content_types");
    let idx_q = dialect.quote_ident("rustango_content_types_natural_key");
    let col_app = dialect.quote_ident("app_label");
    let col_name = dialect.quote_ident("model_name");
    let supports_if_not_exists = matches!(dialect.name(), "postgres" | "sqlite");
    let index_ddl = if supports_if_not_exists {
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {idx_q} ON {table_q} ({col_app}, {col_name})")
    } else {
        format!("CREATE UNIQUE INDEX {idx_q} ON {table_q} ({col_app}, {col_name})")
    };
    match exec_one(pool, &index_ddl).await {
        Ok(()) => Ok(()),
        Err(e) if dialect.name() == "mysql" && is_mysql_dup_index_error(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Execute a single SQL statement against `pool`, dispatching on the
/// backend variant. Internal bootstrap helper — production code
/// should reach for the ORM (`fetch_pool` / `insert_pool` / …).
async fn exec_one(pool: &crate::sql::Pool, sql: &str) -> Result<(), crate::sql::sqlx::Error> {
    match pool {
        #[cfg(feature = "postgres")]
        crate::sql::Pool::Postgres(pg) => {
            crate::sql::sqlx::query(sql).execute(pg).await?;
        }
        #[cfg(feature = "mysql")]
        crate::sql::Pool::Mysql(my) => {
            crate::sql::sqlx::query(sql).execute(my).await?;
        }
        #[cfg(feature = "sqlite")]
        crate::sql::Pool::Sqlite(sq) => {
            crate::sql::sqlx::query(sql).execute(sq).await?;
        }
    }
    Ok(())
}

#[cfg(feature = "mysql")]
fn is_mysql_dup_index_error(e: &crate::sql::sqlx::Error) -> bool {
    if let crate::sql::sqlx::Error::Database(db) = e {
        return db.code().as_deref() == Some("42000")
            || db.message().contains("Duplicate key name");
    }
    false
}

#[cfg(not(feature = "mysql"))]
#[allow(dead_code)]
fn is_mysql_dup_index_error(_e: &crate::sql::sqlx::Error) -> bool {
    false
}

/// Backend-agnostic counterpart of [`ensure_seeded`]. Walks the
/// inventory of registered models and INSERTs a `ContentType` row for
/// every missing one — but routes the table-bootstrap +
/// natural-key probe + insert through the [`crate::sql::Pool`] enum,
/// so sqlite/mysql registries work without the PG-only [`ensure_seeded`]
/// blowing up at runtime.
///
/// Idempotent: re-running on a populated DB inserts nothing and
/// returns `Ok(0)`.
///
/// # Errors
/// Driver / query failures from the SELECT-or-INSERT loop or the
/// table bootstrap.
pub async fn ensure_seeded_pool(pool: &crate::sql::Pool) -> Result<usize, ExecError> {
    ensure_table_pool(pool).await.map_err(ExecError::Driver)?;
    let mut inserted = 0_usize;
    for entry in inventory::iter::<ModelEntry> {
        let table = entry.schema.table;
        if table == ContentType::SCHEMA.table {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("project").to_owned();
        let name = entry.schema.name.to_ascii_lowercase();
        if ContentType::by_natural_key_pool(pool, &app, &name)
            .await?
            .is_some()
        {
            continue;
        }
        let mut row = ContentType {
            id: Auto::Unset,
            app_label: app,
            model_name: name,
            table: table.to_owned(),
        };
        row.insert_pool(pool).await?;
        inserted += 1;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_schema_has_expected_columns() {
        let s = ContentType::SCHEMA;
        assert_eq!(s.table, "rustango_content_types");
        let cols: Vec<&str> = s.fields.iter().map(|f| f.column).collect();
        assert!(cols.contains(&"id"));
        assert!(cols.contains(&"app_label"));
        assert!(cols.contains(&"model_name"));
        assert!(cols.contains(&"table"));
    }

    #[test]
    fn content_type_id_is_auto() {
        let pk = ContentType::SCHEMA
            .primary_key()
            .expect("ContentType has a PK");
        assert_eq!(pk.column, "id");
        assert!(pk.auto, "ContentType.id should be Auto<i64>");
    }
}
