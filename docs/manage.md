# `manage` CLI reference

Every command is invoked via `cargo run --bin manage -- <subcommand>` from inside a rustango project. (To bootstrap a new project, install `cargo-rustango` and run `cargo rustango new <name>` — see the README.)

All commands write user-facing output to stdout and exit non-zero on validation/IO errors. Use `--help` on any command for inline usage.

---

## Table of contents

- [Migrations](#migrations)
- [Data migrations](#data-migrations)
- [Project / app scaffolders](#project--app-scaffolders)
- [File generators (`make:*`)](#file-generators-make)
- [System commands](#system-commands)
- [Tenancy commands](#tenancy-commands)

---

## Migrations

### `manage makemigrations [name]`

Diff the inventory of registered models against the latest snapshot in `migrations/`. Writes a new JSON file with the detected changes.

```bash
manage makemigrations                                  # auto-name (e.g. 0004_add_slug_to_posts)
manage makemigrations rename_status_to_state           # custom suffix
```

**Auto-detected changes:**
- `CreateTable` / `DropTable`
- `AddColumn` / `DropColumn`
- `AlterColumnType` / `AlterColumnNullable` / `AlterColumnDefault` / `AlterColumnMaxLength`
- `AlterColumnUnique`
- `CreateIndex` / `DropIndex`
- `AddCheckConstraint` / `DropCheckConstraint`
- `CreateM2MTable` / `DropM2MTable`

**NOT auto-detected** (rename vs drop+add is ambiguous):
- `RenameTable`, `RenameColumn` — use `--empty` and edit the JSON.

### `manage makemigrations --app <app>`

Per-app migration directory at `<project_root>/<app>/migrations/`. Filters models by their resolved app label.

```bash
manage makemigrations --app blog
manage makemigrations --app blog backfill_slugs
```

### `manage makemigrations --empty <name>`

Write an empty migration scaffold (no `forward` ops). Edit the JSON to add hand-authored data ops or rename ops.

```bash
manage makemigrations --empty rename_status_to_state
# Then edit migrations/0005_rename_status_to_state.json:
#   "forward": [
#     {"schema": {"RenameColumn": {"table": "posts", "old_column": "status", "new_column": "state"}}}
#   ]
```

### `manage migrate`

Apply every pending migration in lex order.

```bash
manage migrate
manage migrate --dry-run                  # print SQL without writing
```

Each file is wrapped in a transaction by default (set `"atomic": false` in the JSON to opt out — needed for `CREATE INDEX CONCURRENTLY` etc.).

### `manage migrate <target>`

Move forward OR back to a specific migration name.

```bash
manage migrate 0003_add_slug              # forward to 0003 (apply 0001..0003)
manage migrate 0001_initial               # roll back to 0001 (unapply 0002+)
manage migrate zero                        # unapply EVERY migration
```

### `manage downgrade [N]`

Step back N applied migrations (default 1). Each step requires the migration to be invertible (i.e. data ops must have `reverse_sql`, schema ops are auto-invertible).

```bash
manage downgrade                          # one step
manage downgrade 3                         # three steps
```

### `manage showmigrations` / `manage status`

Print the migration list with `[X]` (applied) / `[ ]` (pending) markers.

```bash
manage showmigrations
manage status                              # alias
```

Output:

```
[X] 0001_initial
[X] 0002_add_status
[ ] 0003_add_slug
```

---

## Data migrations

### `manage add-data-op`

Add a SQL data-transformation op without hand-editing JSON.

```bash
# New migration with up + down
manage add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Append to an existing migration
manage add-data-op \
    --to 0003_add_slug \
    --sql "UPDATE posts SET slug = id::text"

# Irreversible (no rollback)
manage add-data-op \
    --sql "DELETE FROM legacy_data" \
    --name purge_legacy
```

| Flag | Required | Description |
|---|:-:|---|
| `--sql <SQL>` | yes | Forward SQL to run on `migrate` |
| `--reverse-sql <SQL>` | no | Rollback SQL on `unapply`; omit for irreversible |
| `--name <name>` | no | New-migration name suffix; defaults to `data_op` |
| `--to <migration>` | no | Append to an existing migration instead of creating one |

When omitted, `--reverse-sql` makes the op `reversible: false` and rollback fails fast.

---

## Project / app scaffolders

### `cargo rustango new <name>` *(separate binary)*

Bootstrap a new rustango project. Requires `cargo install cargo-rustango`. Three templates:

```bash
cargo rustango new myblog                          # default = fullstack (ORM + admin)
cargo rustango new myapi --template api            # JSON-only, no admin
cargo rustango new shop --template tenant          # multi-tenancy
```

Writes:

```
<name>/
  Cargo.toml
  .env.example
  .gitignore
  rust-toolchain.toml
  docker-compose.yml
  README.md
  migrations/
  src/{main,models,views,urls,bin/manage}.rs
```

### `manage startapp <name> [--with-manage-bin]`

Scaffold an app module under `src/<name>/`.

```bash
manage startapp blog
manage startapp shop --with-manage-bin             # also writes src/bin/manage.rs (for new projects)
```

Creates:

```
src/<name>/
  mod.rs
  models.rs
  views.rs
  urls.rs
```

Idempotent — existing files are skipped. After running, manually add `pub mod <name>;` to `src/lib.rs`.

---

## File generators (`make:*`)

All generators write to `src/<snake_name>.rs` (or `tests/<snake_name>.rs` for `make:test`). They:

- Validate the name (PascalCase, alphanumeric + underscore).
- Snake-case it for the filename (`PostViewSet` → `post_view_set.rs`).
- Refuse to overwrite existing files.
- Print a "now add `pub mod X;` to your lib.rs" hint.

### `manage make:viewset <Name> [--model <Model>]`

Scaffold a `#[derive(ViewSet)]` struct with placeholder field lists.

```bash
manage make:viewset PostViewSet --model Post
```

Generated `src/post_view_set.rs`:

```rust
#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, ", filter_fields = "", search_fields = "", page_size = 20)]
pub struct PostViewSet;
```

Mount with: `.merge(PostViewSet::router("/api/posts", pool.clone()))`.

### `manage make:serializer <Name> [--model <Model>]`

Scaffold a `#[derive(Serializer)]` struct.

```bash
manage make:serializer PostSerializer --model Post
```

### `manage make:form <Name>`

Scaffold a `#[derive(Form)]` struct.

```bash
manage make:form ContactForm
```

### `manage make:job <Name>`

Scaffold a background-job struct skeleton + scheduler-wiring example comment.

```bash
manage make:job EmailDigestJob
```

### `manage make:notification <Name>`

Scaffold a notification struct that builds an Email.

```bash
manage make:notification WelcomeEmail
```

### `manage make:middleware <Name>`

Scaffold an axum middleware function with pre/post hooks.

```bash
manage make:middleware AuditLog
```

### `manage make:test <Name>`

Scaffold an integration test in `tests/` using `TestClient`.

```bash
manage make:test post_smoke
```

---

## System commands

### `manage version` / `manage --version`

Print the framework version.

```bash
$ manage version
rustango 0.20.32
```

### `manage about`

Env summary — version, registered models/apps, DB connectivity, env-var status. Useful for support tickets and triage.

```bash
$ manage about
rustango
  version:        0.20.32
  models:         3 registered
  apps:           1 (blog)
  RUSTANGO_ENV:   local
  DATABASE_URL:   postgres://***@localhost:5433/myblog
  db_connect:     ok
```

### `manage check [--deploy]`

Run system audits.

**Always-on checks:**
- ≥ 1 model registered via `inventory`
- DB reachable (`SELECT 1`)
- Migration count vs model count

**With `--deploy`:**
- `RUSTANGO_ENV` is `prod` or `production`
- `SECRET_KEY` set and ≥ 32 bytes
- `DATABASE_URL` set

```bash
$ manage check --deploy
running rustango system check (deploy mode)...
  [info]    3 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    SECRET_KEY length OK
all checks passed
```

Returns non-zero exit code if any error-level check fails. Warnings don't trigger failure.

### `manage docs`

Open <https://docs.rs/rustango> in your default browser. Prints the URL regardless (so it works in headless environments).

```bash
manage docs
```

### `manage --help` / `manage help`

Print the full subcommand list with one-line descriptions.

---

## Tenancy commands

> Available only when the project is built with `features = ["tenancy"]`.

### `manage create-tenant <slug> [options]`

Provision a new tenant. Idempotent.

```bash
manage create-tenant acme --display-name "ACME Corp"
manage create-tenant beta --mode database --db-url postgres://...
```

| Flag | Description |
|---|---|
| `--display-name <name>` | Human-readable label shown in admin sidebars |
| `--mode schema \| database` | Storage mode (default: schema) |
| `--db-url <url>` | Tenant-specific DB URL (database mode only) |

### `manage create-operator <username> --password <pwd>`

Create a global operator (admin user with cross-tenant access).

```bash
manage create-operator admin --password letmein
```

### `manage create-user <tenant> <username> --password <pwd> [--superuser]`

Create a tenant-scoped user.

```bash
manage create-user acme alice --password hunter2 --superuser
```

### `manage list-tenants`

Print every registered tenant with its mode + status.

```bash
manage list-tenants
```

### `manage audit-cleanup`

Trim the audit log (`rustango_audit_log`). Either time-based or count-based, optionally per-tenant.

```bash
manage audit-cleanup --days 90                              # delete > 90 days old
manage audit-cleanup --keep-last 50                         # keep most recent 50 per row
manage audit-cleanup --keep-last 50 --tenant acme           # scoped
```

---

## Custom subcommands

You can extend `manage` by intercepting argv before forwarding to `manage::run`. For example, in `src/bin/manage.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let pool = rustango::sql::sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("import-csv") => my_csv_importer::run(&pool, &argv[1..]).await?,
        _ => rustango::manage::run(&pool, "./migrations".as_ref(), argv).await?,
    }
    Ok(())
}
```

This lets you ship project-specific subcommands alongside the framework's.

---

## Common workflows

### First-time project setup

```bash
cargo rustango new myapp
cd myapp
cp .env.example .env             # edit DATABASE_URL
docker compose up -d
cargo run --bin manage -- migrate
cargo run                        # serve at :8080
```

### Add a model

```bash
cargo run --bin manage -- startapp blog        # if not done yet
# Edit src/blog/models.rs — add #[derive(Model)]
# Add `pub mod blog;` to src/lib.rs
cargo run --bin manage -- makemigrations
cargo run --bin manage -- migrate
```

### Add a JSON API for that model

```bash
cargo run --bin manage -- make:viewset PostViewSet --model Post
# Edit src/post_view_set.rs — fill in field lists
# Mount in src/urls.rs
cargo run                        # GET /api/posts now works
```

### Add a data backfill

```bash
cargo run --bin manage -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title) WHERE slug IS NULL" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs
cargo run --bin manage -- migrate
```

### Pre-deploy audit

```bash
cargo run --release --bin manage -- check --deploy
```

### Roll back the last migration

```bash
cargo run --bin manage -- downgrade 1
```
