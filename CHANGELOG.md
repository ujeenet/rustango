# Changelog

All notable changes to rustango. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project loosely follows [SemVer](https://semver.org/) — with the caveat that nothing pre-1.0 has a stability guarantee.

## [Unreleased] — v0.5

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

[Unreleased]: https://github.com/ujeenet/rustango/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ujeenet/rustango/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ujeenet/rustango/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ujeenet/rustango/releases/tag/v0.1.0
