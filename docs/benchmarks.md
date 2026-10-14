# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

The same blog served by ten runtimes: **Go** (standard-library `net/http` +
`pgx`), **Rustango** on axum and on actix-web, plain **axum** and
**actix-web** with raw `sqlx`, a **Python** framework on gunicorn and on
Hypercorn, **Laravel** on php-fpm and on Octane, and **WordPress**. Every app
renders the same HTML from the same data.

Measured on **13 October 2026**: rustango 0.60.6 in the main tables, rustango
0.60.7 in [its own table](#rustango-0607). One harness, one machine, the same
rules for every app.

## Summary

- **Uncached pages, compiled apps.** Plain Rust (axum + sqlx) is 34% faster
  than Go on the index, Go is 22% faster on detail, the tag page is a tie.
- **Rustango 0.60.7** serves 93–104% of plain axum + sqlx on the uncached
  index and 94–100% on detail, tag and compute.
- **Cached pages.** Every Rust app serves 1.5–2.2× Go's requests, with a p99
  near 0.45 ms (Go: 2.0–2.4 ms).
- **Interpreted runtimes** are 5–40× behind the compiled apps on uncached
  pages. On cached pages WordPress is the fastest of them (21k req/s).
- **Memory under load:** Rust apps 7–8 MiB, Go 17–21 MiB, interpreted
  runtimes 89–296 MiB.

---

## The setup

A blog with **authors, posts, tags (many-to-many) and comments**:

| Route | Renders | Cached? |
|---|---|---|
| `GET /` | latest 20 posts, each with author, tags, comment count | no |
| `GET /cached` | same as `/` | Redis, 60 s |
| `GET /post/{slug}` | post body + author + tags + every comment | no |
| `GET /post/{slug}/cached` | same as detail | Redis, 60 s |
| `GET /tag/{slug}` | latest 20 posts carrying a tag | no |
| `GET /compute` | sum of every prime below 20 000 (CPU only, no DB) | no |

The seed is deterministic: 10 authors, 30 tags, **1 000 posts**, 2 600
post-tag links and **10 000 comments**. Relations load eagerly (no N+1).
`/compute` runs the same trial-division loop in every language.

### The apps

| App | Server | Data access | Page cache |
|---|---|---|---|
| **Go** | stdlib `net/http` | `pgx` pool, raw SQL | `go-redis` GET/SET |
| **rustango · axum** | axum 0.8 | rustango ORM | `CachePageLayer` |
| **rustango · actix** | actix-web 4 | rustango ORM | Redis GET/SET by hand |
| **axum + sqlx** | axum 0.8 | raw `sqlx`, Go's SQL text | Redis GET/SET by hand |
| **actix + sqlx** | actix-web 4 | raw `sqlx`, Go's SQL text | Redis GET/SET by hand |
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI) | the framework's ORM, built-in psycopg pool | framework Redis cache |
| **Python · Hypercorn** | Hypercorn, 4 workers (ASGI, sync views) | same | same |
| **Laravel · php-fpm** | nginx + php-fpm, 16 static children | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | same | same |
| **WordPress** | nginx + php-fpm, 16 static children | `WP_Query`, `get_the_tags`, `get_comments` on **MySQL 8.4** | `advanced-cache.php` drop-in, phpredis |

Templates: Tera for the Rust apps, `html/template` for Go, the Python
framework's own engine, Blade for Laravel, a small theme for WordPress.

### Test rules

- **CPU and memory.** Every app gets 4 CPUs (cores 0–3) and 2 GB RAM. Go runs
  with `GOMAXPROCS=4`, Tokio and actix-web with 4 workers.
- **Database pool.** 16 PostgreSQL connections per app (the Python app: 4 per
  worker process, 16 in total).
- **Queries.** The compiled apps run the same plan: 3 queries for the index,
  3 for detail, 4 for the tag page. Rustango builds them with the ORM; the
  plain Rust apps use Go's SQL text. The interpreted apps use their ORM's
  eager loading. WordPress runs 11–13 queries per page.
- **Output.** Before any load, every page of every app is compared with Go's
  HTML; a mismatch stops the run. No compression, so every app sends the
  same ~6.4 KB index page.
- **Production settings.** Debug off. PHP: OPcache on, timestamp checks off,
  `php artisan optimize`. Python: pooled connections.
- **Other services.** PostgreSQL 16, MySQL 8.4 and Redis 7 on cores 4–7; the
  load generator [`oha`](https://github.com/hatoo/oha) 1.16.0 on cores 8–11,
  inside the Docker network. 50 connections, keep-alive on.
- **Runs.** Per endpoint: 5 s warm-up, then 3 × 20 s; the median counts. Only
  HTTP 200 responses count.
- **Builds.** rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`,
  `panic=abort`), Go 1.25.14, PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3,
  WordPress 6.9.10.

Host: Apple M3 Pro, Docker VM with 12 CPUs and 16 GB.

---

## Compiled runtimes

### Throughput: requests per second (higher is better)

Rustango columns: 0.60.6.

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| index, not cached | 9 360 | 8 485 | 6 341 | **12 505** | 9 963 |
| index, cached | 96 214 | 164 942 | 200 167 | **214 097** | 197 252 |
| detail, not cached | **22 675** | 17 634 | 13 883 | 18 578 | 14 697 |
| detail, cached | 143 795 | 194 939 | 214 662 | **233 669** | 211 293 |
| tag, not cached | **6 457** | 5 577 | 5 146 | 6 299 | 5 903 |
| compute | **16 588** | 14 733 | 15 137 | 15 085 | 14 741 |

### Latency: p50 / p99 in ms (lower is better)

| Endpoint | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| index, not cached | 5.11 / 9.31 | 5.81 / 8.00 | 7.78 / 10.20 | **3.93 / 5.80** | 4.95 / 6.99 |
| index, cached | 0.36 / 2.40 | 0.30 / 0.53 | 0.24 / 0.47 | **0.23 / 0.42** | 0.24 / 0.46 |
| detail, not cached | **1.91** / 4.97 | 2.78 / 4.32 | 3.54 / 5.19 | 2.64 / **4.12** | 3.34 / 4.88 |
| detail, cached | 0.24 / 2.01 | 0.25 / 0.47 | 0.22 / 0.46 | **0.21 / 0.40** | 0.22 / 0.47 |
| tag, not cached | **7.53** / 11.73 | 8.83 / 11.99 | 9.59 / 12.65 | 7.82 / **10.42** | 8.36 / 11.28 |
| compute | **2.36** / 10.84 | 3.40 / 6.07 | 3.32 / **3.58** | 3.33 / 5.97 | 3.39 / 3.65 |

### Footprint

| | Go | rustango · axum | rustango · actix | axum + sqlx | actix + sqlx |
|---|--:|--:|--:|--:|--:|
| Image (uncompressed) | **18.5 MB** | 147 MB | 146 MB | 143 MB | 143 MB |
| RAM, idle | 5.1 MiB | 1.4 MiB | 1.7 MiB | **1.3 MiB** | 1.6 MiB |
| RAM, index under load | 21.2 MiB | 7.5 MiB | 7.3 MiB | 7.3 MiB | **6.9 MiB** |
| CPU, index under load (of 400%) | 296% | 393% | 296% | 328% | 272% |

Go ships on `scratch`; the Rust images are `debian:bookworm-slim` plus the
binary.

### Rustango 0.60.7

Measured in a separate run, with axum + sqlx and rustango 0.60.6 re-measured
alongside (median req/s):

| Endpoint | axum + sqlx | rustango 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| index, not cached | 10 750 | 8 596 | **11 143** | 104% |
| detail, not cached | 17 907 | 17 024 | 16 875 | 94% |
| tag, not cached | 6 242 | 5 596 | 6 023 | 96% |
| compute | 15 091 | 14 736 | 15 084 | 100% |

The uncached index varied between runs; comparing each app's best run,
Rustango 0.60.7 reaches 93% of axum + sqlx.

---

## Interpreted runtimes

### Throughput: requests per second (higher is better)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| index, not cached | 683 | 601 | 518 | **1 098** | 599 |
| index, cached | 11 052 | 2 744 | 3 326 | 5 884 | **21 285** |
| detail, not cached | **2 865** | 1 314 | 572 | 1 712 | 687 |
| detail, cached | 11 473 | 2 644 | 3 013 | 5 836 | **21 591** |
| tag, not cached | **1 337** | 786 | 490 | 1 102 | 647 |
| compute | 410 | 353 | **1 611** | 1 521 | 732 |

### Latency: p50 / p99 in ms (lower is better)

| Endpoint | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| index, not cached | 74.5 / 137.7 | 96.4 / 213.0 | 95.5 / 122.6 | **44.1** / 110.1 | 78.1 / **103.9** |
| index, cached | 4.4 / 12.5 | 18.7 / 40.4 | 14.6 / 22.4 | 7.4 / 57.7 | **2.3 / 3.5** |
| detail, not cached | **14.2 / 43.0** | 35.3 / 76.2 | 86.5 / 113.4 | 27.9 / 96.2 | 70.5 / 99.0 |
| detail, cached | 4.1 / 11.4 | 19.0 / 48.2 | 16.4 / 21.6 | 7.4 / 58.2 | **2.3 / 3.6** |
| tag, not cached | **35.6 / 77.0** | 66.0 / 131.1 | 100.8 / 130.1 | 44.0 / 110.9 | 75.9 / 97.0 |
| compute | 117.9 / 222.5 | 129.6 / 245.3 | **31.3 / 38.7** | 32.7 / 78.5 | 67.0 / 89.0 |

### Footprint

| | Python · gunicorn | Python · Hypercorn | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|--:|--:|
| Image (uncompressed) | **294 MB** | **294 MB** | 844 MB | 895 MB | 1.01 GB |
| RAM, idle | 128 MiB | 172 MiB | **54 MiB** | 219 MiB | 61 MiB |
| RAM, index under load | 224 MiB | 296 MiB | **89 MiB** | 241 MiB | 142 MiB |
| CPU, index under load (of 400%) | 260% | 323% | 380% | 337% | 405% |

One of WordPress's three uncached-index runs returned 14 253 "database
connection" errors in 7 s; the median comes from a clean run.

---

## Why: what is faster, and where

Measured facts only; where a cause was not profiled, it says so.

- **Tag page: the database decides.** PostgreSQL used 3.65 of its 4 cores
  on this route, so all compiled apps land within 20% of each other.
- **Uncached index: plain Rust leads Go by 34%.** Same three queries, same
  rows. Not profiled on the Go side.
- **Uncached detail: Go leads plain Rust by 22%.** The page renders one post
  with all its comments. Not profiled.
- **Cached pages: Rust 1.5–2.2× Go, p99 ~0.45 ms vs 2.0–2.4 ms.** A hit is
  one Redis read and a response; no database, no template. The cause of Go's
  wider tail was not profiled.
- **Compute: Go serves 10% more requests, Rust has a shorter tail** (p99
  3.6–6.1 ms vs 10.8 ms) on the same trial-division loop.
- **Memory: Rust 7–8 MiB, Go 17–21 MiB under load.** Go's runtime keeps a
  garbage-collected heap; the Rust binaries free memory as they go.
- **Rustango vs plain axum + sqlx.** In 0.60.6, 46% of Rustango's CPU went to
  decoding query results by trying about ten types per value. 0.60.7 decodes
  each column once by its type (4.9% of CPU, raw sqlx: 5.0%), which brings it
  to plain-sqlx speed. The rest of the CPU is the same in both apps: Tera
  rendering (~26%) and the pool's connection check (~18%).
- **actix-web vs axum.** With one shared sqlx pool, the actix apps use only
  272–296% of 4 CPUs and trail axum on uncached pages. Cause not found
  ([#2660](https://github.com/ujeenet/rustango/issues/2660)).
- **Cached pages, interpreted.** WordPress answers from Redis in
  `advanced-cache.php`, before WordPress itself loads (21k req/s). The Python
  app and Laravel run their framework and middleware first, then read the
  cache (11k and 3–6k).
- **Laravel: Octane 2–3× php-fpm on uncached pages.** php-fpm boots the
  framework on every request; Octane keeps it in memory.
- **Python: gunicorn 2–4× Hypercorn.** The views are synchronous, so under
  ASGI each request goes through a threadpool.
- **Compute, interpreted.** PHP 8.3 with OPcache runs the integer loop about
  4× faster than CPython.
- **WordPress uncached** runs 11–13 queries per page on MySQL, against 3–4 on
  PostgreSQL for the other apps.

---

## Rustango CPU profile

`perf` profile of rustango 0.60.6 under load on the uncached index, with
status in 0.60.7:

| Cost | CPU share | Ticket | Status |
|---|---|---|---|
| Decoding each value by trying about ten types | 46% | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Fixed in 0.60.7** (now 4.9%) |
| `CachePageLayer` stores pages as JSON + base64 | ~21% of a cached hit | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Open |
| A `HashMap` with owned keys for every `values()` row | 6–7% | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Fixed in 0.60.7** (names read once) |
| Querysets rebuilt and compiled on every request | ~2% | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Partly fixed in 0.60.7 |
| `.only()` / `values_list` cannot select a related column (`author__name`) | API gap | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Open |
| No `= ANY($1)` array bind for `IN` lists | under 0.5% | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Open |
| No batch many-to-many prefetch | API gap | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Open |

---

## Limits

- A **read-heavy, server-rendered** workload. No writes, auth flows,
  websockets or heavy business logic.
- One host (M3 Pro laptop, 6 performance + 6 efficiency cores). Absolute
  numbers will differ on other hardware.
- Go and the plain Rust apps are hand-written services; Rustango, the Python
  app, Laravel and WordPress are frameworks.
- WordPress runs on MySQL because it does not support PostgreSQL.

---

## Reproduce

The harness is a self-contained project (`rustango-bench`). From its
directory:

```sh
bench/vendor.sh                 # vendor rustango at origin/develop into the build
docker compose --profile app build
bench/run.sh                    # 5 compiled apps; APPS="..." to pick others
```

Tune the run with `REPS`, `DURATION`, `WARMUP` and `CONCURRENCY`, and the
CPU split with `CAP_CPUS` and the `CAP_CPUSET_*` values in `.env`. Raw `oha`
JSON, the HTML parity files and a `meta.txt` with every version land in
`bench/results/`.

---

## See also

- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
