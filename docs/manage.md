# `manage` CLI reference

In a rustango project scaffolded via `cargo rustango new`, the unified
runner from v0.16 dispatches every verb from a single binary:

```bash
cargo run                          # runserver (no args = boot the HTTP server)
cargo run -- migrate               # any other verb
cargo run -- --help                # full subcommand list
```

The dispatcher lives in [`rustango::manage::Cli`](https://docs.rs/rustango/latest/rustango/manage/struct.Cli.html);
your `src/main.rs` reads:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

In tenancy projects it gets `.tenancy()` chained in, which switches the
dispatcher to [`rustango::tenancy::manage`](https://docs.rs/rustango/latest/rustango/tenancy/manage/index.html)
and unlocks the multi-tenant verbs.

> **Older shape** — projects scaffolded by `manage startapp --with-manage-bin`
> (or pre-v0.16 ones) still ship `src/bin/manage.rs`. Those use
> `cargo run --bin manage -- <verb>`. Both forms accept the same verbs.

All commands write user-facing output to stdout and exit non-zero on
validation/IO errors. `cargo run -- --help` (or `<verb> --help`) prints
inline usage.

---

## Table of contents

- [Migrations](#migrations)
- [Data migrations](#data-migrations)
- [Project / app scaffolders](#project--app-scaffolders)
- [File generators (`make:*`)](#file-generators-make)
- [Database utilities](#database-utilities)
- [System commands](#system-commands)
- [Tenancy commands](#tenancy-commands)
- [Custom subcommands](#custom-subcommands)
- [Common workflows](#common-workflows)

---

## Migrations

### `makemigrations [name]`

Diff the inventory of registered models against the latest snapshot in
`migrations/`. Writes a new JSON file with the detected changes.

```bash
cargo run -- makemigrations                          # auto-name (e.g. 0004_add_slug_to_posts)
cargo run -- makemigrations rename_status_to_state   # custom suffix
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

### `makemigrations --app <app>`

Per-app migration directory at `<project_root>/<app>/migrations/`.
Filters models by their resolved app label.

```bash
cargo run -- makemigrations --app blog
cargo run -- makemigrations --app blog backfill_slugs
```

### `makemigrations --scope <registry|tenant>`

Tenancy-only: emit a single migration tagged with the matching
`MigrationScope`, diff'd against only the models whose
`#[rustango(scope = "...")]` matches. Without the flag, a flagless
`makemigrations` in a tenancy project (any registered model with
`scope = "registry"`) automatically splits the diff into TWO files —
one for registry-scoped models, one for tenant-scoped — so framework
tables (`Org`, `Operator`) don't bleed across scopes when
`migrate-tenants` fans out.

```bash
cargo run -- makemigrations                       # tenancy: writes 0NN_<auto>.json (registry) + 0MM_<auto>.json (tenant) as needed
cargo run -- makemigrations --scope tenant        # explicit single-scope diff
cargo run -- makemigrations --scope registry      # explicit single-scope diff
```

The split is a real bug-fix: pre-v0.24.2, a flagless `makemigrations`
on a tenancy project would emit one tenant-scoped migration containing
ops on `rustango_operators` (a registry table). When `migrate-tenants`
applied that file, `rustango_operators` would resolve via `search_path`
to the registry copy and conflict with the constraint already there.

### `makemigrations --empty <name>`

Write an empty migration scaffold (no `forward` ops). Edit the JSON to
add hand-authored data ops or rename ops.

```bash
cargo run -- makemigrations --empty rename_status_to_state
# Then edit migrations/0005_rename_status_to_state.json:
#   "forward": [
#     {"schema": {"RenameColumn": {"table": "posts", "old_column": "status", "new_column": "state"}}}
#   ]
```

### `migrate`

Apply every pending migration in lex order.

```bash
cargo run -- migrate
cargo run -- migrate --dry-run                       # print SQL without writing
```

Each file is wrapped in a transaction by default (set `"atomic": false`
in the JSON to opt out — needed for `CREATE INDEX CONCURRENTLY` etc.).

In **tenancy mode** (`Cli::tenancy()`), `migrate` is scope-aware: it
runs registry-scoped migrations against the registry pool first, then
fans tenant-scoped ones across every active org. Use
[`migrate-registry`](#migrate-registry) / [`migrate-tenants`](#migrate-tenants)
for fine-grained control.

### `migrate <target>`

Move forward OR back to a specific migration name.

```bash
cargo run -- migrate 0003_add_slug      # forward to 0003
cargo run -- migrate 0001_initial       # roll back to 0001 (unapply 0002+)
cargo run -- migrate zero               # unapply EVERY migration
```

### `downgrade [N]`

Step back N applied migrations (default 1). Each step requires the
migration to be invertible (data ops need `reverse_sql`; schema ops are
auto-invertible).

```bash
cargo run -- downgrade                  # one step
cargo run -- downgrade 3                # three steps
```

### `showmigrations` / `status`

Print the migration list with `[X]` (applied) / `[ ]` (pending) markers.

```bash
cargo run -- showmigrations
cargo run -- status                     # alias
```

Output:

```
[X] 0001_initial
[X] 0002_add_status
[ ] 0003_add_slug
```

---

## Data migrations

### `add-data-op`

Add a SQL data-transformation op without hand-editing JSON.

```bash
# New migration with up + down
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title)" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs

# Append to an existing migration
cargo run -- add-data-op \
    --to 0003_add_slug \
    --sql "UPDATE posts SET slug = id::text"

# Irreversible (no rollback)
cargo run -- add-data-op \
    --sql "DELETE FROM legacy_data" \
    --name purge_legacy
```

| Flag | Required | Description |
|---|:-:|---|
| `--sql <SQL>` | yes | Forward SQL to run on `migrate` |
| `--reverse-sql <SQL>` | no | Rollback SQL on `unapply`; omit for irreversible |
| `--name <name>` | no | New-migration name suffix; defaults to `data_op` |
| `--to <migration>` | no | Append to an existing migration instead of creating one |

When omitted, `--reverse-sql` makes the op `reversible: false` and
rollback fails fast.

---

## Project / app scaffolders

### `cargo rustango new <name>` *(separate binary)*

Bootstrap a new rustango project. Requires `cargo install cargo-rustango`.
Three templates:

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
  migrations/                               (tenant template only — bootstrap JSONs)
  src/{main,models,views,urls}.rs
```

The tenant template drops
`migrations/0001_rustango_registry_initial.json` and
`0001_rustango_tenant_initial.json` directly — see
[`init-tenancy`](#init-tenancy) for what they contain and when to
re-emit them.

### `startapp <name> [flags]`

Scaffold an app module under `src/<name>/`.

```bash
cargo run -- startapp blog
cargo run -- startapp shop --with-manage-bin             # also writes src/bin/manage.rs
cargo run -- startapp shop --with-bootstrap-migration    # tenancy: also seed bootstrap migrations
cargo run -- startapp shop --into apps                   # write under src/apps/shop/ instead
```

Creates:

```
src/<name>/
  mod.rs
  models.rs
  views.rs
  urls.rs
  migrations/                               (when --with-bootstrap-migration on tenancy)
```

Idempotent — existing files are skipped. After running, manually add
`pub mod <name>;` to `src/lib.rs`.

`--with-bootstrap-migration` is tenancy-only and runs
[`init-tenancy`](#init-tenancy) against the new app's `migrations/`
directory, dropping the framework's registry+tenant bootstrap JSONs
there. Skip it if you already have bootstrap files at the project root.

---

## File generators (`make:*`)

All generators write to `src/<snake_name>.rs` (or `tests/<snake_name>.rs`
for `make:test`). They:

- Validate the name (PascalCase, alphanumeric + underscore).
- Snake-case it for the filename (`PostViewSet` → `post_view_set.rs`).
- Refuse to overwrite existing files.
- Print a "now add `pub mod X;` to your lib.rs" hint.

### `make:viewset <Name> [--model <Model>]`

Scaffold a `#[derive(ViewSet)]` struct with placeholder field lists.

```bash
cargo run -- make:viewset PostViewSet --model Post
```

Generated `src/post_view_set.rs`:

```rust
#[derive(ViewSet)]
#[viewset(model = Post, fields = "id, ", filter_fields = "", search_fields = "", page_size = 20)]
pub struct PostViewSet;
```

Mount with: `.merge(PostViewSet::router("/api/posts", pool.clone()))`.

### `make:serializer <Name> [--model <Model>]`

Scaffold a `#[derive(Serializer)]` struct.

```bash
cargo run -- make:serializer PostSerializer --model Post
```

### `make:form <Name>`

Scaffold a `#[derive(Form)]` struct.

```bash
cargo run -- make:form ContactForm
```

### `make:job <Name>`

Scaffold a background-job struct skeleton + scheduler-wiring example
comment.

```bash
cargo run -- make:job EmailDigestJob
```

### `make:notification <Name>`

Scaffold a notification struct that builds an Email.

```bash
cargo run -- make:notification WelcomeEmail
```

### `make:middleware <Name>`

Scaffold an axum middleware function with pre/post hooks.

```bash
cargo run -- make:middleware AuditLog
```

### `make:test <Name>`

Scaffold an integration test in `tests/` using `TestClient`.

```bash
cargo run -- make:test post_smoke
```

---

## Database utilities

### `db:info`

Print connection metadata read from `DATABASE_URL` (host, database
name, user, version reported by the server).

```bash
cargo run -- db:info
```

### `db:dump [--output <path>]`

Run `pg_dump` against `DATABASE_URL` and write the result to disk.
Requires `pg_dump` on `PATH`.

```bash
cargo run -- db:dump                                 # writes ./db_<timestamp>.sql
cargo run -- db:dump --output backups/before-migrate.sql
```

### `db:restore <path>`

Pipe a SQL dump into `psql` against `DATABASE_URL`. Requires `psql` on
`PATH`.

```bash
cargo run -- db:restore backups/before-migrate.sql
```

---

## System commands

### `version` / `--version`

Print the framework version.

```bash
$ cargo run -- version
rustango 0.23.1
```

### `about`

Env summary — version, registered models/apps, DB connectivity, env-var
status. Useful for support tickets and triage.

```bash
$ cargo run -- about
rustango
  version:        0.23.1
  models:         3 registered
  apps:           1 (blog)
  RUSTANGO_ENV:   local
  DATABASE_URL:   postgres://***@localhost:5433/myblog
  db_connect:     ok
```

### `check [--deploy]`

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
$ cargo run -- check --deploy
running rustango system check (deploy mode)...
  [info]    3 models registered via inventory
  [info]    database reachable
  [info]    4 migration(s) on disk
  [info]    SECRET_KEY length OK
all checks passed
```

Returns non-zero exit code if any error-level check fails. Warnings
don't trigger failure.

### `docs`

Open <https://docs.rs/rustango> in your default browser. Prints the URL
regardless (so it works in headless environments).

```bash
cargo run -- docs
```

### `--help` / `help`

Print the full subcommand list with one-line descriptions. In tenancy
mode the help adds the multi-tenant verbs below.

---

## Tenancy commands

> Available only when the project is built with `features = ["tenancy"]`
> AND `Cli::new()` is chained with `.tenancy()`.

### `init-tenancy`

Materialize the framework's registry + tenant bootstrap migrations into
the migrations directory. Writes
`0001_rustango_registry_initial.json` (creates `rustango_orgs`,
`rustango_operators`) and `0001_rustango_tenant_initial.json` (creates
`rustango_users`).

```bash
cargo run -- init-tenancy
```

**Idempotent**: existing files at those paths are left untouched. The
verb is most often invoked indirectly:

- `cargo rustango new --template tenant` writes the same JSONs from a
  static template, so a freshly scaffolded project never needs
  `init-tenancy`.
- `startapp --with-bootstrap-migration` runs it against a per-app
  migrations directory.
- `Builder::migrate(project_root)` runs it implicitly before applying
  pending migrations.

If you've chained `.user_model::<AppUser>()` on `Cli`, this verb writes
the bootstrap JSON using `AppUser`'s schema instead of the framework's
`User` (so any extra columns land in the `CREATE TABLE`). See
[Custom user model](#custom-user-model-extra-columns-on-rustango_users)
below.

### `migrate-registry`

Apply registry-scoped migrations against the registry pool only. The
registry holds `rustango_orgs` + `rustango_operators` and any
project-defined registry-scoped migrations.

```bash
cargo run -- migrate-registry
```

### `migrate-tenants`

Apply tenant-scoped migrations across every active tenant. Each tenant
gets its own pool (schema or database mode); failures on one tenant
don't halt the rest of the batch — the report lists per-tenant
outcomes.

```bash
cargo run -- migrate-tenants
```

`migrate` (without scope) runs registry-scoped first, then tenant-scoped
— the common case.

### `runserver` / `run-server`

Boot the multi-tenant HTTP server. Equivalent to bare `cargo run` in a
tenancy project; explicit form exists so it can be invoked from
project-specific binaries that intercept argv.

```bash
cargo run                        # implicit
cargo run -- runserver           # explicit
```

### `create-tenant <slug> [options]`

Provision a new tenant. Idempotent.

```bash
cargo run -- create-tenant acme --display-name "ACME Corp"
cargo run -- create-tenant beta --mode database --db-url postgres://...
```

| Flag | Description |
|---|---|
| `--display-name <name>` | Human-readable label shown in admin sidebars |
| `--mode schema \| database` | Storage mode (default: schema) |
| `--db-url <url>` | Tenant-specific DB URL (database mode only) |
| `--host-pattern <pattern>` | Override the host pattern used by `SubdomainResolver` |
| `--no-migrate` | Skip applying tenant-scoped migrations after provisioning |

### `drop-tenant <slug>`

Mark a tenant inactive (`active = false`). Reversible — the schema /
database stays intact and can be reactivated by re-running
`create-tenant`.

```bash
cargo run -- drop-tenant acme
```

### `purge-tenant <slug>`

**Destructive.** Drop the tenant's schema (or database) and remove its
row from `rustango_orgs`. No undo.

```bash
cargo run -- purge-tenant acme
```

### `list-tenants`

Print every registered tenant with its mode + status.

```bash
cargo run -- list-tenants
```

### `create-operator <username> --password <pwd>`

Create a global operator (registry-side admin with cross-tenant
console access).

```bash
cargo run -- create-operator admin --password letmein
```

### `create-user <tenant> <username> --password <pwd> [--superuser]`

Create a tenant-scoped user.

```bash
cargo run -- create-user acme alice --password hunter2 --superuser
```

`--superuser` flips `is_superuser = true` inside the tenant — that
elevates them to org-admin within the tenant (write access in the
tenant admin), but never grants the operator console.

### `create-role <tenant> <name>`

Create a tenant-scoped role.

```bash
cargo run -- create-role acme editor
```

### `list-roles <tenant>`

Print roles defined in the given tenant.

```bash
cargo run -- list-roles acme
```

### `assign-role <tenant> <username> <role>`

Grant a role to a user.

```bash
cargo run -- assign-role acme alice editor
```

### `revoke-role <tenant> <username> <role>`

Revoke a previously-assigned role.

```bash
cargo run -- revoke-role acme alice editor
```

### `grant-perm <tenant> <role> <codename>`

Grant a permission codename to a role. Codenames follow Django's
`<app>.<action>_<model>` shape (`blog.add_post`, `blog.change_post`,
…); `auto_create_permissions` seeds the four standard CRUD codenames
for any model carrying `#[rustango(permissions)]`.

```bash
cargo run -- grant-perm acme editor blog.change_post
```

### `revoke-perm <tenant> <role> <codename>`

Revoke a previously-granted permission.

```bash
cargo run -- revoke-perm acme editor blog.change_post
```

### `create-api-key <tenant> <username> [--label <s>]`

Issue an API key for a tenant user. The full token is printed **once**
on stdout — store it now; only the prefix + hash are persisted.

```bash
cargo run -- create-api-key acme alice --label "ci-bot"
```

### `audit-cleanup`

Trim the audit log (`rustango_audit_log`). Either time-based or
count-based, optionally per-tenant.

```bash
cargo run -- audit-cleanup --days 90                       # delete > 90 days old
cargo run -- audit-cleanup --keep-last 50                  # keep most recent 50 per row
cargo run -- audit-cleanup --keep-last 50 --tenant acme    # scoped
```

---

## Custom user model (extra columns on `rustango_users`)

The framework's tenant `User` is fixed at seven columns: `id`,
`username`, `password_hash`, `is_superuser`, `active`, `created_at`,
plus a `data: serde_json::Value` JSONB bag for ad-hoc per-user
metadata. **For most apps the JSONB column is the right answer** —
no schema migration needed, no override, no rough edges.

When you want **typed, indexable** extras on `rustango_users`, you
have two practical options. They are not interchangeable; pick the
one that matches where you are in the project lifecycle.

### Option 1 — Sibling profile model with FK *(works on any project)*

Recommended when the project already exists or when you want the
framework's `User` to remain the schema authority.

```rust
#[derive(rustango::Model)]
pub struct UserProfile {
    #[rustango(primary_key)] pub id: rustango::sql::Auto<i64>,
    #[rustango(fk = "rustango_users")] pub user_id: i64,
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
```

Run `cargo run -- makemigrations` / `cargo run -- migrate` and you have
a typed extras table joined by FK. Read with the ORM:

```rust
let profile = UserProfile::objects()
    .where_(UserProfile::user_id.eq(user.id.get().copied().unwrap()))
    .fetch_one(&pool).await?;
```

Tradeoff: one extra row + JOIN per access. No risk of breaking
framework auth.

### Option 2 — `Cli::user_model::<AppUser>()` *(greenfield only)*

Use when you're building the project from scratch and want the extras
inline on `rustango_users` itself. The `init-tenancy` verb will then
emit a bootstrap migration whose `CREATE TABLE rustango_users` carries
your extra columns.

**Step 1.** Define your model. It must declare every framework-required
column verbatim (`id`, `username`, `password_hash`, `is_superuser`,
`active`, `created_at`, `data`) plus extras. Extras must be `NULL`-able
or carry a `default = "…"`.

```rust
use rustango::sql::Auto;

#[derive(rustango::Model, Debug, Clone)]
#[rustango(table = "rustango_users")]
pub struct AppUser {
    #[rustango(primary_key)] pub id: Auto<i64>,
    #[rustango(max_length = 64, unique)] pub username: String,
    #[rustango(max_length = 255)] pub password_hash: String,
    pub is_superuser: bool,
    pub active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[rustango(default = "'{}'")] pub data: serde_json::Value,
    // extras —
    #[rustango(max_length = 128, default = "''")] pub display_name: String,
    #[rustango(max_length = 64, default = "'UTC'")] pub timezone: String,
}
impl rustango::tenancy::TenantUserModel for AppUser {}
```

**Step 2.** Wire the override into `main.rs`:

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustango::manage::Cli::new()
        .api(my_app::urls::router())
        .tenancy()
        .user_model::<AppUser>()
        .run().await
}
```

**Step 3.** Make sure no bootstrap migration exists yet. If you used
`cargo rustango new --template tenant`, the scaffolder pre-wrote
`migrations/0001_rustango_{registry,tenant}_initial.json` from a
static template — those use the framework's `User` and `init-tenancy`
won't replace them. Either:

- delete both `0001_rustango_*_initial.json` files before continuing, or
- start from a non-template `cargo new` and skip the scaffolder.

**Step 4.** Generate + apply:

```bash
cargo run -- init-tenancy        # writes 0001_*.json using AppUser's schema
cargo run -- migrate             # creates rustango_users with your extras
```

**Caveats:**

- `init-tenancy` is idempotent — once the JSON is on disk, changing
  `AppUser` won't rewrite it. To add columns later, write a regular
  `makemigrations`-style `AddColumn` migration.
- Both the framework's `User` and your `AppUser` register in the model
  inventory (they share `table = "rustango_users"`). `makemigrations`
  may emit redundant ops touching that table — review the generated
  JSON before applying. This is the main reason Option 2 is greenfield-
  only; on an established project Option 1 sidesteps the issue.
- The framework's auth and admin paths read the seven core columns by
  name; extras are accessible via `AppUser::objects().fetch(...)` only.

`Builder::user_model::<AppUser>()` is the equivalent setter for code
that constructs the server `Builder` directly (e.g. when you don't go
through `Cli`).

---

## Custom subcommands

You can extend the dispatcher by intercepting argv before forwarding to
`Cli::run`. Two shapes:

**Inline in `src/main.rs`** (no extra binary):

```rust
#[rustango::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("import-csv")) {
        let url = std::env::var("DATABASE_URL")?;
        let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;
        return my_csv_importer::run(&pool, &args[1..]).await;
    }
    rustango::manage::Cli::new().api(urls::api()).run().await
}
```

**Via `--with-manage-bin`** (separate `src/bin/manage.rs`):

```bash
cargo run -- startapp app --with-manage-bin
```

Then in `src/bin/manage.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = std::env::var("DATABASE_URL")?;
    let pool = rustango::sql::sqlx::PgPool::connect(&url).await?;

    match args.first().map(String::as_str) {
        Some("import-csv") => my_csv_importer::run(&pool, &args[1..]).await,
        _ => rustango::migrate::manage::run(&pool, "./migrations".as_ref(), args)
            .await
            .map_err(Into::into),
    }
}
```

Invoke project-specific subcommands the same way as framework ones:
`cargo run -- import-csv path/to/file.csv` (or
`cargo run --bin manage -- import-csv …` when `--with-manage-bin`).

---

## Common workflows

### First-time project setup (single-tenant)

```bash
cargo rustango new myapp
cd myapp
cp .env.example .env             # edit DATABASE_URL
docker compose up -d
cargo run -- migrate
cargo run                        # serve at :8080
```

### First-time project setup (tenancy)

```bash
cargo rustango new myapp --template tenant
cd myapp
cp .env.example .env             # edit DATABASE_URL + RUSTANGO_APEX_DOMAIN
docker compose up -d
cargo run -- migrate                                      # registry + tenants
cargo run -- create-operator admin --password letmein
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
cargo run                        # serve at :8080
```

### Adding tenants after the app is already running

A real-world tenancy app accumulates user models + migrations long
before its first tenant. The flow that works at any point in the
project's life:

```bash
# 1. (any time) develop user models — define structs with #[derive(Model)],
#    add `pub mod ...;` to src/lib.rs.
# 2. Generate scope-aware migrations. In a tenancy project this writes
#    up to TWO files: one tagged registry-scope (touches Org/Operator),
#    one tagged tenant-scope (touches User + your models). Pre-v0.24.2
#    this used to dump everything into one tenant-scoped file and
#    crash on `create-tenant` — see the changelog.
cargo run -- makemigrations

# 3. Apply migrations. `migrate` is scope-aware: it runs registry-
#    scoped files once against the registry pool first, then fans
#    tenant-scoped files across every active tenant.
cargo run -- migrate

# 4. Provision a NEW tenant whenever (could be days, weeks, many
#    migrations later). The tenancy code applies every accumulated
#    tenant-scoped migration to the new tenant's schema in one pass —
#    the new tenant arrives at the same schema state as existing ones.
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser
```

What makes this safe:
- `#[rustango(scope = "registry")]` on `Org`/`Operator` keeps registry-
  table changes out of tenant migrations.
- `migrate-tenants` walks every active org and applies only the
  tenant-scoped chain — registry-scoped files are skipped.
- `create-tenant` runs the same `migrate-tenants` pass against the
  newly-created schema, so the new tenant starts at the latest
  tenant-chain head with no manual fixup.

### Add a model

```bash
cargo run -- startapp blog        # if not done yet
# Edit src/blog/models.rs — add #[derive(Model)]
# Add `pub mod blog;` to src/lib.rs
cargo run -- makemigrations
cargo run -- migrate
```

### Add a JSON API for that model

```bash
cargo run -- make:viewset PostViewSet --model Post
# Edit src/post_view_set.rs — fill in field lists
# Mount in src/urls.rs
cargo run                        # GET /api/posts now works
```

### Add a data backfill

```bash
cargo run -- add-data-op \
    --sql "UPDATE posts SET slug = lower(title) WHERE slug IS NULL" \
    --reverse-sql "UPDATE posts SET slug = NULL" \
    --name backfill_post_slugs
cargo run -- migrate
```

### Pre-deploy audit

```bash
cargo run --release -- check --deploy
```

### Roll back the last migration

```bash
cargo run -- downgrade 1
```

### Apply a tenancy migration to one specific scope

```bash
cargo run -- migrate-registry            # registry-scoped only
cargo run -- migrate-tenants             # tenant-scoped, fan-out across orgs
```

### Decommission a tenant

```bash
cargo run -- drop-tenant acme            # soft (reversible)
cargo run -- purge-tenant acme           # hard (drops schema/db)
```
