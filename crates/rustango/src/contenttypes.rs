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
    pub async fn by_natural_key(
        pool: &crate::sql::Pool,
        app_label: &str,
        model_name: &str,
    ) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter_op(
                "app_label",
                crate::core::Op::Eq,
                SqlValue::String(app_label.into()),
            )
            .filter_op(
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
    pub async fn by_id(pool: &crate::sql::Pool, id: i64) -> Result<Option<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .filter_op("id", crate::core::Op::Eq, SqlValue::I64(id))
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
    pub async fn for_model<T: crate::core::Model>(
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
        Self::by_natural_key(pool, app, &name).await
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
    /// // String literals work directly — no `.to_string()` needed.
    /// let cts = ContentType::get_for_models(
    ///     &pool,
    ///     [("blog", "post"), ("blog", "author"), ("auth", "user")],
    /// ).await?;
    /// let post_ct = cts.get(&("blog".to_string(), "post".to_string()));
    /// ```
    ///
    /// # Errors
    /// As [`Self::all_pool`].
    pub async fn get_for_models<A, B, I>(
        pool: &crate::sql::Pool,
        pairs: I,
    ) -> Result<std::collections::HashMap<(String, String), Self>, ExecError>
    where
        I: IntoIterator<Item = (A, B)>,
        A: Into<String>,
        B: Into<String>,
    {
        let wanted: std::collections::HashSet<(String, String)> = pairs
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        if wanted.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let all = Self::all(pool).await?;
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
    pub async fn all(pool: &crate::sql::Pool) -> Result<Vec<Self>, ExecError> {
        let rows: Vec<Self> = Self::objects()
            .order_by(&[("app_label", false), ("model_name", false)])
            .fetch_pool(pool)
            .await?;
        Ok(rows)
    }
}

// ============================================================ #35 — in-process cache

/// Process-wide cache of `(app_label, model_name) → ContentType`.
/// Read-mostly: populated lazily by [`ContentType::get_for_model`]
/// / [`ContentType::get_by_natural_key`], invalidated wholesale
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
    pub async fn get_by_natural_key(
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
        match Self::by_natural_key(pool, app_label, model_name).await? {
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

    /// Cached counterpart of [`Self::for_model`] — issue #35.
    /// Resolves `T` to its natural key via the model registry, then
    /// delegates to [`Self::get_by_natural_key`].
    ///
    /// # Errors
    /// As [`Self::for_model`].
    pub async fn get_for_model<T: crate::core::Model>(
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
        Self::get_by_natural_key(pool, app, &name).await
    }
}

// ============================================================ #89 part B — fetch helpers

/// Tri-dialect counterpart of [`fetch_row_as_json`] (v0.38). Takes
/// the unified [`crate::sql::Pool`] enum and routes through
/// [`crate::sql::select_one_row_as_json`] so the same body runs
/// against PG / SQLite / MySQL. Used by the admin's audit log + the
/// generic-FK link renderer on every backend.
///
/// # Errors
/// As [`fetch_row_as_json`].
pub async fn fetch_row_as_json(
    pool: &crate::sql::Pool,
    ct: &ContentType,
    pk: impl Into<SqlValue>,
) -> Result<Option<serde_json::Value>, ExecError> {
    use crate::core::SelectQuery;

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

    // #562 — by_pk constructor for single-PK-lookup shape.
    let select_q = SelectQuery::by_pk(entry.schema, pk_field.column, pk.into());
    let fields: Vec<&'static crate::core::FieldSchema> = entry.schema.scalar_fields().collect();
    crate::sql::select_one_row_as_json(pool, &select_q, &fields).await
}

/// Tri-dialect counterpart of [`for_each_row_of_ct`] (v0.38). Iterates
/// the rows in batches via [`crate::sql::select_rows_as_json`] so
/// the same body runs across every backend the [`crate::sql::Pool`]
/// enum carries. `batch_size = 0` clamps to 1.
///
/// # Errors
/// As [`for_each_row_of_ct`].
pub async fn for_each_row_of_ct<F>(
    pool: &crate::sql::Pool,
    ct: &ContentType,
    batch_size: u32,
    mut f: F,
) -> Result<usize, ExecError>
where
    F: FnMut(serde_json::Value) -> Result<(), ExecError>,
{
    use crate::core::{OrderClause, SelectQuery};

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
        // #562 — struct-update over SelectQuery::new for the
        // paginated full-table scan (no WHERE).
        let select_q = SelectQuery {
            order_by: vec![OrderClause {
                column: pk_field.column,
                desc: false,
            }
            .into()],
            limit: Some(batch),
            offset: Some(offset),
            ..SelectQuery::new(entry.schema)
        };
        let rows = crate::sql::select_rows_as_json(pool, &select_q, &fields).await?;
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

    /// Tri-dialect counterpart of [`Self::for_target`] (v0.38).
    /// Routes the ContentType lookup through the cached
    /// [`ContentType::get_for_model`] so subsequent calls for the
    /// same model type hit the process-wide cache instead of the DB.
    ///
    /// # Errors
    /// As [`Self::for_target`].
    pub async fn for_target<T: crate::core::Model>(
        pool: &crate::sql::Pool,
        object_pk: i64,
    ) -> Result<Self, ExecError> {
        let ct = ContentType::get_for_model::<T>(pool)
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

    /// Resolve the `ContentType` row for this GFK. Wraps
    /// [`ContentType::by_id`] — returns `Ok(None)` when the
    /// ContentType catalog hasn't been seeded or the id is stale.
    ///
    /// Issue #36 — the public entry point for callers that need the
    /// CT metadata (table name, app label, model name) before loading
    /// the related object.
    ///
    /// # Errors
    /// Driver failures from the underlying `SELECT`.
    pub async fn content_type(
        &self,
        pool: &crate::sql::Pool,
    ) -> Result<Option<ContentType>, ExecError> {
        ContentType::by_id(pool, self.content_type_id).await
    }

    /// Load the related object as a JSON map keyed by Rust field
    /// names — issue #36. Equivalent to the Python `gfk.content_object`
    /// descriptor, but since the target type is unknown at compile
    /// time we return a `serde_json::Value` (`Object` shape) instead
    /// of a typed `T: Model`.
    ///
    /// Returns `Ok(None)` gracefully when:
    /// - The ContentType row is stale / not yet seeded.
    /// - No row exists at `self.object_pk` in the target table.
    /// - The target model is no longer registered in the binary's
    ///   inventory (e.g. the app was removed from the project).
    ///
    /// ```ignore
    /// // Assuming TaggedItem { content_type_id, object_pk, tag }:
    /// let item: TaggedItem = ...;
    /// let gfk = GenericForeignKey::new(item.content_type_id, item.object_pk);
    /// if let Some(obj) = gfk.get_object(&pool).await? {
    ///     println!("{}", obj["title"]);   // works for any model with a title
    /// }
    /// ```
    ///
    /// # Errors
    /// Driver failures from ContentType or target-row SELECT.
    pub async fn get_object(
        &self,
        pool: &crate::sql::Pool,
    ) -> Result<Option<serde_json::Value>, ExecError> {
        let Some(ct) = ContentType::by_id(pool, self.content_type_id).await? else {
            return Ok(None);
        };
        fetch_row_as_json(pool, &ct, self.object_pk).await
    }
}

/// v0.37 — backend-agnostic counterpart of [`render_generic_fk_link`].
/// Routes the ContentType lookup through [`ContentType::by_id_pool`]
/// so admin detail views render generic-FK links on any backend.
///
/// # Errors
/// As [`render_generic_fk_link`].
pub async fn render_generic_fk_link(
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
    let ct = match ContentType::by_id(&pool.clone().into(), gfk.content_type_id).await? {
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

/// Tri-dialect counterpart of [`prefetch_soft`] (v0.38). Same shape
/// — one batched SELECT against `target_fk_column`, grouped by
/// parent PK — but routes through [`crate::sql::FetcherPool::fetch_pool`]
/// so the bound is `C: Model + MaybePgFromRow + MaybeMyFromRow +
/// MaybeSqliteFromRow + …` (all auto-implemented by `#[derive(Model)]`).
///
/// # Errors
/// As [`prefetch_soft`].
pub async fn prefetch_soft<C, F>(
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
        .filter_op(
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
/// ContentType lookup through [`ContentType::for_model`] and the
/// target SELECT through [`crate::sql::FetcherPool::fetch_pool`] so
/// the same body works on PG / SQLite / MySQL.
///
/// # Errors
/// As [`prefetch_generic`].
pub async fn prefetch_generic<C>(
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
    let target_ct = ContentType::for_model::<C>(&pool.clone().into())
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
        .filter_op(
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

// ============================================================ Reverse generic relation (issue #37)

/// Fetch all rows of the model described by `child_schema` whose
/// `GenericForeignKey` pair points at `(parent_ct_id, parent_pk)`.
///
/// Reverse-direction counterpart of [`GenericForeignKey::get_object`]
/// — Django's `GenericRelation(...)` field. The polymorphic-target
/// child rows are returned as JSON maps (same shape as
/// [`fetch_row_as_json`]) so callers don't have to commit to a
/// specific typed `Child: Model + Decode<...>` here.
///
/// `relation_name` selects which entry of the child schema's
/// [`crate::core::ModelSchema::generic_relations`] to filter on:
/// - `Some("target")` — match the named relation exactly.
/// - `None` — use the FIRST relation declared on the child. Fine
///   for the common case where a child carries exactly one GFK.
///
/// Returns an empty `Vec` (NOT an error) when:
/// - The child schema has no `generic_relations` declared at all.
/// - The named relation doesn't exist on the child.
/// - No matching rows exist in the child table.
///
/// # Errors
/// Driver / SELECT failures from [`crate::sql::select_rows_as_json`].
pub async fn fetch_reverse_generic(
    pool: &crate::sql::Pool,
    child_schema: &'static crate::core::ModelSchema,
    parent_ct_id: i64,
    parent_pk: i64,
    relation_name: Option<&str>,
) -> Result<Vec<serde_json::Value>, ExecError> {
    use crate::core::{Filter, Op, SelectQuery, WhereExpr};
    // Pick the relation: by name, or the first one declared.
    let rel = match relation_name {
        Some(name) => child_schema
            .generic_relations
            .iter()
            .find(|r| r.name == name),
        None => child_schema.generic_relations.first(),
    };
    let Some(rel) = rel else {
        // No GFK declared on child → no reverse traversal possible.
        // Return empty rather than error; callers using a model
        // that doesn't have a GFK have a bigger problem than a
        // panic deep in the framework.
        return Ok(Vec::new());
    };
    // #562 — composite AND lookup; struct-update over SelectQuery::new.
    let select_q = SelectQuery {
        where_clause: WhereExpr::And(vec![
            WhereExpr::Predicate(Filter {
                column: rel.ct_column,
                op: Op::Eq,
                value: crate::core::SqlValue::I64(parent_ct_id),
            }),
            WhereExpr::Predicate(Filter {
                column: rel.pk_column,
                op: Op::Eq,
                value: crate::core::SqlValue::I64(parent_pk),
            }),
        ]),
        ..SelectQuery::new(child_schema)
    };
    let fields: Vec<&'static crate::core::FieldSchema> = child_schema.scalar_fields().collect();
    crate::sql::select_rows_as_json(pool, &select_q, &fields).await
}

/// Resolve `Parent`'s ContentType automatically and dispatch to
/// [`fetch_reverse_generic`]. The convenient entry point when you
/// have the parent's Rust type in scope:
///
/// ```ignore
/// // Find every TaggedItem pointing at this Post.
/// let tags = reverse_generic_for::<Post>(
///     &pool,
///     TaggedItem::SCHEMA,
///     post.id,
///     None,
/// ).await?;
/// ```
///
/// # Errors
/// - `Parent`'s ContentType not seeded (run
///   [`ensure_seeded`](crate::contenttypes::ensure_seeded) first).
/// - Driver / SELECT failures from [`fetch_reverse_generic`].
pub async fn reverse_generic_for<Parent: crate::core::Model>(
    pool: &crate::sql::Pool,
    child_schema: &'static crate::core::ModelSchema,
    parent_pk: i64,
    relation_name: Option<&str>,
) -> Result<Vec<serde_json::Value>, ExecError> {
    let ct = ContentType::get_for_model::<Parent>(pool)
        .await?
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: Parent::SCHEMA.table,
        })?;
    let ct_id = ct.id.get().copied().ok_or(ExecError::MissingPrimaryKey {
        table: ContentType::SCHEMA.table,
    })?;
    fetch_reverse_generic(pool, child_schema, ct_id, parent_pk, relation_name).await
}

/// Batched reverse-generic prefetch — Django's `GenericPrefetch`.
/// Given a list of parent primary keys (same model), fetches all
/// matching child rows in a single SELECT and groups them by
/// parent_pk. Eliminates the N+1 query pattern when rendering an
/// index page that shows children per parent.
///
/// Returns a `HashMap<parent_pk, Vec<child_json>>` keyed by the
/// parent_pk side of the GFK. Parents with zero children get no
/// entry (caller's responsibility to default to `&[]` per parent).
///
/// # Errors
/// As [`fetch_reverse_generic`].
pub async fn prefetch_reverse_generic_for<Parent: crate::core::Model>(
    pool: &crate::sql::Pool,
    child_schema: &'static crate::core::ModelSchema,
    parent_pks: &[i64],
    relation_name: Option<&str>,
) -> Result<::std::collections::HashMap<i64, Vec<serde_json::Value>>, ExecError> {
    use crate::core::{Filter, Op, SelectQuery, WhereExpr};

    if parent_pks.is_empty() {
        return Ok(::std::collections::HashMap::new());
    }
    let rel = match relation_name {
        Some(name) => child_schema
            .generic_relations
            .iter()
            .find(|r| r.name == name),
        None => child_schema.generic_relations.first(),
    };
    let Some(rel) = rel else {
        return Ok(::std::collections::HashMap::new());
    };
    let ct = ContentType::get_for_model::<Parent>(pool)
        .await?
        .ok_or_else(|| ExecError::MissingPrimaryKey {
            table: Parent::SCHEMA.table,
        })?;
    let ct_id = ct.id.get().copied().ok_or(ExecError::MissingPrimaryKey {
        table: ContentType::SCHEMA.table,
    })?;
    let mut pks: Vec<i64> = parent_pks.to_vec();
    pks.sort_unstable();
    pks.dedup();
    let pk_values: Vec<crate::core::SqlValue> = pks
        .iter()
        .copied()
        .map(crate::core::SqlValue::I64)
        .collect();
    // #562 — composite AND-IN lookup; struct-update over SelectQuery::new.
    let select_q = SelectQuery {
        where_clause: WhereExpr::And(vec![
            WhereExpr::Predicate(Filter {
                column: rel.ct_column,
                op: Op::Eq,
                value: crate::core::SqlValue::I64(ct_id),
            }),
            WhereExpr::Predicate(Filter {
                column: rel.pk_column,
                op: Op::In,
                value: crate::core::SqlValue::List(pk_values),
            }),
        ]),
        ..SelectQuery::new(child_schema)
    };
    let fields: Vec<&'static crate::core::FieldSchema> = child_schema.scalar_fields().collect();
    let rows = crate::sql::select_rows_as_json(pool, &select_q, &fields).await?;
    let mut grouped: ::std::collections::HashMap<i64, Vec<serde_json::Value>> =
        ::std::collections::HashMap::new();
    for row in rows {
        // The pk column on the row carries the parent_pk we matched on.
        if let Some(pk_val) = row.get(rel.pk_column).and_then(serde_json::Value::as_i64) {
            grouped.entry(pk_val).or_default().push(row);
        }
    }
    Ok(grouped)
}

// v0.34 — `ensure_table` (below) routes through
// [`crate::migrate::ddl::create_table_if_not_exists_sql_with_dialect`]
// which already knows how to emit the table for any backend rustango
// supports. The UNIQUE INDEX on `(app_label, model_name)` is still
// hand-emitted because `crate::migrate::diff::render_changes` (the
// index renderer in the migration system) is currently Postgres-only;
// when index emission goes bi-dialect that block collapses too.

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
pub async fn ensure_table(pool: &crate::sql::Pool) -> Result<(), crate::sql::sqlx::Error> {
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
    // UNIQUE INDEX on the natural key. PG + SQLite support
    // `CREATE UNIQUE INDEX IF NOT EXISTS`; MySQL doesn't — we drop
    // the `IF NOT EXISTS` clause there. `run_ddl_idempotent` then
    // swallows MySQL's dup-index error (1061) so re-runs stay
    // idempotent. Hand-rolled until
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
    // #561 — table + index DDL go through the shared
    // `run_ddl_idempotent` runner: split-by-`;` + dispatch +
    // swallow-dup-index. The local `exec_one` helper used to ship
    // its own match-on-pool dispatch alongside an inline dup-index
    // arm; both collapse to one call.
    let combined = format!("{table_ddl}; {index_ddl}");
    crate::sql::run_ddl_idempotent(pool, &combined).await
}

// #561 — `is_mysql_dup_index_error` is now consumed inside
// `crate::sql::run_ddl_idempotent` (the shared DDL runner); the
// direct import here is unused after the `ensure_table` collapse.

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
pub async fn ensure_seeded(pool: &crate::sql::Pool) -> Result<usize, ExecError> {
    ensure_table(pool).await.map_err(ExecError::Driver)?;
    let mut inserted = 0_usize;
    for entry in inventory::iter::<ModelEntry> {
        let table = entry.schema.table;
        if table == ContentType::SCHEMA.table {
            continue;
        }
        let app = entry.resolved_app_label().unwrap_or("project").to_owned();
        let name = entry.schema.name.to_ascii_lowercase();
        if ContentType::by_natural_key(&pool.clone().into(), &app, &name)
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
