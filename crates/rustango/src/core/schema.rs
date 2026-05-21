//! Schema types: what every model in the registry looks like at runtime.

use super::FieldType;

/// Static description of a single column on a model.
///
/// `max_length`, `min`, `max` carry per-field bounds populated from
/// `#[rustango(max_length = …, min = …, max = …)]`. The query layer
/// uses them to validate writes; the migration writer uses them to
/// emit `VARCHAR(N)` and `CHECK` constraints.
///
/// `default` is the raw SQL fragment placed after `DEFAULT` in DDL
/// (e.g. `"0"`, `"'draft'"`, `"NOW()"`). Set via
/// `#[rustango(default = "…")]`. The string is inserted verbatim — it
/// is the developer's responsibility to write a valid Postgres
/// expression and to quote string literals themselves.
#[derive(Debug, Clone, Copy)]
pub struct FieldSchema {
    pub name: &'static str,
    pub column: &'static str,
    pub ty: FieldType,
    pub nullable: bool,
    pub primary_key: bool,
    pub relation: Option<Relation>,
    /// Maximum string length in characters. Only meaningful for `FieldType::String`.
    pub max_length: Option<u32>,
    /// Inclusive integer lower bound. Only meaningful for `I32`/`I64`.
    pub min: Option<i64>,
    /// Inclusive integer upper bound. Only meaningful for `I32`/`I64`.
    pub max: Option<i64>,
    /// Raw SQL expression for the column's `DEFAULT` clause, if any.
    pub default: Option<&'static str>,
    /// `true` for fields whose Rust type is `Auto<T>` — server-assigned
    /// PKs that translate to `BIGSERIAL` / `SERIAL` and skip the column
    /// from explicit INSERTs when `Auto::Unset` so Postgres' DEFAULT
    /// fires. The migration writer reads this; the `Auto::Unset → SQL
    /// DEFAULT` translation happens in the macro-generated INSERT path.
    pub auto: bool,
    /// `true` when `#[rustango(unique)]` is present. The DDL writer emits
    /// `UNIQUE` inline on the column definition.
    pub unique: bool,
    /// Raw SQL expression for a `GENERATED ALWAYS AS (...) STORED`
    /// column. When `Some`, the DDL writer emits the generated-column
    /// clause and the macro skips this column from every INSERT and
    /// UPDATE path — the value is always computed by the database
    /// from the expression. Read-back via `FromRow` works as for any
    /// other column.
    ///
    /// Example: `#[rustango(generated_as = "price * quantity")] pub
    /// total: f64,` produces `total DOUBLE PRECISION GENERATED
    /// ALWAYS AS (price * quantity) STORED`.
    pub generated_as: Option<&'static str>,
    /// Django-shape help text — short caption rendered below the
    /// admin form's input to explain what the field is for. Set
    /// via `#[rustango(help_text = "...")]`. `None` means no
    /// caption (admin renders just the input). Future surfaces
    /// (DRF serializer schemas, OpenAPI descriptions, ModelForm
    /// `<label>` annotations) can read the same string.
    pub help_text: Option<&'static str>,
    /// Django-shape `choices=[(value, label), ...]` — enumerated
    /// allowed values for a string field. Set via
    /// `#[rustango(choices = "draft:Draft, published:Published")]`
    /// (each pair is `value:label`, separated by commas; if no
    /// `:` is present the value is reused as the label). When
    /// `Some`, the admin renders a `<select>` instead of `<input>`,
    /// and `validate_value` rejects values not in the list. Only
    /// meaningful for `FieldType::String`.
    pub choices: Option<&'static [(&'static str, &'static str)]>,
    /// Django-shape `db_comment="..."` — DB-side column comment
    /// emitted alongside CREATE TABLE. Set via
    /// `#[rustango(db_comment = "...")]`. MySQL inlines it
    /// (`<col> <type> COMMENT '...'`); Postgres emits a separate
    /// `COMMENT ON COLUMN "<table>"."<col>" IS '...'` statement
    /// after the table is created; SQLite has no native column
    /// comments and silently drops the value.
    pub db_comment: Option<&'static str>,
    /// Django-shape `verbose_name` — human-readable label for the
    /// field, used in admin column headers, form labels, and any
    /// other surface that wants to display the field with a friendly
    /// caption instead of the Rust identifier. Set via
    /// `#[rustango(verbose_name = "Display title")]`. `None` means
    /// callers should fall back to [`Self::name`].
    pub verbose_name: Option<&'static str>,
    /// Django-shape `editable` flag — `true` (default) means the
    /// field appears in admin / form input renderers; `false` means
    /// the admin change-form excludes the field entirely (the value
    /// is still visible on detail / list views, just not editable).
    /// Set via `#[rustango(editable = false)]`. Mirrors Django's
    /// `editable=False` semantics: auto-generated ModelForms drop
    /// the field. Different from the model-level
    /// `admin.readonly_fields` which renders the input disabled —
    /// `editable = false` removes the input entirely.
    pub editable: bool,
    /// Django-shape `blank` flag — `true` means the form layer
    /// allows the field to be submitted empty even when the column
    /// is `NOT NULL`. Set via `#[rustango(blank)]` or
    /// `#[rustango(blank = true)]`. The admin form drops the
    /// `required` HTML attribute when this is set, and form-side
    /// validators treat an empty string as valid (the DB still
    /// enforces NOT NULL — empty string is a valid non-null value).
    ///
    /// Distinct from `nullable` (which controls whether the SQL
    /// column accepts NULL): a CharField can be
    /// `nullable=false, blank=true` to require *some* value in DB
    /// but accept `""` from the form.
    pub blank: bool,
    /// Django-shape `validators=[...]` — names of value-shape validators
    /// to run on every INSERT/UPDATE through the typed query layer. Set
    /// via `#[rustango(validators = "email,url")]` (comma-separated).
    /// Names dispatch to the `validators::*` family
    /// ([`crate::validators::validate_email`], `validate_url`,
    /// `validate_slug`, `validate_unicode_slug`, `validate_phone_e164`,
    /// `validate_hex_color`, `validate_uuid`, `validate_iso_date`,
    /// `validate_iso_time`, `validate_iso_datetime`, `validate_ipv4`,
    /// `validate_ipv6`, `validate_no_null`). Unknown names error at
    /// runtime via [`crate::core::QueryError::UnknownValidator`].
    pub validators: &'static [&'static str],
}

impl FieldSchema {
    /// Human-readable label for this field — `verbose_name` if set,
    /// otherwise the Rust field identifier (`name`). Use this from
    /// admin / form / serializer renderers that want a friendly
    /// caption without re-implementing the fallback each time.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        self.verbose_name.unwrap_or(self.name)
    }
}

/// Static description of a relation to another model.
#[derive(Debug, Clone, Copy)]
pub enum Relation {
    /// Foreign key. The local column references `to.<on>`.
    Fk { to: &'static str, on: &'static str },
    /// One-to-one. Same shape as FK, separate variant for callers that care.
    O2O { to: &'static str, on: &'static str },
}

/// Generic ("any model") foreign key declared at the model level —
/// pairs a `content_type_id` column with an `object_pk` column whose
/// values together identify a row in any registered model. The
/// pointed-at model varies per row.
///
/// Sub-slice F.4 of the v0.15.0 ContentType plan. Used by audit log
/// targets, comments-on-anything, activity-stream entries, generic
/// tags. See [`crate::contenttypes::GenericForeignKey`] for the
/// runtime value type and `prefetch_generic` for batched hydration.
///
/// Declared on the source model via the container attr
/// `#[rustango(generic_fk(name = "target", ct_column = "content_type_id",
/// pk_column = "object_pk"))]`. The admin renderer uses this metadata
/// to display generic-FK columns as clickable target links.
#[derive(Debug, Clone, Copy)]
pub struct GenericRelation {
    /// Logical name for the relation (used in admin labels, error
    /// messages). Free-form Rust identifier.
    pub name: &'static str,
    /// Source-side column name carrying the `content_type_id` FK
    /// to `rustango_content_types.id`.
    pub ct_column: &'static str,
    /// Source-side column name carrying the target row's primary key.
    pub pk_column: &'static str,
}

/// Multi-column ("composite") foreign key relation, declared at the
/// model level rather than the field level — single-column FKs stay
/// in [`FieldSchema::relation`], composite FKs live here so each
/// participating column keeps its plain Rust type.
///
/// Sub-slice F.2 of the v0.15.0 ContentType plan. Used by audit log
/// (`(entity_table, entity_pk)` → `rustango_content_types`),
/// permissions (`(content_type_id, codename)` → role-permission
/// link), and any user model that points at another table by more
/// than one column.
///
/// Declared on the source model via the container attr
/// `#[rustango(fk_composite(name = "audit_target", to = "rustango_audit_log",
/// on = ("entity_table", "entity_pk"), from = ("table_name", "row_pk")))]`.
/// `from` and `on` must be the same length; the macro errors otherwise.
#[derive(Debug, Clone, Copy)]
pub struct CompositeFkRelation {
    /// Logical name for the relation (used in admin labels, error
    /// messages, and as the prefix for any reverse-relation accessors
    /// the macro generates). Free-form Rust identifier.
    pub name: &'static str,
    /// SQL table name of the target model.
    pub to: &'static str,
    /// Column names on the source (this) table that participate in
    /// the FK, in declaration order. Same length as `on`.
    pub from: &'static [&'static str],
    /// Column names on the target table that the FK references, in
    /// the same order as `from`.
    pub on: &'static [&'static str],
}

/// Descriptor for one many-to-many relation declared via
/// `#[rustango(m2m(name = "tags", to = "app_tags", through = "post_tags",
///                 src = "post_id", dst = "tag_id"))]`.
///
/// Stored in [`ModelSchema::m2m`] — does **not** correspond to any column on
/// the source model's table. The migration writer reads this to emit
/// `CREATE TABLE` for the junction table.
#[derive(Debug, Clone, Copy)]
pub struct M2MRelation {
    /// Rust accessor name used to generate the `<name>_m2m()` method.
    pub name: &'static str,
    /// SQL name of the target (destination) table.
    pub to: &'static str,
    /// SQL name of the junction (through) table.
    pub through: &'static str,
    /// Column in the junction table that references the source model's PK.
    pub src_col: &'static str,
    /// Column in the junction table that references the target model's PK.
    pub dst_col: &'static str,
}

/// Static description of a model.
///
/// `display` is the Rust-side field name that should be used when
/// rendering this model as the *target* of a foreign key — admin UIs
/// and any future "select" widgets render `display`'s value rather than
/// the raw PK. Set via `#[rustango(display = "field")]`; defaults to
/// `None`, in which case callers fall back to the primary key.
#[derive(Debug, Clone, Copy)]
pub struct ModelSchema {
    pub name: &'static str,
    pub table: &'static str,
    pub fields: &'static [FieldSchema],
    pub display: Option<&'static str>,
    /// Explicit Django-style app label, set via
    /// `#[rustango(app = "blog")]` on the struct. `None` when the user
    /// didn't override it; in that case [`ModelEntry::resolved_app_label`]
    /// falls back to inferring from the registered module path.
    pub app_label: Option<&'static str>,
    /// Auto-admin customization (Django ModelAdmin-shape) set via
    /// `#[rustango(admin(...))]` on the struct. `None` when the user
    /// didn't override anything; admin code falls back to
    /// [`AdminConfig::DEFAULT`] in that case.
    pub admin: Option<&'static AdminConfig>,
    /// SQL column name of the field marked `#[rustango(soft_delete)]`,
    /// if the model has one. The admin uses this to route DELETE requests
    /// through an UPDATE-set-column-to-NOW path instead of a hard DELETE.
    pub soft_delete_column: Option<&'static str>,
    /// `true` when the model carries `#[rustango(permissions)]`. Signals
    /// that the four standard CRUD codenames (`table.add`, `table.change`,
    /// `table.delete`, `table.view`) should be auto-seeded by
    /// [`rustango::tenancy::permissions::auto_create_permissions`].
    pub permissions: bool,
    /// Rust field names that `#[rustango(audit(track = "…"))]` selected
    /// for per-write change capture.
    ///
    /// * `None` — no `#[rustango(audit(...))]` on this model; the macro
    ///   emits no audit code. The admin still records changes for all fields.
    /// * `Some(&[])` — `audit` present with no `track` list; every scalar
    ///   field is captured (macro and admin agree on "all fields").
    /// * `Some(&["title", "body"])` — only these named fields are captured
    ///   both by the macro-generated write path and by the admin diff.
    pub audit_track: Option<&'static [&'static str]>,
    /// Many-to-many relations declared via
    /// `#[rustango(m2m(name = "…", to = "…", through = "…",
    ///                 src = "…", dst = "…"))]`.
    ///
    /// Each entry describes one junction table. The migration writer reads
    /// this slice to emit `CREATE TABLE` / `DROP TABLE` for junction tables.
    /// Empty slice when the model has no M2M relations.
    pub m2m: &'static [M2MRelation],
    /// Indexes declared via `#[rustango(index)]` on fields (single-column) or
    /// `#[rustango(index("col1, col2"))]` on the container (composite).
    ///
    /// The migration writer emits `CREATE INDEX` / `DROP INDEX` for each
    /// entry. Empty slice when the model has no declared indexes.
    pub indexes: &'static [IndexSchema],
    /// Table-level CHECK constraints declared via
    /// `#[rustango(check(name = "…", expr = "…"))]` on the container.
    ///
    /// Each entry is rendered as `ALTER TABLE … ADD CONSTRAINT "name"
    /// CHECK (expr)` after the table is created.
    pub check_constraints: &'static [CheckConstraint],
    /// Composite (multi-column) foreign key relations declared via
    /// `#[rustango(fk_composite(name = "...", to = "...", on = (...),
    /// from = (...)))]`. Each entry maps a tuple of source columns
    /// to a tuple of target columns on `to`. Single-column FKs
    /// continue to live on [`FieldSchema::relation`] — `composite_relations`
    /// only carries the multi-column case so the existing single-FK
    /// machinery (admin display, snapshot diff, single-col DDL) stays
    /// untouched.
    ///
    /// Empty slice when the model has no composite FKs.
    pub composite_relations: &'static [CompositeFkRelation],
    /// Generic ("any model") foreign key relations declared via
    /// `#[rustango(generic_fk(name = "...", ct_column = "...",
    /// pk_column = "..."))]`. Each entry pairs a `content_type_id`
    /// column with an `object_pk` column. Empty slice when the
    /// model has no generic FKs. Sub-slice F.4 of the v0.15.0
    /// ContentType plan.
    pub generic_relations: &'static [GenericRelation],
    /// Where this model lives in a tenancy deployment — the registry
    /// DB or each tenant's storage. Drives `makemigrations` so it
    /// emits separate registry-scoped vs tenant-scoped migration
    /// files instead of dumping everything into one tenant migration
    /// (which then breaks when applied to a tenant schema where
    /// registry tables resolve to the wrong place via search_path).
    ///
    /// Set via `#[rustango(scope = "registry")]` on the struct;
    /// defaults to [`ModelScope::Tenant`] when unset. Single-tenant
    /// projects ignore this entirely — every model defaults to
    /// `Tenant` and `makemigrations` produces one file as before.
    pub scope: ModelScope,
    /// Default ordering declared via `#[rustango(default_order = "...")]`.
    /// Issue #291 / T2.5. Each tuple is `(column_name, desc)` — `desc =
    /// true` means descending. Empty slice when the model has no
    /// default order declared.
    ///
    /// **Per-query opt-in**: this list is NOT applied automatically.
    /// Callers must chain `QuerySet::with_default_order()` to invoke
    /// it — this avoids the Django `Meta.ordering` footgun where
    /// every query (including `.count()` / `.exists()`) pays for the
    /// sort by default.
    pub default_order: &'static [(&'static str, bool)],
    /// `true` when the model is backed by a SQL **view** rather than a
    /// table — set via `#[rustango(view)]` on the struct. Issue #293 /
    /// T2.10. View-backed models are excluded from the migration
    /// snapshot so `makemigrations` / `migrate` never emit `CREATE
    /// TABLE` / `DROP TABLE` against them (the view is owned by the
    /// operator, not by rustango). Reads behave like any other model.
    pub is_view: bool,
    /// Django-shape `Meta.verbose_name` — human-readable singular label
    /// for the model. Set via
    /// `#[rustango(verbose_name = "blog post")]`. Used by admin section
    /// headers, breadcrumbs, "Add <X>" buttons, and any other surface
    /// that wants a friendly caption instead of the Rust struct name.
    /// `None` means callers fall back to [`Self::name`].
    pub verbose_name: Option<&'static str>,
    /// Django-shape `Meta.verbose_name_plural` — plural form of
    /// [`Self::verbose_name`]. Set via
    /// `#[rustango(verbose_name_plural = "blog posts")]`. Used by admin
    /// list-page headings ("All blog posts"). `None` means callers
    /// should auto-pluralize (`format!("{}s", verbose_name_or_name)`).
    pub verbose_name_plural: Option<&'static str>,
}

impl ModelSchema {
    /// Human-readable singular label for the model — `verbose_name` if
    /// set, otherwise the Rust struct name (`name`). Use this from
    /// admin / form / serializer renderers that want a friendly
    /// caption without re-implementing the fallback each time.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        self.verbose_name.unwrap_or(self.name)
    }

    /// Human-readable plural label for the model — `verbose_name_plural`
    /// if set, else `verbose_name + "s"` if `verbose_name` is set, else
    /// `name + "s"`. Returns an owned `String` because the auto-plural
    /// fallback can't be `&'static`.
    #[must_use]
    pub fn display_label_plural(&self) -> String {
        if let Some(plural) = self.verbose_name_plural {
            return plural.to_owned();
        }
        let base = self.verbose_name.unwrap_or(self.name);
        format!("{base}s")
    }
}

/// Where a model's table lives in a tenancy deployment. Mirrors
/// [`crate::migrate::MigrationScope`] but on the model side, so the
/// migration generator can route changes to the right scoped file
/// without touching the runtime schema-discovery path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModelScope {
    /// Lives in the registry DB. Cross-tenant — one row per tenant
    /// or one row per operator. Examples: `Org`, `Operator`.
    /// Migrations touching these tables MUST run as
    /// `MigrationScope::Registry`, otherwise `migrate-tenants` will
    /// re-apply them per tenant and constraint names will collide
    /// against the existing registry copy via `search_path`.
    Registry,
    /// Lives in each tenant's storage (schema or dedicated DB).
    /// Default — covers ~all user models and most framework models
    /// (User, Role, ApiKey, audit, …). `makemigrations` emits these
    /// as `MigrationScope::Tenant` and `migrate-tenants` fans them
    /// out across active orgs.
    #[default]
    Tenant,
}

impl ModelScope {
    /// Tiny round-trip helper for snapshot serialization /
    /// container-attr parsing. Recognises `"registry"` and `"tenant"`
    /// case-insensitively; everything else returns `None`.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "registry" => Some(Self::Registry),
            "tenant" => Some(Self::Tenant),
            _ => None,
        }
    }

    /// String form for snapshot JSON / error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Tenant => "tenant",
        }
    }
}

/// Descriptor for one table-level CHECK constraint.
///
/// Declared via `#[rustango(check(name = "name", expr = "raw_sql"))]`.
/// The expression is inserted verbatim into the DDL — quote literals and
/// reference column names yourself.
#[derive(Debug, Clone, Copy)]
pub struct CheckConstraint {
    /// Constraint name used in `ALTER TABLE … ADD CONSTRAINT "name"`.
    pub name: &'static str,
    /// Raw SQL boolean expression placed inside `CHECK ( … )`.
    pub expr: &'static str,
}

/// Descriptor for one `CREATE INDEX` emitted by the migration writer.
///
/// Declared via:
/// - `#[rustango(index)]` on a field → single-column non-unique index
/// - `#[rustango(index("col1, col2"))]` on the model container → composite index
/// - Either form accepts `unique` and `name` sub-attributes.
#[derive(Debug, Clone, Copy)]
pub struct IndexSchema {
    /// Index name used in `CREATE INDEX "name"` and `DROP INDEX "name"`.
    /// Auto-generated as `{table}_{col}_idx` when not supplied.
    pub name: &'static str,
    /// SQL column names included in the index, in order.
    pub columns: &'static [&'static str],
    /// `true` for `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// Access method — defaults to [`IndexMethod::BTree`] when not
    /// specified. Selects the `USING <method>` clause emitted by
    /// the DDL writer. Issue #34.
    pub method: IndexMethod,
    /// Optional `WHERE <expr>` clause for partial indexes — Django's
    /// `UniqueConstraint(condition=Q(...))`. Issue #265 / T1.3.
    /// `None` (default) emits a plain index. `Some(expr)` emits
    /// `CREATE UNIQUE INDEX <name> ON <table> (cols) WHERE <expr>` on
    /// PG / SQLite (both ship partial indexes natively); MySQL has no
    /// native support and silently degrades to a plain UNIQUE index
    /// with a doc-level warning (the SQL accepted; the partial filter
    /// is ignored, so duplicates outside the partition would also be
    /// rejected — document the limitation).
    pub where_clause: Option<&'static str>,
}

/// Index access method — Postgres `CREATE INDEX … USING <method>`.
/// Mirrors Django's `django.contrib.postgres.indexes` types (`GinIndex`,
/// `GistIndex`, `BrinIndex`, `SpGistIndex`, `BloomIndex`, `HashIndex`)
/// plus the default `Btree`. Issue #34.
///
/// ## Backend support matrix
/// - **Postgres**: all variants supported. `Bloom` requires the
///   `bloom` extension (`CREATE EXTENSION bloom`).
/// - **MySQL**: only `Btree` (and `Hash` on the MEMORY engine). All
///   other variants degrade silently to btree at emit time —
///   MySQL's optimizer ignores the `USING` token for unsupported
///   methods, so the index still works (just as btree).
/// - **SQLite**: only btree. Non-btree methods are silently dropped
///   at emit time; SQLite has no `USING` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMethod {
    /// Default B-tree. Universal — every backend supports it.
    #[default]
    BTree,
    /// Generalized Inverted Index — for full-text search, JSONB
    /// keys, array containment. **PG-only**.
    Gin,
    /// Generalized Search Tree — for geometric, full-text, range,
    /// and trigram queries. **PG-only**.
    Gist,
    /// Block Range Index — compact summary index for very large
    /// tables where rows correlate to physical storage order
    /// (time-series, log tables). **PG-only**.
    Brin,
    /// Space-Partitioned GiST — variants of GiST for non-balanced
    /// data structures (quadtrees, k-d trees, suffix trees).
    /// **PG-only**.
    SpGist,
    /// Hash index — equality-only lookups, smaller than btree.
    /// PG: WAL-logged since 10. MySQL: MEMORY engine only.
    Hash,
    /// Bloom filter index — multi-column equality with controllable
    /// false-positive rate. **PG-only**, requires the `bloom`
    /// extension.
    Bloom,
}

impl IndexMethod {
    /// Stable lower-case token for snapshot serialization +
    /// `USING <token>` emission.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BTree => "btree",
            Self::Gin => "gin",
            Self::Gist => "gist",
            Self::Brin => "brin",
            Self::SpGist => "spgist",
            Self::Hash => "hash",
            Self::Bloom => "bloom",
        }
    }

    /// Parse from the lower-case token used in snapshot JSON. Unknown
    /// values fall back to [`Self::BTree`] so older snapshots that
    /// pre-date this addition keep deserializing cleanly.
    #[must_use]
    pub fn from_token(s: &str) -> Self {
        match s {
            "gin" => Self::Gin,
            "gist" => Self::Gist,
            "brin" => Self::Brin,
            "spgist" => Self::SpGist,
            "hash" => Self::Hash,
            "bloom" => Self::Bloom,
            _ => Self::BTree,
        }
    }

    /// `true` when this method only works on Postgres.
    #[must_use]
    pub const fn is_postgres_only(self) -> bool {
        matches!(
            self,
            Self::Gin | Self::Gist | Self::Brin | Self::SpGist | Self::Bloom
        )
    }
}

/// Django ModelAdmin-shape per-model admin customization. Populated by
/// the `Model` derive when the struct carries `#[rustango(admin(...))]`.
///
/// All fields default to "use the framework default" (an empty slice or
/// zero) so users only set the knobs they care about.
#[derive(Debug, Clone, Copy)]
pub struct AdminConfig {
    /// Field names rendered as columns on the list view, in order.
    /// Empty slice means "every scalar field, in declaration order"
    /// (today's default). FK columns auto-render the target's display
    /// value when the target is also visible in the admin.
    pub list_display: &'static [&'static str],
    /// Field names searched by the admin's `?q=` box, in order. Empty
    /// slice falls back to fields whose `searchable` flag is true on
    /// the [`FieldSchema`] (today's behavior, which auto-flags strings
    /// with `max_length`).
    pub search_fields: &'static [&'static str],
    /// Page size on the list view. `0` means "use the admin default"
    /// (currently 50).
    pub list_per_page: usize,
    /// Default ordering for the list view, as `(field_name, desc)` pairs.
    /// Empty slice means "PK ascending" (today's default).
    pub ordering: &'static [(&'static str, bool)],
    /// Field names rendered as text instead of editable inputs on the
    /// edit form. Reserved for slice 10.5; today's admin treats this
    /// as a no-op so existing models stay editable.
    pub readonly_fields: &'static [&'static str],
    /// Field names to render as right-rail facet filters on the list
    /// view. Each named field gets a card showing every distinct
    /// value in the table; clicking a value toggles `?<col>=<value>`
    /// in the URL. Empty slice means "no facets" (today's behavior).
    pub list_filter: &'static [&'static str],
    /// Bulk actions exposed at the top of the list view. Each name
    /// corresponds to a built-in or user-registered handler that
    /// receives the selected row PKs. Built-in: `"delete_selected"`.
    /// Empty slice means the action picker is hidden.
    pub actions: &'static [&'static str],
    /// Field grouping on the create/edit form. Each [`Fieldset`] is
    /// rendered as a `<fieldset><legend>title</legend>...</fieldset>`
    /// block in the listed order. Empty slice means "one unnamed
    /// group with every visible field" (today's default).
    pub fieldsets: &'static [Fieldset],
}

/// One group of fields on a create/edit form (slice 10.5).
///
/// A `title` of `""` renders without a `<legend>` so the operator can
/// have a single-group form without a section header.
#[derive(Debug, Clone, Copy)]
pub struct Fieldset {
    /// Section title shown as `<legend>`. Empty string suppresses it.
    pub title: &'static str,
    /// Field names in this group, in render order. Names must match
    /// declared scalar fields on the model.
    pub fields: &'static [&'static str],
}

impl AdminConfig {
    /// Default config for a model that has no `#[rustango(admin(...))]`
    /// attribute — every knob falls back to "framework default".
    pub const DEFAULT: AdminConfig = AdminConfig {
        list_display: &[],
        search_fields: &[],
        list_per_page: 0,
        ordering: &[],
        readonly_fields: &[],
        list_filter: &[],
        actions: &[],
        fieldsets: &[],
    };
}

impl ModelSchema {
    /// Look up a field by its Rust-side name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Look up a field by its SQL column name.
    #[must_use]
    pub fn field_by_column(&self, column: &str) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.column == column)
    }

    /// The primary-key field, if any. Returns the first `primary_key = true` field.
    #[must_use]
    pub fn primary_key(&self) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|f| f.primary_key)
    }

    /// Iterator over all scalar (column-backed) fields.
    pub fn scalar_fields(&self) -> impl Iterator<Item = &'static FieldSchema> {
        self.fields.iter()
    }

    /// Field used to render this model as a foreign-key target.
    ///
    /// Returns the field declared via `#[rustango(display = "…")]`, or
    /// the primary key if no display is set. Returns `None` only for the
    /// (unusual) model with neither a `display` attribute nor a primary key.
    #[must_use]
    pub fn display_field(&self) -> Option<&'static FieldSchema> {
        if let Some(name) = self.display {
            return self.field(name);
        }
        self.primary_key()
    }

    /// Fields that should participate in free-text search (`?q=…` in the
    /// admin). Heuristic: a `String` field with a `max_length` cap is
    /// likely a name/title/short label; long, uncapped strings (bodies,
    /// descriptions) are excluded so search stays cheap.
    pub fn searchable_fields(&self) -> impl Iterator<Item = &'static FieldSchema> {
        self.fields.iter().filter(|f| {
            matches!(f.ty, FieldType::String) && f.max_length.is_some() && f.relation.is_none()
        })
    }
}

/// Trait every `#[derive(Model)]` struct implements.
///
/// Carries the static `SCHEMA` so the registry and the query layer can
/// reach the model's metadata without an instance.
pub trait Model: Sized + Send + Sync + 'static {
    const SCHEMA: &'static ModelSchema;
}

/// Inventory entry submitted by the `#[derive(Model)]` macro for each model.
///
/// Internal API: end users should not construct these directly.
#[doc(hidden)]
pub struct ModelEntry {
    pub schema: &'static ModelSchema,
    /// Result of `module_path!()` at the registration site (e.g.
    /// `"my_app::blog::models"`). Used by
    /// [`Self::resolved_app_label`] to infer a Django-style
    /// `app_label` when the user didn't set one explicitly.
    pub module_path: &'static str,
}

impl ModelEntry {
    /// Django-shape app label for this model. Returns the explicit
    /// override from `#[rustango(app = "...")]` if set; otherwise
    /// infers from `module_path` by taking the first segment after the
    /// crate root. Examples (assuming crate `my_app`):
    ///
    /// * `"my_app::blog::models"`  → `Some("blog")`
    /// * `"my_app::shop::models"`  → `Some("shop")`
    /// * `"my_app::models"`        → `None` (top-level project model)
    /// * `"my_app"`                → `None`
    ///
    /// `None` means the model lives at the project root, not inside a
    /// dedicated app. Used for per-app migration discovery, admin
    /// sidebar grouping, and `manage makemigrations <app>` filtering.
    #[must_use]
    pub fn resolved_app_label(&self) -> Option<&'static str> {
        if let Some(label) = self.schema.app_label {
            return Some(label);
        }
        infer_app_label_from_module_path(self.module_path)
    }
}

/// Parse the Rust module path produced by `module_path!()` and return
/// the first segment after the crate root, or `None` when the model
/// lives at the project root. Public so callers (admin, makemigrations
/// CLI, the diagnostic `manage list-apps` verb) can apply the same
/// inference rules to module-path strings they already have.
#[must_use]
pub fn infer_app_label_from_module_path(path: &'static str) -> Option<&'static str> {
    let mut parts = path.split("::");
    let _crate_name = parts.next()?;
    let candidate = parts.next()?;
    // Skip pseudo-segments that mean "still at the project root":
    // `models`, `views`, `urls` are sibling files at `src/`, not apps.
    if matches!(candidate, "models" | "views" | "urls" | "main") {
        return None;
    }
    Some(candidate)
}

inventory::collect!(ModelEntry);

#[cfg(test)]
mod tests {
    use super::infer_app_label_from_module_path as infer;

    #[test]
    fn infers_app_from_submodule() {
        assert_eq!(infer("my_app::blog::models"), Some("blog"));
        assert_eq!(infer("my_app::shop::models"), Some("shop"));
        assert_eq!(infer("my_app::auth"), Some("auth"));
    }

    #[test]
    fn returns_none_for_project_root_models() {
        assert_eq!(infer("my_app"), None);
        assert_eq!(infer("my_app::models"), None);
        assert_eq!(infer("my_app::views"), None);
    }
}
