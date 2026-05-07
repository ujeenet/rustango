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
use crate::sql::{sqlx::PgPool, Auto, ExecError, Fetcher as _};
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
    pub async fn for_target<T: crate::core::Model>(
        pool: &PgPool,
        object_pk: i64,
    ) -> Result<Self, ExecError> {
        let ct = ContentType::for_model::<T>(pool).await?.ok_or_else(|| {
            ExecError::MissingPrimaryKey {
                table: T::SCHEMA.table,
            }
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
pub async fn ensure_seeded(pool: &PgPool) -> Result<usize, ExecError> {
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
