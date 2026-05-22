# Django 6.0 parity audit — rustango v0.41 (2026-05-21)

Single-source reference for "what does rustango cover vs Django, what's still missing." Every row points at a specific Django 6.0 doc anchor + the closest rustango source path or issue.

## Method

Status codes per row:
- **SHIPPED** — feature works end-to-end with comparable ergonomics
- **PARTIAL** — present but with caveats (PG-only, missing sub-feature, non-Django shape)
- **MISSING** — no rustango equivalent today
- **N/A** — architectural divergence makes Django's API not directly translatable (e.g. WSGI vs axum)

Counts roll up at the bottom of each section. "Top 10 gaps by user-facing impact" closes the doc.

This is a static snapshot — when in doubt about a row, `grep` the cited source path. The audit is a starting point for picking next work, not a binding contract.

---

## Summary stats

| Category | SHIPPED | PARTIAL | MISSING | N/A |
|---|---:|---:|---:|---:|
| 1. ORM — models / fields / Meta | 8 | 3 | 4 | 1 |
| 2. QuerySet API | 22 | 4 | 4 | 0 |
| 3. Field types & options | 14 | 4 | 7 | 0 |
| 4. Postgres-specific fields | 1 | 1 | 3 | 0 |
| 5. Migrations | 11 | 2 | 2 | 1 |
| 6. Admin (ModelAdmin) | 18 | 7 | 11 | 2 |
| 7. Forms / Formsets | 8 | 3 | 4 | 0 |
| 8. Generic CBVs | 5 | 2 | 3 | 0 |
| 9. URL routing | 5 | 1 | 1 | 1 |
| 10. Templates | 7 | 3 | 1 | 0 |
| 11. Authentication | 14 | 4 | 4 | 1 |
| 12. Sessions | 4 | 1 | 1 | 0 |
| 13. Manage commands | 13 | 5 | 6 | 4 |
| 14. Settings | 10 | 4 | 4 | 2 |
| 15. Security / middleware | 14 | 1 | 1 | 0 |
| 16. Caching | 7 | 1 | 2 | 0 |
| 17. Signals | 7 | 2 | 5 | 0 |
| 18. Email | 6 | 2 | 1 | 0 |
| 19. Files / Storage | 3 | 2 | 3 | 0 |
| 20. i18n / l10n | 1 | 2 | 6 | 0 |
| 21. Testing | 7 | 3 | 3 | 0 |
| 22. Async support | 5 | 0 | 0 | 2 |
| 23. DRF parity | 11 | 4 | 4 | 0 |
| 24. contrib modules | 6 | 4 | 4 | 0 |
| **Totals** | **205** | **65** | **84** | **14** |

Coverage = 205 / (205 + 65 + 84) = **58% full, 19% partial, 24% missing** vs Django 6.0 surface (excluding 14 N/A rows). Partial+shipped = **77%**.

---

## 1. ORM — models, fields, Meta, custom Manager, abstract base classes

| Django capability | Django doc | rustango status | rustango pointer | Gap / notes |
|---|---|---|---|---|
| `class Model(models.Model)` declaration | [Models#fields](https://docs.djangoproject.com/en/6.0/topics/db/models/) | SHIPPED | `#[derive(Model)]` in [crates/rustango-macros/src/lib.rs](crates/rustango-macros/src/lib.rs) | Tri-dialect since v0.38. |
| `Meta.db_table` | [Meta options#db_table](https://docs.djangoproject.com/en/6.0/ref/models/options/#db-table) | SHIPPED | `#[rustango(table = "...")]` | |
| `Meta.ordering` | [Meta options#ordering](https://docs.djangoproject.com/en/6.0/ref/models/options/#ordering) | SHIPPED | `#[rustango(default_order = "-created, +status")]` per-query opt-in via `.with_default_order()` (closed #291) | Per-query opt-in by design — avoids Django's "every query pays" footgun. |
| `Meta.unique_together` | [Meta#unique_together](https://docs.djangoproject.com/en/6.0/ref/models/options/#unique-together) | SHIPPED | `#[rustango(unique_together(...))]` (v0.19) | |
| `Meta.index_together` | [Meta#index_together](https://docs.djangoproject.com/en/6.0/ref/models/options/#index-together) | SHIPPED | `#[rustango(index_together(...))]` (v0.19) | |
| `Meta.constraints` (CheckConstraint, UniqueConstraint) | [Constraints](https://docs.djangoproject.com/en/6.0/ref/models/constraints/) | PARTIAL | `#[rustango(check(name, expr))]` + `unique_when` (partial unique) (#319) | `ExclusionConstraint` MISSING; no row-level CHECK with cross-field expression beyond raw SQL. |
| `Meta.verbose_name` / `verbose_name_plural` | [Meta#verbose_name](https://docs.djangoproject.com/en/6.0/ref/models/options/#verbose-name) | SHIPPED | `#[rustango(verbose_name = "...", verbose_name_plural = "...")]` (#320, v0.42) — `ModelSchema::display_label()` + `display_label_plural()`; admin list / detail / form templates pick up the friendly captions via `model.label` / `model.label_plural` | |
| `Meta.permissions` (custom codenames) | [Meta#permissions](https://docs.djangoproject.com/en/6.0/ref/models/options/#permissions) | SHIPPED | `auto_create_permissions_pool` seeds CRUD codenames; reserved `auth.access_admin` added in PR #313 | Custom per-model codenames not yet declarative — set via `set_user_perm_pool` runtime. |
| `Meta.default_manager_name` | [Custom Managers](https://docs.djangoproject.com/en/6.0/topics/db/managers/) | SHIPPED | `#[rustango(manager_fn = "active")]` (closed #289 / T2.6) | Multiple `manager_fn` allowed. |
| Custom `Manager` subclass | [Custom Managers](https://docs.djangoproject.com/en/6.0/topics/db/managers/) | SHIPPED | `#[rustango(manager(ext = "FooManagerExt"))]` emits an extension trait the user impls on `QuerySet<Self>` (T1.9) | |
| Abstract base classes | [Abstract base](https://docs.djangoproject.com/en/6.0/topics/db/models/#abstract-base-classes) | MISSING | n/a (#321) | No `#[derive(Model)]` "abstract" attribute. Composition via traits is the Rust idiom — but the field-inheritance Django pattern isn't surfaced. |
| Multi-table inheritance | [MTI](https://docs.djangoproject.com/en/6.0/topics/db/models/#multi-table-inheritance) | MISSING | n/a (#322) | rustango-cms uses MTI for PageType (custom impl); framework doesn't auto-generate it. |
| Proxy models | [Proxy](https://docs.djangoproject.com/en/6.0/topics/db/models/#proxy-models) | MISSING | n/a (#323) | No equivalent. |
| `ForeignKey(on_delete=...)` | [FK options](https://docs.djangoproject.com/en/6.0/ref/models/fields/#django.db.models.ForeignKey.on_delete) | SHIPPED | `pub author: ForeignKey<Author>` field; FK constraint emits `ON DELETE` per dialect | `on_delete` enum (CASCADE / PROTECT / SET_NULL / SET_DEFAULT / DO_NOTHING) is currently a fixed default — explicit override MISSING (see "Gap notes"). |
| Self-referential FK | [Self FK](https://docs.djangoproject.com/en/6.0/ref/models/fields/#django.db.models.ForeignKey) | SHIPPED | `#[rustango(fk = "self")]` (v0.17.2) | Tested via `tests/self_fk_live.rs`. |
| `ManyToManyField(through=...)` | [M2M through](https://docs.djangoproject.com/en/6.0/topics/db/models/#extra-fields-on-many-to-many-relationships) | PARTIAL | `#[rustango(m2m(through = "...", src = "...", dst = "..."))]` (#324) | Implicit through-table OK; explicit through-MODEL with extra fields requires hand-rolling. |

Summary: **8 SHIPPED / 3 PARTIAL / 4 MISSING / 1 N/A**. Gaps: abstract base + MTI + proxy + `Meta.verbose_name` + `on_delete=PROTECT/SET_NULL` override + ExclusionConstraint.

---

## 2. QuerySet API (30+ methods)

| Django method | Doc | Status | rustango pointer | Notes |
|---|---|---|---|---|
| `.filter(**kwargs)` (string keyed) | [filter](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#filter) | SHIPPED | [crates/rustango/src/query/mod.rs#QuerySet::filter](crates/rustango/src/query/mod.rs) | `__icontains`, `__in`, `__gte`, `__between` etc. via `parse_lookup`. |
| `.exclude(**kwargs)` | [exclude](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#exclude) | SHIPPED | `.exclude(...)` | |
| `Q()` composable | [Q](https://docs.djangoproject.com/en/6.0/topics/db/queries/#complex-lookups-with-q-objects) | SHIPPED | `Q!()` compile-time macro + `core::query::Q` runtime (T1.1, T1.7) | |
| `F()` expression | [F](https://docs.djangoproject.com/en/6.0/ref/models/expressions/#f) | SHIPPED | `F("col")` in [src/core/expr.rs](crates/rustango/src/core/expr.rs) | |
| `.annotate(alias=expr)` | [annotate](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#annotate) | SHIPPED | `AggregateBuilder::annotate` + tri-dialect GROUP BY parity (T2.8 closed) | |
| `.alias()` (non-projected) | [alias](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#alias) | SHIPPED | T1.6 closed | |
| `.aggregate(**kwargs)` | [aggregate](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#aggregate) | SHIPPED | `aggregates::{count_all, sum, avg, min, max, ...}` | |
| Window functions (`Window`, `RowNumber`, `Rank`, `Lag`, `Lead`) | [Window](https://docs.djangoproject.com/en/6.0/ref/models/expressions/#window-functions) | SHIPPED | `core::window::{row_number, rank, dense_rank, lag, lead, ntile}` | |
| `Subquery()`, `OuterRef()`, `Exists()` | [Subquery](https://docs.djangoproject.com/en/6.0/ref/models/expressions/#subquery-expressions) | SHIPPED | `core::subquery::{Subquery, OuterRef, exists}` | |
| `Case` / `When` | [Case](https://docs.djangoproject.com/en/6.0/ref/models/expressions/#case) | SHIPPED | `core::case::case()` builder | |
| `.values(*fields)` (dict-shape projection) | [values](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#values) | SHIPPED | `.values_dict(&["col", ...])` + `.values(...).annotate(...)` Shape 2/3 (T2.8) | |
| `.values_list(*fields, flat=True)` | [values_list](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#values-list) | SHIPPED | `.values_list(...)` + `.values_list_flat(col)` | |
| `.distinct()` / `.distinct(*fields)` | [distinct](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#distinct) | SHIPPED | T1.2 closed: PG `DISTINCT ON` + MySQL/SQLite window-fn fallback | |
| `.order_by(*fields)` | [order_by](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#order-by) | SHIPPED | `.order_by(&[("col", false), ...])` + `.unordered()` | |
| `.reverse()` | [reverse](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#reverse) | SHIPPED | `QuerySet::reverse()` (#325, v0.42) — flips the `desc` flag on every pending `ORDER BY` entry. No-op when no ordering is set; `Random` entries are left untouched. | |
| `.select_related(*fk)` (single hop) | [select_related](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#select-related) | SHIPPED | `lower_select_related` in query/mod.rs | |
| `.select_related("a__b__c")` (multi-hop) | [select_related](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#select-related) | PARTIAL | T2.2 closed (PR #308) — JOIN emission ships; decoder-side recursive stitching deferred (#451) | Filter-on-deep-column works via `Expr::AliasedColumn`; `post.author.profile.get_pool()` auto-stitch is a follow-up. |
| `.prefetch_related(*rel)` | [prefetch_related](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#prefetch-related) | SHIPPED | `sql::fetch_with_prefetch_pool` | Tri-dialect; non-i64 FK PKs supported (v0.26). |
| `.prefetch_related(Prefetch(qs))` | [Prefetch](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#django.db.models.Prefetch) | SHIPPED | `sql::fetch_with_prefetch_filtered_pool` (T2.1 closed, PR #309) | Global `LIMIT` only — Django's per-parent slice (LATERAL JOIN) is a follow-up. |
| `.raw(sql)` | [raw](https://docs.djangoproject.com/en/6.0/topics/db/sql/#executing-raw-queries) | SHIPPED | `sql::raw_query_pool` | |
| `bulk_create(objs)` | [bulk_create](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#bulk-create) | SHIPPED | `Model::bulk_insert` | |
| `bulk_create(objs, update_conflicts=True)` (UPSERT) | [bulk_create](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#bulk-create) | SHIPPED | `Model::bulk_upsert` + `bulk_insert_or_ignore` (T1.5 closed) | Picks `unique_together` target when defined (v0.26 fix). |
| `.bulk_update(objs, fields)` | [bulk_update](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#bulk-update) | SHIPPED | `sql::bulk_update_pool(pool, rows, fields)` — tri-dialect bulk update emitter (`sql/executor.rs:2402`, sqlite impl `sql/sqlite.rs:448`, mysql impl `sql/mysql.rs:595`). Closes #326. | |
| `.bulk_delete()` / `.delete()` on QuerySet | [delete](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#delete) | SHIPPED | `QuerySet::delete().execute_on(pool)` | |
| `.select_for_update()` | [select_for_update](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#select-for-update) | SHIPPED | `.with_lock(LockMode::ForUpdate)` + dialect-aware warning on SQLite (T2.9 closed) | |
| `.iterator(chunk_size=N)` | [iterator](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#iterator) | SHIPPED | `.iterator(chunk_size)` returns `ChunkedIter<T>` | |
| `.explain()` | [explain](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#explain) | SHIPPED | `sql::explain_pool` tri-dialect (T1.10 closed) | PG / MySQL / SQLite each emit their native EXPLAIN. |
| `.in_bulk(ids)` | [in_bulk](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#in-bulk) | SHIPPED | `QuerySet::in_bulk` | |
| `.dates(field, kind)` | [dates](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#dates) | SHIPPED | `QuerySet::dates(field, DateKind)` + `sql::fetch_dates_pool` (closed #327, v0.42). Tri-dialect via per-backend trunc fragments (PG `DATE_TRUNC`, MySQL `DATE_FORMAT` + cast, SQLite `strftime`). Filters / joins / limits on the underlying QuerySet pass through to the truncation pipeline. `order_desc(true)` reverses output. | |
| `.datetimes(field, kind)` | [datetimes](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#datetimes) | SHIPPED | `QuerySet::datetimes(field, DateTimeKind)` + `sql::fetch_datetimes_pool` (closed #328, v0.42). Same shape as `.dates()` but accepts `Hour` / `Minute` / `Second` granularity and returns `DateTime<Utc>`. | |
| `.latest(*fields)` / `.earliest(*fields)` | [latest](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#latest) | SHIPPED | `QuerySet::latest` / `earliest` on pool | |
| `.first()` / `.last()` | [first](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#first) | SHIPPED | `QuerySet::first` / `.last` | |
| `.count()` / `.exists()` | [count](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#count) | SHIPPED | `.count_pool` / `.exists_pool` | |
| `.get_or_create()` / `.update_or_create()` | [get_or_create](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#get-or-create) | SHIPPED | `sql::get_or_create` + `sql::update_or_create` | |
| `.union()` / `.intersection()` / `.difference()` | [union](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#union) | SHIPPED | `QuerySet::{union, union_all, intersection, difference}` + `with_compound(SetOp, …)` (closed #329, v0.42). Writer emits the bare compound shape (`SELECT … UNION SELECT …`) — portable across PG / MySQL / SQLite. Branches with per-branch `ORDER BY` / `LIMIT` / `OFFSET` wrap in `SELECT * FROM (<branch>)` so those clauses scope to the branch instead of attaching to the outer compound. **MySQL caveat**: native `INTERSECT` / `EXCEPT` require 8.0.31+; pre-8.0.31 surfaces a driver syntax error. | |
| `.contains(obj)` / `.exists()` | [contains](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#contains) | SHIPPED | `ExistsPool::exists_pool(&pool)` + `ExistsPool::contains_pk(&pool, pk)` (#330, v0.42). `contains_pk` looks up the PK column from the schema; takes a typed pk value rather than the obj instance to keep the Rust API explicit. | |
| `.none()` | [none](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#none) | SHIPPED | `QuerySet::none()` (closed #331, v0.42) — SELECT compiles to `LIMIT 0`; UPDATE / DELETE add an `<pk> IS NULL` guard against the (NOT NULL) primary key so every row is rejected. Chained filters / ordering are preserved alongside the marker (Django semantic). | |
| `__lookup`s (`__icontains`, `__lt`, `__in`, `__between`, `__range`, `__isnull`, ...) | [Field lookups](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#field-lookups) | SHIPPED | `parse_lookup` in query/mod.rs | Most Django lookups; missing: `__date`, `__year__lte`, etc. requiring transform chains. |
| `.using(db_alias)` (multi-DB routing) | [using](https://docs.djangoproject.com/en/6.0/ref/models/querysets/#using) | MISSING | n/a (#332) | rustango is single-pool-per-QuerySet (or tenant-scoped); multi-DB router missing. |

Summary: **22 SHIPPED / 4 PARTIAL / 4 MISSING / 0 N/A**. Gaps: `.reverse()`, `.dates/datetimes`, `.contains/.none` sugar, multi-DB `.using()`.

---

## 3. Field types & options

Built-in Django fields:

| Django field | Doc | Status | rustango pointer | Notes |
|---|---|---|---|---|
| `CharField(max_length=N)` | [CharField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#charfield) | SHIPPED | `String` + `#[rustango(max_length = 200)]` | |
| `TextField` | [TextField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#textfield) | SHIPPED | `String` without `max_length` | Same Rust type; DDL emits TEXT. |
| `IntegerField` / `BigIntegerField` / `SmallIntegerField` | [IntegerField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#integerfield) | SHIPPED | `i32` / `i64` / `i16` Rust types | |
| `PositiveIntegerField` | [PositiveIntegerField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#positiveintegerfield) | SHIPPED | `#[rustango(min = 0)]` on any `i32` / `i64` field — `core::validate_value` calls `check_int_range` on every typed INSERT / UPDATE and emits `QueryError::OutOfRange` for negatives (closed #333, v0.42). Migration writer also emits a `CHECK (col >= 0)` constraint so the DB rejects out-of-band writes. Regression: `tests/positive_int_field.rs`. | |
| `FloatField` / `DecimalField` | [DecimalField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#decimalfield) | SHIPPED | `f64` / `rust_decimal::Decimal` | |
| `BooleanField` | [BooleanField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#booleanfield) | SHIPPED | `bool` | |
| `DateField` / `DateTimeField` / `TimeField` | [DateField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#datefield) | SHIPPED | `chrono::NaiveDate` / `DateTime<Utc>` / `NaiveTime` | |
| `auto_now` / `auto_now_add` | [DateField#auto_now](https://docs.djangoproject.com/en/6.0/ref/models/fields/#django.db.models.DateField.auto_now) | SHIPPED | `Auto<DateTime<Utc>>` + `#[rustango(auto_now)]` / `auto_now_add` | |
| `UUIDField` | [UUIDField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#uuidfield) | SHIPPED | `uuid::Uuid` | |
| `JSONField` | [JSONField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#jsonfield) | SHIPPED | `serde_json::Value` + JSON path lookups (T2.3 closed, PR #307) | |
| `BinaryField` | [BinaryField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#binaryfield) | SHIPPED | `Vec<u8>` mapped to BYTEA/BLOB | |
| `EmailField` (validator) | [EmailField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#emailfield) | SHIPPED | `#[rustango(max_length = 200, validators = "email")]` on a `String` field — validator runs model-side on every typed INSERT / UPDATE via `core::validate_value` (`core/validate.rs:104`). Covered by `tests/macro_validators.rs`. Closes #334. | |
| `URLField` | [URLField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#urlfield) | SHIPPED | `#[rustango(validators = "url")]` on a `String` field — same model-side path as EmailField. Closes #335. | |
| `SlugField` | [SlugField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#slugfield) | SHIPPED | `#[rustango(validators = "slug")]` (`core/validate.rs:106`) — validates against Django's `[a-zA-Z0-9_-]+` shape. Slug auto-generation from siblings is the separate `prepopulated_fields` admin facet (#356, v0.42). Closes #336. | |
| `IPAddressField` / `GenericIPAddressField` | [GenericIPAddressField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#genericipaddressfield) | SHIPPED | `#[rustango(validators = "ip_address")]` (or `"ipv4"` / `"ipv6"` for protocol-specific) (#337, v0.42). Alias `genericipaddress` accepted for direct Django translation. Column type stays `String`. | |
| `FilePathField` | [FilePathField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#filepathfield) | SHIPPED | `#[rustango(validators = "filepath")]` (#338, v0.42) — structural-only: non-empty, no NUL, no `..` parent-dir segments. Alias `filepath_field`. Filesystem-existence checks remain project-specific. | |
| `FileField` (model side) | [FileField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#filefield) | MISSING | n/a (#339) | rustango has `Media` model + Storage backends; not a native FileField on arbitrary models. |
| `ImageField` | [ImageField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#imagefield) | MISSING | n/a (#340) | Same — image-specific FileField pattern not surfaced. |
| `GeneratedField` (DB-computed) | [GeneratedField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#generatedfield) | SHIPPED | `#[rustango(generated_as = "price * qty")]` emits `GENERATED ALWAYS AS (...) STORED` | |
| `Auto<i64>` / `BigAutoField` / `DEFAULT_AUTO_FIELD` | [Auto fields](https://docs.djangoproject.com/en/6.0/ref/models/fields/#bigautofield) | SHIPPED | `Auto<i64>` typed | |

Field options:

| Option | Status | rustango | Notes |
|---|---|---|---|
| `max_length` | SHIPPED | `#[rustango(max_length = N)]` | |
| `null=True` (DB-level nullable) | SHIPPED | `Option<T>` Rust type | |
| `blank=True` (form-level allow empty) | SHIPPED | `#[rustango(blank)]` (#445, v0.42) — admin form drops the `required` HTML attribute even on NOT NULL columns; distinct from `Option<T>` which controls SQL nullability | |
| `default` | SHIPPED | `#[rustango(default = "expr")]` | |
| `choices=[...]` | SHIPPED | `#[rustango(choices = "draft:Draft, published:Published")]` (#446, v0.42) — admin renders `<select>`, validator rejects off-choice values | |
| `validators=[...]` | SHIPPED | `#[rustango(validators = "email,url")]` (#447, v0.42) — comma-separated names dispatch to the `validators::*` family on every typed INSERT/UPDATE via `core::validate_value`. Supports email, url, slug, unicode_slug, phone_e164, hex_color, uuid, iso_date, iso_time, iso_datetime, ipv4, ipv6, no_null, email_list, integer. | |
| `db_index=True` | SHIPPED | `#[rustango(index)]` (field-level) + container-level `index(...)` | |
| `db_column` | SHIPPED | `#[rustango(db_column = "...")]` | |
| `help_text` | SHIPPED | `#[rustango(help_text = "...")]` (v0.40) — admin renders below input | |
| `verbose_name` | SHIPPED | `#[rustango(verbose_name = "Display title")]` (#448, v0.42) — admin column headers + form labels render via `FieldSchema::display_label()` | |
| `primary_key=True` | SHIPPED | `#[rustango(primary_key)]` | |
| `unique=True` | SHIPPED | `#[rustango(unique)]` | |
| `editable=False` | SHIPPED | `#[rustango(editable = false)]` (#449, v0.42) — admin change-form skips the field entirely; list / detail views still show the value | |
| `db_comment` | SHIPPED | `#[rustango(db_comment = "...")]` (#450, v0.42) — PG emits post-table `COMMENT ON COLUMN`, MySQL inlines `COMMENT '...'`, SQLite no-op (no native support) | |
| `db_tablespace` | N/A | n/a | Tablespaces are PG-specific niche. |

Summary: **14 SHIPPED / 4 PARTIAL / 7 MISSING / 0 N/A** in this section. Gaps cluster around: `choices`, `verbose_name`, `editable`, IP/FilePath/File/ImageField, model-level validators.

---

## 4. Postgres-specific fields (`django.contrib.postgres.fields`)

| Django field | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `JSONField` | (now built-in) | SHIPPED | See section 3 | |
| `ArrayField` | [ArrayField](https://docs.djangoproject.com/en/6.0/ref/contrib/postgres/fields/#arrayfield) | PARTIAL | `SqlValue::Array` + `Op::ArrayContains/ContainedBy/Overlap` operators emitted on PG; no native typed field wrapper in models (#341) | Future-backlog item. |
| `HStoreField` | [HStoreField](https://docs.djangoproject.com/en/6.0/ref/contrib/postgres/fields/#hstorefield) | MISSING | n/a (#342) | |
| `RangeField` (Int / Date / DateTime / Decimal) | [RangeField](https://docs.djangoproject.com/en/6.0/ref/contrib/postgres/fields/#range-fields) | MISSING | n/a — `SqlValue::RangeLiteral` exists but no typed field (#343) | |
| `CITextField` | [CITextField](https://docs.djangoproject.com/en/6.0/ref/contrib/postgres/fields/#citext-fields) | MISSING | n/a (#344) | |

Summary: **1 / 1 / 3 / 0**.

---

## 5. Migrations

| Capability | Doc | Status | rustango pointer | Notes |
|---|---|---|---|---|
| `python manage.py makemigrations` | [makemigrations](https://docs.djangoproject.com/en/6.0/topics/migrations/#creating-migrations) | SHIPPED | `manage makemigrations` in [src/migrate/manage.rs](crates/rustango/src/migrate/manage.rs) | Snapshot-based JSON file output. |
| `python manage.py migrate [app] [target]` | [migrate](https://docs.djangoproject.com/en/6.0/topics/migrations/#applying-migrations) | SHIPPED | `manage migrate` | |
| `python manage.py showmigrations` | [showmigrations](https://docs.djangoproject.com/en/6.0/ref/django-admin/#showmigrations) | SHIPPED | `manage showmigrations` | |
| `python manage.py sqlmigrate` | [sqlmigrate](https://docs.djangoproject.com/en/6.0/ref/django-admin/#sqlmigrate) | SHIPPED | `manage sqlmigrate <name>` (closed #345, v0.42) — prints the SQL the named migration would emit when applied, no DB touch required. Wraps `migrate::sqlmigrate_one(dir, name)` which reads the JSON file and runs the same render path as `migrate --dry-run`. | |
| `python manage.py squashmigrations` | [squashmigrations](https://docs.djangoproject.com/en/6.0/topics/migrations/#squashing-migrations) | SHIPPED | `manage migrate --squash` (v0.29) | Fresh-table scenarios only; not full Django squash. |
| `python manage.py makemigrations --merge` | [merge](https://docs.djangoproject.com/en/6.0/topics/migrations/#merging-migrations) | MISSING | n/a (#346) | |
| `migrate --fake` / `--fake-initial` | [fake](https://docs.djangoproject.com/en/6.0/ref/django-admin/#cmdoption-migrate-fake) | SHIPPED | `manage migrate --fake` (v0.28) | |
| `RunPython` (data migration) | [RunPython](https://docs.djangoproject.com/en/6.0/ref/migration-operations/#runpython) | SHIPPED | `register_migration_callback!(name, fn)` + `Operation::Callback { name, reverse_name }` (closed #347, v0.42). Migration JSON references the callback by name; runner looks it up in the inventory registry at apply time. Unknown names surface a clear `MigrateError::Validation` with a pointer to the registration macro. The callback runs OUTSIDE the migration's surrounding tx (owned-Pool signature) — operators wanting atomicity should set `atomic: false` on the migration. `sqlmigrate` preview emits `-- RunPython: <name>` as a comment. | |
| `RunSQL` | [RunSQL](https://docs.djangoproject.com/en/6.0/ref/migration-operations/#runsql) | SHIPPED | `migrate_data::RunSQL` op | Includes `reverse_sql`. |
| `AddField` / `RemoveField` / `AlterField` / `RenameField` | [Operations](https://docs.djangoproject.com/en/6.0/ref/migration-operations/) | SHIPPED | `SchemaChange::AddColumn` / `DropColumn` / `AlterColumn` / `RenameColumn` | RenameField via `rename_from` metadata (v0.4). |
| `AddIndex` / `RemoveIndex` | [AddIndex](https://docs.djangoproject.com/en/6.0/ref/migration-operations/#addindex) | SHIPPED | Index attrs auto-diff into `CreateIndex`/`DropIndex` ops | |
| `AddConstraint` / `RemoveConstraint` | [AddConstraint](https://docs.djangoproject.com/en/6.0/ref/migration-operations/#addconstraint) | SHIPPED | CHECK + UNIQUE constraints | |
| Migration dependencies | [dependencies](https://docs.djangoproject.com/en/6.0/topics/migrations/#django.db.migrations.migration.Migration.dependencies) | SHIPPED | `previous_migration` field links the chain | |
| Atomic migrations | [atomic](https://docs.djangoproject.com/en/6.0/howto/writing-migrations/#non-atomic-migrations) | SHIPPED | Each migration applies in a transaction | |
| Schema-mode per-tenant migrations | (Django doesn't ship this) | N/A | `tenancy::migrate` runs per-tenant | Beyond Django's per-DB. |
| inspectdb (tables + views) | [inspectdb](https://docs.djangoproject.com/en/6.0/ref/django-admin/#inspectdb) | SHIPPED | T2.10 closed — views walked too (PR #304) | |

Summary: **11 SHIPPED / 2 PARTIAL / 2 MISSING / 1 N/A**. Gaps: `sqlmigrate` raw-SQL print, `migrations --merge`.

---

## 6. Admin (ModelAdmin parity)

| ModelAdmin option | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `list_display` (scalar fields) | [list_display](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.list_display) | SHIPPED | `#[rustango(admin(list_display = "title, status"))]` | |
| `list_display` (method/computed fields) | same | SHIPPED | Two paths: `register_admin_computed!(table, name, label, fn)` for arbitrary HTML renderers (pre-v0.42), and `list_display = "data.headline"` dotted-path syntax for JSON-column subkeys (closed #348, v0.42). The dotted path drills into a `FieldType::Json` column at `.split('.')` segments (numeric segments index arrays); bools render as ☑/☐ glyphs, missing paths fall back to `<em>NULL</em>`. | |
| `list_display` (boolean checkbox icon) | same | SHIPPED | v0.37+ render in admin/render.rs | |
| `list_display` (callable display_link) | same | MISSING | n/a (#349) | FK columns auto-link; method-field callables don't. |
| `list_display_links` | [list_display_links](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.list_display_links) | SHIPPED | `admin(list_display_links = "title, views")` (#350, v0.42) — comma-separated names from `list_display`. Each matched cell wraps its inner HTML in `<a href="{admin_prefix}/{table}/{pk}">…</a>`. Empty whitelist keeps the trailing "View" column as the only link. | |
| `list_filter` (FieldListFilter) | [list_filter](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.list_filter) | SHIPPED | `#[rustango(admin(list_filter = "..."))]` | |
| `list_filter` (SimpleListFilter — custom) | same | SHIPPED | `register_admin_list_filter!(table, parameter_name, title, lookups, to_filters_fn)` (closed #351, v0.42) — declares a custom filter card on the list view with operator-defined lookups + predicate function. The function maps the URL value to `Vec<Filter>` predicates that AND onto the WHERE. Rendered as a right-rail card alongside field-value facets. | |
| `list_per_page` | [list_per_page](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.list_per_page) | SHIPPED | `#[rustango(admin(list_per_page = 50))]` | |
| `list_select_related` | [list_select_related](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.list_select_related) | SHIPPED | `admin(list_select_related = "all"\|"none"\|"author, …")` (closed #352, v0.42). Three modes via new `core::ListSelectRelated` enum: `All` (default, joins every visible FK — rustango's pre-existing behavior), `None` (opt out of all auto-joins; cells render the raw PK), `Only(&[..])` (whitelist named FK fields). `build_fk_joins` consults the attr. | |
| `ordering` | [ordering](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.ordering) | SHIPPED | `#[rustango(admin(ordering = "..."))]` | |
| `search_fields` | [search_fields](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.search_fields) | SHIPPED | `#[rustango(admin(search_fields = "..."))]` + tri-dialect ILIKE (fixed v0.37) | |
| `search_help_text` | [search_help_text](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.search_help_text) | SHIPPED | `admin(search_help_text = "Search by title only")` (#353, v0.42) — caption rendered next to the search box as `<small class="search-help">…</small>`. Empty string suppresses. | |
| `readonly_fields` | [readonly_fields](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.readonly_fields) | SHIPPED | `#[rustango(admin(readonly_fields = "..."))]` | |
| `fieldsets` | [fieldsets](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.fieldsets) | SHIPPED | `#[rustango(admin(fieldsets = "Title: a, b | Other: c"))]` | |
| `actions` (bulk) | [actions](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.actions) | SHIPPED | `#[rustango(admin(actions = "publish, archive"))]` + handler registry | |
| `actions_on_top` / `actions_on_bottom` | same | SHIPPED | `admin(actions_on_top = false, actions_on_bottom = true)` (#354, v0.42). Defaults match Django: top=true, bottom=false. Bottom selector uses `name="action_bottom"` to avoid clobbering top's empty default; handler picks first non-empty value. | |
| `date_hierarchy` | [date_hierarchy](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.date_hierarchy) | SHIPPED | `admin(date_hierarchy = "field")` (closed #355, v0.42) — clickable year/month/day strip above the list table. URL `?year[&month[&day]]` narrows via half-open `[lo, hi)` predicates; bucket enumeration uses tri-dialect `EXTRACT` (PG/MySQL) / `strftime` (SQLite). | |
| `prepopulated_fields` (slug-from-title) | [prepopulated_fields](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.prepopulated_fields) | SHIPPED | `admin(prepopulated_fields = "slug:title")` (closed #356, v0.42) — change-form emits inline JS that slugifies source field values into the target field on every keystroke. Multi-source via `target:src1+src2`. Suppressed on edit (Django semantic). | |
| `raw_id_fields` (large-FK widget) | [raw_id_fields](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.raw_id_fields) | SHIPPED | `admin(raw_id_fields = "author_id")` (closed #357, v0.42) — change-form FK input gets a magnifying-glass lookup link pointing at the target model's admin list view, opening in a new tab so operators can find the right PK without scrolling. | |
| `autocomplete_fields` | [autocomplete_fields](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.autocomplete_fields) | SHIPPED | `admin(autocomplete_fields = "author_id")` (closed #358, v0.42) — change-form FK input wires to a `<datalist>` populated via a new `GET <admin>/<target>/__autocomplete?q=…` JSON endpoint. Server filters by `search_fields` (with auto-searchable fallback); results capped at 100. | |
| `formfield_overrides` | [formfield_overrides](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.formfield_overrides) | MISSING | n/a (#359) | |
| `get_queryset(self, request)` override | [get_queryset](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.get_queryset) | PARTIAL | Custom Manager (`manager_fn`) gives the parallel (#360) | No request-aware QS hook. |
| `has_add_permission` / `change` / `delete` / `view` | [has_*_permission](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.has_add_permission) | PARTIAL | `auto_create_permissions_pool` seeds codenames; `permission_required` middleware gates routes (#361) | Per-object hook MISSING. |
| Tabular / Stacked inlines | [InlineModelAdmin](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#inlinemodeladmin-objects) | SHIPPED | `register_admin_inline_tabular!` / `_stacked!` macros (v0.27+) | |
| Generic inlines | [generic-inline-admin](https://docs.djangoproject.com/en/6.0/ref/contrib/contenttypes/#generic-inline-model-admin) | SHIPPED | `register_admin_inline_generic!` (v0.40, closed #242–#244) | |
| Custom URLs (`get_urls`) | [get_urls](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.get_urls) | PARTIAL | Builder exposes `register_action` for bulk actions (#362) | No arbitrary `/<model>/custom/` routes per admin. |
| Custom views per model | (above) | MISSING | n/a (#363) | |
| Multiple AdminSite registries | [AdminSite](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.AdminSite) | PARTIAL | `admin::Builder` supports `show_only` / `read_only` per builder (#364) | Not multiple side-by-side admin instances; tenant admin is a distinct surface. |
| `ModelAdmin.save_model()` / `delete_model()` hooks | [save_model](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.save_model) | SHIPPED | `signals::admin::{connect_admin_pre_save, connect_admin_post_save, connect_admin_pre_delete, connect_admin_post_delete}` (closed #365, v0.42). Admin-only seam complementing `post_save` (which fires for every ORM write). Context carries `table`, `pk`, and `change` (create-vs-update). | |
| `ModelAdmin.history_view` (object history) | [history-view](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#django.contrib.admin.ModelAdmin.history_view) | SHIPPED | Audit log read-only panel on detail page (v0.28+) | |
| Recent actions widget | (admin home) | SHIPPED | Right-rail activity feed on the admin `/` index (closed #366, v0.42) — pulls the newest 10 entries from `rustango_audit_log` via `audit::list`, links each entry to its detail page. Hidden when the log is empty. | |
| Theming (light/dark) | (Django 5.1+ official) | SHIPPED | Token-driven theme + dark mode toggle + per-tenant branding (v0.26+) | Goes beyond Django's basic palette. |
| Password change for staff | [password change](https://docs.djangoproject.com/en/6.0/topics/auth/default/#changing-passwords) | SHIPPED | `/account/password` (v0.40) | |
| Two-factor admin login | n/a (Django needs `django-otp`) | PARTIAL | `totp::*` ships RFC 6238 primitives; no admin enrollment UI (#367) | |
| Custom dashboard | [admin templates override](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#overriding-admin-templates) | PARTIAL | Tera template override works (#368) | No formal widget API. |
| Admin styling / branding | [admin templates](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/#overriding-admin-templates) | SHIPPED | `Storage`-backed per-tenant brand + theme tokens | |
| Admin docs (`django.contrib.admindocs`) | [admindocs](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/admindocs/) | N/A | n/a | Rust doc-comments cover it. |

Summary: **18 SHIPPED / 7 PARTIAL / 11 MISSING / 2 N/A**. Concentration of MISSING items in admin polish: method-field display, list_display_links, raw_id_fields, autocomplete, formfield_overrides, date_hierarchy, prepopulated_fields, custom URLs.

---

## 7. Forms / Formsets

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `Form` class | [Forms](https://docs.djangoproject.com/en/6.0/topics/forms/) | SHIPPED | `#[derive(Form)]` macro emits `Form::parse` | |
| `ModelForm` | [ModelForm](https://docs.djangoproject.com/en/6.0/topics/forms/modelforms/) | PARTIAL | `forms::ModelForm` struct + `from_model_schema` runtime — works for admin path (#369) | No `#[derive(ModelForm)]` shortcut. Backlog. |
| Form fields (CharField, IntegerField, etc.) | [Form fields](https://docs.djangoproject.com/en/6.0/ref/forms/fields/) | SHIPPED | Field attrs in `Form` derive | |
| Widgets (Select, Textarea, RadioSelect, etc.) | [Widgets](https://docs.djangoproject.com/en/6.0/ref/forms/widgets/) | PARTIAL | Admin renders fixed HTML per FieldType; no widget swap (#370) | |
| Per-field validators chain | [validators](https://docs.djangoproject.com/en/6.0/ref/validators/) | SHIPPED | `#[rustango(validators = "email,url")]` (#447, v0.42) — declarative comma-separated chain dispatched in `core::validate_value` on every INSERT/UPDATE. Closes #371 alongside this PR. | |
| `clean_<field>` per-field clean | [clean methods](https://docs.djangoproject.com/en/6.0/ref/forms/validation/) | MISSING | n/a (#372) | Macro doesn't emit per-field clean hooks. |
| `clean()` cross-field validation | (above) | MISSING | n/a (#373) | Closes ORM-improvement backlog #9. |
| `FormErrors` (multi-error) | [error handling](https://docs.djangoproject.com/en/6.0/ref/forms/validation/) | SHIPPED | `forms::FormErrors` collects all field+non-field errors | |
| Formsets (`formset_factory`) | [Formsets](https://docs.djangoproject.com/en/6.0/topics/forms/formsets/) | SHIPPED | `forms::formset` module — `<prefix>-<N>-<field>` keying | |
| Model formsets / inline formsets | [modelformsets](https://docs.djangoproject.com/en/6.0/topics/forms/modelforms/#model-formsets) | PARTIAL | Admin inlines use formset POST shape (#374) | Standalone `inline_formset_factory(parent, child)` MISSING. |
| `DynamicForm` (runtime schema) | n/a (Django) | SHIPPED | `forms::DynamicForm` — Django doesn't ship this; rustango ahead | |
| File upload | [File uploads](https://docs.djangoproject.com/en/6.0/topics/http/file-uploads/) | SHIPPED | `uploads::*` + multer | |
| `save(commit=False)` + `save_m2m()` | [ModelForm.save](https://docs.djangoproject.com/en/6.0/topics/forms/modelforms/#the-save-method) | MISSING | n/a (#375) | `ModelForm::save(pool)` is one-shot. |
| CSRF token | [CSRF](https://docs.djangoproject.com/en/6.0/ref/csrf/) | SHIPPED | `forms::csrf::CsrfLayer` + `{{ csrf_token \| csrf_input \| safe }}` Tera | |
| `UniqueTogetherValidator` | [DRF form-level uniqueness](https://docs.djangoproject.com/en/6.0/ref/contrib/postgres/constraints/) | MISSING | n/a (#376) | Backlog: friendly form-error on `unique_together` violation. |

Summary: **8 SHIPPED / 3 PARTIAL / 4 MISSING / 0 N/A**.

---

## 8. Generic class-based views

| Django CBV | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `View` (base) | [base View](https://docs.djangoproject.com/en/6.0/ref/class-based-views/base/#view) | N/A | axum handlers play the role | |
| `TemplateView` | [TemplateView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/base/#templateview) | PARTIAL | `shortcuts::render(template, ctx)` (#377) | No view builder — handler renders directly. |
| `RedirectView` | [RedirectView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/base/#redirectview) | SHIPPED | `redirects::*` table-driven + `shortcuts::redirect()` | |
| `ListView` | [ListView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-display/#listview) | SHIPPED | `template_views::ListView::for_model(...)` (v0.21+, tri-dialect v0.38) | |
| `DetailView` | [DetailView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-display/#detailview) | SHIPPED | `template_views::DetailView` | |
| `FormView` | [FormView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-editing/#formview) | SHIPPED | `template_views::CreateView` plays both roles | |
| `CreateView` | (above) | SHIPPED | `template_views::CreateView` | |
| `UpdateView` | [UpdateView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-editing/#updateview) | SHIPPED | `template_views::UpdateView` | |
| `DeleteView` | [DeleteView](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-editing/#deleteview) | SHIPPED | `template_views::DeleteView` | |
| Date-based views (ArchiveIndexView, YearArchiveView, etc.) | [date views](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-date-based/) | MISSING | n/a (#378) | Niche; build via QuerySet date-trunc + Tera. |
| `LoginRequiredMixin` | [LoginRequiredMixin](https://docs.djangoproject.com/en/6.0/topics/auth/default/#django.contrib.auth.mixins.LoginRequiredMixin) | SHIPPED | `auth_decorators::login_required` middleware layer | |
| `UserPassesTestMixin` | [UserPassesTestMixin](https://docs.djangoproject.com/en/6.0/topics/auth/default/#django.contrib.auth.mixins.UserPassesTestMixin) | SHIPPED | `auth_decorators::user_passes_test` | |
| `PermissionRequiredMixin` | [PermissionRequiredMixin](https://docs.djangoproject.com/en/6.0/topics/auth/default/#django.contrib.auth.mixins.PermissionRequiredMixin) | SHIPPED | `auth_decorators::permission_required` (PR #313) | |
| MultipleObjectMixin / SingleObjectMixin (object-level customization) | [generic-display](https://docs.djangoproject.com/en/6.0/ref/class-based-views/generic-display/#multipleobjectmixin) | PARTIAL | `ListView::with_queryset(custom_qs)` exists (#379) | Mixin composability MISSING. |

Summary: **5 SHIPPED / 2 PARTIAL / 3 MISSING / 0 N/A**.

---

## 9. URL routing

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `path("/foo", view)` | [URL dispatcher](https://docs.djangoproject.com/en/6.0/topics/http/urls/) | SHIPPED | axum `Router::route` | |
| `re_path()` (regex) | same | N/A | axum uses path patterns (not regex by default) | Use axum's `:param` and `*rest`. |
| `include("app.urls")` | [include](https://docs.djangoproject.com/en/6.0/topics/http/urls/#including-other-urlconfs) | SHIPPED | `Router::nest("/app", app::router())` | |
| URL namespaces | [namespaces](https://docs.djangoproject.com/en/6.0/topics/http/urls/#url-namespaces) | MISSING | n/a (#380) | Just nest paths. |
| `reverse("name")` | [reverse](https://docs.djangoproject.com/en/6.0/ref/urlresolvers/#reverse) | SHIPPED | `urls::reverse(name, &HashMap)` (`urls.rs:163`) — named-route reverse lookup against the `register_url!`-populated registry; `all_routes()` at `urls.rs:122`. Closes #381. | |
| `{% url 'name' %}` template tag | [url tag](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#url) | SHIPPED | `urls::register_url_tag(&mut Tera)` (`urls.rs:403`) registers the Tera `url(name=...)` function so templates can call `{{ url(name="dashboard") }}`. Closes #382. | |
| Static + media URLs | [static](https://docs.djangoproject.com/en/6.0/ref/contrib/staticfiles/) | SHIPPED | `Cli::with_static(prefix, dir)` + `Media` model + `Storage` | |

Summary: **5 / 1 / 1 / 1**.

---

## 10. Templates

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| Template engine | [Templates](https://docs.djangoproject.com/en/6.0/topics/templates/) | SHIPPED | Tera (Jinja2-shape) | Django ships DTL + Jinja2; rustango picks Tera. |
| Template inheritance (`{% extends %}` / `{% block %}`) | [inheritance](https://docs.djangoproject.com/en/6.0/ref/templates/language/#template-inheritance) | SHIPPED | Tera native | |
| `{% include %}` | [include](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#include) | SHIPPED | Tera | |
| Built-in filters (`pluralize`, `truncatewords`, `linebreaks`, `default_if_none`) | [filters](https://docs.djangoproject.com/en/6.0/ref/templates/builtins/#built-in-filter-reference) | SHIPPED | Django-shape filters via `template_filters::register_all()` | |
| Custom template tags | [custom tags](https://docs.djangoproject.com/en/6.0/howto/custom-template-tags/) | PARTIAL | Tera function registration (`tera.register_function`) (#383) | Django's `{% mytag %}` block tags are deeper than Tera supports. |
| Context processors | [context_processors](https://docs.djangoproject.com/en/6.0/ref/templates/api/#built-in-template-context-processors) | PARTIAL | Per-handler context build manually; no global registry (#384) | |
| Template loaders | [loaders](https://docs.djangoproject.com/en/6.0/ref/templates/api/#template-loader-types) | SHIPPED | `Tera::new(glob)` | |
| Autoescaping | [autoescape](https://docs.djangoproject.com/en/6.0/ref/templates/language/#automatic-html-escaping) | SHIPPED | Tera default-on | |
| staticfiles app | [staticfiles](https://docs.djangoproject.com/en/6.0/ref/contrib/staticfiles/) | SHIPPED | `Cli::with_static(prefix, dir)` + `Storage` | |
| `{% csrf_token %}` template tag | [csrf](https://docs.djangoproject.com/en/6.0/ref/csrf/) | SHIPPED | `{{ csrf_token \| csrf_input \| safe }}` | |
| `{% cache %}` template fragment | [template fragment caching](https://docs.djangoproject.com/en/6.0/topics/cache/#template-fragment-caching) | PARTIAL | `cache_fragment::cached_render(key, fn)` exists; not a Tera tag (#385) | |
| Template debug page | (Django DEBUG) | MISSING | n/a (#386) | Tera errors go to stderr; no DEBUG template-error overlay yet. |

Summary: **7 / 3 / 1 / 0**.

---

## 11. Authentication

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `User` model | [User model](https://docs.djangoproject.com/en/6.0/ref/contrib/auth/#django.contrib.auth.models.User) | SHIPPED | `tenancy::auth::User` + per-tenant `rustango_users` | |
| Customize User model (`AUTH_USER_MODEL`) | [custom user](https://docs.djangoproject.com/en/6.0/topics/auth/customizing/) | SHIPPED | `TenantUserModel` trait + `Cli::user_model(...)` | |
| `Group` model | [Group](https://docs.djangoproject.com/en/6.0/ref/contrib/auth/#django.contrib.auth.models.Group) | SHIPPED | `tenancy::permissions::Role` + `RolePermission` | Called "Role" not "Group"; same semantic. |
| `Permission` model | [Permission](https://docs.djangoproject.com/en/6.0/ref/contrib/auth/#django.contrib.auth.models.Permission) | SHIPPED | `rustango_permissions` registry + per-user / per-role grants | |
| `auth.authenticate()` | [authenticate](https://docs.djangoproject.com/en/6.0/topics/auth/default/#django.contrib.auth.authenticate) | SHIPPED | `tenancy::admin::login_submit` + `password::verify` | |
| `auth.login()` / `auth.logout()` | [login](https://docs.djangoproject.com/en/6.0/topics/auth/default/#django.contrib.auth.login) | SHIPPED | Tenancy login_submit / logout handlers | |
| Authentication backends (`AUTHENTICATION_BACKENDS`) | [backends](https://docs.djangoproject.com/en/6.0/topics/auth/customizing/#specifying-authentication-backends) | SHIPPED | `auth_backends::{UsernameBackend, EmailBackend, RemoteUserBackend}` + chain | |
| Password hashing (`PASSWORD_HASHERS`) | [password management](https://docs.djangoproject.com/en/6.0/topics/auth/passwords/) | SHIPPED | `passwords::*` (Argon2id) + `password_hashers::PasswordHasher` chain for migration | |
| Auto-upgrade hash on login | (above) | SHIPPED | v0.27+ | |
| Password validators (`AUTH_PASSWORD_VALIDATORS`) | [password validators](https://docs.djangoproject.com/en/6.0/topics/auth/passwords/#module-django.contrib.auth.password_validation) | SHIPPED | `password_validators::*` (MinimumLength, NumericPassword, CommonPassword, etc.) | |
| Password reset flow | [password reset](https://docs.djangoproject.com/en/6.0/topics/auth/default/#password-management-in-django) | SHIPPED | `auth_flows::PasswordReset::issue` + `verify` (`auth_flows/mod.rs:80, 152`) — signed-token mint/redeem with mailable wired at `auth_flows/mailable.rs:24`. Closes #387. | |
| Email verification | (custom in Django) | SHIPPED | `auth_flows::EmailVerification` | |
| Magic-link login | n/a | SHIPPED | `auth_flows` magic-link | Rustango ahead. |
| Session middleware | [sessions](https://docs.djangoproject.com/en/6.0/topics/http/sessions/) | SHIPPED | `session::*` + admin + tenancy variants | |
| OAuth2 / Social auth | n/a in Django core (needs `django-allauth`) | SHIPPED | `oauth2::OAuth2Registry` + PKCE + OIDC | Rustango ahead. |
| Built-in JWT auth | n/a in Django core (needs `simplejwt`) | SHIPPED | `tenancy::auth_routes::jwt_router(Config)` one-liner (`tenancy/auth_routes.rs:149`) wraps the `jwt::*` primitives + `JwtLifecycle` + `JwtBackend` into a router with `/login`, `/refresh`, `/logout`, `/me` endpoints. Closes #388 (backlog #81). | |
| API keys | n/a in Django core | SHIPPED | `api_keys::*` (Argon2 hash) | |
| TOTP / 2FA | n/a in Django core | PARTIAL | `totp::*` primitives + QR `otpauth_url`; no enrollment UI (#389) | |
| Rate limiting / account lockout | n/a | SHIPPED | `account_lockout::Lockout` + `rate_limit::*` | |
| LoginView / LogoutView CBVs | [Auth CBVs](https://docs.djangoproject.com/en/6.0/topics/auth/default/#all-authentication-views) | SHIPPED | `admin::login_view::{public_router, protected_router, login_submit, logout_submit}` (`admin/login_view.rs:37, 88, 353`) — bundled login/logout views with session-cookie mint + redirect handling; `tenancy::auth_routes::jwt_router` provides the JWT analog. Closes #390. | |
| PasswordChangeView CBV | (above) | SHIPPED | `/account/password` (v0.40) | |
| PasswordResetConfirmView | (above) | MISSING | n/a (#391) | Email + token machinery is there; no view scaffold. |
| Passkey / WebAuthn | (Django 5.2+) | MISSING | n/a (#392) | |

Summary: **14 SHIPPED / 4 PARTIAL / 4 MISSING / 1 N/A**.

---

## 12. Sessions

| Backend / capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| Signed-cookie session | [signed cookies](https://docs.djangoproject.com/en/6.0/topics/http/sessions/#using-cookie-based-sessions) | SHIPPED | `session::SessionSecret` HMAC-SHA256 signed cookie (v0.40 admin) | |
| Cache-backed session | [cache backend](https://docs.djangoproject.com/en/6.0/topics/http/sessions/#using-cache-sessions) | SHIPPED | Server-side opaque-id sessions via `Cache` backend (v0.24+) | |
| Database-backed session | [database backend](https://docs.djangoproject.com/en/6.0/topics/http/sessions/#using-database-backed-sessions) | SHIPPED | `sessions::SessionStore` (`sessions.rs:130`) wraps the `Cache` trait; `PgCache` backend persists session blobs in a database table, matching Django's `db_session` semantics. Closes #393. | |
| File-backed session | [file backend](https://docs.djangoproject.com/en/6.0/topics/http/sessions/#using-file-based-sessions) | MISSING | n/a (#394) | Niche. |
| Cached-DB hybrid | [cached_db backend](https://docs.djangoproject.com/en/6.0/topics/http/sessions/#using-cached-database-sessions) | SHIPPED | Cache backend can wrap any storage | |
| Session expiry / `SESSION_COOKIE_AGE` | (cookie options) | SHIPPED | Configurable TTL per backend | |
| Session middleware (autoinstall) | (auto in MIDDLEWARE) | SHIPPED | `Cli::tenancy()` and `Admin::Builder` auto-attach | |

Summary: **4 / 1 / 1 / 0**.

---

## 13. Manage commands

Django built-in management commands:

| Command | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `startproject` | [startproject](https://docs.djangoproject.com/en/6.0/ref/django-admin/#startproject) | PARTIAL | `cargo rustango new <name>` — version-pin bug filed #79 | |
| `startapp` | [startapp](https://docs.djangoproject.com/en/6.0/ref/django-admin/#startapp) | SHIPPED | `manage startapp <name>` | Scaffolds model + views + tests + migrations. |
| `makemigrations` | (sec 5) | SHIPPED | `manage makemigrations` | |
| `migrate` | (sec 5) | SHIPPED | `manage migrate` | |
| `showmigrations` | (sec 5) | SHIPPED | `manage showmigrations` | |
| `sqlmigrate` | (sec 5) | SHIPPED | `manage sqlmigrate <name>` (closed #345/#395, v0.42) — see ORM row for details. | |
| `squashmigrations` | (sec 5) | SHIPPED | `manage migrate --squash` | |
| `runserver` | (Django) | N/A | rustango uses axum's runserver — `manage run-server` or `Cli::new().run().await` | |
| `runserver_plus` (django-extensions) | n/a | N/A | n/a | |
| `shell` (REPL) | [shell](https://docs.djangoproject.com/en/6.0/ref/django-admin/#shell) | MISSING | n/a (#396) | Rust doesn't have a REPL story. |
| `shell_plus` (django-extensions) | n/a | N/A | n/a | |
| `dbshell` | [dbshell](https://docs.djangoproject.com/en/6.0/ref/django-admin/#dbshell) | SHIPPED | `manage dbshell` (psql / mysql / sqlite3 via execvp) | |
| `createsuperuser` | [createsuperuser](https://docs.djangoproject.com/en/6.0/ref/django-admin/#createsuperuser) | SHIPPED | `manage create-admin` + `manage create-user --superuser` | |
| `changepassword` | [changepassword](https://docs.djangoproject.com/en/6.0/ref/django-admin/#changepassword) | SHIPPED | `manage reset-password <slug> <user> --password ...` | |
| `collectstatic` | [collectstatic](https://docs.djangoproject.com/en/6.0/ref/contrib/staticfiles/#django-admin-collectstatic) | N/A | n/a — rustango bundles static via `Cli::with_static(prefix, dir)` | |
| `findstatic` | (above) | N/A | n/a | |
| `loaddata` | [loaddata](https://docs.djangoproject.com/en/6.0/ref/django-admin/#loaddata) | SHIPPED | `fixtures::load_pool` + `manage load-fixture` | |
| `dumpdata` | [dumpdata](https://docs.djangoproject.com/en/6.0/ref/django-admin/#dumpdata) | SHIPPED | `manage dumpdata [--model app.Name] [--indent N]` (`migrate/manage.rs:115, 2248, 2318`) — JSON fixture writer for the round-trip with `loaddata` / `fixtures`. Closes #397. | |
| `makemessages` | [makemessages](https://docs.djangoproject.com/en/6.0/ref/django-admin/#makemessages) | MISSING | n/a (#398) | i18n scaffolding deferred. |
| `compilemessages` | (above) | MISSING | n/a (#399) | |
| `test` | [test](https://docs.djangoproject.com/en/6.0/ref/django-admin/#test) | N/A | `cargo test` is the canonical entry | |
| `check` | [check](https://docs.djangoproject.com/en/6.0/ref/django-admin/#check) | SHIPPED | `manage check` + `manage check --deploy` (v0.29) | |
| `check --deploy` (security audit) | (above) | SHIPPED | (above) | |
| `inspectdb` | (sec 5) | SHIPPED | `manage inspectdb` | |
| `sendtestemail` | [sendtestemail](https://docs.djangoproject.com/en/6.0/ref/django-admin/#sendtestemail) | SHIPPED | `manage sendtestemail` (config feature) | |
| `flush` | [flush](https://docs.djangoproject.com/en/6.0/ref/django-admin/#flush) | SHIPPED | `manage flush` | |
| `make:viewset` / `make:serializer` / etc. (rustango-specific generators) | n/a | SHIPPED | 8 generators (viewset, serializer, form, job, notification, middleware, test, api-routes) | Rustango ahead. |
| `create-tenant` / `create-operator` (rustango-specific) | n/a | SHIPPED | tenancy verbs | |
| `migrate-tenant-storage` (rustango-specific) | n/a | SHIPPED | v0.26 | |

Summary: **13 SHIPPED / 5 PARTIAL / 6 MISSING / 4 N/A**. Gaps: `shell` REPL (Rust constraint), `dumpdata`, i18n scaffolding (`makemessages` / `compilemessages`).

---

## 14. Settings

| Setting / capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `DATABASES` (multi-DB) | [databases](https://docs.djangoproject.com/en/6.0/ref/settings/#databases) | PARTIAL | One Pool per app or per tenant; per-app multi-DB routing MISSING (#400) | No `default`/`replica` routing. |
| `DATABASE_ROUTERS` | [database routers](https://docs.djangoproject.com/en/6.0/topics/db/multi-db/#database-routers) | MISSING | n/a (#401) | Read replicas not surfaced. |
| `DEFAULT_AUTO_FIELD` | [DEFAULT_AUTO_FIELD](https://docs.djangoproject.com/en/6.0/ref/settings/#default-auto-field) | SHIPPED | `Auto<i64>` is canonical — `BigAutoField` equivalent | |
| `INSTALLED_APPS` | [installed apps](https://docs.djangoproject.com/en/6.0/ref/settings/#installed-apps) | N/A | n/a — inventory-based registration | |
| `MIDDLEWARE` (ordered list) | [middleware](https://docs.djangoproject.com/en/6.0/ref/settings/#middleware) | N/A | axum per-router `.layer(...)` idiom | |
| `TEMPLATES` (engines list) | [templates](https://docs.djangoproject.com/en/6.0/ref/settings/#templates) | SHIPPED | One Tera engine per app | |
| `CACHES` (backends) | [caches](https://docs.djangoproject.com/en/6.0/ref/settings/#caches) | SHIPPED | `Settings.cache` section (v0.29) | |
| `SESSION_ENGINE` | [session_engine](https://docs.djangoproject.com/en/6.0/ref/settings/#session-engine) | SHIPPED | Pluggable session storage via `Cache` | |
| `EMAIL_BACKEND` | [email_backend](https://docs.djangoproject.com/en/6.0/ref/settings/#email-backend) | SHIPPED | `Mailer` trait + backends (SMTP, console, locmem, dummy) | |
| `STATIC_URL` / `MEDIA_URL` | [static_url](https://docs.djangoproject.com/en/6.0/ref/settings/#static-url) | SHIPPED | RouteConfig + Cli::with_static() | |
| `SECURE_*` (HSTS, XSS, content-type sniff, etc.) | [security settings](https://docs.djangoproject.com/en/6.0/ref/settings/#security) | SHIPPED | `security_headers::*` + Settings.security section | |
| `TIME_ZONE` / `USE_TZ` | [time_zone](https://docs.djangoproject.com/en/6.0/ref/settings/#std-setting-TIME_ZONE) | PARTIAL | `Settings.locale.tz` exists; per-user TZ activation via `i18n::timezone::with_tz` (#402) | App-wide TZ override not fully wired into ORM date formatters. |
| `LANGUAGE_CODE` / `LANGUAGES` / `LOCALE_PATHS` | [language](https://docs.djangoproject.com/en/6.0/ref/settings/#language-code) | PARTIAL | `i18n::Translator` per-locale JSON; no gettext .po pipeline (#403) | Backlog. |
| `LOGGING` (dictConfig) | [logging](https://docs.djangoproject.com/en/6.0/ref/settings/#logging) | PARTIAL | `tracing-subscriber` env-filter; not Django's dictConfig shape (#404) | |
| `DATA_UPLOAD_MAX_MEMORY_SIZE` | [data_upload](https://docs.djangoproject.com/en/6.0/ref/settings/#data-upload-max-memory-size) | SHIPPED | `body_limit::*` middleware | |
| Tiered config (dev/staging/prod files) | n/a (Django uses env-var + manual import) | SHIPPED | `dev_settings.toml` / `staging_settings.toml` / `prod_settings.toml` (v0.29 closed #87) | Rustango ahead. |
| Detected features at runtime | n/a | SHIPPED | `Settings::detected_features` | |
| Settings validation (`manage check --deploy`) | [check --deploy](https://docs.djangoproject.com/en/6.0/ref/django-admin/#cmdoption-check-deploy) | SHIPPED | (sec 13) | |
| Secrets manager integration (Vault, AWS Secrets, etc.) | n/a in Django core | MISSING | n/a — `secrets::*` is local-env focus (#405) | Backlog #47. |

Summary: **10 / 4 / 4 / 2**. Multi-DB routing is the headline MISSING.

---

## 15. Security / middleware

| Middleware | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `SecurityMiddleware` (HSTS, XSS, content-type-nosniff) | [SecurityMiddleware](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.security.SecurityMiddleware) | SHIPPED | `security_headers::*` (HSTS, X-Frame, Referrer, COOP, Permissions-Policy) (v0.20+) | |
| `CsrfViewMiddleware` | [CSRF](https://docs.djangoproject.com/en/6.0/ref/csrf/) | SHIPPED | `forms::csrf::CsrfLayer` | Double-submit-cookie. |
| `XFrameOptionsMiddleware` | [clickjacking](https://docs.djangoproject.com/en/6.0/ref/clickjacking/) | SHIPPED | Part of security_headers | |
| `SessionMiddleware` | [sessions](https://docs.djangoproject.com/en/6.0/topics/http/sessions/) | SHIPPED | (sec 12) | |
| `AuthenticationMiddleware` | [auth middleware](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.contrib.auth.middleware.AuthenticationMiddleware) | SHIPPED | `SessionUser` extractor | |
| `MessageMiddleware` | [messages](https://docs.djangoproject.com/en/6.0/ref/contrib/messages/) | SHIPPED | `messages::*` flash-message store | |
| `CommonMiddleware` (trailing slash) | [CommonMiddleware](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.common.CommonMiddleware) | SHIPPED | `trailing_slash::*` | |
| `LocaleMiddleware` | [LocaleMiddleware](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.locale.LocaleMiddleware) | PARTIAL | `i18n::Translator` Accept-Language negotiation + cookie (#406) | Per-URL locale prefix MISSING. |
| `GZipMiddleware` | [GZip](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.gzip.GZipMiddleware) | SHIPPED | `compression::*` (gzip + deflate) | |
| `ConditionalGetMiddleware` (ETag / Last-Modified) | [ConditionalGet](https://docs.djangoproject.com/en/6.0/ref/middleware/#django.middleware.http.ConditionalGetMiddleware) | SHIPPED | `etag::*` body-hash + 304 | |
| Real-IP extraction | n/a in Django core | SHIPPED | `real_ip::*` (X-Forwarded-For / X-Real-IP / RFC 7239) | |
| Request ID middleware | n/a | SHIPPED | `request_id::*` | |
| CORS | n/a (Django needs `django-cors-headers`) | SHIPPED | `cors::*` allowlist + presets | |
| Rate limiting | n/a (Django needs `django-ratelimit`) | SHIPPED | `rate_limit::*` + `rate_limit_cache::*` | |
| Access logging with PII redaction | n/a | SHIPPED | `access_log::*` (v0.22+) | |
| Request timeout | n/a | SHIPPED | `request_timeout::*` | |
| Idempotency keys | n/a | SHIPPED | `idempotency::*` (Stripe-shape) | |
| Maintenance mode | n/a | SHIPPED | `maintenance::*` | |
| Method override (`X-HTTP-Method-Override`) | n/a | SHIPPED | `method_override::*` | |
| OpenTelemetry / traceparent | n/a | SHIPPED | `tracing_layer::*` | |
| Body size limit | [DATA_UPLOAD_MAX_MEMORY_SIZE](https://docs.djangoproject.com/en/6.0/ref/settings/#data-upload-max-memory-size) | SHIPPED | `body_limit::*` | |
| IP allowlist / blocklist | n/a | SHIPPED | `ip_filter::*` | |
| Server-Timing header | n/a | SHIPPED | `server_timing::*` | |
| CSP nonce | [CSP](https://docs.djangoproject.com/en/6.0/topics/security/#content-security-policy) | SHIPPED | `csp_nonce::*` middleware | |
| `SECURE_PROXY_SSL_HEADER` | [proxy headers](https://docs.djangoproject.com/en/6.0/ref/settings/#secure-proxy-ssl-header) | SHIPPED | Real-IP layer handles it | |
| API versioning (header / query / prefix) | n/a | SHIPPED | `api_version::*` | |

Summary: **14 SHIPPED / 1 PARTIAL / 1 MISSING / 0 N/A** for the dimensions enumerated. Rustango is notably ahead on per-request observability + CSRF / rate limiting / CSP / OTel out-of-the-box.

---

## 16. Caching

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| Cache trait + pluggable backends | [cache framework](https://docs.djangoproject.com/en/6.0/topics/cache/) | SHIPPED | `cache::Cache` trait | |
| Memcached backend | [memcached](https://docs.djangoproject.com/en/6.0/topics/cache/#memcached) | MISSING | n/a (#407) | Use Redis or in-memory. |
| Redis backend | [redis](https://docs.djangoproject.com/en/6.0/topics/cache/#redis) | SHIPPED | `cache-redis` feature (v0.18) | |
| File-system cache | [filesystem](https://docs.djangoproject.com/en/6.0/topics/cache/#file-system-caching) | MISSING | n/a (#408) | |
| In-memory cache | [local-memory](https://docs.djangoproject.com/en/6.0/topics/cache/#local-memory-caching) | SHIPPED | `InMemoryCache` | |
| Database cache | [db](https://docs.djangoproject.com/en/6.0/topics/cache/#database-caching) | PARTIAL | DB tables can back the trait; no first-class `DbCache` (#409) | |
| `@cache_page` view decorator | [cache_page](https://docs.djangoproject.com/en/6.0/topics/cache/#the-per-view-cache) | SHIPPED | `cache_page::cache_page_layer` (v0.19) | |
| `@cache_control` headers | [cache_control](https://docs.djangoproject.com/en/6.0/topics/cache/#using-vary-headers) | SHIPPED | Cache-Control header helpers | |
| `@vary_on_headers` / `@vary_on_cookie` | [vary](https://docs.djangoproject.com/en/6.0/topics/cache/#using-vary-headers) | SHIPPED | Vary header helpers | |
| Template fragment caching | [template fragments](https://docs.djangoproject.com/en/6.0/topics/cache/#template-fragment-caching) | SHIPPED | `cache_fragment::cached_render` | |

Summary: **7 / 1 / 2 / 0**.

---

## 17. Signals

| Signal | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `pre_save` / `post_save` | [signals](https://docs.djangoproject.com/en/6.0/topics/signals/) | SHIPPED | `signals::connect_pre_save` / `_post_save` | |
| `pre_delete` / `post_delete` | (above) | SHIPPED | (above) | |
| `m2m_changed` | (above) | SHIPPED | `signals::m2m::connect_m2m_changed` (#410, v0.42) — fires from `M2MManager::{add_pool, remove_pool, set_pool, clear_pool}`. `M2mAction` enum: Add / Remove / Set / Clear; `dst_pks` carries affected ids per action. | |
| `pre_migrate` / `post_migrate` | [pre_migrate](https://docs.djangoproject.com/en/6.0/ref/signals/#pre-migrate) | SHIPPED | `signals::migrate::{connect_pre_migrate, connect_post_migrate}` (#411, v0.42) — wired into `migrate::{apply_all, apply_all_pool, migrate_with_ledger}`. `PostMigrateContext.applied: Vec<String>` lists newly-applied migration names. | |
| `class_prepared` | (above) | N/A — Rust doesn't have lazy class creation | n/a | |
| `request_started` / `request_finished` | [request signals](https://docs.djangoproject.com/en/6.0/ref/signals/#django.core.signals.request_started) | SHIPPED | `signals::request::{connect_request_started, connect_request_finished, send_*}` (`signals/request.rs:187, 216`) — discrete signals auto-fired by `RequestSignalsLayer` (lines 352 + 384) before / after every wrapped request. Closes #412. | |
| `got_request_exception` | (above) | SHIPPED | `signals::request::got_request_exception` fires from `RequestSignalsLayer` on every 5xx response + the (rare) `Service::Error` arm (#413, v0.42). `RequestExceptionContext.status: Option<u16>` lets receivers distinguish the two cases. | |
| `user_logged_in` / `user_logged_out` / `user_login_failed` | [auth signals](https://docs.djangoproject.com/en/6.0/ref/contrib/auth/#module-django.contrib.auth.signals) | SHIPPED | `signals::auth::*` (#414, v0.42) — fired from admin / tenant admin / operator console / JWT login + logout paths with `AuthRequestMeta` context | |
| `setting_changed` | (above) | SHIPPED | `signals::setting::connect_setting_changed` (#415, v0.42) — fires from `test_settings::with_overridden` on scope enter (`enter: true`) and exit (`enter: false`). Receivers typically flush config-derived caches. | |
| Disconnect / weak references | (above) | SHIPPED | `signals::disconnect_*` | |
| Async receivers | (Django 4.1+ async receivers) | SHIPPED | All rustango signal receivers are `async fn` | |

Summary: **7 / 2 / 5 / 0**. Gaps cluster around `m2m_changed`, migrate signals, auth signals.

---

## 18. Email

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `EmailMessage` builder | [email message](https://docs.djangoproject.com/en/6.0/topics/email/#emailmessage-and-emailmultialternatives) | SHIPPED | `email::Email` + `Mailable` pattern | |
| `EmailMultiAlternatives` (HTML+text) | (above) | SHIPPED | `email::Email::with_html(...)` | |
| `send_mail()` shortcut | [send_mail](https://docs.djangoproject.com/en/6.0/topics/email/#send-mail) | SHIPPED | `email::send_pool(mailer, msg)` | |
| `mail_admins` / `mail_managers` | [mail_admins](https://docs.djangoproject.com/en/6.0/topics/email/#mail-admins) | PARTIAL | Manual recipient list (#416) | No `ADMINS` setting hook. |
| Console backend | [console backend](https://docs.djangoproject.com/en/6.0/topics/email/#console-backend) | SHIPPED | `email::ConsoleMailer` | |
| In-memory / dummy backend | [locmem](https://docs.djangoproject.com/en/6.0/topics/email/#in-memory-backend) | SHIPPED | `email::InMemoryMailer` + `NullMailer` | |
| SMTP backend | [smtp](https://docs.djangoproject.com/en/6.0/topics/email/#smtp-backend) | SHIPPED | `SmtpMailer` via lettre + rustls (`email-smtp` feature) | |
| Email templates (Tera) | n/a in Django core | SHIPPED | `email_templates::EmailRenderer` | |
| File backend | [file backend](https://docs.djangoproject.com/en/6.0/topics/email/#file-backend) | MISSING | n/a (#417) | Niche dev tool. |
| Email job queue integration | n/a | SHIPPED | `email_jobs::dispatch_email` | |
| Multi-channel notifications (Mail + Slack + DB) | n/a (Django needs `django-notifications`) | PARTIAL | `notifications::*` skeleton (v0.21); incomplete provider set (#418) | |

Summary: **6 / 2 / 1 / 0**.

---

## 19. Files / Storage

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `FileField` (on model) | [FileField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#filefield) | MISSING | n/a — manage via `Media` table FK (#419) | (see sec 3) |
| `ImageField` | [ImageField](https://docs.djangoproject.com/en/6.0/ref/models/fields/#imagefield) | MISSING | n/a (#420) | (sec 3) |
| `Storage` trait | [Storage](https://docs.djangoproject.com/en/6.0/ref/files/storage/) | SHIPPED | `storage::Storage` trait | |
| `FileSystemStorage` | [FileSystemStorage](https://docs.djangoproject.com/en/6.0/ref/files/storage/#filesystemstorage) | SHIPPED | `LocalStorage` | |
| `S3Storage` / GCS / Azure (3rd-party) | n/a in Django core | SHIPPED | `S3Storage` (S3 / R2 / B2 / MinIO) hand-rolled SigV4 | |
| In-memory storage (tests) | n/a | SHIPPED | `InMemoryStorage` | |
| File upload handlers (chunked, memory, etc.) | [upload handlers](https://docs.djangoproject.com/en/6.0/topics/http/file-uploads/#upload-handlers) | PARTIAL | multer-driven via `uploads::*` (#421) | No pluggable upload handler abstraction. |
| File validators (type, size, extension) | n/a (Django has separate validators) | SHIPPED | `validators::{file_type, file_size_max, file_extension}` (v0.25) | |
| `Media` model | n/a (each Django project rolls its own) | SHIPPED | `media::Media` model (v0.24) | Rustango ahead. |

Summary: **3 / 2 / 3 / 0**.

---

## 20. i18n / l10n

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `gettext` / `pgettext` / `ngettext` translation API | [translation](https://docs.djangoproject.com/en/6.0/topics/i18n/translation/) | PARTIAL | `i18n::Translator::t(key)` lookup against JSON per-locale (#422) | Not gettext shape; ICU MessageFormat partial. |
| `.po` / `.mo` compilation (`makemessages`, `compilemessages`) | (sec 13) | MISSING | n/a (#423) | |
| Per-language URL prefix (`/en/foo` / `/es/foo`) | [URL i18n](https://docs.djangoproject.com/en/6.0/topics/i18n/translation/#internationalization-in-url-patterns) | MISSING | n/a — rustango-cms ships a `LocaleMode::PathOrQuery` for itself, framework doesn't (#424) | |
| Fallback locales | [fallbacks](https://docs.djangoproject.com/en/6.0/topics/i18n/translation/#how-django-discovers-translations) | PARTIAL | `Translator` falls back to default locale (#425) | |
| Per-user TIME_ZONE activation | [time zones](https://docs.djangoproject.com/en/6.0/topics/i18n/timezones/) | SHIPPED | `i18n::timezone::with_tz(fixed_offset, future)` task-local + `tz_offset` cookie middleware | rustango-cms uses this end-to-end. |
| Locale-aware number / date formatting | [formatting](https://docs.djangoproject.com/en/6.0/topics/i18n/formatting/) | MISSING | n/a (#426) | |
| `{% trans %}` / `{% blocktrans %}` template tags | [template translation](https://docs.djangoproject.com/en/6.0/topics/i18n/translation/#internationalization-in-template-code) | MISSING | n/a (#427) | Tera filter exists in rustango-cms; framework-side missing. |
| Currency / region formatting | [LANGUAGES setting](https://docs.djangoproject.com/en/6.0/ref/settings/#languages) | MISSING | n/a (#428) | |
| Right-to-left layout support | [BiDi text](https://docs.djangoproject.com/en/6.0/topics/i18n/translation/#translator-comments-in-templates) | MISSING | n/a (#429) | |

Summary: **1 SHIPPED / 2 PARTIAL / 6 MISSING / 0 N/A**. **One of the weakest sections.** Backlog #87 sub-bullets track this; deliberate deferral pending demand signal.

---

## 21. Testing

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `TestCase` (transactional) | [TestCase](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#testcase) | SHIPPED | `test_db::with_rollback()` | |
| `TransactionTestCase` | [TransactionTestCase](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#transactiontestcase) | SHIPPED | Plain `#[tokio::test]` against live pool | |
| `LiveServerTestCase` | [LiveServerTestCase](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#liveservertestcase) | SHIPPED | `test_server::*` random-port bind | |
| `Client` (HTTP test client) | [Client](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#the-test-client) | SHIPPED | `test_client::*` in-process axum::Router | |
| `RequestFactory` | [RequestFactory](https://docs.djangoproject.com/en/6.0/topics/testing/advanced/#django.test.RequestFactory) | SHIPPED | `test_client::RequestFactory` (#430, v0.42) — `.get/post/put/patch/delete/head/options(path).header().json().form().body().extension().build()`. Builds `axum::http::Request<Body>` without router dispatch — Django-shape direct-call testing. | |
| `override_settings` | [override_settings](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#django.test.override_settings) | SHIPPED | `test_settings::with_settings(...).await` task-local | |
| `setUpTestData` (class-level fixtures) | [setUpTestData](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#django.test.TestCase.setUpTestData) | SHIPPED | `setup_test_data!(...)` macro | |
| Test tags (`@tag('slow')`) | [tags](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#tagging-tests) | SHIPPED | `test_filter::*` (`#[rustango_test(tag = "slow")]`) | |
| Fixtures (JSON / YAML) | [fixtures](https://docs.djangoproject.com/en/6.0/howto/initial-data/) | SHIPPED | `fixtures::load_pool` | |
| Test assertions (`assertContains`, `assertRedirects`, `assertStatus`, etc.) | [assertions](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#assertions) | SHIPPED | `test_assertions::*` | |
| `assertNumQueries(N)` | [assertNumQueries](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#django.test.TransactionTestCase.assertNumQueries) | SHIPPED | `test_assertions::assert_num_queries(N, async {...}).await` + `QueryCounter::{scope, current, take}` (#431, v0.42). Per-task counter via `tokio::task_local!`; instrumented at every `*_pool` chokepoint in `sql::executor`. Zero overhead outside an active scope. | |
| Email outbox assertions | [mail outbox](https://docs.djangoproject.com/en/6.0/topics/testing/tools/#django.core.mail.outbox) | SHIPPED | `InMemoryMailer.messages()` | |
| Factories (factory-boy shape) | n/a (3rd-party) | PARTIAL | `test_data::*` shipped basics; less ergonomic (#432) | |
| `selenium` / `playwright` integration | (Django uses LiveServerTestCase + selenium) | MISSING | n/a (#433) | Playwright MCP available externally. |

Summary: **7 / 3 / 3 / 0**.

---

## 22. Async support

| Capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| Async views | [async](https://docs.djangoproject.com/en/6.0/topics/async/) | N/A | All rustango handlers are `async fn` | |
| Async ORM (`.aget`, `.aupdate`, etc.) | [async ORM](https://docs.djangoproject.com/en/6.0/topics/async/#queryset-and-manager) | SHIPPED | All ORM methods are `async fn` natively | |
| Async middleware | (above) | SHIPPED | tower `Layer` is async | |
| Async signal receivers | [async signals](https://docs.djangoproject.com/en/6.0/topics/signals/#defining-and-sending-signals) | SHIPPED | All receivers are `async fn` | |
| `asgi.py` (ASGI entry) | [asgi](https://docs.djangoproject.com/en/6.0/howto/deployment/asgi/) | N/A | Rustango binds via `axum::serve` directly | |
| Async send_mail | (Django 5.x+) | SHIPPED | `email::send_pool(mailer, msg).await` | |
| Sync-to-async / async-to-sync bridging | [sync_to_async](https://docs.djangoproject.com/en/6.0/topics/async/#sync-to-async-async-to-sync) | N/A | Pure async — no bridging needed | |

Summary: **5 / 0 / 0 / 2**. Async is native; no parity gap.

---

## 23. DRF (Django REST Framework) parity

| DRF capability | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `Serializer` class | [serializers](https://www.django-rest-framework.org/api-guide/serializers/) | SHIPPED | `#[derive(Serializer)]` macro (v0.18) | |
| `ModelSerializer` | [ModelSerializer](https://www.django-rest-framework.org/api-guide/serializers/#modelserializer) | SHIPPED | `ModelSerializer` trait via macro | |
| `HyperlinkedModelSerializer` | [HyperlinkedModelSerializer](https://www.django-rest-framework.org/api-guide/serializers/#hyperlinkedmodelserializer) | MISSING | n/a (#434) | Niche. |
| `ListSerializer` (bulk shape) | [ListSerializer](https://www.django-rest-framework.org/api-guide/serializers/#listserializer) | PARTIAL | Vec serialization works; bulk-create from `ListSerializer` shape MISSING (#435) | |
| Field-level options (read_only, write_only, source, skip) | (above) | SHIPPED | All four supported | |
| Nested serializer (one-to-many auto-expand) | [nested](https://www.django-rest-framework.org/api-guide/serializers/#dealing-with-nested-objects) | SHIPPED | `#[serializer(nested = ChildSerializer)]` (v0.18) | |
| `SerializerMethodField` | [SerializerMethodField](https://www.django-rest-framework.org/api-guide/fields/#serializermethodfield) | SHIPPED | v0.18 | |
| Per-field validators chain | (above) | SHIPPED | v0.18 | |
| Cross-field validation (`validate()`) | [object-level validation](https://www.django-rest-framework.org/api-guide/serializers/#object-level-validation) | SHIPPED | `#[serializer(validate = "fn_name")]` container attr (closed #436, v0.42) — emitted `validate()` runs per-field validators first, then `self.<fn_name>()` returning `Result<(), FormErrors>`, merging both via new `FormErrors::merge`. | |
| `UniqueTogetherValidator` | [UniqueTogetherValidator](https://www.django-rest-framework.org/api-guide/validators/#uniquetogethervalidator) | MISSING | n/a (#437) | Friendly form errors on `unique_together`. |
| `ViewSet` | [ViewSet](https://www.django-rest-framework.org/api-guide/viewsets/) | SHIPPED | `#[derive(ViewSet)]` (v0.16+, tri-dialect v0.38) | |
| `ModelViewSet` | [ModelViewSet](https://www.django-rest-framework.org/api-guide/viewsets/#modelviewset) | SHIPPED | (above, includes list/retrieve/create/update/delete) | |
| Routers (`DefaultRouter`, `SimpleRouter`) | [routers](https://www.django-rest-framework.org/api-guide/routers/) | SHIPPED | `ViewSet::router(prefix, &Pool)` + `tenant_router(prefix)` | |
| `DjangoFilterBackend` | [filtering](https://www.django-rest-framework.org/api-guide/filtering/) | SHIPPED | `?field=value` querystring filtering via `Q!` (v0.41) | |
| `SearchFilter` | (above) | SHIPPED | `ViewSet::search_fields(&["title", "body"])` + `?search=q` (closed #438, v0.42) — emits per-column ILIKE OR'd via dialect `write_ilike`; tri-dialect (PG/MySQL/SQLite). Same PR fixed a multi-column placeholder bug that affected MySQL+SQLite. | |
| `OrderingFilter` | (above) | SHIPPED | `ViewSet::ordering(&[(...,desc)])` default + `ordering_fields(&[...])` whitelist + `?ordering=field,-other` query parse (#439, v0.42). Whitelist silently drops off-list names so clients can't sort by sensitive columns. | |
| Pagination — PageNumber / LimitOffset | [pagination](https://www.django-rest-framework.org/api-guide/pagination/) | SHIPPED | `pagination::*` + RFC 5988 Link headers | |
| Pagination — Cursor | [cursor pagination](https://www.django-rest-framework.org/api-guide/pagination/#cursorpagination) | SHIPPED | `ViewSet::cursor_pagination("id")` / `cursor_pagination_desc("id")` + `pagination::CursorPaginator` (closed #440 — audit was wrong; primitive already shipped, just lacked an HTTP end-to-end test) | |
| Throttling (Anon / User / Scoped) | [throttling](https://www.django-rest-framework.org/api-guide/throttling/) | SHIPPED | `rate_limit::*` (per-IP / per-user) | |
| Permission classes (IsAuthenticated, AllowAny, IsAdminUser, etc.) | [permissions](https://www.django-rest-framework.org/api-guide/permissions/) | SHIPPED | `auth_decorators::*` + `viewset::permissions` | |
| Authentication classes (TokenAuth, BasicAuth, SessionAuth) | [authentication](https://www.django-rest-framework.org/api-guide/authentication/) | SHIPPED | `hmac_auth`, `api_keys`, `jwt`, session auth | |
| Browsable API HTML | [browsable](https://www.django-rest-framework.org/topics/browsable-api/) | MISSING | n/a (#441) | Use Swagger UI mount instead. |
| OpenAPI schema (drf-spectacular) | [schema](https://www.django-rest-framework.org/api-guide/schemas/) | SHIPPED | `openapi::*` (v0.24) + Swagger UI mount | |

Summary: **11 SHIPPED / 4 PARTIAL / 4 MISSING / 0 N/A**.

---

## 24. contrib modules

| `django.contrib.*` | Doc | Status | rustango | Notes |
|---|---|---|---|---|
| `contenttypes` | [contenttypes](https://docs.djangoproject.com/en/6.0/ref/contrib/contenttypes/) | SHIPPED | `contenttypes::*` + GenericForeignKey + admin inlines | |
| `auth` | (sec 11) | SHIPPED | tenancy::auth + permissions | |
| `sessions` | (sec 12) | SHIPPED | session backends | |
| `admin` | (sec 6) | SHIPPED | admin::Builder | |
| `staticfiles` | [staticfiles](https://docs.djangoproject.com/en/6.0/ref/contrib/staticfiles/) | SHIPPED | `Cli::with_static` | |
| `messages` | [messages](https://docs.djangoproject.com/en/6.0/ref/contrib/messages/) | SHIPPED | `messages::*` cookie-backed flash | |
| `humanize` (template filters: naturaltime, intcomma) | [humanize](https://docs.djangoproject.com/en/6.0/ref/contrib/humanize/) | SHIPPED | `humanize::*` | |
| `sites` | [sites](https://docs.djangoproject.com/en/6.0/ref/contrib/sites/) | N/A | Multi-tenancy supersedes Site model | |
| `sitemaps` | [sitemaps](https://docs.djangoproject.com/en/6.0/ref/contrib/sitemaps/) | SHIPPED | `sitemaps::*` (v0.23) | |
| `syndication` (RSS / Atom feeds) | [syndication](https://docs.djangoproject.com/en/6.0/ref/contrib/syndication/) | SHIPPED | `syndication::*` (v0.23) | |
| `flatpages` | [flatpages](https://docs.djangoproject.com/en/6.0/ref/contrib/flatpages/) | SHIPPED | `flatpages::*` (v0.22) | |
| `redirects` | [redirects](https://docs.djangoproject.com/en/6.0/ref/contrib/redirects/) | SHIPPED | `redirects::*` (v0.22) | |
| `admindocs` | [admindocs](https://docs.djangoproject.com/en/6.0/ref/contrib/admin/admindocs/) | N/A | Rust doc-comments + cargo doc | |
| `postgres` (ArrayField, JSONField, RangeField, HStoreField, CITextField) | (sec 4) | PARTIAL | JSONField shipped; Array/Range partial; HStore/CIText MISSING (#442) | |
| `gis` (GeoDjango) | [gis](https://docs.djangoproject.com/en/6.0/ref/contrib/gis/) | MISSING | [#58](https://github.com/ujeenet/rustango/issues/58) | |
| `gis.geos` (geometry types) | (above) | MISSING | (above) (#443) | |
| `gis.gdal` (raster) | (above) | MISSING | (above) (#444) | |
| Generic relations admin (GenericTabularInline) | [generic-inline](https://docs.djangoproject.com/en/6.0/ref/contrib/contenttypes/#generic-relations-and-aggregation) | SHIPPED | `register_admin_inline_generic!` | |

Summary: **6 SHIPPED / 4 PARTIAL / 4 MISSING / 0 N/A** (within Django contrib scope).

---

## Top 10 gaps by user-facing impact

Ranked by how often a real-world Django project hits the gap. Each gets a one-liner reasoning.

1. **Multi-DB routing / read replicas** (sec 14 — `DATABASES`, `DATABASE_ROUTERS`). Every non-trivial production app eventually wants read replicas. Currently MISSING; ORM is single-pool-per-QuerySet. Closest existing primitive: `Cli::api(...).migrations_dir(...).run()` is single-registry-pool.

2. **`FileField` / `ImageField` on arbitrary models** (sec 3 + 19). Rustango has `Media` model + `Storage` trait, but no `pub avatar: FileField` shape on user-defined models. Major DX gap vs Django's "drop a field, get upload + URL + ORM round-trip for free."

3. **`choices=[...]` field option + ChoiceField in forms** (sec 3). Single most-used Django field option after `max_length`. Currently MISSING — users define their own enum + manual coerce.

4. **Method-field display in admin (`list_display = [..., "method_name"]`)** (sec 6). Backlog #51. Common pattern for "show a derived value in the list view." Currently MISSING.

5. **Per-tenant + per-app i18n / l10n** (sec 20). Translations (`gettext`), locale-aware formatting, per-language URL prefixes — only 1 SHIPPED out of 9 in this category. Most weakly-covered area.

6. **`raw_id_fields` / `autocomplete_fields` in admin** (sec 6). FK widgets currently always `<select>` — chokes on tables with >1000 rows. Backlog item, real production blocker.

7. **`select_related("a__b__c")` decoder-side auto-stitching** (sec 2). T2.2 shipped JOIN emission; `post.author.profile.get_pool(...)` auto-stitch is deferred. Multi-hop reads still pay N+1 unless the user explicitly walks the JSON map.

8. **Auth signals (`user_logged_in`, `user_logged_out`, `user_login_failed`)** (sec 17). Every audit-log story needs these. Currently MISSING — manual hook via login_submit override.

9. **Cursor pagination** (sec 23). For high-volume APIs, page/offset gets slow at large offsets. Backlog. Trivial to add atop existing pagination primitive.

10. **`abstract = True` base classes** (sec 1). Pattern Django uses for `created_at` / `updated_at` mixins. Currently MISSING — Rust trait composition is close but not the same UX.

---

## Honorable mentions (high-impact for niche use cases)

- **`select_for_update(of=...)` PG-specific clauses** — currently always whole-row lock.
- **`Meta.verbose_name` / `verbose_name_plural`** — admin headers, i18n footing.
- **`raw_id_fields`-equivalent on FK admin widgets** — see #6.
- **`history_view` time-travel (rebuild row from audit log)** — admin history panel is read-only listing today.
- **Per-request perm cache for `permission_required` middleware** — one `has_perm_pool` round-trip per request; high-RPS admin would benefit from session-level caching.
- **Maintenance mode UI** (backlog `A6`) — shipped middleware, no template scaffold.
- **`makemigrations --merge`** for divergent branch chains.
- **Async signal `m2m_changed`** — currently dispatchers exist for pre/post-save only.

---

## Source pointers used for this audit

- Capability inventory + known-gaps research from the two agent reports earlier in the planning conversation
- `crates/rustango/Cargo.toml` (49 features)
- `crates/rustango/src/lib.rs` (top-level module gating)
- `crates/rustango/src/core/field_type.rs` (`FieldType` enum — 14 variants)
- `crates/rustango-macros/src/lib.rs` (`#[rustango(...)]` attr surface)
- `crates/rustango/src/migrate/manage.rs` (manage verbs)
- `crates/rustango/src/admin/` (admin Builder + helpers + render)
- `crates/rustango/src/template_views.rs` (CBV-equivalents)
- `crates/rustango/src/i18n/timezone.rs` (i18n surface)
- `crates/rustango/src/sql/executor.rs` (QuerySet executor)
- `crates/rustango/src/tenancy/permissions.rs` (auth + permissions)
- `~/.claude/projects/.../memory/future-backlog.md` (89-item backlog)
- `~/.claude/projects/.../memory/framework-comparison-2026-05-02.md` (prior 5-dim audit)
- `gh issue list --state open` (18 open issues, last checked 2026-05-21)
- Django 6.0 docs (https://docs.djangoproject.com/en/6.0/) — all linked inline

This audit is dated **2026-05-21** and represents rustango at the SHA on `main` right after PR #316 merged. Re-run on a quarterly cadence (or when ≥10 SHIPPED items accumulate) to keep the inventory honest.
