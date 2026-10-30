# orm_cookbook — runnable verification for `docs/orm.md`

Defines the recurring `Post` / `Author` models once and exercises the ORM
recipes from [`docs/orm.md`](../../../../docs/orm.md) against a **real
database**, so the documented API can't silently drift.

In-repo example → it depends on the framework by path
(`rustango = { path = "../.." }`).

## Run it

```bash
docker compose -f ../getting_started_blog/docker-compose.yml up -d --wait postgres   # or any Postgres
docker compose -f ../getting_started_blog/docker-compose.yml exec postgres createdb -U rustango orm_cookbook_dev
# Use a throwaway database — the test drops and recreates the model tables each run:
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/orm_cookbook_dev
cargo test
```

Or on SQLite, no server needed (MySQL works the same with `--features mysql`):

```bash
DATABASE_URL='sqlite:./orm_cookbook.db?mode=rwc' cargo test --no-default-features --features sqlite
```

## Sections verified (`tests/orm_smoke.rs`)

- **Querying** — `fetch`, typed-column `where_`, chained AND, `order_by` + `limit`, `filter_op`, `where_raw(WhereExpr::Or)`
- **Comparison filters** — `gt` / `is_in` / `ne` / `ilike` / `is_null` / `between`
- **Aggregations** — `count` / `sum` / `avg` / `max`, and `values(...).annotate(...)` GROUP BY via `fetch_aggregate_dict`

More sections (joins & preloading, bulk ops, upsert, transactions,
many-to-many, JSON, window functions, set operations, soft delete, raw
SQL, lazy FK loading) are queued to be added here the same way.
