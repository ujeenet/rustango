# Migrations & the migration engine

**Rustango** ships a Django-style migration engine: you edit your models,
run `makemigrations` to generate a versioned JSON file describing the
schema change, and `migrate` to apply it. Since **0.48** the framework
even migrates **its own** `rustango_*` tables through the same engine —
no hand-shipped bootstrap DDL. This page explains the moving parts that
make an upgrade safe: the two migration chains, squash reconciliation,
and the guarded fake-initial that lets a pre-existing database adopt the
engine without collisions.

> **New to migrations?** The day-to-day CLI verbs — `makemigrations`,
> `migrate`, `migrate --squash`, `migrate --fake`, `downgrade`,
> `showmigrations` — are covered command-by-command in the
> [manage guide](manage.md#migrations). This page is the conceptual
> model behind them.

> **Source:** `rustango::migrate` (`runner`, `make`, `file`, `manage`)
> and `rustango::tenancy::migrate` — the runner's reconciliation lives in
> `migrate::runner::reconcile`.

---

## Two migration chains

A migration belongs to one of two independent chains, each with its own
ledger table (the row of applied names the runner consults to skip work):

| Chain | What it manages | Ledger table |
|---|---|---|
| **Project** | your `#[derive(Model)]` tables, in `migrations/` | `__rustango_migrations__` |
| **System** | the framework's own `rustango_*` tables (`Org`, `User`, roles/permissions, agents, media, …), in `system/migrations/` | `__rustango_system_migrations__` |

The **system chain** is what makes framework schema self-describing.
Its files are generated from the compiled framework models — and they're
**`#[cfg(feature = …)]`-aware**: a feature-gated column or table is
compiler-stripped when the feature is off, so enabling a feature makes
`makemigrations` emit an `AddColumn` / `CreateTable` and disabling it
emits a `DropColumn` / `DropTable`. Scaffolded tenant projects ship an
**empty** `system/migrations/`; the first `cargo run -- migrate`
generates and applies it (see [scaffolding](scaffolding.md)).

`migrate` applies the system chain **before** your project's migrations.
In tenancy mode the two scopes deliberately overlap on the shared
framework tables, so the tenant-scope chain is the one that runs; the
registry-only tables (`rustango_orgs`, `rustango_operators`) mean nothing
without tenancy. Non-tenancy apps that use a framework subsystem (e.g.
`media`) get the system chain applied too.

---

## Squash reconciliation — `Migration.replaces`

A **squash** collapses a run of historical migrations into one freshly
generated file that recreates the same end state — handy when a stack of
half-finished migrations is easier to regenerate than to fix. The catch:
the file's `CREATE TABLE`s would collide on any database that already
applied the migrations it collapsed (a colleague's checkout, staging,
CI).

`migrate --squash` solves this by stamping the new file's **`replaces`**
list with the names it collapsed:

```jsonc
{
  "name": "0007_squashed_0001_0006",
  "replaces": ["0001_initial", "0002_add_status", "0003_add_slug", "…"],
  "forward": [ /* recreates the end state */ ]
}
```

With `replaces` set, the runner **reconciles** the squash against the
database's actual state instead of blindly running it. The decision is
automatic and depends entirely on what's already there:

| Database state | What the runner does |
|---|---|
| fresh — no history, no tables | runs the squash for real |
| every replaced migration is in the ledger | records it, tombstones the predecessors, **no DDL** |
| tables exist but the ledger has no history | records it, **no DDL** (Django's cross-ledger `--fake-initial`) |
| only *some* replaced rows / tables present | **refused** — names what's missing, tells you to resolve by hand |

The **partial** case is a hard error on purpose: no automatic choice is
safe there, so the runner stops and reports what it found rather than
guessing. Resolve it with `migrate --fake` (below).

Migrations superseded by an applied squash count as applied, so you can
leave the collapsed files on disk for a release or two — deployments that
never ran them still migrate forward correctly. Ordinary (non-squash)
migrations are unaffected: a plain migration whose table already exists
still fails loudly, because that's a real conflict, not a
known-equivalent history.

---

## The guarded fake-initial reconcile

This is the mechanism that lets an **existing** database adopt the system
chain seamlessly. Before 0.51 the framework built some of its tables via
lazy `ensure_table` raw DDL; those tables exist but aren't recorded in
the `__rustango_system_migrations__` ledger, so a fresh `CREATE TABLE`
from the new system migration would collide (`relation "rustango_media"
already exists`, MySQL 1050, …).

The system chain reconciles this itself. A pending **system** migration is
inspected: the operations that make up *creating its tables* —
`CreateTable`, plus `CreateIndex` / `CreateM2MTable` targeting a table the
same migration creates — are the accepted set. If **every** table it
creates already exists, the migration is **recorded into the ledger
without running any DDL**, and existing data is left untouched. If only
*some* of its tables exist, the chain creates just the missing ones
(`CREATE TABLE IF NOT EXISTS` semantics) and leaves the rest alone.

The guard is deliberately narrow:

- **Scoped to the framework's own system chain.** User migrations use the
  plain runner and never auto-fake — table-existence faking is opt-in to
  the system path only.
- **Anything that isn't table-creation disqualifies faking** — an index
  on a pre-existing table, an alter / drop / data op / callback falls
  through to a real run, so genuine work is never skipped.
- **Existence is asked of the current namespace only** — Postgres
  `current_schema()`, MySQL `DATABASE()`, SQLite `sqlite_master` — not
  through the `search_path`, so in schema-mode multi-tenancy a same-named
  table in `public` can't fool a tenant into skipping its own tables.

A squash's partial state is still refused (see above); only the
framework's system chain does the piecemeal "create the missing ones"
repair.

---

## Repairing drift by hand — `migrate --fake`

When the database is already in the target state but the ledger doesn't
know it (a DB set up out-of-band, a dropped ledger, a partially-succeeded
migration, a refused partial squash), stamp a migration as applied
**without running its SQL**:

```bash
cargo run -- migrate --fake 0004_add_indexes
cargo run -- migrate --fake 0001_rustango_registry_initial --system       # framework's own chain
cargo run -- migrate --fake 0001_rustango_registry_initial --all-tenants  # every active tenant
```

- `--system` stamps the framework's system chain
  (`system/migrations/` → `__rustango_system_migrations__`) instead of
  your project's.
- `--all-tenants` fans the stamp across every active tenant, reporting
  each and continuing past failures — the framework tables live per
  tenant, so repairing them is a per-tenant job. Combine with `--system`
  for the framework's tables across all tenants.

The name is validated against the migration directory first, so a typo
can't land a bogus row; stamping is idempotent, and the flag can be
repeated to repair a stretch of rows in one command.

---

## Upgrading to 0.51.2

> **0.51.0 and 0.51.1 were yanked** — the reconcile they promised never
> actually fired against real 0.46–0.50 databases (0.51.0 moved the media
> tables onto system migrations and collided; 0.51.1's guard demanded a
> migration be *purely* `CreateTable`, which no generated migration is).
> **Upgrade straight to 0.51.2**, which fixes both.

For an existing deployment, the upgrade is a plain deploy — no
reprovision, no manual DDL:

```bash
cargo run -- migrate
```

The guarded fake-initial handles the pre-existing framework tables: the
first `migrate` records the system migration whose tables already exist
into the ledger without touching them, creates only what's genuinely
missing, and leaves your data alone. If a database is in a truly
inconsistent partial state the runner stops and tells you what it found;
resolve it with `migrate --fake` rather than forcing.

---

## See also

- [`manage` guide](manage.md#migrations) — every migration CLI verb, with
  examples.
- [Scaffolding](scaffolding.md) — where `migrations/` and
  `system/migrations/` come from.
- [Models](models.md) — the derive the migrations are generated from.
