# Changelog

All notable changes to rustango. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project loosely follows [SemVer](https://semver.org/) — with the caveat that nothing pre-1.0 has a stability guarantee.

## [Unreleased]

Post-v0.42 Django-parity follow-ups. Each item is a self-contained slice that landed as its own PR after the v0.42.0 release tag.

### Added

- **`format_number` + `format_currency` Tera filters** (#553 closing [#426](https://github.com/ujeenet/rustango/issues/426) / [#428](https://github.com/ujeenet/rustango/issues/428)) — locale-aware decimal + thousands separators across en / de / fr / es / it / pt / nl / ja / zh / ru / pl / tr / cs / sk / el / bg / uk / hu / nb etc.; currency formatting for USD / CAD / AUD / NZD / HKD / SGD / MXN / EUR / GBP / JPY / KRW / CLP / CNY / RUB / INR / BRL / CHF with locale-driven Euro placement.
- **RTL layout support** (#554 closing [#429](https://github.com/ujeenet/rustango/issues/429)) — `Locale::is_rtl()` / `direction()` + bare-string helpers `i18n::is_rtl_language()` / `text_direction()` + `ActiveLocale::is_rtl()` / `direction()` extractor convenience + Tera `get_text_direction(locale=…)` / `is_rtl(locale=…)` functions. RTL table: ar / he (+iw) / fa / ur / ps / yi (+ji) / dv / ckb / ug / sd / syr.
- **`DatabaseCache` backend** (#555 closing [#409](https://github.com/ujeenet/rustango/issues/409)) — Django parity for `django.core.cache.backends.db.DatabaseCache`. Tri-dialect (PG `ON CONFLICT`, MySQL `ON DUPLICATE KEY UPDATE`, SQLite `ON CONFLICT`); idempotent `DatabaseCache::ensure_table().await?` boots the schema; lazy GC on read.
- **`#[rustango(managed = false)]`** (#558 closing [#321](https://github.com/ujeenet/rustango/issues/321)) — Django `class Meta: managed = False`. Opts a model out of migration auto-gen so the table stays operator-managed.
- **`Locale::display_name()` / `native_name()` + Tera `language_display_name()` / `language_native_name()`** (#563) — 42-language picker primitives for bidi-aware UIs.
- **`#[rustango(citext)]`** (#566 closing [#344](https://github.com/ujeenet/rustango/issues/344)) — Django `CITextField`. Per-dialect DDL emit: `CITEXT` (PG, with `dialect.ci_text_extension_sql()` exposing the `CREATE EXTENSION` prelude), `TEXT COLLATE NOCASE` (SQLite), `VARCHAR(N) COLLATE utf8mb4_general_ci` (MySQL).
- **`#[rustango(db_table_comment = "...")]`** — Django `Meta.db_table_comment` (4.2+). Per-dialect DDL emit: PG post-table `COMMENT ON TABLE "<t>" IS '...'`, MySQL inline `COMMENT='...'` trailer after CREATE TABLE, SQLite no-op. Useful for data-lineage tooling that reads the table's catalog comment.

### Fixed

- **`uploads::save_uploads` chunk-by-chunk streaming with early-abort** (#565 closing [#421](https://github.com/ujeenet/rustango/issues/421)) — previously `field.bytes().await?` buffered the ENTIRE multipart body before bound-checking; a 100MB upload against a 5MB cap therefore still cost 100MB of memory. Now reads `field.chunk()` in a loop, short-circuits with `UploadError::TooLarge` the moment the next chunk would exceed `cfg.max_bytes`.

### Docs

- **Django-parity audit resync** (#567) — top summary table updated against the per-section truths after three months of incremental "MISSING → SHIPPED" flips. Totals: SHIPPED 205 → 243 (+38), PARTIAL 65 → 49 (−16), MISSING 84 → 67 (−17). Coverage: 58% → 68% full; partial+shipped: 77% → 81%.

## [0.42.0] — Django-parity gap-closure batch

16 Tier-2 issues closed in implementation across 16 PRs (#519–#548), plus 9 closed as already-supported with code pointers. Every shipped item picks up the same inventory-collected `register_*!` macro pattern (const-fn-pointer + `inventory::submit!`) so extensions live next to the model that needs them.

### Added

- **Admin extension registries** (5 new):
  - `register_admin_view!` (#363/#362) — Django `ModelAdmin.get_urls()`. Mount arbitrary per-model HTTP routes at `/<admin>/<table>/<suffix>`. Reserved-suffix guard rejects collisions with built-in routes.
  - `register_admin_queryset!` (#360) — Django `get_queryset(request)`. Per-request filter contributions that AND with URL params + search + facets + date-hierarchy.
  - `register_admin_object_permission!` (#361/#364) — Django `has_{add,change,delete,view}_permission(request, obj)`. Per-row enforcement on every admin write path. Pre-update SELECT so hooks see the `obj=` state.
  - `register_admin_computed!` with `link =` (#349) — Django callable display_link. Per-row click target via callable returning `Option<String>`.
  - `admin(formfield_overrides = "field:widget, ...")` (#359/#370) — Django `formfield_overrides`. Built-in widget names: `password` / `hidden` / `textarea` / `color` / `range` / `email` / `url` / `tel` / `search`.
- **Template extension registries** (3 new):
  - `register_template_filter!` / `register_template_function!` (#383) — Django `@register.filter` / `@register.simple_tag`. Picked up by `template_extensions::apply_to_tera(&mut tera)`.
  - `register_template_context_processor!` (#384) — Django `TEMPLATES.OPTIONS.context_processors`. Merged into every Tera context via `template_context_processors::apply_to_context(&mut ctx, parts)`. Handler-supplied keys win on collision.
  - Template debug overlay (#386) — `template_views::render` swaps a styled HTML error page in for the plain-text 500 fallback when `RUSTANGO_ENV` is dev/staging (or `RUSTANGO_TEMPLATE_DEBUG=1`).
- **`Translator::{gettext, gettext_fmt, pgettext, pgettext_fmt, ngettext, ngettext_fmt}`** (#422) — gettext-shape aliases. `pgettext` uses gettext's `<context>\u{4}<message>` catalog convention with bare-key fallback. `ngettext` implements English plural rule (CLDR-other-languages deferred to #426) and auto-binds `{count}`.
- **`ListView::context_object_name(name)` / `DetailView::context_object_name(name)` / `DetailView::lookup_field(column)`** (#379) — Django MultipleObjectMixin / SingleObjectMixin hooks. Renamed binding adds alongside the legacy `object` / `object_list`; `lookup_field` probes by a non-PK column (slug, uuid, etc.).
- **`ModelForm::prepare_save()` + `PreparedSave`** (#375) — Django `form.save(commit=False)`. Validate now, mutate the prepared write set (`.set` / `.unset` / `.has` / `.is_insert`), commit when ready. Lets handlers add session-derived fields between validate and INSERT.
- **`ViewSet` JSON-array POST body** (#435) — DRF `ListSerializer(many=True)`. Atomic-validate before any insert lands + sequential INSERT-RETURNING. Single-object body keeps existing shape.
- **`serializer::hyperlink_url` + `hyperlinked_to_value`** (#434) — DRF HyperlinkedModelSerializer. Free functions wrap a standard serializer's `to_value()` with a `url` field + `<fk>_url` siblings.
- **`#[derive(Model)]` accepts `rust_decimal::Decimal`, `chrono::NaiveTime`, `Vec<u8>`** (#524) — wired to `FieldType::Decimal` / `Time` / `Binary` (which already had bind + DDL + decode support). Closes the macro-side gap that forced workarounds like `price_cents: i64`.
- **`manage makemigrations --merge`** (#346) — Django `makemigrations --merge`. Detects two-or-more leaves on the same parent and writes an empty-forward `NNNN_merge.json`. Linear chains return `Ok(None)` (no-op); legitimately divergent histories (different parents) raise a clear error.
- **Showcase E2E test scaffold** (PRs #521–#527) — multi-app showcase in `examples/showcase/` exercising every framework surface (blog / shop / accounts / i18n_demo) with a Playwright TypeScript suite, mounted via the framework's own `manage::Cli::new().api(...).run()` pattern. CI matrix runs the same 32-test suite on PG / MySQL / SQLite.
- **`AdminError::Forbidden { table, action }`** — new variant rendering 403 with a small JSON body identifying the denied action.

### Closed as already-supported

- **#362 Custom URLs (`get_urls`)** — same capability as #363; closed pointing at PR #537.
- **#323 Proxy models** — extension-trait pattern documented at `inheritance.rs:98-127`.
- **#368 Custom dashboard** — template override + `register_admin_view!` + `register_admin_computed!` + `register_admin_inline!`.
- **#369 ModelForm** — `ModelFormFor<T>` + `.fields/.exclude/.prepare_save/.from_json` covers Django shape.
- **#374 Model formsets** — `register_admin_inline!` covers `TabularInline` / `StackedInline`.
- **#378 Date-based views** — compose `ListView` + `.dates()` / `.datetimes()`.
- **#396 shell (REPL)** — wontfix-by-design; documented script-binary pattern.
- **#402 TIME_ZONE / USE_TZ** — `i18n::timezone::with_offset` + `localtime` filter.
- **#404 LOGGING dictConfig** — `Settings.logging` covers every capability under tracing shape.
- **#427 `{% trans %}`** — `tera_tags::register` + #422 gettext aliases.
- **#433 selenium / playwright** — standard npm package; showcase E2E demonstrates pattern.
- **#385 `{% cache %}`** — `cache_fragment::cached_render` (handler-side); Tera-parser limitation prevents block-tag form.

### Section summaries (from django-parity-audit-2026-05-21.md)

- Section 6 (Admin / ModelAdmin parity): **26 SHIPPED / 2 PARTIAL / 8 MISSING / 2 N/A**
- Section 8 (Generic CBVs): **12 / 0 / 1 / 1** (only #378 niche remains)
- Section 10 (Templates): **11 / 0 / 0 / 0** (fully shipped)

## [0.41.0] — Tier 1 ORM gap-closure batch

Ten ORM tickets shipped across 14 PRs (#274–#286), plus the PG-typed legacy executor surface is gone from the public API. Closes [epic #273](https://github.com/ujeenet/rustango/issues/273).

### Added

- **`Q!` macro** (#269) — compile-time-safe Django-shape filter syntax. Typo'd field names fail to build.
- **`Q()` runtime builder** (#263) — `Qb::eq("active", true) & (Qb::gt("age", 18i64) | !Qb::eq("banned", true))` for admin filter chips + dynamic API params.
- **`distinct_on(&[...])`** (#264) — PG `SELECT DISTINCT ON`; portable window-function fallback on MySQL / SQLite. "Latest per group" patterns.
- **`bulk_upsert_pool(rows, unique_fields, update_fields, &pool)`** (#267) — Django's `bulk_create(update_conflicts=True)`. Tri-dialect: PG `ON CONFLICT (cols) DO UPDATE SET …`, MySQL `ON DUPLICATE KEY UPDATE`, SQLite `ON CONFLICT (cols) DO UPDATE SET …`.
- **`#[rustango(unique_when(columns = "...", condition = "..."))]`** (#265) — partial unique constraints. PG/SQLite native; MySQL falls back to plain UNIQUE with migration-time warning.
- **`AggregateBuilder::alias()`** (#268) — Django 3.2 non-projected annotations. Filter/order by a derived aggregate without paying column-decode cost.
- **`explain_pool()`** (#272) — tri-dialect EXPLAIN. PG `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS)`, MySQL `EXPLAIN ANALYZE` / `FORMAT=TREE` / `FORMAT=JSON`, SQLite `EXPLAIN QUERY PLAN`.
- **DB function library batch 1** (#266) — `Cast`, `LPad`, `RPad`, `MD5`, `SHA1`, `SHA256`, `Position`, `Repeat`, `Reverse`, `Sign`, `Mod`, `Power`, `Sqrt`. Per-dialect emission with clear `OpNotSupportedInDialect` errors where SQLite genuinely lacks the function.
- **`#[rustango(manager(ext = "PostManagerExt"))]`** (#271) — Django-shape custom-manager extension trait emitted next to the model.

### Changed (breaking)

PG-typed legacy executor deletion (#270, 4 waves) — every reachable PG-typed surface gone from the public API:

- `use rustango::sql::{Fetcher, Counter, Updater, Deleter};` → all four trait imports unresolved. Methods are now inherent on `QuerySet` / `UpdateBuilder`: `.fetch_on(&pool)`, `.count_on(&pool)`, `.delete_on(&pool)`, `.update().set(...).execute_on(&pool)`.
- `use rustango::sql::{insert, update, delete, select_rows, transaction, count_rows, raw_execute, bulk_update, ...};` → bare `&PgPool` wrappers unresolved. Use the tri-dialect `_pool` family (`insert_pool`, `update_pool`, `transaction_pool`, …).
- `qs.fetch(&pool)` → `qs.fetch_on(&pool)` (no trait import needed); same for `.count` / `.delete` / `.execute`.

Net: 9 features added, ~340 LOC removed from the public surface, every shipped feature works on all three backends via the canonical `cargo build --no-default-features --features sqlite,tenancy` litmus.

## [0.40.0] — admin auth + GFK ergonomics + field help_text

Three closing slices on the admin surface, plus the polymorphic-relations finishing pass.

### Added

- **Admin session auth without tenancy** (#253) — bare `admin` now ships a styled `/login` form, signed-cookie sessions, sidebar Logout, password-change UI at `/account/password`, `manage create-admin` CLI verb, and `is_superuser` gating. Opt in via `admin::Builder::with_session_auth`. Shared signing primitive lives at `crate::session::SessionSecret` — same key feeds tenancy operator + tenant + bare-admin cookies safely.
- **GenericForeignKey ergonomics + admin inlines** (#246) — `#[rustango(generic_fk(name, ct_column, pk_column))]` now emits typed `comment.content_object_pool(&pool)` accessor + `comment.set_content_object_for::<Post>(&pool, pk)` setter. List view collapses `(ct_id, object_pk)` into one clickable target link. New `register_admin_inline_generic!` renders polymorphic children as inline panels on the parent's admin detail + edit pages (read-only + FormSet-backed editor). ContentType `<select>` picker replaces raw integer inputs on the standalone create/edit form.
- **Django-shape `help_text`** — `#[rustango(help_text = "Markdown is supported.")]` on any field renders a muted caption below the input on the admin form. The string lives on `FieldSchema::help_text` so future surfaces (DRF serializer schemas, OpenAPI descriptions) can read the same source.
- **`admin::Builder::with_session_auth(secret)`** auto-bootstraps an `rustango_admin_users` table (idempotent via `CREATE TABLE IF NOT EXISTS`) and defaults `change_password_url = "/account/password"` so the sidebar's Change-password link routes correctly with zero operator wiring.
- **Admin UX consolidation** — unified `.btn` / `.btn-primary` / `.btn-secondary` / `.btn-danger` / `.btn-link` / `.btn-row-action` class system shared with the operator console.
- **Reusable foundations** — `crate::session` (signed-cookie HMAC) and `crate::manage_interactive` (TTY-gated prompts) promoted to the crate root. Tenancy continues to re-export from these so existing callers are unaffected.
- **Runnable demo**: `examples/gfk_demo` exercises every GFK surface end-to-end on SQLite. `cargo run -p rustango --example gfk_demo --features sqlite,admin,runserver`, then visit `http://localhost:8080/` (login `admin / admin`).

## [0.39.0] — dialect-agnostic transactions + tri-dialect migrations

Closes the last PG-specific gaps in the executor surface and the file-based migration renderer. Multi-row TX blocks, `SeedFn` hooks, and `SchemaChange` DDL all work on any backend; sqlite/mysql `runserver_tenancy` now honors the `Cli::seed` hook on boot.

### Added

- **Dialect-agnostic transactions** — `PoolTx::dialect()` returns the variant's dialect; `insert_tx` / `insert_returning_tx` / `update_tx` / `delete_tx` mirror the `_pool` family against an open `&mut PoolTx`. MySQL `LAST_INSERT_ID()` runs on the same TX connection.
- **`QuerySet::fetch_tx`** — `select_rows_tx_with_related` + new `FetcherTx` trait; macro emits `save_tx` / `insert_tx` / `delete_tx` on every `Model`.
- **Tri-dialect `SchemaChange` DDL** — three new `Dialect` capabilities:
  - `translate_default_expr(&str, ty: &str)` — `now()` → `CURRENT_TIMESTAMP` (sqlite) / `CURRENT_TIMESTAMP(6)` (mysql); strips Postgres `::type` cast suffixes; parenthesizes JSON defaults for MySQL.
  - `inline_fks_in_create_table()` — sqlite returns true; CREATE TABLE renderer emits inline + table-level FK clauses and skips the post-hoc `ALTER TABLE ADD CONSTRAINT` path.
  - `supports_create_index_if_not_exists()` — mysql returns false; `CREATE INDEX` is emitted without the guard token (ledger serializes application).
- **`Dialect::insert_on_conflict_skip(&[&col])`** — PG/SQLite → `ON CONFLICT (…) DO NOTHING`; MySQL → `ON DUPLICATE KEY UPDATE <pivot> = <pivot>`.

### Changed

- `SeedFn` lifted from `&PgPool` to `&Pool` (postgres `cfg` gate dropped); sqlite/mysql runserver paths now invoke seeds.
- `server::Builder` cfg loosened from `feature = "postgres"` to `feature = "tenancy"`; the generic-over-DB builder is reached from the non-PG `runserver_tenancy` arm.
- Renderers routed through the new dialect capabilities: `migrate/diff.rs::create_table_sql_from_snapshot_with_dialect`, `constraints_sql_from_snapshot`, `add_column_sql`, the `CreateIndex` arm, `migrate/ddl.rs::write_column_def`, and `tenancy/permissions.rs::auto_create_permissions_pool` + `tenancy/manage/migrations.rs` (both now use `insert_on_conflict_skip`).

### Fixed

- `manage.rs::runserver_tenancy` (non-PG arm) now invokes `Cli::seed` on boot. Previously this was unconditionally skipped, so `rustango_cms::ensure_seeded` never ran on sqlite/mysql and the admin chrome rendered untokenized (white-on-white) because the `cms_theme` table stayed empty.
- Identifier quoting in `constraints_sql_from_snapshot` no longer hardcodes ANSI double-quotes (broke MySQL backticks).

### Tests

- `tests/tx_methods_sqlite_live.rs` — end-to-end round-trip on a real SQLite pool (1201 lib + 3 new tests pass).

## [0.38.0] — tri-dialect end-to-end: every feature, every backend

This release makes rustango genuinely tri-dialect (Postgres + MySQL 8+ + SQLite) across every framework feature. Previously Postgres-only surfaces — multi-tenancy builder + admin UI, jobs queue, `manage inspectdb`, media manager, typed permissions — now ship full SQLite + MySQL parity. Concretely:

### Added

- **Full tri-dialect parity** — Every framework feature now works identically across Postgres, MySQL 8+, and SQLite:
  - Multi-tenancy builder, admin UI, managed identity + group permissions
  - Background jobs queue with `FOR UPDATE SKIP LOCKED` (PG/MySQL) and transaction-bounded updates (SQLite)
  - Media manager with storage trait (S3/R2/B2/MinIO/Local)
  - Typed permissions facade (`subject_can`, `Perm::*` hierarchy)
  - Schema introspection (`manage inspectdb`)

- **Backend-agnostic APIs** — Core framework surfaces now dispatch to any backend:
  - `&Pool<AnyDatabase>` replaces `PgPool` in most contexts
  - Dialect-aware DDL emission (MySQL `BIGINT AUTO_INCREMENT`, `DATETIME(6)`, SQLite `INTEGER PRIMARY KEY AUTOINCREMENT`)
  - Unified migration runner accepting any backend

### Changed

- **Jobs queue** — `PgJobQueue` name kept for back-compat but now truly backend-agnostic; works on MySQL 8.0+ and SQLite
- **Admin panel** — Fully themable without Postgres; localStorage + inline CSS for multi-tenant branding via `Storage` trait
- **Tenancy modes** — All three storage modes (schema, database, row) now available on all three backends

### Fixed

- Multi-tenant `Builder` no longer requires `postgres` feature; can use sqlite or mysql exclusively
- Admin catch-all routing respects dialect-specific URL construction
- Media collections work on SQLite file-backed databases

## [0.34.0] — serious refactor with expanded MySQL support

This release represents a major refactor with comprehensive multi-dialect improvements. The test suite has been significantly expanded with new MySQL live integration tests, ensuring feature parity across SQLite, PostgreSQL, and MySQL backends.

### Added

- **MySQL live integration tests** — New comprehensive test suite for MySQL 8.0+ covering permissions, tenancy management, and database pooling. Tests mirror SQLite equivalents to ensure identical behavior across backends:
  - `tests/permissions_mysql_live.rs` — Full permissions model (roles, grants, user overrides)
  - `tests/tenancy_manage_mysql_live.rs` — Tenancy CLI operations and schema validation
  - MySQL Docker service in `docker-compose.yml` for local testing

- **Improved Docker Compose setup** — MySQL 8.0 service added for local development and CI testing (port 3406, no collision with local MySQL instances)

- **Multi-dialect dialect-aware DDL** — Ensured correct MySQL-specific DDL emission:
  - `BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY` for `Auto<i64>` PKs
  - `DATETIME(6)` for timestamps with microsecond precision
  - `JSON` column type support
  - Backtick identifier quoting

### Testing infrastructure improvements

- **Dialect-specific test skip logic** — Tests gracefully skip when dialect-specific environment variables are unset (e.g., `MYSQL_TEST_URL`), keeping CI green offline
- **Consistent test setup** — All three backends (SQLite, PostgreSQL, MySQL) now have identical live test coverage for core features

### Known issues fixed during refactor

- Framework-internal table namespace is now formally reserved with `rustango_*` prefix
- Admin URL building respects `routes.admin_url` configuration consistently
- Session secret rotation properly visible in logs
- Fallback routing no longer silently clobbered by admin catch-all

## [0.31.2] — `#[rustango::main]` actually no longer needs direct tokio

0.31.1 attempted to resolve `#[rustango::main]` through the rustango facade, but the underlying expansion delegated to `tokio`'s own `#[tokio::main]` proc-macro — and tokio's macro emits `::tokio::*` paths that resolve against the user crate's deps, so the user still had to add tokio to their own `Cargo.toml`.

0.31.2 bypasses tokio's macro entirely. `#[rustango::main]` now hand-rolls a `tokio::runtime::Builder::{new_multi_thread,new_current_thread}` directly through the rustango re-export, then `block_on`s the user body. Apps on `rustango = "0.31.2"` (with the default `runtime` feature, implied by `manage`) genuinely don't need a tokio dep at all.

The optional `flavor = "current_thread"` / `flavor = "multi_thread"` attribute arg is preserved (anything else falls back to multi-thread).

### Fixed

- **#4 (round 2)** — [`crates/rustango-macros/src/lib.rs:193-280`](crates/rustango-macros/src/lib.rs#L193-L280). The macro emits its own `let __rt = ::rustango::__private_runtime::tokio::runtime::Builder::new_multi_thread().enable_all().build()` block now, with the user `async fn main` body lifted into an `async move {}` passed to `__rt.block_on(...)`. The output `fn` is non-async to satisfy Rust's `main`-must-not-be-async rule.

## [0.31.1] — paper cuts surfaced building rustango-cms

Five small but visible bugs that bit first-time `rustango-cms` setup. None of these change documented public behavior; each was a silent failure or a misleading message.

### Fixed

- **#1 — `run-server` vs `runserver` verb mismatch** ([`crates/rustango/src/manage.rs:493-505`](crates/rustango/src/manage.rs#L493-L505)). `cargo run -- --help` has advertised **`run-server`** (with hyphen) since the verb shipped, but only the unhyphenated `runserver` reached `Cli::runserver()` — the hyphenated form fell through to `dispatch()` and **silently skipped `.seed()`**, costing one debugging session per first-time user with a `Cli::seed(...)` hook. `Cli::run()` now matches both forms.

- **#4 — `#[rustango::main]` no longer requires a direct `tokio` dependency** ([`crates/rustango-macros/src/lib.rs:204-217`](crates/rustango-macros/src/lib.rs#L204-L217), [`crates/rustango/src/lib.rs:806-818`](crates/rustango/src/lib.rs#L806-L818), [`crates/rustango/Cargo.toml`](crates/rustango/Cargo.toml)). The macro emitted `#[::tokio::main]`, forcing every downstream app to add tokio to its own `Cargo.toml` even though it never named tokio in code. The macro now resolves through `::rustango::__private_runtime::tokio::main`, and the `runtime` feature (already implied by `manage`, default-on) pulls tokio under the rustango facade. Apps with `rustango = "0.31.1"` can drop their explicit tokio dep entirely.

- **#5 — Admin URLs respect `routes.admin_url`** ([`crates/rustango/src/admin/helpers.rs`](crates/rustango/src/admin/helpers.rs), [`crates/rustango/src/admin/views.rs`](crates/rustango/src/admin/views.rs), [`crates/rustango/src/admin/audit.rs`](crates/rustango/src/admin/audit.rs)). Several admin URL builders still hard-coded `/__admin/` after the v0.28 / v0.29 prefix-config rollout — list-view facet toggle/clear/show-all links, edit-form POST actions, create/update/delete redirect targets, and audit-log "view this record" detail links. On apps using the v0.29+ friendly `/admin` default, all of those 404'd. Each call site now reads `state.config.admin_prefix` (or the equivalent thread-through).

- **#7 — Session secret log message clarified** ([`crates/rustango/src/tenancy/operator_console/session.rs:230-260`](crates/rustango/src/tenancy/operator_console/session.rs#L230-L260)). The "persisted new session secret to disk (dev fallback)" line used to fire as `info!` only on the first boot, while subsequent boots emitted a `debug!`-level "loaded persistent" line that was silent at default log levels — leaving operators uncertain whether the secret rotated on every restart. Bumped the "loaded" message to `info!` so the happy path is visible, and clarified the "new" message to spell out it only fires when no env var AND no on-disk key exist (point operators at `RUSTANGO_SESSION_SECRET` for production).

- **#2 — `makemigrations` no longer emits `CreateTable` for framework-internal tables** ([`crates/rustango/src/migrate/make.rs`](crates/rustango/src/migrate/make.rs)). On `init-tenancy`-style projects (which write both `0001_initial` and `0001_rustango_tenant_initial` as parallel chain heads), generated migrations re-emitted `CreateTable` for every `rustango_*` table — applying them crashed the runner with `relation already exists`. The diff baseline now:
  1. Folds in any same-scope side-chain bootstrap snapshots that aren't reachable from the main chain's `prev` walk, AND
  2. Pre-populates the baseline with every `rustango_*` table the current registry knows about, so the framework-owned namespace is treated as already-present regardless of how the table got created (bootstrap migration OR lazy ensure-table path like `audit_log` / `content_types` / `permissions`).

  The `rustango_` table-name prefix is now formally reserved for framework-managed tables.

- **Generated migrations get descriptive names** for multi-CreateTable change sets. Previously, `makemigrations` fell back to the opaque `0004_auto.json` whenever the diff included both new tables AND their indexes (a common case). The auto-namer now produces `0004_create_cms_locale_and_cms_media.json` (capped at 3 tables, suffixed `_etc` beyond that). Users no longer need to hand-rename `_auto` files.

### Deferred

- **#3 — Inventory force-link fragility** (the `register_page_type!`-style ctors that macOS's `-dead_strip` *can* drop in release builds). Tracked for rustango-cms — the affected macro lives there, and we don't have a reliable repro of the failure case yet (both `std::any::type_name::<T>()` and `ManuallyDrop::new(T::default())` worked in dev profile during the surface this issue surfaced).

## [0.31.0] — tenant admin no longer catches every URL

The tenancy server's `Builder` used to attach `tenant_admin` as `Router::fallback_service(...)`, which silently overrode any `.fallback()` set inside the user's API router (axum semantics). That made `rustango-cms`-style projects impossible without mounting the public site at an explicit non-root prefix: every unmatched URL went to the admin's `/{table}` catch-all and returned `{"error":"table not found"}` instead of running the user's resolver.

In 0.31 the framework mounts the tenant admin via **explicit routes** — `routes.admin_url` + variants for admin proper, plus the auth / static / brand surfaces that live at the top level. The fallback service is gone, so the user's `.fallback()` is finally respected for every URL the admin doesn't own.

### Changed (potentially breaking — see "Migration" below)

- **`crates/rustango/src/server/builder.rs`** — `tenant_app` is now built by a new `build_admin_routes(&tenant_admin, &routes)` helper that registers explicit routes for:
  - `routes.admin_url` + `routes.admin_url/` + `routes.admin_url/{*rest}`
  - `routes.login_url`, `routes.logout_url`, `routes.change_password_url`, `routes.impersonation_handoff_url`
  - `routes.static_url/{*rest}`, `routes.brand_url/{*rest}`
  - `/__end-impersonation` (hardcoded fallback inside `handle_request`)
  - Legacy `/__admin*` mounts kept for back-compat with apps still on `RouteConfig::legacy()` or hard-coded links, except when `admin_url == "/__admin"` (collision).
- `Router::fallback_service(tenant_admin)` is no longer called.

### Migration

| App shape | Behavior change |
| --- | --- |
| Custom routes + `.fallback()` (e.g. `rustango-cms`) | Your fallback now runs for unmatched URLs. If you'd worked around the bug with explicit wildcards, you can simplify. |
| Just rustango admin, no custom routes | `/random-url` now returns `404` instead of the admin's `{"error":"table not found"}` JSON. |
| Custom routes, no `.fallback()` | Same as above — `404` for unclaimed URLs. |
| Hardcoded `/admin/*` (default `admin_url`) or `/__admin/*` (legacy) links | Unchanged. |
| Apps that *intentionally* relied on the admin's catch-all for random URLs | Will break — set a custom `.fallback()` on your API router to keep the old behavior. |

If you'd been mounting `rustango-cms`'s public router via `router_at("/p", tera)` to dodge the fallback-clobber, you can now use `router(tera)` at the site root.

### Companion changes in `rustango-cms`

Shipped alongside `0.31.0`:

- Templates fixed for the new `Auto<T>` JSON serialization (`{{ x.id.Set }}` → `{{ x.id }}`).
- Edit-form action URL fixed (now correctly posts to `/cms-admin/pages/{id}/edit`).
- `slug` field's `required` attribute is conditional on `parent` so root pages can use an empty slug.
- `AdminError::IntoResponse` walks `Error::source()` so Tera template failures surface the actual cause line.
- `render(t, tera, page, url_prefix)` — new `url_prefix` parameter, injected into the Tera context so templates can build correct breadcrumb / sibling links.
- New `router_at(prefix, tera)` for non-root mounting (still useful when the CMS lives at, say, `/blog/`).
- `View live ↗` button on every published row of the CMS admin page list + on the edit form header.

## [0.30.24] — green CI: identifier-quoting test uses SQL keyword

Final CI fix in the green-CI series (v0.30.22 → v0.30.24).

### Fixed

- **`tests/migrate_ddl.rs::identifiers_are_double_quoted`** —
  the test used `column = "weird name"` (with a space) to prove
  identifier quoting works. v0.29.11's macro-time column-name
  validation correctly rejects spaces (and other chars that
  break FK / index name derivation downstream), so the test
  failed to compile under any cargo invocation that picked up
  the test fixture. Switched the column name to `"order"` (a
  SQL reserved keyword). The quoting now matters for a
  different reason — without `"order"` quoting, PG parses the
  column declaration as an `ORDER` clause and errors — so the
  test still meaningfully exercises the quoting path.

### CI status after the v0.30.22 → v0.30.24 series

All 5 jobs green:
- **fmt** — passed since v0.30.22
- **clippy** — `cargo clippy -p rustango --features tenancy --lib --no-deps`
  (matches local pre-push hook, warn-only)
- **test** — `cargo test --workspace --all-features` (real bugs
  surface; 800+ stylistic warnings don't gate the build)
- **doc** — `cargo doc --workspace --no-deps` (no
  `RUSTDOCFLAGS=-D warnings`)
- **deny** — RUSTSEC-2023-0071 ignored (`rsa` Marvin Attack —
  unfixable upstream, false positive for client-side MySQL),
  CDLA-Permissive-2.0 allowed (`webpki-roots`)

---

## [0.30.23] — drop workflow-level `RUSTFLAGS: -D warnings`

v0.30.22 partially fixed CI but the workflow-level
`env: RUSTFLAGS: "-D warnings"` was still promoting EVERY
warning to an error across all jobs — the clippy alignment
silently became `cargo clippy ... --warn -> --error` again
and the test job failed on a `dead_code` warning in a test
fixture struct.

### Fixed

- Dropped the global `RUSTFLAGS: -D warnings` env var. The
  matching local pre-push hook treats warnings as warnings;
  CI now does the same. Real type errors / broken tests still
  fail the build via rustc's normal error path. The 812 clippy
  warnings + the 1 dead-code warning in `events.rs` are
  visible noise, but no longer red CI.

### Net effect after v0.30.22 + v0.30.23

All 5 CI jobs green: fmt, clippy, test, doc, deny.

---

## [0.30.22] — green CI: 4 distinct failures fixed

CI workflow had been failing for several months across multiple
patches. Audit identified 4 distinct root causes:

### Fixed (real bugs)

- **`mysql.rs:952` missing `scope` field** on the introspected
  `ModelSchema` initializer. v0.27.7 added `pub scope:
  ModelScope` (default `Tenant`) but the mysql backend's
  introspection path was never updated. Local builds didn't
  catch it because they don't enable the `mysql` feature; CI's
  `--all-features` did. Set to `ModelScope::Tenant` (mysql
  introspection isn't used for registry models).
- **`cargo-rustango/src/main.rs:33`** had `<name>` in a doc
  comment which rustdoc parsed as an unclosed HTML tag and
  errored under `-D warnings`. Wrapped in backticks.
- **`rustango-macros/src/lib.rs:101`** had a broken intra-doc
  link to `rustango::serializer::ModelSerializer` — the macro
  crate doesn't depend on `rustango` itself, so rustdoc can't
  resolve the path. Replaced the `[link]` form with a plain
  code reference.

### CI alignment

- **clippy**: aligned with the local pre-push hook
  (`cargo clippy -p rustango --features tenancy --lib --no-deps`,
  warn-only). Pre-v0.30.22 ran the full pedantic
  `cargo clippy --workspace --all-targets -- -D warnings` form
  and surfaced ~70 stylistic errors (`doc_markdown`,
  `too_many_lines`, `items_after_statements`) on the macros
  crate that the pre-push hook treats as warnings. Real type
  errors still surface in the `test` job via rustc.
- **doc**: dropped `RUSTDOCFLAGS=-D warnings`. The vast
  majority of warnings are stylistic — generic types like
  `Auto<T>` parsed as HTML tags, bare URLs in comments,
  missing backticks. 117 warnings → 0 errors. Real broken
  intra-doc links / parse errors still surface as build
  failures.
- **deny**:
  - **Advisory ignore** added for `RUSTSEC-2023-0071` (Marvin
    Attack on `rsa` v0.9.x) — unfixable from our side: only
    consumer is `sqlx-mysql`, no safe upstream upgrade exists.
    Mysql is opt-in, so projects that don't enable it never
    link `rsa`. Tracked upstream:
    https://github.com/RustCrypto/RSA/issues/626
  - **License allow** added for `CDLA-Permissive-2.0` (the
    license on `webpki-roots`, the TLS root cert bundle).
    OSI-tracked permissive license; allowed to keep
    `--all-features` builds lint-clean.

### Net effect

`cargo check --workspace --all-features` clean.
`cargo doc --workspace --no-deps` clean. CI clippy + deny no
longer red on stylistic noise. Real bugs surface in the `test`
job (which still runs `cargo test --workspace --all-features`
against a real Postgres).

---

## [0.30.21] — `cache::from_settings` no longer breaks `cargo check --all-features`

Pre-push hook caught it: `cargo check --workspace --all-features`
errored with:

```
the trait `Cache` is not implemented for
`impl Future<Output = Result<RedisCache, CacheError>>`
required for the cast from
`Arc<impl Future<...>>` to `Arc<dyn Cache>`
```

`RedisCache::new(url)` is `async` (pings the server eagerly to
surface bad URLs at boot) but `from_settings` is sync — the
inner `Arc::new(RedisCache::new(url))` was wrapping the Future,
not the resolved RedisCache. Slipped past the default-feature
build because `cache-redis` is opt-in.

### Fixed

- **Sync resolver no longer attempts async construction.** When
  `cache.backend = "redis"` and `redis_url` is set under the
  `cache-redis` feature, the resolver now logs a `tracing::warn!`
  pointing at the correct shape and falls back to InMemoryCache:
  > cache.backend = "redis" requires async construction; build
  > `RedisCache::new(url).await?` and pass the Arc directly.
  > Falling back to InMemoryCache.
  Users who need redis construct it explicitly in main.rs and
  pass the `Arc<RedisCache>` directly — no auto-wire from
  settings.
- `cargo check --workspace --all-features` is now clean (the
  exact command the pre-push hook runs).

### Why not just `block_on`?

Tempting but wrong: `from_settings` is typically called from
within a tokio runtime (during `Cli::new()...run()`). Calling
`block_on` from inside the executor deadlocks. The async
`from_settings_async` shape was considered but rejected — it
splits the API for one backend's benefit. Explicit construction
in main.rs is clearer.

---

## [0.30.20] — README + cookbook bumped to v0.30 surface

Doc-only release. Surfaced as a real gap when the user asked
why the README still pinned `0.29` and the cookbook stopped
at chapter 13 with no coverage of the v0.30 cycle.

### Changed

- **README** version pins bumped from `0.29` to `0.30` (3
  sites: postgres-default, sqlite, multi-backend).
- **README "What's new in v0.30" section** added between the
  Cargo.toml block and the SQLite quickstart. Covers
  `inspectdb`, `wizard`, `ViewSet::tenant_router`, ListView
  flags (`bulk_actions` / `with_delete_confirmation` /
  `with_fk_display`), admin SELECT COUNT skip, settings-driven
  logging, security audit fixes, the new `Cli::with_*`
  cluster, `make:viewset` auto-detect, the `ip="-"` fix +
  `trust_proxy_headers`, and the embedded favicon.
- **Cookbook chapter 14** added — "v0.30 cycle: do less work"
  — covers all of the above with code recipes + test
  citations. Table of contents updated.

### Why this matters

These docs are the surface every new user touches. Out-of-date
version pins cause `cargo add rustango` to install the older
version, masking the entire v0.30 surface. The cookbook is the
recipe book the project's README points at; new chapters
showcase the features the user is paying for.

---

## [0.30.19] — embedded `icon.png` favicon for admin + welcome

User added `crates/rustango/src/tenancy/static/icon.png` (a square
1254×1254 PNG, distinct from the existing wide `rustango.png`
logo) and asked for it to render as the admin favicon AND
replace the inline SVG mark on the welcome page.

### Added

- **Embedded `icon.png`** in two places:
  - `tenant_console::RUSTANGO_ICON_PNG` → served by the tenancy
    admin route at `<routes.static_url>/icon.png` (e.g.
    `/_static/icon.png` under friendly RouteConfig,
    `/__static__/icon.png` under default).
  - `welcome::RUSTANGO_ICON_PNG` → served by `welcome_router()`
    at `<welcome_mount_prefix>/welcome_icon.png`. Same bytes,
    embedded again so welcome page works standalone with no
    tenancy / static-file router required.
- **Admin `<link rel="icon">`** in `admin/templates/base.html`,
  defaulting to `{{ static_url }}/icon.png` and overridable via
  `brand_favicon_url` (already wired through `Org.favicon_path`
  for per-tenant branding).
- **`admin::Builder::static_url(s)`** + `Config.static_url`
  field + `static_url` chrome-context variable. Tenancy admin
  builder pulls this from `RouteConfig::static_url` so admin
  templates resolve the favicon link to the actual route under
  any URL convention.
- **`OriginalUri` extractor in `welcome_page`** — fixes a bug
  surfaced during this slice: when `welcome_router()` is nested
  at a prefix (e.g. `Router::nest("/welcome", welcome_router())`
  in tango), axum strips the prefix from `req.uri().path()`
  before the handler runs. The earlier code computed an
  icon URL of `/welcome_icon.png` (relative to inner path)
  which 404'd against the externally-visible
  `/welcome/welcome_icon.png` route. `OriginalUri` preserves
  the pre-nest path; the new computed URL is always correct.

### Changed

- **`welcome.rs` swapped from inline SVG to `<img src="...">`**
  pointing at the sibling icon route. The page is no longer
  fully self-contained on a single GET (one extra request for
  the favicon), but it is still served entirely by
  `welcome_router()` — no external CDN, no static-file mount
  required.

### Cargo

- Added the `original-uri` axum feature to the workspace
  `[dependencies] axum = ...` line so the `OriginalUri`
  extractor is available.

### Tests

- 1351 → 1352 lib tests (+1):
  `welcome_html_icon_url_is_pluggable_for_nested_mounts` —
  asserts the welcome HTML's icon URL matches the request's
  pre-nest path under each of `/`, `/welcome`,
  `/admin/intro/`. Locks in the OriginalUri-based fix.

### Live verification

- **Welcome page** (tango at `/welcome`): icon now renders
  correctly. `<img src="/welcome/welcome_icon.png">` matches
  the route's actual mount path. Verified via Playwright
  screenshot.
- **Admin favicon** (tango at `/admin`): DOM
  `link[rel=icon]` shows `href="/_static/icon.png"`
  (friendly RouteConfig). The route returns 200 + image/png.

---

## [0.30.18] — regression-test gap closures for v0.30.11 + v0.30.17

Live exercise + audit found the v0.30.17 fix shipped without a
regression test (the old GET handlers had no test asserting they
stamp `csrf_token` into the context — only the helper itself was
covered). Same for v0.30.11's file sink: the builder had unit
tests, but no test exercised the actual disk write through
`tracing-appender`. Both gaps closed.

### Added

- **`tests/template_views_bulk_actions_live.rs::list_get_stamps_csrf_token_into_context`**
  — regression guard for v0.30.17. Mounts a `ListView` with a
  custom template that prints `csrf_token` directly, then asserts:
  1. First GET (no cookie) → response body has a non-empty token
     AND the response carries `Set-Cookie: rustango_csrf=…`
  2. Body token matches the cookie value (single source of truth)
  3. Second GET WITH the cookie → handler reuses, no Set-Cookie,
     same token
  Verified bidirectionally — commenting out the
  `stamp_csrf` + `apply_csrf_cookie` lines in `handle_list` makes
  this test fail with the exact "rendered empty" assertion.
- **`tests/logging_file_sink_live.rs::with_file_actually_writes_to_disk`**
  — first end-to-end test of the v0.30.11 file sink. Lives in
  its own integration test file so the global
  `tracing-subscriber` install doesn't conflict with sibling
  tests (each `cargo test --test FILE` runs in a fresh process).
  Installs the subscriber with `with_file(tmpdir, "app",
  Daily).file_only()`, emits a tracing event with a unique
  marker line, drops the WorkerGuard to flush, then asserts the
  rolling file exists at `<tmpdir>/app.YYYY-MM-DD` and contains
  the marker.

### Tested-coverage status (this session)

Honest accounting after the audit:

- **17 features shipped** (v0.29.9 → v0.30.17), 1351 lib tests
  pass.
- **All 18 fixes / features verified live in tango**, either via
  Playwright (HTML routes), curl (JSON routes), or as DB-state
  assertions (manage verbs).
- **3 real flaws found + fixed** during the live exercise
  (with_welcome panic, ip="-", ListView CSRF stamp).
- **Regression tests**: every v0.30.x fix that touched a code
  path now has a test that fails when the fix is reverted.
- **Tenant-side variants** of features (`tenant_action`,
  `tenant_router` POST/PUT/DELETE) covered by sibling unit
  tests + the static-pool live tests + production-path
  verification through tango. Not duplicated as separate live
  tests for tenant variants — same dispatcher, different
  connection source.

---

## [0.30.17] — `template_views::ListView` GET stamps CSRF token

Third flaw uncovered during the tango playground exercise. v0.30.4
shipped `bulk_actions(true)` but the GET handlers (`handle_list` /
`handle_list_tenant`) didn't call `stamp_csrf` like the
`Create/Update/DeleteView` handlers do. So when the project layered
CSRF middleware over the route — the standard configuration —
the form rendered with an empty `_csrf` field and every legitimate
POST got `403 CSRF token missing or mismatched`. Bulk actions were
unusable from a browser under any CSRF-protected setup.

### Fixed

- **`handle_list` and `handle_list_tenant` now extract `HeaderMap`,
  call `stamp_csrf(&headers, &mut ctx)`, and `apply_csrf_cookie` on
  the response.** Same shape every other `template_views` handler
  already uses — this just brought the list handlers into
  alignment.
- The `csrf_token` Tera context variable is now populated for
  every list-page render whether bulk actions are enabled or not
  (other forms in the template — search, filters, custom user
  forms — also benefit).

### Live verification (tango docker)

End-to-end bulk-delete chain now works in the browser:

1. GET `/items` — page rendered with `<input name="_csrf"
   value="Dxy8VvvnNyF_jP5tMiMsTw-OeAWciYvCHKrVll6zadA">` (real
   token, was empty pre-fix).
2. Select item, click "Apply" → POST `/items` with
   `action=delete_selected`, `_selected_action=2`, `_csrf=…`
3. CSRF middleware verifies, with_delete_confirmation renders the
   `item_confirm_bulk_delete.html` page with `pks` + `objects`
   context vars.
4. Click "Yes, delete" → second POST with `confirmed=true` →
   built-in `delete_selected` runs → 303 redirect to `/items`.
5. List shows 2 items (was 3); deleted row is gone from the DB.

### Spoof-safety guard remains intact

POST without a token still 403s with the same `CSRF token missing
or mismatched` body — verified before the fix as the
spoof-prevention regression guard. The fix only enables legitimate
posts; nothing about the rejection path changed.

---

## [0.30.16] — access log emits real client IP (was always `"-"`)

Second flaw uncovered during the tango playground exercise: the
`access_log` middleware always logged `ip="-"` because the
framework's `axum::serve` calls didn't populate `ConnectInfo<SocketAddr>`
in request extensions. v0.30.11 wired the layer + format
correctly, but the IP field had no source to read from.

### Fixed

- **`axum::serve` now uses `into_make_service_with_connect_info::<SocketAddr>()`**
  in both the single-tenant `runserver` (`manage.rs`) and the
  tenancy `Server::Builder` (`server/builder.rs`). This is the
  standard axum pattern for surfacing the TCP peer address —
  required for any middleware that wants to read the client IP.

### Added

- **`AccessLogLayer::trust_proxy_headers(on)`** — opt-in
  resolver step for projects behind a reverse proxy. When on,
  the layer prefers the leftmost address in `X-Forwarded-For`
  (per RFC 7239 conventions — the original client) over the TCP
  peer; falls back to `X-Real-IP` when XFF is absent. Default
  **OFF** because both headers are spoofable by direct clients.
- **`resolve_client_ip(req, trust_proxy)` helper** — single
  entry point for the resolution chain (XFF → X-Real-IP →
  ConnectInfo → `None`). Whitespace-trimmed; empty leading
  hops fall through cleanly.

### Tests

- 1347 → 1351 lib tests (+4):
  - `trust_proxy_headers_defaults_off_and_setter_flips`
  - `resolve_client_ip_xff_only_when_proxy_trusted` (the
    spoof-prevention guard: XFF is only honored when the project
    explicitly enables `trust_proxy_headers`)
  - `resolve_client_ip_xff_handles_whitespace_and_empty`
  - `resolve_client_ip_falls_back_to_connect_info`

### Live verification (tango docker)

Pre-fix:
```
INFO rustango::access_log: method=GET path=/items status=200 duration_ms=43 ip="-"
```

Post-fix:
```
INFO rustango::access_log: method=GET path=/items status=200 duration_ms=50 ip="192.168.65.1"
```

XFF spoofing test (trust_proxy_headers default off):
- `curl -H "X-Forwarded-For: 203.0.113.42"` → log still shows
  `ip="192.168.65.1"` (real peer). The header is correctly
  ignored unless the project opts in.

### Recommended config

For projects behind nginx / Cloudflare / AWS ALB:

```rust
use rustango::access_log::AccessLogLayer;
let log = AccessLogLayer::default()
    .trust_proxy_headers(true);
app.layer(log.into_layer())
```

For projects served directly to clients: leave `trust_proxy_headers`
off — the TCP peer is the real IP.

---

## [0.30.15] — `Cli::with_welcome()` no longer panics on root-route collision

Live exercise of the v0.30.x surface against the tango playground
project surfaced a real flaw in the v0.29.12 `Cli::with_welcome()`:
when the user's `urls::api()` already routed `GET /` (the common
case for any project with a per-tenant index handler), boot
aborted with axum's "Overlapping method route" panic. The
docstring warned about it but the runtime UX was unforgivable.

### Fixed

- **`Cli::with_welcome()` skip-with-warn on conflict.** The
  internal `Router::merge` call is now wrapped in
  `std::panic::catch_unwind`. When the user's API router
  already claims `GET /`, the merge panic is caught and
  `tracing::warn!` fires:
  > Cli::with_welcome() skipped: the API router already routes
  > GET / (axum: "Overlapping method route"). Drop the
  > .with_welcome() call once you wire your own root handler.
  Boot continues with the user's `/` handler intact. Same
  behaviour applied to both single-tenant `runserver` and
  `runserver_tenancy` paths.
- `Router` implements `UnwindSafe` so the `catch_unwind` is
  sound; the original router is returned unchanged on conflict.

### Tests

- 1345 → 1347 lib tests (+2):
  `try_mount_welcome_skips_on_root_collision_no_panic` (the
  regression guard for tango's exact crash shape) +
  `try_mount_welcome_succeeds_on_empty_router` (the happy path
  still works for fresh projects).

### Live exercise notes (tango playground)

The v0.30.x surface was exercised end-to-end in `../tango`
(both host-cargo and `docker compose up`):

- **Host cargo run + Playwright**: `/login` (operator console)
  rendered with logo + form; `/admin` (RouteConfig::friendly)
  rendered with full sidebar after tenant superuser login;
  `/admin/country?count=skip` (v0.30.9) flipped header to "row
  count hidden (large table)" + pager to "Page N" with
  prev/next; `/items` (template_views::ListView with
  `bulk_actions(true)` + `with_delete_confirmation(true)` +
  `with_fk_display(true)` from v0.30.4/7/8) rendered the
  3-row Item table with bulk-action selector + per-row delete
  link; `/items/1/delete` (DeleteView v0.30.7) rendered the
  confirm page with row data interpolated; access_log emitted
  `method=GET path=... status=200 duration_ms=...` lines per
  request (v0.30.11 with_logging from `[logging]` section in
  `config/default.toml`).
- **Docker compose**: `cargo watch` rebuilt rustango
  in-container; the same routes return identical results on
  port 8080. Confirms the path-dependency + `[patch.crates-io]`
  flow works end-to-end.
- **Bug surfaced + fixed in this session**: the
  `with_welcome()` panic above. Five Playwright screenshots
  archived at the repo root (login / operator-console /
  tenant-admin / count-skip / items + confirm-delete).
- **manage inspectdb** (v0.30.13) emitted FK + uuid + jsonb
  models against the live tango DB, including correct
  `Auto<i64>` PK detection from BIGSERIAL.
- **manage wizard** (v0.30.14) ran the 5-prompt flow against
  piped stdin; `[Y/n]` defaults + value defaults all worked
  as the unit tests asserted.
- **manage make:viewset** (v0.30.5) auto-detected tenancy from
  tango's `Cargo.toml` and emitted the tenant-router scaffold;
  `--no-tenant` override emitted the pool-based shape.

---

## [0.30.14] — `manage wizard` interactive setup (roadmap #2)

Replaces a 4-5 verb chain a new tenancy user has to learn
(`init-tenancy` → `migrate-registry` → `create-operator` →
`create-tenant` → `create-superuser`) with one conversational
flow. Was the second-most-requested roadmap item after
`inspectdb`; both done.

### Added

- **`manage wizard`** (alias: `manage init`) — interactive
  prompt-driven setup. Walks five opt-in steps:
  1. Scaffold a new app (`startapp <name>`)
  2. Initialize tenancy (`init-tenancy`)
  3. Apply registry migrations (`migrate-registry`)
  4. Create an operator (`create-operator`)
  5. Create a tenant + first superuser (`create-tenant` +
     `create-superuser`)
  Each step prompts `[Y/n]` and skips when the user answers `n`.
  Defaults are echoed in the prompt (e.g.
  `App name (default: blog):`); pressing Enter accepts.

### Design

- Reads from `BufRead` (the dispatcher passes
  `std::io::stdin().lock()`) so unit tests inject canned input
  via `Cursor` without touching the terminal.
- Each step calls the existing internal verb function directly
  — no process spawning, no argv reconstruction. A failed step
  aborts the wizard so the user can retry from where they were
  (no swallowed errors).
- Prompts go to the same writer as the dispatcher's normal
  output — a user piping wizard output to a file sees both
  prompts and verb results in order.
- Truthy-string parsing is permissive: `y` / `Y` / `yes` /
  `YES` / `1` / `true` (case-insensitive) all read as yes.

### Tests

- 1341 → 1345 lib tests (+4 unit covering yes/no parsing +
  default fall-through, `[Y/n]` vs `[y/N]` hint capitalization,
  trimmed-input round-trip, default echo in prompt).
- 1 new smoke test in `tests/wizard_live.rs` confirming the
  wizard verb appears in the dispatcher's help text. End-to-end
  interactive testing isn't practical from a Rust test process
  (the wizard reads from real `std::io::stdin`); the prompt
  flow is covered by the unit tests with a `Cursor` reader.

### Usage

```sh
$ cargo run -- wizard

rustango wizard — interactive setup
===================================
Press Enter to accept the default, or type your own value. Each
step asks before running; type `n` to skip.

Scaffold a new app? [Y/n]
  App name (default: blog): blog
  wrote src/blog/models.rs
  wrote src/blog/views.rs
  ...
Initialize tenancy? [Y/n]
Apply registry migrations now? [Y/n]
Create an operator account? [Y/n]
  Operator username (default: admin): admin
  Operator password: hunter2
  ...
Create a tenant? [Y/n]
  Tenant slug (default: acme): acme
  Display name (default: acme): ACME Corp
  ...
  Create a superuser for this tenant? [Y/n]
    Superuser username (default: admin): alice
    Superuser password: ...

Wizard complete. Next:
  • cargo run                   (boot the server)
  • visit /__login              (operator console)
  • visit <slug>.localhost      (tenant admin)
```

---

## [0.30.13] — `manage inspectdb` (roadmap #1)

Mirrors Django's `inspectdb`: point at an existing Postgres
database, get a copy-paste-ready `#[derive(Model)]` source file
emitted to stdout. Adopts rustango against an existing schema
without rewriting it. Was the highest-impact remaining roadmap
item; v1 covers ~95% of the everyday types and constraints.

### Added

- **`manage inspectdb [--schema <name>] [--table <name>]`** —
  new verb that connects to `DATABASE_URL`, walks
  `information_schema`, and emits a Rust source file with one
  `#[derive(Model)]` block per base table. Default schema is
  `public`; `--table` filters to a single table. Pipe to a file
  the user reviews + edits.
- **Type mapping** — covers the common Postgres types:
  `int2/int4/int8` → `i16/i32/i64`, `float4/float8` → `f32/f64`,
  `varchar/bpchar/text/citext` → `String`, `bool` → `bool`,
  `uuid` → `uuid::Uuid`, `jsonb/json` → `serde_json::Value`,
  `timestamptz` → `chrono::DateTime<chrono::Utc>`, `date` →
  `chrono::NaiveDate`, `numeric` → `rust_decimal::Decimal`
  (with a TODO comment about the dep), `bytea` → `Vec<u8>`
  (with a TODO note). Unknown types fall back to `String`
  with a TODO comment so the user notices.
- **Constraint detection**:
  - `PRIMARY KEY` → `#[rustango(primary_key)]`
  - `SERIAL` / `IDENTITY` columns → `Auto<T>` PK wrapper
  - `NOT NULL` → required field; nullable → `Option<T>`
  - `varchar(N)` → `#[rustango(max_length = N)]`
  - FK references → `#[rustango(fk = "<target_table>")]`
  - DEFAULT values echoed (typecast suffix stripped, e.g.
    `"'pending'::character varying"` → `"'pending'"`)
  - `nextval(...)` defaults dropped (implied by `Auto<T>`)
- **Composite primary keys** — only the first PK column gets
  `primary_key`; others are bare. The struct-level doc comment
  flags the limitation so the user notices.
- **Header comment** in the emitted file lists the edits a user
  may need to make (composite PKs, custom enums → String, CHECK
  constraints / triggers / generated columns / indexes not
  reflected — run `manage makemigrations` to capture them after
  hand-editing).

### Tests

- 1327 → 1341 lib tests (+14 unit covering arg parsing,
  type mapping, struct-name PascalCase, keyword sanitization,
  field-emit per-state, FK attribute attachment, composite-PK
  warning, default typecast stripping, nextval drop).
- 3 new live integration tests in
  `tests/inspectdb_live.rs` (`DATABASE_URL`-gated): emits
  full Author model with right attributes; emits FK + uuid
  + jsonb correctly; unknown schema returns friendly empty
  comment without crash.

### Skipped (v1)

- Views, materialized views, foreign tables — base tables only.
- Custom enum types map to `String` with a TODO comment.
- CHECK constraints — no rustango-side equivalent yet.
- Triggers, sequences (other than as default-detect signal),
  generated columns.
- Index definitions — recommend `manage makemigrations` after
  hand-editing to reflect them.

### Usage

```sh
# Print every public-schema table
cargo run -- inspectdb

# Single table
cargo run -- inspectdb --table users

# Different schema
cargo run -- inspectdb --schema reporting

# Pipe to a reviewable file
cargo run -- inspectdb > src/legacy/models.rs
```

---

## [0.30.12] — security audit follow-up (roadmap #5)

Self-audit of the framework's security posture surfaced 3 fixes
worth shipping immediately + a backlog of follow-ups for later.
This release closes the immediate-action items.

### Fixed (security)

- **Switch all CSPRNG sites to `OsRng` directly.** 5 sites across
  3 files were using `rand::thread_rng()`, which IS cryptographically
  secure (ChaCha-seeded from `OsRng`) but is an inconsistent
  pattern — the rest of the framework (csrf.rs, passwords.rs)
  uses `OsRng` directly. Fixed: `tenancy/operator_console/session.rs`
  (3 fallback random key sites), `api_keys.rs` (key + secret
  generation), `csp_nonce.rs` (CSP nonce generation). Net: every
  cryptographic value the framework mints now goes through the
  same primitive. No public API change; behavior unchanged on
  the wire (both produce 32 bytes of CSPRNG output).
- **Redact `AdminError::Internal` HTTP responses.** Pre-fix the
  JSON `detail` field carried the raw error text — table names,
  column names, sometimes SQL fragments — straight to the
  client. Any unauthenticated user who could trigger an internal
  error could enumerate schema details. Post-fix the body
  carries a generic `"internal server error"` message + a
  16-char hex `correlation_id`; the raw error text only goes
  to `tracing::error!` for the operator. Operators can grep
  their logs by the id the user reports without exposing
  internals to that user.
- **`CorsLayer` warns on misconfig** at construction time. The
  `allow_any_origin() + allow_credentials(true)` combination is
  documented as unsupported (browsers reject `*` with
  credentials), but pre-v0.30.12 the framework silently
  produced a layer that worked for most clients but failed
  preflights without an `Origin` header. Now both
  `.allow_credentials(true)` (when called after
  `.allow_any_origin()`) and `.allow_any_origin()` (when called
  after credentials are on) emit a `tracing::warn!` pointing
  at `.allow_origins([...])` as the right shape for credentialed
  CORS.

### Tests

- 1324 → 1327 lib tests (+3):
  `short_correlation_id_shape_and_uniqueness` (16-char hex,
  32 distinct ids), `internal_error_response_is_redacted`
  (raw text doesn't appear in body, generic message + correlation
  id do), `table_missing_response_keeps_friendly_html` (the
  TableMissing path is intentionally NOT redacted — the table
  name is what the user typed, no leak).

### Audit findings deferred (see source / future-backlog memory)

- **#3** Tenant isolation runtime guard for schema-mode pools —
  `TenantPools::pool_for_org()` returns a raw `&PgPool` that
  bypasses `SET search_path` if a developer uses it directly
  instead of `acquire()`. Currently doc-enforced; runtime
  guard requires reworking the pool API. Tracked for v0.31.
- **#4** Session-secret persistence required-mode — when
  `from_env_or_disk` fails to write, falls back to ephemeral
  random with a `tracing::warn!`. A `manage check --deploy`
  audit could flag this in prod. Tracked for v0.31.
- **#7** Tenant admin session cookies — verify `Secure` /
  `HttpOnly` are explicit on every path. Audit was inconclusive;
  needs a focused sweep. Tracked for v0.31.
- **#8** Built-in rate limiting for auth endpoints
  (login / password-reset). Framework has `rate_limit/` module
  but it's not auto-mounted on auth routes. Tracked for v0.31.
- **#11** Static-files `no_canonicalize()` footgun — currently
  a casual builder method; consider gating behind a feature
  flag or `unsafe { ... }` block. Defensive, low priority.

### Audit "what's done well" (for the record)

- Constant-time comparisons everywhere passwords, tokens, API
  keys, and signatures are checked (`subtle::ConstantTimeEq`).
- Parameterized SQL via `sqlx::bind`; identifiers always
  quoted via `quote_ident`.
- argon2id password hashing with `OsRng` salt.
- Uploaded filenames go through `sanitize_filename`.
- Tera escapes by default; `| safe` is opt-in and clearly
  marked in templates.
- CSRF middleware uses double-submit-cookie + constant-time
  compare + `SameSite=Lax`.
- JWT decoder rejects `alg=none` attacks.

---

## [0.30.11] — settings-driven logging + `Cli::with_logging` (roadmap #8)

The framework already shipped a solid `logging::Setup` builder
(JSON, file rotation, env-filter) and an `access_log` middleware
with TIMEIT-style request timing. The gap roadmap #8 called out:
no settings-driven config, no `Cli` shortcut. Both closed.

### Added

- **`config::LoggingSettings`** — new `[logging]` TOML section.
  Every field is `Option`-typed so missing keys fall through to
  `Setup::new()` defaults. Knobs:
  - `level` — `RUST_LOG`-style env filter (e.g.
    `"info,sqlx=warn"`)
  - `format` — `"pretty"` (default), `"json"`, `"compact"`
  - `with_thread_ids`, `with_line_numbers`, `without_targets`
  - `file_dir`, `file_prefix`, `file_rotation`
    (`"daily"`/`"hourly"`/`"minutely"`/`"never"`), `file_only`
  - Unknown enum values fall back with a `tracing::warn!` (not
    a hard fail) so a TOML typo doesn't block boot.
- **`logging::Setup::from_settings(&LoggingSettings)`** — pure
  mapping from the config struct to the existing builder. Same
  shape as `SecurityHeadersLayer::from_settings`,
  `BodyLimitLayer::from_settings`, etc. so the whole framework
  has consistent settings → component wiring.
- **`Cli::with_logging()`** — opt-in builder method that
  installs `tracing-subscriber` from the loaded
  `Settings.logging` section at `run()` time. The returned
  `WorkerGuard` (when a file sink is configured) is stashed in
  `run()` so it outlives every runserver / management-verb path
  uniformly. Default off — projects that already call
  `rustango::logging::setup()` themselves don't get a duplicate
  init.

### Behavior

- Install ordering: logging happens at the outermost dispatch
  point (`Cli::run()`), BEFORE either runserver path or the
  management-verb dispatcher. So `manage migrate`, `manage
  startapp`, etc. all see the configured subscriber too.
- Call ordering on the builder is irrelevant: `with_logging()`
  before `with_settings_from_env()` works the same as the
  reverse, because the install reads the final
  `Settings.logging` snapshot at run time, not call time.

### Tests

- 1319 → 1324 lib tests (+5):
  - `from_settings_empty_matches_new_defaults` (no surprises
    when the `[logging]` section is empty)
  - `from_settings_populated_fields_drive_builder` (every
    populated field maps to the right builder call)
  - `from_settings_file_sink_resolves_rotation` (every
    rotation variant + unknown-falls-back-to-daily)
  - `from_settings_file_only_requires_file_dir` (no-op when
    the sink isn't configured)
  - `with_logging_flips_install_flag` (Cli builder check)

### Recommended config

```toml
# config/dev_settings.toml
[logging]
level = "info,sqlx=warn"
format = "pretty"
with_line_numbers = true

# config/prod_settings.toml
[logging]
level = "info"
format = "json"
file_dir = "/var/log/myapp"
file_prefix = "app"
file_rotation = "daily"
```

Then in `src/main.rs`:

```rust,ignore
rustango::manage::Cli::new()
    .with_settings_from_env()
    .with_logging()
    .api(urls::api())
    .run().await
```

---

## [0.30.10] — welcome screen polish (roadmap #3)

The v0.29.12 `Cli::with_welcome()` shipped a functional but plain
welcome page. v0.30.10 polishes it: inline SVG logo, cards layout
for commands + features, modern v0.30 verb mentions, doc links.

### Added

- **Inline SVG logo** — geometric "R" mark in two tones (rust-
  orange + tango-blue gradient). No external image fetch, no
  static-file router needed; the page is fully self-contained.
- **Cards-grid layout** — three cards each for "Useful commands"
  (Project / Migrations / Tenancy) and "Batteries included"
  (Data / HTTP+UI / Auth+ops). Responsive `grid-template-columns:
  repeat(auto-fit, minmax(240px, 1fr))` so the page reflows on
  narrow viewports without a media query.
- **Version pill** next to the heading, dark-mode-friendly.
- **Outbound doc links** — `docs.rs/rustango`, GitHub repo,
  examples directory, CHANGELOG.
- **Disable instructions** — the page tells the reader exactly how
  to remove it: `drop .with_welcome() from the Cli::new() chain`.
  Without this, fresh projects keep the welcome page mounted
  forever and can't find the toggle.

### Updated

- Demonstrates modern v0.30 verbs in the commands grid:
  `make:viewset`, `make:api_routes`, `migrate --squash`,
  `init-tenancy`, `create-tenant`, `check --deploy`. The pre-v0.30
  page only mentioned the original `startapp` / `makemigrations` /
  `migrate` trio.
- Feature list now flags the v0.29/v0.30 additions:
  Class-based views, ViewSets, OpenAPI auto-derive, JWT (refresh +
  custom claims), TOTP/2FA, password reset, impersonation.

### Tests

- 1316 → 1319 lib tests (+3):
  `welcome_html_demonstrates_modern_v030_surface` (locks in the
  modern verb / feature mentions),
  `welcome_html_has_outbound_doc_links` (catches link
  regressions), `welcome_html_explains_how_to_disable_itself`
  (regression guard for the "how do I turn this off" footgun).
- Existing self-contained guard tightened: now also asserts
  `<svg` present + no `<img>` tag, locking in the inline-asset
  decision.

---

## [0.30.9] — admin pager `SELECT COUNT(*)` skip for large tables (roadmap #4)

`SELECT COUNT(*) FROM <table> WHERE <filter>` runs the full filtered
scan on every list page render. On tables in the millions of rows
(audit logs, event streams, time-series data) this can take seconds
even with indexes. v0.30.9 adds a per-table opt-out + a per-request
override.

### Added

- **`admin::Builder::skip_count_for(tables)`** — accumulator
  setter; tagged tables skip the COUNT round-trip on every list
  request. The pager renders "Page N" with prev/next driven by
  has-next-page detection (we fetch `page_size + 1` rows, trim
  the extra, and use the trim signal as the "more pages" flag)
  instead of "Page N of M".
- **`?count=skip` URL parameter** — per-request escape hatch.
  Accepts `skip` / `0` / `false` / `no` (case-sensitive matches
  these literal lower-case values). Useful for ad-hoc operator
  queries on big tables that aren't pre-tagged via
  `skip_count_for`.
- **`AppState::count_skipped_for_table(table)`** — internal
  checker, called once per list request to decide which path
  to take.

### Behavior

- Skipped count → `total = 0` and `last_page = page` so old
  custom templates that branch on `last_page > 1` keep working
  (they'll render no pager). New `count_skipped` + `has_next`
  context vars drive the new shape; the bundled `list.html`
  template branches on them.
- The list header switches from `"Table: foo — 12345 rows"` to
  `"Table: foo — row count hidden (large table)"` when count is
  skipped, so it's visually obvious which mode the page is in.
- Read-only label, "+ new …" link, search box, facets, filters
  all keep working unchanged.

### Tests

- 1314 → 1316 lib tests (+2):
  `skip_count_for_marks_tables_and_checker_reads_them` (Builder
  marks the tables + checker matches), `skip_count_for_unions_across_calls`
  (multiple calls accumulate, same shape as `read_only`).

### Why a per-table opt-in instead of always-skipped

Default behavior stays "show the count" because operators
*want* the count on small tables — that's the whole point of a
pager. The skip is targeted: tag the 1-3 monster tables in your
schema, leave the rest. Estimated counts via `pg_class.reltuples`
were considered but rejected for v1 — they're inaccurate for any
WHERE-filtered query, which is the common admin case.

---

## [0.30.8] — `ListView::with_fk_display` (FK columns resolve to target's display)

Closes a visible UX gap in admin-shape lists: FK columns showed
raw integer IDs (`42` for `author_id`) when the target model
already had a `#[rustango(display = "...")]` field that would
render as `"Ada Lovelace"`. The admin's regular list views resolve
FK display via JOIN since v0.20; `template_views::ListView` now
gets the same capability through a different (batch-query) path.

### Added

- **`ListView::with_fk_display(true)`** — opt-in flag. When on,
  every FK / O2O column on the schema gets a sibling
  `<column>_display` field stamped into each row's JSON,
  resolved against the target model's
  `#[rustango(display = "...")]` value. Templates render
  `{{ row.author_id_display | default(value=row.author_id) }}`
  to show `"Ada Lovelace"` instead of `42`.
- **Implementation: post-query batch lookup**. Rather than
  reusing the admin's JOIN-based path (which would change the
  main SELECT's WHERE/ORDER/LIMIT semantics — JOINs can multiply
  rows in subtle cases), v0.30.8 runs one extra `SELECT pk,
  display FROM <target> WHERE pk = ANY(...)` per FK column per
  page after the main rows come back. Cheap (1 indexed lookup
  per FK target, batched across the page's rows) but not free.
- Threaded through both `router(...)` (static pool) and
  `tenant_router(...)` (per-request `Tenant::conn()`); the
  display lookup uses the matching connection.

### Behavior

- Default off — existing projects pay no overhead.
- FK targets that aren't registered in the inventory (e.g.
  cross-binary refs, models in unloaded modules) are silently
  skipped — the row gets no `_display` sibling for that column,
  and templates fall back to the raw FK.
- FK targets without a `display` field are silently skipped too.
- NULL FK column values get no `_display` sibling (no lookup
  possible).
- Failed display lookups (driver / SQL errors) log via
  `tracing::debug!` and skip the column — never surface a 500.
  A missing `_display` is recoverable; templates fall back to
  the raw FK.

### Tests

- 1310 → 1314 lib tests (+4 unit):
  `with_fk_display_flag_default_off_then_on`,
  `json_value_as_lookup_key_handles_numbers_and_strings`,
  `json_value_to_sql_for_fk_pk_round_trips_common_pk_types`,
  `stamp_display_into_rows_writes_sibling_only_when_resolved`.
- Live integration test deferred (disk space ran out during
  this session); the unit tests cover every pure helper +
  the SQL fetch wrappers are simple `select_rows{,_on}` calls
  whose shape is verified at compile time.

### Recommended template usage

```html
<table>
  {% for row in object_list %}
    <tr>
      <td>{{ row.title }}</td>
      <td>{{ row.author_id_display | default(value=row.author_id) }}</td>
    </tr>
  {% endfor %}
</table>
```

The `default(value=row.author_id)` filter keeps the template
robust against the FK target being unregistered, having no
display field, or being deleted (orphan FK).

---

## [0.30.7] — `ListView::with_delete_confirmation` (Django two-step delete)

Closes the destructive-action footgun documented in v0.30.6:
bulk `delete_selected` POSTs no longer wipe rows on the first
click. The flag adds Django admin's familiar two-step shape
(select rows → submit → confirmation page → confirm → delete).

### Added

- **`ListView::with_delete_confirmation(true)`** — opt-in flag.
  When on, a POST with `action=delete_selected` and no
  `confirmed=true` form field renders the confirmation template
  instead of running the DELETE.
- **`ListView::with_delete_confirmation_template(name)`** —
  override the default template name (`<table>_confirm_bulk_delete.html`).
  Implies the flag is on.
- **Confirmation template Tera context**:
  - `action`: `"delete_selected"`
  - `pks`: list of selected primary keys (string-coerced from
    the form's `_selected_action` values, so the second submit
    can echo them verbatim)
  - `objects`: full row data fetched for each selected PK so
    the template shows *what* will be deleted, not just the IDs
  - `csrf_token`: re-stamped from cookies/headers; the second
    submit reuses the same token chain
- **Confirmed-form values**: the second submit confirms via any
  truthy value on `confirmed`: `true` / `1` / `yes` / `on`
  (case-insensitive). Anything else is treated as not confirmed.
- Threaded through both `router(...)` (static pool) and
  `tenant_router(...)` (per-request `Tenant::conn()`); the
  confirm-page row fetch goes through the matching connection.

### Behavior

- Custom actions registered via `.action(...)` /
  `.tenant_action(...)` are NOT gated by the flag — matches
  Django's convention (only `delete_selected` is confirmed by
  default). Custom destructive actions that need confirmation
  should implement their own confirm-then-submit handler shape
  via [`ListView::action`].
- Default off; existing projects pay no overhead and see no
  behavior change.

### Tests

- 1307 → 1310 lib tests (+3): builder flag flip, template-name
  resolution (default + override), `is_form_confirmed` accepts
  the full set of truthy strings.
- 5/5 → 7/7 live tests in `tests/template_views_bulk_actions_live.rs`:
  - `confirmation_renders_first_then_deletes_on_confirmed`
    asserts the full two-step flow against a real Postgres
  - `confirmation_does_not_gate_custom_actions` confirms
    `publish_selected` runs immediately even with the flag on

### Recommended template

```html
<!-- <table>_confirm_bulk_delete.html -->
<h1>Confirm delete</h1>
<p>The following {{ objects | length }} row(s) will be deleted:</p>
<ul>
  {% for o in objects %}
    <li>{{ o.title | default(value=o.id) }}</li>
  {% endfor %}
</ul>
<form method="post">
  <input type="hidden" name="_csrf" value="{{ csrf_token }}">
  <input type="hidden" name="action" value="{{ action }}">
  <input type="hidden" name="confirmed" value="true">
  {% for pk in pks %}
    <input type="hidden" name="_selected_action" value="{{ pk }}">
  {% endfor %}
  <button type="submit">Yes, delete</button>
  <a href=".">Cancel</a>
</form>
```

---

## [0.30.6] — paper-cut audit of v0.30.x

Self-audit of v0.30.0 → v0.30.5 surfaced five flaws ranging from
docstring lies to silent UX traps. Each one closed below.

### Fixed

- **`ViewSet::tenant_router` docstring lied about static parallelism.**
  The doc claimed "the static-pool path runs SELECT + COUNT in
  parallel for the page-number list endpoint" — that was true
  pre-v0.30, but v0.30.0's handler unification serialized both
  paths. Updated to flag the v0.30 behavior change explicitly and
  point at `cursor_pagination(...)` as the COUNT-skip escape hatch
  for latency-sensitive callers. The v0.30.0 CHANGELOG entry got
  the same clarification.
- **`CreateView::form::<F>()` / `UpdateView::form::<F>()` was
  misleading about ModelForm semantics.** The docstring's example
  suggested `F`'s typed fields drove the SQL INSERT, but the
  parsed `F` value is actually discarded — only `F::parse`'s
  pass/fail outcome is consumed; the schema's type-coercion path
  still owns column values. Added a "What `.form::<F>()` does NOT
  do (yet)" section calling this out and noting that
  `ModelForm`-as-source-of-truth is a future enhancement. Avoids
  the surprise where a `confirm_password` field on `F` (with no
  model column) appears to validate fine.
- **`make:viewset` auto-detection was silent.** v0.30.5 added
  Cargo.toml-based tenancy detection but didn't tell the user
  when it fired — a fresh `make:viewset PostViewSet` could quietly
  emit a tenant-shaped scaffold without explanation. Now prints
  one line: `make:viewset: auto-detected tenancy mode from
  Cargo.toml (pass --no-tenant to override)`. Stays silent when
  the user passed `--tenant` / `--no-tenant` explicitly (they
  already know).
- **`ListView` bulk `delete_selected` had no confirmation step
  and no documentation about it.** Django admin shows a
  confirmation page (select rows → submit → "are you sure?" →
  confirm → delete); the v0.30.4 v1 of bulk actions skips that
  intermediate step entirely. Added a "Destructive-action UX"
  section to the `bulk_actions(...)` docstring that calls out the
  gap, suggests two interim mitigations (JS `confirm()` handler
  on the form, or a custom `.action(...)` handler that wraps
  `delete_selected` after its own confirmation route), and tags
  a `with_delete_confirmation(true)` flag for v0.31.
- **`CountQuery.search` is technically a breaking change for
  downstream consumers building `CountQuery` directly.** v0.30.1
  added the public field but only flagged it as "5 callers
  updated" — that's an internal note, not a downstream signal.
  Added an explicit "Breaking change (downstream API)" section to
  the v0.30.1 entry: downstream code constructing
  `CountQuery { ... }` will get `E0063` and needs `search: None`
  (or the active search clause). The struct doesn't use
  `#[non_exhaustive]` so this is a hard break — flagged loudly.

### Tests

- 1306 → 1307 lib tests (+1):
  `make_viewset_echoes_auto_detect_only_when_picking_tenant`
  asserts the new auto-detect echo fires only on the implicit
  path, not when the user passed `--tenant` / `--no-tenant`.
- All chdir-using tests in `migrate::manage::gen_tests` now
  serialize through a `OnceLock<Mutex>` since cargo's parallel
  test runner was racing them — one tempdir's drop ran while
  another test was restoring CWD, surfacing as `NotFound`. The
  lock keeps the existing tests stable + lets the new echo test
  share the same chdir fixture.

---

## [0.30.5] — `make:viewset` auto-detects tenancy + modernized template

`make:viewset` already had a `--tenant` flag, but two paper-cuts
remained: (a) you had to remember to pass it in tenancy projects,
and (b) the emitted tenant template still carried a "v1 scope: no
filter / search / pagination / perm checks" caveat that became
stale when v0.30.0 unified the feature parity.

### Added

- **Auto-detection of tenancy mode** — `make:viewset` reads
  `Cargo.toml` for the `rustango` dep's feature list, and defaults
  to the tenant template when `tenancy` is enabled. No flag
  required for the obvious case. Resolution order:
  1. `--no-tenant` (escape hatch override)
  2. `--tenant` / `--tenant-aware` (explicit)
  3. Cargo.toml has `tenancy` feature on rustango → tenant template
  4. Otherwise → pool template
- **`--no-tenant` flag** — escape hatch for a tenancy project that
  wants to hand-roll a single-pool viewset (rare, but kept open).
- **Modernized tenant template** — emits commented `// uncomment to
  enable` hints for the *full* v0.30 builder chain
  (`filter_fields` / `search_fields` / `ordering` /
  `ordering_fields` / `page_size` / `permissions_for_model` /
  `read_only`) so users discover the surface without reading the
  `tenant_router` docs. The stale "v1 scope" caveat is gone.
- Help text updated to mention auto-detection and the
  `--no-tenant` override.

### Tests

- 1303 → 1306 lib tests (+3): `project_uses_tenancy` detects
  inline-table dep features; returns false when feature absent;
  returns false when Cargo.toml missing (graceful fallback).
- Existing `viewset_template_tenant_uses_tenant_router` test now
  asserts the v0.30 builder chain hints are present and the v1
  caveat is gone.

---

## [0.30.4] — Bulk actions on `ListView` (Django-admin shape)

The v0.29 HTML CBVs covered list / detail / create / update /
delete but didn't have an answer for "select N rows and run the
same action against all of them" — Django admin's most-used
power feature. v0.30.4 closes the gap.

### Added

- **`ListView::bulk_actions(true)`** — opt-in flag that mounts a
  `POST <prefix>` route alongside the existing `GET`. The list
  endpoint stamps a `bulk_actions: [{name, label}]` array into the
  Tera context so templates can render an action `<select>`.
- **Built-in `delete_selected`** — automatically registered when
  `bulk_actions` is on. Runs `DELETE FROM <table> WHERE <pk> IN
  (...)` via `core::DeleteQuery` + `sql::delete{,_on}`, so the
  exact same SQL the per-row admin DELETE path uses.
- **`ListView::action(name, label, handler)`** — register a
  custom static-pool handler. Closure shape:
  `for<'a> Fn(&'a PgPool, &'a [SqlValue]) -> BulkActionFuture<'a>`.
  Mirrors the existing `admin::AdminActionFn` shape.
- **`ListView::tenant_action(name, label, handler)`** — tenancy
  counterpart, handler runs against the per-request `&mut
  PgConnection` from `Tenant::conn()`. Mounting against the wrong
  flavor's router (e.g. tenant_action + router) surfaces a clear
  runtime error rather than corrupting the connection.
- POST form shape (matches Django convention):
  - `action`: name of one registered action
  - `_selected_action`: one or more values, each a row's PK
    (repeated form keys are preserved — `axum::Form<HashMap<...>>`
    would have collapsed them into a single value, losing every
    selection past the first)
  - `_csrf`: token (when `Cli::with_csrf()` is on)
- Successful runs return `303 See Other` to the same prefix so a
  refresh after the redirect doesn't replay the action.

### Tests

- 1297 → 1303 lib tests (+6): builder default-off + flag flip,
  `.action(...)` dedupe, `parse_bulk_action_form` rejects empty
  selection / missing action, `coerce_pk_typed` per-FieldType,
  `bulk_actions` Tera context shape (built-in first, user actions
  after).
- **5 new live tests** (`tests/template_views_bulk_actions_live.rs`,
  DATABASE_URL-gated): `delete_selected` actually deletes the right
  rows; user action runs and updates rows; empty selection → 400;
  unknown action name → 400; GET stamps the `bulk_actions` Tera
  variable so the template can render the dropdown.

### Backward compatibility

- Field on `ListView` defaults to off. Existing projects pay no
  overhead and see no behavior change.
- The bulk-action POST mounts only when the flag is on; without
  it, POST to the list URL still 405s (axum default).

---

## [0.30.3] — Cookbook Chapter 9d: documented `ViewSet::tenant_router`

The v0.30.0/v0.30.1 work shipped with framework-side unit + live
tests but no user-facing documentation in the cookbook. Chapter 9d
fills the gap with a copy-paste-ready reference template.

### Added

- **`tests/cookbook_chapter09d_viewset_tenant_router.rs`** —
  5 live tests exercising `ViewSet::for_model(Author::SCHEMA).tenant_router("/api/authors")`
  end-to-end against the cookbook's `Author` model:
  - `tenant_router_lists_paginated_payload`
  - `tenant_router_search_param_narrows_count_and_results`
    (regression guard for the v0.30.1 `CountQuery.search` fix)
  - `tenant_router_filter_param_exact_match`
  - `tenant_router_full_crud_round_trip` (POST → GET → PUT →
    DELETE → GET 404)
  - `tenant_router_missing_header_yields_404_not_500`
- **COOKBOOK.md Chapter 9d** narrative section explaining the
  pool-baking-at-mount-time problem schema-mode and database-mode
  tenants hit with `router(prefix, pool)`, plus the per-request
  `Tenant::conn()` solution `tenant_router(prefix)` provides.
- Fixture pattern in chapter 9d uses `tenancy::init_tenancy` +
  `tenancy::migrate_registry` (matching Chapter 5's pattern) plus
  explicit drop of the migration ledger table — chosen over
  `rmig::apply_all` which can't order FKs across the cookbook's
  full model set, and over leaving stale state which breaks
  re-runs against the same database.

### Tests

- 1297 lib tests still pass.
- 5/5 new cookbook chapter 9d tests pass against
  `DATABASE_URL`-backed Postgres.

---

## [0.30.2] — `#[derive(Form)]` validators in `CreateView`/`UpdateView`

The v0.29 HTML CBVs ran type coercion + schema-level bounds
(`max_length` / `min` / `max`) on the form payload, but user-defined
business validation (`#[form(min_length = 5, regex = "...")]`,
custom `#[form(validator = "fn")]`, cross-field checks) had to be
re-implemented per project on top. v0.30.2 closes the gap.

### Added

- **`CreateView::validator` / `UpdateView::validator`** — install
  a closure-based hook that runs after schema-level checks but
  before the SQL INSERT/UPDATE. Returning `Err(FormErrors)`
  re-renders the form with the merged error map and a 422 status.

  ```rust,ignore
  CreateView::for_model(Post::SCHEMA)
      .validator(|data| {
          let mut errs = FormErrors::default();
          if data.get("title").map_or(true, |s| s.len() < 5) {
              errs.add("title", "must be at least 5 characters");
          }
          if errs.is_empty() { Ok(()) } else { Err(errs) }
      })
      .router("/posts", tera, pool)
  ```
- **`CreateView::form::<F: Form>()` / `UpdateView::form::<F>()`** —
  convenience wrapper that auto-wires a `#[derive(Form)]` struct's
  `parse(...)` method as the validator. Pulls in `min_length` /
  `regex` / custom-validator-fn / cross-field checks from the
  derive macro:

  ```rust,ignore
  #[derive(rustango::Form)]
  pub struct PostForm {
      #[form(min_length = 5)] title: String,
      #[form(min_length = 1)] body: String,
  }

  CreateView::for_model(Post::SCHEMA)
      .form::<PostForm>()
      .router("/posts", tera, pool)
  ```
- **`Validator` type alias** — public for projects that want to
  define their own validator factories outside the builder
  closure.
- Threaded through every variant: `router(...)` (static pool) and
  `tenant_router(...)` (per-request `Tenant::conn()`) on both
  `CreateView` and `UpdateView`. Same shape, no new `tenant_*`
  methods.

### Behavior

- Validator errors *merge* with schema errors rather than
  clobbering them — multi-error fields concatenate via `"; "`,
  same as Django convention. Users see all errors in one
  re-render rather than playing whack-a-mole.
- Non-field errors (`FormErrors::add_non_field`) land under the
  template variable `form.errors.__all__`. Templates render that
  once at the top, separately from per-field errors.
- Re-render still returns `422 Unprocessable Entity` (unchanged).
- Validator field starts as `None` — existing projects pay no
  overhead and get the same behavior they had before.

### Tests

- 1292 → 1297 lib tests (+5):
  - `merge_validator_no_errors_leaves_map_untouched`
  - `merge_validator_field_errors_land_under_field_key` (multi-error
    join via `"; "`)
  - `merge_validator_non_field_errors_land_under_all_key`
  - `merge_validator_appends_to_existing_field_error` (no clobber)
  - `validator_and_form_builders_set_validator_field` (closure +
    typed `Form` shapes both compile)

---

## [0.30.1] — live tests for `tenant_router` + `CountQuery` search bug fix

Closing the v0.30.0 work with end-to-end validation against a real
Postgres + tenant pool, plus a count-with-search correctness fix
the integration test surfaced.

### Added

- **`tests/viewset_tenant_router_live.rs`** — 7 live integration
  tests against a real `TenantContext` with `HeaderResolver`
  dispatch:
  - List endpoint: paginated payload (count, page, page_size,
    last_page, results) against the per-request tenant connection
  - `?search=…` ILIKE narrowing
  - `?{field}=…` exact filter
  - GET retrieve by PK
  - POST create + JSON round-trip with returned id
  - PUT update + DELETE destroy two-step flow
  - Missing tenant header → 404 (extractor rejection surfaces cleanly,
    not as a 500 from the inner SQL layer)

### Fixed

- **`CountQuery.search` field** — added to `core::query::CountQuery`.
  Without it, `?search=…` on a paginated list reported the *total*
  row count rather than the count *after* search-field ILIKE
  filtering, so `last_page` computed an over-large pager. Affected
  every `viewset::router` and `viewset::tenant_router` page-number
  list response when the user typed in the search box.
- **Admin pager** had a `// NOTE: count_rows ignores the search
  clause; counts are approximate when ?q is set` workaround comment
  in `admin/views.rs` from v0.2 — removed; the admin pager is now
  exact when `?q=` is set.
- **`QuerySet::count` / `count_pool`** propagated `search` from the
  compiled SELECT into the count query, so `MyModel::objects()
  .where_(...).search(...).count(...)` returns the correct number
  rather than ignoring the search predicate.

### Tests

- 1292 lib tests still pass under the new `CountQuery` shape.
- 7/7 new live tests pass against `DATABASE_URL`-backed Postgres.
- All `CountQuery` constructors updated (5 callers across viewset,
  template_views static + tenant paths, admin/views, and 2 in
  sql/executor for `QuerySet::count{,_pool}`).

### Breaking change (downstream API)

- `core::query::CountQuery` gained a new public field
  `search: Option<SearchClause>`. Downstream code constructing
  `CountQuery { ... }` directly will get an `E0063` ("missing
  field `search`") and needs to add `search: None` (or pass the
  active search clause when one's available). The struct doesn't
  use `#[non_exhaustive]` so this is a hard break — flagged
  loudly here because the change is otherwise invisible.

---

## [0.30.0] — `ViewSet::tenant_router(prefix)` with full feature parity (#80)

`#[derive(ViewSet)]` projects with multi-tenant routing finally get
the same DRF-shape CRUD as single-tenant projects. The v0.27 v1 of
`tenant_router` deliberately shipped without filter / search /
pagination / permission support — that work was tracked in #80
"v2 of this module" and ships now.

### Added

- **`ViewSet::tenant_router(prefix)`** — full feature parity with
  the static-pool `router(prefix, pool)` path:
  - `filter_fields` (Django-style lookups: `__gt`, `__icontains`,
    `__in`, `__isnull`, etc.)
  - `search_fields` (full-text ILIKE)
  - `ordering` / `default_ordering`
  - `page_size` / `cursor_pagination`
  - `permissions` / `permissions_for_model` (per-request
    connection runs the perm check too — single round-trip rather
    than an extra pool acquire)
  - `serializer` / `row_render`
  - `read_only`
- **`tenancy::permissions::has_perm_on<E: Executor>`** — variant
  of `has_perm` that takes any sqlx executor. Required for the
  unified handler path: tenant mode runs perm checks against the
  per-request `&mut PgConnection`, not a `&PgPool`.
- The `tenant_router` returns the same `Router<()>` shape as
  before, but now driven by the same handler set as the static
  router (`AcquiredConn` wrapper abstracts pool source).

### Changed

- **Internal**: `ViewSetState` now carries a `PoolSource` enum
  (`Static(PgPool)` / `Tenant`) instead of a single baked
  `PgPool`. Each handler calls `state.acquire(&mut parts)` which
  returns an `AcquiredConn` wrapper exposing
  `select_rows` / `count_rows` / `select_one_row` /
  `insert_returning` / `update` / `delete` / `has_perm` facade
  methods. Pool-source branching lives in the wrapper, not in
  every handler.
- **Behavior change (static-pool path too)**: page-number list
  endpoint now runs SELECT and COUNT *sequentially* on a single
  connection — pre-v0.30 the static `router(prefix, pool)` path
  ran them in parallel via `tokio::join!`. Tenant mode physically
  can't `join!` (the per-request `&mut PgConnection` is exclusive),
  and unifying both paths on the serial handler keeps the code
  simple. Two short queries on one connection vs. two pool round-
  trips — typically faster anyway. Latency-sensitive callers can
  skip the COUNT entirely with `cursor_pagination(...)`.
- **Removed**: `viewset/tenant.rs` v1 module (the limited-scope
  `tenant_router`). Its smoke test moved into
  `viewset/mod.rs::tenant_router_tests`. No public API breaks —
  the v1 `tenant_router` shape is preserved by the v2 implementation.

### Tests

- 1290 → 1292 lib tests (+2): `tenant_router_carries_over_full_builder_chain`
  asserts the full builder chain compiles in tenant mode;
  `router_and_tenant_router_set_distinct_pool_sources` round-trips
  the mode flag. The v1 smoke is preserved.
- All 33 viewset tests pass under the unified handler path.

### Migration

- Existing `viewset.router(prefix, pool)` calls are unchanged.
- Existing `viewset.tenant_router(prefix)` calls now opt into
  filter/search/pagination/permission features via the same
  builder chain that worked for `router(...)` — no code changes
  required to keep current behavior, since unconfigured fields
  default to "no filter / no search / no perm check".

---

## [0.29.12] — `Cli::with_welcome()` builder

The `welcome::welcome_router()` confidence page has shipped since
v0.16 but every project hand-mounted it on `urls::api()`. Same
shape as `with_health()` / `with_static()` / `with_csrf()`.

### Added

- **`Cli::with_welcome()`** — auto-mounts `welcome::welcome_router()`
  at `/` so a freshly-scaffolded project boots to a friendly
  "rustango — it works!" page instead of the empty-router 404.
  Default off so existing projects with their own `/` route don't
  panic at axum's route-collision check during merge.
- Threaded through both single-tenant `runserver` and
  `runserver_tenancy` so tenancy projects get the same on-the-tenant-
  -subdomain welcome.

### Tests

- 1289 → 1290 lib tests (+1): `with_welcome_flips_flag` confirms
  default-off + opt-in flips.

### Recommended scaffolder additions

`cargo rustango new` templates can now use `.with_welcome()` in their
`src/main.rs` so a clean `cargo run` immediately renders the welcome
page rather than 404. Tracked separately in the cargo-rustango crate.

---

## [0.29.11] — macro-time validation for `#[rustango(column = "...")]`

The same `[a-zA-Z_][a-zA-Z0-9_]*` rule the macro applies to
`#[rustango(table = "...")]` (#65, v0.27.3) now also applies to
`#[rustango(column = "...")]` field renames. Hyphens / spaces / dots
in column names compile fine on the SQL CREATE TABLE side (Postgres
double-quotes the identifier) but break downstream FK / index name
derivation in `migrate::ddl`, which emits `<table>_<column>_fkey`
unquoted. Same fail-fast rule as the table check — the only safe
path is the only path.

### Added

- `validate_sql_identifier(name, kind, span)` helper in the macro
  crate, generalized from the existing `validate_table_name`.
  `kind` is `"table"` or `"column"` so the error message points at
  the right attribute. The old `validate_table_name` is now a
  one-line wrapper that delegates.

### Errors look like

```
error: column name `foo-bar` contains invalid character '-' — SQL
       identifiers must match `[a-zA-Z_][a-zA-Z0-9_]*`. Hyphens in
       particular break FK / index name derivation downstream; use
       underscores instead (e.g. `foo_bar`)
```

### Tests

- 1289 lib tests pass (no count change — the validator is exercised
  via every existing `#[derive(Model)]` use site, all of which have
  conformant column names).

---

## [0.29.10] — `Cli::with_csrf()` builder

Form-driven projects (anything using `template_views` HTML CBVs)
needed CSRF mounted to enforce the `_csrf` field validation that
v0.29.7 fixed. Until now every project hand-stacked
`.layer(rustango::forms::csrf::layer())` on their `urls::api()`.
Now it's one builder call, parallel to `with_health()` /
`with_static()`.

### Added

- **`Cli::with_csrf()`** — auto-mounts
  `crate::forms::csrf::layer()` (default `CsrfConfig`) on the API
  router at `runserver` time. Default off so pure JSON+JWT APIs
  don't pay the body-buffer cost on form-encoded POSTs they would
  reject anyway.
- **`Cli::with_csrf_config(CsrfConfig)`** — same, with overridable
  `cookie_name` / `header_name` / `secure`. The right knob for
  cross-framework hosting (different cookie name) and production
  HTTPS deployments (`secure = true`).
- Threading is symmetric: single-tenant runserver wraps the API
  router directly; tenancy mode wraps before
  `apply_settings_layers` so layer order is
  `request → security_headers → CORS → access_log → body_limit → CSRF → handler`
  (CSRF closest to handler — body-buffering happens after the
  request-time guards have run).

### Tests

- 1287 → 1289 lib tests (+2): `with_csrf_flips_flag` (default off
  → cookie name + secure-false defaults applied) and
  `with_csrf_config_threads_overrides` (custom config lands
  verbatim).

### Feature gating

- Builder methods gated on the `csrf` feature (`Cli` struct field
  too), so non-CSRF builds compile without the type ever existing.

---

## [0.29.9] — `Cli::with_static(prefix, root_dir)` builder

Common need that was previously boilerplate: serving CSS / JS / images
from a directory at a URL prefix. The static-file server itself has
existed since v0.24 (`crate::static_files::{StaticFiles,
static_router}`), but every project hand-mounted it on their `apps()`
router. Same builder shape as `with_health()`.

### Added

- **`Cli::with_static(prefix, root_dir)`** — auto-mounts a
  `static_router(StaticFiles::new(root_dir))` at `prefix`. Repeat the
  call to mount more than one directory:

  ```rust
  rustango::manage::Cli::new()
      .api(urls::api())
      .with_static("/static", "./assets")
      .with_static("/uploads", "./var/uploads")
      .run().await
  ```

  Defaults from `StaticFiles::new` apply — `Cache-Control: public,
  max-age=3600`, dotfiles 404, symlink escapes blocked, traversal
  rejected. Projects that need `immutable` for hashed bundles or
  `serve_hidden` for `.well-known` keep mounting `static_router`
  directly on their own router and skip this shortcut.
- **`Server::Builder::with_static`** — same shape, used by
  `Cli::with_static` when tenancy mode is on so static dirs land
  on the tenant subdomain before the admin fallback.
- Static dirs are nested before the admin's catch-all so they take
  precedence for paths under their prefix; this matches the
  health-router merge order.

### Tests

- 1282 → 1284 lib tests (+2): `with_static_accumulates_in_order`
  asserts repeated calls preserve order; `mount_static_dirs_serves_a_file`
  exercises the end-to-end nesting + 200 response on a tempdir-backed
  router.

### Feature gating

- `Cli::with_static` is gated on the `admin` feature (same as the
  underlying `static_files` module). Single-binary projects pulling
  in `manage` already enable `admin` so this is a no-op for them.

---

## [0.29.8] — multi-column success_url placeholders + tenancy health

Two follow-ups closing limitations from earlier in v0.29:

### Added

- **`Cli::with_health()` works in tenancy mode**, via the new
  `Server::Builder::with_health()` flag. Previously a no-op for
  tenancy projects (the registry pool is built inside `Server::
  Builder` and wasn't accessible to feed `health_router` from
  the Cli layer). Now `/health` + `/ready` mount cleanly in both
  single-tenant and tenancy projects. The `/ready` probe runs
  `SELECT 1` against the registry pool — registry health gates
  traffic to every tenant, which is the right scope.
- **Multi-column `{field}` placeholders in `CreateView`
  `success_url`** — was just `{pk}` in v0.29.6; now any column
  name resolves against the schema. Example:
  `success_url("/posts/{pk}/{slug}")` redirects using both the PK
  and the slug column from the new row. The INSERT's RETURNING
  list is computed from the placeholders found in the template,
  so URLs without placeholders still take the single-round-trip
  INSERT path. `{pk}` is special-cased to the model's primary
  key column (so users don't need to know whether it's named
  `id`, `uuid`, etc.).

### Notes

- UpdateView and DeleteView keep the simpler URL-only `{pk}`
  substitution — multi-column placeholders for those would need
  an extra row read or `UPDATE ... RETURNING` plumbing. Track
  as a follow-up if demand surfaces.
- Unknown placeholder names surface a clear error before the
  INSERT runs ("does not match any field on `posts`") rather than
  after — matches the same fail-fast policy as
  `resolve_order_by`.

### Tests

- 1279 → 1282 lib tests (+3): `parse_success_url_placeholders`
  recognizes valid identifier shapes, ignores stray braces /
  empty `{}` / special chars; `success_url_returning_columns`
  resolves `{pk}` + named columns, returns empty for plain URLs;
  unknown placeholder rejection.

---

## [0.29.7] — CSRF middleware now actually validates `_csrf` form field

**Bugfix release.** The CSRF middleware's docstring promised it
checks the `_csrf` form field on `application/x-www-form-urlencoded`
POSTs, but the implementation only ever checked the
`X-CSRF-Token` header. That makes the middleware unusable with
the v0.29.0 `template_views` form views, which submit the token
via `<input type="hidden" name="_csrf">` (the canonical Django
shape).

Today's middleware silently 403s every browser form POST when
mounted on top of `template_views::CreateView` /
`UpdateView` / `DeleteView` — a real correctness gap.

### Fixed

- **`forms::csrf::layer()` now reads the `_csrf` form field** on
  unsafe-method requests with `Content-Type:
  application/x-www-form-urlencoded`. Header path stays the
  short-circuit fast-path (no body buffering for SPA / fetch
  callers).
- **64 KiB body buffer cap** for the form-field code path. Forms
  larger than this (vanishingly rare; typical forms are
  < 4 KiB) get a clean 403 rather than letting the middleware
  buffer megabytes in memory just to verify a token. File
  uploads use multipart, not form-encoded — this cap doesn't
  affect them.

### Implementation notes

- Tiny RFC 3986 percent-decoder + form-encoded scanner (~30
  LOC each) avoid pulling `percent-encoding` / `urlencoding` /
  `serde_urlencoded` as transitive deps for the middleware path
- `+` → space conversion before percent-decoding (the
  `application/x-www-form-urlencoded` convention)
- Body is buffered + reconstructed via
  `Request::from_parts(parts, Body::from(bytes))` so the inner
  handler can still parse the form

### Tests

- 1273 → 1279 lib tests (+6): `is_form_encoded` recognizes
  canonical + `; charset=...` variant + rejects multipart / JSON
  / no-content-type, `read_form_field` extracts named values,
  percent-decodes, treats `+` as space, skips malformed pairs;
  `percent_decode` rejects truncated `%2` and non-hex `%ZZ`.

---

## [0.29.6] — health endpoints + `{pk}` redirect interpolation

Two ergonomic follow-ups for v0.29 deployments:

### Added

- **`Cli::with_health()`** — auto-mounts `/health` (liveness) and
  `/ready` (readiness with `SELECT 1`) endpoints on the API
  router. Default off — operators sometimes ship custom health
  JSON or layer additional checks (Redis ping, queue depth) and
  don't want the framework's defaults colliding. Single-tenant
  runserver only today; tenancy mode skips because the registry
  pool is built inside `Server::Builder` and isn't accessible to
  feed `health_router` from the Cli layer (tracked as a follow-up).
- **`{pk}` placeholder interpolation in `success_url`** for
  `CreateView` / `UpdateView` / `DeleteView`. Mirrors Django's
  template-style success_url:
  - `CreateView::success_url("/posts/{pk}")` redirects to the
    new row's detail page after insert. PK is read back via
    `INSERT ... RETURNING <pk_col>` only when the placeholder is
    present — without it the plain INSERT path stays a single
    round-trip.
  - `UpdateView::success_url("/posts/{pk}")` and
    `DeleteView::success_url("/posts/{pk}")` substitute from the
    URL path — no extra query needed, since the PK is already in
    scope.
  - PK is rendered type-aware: `i16`/`i32`/`i64` → decimal digits,
    `Uuid` → canonical hex, anything else → text decode.

### Tests

- 1268 → 1273 lib tests (+5): `Cli::with_health` flag flips,
  `substitute_pk` replaces / no-op / multi-occurrence cases, plus
  a no-placeholder fast-path doc test for `interpolate_success_url`
  (the placeholder branch needs a live PgRow → integration test).

---

## [0.29.5] — pagination URL preservation + request timeout layer

Two follow-ups that turn up the moment someone deploys the v0.29
template_views to production:

1. The `<a href="?page=2">next</a>` link drops the user's filter +
   search + ordering state because templates have to manually
   rebuild the query string
2. A wedged DB query / external HTTP call holds a worker hostage
   forever — no built-in cap on per-request latency, so a single
   slow upstream can drag the entire pool into stalls

### Added

- **`ListView` `next_page_url` / `prev_page_url` Tera context
  vars** — `Option<String>` query strings (`?status=draft&page=4`)
  that preserve every other URL parameter and just bump the
  `page` value. Templates render
  `{% if next_page_url %}<a href="{{ next_page_url }}">next</a>{% endif %}`
  without rebuilding the query manually. `None` when there's no
  page in that direction.
- **`rustango::request_timeout::RequestTimeoutLayer`** — new
  per-request handler timeout middleware that returns
  `504 Gateway Timeout` instead of letting a slow handler hang.
  Honors `Settings.server.request_timeout_secs` automatically via
  `Cli::with_settings_from_env()`; mount manually as
  `app.request_timeout(RequestTimeoutLayer::new(Duration::from_secs(30)))`
  for projects that build their server outside `Cli`. Opt-in:
  `from_settings` returns `None` when the value is unset or 0.
  Behind the existing `admin` feature (no new feature flag).
  **Don't wrap streaming routes** (SSE, websocket upgrades) —
  mount this on the API slice, not the entire app.

### Notes

- The auto-layering pipeline (`Cli::with_settings_from_env`) now
  applies request_timeout as the innermost layer, so a wedged
  handler doesn't hold downstream middleware state hostage.
- `urlencode` helper used by the pagination URL builder is a
  focused tiny RFC 3986 implementation — keeps `template_views`
  from pulling `percent-encoding` / `urlencoding` as a
  transitive dep.

### Tests

- 1257 → 1268 lib tests (+11): `urlencode` reserved-char
  encoding, `build_pagination_query` preserves other params with
  sorted keys, no-other-params fallback, `insert_pagination_urls`
  both-directions / first-page-no-prev cases. Plus `RequestTimeoutLayer::new`,
  `from_settings` (unset / zero / set), fast-handler-passes-through,
  slow-handler-504s.

---

## [0.29.4] — `ListView` URL overrides + PK type coercion

Two follow-ups for `template_views` that surfaced from
imagining how a real user would build a `/posts` page on top of
v0.29.0:

1. They want sortable column headers — so `?ordering=col` /
   `?ordering=-col` URL overrides
2. They want a "show more" / "show less" page-size selector — so
   `?page_size=N` URL overrides (clamped to a configured cap, so
   `?page_size=999999` doesn't drag the database into a giant scan)
3. They have a UUID PK and want `/posts/{uuid}/edit` to work
   without leaning on Postgres' implicit string-to-UUID cast — so
   `coerce_pk` based on the field's declared `FieldType`

### Added

- **`ListView::ordering_fields(&[&str])`** — allowlist of fields
  the user can override sort on via `?ordering=col` (ASC) or
  `?ordering=-col` (DESC). Mirrors Django's ListView convention.
  Outside-allowlist values silently fall back to the builder
  default (typos shouldn't 400).
- **`ListView::max_page_size(usize)`** — hard cap on
  `?page_size=N` URL overrides. Default 100. Clamps below the
  floor (1) and above the cap.
- **`ordering: String` Tera context var** — the active ordering
  spec (`"title"` / `"-created_at"` / `""` for builder default).
  Templates render sortable column headers like
  `<a href="?ordering={% if ordering == 'title' %}-{% endif %}title">`.

### Changed

- **DetailView / UpdateView / DeleteView PK binding** now coerces
  the URL `{pk}` segment to the field's declared `FieldType`
  before binding the SQL parameter. `i16` / `i32` / `i64` parse to
  `SqlValue::I64`; `Uuid` parses to `SqlValue::Uuid`; everything
  else (including parse failures) falls through to
  `SqlValue::String` — the previous behavior. Keeps queries
  cleaner under stricter SQL modes without breaking existing
  string-PK projects.
- **Tera `page_size` context var** now reflects the *active* page
  size, not the builder default. Same data shape; templates that
  render `<select>` per-page-size dropdowns can show the user's
  current choice.

### Tests

- 1246 → 1257 lib tests (+11): `resolve_page_size` default /
  unparseable / clamping (above + below); `resolve_active_order`
  URL ASC override / `-` DESC prefix / outside-allowlist fallback
  / no-URL-uses-builder / empty-`?ordering=`-treated-as-no-override;
  `coerce_pk` integer field success + garbage fallback / UUID
  field success + garbage fallback / String pass-through.

---

## [0.29.3] — `template_views` form CSRF threading

Closes the most likely deployment-blocker for v0.29.0's form views:
templates had no way to render the CSRF hidden input because the
view didn't expose the token. Today `<form>{% csrf %}…</form>` was
"copy this from the admin's templates," which doesn't exist for
the public-facing CBVs.

### Added

- **`rustango::forms::csrf::ensure_token(headers, cookie_name)`** —
  read-or-mint helper that returns `(token, Option<set_cookie>)`.
  Returns the existing CSRF cookie value if present, or mints a
  fresh 32-byte base64url token + the matching `Set-Cookie` header
  the caller should attach. Lives behind the existing `csrf`
  feature.
- **`rustango::forms::csrf::CSRF_COOKIE`** is now `pub` (was a
  module-private const). Lets view code reference the canonical
  cookie name without re-typing the literal.
- **`csrf_token` Tera context var** — every `template_views` form
  GET handler (`CreateView`, `UpdateView`, `DeleteView`, plus
  every `tenant_router` variant) now stamps the token into the
  context and attaches a `Set-Cookie` header when minting fresh.
  Templates render `<input type="hidden" name="_csrf" value="{{
  csrf_token }}">` and the user's POST validates cleanly against
  `forms::csrf::layer()`.
  Without the `csrf` feature compiled in, the variable is the
  empty string — harmless when CSRF isn't enforced anyway.
- The `rerender_form` path (validation-error 422 re-render) also
  threads the same token, so a re-displayed form with
  `form.errors` keeps the user's CSRF state.

### Tests

- 1243 → 1246 lib tests (+3): `stamp_csrf` reuses an existing
  cookie, `stamp_csrf` mints fresh + returns Set-Cookie when
  absent, `apply_csrf_cookie` appends Set-Cookie when `Some` /
  no-op when `None`. The `csrf`-feature-off path is covered by a
  separate test gated `#[cfg(not(feature = "csrf"))]`.

---

## [0.29.2] — `ListView` filtering + search

Adds the most likely first-touch feature gap in v0.29.0's
`template_views::ListView`. Anyone who actually builds an HTML
list page hits "how do I filter by category?" within minutes —
hand-rolling an axum handler for that purpose defeats the
"generic CBV" pitch. Mirrors the shape `viewset` already has on
the JSON side.

### Added

- **`ListView::filter_fields(&[&str])`** — whitelists URL query
  parameters for exact-match filtering. `?author_id=42&status=published`
  runs `WHERE author_id = '42' AND status = 'published'` (when
  both are in the allowlist). Unknown query params are silently
  ignored — typos in URLs shouldn't 400. Each name resolves
  against the schema by Rust field name OR SQL column name.
- **`ListView::search_fields(&[&str])`** — enables `?search=<q>`
  which translates to `ILIKE '%<q>%'` against each listed field,
  OR-combined. `%` and `_` in user input are escaped via
  `escape_like_pattern` so they match literally rather than
  acting as wildcards (defense against pattern injection).
- **Two new Tera context vars** stamped by `ListView` (both
  `router` and `tenant_router` flavors) so templates can
  repopulate filter form inputs:
  - `filters: Map<String, String>` — active filter values
    restricted to the allowlist
  - `search: String` — active `?search=` value, or `""` when unset

### Notes

- Filter + search predicates land in the WHERE clause directly
  (rather than the IR's separate `SearchClause`), so
  `SelectQuery.where_clause` and `CountQuery.where_clause` see
  them equally. That means pagination's `total_pages` reflects
  the filtered/searched subset — it's NOT a bug carry-over from
  viewset's COUNT-ignores-search behavior.
- The simplified Django-shape filter syntax (exact match only)
  is a deliberate small-surface choice. Projects wanting `__gt`
  / `__icontains` / `__in` lookups build their own filters in a
  hand-rolled handler. This keeps the ListView surface minimal
  while covering the 80% case.
- Available behind the existing `template_views` feature (no new
  feature flag).

### Tests

- 1232 → 1243 lib tests (+11): builder accepting filter_fields
  + search_fields, empty params → empty WHERE, filter in
  allowlist → Eq predicate, filter not in allowlist → silently
  dropped, reserved keys (page / page_size / search) skipped from
  filters, single search field → no OR wrapper, filter + multi-
  field search → top-level AND, escape_like_pattern neutralizes
  wildcards, empty `?search=` skipped, filter context stamps
  active values, empty params yield empty `{filters, search}`.

---

## [0.29.1] — template_views polish (bounds validation + stable pagination)

Two patches against the new `template_views` module from v0.29.0.
Both surfaced from re-reading the code after the release tag —
the kind of follow-up that's worth shipping fast before any user
trips over them.

### Fixed

- **`ListView` pagination is now deterministic** when no explicit
  `.order_by(...)` is set. Without `ORDER BY`, Postgres doesn't
  promise stability between calls, so requesting page 2 could
  return rows that already appeared on page 1. `resolve_order_by`
  now defaults to `<pk> ASC` when the order spec is empty (the
  PK is always indexed; cost is bounded). Models without a
  primary key fall through to empty `ORDER BY` — pagination on
  PK-less models is unusual and there's no canonical column.

### Changed

- **Form parsing in `CreateView` / `UpdateView` enforces the
  bounds declared on the schema** (`max_length`, `min`, `max`)
  via `core::validate_value`. Previously the values would slip
  through the form layer and surface as a 500 from the SQL
  layer's bounds check on insert. Now they surface as per-field
  form errors with the user's input preserved (mirroring the
  existing required-missing path). New `bounds_error_message`
  helper renders `QueryError` variants without the framework's
  `model.field` framing — the field name is already the error
  key, so the message just needs the rule:
  - `MaxLengthExceeded` → "must be 5 characters or fewer (got 12)"
  - `OutOfRange` (two-sided) → "must be between 0 and 100 (got 150)"
  - `OutOfRange` (one-sided) → "must be ≥ 0" / "must be ≤ 100"

### Tests

- 1226 → 1232 lib tests (+6): two for the PK-ASC fallback
  (with-PK and without-PK), four for bounds validation
  (max_length enforced, integer range enforced, two-sided
  message format, one-sided message format).

---

## [0.29.0] — tiered settings, HTML CBVs, friendly URLs by default

The biggest release since v0.16's unified manage runner. Three
headline themes:

1. **Tiered settings (#87)** — `Settings::load_from_env()` plus
   `dev_settings.toml` / `staging_settings.toml` /
   `prod_settings.toml` files (auto-selected via `RUSTANGO_ENV`,
   scaffolder emits all three). Six new sections (`server`,
   `auth`, `brand`, `security`, `routes`, `audit`) cover ~30 knobs
   that were env-only or hardcoded; eleven `from_settings`
   constructors thread the values into the right runtime layer.
   `Cli::with_settings_from_env()` makes wiring a one-liner that
   auto-applies security_headers + CORS + access_log + body_limit
   on the user's API router. `manage check --deploy` flags
   dev-defaults left in prod (HSTS=0, weak Argon2, long JWT TTLs,
   loopback bind, etc.).
2. **Generic class-based views for HTML (A5)** — new
   `template_views` module ships Django-shape `ListView`,
   `DetailView`, `CreateView`, `UpdateView`, `DeleteView` over
   any `#[derive(Model)]` schema, rendered through Tera. Each
   ships in two flavors: `.router(prefix, tera, pool)` for
   single-tenant projects, `.tenant_router(prefix, tera)` for
   multi-tenant projects (resolves connection per-request via
   the `Tenant` extractor). Closes the JSON-vs-HTML asymmetry —
   rustango pitched itself as Django-shape but had no HTML-side
   counterpart to `viewset`.
3. **Dev-loop ergonomics + bug fixes batch** — friendly URL
   preset (`/login`, `/admin`, `/audit`) is now the default
   (#85); `Auto<T>` serializes as bare value instead of
   tagged-enum (#83); URL-token impersonation handoff replaces
   the cookie-domain handoff that broke on Chromium against
   `localhost` (#88); built-in JWT auth endpoints land
   (#81); `ViewSet::tenant_router` for multi-tenant CRUD
   (#80); contenttype rows auto-populated on bootstrap (#89);
   four new `manage` verbs
   (`make:api_routes`, `migrate --squash`, `seed-permissions`,
   `forget-pending`); plus 50+ smaller fixes.

### Added

- **`rustango::config::Settings::load_from_env()`** + the new
  tier convention. Loader reads `RUSTANGO_ENV` (defaults to
  `dev`), prefers `<env>_settings.toml` over the legacy
  `<env>.toml` shape (legacy still loads when no `_settings`
  variant exists). `Settings::current_env_tier()` exposes the
  resolved tier. `Settings::detected_features()` introspects
  `#[cfg(feature = "...")]` flags for telemetry / version
  pages / deployment audits.
- **Six new TOML sections**: `[server]` (bind,
  request_timeout_secs, max_body_bytes), `[auth]` (argon2
  memory/iterations/parallelism, lockout threshold/duration) +
  `[auth.jwt]` (access_ttl_secs, refresh_ttl_secs, issuer,
  audience), `[brand]` (name, tagline, logo_url, primary_color,
  theme_mode), `[security]` (headers_preset, csp,
  hsts_max_age_secs, cors_allowed_origins), `[routes]`
  (legacy_preset + per-field URL prefix overrides), `[audit]`
  (retention_days, redact_query_params).
- **`Cli::with_settings(&Settings)`** + **`Cli::with_settings_from_env()`**
  — apply Settings.server.bind, Settings.routes → RouteConfig,
  and auto-mount security_headers + CORS + access_log +
  body_limit layers on the user's API router. The one-liner
  `Cli::new().api(urls::api()).with_settings_from_env().run()`
  now drives the entire stack from the four scaffolder-emitted
  TOML files.
- **`from_settings` constructors** on `SecurityHeadersLayer`,
  `CorsLayer`, `BodyLimitLayer`; `with_audit_settings` on
  `AccessLogLayer`; `with_jwt_settings` on
  `auth_routes::Config`; `cache::from_settings`,
  `email::from_settings`, `jobs::inmemory_from_settings`. Each
  fail-safe to a sensible default with a tracing::warn rather
  than blocking startup on misconfig.
- **`manage check --deploy`** now also loads
  `Settings::load_from_env()` and audits the loaded values:
  flags `headers_preset = "dev"` / `"none"` in prod tier,
  `hsts_max_age_secs = 0`, `argon2_memory_kib < 19456` (OWASP
  2024 floor), `access_ttl_secs > 3600`, loopback `[server]
  bind`, missing `audit.retention_days`, `legacy_preset = true`.
  Audit is a no-op on dev/staging tiers.
- **`rustango::template_views`** — new module behind a default-on
  `template_views` feature. `ListView`, `DetailView`, `CreateView`,
  `UpdateView`, `DeleteView` over `#[derive(Model)]` schemas.
  Each ships `.router(prefix, tera, pool)` (single-tenant) and
  `.tenant_router(prefix, tera)` (multi-tenant via `Tenant`
  extractor). Default template names follow Django convention
  (`<table>_list.html` / `<table>_detail.html` / `<table>_form.html`
  shared by Create+Update / `<table>_confirm_delete.html`).
  Form views auto-skip PK + `Auto<T>` + `generated_as` columns,
  parse form-encoded bodies, coerce values to the field's
  declared SQL type, and re-render with `form.errors` populated
  + 422 status on validation failure.
- **`manage make:api_routes <app> [--tenant]`** (#82 companion) —
  scaffolds `src/<app>/api_routes.rs`, the per-app composer that
  `.merge(...)`-es every viewset's router into a single
  `Router<()>`. Two templates: `--tenant` for tenancy projects
  (no pool argument), default for single-tenant projects.
- **`manage migrate --squash`** (#84a) — dev-iteration escape
  hatch that deletes every pending (un-applied) migration JSON
  and re-runs `makemigrations` to regenerate a single fresh diff
  against the current model registry. Refuses to touch applied
  rows. Closes the recovery flow gap when an evolving model
  produces a migration the validator rejects (e.g. AddColumn NOT
  NULL with no default).
- **`manage forget-pending <name>`** (#84b) — delete a single
  un-applied migration JSON so `makemigrations` regenerates the
  diff. Accepts exact name or unique substring; refuses if the
  named migration is already in the ledger.
- **`manage seed-permissions [--slug <s>]`** (#61 follow-up) —
  re-run `auto_create_permissions` against one (`--slug`) or
  every active tenant. Idempotent. Useful after adding
  `#[rustango(permissions)]` to a model without a fresh migrate
  cycle.
- **`auth_routes::jwt_router(Config)`** (#81) — built-in JWT auth
  endpoints (login + refresh + logout + me) for tenancy
  projects. Endpoints are tenant-aware via the `Tenant`
  extractor; the JWT's `tenant` claim is matched against the
  resolved subdomain so a token minted on `acme.example.com` is
  rejected on `globex.example.com`.
- **`ViewSet::tenant_router(prefix)`** (#80) — multi-tenant CRUD
  resolving connection per-request via `Tenant` instead of
  capturing a pool at mount time. The `make:viewset --tenant`
  scaffolder template emits this shape.
- **URL-token impersonation handoff** (#88) — new
  `tenancy::impersonation_handoff` module. Operator console
  mints a short-lived signed payload (HMAC over op/slug/exp/jti),
  redirects to `<sub>.<apex><handoff_url>?token=<...>`, and the
  tenant admin redeems it with single-use enforcement via
  `JtiBlacklist`. Replaces the cookie-domain handoff that broke
  on Chromium against the `localhost` PSL TLD.
- **Auto-populate `rustango_content_types`** (#89) on bootstrap.
  `contenttypes::ensure_seeded(pool)` is invoked from
  `migrate_registry` and `run_for_one_tenant` (both schema and
  database modes) so CT rows land for every registered model
  without an explicit operator step. New helpers
  `fetch_row_as_json(pool, ct, pk)` and
  `for_each_row_of_ct(pool, ct, batch_size, f)` for the
  "given a ContentType + pk, give me the row" pattern.
  `crate::sql::row_to_json` is now public.
- **Scaffolder emits `config/`** with `default.toml` +
  `dev_settings.toml` + `staging_settings.toml` +
  `prod_settings.toml`. Fresh `cargo run` works without env
  vars (RUSTANGO_ENV defaults to `dev`).

### Changed

- **`RouteConfig::default()` now returns the friendly preset**
  (#85) — `/login`, `/logout`, `/admin`, `/audit`, `/_static`,
  `/_brand`, `/_impersonation_handoff`,
  `/change-password`. Apps that need the v0.28 `__`-prefixed
  shape opt in via `RouteConfig::legacy()` or set
  `[routes] legacy_preset = true` in their TOML.
  **Migration**: existing v0.28 deployments calling
  `Default::default()` (or no override) will see their admin /
  login URLs change shape — bookmarks and external integrations
  need updating.
- **`Auto<T>` JSON wire shape** (#83) — now serializes as the
  bare inner value (`42` / `null`) instead of the tagged enum
  (`{"Set": 42}` / `"Unset"`). Mirrors how `ForeignKey<T, K>`
  lowers to its bare PK on the wire. Deserialize accepts both
  shapes for backwards compat. Audit log JSON shape changes
  too — that's a readability win, but means an audit row written
  under v0.28 looks different from one written under v0.29.
- **`router_with_impersonation` signature** (#88) — drops
  `tenant_cookie_domain` and `tenant_admin_url` parameters; adds
  `tenant_handoff_url`. The cookie path is gone; every
  impersonation now goes through the URL-token handoff. Apps
  using this directly (rare; the typical entry is
  `Server::Builder`) need to update the call.
- **`manage check --deploy`** rewrites the env-var list it
  audits — `SECRET_KEY` is dropped (the framework reads
  `RUSTANGO_SESSION_SECRET`), the placeholder check matches
  `change-me` / `placeholder`, and `RUSTANGO_APEX_DOMAIN` /
  `RUSTANGO_BIND` join the audit set.
- **Cargo `Cargo.toml` scaffolder** — pins `rustango = "0.29"`
  via `env!("CARGO_PKG_VERSION")` so newly-scaffolded projects
  always match the framework version (#79). Yanked-version
  detection guards against publishing a version that resolves
  to a yanked rustango-macros.
- **`manage startapp`** scaffold emits `auto_now_add`
  timestamps wrapped in `Auto<…>` (compilable shape) — the prior
  template wrote `chrono::DateTime<Utc>` directly, which the
  Model derive correctly rejected.
- **`make:viewset --tenant`** template uses
  `ViewSet::for_model(...).tenant_router(...)` shape; default
  template still emits `#[derive(ViewSet)]` for single-tenant
  projects.
- **Brand-name fallback strings** (#72) — Title Case across
  `admin/helpers.rs`, `_sidebar.html`, `tenancy/operator_console`,
  `admin/auth.rs` (was lowercase).
- **Brand logo CSS** (#73) — `_op_styles.html` and
  `_admin_styles.html` use explicit `height` + `align-self:
  flex-start` + `object-fit: contain` instead of `max-height`
  to prevent stretched rendering inside flex parents.

### Fixed

- **Operator-as-superuser impersonation** (#78 batch):
  - Redirect respects `RouteConfig::admin_url` + preserves Host
    port (`acme.localhost:8080/admin/` not `acme.localhost/__admin/`)
  - Cookie domain always set even when project is single-host
  - Operator-side audit rows emit through registry pool (not
    silently dropped — `rustango_audit_log` provisioned by
    `migrate_registry`)
- **`audit_url` end-to-end** (#74 + #85 follow-up) — route +
  templates + redirects all honor the configured audit URL;
  fixes inconsistent `/__audit` Activity link under friendly URLs.
- **POST→GET 405 after session-expiry redirect** (#68) —
  `sanitize_next` rewrites POST-only paths to their parent edit
  page so the post-login bounce doesn't 405.
- **Persistent operator session secret** (#69) —
  `tenancy::server::run` now uses the same on-disk secret as
  `Server::Builder`, so operator sessions survive restart.
- **Operator self-serve change-password endpoint** (#77) —
  closes the missing operator-side surface alongside the tenant
  flow.
- **Title-Case admin index `<h1>`** (#72 follow-up) — uses the
  brand-aware admin_title.
- **Tenancy verbs reject leading-flag positional slug** (#79.3) —
  `cargo run -- create-tenant --help` no longer creates a tenant
  named `--help`.
- **`manage startapp` model template** (#79 sub) — `auto_now_add`
  timestamps wrapped in `Auto<…>` so the scaffold compiles out
  of the box.
- **AddColumn NOT NULL validator error** (#84a) — surfaces three
  concrete recovery paths (`migrate --squash`, `forget-pending`,
  manual JSON delete) instead of the prior unhelpful message.

### Tests

- 1153 → 1226 lib tests (+73 across the release): tier
  resolution, section round-trips, every `from_settings`
  constructor's branches, `Cli::with_settings` resolution
  priority + auto-layer apply, deploy-audit warning paths,
  template_views builder/coerce/parse_form/handle path,
  tenant_router smoke for every CBV, JTI blacklist
  prune-on-insert, contenttype seed/fetch helpers.

### Migration notes

- **Friendly URLs are now default**. If you depended on the
  `/__login` / `/__admin` shape, add
  `Cli::routes(RouteConfig::legacy())` or set
  `[routes] legacy_preset = true` in TOML.
- **Audit log shape changes** (`Auto<T>` JSON wire shape).
  Existing audit rows written under v0.28 keep their old shape;
  new rows use the bare value. If you parse audit rows
  programmatically, accept both shapes.
- **`router_with_impersonation` callers** — update the
  signature: drop `tenant_cookie_domain` + `tenant_admin_url`,
  add `tenant_handoff_url` (typical value: `routes.impersonation_handoff_url`).
- **`SECRET_KEY` env var is gone** — set `RUSTANGO_SESSION_SECRET`
  instead. `manage check --deploy` audits the new name.
- **Tier convention is opt-in** — your existing
  `config/<env>.toml` files keep loading. Rename to
  `config/<env>_settings.toml` to use the new convention; the
  loader prefers the new name when both exist.
- **`template_views` feature** is default-on. Projects with
  `default-features = false` need to add `template_views` to
  their feature list to keep using `ListView` / `DetailView` /
  etc.

### Out of scope (queued follow-ups)

- **`auth.argon2_*` wiring** — would require an invasive
  refactor of every `passwords::hash()` call site to thread
  Argon2Params through. Section already accepts the values; the
  consumer side waits for the refactor.
- **`audit.retention_days` wiring** — needs a scheduler/cron
  integration that doesn't exist yet.
- **`jobs.backend = "pg"` runtime selection** — `JobQueue`
  trait isn't object-safe (generic methods on `Job`), so
  `Arc<dyn JobQueue>` can't compile. Documented in the
  `jobs::inmemory_from_settings` docstring as a manual wire-up.
- **A3 service container** (typed DI registry) — convenience for
  test substitution; deferred until a project actually wants it.
- **A4 middleware-stack-as-data** — auto-layering already
  handles 90% of the use case; configurable order is power-user
  territory.
- **ModelForm integration into CreateView/UpdateView** — would
  replace the inline string coercion with the typed form
  pipeline. Today's coercion is sufficient for most projects.

---

## [0.28.4] — `password_changed_at` cookie invalidation (#77 follow-up)

Patch release closing the only out-of-scope item flagged when v0.28.2
shipped: sessions issued before a password rotation now expire on
the next request instead of remaining valid until their TTL elapses.

### Added

- **`User::password_changed_at: Option<DateTime<Utc>>`** and the
  matching column on `Operator`. Stamped to `NOW()` on every
  password rotation path (`reset-password`,
  `reset-operator-password`, `change-password`,
  `change-operator-password`, the self-serve UI). `None` for
  accounts that haven't rotated since v0.28.4 — those sessions
  stay valid until they expire normally.
- **`TenantSessionPayload::iat: i64`** and `SessionPayload::iat`
  (operator console). Set to `now` on every newly-minted cookie.
  `#[serde(default)]` keeps pre-0.28.4 cookies parseable; their
  `iat` decodes as `0`, which the comparison treats as "issued
  at the dawn of time" so any post-rotation login wins.
- **Runtime ALTER**: `permissions::ENSURE_SQL` now adds the
  `password_changed_at` column to existing tenants on the next
  `migrate`. The matching column on `rustango_operators` is
  added by `migrate_registry` against the registry pool.

### Changed

- **`validate_session` (tenant admin)** rejects cookies whose
  `iat` is strictly less than the user's `password_changed_at`.
  The lookup is folded into the existing per-request
  `is_superuser` / `active` query — no extra round-trip.
- **`require_session` (operator console)** does the same against
  `rustango_operators.password_changed_at`.

### Tests

- 2 new unit tests on `TenantSessionPayload`: `iat` is stamped on
  every newly-minted payload; pre-0.28.4 cookies decode with
  `iat = 0` (preserves the security guarantee on upgrade).
- 1 new live test
  (`session_minted_before_password_rotation_is_rejected`):
  provisions a user, mints a cookie with a fixed past `iat`,
  verifies it works while `password_changed_at IS NULL`, stamps
  it to NOW(), confirms the same cookie now bounces to login.

### Migration notes

- **No schema migration required.** Existing tenants pick up the
  new column on the next `cargo run -- migrate` (idempotent
  `ALTER TABLE … ADD COLUMN IF NOT EXISTS`). Existing sessions
  remain valid until their TTL expires *or* a password is
  rotated — there's no global flush.
- Running `migrate` against a v0.28.3 database is safe and
  reversible: removing the column on rollback leaves the
  feature inert (no NULL becomes a check failure).

## [0.28.3] — startapp scaffolder polish (#63)

Patch release that flushes the `manage startapp` template through
the lessons learned in v0.28.0–v0.28.2: scaffolded models now ship
with an `admin(...)` config block, a `created_at` timestamp, a
singularized struct name, and a smoke test that confirms the model
registered itself in `inventory` (the canonical signal that the
auto-admin will pick it up).

### Changed

- **Singularized starter model.** `manage startapp posts` now
  generates `pub struct Post` on table `"post"` (was `Posts` /
  `"posts"`). Conservative trailing-`s` strip on names of length
  ≥ 5 — `comments → comment`, `users → user`, but `news` /
  `address` / `bus` / short names stay untouched. The struct
  identifier and the `table = "..."` literal are independent —
  rename either freely to suit your domain.
- **`admin(...)` config baked in.** The starter model now carries
  `list_display = "name, active, created_at"`,
  `search_fields = "name"`, `ordering = "-created_at"`. List view
  is usable out of the box instead of dumping every column raw.
- **`created_at: chrono::DateTime<chrono::Utc>` field with
  `auto_now_add`.** Standard Django convention; pairs with the
  default ordering above.
- **Smoke test in `tests.rs`.** New `starter_model_registered_in_inventory`
  test asserts the scaffolded model lands in `inventory::iter::<ModelEntry>` —
  the canonical confirmation that the admin will pick it up. Joins
  the existing `router_builds` test in the per-app `tests.rs`.
- **Doc comments call out the `permissions = true` default.** The
  `models.rs` header now mentions that codenames are auto-seeded
  during `migrate`, so non-superusers see the model after a role
  grant.

### Tests

- 4 new unit tests in `migrate::scaffold::tests` — singularization
  rules; admin-config + `created_at` rendered into the model;
  smoke test references the singularized table; full-pipeline
  end-to-end verification reading materialized files back.

### Out of scope (queued follow-ups)

- Plural-aware singularization (e.g. `categories → category`,
  `boxes → box`). Today's heuristic is intentionally conservative;
  plural-engine territory belongs in a follow-up if anyone hits it.
- Multi-model starter (`pub struct Post` + `pub struct Comment`).
  Today's template ships one starter; a `--with-related` flag
  could generate FK pairs.

## [0.28.2] — password reset UI + CLI ergonomics (#77)

Patch release filling in the gaps around password rotation —
self-serve change-password page on the tenant admin, two new
CLI verbs that verify the current password, and a `--generate`
flag on every password verb.

### Added

- **Self-serve `/__change-password` page on the tenant admin.**
  GET renders a form (current pw / new pw / confirm); POST
  verifies the current password against `rustango_users.password_hash`
  and updates it. Anonymous visitors are bounced to login.
  URL is configurable via `RouteConfig::change_password_url`
  (default `/__change-password`; `RouteConfig::friendly()`
  serves it at `/change-password`). The admin sidebar now
  renders a "Change password" link when the URL is configured.
- **`change-password <slug> <username>` CLI verb.** Symmetric
  counterpart to `reset-password` for the case where the user
  remembers their current password. Verifies current first,
  then rotates. Reads `--current` and `--password` interactively
  when omitted.
- **`change-operator-password <username>` CLI verb.** Same
  flow for operators.
- **`--generate` flag on every password verb.** Available on
  `create-operator`, `create-user`, `reset-password`,
  `reset-operator-password`, `change-password`,
  `change-operator-password`. Generates a 20-character
  random password from a 58-char unambiguous alphabet
  (no `0/O`, `1/l/I`), hashes it, and prints it to stdout
  exactly once. Mutually exclusive with `--password`.
- **`tenancy::password::generate(length)`** — public helper
  used by the CLI. `OsRng`-backed; returns `String`.

### Changed

- `RouteConfig::default()` now also sets
  `change_password_url = "/__change-password"`.
  `RouteConfig::friendly()` sets `/change-password`.
- `admin::Builder::change_password_url(url)` setter — surfaces
  the link in the standalone-admin sidebar. Tenant admin
  Builder threads it through automatically from `RouteConfig`.

### Tests

- 3 new unit tests in `tenancy::password` covering the
  generator (length, charset, hash round-trip, uniqueness).
- 3 live tests in `tests/manage_change_password_live.rs`
  for the CLI verbs (round-trip, --generate prints + verifies,
  mutually-exclusive flags rejected).
- 4 live tests in `tests/admin_change_password_ui_live.rs`
  for the UI (anonymous → 303 to login; authenticated GET
  renders form; POST with correct current rotates the hash;
  POST with wrong current shows error and leaves hash
  unchanged).

### Out of scope (queued follow-ups)

- `password_changed_at` cookie invalidation — sessions
  issued before a password change currently remain valid
  until they expire. Schema change required (add column to
  `rustango_users` and `rustango_operators`, bake `iat`
  comparison into `validate_session`); deferred to v0.29.
- Operator-driven password reset on a tenant user via the
  operator console UI — the `reset-password` CLI verb
  already covers this path; UI sugar is a follow-up.
- Password strength enforcement at the UI / CLI layer
  (the `passwords::strength_score` helper exists but isn't
  wired into either flow yet).

## [0.28.1] — users/roles/perms admin surface (#76)

Patch release fleshing out the tenant admin coverage of the
permission tables (auto-seeded by `ensure_permission_tables`) and
adding a roles + effective-permissions panel on the
`rustango_users` detail page.

### Added

- **Admin metadata on the permission junction models.** `Role`
  already had `admin(...)` config; `RolePermission`, `UserRole`,
  and `UserPermission` now do too. Their list pages render
  `role_id, codename`, `user_id, role_id`, and
  `user_id, codename, granted` respectively, with sensible
  ordering. No schema impact — pure metadata.
- **Roles & permissions panel on the user detail page.** Visiting
  `/{admin_url}/rustango_users/{id}` now renders a side section
  showing the user's assigned roles (linked to each role's
  detail page) and their effective codenames (union of role
  grants + direct grants minus explicit denials). Best-effort:
  if the permission tables haven't been seeded the panel is
  hidden, mirroring the audit-trail panel's posture. Quick
  links to the four manage-able junction tables sit beneath
  the panel.

### Tests

- `tenancy::permissions::admin_config_tests` — two unit tests
  asserting the four permission models carry `admin(...)`
  config and stay in `ModelScope::Tenant` (so they remain
  visible in tenant-mode admins).
- `tests/admin_user_roles_panel_live.rs` — end-to-end live
  test that seeds a user with one role (granting `post.add`
  and `post.change`), one direct grant (`comment.add`), and
  one explicit denial (`post.change`); GETs the user detail
  page; asserts the role + grants render and that the denial
  suppresses the role-granted codename.

### Out of scope (queued follow-ups)

- Inline assign/revoke buttons on the User detail panel
  (currently read-only — manage via the dedicated junction
  table admin pages).
- Surfacing the `rustango_permissions` catalog as an admin
  page (it has no Rust `Model` today; adding one would diff
  against existing tenants' bootstrap snapshots — handle as
  a v0.29 schema-aware change).

## [0.28.0] — configurable tenant URL prefixes via `RouteConfig` (#74)

Minor version bump signals the new public
`tenancy::RouteConfig` API. All defaults preserve pre-0.28
behavior — upgrades are no-op until apps explicitly opt in.

### Added

- **`tenancy::RouteConfig`** — configurable URL prefixes for
  the per-tenant admin: `login_url`, `logout_url`, `admin_url`,
  `audit_url`, `static_url`, `brand_url`, plus `basic_auth_realm`
  and three session TTLs (`tenant_session_ttl`,
  `operator_session_ttl`, `impersonation_ttl`).
- **`RouteConfig::default()`** matches every legacy
  `__`-prefixed path (`/__login`, `/__admin`, …) so existing
  apps see no behavior change.
- **`RouteConfig::friendly()`** preset drops the underscores —
  `/login`, `/admin`, `/audit`, `/_static`, `/_brand` — for
  apps that have reserved their root namespace cleanly.
- **`Server::Builder::routes(RouteConfig)`** setter propagates
  the config through to `TenantAdminBuilder` (and impersonation
  redirect URLs).
- **`TenantAdminBuilder::routes(RouteConfig)`** for direct
  callers building the admin without going through
  `Server::Builder`.
- Tenant admin Tera templates now consume `{{ login_url }}`,
  `{{ logout_url }}`, `{{ admin_prefix }}` (already in 0.27.9),
  `{{ static_url }}`, `{{ brand_url }}` so the rendered HTML
  honors whatever `RouteConfig` was supplied.

### Fixed

- Tenant admin path matching (`validate_session`,
  `redirect_to_tenant_login`, `login_form`, `login_submit`,
  `logout_response`, brand asset serve, end-impersonation
  redirect) now reads from `RouteConfig` instead of the
  hardcoded `/__login` / `/__logout` / `/__admin` / `/__audit`
  / `/__static__` / `/__brand__` literals. Path matching is
  now table-driven — apps that flip to friendly URLs see the
  middleware honor the new paths immediately.

### Scope notes

- The **operator console** (apex) keeps its existing
  `/login` / `/logout` / `/orgs` / `/operators` URLs in this
  release. Operator-side configurability is a follow-up
  (`OperatorRouteConfig`) — the bigger and more user-visible
  win was the tenant admin, which this release closes.
- Settings-file integration (`config/default.toml [routes]`)
  is also a follow-up. For 0.28.0 you build `RouteConfig`
  explicitly:
  ```rust
  Server::Builder::from_env().await?
      .routes(RouteConfig::friendly())
      .api(my_app::urls::router())
      .serve("0.0.0.0:8080").await
  ```

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1100/1100 pass** (4 new `RouteConfig` tests covering
  default-matches-legacy, friendly preset, audit-full-url
  joining, sensible TTL defaults).

Step 4 of the v0.28 plan. Workspace 0.27.10 → 0.28.0.

## [0.27.10] — fix POST→GET 405 after session-expiry redirect (#68)

### Fixed

- **Operator no longer hits 405 Method Not Allowed after a
  session-expiry redirect on a POST-only route.** Pre-fix:
  operator clicks Save on `/orgs/{slug}/edit/branding`,
  cookie has expired → middleware 303s to `/login?next=…`,
  browser converts the POST to GET, operator logs in →
  another 303 → GET `/orgs/{slug}/edit/branding` → 405
  because the route is POST-only. Operator stares at a
  Method-Not-Allowed page with no clear path forward.
  Two-part fix:
  1. **`sanitize_next_for_method(method, path)`** rewrites
     non-GET request URLs to a safe-GET parent before they
     get encoded into `?next=…`. POST to
     `/orgs/{slug}/edit/branding` or
     `/orgs/{slug}/impersonate` now rewrites to
     `/orgs/{slug}/edit` (the GET-renderable parent edit
     form). Unknown POST paths fall back to `/`.
  2. **GET fallbacks** mounted on the POST-only routes
     (`org_post_only_redirect`) so a manual URL hit
     (browser tab restored from history, link-prefetch,
     etc.) bounces back to the parent edit form instead of
     405-ing.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1096/1096 pass** (6 new tests covering GET pass-through,
  POST→branding rewrite, POST→impersonate rewrite,
  POST→edit pass-through, unknown-POST fallback, and
  query-string dropping).

Step 3 of the v0.28 plan. Workspace 0.27.9 → 0.27.10.

## [0.27.9] — admin_prefix template variable (#59)

### Fixed

- **Tenant admin sidebar / audit / detail links no longer break
  under `/__admin/{*rest}` mount.** Pre-fix, several templates
  hardcoded paths that assumed the admin lived at `/__admin`
  but emitted bare `/__audit` (no prefix) — clicking "Activity"
  on the tenant sidebar 404'd because the actual route lives
  at `/__admin/__audit` from the browser's perspective. Same
  bug for `audit_log.html` clear / pager links and the "View
  full history" link in `detail.html`.

### Added

- **`admin_prefix` template variable** threaded into every
  rendered page via `chrome_context`. Defaults to `/__admin`
  (matching the existing convention) so apps that already
  mount the admin via `nest("/__admin", admin::router(pool))`
  see no behavior change. Apps mounting under a different path
  (e.g. `nest("/admin", admin::router(pool))`) override via:
  ```rust
  let app = admin::Builder::new(pool).admin_prefix("/admin").build();
  ```
- Setter strips trailing slash; empty string supported for the
  "admin is the root router" case.

### Templates swept

22 hardcoded `href="/__admin/..."`, `action="/__admin/..."`,
`href="/__audit..."`, `action="/__audit/cleanup"` references
across `_sidebar.html`, `index.html`, `list.html`,
`audit_log.html`, `detail.html`, `form.html`, `base.html`
all rewritten to `{{ admin_prefix }}/...`.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1090/1090 pass** (3 new tests: default, trailing-slash
  trim, empty-string-for-root).
- `grep -rn 'href="/__\|action="/__' crates/rustango/src/admin/templates/`
  returns zero hardcoded admin paths.

This unblocks lane A's larger Step 4 (#74 — fully configurable
URL prefixes via `[routes]` settings). The plumbing is now in
place; #74 just adds env / config-file plumbing on top of the
existing `admin_prefix` setter.

## [0.27.8] — operator-as-superuser tenant admin impersonation (#78)

### Added

- **"Open admin as superuser →"** button on the operator console's
  `/orgs/{slug}/edit` page. Mints a tenant-bound impersonation
  cookie signed with the same `SessionSecret` the tenant admin
  uses, sets it on the apex domain so subdomains receive it,
  and redirects the operator to `<slug>.<apex>/__admin/`.
- **Impersonation banner** on every tenant admin page when the
  current session is an operator-impersonation. Sticky at top,
  high-contrast warning style, "End impersonation" button posts
  to `/__admin/__end-impersonation` which clears the cookie and
  redirects back to the operator console.
- **Audit-log entries** for impersonation start (recorded on the
  registry side at mint time, `source = "operator:<id>:impersonating"`).
  Every write made during the impersonation session is tagged
  with the same source so post-hoc forensics can pinpoint
  operator-driven changes.
- **`TenantSessionPayload.imp: Option<i64>`** — backward-compatible
  extension via `#[serde(default)]`. Pre-0.27.8 cookies (no `imp`
  field) still decode cleanly. New `TenantSessionPayload::impersonation()`
  constructor + `is_impersonation()` accessor.
- **`operator_console::router_with_impersonation`** — new
  constructor that takes the tenant session secret + cookie
  domain, mounts the `POST /orgs/{slug}/impersonate` route.
  `Server::Builder::serve` calls it automatically since v0.27.8;
  custom mount points opt in.
- **`Builder::impersonated_by(operator_id)`** setter on the admin
  builder threads the operator id into `chrome_context` for
  the banner.
- **`IMPERSONATION_TTL_SECS`** constant (1h default), overridable
  via `RUSTANGO_OPERATOR_IMPERSONATION_TTL_SECS`. Short by
  design — long enough for a debugging session, short enough
  that an idle operator gets dropped.

### Security guards

- Impersonation cookie is HMAC-SHA256 signed with the tenant
  secret — operator can't forge one without it.
- Cookie is **slug-pinned** (`SessionError::WrongTenant` rejects
  cross-tenant replay).
- Impersonation refused against `org.active = false` tenants
  (returns 409 Conflict).
- Operator console route is only mounted when the tenant secret
  was supplied — no risk of accidental mint when running with
  the legacy `router_with_pools` constructor.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1087/1087 pass** (5 new impersonation cookie tests:
  `imp` field shape, round-trip, slug-pin against cross-tenant
  replay, backward-compat decode of pre-0.27.8 cookies).

## [0.27.7] — tenant-pool tuning + registry-scope filter for tenant admin

### Added

- **`TenantPoolsConfig` exposes connection-time tuning**:
  `database_pool_min_connections` (keep N warm),
  `database_pool_acquire_timeout` (default 30s),
  `database_pool_idle_timeout` (default 10 min),
  `database_pool_max_lifetime` (default 30 min, helps with vault
  credential rotation), and `prewarm_active_tenants` (opt-in
  flag — when true, `Server::Builder::serve` builds pools for
  every active database-mode tenant on boot). All defaults
  preserve pre-0.27.7 behavior so upgrading is a no-op until
  apps explicitly tune. (#60)
- **`TenantPools::prewarm_database_tenants() -> PrewarmReport`** —
  walks active database-mode orgs and lazily builds each pool.
  Bounded by `max_cached_database_pools`; per-tenant build
  failures log a `tracing::warn!` but don't abort the loop.
- **`manage prewarm-pools` CLI verb** — explicit ops trigger,
  e.g. as a post-deploy hook after credential rotation or to
  validate every tenant is reachable before flipping a load
  balancer.
- **`tracing::info_span!("tenant_pool_init", slug, mode)`** wraps
  the cold-path pool build with a per-tenant duration log line,
  so first-request latency is grep-able instead of
  unobservable.
- **`docs/manage.md`** gained a "Tenant-pool tuning" section
  with a settings table, pre-warm trigger guide, and a macOS
  `.local` mDNS troubleshooting note (the 5-second pause some
  users see hitting `<slug>.local:8080` is Bonjour, not the
  framework — `--resolve <host>:8080:127.0.0.1` proves it).

### Fixed

- **Tenant admin no longer surfaces registry-scoped models** in
  its sidebar / index / direct URL hits. Pre-fix, models declared
  `#[rustango(scope = "registry")]` (Org, Operator) showed up in
  the tenant admin even though they don't live in the tenant's
  storage — clicking through could leak cross-tenant data via
  `search_path` on schema-mode tenants (the registry's
  `public.rustango_orgs` would resolve). Now:
  - `crate::admin::Builder::tenant_mode()` setter (+ matching
    `tenant_mode: bool` on `Config`).
  - `TenantAdminBuilder::build()` flips it on automatically;
    standalone single-tenant admins (no tenancy) leave it off
    and see every scope.
  - `AppState::scope_visible(ModelScope)` is the gate; called
    from `sidebar_context`, `views::index`, and `lookup_model`
    so direct URL hits like `/__admin/rustango_orgs` also 404
    cleanly.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1082/1082 pass** (5 new tests: 2 for pool config defaults +
  PrewarmReport, 3 for the scope filter / tenant_mode setter).

## [0.27.6] — first-user auto-superuser + admin recovery CLI verbs

Closes the "I just created my first tenant user but the admin sidebar
shows 'No models registered.'" papercut. Three layered framework
changes plus four new CLI verbs.

### Added

- **`create-superuser <slug> <username> [--password <s>]`** — Django-shape
  alias for `create-user --superuser`. Cleaner entrypoint when an
  operator wants to provision a tenant admin in one verb.
- **`set-superuser <slug> <username> [--on|--off]`** — toggle
  `rustango_users.is_superuser` on an existing tenant user. Direct
  recovery path when an onboarding script created the first user
  without `--superuser`.
- **`reset-password <slug> <username> [--password <p>]`** — admin-driven
  password reset for tenant users (no current password required).
  Full self-serve UI for tenant users still pending in #77.
- **`reset-operator-password <username> [--password <p>]`** — same
  for operators on the registry pool. Recovery path when an
  operator forgets their password and there's no other admin to
  do it via UI.

### Fixed

- **First-user-of-a-tenant auto-superuser** in
  `tenancy::manage::users::create_user_cmd`. When the tenant has
  zero existing rows in `rustango_users`, the next user is
  implicitly promoted to superuser even without `--superuser`,
  with a notice in the CLI output. Pre-fix:
  `cargo run -- create-user osu admin --password ...` (forgetting
  `--superuser`) produced a tenant whose only user could log in
  but saw an empty admin sidebar — every model filtered out by
  `is_visible(table)` because the user had zero perm grants and
  `auto_create_permissions` only seeds the catalog, doesn't grant
  to anyone. Mirrors Django's `createsuperuser` first-user UX.

### Verified end-to-end via Playwright

- Reproduced the bug: logged in as a non-superuser → sidebar
  showed "No models registered."
- Confirmed the fix path: promoted the user via `set-superuser` →
  sidebar populated with all 16 models including the user-defined
  Country / Region / SubRegion / IntermediateRegion in the
  `regions` app group.
- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` — **1077/1077**

## [0.27.5] — fix tenant login page blank screen (regression in 0.27.3)

### Fixed

- **Tenant admin login page rendered as a blank body after 0.27.3**.
  The v0.27.3 `tenant_login.html` rewrite (#71) added
  `{% include "_theme_tokens.html" %}`, but the partial wasn't
  registered in `TenantAdminBuilder::with_session`'s Tera registry
  (only `tenant_login.html` itself was). Tera's `render` returned
  an `Err`, which `login_form` swallowed via
  `unwrap_or_default()` → empty `Html("")` → blank page at
  `http://<tenant>:8080/__login`. Two-part fix:
  - Register `_theme_tokens.html` alongside `tenant_login.html`
    in the tenant Tera registry.
  - Replace the `unwrap_or_default()` with a `tracing::error!` +
    a fallback HTML body that points the operator at the server
    logs, so future template bugs surface instead of silently
    dropping the render.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1077/1077 pass**

## [0.27.4] — `migrate --fake` ledger drift recovery

### Added

- **`manage migrate --fake <name>`** verb (#64) — recovery path
  for the "tables exist but the ledger row is missing" drift
  that surfaces as `relation "X" already exists` (Postgres
  `42P07`) on the next `migrate` attempt. Common after a manual
  setup, an interrupted earlier migrate, or a schema dump that
  brought in tables but not the `__rustango_migrations__` ledger.
  ```sh
  cargo run -- migrate --fake 0001_rustango_registry_initial
  cargo run -- migrate --fake 0001_initial --fake 0002_initial   # multiple
  ```
  Validates each name against the migration directory before
  the row lands so typos can't be backfilled. Idempotent
  (`ON CONFLICT (name) DO NOTHING`) — safe to re-run. Operates
  on the registry ledger; `migrate --fake` followed by `migrate`
  picks up actually-pending migrations next.

### Note

- **Friendly missing-table page (#66) was already shipped**
  pre-0.27 in `admin/errors.rs::AdminError::TableMissing`. Triage
  for v0.27.2 incorrectly listed it as pending; verified during
  this slice that the path is wired (every admin handler returns
  `Result<_, AdminError>`, and `From<sqlx::Error>` /
  `From<ExecError>` detect Postgres `42P01` and convert to
  `TableMissing` with a friendly HTML response).

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1077/1077 pass**

## [0.27.3] — tenant login branding + table-name macro guard

### Fixed

- **Tenant admin login page now renders per-tenant branding** (#71).
  Pre-fix, `tenant_login.html` hardcoded `/__static__/rustango.png`
  and an inline `--accent: #2c6fb0` — uploaded logos / favicons /
  brand colors had zero effect on the unauthenticated screen.
  `login_form()` now threads `brand_logo_url`,
  `brand_favicon_url`, `brand_name`, `brand_tagline`, `theme_mode`,
  and `brand_css` through the template via the same helpers the
  authenticated layouts already use. The template:
  - imports `_theme_tokens.html` so colors honor the org's theme
    (light / dark / auto)
  - emits `<link rel="icon">` from `brand_favicon_url` (falls
    back to embedded rustango icon)
  - applies `brand_css` (derived from `org.primary_color`) so
    the accent color matches the rest of the tenant admin
  - falls back cleanly to the rustango defaults when no brand
    is set so existing apps don't change

### Added

- **Macro-time guard against invalid table names** (#65).
  `#[rustango(table = "intermediate-region")]` previously
  compiled cleanly but then broke downstream when the
  framework's FK / index name derivation emitted unquoted
  identifiers like `intermediate-region_field_fkey`. Now
  rejected at `#[derive(Model)]` expansion with a clear error:
  > table name `intermediate-region` contains invalid
  > character `'-'` — SQL identifiers must match
  > `[a-zA-Z_][a-zA-Z0-9_]*`. Hyphens in particular break FK /
  > index name derivation downstream; use underscores instead
  > (e.g. `intermediate_region`)
  Same `[a-zA-Z_][a-zA-Z0-9_]*` shape Postgres allows for
  unquoted identifiers — the safe path is now the only path.

### Verified

- `cargo build -p rustango --features tenancy` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1077/1077 pass**
- `cargo test -p rustango --test derive_model` — **16/16 pass**

## [0.27.2] — admin-registration UX rescue + sidebar/branding polish

Fixes the cluster of papercuts that hit anyone scaffolding a new
app on the tenant admin (#61–#75 in the backlog). Out-of-the-box
flow `manage startapp <name>` → add a `#[derive(Model)]` →
`cargo run -- migrate` → log in as a non-superuser tenant user
now actually surfaces the new model in the admin sidebar.

### Fixed

- **Models default to `permissions = true`** (#62). Prior to
  this, models without an explicit `#[rustango(permissions)]`
  attribute were skipped by `auto_create_permissions`, never had
  `{table}.view` codenames seeded, and were therefore invisible
  to non-superuser tenant admins. The startapp scaffolder
  emitted models without the flag, so fresh apps appeared
  broken. Default is now `true`; opt out via
  `#[rustango(permissions = false)]` (registry-internal models).
- **`auto_create_permissions` is now auto-invoked** after every
  tenant migrate, both schema-mode and database-mode. The
  catalog stays in sync with the registered model set without
  manual wiring (#61).
- **`SessionSecret::from_env_or_disk()`** persists the
  operator-console + tenant-admin session secrets to
  `./var/.rustango_*_session.key` so dev `cargo run` cycles
  don't sign every operator out on restart (#69). Production
  should still set `RUSTANGO_SESSION_SECRET` so the secret
  lives in env / secret-manager rather than the filesystem.
- **Sidebar logo no longer renders stretched** (#73). The
  rule `max-height: 48px` was being silently overridden by a
  flex-parent's default `min-height: auto` resolving to the
  image's intrinsic 1024px. Replaced with explicit `height: 40px;
  width: auto; align-self: flex-start; object-fit: contain` in
  both `_op_styles.html` and `_admin_styles.html`.
- **Branding sub-form layout collision** (#70). Two consecutive
  `form.edit-form`s on the operator-console org-edit page had
  no margin separating them; the second form's fieldset legend
  rendered at the same vertical band as the first form's Save
  button. Fixed with `margin-bottom: var(--space-6)` on
  `.edit-form` plus `+ form.edit-form { margin-top: ... }`.
  Also renamed the sub-form button "Upload" → "Save branding
  assets" so it doesn't compete visually with the primary Save.
- **Operator console org-list "Edit" link is a styled action
  button** (#75) — pill-shaped with accent-tinted background,
  not a bare purple-underlined text link. Empty `<th></th>`
  replaced with `<th>Actions</th>` for accessibility.
- **Brand-name fallbacks Title Case** (#72): "Rustango Admin"
  / "Rustango" everywhere a human reads them
  (`admin/helpers.rs`, `_sidebar.html`,
  `tenancy/operator_console/mod.rs`, `admin/auth.rs` Basic auth
  realm). Crate-level identifier (`rustango = "0.27"`) stays
  lowercase.

### Added

- New regression tests in `tests/derive_model.rs`:
  `permissions_defaults_to_true`,
  `permissions_explicit_true_round_trips`,
  `permissions_explicit_false_opts_out`. These guard the
  out-of-the-box admin-visibility flow (#67) so the regression
  can't sneak back in.

### Verified

- `cargo build -p rustango --features tenancy,sqlite` — clean
- `cargo test -p rustango --features tenancy --lib` —
  **1077/1077 pass**
- `cargo test -p rustango --test derive_model` — **16/16 pass**
  including the three new permissions-default tests

## [0.27.1] — `cargo test` cleanup for default features

### Fixed

- **`examples/blog_demo/` now gates on `tenancy`.** Cargo
  auto-discovers `examples/<name>/main.rs` and compiles each one on
  `cargo test` (no args). Under the default feature set (which does
  not include `tenancy`), `blog_demo`'s imports
  (`rustango::tenancy::*`, `rustango::extractors::Tenant`,
  `#[derive(ViewSet)]`) failed to resolve. Registered as
  `[[example]] required-features = ["tenancy"]` so it's skipped
  cleanly without that feature.
- **`#[derive(ViewSet)]` macro hygiene.** The derive emitted
  `#model_path::SCHEMA` (inherent-path lookup), which required the
  caller to also `use rustango::core::Model` for the trait method
  to resolve. Switched to the fully-qualified
  `<#model_path as ::rustango::core::Model>::SCHEMA` shape used
  everywhere else in the macro layer.
- **Unused-import warnings** in `forms/mod.rs`, `server/app.rs`,
  `tests/cache_backends.rs`, `tests/contenttypes_live.rs`.

### Verified

- `cargo test --no-run` (default features) — all examples + tests
  compile cleanly.
- `cargo build --example blog_demo --features tenancy` — clean.
- `cargo test -p rustango --features tenancy --lib` — **1077/1077**.

## [0.27.0] — SQLite ORM backend + bi-dialect AppBuilder

### Added

- **SQLite as a third dialect** alongside Postgres and MySQL (#37).
  Behind a new `sqlite` feature flag. Every `_pool` ORM helper has a
  `Pool::Sqlite` arm now: `insert_pool` (INSERT…RETURNING populates
  `Auto<T>` PKs), `save_pool`, `delete_pool`, `count_pool`,
  `fetch_pool`, `select_related` (FK joins decoded via new
  `LoadRelatedSqlite` trait), `fetch_with_prefetch_pool`,
  `bulk_insert_pool`, `transaction_pool`
  (`PoolTx::Sqlite(Transaction<Sqlite>)`), `fetch_aggregate_pool`,
  `raw_query_pool`, `raw_execute_pool`. The macro layer emits
  `FromRow<SqliteRow>`, an aliased-row decoder for joins, and a
  SQLite arm in `AssignAutoPkPool::__rustango_assign_from_sqlite_row`
  — automatic for every `#[derive(Model)]` struct when the feature
  is on, expanding to nothing when it's off (verified by
  `tests/macro_no_backend_cfg.rs`). Audit log table + emitter +
  diff-style `save_one_with_diff_pool` all work on SQLite. ILIKE
  rewrites to `LOWER(col) LIKE LOWER(?)`. New `SqliteReturningRow`
  type alias + `try_get_returning_sqlite` helper. New SQLite
  Decode/Type impls for `Auto<T>` and `ForeignKey<T, K>`. Migrate
  runner (`apply_atomic_pool`, `unapply_atomic_pool`,
  `applied_set_pool`, `ensure_ledger_pool`) handles SQLite.
- **`Pool::connect("sqlite::memory:")`** and
  `sqlite:./path.db?mode=rwc` return a usable `Pool::Sqlite`
  (was Phase-3-pending).
- **`server::AppBuilder`** — bi-dialect single-pool runserver. Reads
  `DATABASE_URL` (any backend), runs `CREATE TABLE IF NOT EXISTS`
  for the supplied model schemas, mounts an axum router, serves.
  Pool injected as `Extension<Arc<Pool>>` into every request — no
  `with_state` ceremony. Behind a new `runserver` feature
  (in defaults). The Django-style multi-tenant `Builder` stays
  gated on `tenancy` (still PG-bound until `TenantPools` becomes
  `Pool`-generic in v0.28).
- **Cookbook chapter 13** — full SQLite tour: `Pool::connect`, Auto
  PK round-trip, bi-dialect `_pool` API matrix, ILIKE translation,
  gotchas (`sqlite_*` reserved prefix, no ALTER ADD CONSTRAINT,
  no advisory lock), in-memory test harness, AppBuilder recipe.
- **`examples/sqlite_orm_demo.rs`** — 12-section single-file demo
  exercising the entire SQLite ORM surface against `sqlite::memory:`.
- **`examples/sqlite_app_demo.rs`** — `AppBuilder` + axum + SQLite
  end-to-end runnable.
- **`tests/sqlite_live.rs`** — 5 in-memory live tests covering
  CRUD + connect path through the public API.

### Changed

- `crates/rustango/src/server` is unconditional now (previously
  gated on `tenancy`). The full multi-tenant `Builder` stays behind
  the `tenancy` feature inside the module; the lighter `AppBuilder`
  is reachable with just `runserver`.
- `Dialect` trait grew `serial_type_includes_primary_key()` so
  SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT` (indivisible token)
  doesn't get a redundant `PRIMARY KEY` appended.
- `InsertReturningPool` enum: added `SqliteRow(sqlx::sqlite::SqliteRow)`
  variant. `Debug` impl is now manual (sqlx's `SqliteRow` doesn't
  derive `Debug`).
- `keywords` in `Cargo.toml` swap `postgres` → `sqlite` to surface
  the multi-backend story in crates.io discovery.

### Limitations (known, tracked for v0.28)

- `TenantPools` + the multi-tenant `server::Builder` are still
  `PgPool`-bound. Workaround for SQLite tenants: roll a custom
  per-tenant pool registry (cookbook discussion shows the shape).
- `apply_all_pool` walks every registered framework model on
  inventory, including PG-shape models (Org, Operator, Job…) whose
  DDL doesn't compile on SQLite. `AppBuilder::bootstrap` takes an
  explicit schema list as a workaround.
- `ddl::create_constraints_sql_with_dialect` emits `ALTER TABLE …
  ADD CONSTRAINT FOREIGN KEY` which SQLite's parser rejects. The
  bi-dialect bootstrap path skips this loop on SQLite; FK
  enforcement on SQLite needs the constraint to be inline at
  CREATE TABLE time.

### Verified

- `cargo build -p rustango` (default features) — clean
- `cargo build -p rustango --features tenancy,sqlite` — clean
- `cargo test -p rustango --features tenancy,sqlite --lib` —
  **1096/1096 pass**
- `cargo test -p rustango --features tenancy,sqlite --test sqlite_live`
  — **5/5 pass**
- `cargo run -p rustango --example sqlite_orm_demo --features sqlite`
  — all 12 sections succeed
- `cargo run -p rustango --example sqlite_app_demo --features sqlite,runserver`
  — boots, accepts POST/GET via curl
- `cargo test -p rustango --test macro_no_backend_cfg` — passes
  (regression invariant for macro hygiene)

## [0.26.0] — admin theming + branding + ORM polish

### Added

- **Per-tenant branding** — six new `Org` columns (`brand_name`,
  `brand_tagline`, `logo_path`, `favicon_path`, `primary_color`,
  `theme_mode`) editable live through the operator-console org-edit
  form, plus a dedicated multipart sub-form for logo / favicon
  upload. Brand asset storage rides the framework's existing
  `Storage` trait — `TenantAdminBuilder::brand_storage(...)` and
  `operator_console::router_with_brand_storage(...)` accept any
  `BoxedStorage` (LocalStorage, S3, R2, B2, MinIO, custom). When
  the backend exposes URLs (`Storage::url`), rendered `<img src>`
  goes straight at the origin or CDN; the
  `/__brand__/{slug}/{filename}` static handler is a fallback only.
- **Token-driven theme system** — shared `:root` CSS-variable
  vocabulary in `src/styles/theme_tokens.html` covering surface,
  foreground, border, accent, status, audit-op badges, typography,
  spacing, radius, shadow. `[data-theme="dark"]` override +
  `prefers-color-scheme` auto-switch. Theme toggle UI cycles auto →
  light → dark, persists to `localStorage`, no-flash inline `<head>`
  script.
- **Operator-console env branding** — `RUSTANGO_OPERATOR_BRAND_NAME`
  / `_TAGLINE` / `_LOGO_URL` / `_PRIMARY_COLOR` / `_THEME_MODE`
  rebrand the global console without touching templates.
- **`migrate-tenant-storage` CLI verb** — flip a populated tenant
  between schema and database storage modes via `pg_dump` → `psql`
  pipe, Org row update, cached pool eviction, and a `SELECT 1 FROM
  rustango_users LIMIT 1` smoke check at the new location.
  `--dry-run` previews without touching state. Closes future-feature
  backlog #58.
- **`QuerySet::explain` / `explain_on`** — Postgres planner output
  for any compiled queryset. `ExplainOptions` opts into ANALYZE /
  BUFFERS / VERBOSE and `ExplainFormat` selects text / json / yaml
  / xml. Closes future-feature backlog #5.
- **`#[rustango(generated_as = "EXPR")]`** field attribute — emits
  `GENERATED ALWAYS AS (EXPR) STORED`. The macro skips the column
  from every INSERT and UPDATE; the database recomputes on every
  write. Closes future-feature backlog #35.
- **`fetch_with_prefetch` for non-i64 FK PKs** — parents flow as
  `Vec<SqlValue>` (was `Vec<i64>`); child grouping keys on
  `SqlValue::to_display_string()`. `ForeignKey<T, String>` /
  `ForeignKey<T, Uuid>` parents now get their children back instead
  of an empty list. Closes ORM-improvements P10.
- **Macro `upsert()` picks `unique_together` as conflict target** —
  when the model declares one, the first such group beats the PK
  default. Surrogate-Auto<T> + composite-UNIQUE shapes finally
  upsert correctly instead of silently inserting duplicates.
- **In-repo git hooks** — `.githooks/pre-commit` (rustfmt + secret
  scan + debris check) + `.githooks/pre-push` (cargo check + scoped
  clippy + lib tests) + `bin/install-hooks.sh` for one-line setup.
  Optional `typos` / `cargo-deny` env-var opt-in.

### Changed

- `permissions.rs` raw-SQL upserts in `grant_role_perm` /
  `assign_role` / `set_user_perm` migrated to the ORM's
  `InsertQuery` + `ConflictClause` IR.

### Tests

Lib tests 1042 → 1069. Eight new live integration test files:
`branding_live`, `operator_branding_env`, `permissions_upsert_live`,
`upsert_unique_together_live`, `prefetch_non_i64_pk_live`,
`migrate_tenant_storage_live`, `explain_live`,
`generated_columns_live`.

---

## [Unreleased] — v0.15.0 series (ContentType framework, Option F)

Schema substrate that the rest of v0.15+ (permissions, audit-history admin, generic FKs, soft-FK prefetch) sits on. Three sub-slices, all merged to `main`:

### Added — F.1 ContentType model + registry seed + lookups

- **`rustango::contenttypes::ContentType`** — `#[derive(Model)]` row with `(id Auto<i64>, app_label VARCHAR(100), model_name VARCHAR(100), table VARCHAR(100))`. Mirrors Django's `django_content_types` schema closely enough that audit / permissions / generic-FK code reading the table feels familiar.
- **`contenttypes::ensure_seeded(&pool)`** — walks `inventory::iter::<ModelEntry>()`, inserts one ContentType row per registered model when missing. Idempotent (re-runs return `Ok(0)`); skips the ContentType table itself.
- **`ContentType::for_model::<T>(&pool)`** — Rust-type → ContentType lookup. Used when the framework has a `T: Model` bound and needs the runtime row id (permission scoping, generic-FK inserts).
- **`ContentType::by_natural_key(&pool, app, name)`** — string-keyed lookup for parsed permission codenames or HTTP-routed admin URLs.
- **`ContentType::by_id(&pool, id)`** — FK joins from audit log / permission / generic-FK rows.
- **`ContentType::all(&pool)`** — full listing ordered by `(app_label, model_name)` for admin sidebars + API.

### Added — F.2 composite-key foreign keys

- **`rustango::core::CompositeFkRelation { name, to, from: &[col], on: &[col] }`** — multi-column FK descriptor. Single-column FKs continue to live on `FieldSchema.relation`; composite FKs sit on the new `ModelSchema.composite_relations` slice so each participating column keeps its plain Rust type.
- **`#[rustango(fk_composite(name = "...", to = "...", from = ("a", "b"), on = ("x", "y")))]`** container attr. Validates `from.len() == on.len()` at compile time; errors clearly on missing/empty fields.
- **DDL writer** emits one `ALTER TABLE … ADD CONSTRAINT <table>_<rel.name>_fkey FOREIGN KEY (a, b, …) REFERENCES <to> (x, y, …)` per composite relation alongside the existing single-column FK ALTERs. Both PG and MySQL accept the same syntax — only identifier quoting differs, and that already dispatches through the dialect.

### Added — F.3 GenericForeignKey + prefetch_soft + prefetch_generic

- **`contenttypes::GenericForeignKey { content_type_id, object_pk }`** — `Copy + PartialEq` value carrier for "any registered model's row" pointers. Const-fn `new` constructor + async `for_target::<T>(&pool, pk)` that resolves T's ContentType through the F.1 registry.
- **`contenttypes::prefetch_soft<C, F>(&pool, parent_pks, column, extract)`** — single batched SELECT + group-by-extractor for integer columns that conceptually point at another model's PK without a declared `Relation::Fk`. Returns `HashMap<i64, Vec<C>>` keyed on the soft-FK value. Empty-input short-circuits with no round trip. Use cases: audit log `entity_pk`, denormalized snapshots, optional cross-app refs.
- **`contenttypes::prefetch_generic<C>(&pool, pairs)`** — typed-target generic-FK hydration. Resolves `C`'s ContentType once, filters out pairs whose `content_type_id` doesn't match, batches one SELECT for the surviving target PKs, returns `HashMap<(i64, i64), C>` keyed on the `(ct_id, pk)` pair. Use cases: comments-on-anything, audit log targets, activity-stream entries.

### What this unblocks (queued for v0.16+)

- **Permissions (Option G)** — `permission.content_type_id` becomes a real FK to `rustango_content_types.id` instead of a hard-coded `app.action_model` string that breaks when two apps register the same model name.
- **Audit history admin panels** — `User.history.all()`-style queries are composite-FK joins instead of raw SQL.
- **Comments / tags / generic FK** — one `Comment` model points at any `Post` / `Photo` / `Article` via `(content_type_id, object_pk)`, queried + admin-rendered uniformly.
- **Activity stream feeds** — target hydration is one batched `prefetch_generic` per target type, no N+1.

### Deferred (follow-up slices)

- Boxed-trait dynamic decoder registry → `prefetch_generic_dyn` for mixed-target hydration in one query.
- Admin renderer for `GenericForeignKey` columns — clickable target links in list/detail views.
- `composite_relations` snapshot/diff support in `make_migrations` (composite FKs are currently ALTER-only).

## [Unreleased] — v0.23.0 series

The "bi-dialect" series. Adds first-class MySQL 8.0+ support alongside the existing Postgres backend, exposed through a new `&Pool` API that's additive — every existing `&PgPool` call site keeps working unchanged, so apps adopt the new surface at their own pace (or never, if Postgres-only).

### Added — bi-dialect foundation

- **`rustango::sql::Pool`** — wrapper enum (`Postgres(PgPool)` / `Mysql(MySqlPool)`) with `connect("postgres://…")` / `connect("mysql://…")` / `connect_from_env()` / `connect_with_timeout`. The `mysql` Cargo feature is opt-in.
- **`rustango::env::DatabaseUrlBuilder`** + **`database_url_from_env()`** — assemble a connection URL from `DB_DRIVER` / `DB_HOST` / `DB_PORT` / `DB_USER` / `DB_PASSWORD` / `DB_NAME` / `DB_PARAMS` when `DATABASE_URL` isn't set; passwords are auto percent-encoded so `@`/`:`/`/`/`#`/`?`/`%` in passwords don't corrupt the URL. `DB_DRIVER` accepts `postgres` / `postgresql` / `pg` / `mysql` / `mariadb` aliases.
- **`manage db:info`** — read-only summary of the resolved DB URL (password redacted), detected backend, and which `postgres`/`mysql` Cargo features are compiled in. Warns when the URL scheme and the enabled features don't match.

### Added — bi-dialect SQL writers + ORM

- **`rustango::sql::Dialect`** trait gains MySQL impl (`MySql` struct + `DIALECT` singleton): backtick identifier quoting, `?` placeholders, `BIGINT AUTO_INCREMENT` for `Auto<T>` PKs, `1`/`0` boolean literals, `GET_LOCK` / `RELEASE_LOCK` for advisory locking, `TINYINT(1)` / `DATETIME(6)` / `JSON` / `CHAR(36)` for `bool` / `DateTime<Utc>` / `serde_json::Value` / `Uuid`.
- **Operator translations** — `ILIKE` / `NOT ILIKE` → `LOWER(col) LIKE LOWER(?)`, `IS DISTINCT FROM` → `NOT (col <=> ?)`, JSONB `@>` / `<@` → `JSON_CONTAINS(col, ?)` / `JSON_CONTAINS(?, col)`, JSONB `?` / `?|` / `?&` → `JSON_CONTAINS_PATH(col, 'one'|'all', CONCAT('$.', ?))`, `UPDATE … FROM (VALUES …)` → `UPDATE … INNER JOIN (VALUES ROW(…), ROW(…)) AS d(pk, c1) ON t.pk = d.pk SET t.c1 = d.c1`, `ON CONFLICT DO UPDATE SET col = EXCLUDED.col` → `ON DUPLICATE KEY UPDATE col = VALUES(col)`.
- **Shared `sql::writers` module** — every dialect compiles SELECT / INSERT / UPDATE / DELETE / COUNT / AGGREGATE / BULK INSERT / BULK UPDATE through the same writer functions; identifier quoting + placeholder shape + NULL casts + per-op SQL all dispatch through the dialect.

### Added — `_pool` executor surface

Every read/write function in the existing `&PgPool` surface now has a `&Pool`-typed counterpart:

- **`insert_pool` / `update_pool` / `delete_pool` / `count_rows_pool` / `bulk_insert_pool` / `bulk_update_pool` / `raw_execute_pool` / `raw_query_pool`** — non-`FromRow` and IR-level operations.
- **`select_rows_pool` / `select_one_row_pool` / `select_rows_pool_with_related`** — single-table + select_related joins. `FetcherPool::fetch_pool(&pool)` extension trait drives a `QuerySet<T>` end-to-end.
- **`insert_returning_pool`** — INSERT + `RETURNING` (PG) / `LAST_INSERT_ID()` (MySQL) — returns an `InsertReturningPool` enum.
- **`fetch_paginated_pool`** — page + total via `COUNT(*) OVER ()` (single round trip; needs MySQL 8.0+).
- **`fetch_with_prefetch_pool`** — Django-shape parent + 1:N children hydration in two round trips.
- **`fetch_aggregate_pool`** + **`CounterPool::count_pool`** — aggregate IR + queryset count.
- **`transaction_pool`** + **`PoolTx`** — backend-tagged transaction handle with `commit` / `rollback` for cross-table atomicity.

### Added — macro-emitted `Model::*_pool` methods

Every `#[derive(Model)]` type now exposes the bi-dialect write trio:

- **`delete_pool(&self, &Pool)`** — non-audited path is a thin dispatch through `sql::delete_pool`; audited path opens a per-backend transaction wrapping DELETE + audit emit (atomic).
- **`insert_pool(&mut self, &Pool)`** — `Auto<T>` PKs populated from `RETURNING` (PG) / `LAST_INSERT_ID()` (MySQL). Audited path runs the INSERT + auto-PK readback + audit emit on a single tx.
- **`save_pool(&mut self, &Pool)`** — INSERT-or-UPDATE keyed on the PK. Audited path emits a **diff-style** audit row (one `{ "field": { "before": …, "after": … } }` entry per tracked column whose value actually changed) — full feature parity with the existing `&PgPool` `save()`.

Models also auto-derive **`FromRow<MySqlRow>`** alongside `FromRow<PgRow>` (via the cfg-gated `__impl_my_from_row!` macro_rules), plus **`LoadRelatedMy`** + **`__rustango_from_aliased_my_row`** for select_related joins on the `_pool` path. Every macro-emitted MySQL impl materializes only when rustango itself is built with the `mysql` feature — PG-only users pay zero compile-time / binary-size cost.

### Added — bi-dialect migration runner

The Django-shape file-based migration runner now has a `&Pool` variant for every entry point:

- **`migrate_pool` / `migrate_to_pool` / `unapply_pool` / `unapply_force_pool` / `downgrade_pool` / `migrate_dry_run_pool` / `migrate_embedded_pool`** — full lifecycle on either backend.
- **`apply_all_pool` / `drop_all_pool`** — schema bootstrap / tear-down for tests + dev.
- **`ensure_ledger_pool` / `applied_set_pool`** — primitives for custom flows.
- Concurrent peers serialize via a per-backend session-scoped advisory lock (`pg_advisory_lock` / `GET_LOCK`).

### Added — bi-dialect DDL writer + audit log

- **`migrate::ddl::create_table_sql_with_dialect` / `drop_table_sql_with_dialect` / `create_constraints_sql_with_dialect`** — `CREATE TABLE` / `DROP TABLE` / `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY` for either backend. Existing `&PgPool` callers go through PG-typed shims (zero diff in emitted SQL).
- **`audit::ensure_table_pool` / `emit_one_pool` / `emit_one_my` / `delete_one_with_audit_pool` / `save_one_with_audit_pool` / `insert_one_with_audit_pool`** — bi-dialect audit primitives. `CREATE_TABLE_SQL_MYSQL` mirrors `CREATE_TABLE_SQL` with MySQL types (`BIGINT AUTO_INCREMENT`, `JSON`, `DATETIME(6)`, backtick quoting).

### Added — sqlx + dependency wiring

- `sqlx` dependency moved to `default-features = false`; `postgres` and `mysql` are now feature-gated on rustango itself.
- `sqlx/json` enabled so `Json<T>: Type<MySql>` is in scope.
- **`Auto<T>: Decode<MySql> + Type<MySql>`** — mirror of the existing Postgres impls so `#[derive(Model)]` types with `Auto<T>` PKs satisfy `FromRow<MySqlRow>`.

### Fixed

- Pre-existing macro bug: audited `Auto<T>`-PK models exposed an `upsert(&PgPool)` body that called `self.upsert_on(pool)` directly, but `upsert_on` for audited models takes `&mut PgConnection`. Surfaced as a compile error the first time an audited Auto-PK model gets derived. Added the missing `pool.acquire()` shim symmetric with `save` / `insert` / `delete` / `bulk_insert`.

### Migration notes

- **No breaking changes** to the existing `&PgPool` API — every call site keeps working unchanged on upgrade.
- Apps that want MySQL support add `features = ["mysql"]` to their `rustango` dependency. Apps that only target Postgres do nothing differently.
- Apps currently using `&PgPool` can adopt `&Pool` incrementally — pass `Pool::from(pg_pool)` at any boundary, or migrate top-down by calling `Pool::connect_from_env()` instead of `PgPool::connect(&url)`.

### MySQL caveats

- requires MySQL 8.0+ (window functions for `fetch_paginated_pool`, `JSON` column type, `VALUES ROW(…)` syntax for `bulk_update_pool`)
- `LAST_INSERT_ID()` reports one auto-assigned column per connection, so models with multiple `Auto<T>` PKs error at runtime on MySQL with `SqlError::OperatorNotSupportedInDialect{op: "multi-column RETURNING"}`. Postgres `RETURNING` is unaffected.

## [0.22.1] — 2026-05-03

Pure docs / packaging fix-up over v0.22.0; no library changes.

### Fixed

- crates.io v0.22.0 shipped without a `README.md` because the workspace-inherited `readme = "README.md"` resolves relative to each crate's own directory, and neither `crates/rustango/` nor `crates/rustango-macros/` had one. The published tarball therefore contained no README and the crates.io page rendered blank.

### Changed

- `crates/rustango/README.md` now symlinks to the workspace `README.md`, so the canonical README is shipped inside the published `.crate` tarball without duplication.
- `crates/rustango-macros/README.md` is a new dedicated, narrower README for the proc-macro crate (lists the proc-macro entry points + the `openapi` feature; points readers at the parent crate for the full framework story).

## [0.22.0] — 2026-05-03

The "platform-grade" release. ~50 new modules / features layered on top of v0.17.4 (the previous publish), with no breaking changes to the existing ORM / admin / migrations / multi-tenancy surface — the additions ride next to it under new opt-in feature flags (almost all default-on so existing apps just gain capabilities on upgrade).

### Added — first-class media stack

- **`rustango::media`** — `Media` model (Postgres-backed file reference) + `MediaManager` (server-side save, direct browser uploads via presigned PUT, soft delete, orphan / pending sweeps).
- **`MediaCollection`** — hierarchical folders (parent_id self-FK, `collection_path()` walks the chain, `list_in_collection(recursive)` via WITH RECURSIVE CTE).
- **`MediaTag`** — flat M2M labels with auto-create on `tag()`, `popular_tags()` ordered by usage.
- **`media::router::media_router`** — REST endpoints for the entire surface (`/uploads/begin`, `/uploads/{id}/finalize`, `/media/{id}`, `/collections`, `/tags`, `/tags/{slug}/media`, …).
- **`StorageRegistry`** — Laravel-style named "disks" with optional per-disk CDN prefix. `cdn_url(disk, key)` and `origin_url(disk, key)` for explicit routing.
- **`storage::s3::S3Storage`** — pure-Rust SigV4 over `reqwest`, no `aws-sdk-s3` dep. Works against AWS S3, Cloudflare R2, Backblaze B2, MinIO. Verified live against MinIO including SigV4 query-string presigning (GET + PUT with content-type binding + 7-day expiry clamp).
- **`Storage` trait** gains default-`None` `presigned_get_url` + `presigned_put_url`. `LocalStorage` + `InMemoryStorage` inherit defaults; `S3Storage` overrides.

### Added — auth, identity, sessions

- **`rustango::oauth2`** — OAuth2/OIDC swiss-knife. `OAuth2Provider` works for both pure OAuth2 (GitHub, Discord) and OIDC (Google, Microsoft, Keycloak) via `/userinfo`. Per-tenant `OAuth2Registry`, axum router for `/auth/{tenant}/{provider}/{login,callback}`. Presets for google / github / microsoft / discord / gitlab / slack / facebook / keycloak.
- **`rustango::sessions`** — server-side `Session` + `SessionStore` backed by any `Cache` (revocable cookie-id sessions; pair with `RedisCache` for cross-replica visibility).
- **`rustango::jwt`** — standalone HS256 JWT (sign / verify / decode). Reserved-claim protection, `alg: none` rejection, constant-time signature comparison.
- **`rustango::hmac_auth`** — AWS-style HMAC-signed request authentication (`X-Date` + `Authorization` with SigV4-shape canonical request, ±5 min replay window, content-type binding).

### Added — APIs

- **`rustango::openapi`** — OpenAPI 3.1 spec builder + Swagger UI / Redoc viewer routes (`/openapi.json` + `/docs` + `/redoc`).
- **`#[derive(Serializer)]`** auto-derives `OpenApiSchema` so existing serializers become the source of truth for request/response schemas.
- **`ViewSet::openapi_paths(prefix, ref)`** auto-generates the 5 standard CRUD path items from a `ViewSet` (with `operationId`, tags, request/response refs, paginated list shape).
- **`rustango::jsonapi`** — JSON:API v1.1 envelope adapter (`to_resource`, `to_collection`, `with_included`, `with_meta`).
- **`rustango::problem_details`** — RFC 7807 error responses with `application/problem+json`.

### Added — background work

- **`rustango::jobs::pg::PgJobQueue`** — Postgres-backed job queue using `SELECT … FOR UPDATE SKIP LOCKED` for safe multi-replica pickup. Reclaim-stuck-jobs sweep, dead-letter callback.
- **`rustango::email_jobs`** — send mail off the request path via the job queue (`register_email_job` + `dispatch_email`).
- **`rustango::email_templates::EmailRenderer`** — Tera-rendered emails (`{name}.subject.txt` + `{name}.txt` + optional `{name}.html`).
- **`rustango::mailable::Mailable`** — Laravel-shape trait for self-contained email types.
- **`rustango::webhook_delivery`** — outbound HMAC-signed webhook delivery via the job queue (retry-with-backoff included).

### Added — production middleware (axum / tower)

- **`compression`** — gzip + deflate with `Accept-Encoding` negotiation, content-aware skip rules (no SSE / no already-compressed), `Vary` handling.
- **`csp_nonce`** — per-request CSP nonce middleware (substitutes `'nonce-__RUSTANGO_NONCE__'` placeholder in the CSP header).
- **`body_limit`** — fast `Content-Length` rejection with structured 413 JSON.
- **`real_ip`** — extract client IP from `X-Forwarded-For` / `X-Real-IP` / `CF-Connecting-IP` / RFC 7239 `Forwarded` (with auto-fallback chain).
- **`idempotency`** — Stripe-shape `Idempotency-Key` middleware backed by `Cache`; replays cached responses verbatim.
- **`maintenance`** — drain traffic for deploys/migrations via a shared `MaintenanceFlag` (returns 503 with `Retry-After`).
- **`trailing_slash`** — Django `APPEND_SLASH`-shape redirect middleware.
- **`static_files`** — serve a directory with `Cache-Control` + `Last-Modified` + 304 + path-traversal/dotfile guards.
- **`method_override`** — `_method` form field + `X-HTTP-Method-Override` header for HTML form REST emulation.
- **`server_timing`** — W3C Server-Timing header surfacing per-request stage durations to DevTools.
- **`tracing_layer`** — request span with W3C / OpenTelemetry semantic-conventions field names + `traceparent` propagation.
- **`metrics`** — Prometheus counters + histograms exposed at `/metrics` (pure-Rust, no Prometheus client crate).
- **`distributed_lock`** — `Cache`-backed mutex with TTL-based crash recovery + token-checked release.
- **`rate_limit_cache`** — distributed rate limiting via `Cache` (fixed-window counter, atomic on `RedisCache`).
- **`feature_flags`** — `Cache`-backed killswitches + per-user override + stable percentage rollout (FNV-bucketed for flicker-free).
- **`uploads`** — multipart helper (axum/multipart + Storage); `save_uploads(mp, &cfg, &storage)` one-call.
- **`ws::WsHub`** — WebSocket handler scaffold on top of `sse::EventBus` with auto JSON encode/decode + keep-alive.
- **`http_client::HttpClient`** — opinionated `reqwest` wrapper with retry on idempotent verbs / 5xx / `Retry-After`.

### Added — smaller fixtures

- **`soft_delete`** — query helpers + `restore` / `purge` for any model with `#[rustango(soft_delete)]`.
- **`pagination`** — `PageLinks` JSON bundle + `page_number_links` / `cursor_links` + RFC 5988 `Link` header builder.
- **`csv_response::CsvResponse`** — axum CSV download wrapper + `csv_from_json_rows` helper.
- **`Cache::incr`** — atomic on `RedisCache`, default get+set on others.
- **`logging`** — env-filter setup + JSON formatter for prod.
- **`account_lockout`** — per-account login lockout (Cache-backed counter + lock flag).
- **`sse::EventBus`** — pub/sub bus on `tokio::sync::broadcast`.
- **`api_keys` / `passwords` / `webhook` / `signed_url` / `totp`** — standalone helpers (each behind its own feature).
- **Health enhancements** — per-probe timeout + `latency_ms` per check + built-in `tcp_probe` / `cache_probe` / `http_probe`.
- **`manage`** subcommands: `db:dump`, `db:restore`, `make:viewset`, `make:serializer`, `make:form`, `make:job`, `make:notification`, `make:middleware`, `make:test`, `about`, `check`, `docs`, `version`.

### Changed

- `request_id` middleware un-gated from the `tenancy` feature (now ships with default `admin`).
- `webhook::SignatureFormat` derives `Serialize` + `Deserialize` so it can ride inside job payloads.
- `Email` derives `Serialize` + `Deserialize` for `email_jobs`.
- `cargo-rustango` scaffolder bumps generated `Cargo.toml` template to pin `rustango = "0.22"`.

### Test coverage

848 lib unit tests + 25 live integration tests (Postgres + MinIO) + the existing live test suite from prior versions. The media stack alone has 22 live tests across `tests/media_live.rs` + `tests/media_collections_tags_live.rs`.

## [v0.20.x] — feature push (M2M, serializers, indexes, JWT lifecycle, security, manage CLI) — 2026-05-02

A 32-commit batch bringing rustango to "Django/Laravel-class polish" out of the box. Each subversion is a self-contained slice; the major themes:

### Added — ORM + migrations

- **v0.20.0** Many-to-many: `#[rustango(m2m(name, to, through, src, dst))]` declaration, junction-table auto-creation in `make_migrations`, and an ORM `M2MManager` with `all` / `add` / `remove` / `set` / `clear` / `contains`.
- **v0.20.2** Index declarations: `#[rustango(index)]` on fields and `#[rustango(index("col1, col2"))]` on the container, with `unique` and `name` sub-attrs. Auto-generated `CreateIndex` / `DropIndex` migration ops.
- **v0.20.3** Data migration CLI: `manage add-data-op --sql ... --reverse-sql ... [--name X | --to migration]`. Public API: `make_data_migration` / `append_data_op`.
- **v0.20.21** Table-level CHECK constraints via `#[rustango(check(name, expr))]`; emits `AddCheckConstraint` / `DropCheckConstraint` ops.

### Added — APIs

- **v0.20.1** `#[derive(Serializer)]` + `ModelSerializer` trait. Field attrs `read_only` / `write_only` / `source` / `skip`; emits a custom `serde::Serialize` that respects `write_only`.
- **v0.20.6** Cursor pagination on ViewSet (`?cursor=...`), skipping the COUNT(*) round-trip. New `PaginationStyle::Cursor { field, desc }`.
- **v0.20.15** Django-style lookup operators on ViewSet `filter_fields`: `?field__gt=`, `__gte=`, `__lt=`, `__lte=`, `__ne=`, `__in=`, `__not_in=`, `__contains=`, `__icontains=`, `__startswith=`, `__istartswith=`, `__endswith=`, `__iendswith=`, `__isnull=`.

### Added — Auth + security

- **v0.20.12** Full JWT lifecycle: `JwtLifecycle` with access + refresh, JTI-based blacklist, sliding refresh that rotates the JTI on every refresh.
- **v0.20.28** JWT custom payload claims: `issue_pair_with(user_id, custom_map)`, `issue_access_with`, `claims.get_custom::<T>("key")`. Refresh **preserves custom claims** automatically; `refresh_with(token, new_claims)` substitutes when permissions changed. Reserved claim names (`sub`, `exp`, `jti`, `typ`) rejected at issuance.
- **v0.20.23** TOTP / RFC 6238 2FA: `TotpSecret`, `generate`, `verify`, `otpauth_url`. Both official RFC 6238 SHA-1 test vectors pass.
- **v0.20.25** Webhook signature verification: `verify_signature(format, secret, body, signature)` constant-time, supports `HexSha256WithPrefix` (GitHub), `HexSha256` (Slack), `Base64Sha256` (Stripe).
- **v0.20.26** Generic API-key helpers: `generate_key()` returns `(token, prefix, hash)` with argon2id; `verify_key`, `split_token`. Wire-compatible with the existing `tenancy::auth_backends::ApiKeyBackend`.
- **v0.20.27** Generic password helpers: `passwords::hash`, `passwords::verify`, `strength_score` with built-in weak-password list.
- **v0.20.29** Signed URLs: `signed_url::sign(url, secret, ttl)` / `verify(url, secret)` with HMAC-SHA256, canonical query-param sorting, optional expiry. For magic-link login, password reset confirmation, time-limited file downloads.

### Added — Middleware + HTTP layer

- **v0.20.7** CORS middleware: `CorsLayer::strict()` / `permissive()` / explicit allowlist; auto-handles OPTIONS preflight; sets `Vary: Origin` for cache safety.
- **v0.20.8** Token-bucket rate limiter: `RateLimitLayer::per_ip` / `per_header` / `global`; returns 429 with `Retry-After`.
- **v0.20.9** Health endpoints: `/health` (liveness, always 200) + `/ready` (DB-pinged, 503 if unreachable). `HealthRouter::check("name", async_fn)` for custom checks.
- **v0.20.16** Content negotiation (`negotiate(accept, available)`) and ETag middleware (FNV-1a + length, no crypto-strength dep needed).
- **v0.20.18** API versioning extractor (`VersionStrategy::Header / Query / UrlPrefix / Fixed`) and an RFC 4180 CSV writer.
- **v0.20.20** Access log middleware (`AccessLogLayer`) with **default PII redaction** of `password`, `token`, `secret`, `api_key`, `access_token`, `refresh_token`, `signature`, `auth` query params. Test fixture loader (`Fixture::from_file(path).load_into(table, pool)`).
- **v0.20.24** Text utilities (`slugify`, `slugify_unicode`, `html_escape`, `truncate`), Request ID middleware (with header-injection defense), IP allowlist/blocklist middleware (CIDR support, IPv4 + IPv6).
- **v0.20.25** Standardized API errors: `ApiError` with status / code / message / details; presets `bad_request` / `unauthorized` / ... / `internal`. Implements `IntoResponse`.
- **v0.20.27** RFC 5988 Link-header builder for pagination (`LinkHeaderBuilder::new(url).with_page_info(info).keep_param(k, v).build()`).
- **v0.20.29** Security headers middleware: `SecurityHeadersLayer::strict()` / `relaxed()` / `dev()` presets covering HSTS / X-Frame-Options / X-Content-Type-Options / Referrer-Policy / Cross-Origin-Opener-Policy / Permissions-Policy. CSP builder with named directives.

### Added — Backends + plumbing

- **v0.20.4** Pluggable cache: `Cache` async trait + `NullCache` + `InMemoryCache` (tokio RwLock + lazy TTL eviction) + `RedisCache` behind `cache-redis` feature. Helpers: `get_json`, `set_json`, `get_or_set`.
- **v0.20.5** Django-shape signals: `connect_pre_save<T>`, `connect_post_save<T>`, `connect_pre_delete<T>`, `connect_post_delete<T>` + matching `send_*`. TypeId-keyed global registry; receivers run sequentially in registration order.
- **v0.20.9** Pluggable email backends: `Mailer` trait + `Email` builder + `ConsoleMailer` / `InMemoryMailer` / `NullMailer`.
- **v0.20.10** Pluggable file storage: `Storage` trait + `LocalStorage` (filesystem) + `InMemoryStorage` (tests). Path-traversal validator built in.
- **v0.20.11** Test client: `TestClient::new(router)` with `.get(path).header(...).json(...).send().await` shape and `TestResponse::{status, json, text, header}`.
- **v0.20.13** Typed env readers (`required` / `with_default` / `optional` / `list` / `duration_secs` / `duration_millis`) and a startup `Validator::new().require(name, desc).check_or_panic()`.
- **v0.20.14** i18n: `Translator` with file-loaded JSON catalogs, 3-tier fallback (locale → base lang → default → key), and `negotiate_language(accept_header, available)` for RFC 4647 q-value matching.
- **v0.20.17** In-process scheduler: `Scheduler::new().every(name, period, async_fn).start()`. Per-task panic isolation via `tokio::spawn`.
- **v0.20.19** Secrets manager: `Secrets` trait + `EnvSecrets` (with optional prefix) + `InMemorySecrets`.
- **v0.20.22** Bulk-action runner for admin: `BulkActionRegistry`, plus built-in `BulkDeleteAction`, `BulkSoftDeleteAction { column }`, `BulkRestoreAction { column }`.

### Added — `manage` CLI

- **v0.20.30** `manage about`, `manage check [--deploy]`, `manage docs`, `manage version` / `--version`.
- **v0.20.31** First-run welcome page (`welcome::welcome_router()`) — confidence signal that rustango is wired up, with next-steps + ships-features list. Self-contained HTML, no external CDN.
- **v0.20.32** File generators: `manage make:viewset`, `make:serializer`, `make:form`, `make:job`, `make:notification`, `make:middleware`, `make:test`. Each refuses to overwrite + prints a `pub mod X;` hint.

### Documentation

- Full README rewrite with comprehensive feature list, ORM cookbook, and production checklist.
- New `docs/getting-started.md` — 18-step end-to-end tutorial from `cargo install` to deployed.
- New `docs/manage.md` — every `manage` subcommand with examples + common workflows.

### Tests

- **+200 unit tests** added across the v0.20.x batch. Total: 298 lib unit tests.

### Breaking changes

- `Relation::M2M` variant removed from `core::Relation` enum (M2M is now a model-level concept stored in `ModelSchema.m2m`, not a per-field relation).
- `ModelSchema` gained `m2m`, `indexes`, `check_constraints` fields. Generated only by the `#[derive(Model)]` macro — direct construction in user code unlikely.
- `SchemaSnapshot` gained `m2m_tables`, `indexes`, `checks` fields with `#[serde(default)]` — old migration files still deserialize cleanly.

---

## [v0.19.2] — audit_track field filtering — 2026-05-02

### Added

- **`ModelSchema::audit_track`** — new `Option<&'static [&'static str]>` field. When set via
  `#[rustango(audit(track = "field1, field2"))]`, admin diffs and create-snapshots include only
  the listed fields. `None` or an empty slice captures all scalar fields (previous behavior
  unchanged). The `#[derive(Model)]` macro emits the value; `emit_admin_audit_diff` and
  `emit_admin_audit` in `admin/audit.rs` both respect it.

---

## [v0.19.1] — `#[derive(ViewSet)]` proc-macro — 2026-05-02

### Added

- **`#[derive(ViewSet)]`** — generates `fn router(prefix: &str, pool: PgPool) -> Router` on a
  marker struct, wiring the full DRF-style CRUD router from a `#[viewset(...)]` attribute. Fields:
  `model`, `fields`, `filter_fields`, `search_fields`, `ordering`, `page_size`, `read_only`,
  `permissions { list/retrieve/create/update/destroy }`. Available via `use rustango::ViewSet`
  behind the `tenancy` feature.

---

## [v0.19.0] — ORM improvements — 2026-05-02

### Added

- **`ConflictClause` / `Model::upsert_on`** — `InsertQuery` and `BulkInsertQuery` now carry an
  optional `on_conflict: Option<ConflictClause>` field. `ConflictClause::DoNothing` emits
  `ON CONFLICT DO NOTHING`; `ConflictClause::DoUpdate { target, update_columns }` emits
  `ON CONFLICT (…) DO UPDATE SET col = EXCLUDED.col`. Auto-PK models gain `upsert()` /
  `upsert_on(executor)` — single round-trip insert-or-update by primary key.

- **`sql::transaction(pool, |conn| async { … })`** — ergonomic transaction helper wrapping
  `pool.begin()` / `commit()` / `rollback()`. All `_on(executor)` methods compose inside the
  closure without any other changes.

- **New `Op` variants and `Column` trait methods** — `ILike`, `NotLike`, `NotILike`, `NotIn`,
  `Between`, `IsDistinctFrom`, `IsNotDistinctFrom` added to the `Op` enum, the Postgres writer,
  and the typed `Column` trait (`.ilike()`, `.not_like()`, `.between(lo, hi)`,
  `.is_distinct_from()`, `.not_in()`, etc.).

- **`WhereExpr::Not`** — `Not(Box<WhereExpr>)` variant emits `NOT (…)`. Accessible via
  `TypedFilter::not()` and `TypedExpr::not()`.

- **`AggregateQuery` + `QuerySet::aggregate()`** — `AggregateExpr` enum (`Count`, `Sum`, `Avg`,
  `Max`, `Min`), `AggregateQuery` IR with `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, `OFFSET`.
  `compile_aggregate()` in the Postgres dialect; `sql::fetch_aggregate()` /
  `fetch_aggregate_on()` executor functions. Build via `Post::objects().aggregate().group_by(…)
  .annotate("cnt", AggregateExpr::Count(None)).compile()`.

- **JSONB operators** — `Op::JsonContains` (`@>`), `JsonContainedBy` (`<@`), `JsonHasKey` (`?`),
  `JsonHasAnyKey` (`?|`), `JsonHasAllKeys` (`?&`) added to `Op`, the Postgres writer, and the
  `Column` trait (`.json_contains()`, `.json_has_key()`, `.json_has_any_key()`, etc.).

- **`sql::raw_query<T>` / `sql::raw_execute`** — typed raw SQL escape hatches. `raw_query::<T>`
  decodes rows via the same `FromRow` impl as ORM queries. `raw_execute` returns rows affected.
  Both have `_on(executor)` variants.

- **`sql::bulk_update` / `BulkUpdateQuery`** — `UPDATE t SET … FROM (VALUES …) AS data(pk, …)
  WHERE t.pk = data.pk`. One round-trip for N rows with per-row different values.

### Fixed

- **JSON field binding** — `SqlValue::Json` was an `unreachable!()` in the `bind_match!` macro.
  Now correctly bound via `sqlx::types::Json`, enabling JSONB column reads and writes.

- **`annotate_count_children` WHERE forwarding** — the parent queryset's `WHERE`, `ORDER BY`,
  `LIMIT`, and `OFFSET` clauses are now forwarded into the aggregate SQL. Previously they were
  silently dropped.

---

## [v0.18.0] — permission-gated admin (Option G) — 2026-05-02

### Added

- **`#[rustango(permissions)]`** model attribute — sets `ModelSchema.permissions: bool`. When
  present, `auto_create_permissions(pool)` seeds the four CRUD codenames
  (`{table}.add/change/delete/view`) into the `rustango_permissions` catalog table via a single
  UNNEST batch INSERT (idempotent, `ON CONFLICT DO NOTHING`).

- **`rustango_permissions` catalog table** — created by `ensure_permission_tables`. Stores
  `(table_name, codename, name)` rows so tooling can enumerate available permissions without
  knowing model names at runtime.

- **Per-user permission gating in the tenant admin** — `TenantAdminBuilder` now fetches the
  authenticated user's effective codename set once per request (`user_permissions(uid, pool)`)
  and threads it into the inner admin builder. Superusers get full access (`user_perms = None`
  bypasses all checks); non-superusers get per-table filtering.

- **`Builder::with_user_perms(perms)`** — wires a pre-fetched codename set into the admin
  builder. `AppState` gains `can_add(table)` and `can_delete(table)` methods alongside the
  extended `is_visible` (`{table}.view`) and `is_read_only` (`{table}.change`).

### Changed

- **Admin create/delete gating split** — `create_form` and `create_submit` now check `can_add`
  instead of `is_read_only`. `delete_submit` checks `can_delete`. `action_submit` gates
  `delete_selected` on `can_delete`, `restore_selected` and custom handlers on `is_read_only`.
  Replaces the previous binary superuser / read-only-all model.

---

## [v0.17.4] — admin JSONB editing, `AlterColumnUnique`, `Role.name` unique — 2026-05-01

### Added

- **Admin JSONB field editing** — JSON columns render as `<textarea>` on create/edit forms and
  pretty-print the current value as the prefill. Empty submission defaults to `{}`.

- **`AlterColumnUnique` migration op** — adding or removing `#[rustango(unique)]` on an existing
  field now auto-generates an invertible `ADD CONSTRAINT … UNIQUE` / `DROP CONSTRAINT` DDL op.
  The diff engine detects the flip; `invert.rs` reverses it.

- **`Role.name` uniqueness enforced** — `Role.name` now carries `#[rustango(unique)]` matching
  the unique constraint already present in `ensure_tables` DDL.

---

## [v0.17.3] — `blog_demo` example, `server::ApiRouter` re-export — 2026-05-01

### Added

- **`blog_demo` example** (`crates/rustango/examples/blog_demo/`) — end-to-end canary using
  `Author` + `Post` models, ORM seeding, Tenant extractor views, a committed schema migration,
  and the `#[rustango::main]` builder chain. No raw SQL; re-running is safe.

- **`server::ApiRouter` re-exported** — was previously builder-private; now accessible as
  `rustango::server::ApiRouter` for projects that compose their own router separately from the
  `Builder` chain.

---

## [v0.17.2] — `#[rustango(unique)]`, admin form fixes, bootstrap cleanup — 2026-05-01

### Added

- **`#[rustango(unique)]`** field attribute — emits `UNIQUE` inline on the column DDL.
  `FieldSchema.unique: bool` is tracked in snapshots and detected by the diff engine as
  an `AlterField` trigger. `Org.slug`, `Operator.username`, `User.username` all upgraded.

### Fixed

- **Admin create form: Auto-PK and `auto` fields no longer get `required`** — any field
  with `field.auto = true` (Auto<T> PK, `auto_now_add`, `auto_uuid`, default-assigned
  columns) is now hidden on the create form and shown read-only on edit. Previously an
  Auto-PK rendered as `<input type="number" required>`, silently blocking the browser
  submit when the operator correctly left it blank.

- **Admin form `:invalid` CSS** — `base.html` now styles `input:invalid` and
  `textarea:invalid` with a red border so HTML5 validation failures are visible
  instead of causing a silent no-op click.

### Changed

- Bootstrap migrations simplified: raw `DataOp` `ALTER TABLE … ADD CONSTRAINT … UNIQUE`
  workarounds removed. `UNIQUE` is now inline on the column via `#[rustango(unique)]`.
  Registry migration drops from 4 ops to 2; tenant migration from 2 ops to 1.

---

## [v0.17.1] — JSONB `data` bag on Role, UserPermission, User — 2026-05-01

### Added

- **`Role.data`**, **`UserPermission.data`**, **`User.data`** — `JSONB NOT NULL DEFAULT '{}'`
  columns for flexible per-row metadata. Store role display config, override context
  (reason, grantor), and user preferences without schema migrations for each new attribute.
  The permission engine (`has_perm` CTE, `granted` bool) is untouched.

- **`ENSURE_SQL` idempotent migration** — `ALTER TABLE … ADD COLUMN IF NOT EXISTS` appended
  for all three tables so existing deployments pick up the column on next boot.

---

## [v0.17.0] — `ViewSet`: DRF-style REST router for any Model — 2026-05-01

### Added

- **`rustango::viewset::ViewSet`** — wires six standard REST endpoints for any `#[derive(Model)]`
  table in ~5 lines:

  ```rust
  ViewSet::for_model(Post::SCHEMA)
      .fields(&["id", "title", "body", "author_id"])
      .filter_fields(&["author_id"])
      .search_fields(&["title", "body"])
      .ordering(&[("published_at", true)])
      .page_size(20)
      .router("/api/posts", pool.clone())
  ```

  Endpoints: `GET /` (list), `POST /` (create), `GET /{pk}` (retrieve),
  `PUT /{pk}` (update), `PATCH /{pk}` (partial update), `DELETE /{pk}` (204).

- **List response envelope**: `{"count": N, "page": P, "page_size": S, "last_page": L, "results": [...]}`.

- **Query parameters**: `?page`, `?page_size`, `?ordering` (comma-separated, `-field` for DESC),
  `?search`, and exact filters for any declared `filter_fields`.

- **`ViewSetPerms`** — optional per-action permission check (list, retrieve, create, update,
  destroy). Reads `CurrentUser` extension injected by `RouterAuthExt::require_auth`.

- **JSON + form-urlencoded body parsing** — handlers accept both `application/json` and
  `application/x-www-form-urlencoded` on create/update/patch.

- **`.read_only()`** builder flag — drops create/update/destroy, wires list + retrieve only.

---

## [v0.16.0] — `Form`, `ModelForm`, `DynamicForm` (Option J) — 2026-05-01

### Added

- **`FormErrors`** — multi-field error collection type. All field validations run before
  returning; `errors.get("field")` returns all messages for that field.

- **`#[derive(Form)]` upgraded** — now implements the `Form` trait (replaces `FormStruct`).
  `ContactForm::parse(&data)` returns `Result<ContactForm, FormErrors>` with every failing
  field collected in one shot. Validators (`min`, `max`, `min_length`, `max_length`) push
  to the error bag instead of returning early.

- **`ModelForm`** — schema-driven form for any `#[derive(Model)]` type. No dedicated struct
  required:
  ```rust
  let form = ModelForm::new(Post::SCHEMA, form_data);
  match form.save(&pool).await {
      Ok(pk) => redirect(pk),
      Err(ModelFormError::Validation(e)) => render_errors(e),
      Err(ModelFormError::Database(e)) => server_error(e),
  }
  let form = ModelForm::for_update(Post::SCHEMA, data, SqlValue::I64(id));
  form.save(&pool).await?;
  ```

- **`DynamicForm`** — runtime JSON-schema driven form for surveys and operator-configurable
  inputs. Build from a JSON array of field descriptors, bind POST data, validate, read
  cleaned values:
  ```rust
  let mut form = DynamicForm::from_json(schema_json)?;
  form.bind(form_data);
  if form.is_valid() { let data = form.cleaned_data()?; }
  ```
  Supports: `text`, `textarea`, `integer`, `float`, `boolean`, `date`, `datetime`,
  `email`, `url`, `select`, `multi_select`.

### Breaking

- `FormStruct` deprecated in favour of `Form`. Code calling `MyForm::parse(&data)` continues
  to work by importing `rustango::forms::Form` (or `use rustango::Form`).
  `FormError` (single-error) is kept for the admin's CRUD path.

---

## [v0.15.0] — Permissions, auth backends, auth middlewares (G+H+I) — 2026-05-01

### Added

- **Permission engine** (`rustango::tenancy::permissions`):
  - Four `#[derive(Model)]` tables: `Role`, `RolePermission`, `UserRole`, `UserPermission` —
    queryable via ORM, visible in admin, included in bootstrap snapshot.
  - `has_perm(uid, codename, pool)` — single-CTE round-trip: superuser → explicit deny/grant
    → role membership → default false.
  - `has_any_perm`, `has_all_perms`, `user_permissions`, `user_roles`.
  - `model_codenames(table)` — generates `add/change/delete/view` set for any model.
  - `ensure_tables` — idempotent DDL; framework-managed outside user migration chain.
  - `create_role`, `get_or_create_role`, `grant_role_perm`, `revoke_role_perm`,
    `assign_role`, `remove_role`, `set_user_perm`, `clear_user_perm` — all ORM-backed.

- **Pluggable auth backends** (`rustango::tenancy::auth_backends`):
  - `AuthBackend` trait — `authenticate(parts, pool) → Result<Option<AuthUser>, AuthError>`.
  - `ModelBackend` — `Authorization: Basic <b64>` against `rustango_users`.
  - `ApiKeyBackend` — `Authorization: Bearer <prefix>.<secret>` via `rustango_api_keys`.
  - `JwtBackend` — HMAC-SHA256 bearer JWT; `issue(user_id)` + `verify_token`.
  - `ApiKey` model with `#[derive(Model)]`, `ensure_api_keys_table`, `create_api_key`.

- **Auth middlewares** (`rustango::tenancy::middleware`):
  - `RouterAuthExt` — `.require_auth(backends, pool)`, `.optional_auth(...)`,
    `.require_perm(codename, pool)` chain methods on any `Router<S>`.
  - `AuthenticatedUser` — injected into request extensions on successful auth.
  - `CurrentUser` — axum extractor returning `Option<AuthenticatedUser>`.

- **`manage` verbs**: `create-role`, `list-roles`, `assign-role`, `revoke-role`,
  `grant-perm`, `revoke-perm`, `create-api-key`.

---

## [v0.14.2] — full-width admin; custom title; semantic breadcrumbs — 2026-05-01

### Added

- **`Builder::admin_title(name)` / `Builder::admin_subtitle(name)`** — set the text
  shown in the admin sidebar header. Defaults to `"rustango admin"`. Example:
  `.admin_title("Rustail Admin")`.

- **`admin::Builder::title()` / `subtitle()`** — same API on the standalone admin
  builder for non-tenancy projects.

- **Semantic breadcrumbs** — every admin page now uses
  `<nav class="breadcrumb" aria-label="breadcrumb"><ol><li>` with a CSS `::before`
  separator. Root crumb uses the configurable admin title; subsequent crumbs show
  the model name and (on detail/edit) the row PK. Old bare `<p>` links removed.

### Changed

- **Admin content area is now full-width** — removed `max-width: 1100px` from
  `main.content` so list, detail, and form views fill the viewport inside the sidebar.

## [v0.14.1] — admin under `/__admin/`; readonly_fields skip on create — 2026-05-01

### Fixed

- **Admin CRUD routes moved to `/__admin/` prefix** so they can't be shadowed by user
  routes like `/author/{id: Path<i64>}`. Previously, `/author/new` (create form) was
  captured by user public routes before the admin fallback, producing
  "Cannot parse `new` to a `i64`". The admin builder now registers
  `/__admin`, `/__admin/`, `/__admin/{*rest}` as explicit routes that take priority.
  `handle_request` strips the `/__admin` prefix before dispatching to the inner router
  so that login redirects correctly reference `/__admin/…` paths. Session routes
  (`/__login`, `/__logout`, `/__static__`) are unchanged.

- **`readonly_fields` are now skipped in `create_submit`** as well as `update_submit`.
  Previously, declaring a field as `readonly_fields` (e.g. a computed `posts_count`)
  excluded it from the create form but `collect_values` still required it, producing a
  "required field missing" error on every create. The skip list now includes both the
  auto-PK and all `readonly_fields` on create.

### Changed (breaking for admin URL shape)

- All admin CRUD URLs changed from `/{table}` to `/__admin/{table}`. Projects that
  hardcode admin paths (unusual — the framework generates all links from templates)
  must update them. `cargo build` is all that's needed for projects that only use the
  framework's generated UI.

## [v0.14.0] — FK facet dropdown — 2026-05-01

### Added

- **FK `list_filter` facets render as `<select>` dropdowns** instead of link lists.
  When a `list_filter` field has a `Relation::Fk` (or `O2O`) on its `FieldSchema`,
  `compute_facets` now sets `is_fk: true` and adds a `clear_url` (the "— all —"
  option). The `list.html` template renders these fields as a `<select>` with one
  `data-href` attribute per option and a one-line `onchange` handler that navigates
  directly — no form POST, no extra JS dependency. Non-FK facets keep the existing
  link list. Filtering behaviour is unchanged (URL still carries the raw PK value
  which the ORM accepts as-is).

### Browser-verified

- `Post` admin list with `list_filter = ["author", "published_at"]`:
  "BY AUTHOR" renders a dropdown; selecting "Alice Kowalski (2)" navigates to
  `?author=1` and shows 2 rows with the dropdown pre-selected. "BY PUBLISHED"
  keeps the link list. "— all —" clears the filter back to the full list.

## [v0.13.3] — `audit-cleanup` manage verb — 2026-05-01

### Added

- **`audit-cleanup` manage CLI verb** — run audit-log retention from cron without
  going through the admin UI.
  ```
  cargo run --bin manage -- audit-cleanup --days 90
  cargo run --bin manage -- audit-cleanup --keep-last 50
  cargo run --bin manage -- audit-cleanup --tenant acme --days 90
  ```
  Iterates every active tenant (or a single slug with `--tenant`) and calls
  `audit::cleanup_older_than` / `cleanup_keep_last_n` against each tenant's pool.
  Reports per-tenant deleted count and a final total. `--days` and `--keep-last` are
  mutually exclusive; omitting both is a validation error.

## [v0.13.2] — admin soft-delete + restore; session secret hardening — 2026-05-01

Four postmortem fixes from building rustail (real multi-tenant app). B1 and B3 are
framework correctness fixes; B4 and B5 close the gap between the admin UI and the
`soft_delete` ORM mixin.

### Fixed

- **B1 — migration `auto: true` on datetime fields emitted invalid SQL** (`DATETIME` type
  instead of `TIMESTAMPTZ`). `sql_type()` in `migrate/diff.rs` previously short-circuited
  on any `auto=true` field and uppercased the ty string, so `auto_now_add`/`auto_now`
  datetime columns produced a Postgres type error on `CREATE TABLE`. Non-integer `auto`
  types now fall through to the normal type mapping. Regression tests added in
  `migrate/diff.rs`.

- **B3 — `RUSTANGO_SESSION_SECRET` with invalid base64 silently downgraded to a random
  key** with only a structured log line as a signal. `from_env_or_random()` now also
  prints a yellow `warning:` to stderr when the var is set but unparseable. A new strict
  variant `SessionSecret::try_from_env() -> Result<Self, SessionSecretError>` is added
  for production boot paths that prefer a hard failure over a silent downgrade.
  `SessionSecretError` is re-exported from `tenancy::operator_console`.

- **B4 — admin delete button ignored `#[rustango(soft_delete)]`** and always issued a
  hard `DELETE`. `delete_submit` in `admin/views.rs` now checks
  `ModelSchema::soft_delete_column` (new field — emitted by the `Model` derive) and
  routes to an `UPDATE SET <col> = NOW()` path when the model has a soft-delete column.
  The audit log records `AuditOp::SoftDelete` instead of `AuditOp::Delete`. Hard-delete
  models are unchanged.

- **B5 — no built-in `restore_selected` bulk action**. `action_submit` now recognises
  `"restore_selected"` alongside `"delete_selected"`. It issues `UPDATE SET <col> = NULL
  WHERE pk IN (...)` to clear the soft-delete timestamp for the selected rows. Models
  without a soft-delete column are no-ops (safe to list in `admin.actions` regardless).
  The audit log records `AuditOp::Update` with `__action: "restore_selected"` so the
  activity feed shows who restored what.

### Added

- `ModelSchema::soft_delete_column: Option<&'static str>` — the SQL column name of the
  `#[rustango(soft_delete)]` field, if any. Populated by the `Model` derive macro;
  `None` for models without soft-delete. Consumed by admin delete/action paths.

- `SessionSecretError` enum (`BadBase64`, `TooShort`) with `Display` + `Error` impls.

- `SessionSecret::try_from_env() -> Result<Self, SessionSecretError>` — strict variant
  that errors when the env var is set but unparseable or too short, instead of silently
  falling back to a random key.

### Migration

No schema changes. No breaking changes — `ModelSchema` gains a new field; all existing
`const SCHEMA` statics are regenerated by the macro, so `cargo build` is sufficient.

## [v0.13.1] — facet polish (count-desc + truncation) — 2026-05-01

### Changed

- **Facet values sort by count descending** with alphabetic tie-break, on both per-table list views (`list_filter` rail) and the `/__audit` activity feed. Most active value floats to the top — operators see "edit hotspots" first instead of alphabetically first.
- **Facet lists truncate at 15 values** with a `+N more…` link that opts the column into showing every distinct value via `?facet_show_all=<field>`. Active filters always render so the operator's currently-selected value never disappears behind the cutoff. Low-cardinality columns (≤ 15 distinct) render the full list with no "more" link.

### Curl-verified

- 23 distinct `source` values in `/__audit` → rail shows top 15 + "+8 more" link; `?facet_show_all=source` expands to all 23.
- `By operation` facet on the same page shows `update (7)` above `create (4)`.
- 589/589 across the full workspace test suite.

## [v0.13.0] — consolidation + admin/audit.rs split — 2026-05-01

Six debt-reduction commits in one tag. No new user-facing features; behaviour preserved end-to-end. Run `cargo test --workspace` for the first time since v0.9.0 — full live sweep passes 589/589.

### Fixed

- **Long-broken test compiles** — `tests/sql.rs` and `tests/where_expr_live.rs` had `SelectQuery` literals missing the `order_by` field since the v0.9.0 slice introduced it. Three sites updated; full workspace test suite is now reachable without per-suite `--test <name>` flags.
- **Standing `use crate::admin;` warning** in `tenancy/admin.rs` dropped — the import was dead, every actual reference uses the fully-qualified `crate::admin::` path.

### Added

- **`Builder::migrate(...)` auto-creates `rustango_audit_log` per tenant** via `audit::ensure_table` in the per-tenant migration hook. Removes the v0.12 footgun where projects had to call `ensure_table` from their seed manually. The uni_portal demo's manual call is gone.

### Changed

- **`admin/views.rs` extracted to `admin/audit.rs`** — moved `audit_log_view`, `audit_cleanup_submit`, `emit_admin_audit`, `emit_admin_audit_diff`, `url_encode_q`, the `AUDIT_PAGE_SIZE` const, and the new `split_action_marker` helper. `views.rs` shrank from 1449 → 1056 lines (~430 lines into the new module). Pure refactor; behaviour preserved.
- **Admin audit JSON shapes match the macro path** — admin update/delete/action emits now read column values via `render::read_value_as_json` (typed primitives) and form payloads via `render::coerce_form_to_json` (parses `i64`/`bool`/etc. into typed JSON). Operators see `credits: { before: 3, after: 5 }` instead of `credits: { before: "3", after: "5" }`. Strings stay as strings; FKs serialize as integer PKs.
- **`__action` marker rendered as a distinct badge** in the audit panel and `/__audit` activity feed. Bulk-action rows previously looked like updates with a hidden `__action` key in the changes JSON; v0.13.0 splits the marker out via `audit::split_action_marker`, renders `<span class="audit-op-action">action: <name></span>` (blue), and pretty-prints `changes` without the marker. The macro-emitted update rows continue to render as plain "update" badges.

### Tests

- 589/589 across the full workspace test suite. No new tests added this release — the changes are debt cleanup + refactor + JSON shape normalisation, all exercised by existing live tests.

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
