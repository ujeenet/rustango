# Benchmarks: Rustango vs Django vs Laravel

How fast is **Rustango**, really? This page reports a head-to-head benchmark
against the two frameworks **Rustango** is modeled on — Django and Laravel —
using three *functionally identical* blog sites. Same data, same schema, same
endpoints, same hardware budget. The only variable is the framework.

Every number below is **measured and reproducible** — there's a one-command
harness (see [Reproduce](#reproduce)). Nothing here is hand-waved.

> **TL;DR.** On identical hardware, serving identical rendered-HTML blog pages,
> **Rustango** handled **6×** the requests of Django and **12×** the requests of
> Laravel on the non-cached index, while using **~25×** less memory and shipping
> in the **smallest** container image. With a Redis page cache in front, the gap
> widened to **7×** (Django) and **29×** (Laravel).

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

Each app loads its relations **eagerly** (no N+1): **Rustango** batches the
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
| **Django 5.2** | gunicorn, gthread workers + keep-alive | `DEBUG=False`; built-in Redis cache + `@cache_page`; persistent DB connections |
| **Laravel 13** | php-fpm + nginx, OPcache **on**, 16 workers | `APP_ENV=production`; `composer install --no-dev --optimize-autoloader`; Blade views cached; `Cache::remember` on Redis |

> Production mode is not a footnote. Laravel's first (un-tuned) run managed only
> ~53 req/s; enabling OPcache, the optimized autoloader, and a proper worker
> pool took it to ~396 req/s — a **7× swing** from configuration alone. The
> numbers below are all from the production configuration.

---

## Results

### Throughput — requests per second (higher is better)

| Endpoint | **Rustango** | Django | Laravel | Rustango advantage |
|---|--:|--:|--:|---|
| index, non-cached | **4 982** | 812 | 396 | 6.1× / 12.6× |
| index, **cached** | **30 455** | 4 133 | 1 059 | 7.4× / 28.8× |
| detail, non-cached | **6 559** | 1 874 | 439 | 3.5× / 14.9× |
| detail, **cached** | **43 044** | 4 250 | 848 | 10.1× / 50.8× |
| tag, non-cached | **4 020** | 1 025 | 417 | 3.9× / 9.6× |

On the cached post-detail page, **Rustango** served **43 000 requests/second** —
ten times Django and fifty times Laravel from the same 4-core box.

### Latency — p50 / p95 / p99 in milliseconds (lower is better)

| Endpoint | **Rustango** | Django | Laravel |
|---|--:|--:|--:|
| index, non-cached | **9.9 / 11.0 / 12.7** | 51 / 123 / 160 | 118 / 171 / 179 |
| index, cached | **1.6 / 2.1 / 2.7** | 8.3 / 36 / 43 | 26 / 86 / 100 |
| detail, non-cached | **7.5 / 8.4 / 9.8** | 25 / 47 / 54 | 109 / 160 / 171 |
| detail, cached | **1.1 / 1.6 / 2.0** | 9.0 / 34 / 40 | 76 / 95 / 101 |
| tag, non-cached | **12.2 / 13.7 / 16.9** | 47 / 80 / 93 | 114 / 160 / 170 |

**Rustango**'s tail (p99) on the non-cached index — 12.7 ms — is lower than the
*median* of either competitor, and its cached p99 (2.7 ms) is under Django's and
Laravel's best-case numbers.

### Footprint — image size, memory, CPU

| | **Rustango** | Django | Laravel |
|---|--:|--:|--:|
| Container image (uncompressed) | **164 MB** | 290 MB | 959 MB |
| RAM, idle | **2.6 MiB** | 128 MiB | 48 MiB |
| RAM, under load | **8.5 MiB** | 219 MiB | 87 MiB |
| CPU under load (of 400 % cap) | 312 % | 375 % | 404 % |
| Boot to first response | **0.8 s** | 1.1 s | 1.0 s |

A **Rustango** app under full load fits in **8 MiB** of RAM — less than these
frameworks use sitting idle. The single static-ish binary also produces the
smallest deployable image despite shipping with every battery included.

### Efficiency — work done per resource (the real story)

Raw throughput is one thing; **throughput per unit of resource** is what your
cloud bill actually tracks.

| Metric (non-cached index) | **Rustango** | Django | Laravel |
|---|--:|--:|--:|
| Requests/sec **per MiB RAM** | **586** | 3.7 | 4.6 |
| Requests/sec **per CPU %** | **16.0** | 2.2 | 1.0 |

Per megabyte of memory, **Rustango** does roughly **125× more work** than either
framework. Put differently: to match one **Rustango** instance's index
throughput you'd run ~6 Django or ~13 Laravel instances — each carrying its own
multi-hundred-MB memory footprint.

---

## What the cache changes

Every framework is fastest when a Redis page cache lets it skip the database and
the render entirely — but the ranking and the multiples hold:

- **Rustango**: 4 982 → **30 455** req/s on the index (**6.1×** from caching).
- **Django**: 812 → 4 133 req/s (5.1×).
- **Laravel**: 396 → 1 059 req/s (2.7×).

Caching helps everyone, but it doesn't erase the gap — even when no application
code runs, the HTTP-accept → cache-read → response path differs by runtime, and
**Rustango**'s async core stays ahead.

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
- Both competitors can go faster than their standard setups here: **Laravel on
  Octane/FrankenPHP** and **Django on an async ASGI stack** would narrow the
  gap. We benchmarked the conventional production deployment of each.

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
