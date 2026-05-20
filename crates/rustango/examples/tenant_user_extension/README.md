# `tenant_user_extension` — extra columns on `rustango_users`

Minimal demo of the [`rustango::tenancy::TenantUserModel`](https://docs.rs/rustango/latest/rustango/tenancy/trait.TenantUserModel.html)
trait. The `AppUser` model in [`src/models.rs`](src/models.rs) replaces
the framework's `User` for tenant storage and adds two typed columns
(`display_name`, `timezone`) inline on `rustango_users`.

## What this example shows

- Implementing `TenantUserModel` on a `#[derive(Model)]` struct that
  declares all seven framework-required columns plus extras.
- Wiring the override through `Cli::user_model::<AppUser>()` in
  [`src/main.rs`](src/main.rs) so `init-tenancy` materializes a
  bootstrap migration whose `CREATE TABLE rustango_users` carries the
  extras.
- Reading the extras via the ORM — see
  [`src/api.rs`](src/api.rs) for the `GET /api/users/:username`
  handler.
- Schema-validation invariants — the test in
  [`tests/bootstrap_migration.rs`](tests/bootstrap_migration.rs)
  asserts the bootstrap migration includes the extras and the
  framework's required columns, with no DB needed.

## Run end-to-end

Requires Postgres reachable at `DATABASE_URL` (the standard
`docker compose up -d postgres` from any rustango project works).

```bash
cd crates/rustango/examples/tenant_user_extension

# Sanity check (no DB)
cargo test --test bootstrap_migration

# Materialize the bootstrap migration with AppUser's schema —
# `migrations/` ships empty so the verb writes both 0001 JSONs.
cargo run -- init-tenancy

# Apply registry + tenant migrations to the configured DB
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/rustango_demo
cargo run -- migrate

# Provision a tenant + user
cargo run -- create-operator admin --password letmein
cargo run -- create-tenant acme --display-name "ACME Inc" \
                  --host-pattern acme.localhost
cargo run -- create-user acme alice --password tenantpw --superuser

# Serve. Requests to acme.localhost:8080 land in the `acme` tenant.
cargo run

# In another terminal: log in (sets session cookie), then read the extras.
curl -sc /tmp/c.txt -H "Host: acme.localhost" \
     -d "username=alice&password=tenantpw" \
     http://127.0.0.1:8080/__login >/dev/null
curl -sb /tmp/c.txt -H "Host: acme.localhost" http://127.0.0.1:8080/users/alice
# → {"id":1,"username":"alice","display_name":"","timezone":"UTC","is_superuser":true}
```

`display_name` and `timezone` come up empty / `UTC` because
`create-user` doesn't write to extras (the framework command knows
only the seven core columns). Set them with a regular `UPDATE` or
through your own admin / form / API — they're application data.

### Admin

Browse to <http://acme.localhost:8080/__admin/> after logging in. The
sidebar shows `Project → AppUser`; clicking through renders
`AppUser`'s `list_display` (with `display_name` and `timezone`
columns) and the detail view exposes every column on the row. The
admin's table-keyed lookup deduplicates by picking the richer schema
when two models share a table name — `AppUser` (9 fields) wins over
the framework's `User` (7 fields). See
[`crates/rustango/src/admin/helpers.rs::inventory_entries_dedup_by_table`](../../src/admin/helpers.rs)
for the dedup rule.

## How this differs from a fresh `cargo rustango new --template tenant`

`cargo rustango new --template tenant` writes
`migrations/0001_rustango_{registry,tenant}_initial.json` from a
**static** template using the framework's `User`. `init-tenancy` is
idempotent and won't replace those files. To use a custom user model
on a scaffolded project you must `rm` the two bootstrap JSONs first;
this example ships with `migrations/` empty for exactly that reason.

## Constraints

- `AppUser` declares every framework column verbatim
  (`id, username, password_hash, is_superuser, active, created_at,
  data`). Missing one trips
  `validate_tenant_user_schema` — covered by
  `tests/bootstrap_migration.rs::validate_accepts_app_user`.
- Both the framework's `User` and `AppUser` register in the model
  inventory under the same table name. Subsequent
  `cargo run -- makemigrations` runs may emit redundant ops touching
  `rustango_users`; review the JSON before applying. (Workaround for
  this example: don't run `makemigrations` against `rustango_users`
  after the initial bootstrap; use hand-written `AddColumn`
  migrations to add more extras later.)
- Extras must be `NULL`-able or carry `default = "…"` so the bootstrap
  migration applies cleanly to a fresh tenant schema.
- Framework auth/admin still reads the seven core columns by name;
  the two extras are only visible via `AppUser::objects().fetch_on(...)`.
