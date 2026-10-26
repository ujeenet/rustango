# Benchmarks: Rustango vs Go vs Python vs Laravel vs WordPress

The same blog served by every app, with the same data and the same HTML. Two
rounds so far:

- **Round 2, 14 October 2026: compiled apps, profiled.** **Go** (stdlib
  `net/http` + `pgx`) and seven Rust apps: plain **axum** and **actix-web**
  with raw `sqlx`, **Rocket**, **Loco**, **Cot**, and **Rustango** on axum and
  on actix-web. Only the app under test runs. CPU, syscall and Postgres wire
  profiles show where each app spends its time.
- **Round 1, 13 October 2026: every runtime.** Go, Rustango and plain Rust,
  a **Python** framework on gunicorn, Hypercorn, Uvicorn and Granian (Python
  servers measured on 14 October), **Laravel** on php-fpm and on Octane, and
  **WordPress**. Its compiled results are replaced by round 2; its
  interpreted results are below.

Write-ups with charts:
[Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled)
and [round 1](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress).

## Summary

- **Uncached pages.** Rustango on actix-web and on axum are the two fastest
  of the eight compiled apps on the index, detail and tag pages (index:
  17 401 and 14 686 req/s; axum + sqlx 14 140, Go 8 741).
- **Cached pages.** Every Rust app serves more than Go: 138 224–231 975 req/s
  on the cached index against Go's 91 839. Go's garbage collector is the main
  cause.
- **Compute.** Go is 8–17% faster than the Rust apps. Rust compiled with Go's
  instruction order runs in Go's time.
- **Memory under load:** 6.3–12.0 MiB for six of the Rust apps, Cot 22.6 MiB,
  Go 22.8 MiB.
- **Interpreted runtimes** (round 1) are 4–40× behind the compiled apps on
  uncached pages. On cached pages WordPress is the fastest of them (21k
  req/s). Python's gunicorn (sync) beats the three ASGI servers; Granian is
  the fastest ASGI server. Memory under load: 89–293 MiB.
- **Rustango fixes.** The profiles found where Rustango lost time; 0.60.7 and
  0.60.8 fix the large costs and 0.61.2 adds two missing query features (see
  [the table](#rustango-profile-findings-and-status)).

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

### Test rules

- **CPU and memory.** Every app gets 4 CPUs (cores 0–3) and 2 GB RAM. Go runs
  with `GOMAXPROCS=4`, Tokio and actix-web with 4 workers.
- **Database pool.** 16 PostgreSQL connections per app (round 2: 16 max, 4
  min; the Python app: 4 per worker process, 16 in total).
- **Queries.** The compiled apps run the same plan: 3 queries for the index,
  3 for detail, 4 for the tag page (Cot: 5, 5 and 7). Rustango and Loco build
  them with their ORM; the plain Rust apps use Go's SQL text. The interpreted
  apps use their ORM's eager loading. WordPress runs 11–13 queries per page.
- **Output.** Before any load, every page of every app is compared with Go's
  HTML; a mismatch stops the run. No compression, so every app sends the
  same ~6.4 KB index page.
- **Production settings.** Debug off. PHP: OPcache on, timestamp checks off,
  `php artisan optimize`. Python: pooled connections, access logs off.
- **Other services.** PostgreSQL 16, MySQL 8.4 (WordPress only) and Redis 7 on
  cores 4–7; the load generator [`oha`](https://github.com/hatoo/oha) 1.16.0
  on cores 8–11, inside the Docker network. 50 connections, keep-alive on.
- **Isolation (round 2).** Only the app under test runs; the others are
  stopped. Between apps, the number of Postgres connections goes back to its
  baseline.
- **Runs.** Per endpoint: 5 s warm-up, then 3 × 20 s; the median counts. Only
  HTTP 200 responses count. Round 2 had 0 errors or non-200 responses in 144
  runs.

Host: Apple M3 Pro, Docker VM with 12 CPUs and 16 GB.

---

## Round 2: compiled runtimes

Measured on **14 October 2026**.

### The apps

| App | Server | Data access | Templates | Page cache |
|---|---|---|---|---|
| **Go** | stdlib `net/http` | `pgx` v5 pool, raw SQL | `html/template` | `go-redis` |
| **axum + sqlx** | axum | raw `sqlx`, Go's SQL text | Tera | Redis by hand |
| **actix + sqlx** | actix-web | raw `sqlx`, Go's SQL text | Tera | Redis by hand |
| **Rocket** | Rocket | `rocket_db_pools` (sqlx) | `rocket_dyn_templates` (Tera) | Redis |
| **Rustango · axum** | axum | rustango ORM | Tera | `CachePageLayer` |
| **Rustango · actix** | actix-web | rustango ORM | Tera | rustango page cache |
| **Loco** | Loco | SeaORM | Tera | Redis |
| **Cot** | Cot | Cot's ORM | askama | Redis |

### Versions

- Go 1.25.14, pgx v5.7.2, go-redis v9.7.0
- rustc 1.99.0 (`opt-level=3`, fat LTO, `codegen-units=1`, `panic=abort`);
  axum 0.8.9, actix-web 4.16.0, tokio 1.53, sqlx 0.8.6, Tera 1.20.1
- Rocket 0.5.1 (sqlx 0.7.4, hyper 0.14)
- Loco 1.2.0 (SeaORM 2.0.4, sqlx 0.9.0)
- Cot 0.7.0 (sqlx 0.9.0, askama 0.16.1)
- Rustango: a development build with no acquire ping, a page cache that
  stores raw bytes and one pooled connection per request. These three
  changes shipped in 0.60.8.

### Settings that could not be equalized

| Setting | Apps | Why |
|---|---|---|
| Pool pings Postgres on every checkout (sqlx `test_before_acquire`) | Rocket, Loco, Cot | No setting to turn it off. It is off in axum + sqlx, actix + sqlx and Rustango. |
| 10 pool connections instead of 16 | Cot | Cot has no pool settings. |
| One pool slice per worker (4 of the 16 connections) and one Redis connection per worker | actix + sqlx, Rustango · actix | A shared pool is slower on actix-web ([finding](#actix-web-a-shared-pool-is-slow)). |
| sqlx 0.7.4 and hyper 0.14 | Rocket | Pinned by Rocket 0.5.1. |
| 5, 5 and 7 queries per index, detail and tag page instead of 3, 3 and 4 | Cot | Its query API cannot express Go's queries. |
| askama instead of Tera | Cot | Cot's template engine. |

### Throughput: requests per second (higher is better)

| App | Index | Detail | Tag | Index, cached | Detail, cached | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 8 741 | 22 607 | 6 386 | 91 839 | 141 574 | **16 580** |
| axum + sqlx | 14 140 | 22 842 | 6 467 | 198 669 | 214 871 | 14 963 |
| actix + sqlx | 14 408 | 21 630 | 6 628 | 226 887 | 240 161 | 14 578 |
| Rocket | 13 316 | 24 561 | 6 836 | 138 224 | 155 691 | 14 696 |
| Rustango · axum | 14 686 | 25 788 | **7 090** | 174 501 | 192 897 | 14 799 |
| Rustango · actix | **17 401** | **27 292** | 7 056 | **231 975** | **242 609** | 14 710 |
| Loco | 10 290 | 15 635 | 5 604 | 153 243 | 170 158 | 14 184 |
| Cot | 2 018 | 9 282 | 3 065 | 190 245 | 203 119 | 15 330 |

Index, detail and tag are not cached.

### Latency: p50 / p99 in ms (lower is better)

| App | Index | Detail | Tag | Index, cached | Detail, cached | Compute |
|---|--:|--:|--:|--:|--:|--:|
| Go | 5.50 / 9.96 | 1.91 / 5.02 | 7.61 / 11.72 | 0.35 / 2.73 | 0.24 / 2.05 | **2.37** / 10.75 |
| axum + sqlx | 3.49 / 4.71 | 2.14 / 3.60 | 7.42 / 13.89 | 0.24 / 0.53 | 0.22 / 0.51 | 3.35 / 6.07 |
| actix + sqlx | 3.32 / 6.38 | 2.21 / 4.30 | 7.23 / 13.23 | **0.20** / 0.50 | **0.19 / 0.45** | 3.41 / 4.97 |
| Rocket | 3.66 / 5.83 | 1.97 / 3.58 | 7.14 / 11.55 | 0.34 / 0.70 | 0.30 / 0.67 | 3.40 / 6.14 |
| Rustango · axum | 3.28 / 5.65 | 1.83 / 3.55 | **6.90 / 11.18** | 0.27 / 0.59 | 0.25 / 0.52 | 3.38 / 6.09 |
| Rustango · actix | **2.80 / 4.51** | **1.77 / 3.37** | 6.91 / 11.53 | **0.20 / 0.48** | **0.19 / 0.45** | 3.41 / **3.96** |
| Loco | 4.62 / 10.84 | 2.99 / 6.34 | 8.67 / 14.52 | 0.30 / 0.79 | 0.28 / 0.62 | 3.48 / 6.90 |
| Cot | 23.35 / 50.36 | 5.28 / 8.02 | 15.57 / 27.06 | 0.25 / 0.50 | 0.23 / 0.52 | 3.27 / 5.85 |

### Memory under load

Resident memory of the app container on the uncached index (`docker stats`,
the sample with the highest CPU out of six taken during the first index run):

| | Go | axum + sqlx | actix + sqlx | Rocket | Rustango · axum | Rustango · actix | Loco | Cot |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| RAM, MiB | 22.8 | 7.6 | 7.2 | 8.7 | 6.9 | **6.3** | 12.0 | 22.6 |

Round 1 image sizes (uncompressed): Go 18.5 MB on `scratch`, the Rust apps
143–147 MB on `debian:bookworm-slim`.

### Rustango from round 1 to round 2

Rustango · actix, uncached index, req/s:

| Run | req/s |
|---|--:|
| Round 1 (0.60.6) | 6 341 |
| Fixes, shared pool (profiling run) | 12 052 |
| Fixes, one pool slice per worker (round 2) | **17 401** |

Rustango · axum before → after the fixes, profiling runs:

| | Before | After |
|---|--:|--:|
| Index, req/s | 11 215 | **15 849** |
| Detail, req/s | 17 267 | **28 097** |
| Index, app CPU per request | 286.6 µs | **220.8 µs** |
| Detail, app CPU per request | 164.7 µs | **110.4 µs** |

---

## Why: what is faster, and where

Measured facts only; where a cause is not proven, it says so. Profiles are
single 25 s runs, one app at a time, so their req/s differ a little from the
tables above.

### Compute: Go is 8–17% faster

Both compiled loops have 8 instructions, a 64-bit `sdiv` and one
divide-by-zero check; LLVM picks a different instruction order. Alone on one
core, outside HTTP, one call takes 194 µs in Go, 224 µs in Rust and 194 µs in
Rust with Go's exact instruction order. Which CPU detail makes that order
slower is unproven: the Docker VM has no hardware counters. The rest of each
request is cheaper in Rust (8 µs against Go's 14 µs).

Go has the lower p50 but the higher p99: it serves requests out of order
(p10 0.56 ms, p99 10.75 ms), the Rust apps close to first-in, first-out
(axum + sqlx: p10 1.85 ms, p99 6.07 ms). Why Go's p99 sits near 10 ms is
unproven.

### Cached pages: Go's garbage collector

Go runs 441 garbage collections per second on the cached index; the
stop-the-world pauses add up to about 10% of the time. Raising `GOGC` removes
most of the gap (profiling runs, default `GOGC=100`: 97 289 req/s):

| Go, cached index | req/s | p99 |
|---|--:|--:|
| `GOGC=100` (default) | 97 289 | 2.48 ms |
| `GOGC=400` | 144 083 | |
| GC nearly off | 159 734 | 0.90 ms |

The rest is CPU per request:

| Cached index, per request | Go | axum + sqlx |
|---|--:|--:|
| App CPU | 27.2 µs | **14.6 µs** |
| Kernel write path | 10.6 µs | **6.9 µs** |
| Syscalls | 10.7 (6.6 read, 2.9 write) | **4.8** (2.0 recvfrom, 1.4 writev) |
| Redis CPU | 6.8 µs | **2.5 µs** |
| Redis traffic | go-redis: one GET per request | one connection, GETs from many requests batched in one pipeline |

### Detail page: Postgres round trips

All apps send the same three queries, but the number of round trips
differs. pgx sends one per query. sqlx adds one when a connection is taken
from the pool (the acquire ping) and one when it goes back. axum + sqlx takes
a connection per query, so it pays per query; Rocket and Rustango take one
connection per page, so they pay once. Wire capture during load, counting
ReadyForQuery messages per detail page (the index shows the same counts):

| App | Round trips |
|---|--:|
| Go · pgx | **3** |
| Rustango | 4 |
| Rocket | 5 |
| axum + sqlx, ping off | 6 |
| axum + sqlx, ping on | 9 |

Turning the acquire ping off in axum + sqlx (profiling runs): detail
18 222 → 23 472 req/s (Postgres CPU 166.6 → 134.3 µs per request), index
12 140 → 14 419 req/s (237.9 → 208.5 µs). Three fewer round trips save
29–32 µs of Postgres CPU per page: about 10 µs each. Go has the fewest round
trips but its garbage collector stops it about 9% of the time on this route.

### Index page: GC and template rendering

Go's garbage collector again (pauses about 8% of the time; `GOGC=400` takes
the index from 9 359 to 12 848 req/s, GC nearly off to 13 967), and template
rendering: Go's `html/template` uses reflection and costs 167 µs per request,
serde + Tera 72 µs. In axum + sqlx, sqlx costs about 112 µs per request (of
which about 25 µs is the ping when a connection goes back to the pool), glibc
malloc about 56 µs and kernel `sendto` 38 µs; the parts overlap.

### actix-web: a shared pool is slow

In round 1 the actix-web apps trailed axum on uncached pages and used under
300% of their 400% CPU. axum runs one shared, multi-threaded Tokio runtime;
actix-web runs one single-threaded runtime per worker. With one pool shared
by all workers, every query wakes a thread of another runtime. One pool slice
(4 of the 16 connections) and one Redis connection per worker fix it
(profiling runs, req/s):

| | Shared pool | Pool per worker |
|---|--:|--:|
| actix + sqlx, index | 11 094 | **15 108** |
| actix + sqlx, detail | 18 080 | **23 330** |
| actix + sqlx, index cached | 192 604 | **238 884** |
| Rustango · actix, index | 12 052 | **17 454** |
| Rustango · actix, detail | 23 032 | **28 693** |
| Rustango · actix, index cached | 190 889 | **237 962** |

Context switches per request drop about 2.5× (actix + sqlx index 4.04 →
1.45, detail 3.57 → 1.64; Rustango · actix index 2.70 → 0.81, detail 2.34 →
1.03). More workers do not help: 8 workers with a shared pool reach 11 904
req/s on the actix + sqlx index. The effect is measured; the mechanism (a
socket is driven by the runtime that opened it) is the best explanation, not
proven directly. **On actix-web, create the database pool inside each
worker, not once for all workers.**

### Rocket, Loco and Cot

- **Rocket** is strong on database pages (one connection per page, 5 round
  trips) and the slowest Rust app on cached pages: 24.1 µs of CPU per request
  against axum + sqlx's 14.6 µs, 8.1 syscalls against 4.8, an extra `writev`
  (2.24 per request against 1.36) and contention in malloc (futex 2.88 per
  request against 0.91).
- **Loco**: SeaORM sends the same number of queries as Go. On cached pages
  Loco spends 21.0 µs per request: 2.4 µs more in malloc and 1.2 µs more in
  middleware than axum + sqlx. On database pages the acquire ping cannot be
  turned off; those pages were not profiled, so how much of the gap it
  explains is unproven.
- **Cot**: the query API has only filter, limit and offset (no ORDER BY,
  JOIN, IN or COUNT), so the index loads all 1 000 posts with their bodies
  on every request: 5, 5 and 7 queries instead of 3, 3 and 4, and 22.6 MiB
  under load. Cached pages skip the ORM and are fast (16.4 µs of CPU per
  request).

### Shared costs

Two costs show up in every Rust app: Tera rendering (about 18–20% of CPU on
the uncached pages) and glibc malloc (about 12%). A faster allocator such as
mimalloc or jemalloc could cut the second one; this is untested.

### Interpreted runtimes (round 1)

- **Cached pages.** WordPress answers from Redis in `advanced-cache.php`,
  before WordPress itself loads (21k req/s). The Python apps and Laravel run
  their framework and middleware first, then read the cache (Python 5–12k,
  Laravel 3–6k).
- **Laravel: Octane 2–3× php-fpm on uncached pages.** php-fpm boots the
  framework on every request; Octane keeps it in memory.
- **Python: async views do not beat gunicorn.** gunicorn serves 1.4–2.3× the
  ASGI servers on cached pages and 1.4–1.8× on uncached detail and tag; on
  the uncached index all four are equal (663–697). From the framework's code:
  the async ORM runs the sync ORM in a thread (`sync_to_async`), there is no
  async database pool, and the ASGI handler creates a thread executor per
  request plus about 3 thread hops (request signals, response close). The
  async cached path does less cache work than gunicorn's (one Redis GET of
  raw HTML against two GETs and an unpickle) and is still slower. The share
  of each cost was not profiled.
- **ASGI servers: Granian > Uvicorn > Hypercorn.** Cached index 7 706 /
  6 233 / 5 151 req/s, uncached detail 1 854 / 1 654 / 1 550. Not profiled.
- **Compute.** PHP 8.3 with OPcache runs the integer loop about 4× faster
  than CPython.
- **WordPress uncached** runs 11–13 queries per page on MySQL, against 3–4 on
  PostgreSQL for the other apps.

---

## Rustango profile findings and status

From `perf` profiles of Rustango under load: round 1 (0.60.6) and the round 2
profiling runs.

| Cost | CPU share | Status |
|---|---|---|
| Decoding each value by trying about ten types | 46% of the index | **Fixed in 0.60.7**: each column decoded once by its type (now 4.9%) |
| A pool checkout for every query | 19–35% | **Fixed in 0.60.8**: `pool.acquire()` runs a request's queries on one connection (detail +26%) |
| Ping to Postgres on every checkout | 8–16% | **Fixed in 0.60.8**: off by default; `pool_test_before_acquire` turns it on |
| `CachePageLayer` stores pages as JSON + base64 | ~12–20% per cache hit | **Fixed in 0.60.8**: pages stored as raw bytes |
| A `HashMap` with owned keys for every `values()` row | 6–7% | Reduced in 0.60.7: names read once per result set; the owned key stays (public type) |
| Querysets rebuilt and compiled on every request | ~2% | Open; 0.60.7 quotes identifiers with one allocation |
| `.only()` / `values_list` cannot select a related column (`author__name`) | API gap | **Added in 0.61.2** |
| No `= ANY($1)` array bind for `IN` lists | under 0.5% | **Added in 0.61.2** (PostgreSQL, lists of one type) |
| No batch many-to-many prefetch | API gap | Open |
| Cached index: Rustango · axum 174 501 req/s against axum + sqlx's 198 669 | not profiled | Open |

---

## Round 1: interpreted runtimes

The four Python servers come from one run (14 October 2026); Laravel and
WordPress from 13 October 2026. Same rules as above, with every app running.

### The apps

| App | Server | Data access | Page cache |
|---|---|---|---|
| **Python · gunicorn** | gunicorn, 4 × 8 gthread (WSGI, sync views) | the framework's ORM, built-in psycopg pool | framework Redis cache |
| **Python · Hypercorn** | Hypercorn, 4 workers, uvloop (ASGI, async views) | the framework's async ORM API, same pool | `redis.asyncio`, one GET per hit |
| **Python · Uvicorn** | Uvicorn, 4 workers, uvloop + httptools (ASGI, async views) | same | same |
| **Python · Granian** | Granian, 4 workers, uvloop (ASGI, async views) | same | same |
| **Laravel · php-fpm** | nginx + php-fpm, 16 static children | Eloquent, `with()` + `withCount()` | `Cache::remember`, phpredis |
| **Laravel · Octane** | Octane + Swoole, 4 workers | same | same |
| **WordPress** | nginx + php-fpm, 16 static children | `WP_Query`, `get_the_tags`, `get_comments` on **MySQL 8.4** | `advanced-cache.php` drop-in, phpredis |

Templates: the Python framework's own engine, Blade for Laravel, a small
theme for WordPress. The async Python apps use `async def` views on the
framework's async ORM API, an async middleware wrapper, and one shared
`redis.asyncio` pool of 16 per process; a cached hit is one Redis GET of raw
HTML.

Versions: PHP 8.3.35, Laravel 13.36.0, Swoole 6.2.3, WordPress 6.9.10,
Uvicorn 0.54.0, Granian 2.8.3, uvloop 0.22.1, httptools 0.8.0.

### Python servers

#### Throughput: requests per second (higher is better)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| index, not cached | **697** | 663 | 675 | 692 |
| index, cached | **11 012** | 5 151 | 6 233 | 7 706 |
| detail, not cached | **2 778** | 1 550 | 1 654 | 1 854 |
| detail, cached | **11 848** | 5 085 | 6 215 | 7 810 |
| tag, not cached | **1 411** | 906 | 948 | 1 020 |
| compute | **422** | 388 | 385 | 404 |

#### Latency: p50 / p99 in ms (lower is better)

| Endpoint | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| index, not cached | **66.9** / 155.8 | 71.2 / 128.1 | 72.8 / **113.6** | 69.9 / 119.9 |
| index, cached | **4.9 / 10.1** | 9.3 / 23.5 | 7.1 / 19.6 | 6.4 / 15.4 |
| detail, not cached | **18.9 / 34.9** | 30.5 / 57.0 | 29.2 / 52.5 | 26.0 / 47.6 |
| detail, cached | **3.8 / 8.5** | 9.7 / 24.5 | 7.7 / 21.1 | 6.2 / 15.1 |
| tag, not cached | **27.9** / 89.0 | 52.3 / 95.0 | 52.4 / 84.4 | 49.0 / **80.3** |
| compute | **113.0** / 215.8 | 128.6 / 160.6 | 131.7 / **154.5** | 117.9 / 187.1 |

#### Footprint

| | gunicorn | Hypercorn | Uvicorn | Granian |
|---|--:|--:|--:|--:|
| Image (uncompressed, shared) | 345 MB | 345 MB | 345 MB | 345 MB |
| RAM, index under load | **224 MiB** | 255 MiB | 249 MiB | 293 MiB |
| CPU, index under load (of 400%) | 244% | 323% | 314% | 303% |

### PHP

#### Throughput: requests per second (higher is better)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| index, not cached | 518 | **1 098** | 599 |
| index, cached | 3 326 | 5 884 | **21 285** |
| detail, not cached | 572 | **1 712** | 687 |
| detail, cached | 3 013 | 5 836 | **21 591** |
| tag, not cached | 490 | **1 102** | 647 |
| compute | **1 611** | 1 521 | 732 |

#### Latency: p50 / p99 in ms (lower is better)

| Endpoint | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| index, not cached | 95.5 / 122.6 | **44.1** / 110.1 | 78.1 / **103.9** |
| index, cached | 14.6 / 22.4 | 7.4 / 57.7 | **2.3 / 3.5** |
| detail, not cached | 86.5 / 113.4 | **27.9** / 96.2 | 70.5 / **99.0** |
| detail, cached | 16.4 / 21.6 | 7.4 / 58.2 | **2.3 / 3.6** |
| tag, not cached | 100.8 / 130.1 | **44.0** / 110.9 | 75.9 / **97.0** |
| compute | **31.3 / 38.7** | 32.7 / 78.5 | 67.0 / 89.0 |

#### Footprint

| | Laravel · php-fpm | Laravel · Octane | WordPress |
|---|--:|--:|--:|
| Image (uncompressed) | **844 MB** | 895 MB | 1.01 GB |
| RAM, idle | **54 MiB** | 219 MiB | 61 MiB |
| RAM, index under load | **89 MiB** | 241 MiB | 142 MiB |
| CPU, index under load (of 400%) | 380% | 337% | 405% |

One of WordPress's three uncached-index runs returned 14 253 "database
connection" errors in 7 s; the median comes from a clean run.

---

## Limits

- A **read-heavy, server-rendered** workload. No writes, transactions, auth
  flows, websockets or heavy business logic.
- One host: an Apple M3 Pro laptop (6 performance + 6 efficiency cores).
  Other CPUs can order the results differently, especially compute.
- The apps run in the Docker VM on macOS, not on bare-metal Linux. The VM has
  no hardware performance counters.
- Go and the plain Rust apps are hand-written services; Rustango, Rocket,
  Loco, Cot, the Python app, Laravel and WordPress are frameworks.
- Rounds 1 and 2 are separate runs; compare numbers within one round.
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

- [Benchmark Round 2: Rust, Go and the Rust Frameworks, Profiled](https://im-fullstack.dev/blog/benchmark-round-2-rust-go-rust-frameworks-profiled)
- [Round 1: the fair benchmark](https://im-fullstack.dev/blog/fair-benchmark-rust-go-python-laravel-wordpress)
- [Getting started](getting-started.md)
- [ORM cookbook](orm.md)
