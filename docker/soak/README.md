# The commerce soak

A Docker fleet that runs the two `platform_commerce*` examples across
all three dialects, under sustained load, and asserts one named check
per behaviour change since 0.57.0.

It exists because every fix in the 0.57.x train was verified **in
isolation**, by a test written for that one issue. Nothing had run them
together, under load, against real infrastructure.

## Running it

```bash
export RUSTANGO_SESSION_SECRET="$(openssl rand -base64 32)"

# ~25 min of image builds the first time; the BuildKit cache is shared
# across all four, so later builds are minutes.
docker compose -f docker/soak/docker-compose.yml build

docker compose -f docker/soak/docker-compose.yml up -d
docker compose -f docker/soak/docker-compose.yml --profile driver up driver
```

`up -d` gives you a running fleet to poke at. The driver sits behind a
profile so bringing the stack up does not start a 30-minute run.

Knobs: `SOAK_DURATION_SECS` (1800), `SOAK_CONCURRENCY` (24),
`SOAK_TENANTS` (20), `SOAK_FAIL_RATIO_PCT` (2).

## Reading the log

`INFO` is the default and tells the story — what each instance is,
which tenant queues started, every order queued, every job completed:

```
web-saas-pg   INFO platform_commerce_saas: starting tenants=20 fail_ratio_pct=2
web-saas-pg   INFO supervisor: tenant queue started tenant=t01 workers=2
web-saas-pg   INFO urls: order queued for fulfilment order=8123 tenant=t07
worker-pg     INFO jobs: order confirmed tenant=t07 order=8123
```

`WARN` is for failures the soak **injected on purpose** — they carry
`expected=true`, and they are how the run proves `MAX_ATTEMPTS` is a
total-attempt ceiling (`attempts=4`) and that `JobError::Fatal` bypasses
retry (`attempts=1`).

`ERROR` is reserved for a failure nobody asked for. If the log has one,
that is the finding.

That split is deliberate. At a 10% injection rate and ERROR severity the
fleet produced **6308 ERROR lines in five minutes** — every one an
assertion passing, and a real failure in that stream would have been
invisible. A soak whose output cannot be read is worse at its job than
no soak.

For per-attempt detail — each retry of an injected failure, each
storefront render, each supervisor tick:

```bash
RUST_LOG='info,platform_commerce=debug,platform_commerce_saas=debug' \
  docker compose -f docker/soak/docker-compose.yml up -d
```

`sqlx=warn` stays set: at these volumes sqlx's own DEBUG is a line per
statement and drowns everything else.

The report lands in the `soak-results` volume as `report.json` and is
printed. **Exit code is non-zero only on FAIL.** NOT-COVERED does not
fail the run: an honest gap is not a regression, and the whole point is
that a check which cannot run must never report a pass.

## Four images, not six

`platform_commerce` compiles with all three backend features at once and
picks its dialect at run time from `DATABASE_URL`, so one image covers
the matrix.

`platform_commerce_saas` cannot. `DefaultTenantDb` resolves to
`sqlx::Postgres` whenever the `postgres` feature is on
(`tenancy/pools.rs`), so a tenancy build's *registry* dialect is fixed
at compile time — hence one SaaS image per dialect, each built with
exactly one backend feature. That is a real property of the framework,
not a packaging choice.

## Things that will bite, and why they are set up this way

**Connections.** 20 tenants × 16 connections × (web + worker) = 640,
against a stock Postgres limit of 100. `TenantPoolsConfig` is
unreachable from `manage::Cli` and reads no environment variables
(#1456), so the database server's own limit is the only lever. Hence
`max_connections=600` on `pg` and `--max-connections=1200` on `my`.

**Shutdown grace.** `stop_grace_period: 45s`, because Docker's default
is 10s, `PgJobQueue::shutdown` gives each in-flight job a hard-coded 5s,
and `Cli::on_shutdown` runs *after* the server drains. Ten seconds
reliably SIGKILLs mid-drain and loses exactly the jobs being counted.

**Tenant databases.** rustango does not create them — `provision.rs`
says "database-mode tenants bring their own database", and preflight
creates and drops a table to prove migrations can run, so a missing
database fails provisioning outright. `bootstrap.sh` pre-creates the
MySQL ones with a single wildcard grant. Postgres schema mode and
SQLite's `?mode=rwc` need nothing.

**Resolver convergence.** Sleep, do not poll. Negative results are
cached for 30s, so probing a tenant host too early poisons the cache and
makes a working tenant look broken for a minute. The driver waits and
then probes once, and asserts on the tenant in the *response* rather
than the Host it sent.

**Health checks do not use `/health`.** `.with_health()` mounts it on a
Postgres build and silently does nothing on SQLite or MySQL (#1457).
On the SaaS app every app route lives on the tenant router, so a probe
with no tenant Host lands on the operator console anyway. The check asks
the only question it can answer for both apps on all three dialects: is
the process listening and routing?

**SQLite runs its workers in-process.** Two containers sharing one
SQLite file need POSIX advisory locks plus WAL shared memory across the
container boundary, which is unreliable on Docker Desktop. The files
live on a named volume, never a bind mount.

**Postgres and MySQL get a separate worker container**, which is the
point of having one: `rustango_jobs` rows are claimed with three
different per-dialect strategies, and only two processes competing for
the same rows exercises that. A MySQL clause-order bug in v0.38 meant
workers silently never picked anything up; a single-process queue would
not have found it.

## Ports

Shifted, because this stack is meant to run *alongside* the repo-root
`docker-compose.yml`. Container-internal ports are the standard ones.

| | host | container |
|---|---|---|
| postgres | 5433 | 5432 |
| mysql | 3406 | 3306 |
| redis | 6479 | 6379 |
| minio | 9100 / 9101 | 9000 / 9001 |
| web-single-{pg,my,sq} | 18081–18083 | 8080 |
| web-saas-{pg,my,sq} | 18084–18086 | 8080 |

MySQL publishes 3406 to match the root compose, which picked it to dodge
a local MySQL. CI and the scaffolder use 3306 because they run in
isolated environments. That discrepancy has confused people; the
container port is 3306 everywhere and is the only one that matters
inside the network.

## What the driver cannot check

Stated because a harness that silently scores these as passes would be
the exact failure this release was about.

- **#1450 is Postgres-only.** The MySQL and SQLite binders were
  deliberately left unchanged, so those two arms are controls. The
  report says so per instance rather than printing three passes.
- **#1412** is a dropped column plus a `tracing::warn!`. HTTP cannot see
  it; it needs a subscriber capturing the event.
- **#1410** (backoff timing, `MAX_ATTEMPTS` as a total ceiling) is
  in-process. The deterministic flaky job makes the dead-letter count
  exactly predictable, which is the closest an HTTP harness gets.
- **#1422** has no runtime behaviour at all — the only assertion
  available is `--locked` resolving, which `lockfiles` already does.
- **#1440** is best proven by pointing a suite at a dead port, which is
  a test-suite property rather than an application one.

## Known gaps

A `KNOWN-GAP` verdict is a behaviour that was exercised, is **wrong**,
has an issue, and is being shipped anyway on purpose. It is printed in
its own block on every run and never counted as a pass — the point of a
fourth verdict is that "broken and known" and "working" must not look
alike in a report.

- **[#1464](https://github.com/ujeenet/rustango/issues/1464) — SQLite
  `auto_now_add` columns cannot be compared against a Rust-bound
  `DateTime`.** `DEFAULT CURRENT_TIMESTAMP` writes
  `2026-09-15 02:53:25`; sqlx binds `DateTime<Utc>` as RFC3339
  `2026-09-15T02:53:25+00:00`. Lexically `' '` (0x20) sorts before `'T'`
  (0x54), so `WHERE placed_at < $cursor` is true for *every* row and
  cursor pagination serves page one forever. Both SQLite legs report it;
  Postgres and MySQL page correctly.

  Not fixed in 0.57.5 because every available fix changes SQLite's
  stored datetime format, which wants its own release and a migration
  for databases that already hold both formats. The framework has hit
  this once before and patched it at a single call site (`audit.rs`,
  citing #560) rather than centrally, which is why it was still here to
  find.

  The scoping is deliberate: only this check, only on `*-sq` instances,
  only for the repeat-rows symptom. A cursor that fails to advance on
  Postgres or MySQL, or fails any other way, is still a `FAIL`.
