# cookbook_blog — multi-tenant blog + feature cookbook

A living example that exercises every public feature surface in
`rustango` plus a chapter-by-chapter [COOKBOOK](COOKBOOK.md) keyed to
one minimal recipe and one verifying test per feature.

## Why

* **Reference**: every feature has a tested recipe.
* **Canary**: behaviour breakages surface in one place.
* **On-ramp**: read the cookbook top-to-bottom to learn rustango.

## Run

```sh
docker compose -f ../../../../docker-compose.yml up -d        # postgres
DATABASE_URL=postgres://rustango:rustango@localhost:5432/cookbook_blog \
    cargo run --bin manage -- migrate
DATABASE_URL=postgres://rustango:rustango@localhost:5432/cookbook_blog \
    cargo run --bin cookbook_blog
```

Visit:

* `http://localhost:8080/`           landing
* `http://localhost:8080/admin/`     auto-admin
* `http://acme.localhost:8080/`      tenant `acme`

## Test

Tests run against the docker postgres started above:

```sh
DATABASE_URL=postgres://rustango:rustango@localhost:5432/cookbook_blog \
    cargo test
```

Test files mirror the cookbook chapters (`tests/cookbook_chapter01_*.rs`
… `tests/cookbook_chapter12_*.rs`).
