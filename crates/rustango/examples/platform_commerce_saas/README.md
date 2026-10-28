# platform_commerce_saas

Generated with `cargo rustango new platform_commerce_saas` — template `tenant (multi-tenancy + operator console)`,
backend `postgres`.

## Run locally — two paths

### A. All-in-Docker (default; hot-reload via cargo-watch)

```sh
cp .env.example .env
docker compose up -d                 # boots postgres + rust + cargo-watch
docker compose run --rm rust cargo run -- migrate
# server lives at http://localhost:8080 — edits to src/ trigger rebuild
```

The `rust` service runs `cargo watch -x run` against the bind-mounted
source tree. Three named volumes preserve incremental build state
across container restarts so a fresh `up` doesn't recompile from
scratch.

### B. Cargo on the host (Docker just for postgres)

```sh
cp .env.example .env                 # then change `postgres` -> `localhost` in DATABASE_URL
docker compose up -d --no-deps postgres  # only the DB: the rust service holds 8080
cargo run -- migrate                 # apply pending migrations
cargo run                            # boot the HTTP server
cargo run -- --help                  # full verb list (makemigrations, startapp, etc.)
```

Either way: `cargo run` (no args) is `runserver`. Every other
management verb (`makemigrations`, `migrate`, `startapp`, `check`, …)
flows through the same binary via `rustango::manage::Cli` — see
`src/main.rs`.

## Project layout

```text
src/
  main.rs         — Cli::new().api(urls::api()).run() boots both server + verbs
  models.rs       — empty; the models live in the commerce app
  views.rs        — request handlers ("views")
  urls.rs         — pub fn api() -> Router aggregator
  commerce/       — the app: models.rs, urls.rs, serializers.rs, jobs.rs

migrations/       — JSON migration files (committed to git)
```

Adding a new model is one struct in `commerce/models.rs`; the auto-admin sees
it immediately. See <https://github.com/ujeenet/rustango> for the full
feature list.
