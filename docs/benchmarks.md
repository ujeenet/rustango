# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

How fast is **Rustango**? This page compares the same blog on ten runtimes:
**Go** (standard-library `net/http` + `pgx`), **Rustango** on axum and on
actix-web, plain **axum** and **actix-web** with raw `sqlx`, a **Python**
framework on gunicorn and on Hypercorn, **Laravel** on php-fpm and on Octane,
and **WordPress**. Every app renders the same HTML from the same data.

All numbers were measured on **13 October 2026** with rustango 0.60.6, in one
harness, on one machine, under the same rules.

> **TL;DR.** With Go's exact SQL, plain Rust (axum + sqlx) beats Go by **34%**
> on the uncached index page, ties it on the tag page and trails it by **18%**
> on the uncached detail page. On Redis-cached pages every Rust app serves
> **1.5–2.2×** Go's requests with a p99 near **0.45 ms** (Go: 2.0–2.4 ms).
> In 0.60.6 **Rustango** cost **5–32%** against raw sqlx on the same server
> and the same queries; **0.60.7 closes that gap**: Rustango now serves
> **93–104%** of raw axum + sqlx on the uncached index and 94–100% on the
> other pages (see [0.60.7](#0607-closing-the-orm-gap)). The interpreted runtimes are **5–40×** behind on uncached pages; the
> fastest of them is the Python app on detail (2 865 req/s) and Laravel on
> Octane on index (1 098). All Rust apps run in **7–8 MiB** under load; Go in
> 17–21 MiB; the interpreted runtimes in 89–296 MiB.

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

### What keeps it fair

- **Same CPU budget.** Every app gets 4 CPUs (cores 0–3, `cpus: 4`, 2 GB RAM).
  Go runs with `GOMAXPROCS=4`, Tokio and actix-web with 4 workers.
- **Same pool.** Every compiled app has 16 PostgreSQL connections, minimum 4.
  The Python app uses its framework's built-in pool, 4 per worker process
  (16 per app).
- **Same queries.** The compiled apps all issue Go's plan: 3 queries for the
  index, 3 for detail, 4 for the tag page. The Rustango apps build them with
  the ORM; the plain Rust apps use Go's SQL text verbatim. The interpreted
  apps use their ORM's normal eager loading. WordPress runs 11–13 queries per
  page, 3 of them its own start-up queries.
- **Same output.** Before any load, every endpoint on every app is fetched
  and compared with Go's HTML; it must match after whitespace and
  entity-style normalisation, or the run stops. Responses are uncompressed
  everywhere (`Accept-Encoding: identity`, gzip off in nginx), so every app
  sends the same ~6.4 KB index page.
- **Production mode.** Debug off everywhere. PHP: OPcache on with timestamp
  checks off, `php artisan optimize` (config, routes, events, views cached).
  Python: persistent pooled connections.
- **Separate cores for everything else.** PostgreSQL 16, MySQL 8.4 and
  Redis 7 run on cores 4–7, the load generator on cores 8–11.
- **Load generator inside Docker.** [`oha`](https://github.com/hatoo/oha)
  1.16.0 (pinned by digest) runs on the compose network, not through Docker
  Desktop's port forwarding. 50 connections, keep-alive on.
- **Repeated runs.** Per endpoint: a 5 s warm-up, then 3 × 20 s runs; the
  median is reported. The app order rotates each round. Only HTTP 200
  responses count toward req/s.
- **Pinned builds.** Rust: rustc 1.99.0, `opt-level=3`, fat LTO,
  `codegen-units=1`, `panic=abort`. Go 1.25.14. PHP 8.3.35, Laravel 13.36.0,
  Swoole 6.2.3, WordPress 6.9.10. Base images by digest, lock files committed
  to the harness.

Host: Apple M3 Pro, Docker VM with 12 CPUs and 16 GB. Go ran in every batch
as an anchor and stayed within 6% of its first run.

---

## Compiled runtimes

### Throughput: requests per second (higher is better)

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
binary, so most of their size is the base image.

### Reading the numbers

- **Rust vs Go, same SQL.** axum + sqlx leads on the uncached index (+34%),
  Go leads on uncached detail (+22%), and the tag page is a tie. Go also
  serves 10% more compute requests, but its compute p99 (10.8 ms) is 2–3×
  the Rust apps'.
- **Cached pages.** With no database work left, every Rust app serves
  1.5–2.2× Go's requests, and the p99 stays under 0.55 ms.
- **The tag page measures PostgreSQL.** The database used 3.65 of its 4
  cores on this route, so all five apps hit the same wall.
- **Rustango vs raw sqlx** (same axum server, same queries): −32% on the
  index, −5% on detail, −12% on tag. Rustango is the only app at its full
  CPU budget (393%). Most of the gap is one function; see
  [Known costs](#known-costs-in-rustango). 0.60.7 closes it; see below.
- **actix-web vs axum.** With one shared sqlx pool, the actix apps stay at
  272–296% CPU and trail axum on uncached pages. The cause has not been
  pinned down ([#2660](https://github.com/ujeenet/rustango/issues/2660)).

### 0.60.7: closing the ORM gap

The table above is rustango 0.60.6. 0.60.7 decodes each column once by its
type instead of trying ten types per cell
([#2655](https://github.com/ujeenet/rustango/issues/2655)), reads column
names once per result set
([#2661](https://github.com/ujeenet/rustango/issues/2661)) and quotes
identifiers in one allocation
([#2662](https://github.com/ujeenet/rustango/issues/2662)). Measured in one
run, same method, old and new Rustango side by side (median req/s, no other
builds running):

| Endpoint | axum + sqlx | rustango 0.60.6 | rustango 0.60.7 | 0.60.7 vs axum |
|---|--:|--:|--:|--:|
| index, not cached | 10 750 | 8 596 | **11 143** | 104% |
| detail, not cached | 17 907 | 17 024 | 16 875 | 94% |
| tag, not cached | 6 242 | 5 596 | 6 023 | 96% |
| compute | 15 091 | 14 736 | 15 084 | 100% |

Decoding fell from 46% of the app's CPU to 4.9%, the same as raw sqlx's
typed reads (5.0%). The uncached index varied between runs; comparing each
app's best run, Rustango reaches 93% of axum + sqlx. What remains is shared
by both apps: Tera rendering (about 26%) and the pool's connection check on
acquire (about 18%).

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

### Reading the numbers

- **Uncached pages.** The best interpreted result on each page is 6–11×
  behind plain Rust and Go on the index and detail pages, and about 5× on the
  tag page.
- **Cached pages.** WordPress's `advanced-cache.php` serves a hit before
  WordPress loads, so it beats every other interpreted runtime (21k req/s).
  The Python app's cache is next on gunicorn (11k); Laravel runs its
  middleware stack before `Cache::remember` (3–6k).
- **gunicorn vs Hypercorn.** The views are synchronous, so ASGI only adds a
  threadpool hop: Hypercorn is 2× slower on detail and 4× slower on cached
  pages than gunicorn running the same app.
- **Octane vs php-fpm.** A resident Swoole worker is 2–3× php-fpm on
  uncached pages and about 1.8× on cached pages, and equal on compute.
- **Compute.** PHP 8.3 with OPcache runs the integer loop about 4× faster
  than CPython.
- **WordPress** runs on MySQL, not PostgreSQL, so its uncached numbers
  partly measure a different database. One of its three uncached-index runs
  failed with 14 253 "database connection" errors in 7 s (WordPress opens a
  MySQL connection per request); the median, 599, comes from a clean run.

---

## What changed since the previous version of this page

The June 2026 version reported Go at 6 651 req/s and Rustango at 4 781 on the
index, and Laravel, the Python app and Hypercorn with "100 % success". Several
of those numbers were wrong.

**Compiled runtimes.** Two gaps, each measured alone with the method above:

| Endpoint | Go, 12 threads | Go, 4 threads | rustango, old plan | rustango, Go's plan |
|---|--:|--:|--:|--:|
| index, not cached | 6 814 | 9 322 (+37%) | 7 385 | 8 462 (+15%) |
| detail, not cached | 19 184 | 22 125 (+15%) | 12 546 | 17 594 (+40%) |
| tag, not cached | 5 450 | 6 250 (+15%) | 5 965 | 5 515 (−8%) |
| compute | 9 263 | 16 599 (+79%) | 14 714 | 14 710 (0%) |

- **Go's thread count.** The Go module declared `go 1.23`, so Go 1.25 did not
  apply its container-aware `GOMAXPROCS` default and ran 12 threads on a
  4-CPU quota. The CPU throttling that caused also explains the old 47 ms
  compute p99, which the old page put down to garbage collection.
- **Rustango's queries.** The old app ran 5 queries on index and detail and 7
  on tag, loading every author, every tag and each post's full body. It now
  runs Go's plan.
- **Also fixed, not measured alone:** pool size (sqlx's default 10 against
  Go's 16), rustango 0.43 → 0.60.6, the load generator moved from the macOS
  host into the Docker network, core pinning, and one 10 s run replaced by
  the median of three 20 s runs.

**Interpreted runtimes.**

- **"100 % success" was connection success, not HTTP 200.** The June
  Hypercorn detail run had 5 457 HTTP 500s against 3 655 good responses.
  Only HTTP 200 counts now.
- **Hypercorn leaked database connections.** Persistent connections under
  ASGI opened a new PostgreSQL connection for most requests and hit the
  100-connection limit within seconds. It also opened a new Redis connection
  per request. It now uses the framework's PostgreSQL pool and one shared
  Redis connection pool per worker.
- **Laravel ran out of ports.** predis opened a new Redis connection per
  request; under 20 s of load the container ran out of local ports and
  returned 500s. It now uses phpredis with persistent connections.
- **Laravel sent compressed pages.** nginx and Octane gzipped the response, so
  Laravel sent ~700 bytes where the other apps sent 6.4 KB. Compression is now
  off everywhere.
- **Laravel's production caches were missing.** Only Blade views were cached;
  `php artisan optimize` now caches config, routes and events as well.

---

## Known costs in Rustango

These showed up while matching Go's queries. They are framework work, not
harness problems. The CPU shares come from `perf` profiles of the rustango
app under the same load (0.60.6). Each one has a ticket:

| Cost | Ticket | Status |
|---|---|---|
| `values_list` rows on PostgreSQL try about ten types per cell, building an error for each miss. **46% of the app's CPU** on the uncached index. | [#2655](https://github.com/ujeenet/rustango/issues/2655) | **Fixed in 0.60.7** (now 4.9%) |
| `CachePageLayer` stores the body as base64 inside JSON, so each hit parses JSON and decodes base64: **about 21% of the CPU per hit**, 165k hits/s against 214k for a plain Redis `GET`. | [#2656](https://github.com/ujeenet/rustango/issues/2656) | Open |
| `values()` and `annotate()` build a `HashMap` with owned string keys for every row: about 6–7% of the CPU on the uncached index. | [#2661](https://github.com/ujeenet/rustango/issues/2661) | **Fixed in 0.60.7** (names read once; the owned key stays, it is the public type) |
| Every request rebuilds and recompiles the same querysets: about 2% of the CPU. | [#2662](https://github.com/ujeenet/rustango/issues/2662) | Partly fixed in 0.60.7 (quoting); compiled-SQL cache open |
| `.only()` and `values_list` cannot select a related column such as `author__name`; matching Go's columns needs a manual `Join` with `project`. | [#2657](https://github.com/ujeenet/rustango/issues/2657) | Open |
| There is no `= ANY($1)` array bind; an `IN` list becomes `$1..$n`, so each list length is its own prepared statement. Not a CPU cost in this benchmark (under 0.5%). | [#2658](https://github.com/ujeenet/rustango/issues/2658) | Open |
| There is no batch many-to-many prefetch for a list of rows; tags come through a join on the through table. | [#2659](https://github.com/ujeenet/rustango/issues/2659) | Open |
| actix-web apps use under 300% of their 4 CPUs with a shared sqlx pool. Also seen without rustango. | [#2660](https://github.com/ujeenet/rustango/issues/2660) | Open |

---

## Limits of this benchmark

- It is a **read-heavy, server-rendered** workload. It does not measure
  writes, auth flows, websockets or heavy business logic.
- One host (an M3 Pro laptop with 6 performance and 6 efficiency cores) that
  also ran other work during the runs (load average 3–4). The relative
  picture travels; absolute numbers will differ on other hardware.
- Go and the plain Rust apps are hand-written services. Rustango, the Python
  app, Laravel and WordPress are frameworks; the Rustango vs raw sqlx rows
  show what that layer costs today.
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
