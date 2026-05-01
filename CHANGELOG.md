# Changelog

All notable changes to rustango. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project loosely follows [SemVer](https://semver.org/) — with the caveat that nothing pre-1.0 has a stability guarantee.

## [v0.12.8] — per-row retention (`cleanup_keep_last_n`) — 2026-05-01

### Added

- **`audit::cleanup_keep_last_n(pool, keep) -> u64`** — alternative retention shape: keeps the `keep` most recent entries per `(entity_table, entity_pk)` pair, deleting the rest. Useful when "the last N revisions of every row" is the right policy regardless of wall-clock age. Implementation: single window-function DELETE with `ROW_NUMBER() OVER (PARTITION BY entity_table, entity_pk ORDER BY occurred_at DESC, id DESC)`. One round-trip regardless of how many distinct rows the table holds. `keep = 0` clears everything; negative values clamp to 0.
- **Cleanup form on `/__audit` now has a mode picker** — radio between `older than N days` (the v0.12.6 default) and `keep last N per row` (new). Self-audit entry records the mode chosen + the corresponding numeric input.

### Tests

- 2 new live tests in `audit_live`: keep_last keeps N per row across multiple `entity_pk`s, keep_last(0) clears everything. 21/21 pass.

### Curl-verified

5 audit rows on `course#1` + 3 on `course#2` + 1 each on `course#3` + `course#4` → POST `/__audit/cleanup` with `mode=keep_last&keep=2` → row 1 trimmed to 2, row 2 trimmed to 2, rows 3+4 untouched (already ≤ keep). Self-audit row records `{ "keep": 2, "mode": "keep_last", "removed": 4 }`.

## [v0.12.7] — per-row "View full history" link — 2026-05-01

### Added

- **"View full history" link** on every audited row's detail page, appended to the "Audit trail" heading. Points at `/__audit?entity_table=<table>&entity_pk=<pk>` so the activity feed pre-filters to that single row's lifecycle. Lets operators jump from the detail-page snippet (3 most recent) to the full paginated history with one click.
- **`/__audit` accepts `entity_pk` as a filter param** alongside `entity_table` / `operation` / `source`. `entity_pk` is intentionally NOT a facet (per-PK distinct-value cardinality is unbounded); appears only as an active-filter pill when set in the URL.

### Curl-verified

7 audit rows in pg-sju (4 system creates + 3 user:1 updates spread across courses 1, 1, 2). Filtered URL `/__audit?entity_table=course&entity_pk=1` correctly shows 3 entries (1 create + 2 updates for course 1) with both filter pills visible.

## [v0.12.6] — audit retention — 2026-05-01

### Added

- **`audit::cleanup_older_than(pool, cutoff_days) -> u64`** — deletes `rustango_audit_log` entries where `occurred_at < NOW() - cutoff_days * INTERVAL '1 day'`. Returns the number of rows removed. Per-tenant scope: each tenant's audit table is its own retention boundary, so the same call against a tenant pool only expires that tenant's history. `cutoff_days = 0` clears everything; negative values are clamped to 0.
- **Cleanup form on `/__audit`** — number input + "Apply cleanup" button. Defaults to 90 days, validates `≥ 0`, includes a `confirm()` dialog before submit. The cleanup itself emits an audit entry via `emit_one` with `entity_table = "rustango_audit_log"`, `entity_pk = "*"`, `operation = "delete"`, `changes = { __action: "audit_cleanup", cutoff_days, removed }` so the trail is self-describing — operators see who pruned what and when.

### Tests

- 3 new live tests in `audit_live`: 7-day cutoff retains recent rows, `0 days` clears the table, negative values clamp to 0. 19/19 pass.

### Curl-verified

- 4 seed-time `system` audit entries → POST `/__audit/cleanup` with `days=0` (alice / uid=1) → 4 rows deleted, 1 self-audit row remains: `{ "__action": "audit_cleanup", "cutoff_days": 0, "removed": 4 }` attributed to `user:1`.

## [v0.12.5] — admin /__audit activity feed — 2026-05-01

### Added

- **First-class admin activity feed at `/__audit`** — cross-row audit log view that lists every entry in `rustango_audit_log` newest-first with pagination (50/page). Each row is rendered with the same operation-coded badge as the per-row detail panel + a clickable link back to `entity_table#entity_pk`'s detail page. JSON `changes` payload is pretty-printed inline.
- **Facet filters on `/__audit`**: right rail shows distinct `entity_table`, `operation`, and `source` values with row counts; clicking a value toggles `?<col>=<value>` in the URL (mirrors the `list_filter` UI shape). Active filters render as `<code>` pills above the list with a "clear" link. Pager preserves the active filters across pages.
- **"Activity" link in the sidebar** pointing at `/__audit`. Highlights as active when on the audit page.

### Tests

- Browser-driven verification (curl + DB inspection): 8 audit rows in `pg-sju` (4 system creates + 4 user:1 updates) all render on the page; `?source=user%3A1` filters to 4 entries with the active-filter pill visible. 86/86 across the touched suites.

## [v0.12.4] — bulk-action audit — 2026-05-01

### Added

- **Admin bulk actions emit batched audit entries** — one `PendingEntry` per affected row, all written via a single `emit_many` after the action runs. Closes the gap from v0.12.3 where `admin_write_records_user_source_via_with_source_install` covered single-row writes but bulk actions left no audit trail.
- **Built-in `delete_selected`**: each row's pre-delete state is SELECTed before the bulk DELETE and snapshotted into the per-row audit entry's `changes`. Operators see exactly what got removed.
- **User-registered actions** (any name in `admin(actions = "...")` other than `delete_selected`): each affected row gets an `Update`-tagged audit entry with the row's pre-action snapshot plus an `__action` marker carrying the action's name. Lets the audit panel show "alice ran publish_selected on these rows; here's what they looked like before."
- All bulk audit entries inherit the per-request `with_source(User { id })` install from `tenancy::admin`, so the operator who ran the action shows up in `source` for every row.

### Implementation

- `action_submit` runs one `select_rows(WHERE pk IN (...))` before the action to capture pre-state, then dispatches to the action handler, then assembles `Vec<PendingEntry>` and calls `audit::emit_many`. One extra round-trip pre + one batched audit INSERT post — bounded cost regardless of N rows. Best-effort: a SELECT failure logs a tracing warning but doesn't fail the user-visible request, since the data write may have already partially committed.

### Tests

- Existing `audit_live`, `admin_live`, `tenant_auth_live` suites stay green (86/86 across the touched suites; full sweep stays at 126/126). Browser-verified end-to-end: ran `mark_4_credits` + `delete_selected` on uni_portal courses, confirmed 4 audit rows with correct ops + payloads.

## [v0.12.3] — admin update + delete also produce diff/snapshot audit JSON — 2026-05-01

### Improved

- **Admin update_submit now emits a diff** instead of a flat snapshot. Before the UPDATE, the handler runs a one-PK SELECT, captures every scalar field's prior value, and after the UPDATE compares against the form payload via `audit::diff_changes`. Resulting JSON: `{ "field": { "before": v, "after": v } }`. Unchanged fields drop out entirely. Closes the parity gap from v0.12.2 — both `Model::save_on(...)` and admin form POSTs now produce the same diff shape.
- **Admin delete_submit now emits a snapshot of the deleted row** (rather than an empty payload). SELECTs every scalar field before the DELETE, packages them into `snapshot_changes`. Operators see what was actually removed in the audit panel.

### Implementation

- `update_submit` / `delete_submit` both call `crate::sql::select_one_row` immediately before the data write to capture the before-state. Best-effort — a missing row (concurrent delete race) falls back gracefully without failing the user-visible request. The pre-select is one extra round-trip per admin write — bounded cost, paid only once per request.
- Field values stringify via `render::render_value_for_input` so the JSON shape matches what the operator typed in the form, regardless of the column's Postgres type. Keeps the admin audit consistent across `i64` / `String` / `DateTime` / `ForeignKey` / `Bool`.

### Tests

- Existing audit_live + admin_live + tenant_auth_live suites still pass (86/86 across the touched ones; full sweep stays at 126/126). No new test added — the existing `admin_write_records_user_source_via_with_source_install` covers the round-trip, and the diff shape is browser-verified manually given the variability of timestamps.

### Deferred to v0.12.4

- Diff for the admin's `delete_submit` is currently a snapshot (no "before/after"). Could be marked as a delete with `{ "field": { "before": v, "after": null } }` for symmetry — opinion split, defer until a user asks.

## [v0.12.2] — UPDATE diff + admin audit-trail panel — 2026-05-01

### Added

- **True before/after diff on `Model::save_on` UPDATE branch** — for audited models, the macro now emits a single-PK `SELECT` of the tracked columns BEFORE the UPDATE, captures each field's prior value, and after the UPDATE runs `audit::diff_changes(before, after)` so unchanged columns drop out of the JSON. The audit row's `changes` becomes the canonical Django shape `{ "field": { "before": <v>, "after": <v> } }`. The before-SELECT is one extra round-trip per audited UPDATE — bounded cost, paid only when audit is opted-in.
- **Admin "Audit trail" panel on the detail page** — every model's `/<table>/<pk>` page now renders an `<section class="audit-trail">` showing the most recent audit entries newest-first, with operation badge, source attribution, timestamp, and a pretty-printed JSON of `changes`. Best-effort lookup: missing `rustango_audit_log` table renders an empty section instead of failing the page.

### Two audit-emission paths, two shapes

The admin's `update_submit` handler bypasses `Model::save_on` (it builds a generic `UpdateQuery` because the admin works across every model uniformly), so admin writes still emit a *snapshot* of the form payload — not a diff. Application code that calls `model.save_on(&mut conn)` gets the diff. Both paths land in the same `rustango_audit_log` table; the JSON shape distinguishes them. A future v0.12.x can teach the admin handler to do its own before-SELECT for parity.

### Tests

- Updated `macro_emits_audit_update_entry_with_before_after_diff` in `audit_live` — asserts unchanged columns are excluded from the diff JSON. 16/16 audit_live tests pass; 126/126 across the full live sweep.

## [v0.12.1] — admin auto-attribution + admin write audit + uni_portal demo — 2026-05-01

Closes the v0.12.0 deferred items so the audit story is end-to-end.

### Added

- **Admin handlers emit audit entries for every write**, regardless of whether the model declares `#[rustango(audit(...))]`. `create_submit` writes `operation = "create"`, `update_submit` writes `"update"`, `delete_submit` writes `"delete"`. Form values become the `changes` JSON snapshot. Best-effort emit — failures log a warning but don't fail the user-visible request.
- **Tenant admin auto-attributes user**: `tenancy::admin::handle_request` now wraps the inner-router dispatch in `audit::with_source(AuditSource::User { id: session.uid })` for every authenticated request. Anonymous public surface and projects without `with_session` keep `AuditSource::System` as the default (no scope entered).
- **`ForeignKey<T>: Serialize`** — the FK enum now serializes to its PK integer. Lets audited models include FK columns in `audit(track = "...")` and have the audit JSON record the parent's PK without forcing every FK target to also derive `Serialize`.

### Demo

- **uni_portal `Course` is now audited** (`audit(track = "code, title, credits, instructor")`). The seed creates 4 tenants with `audit::ensure_table` per tenant pool, and a new `GET /api/courses/:pk/audit` endpoint reads the per-row trail. Browser-driven verification: an admin update by `alice` (uid=1) produces an audit entry with `source = "user:1"` while the seed-time create stays attributed to `system`.

### Tests

- New `tenant_auth_live::admin_write_records_user_source_via_with_source_install` — full round-trip through login + admin POST update + audit read, asserts `source = "user:<uid>"`.
- 126/126 across the full live sweep (audit_live, mixins_live, admin_live, save_live, order_by_annotate_live, foreign_key_live, prefetch_related_live, select_related_live, tenant_admin_live, tenant_auth_live, tenant_migrate_live, manage_live).

### Still deferred to v0.12.2

- True before/after diff in `save_on` UPDATE branch (today snapshots the after-state only). Requires a before-SELECT round-trip.
- A "View audit trail" panel in the admin detail page (today exposes via the user's API; the panel needs a Tera template + helper).

## [v0.12.0] — base-model mixins + per-tenant audit log — 2026-05-01

Brings Django-shape "BaseModel inheritance" semantics to rustango: opt-in `auto_uuid` / `auto_now_add` / `auto_now` / `soft_delete` field-level mixins, and a per-tenant audit log that records who changed what, with source-of-change attribution.

### Added

- **Field-level mixins (commit 1)**:
  - `#[rustango(auto_uuid)]` on `Auto<uuid::Uuid>` — UUID PK; DB-side `gen_random_uuid()` default.
  - `#[rustango(auto_now_add)]` on `Auto<DateTime<Utc>>` — `created_at` shape; server-set on INSERT, immutable on UPDATE.
  - `#[rustango(auto_now)]` on `Auto<DateTime<Utc>>` — `updated_at` shape; macro rewrites every UPDATE to bind `chrono::Utc::now()`.
  - `#[rustango(soft_delete)]` on `Option<DateTime<Utc>>` — adds `soft_delete_on(executor)` and `restore_on(executor)` methods.
  - `Auto<T>` now accepts `Uuid` and `DateTime<Utc>` in addition to integers.

- **Audit primitives (commit 2)** — new `rustango::audit` module:
  - Composite-key `rustango_audit_log(entity_table, entity_pk, operation, source, changes JSONB, occurred_at)` with covering indexes. Lives **per-tenant** for tenancy projects (one table per schema/database).
  - `AuditSource { System, User { id }, Custom(String) }` flows through a tokio task-local; `audit::with_source(src, fut).await` scopes a source for the duration of `fut`. Default is `System`.
  - `emit_one(executor, &entry)` / `emit_many(executor, &entries)` write paths. `fetch_for_entity(pool, table, pk)` reads the per-row history newest-first.
  - `diff_changes(before, after)` and `snapshot_changes(after)` JSON builders. Idempotent `ensure_table(pool)` for ad-hoc setup.

- **Macro emits audit hooks** (commits 3a/3b/3c) — declare `#[rustango(audit(track = "title, body"))]` on a Model derive and the macro auto-emits a `PendingEntry` after every per-row write:
  - `insert_on` → operation = "create" (snapshot of after-state)
  - `save_on` UPDATE branch → operation = "update" (snapshot of after-state)
  - `delete_on` → operation = "delete" (snapshot of in-memory `&self`)
  - `soft_delete_on` → operation = "soft_delete"
  - `restore_on` → operation = "restore"
  - `bulk_insert_on` → one batched `emit_many` regardless of N rows. One audit round-trip per call.
  - Field-name list in `track = "..."` validated at compile time against declared scalar fields.
  - Per-call source override: `save_on_with(executor, source)`, `insert_on_with`, `delete_on_with` — wrap the underlying call in `audit::with_source(...)` so seed scripts and CLI tools can attribute writes without touching the task-local.

### Changed

- For audited models, the executor on `_on` methods (`insert_on`, `save_on`, `delete_on`, `soft_delete_on`, `restore_on`, `bulk_insert_on`) is now `&mut sqlx::PgConnection` (concrete) rather than `_E: Executor` (generic), so the macro can reborrow `&mut *_executor` across the data write and the audit write. Non-audited models keep the generic signature for backward compatibility.
- `&PgPool` convenience wrappers (`save`, `insert`, `delete`, `bulk_insert`) acquire a connection from the pool internally for audited models, then forward to the `_on(&mut PgConnection)` variant. Non-audited models keep the direct delegation.

### Deferred to v0.12.1

- True before/after diff in `save_on` UPDATE branch (today snapshots the after-state only). Requires a before-SELECT round-trip; queued.
- Admin handler auto-install of `audit::with_source(User { session.user_id })` per request.
- uni_portal end-to-end demo.

Tests: 16 new in `audit_live` (per-op emit, with-source override, per-call `_with` override, bulk audit) + 4 in `mixins_live` (Auto<UUID> insert, auto_now_add fill, auto_now rebind, soft_delete + restore round-trip). Full sweep: 109/109.

## [v0.11.0] — user-defined bulk actions — 2026-04-30

### Added

- **`admin::Builder::register_action(table, name, handler)`** — register custom bulk action handlers. The action's name must also appear in the model's `#[rustango(admin(actions = "..."))]` allowlist; the attribute is the allowlist, this is the executable. Built-in `delete_selected` keeps working without registration. Handler receives `(&PgPool, &[SqlValue])` and returns `Result<(), AdminError>`.
- **`tenancy::admin::TenantAdminBuilder::register_action(...)`** — same shape, but the handler runs against the resolved tenant's pool (search_path scoped to the tenant's schema).
- **`server::Builder::admin_register_action(...)`** — top-level chain entry point that forwards into the auto-mounted tenant admin. Lets a multi-tenant app register actions in `main.rs` alongside `admin_show_only` / `migrate` / `seed_with`.
- **`AdminError`, `AdminActionFn`, `AdminActionFuture`** — promoted from `pub(crate)` to public so user code can return errors and type-annotate handlers.

Tests: 2 new in `admin_live` covering a custom UPDATE action via `register_action` and the "allowlisted but unregistered" hint that points at `register_action` in the 500 body. 63/63 pass.

## [v0.10.0] — admin Django-parity — 2026-04-30

Pulling the auto-admin from "functional CRUD" toward Django ModelAdmin shape. v0.10 lands across slices; this entry tracks what's shipped so far.

### Added

- **Sidebar nav on every admin page (slice 10.1)** — `admin/templates/base.html` is now a CSS-grid with a left rail listing every visible model grouped by app label, with active-state highlighting on the current table. Tenant operators can navigate between models without bouncing through the index. Mobile breakpoint stacks the rail above content.
- **Per-model `#[rustango(admin(...))]` attribute (slice 10.2)** — Django ModelAdmin-shape knobs declared inline on the model derive, surfaced as `ModelSchema.admin: Option<&'static AdminConfig>`. Field-name lists (`list_display`, `search_fields`, `readonly_fields`, `ordering`) are validated against declared fields at compile time via `compile_error!`.
- **`list_display` / `search_fields` / `list_per_page` / `ordering` driven by the new attribute (slice 10.3)** — list view's columns, search columns, page size, and default sort all read from `AdminConfig`. Defaults preserve today's behavior so existing models render identically. Django-shape `-name` syntax for descending order.
- **`list_filter` right-rail facet filters (slice 10.4)** — declare `admin(list_filter = "field1, field2")` and the list view grows a right rail with one card per facet showing every distinct value with its row count. Clicking a value toggles `?<col>=<value>` in the URL; clicking the active value clears the filter. Two-column subgrid collapses below a 1000px viewport. SQL is one `GROUP BY` round-trip per facet (acceptable for low-cardinality fields; high-cardinality fields should not be added to `list_filter`).
- **Bulk actions (slice 10.6)** — declare `admin(actions = "delete_selected")` and the list view grows an action picker `<select>` + `Go` button at the top of the table, plus a per-row checkbox. Selected PKs POST to `/<table>/__action` and the named action runs in a single round-trip. Built-in: `delete_selected`. Action names that aren't in the model's allowlist are rejected with a 500 (defense against URL guessing). User-defined action handlers queue for v0.11.

- **`fieldsets` + `readonly_fields` on create/edit forms (slice 10.5)** — declare `admin(fieldsets = "Identity: name, office | Audit: created_at")` and the form renders each section as `<fieldset><legend>...</legend>` with grouped fields. `readonly_fields = "created_at"` flips matching inputs to HTML `readonly` AND skips them server-side in `update_submit` so a manipulated POST can't override the value. PK on edit form is read-only; PK on create form is omitted entirely (slice 10.2).

### Improved

- **FK display in `list_filter` facets (slice 10.7)** — when a faceted field is a `ForeignKey`, the facet card now JOINs to the target's `display` column and renders the target's display value (e.g. `Dr. Maeve O'Hara (3)`) instead of the raw PK number (`1 (3)`). One JOINed `GROUP BY` query per facet — same round-trip count as before. List view's column rendering already JOINed for FK display in v0.7's auto-admin; this brings facets to parity. Falls back to raw value for FK targets that aren't visible in the admin or have no `display = "..."` attribute.

## [v0.9.1] — multi-tenant polish — 2026-04-30

Fixes surfaced while building a real four-tenant demo (database-mode + schema-mode mixed) and driving its admin end-to-end with a real browser.

### Fixed

- **Admin create form rendered server-assigned `Auto<T>` PK as `<input required>`**, so HTML5 native validation silently blocked submit when the operator left the column blank — exactly the right thing to do, but the column shouldn't appear at all. The create form now omits Auto-PK columns; Postgres' `BIGSERIAL` DEFAULT fills the value via `insert_returning`, and the redirect uses the returned PK. Edit forms still display the existing PK as read-only. Regression tests in `admin_live`: `create_form_for_auto_pk_omits_id_input` and `create_submit_for_auto_pk_assigns_pk_and_redirects`.
- **`tenancy::manage::api::create_tenant{,_if_missing}(.., migrations_dir, ..)`** silently no-op'd (and then errored with `relation "rustango_users" does not exist`) when the caller passed a project root rather than a flat migrations directory. The typed API now mirrors `Builder::migrate`'s auto-detect via a new `resolve_migration_dirs` helper: it accepts a project root that contains a flat `migrations/` subdir or per-app `<x>/migrations/` subdirs, the flat dir directly, or both.

### Added

- **`annotate_count_children_on(parent_qs, child_table, fk_column, executor)`** — `_on(executor)` companion to v0.9.0's `annotate_count_children`. Lets tenant-scoped admin / API code drive the optimized one-query annotation path through a `&mut PgConnection` (search_path scoped to the tenant's schema), instead of falling back to a per-parent `count_on` loop (N+1). The pool variant now delegates to this, mirroring the `_on` shape we ship for `insert`/`update`/`delete`/`bulk_insert`/`fetch`. Regression test: `order_by_annotate_live::annotate_count_children_on_works_against_acquired_connection`.

### Improved

- **`migrate_tenants` log line now includes `migrations=<n>` and `dir=<path>`**, and emits a `WARN` when the runner is asked to apply a tenant-scoped migration but found zero in the dir — that's the most common bug shape ("applied=0" everywhere) and the warning names the likely root cause directly.

## [v0.9.0] — ORM-shape complete

Closes the gap between rustango's ORM and Django's. Every advanced query pattern Django ships — `select_related`, `prefetch_related`, `.annotate(Count(...))`, `.order_by(...)`, paginated counts in one query, multi-app projects — is now first-class. The unreleased v0.8.2 changes (write-path `_on`, `Builder`, `Tenant` extractor, reverse-FK helper, `count_on`, `fetch_paginated`, demo refactor, `manage` polish) are folded into this release rather than published separately.

### Added — query layer (slice 9.0b)

- **`QuerySet::order_by(&[(field, desc)])`** — schema-validated `ORDER BY` clauses, multiple calls compose left-to-right, qualified column refs when JOINs are present so it composes cleanly with `select_related`.
- **`fetch_with_prefetch::<P, C>(qs, fk_column, &pool) -> Vec<(P, Vec<C>)>`** — Django's `prefetch_related` shape. Two SQL queries flat regardless of N parents: one over the parent queryset, one batched `WHERE <fk> IN (...)` over the children. Each parent paired with its matching children; parents with no children get an empty `Vec`.
- **`annotate_count_children::<P>(qs, child_table, fk_column, &pool) -> Vec<(P, i64)>`** — Django's `Author.objects.annotate(post_count=Count('post'))`. One SQL with `LEFT JOIN child` + `COUNT(child.id)` + `GROUP BY` over every parent column. MVP scope: single Count over a single reverse-FK; multi-aggregate annotation queues for follow-on.

### Added — `select_related` (slice 9.0d)

- **`QuerySet::select_related(field)`** — eagerly load a `ForeignKey<Parent>` field via a `LEFT JOIN`, with `ForeignKey::Loaded { pk, value }` on the returned rows. Single SQL round trip, no N+1.
- Schema validation at `compile()` — rejects non-FK fields with `QueryError::SelectRelatedInvalid`.
- Per-Model `__rustango_from_aliased_row(row, prefix)` macro emit reads aliased columns from a JOINed row.
- `LoadRelated` trait (auto-impl'd by every Model derive) is the polymorphic dispatcher `fetch_on` calls for each select_related entry.

### Added — multi-app project support (slice 9.0g)

- **`ModelEntry::resolved_app_label()`** — Django-shape `app_label` resolution. Explicit override via `#[rustango(app = "blog")]`; otherwise inferred from `module_path!()` at registration site.
- **Per-app migration directories** — `file::list_dirs` + `file::discover_migration_dirs(project_root)` walk both `<root>/migrations/` and every `<root>/<app>/migrations/`. `Builder::migrate(project_root)` applies all of them in dependency order with shared ledger dedup.
- **`manage makemigrations --app <name>`** — diffs only that app's models, writes to `<project_root>/<app>/migrations/`.
- **`manage startapp` auto-mount** — patches `src/main.rs` to add `mod <name>;` and `src/urls.rs` to add `.merge(crate::<name>::urls::api())` after `Router::new()`. Idempotent, with bail-out hints when the user's layout doesn't match the canonical pattern. New `StartAppReport.patched` / `manual_steps` fields surface to the CLI.
- **Admin sidebar grouped by app** — index template renders one `<section>` per `app_label`; "Project" group pinned at the bottom for unlabelled models.
- **`startapp --into <dir>`** + **`--with-bootstrap-migration`** — non-standard layouts (examples, workspace members without `src/`) and one-command tenancy bootstrap respectively.

### Added — server + extractors (slice 9.0)

- **`rustango::server::Builder`** — Django-style runserver. Owns `PgPool::connect`, `TenantPools` construction, resolver chain, host-based dispatch (apex → operator console / subdomain → tenant admin + user routes), bind + `axum::serve`. Methods: `from_env`, `admin_show_only`, `api(Router<()>)`, `migrate(project_root)`, `seed_with(closure)`, `serve(addr)`. A whole tenancy app's `main` is now five framework calls.
- **`rustango::extractors::Tenant`** — `FromRequestParts` extractor that resolves the request's tenant via `ChainResolver` and exposes a tenant-scoped `&mut PgConnection` through `tenant.conn()`. Reads `TenantContext` from request extensions populated by `Builder` — no `with_state` plumbing needed.

### Added — paginated reads in one query (slice 9.0f)

- **`QuerySet::fetch_paginated_on(executor) -> Page<T>`** — returns `{ rows, total }` from a single SQL via Postgres' `COUNT(*) OVER ()` window function. **Beats Django's `Paginator`**, which always runs two queries; same for DRF's pagination.

### Added — write-path executor variants (carryover from v0.8.1)

- **`Model::save_on / insert_on / bulk_insert_on / delete_on`** + low-level `executor::*_on` functions. Accept any `sqlx::Executor` (pool, connection, transaction). Pool methods delegate. Closes the tenancy gap where schema-mode connections (carrying per-checkout `SET search_path`) couldn't drive ORM writes.
- **`Fetcher::fetch_on` + `ForeignKey::get_on`** — read-side counterpart for tenant-scoped queries.
- **`QuerySet::count_on`** — typed COUNT for tenant connections.
- **Reverse-FK helper `<parent>::<child>_set(&self, executor) -> Vec<Child>`** — auto-emitted from `ForeignKey<Parent>` fields. One SQL query, no manual `where_(Post::author.eq(id))` required.

### Added — typed tenancy management (carryover from v0.8.1)

- **`tenancy::manage::api`** — typed Rust API for `create_tenant_if_missing`, `create_operator_if_missing`, `create_user_if_missing`, `find_org`. Idempotent variants replace the verb-string CLI dispatcher for in-process callers; the verb dispatcher remains for shell consumers.
- **`#[rustango::main]`** — proc macro wrapping `#[tokio::main]` + default `tracing_subscriber` boot. New `runtime` feature (implied by `tenancy`) gates the dep.

### Added — `cargo-rustango` template improvements (carryover)

- All three templates (api / fullstack / tenant) expose `pub fn api() -> Router<...>` aggregator shapes so `manage startapp` auto-mount produces well-typed code.
- Generated projects ship `rust-toolchain.toml` (1.88) and `[workspace]` table to neutralize parent-workspace inheritance.
- `default-run = "<name>"` resolves the bare-`cargo run` ambiguity created by shipping two binaries.
- Tenant template bundles registry+tenant bootstrap migrations so the very first `manage migrate` works without a separate `init-tenancy` step.
- `manage` CLI: `help` / `--help` / no-args verb (works without `DATABASE_URL`); friendly error message when `DATABASE_URL` is unset.
- `manage startapp --into <dir>` + `--with-bootstrap-migration` flags.

### Roadmap — what's next (queued for v0.9.x or v0.10)

- **Slice 9.1 — Serializers** (`#[derive(ModelSerializer)]`). Design locked: sync `dump`/`validate` + async `validate_async` for DB-touching validators; nested serializers stay sync (push hydration to `select_related`/`prefetch_related`); streaming responses async at I/O boundary only. ~10 days.
- **Slice 9.2 — ViewSets**, **9.3 — OpenAPI auto-gen**, **9.4 — browsable API**, **9.5 — multi-auth (Session / Token / JWT / Basic)** — all gated behind 9.1.
- Multi-aggregate annotation (`Sum`, `Avg`, `Min`, `Max`); `prefetch_related` connection variant for tenant-scoped users; full Django-shape `Author::objects().prefetch_related("post_set")` builder API.

## [v0.8.2] — folded into v0.9.0

The unreleased v0.8.2 section below documents the demo-as-canary work (write-path `_on` ORM, `Builder::migrate`, `manage::api`, `#[rustango::main]`, scaffolder polish, paginated reads, friendly error messages, multi-app urls aggregator, etc.). All of it is included in the v0.9.0 release above; this section is preserved for the per-slice detail.

Demo-as-canary release: drove every line of `examples/blog_demo` through framework features. The user reviewed the prior v0.8.1 demo and asked the right question — "why don't you use ORM and migrations tool in seeds file?". v0.8.2 closes the last gaps so the answer is "we do, all the way down".

### Added — write-path executor variants (`save_on` / `insert_on` / `bulk_insert_on` / `delete_on` / `update_on`)

- **Macro-generated `_on` methods** on every `#[derive(Model)]` type: `Model::save_on(executor)`, `Model::insert_on(executor)`, `Model::bulk_insert_on(executor)` (Auto + non-Auto variants), `Model::delete_on(executor)`. Accept any `sqlx::Executor<'_, Database = Postgres>` — `&PgPool`, `&mut PgConnection`, transactions. The pool methods (`save`, `insert`, …) keep working: they're now 1-line delegates to the new variants. Non-breaking for v0.8.1 callers.
- **Low-level `executor::*_on` functions**: `insert_on`, `insert_returning_on`, `bulk_insert_on`, `update_on`, `delete_on`. The pool functions delegate. Re-exported from `rustango::sql`.
- **Why this matters:** schema-mode tenancy shares the registry pool but relies on per-checkout `SET search_path` — passing `&PgPool` would silently hit `public`. With v0.8.2 the demo's seed runs `Author { … }.save_on(tenant.conn()).await?` and the row lands in the tenant's schema as expected.

### Added — `rustango::tenancy::manage::api` typed Rust API

- New public module wrapping the previously `pub(super)` provisioning verbs. Functions: `create_tenant_if_missing(pools, registry_url, dir, slug, opts)`, `create_tenant(...)`, `create_operator_if_missing(pools, username, password)`, `create_user_if_missing(pools, slug, username, password, superuser)`, `find_org(pools, slug)`. All return typed model values; `*_if_missing` variants are idempotent (return existing on duplicate, no error-string matching needed).
- `CreateTenantOpts` carries `mode`, `display_name`, `schema_name`, `database_url`, `host_pattern`, `port`, `path_prefix`, `no_migrate` — all `Option`s with `Default::default()`. Replaces stringly-typed `vec!["create-tenant", slug, "--mode", "schema", …]` for in-process callers.
- The verb dispatcher (`tenancy::manage::run_with_writer`) is unchanged for CLI consumers.

### Added — `rustango::server::Builder::migrate(dir)`

- One Builder method that subsumes the three calls every tenancy app would otherwise wire up: `init_tenancy(dir)` (writes registry + tenant bootstrap migrations if absent), `migrate_registry(pools, dir)`, `migrate_tenants(pools, dir, registry_url)`. Creates `dir` via `fs::create_dir_all` if it doesn't exist — first-run friendly.
- Self-returning, so it composes: `Builder::from_env().await?.migrate("migrations").await?.api(...).seed_with(...).await?.serve(...).await`.

### Added — `#[rustango::main]` attribute proc macro

- New `#[rustango::main]` wraps `#[tokio::main]` plus a default `tracing_subscriber` boot (`EnvFilter::try_from_default_env().unwrap_or("info,sqlx=warn")`). User `main` becomes zero-boilerplate.
- Optional args pass through to tokio: `#[rustango::main(flavor = "current_thread")]`.
- Lives behind a new `runtime` feature (implied by `tenancy`) so apps that don't want the macro can opt out and skip the `tracing-subscriber` dependency. `tracing-subscriber` moved from dev-deps into the optional dep list with `default-features = false, features = ["fmt", "env-filter"]` for minimal cold-compile cost.

### Added — `manage startapp --with-bootstrap-migration` (one-command tenancy setup)

The tenancy-aware `startapp` now optionally drops the framework's registry + tenant bootstrap migrations into the new app's `<app>/migrations/` subdirectory in the same invocation that scaffolds the code files. Pair with `Builder::migrate("<dir>/<app>/migrations")` and a fresh tenancy project is `cargo run`-ready in one command — no separate `manage init-tenancy && manage migrate` step.

```sh
cargo run --example blog_demo_manage --features tenancy -- \
    startapp shop --into examples/myproj --with-bootstrap-migration
# writes:
#   examples/myproj/shop/{mod,models,views,urls}.rs
#   examples/myproj/shop/migrations/0001_rustango_registry_initial.json
#   examples/myproj/shop/migrations/0001_rustango_tenant_initial.json
```

The post-scaffold hint message switches accordingly: with the flag, it says "bootstrap migrations are already in `<dir>/<app>/migrations/` — point `Builder::migrate(...)` at that directory and `cargo run` is enough." Without the flag, the original `manage init-tenancy && manage migrate` recipe is printed (plus a tip about the new flag for next time). Idempotent — the flag re-runs against an existing directory skip files that are already there.

Verified end-to-end via the demo's manage CLI; the bootstrap files emitted are byte-identical to the standalone `init-tenancy` verb's output.

### Added — `manage startapp --into <dir>` for non-standard project layouts

The v0.7 scaffolder hard-coded the destination to `<cwd>/src/<app>/` — correct for `cargo new`-shaped projects but wrong for examples, workspace members without `src/`, or any layout that puts apps in `examples/`, `app/`, etc. Reviewers running `cargo run --example blog_demo_manage -- startapp shop` from the workspace root got `src/shop/` written next to the workspace `Cargo.toml` instead of next to the demo's `blog/`.

- New `--into <dir>` flag on both `manage startapp` (single-tenant) and `tenancy::manage startapp` (tenancy-aware). Overrides the default `src/` base. The scaffolder writes `<cwd>/<dir>/<app_name>/{mod,models,views,urls}.rs` and (when `--with-manage-bin` is set) `<cwd>/<dir>/bin/manage.rs`.
- New public `StartAppOptions::base_dir: Option<PathBuf>` field exposes the same hook to programmatic callers. `StartAppOptions` now `#[derive(Default)]` so callers can `..Default::default()` instead of listing every field.
- Verified end-to-end: `cargo run --example blog_demo_manage --features tenancy -- startapp shop --into crates/rustango/examples/blog_demo` writes `crates/rustango/examples/blog_demo/shop/{mod,models,views,urls}.rs` — Django shape, in the right place, sibling to `blog/`.

### Changed — `examples/blog_demo` reshaped into Django project layout

The demo's files used to be flat under `examples/blog_demo/`; reviewers correctly noted this didn't match Django's "project shell at the top, apps as subdirectories" shape. Reorganized to:

```
examples/blog_demo/
├── main.rs              ← project shell (Builder + serve)
├── manage.rs            ← CLI dispatcher
└── blog/                ← the "blog" app (matches `manage startapp blog` output)
    ├── mod.rs
    ├── models.rs
    ├── views.rs
    ├── urls.rs
    ├── seed.rs
    └── migrations/
        ├── 0001_rustango_registry_initial.json
        ├── 0001_rustango_tenant_initial.json
        └── 0002_blog_initial.json
```

`main.rs` becomes `mod blog;` + a single Builder chain. `manage.rs` mounts the same `blog` module. To add another app, run `cargo run --example blog_demo_manage --features tenancy -- startapp shop` — the existing v0.7 scaffolder writes the same `<app>/{mod,models,views,urls}.rs` shape into a new directory; add `mod shop;` next to `mod blog;` in `main.rs` and the second app is mounted. No framework changes — just demonstrating that the existing scaffolder + the v0.8.2 `Builder::migrate(dir)` already compose into the canonical Django shape.

Migrations live inside the app at `blog/migrations/` (per-app, Django-shaped). For multi-app projects with separate migration sets, v0.9 will add per-app migration discovery; today the runner takes one directory, so single-app demos like this one fit the Django shape exactly.

### Added — `fetch_paginated_on` — rows + total in one SQL query (better than Django)

- New `QuerySet::fetch_paginated_on<E: sqlx::Executor>` (and pool-side `fetch_paginated`) returns a `Page<T> { rows: Vec<T>, total: i64 }` from a **single** SQL round trip. The total is the count of rows matching the WHERE before LIMIT/OFFSET; same trip as the page slice. Built on Postgres' `COUNT(*) OVER ()` window function, stable since 8.4.
- **Beats Django's `Paginator`**, which always runs two queries (one `SELECT`, one `SELECT COUNT(*)`); same for DRF's pagination. With `fetch_paginated_on` paginated endpoints get rows + total without the second round trip.
- SQL emitted (verified via `RUST_LOG=sqlx::query=debug`):
  ```sql
  SELECT id, title, body, author, published_at, COUNT(*) OVER () AS "__rustango_total"
  FROM post
  LIMIT 2 OFFSET 0
  ```
- New `Page<T> { pub rows: Vec<T>, pub total: i64 }` re-exported from `rustango::sql`. Empty result set → `Page { rows: vec![], total: 0 }`. The total-count column injection happens in `executor.rs` via string splice at the ` FROM ` boundary — fully contained, no dialect-writer surface change. SQLite (window functions since 3.25) and MySQL (8.0+) both support `COUNT(*) OVER ()`, so the v0.10 multi-DB story is covered too.
- Demo: new `GET /api/articles/paginated?page=N&per_page=M` endpoint in `examples/blog_demo/views.rs::list_articles_paginated`. Verified `total=6` on every page slice (`page=1 per_page=2`, `page=2 per_page=3`, etc.) with one SQL query for the page itself plus the existing batched author fetch (`WHERE id IN`) for embedding — N posts in two queries flat.

### Added — `count_on(executor)` for tenant-scoped row counts

- New `QuerySet::count_on<E: sqlx::Executor>` mirrors the existing `Counter::count(&PgPool)` for tenant connections. The pool method now delegates. Re-exported with the low-level `count_rows_on` from `rustango::sql`.
- The blog demo's `/api/authors` count loop drops its raw `sqlx::query_as("SELECT COUNT(*) ...")` for `Post::objects().where_(Post::author.eq(id)).count_on(tenant.conn()).await?` — zero raw SQL in the entire view module. Still N+1 (one COUNT per author) until v0.9 ships aggregation; the win here is "ORM-driven, not stringly-typed".

### Changed — `/api/articles` embeds full author + drops N+1 via batched `IN` fetch

- Articles now serialize as `{id, title, body, published_at, author: {id, name, bio, post_count}}` — the embedded author is the full record, not just `author_name`.
- The handler no longer does `post.author.get_on(conn)` per row (N+1). Instead: one `SELECT … FROM post`, then collect distinct PKs and one batched `SELECT … FROM author WHERE id IN ($1, $2, …)` via `Author::id.is_in(pks)`. Stitched into a `HashMap<i64, Author>` and rendered. **Two queries total**, regardless of post count — verified via `RUST_LOG=sqlx::query=debug`.
- The proper one-query forward JOIN (`Post::objects().select_related("author").fetch_on(conn)` → single SQL with `LEFT JOIN`) still queues for v0.9 — it needs compile_select JOIN emit, alias-prefixed column decoder, and macro-generated `ForeignKey::Loaded` setters. The current two-query batched fetch closes the user-visible N+1 today; `select_related` will collapse it to one round trip later.

### Added — reverse-FK macro helper (`<parent>::<child>_set`)

- **`#[derive(Model)]` now emits an inherent `<child>_set(&self, executor)` method on every parent type** for each `ForeignKey<Parent>` field on a child. So `Post { author: ForeignKey<Author>, … }` automatically gives `Author` a method `post_set(&self, executor) -> Result<Vec<Post>, ExecError>`. One SQL query: `SELECT … FROM post WHERE author = $1` — no N+1, no client-side join, no hand-written WHERE clause.
- **Naming convention:** `<snake_case_child_name>_set` — Django shape, predictable across irregular plurals.
- **Implementation:** the parent's own `Model` derive emits a private `__rustango_pk_value(&self) -> SqlValue` inherent helper that the reverse method calls to read the parent's PK at runtime. Both impls coexist as inherent impls; works as long as parent and child are in the same crate (the Django shape).
- **Demo endpoint:** new `/api/authors/{id}/articles` in `examples/blog_demo/views.rs::articles_by_author` uses `author.post_set(tenant.conn()).await?` end-to-end. Verified single-query behaviour via `RUST_LOG=sqlx::query=debug`: one `SELECT ... FROM "post" WHERE "author" = $1` returning the right rows.
- **Forward `select_related` (one-query JOIN to load `post.author` along with `Post`)** is the natural complement and is queued as a v0.9 slice — bigger surface area (compile_select JOIN emit + per-Model alias-row decoder + macro support for setting `ForeignKey::Loaded` from a joined row). `list_articles` keeps the FK lazy-load (N+1) until that lands.

### Changed — `examples/blog_demo` end-to-end refactor

The blog demo is now the canonical Django-shape example, with **zero** raw SQL or hand-rolled DDL:

- **`migrations/0002_blog_initial.json` is committed** — generated via `cargo run --example blog_demo_manage --features tenancy -- makemigrations blog_initial`. Schema lives in JSON; `Builder::migrate(...)` applies it on every boot. The framework's existing make-migrations machinery (`src/migrate/make.rs`) auto-creates the `migrations/` dir on first run, so `manage makemigrations` works in a fresh project with no setup.
- **`seed.rs` is ORM-only and idempotent.** Provisioning via `manage::api::create_*_if_missing(...)` (typed args, returns the model). Author + Post seed data via `Author { … }.save_on(conn).await?`. Re-runs against the same DB are no-ops — tables, tenant, operator, user, and seed rows are all checked-and-skipped. No `drop_all`, no `DROP SCHEMA`, no destructive cleanup anywhere.
- **`main.rs` is one framework chain** — `#[rustango::main]` + `Builder::from_env().migrate("…").api(urls::api()).seed_with(seed::run).serve(...)`. No `tokio::main`, no `tracing_subscriber::fmt()`, no pool wiring, no resolver chain, no host dispatcher.
- **New `examples/blog_demo_manage` binary** — same models linked, dispatches to `tenancy::manage::run` so the full Django verb set works against the demo: `init-tenancy`, `makemigrations`, `migrate`, `migrate-registry`, `migrate-tenants`, `create-tenant`, `create-user`, `create-operator`, `showmigrations`. Doc header explains how to regenerate `0002_blog_initial.json` after a model change.
- **Live smoke verified twice**: a fresh boot provisions tenant + operator + user + 3 authors + 6 posts; a second boot against the same DB applies 0 migrations, creates 0 rows, and serves the same data unchanged.

## [Unreleased] — v0.8.1

Patch on top of v0.8 surfacing two DX gaps the `examples/blog_demo` review caught: tenant-scoped queries couldn't use the ORM, and every tenancy app had to hand-roll ~50 lines of pool / resolver / dispatcher wiring before serving. Both addressed without breaking 0.8 callers.

### Added — `Fetcher::fetch_on` + `ForeignKey::get_on` (tenant-scoped ORM)

- **`QuerySet::fetch_on<E: sqlx::Executor>`** in `src/sql/executor.rs` — runs a queryset against any executor, not just `&PgPool`. The escape hatch tenancy needs: schema-mode tenants share the registry pool but rely on a per-checkout `SET search_path`, so passing `&PgPool` would silently hit the public schema. Pass `tenant.conn()` (or any `&mut PgConnection`) and the ORM works in tenant scope.
- **`ForeignKey::get_on<E>`** mirrors the same shape for FK lazy-loads. `ForeignKey::get(&pool)` keeps working, delegating to `get_on(pool)` — non-breaking for v0.8 callers.

### Added — `rustango::server::Builder` (Django-style runserver)

- **New `rustango::server::Builder`** owns every line of boilerplate every tenancy app would otherwise rewrite: `PgPool::connect` from `DATABASE_URL`, `Arc::new(TenantPools::new(...))`, the `ChainResolver` (subdomain + header fallback) from `RUSTANGO_APEX_DOMAIN`, the host-based dispatcher (apex → operator console / subdomain → tenant admin + user routes), session-secret resolution, and `axum::serve` on a bound TCP listener. A tenancy-app `main` is now three framework calls — see `examples/blog_demo/main.rs`.
- **`Builder::api(Router<()>)`** mounts a stateless user-supplied router on the tenant subdomain. The Builder layers `Extension<Arc<TenantContext>>` so `extractors::Tenant` works in every handler — users don't have to thread state through `with_state`.
- **`Builder::admin_show_only`** narrows the auto-mounted tenant admin to specific tables.
- **`Builder::seed_with(closure)`** runs a first-run hook with `(Arc<TenantPools>, PgPool, String)` — for `init-tenancy` / `migrate-registry` / `create-tenant` provisioning.

### Added — `rustango::extractors::Tenant`

- **`Tenant` extractor** (`FromRequestParts`) resolves the request's tenant via the chain resolver in `TenantContext` (populated by `Builder`), acquires a tenant-scoped connection, and exposes it as `tenant.conn() -> &mut PgConnection`. Handlers become one-liners:
  ```rust
  pub async fn list_articles(mut t: Tenant) -> Result<Json<Vec<Post>>, StatusCode> {
      let posts = Post::objects().fetch_on(t.conn()).await?;
      Ok(Json(posts))
  }
  ```
- Rejection types: `MissingContext` (Builder didn't run, 500), `NotFound` (no tenant matches, 404), `Internal(String)` (resolver / pool error, 500). All implement `IntoResponse`.

### Added — `examples/blog_demo` end-to-end demo

- **New multi-file example** (`examples/blog_demo/{main,models,views,urls,seed}.rs`) demonstrating the full v0.8.1 shape: pre-seeded operator + tenant + superuser + 3 authors + 6 posts; `Author` + `Post` (`ForeignKey<Author>`) models; `GET /api/articles` + `GET /api/authors` JSON endpoints via the `Tenant` extractor; auto-mounted tenant admin + operator console via `server::Builder`. Three framework calls in `main.rs`. Run with `cargo run --example blog_demo --features tenancy`.

## [Unreleased] — v0.8

Absorb-the-field release. After a deep competitive review of Cot, Loco, and Reinhardt, v0.8 closes the four highest-impact gaps every reviewer notices in week 1: a `Dialect` seam for multi-DB, a `cargo rustango new` project scaffolder, layered TOML config, and a public forms framework with CSRF middleware. v0.9 (API-first surface — serializers, ViewSets, OpenAPI, browsable API, multi-auth) and v0.10 (operations + multi-DB — jobs, mail, cache, test harness, SQLite + MySQL) follow.

### Added — `Dialect` trait promotion + multi-DB seam (slice 8.1)

- **`sql::Dialect` extended with seven new methods** so SQLite + MySQL impls can slot in via v0.10 with minimal extra ceremony: `name()`, `quote_ident(name)`, `placeholder(n)`, `serial_type(field_type)`, `bool_literal(b)`, `supports_concurrent_index()`, `supports_returning()`. Default impls return ANSI-leaning shapes (`"foo"` quoting, `?` placeholders, `BIGINT`/`INTEGER`, `TRUE`/`FALSE`, no concurrent index, no RETURNING). `Postgres` overrides what diverges (`$N` placeholders, `BIGSERIAL`/`SERIAL`, supports both).
- **Lock dispatch through the dialect.** `migrate::runner`'s `with_migrate_lock` and `ensure_ledger_for` no longer inline `pg_advisory_lock` SQL — they ask the dialect for `acquire_session_lock_sql()` / `release_session_lock_sql()` / `acquire_xact_lock_sql()`. Default returns `None` (skip lock — SQLite's single-writer model + `BEGIN EXCLUSIVE` provides equivalent exclusion); Postgres returns the existing `pg_advisory_*` calls parameterised through `placeholder(1)`.
- Behaviour-preserving for Postgres: the SQL strings emitted are byte-for-byte identical to v0.7's hardcoded versions. Workspace tests pass with `--features tenancy --test-threads=1`; every `migrate_*_live` test green.

### Added — `cargo rustango new` project scaffolder (slice 8.2)

- **New `cargo-rustango` crate** (`cargo install cargo-rustango`) with a `cargo rustango new <name> [--template api|fullstack|tenant]` verb. Three templates:
  - **`api`** — bare ORM + axum, no admin (JSON-only services). `rustango = { version = "0.8", default-features = false, features = ["postgres"] }`.
  - **`fullstack`** (default) — ORM + auto-admin. `rustango = "0.8"`.
  - **`tenant`** — multi-tenancy enabled, `tenancy_manage`-style dispatcher in `src/bin/manage.rs`. `rustango = { version = "0.8", features = ["tenancy"] }`.
- Each template scaffolds `Cargo.toml`, `.env.example`, `.gitignore`, `docker-compose.yml`, `README.md`, `migrations/`, `src/{main,models,views,urls}.rs`, and `src/bin/manage.rs`. Templates live as `const &str` in the binary — zero runtime filesystem dependency.
- Smoke-tested: each template `cargo check`s cleanly when patched against the local v0.8 in-development rustango.

### Added — `rustango::config` layered TOML Settings (slice 8.3)

- **New `config` feature** (in `default = ["postgres", "admin", "config", "forms"]`) gating a `rustango::config::Settings` loader that merges three layers in order: `config/default.toml` → `config/{env}.toml` → `RUSTANGO__SECTION__KEY` env-var overrides (double-underscore is the path separator, lowercased).
- **Typed sections** (each `#[serde(default)]` so missing keys never error and new fields stay forward-compatible): `database` (url, pool sizing), `secret_key`, `admin` (allowed_tables, read_only_tables), `tenancy` (apex_domain), `cache` (backend, redis_url), `jobs` (backend, concurrency), `mail` (backend, smtp_host, from_address). The cache/jobs/mail sections are placeholders for v0.10 slices that will read from them.
- **Hand-rolled merger** — `toml = "0.8"` parser-only plus ~200 lines of merger / env-var grafter. Env-var values type-coerce automatically through TOML's own scalar lexer (`RUSTANGO__ADMIN__ALLOWED_TABLES='["user","post"]'` → `Vec<String>` with no manual coercion).
- 7 unit tests covering default-only load, file overlay, env-var override of a string, typed-int env-var override, nested section graft, missing default file errors, parse error includes file path.

### Added — public forms framework + `#[derive(Form)]` + CSRF (slice 8.4)

- **`rustango::forms`** — promoted from admin-internal `pub(crate)` to a public module. `FormError`, `parse_pk_string`, `parse_form_value`, `collect_values` all available to user route handlers. Admin's existing CRUD code re-exports from here.
- **`#[derive(Form)]`** in `rustango-macros` — generates `rustango::forms::FormStruct::parse(&HashMap<String, String>) -> Result<Self, FormError>` for any struct with named fields. Supported field types: `String`, `i32`, `i64`, `f32`, `f64`, `bool`, plus `Option<T>` for any of those. Per-field `#[form(min, max, min_length, max_length)]` validators apply in declaration order; first failure short-circuits.
  - Bool field semantics match HTML checkbox shape: absent = `false`; non-empty = `true` except literal `"false"` / `"0"` / `"off"` / `"no"`.
  - Empty string + `Option<T>` = `None`; empty string + non-null = `FormError::Missing`.
- **`rustango::forms::csrf::layer()`** — axum tower `Layer` enforcing double-submit-cookie CSRF. Safe methods (GET/HEAD/OPTIONS/TRACE) pass through and seed a fresh `rustango_csrf` cookie; unsafe methods (POST/PUT/PATCH/DELETE) require the cookie value to match an `X-CSRF-Token` header (constant-time compare). Mismatch / absent → 403 Forbidden.
  - 32-byte tokens from `OsRng`, base64url-encoded (no padding). Cookie: `SameSite=Lax`, `HttpOnly` off (SPA must read it), `Secure` configurable via `CsrfConfig`.
  - Cookie name `rustango_csrf` deliberately distinct from tenancy's `rustango_session` / `rustango_tenant_session` so the two flows don't collide on the same domain.
- **New feature flags:** `forms` (in default), `csrf` (in default via `admin`). `admin` now implies both. `csrf` pulls `cookie + rand + base64 + tower + axum`. Forms-only users (parsers without CSRF) skip those deps.
- 9 unit tests for `#[derive(Form)]` (minimal payload, full payload, empty Optional, missing required, unparseable int, all four validators, checkbox falsy aliases) + 5 integration tests for the CSRF layer (cookie seed on GET, 403 on tokenless POST, 403 on mismatched tokens, 403 on cookie-only no-header, pass with matching cookie + header).

### Notes

- v0.8 is content-complete on the working tree. v0.9 (API-first surface — serializers, ViewSets, OpenAPI, browsable API, multi-auth) and v0.10 (operations + multi-DB — jobs, mail, cache, test harness, SQLite + MySQL via the v0.8 Dialect seam) are queued per the absorb-the-field roadmap.
- **Multipart file uploads** (originally part of slice 8.4C's "+ multipart" piece) are deferred to v0.9. They need a follow-up design pass on the `UploadedFile` extractor + the `#[derive(Form)]` macro field-type detection for `Vec<u8>` / `UploadedFile` — neither blocks v0.8's "missing 30%" theme.
- The `Dialect` advisory-lock dispatch (slice 8.1B) currently still hardcodes `Postgres` inside `migrate::runner` via `let dialect = Postgres;`. v0.10's slice 10.5 will replace these with a generic dispatch (or a `Builder::dialect(...)` knob) once `SqliteDialect` / `MySqlDialect` exist. The seam itself is clean; only the call sites are still type-bound.

## [Unreleased] — v0.7

ORM ergonomics catch-up. v0.6 closed the multi-tenancy production gap; v0.7 is the day-2 ORM polish — `save()` insert-or-update, OR / nested-expr query filters, `ForeignKey<T>` lazy-load, and per-app migration namespacing. Tracked slice-by-slice.

### Added — `Model::save()` insert-or-update (slice 1)

- **`save(&mut self, &PgPool)`** — derived for any model whose primary key is `Auto<T>`. Dispatches on the in-memory PK: `Auto::Unset` → `INSERT … RETURNING <pk>` (populates the PK from the returned row, same shape as `insert`); `Auto::Set(_)` → `UPDATE … SET <every-non-pk-col> WHERE <pk> = …`. UPDATE matching no row returns `Ok(())` silently (matches Django's `save()` default).
- **Manually-managed PKs** (e.g. `id: i64` with caller-supplied values) are intentionally not given a `save()` — there's no way to infer insert-vs-update from the in-memory value, so the caller must use `insert` or the QuerySet update builder explicitly.
- 3 live tests in `crates/rustango/tests/save_live.rs` cover insert-on-unset (PK populated), update-on-set (PK preserved, every non-PK column written), and silent-ok on no-match.

### Added — per-app migration ledger naming (slice 2)

- **`migrate::Builder`** — fluent config object that overrides the migration ledger table name. `Builder::default()` keeps the default `__rustango_migrations__`; `Builder::new().ledger("__myapp_migrations__")` swaps it. Two rustango apps in one Postgres database can now coexist by picking distinct ledgers — previously the shared ledger meant either app applying its migrations would mark the other's as "already applied" or otherwise tangle bookkeeping.
- **Verbs mirrored** on the Builder: `migrate`, `migrate_to`, `migrate_embedded`, `migrate_dry_run`, `downgrade`, `unapply`, `unapply_force`, `applied_set`, `ensure_ledger`. Each delegates to a private `*_with_ledger` helper that threads the configured name through every internal SQL statement (`CREATE TABLE`, `INSERT INTO`, `DELETE FROM`, `SELECT FROM`).
- **Free functions unchanged.** `migrate::migrate(&pool, dir)` and friends thunk through `Builder::default()`, so existing call sites (the manage CLI, tenancy's `migrate_registry` / `migrate_tenants`, downstream apps) keep working without edits.
- **Ledger-name validation.** `Builder::ledger` panics if the name isn't a valid SQL identifier (`[A-Za-z_][A-Za-z0-9_]*`, ≤ 63 bytes). Configuration error caught at construction time, not deep in a SQL call.
- 3 live tests in `crates/rustango/tests/migrate_builder_live.rs` cover two-builder isolation (each ledger sees only its own entries; default `applied_set` doesn't see custom-ledger rows), default-Builder parity with the free functions, and synchronous validation panic on a quote-injection name.

### Added — `manage startapp` scaffolder (slice 7)

- **`rustango::migrate::scaffold`** — new public module with
  `startapp(project_root, opts) -> StartAppReport`. Materializes a
  Django-shape app module under `src/<name>/`:

  ```text
  src/<name>/
    mod.rs       — re-exports models / views / urls
    models.rs    — starter `#[derive(Model)]` (admin-visible)
    views.rs     — landing page + healthz handler stubs
    urls.rs      — `pub fn router(pool) -> Router` nesting the auto-admin
  ```

  Idempotent: existing files are reported as `skipped` and left
  untouched. Parent directories created on demand. App name is
  validated against `[A-Za-z_][A-Za-z0-9_]*`.

- **`manage startapp <name> [--with-manage-bin]`** — new verb in
  `rustango::migrate::manage`. With `--with-manage-bin`, additionally
  writes `src/bin/manage.rs` carrying the standard 5-line dispatcher
  boilerplate (`rustango::migrate::manage::run`).

- **`rustango_tenancy::manage startapp …`** — sister verb in the
  tenancy dispatcher. Same models/views/urls files (delegates to
  `rustango::migrate::scaffold::startapp`) but the
  `--with-manage-bin` template wires `rustango_tenancy::manage::run`
  + `TenantPools::new(...)` instead of the single-tenant dispatcher,
  so the resulting binary recognizes `create-tenant` /
  `migrate-tenants` / `run-server` / etc.

- 5 unit tests in `crates/rustango-migrate/src/scaffold.rs` (writes,
  idempotency, manage-bin template, name validation, mod template
  shape). End-to-end smoke against the docker postgres — both the
  single-tenant and tenancy flavors generate the expected file tree
  and re-run cleanly with all entries reported as skipped.

### Added — `ForeignKey<T>` lazy-load (slice 3)

- **`rustango::sql::ForeignKey<T>`** — new wrapper type that stores a parent's PK alongside an optional cached `Box<T>`. Replaces the v0.1 `i64` + `#[rustango(fk = "users")]` form for fields that want lazy-load ergonomics:

  ```rust
  #[derive(Model)]
  struct Book {
      #[rustango(primary_key)] id: Auto<i64>,
      title: String,
      author: ForeignKey<Author>,   // no attr — type carries the target
  }

  let mut book: Book = Book::objects().filter("id", Op::Eq, 1).fetch(&pool).await?[0].clone();
  let alice: &Author = book.author.get(&pool).await?;   // lazy-load + cache
  ```
- **State machine.** Just-decoded rows hold `ForeignKey::Unloaded(pk)` (sqlx `Decode` reads `BIGINT`); the first `.get(&pool)` swaps to `ForeignKey::Loaded { pk, value }` with a `Box<T>` cache, so subsequent `.get()` calls are zero-SQL. Constructors: `ForeignKey::unloaded(pk)`, `ForeignKey::loaded(pk, parent)`, plus `From<i64>` for `pk.into()`.
- **Write path.** `From<ForeignKey<T>> for SqlValue` extracts the PK regardless of state, so INSERT / UPDATE on the parent row keeps writing the FK column as a plain `BIGINT` — no schema change for the FK column itself (DDL stays `BIGINT … REFERENCES …`).
- **Type-driven schema.** When the macro sees `ForeignKey<T>`, the generated `Relation::Fk { to, on }` reads `to` from `<T as Model>::SCHEMA.table` at compile time, so the user no longer has to repeat the table name in an attribute. `#[rustango(on = "user_uuid")]` still overrides the default `"id"` PK column. `Auto<ForeignKey<T>>` and `ForeignKey<T>` on a `#[rustango(primary_key)]` field are rejected with clear messages.
- **Macro hygiene.** The hidden `__rustango_cols_<Model>` submodule now opens with `use super::*;` so field types referencing sibling models (`ForeignKey<Author>` from inside `Book`'s codegen) resolve under the proc-macro derive resolution rules.
- **New `ExecError` variants**: `ForeignKeyTargetMissing { table, pk }` (FK pk not in target table — e.g. parent deleted under a non-CASCADE constraint) and `MissingPrimaryKey { table }` (target model has no PK; programming error).
- **v1 limitation**: target's PK must be `i64` (or `Auto<i64>`). `i32` PK targets and the rest of the type matrix are deferred until asked for.
- 5 unit tests in `crates/rustango-sql/src/foreign_key.rs` (constructors, `pk()`, `Into<SqlValue>`, `into_value`) plus 3 live tests in `crates/rustango/tests/foreign_key_live.rs` (round-trip lazy-load, `loaded()` constructor skips select, missing-target named error).

### Added — OR / nested-expr query filters (slice 4)

- **`WhereExpr` IR** in `rustango-core` — replaces the flat `filters: Vec<Filter>` field on `SelectQuery` / `UpdateQuery` / `DeleteQuery` / `CountQuery` with a `where_clause: WhereExpr` tree:
  - `WhereExpr::Predicate(Filter)` — leaf.
  - `WhereExpr::And(Vec<WhereExpr>)` — conjunction. Empty list = no `WHERE` emitted (the unfiltered default).
  - `WhereExpr::Or(Vec<WhereExpr>)` — disjunction. Empty list rejected at SQL-write time (`SqlError::EmptyOrBranch`) to avoid silently matching nothing.
- **`TypedExpr<M>`** in `rustango-core` — typed sub-expression with `.and()` / `.or()` combinators. `TypedFilter::and()` / `.or()` lift a single predicate into a `TypedExpr`; chaining is shallow-flattened so `a.and(b).and(c)` produces `And([a,b,c])` rather than `And(And(a,b),c)`.
- **`QuerySet::where_(impl Into<TypedExpr<T>>)`** — accepts both single `TypedFilter`s (existing v0.6 ergonomics) and composed `TypedExpr`s. Successive `.where_()` calls AND-join their arguments at the top level; OR is contained inside the expression argument:

  ```rust
  // (name = "alice" OR name = "bob") AND active = true
  Person::objects()
      .where_(Person::name.eq("alice").or(Person::name.eq("bob")))
      .where_(Person::active.eq(true))
      .fetch(&pool).await?;
  ```
- **Postgres writer** renders the tree precedence-aware: top-level expressions emit bare; nested composite children are parenthesized so `And(Predicate(a), Or(Predicate(b), Predicate(c)))` becomes `a AND (b OR c)` instead of the SQL-default-precedence-ambiguous `a AND b OR c`. Single-element AND/OR collapses to its child.
- **`WhereExpr::as_flat_and(&self) -> Option<Vec<&Filter>>`** — backwards-compat introspection for legacy AND-only WHERE clauses. Returns `Some(predicates)` only when the tree is a flat AND (or single predicate); returns `None` if any `Or` or nesting is present.
- **`WhereExpr::and_predicates(filters)`** — convenience constructor for the legacy "list of AND-joined predicates" shape, used by callers that build up a `Vec<Filter>` directly (admin pager, the `manage` CLI, downstream apps).
- **Migrated call sites**: `rustango-admin` (list pager + edit/update/delete row lookups), the macro-generated `delete()` / `save()` codegen, and ~30 tests across `sql.rs` / `queryset.rs` / `typed_columns.rs` / `validation.rs` now build `WhereExpr` instead of `Vec<Filter>`. The string-keyed `.filter("col", Op, val)` API is unchanged in shape.
- **New `SqlError::EmptyOrBranch`** variant. Raised when the writer encounters a `WhereExpr::Or(vec![])`.
- 5 live tests in `crates/rustango/tests/where_expr_live.rs`: two-branch OR matches either, OR-then-AND grouping, nested `(A AND B) OR C`, multiple `.where_()` calls keep AND'ing at top level, empty-OR rejected by writer.

### Added — README + demo close-out (slice 5)

- **README** updated end-to-end for v0.7:
  - New "Day-2 ORM ergonomics" bullet in **What's distinct** covering all four slice 1–4 additions.
  - **Field attributes** snippet now shows `ForeignKey<User>` (with the legacy `#[rustango(fk = "user", on = "id")] author_id: i64` form noted as still-supported).
  - **Query API** drops the "no `.or(...)` yet" caveat. Adds an OR / nested-expr example (`User::name.eq("alice").or(User::name.eq("bob"))`) plus a Postgres-grouping note.
  - **Per-instance** section grew a `save()` example showing `Auto::Unset` → INSERT then `Auto::Set(_)` → UPDATE dispatch, plus a `ForeignKey<T>::get` lazy-load snippet.
  - **Migrations** section gained a "Per-app ledger naming" subsection covering `migrate::Builder::new().ledger("__myapp__")` and the validation panic.
  - **Status** mentions v0.7's headline closures.
  - Cargo dep snippet bumped to `rustango = "0.7"`.
- **`crates/rustango/examples/v07_ergonomics_demo.rs`** — new ~150-line walk through all four v0.7 features against a fresh DB. `cargo run --example v07_ergonomics_demo` performs `save()` (INSERT then UPDATE), constructs a `ForeignKey<Author>`, lazy-loads it, runs an OR-then-AND query, runs a nested `(active OR (carol AND id > 0))` query, and configures two `migrate::Builder`s with distinct ledger names against the same database. Re-runnable; cleans up after itself.

### Notes

- v0.7 is content-complete on the working tree as of slice 5; ready for a `v0.7` release tag whenever the repo is ready to publish.
- The previous-version `[v0.6] — Unreleased` block stays as-is below: v0.6 was content-complete + close-outed but never tagged. A future release tag will cover both v0.6 and v0.7 in one go (or split, at the user's call).

## [v0.6] — Unreleased

Production-readiness for multi-tenancy. v0.5 shipped the headline (tenants as rows, no `DATABASES` dict); v0.6 fills the gaps that block real deployments: form-based login on both consoles, packaged bootstrap migrations, scope-aware `manage migrate`, hard-delete companion to soft-delete, and `is_superuser` gating in the tenant admin. Seven steps, all merged.

### Added — operator console (step 1)

- **`rustango_tenancy::operator_console`** module — form-based login + sidebar layout for the operator UI, independent of `rustango-admin`'s stock look. `GET /login`, `POST /login` (verifies via `authenticate_operator`), `POST /logout`, welcome page (`/`), read-only `/operators` and `/orgs` lists. Mutations stay on the CLI so side-effects (CREATE SCHEMA, migrations) happen atomically.
- **HMAC-SHA256 signed session cookies** — stateless `{operator_id, exp}` payload, no DB session table for v1. `RUSTANGO_SESSION_SECRET` env var (base64, ≥32 bytes); auto-generated random key with `tracing::warn` fallback if unset. Constant-time MAC verify via `subtle::ConstantTimeEq`. Open-redirect-safe `next=` sanitizer.
- **Embedded brand asset** — `rustango.png` baked into the crate via `include_bytes!`, served at `/__static__/rustango.png`.

### Added — interactive `manage` CLI + `.env` auto-load (step 2)

- **`tenancy_manage` example binary** — runnable in-repo via `cargo run --example tenancy_manage -p rustango-tenancy --`. Auto-bootstraps the registry on first run (`init-tenancy` + `migrate-registry` programmatically) so `run-server` / `create-operator` / `create-tenant` Just Work against a fresh DB.
- **TTY-gated interactive prompts** via `rpassword` (pinned to `=7.3.1`; 7.4+ uses Linux-only `__errno_location`):
  - `create-operator <username>` — prompts for username + password if absent.
  - `create-user <slug> <username>` — prompts for any of the three.
  - `create-tenant <slug>` — prompts for slug if absent.
  - `drop-tenant <slug>` — prompts for slug + retype-the-slug confirmation when `--confirm` is missing.
- **`dotenvy::dotenv()` at startup** — auto-loads `./.env` (or any ancestor); operators no longer re-export `DATABASE_URL` / `RUSTANGO_APEX_DOMAIN` / `RUSTANGO_SESSION_SECRET` each session. Non-TTY contract preserved (programmatic callers / piped scripts still get the original `Validation` errors).

### Added — `manage run-server` (step 3)

- **`rustango_tenancy::server::run`** — Django-style `runserver` for rustango. Boots operator console at the apex + tenant admin at every subdomain via host-based dispatch with sensible defaults: `RUSTANGO_BIND` (default `0.0.0.0:8080`), `RUSTANGO_APEX_DOMAIN` (default `localhost`). `--bind` / `--apex` argv overrides.
- Banner prints bound addr + URL pattern; pre-flight loud warning when `rustango_operators` is empty (operator UI would reject every login). Graceful shutdown via `tokio::signal::ctrl_c`.
- Aliases: `run-server` (primary) and `runserver` (Django muscle memory).

### Added — packaged bootstrap migrations (step 5)

- **`rustango_tenancy::bootstrap`** module — `init_tenancy(dir)` + `registry_bootstrap_migration()` / `tenant_bootstrap_migration()` factories build the bootstrap migrations in memory from `Org::SCHEMA` + `Operator::SCHEMA` + `User::SCHEMA` so they stay in sync with the model definitions automatically. UNIQUE constraints on slug/username land via raw `DataOp` SQL pending `#[rustango(unique)]`.
- **New `manage init-tenancy` verb** writes two scoped fixture migrations into the operator's migrations dir:
  - `0001_rustango_registry_initial.json` — `scope: registry`, creates `rustango_orgs` + `rustango_operators` with UNIQUE on slug / username.
  - `0001_rustango_tenant_initial.json` — `scope: tenant`, creates `rustango_users` with UNIQUE on username.
  Idempotent: existing files are reported as skipped.
- **New `manage migrate-registry` verb** — explicit registry-only sibling of the existing `migrate-tenants`.
- **`manage migrate` is now scope-aware** — applies registry-scoped migrations to the registry pool first, then fans out tenant-scoped migrations across active orgs. Pre-fix, the rustango-migrate fall-through was scope-blind and silently applied tenant migrations to the registry pool.
- **`create-tenant <slug>`** (without `--no-migrate`) actually migrates by default now — runs the packaged tenant bootstrap so `rustango_users` exists in the new schema out of the box.
- **`SchemaSnapshot::from_models(&[&ModelSchema])`** — new helper for assembling curated snapshots without going through the global inventory. `TableSnapshot::from_schema` is now `pub` for the same reason.

### Added — `purge-tenant` hard-delete (step 6)

- **New `manage purge-tenant <slug> --confirm <slug> [--purge-database]` verb** — symmetric companion to soft-delete `drop-tenant`.
  - **Schema mode**: `DROP SCHEMA "<slug>" CASCADE` against the registry pool, then `DELETE FROM rustango_orgs`. Idempotent w.r.t. an already-dropped schema (`IF EXISTS`).
  - **Database mode without `--purge-database`**: refuses with a loud error pointing at the flag. Org row stays put.
  - **Database mode with `--purge-database`**: invalidates the cached pool, resolves `Org.database_url` through the configured `SecretsResolver`, parses via `PgConnectOptions::from_str`, switches the connection to the `postgres` admin DB, runs `DROP DATABASE IF EXISTS "<dbname>"`, then deletes the Org row. System DBs (`postgres` / `template0` / `template1`) are refused — the operator can't accidentally drop the registry.
  - **Interactive confirmation**: TTY-gated retype-the-slug prompt with louder verb when `--confirm` is missing.
  - **Soft-deleted orgs purge cleanly** — natural progression after `drop-tenant`.
- **`TenantPools::resolved_database_url(org)`** — new public method that surfaces the secrets-resolved URL for `purge-tenant --purge-database` and any other admin-side use.

### Added — `is_superuser` admin gating for tenant users (step 7)

- **`rustango_admin::Builder::read_only_all()`** — new flag that toggles `Config.read_only_all`. `is_read_only(table)` returns true unconditionally when set, so callers don't have to enumerate every table to gate every mutation.
- **`rustango_tenancy::tenant_console`** module — tenant-side analog of `operator_console::session`. Cookie name `rustango_tenant_session`, payload `{ uid, slug, exp }`. The `slug` field binds the cookie to one tenant — `decode` returns `SessionError::WrongTenant` if the resolved org's slug doesn't match (defense in depth on top of browser subdomain isolation).
- **`TenantAdminBuilder::with_session(SessionSecret)`** — opt-in per-tenant auth. Without it, the v0.5 unauthenticated path remains. With it:
  - `GET /__static__/rustango.png`, `GET /__login`, `POST /__login`, `POST /__logout` are public.
  - Every other path requires a valid cookie. Anon → `303 → /__login?next=<sanitized-path>`.
  - Cookie validated → user looked up in `rustango_users` (fresh `is_superuser` + `active` per request).
  - **Superusers** get full read/write admin.
  - **Non-superusers** get an admin built with `read_only_all` — list/detail render, mutating routes 403, write-buttons hidden.
- **Shared `SessionSecret`** between operator + tenant consoles — `SessionSecret` is now `Clone`. Different cookie names + payload shapes keep the two domains isolated; one `RUSTANGO_SESSION_SECRET` covers both.
- **`tenant_login.html`** — centered login card, blue accent (distinct from operator's warm-rust), references the embedded `rustango.png`.
- **`server::run`** wires the same secret into both consoles automatically; `multitenant_demo` opts in via `with_session`.

### Changed

- **README multi-tenancy section** — added blockquote pointing both invocation shapes at each other (`--bin manage` for user projects, `--example tenancy_manage -p rustango-tenancy` for in-repo). Documents `init-tenancy`, `purge-tenant`, interactive prompts, and `.env` auto-load.
- **`Cargo.toml`** — new workspace deps: `hmac 0.12`, `sha2 0.10`, `subtle 2`, `cookie 0.18`, `rand 0.8`, `dotenvy 0.15`, `rpassword =7.3.1`, `serde_urlencoded 0.7`, `argon2 0.5`, `password-hash 0.5`. `tokio` features grew `signal` + `net`.
- **`SessionError`** gains a `WrongTenant` variant for cross-tenant cookie replay defense.
- **`tenancy_manage` example** dropped its first-run `CREATE TABLE IF NOT EXISTS` workaround; the bootstrap now goes through the migration ledger as it should.

### Notes

- **Known follow-up — scoped subset chain validation.** If a user later authors a registry-scoped migration whose `prev` points at the lex-greatest `0001_rustango_tenant_initial` (because `make_migrations` doesn't yet emit scope-aware `prev`), `migrate-registry`'s scoped subset will fail `validate_chain`. Acceptable v1 — registry-scoped user migrations are rare; a scope-aware `make_migrations` is the proper resolution.
- **`#[rustango(unique)]`** is still missing — bootstrap migrations carry UNIQUE constraints as raw `DataOp` SQL until that ~1-day add lands.
- **No revocation on session cookies** — once issued, a cookie is valid until `exp` (default 7 days). Operator delete / password change doesn't invalidate live cookies; secret rotation does. v2 can add a short-lived cookie + revocation list.

## [0.5.0] — 2026-04-29

Multi-tenancy, organizations-aware. The headline is the anti-Django footgun: **tenants are first-class rows in a `rustango_orgs` table, not entries in a config file**. Adding a tenant is one `INSERT` — no restart, no redeploy, no edit to a `DATABASES`-style dict. Seven slices, all merged.

### Added — new opt-in `rustango-tenancy` crate

Pulls in the facade `rustango` for `#[derive(Model)]` path resolution; the facade does NOT re-export tenancy (cycle would form). Users opt in with `rustango-tenancy = "..."` in their own `Cargo.toml`.

- **`Org` registry model** — `slug` (globally unique), `display_name`, `storage_mode` (`schema`/`database`), `database_url` (secret reference), `schema_name`, `host_pattern`, `port`, `path_prefix`, `active`, `created_at`. Adding a tenant = `INSERT INTO rustango_orgs (...)`. (Slice 1)
- **`OrgResolver` async trait** + 5 built-in impls: `SubdomainResolver`, `PathPrefixResolver`, `HeaderResolver`, `PortResolver`, `ChainResolver`. `ChainResolver::standard(apex)` = `[Subdomain, Header]` — subdomain-first by design (cookie isolation by browser policy). Apex (`app.com` without subdomain) returns `Ok(None)` so `/operator/*` can bypass cleanly. (Slice 2)
- **`TenantPools`** — lazy connection registry. Schema-mode tenants share the registry pool with per-checkout `SET search_path`; database-mode tenants get a dedicated pool, lazy-built and cached in a bounded `RwLock<HashMap>` (default cap 64; cache full → clear `Validation` error, no silent eviction). `acquire(&Org) -> TenantConn` is the only sanctioned access path; `invalidate(slug)` drops a cached pool for vault rotation. (Slice 3)
- **`SecretsResolver`** — pluggable indirection so `Org.database_url` can be a vault reference instead of a literal connection URL. Defaults: `LiteralSecretsResolver` (pass-through), `EnvSecretsResolver` (`env://VAR_NAME`), `ChainSecretsResolver` (scheme-keyed). Vault backends slot in by implementing the trait; no API churn when vault crates land. (Slice 3.5)
- **Scoped migrations** — `Migration.scope: MigrationScope` field (`Tenant` default, `Registry` opt-in). `migrate::migrate_registry(pools, dir)` runs registry-scoped migrations against the registry pool; `migrate::migrate_tenants(pools, dir, registry_url)` walks active orgs and applies tenant-scoped migrations to each. Per-schema ledger (`<schema>.__rustango_migrations__`) for schema mode; per-DB ledger for database mode. Per-tenant failure isolation via `TenantMigrationReport`. (Slice 3)
- **`TenantAdminBuilder`** — wraps `rustango_admin` with per-request resolver dispatch. Same `show_only` / `read_only` API; mounts under any prefix via `Router::nest`. Database-mode tenants serve through cached `Arc<PgPool>`; schema-mode tenants get a short-lived per-request pool with `after_connect` running `SET search_path`. **Cross-tenant isolation proven** in tests: same admin URL serves acme's data when `X-Org: acme` and globex's when `X-Org: globex`, no leakage. (Slice 4)
- **`manage::run`** — single dispatcher for tenancy + standard subcommands. New verbs: `create-tenant <slug>` (with `--mode`/`--display-name`/`--database-url`/`--schema-name`/`--host-pattern`/`--port`/`--path-prefix`/`--no-migrate`), `drop-tenant <slug> --confirm <slug>` (soft-delete; double-typed-slug guard against typos), `list-tenants` (table format), `migrate-tenants` (per-tenant report). Anything else delegates to `rustango_migrate::manage::run` against the registry pool. Defaults `host_pattern` to `<slug>.<RUSTANGO_APEX_DOMAIN>` matching the locked subdomain-first design. (Slice 5)
- **2-domain auth — `Operator` + `User`** with hard wall. `Operator` lives in the registry's `rustango_operators` and signs in at `/operator`; `User` lives in the tenant's `rustango_users` (schema or DB) with an `is_superuser` flag for org-admin within that tenant. **Operator credentials never authenticate against a tenant; tenant user credentials never authenticate as an operator** — proven in tests. Argon2id PHC hashing via `password::hash` / `password::verify`. `authenticate_operator(&PgPool, ...)` and `authenticate_user(&mut PgConnection, ...)` both collapse "wrong pw / unknown / inactive" into one `Ok(None)` return path so there's no timing oracle on whether the username exists. `parse_basic_auth` decodes `Authorization: Basic`. New manage verbs: `create-operator <user> --password <p>` and `create-user <slug> <user> --password <p> [--superuser]`. (Slice 6)
- **`examples/multitenant_demo/`** — three tenants on `*.localhost`, mixed storage modes, end-to-end provision → migrate → admin walkthrough. (Slice 7)

### Changed

- **`Migration` JSON format** gains an optional `scope: "registry" | "tenant"` field (default `Tenant`, `skip_serializing_if = is_default`). v0.4 migrations missing the field deserialize as `Tenant` and behave identically.
- **`SqlValue::Null` parameters now carry typed Postgres casts** (`$N::INTEGER`, `$N::TEXT`, etc.) when the column's `FieldType` is known to the writer. Fixes a pre-existing bug where `None::<String>` was bound for every NULL, breaking nullable integer / bool / timestamp columns. Surfaced by Org's `Option<i32> port` field; the cast threads through `compile_insert`, `compile_bulk_insert`, `compile_update`, `compile_count`, `compile_select`, and the WHERE/search clauses.
- **`TenancyError` enum** now carries `Resolution`, `Validation`, `Secrets(SecretsError)`, `Migrate(MigrateError)`, `Exec(ExecError)`, `Driver(sqlx::Error)`, `Io(std::io::Error)`.

### Notes

- **What's NOT in v0.5**: session middleware / cookies / login forms (slice 6 ships HTTP Basic + the parser; the wiring is a v0.6.x follow-up); `purge-tenant` hard-delete (`DROP SCHEMA` / `DROP DATABASE`) — too footgun-y for slice 5; bootstrap migrations packaged with rustango-tenancy (operators currently CREATE TABLE manually for `rustango_operators` / `rustango_users` or use `apply_all` on a fresh DB).
- **Schema-mode admin per-request cost**: builds a short-lived `PgPool` per request with `after_connect` setting `search_path`. Real cost; v0.6 may move to a connection-level model. Database-mode is free (cached pool).
- **Apex routing** (subdomain-first design): bare `app.com` does not resolve to a tenant. Operator UI lives at `app.com/operator/*`; everything else under the apex returns 404. `*.localhost` works for local dev without DNS infra.

## [0.4.0] — 2026-04-28

ORM ergonomics + migration tooling — closes the day-2 gaps surfaced by the [Cot](https://cot.rs) and [Loco](https://loco.rs) framework comparisons (see `memory/framework-landscape.md` in the dev memory). Six slices, all merged.

### Added
- **`Auto<T>` server-assigned PK wrapper.** `id: Auto<i64>` → `BIGSERIAL`; `Auto<i32>` → `SERIAL`. `Auto::default()` lets the database fill the value via the sequence; `Auto::Set(v)` honors a caller-supplied value. `&mut self.insert(&pool)` reads the assigned id back through `RETURNING` and stores it in place. Re-exported as `rustango::Auto`. (Slice 1)
- **`Model::bulk_insert(rows, &pool)`** — multi-row INSERT, one round-trip for N rows. Non-Auto models take `&[Self]`; Auto-bearing models take `&mut [Self]` and populate each row's PK from `RETURNING` in input order. Mixed `Auto::Set`/`Auto::Unset` within one batch is rejected (`SqlError::BulkAutoMixed`) — column lists must be uniform; use single-row `insert` for that case. (Slice 2)
- **`AlterField` + `Rename` operations.** Six new `SchemaChange` variants — `AlterColumnType`, `AlterColumnNullable`, `AlterColumnDefault`, `AlterColumnMaxLength`, `RenameTable`, `RenameColumn` — with full render, invert, and (for the four alters) autodetection in `detect_changes`. Renames are not auto-detected (rename vs drop+add is ambiguous, same Django reasoning); author them via `manage makemigrations --empty <name>` and edit the JSON. The v0.3.1 polish #3 hard-error narrows to PK / min / max / FK / Auto add-remove changes, which still need a follow-up slice. (Slice 3)
- **`manage migrate --dry-run`** + **`migrate::migrate_dry_run(pool, dir) -> Vec<MigrationPreview>`**. Print every DDL/DML statement the next `migrate` would run, without executing any of it. Reads the ledger so the preview reflects the actual pending set. Atomic migrations show synthetic `BEGIN`/`COMMIT` markers; the ledger INSERT is included verbatim. **No other Django-shape Rust framework has this** — Cot and Loco can't, and Django's `sqlmigrate` only previews one migration at a time. (Slice 4)
- **Compile-time `embed_migrations!` chain validation.** The proc-macro now reads each JSON at expansion time, parses out `name` and `prev`, and emits a `compile_error!` for any broken chain, orphan predecessor, file-stem-vs-name mismatch, or malformed JSON. **The only Rust ORM where a broken migration set fails to compile** — Cot's migrations are imperative Rust code with no static chain to validate, Loco's are SeaORM up/down (same), Rwf's are raw SQL. (Slice 5)

### Changed
- **`InsertQuery` gains `returning: Vec<&'static str>`.** Empty (default) preserves existing behavior; non-empty triggers `RETURNING` emission and the new `executor::insert_returning` path.
- **`SchemaChange` enum becomes non-exhaustive in spirit** — six new variants land. JSON migration files written by v0.3 still parse; the new variants only appear when authors hand-write them or when `make_migrations` detects metadata changes.
- **`detect_unsupported_field_changes` narrows** to PK / min / max / FK / Auto add-remove. Type / nullable / default / max_length now produce `AlterColumn*` ops via `detect_changes` rather than the v0.3.1 hard error.

### Removed (effective for users hitting the v0.3.1 hard error)
- The "field metadata changed but v0.3 has no AlterField operation" error message no longer fires for type / nullable / default / max_length changes — those are real ops now.

### Documentation
- README headline snippet now shows `id: Auto<i64>` + the in-place insert pattern. New "What's distinct" section calls out the four genuine differentiators against Cot/Loco/Rwf — registry-driven admin, JSON migrations, `migrate --dry-run`, and interleaved `DataOp`/`SchemaChange`.

## [0.3.1] — pre-release

Hardening pass merged into the v0.4 unreleased section above. Originally:

- Concurrent-migrate `pg_advisory_lock`.
- Prev-chain validation in `file::list_dir` + `migrate_embedded` (slice 5 of v0.4 promoted this to compile-time for `embed_migrations!`).
- Metadata-change detection (slice 3 of v0.4 turned the hard error into real ops for the common cases).
- `unapply` head check + `unapply_force` escape.
- `tracing::info!` at apply/unapply boundaries.
- `manage::run_with_writer` for capturable output.

## [0.3.0] — 2026-04-28

On-disk migration files, autogeneration from registry diffs, ledger-tracked apply/rollback, and a Django-style `manage.py` analog. Slices 0-7 of the v0.3 plan; slice 8 (Rust callbacks) deferred.

### Added
- **`#[rustango(default = "...")]` attribute** for column DEFAULTs. Required when adding a non-null column to an existing table — Postgres needs the default to backfill rows. Verbatim Postgres expression: numeric literal, quoted SQL string, function call, etc. (Slice 0)
- **On-disk migration file format** — JSON, one file per migration, lex-sortable name (`0001_initial.json`). Carries the migration name, RFC3339 timestamp, optional `prev` predecessor, `atomic` flag, full `SchemaSnapshot` at this point, and a flat `forward: Vec<Operation>` list interleaving `Schema(SchemaChange)` and `Data(DataOp)` ops. `DataOp` pairs forward `sql` with `reverse_sql` (or `reversible: false` for one-way migrations). (Slice 1)
- **`migrate::make_migrations(dir, name)`** — diff the inventory registry against the latest snapshot, write the next migration file. Auto-derives names: `initial` for the first run; `create_X` / `drop_X` / `add_C_to_T` / `drop_C_from_T` for single-shape changes; `auto` otherwise. `name` overrides. `make_migrations_from` exposes the testable form taking the snapshot as input. (Slice 2)
- **`__rustango_migrations__` ledger table** + **`migrate::migrate(pool, dir)`** runner. Each pending migration applies in its own transaction by default (per-file `atomic: false` opts out). `ensure_ledger`, `applied_set` are public for callers that want their own runners. (Slice 3)
- **`invert::invert`** computes the inverse op list from a forward op list + predecessor snapshot; **`migrate::unapply(pool, dir, name)`** rolls back a single migration. Schema reversal uses the snapshot at the predecessor; data reversal uses the migration's `reverse_sql`. Irreversible migrations fail fast before any DB write. (Slice 4)
- **`migrate::migrate_to(pool, dir, target)`** walks forward or back to a named migration in lex order. `target == "zero"` unapplies everything. **`migrate::downgrade(pool, dir, n)`** steps back the `n` most recent. (Slice 5)
- **`migrate::manage::run(pool, dir, args)`** Django-style dispatcher. Subcommands: `makemigrations [name]`, `makemigrations --empty <name>`, `migrate`, `migrate <target>`, `downgrade [N]`, `showmigrations` / `status`, `--help`. Users drop a `src/bin/manage.rs` 5-line entrypoint to wire it up. (Slice 6)
- **`embed_migrations!("./migrations")`** proc-macro + **`migrate::migrate_embedded(pool, &[(name, json)])`** runner. The macro `include_str!`s every `*.json` in the directory at compile time and emits `&[(&'static str, &'static str)]` so single-binary deployments don't ship a migrations folder alongside the binary. (Slice 7)

### Fixed
- FK `ALTER TABLE` constraints emitted by `CreateTable` are now **deferred to the end of a migration's forward execution**, so two `CreateTable` ops in one migration where one FKs the other no longer fail because the referenced sibling table doesn't yet exist. `RenderedBatch::deferred_fks` exposes this split for callers. ([b2b9334](https://github.com/ujeenet/rustango/commit/b2b9334))

### Notes
- The `embed_migrations!` macro relies on `include_str!`, and **cargo doesn't watch directory listings** — adding or removing a migration file requires `cargo clean` to refresh the bake. Real footgun in active development.
- `DROP TABLE … CASCADE` is the default for `DropTable`. Not configurable; cascades to dependent FKs and views silently.
- Schema reversal restores **shape, not data** — `DropColumn` then `unapply` does not bring back the column's row values.
- Slice 8 (Rust callbacks for data migrations) was descoped from v0.3.

## [0.2.0] — 2026-04-27

Schema snapshots, diff/render to DDL, admin polish.

### Added
- **`SchemaSnapshot` IR** capturing every registered model's table + column metadata as JSON. Round-trips through serde for migration files.
- **`diff::detect_changes(prev, current)`** computes a `Vec<SchemaChange>` (CreateTable / DropTable / AddColumn / DropColumn) from two snapshots. **`diff::render_changes`** writes the changes as Postgres DDL. Type/nullability/PK/CHECK/FK changes were silently dropped — fixed in v0.3.1. (Slice 5)
- **Admin LEFT JOIN support** — list views render FK columns as `<a href="/<target>/<pk>">display_value</a>` using a single LEFT JOIN per FK column at query time. No N+1. (Slice 4)
- **Admin Tera templating** — list / detail / new / edit / delete pages baked from a templates directory via `include_str!`. (Slice 3)
- **Admin search and per-field filters.** `?q=foo` substring search across `max_length` String fields; `?<column>=v` per-field filter; both compose with `?page=N` pagination. (Slice 2)
- **Admin renders FK columns as links** to the target row's `display` field. (Slice 1)
- README and `repository` Cargo metadata for crates.io publish prep. (Slice 6)

## [0.1.0] — 2026-04 (pre-history)

Initial workspace scaffolding through the first usable axe of the framework.

### Added
- **Workspace scaffolding** — 7-crate split: `rustango-core` (IR, registry traits), `rustango-macros` (`#[derive(Model)]`), `rustango-query` (typed `QuerySet<T>`), `rustango-sql` (Postgres writer + executor), `rustango-migrate` (DDL writer + bootstrap runner), `rustango-admin` (auto-CRUD over the registry), `rustango` (facade re-exports).
- **`#[derive(Model)]`** populates `inventory` for the registry-driven admin. `#[rustango(table = "...")]`, `#[rustango(primary_key)]`, `#[rustango(column = "...")]`, `#[rustango(fk = "...", on = "...")]`, `#[rustango(o2o = "...", on = "...")]`, `#[rustango(display = "...")]`.
- **`User::objects()`** typed `QuerySet`. Per-field zero-sized types in a hidden module (`User::id`, `User::name`) carry `Column` impls with `Eq`/`Ne`/`Lt`/`Lte`/`Gt`/`Gte`/`Like`/`In`/`IsNull` ops. Both typed (`User::id.eq(10)`) and string-keyed (`.filter("id", Op::Eq, 10)`) forms exist; mix freely.
- **INSERT / UPDATE / DELETE** — IR, Postgres writers, executors, per-instance `insert(&pool)` / `delete(&pool)` derived methods. Bulk via `User::objects().filter(...).update().set(...).execute(&pool)` and `.delete(&pool)`.
- **Per-field bounds** — `#[rustango(max_length = N)]`, `#[rustango(min = N, max = M)]` translate into VARCHAR length, CHECK constraints, and pre-DB validation in `validate()`.
- **LIMIT / OFFSET, COUNT, admin pagination, HTTP Basic auth** for the admin.
- **Admin CRUD** — list, detail, new, edit, delete forms over registry models.
- **`rustango-admin`** auto-CRUD router over the inventory registry. Zero per-model wiring — every derive shows up.
- **Postgres DDL writer** in `rustango-sql` + **`migrate::apply_all(&pool)` / `migrate::drop_all(&pool)`** for fresh-DB bootstrap.

[Unreleased]: https://github.com/ujeenet/rustango/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/ujeenet/rustango/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/ujeenet/rustango/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ujeenet/rustango/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ujeenet/rustango/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ujeenet/rustango/releases/tag/v0.1.0
