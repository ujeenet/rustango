//! Schema types: what every model in the registry looks like at runtime.

use super::FieldType;

/// Static description of a single column on a model.
///
/// The derive macro fills this in; the query layer and the migration
/// writer read it.
///
/// `max_length`, `min` and `max` come from
/// `#[rustango(max_length = …, min = …, max = …)]`. The query layer
/// checks writes against them, and the migration writer turns them
/// into `VARCHAR(N)` and `CHECK` constraints.
///
/// `default` is the raw SQL placed after `DEFAULT` in DDL, such as
/// `"0"`, `"'draft'"` or `"NOW()"`. Set it with
/// `#[rustango(default = "…")]`. The string goes in as written, so
/// you must make it valid SQL and quote any string literal yourself.
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
    /// `true` when the Rust type is `Auto<T>`: a server-assigned value
    /// that becomes `BIGSERIAL` / `SERIAL`. An `Auto::Unset` column is
    /// left out of the INSERT so the database default fires. The
    /// migration writer reads this flag; the macro's INSERT path does
    /// the omission.
    pub auto: bool,
    /// `true` when `#[rustango(unique)]` is present. The DDL writer emits
    /// `UNIQUE` inline on the column definition.
    pub unique: bool,
    /// Raw SQL for a `GENERATED ALWAYS AS (...) STORED` column. When
    /// `Some`, the DDL writer emits the generated-column clause and
    /// the macro leaves the column out of every INSERT and UPDATE,
    /// because the database always computes it. Reading it back with
    /// `FromRow` works like any other column.
    ///
    /// For example `#[rustango(generated_as = "price * quantity")]`
    /// on `total: f64` gives
    /// `total DOUBLE PRECISION GENERATED ALWAYS AS (price * quantity) STORED`.
    pub generated_as: Option<&'static str>,
    /// Short caption shown under the admin form input, from
    /// `#[rustango(help_text = "...")]`. `None` means no caption.
    pub help_text: Option<&'static str>,
    /// Django-shape `choices`: the allowed values for a string field.
    /// Set it with
    /// `#[rustango(choices = "draft:Draft, published:Published")]`.
    /// Each comma-separated pair is `value:label`; without a `:` the
    /// value is also the label. When `Some`, the admin renders a
    /// `<select>` and `validate_value` rejects anything not listed.
    /// Only meaningful for `FieldType::String`.
    pub choices: Option<&'static [(&'static str, &'static str)]>,
    /// Django-shape `db_comment`: a database-side column comment from
    /// `#[rustango(db_comment = "...")]`. MySQL puts it inline on the
    /// column. Postgres emits a separate `COMMENT ON COLUMN` after
    /// the table. SQLite has no column comments and drops it.
    pub db_comment: Option<&'static str>,
    /// Django-shape `verbose_name`: a readable label for admin
    /// headers and form labels, from
    /// `#[rustango(verbose_name = "Display title")]`. When `None`,
    /// fall back to [`Self::name`].
    pub verbose_name: Option<&'static str>,
    /// Django-shape `editable`. `true`, the default, shows the field
    /// in admin and form renderers. `false` drops it from the admin
    /// change form completely, though detail and list views still
    /// show the value. Set it with `#[rustango(editable = false)]`.
    ///
    /// This differs from the model-level `admin.readonly_fields`,
    /// which renders the input but disables it.
    pub editable: bool,
    /// Django-shape `blank`. `true` lets the form layer accept an
    /// empty value even when the column is `NOT NULL`: the admin form
    /// drops the `required` attribute and form validators treat `""`
    /// as valid. Set it with `#[rustango(blank)]`.
    ///
    /// This is not `nullable`, which decides whether the column
    /// accepts NULL. A field can be `nullable = false, blank = true`
    /// to demand a value in the database but accept `""` from a form.
    pub blank: bool,
    /// Django-shape `CITextField`. When `true`, the DDL writer emits
    /// a case-insensitive column type: `CITEXT` on Postgres, which
    /// also emits `CREATE EXTENSION IF NOT EXISTS citext;`,
    /// `TEXT COLLATE NOCASE` on SQLite, and a
    /// `COLLATE utf8mb4_general_ci` column on MySQL. So
    /// `WHERE col = 'foo'` also matches `'FOO'` with no `LOWER(…)`
    /// in the query.
    ///
    /// Set it with `#[rustango(citext)]`. Only meaningful for
    /// `FieldType::String`.
    pub case_insensitive: bool,
    /// What happens to this row when the referenced row is deleted.
    /// `None` leaves the clause off, so the database default applies,
    /// which is `NO ACTION` on every supported backend. `Some(action)`
    /// makes the migration writer append
    /// `ON DELETE <action.as_sql()>` to the FK constraint.
    ///
    /// Only meaningful when [`Self::relation`] is `Some(Relation::Fk
    /// {..})` or `Some(Relation::O2O {..})`; ignored on plain columns.
    /// Set it with `#[rustango(on_delete = "cascade" | "restrict" |
    /// "set_null" | "set_default" | "no_action")]`, case-insensitive.
    pub fk_on_delete: Option<OnDeleteAction>,
    /// Django-shape `validators`: names of value checks to run on
    /// every INSERT and UPDATE through the typed query layer. Set
    /// them with `#[rustango(validators = "email,url")]`.
    ///
    /// Each name maps to a function in the `validators` module, such
    /// as [`crate::validators::validate_email`], `validate_url`,
    /// `validate_slug`, `validate_uuid`, `validate_ipv4` or
    /// `validate_filepath`. An unknown name errors at runtime with
    /// [`crate::core::QueryError::UnknownValidator`].
    pub validators: &'static [&'static str],
}

impl FieldSchema {
    /// Readable label for this field: `verbose_name` if set, else the
    /// Rust field name. Renderers should call this instead of
    /// repeating the fallback.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        self.verbose_name.unwrap_or(self.name)
    }

    /// `true` for a server-assigned timestamp: an
    /// `#[rustango(auto_now_add)]` or `#[rustango(auto_now)]` column.
    ///
    /// **A writer that builds an INSERT from the schema, rather than
    /// from the macro's codegen, must fill these from the clock.** No
    /// client payload carries them, so such a writer would otherwise
    /// omit the column and let the database default fire. On a SQLite
    /// database created by an older version that default is
    /// `CURRENT_TIMESTAMP`, whose `YYYY-MM-DD HH:MM:SS` text sorts
    /// below the canonical spelling. Cursor pagination keyed on the
    /// column then serves page one forever, and `ALTER TABLE` on
    /// SQLite cannot replace the default.
    ///
    /// This is inferred, not stored, and the inference is exact: the
    /// macro rejects a non-PK `Auto<T>` field unless it has
    /// `auto_uuid`, `default_uuid_v7`, `auto_now_add` or `auto_now`,
    /// and only the last two may be `DateTime`.
    #[must_use]
    pub fn is_auto_timestamp(&self) -> bool {
        self.auto && !self.primary_key && matches!(self.ty, FieldType::DateTime)
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

/// Django-shape `ForeignKey(on_delete=...)`: the `ON DELETE` clause
/// on `ALTER TABLE … ADD FOREIGN KEY`. When
/// [`FieldSchema::fk_on_delete`] is `None`, the migration writer
/// leaves the clause off and the database default applies, which is
/// `NO ACTION` on every supported backend.
///
/// Set it with `#[rustango(on_delete = "cascade" | "restrict" |
/// "set_null" | "set_default" | "no_action")]`, case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDeleteAction {
    /// `ON DELETE CASCADE` — delete this row when the referenced row goes.
    Cascade,
    /// `ON DELETE RESTRICT` — block the parent delete if any child references it.
    Restrict,
    /// `ON DELETE SET NULL` — null out the FK column. Requires a nullable column.
    SetNull,
    /// `ON DELETE SET DEFAULT` — reset the FK column to its declared `DEFAULT`.
    SetDefault,
    /// `ON DELETE NO ACTION` — an explicit no-op. Same as leaving the
    /// clause off on most backends, but clearer when a project always
    /// states the FK action.
    NoAction,
}

impl OnDeleteAction {
    /// SQL token rendered after `ON DELETE` in `ALTER TABLE … ADD
    /// CONSTRAINT`.
    ///
    /// The token is the same on PG, MySQL and SQLite, but the server
    /// behaviour is not. Two cases differ on MySQL 8.0:
    ///
    /// - `SetNull` on a NOT NULL column is `ERROR 1830` at DDL time on
    ///   MySQL. PG and SQLite accept the constraint and fail only at
    ///   delete time. [`UPGRADING.md`] has the fix.
    /// - `SetDefault` works on PG and SQLite, but InnoDB parses and
    ///   ignores it. MySQL still records `DELETE_RULE = 'SET DEFAULT'`
    ///   in `information_schema`, so introspection looks right while
    ///   the parent delete is refused with error 1451.
    ///
    /// [`UPGRADING.md`]: https://github.com/ujeenet/rustango/blob/main/UPGRADING.md
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Cascade => "CASCADE",
            Self::Restrict => "RESTRICT",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
            Self::NoAction => "NO ACTION",
        }
    }
}

/// A generic ("any model") foreign key, declared at the model level.
/// It pairs a `content_type_id` column with an `object_pk` column.
/// Together they point at a row in any registered model, and the
/// model can differ per row.
///
/// Use it for audit-log targets, comments on anything,
/// activity-stream entries and generic tags. See
/// [`crate::contenttypes::GenericForeignKey`] for the runtime value
/// type and `prefetch_generic` for batched loading.
///
/// Declare it with
/// `#[rustango(generic_fk(name = "target", ct_column = "content_type_id",
/// pk_column = "object_pk"))]`. The admin renders such columns as
/// links to the target.
#[derive(Debug, Clone, Copy)]
pub struct GenericRelation {
    /// Name for the relation, used in admin labels and error
    /// messages.
    pub name: &'static str,
    /// Column on this table holding the `content_type_id` FK to
    /// `rustango_content_types.id`.
    pub ct_column: &'static str,
    /// Column on this table holding the target row's primary key.
    pub pk_column: &'static str,
}

/// Reverse-FK existence metadata from
/// `#[rustango(reverse_has(name, child, child_fk_column))]`. It holds
/// the child table, child FK column and self PK column, which is what
/// a correlated `EXISTS (SELECT 1 FROM child WHERE child_fk_column =
/// <outer>.self_pk_column)` needs.
///
/// It lives in `ModelSchema`, not only in macro output, because
/// `QuerySet::where_has(name)` and `where_doesnt_have(name)` must
/// resolve a relation by name with no `self` value at hand.
#[derive(Debug, Clone, Copy)]
pub struct ReverseRelation {
    /// Relation name as declared in `reverse_has(name = "books")`.
    /// Queryset shortcuts look it up by this.
    pub name: &'static str,
    /// `ModelSchema` of the **child** model, which is the `FROM` of
    /// the correlated subquery. The macro fills it from
    /// `<Child as Model>::SCHEMA`, so building a
    /// [`super::query::SelectQuery`] needs no runtime type lookup.
    pub child_schema: &'static ModelSchema,
    /// Column on the child table that references this model's primary
    /// key. For `Book::author_id: ForeignKey<Author>` that is
    /// `"author_id"`.
    pub child_fk_column: &'static str,
    /// Primary-key column on **this** model's table, the `OuterRef`
    /// target. Defaults to `"id"`.
    pub self_pk_column: &'static str,
}

/// Metadata for a **reverse generic-FK** relation, from
/// `#[rustango(generic_has(name, child, ct_column, pk_column))]`. It
/// lets a queryset answer "do polymorphic children point at me?" by
/// name, the way [`ReverseRelation`] does for a plain reverse FK.
///
/// The child is discriminated by content type: its `pk_column`, such
/// as `object_pk`, holds the parent's PK, and its `ct_column`, such as
/// `content_type_id`, holds the parent's content-type id. The
/// existence subquery ANDs in
/// `ct_column = (SELECT id FROM rustango_content_types WHERE "table" =
/// '<parent_table>')`, so only children pointing at *this* model
/// match. The parent table name is a compile-time constant, so no
/// async content-type lookup is needed.
#[derive(Debug, Clone, Copy)]
pub struct GenericReverseRelation {
    /// Relation name as declared in `generic_has(name = "tags")`.
    pub name: &'static str,
    /// `ModelSchema` of the **child** model, the `FROM` of the
    /// correlated subquery. The macro fills it from
    /// `<Child as Model>::SCHEMA`.
    pub child_schema: &'static ModelSchema,
    /// Column on the child table holding the parent's content-type id,
    /// such as `"content_type_id"`.
    pub ct_column: &'static str,
    /// Column on the child table holding the parent's primary key,
    /// such as `"object_pk"`. This is what the `OuterRef` matches.
    pub pk_column: &'static str,
    /// Primary-key column on **this** (parent) model's table.
    /// Defaults to `"id"`.
    pub self_pk_column: &'static str,
}

/// A multi-column ("composite") foreign key, declared at the model
/// level. Single-column FKs stay in [`FieldSchema::relation`];
/// composite ones live here so each column keeps its plain Rust type.
///
/// Declare it with
/// `#[rustango(fk_composite(name = "audit_target", to = "rustango_audit_log",
/// on = ("entity_table", "entity_pk"), from = ("table_name", "row_pk")))]`.
/// `from` and `on` must be the same length, or the macro errors.
#[derive(Debug, Clone, Copy)]
pub struct CompositeFkRelation {
    /// Name for the relation, used in admin labels, error messages
    /// and as the prefix for generated reverse accessors.
    pub name: &'static str,
    /// SQL table name of the target model.
    pub to: &'static str,
    /// Columns on this table that make up the FK, in declaration
    /// order. Same length as `on`.
    pub from: &'static [&'static str],
    /// Columns on the target table the FK points at, in the same
    /// order as `from`.
    pub on: &'static [&'static str],
}

/// One many-to-many relation, from
/// `#[rustango(m2m(name = "tags", to = "app_tags", through = "post_tags",
///                 src = "post_id", dst = "tag_id"))]`.
///
/// It lives in [`ModelSchema::m2m`] and matches **no** column on the
/// source table. The migration writer reads it to emit `CREATE TABLE`
/// for the junction table.
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
    /// Whether the migration writer creates the junction table. The
    /// default `true` emits `CREATE TABLE <through> (src_col,
    /// dst_col, UNIQUE(src_col, dst_col))`.
    ///
    /// Set `#[rustango(m2m(..., auto_create = false))]` when you
    /// declare the through table yourself with `#[derive(Model)]` and
    /// extra columns. This matches Django's
    /// `ManyToManyField(through=…)` with a custom through model.
    pub auto_create: bool,
}

/// Static description of a model.
///
/// `display` names the field to show when this model is the *target*
/// of a foreign key, so the admin renders that value instead of the
/// raw PK. Set it with `#[rustango(display = "field")]`. When `None`,
/// callers fall back to the primary key.
#[derive(Debug, Clone, Copy)]
pub struct ModelSchema {
    pub name: &'static str,
    pub table: &'static str,
    pub fields: &'static [FieldSchema],
    pub display: Option<&'static str>,
    /// App label from `#[rustango(app = "blog")]`. When `None`,
    /// [`ModelEntry::resolved_app_label`] infers it from the module
    /// path.
    pub app_label: Option<&'static str>,
    /// Admin settings from `#[rustango(admin(...))]`. When `None`,
    /// admin code uses [`AdminConfig::DEFAULT`].
    pub admin: Option<&'static AdminConfig>,
    /// Column of the `#[rustango(soft_delete)]` field, if there is
    /// one. The admin then turns a DELETE into an UPDATE that sets
    /// this column to now, instead of a hard delete.
    pub soft_delete_column: Option<&'static str>,
    /// `true` when the model has `#[rustango(permissions)]`, so
    /// [`rustango::tenancy::permissions::auto_create_permissions`]
    /// seeds the four CRUD codenames (`table.add`, `table.change`,
    /// `table.delete`, `table.view`).
    pub permissions: bool,
    /// Which fields `#[rustango(audit(track = "…"))]` captures on
    /// each write.
    ///
    /// * `None` — no `audit` attribute, so the macro emits no audit
    ///   code. The admin still records changes for all fields.
    /// * `Some(&[])` — `audit` with no `track` list: every scalar
    ///   field is captured.
    /// * `Some(&["title", "body"])` — only these fields are captured,
    ///   both by the macro's write path and by the admin diff.
    pub audit_track: Option<&'static [&'static str]>,
    /// Many-to-many relations declared via
    /// `#[rustango(m2m(name = "…", to = "…", through = "…",
    ///                 src = "…", dst = "…"))]`.
    ///
    /// Each entry describes one junction table. The migration writer
    /// reads this to emit `CREATE TABLE` / `DROP TABLE` for them.
    pub m2m: &'static [M2MRelation],
    /// Indexes from `#[rustango(index)]` on a field, or
    /// `#[rustango(index("col1, col2"))]` on the struct for a
    /// composite one. The migration writer emits `CREATE INDEX` /
    /// `DROP INDEX` per entry.
    pub indexes: &'static [IndexSchema],
    /// Table-level CHECK constraints from
    /// `#[rustango(check(name = "…", expr = "…"))]`. Each becomes
    /// `ALTER TABLE … ADD CONSTRAINT "name" CHECK (expr)` after the
    /// table is created.
    pub check_constraints: &'static [CheckConstraint],
    /// Table-level `EXCLUDE` constraints from
    /// `#[rustango(exclude(name = "…", using = "gist", elements =
    /// "col WITH op, col WITH op", where = "…"))]`.
    ///
    /// Each becomes `ALTER TABLE … ADD CONSTRAINT "name" EXCLUDE
    /// USING <using> (<elements>) [WHERE (<expr>)]` on Postgres. On
    /// MySQL and SQLite the writer emits nothing and logs a warning.
    /// Use it for "no two rows in group X may overlap in column Y",
    /// such as room bookings or calendar holds.
    pub exclusion_constraints: &'static [ExclusionConstraint],
    /// Django-shape `Meta.default_permissions`: which CRUD codenames
    /// (`"add"`, `"change"`, `"delete"`, `"view"`) are auto-created
    /// when [`Self::permissions`] is `true`. An empty slice means
    /// **all four**, as in Django.
    ///
    /// Set `#[rustango(default_permissions = "view,change")]` to drop
    /// `add` and `delete` on a read-mostly model. See also
    /// [`Self::extra_permissions`], which adds codenames without
    /// removing any CRUD ones.
    pub default_permissions: &'static [&'static str],
    /// Composite foreign keys from
    /// `#[rustango(fk_composite(name = "...", to = "...", on = (...),
    /// from = (...)))]`. Each maps a tuple of source columns to a
    /// tuple of target columns. Single-column FKs stay on
    /// [`FieldSchema::relation`], so the single-FK machinery for
    /// admin display, snapshot diff and DDL is untouched.
    pub composite_relations: &'static [CompositeFkRelation],
    /// Generic ("any model") foreign keys from
    /// `#[rustango(generic_fk(name = "...", ct_column = "...",
    /// pk_column = "..."))]`. Each pairs a `content_type_id` column
    /// with an `object_pk` column.
    pub generic_relations: &'static [GenericRelation],
    /// Where this model's table lives in a tenancy deployment: the
    /// registry database or each tenant's storage.
    ///
    /// `makemigrations` uses it to write separate registry-scoped and
    /// tenant-scoped migration files. One mixed file breaks when
    /// applied to a tenant schema, because registry tables then
    /// resolve to the wrong place through `search_path`.
    ///
    /// Set it with `#[rustango(scope = "registry")]`; the default is
    /// [`ModelScope::Tenant`]. Single-tenant projects can ignore it.
    pub scope: ModelScope,
    /// Default ordering from `#[rustango(default_order = "...")]`.
    /// Each tuple is `(column_name, desc)`, where `desc = true` sorts
    /// descending.
    ///
    /// **It is not applied automatically.** Callers must chain
    /// `QuerySet::with_default_order()`. This avoids the Django
    /// `Meta.ordering` trap where even `.count()` and `.exists()` pay
    /// for a sort.
    pub default_order: &'static [(&'static str, bool)],
    /// `true` when a SQL **view**, not a table, backs the model. Set
    /// it with `#[rustango(view)]`. View-backed models stay out of the
    /// migration snapshot, so `makemigrations` and `migrate` never
    /// emit `CREATE TABLE` or `DROP TABLE` for them; the operator owns
    /// the view. Reads work as usual.
    pub is_view: bool,
    /// Django-shape `Meta.verbose_name`: a readable singular label for
    /// the model, from `#[rustango(verbose_name = "blog post")]`. Used
    /// in admin headers, breadcrumbs and "Add <X>" buttons. When
    /// `None`, fall back to [`Self::name`].
    pub verbose_name: Option<&'static str>,
    /// Django-shape `Meta.verbose_name_plural`: the plural of
    /// [`Self::verbose_name`], from
    /// `#[rustango(verbose_name_plural = "blog posts")]`. Used in
    /// admin list headings. When `None`, callers add an `s`.
    pub verbose_name_plural: Option<&'static str>,
    /// Django-shape `Meta.managed`. `true`, the default, means
    /// rustango owns the table and migrations create, alter and drop
    /// it as the struct changes. `false` means the operator owns it:
    /// snapshots skip it, so migrations never touch it. Reads and
    /// writes still work, assuming the schema matches.
    ///
    /// Set it with `#[rustango(managed = false)]` for a table owned by
    /// another team, pipeline or legacy database that you want to
    /// query but not re-create.
    pub managed: bool,
    /// Django-shape `Meta.db_table_comment`: free text attached to
    /// the table, from `#[rustango(db_table_comment = "free text")]`.
    /// The migration writer emits it per dialect:
    ///
    /// - Postgres: `COMMENT ON TABLE "<t>" IS '...'` after the table
    /// - MySQL: an inline `COMMENT='...'` trailer
    /// - SQLite: nothing, since it has no table comments
    ///
    /// Useful for ops tooling that reads the catalog comment.
    pub db_table_comment: Option<&'static str>,
    /// Django-shape `Meta.default_related_name`: the accessor name a
    /// reverse-relation manager uses when an FK or M2M field does not
    /// set `related_name`. Rustango does not generate reverse
    /// managers, so this is metadata only.
    ///
    /// Set it with
    /// `#[rustango(default_related_name = "snake_case_name")]`. The
    /// macro checks it is a snake_case ASCII identifier, so it is
    /// safe to use as a Rust ident.
    pub default_related_name: Option<&'static str>,
    /// Django-shape `Meta.base_manager_name`: the Manager subclass
    /// `<instance>.<relation>_set` would use. This is not
    /// `default_manager_name`, which is what `Model.objects` returns.
    ///
    /// Set it with `#[rustango(base_manager_name = "ManagerExt")]`.
    /// Metadata only, like `default_related_name`.
    pub base_manager_name: Option<&'static str>,
    /// Django-shape `Meta.required_db_vendor`: the backend this model
    /// is meant to run on, from
    /// `#[rustango(required_db_vendor = "postgres|mysql|sqlite")]`.
    /// `manage check --deploy` warns when the active backend differs,
    /// so a forgotten `DATABASE_URL` shows up at deploy time and not
    /// on the first request that hits a PG-only feature.
    ///
    /// The macro normalises aliases: `"postgresql"` and `"pg"` become
    /// `"postgres"`, `"mariadb"` becomes `"mysql"`, `"sqlite3"`
    /// becomes `"sqlite"`. `None` means any backend is fine.
    pub required_db_vendor: Option<&'static str>,
    /// Django-shape `Meta.required_db_features`: capability tokens
    /// this model needs, such as `"json_extract"` or
    /// `"window_functions"`. Set them with
    /// `#[rustango(required_db_features = "tok1, tok2")]`.
    ///
    /// `manage check --deploy` warns when `Dialect::supports(token)`
    /// is `false`. It is finer-grained than `required_db_vendor` and
    /// works together with it. An empty slice needs nothing special.
    pub required_db_features: &'static [&'static str],
    /// Django-shape `Meta.order_with_respect_to`: the FK field this
    /// model's rows are ordered relative to. Django would add an
    /// `_order` column and admin reordering UI; rustango stores the
    /// name only. Set it with
    /// `#[rustango(order_with_respect_to = "parent_fk")]`.
    pub order_with_respect_to: Option<&'static str>,
    /// Django-shape `Meta.proxy`: `true` when this model reuses
    /// another struct's table. Set it with `#[rustango(proxy)]`.
    ///
    /// `makemigrations` then skips `CreateTable` for this entry,
    /// because the other struct owns the table.
    ///
    /// The migration writer and admin still treat every model as
    /// table-owning, so this is mostly metadata. For the same shape
    /// today, use the [`crate::inheritance`] extension-trait pattern.
    pub proxy: bool,
    /// Django-shape `Meta.get_latest_by`: the field
    /// `QuerySet::latest_default()` and `earliest_default()` sort on
    /// when the caller names none. Stored as `(column, descending)`;
    /// the macro splits off a leading `-`.
    ///
    /// Set it with `#[rustango(get_latest_by = "created_at")]` or
    /// `#[rustango(get_latest_by = "-priority")]`. When `None`, those
    /// methods return an error pointing at this attribute.
    pub get_latest_by: Option<(&'static str, bool)>,
    /// Django-shape `Meta.permissions`: extra permission codenames
    /// registered after the default `add/change/delete/view` set.
    /// Each tuple is `(codename, display_name)`, so an app can
    /// declare its own buckets like `("approve", "Can approve
    /// posts")`.
    ///
    /// Set them with
    /// `#[rustango(extra_permissions = "approve:Can approve, archive:Can archive")]`,
    /// the same `codename:label` shape as `choices`.
    pub extra_permissions: &'static [(&'static str, &'static str)],
    /// Eloquent-shape **global scopes**: filters added to every
    /// [`crate::query::QuerySet`] for this model. Each entry pairs a
    /// name with a function that returns a
    /// [`crate::core::WhereExpr`] at query-build time.
    ///
    /// An empty slice adds nothing, so every queryset starts
    /// unfiltered as in Django. Otherwise each queryset behaves like
    /// `qs.filter(<scope_expr>)` unless the caller chains
    /// [`crate::query::QuerySet::without_global_scope`] or
    /// [`crate::query::QuerySet::without_global_scopes`].
    ///
    /// Set them with repeated
    /// `#[rustango(global_scope(name = "...", apply = fn_path))]`,
    /// where `fn_path` names a `fn() -> WhereExpr` in scope. Use them
    /// for soft-delete hiding and tenant isolation.
    pub global_scopes: &'static [GlobalScope],
}

/// One auto-applied query filter, from
/// `#[rustango(global_scope(name = "...", apply = fn))]` on the model
/// struct. It is folded into every [`crate::query::QuerySet`] for
/// that model.
///
/// `apply` runs at query-compile time, so it can read current state
/// such as `chrono::Utc::now()` for a "published before now" scope.
/// Keep it cheap: it runs once per `compile()`.
#[derive(Debug, Clone, Copy)]
pub struct GlobalScope {
    /// Name that
    /// [`crate::query::QuerySet::without_global_scope`] uses to opt
    /// out of this one scope. Keep it unique within the model's
    /// `global_scopes`. Rustango does not check that, but a duplicate
    /// name makes the opt-out ambiguous.
    pub name: &'static str,
    /// Called at query-compile time. Returns the `WhereExpr` that is
    /// ANDed into the WHERE clause. It is a plain `fn`, not a
    /// closure, so the declaration fits in a `const` slice.
    pub apply: fn() -> crate::core::WhereExpr,
}

impl ModelSchema {
    /// Readable singular label: `verbose_name` if set, else the Rust
    /// struct name. Renderers should call this instead of repeating
    /// the fallback.
    #[must_use]
    pub fn display_label(&self) -> &'static str {
        self.verbose_name.unwrap_or(self.name)
    }

    /// Readable plural label: `verbose_name_plural` if set, else the
    /// singular label plus `"s"`. It returns an owned `String`
    /// because that fallback cannot be `&'static`.
    #[must_use]
    pub fn display_label_plural(&self) -> String {
        if let Some(plural) = self.verbose_name_plural {
            return plural.to_owned();
        }
        let base = self.verbose_name.unwrap_or(self.name);
        format!("{base}s")
    }
}

/// Where a model's table lives in a tenancy deployment. The
/// model-side twin of [`crate::migrate::MigrationScope`], so the
/// migration generator can route each change to the right scoped
/// file without touching runtime schema discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModelScope {
    /// Lives in the registry database, shared across tenants, with
    /// one row per tenant or per operator. `Org` and `Operator` are
    /// examples.
    ///
    /// A migration touching these tables MUST run as
    /// `MigrationScope::Registry`. Otherwise `migrate-tenants`
    /// re-applies it per tenant, and constraint names collide with
    /// the registry copy through `search_path`.
    Registry,
    /// Lives in each tenant's schema or database. This is the
    /// default, and covers nearly all user models and most framework
    /// ones. `makemigrations` emits them as `MigrationScope::Tenant`,
    /// and `migrate-tenants` applies them to every active org.
    #[default]
    Tenant,
}

impl ModelScope {
    /// Parse a scope for snapshot loading and attribute parsing.
    /// Accepts `"registry"` and `"tenant"`, ignoring case. Anything
    /// else gives `None`.
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

/// One table-level CHECK constraint, from
/// `#[rustango(check(name = "name", expr = "raw_sql"))]`. The
/// expression goes into the DDL as written, so quote literals and
/// name columns yourself.
#[derive(Debug, Clone, Copy)]
pub struct CheckConstraint {
    /// Constraint name used in `ALTER TABLE … ADD CONSTRAINT "name"`.
    pub name: &'static str,
    /// Raw SQL boolean expression placed inside `CHECK ( … )`.
    pub expr: &'static str,
}

/// One Postgres `EXCLUDE` constraint, Django's
/// `ExclusionConstraint`. **PG only**: MySQL and SQLite have no
/// equivalent, so the migration writer skips it there and logs a
/// `tracing::warn!`, and the rest of the migration still applies.
///
/// Declare it with `#[rustango(exclude(name = "x", using = "gist",
/// elements = "col WITH op, col WITH op", where = "raw_sql"))]`. For
/// "no two bookings of the same room may overlap in time":
///
/// ```ignore
/// #[rustango(exclude(
///     name = "no_overlap",
///     using = "gist",
///     elements = "room_id WITH =, during WITH &&",
/// ))]
/// ```
///
/// which renders as `ALTER TABLE … ADD CONSTRAINT "no_overlap" EXCLUDE
/// USING gist ("room_id" WITH =, "during" WITH &&)`.
#[derive(Debug, Clone, Copy)]
pub struct ExclusionConstraint {
    /// Constraint name used in `ALTER TABLE … ADD CONSTRAINT "name"`.
    pub name: &'static str,
    /// Index method (`gist`, `btree_gist`, `spgist`). The macro
    /// defaults to `"gist"`, because most exclusion constraints need
    /// its range-overlap support.
    pub using: &'static str,
    /// `(column, operator)` pairs in declaration order. The operator
    /// is the PG comparison for that column: usually `=` for
    /// equality, `&&` for range overlap, `@>` for containment.
    pub elements: &'static [(&'static str, &'static str)],
    /// Optional `WHERE` predicate that limits the constraint to some
    /// rows, such as only active bookings. `None` applies it to all.
    pub where_clause: Option<&'static str>,
}

/// One `CREATE INDEX` for the migration writer.
///
/// Declare it with:
/// - `#[rustango(index)]` on a field, for a single-column index
/// - `#[rustango(index("col1, col2"))]` on the struct, for a
///   composite one
///
/// Both forms take `unique` and `name` sub-attributes.
#[derive(Debug, Clone, Copy)]
pub struct IndexSchema {
    /// Index name used in `CREATE INDEX "name"` and `DROP INDEX "name"`.
    /// Auto-generated as `{table}_{col}_idx` when not supplied.
    pub name: &'static str,
    /// SQL column names included in the index, in order.
    pub columns: &'static [&'static str],
    /// `true` for `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// Access method, which becomes the `USING <method>` clause.
    /// Defaults to [`IndexMethod::BTree`].
    pub method: IndexMethod,
    /// Optional `WHERE <expr>` for a partial index, like Django's
    /// `UniqueConstraint(condition=Q(...))`. `None` emits a plain
    /// index.
    ///
    /// PG and SQLite support partial indexes natively. **MySQL does
    /// not**: it accepts the SQL but ignores the filter, so the index
    /// also rejects duplicates outside the intended subset.
    pub where_clause: Option<&'static str>,
    /// Covering-index columns, Django's
    /// `Index(fields=..., include=[...])`. PG 11+ supports
    /// `CREATE INDEX … (key_cols) INCLUDE (non_key_cols)`, where the
    /// non-key columns sit in the index leaf so an index-only scan
    /// can read them without touching the heap. Set it with
    /// `include = "col1, col2"`.
    ///
    /// MySQL has no equivalent, so the writer drops the clause and
    /// logs a `tracing::warn!`. SQLite ignores it. An empty slice
    /// means no covering columns.
    pub include: &'static [&'static str],
}

/// Index access method, the `USING <method>` in `CREATE INDEX`.
/// Covers Django's `django.contrib.postgres.indexes` types plus the
/// default B-tree.
///
/// ## Backend support
/// - **Postgres**: all variants. `Bloom` needs `CREATE EXTENSION
///   bloom`.
/// - **MySQL**: only `BTree`, plus `Hash` on the MEMORY engine.
///   Anything else falls back to btree at emit time, so the index
///   still works.
/// - **SQLite**: btree only; it has no `USING` clause, so other
///   methods are dropped at emit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexMethod {
    /// Default B-tree. Every backend supports it.
    #[default]
    BTree,
    /// Generalized Inverted Index, for full-text search, JSONB keys
    /// and array containment. **PG only**.
    Gin,
    /// Generalized Search Tree, for geometric, full-text, range and
    /// trigram queries. **PG only**.
    Gist,
    /// Block Range Index: a compact summary for very large tables
    /// whose rows follow physical storage order, such as time-series
    /// or log tables. **PG only**.
    Brin,
    /// Space-Partitioned GiST, for unbalanced structures such as
    /// quadtrees, k-d trees and suffix trees. **PG only**.
    SpGist,
    /// Hash index: equality lookups only, smaller than a btree. PG
    /// has WAL-logged it since 10; MySQL allows it on MEMORY only.
    Hash,
    /// Bloom filter index: multi-column equality with a tunable
    /// false-positive rate. **PG only**, needs the `bloom` extension.
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

/// Per-model admin settings, in the shape of Django's ModelAdmin.
/// The `Model` derive fills it in from `#[rustango(admin(...))]`.
///
/// Every field's default — an empty slice or zero — means "use the
/// framework default", so you only set what you care about.
#[derive(Debug, Clone, Copy)]
pub struct AdminConfig {
    /// Columns on the list view, in order. An empty slice shows every
    /// scalar field in declaration order. An FK column renders the
    /// target's display value when that model is in the admin too.
    pub list_display: &'static [&'static str],
    /// Fields the admin's `?q=` box searches, in order. An empty
    /// slice falls back to [`ModelSchema::searchable_fields`].
    pub search_fields: &'static [&'static str],
    /// Page size on the list view. `0` uses the admin default of 50.
    pub list_per_page: usize,
    /// List-view ordering, as `(field_name, desc)` pairs. An empty
    /// slice sorts by primary key ascending.
    pub ordering: &'static [(&'static str, bool)],
    /// Field names the edit form renders as locked inputs. The admin
    /// also leaves them out of the values it writes back.
    pub readonly_fields: &'static [&'static str],
    /// Fields shown as facet filters beside the list view. Each gets
    /// a card of its distinct values, and clicking one toggles
    /// `?<col>=<value>` in the URL. An empty slice shows no facets.
    pub list_filter: &'static [&'static str],
    /// Bulk actions above the list view. Each name is a built-in or
    /// registered handler that receives the selected row PKs;
    /// `"delete_selected"` is built in. An empty slice hides the
    /// action picker.
    pub actions: &'static [&'static str],
    /// Field groups on the create and edit form. Each [`Fieldset`]
    /// becomes a `<fieldset><legend>title</legend>…</fieldset>` block,
    /// in order. An empty slice puts every visible field in one
    /// unnamed group.
    pub fieldsets: &'static [Fieldset],
    /// Django-shape `list_display_links`: which [`Self::list_display`]
    /// columns link to the row's detail view. Each named cell is
    /// wrapped in an `<a href=…>`, so an operator can click the title
    /// directly. An empty slice leaves the trailing "View" column as
    /// the only link.
    pub list_display_links: &'static [&'static str],
    /// Django-shape `search_help_text`: a short caption beside the
    /// list view's search box, such as `"by title and author"`, so
    /// operators know what the search matches. An empty string hides
    /// it.
    pub search_help_text: &'static str,
    /// Django-shape `actions_on_top`, default `true`. Set `false` to
    /// hide the action bar above the table.
    pub actions_on_top: bool,
    /// Django-shape `actions_on_bottom`, default `false`. Set `true`
    /// to add a second action bar below the table, which helps on
    /// long list pages.
    pub actions_on_bottom: bool,
    /// Django-shape `date_hierarchy`: a date or datetime field shown
    /// as a clickable year / month / day drill-down strip above the
    /// list table. An empty string hides the strip.
    pub date_hierarchy: &'static str,
    /// Django-shape `prepopulated_fields`: `target ← source(s)` rules
    /// for the change form's client-side slug JS. An empty slice
    /// fills nothing in.
    pub prepopulated_fields: &'static [PrepopulatedField],
    /// Django-shape `raw_id_fields`: FK fields whose change-form
    /// widget gets a lookup link next to the input. The link opens
    /// the target model's list view, so the operator can find the PK
    /// without scrolling a `<select>` of every row. An empty slice
    /// adds no link.
    pub raw_id_fields: &'static [&'static str],
    /// Django-shape `autocomplete_fields`: FK fields whose
    /// change-form widget becomes a typeahead. Typing fires
    /// `GET <admin>/<target>/__autocomplete?q=…`, which filters the
    /// target rows by display field and fills a `<datalist>`. An
    /// empty slice keeps the plain input.
    pub autocomplete_fields: &'static [&'static str],
    /// Django-shape `list_select_related`: how the list view
    /// auto-joins FK columns.
    ///
    /// Rustango's default differs from Django's. Every visible FK is
    /// LEFT JOINed, so the cell shows the target's display value with
    /// no N+1 query. This setting tunes that per model.
    pub list_select_related: ListSelectRelated,
    /// Django-shape `formfield_overrides`: per-field widget overrides
    /// for the change form, as `(field_name, widget_name)` pairs. An
    /// empty slice leaves every field on its `FieldType` default.
    ///
    /// Supported widget names (built-in):
    ///
    /// - `"password"` — `<input type="password">` for String fields
    /// - `"hidden"` — `<input type="hidden">` for any field
    /// - `"textarea"` — force a `<textarea>` for short String fields
    /// - `"color"` — `<input type="color">` for String fields
    /// - `"range"` — `<input type="range">` for integer fields
    /// - `"email"` — `<input type="email">` for String fields
    /// - `"url"` — `<input type="url">` for String fields
    /// - `"tel"` — `<input type="tel">` for String fields
    /// - `"search"` — `<input type="search">` for String fields
    ///
    /// Unknown widget names log a warning and fall back to the
    /// FieldType default so a typo doesn't render an empty cell.
    pub formfield_overrides: &'static [(&'static str, &'static str)],
}

/// Django-shape `ModelAdmin.list_select_related`: the auto-JOIN
/// policy for FK columns on the admin list view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSelectRelated {
    /// The default: join every visible FK, so list cells show the
    /// target's display value in one round trip.
    All,
    /// Join nothing; FK cells show the raw PK. Good when
    /// `list_display` leaves FK columns out anyway.
    None,
    /// Join only the named FK fields. The rest show their raw PK.
    Only(&'static [&'static str]),
}

/// One `prepopulated_fields` rule: `target ← source(s)`. The admin
/// form adds JS that watches `input` events on each source field and
/// rewrites the target from a slugified join of their values. Same
/// shape as Django's `prepopulated_fields = {"slug": ("title",)}`.
#[derive(Debug, Clone, Copy)]
pub struct PrepopulatedField {
    /// Field to fill in, such as `"slug"`.
    pub target: &'static str,
    /// Source fields feeding the slug, in order, such as
    /// `&["title"]` or `&["section", "title"]`.
    pub sources: &'static [&'static str],
}

/// One group of fields on a create or edit form.
///
/// An empty `title` renders no `<legend>`, which gives a single-group
/// form with no section header.
#[derive(Debug, Clone, Copy)]
pub struct Fieldset {
    /// Section title shown as `<legend>`. Empty string suppresses it.
    pub title: &'static str,
    /// Fields in this group, in render order. Each name must match a
    /// scalar field on the model.
    pub fields: &'static [&'static str],
}

impl AdminConfig {
    /// Config for a model with no `#[rustango(admin(...))]`: every
    /// setting takes the framework default.
    pub const DEFAULT: AdminConfig = AdminConfig {
        list_display: &[],
        search_fields: &[],
        list_per_page: 0,
        ordering: &[],
        readonly_fields: &[],
        list_filter: &[],
        actions: &[],
        fieldsets: &[],
        list_display_links: &[],
        search_help_text: "",
        actions_on_top: true,
        actions_on_bottom: false,
        date_hierarchy: "",
        prepopulated_fields: &[],
        raw_id_fields: &[],
        autocomplete_fields: &[],
        list_select_related: ListSelectRelated::All,
        formfield_overrides: &[],
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

    /// Field used to show this model as a foreign-key target.
    ///
    /// Returns the `#[rustango(display = "…")]` field, or the primary
    /// key when none is set. `None` only for the unusual model with
    /// neither.
    #[must_use]
    pub fn display_field(&self) -> Option<&'static FieldSchema> {
        if let Some(name) = self.display {
            return self.field(name);
        }
        self.primary_key()
    }

    /// Fields the admin's `?q=…` search covers.
    ///
    /// The rule of thumb: a `String` field with a `max_length` is
    /// probably a name or title. Uncapped strings such as bodies and
    /// descriptions are left out, so search stays cheap.
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

    /// Reverse-FK existence relations from
    /// `#[rustango(reverse_has(name, child, child_fk_column))]`. The
    /// macro overrides this; a model with none keeps the empty
    /// default.
    ///
    /// [`crate::query::QuerySet::where_has`] and
    /// [`crate::query::QuerySet::where_doesnt_have`] use it to turn a
    /// relation name into the correlated-subquery triple without a
    /// concrete `self`.
    fn reverse_relations() -> &'static [ReverseRelation] {
        &[]
    }

    /// Reverse **generic-FK** existence relations from
    /// `#[rustango(generic_has(name, child, ct_column, pk_column))]`.
    /// The macro overrides this; a model with none keeps the empty
    /// default.
    ///
    /// The relation-existence methods, such as
    /// [`crate::query::QuerySet::where_has`] and
    /// [`crate::query::QuerySet::annotate_count`], use it to resolve
    /// a content-type-discriminated child relation by name.
    fn generic_reverse_relations() -> &'static [GenericReverseRelation] {
        &[]
    }
}

/// Inventory entry submitted by the `#[derive(Model)]` macro for each model.
///
/// Internal API: end users should not construct these directly.
#[doc(hidden)]
pub struct ModelEntry {
    pub schema: &'static ModelSchema,
    /// `module_path!()` at the registration site, such as
    /// `"my_app::blog::models"`. [`Self::resolved_app_label`] infers
    /// the app label from it when none is set.
    pub module_path: &'static str,
}

impl ModelEntry {
    /// App label for this model: the `#[rustango(app = "...")]`
    /// override if set, else the first module segment after the crate
    /// root. For crate `my_app`:
    ///
    /// * `"my_app::blog::models"`  → `Some("blog")`
    /// * `"my_app::shop::models"`  → `Some("shop")`
    /// * `"my_app::models"`        → `None`
    /// * `"my_app"`                → `None`
    ///
    /// `None` means the model sits at the project root, not in an
    /// app. Used for per-app migration discovery, admin sidebar
    /// grouping and `manage makemigrations <app>`.
    #[must_use]
    pub fn resolved_app_label(&self) -> Option<&'static str> {
        if let Some(label) = self.schema.app_label {
            return Some(label);
        }
        infer_app_label_from_module_path(self.module_path)
    }
}

/// Return the first module segment after the crate root, or `None`
/// when the model lives at the project root. Public so the admin, the
/// makemigrations CLI and `manage list-apps` all infer the app label
/// the same way.
#[must_use]
pub fn infer_app_label_from_module_path(path: &'static str) -> Option<&'static str> {
    let mut parts = path.split("::");
    let _crate_name = parts.next()?;
    let candidate = parts.next()?;
    // These segments still mean the project root: `models`, `views`
    // and `urls` are sibling files under `src/`, not apps.
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
