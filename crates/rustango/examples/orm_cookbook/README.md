# orm_cookbook — runnable verification for `docs/orm.md`

Defines the recurring `Post` / `Author` models once and exercises the ORM
recipes from [`docs/orm.md`](../../../../docs/orm.md) against a **real
Postgres**, so the documented API can't silently drift.

In-repo example → it depends on the framework by path
(`rustango = { path = "../.." }`).

## Run it

```bash
docker compose -f ../getting_started_blog/docker-compose.yml up -d postgres   # or any Postgres
# Use a throwaway database — the test resets the `public` schema each run:
export DATABASE_URL=postgres://rustango:rustango@localhost:5432/orm_cookbook_dev
cargo test
```

## Sections verified (`tests/orm_smoke.rs`)

- **Querying** — `fetch`, typed-column `where_`, chained AND, `order_by` + `limit`, `filter_op`, `where_raw(WhereExpr::Or)`
- **Comparison filters** — `gt` / `is_in` / `ne` / `ilike` / `is_null` / `between`
- **Aggregations** — `count` / `sum` / `avg` / `max`, and `values(...).annotate(...)` GROUP BY via `fetch_aggregate_dict`

More sections (joins & preloading, bulk ops, upsert, transactions,
many-to-many, JSON, window functions, set operations, soft delete, raw
SQL, lazy FK loading) are queued to be added here the same way.
