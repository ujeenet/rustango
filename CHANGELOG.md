# Changelog

All notable changes to rustango. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project loosely follows [SemVer](https://semver.org/) — with the caveat that nothing pre-1.0 has a stability guarantee.

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
