# Migrations engine v2: `manage dbmigrate`

Engine v2 projects run every migration command through `manage dbmigrate <verb>`.
A project switches once with `adopt`; after that the engine v1 verbs refuse it.

Engine v2 runs on PostgreSQL, SQLite and MySQL 8.0.16+. It does not support MariaDB: every verb, `adopt`
included, refuses a MariaDB server before it writes anything. A MariaDB project stays on engine v1
(`manage migrate`, `makemigrations`).

## Verbs

| Verb | What it does |
|---|---|
| `make [name]` | Writes the next file from your models. Contract ops (drops, renames, NOT NULL on an existing column) go in a second `phase: contract` file. |
| `apply [<id>]` | Applies pending files, resuming a run that stopped. With an id, stops after it. `--plan` prints the SQL and runs nothing. |
| `down [N]` / `down --to <id>` | Rolls back the last N files, or the descendants of `<id>`. Planned in full before anything runs; `--plan` prints that plan and runs nothing. |
| `status` | Lists applied, pending and partial files. |
| `check` | Checks files, defaults and data steps; online it also warns about long moves and a contract file shipped with its expand file (`MV2-W012`). |
| `check --drift` | Compares the database with the state at its applied files. |
| `sql <id>` | Prints the SQL a file runs on this database's backend (under tenancy, on each backend in use); `--dialect pg\|mysql\|sqlite\|all` picks others. |
| `adopt` | Switches an engine v1 project: one checkpoint per app and target, then the v1 files go. |
| `squash <app> <from>..<to>` | Replaces an app's chain from its root with one checkpoint. |
| `squash --prune <checkpoint>` | Deletes the replaced files once every database (every tenant, under tenancy) ran the checkpoint. |

## Flags

| Flag | Verb | Meaning |
|---|---|---|
| `--app <app>` | `make`, `squash --prune` | Work in that app's folder. |
| `--rename t.old:new` / `--no-rename` | `make` | A drop and an add of one type: keep the data, or drop and add. |
| `--fill t.c=<sql>` | `make` | Value written into existing rows by a new NOT NULL column. A `--fill` for a column that already exists is refused. |
| `--no-split` | `make` | Keep contract ops in the one file. |
| `--merge` | `make` | Write a merge file over two leaves. Branches that do not commute are refused (`MV2-E033`). |
| `--rebase <id> [--force]` | `make` | Move a leaf on top of the other one. Refused once a database ran it or a remote branch has it. |
| `--data-rust <name>` | `make` | Write a `data_rust` op, its step stub and a test. |
| `--check` | `make` | Fail when the models have changes no file has. |
| `--backends pg,mysql,sqlite` | `make`, `check` | The backends the project runs on; every default must be one string on all of them. Default: the connected one. |
| `--plan` / `--dry-run` | `apply`, `down` | Print what would run; write nothing. |
| `--allow-rollback` | `apply` | Allow `apply <earlier id>` to roll back. |
| `--resume-forward` | `apply` | Take a half rolled back file forward again. |
| `--accept-drift <reason>` | `apply` | Record an adopt checkpoint over a database that differs from it. |
| `--accept-step-change` | `apply` | Resume a partial `data_rust` step whose code changed. |
| `--accept-failover <reason>` | `apply` | Resume after a MySQL failover without ending the recorded DDL session. |
| `--accept-data-loss` | `down` | Allow a rollback that re-creates dropped tables or columns empty. |
| `--cascade` | `down --to` | Roll back dependents in other apps too. |
| `--max-duration <secs>` | `apply` | Stop between batches after this long; the next run resumes. |
| `--offline` | `check` | Skip the checks that need a database. |
| `--assume-all-applied` | `squash --prune` | Prune without checking the databases. |
| `--database <alias>` | `apply`, `down`, `status` | Work on that configured database's `database` chain. |
| `--registry`, `--tenant <slug>`, `--all-tenants` | `down` (tenancy) | What to roll back; one is required. Tenants roll back before the registry. |
| `--abandon-fleet-run` | `apply`, `down` (tenancy) | Replace an unfinished fleet run instead of refusing. |

A flag a verb does not know is refused; nothing runs.

SIGTERM or Ctrl-C during `apply` (or an engine v2 boot) finishes the unit in flight, records the migration as
`stopped`, and exits non-zero; the next run resumes. A booting replica waits on the migrate lock as long as the
holder's heartbeat is fresh (refreshed every 30 s) and exits non-zero only when it is older than
`[migrate] boot_stale_after_secs` (120 s); `[server] migrate_lock_timeout_secs` applies to engine v1 only.

## Targets

A file's `target` picks where it runs. `tenant` runs on the single database or on every tenant; `registry` runs once
on the tenancy registry. `every_database` runs once per physical database, before the tenant chain: the single or
registry database and each database-mode tenant database (schema-mode tenants share the registry's). Use it for
extensions a database-mode tenant needs. `database` runs once on the configured database named by `"database"`:
register it with `rustango::databases::register` before migrating; an app's files name one database.

## Under tenancy

Run `dbmigrate` from the tenancy CLI (`Cli::tenancy()`). There `apply`, `down`, `status`, `check --drift` and `sql`
work per database: the registry first, then every tenant (inactive ones included). The engine v1 verbs
(`migrate`, `migrate-tenants`, `migrate-registry`, `downgrade`) refuse a v2 project and name the `dbmigrate` verb.

- `apply` runs the registry chain once, then each tenant's, under one fleet lock. Flags: `--max-failures N`,
  `--fail-fast`, `--parallel K`, `--canary a,b`, `--active-only`, `--phase expand`, `--allow-mixed-heads`,
  `--abandon-fleet-run`, `--plan`. Contract files run only after every tenant reached its expand head.
- `down` needs `--registry`, `--tenant <slug>` or `--all-tenants`. With both tenants and `--registry`, the tenants
  roll back first; the first failure stops it, so the registry is never rolled back past a tenant.
- `down N` counts N in each database's own ledger, read once when the run starts. If it fails part way, run the
  same command again: it stops at the same targets, so tenants already rolled back do not lose another file.
  A different `down N` is refused until that run finishes; `--to <id>` or `--abandon-fleet-run` replaces it, and
  `apply` abandons it.
- Rolling back a `create_table` drops the table with its rows; `--accept-data-loss` only guards re-creating a
  dropped table or column empty.
- `status` and `check --drift` list the registry, then each tenant.
- A failed tenant is listed with its error. Every refusal (another plan's unfinished run, a blocker in any tenant,
  an unknown `--canary`) comes before any write. Canaries run first, in the order given.
- SIGTERM or Ctrl-C during `apply`: each tenant finishes its unit in flight, no other tenant starts, and the
  command exits non-zero. The next run resumes.
- Database-mode tenants are reached through short-lived connections, never the app's tenant pools: about three
  per tenant in flight, so the peak follows `--parallel`.

The registry keeps one row per fleet run in `__rustango_fleet_runs__` (direction, state, heartbeat; `down` runs
also store their targets) and one row per tenant in `__rustango_tenant_migrations__` (state, head, error class).
Query them to see a run's progress; the error text itself is only in the command output and the log.

### Boot under tenancy

Boot runs the registry chain, then the fleet, with `[migrate] fleet_parallel` tenants at once (1 by default; raise
it so a long rollout fits your deploy timeout). Another replica booting meanwhile waits on the fleet lock as long
as the run's heartbeat is fresh (refreshed every 30 s); it exits non-zero only when the heartbeat is older than
`[migrate] boot_stale_after_secs` (120 s). `[server] migrate_lock_timeout_secs` does not apply here. A SIGTERM
during the boot fleet stops it as above, and the server exits without opening `/ready`.

A tenant that failed at boot answers 503 maintenance on every request that opens its database (a handler that
never touches the tenant database still answers) until a run applies it. A server parks only the tenants that
failed at its own boot: one that fails in a later CLI run is not parked on a running server.

### Rights

- The login that runs the fleet on the registry (Postgres) needs `CREATE` on the database, `CREATE` on schema
  `public` (PG 15+) and `CREATEROLE` while schema tenant roles are on; the database owner runs
  `REVOKE TEMPORARY ON DATABASE <db> FROM PUBLIC` once.
- Set `RUSTANGO_TENANT_MIGRATOR=user:password` to migrate database-mode tenants as a separate login; the tenant
  login then cannot write the ledger. On Postgres the migrator owns every object in the tenant database and the
  tenant login keeps rows only, also in tables added later. The tenant login must not own its database: make the
  migrator the owner (`ALTER DATABASE <db> OWNER TO <migrator>`) or run as a superuser, else the tenant is refused
  with that fix. To take over objects the login made, the migrator needs its rights (`GRANT <login> TO <migrator>`).
  On MySQL the migrator needs `SELECT` on the `mysql` schema and `GRANT OPTION`.

## Single database: expand, then contract

`apply` on one database has no `--phase`. Deploy the expand file first with `apply <expand id>`, then run `apply`
again (all files) once the code no longer needs what the contract file drops. `apply <id>` refuses an id no file
has; nothing runs.

## Writing a file by hand

`make` writes schema ops. Write data and raw SQL ops by hand in a new file (the next number, `parents` set to the
current leaf), then run `check`.

```json
{"engine": 2, "format": 1, "id": "0012_fill_slug", "app": "project", "target": "tenant",
 "parents": ["0011_add_slug"],
 "operations": [
  {"op": "data_sql", "v": 1, "id": "fill_slug_v1",
   "tables": {"read": ["post"], "write": ["post.slug"]},
   "up": {"any": "UPDATE {{t:post}} SET {{c:slug}} = {{c:title}} WHERE {{c:slug}} IS NULL"},
   "down": {"noop": "the column is dropped on rollback"},
   "batch": {"table": "post", "key": "id", "size": 1000}},
  {"op": "raw_sql", "v": 1, "id": "post_title_trgm",
   "up": {"postgres": "CREATE INDEX post_title_trgm ON post USING gin (title gin_trgm_ops)"},
   "down": {"postgres": "DROP INDEX post_title_trgm"},
   "state": [{"op": "add_index", "v": 1, "table": "post", "name": "post_title_trgm",
              "columns": ["title"], "method": "gin", "skipped_on": ["mysql", "sqlite"]}]}
 ]}
```

- `data_sql`: `up` and `down` map a backend (`postgres`, `mysql`, `sqlite`) or `any` to one statement. Write
  `{{t:table}}` and `{{c:column}}` for names (quoted per backend, checked against `tables`), and `{{?}}` for a
  value from `binds`. `down` is SQL, `{"noop": "<why>"}` or `{"irreversible": "<why>"}`. `on_fresh: "skip"` leaves
  the step out of a fresh install.
- `batch: {table, key, size}` runs the step in batches of `size` rows by the unique `key`, each batch its own
  transaction; the statement must use `{{batch_range}}` where the key range goes. A killed run resumes mid-move.
- `raw_sql`: one statement per backend; a backend with no entry runs nothing. `state` lists the schema ops the SQL
  performs, so replay and drift know about it; rollback runs `down` and reverses `state`. MySQL DDL, and anything
  with `"tx": "autocommit"`, needs `"probe": "state"` (or a probe SQL) so a cut-off run can resume.
- `skipped_on: [backend]` on an index (or an extension) says that backend does not have it: render, probe and drift
  skip it there. `make` writes it for a partial, `INCLUDE` or non-btree index on the backends that cannot build one.

## Rollback walls

- Dropping a NOT NULL column with no default cannot be rolled back, even with `--accept-data-loss`: the column
  cannot come back empty. Give the drop `"restore": "archive"`: the forward run copies the values into an archive
  table, and rollback adds the column back and refills it.
- `drop_table` and other `drop_column`s roll back only with `--accept-data-loss` (empty), or with a data op that
  declares `restores` and puts the values back.
- Rollback probes the rows before it runs anything: NULLs where NOT NULL comes back, duplicates for a unique index
  (a partial one too), orphans for an FK, values too long or out of range for a narrower type, and broken checks.
  Every blocker is printed, with the query that finds the rows.

## Squash

- `squash <app> <from>..<to>` starts at the app's root (the adopt checkpoint or the first file); another `from` is
  refused (`MV2-E015`). To squash further later, squash again from the checkpoint: `squash <app> 0023_squash..0030_x`.
  The new checkpoint replaces the files the old one did.
- The checkpoint is named after `<to>`'s number: `0023_squash.json`.
- A database that ran every replaced file records the checkpoint with no SQL; rollback and `check --drift` treat it
  as sitting where the last replaced file ran, so `down` and `down --to <checkpoint>` work as on a fresh install.
  While the replaced files are on disk, rollback below the checkpoint uses them; after `--prune` it stops there.
- A database that ran only part of the replaced files is refused: migrate it with the last release of your app from
  before the squash (it runs them one by one), then deploy this one.
- `irreversible_below` is set when a replaced op could not be rolled back (or an older checkpoint was replaced):
  rollback stops at the checkpoint.

## Many-to-many fields

A many-to-many junction table is a plain table in v2 files: `make` writes `create_table` (two NOT NULL columns typed
as each end's primary key, both the primary key, a `CASCADE` FK each) and `drop_table`.

## Portable defaults

A column's `default` is SQL text, written as is on every backend. For a value each backend spells its own way,
write `portable_default` instead: `now`, `true`, `false`, `empty_object` or `empty_array`. `make` writes it when a
model default maps to one.

## Framework tables

The framework's system chain owns every `rustango_*` table. A project file may declare one with `extern_table`
and write its rows, but a file that creates, alters, drops or renames one is refused (`MV2-E034`).
`adopt_table` takes over an existing table: it verifies the full definition and runs no DDL, or creates the table on a fresh database.
`adopt_column` takes over one column the same way, and its `indexes` list takes over indexes on it: each is verified if present and created if missing.
The system apps run before your project, on their own ledger `__rustango_system_v2`.

## Switching from engine v1

1. Bring every database to the engine v1 head with `manage migrate` on 0.71.0. It also repairs framework FKs that
   older releases created without their `ON DELETE CASCADE`.
2. Run `manage dbmigrate adopt` once and commit the result.
3. Run `manage dbmigrate apply` on each database (under tenancy, once: it covers the registry and every tenant).

The switch is one way: v2 rollback stops at the adopt checkpoint. A database behind the v1 head is refused before
anything is written; migrate it with `manage migrate` from the commit before `adopt`, then apply again. After
`adopt`, the `system/migrations` folder and the v1 ledger table stay as they were; engine v2 does not read them.
