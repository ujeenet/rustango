# Migrations engine v2: `manage dbmigrate`

Engine v2 projects run every migration command through `manage dbmigrate <verb>`.
A project switches once with `adopt`; after that the engine v1 verbs refuse it.

## Verbs

| Verb | What it does |
|---|---|
| `make [name]` | Writes the next file from your models. Contract ops (drops, renames, NOT NULL on an existing column) go in a second `phase: contract` file. |
| `apply [<id>]` | Applies pending files, resuming a run that stopped. With an id, stops after it. |
| `down [N]` / `down --to <id>` | Rolls back the last N files, or the descendants of `<id>`. Planned in full before anything runs. |
| `status` | Lists applied, pending and partial files. |
| `check` | Checks files, defaults and data steps; online it also warns about long moves and a contract file shipped with its expand file (`MV2-W012`). |
| `check --drift` | Compares the database with the state at its applied files. |
| `sql <id>` | Prints the SQL a file runs, per backend. |
| `adopt` | Switches an engine v1 project: one checkpoint per app and target, then the v1 files go. |
| `squash <app> <from>..<to>` | Replaces an app's chain from its root with one checkpoint. |
| `squash --prune <checkpoint>` | Deletes the replaced files once every database (every tenant, under tenancy) ran the checkpoint. |

## Flags

| Flag | Verb | Meaning |
|---|---|---|
| `--app <app>` | `make`, `squash --prune` | Work in that app's folder. |
| `--rename t.old:new` / `--no-rename` | `make` | A drop and an add of one type: keep the data, or drop and add. |
| `--fill t.c=<sql>` | `make` | Value written into existing rows by a new NOT NULL column. |
| `--no-split` | `make` | Keep contract ops in the one file. |
| `--merge` | `make` | Write a merge file over two leaves. Branches that do not commute are refused (`MV2-E033`). |
| `--rebase <id> [--force]` | `make` | Move a leaf on top of the other one. Refused once a database ran it or a remote branch has it. |
| `--data-rust <name>` | `make` | Write a `data_rust` op, its step stub and a test. |
| `--check` | `make` | Fail when the models have changes no file has. |
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

## Under tenancy

`migrate-tenants` runs the registry chain once, then every tenant (inactive ones included) under one fleet lock.
Flags: `--max-failures N`, `--fail-fast`, `--parallel K`, `--canary a,b`, `--active-only`, `--phase expand`, `--allow-mixed-heads`, `--abandon-fleet-run`.
Contract files run only after every tenant reached its expand head.
Set `RUSTANGO_TENANT_MIGRATOR=user:password` to migrate database-mode tenants as a separate login; the tenant login then cannot write the ledger.

## Framework tables

The framework's system chain owns every `rustango_*` table. A project file may declare one with `extern_table`
and write its rows, but a file that creates, alters, drops or renames one is refused (`MV2-E034`).
`adopt_table` takes over an existing table: it verifies the full definition and runs no DDL, or creates the table on a fresh database.
