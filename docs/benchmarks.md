# Benchmarks: Rustango vs Django vs Laravel

How fast is **Rustango**, really? This page reports a head-to-head benchmark
against the two frameworks **Rustango** is modeled on — Django and Laravel —
using *functionally identical* blog sites. Same data, same schema, same
endpoints, same hardware budget — the only variable is the runtime. Each
framework is benchmarked in **both** its conventional production deployment *and*
its more robust runtime: **Django** on **gunicorn** (WSGI) and on **Hypercorn**
(ASGI); **Laravel** on **php-fpm + nginx** and on **Octane** (Swoole).

Every number below is **measured and reproducible**, from one consistent run of a
one-command harness (see [Reproduce](#reproduce)). Nothing here is hand-waved.

> **TL;DR.** On identical hardware serving identical rendered-HTML pages,
> **Rustango** handled **4,951 req/s** on the non-cached index — **6×** Django
> (gunicorn) and **12.6×** Laravel (php-fpm). The frameworks' robust runtimes
> narrow the gap but don't close it: even **Laravel on Octane** — the fastest
> non-Rustango result — is **4.2×** behind, and **Django on Hypercorn** 5.6×.
> Cached, the field tops out near **5.8k** req/s (Octane) against Rustango's
> **30k**; on pure compute Rustango is **9–32×** ahead. It also used the least
> RAM and shipped the smallest image.

[![Requests/sec on the non-cached blog index across all five runtimes — Rustango 4,951, then Laravel+Octane 1,185, Django+Hypercorn 886, Django+gunicorn 819, Laravel+php-fpm 393](/static/img/benchmarks.png?v=2)](/static/img/benchmarks.png?v=2)

---

## The setup

Three blog apps — **authors, posts, tags (many-to-many), and comments** —
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
`annotate(Count)`, Laravel uses `with()` + `withCount()`. Templates are
deliberately tiny and equivalent (Tera, Django templates, Blade) so we measure
the *framework*, not template effort.

### What makes it a fair fight

- **Identical data.** One Postgres schema and a deterministic seed shared by all
  three apps — they read the *same tables*: 10 authors, 30 tags, **1 000 posts**,
  2 600 post-tag links, **10 000 comments**. The index shows the same 20 posts in
  the same order on every framework.
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
| **Django 5.2** · gunicorn | gthread workers + keep-alive (WSGI) | `DEBUG=False`; built-in Redis cache + `@cache_page`; persistent DB connections |
| **Django 5.2** · Hypercorn | ASGI server, 4 workers, keep-alive | same app, served over ASGI; the sync views run in a threadpool |
| **Laravel 13** · php-fpm | php-fpm + nginx, OPcache **on**, 16 workers | `APP_ENV=production`; `composer install --no-dev --optimize-autoloader`; Blade cached; `Cache::remember` on Redis |
| **Laravel 13** · Octane | Octane + **Swoole**, persistent workers | same app on a memory-resident runtime — no per-request framework bootstrap |

> Production mode is not a footnote. Laravel's first (un-tuned) run managed only
> ~53 req/s; enabling OPcache, the optimized autoloader, and a proper worker
> pool took it to ~396 req/s — a **7× swing** from configuration alone. The
> numbers below are all from the production configuration.

---

## Results

### Throughput — requests per second (higher is better)

Each framework in its conventional runtime **and** its robust runtime, beside
Rustango:

| Endpoint | **Rustango** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|
| index, non-cached | **4 951** | 819 | 886 | 393 | 1 185 |
| index, **cached** | **29 608** | 3 982 | 1 653 | 1 179 | 5 753 |
| detail, non-cached | **6 491** | 1 805 | 896 | 457 | 1 751 |
| detail, **cached** | **43 461** | 4 108 | 1 168 | 786 | 5 658 |
| tag, non-cached | **4 079** | 978 | 921 | 402 | 1 149 |
| **compute** (CPU-bound) | **13 586** | 419 | 395 | 693 | 1 502 |

Two runtime stories jump out. **Laravel + Octane** (Swoole) is a **3–7× jump**
over php-fpm — a resident worker that skips Laravel's per-request framework
bootstrap — and is the fastest non-Rustango result on every page. **Django +
Hypercorn** (ASGI) is roughly **flat, and slower on the cached paths**: the
blog's views are *synchronous*, so ASGI just adds a threadpool hop with none of
the concurrency payoff *async* views would bring — gunicorn's thread pool already
saturates this I/O-bound workload. A robust runtime only helps if the app is
written to use it. Even so, the best of the field (Octane, 1 185 req/s
non-cached; 5 658 on cached detail) trails **Rustango**'s 4 951 / 43 461.

### Latency — p50 in milliseconds (lower is better)

| Endpoint | **Rustango** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|
| index, non-cached | **9.9** | 51.5 | 47.7 | 118.0 | 40.7 |
| index, cached | **1.7** | 8.9 | 26.6 | 20.9 | 7.5 |
| detail, cached | **1.1** | 9.4 | 36.7 | 81.4 | 7.6 |
| compute (CPU-bound) | **3.7** | 116.7 | 86.3 | 88.4 | 32.8 |

(Medians shown; full p50 / p95 / p99 for every endpoint and runtime are in
`bench/results/summary.tsv`.) **Rustango**'s median on the non-cached index
(9.9 ms) is below every competitor's, and its cached median (1.1–1.7 ms) is an
order of magnitude under even the fastest framework runtime.

### Footprint — image size, memory, CPU

| | **Rustango** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|
| Container image (uncompressed) | **164 MB** | 290 MB | 293 MB | 959 MB | 1.01 GB |
| RAM, idle | **12 MiB** | 130 MiB | 173 MiB | 92 MiB | 221 MiB |
| RAM, under load | **18 MiB** | 231 MiB | 270 MiB | 131 MiB | 248 MiB |
| CPU under load (of 400 % cap) | 311 % | 369 % | 406 % | 405 % | 336 % |

A **Rustango** app under full load fits in **~18 MiB** of RAM — below what any of
these use sitting *idle* — in the smallest image, despite shipping every battery
included. Note the robust runtimes cost *more* memory, not less: Octane keeps a
resident Laravel in every worker; Hypercorn adds the ASGI stack on top of Django.

### Efficiency — work done per resource (the real story)

Raw throughput is one thing; **throughput per unit of resource** is what your
cloud bill actually tracks (non-cached index):

| Metric | **Rustango** | Django · gunicorn | Django · Hypercorn | Laravel · php-fpm | Laravel · Octane |
|---|--:|--:|--:|--:|--:|
| Requests/sec **per MiB RAM** | **269** | 3.6 | 3.3 | 3.0 | 4.8 |
| Requests/sec **per CPU %** | **15.9** | 2.2 | 2.2 | 1.0 | 3.5 |

Per megabyte of memory, **Rustango** does **55–90× more work** than any of these
runtimes, Octane included. To match one **Rustango** instance's index throughput
you'd run ~4 Octane'd Laravels or ~6 gunicorn Djangos — each carrying its own
multi-hundred-MB footprint.

---

## What the cache changes

Every runtime is fastest when a Redis page cache lets it skip the database and
the render entirely — non-cached → **cached** index req/s:

- **Rustango**: 4 951 → **29 608** (6.0× from caching).
- **Laravel · Octane**: 1 185 → **5 753** (4.9×) — the field's best cached result.
- **Django · gunicorn**: 819 → **3 982** (4.9×).
- **Django · Hypercorn**: 886 → **1 653** (1.9×) — the ASGI threadpool overhead
  caps the gain even on cache hits.
- **Laravel · php-fpm**: 393 → **1 179** (3.0×).

Caching helps everyone, but it doesn't erase the gap — even when no application
code runs, the HTTP-accept → cache-read → response path differs by runtime, and
**Rustango**'s async core (29 608 req/s) stays ~5× ahead of the best framework
runtime.

---

## Raw computation: compiled vs interpreted

The five page routes are dominated by the database and the template engine. The
`/compute` route strips those away — it sums every prime below 20 000 by trial
division, the *identical* algorithm in Rust, Python, and PHP. All three return
the same answer (`21171191`); only the speed differs:

| | **Rustango** | Django | Laravel |
|---|--:|--:|--:|
| Throughput | **14 859 req/s** | 422 | 651 |
| p50 latency | **3.4 ms** | 117.8 ms | 90.6 ms |

**Rustango** runs the loop ~**35×** faster than Django and ~**23×** faster than
Laravel — the gap between a compiled native binary and a bytecode interpreter.
Interestingly PHP 8.3 (with OPcache) edges out CPython on a tight integer loop,
so Laravel *out-computes* Django here even though it loses on every I/O-bound
page. This is the workload where the language, not the framework, dominates —
and where pushing hot logic into **Rustango** pays off most.

---

## Honest caveats

A benchmark you can't poke holes in isn't worth publishing. So:

- Numbers are from **one** 12-core / 18 GB host (macOS + Docker). Absolute
  values shift on other hardware; the **relative** picture is what travels.
- This is a **read-heavy, server-rendered** workload — the most common blog
  shape. It does not measure writes, auth flows, websockets, or heavy
  business logic.
- The runtimes are fundamentally different: **Rustango** is one async binary
  using all cores with cheap tasks; Django and Laravel use fixed pools of
  worker processes/threads. That difference *is* part of the result, and the
  worker counts were set to sensible per-CPU values, not tuned to favor anyone.
- Each framework is shown in **both** runtimes in the tables above — conventional
  (gunicorn, php-fpm) and robust (Hypercorn, Octane). **Laravel on Octane is
  3–7× faster** than php-fpm; **Django on Hypercorn** (sync views) is roughly
  flat. Both narrow the gap to Rustango; neither closes it.

The point isn't that Django or Laravel are slow — they power a huge slice of the
web. It's that **Rustango** gives you that same batteries-included developer
experience with the performance and footprint of compiled Rust.

---

## Reproduce

The full harness — the three apps, the shared Postgres schema + deterministic
seed, the Docker Compose setup, and the runner — is a self-contained project
(`rustango-bench`). From its directory:

```sh
bench/vendor.sh                       # vendor the framework into the build
docker compose build                  # build all three images
DURATION=10s CONCURRENCY=50 bench/run.sh
```

Requirements: Docker + Compose and Rust (for `cargo install oha`). Tune the
hardware cap in `.env` (`CAP_CPUS`, `CAP_MEM`) and the load with `DURATION` /
`CONCURRENCY`. Raw per-run output lands in `bench/results/`.
