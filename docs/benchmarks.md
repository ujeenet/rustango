# Benchmarks: Rustango vs Django vs Laravel vs Go

How fast is **Rustango**, really? This page reports a head-to-head benchmark
against the two frameworks **Rustango** is modeled on — Django and Laravel — and
against a **Go** baseline (standard-library `net/http`) that anchors what a
second compiled, native runtime achieves on the same workload. All use
*functionally identical* blog sites: same data, same schema, same endpoints,
same hardware budget — the only variable is the runtime. Django and Laravel are
each benchmarked in **both** their conventional production deployment *and* a
more robust runtime: **Django** on **gunicorn** (WSGI) and on **Hypercorn**
(ASGI); **Laravel** on **php-fpm + nginx** and on **Octane** (Swoole). Rustango
and Go are each a single resident binary, so there is one of each.

Every number below is **measured and reproducible**, from one consistent run of a
one-command harness (see [Reproduce](#reproduce)). Nothing here is hand-waved.

> **TL;DR.** On identical hardware serving identical rendered-HTML pages, the two
> **compiled, native** runtimes — **Rustango** and **Go** — leave the interpreted
> frameworks **5–30× behind** and trade the lead between themselves. On the
> non-cached index **Go** led at **6,651 req/s** and **Rustango** followed at
> **4,781** — **5.6×** Django (gunicorn) and **11.7×** Laravel (php-fpm). Go's
> edge is widest on the non-cached detail page (**13,921 vs 6,538**, 2.1×).
> **Rustango** reclaims the lead where it counts for served traffic: the
> **Redis-cached** paths (**25,546** vs Go's 20,929 on the index; 35,781 vs
> 29,470 on detail) and **pure compute** (14,341 vs 11,573). It also held the
> **least RAM under load** (18.5 MiB, no GC) and — unlike the Go stdlib app —
> ships a full batteries-included framework. Even the fastest non-compiled
> result, **Laravel on Octane**, is 4–7× behind both binaries.

[![Requests/sec on the non-cached blog index across all six runtimes — Go 6,651, Rustango 4,781, Laravel+Octane 1,238, Django+Hypercorn 910, Django+gunicorn 850, Laravel+php-fpm 408](img/benchmarks.png)](img/benchmarks.png)

---

## The setup

Four blog apps — **authors, posts, tags (many-to-many), and comments** —
rendering HTML pages:

| Route | Renders | Cached? |
|---|---|---|
| `GET /` | latest 20 posts, each with author, tags, comment count | no |
| `GET /cached` | same as `/` | Redis, 60 s |
| `GET /post/{slug}` | post body + author + tags + every comment | no |
| `GET /post/{slug}/cached` | same as detail | Redis, 60 s |
| `GET /tag/{slug}` | posts carrying a tag | no |
| `GET /compute` | sum of every prime below 20 000 (CPU-bound; no DB, no cache) | no |

The first five are I/O + render bound; `/compute` is a pure CPU workload — the
identical trial-division algorithm in each language — to isolate raw runtime
speed. Each app loads its relations **eagerly** (no N+1): **Rustango** batches the
queries explicitly, Django uses `select_related` / `prefetch_related` /
`annotate(Count)`, Laravel uses `with()` + `withCount()`, **Go** batch-loads
with `= ANY($1)` queries. Templates are deliberately tiny and equivalent (Tera,
Django templates, Blade, Go `html/template`) so we measure the *framework*, not
template effort.

### What makes it a fair fight

- **Identical data.** One Postgres schema and a deterministic seed shared by all
  four apps — they read the *same tables*: 10 authors, 30 tags, **1 000 posts**,
  2 600 post-tag links, **10 000 comments**. The index shows the same 20 posts in
  the same order on every framework, and the rendered HTML is byte-identical
  (modulo each engine's entity-escaping style).
- **Identical hardware budget.** Every app runs in a container capped to
  **4 CPUs / 2 GB RAM**. Postgres and Redis are shared and identical.
- **One at a time.** Apps are load-tested sequentially so they never compete for
  the host (a 12-core / 18 GB machine). Load generator:
  [`oha`](https://github.com/hatoo/oha), 50 concurrent connections, 10 s per
  endpoint, HTTP keep-alive on. Every run reported **100 % success**.
- **All in production mode.** This matters a lot (see below).

### Production configuration

| | Runtime | Production hardening |
|---|---|---|
| **Rustango** | one `--release` binary (axum + Tokio, async, all cores) | `opt-level=3` + LTO; Redis page cache via `CachePageLayer` |
| **Go** | one static binary (stdlib `net/http`, goroutines, all cores) | `pgx` pool + `go-redis` page cache; templates embedded; ships on `scratch` |
| **Django 5.2** · gunicorn | gthread workers + keep-alive (WSGI) | `DEBUG=False`; built-in Redis cache + `@cache_page`; persistent DB connections |
| **Django 5.2** · Hypercorn | ASGI server, 4 workers, keep-alive | same app, served over ASGI; the sync views run in a threadpool |
| **Laravel 13** · php-fpm | php-fpm + nginx, OPcache **on**, 16 workers | `APP_ENV=production`; `composer install --no-dev --optimize-autoloader`; Blade cached; `Cache::remember` on Redis |
| **Laravel 13** · Octane | Octane + **Swoole**, persistent workers | same app on a memory-resident runtime — no per-request framework bootstrap |

> Production mode is not a footnote. Laravel's first (un-tuned) run managed only
> ~53 req/s; enabling OPcache, the optimized autoloader, and a proper worker
> pool took it to ~408 req/s — a **7.7× swing** from configuration alone. The
> numbers below are all from the production configuration.

---

## Results

### Throughput — requests per second (higher is better)

The two compiled binaries, then each interpreted framework in its conventional
runtime **and** its robust runtime:

| Endpoint | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| index, non-cached | 4 781 | **6 651** | 850 | 910 | 408 | 1 238 |
| index, **cached** | **25 546** | 20 929 | 4 841 | 1 537 | 1 224 | 5 777 |
| detail, non-cached | 6 538 | **13 921** | 1 983 | 916 | 464 | 1 790 |
| detail, **cached** | **35 781** | 29 470 | 4 843 | 1 320 | 793 | 5 811 |
| tag, non-cached | 3 926 | **4 353** | 1 129 | 1 033 | 398 | 1 179 |
| **compute** (CPU-bound) | **14 341** | 11 573 | 452 | 400 | 716 | 1 504 |

Three stories jump out. First, **Go and Rustango cluster far above everyone
else** — the gap between the two native binaries (tens of percent, with Go ahead
on uncached I/O and Rustango ahead on cached hits + compute) is small next to the
5–30× gulf to the interpreted runtimes. Second, **Laravel + Octane** (Swoole) is
a **3–7× jump** over php-fpm — a resident worker that skips Laravel's per-request
framework bootstrap — and is the fastest non-compiled result on every page.
Third, **Django + Hypercorn** (ASGI) is roughly **flat, and slower on the cached
paths**: the blog's views are *synchronous*, so ASGI just adds a threadpool hop
with none of the concurrency payoff *async* views would bring. Even the best of
that field (Octane, 1 238 req/s non-cached index; 5 811 on cached detail) trails
both binaries by 4–7×.

### Latency — p50 in milliseconds (lower is better)

| Endpoint | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| index, non-cached | 10.2 | **7.2** | 56.9 | 43.2 | 114.7 | 39.9 |
| index, cached | **1.8** | 2.1 | 9.3 | 5.0 | 20.2 | 7.6 |
| detail, cached | **1.3** | 1.5 | 5.8 | 32.1 | 81.8 | 7.6 |
| compute (CPU-bound) | 3.5 | **2.5** | 70.8 | 130.0 | 87.4 | 32.9 |

(Medians shown; full p50 / p95 / p99 for every endpoint and runtime are in
`bench/results/summary.tsv`.) **Rustango**'s and **Go**'s medians on the
non-cached index (10.2 / 7.2 ms) are below every interpreted competitor's, and
their cached medians (1.3–2.1 ms) are an order of magnitude under even the
fastest framework runtime. One caveat in Go's favor *and* against it: its compute
p50 (2.5 ms) beats Rustango's, but its compute p99 spikes to ~47 ms on a GC
pause — a tail the no-GC Rust binary doesn't have.

### Footprint — image size, memory, CPU

| | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Container image (uncompressed) | 164 MB | **18.5 MB** | 293 MB | 293 MB | 959 MB | 1.01 GB |
| RAM, idle | 12.1 MiB | **5.2 MiB** | 128 MiB | 173 MiB | 92 MiB | 248 MiB |
| RAM, under load | **18.5 MiB** | 34.7 MiB | 218 MiB | 277 MiB | 133 MiB | 267 MiB |
| CPU under load (of 400 % cap) | 295 % | 366 % | 356 % | 408 % | 406 % | 335 % |

**Go** ships the smallest image — a static binary on `scratch`, **18.5 MB** — and
idles at just **5.2 MiB**. But under load its GC heap grows to **34.7 MiB**,
~1.9× **Rustango**'s flat **18.5 MiB**: with no garbage collector and no
per-request allocation, the Rust binary under full load fits in less RAM than Go
does, and below what any interpreted runtime uses sitting *idle*. The robust
runtimes cost *more* memory, not less: Octane keeps a resident Laravel in every
worker; Hypercorn adds the ASGI stack on top of Django.

### Efficiency — work done per resource (the real story)

Raw throughput is one thing; **throughput per unit of resource** is what your
cloud bill actually tracks (non-cached index):

| Metric | **Rustango** | **Go** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|--:|
| Requests/sec **per MiB RAM** | **258** | 192 | 3.9 | 3.3 | 3.1 | 4.6 |
| Requests/sec **per CPU %** | 16.2 | **18.2** | 2.4 | 2.2 | 1.0 | 3.7 |

Per megabyte of memory, **Rustango** does the most work of any runtime here —
~35× the best interpreted result and ~1.3× Go's, because its footprint stays flat
under load. Per CPU-percent, **Go** edges ahead (it converts the extra cores it
spins up into slightly more throughput). Either way, to match one compiled
binary's index throughput you'd run ~4 Octane'd Laravels or ~6 gunicorn
Djangos — each carrying its own multi-hundred-MB footprint.

---

## What the cache changes

Every runtime is fastest when a Redis page cache lets it skip the database and
the render entirely — non-cached → **cached** index req/s:

- **Rustango**: 4 781 → **25 546** (5.3× from caching) — reclaims first place.
- **Go**: 6 651 → **20 929** (3.1×) — leads uncached, second cached.
- **Laravel · Octane**: 1 238 → **5 777** (4.7×) — the interpreted field's best.
- **Django · gunicorn**: 850 → **4 841** (5.7×).
- **Django · Hypercorn**: 910 → **1 537** (1.7×) — the ASGI threadpool overhead
  caps the gain even on cache hits.
- **Laravel · php-fpm**: 408 → **1 224** (3.0×).

Caching helps everyone, but it doesn't erase the gap — and it's where the two
compiled binaries separate: with no application code running, the
HTTP-accept → cache-read → response path is all that's left, and **Rustango**'s
allocation-free `CachePageLayer` (25 546 req/s) pulls ahead of Go's `go-redis`
path (20 929), with both ~4× the best framework runtime.

---

## Raw computation: compiled vs interpreted (and compiled vs compiled)

The five page routes are dominated by the database and the template engine. The
`/compute` route strips those away — it sums every prime below 20 000 by trial
division, the *identical* algorithm in Rust, Go, Python, and PHP. All four return
the same answer (`21171191`); only the speed differs:

| | **Rustango** | **Go** | Django | Laravel |
|---|--:|--:|--:|--:|
| Throughput | **14 341 req/s** | 11 573 | 452 | 716 |
| p50 latency | 3.5 ms | **2.5 ms** | 70.8 ms | 87.4 ms |

The two native binaries run the loop **~26–32×** faster than Django and
**~16–20×** faster than Laravel — the gap between compiled machine code and a
bytecode interpreter. Between Rust and Go, Rust's LTO'd `--release` loop takes the
throughput crown while Go's median latency is actually lower; Go's GC then shows
up as tail latency (p99 ~47 ms) the Rust binary never pays. Interestingly PHP 8.3
(with OPcache) out-computes CPython on this tight integer loop, so Laravel
*out-computes* Django here even though it loses on every I/O-bound page. This is
the workload where the language, not the framework, dominates — and where pushing
hot logic into **Rustango** pays off most.

---

## Honest caveats

A benchmark you can't poke holes in isn't worth publishing. So:

- Numbers are from **one** 12-core / 18 GB host (macOS + Docker). Absolute
  values shift on other hardware; the **relative** picture is what travels.
- This is a **read-heavy, server-rendered** workload — the most common blog
  shape. It does not measure writes, auth flows, websockets, or heavy
  business logic.
- **Go here is the standard library, not a peer framework.** Rustango, Django,
  and Laravel are batteries-included frameworks (ORM, admin, migrations, routing,
  templating, multi-tenancy); the Go app is hand-written `net/http` + raw SQL —
  the leanest, fastest baseline a Go service realistically reaches, and the
  fairest representation of the language. That it ties or beats Rustango on raw
  uncached throughput is exactly the point: **Rustango delivers Go-class
  performance with a Django/Laravel-class developer experience.** Go's edge on
  uncached endpoints is bought by writing the SQL, mapping, and wiring yourself.
- The runtimes are fundamentally different: Rustango and Go are each one binary
  using all cores with cheap tasks; Django and Laravel use fixed pools of worker
  processes/threads. That difference *is* part of the result, and the worker
  counts were set to sensible per-CPU values, not tuned to favor anyone.
- Django and Laravel are each shown in **both** runtimes — conventional
  (gunicorn, php-fpm) and robust (Hypercorn, Octane). **Laravel on Octane is
  3–7× faster** than php-fpm; **Django on Hypercorn** (sync views) is roughly
  flat. Both narrow the gap to the compiled binaries; neither closes it.

The point isn't that Django or Laravel are slow — they power a huge slice of the
web. It's that **Rustango** gives you that same batteries-included developer
experience with the performance and footprint of compiled Rust — matching a
hand-tuned Go service while handing you the framework Go makes you build.

---

## Reproduce

The full harness — the four apps, the shared Postgres schema + deterministic
seed, the Docker Compose setup, and the runner — is a self-contained project
(`rustango-bench`). From its directory:

```sh
bench/vendor.sh                       # vendor the framework into the build
docker compose build                  # build all six images
DURATION=10s CONCURRENCY=50 bench/run.sh
```

Requirements: Docker + Compose and Rust (for `cargo install oha`). Tune the
hardware cap in `.env` (`CAP_CPUS`, `CAP_MEM`) and the load with `DURATION` /
`CONCURRENCY`. Raw per-run output lands in `bench/results/`.
